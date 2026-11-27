// The login shell's PATH, for an app opened from the Finder.
import { describe, expect, it } from 'vitest';
import { between, loginShellPath, mergePaths } from '../src/main/shell-path';
import { scratch, script } from './helpers';

const START = '__KODA_LOGIN_PATH_START__';
const END = '__KODA_LOGIN_PATH_END__';

describe('loginShellPath', () => {
  it('reads the PATH between the markers, past whatever a profile prints', () => {
    expect(between(`Welcome!\n${START}/opt/homebrew/bin:/usr/bin${END}`)).toBe('/opt/homebrew/bin:/usr/bin');
    // A profile that echoes the command line: the last start marker counts.
    expect(between(`${START}junk ${START}/a:/b${END}`)).toBe('/a:/b');
    expect(between('no markers')).toBeNull();
    expect(between(`${START}${END}`)).toBeNull();
  });

  it("puts the shell's entries first and keeps the app's own, without repeats", () => {
    expect(mergePaths('/opt/homebrew/bin:/usr/bin', '/usr/bin:/bin::/sbin')).toBe('/opt/homebrew/bin:/usr/bin:/bin:/sbin');
  });

  it("runs the user's shell as a login shell and merges what it says", async () => {
    // A shell whose profile prints a banner, then runs the probe it was given.
    const shell = script(scratch(), 'shell', 'echo "Last login: today"\nPATH=/from/profile:/usr/bin; export PATH\nshift; eval "$1"');
    const path = await loginShellPath({ SHELL: shell, PATH: '/usr/bin:/bin' });
    expect(path).toBe('/from/profile:/usr/bin:/bin');
  });

  it('falls back to a plain login shell when -ilc gives nothing', async () => {
    const shell = script(scratch(), 'shell', '[ "$1" = "-ilc" ] && exit 1\nPATH=/plain:/usr/bin; export PATH\nshift; eval "$1"');
    expect(await loginShellPath({ SHELL: shell, PATH: '/bin' })).toBe('/plain:/usr/bin:/bin');
  });

  it("keeps the app's PATH when the shell hangs or fails", async () => {
    const hangs = script(scratch(), 'hangs', 'sleep 30');
    expect(await loginShellPath({ SHELL: hangs, PATH: '/usr/bin:/bin' }, 200)).toBe('/usr/bin:/bin');
    expect(await loginShellPath({ SHELL: '/nonexistent/shell', PATH: '/bin' })).toBe('/bin');
  });
});
