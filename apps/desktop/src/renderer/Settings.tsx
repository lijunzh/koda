// Settings, as a window over the app, as Claude Code's desktop app has
// them, with the sections Koda has something behind: Appearance (the theme,
// the conversation's width), Conversation (the view the app opens in), the
// desktop app itself (notifications, keeping the computer awake, versions)
// and the keyboard shortcuts.
import { type ReactNode, useEffect, useState } from 'react';
import type { DefaultView, Prefs, Theme, Width } from './prefs';

interface Props {
  prefs: Prefs;
  onPrefs(prefs: Prefs): void;
  /** The app's and the server's versions, when connected. */
  appVersion: string | null;
  version: string | null;
  onClose(): void;
}

type Section = 'appearance' | 'conversation' | 'app' | 'shortcuts';

const SECTIONS: Array<{ id: Section; label: string }> = [
  { id: 'appearance', label: 'Appearance' },
  { id: 'conversation', label: 'Conversation' },
  { id: 'app', label: 'Desktop app' },
  { id: 'shortcuts', label: 'Keyboard shortcuts' },
];

const THEMES: Array<{ value: Theme; label: string; icon: string }> = [
  { value: 'system', label: 'System', icon: '▭' },
  { value: 'light', label: 'Light', icon: '☀' },
  { value: 'dark', label: 'Dark', icon: '☾' },
];
const WIDTHS: Array<{ value: Width; label: string }> = [
  { value: 'narrow', label: 'Narrow' },
  { value: 'wide', label: 'Wide' },
];
const VIEWS: Array<{ value: DefaultView; label: string }> = [
  { value: 'normal', label: 'Normal' },
  { value: 'thinking', label: 'Thinking' },
  { value: 'verbose', label: 'Verbose' },
];

/** The app's shortcuts, as its manual lists them. */
const SHORTCUTS: Array<[string, string]> = [
  ['⌘N', 'New session'],
  ['⌘,', 'Settings'],
  ['⌘B', 'Hide or show the sidebar'],
  ['⌘[  ⌘]', 'Back and Forward'],
  ['Enter', 'Send, or add to the running turn'],
  ['Shift Enter', 'A new line in the prompt'],
  ['Esc', 'Stop the turn; close a menu'],
  ['⌘⇧I', 'The model menu'],
  ['⌘⇧M', 'The mode menu'],
  ['Ctrl O', 'Cycle Normal, Thinking, Verbose'],
  ['Ctrl T', 'Hide or show the plan'],
  ['⌘.', 'Hide or show the side panel'],
  ['⌘⇧D', 'Open Changes in the side panel'],
  ['⌥ click', 'Open a session in split view'],
  ['⌘⌃W', 'Take the session out of split view'],
  ['⌘↵', 'Send review comments; submit an answer'],
];

function Setting({ label, description, children }: { label: string; description?: string; children: ReactNode }) {
  return (
    <div className="setting">
      <span>
        {label}
        {description && <span className="setting-description">{description}</span>}
      </span>
      {children}
    </div>
  );
}

function Choice<T extends string>({
  label,
  value,
  choices,
  onPick,
}: {
  label: string;
  value: T;
  choices: Array<{ value: T; label: string; icon?: string }>;
  onPick(value: T): void;
}) {
  return (
    <div className="views" role="group" aria-label={label}>
      {choices.map((c) => (
        <button
          key={c.value}
          aria-label={c.icon ? c.label : undefined}
          title={c.icon ? c.label : undefined}
          aria-pressed={value === c.value}
          onClick={() => onPick(c.value)}
        >
          {c.icon ? <span aria-hidden>{c.icon}</span> : c.label}
        </button>
      ))}
    </div>
  );
}

function Switch({ label, on, onToggle }: { label: string; on: boolean; onToggle(on: boolean): void }) {
  return (
    <button className="switch" role="switch" aria-checked={on} aria-label={label} onClick={() => onToggle(!on)}>
      <span />
    </button>
  );
}

export function Settings({ prefs, onPrefs, appVersion, version, onClose }: Props) {
  const [section, setSection] = useState<Section>('appearance');
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="settings" role="dialog" aria-modal="true" aria-label="Settings" data-testid="settings">
        <header>
          <strong>Settings</strong>
          <button className="icon-button" aria-label="Close settings" onClick={onClose}>
            ✕
          </button>
        </header>
        <div className="settings-body">
          <nav aria-label="Settings sections">
            {SECTIONS.map((s) => (
              <button
                key={s.id}
                className={s.id === section ? 'session selected' : 'session'}
                aria-current={s.id === section ? 'true' : undefined}
                onClick={() => setSection(s.id)}
              >
                {s.label}
              </button>
            ))}
          </nav>
          {section === 'appearance' && (
            <section>
              <h2>Appearance</h2>
              <Setting label="Theme">
                <Choice label="Theme" value={prefs.theme} choices={THEMES} onPick={(theme) => onPrefs({ ...prefs, theme })} />
              </Setting>
              <Setting label="Transcript width">
                <Choice label="Transcript width" value={prefs.width} choices={WIDTHS} onPick={(width) => onPrefs({ ...prefs, width })} />
              </Setting>
              <p className="meta">Motion follows your system's Reduce motion setting.</p>
            </section>
          )}
          {section === 'conversation' && (
            <section>
              <h2>Conversation</h2>
              <Setting label="Default transcript view" description="How much detail the app shows when it opens. Ctrl+O changes it at any time.">
                <Choice label="Default transcript view" value={prefs.view} choices={VIEWS} onPick={(view) => onPrefs({ ...prefs, view })} />
              </Setting>
            </section>
          )}
          {section === 'app' && (
            <section>
              <h2>Desktop app</h2>
              <Setting label="Notifications" description="Say when Koda needs you or has finished, while the app isn't in front.">
                <Switch label="Notifications" on={prefs.notifications} onToggle={(notifications) => onPrefs({ ...prefs, notifications })} />
              </Setting>
              <Setting
                label="Keep computer awake"
                description="Keep the computer from idle-sleeping while the app is open, so a long turn can finish. The display can still turn off."
              >
                <Switch label="Keep computer awake" on={prefs.keepAwake} onToggle={(keepAwake) => onPrefs({ ...prefs, keepAwake })} />
              </Setting>
              <Setting label="Desktop app version">
                <span className="meta">{appVersion ?? '—'}</span>
              </Setting>
              <Setting label="Server version" description="The app and Koda's background server always run one version.">
                <span className="meta">{version ?? 'not connected'}</span>
              </Setting>
            </section>
          )}
          {section === 'shortcuts' && (
            <section>
              <h2>Keyboard shortcuts</h2>
              <dl className="shortcuts">
                {SHORTCUTS.map(([keys, what]) => (
                  <div key={keys} className="setting">
                    <dt>{what}</dt>
                    <dd>
                      <kbd>{keys}</kbd>
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          )}
        </div>
      </div>
    </div>
  );
}
