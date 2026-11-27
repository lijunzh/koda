// The server from the app's side: where it is, which koda starts it, and
// what `koda serve --start` answered.
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import { compareVersions, defaultSocket, findKoda, startServer } from '../src/main/server';
import { scratch, script } from './helpers';

describe('server', () => {
  it("finds Koda's socket as koda does", () => {
    expect(defaultSocket({ HOME: '/Users/me' })).toBe('/Users/me/.config/koda/serve/koda.sock');
    expect(defaultSocket({ HOME: '/Users/me', XDG_CONFIG_HOME: '/x' })).toBe('/x/koda/serve/koda.sock');
  });

  it('compares versions as koda_client::version does', () => {
    expect(compareVersions('0.7.13', '0.7.13')).toBe('same');
    expect(compareVersions('0.7.12', '0.7.13')).toBe('older');
    expect(compareVersions('0.7.14', '0.7.13')).toBe('newer');
    expect(compareVersions('0.10.0', '0.9.9')).toBe('newer');
    expect(compareVersions('1.0.0', '0.99.0')).toBe('newer');
    // Another build of the same release is replaced, never kept.
    expect(compareVersions('0.7.13-dev', '0.7.13')).toBe('older');
    expect(compareVersions('0.7.13', '0.7.13+local')).toBe('older');
    expect(compareVersions('0.8.0-rc.1', '0.7.13')).toBe('newer');
    // What can't be read is replaced.
    expect(compareVersions('koda', '0.7.13')).toBe('older');
    expect(compareVersions('0.7', '0.7.13')).toBe('older');
  });

  it('uses KODA_BIN, then the koda inside the app, then the first on PATH', () => {
    const dir = scratch();
    const onPath = script(dir, 'koda', 'exit 0');
    const bundledDir = scratch();
    const bundled = script(bundledDir, 'koda', 'exit 0');
    expect(findKoda({ KODA_BIN: '/x/koda' }, dir, bundled)).toBe('/x/koda');
    expect(findKoda({}, `/nowhere:${dir}`, bundled)).toBe(bundled);
    expect(findKoda({}, `/nowhere:${dir}`, path.join(bundledDir, 'missing'))).toBe(onPath);
    expect(findKoda({}, '/nowhere')).toBeNull();
  });

  it("reads koda serve --start's JSON, whatever the exit code, and passes its options", async () => {
    const dir = scratch();
    const args = path.join(dir, 'args');
    const koda = script(
      dir,
      'koda',
      `echo "$@" > ${args}\necho '{"status":"busy","serverVersion":"0.0.1","working":2,"draining":false}'\nexit 1`,
    );
    const answer = await startServer({ koda, env: { HOME: '/home/me', PATH: '/bin' }, socket: '/s.sock', runningTurns: 'stop' });
    expect(answer).toMatchObject({ status: 'busy', working: 2 });
    const { readFileSync } = await import('node:fs');
    expect(readFileSync(args, 'utf8').trim()).toBe(
      'serve --start --project-root /home/me --listen unix:///s.sock --running-turns stop',
    );
  });

  it('says what went wrong when koda gives no answer', async () => {
    const koda = script(scratch(), 'koda', 'echo "error: unexpected argument" >&2\nexit 2');
    const answer = await startServer({ koda, env: { PATH: '/bin' } });
    expect(answer.status).toBe('failed');
    expect(answer.message).toMatch(/unexpected argument/);
  });
});
