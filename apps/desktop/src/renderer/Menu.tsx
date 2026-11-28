// The app's own menus, in place of the browser's select: a button that opens
// a list of entries, some checked, some with a line of description or a
// number key, some opening a further list, as Claude Code's desktop app has
// for the model, the effort, the mode and a session's actions.
import { type CSSProperties, type ReactNode, useEffect, useRef, useState } from 'react';

export type Entry =
  | {
      type: 'item';
      id: string;
      label: string;
      /** A second, quieter part of the label. */
      detail?: string;
      /** A line under the label. */
      description?: string;
      /** A key that picks it while the menu is open, shown at the right. */
      hint?: string;
      checked?: boolean;
      disabled?: boolean;
      danger?: boolean;
      onPick(): void;
    }
  | { type: 'submenu'; id: string; label: string; detail?: string; entries: Entry[] | (() => Promise<Entry[]>) }
  | { type: 'heading'; label: string }
  | { type: 'separator' };

interface ListProps {
  entries: Entry[];
  label: string;
  className: string;
  /** Where a menu opened at the pointer goes. */
  style?: CSSProperties;
  close(): void;
}

/** A menu's entries; `MenuButton` opens one under a button, a context menu at the pointer. */
export function MenuList({ entries, label, className, style, close }: ListProps) {
  /** The open submenu, and its entries once they are there. */
  const [sub, setSub] = useState<{ id: string; entries: Entry[] | null } | null>(null);
  const openSub = (entry: Extract<Entry, { type: 'submenu' }>) => {
    if (sub?.id === entry.id) return;
    if (Array.isArray(entry.entries)) {
      setSub({ id: entry.id, entries: entry.entries });
      return;
    }
    setSub({ id: entry.id, entries: null });
    void entry.entries().then(
      (loaded) => setSub((now) => (now?.id === entry.id ? { id: entry.id, entries: loaded } : now)),
      () => setSub((now) => (now?.id === entry.id ? { id: entry.id, entries: [] } : now)),
    );
  };
  return (
    <div className={className} style={style} role="menu" aria-label={label}>
      {entries.map((entry, i) => {
        if (entry.type === 'separator') return <hr key={i} />;
        if (entry.type === 'heading') {
          return (
            <div key={i} className="menu-heading">
              {entry.label}
            </div>
          );
        }
        if (entry.type === 'submenu') {
          const open = sub?.id === entry.id;
          return (
            <div key={entry.id} className="menu-sub" onMouseEnter={() => openSub(entry)}>
              <button role="menuitem" aria-haspopup="menu" aria-expanded={open} onClick={() => openSub(entry)}>
                <span className="menu-label">{entry.label}</span>
                {entry.detail && <span className="meta">{entry.detail}</span>}
                <span className="meta" aria-hidden>
                  ›
                </span>
              </button>
              {open &&
                (sub.entries === null ? (
                  <div className="menu side">
                    <div className="menu-heading">Loading…</div>
                  </div>
                ) : sub.entries.length === 0 ? (
                  <div className="menu side">
                    <div className="menu-heading">Nothing here</div>
                  </div>
                ) : (
                  <MenuList entries={sub.entries} label={entry.label} className="menu side" close={close} />
                ))}
            </div>
          );
        }
        return (
          <button
            key={entry.id}
            role={entry.checked === undefined ? 'menuitem' : 'menuitemradio'}
            aria-checked={entry.checked}
            disabled={entry.disabled}
            className={entry.danger ? 'danger' : undefined}
            onMouseEnter={() => setSub(null)}
            onClick={() => {
              close();
              entry.onPick();
            }}
          >
            <span className="menu-label">
              {entry.label}
              {entry.detail && <span className="meta"> {entry.detail}</span>}
              {entry.description && <span className="menu-description">{entry.description}</span>}
            </span>
            {entry.checked && (
              <span className="check" aria-hidden>
                ✓
              </span>
            )}
            {entry.hint && <span className="menu-hint">{entry.hint}</span>}
          </button>
        );
      })}
    </div>
  );
}

interface Props {
  /** What the control is, for assistive tech and tests. */
  label: string;
  /** What the button shows. */
  children: ReactNode;
  entries: Entry[];
  /** Open above the button, for a control at the bottom of the window. */
  up?: boolean;
  /** Line the menu up with the button's right edge. */
  right?: boolean;
  /** With Cmd+Shift, the letter that opens it (in the focused pane). */
  hotkey?: string;
  className?: string;
}

export function MenuButton({ label, children, entries, up, right, hotkey, className }: Props) {
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!hotkey) return;
    const onKey = (e: KeyboardEvent) => {
      if (!e.metaKey || !e.shiftKey || e.key.toLowerCase() !== hotkey) return;
      const pane = anchor.current?.closest('.pane');
      if (pane && !pane.classList.contains('focused') && document.querySelectorAll('.pane').length > 1) return;
      e.preventDefault();
      setOpen((o) => !o);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [hotkey]);
  useEffect(() => {
    if (!open) return;
    const outside = (e: MouseEvent) => {
      if (!anchor.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setOpen(false);
        return;
      }
      // A number key picks the entry that shows it.
      const hinted = entries.find((entry) => entry.type === 'item' && entry.hint === e.key && !entry.disabled);
      if (hinted?.type === 'item' && !e.metaKey && !e.ctrlKey && !e.altKey) {
        e.preventDefault();
        setOpen(false);
        hinted.onPick();
      }
    };
    const away = () => setOpen(false);
    window.addEventListener('mousedown', outside);
    window.addEventListener('keydown', onKey);
    window.addEventListener('blur', away);
    return () => {
      window.removeEventListener('mousedown', outside);
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('blur', away);
    };
  }, [open, entries]);
  return (
    <span className="menu-anchor" ref={anchor}>
      <button
        className={className ?? 'menu-button'}
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        title={label}
        onClick={() => setOpen((o) => !o)}
      >
        {children}
      </button>
      {open && (
        <MenuList
          entries={entries}
          label={label}
          className={['menu anchored', up && 'up', right && 'right'].filter(Boolean).join(' ')}
          close={() => setOpen(false)}
        />
      )}
    </span>
  );
}
