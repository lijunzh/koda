// The app starts Koda's server, or joins it, and follows the version rule,
// driven as a user would, against a real `koda serve`.
import { chmodSync, existsSync, mkdirSync, readFileSync, rmSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { KodaClient } from '../../src/main/koda-client';
import { scratch } from '../helpers';
import { type OpenApp, koda, openApp, socketOf, stopServer, shutDown } from './app';

const appVersion = (JSON.parse(readFileSync(path.join(__dirname, '../../package.json'), 'utf8')) as { version: string })
  .version;

let open: OpenApp | undefined;
const homes: string[] = [];

afterEach(async () => {
  // The server first: on Linux, a server the app started holds one of the
  // app's output pipes, and Playwright counts the app closed only once they
  // all are (the app itself has quit by then).
  await shutDown(open, homes.splice(0));
  open = undefined;
});

function home(): string {
  const folder = scratch();
  homes.push(folder);
  return folder;
}

/**
 * A server of an older Koda on `socket`, with a turn running in one session:
 * asked only if idle, it says so; told to stop, it stops.
 */
function olderServer(socket: string): Promise<net.Server> {
  mkdirSync(path.dirname(socket), { recursive: true });
  chmodSync(path.dirname(socket), 0o700);
  const server = net.createServer((connection) => {
    let buffer = '';
    connection.setEncoding('utf8');
    connection.on('data', (chunk: string) => {
      buffer += chunk;
      let end;
      while ((end = buffer.indexOf('\n')) >= 0) {
        const request = JSON.parse(buffer.slice(0, end));
        buffer = buffer.slice(end + 1);
        const answer = (result: object) =>
          connection.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }) + '\n');
        if (request.method === 'initialize') {
          answer({ serverInfo: { name: 'koda', version: '0.0.1' } });
        } else if (request.method === 'serve/stop' && request.params?.ifIdle) {
          answer({ stopped: false, working: 1, reason: '1 session(s) working' });
        } else if (request.method === 'serve/stop') {
          answer({ stopped: true, working: 1 });
          server.close();
          rmSync(socket, { force: true });
        }
      }
    });
  });
  return new Promise((resolve) => server.listen(socket, () => resolve(server)));
}

describe.skipIf(!koda)('the app and the server', () => {
  it('starts the server, and starts it again when it goes away', async () => {
    const h = home();
    open = await openApp(h);
    const connected = open.page.getByTestId('connected');
    await connected.waitFor({ timeout: 30_000 });
    expect(await connected.textContent()).toContain(`Connected to Koda ${appVersion}`);
    expect(existsSync(socketOf(h))).toBe(true);

    stopServer(h);
    // The window reconnects, starting a server again, as the TUI does.
    await open.page.getByTestId('reconnecting').waitFor({ timeout: 10_000 }).catch(() => {});
    await connected.waitFor({ timeout: 30_000 });
    expect(existsSync(socketOf(h))).toBe(true);
  });

  it("asks about an older server's running turns, and replaces it when told to", async () => {
    const h = home();
    const older = await olderServer(socketOf(h));
    open = await openApp(h);
    const busy = open.page.getByTestId('busy');
    await busy.waitFor({ timeout: 30_000 });
    expect(await busy.textContent()).toContain('turn running in 1 session');
    await open.page.getByRole('button', { name: /Restart it now/ }).click();
    await open.page.getByTestId('connected').waitFor({ timeout: 30_000 });
    expect(older.listening).toBe(false);
  });

  it("says Koda was updated when a newer Koda replaces its server, and doesn't start its own", async () => {
    const h = home();
    open = await openApp(h);
    await open.page.getByTestId('connected').waitFor({ timeout: 30_000 });

    // A newer Koda asks the server to make way, as after `brew upgrade`.
    const newer = new KodaClient();
    await newer.connect(socketOf(h));
    await newer.request('initialize', { clientInfo: { name: 'newer-koda', version: '99.0.0' } });
    await newer.request('serve/stop', { ifIdle: true, forUpdate: '99.0.0' });
    newer.close();

    const updated = open.page.getByTestId('updated');
    await updated.waitFor({ timeout: 30_000 });
    expect(await updated.textContent()).toContain('Koda was updated to 99.0.0');
    expect(await open.page.getByRole('button', { name: 'Restart now' }).isVisible()).toBe(true);
    // The old window doesn't bring back a server of its own version.
    await new Promise((r) => setTimeout(r, 3000));
    expect(existsSync(socketOf(h))).toBe(false);
  });
});
