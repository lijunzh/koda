// Settings, as a window over the app, as Claude Code's desktop app has
// them: Appearance, with the theme (the system's, light or dark) and the
// conversation's width.
import { useEffect } from 'react';
import type { Prefs, Theme, Width } from './prefs';

interface Props {
  prefs: Prefs;
  onPrefs(prefs: Prefs): void;
  /** The server's version, for the foot. */
  version: string | null;
  onClose(): void;
}

const THEMES: Array<{ theme: Theme; label: string; icon: string }> = [
  { theme: 'system', label: 'System', icon: '▭' },
  { theme: 'light', label: 'Light', icon: '☀' },
  { theme: 'dark', label: 'Dark', icon: '☾' },
];

const WIDTHS: Array<{ width: Width; label: string }> = [
  { width: 'narrow', label: 'Narrow' },
  { width: 'wide', label: 'Wide' },
];

export function Settings({ prefs, onPrefs, version, onClose }: Props) {
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
            <button className="session selected" aria-current="true">
              Appearance
            </button>
            {version && <p className="meta settings-foot">Koda {version}</p>}
          </nav>
          <section>
            <h2>General</h2>
            <div className="setting">
              <span>Theme</span>
              <div className="views" role="group" aria-label="Theme">
                {THEMES.map((t) => (
                  <button
                    key={t.theme}
                    aria-label={t.label}
                    title={t.label}
                    aria-pressed={prefs.theme === t.theme}
                    onClick={() => onPrefs({ ...prefs, theme: t.theme })}
                  >
                    <span aria-hidden>{t.icon}</span>
                  </button>
                ))}
              </div>
            </div>
            <div className="setting">
              <span>Transcript width</span>
              <div className="views" role="group" aria-label="Transcript width">
                {WIDTHS.map((w) => (
                  <button key={w.width} aria-pressed={prefs.width === w.width} onClick={() => onPrefs({ ...prefs, width: w.width })}>
                    {w.label}
                  </button>
                ))}
              </div>
            </div>
          </section>
        </div>
      </div>
    </div>
  );
}
