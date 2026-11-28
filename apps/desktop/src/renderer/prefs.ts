// What the user set in Settings, kept in the page's own storage. The theme
// and the conversation's width are applied as attributes on the document,
// which the stylesheet reads; notifications and keeping the computer awake
// are the main process's to act on, and are sent to it.

export type Theme = 'system' | 'light' | 'dark';
export type Width = 'narrow' | 'wide';

export type DefaultView = 'normal' | 'thinking' | 'verbose';

export interface Prefs {
  theme: Theme;
  width: Width;
  /** The transcript view the app opens in. */
  view: DefaultView;
  /** System notifications while the app isn't in front. */
  notifications: boolean;
  /** Keep the computer from idle-sleeping while the app is open. */
  keepAwake: boolean;
  /** Whether the session list shows (Cmd+B); kept as the user left it. */
  sidebar: boolean;
}

export const DEFAULT_PREFS: Prefs = { theme: 'system', width: 'narrow', view: 'normal', notifications: true, keepAwake: false, sidebar: true };
const KEY = 'koda.prefs';

/** The stored settings; anything missing or unreadable is the default. */
export function loadPrefs(storage: Pick<Storage, 'getItem'>): Prefs {
  try {
    const stored = JSON.parse(storage.getItem(KEY) ?? '{}') as Partial<Prefs>;
    return {
      theme: stored.theme === 'light' || stored.theme === 'dark' ? stored.theme : 'system',
      width: stored.width === 'wide' ? 'wide' : 'narrow',
      view: stored.view === 'thinking' || stored.view === 'verbose' ? stored.view : 'normal',
      notifications: stored.notifications !== false,
      keepAwake: stored.keepAwake === true,
      sidebar: stored.sidebar !== false,
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
