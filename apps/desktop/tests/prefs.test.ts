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
    savePrefs(s, { theme: 'dark', width: 'wide' });
    expect(loadPrefs(s)).toEqual({ theme: 'dark', width: 'wide' });
  });

  it('puts a chosen theme on the document, and takes it off for the system\'s', () => {
    const root = { dataset: {} as Record<string, string> } as unknown as HTMLElement;
    applyPrefs(root, { theme: 'dark', width: 'wide' });
    expect(root.dataset).toEqual({ theme: 'dark', width: 'wide' });
    applyPrefs(root, { theme: 'system', width: 'narrow' });
    expect(root.dataset).toEqual({ width: 'narrow' });
  });
});
