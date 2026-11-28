// Opening the real app against a scratch Koda home, for the end-to-end tests.
import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { _electron, type ElectronApplication, type Page } from 'playwright-core';

export const koda = process.env.KODA_BIN;
const appDir = path.join(__dirname, '../..');
const electronPath = createRequire(__filename)('electron') as unknown as string;

export interface OpenApp {
  app: ElectronApplication;
  page: Page;
}

/** Open the built app with `XDG_CONFIG_HOME` at `home`, unseen, starting servers with `KODA_BIN`. */
export async function openApp(home: string): Promise<OpenApp> {
  // Its own settings and caches: without this the tests share the real
  // app's folder (~/Library/Application Support/Koda), and a test that
  // picked Verbose in Settings left it picked for the user (2026-11-27).
  const args = [appDir, `--user-data-dir=${path.join(home, 'app-data')}`];
  // GitHub's Linux runners don't allow the unprivileged user namespaces
  // Chromium's own sandbox needs. The page stays sandboxed by Electron.
  if (process.platform === 'linux') args.unshift('--no-sandbox');
  const app = await _electron.launch({
    executablePath: electronPath,
    args,
    env: { ...process.env, XDG_CONFIG_HOME: home, KODA_DESKTOP_TEST: '1', KODA_BIN: koda! },
  });
  const page = await app.firstWindow();
  return { app, page };
}

/**
 * `koda serve --stop` for the server under `home`, if one runs. Throws when a
 * server is there and didn't stop: its home must not be removed under it.
 */
export function stopServer(home: string) {
  try {
    execFileSync(koda!, ['serve', '--stop'], {
      env: { ...process.env, XDG_CONFIG_HOME: home },
      stdio: ['ignore', 'ignore', 'pipe'],
      encoding: 'utf8',
    });
  } catch (error) {
    const said = String((error as { stderr?: unknown }).stderr ?? '').trim();
    // None was running: the only failure that is fine.
    if (said.includes('no koda serve is listening')) return;
    throw new Error(`koda serve --stop under ${home}: ${said || (error as Error).message}`);
  }
}

/** The socket of the server under `home`. */
export const socketOf = (home: string) => path.join(home, 'koda', 'serve', 'koda.sock');

/** A hold on the app's requests of one method, kept in the main process. */
export interface Hold {
  /** How many requests wait. */
  waiting(): Promise<number>;
  /** Let them go on to the server, and hold no more. */
  release(): Promise<void>;
  /** Answer them with an error, and hold no more. */
  fail(message: string): Promise<void>;
}

interface HoldGlobals {
  kodaTestConnection: { request(method: string, params: unknown): Promise<unknown> };
  kodaHold: {
    real(method: string, params: unknown): Promise<unknown>;
    waiting: Array<{ go(): void; fail(message: string): void }>;
  };
}

/**
 * Hold the app's `method` requests before they reach the server, until the
 * test lets them go: a session stays "opening" for as long as a test needs.
 */
export async function holdRequests(app: ElectronApplication, method: string): Promise<Hold> {
  await app.evaluate((_electron, held) => {
    const g = globalThis as unknown as HoldGlobals;
    const conn = g.kodaTestConnection;
    const real = conn.request.bind(conn);
    g.kodaHold = { real, waiting: [] };
    conn.request = (method, params) =>
      method !== held
        ? real(method, params)
        : new Promise((resolve, reject) => {
            g.kodaHold.waiting.push({
              go: () => resolve(real(method, params)),
              fail: (message) => reject(new Error(message)),
            });
          });
  }, method);
  const end = (message: string | null) =>
    app.evaluate((_electron, message) => {
      const g = globalThis as unknown as HoldGlobals;
      g.kodaTestConnection.request = g.kodaHold.real;
      for (const held of g.kodaHold.waiting.splice(0)) {
        if (message === null) held.go();
        else held.fail(message);
      }
    }, message);
  return {
    waiting: () => app.evaluate(() => (globalThis as unknown as HoldGlobals).kodaHold.waiting.length),
    release: () => end(null),
    fail: (message) => end(message),
  };
}

/** Quit the app; if it hasn't gone within 10 seconds, kill it, so a hang costs one test, not the run. */
export async function closeApp({ app }: OpenApp): Promise<void> {
  let timer: NodeJS.Timeout | undefined;
  const late = new Promise<'late'>((resolve) => (timer = setTimeout(() => resolve('late'), 10_000)));
  const closed = await Promise.race([app.close().then(() => 'closed' as const), late]);
  clearTimeout(timer);
  if (closed === 'late') app.process().kill('SIGKILL');
}

/**
 * End a test: close the app's connection, so it doesn't reconnect and start
 * another server; stop each home's server; then close the app. On Linux the
 * app closes only once no server it started holds its output (see
 * `closeApp`), so the server stops first. Without the first step the app
 * started a new server at once, and `koda serve --stop` waited its full 15
 * seconds for the socket to go, in every test.
 */
export async function shutDown(open: OpenApp | undefined, homes: string[]): Promise<void> {
  if (open) {
    await open.app
      .evaluate(() => (globalThis as { kodaTestConnection?: { close(): void } }).kodaTestConnection?.close())
      .catch(() => {});
  }
  // A stop that fails is said once the app is closed, and only if the server
  // is still there: the scratch home is removed after this.
  const failed = new Map<string, Error>();
  const stop = (home: string) => {
    try {
      stopServer(home);
      failed.delete(home);
    } catch (error) {
      failed.set(home, error as Error);
    }
  };
  for (const home of homes) stop(home);
  if (open) await closeApp(open);
  // A server a reconnect started before the connection closed: stop it too.
  for (const home of homes) if (existsSync(socketOf(home))) stop(home);
  for (const [home, error] of failed) if (existsSync(socketOf(home))) throw error;
}
