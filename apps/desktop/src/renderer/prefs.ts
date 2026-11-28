// What the user set in Settings, kept in the page's own storage: the theme
// (the system's, or always light or dark) and how wide the conversation
// runs. Applied as attributes on the document, which the stylesheet reads.

export type Theme = 'system' | 'light' | 'dark';
export type Width = 'narrow' | 'wide';

export interface Prefs {
  theme: Theme;
  width: Width;
}

export const DEFAULT_PREFS: Prefs = { theme: 'system', width: 'narrow' };
const KEY = 'koda.prefs';

/** The stored settings; anything missing or unreadable is the default. */
export function loadPrefs(storage: Pick<Storage, 'getItem'>): Prefs {
  try {
    const stored = JSON.parse(storage.getItem(KEY) ?? '{}') as Partial<Prefs>;
    return {
      theme: stored.theme === 'light' || stored.theme === 'dark' ? stored.theme : 'system',
      width: stored.width === 'wide' ? 'wide' : 'narrow',
    };
  } catch {
    return DEFAULT_PREFS;
  }
}

export function savePrefs(storage: Pick<Storage, 'setItem'>, prefs: Prefs) {
  try {
    storage.setItem(KEY, JSON.stringify(prefs));
  } catch {
    // Storage is full or unavailable: the setting holds until the app closes.
  }
}

/** Put the settings on the document: `data-theme` unless it follows the system, and `data-width`. */
export function applyPrefs(root: HTMLElement, prefs: Prefs) {
  if (prefs.theme === 'system') delete root.dataset.theme;
  else root.dataset.theme = prefs.theme;
  root.dataset.width = prefs.width;
}
