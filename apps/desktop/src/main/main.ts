// Koda's desktop app, main process: it starts or joins Koda's background
// server (`connection.ts`), owns the connection to it, and passes the page
// only what `relay.ts` allows. The page runs sandboxed, without Node, served
// from app://koda/ under a strict content policy, and can't load anything
// else.
import {
  app,
  BrowserWindow,
  ipcMain,
  protocol,
  session,
  shell,
  type IpcMainEvent,
  type IpcMainInvokeEvent,
} from 'electron';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { Connection } from './connection';
import type { RpcId } from './koda-client';
import { refusal } from './relay';
import { findKoda, type RunningTurns } from './server';
import { loginShellPath } from './shell-path';

const APP_VERSION = app.getVersion();
/** Tests run the app unseen, without taking focus. */
const TEST = process.env.KODA_DESKTOP_TEST === '1';
const RENDERER_DIR = path.join(__dirname, '../renderer');
const ORIGIN = 'app://koda';
const CONTENT_POLICY = [
  "default-src 'none'",
  "script-src 'self'",
  "style-src 'self'",
  "img-src 'self' data:",
  "font-src 'self'",
  "connect-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-ancestors 'none'",
].join('; ');
const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.woff2': 'font/woff2',
};

protocol.registerSchemesAsPrivileged([
  { scheme: 'app', privileges: { standard: true, secure: true, supportFetchAPI: true } },
]);

let win: BrowserWindow | null = null;
let connection: Connection | null = null;

/** Only the app's own page, in the app's own window, may use the bridge. */
function fromOurPage(event: IpcMainInvokeEvent | IpcMainEvent): boolean {
  if (!win || event.sender !== win.webContents || !event.senderFrame) return false;
  try {
    // Node gives a custom scheme an opaque origin, so compare scheme and host.
    const url = new URL(event.senderFrame.url);
    return url.protocol === 'app:' && url.host === 'koda';
  } catch {
    return false;
  }
}

function guarded<A extends unknown[], R>(handler: (...args: A) => R) {
  return (event: IpcMainInvokeEvent, ...args: A): R => {
    if (!fromOurPage(event)) throw new Error('refused: not the app page');
    return handler(...args);
  };
}

async function main() {
  await app.whenReady();

  protocol.handle('app', async (request) => {
    const url = new URL(request.url);
    const relative = path.normalize(decodeURIComponent(url.pathname === '/' ? '/index.html' : url.pathname));
    const file = path.join(RENDERER_DIR, relative);
    if (url.host !== 'koda' || !file.startsWith(RENDERER_DIR + path.sep)) {
      return new Response('not found', { status: 404 });
    }
    let body: Buffer;
    try {
      body = await readFile(file);
    } catch {
      return new Response('not found', { status: 404 });
    }
    return new Response(new Uint8Array(body), {
      headers: {
        'Content-Type': MIME[path.extname(file)] ?? 'application/octet-stream',
        'Content-Security-Policy': CONTENT_POLICY,
        'X-Content-Type-Options': 'nosniff',
      },
    });
  });
  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
  session.defaultSession.setPermissionCheckHandler(() => false);
  // Nothing from outside the app: every other request is cancelled.
  session.defaultSession.webRequest.onBeforeRequest((details, callback) => {
    const ours = details.url.startsWith(ORIGIN + '/') || details.url.startsWith('devtools:');
    callback({ cancel: !ours });
  });

  // The server a `koda` started from here gets this environment: the login
  // shell's PATH, since an app opened from the Finder has launchd's.
  const env = { ...process.env, PATH: await loginShellPath(process.env) };
  const bundled = path.join(process.resourcesPath, 'bin', 'koda');
  const conn = new Connection({
    appVersion: APP_VERSION,
    koda: findKoda(process.env, env.PATH, bundled),
    env,
    socket: process.env.KODA_SOCKET || undefined,
  });
  connection = conn;
  conn.on('status', (status) => win?.webContents.send('koda:status', status));
  conn.on('messages', (batch) => win?.webContents.send('koda:messages', batch));

  ipcMain.handle('koda:status', guarded(() => conn.status));
  ipcMain.handle(
    'koda:request',
    guarded((method: unknown, params: unknown) => {
      const why = refusal(method, params);
      if (why) throw new Error(`refused: ${why}`);
      return conn.request(method as string, params ?? {});
    }),
  );
  ipcMain.handle(
    'koda:respond',
    guarded((id: RpcId, result: unknown) => {
      if (!conn.isOpenRequest(id)) throw new Error(`refused: no open request ${id}`);
      conn.respond(id, result);
    }),
  );
  // The user's answers about the server, from the window's banners.
  ipcMain.handle(
    'koda:replaceServer',
    guarded((turns: unknown) => {
      if (turns !== 'wait' && turns !== 'stop') throw new Error('refused: wait or stop');
      if (conn.status.state !== 'busy') throw new Error('refused: no older server is waiting');
      void conn.open(turns as RunningTurns);
    }),
  );
  ipcMain.handle(
    'koda:retry',
    guarded(() => {
      if (conn.status.state !== 'unreachable' && conn.status.state !== 'noKoda') return;
      void conn.open();
    }),
  );
  ipcMain.handle(
    'koda:relaunch',
    guarded(() => {
      // Koda was updated: the app on disk is the new one.
      app.relaunch();
      app.exit(0);
    }),
  );

  win = new BrowserWindow({
    width: 1100,
    height: 760,
    title: 'Koda',
    show: !TEST,
    webPreferences: {
      preload: path.join(__dirname, '../preload/preload.js'),
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
      webSecurity: true,
      spellcheck: false,
    },
  });
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (url.startsWith('https://')) void shell.openExternal(url);
    return { action: 'deny' };
  });
  win.webContents.on('will-navigate', (event, url) => {
    if (!url.startsWith(ORIGIN + '/')) event.preventDefault();
  });
  win.on('closed', () => {
    win = null;
  });
  if (TEST) win.showInactive();
  await win.loadURL(`${ORIGIN}/index.html`);
  await conn.open();
}

app.on('window-all-closed', () => app.quit());
app.on('will-quit', () => connection?.close());
main().catch((error) => {
  process.stderr.write(`koda-desktop: ${(error as Error).stack}\n`);
  app.exit(1);
});
