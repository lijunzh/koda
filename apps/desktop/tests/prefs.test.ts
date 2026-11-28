// The settings kept in the page's storage, and how they reach the stylesheet.
import { describe, expect, it } from 'vitest';
import { applyPrefs, DEFAULT_PREFS, loadPrefs, savePrefs } from '../src/renderer/prefs';

function storage(initial: Record<string, string> = {}) {
  const data = { ...initial };
  return { data, getItem: (k: string) => data[k] ?? null, setItem: (k: string, v: string) => void (data[k] = v) };
}

describe('prefs', () => {
  it('follows the system and runs narrow until the user says otherwise', () => {
    expect(loadPrefs(storage())).toEqual(DEFAULT_PREFS);
    expect(loadPrefs(storage({ 'koda.prefs': 'not json' }))).toEqual(DEFAULT_PREFS);
    expect(loadPrefs(storage({ 'koda.prefs': '{"theme":"pink","width":7}' }))).toEqual(DEFAULT_PREFS);
  });

  it('remembers what was set', () => {
    const s = storage();
    const set = { theme: 'dark', width: 'wide', view: 'verbose', notifications: false, keepAwake: true, sidebar: false } as const;
    savePrefs(s, set);
    expect(loadPrefs(s)).toEqual(set);
  });

  it('puts a chosen theme on the document, and takes it off for the system\'s', () => {
    const root = { dataset: {} as Record<string, string> } as unknown as HTMLElement;
    applyPrefs(root, { ...DEFAULT_PREFS, theme: 'dark', width: 'wide' });
    expect(root.dataset).toEqual({ theme: 'dark', width: 'wide' });
    applyPrefs(root, DEFAULT_PREFS);
    expect(root.dataset).toEqual({ width: 'narrow' });
  });
});

import { appPrefs, KeepAwake } from '../src/main/app-prefs';

describe('what the main process takes from Settings', () => {
  it('takes the two switches, and nothing else', () => {
    expect(appPrefs({ notifications: false, keepAwake: true })).toEqual({ notifications: false, keepAwake: true });
    expect(() => appPrefs({ notifications: 'no', keepAwake: true })).toThrow(/on or off/);
    expect(() => appPrefs({ notifications: true, keepAwake: true, shell: 'rm' })).toThrow(/not shell/);
    expect(() => appPrefs(null)).toThrow(/on or off/);
  });

  it('holds the computer awake once, and lets go once', () => {
    const calls: string[] = [];
    const awake = new KeepAwake({ start: () => (calls.push('start'), 7), stop: (id) => void calls.push(`stop ${id}`) });
    awake.set(true);
    awake.set(true);
    awake.set(false);
    awake.set(false);
    expect(calls).toEqual(['start', 'stop 7']);
  });
});
