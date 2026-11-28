// The two settings the main process acts on, sent by the page whenever they
// change: whether to send system notifications, and whether to keep the
// computer from idle-sleeping while the app is open. The page keeps them
// (with the rest of Settings); the main process only checks their shape.

export interface AppPrefs {
  notifications: boolean;
  keepAwake: boolean;
}

export const DEFAULT_APP_PREFS: AppPrefs = { notifications: true, keepAwake: false };

/** What the page sent, as settings; anything else is refused. */
export function appPrefs(sent: unknown): AppPrefs {
  const o = (sent ?? {}) as Record<string, unknown>;
  const extra = Object.keys(o).filter((key) => key !== 'notifications' && key !== 'keepAwake');
  if (extra.length) throw new Error(`refused: settings are notifications and keepAwake, not ${extra.join(', ')}`);
  if (typeof o.notifications !== 'boolean' || typeof o.keepAwake !== 'boolean') {
    throw new Error('refused: notifications and keepAwake are on or off');
  }
  return { notifications: o.notifications, keepAwake: o.keepAwake };
}

/** Starts and stops what keeps the computer awake, so it is held at most once. */
export class KeepAwake {
  private held: number | null = null;
  constructor(private readonly blocker: { start(type: 'prevent-app-suspension'): number; stop(id: number): void }) {}

  set(on: boolean) {
    if (on && this.held === null) this.held = this.blocker.start('prevent-app-suspension');
    if (!on && this.held !== null) {
      this.blocker.stop(this.held);
      this.held = null;
    }
  }
}
