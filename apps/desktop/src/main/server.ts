// Where Koda's background server is, and whether it runs this app's version.
// Starting it with `koda serve --start` comes in the next step; for now the
// app connects to one that is running.
import os from 'node:os';
import path from 'node:path';

/** Koda's socket, as `koda` finds it: `$XDG_CONFIG_HOME/koda/serve/koda.sock`, else under `~/.config`. */
export function defaultSocket(env: NodeJS.ProcessEnv = process.env): string {
  const config = env.XDG_CONFIG_HOME || path.join(env.HOME || os.homedir(), '.config');
  return path.join(config, 'koda', 'serve', 'koda.sock');
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

/** What the window shows about the server. */
export type ServerStatus =
  | { state: 'connecting'; socket: string }
  | { state: 'connected'; socket: string; serverVersion: string; appVersion: string; match: VersionMatch }
  | { state: 'unreachable'; socket: string; error: string };
