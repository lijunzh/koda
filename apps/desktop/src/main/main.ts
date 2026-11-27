// Koda's desktop app, main process: it owns the connection to Koda's
// background server and passes the page only what `relay.ts` allows. The
// page runs sandboxed, without Node, served from app://koda/ under a strict
// content policy, and can't load anything else.
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
import { KodaClient, type RpcId } from './koda-client';
import { refusal } from './relay';
import { compareVersions, defaultSocket, type ServerStatus } from './server';

const APP_VERSION = app.getVersion();
const SOCKET = process.env.KODA_SOCKET || defaultSocket();
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
const client = new KodaClient();
let status: ServerStatus = { state: 'connecting', socket: SOCKET };

function setStatus(next: ServerStatus) {
  status = next;
  win?.webContents.send('koda:status', status);
}

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

async function connect() {
  setStatus({ state: 'connecting', socket: SOCKET });
  try {
    await client.connect(SOCKET);
    const answer = await client.request<{ serverInfo: { version: string } }>('initialize', {
      clientInfo: { name: 'koda-desktop', title: 'Koda', version: APP_VERSION },
    });
    const serverVersion = answer.serverInfo.version;
    setStatus({
      state: 'connected',
      socket: SOCKET,
      serverVersion,
      appVersion: APP_VERSION,
      match: compareVersions(serverVersion, APP_VERSION),
    });
  } catch (error) {
    client.close();
    setStatus({ state: 'unreachable', socket: SOCKET, error: (error as Error).message });
  }
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

  ipcMain.handle('koda:status', guarded(() => status));
  ipcMain.handle(
    'koda:request',
    guarded((method: unknown, params: unknown) => {
      const why = refusal(method, params);
      if (why) throw new Error(`refused: ${why}`);
      return client.request(method as string, params ?? {});
    }),
  );
  ipcMain.handle(
    'koda:respond',
    guarded((id: RpcId, result: unknown) => {
      if (!client.openServerRequests.has(id)) throw new Error(`refused: no open request ${id}`);
      client.respond(id, result);
    }),
  );

  client.on('messages', (batch) => win?.webContents.send('koda:messages', batch));
  client.on('close', () => {
    if (status.state === 'connected') {
      setStatus({ state: 'unreachable', socket: SOCKET, error: 'the server closed the connection' });
    }
  });

  win = new BrowserWindow({
    width: 1100,
    height: 760,
    title: 'Koda',
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
  await win.loadURL(`${ORIGIN}/index.html`);
  await connect();
}

app.on('window-all-closed', () => app.quit());
app.on('will-quit', () => client.close());
main().catch((error) => {
  process.stderr.write(`koda-desktop: ${(error as Error).stack}\n`);
  app.exit(1);
});
