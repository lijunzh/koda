// A button that opens a menu of choices, one of them current: the app's own
// menu in place of the browser's select, as Claude Code's desktop app has
// for the model, the effort and the mode.
import { useEffect, useRef, useState } from 'react';

export interface MenuItem {
  value: string;
  label: string;
  /** A second, quieter part of the label. */
  detail?: string;
}

interface Props {
  /** What the control is, for assistive tech and tests. */
  label: string;
  value: string;
  items: MenuItem[];
  onPick(value: string): void;
  /** Open above the button, for a control at the bottom of the window. */
  up?: boolean;
}

export function MenuButton({ label, value, items, onPick, up }: Props) {
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!open) return;
    const outside = (e: MouseEvent) => {
      if (!anchor.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && setOpen(false);
    window.addEventListener('mousedown', outside);
    window.addEventListener('keydown', onKey);
    const away = () => setOpen(false);
    window.addEventListener('blur', away);
    return () => {
      window.removeEventListener('mousedown', outside);
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('blur', away);
    };
  }, [open]);
  const current = items.find((item) => item.value === value);
  return (
    <span className="menu-anchor" ref={anchor}>
      <button
        className="menu-button"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        title={label}
        onClick={() => setOpen((o) => !o)}
      >
        {current?.label ?? value}
        <span className="caret" aria-hidden>
          ⌄
        </span>
      </button>
      {open && (
        <div className={up ? 'menu anchored up' : 'menu anchored'} role="menu" aria-label={label}>
          {items.map((item) => (
            <button
              key={item.value}
              role="menuitemradio"
              aria-checked={item.value === value}
              onClick={() => {
                setOpen(false);
                if (item.value !== value) onPick(item.value);
              }}
            >
              <span className="menu-label">
                {item.label}
                {item.detail && <span className="meta"> {item.detail}</span>}
              </span>
              {item.value === value && (
                <span className="check" aria-hidden>
                  ✓
                </span>
              )}
            </button>
          ))}
        </div>
      )}
    </span>
  );
}
