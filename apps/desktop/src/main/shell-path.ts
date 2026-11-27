// The PATH a terminal would have. An app opened from the Finder or the Dock
// gets launchd's minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin), so neither
// `koda` from Homebrew nor what Koda's commands need (`cargo`, `gh`) would be
// found, and a server the app starts would keep that PATH. As Goose's and
// Hermes's desktop apps do (and VS Code), the user's shell runs once as a
// login shell to print its PATH. Only PATH: API keys come from Koda's own key
// store (`/key`), never from a shell's environment.
import { spawn } from 'node:child_process';

const START = '__KODA_LOGIN_PATH_START__';
const END = '__KODA_LOGIN_PATH_END__';
// printenv, not `echo $PATH`: fish keeps PATH as a list and would join it with spaces.
const PROBE = `printf '%s' '${START}'; printenv PATH; printf '%s' '${END}'`;
const ATTEMPT_MS = 5000;

/** The PATH between the markers; the last start marker, so a profile that echoes the command can't spoil it. */
export function between(stdout: string): string | null {
  const start = stdout.lastIndexOf(START);
  if (start < 0) return null;
  const from = start + START.length;
  const end = stdout.indexOf(END, from);
  if (end < 0) return null;
  return stdout.slice(from, end).trim() || null;
}

/** Entries of `first`, then those of `second` not already there; empty ones dropped. */
export function mergePaths(first: string, second: string): string {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const entry of [...first.split(':'), ...second.split(':')]) {
    if (entry && !seen.has(entry)) {
      seen.add(entry);
      out.push(entry);
    }
  }
  return out.join(':');
}

function probe(shell: string, flags: string[], timeoutMs: number): Promise<string | null> {
  return new Promise((resolve) => {
    let stdout = '';
    let settled = false;
    let timer: NodeJS.Timeout | undefined;
    const finish = (value: string | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(value);
    };
    let child: ReturnType<typeof spawn>;
    try {
      // Detached, in a session of its own, so an interactive shell's job
      // control can't take the terminal from the app; no stdin, so a profile
      // that reads it can't hang.
      child = spawn(shell, [...flags, PROBE], { stdio: ['ignore', 'pipe', 'ignore'], detached: true });
    } catch {
      finish(null);
      return;
    }
    timer = setTimeout(() => {
      child.kill('SIGKILL');
      finish(between(stdout));
    }, timeoutMs);
    child.stdout?.setEncoding('utf8');
    child.stdout?.on('data', (chunk: string) => (stdout += chunk));
    child.on('error', () => finish(null));
    // A profile may exit with an error after printing: trust the markers, not the exit code.
    child.on('close', () => finish(between(stdout)));
  });
}

/**
 * The login shell's PATH merged ahead of `env.PATH`, or `env.PATH` alone if
 * the shell can't say. An interactive login shell first (`-ilc`: profiles and
 * rc files, where Homebrew and version managers add themselves), then a plain
 * login shell, which some shells need without a terminal.
 */
export async function loginShellPath(
  env: NodeJS.ProcessEnv = process.env,
  timeoutMs = ATTEMPT_MS,
): Promise<string> {
  const current = env.PATH ?? '';
  const shell = env.SHELL || '/bin/zsh';
  for (const flags of [['-ilc'], ['-lc']]) {
    const found = await probe(shell, flags, timeoutMs);
    if (found) return mergePaths(found, current);
  }
  return current;
}
