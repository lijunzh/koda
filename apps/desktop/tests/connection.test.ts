// The app's connection to the server through its life: started or joined,
// an older server replaced by the user's choice, an update past the app,
// and reconnecting when the server goes away.
import { describe, expect, it } from 'vitest';
import { Connection, type ConnectionDeps } from '../src/main/connection';
import type { RunningTurns, ServerStatus, StartAnswer } from '../src/main/server';
import { kodaServe } from './helpers';

const VERSION = '0.8.1';

/** A connection whose `koda serve --start` answers from `answers`, in turn, recording the choices it was given. */
function connection(answers: Array<StartAnswer | (() => Promise<StartAnswer>)>, extra: Partial<ConnectionDeps> = {}) {
  const asked: Array<RunningTurns | undefined> = [];
  const statuses: ServerStatus[] = [];
  const conn = new Connection({
    appVersion: VERSION,
    koda: '/bin/koda',
    env: {},
    sleep: async () => {},
    start: async (opts) => {
      asked.push(opts.runningTurns);
      const next = answers.shift();
      if (!next) return { status: 'failed', message: 'no more answers' };
      return typeof next === 'function' ? next() : next;
    },
    ...extra,
  });
  conn.on('status', (s: ServerStatus) => statuses.push(s));
  return { conn, asked, statuses };
}

const until = async (check: () => boolean) => {
  for (let i = 0; i < 200 && !check(); i++) await new Promise((r) => setTimeout(r, 10));
  expect(check()).toBe(true);
};

describe('Connection', () => {
  it('joins the server koda serve --start reports', async () => {
    const server = await kodaServe(VERSION);
    const { conn } = connection([{ status: 'started', socketPath: server.socket }]);
    await conn.open();
    expect(conn.status).toMatchObject({ state: 'connected', serverVersion: VERSION, socket: server.socket });
    conn.close();
  });

  it('says no koda was found', async () => {
    const { conn } = connection([], { koda: null });
    await conn.open();
    expect(conn.status.state).toBe('noKoda');
  });

  it("asks about an older server's running turns, then replaces it as the user chose", async () => {
    const server = await kodaServe(VERSION);
    const { conn, asked, statuses } = connection([
      { status: 'busy', serverVersion: '0.8.0', working: 2, draining: false },
      { status: 'restarted', socketPath: server.socket },
    ]);
    await conn.open();
    expect(conn.status).toEqual({ state: 'busy', serverVersion: '0.8.0', appVersion: VERSION, working: 2, draining: false });
    await conn.open('wait');
    expect(asked).toEqual([undefined, 'wait']);
    expect(statuses.map((s) => s.state)).toContain('waiting');
    expect(conn.status.state).toBe('connected');
    conn.close();
  });

  it('stops at a newer server: Koda was updated past this app', async () => {
    const { conn } = connection([{ status: 'newerServer', serverVersion: '0.9.0' }]);
    await conn.open();
    expect(conn.status).toEqual({ state: 'updated', version: '0.9.0', appVersion: VERSION });
  });

  it("says why when koda can't start one", async () => {
    const { conn } = connection([{ status: 'failed', message: "koda serve didn't start" }]);
    await conn.open();
    expect(conn.status).toEqual({ state: 'unreachable', error: "koda serve didn't start" });
  });

  it('gets back after the server goes away, starting a new one', async () => {
    const first = await kodaServe(VERSION);
    const second = await kodaServe(VERSION);
    const { conn, asked } = connection([
      { status: 'started', socketPath: first.socket },
      { status: 'started', socketPath: second.socket },
    ]);
    await conn.open();
    first.stop();
    await until(() => conn.status.state === 'connected' && 'socket' in conn.status && conn.status.socket === second.socket);
    expect(asked).toEqual([undefined, undefined]);
    conn.close();
  });

  it('gives up reconnecting when its time runs out', async () => {
    const first = await kodaServe(VERSION);
    let now = 0;
    const { conn } = connection(
      [
        { status: 'started', socketPath: first.socket },
        { status: 'failed', message: 'nothing answers' },
        { status: 'failed', message: 'nothing answers' },
      ],
      { reconnectBudgetMs: 2500, sleep: async (ms) => void (now += ms) },
    );
    const realNow = Date.now;
    Date.now = () => realNow() + now;
    try {
      await conn.open();
      first.stop();
      await until(() => conn.status.state === 'unreachable');
      expect(conn.status).toEqual({ state: 'unreachable', error: 'nothing answers' });
    } finally {
      Date.now = realNow;
    }
  });

  it('after serve/updating for a newer Koda, says so and does not reconnect', async () => {
    const server = await kodaServe(VERSION);
    const { conn, asked } = connection([{ status: 'alreadyRunning', socketPath: server.socket }]);
    const seen: unknown[] = [];
    conn.on('messages', (batch: unknown[]) => seen.push(...batch));
    await conn.open();
    server.broadcast({ jsonrpc: '2.0', method: 'serve/updating', params: { version: '0.9.0', working: 0 } });
    await until(() => conn.status.state === 'updated');
    expect(seen).toHaveLength(1);
    server.stop();
    await new Promise((r) => setTimeout(r, 50));
    expect(conn.status.state).toBe('updated');
    expect(asked).toHaveLength(1);
  });

  it('refuses requests while not connected', async () => {
    const { conn } = connection([{ status: 'failed', message: 'x' }]);
    await conn.open();
    await expect(conn.request('session/list', {})).rejects.toThrow(/not connected/);
    expect(conn.openRequest(1)).toBeUndefined();
  });
});
