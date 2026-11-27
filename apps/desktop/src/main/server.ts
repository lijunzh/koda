// Koda's background server, from the app's side: where it is, which `koda`
// starts it, and `koda serve --start`, which makes sure one of the app's
// version answers there (see "The background server" in Koda's docs). The
// app follows the same rule as the TUI because it runs the same code.
import { execFile } from 'node:child_process';
import { accessSync, constants } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/** Koda's socket, as `koda` finds it: `$XDG_CONFIG_HOME/koda/serve/koda.sock`, else under `~/.config`. */
export function defaultSocket(env: NodeJS.ProcessEnv = process.env): string {
  const config = env.XDG_CONFIG_HOME || path.join(env.HOME || os.homedir(), '.config');
  return path.join(config, 'koda', 'serve', 'koda.sock');
}

/**
 * The `koda` that starts the server: `KODA_BIN` if set, else the one inside
 * the app once it carries one, else the first on `pathEnv`. `null` if none.
 */
export function findKoda(env: NodeJS.ProcessEnv, pathEnv: string, bundled?: string): string | null {
  const executable = (file: string) => {
    try {
      accessSync(file, constants.X_OK);
      return true;
    } catch {
      return false;
    }
  };
  if (env.KODA_BIN) return env.KODA_BIN;
  if (bundled && executable(bundled)) return bundled;
  for (const dir of pathEnv.split(':')) {
    if (!dir) continue;
    const file = path.join(dir, 'koda');
    if (executable(file)) return file;
  }
  return null;
}

/** How the server's version stands against the app's. Koda runs one version. */
export type VersionMatch = 'same' | 'older' | 'newer';

/** Compare by release number, as `koda_client::version::compare` does. */
export function compareVersions(server: string, app: string): VersionMatch {
  if (server === app) return 'same';
  const release = (v: string): [number, number, number] | null => {
    const core = v.trim().split(/[-+]/)[0];
    const parts = core.split('.');
    if (parts.length !== 3 || !parts.every((p) => /^\d+$/.test(p))) return null;
    return [Number(parts[0]), Number(parts[1]), Number(parts[2])];
  };
  const s = release(server);
  const a = release(app);
  if (s && a) {
    for (let i = 0; i < 3; i++) {
      if (s[i] !== a[i]) return s[i] > a[i] ? 'newer' : 'older';
    }
  }
  // Another build of the same release, or a version that can't be read.
  return 'older';
}

/** `koda serve --start`'s answer: one JSON object (see `koda serve --start` in Koda's docs). */
export interface StartAnswer {
  status:
    | 'alreadyRunning'
    | 'started'
    | 'restarted'
    | 'busy'
    | 'newerServer'
    | 'predatesUpdate'
    | 'failed';
  socketPath?: string;
  cliVersion?: string;
  serverVersion?: string;
  message?: string;
  working?: number;
  draining?: boolean;
}

/** Whether the answer means a server of `koda`'s version is up. */
export function ready(answer: StartAnswer): boolean {
  return answer.status === 'alreadyRunning' || answer.status === 'started' || answer.status === 'restarted';
}

/** What to do with an older server's running turns: `wait` for them or `stop` them. */
export type RunningTurns = 'wait' | 'stop';

/**
 * Run `koda serve --start` and read its answer. The server it starts keeps
 * `env`, so this is where the login shell's PATH reaches it.
 */
export function startServer(opts: {
  koda: string;
  env: NodeJS.ProcessEnv;
  socket?: string;
  runningTurns?: RunningTurns;
}): Promise<StartAnswer> {
  const args = ['serve', '--start', '--project-root', opts.env.HOME || os.homedir()];
  if (opts.socket) args.push('--listen', `unix://${opts.socket}`);
  if (opts.runningTurns) args.push('--running-turns', opts.runningTurns);
  return new Promise((resolve) => {
    // No timeout when waiting for an older server's turns: that takes as long as they do.
    const timeout = opts.runningTurns === 'wait' ? 0 : 60_000;
    execFile(opts.koda, args, { env: opts.env, timeout, encoding: 'utf8' }, (error, stdout, stderr) => {
      const line = stdout.trim().split('\n').pop() ?? '';
      try {
        const answer = JSON.parse(line) as StartAnswer;
        if (typeof answer.status === 'string') {
          resolve(answer);
          return;
        }
      } catch {
        // Not the JSON answer: said below.
      }
      const why = stderr.trim() || error?.message || 'no answer';
      resolve({ status: 'failed', message: `${opts.koda} serve --start: ${why}` });
    });
  });
}

/** What the window shows about the server. */
export type ServerStatus =
  | { state: 'starting' }
  | { state: 'connected'; socket: string; serverVersion: string; appVersion: string }
  | { state: 'reconnecting'; error: string }
  | {
      state: 'busy';
      serverVersion: string;
      appVersion: string;
      working: number;
      draining: boolean;
    }
  | { state: 'waiting'; working: number }
  | { state: 'updated'; version: string; appVersion: string }
  | { state: 'noKoda' }
  | { state: 'unreachable'; error: string };
