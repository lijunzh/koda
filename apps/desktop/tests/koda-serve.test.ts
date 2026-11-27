// The app's client against a real `koda serve`, started with
// `koda serve --start` on a scratch config folder (never the user's own
// server). Runs when KODA_BIN names a koda binary built from this checkout:
//   KODA_BIN=../../target/debug/koda npm test
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterAll, describe, expect, it } from 'vitest';
import { KodaClient } from '../src/main/koda-client';
import { refusal } from '../src/main/relay';
import type { InitializeResponse, SessionListResponse, StateReadResponse } from '../src/protocol/types.gen';

const koda = process.env.KODA_BIN;
const appVersion = (JSON.parse(readFileSync(path.join(__dirname, '../package.json'), 'utf8')) as { version: string })
  .version;
// Short, so the socket's path stays under the 104 bytes macOS allows.
const home = mkdtempSync(path.join(os.tmpdir(), 'kd-'));
const env = { ...process.env, XDG_CONFIG_HOME: home };

afterAll(() => {
  if (koda) {
    try {
      execFileSync(koda, ['serve', '--stop'], { env, stdio: 'ignore' });
    } catch {
      // None was running.
    }
  }
  rmSync(home, { recursive: true, force: true });
});

describe.skipIf(!koda)('against koda serve', () => {
  it('starts a server of its version and reads it through the methods the page may use', async () => {
    const started = JSON.parse(
      execFileSync(koda!, ['serve', '--start', '--project-root', home], { env, encoding: 'utf8' }),
    ) as { status: string; socketPath: string; serverVersion: string };
    expect(started.status).toBe('started');
    expect(started.serverVersion).toBe(appVersion);

    const client = new KodaClient();
    await client.connect(started.socketPath);
    const init = await client.request<InitializeResponse>('initialize', {
      clientInfo: { name: 'koda-desktop-test', title: 'Koda', version: appVersion },
    });
    expect(init.serverInfo.version).toBe(appVersion);

    expect(refusal('state/read', {})).toBeNull();
    const state = await client.request<StateReadResponse>('state/read', {});
    expect(state).toHaveProperty('mode');

    expect(refusal('session/list', {})).toBeNull();
    const list = await client.request<SessionListResponse>('session/list', {});
    expect(list.data).toEqual([]);
    client.close();
  });
});
