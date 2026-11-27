// Opening the real app against a scratch Koda home, for the end-to-end tests.
import { execFileSync } from 'node:child_process';
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
  const args = [appDir];
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

/** `koda serve --stop` for the server under `home`, if one runs. */
export function stopServer(home: string) {
  try {
    execFileSync(koda!, ['serve', '--stop'], { env: { ...process.env, XDG_CONFIG_HOME: home }, stdio: 'ignore' });
  } catch {
    // None was running.
  }
}

/** The socket of the server under `home`. */
export const socketOf = (home: string) => path.join(home, 'koda', 'serve', 'koda.sock');

/** Quit the app; if it hasn't gone within 10 seconds, kill it, so a hang costs one test, not the run. */
export async function closeApp({ app }: OpenApp): Promise<void> {
  let timer: NodeJS.Timeout | undefined;
  const late = new Promise<'late'>((resolve) => (timer = setTimeout(() => resolve('late'), 10_000)));
  const closed = await Promise.race([app.close().then(() => 'closed' as const), late]);
  clearTimeout(timer);
  if (closed === 'late') app.process().kill('SIGKILL');
}
