import { useEffect, useState } from 'react';
import { byFolder, folderName, type SessionsState, statusLabel, title } from './sessions';

interface Props {
  sessions: SessionsState;
  /** The sessions open in panes, and the one in the focused pane. */
  openIds: string[];
  selectedId: string | null;
  /** Whether another pane fits. */
  canSplit: boolean;
  /** Open a session; `beside`, in a new pane (Claude Code's split view). */
  onOpen(id: string, beside: boolean): void;
  onNew(): void;
  onMore(): void;
  /** The server's version while connected; `null` while reconnecting. */
  connectedTo: string | null;
}

/** Every folder's sessions, the most recently active first, with what each is doing. */
export function Sidebar({ sessions, openIds, selectedId, canSplit, onOpen, onNew, onMore, connectedTo }: Props) {
  const groups = byFolder(sessions);
  // Right-click a session for "Open in split view", as in Claude Code.
  const [menu, setMenu] = useState<{ id: string; x: number; y: number } | null>(null);
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && close();
    window.addEventListener('click', close);
    window.addEventListener('keydown', onKey);
    window.addEventListener('blur', close);
    return () => {
      window.removeEventListener('click', close);
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('blur', close);
    };
  }, [menu]);
  return (
    <nav className="sidebar" aria-label="Sessions">
      <div className="sidebar-top">
        <button className="new-session" onClick={onNew}>
          New session
        </button>
      </div>
      <div className="groups">
        {sessions.loaded && groups.length === 0 && <p className="empty">No sessions yet.</p>}
        {groups.map((group) => (
          <section key={group.folder ?? ''} className="group" data-testid="folder-group">
            <h2 title={group.folder ?? undefined}>{folderName(group.folder)}</h2>
            <ul>
              {group.sessions.map((session) => {
                const label = statusLabel(session.status);
                const shown = openIds.includes(session.id);
                const classes = ['session', session.id === selectedId && 'selected', shown && 'shown'].filter(Boolean);
                return (
                  <li key={session.id}>
                    <button
                      className={classes.join(' ')}
                      aria-current={session.id === selectedId ? 'true' : undefined}
                      draggable
                      onDragStart={(e) => {
                        e.dataTransfer.setData('application/x-koda-session', session.id);
                        e.dataTransfer.effectAllowed = 'copy';
                      }}
                      onClick={() => onOpen(session.id, false)}
                      onContextMenu={(e) => {
                        e.preventDefault();
                        setMenu({ id: session.id, x: e.clientX, y: e.clientY });
                      }}
                    >
                      <span className="session-title">{title(session)}</span>
                      {label && <span className={`badge ${label.replace(' ', '-')}`}>{label}</span>}
                    </button>
                  </li>
                );
              })}
            </ul>
          </section>
        ))}
        {sessions.nextCursor && (
          <button className="more" onClick={onMore}>
            Show more
          </button>
        )}
      </div>
      {menu && (
        <div className="menu" role="menu" style={{ left: menu.x, top: menu.y }}>
          <button role="menuitem" onClick={() => onOpen(menu.id, false)}>
            Open
          </button>
          <button
            role="menuitem"
            disabled={!canSplit || openIds.includes(menu.id)}
            onClick={() => onOpen(menu.id, true)}
          >
            Open in split view
          </button>
        </div>
      )}
      {connectedTo && (
        <p className="sidebar-foot" data-testid="connected">
          Connected to Koda {connectedTo}
        </p>
      )}
    </nav>
  );
}
