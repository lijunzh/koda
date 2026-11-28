// The session list, as Claude Code's desktop app has it: New session and
// Search at the top; every folder's sessions, the most recently active
// first; a mark on a session that is working or waiting for you; and a "…"
// menu on each (also on a right-click) to open it beside another, rename,
// fork or delete it. Option-click opens a session in split view.
import { useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import { call } from './bridge';
import { type Entry, MenuButton, MenuList } from './Menu';
import { byFolder, folderName, type ListedSession, type SessionsState, statusLabel, title } from './sessions';

interface Props {
  sessions: SessionsState;
  /** The sessions open in panes, and the one in the focused pane. */
  openIds: string[];
  selectedId: string | null;
  /** Whether another pane fits. */
  canSplit: boolean;
  /** Open a session; `beside`, in a new pane (Claude Code's split view). */
  onOpen(id: string, beside: boolean): void;
  /** Open a session the server just made (a fork). */
  onOpenSession(session: Session): void;
  onNew(): void;
  onMore(): void;
  onSettings(): void;
  /** The server's version while connected; `null` while reconnecting. */
  connectedTo: string | null;
}

export function Sidebar({ sessions, openIds, selectedId, canSplit, onOpen, onOpenSession, onNew, onMore, onSettings, connectedTo }: Props) {
  const [query, setQuery] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<{ id: string; name: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  // A right-click opens the session's menu at the pointer.
  const [menu, setMenu] = useState<{ session: ListedSession; x: number; y: number } | null>(null);
  const menuAt = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const outside = (e: MouseEvent) => {
      if (!menuAt.current?.contains(e.target as Node)) close();
    };
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && close();
    window.addEventListener('mousedown', outside);
    window.addEventListener('keydown', onKey);
    window.addEventListener('blur', close);
    return () => {
      window.removeEventListener('mousedown', outside);
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('blur', close);
    };
  }, [menu]);

  const wanted = query?.trim().toLowerCase() ?? '';
  const groups = byFolder(sessions)
    .map((group) => ({
      ...group,
      sessions: wanted ? group.sessions.filter((s) => title(s).toLowerCase().includes(wanted)) : group.sessions,
    }))
    .filter((group) => group.sessions.length);

  const entriesFor = (session: ListedSession): Entry[] => [
    { type: 'item', id: 'open', label: 'Open', onPick: () => onOpen(session.id, false) },
    {
      type: 'item',
      id: 'split',
      label: 'Open in split view',
      disabled: !canSplit || openIds.includes(session.id),
      onPick: () => onOpen(session.id, true),
    },
    { type: 'separator' },
    { type: 'item', id: 'rename', label: 'Rename', onPick: () => setRenaming({ id: session.id, name: session.name ?? '' }) },
    {
      type: 'item',
      id: 'fork',
      label: 'Fork',
      onPick: () =>
        void call('session/fork', { sessionId: session.id }).then(
          (answer) => onOpenSession(answer.session),
          (e: Error) => setError(e.message),
        ),
    },
    { type: 'separator' },
    { type: 'item', id: 'delete', label: 'Delete', danger: true, onPick: () => void window.koda.deleteSession(session.id) },
  ];

  return (
    <nav className="sidebar" aria-label="Sessions">
      <div className="sidebar-top">
        <button className="new-session" onClick={onNew}>
          New session
        </button>
        {query === null ? (
          <button className="search-sessions" onClick={() => setQuery('')}>
            Search
          </button>
        ) : (
          <input
            className="search-input"
            aria-label="Search sessions"
            placeholder="Search sessions"
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => e.key === 'Escape' && setQuery(null)}
            onBlur={() => !query && setQuery(null)}
          />
        )}
      </div>
      <div className="groups">
        {sessions.loaded && groups.length === 0 && <p className="empty">{wanted ? 'No session matches.' : 'No sessions yet.'}</p>}
        {error && <p className="problem empty">{error}</p>}
        {groups.map((group) => (
          <section key={group.folder ?? ''} className="group" data-testid="folder-group">
            <h2 title={group.folder ?? undefined}>{folderName(group.folder)}</h2>
            <ul>
              {group.sessions.map((session) => {
                const label = statusLabel(session.status);
                const shown = openIds.includes(session.id);
                const classes = ['session', session.id === selectedId && 'selected', shown && 'shown'].filter(Boolean);
                if (renaming?.id === session.id) {
                  return (
                    <li key={session.id}>
                      <input
                        className="rename-row"
                        aria-label="Session name"
                        autoFocus
                        maxLength={100}
                        value={renaming.name}
                        onChange={(e) => setRenaming({ id: session.id, name: e.target.value })}
                        onBlur={() => setRenaming(null)}
                        onKeyDown={(e) => {
                          if (e.key === 'Escape') setRenaming(null);
                          if (e.key === 'Enter' && renaming.name.trim() && !e.nativeEvent.isComposing) {
                            void call('session/name/set', { sessionId: session.id, name: renaming.name.trim() }).finally(() =>
                              setRenaming(null),
                            );
                          }
                        }}
                      />
                    </li>
                  );
                }
                return (
                  <li key={session.id} className="session-row">
                    <button
                      className={classes.join(' ')}
                      aria-current={session.id === selectedId ? 'true' : undefined}
                      draggable
                      onDragStart={(e) => {
                        e.dataTransfer.setData('application/x-koda-session', session.id);
                        e.dataTransfer.effectAllowed = 'copy';
                      }}
                      // Option-click opens it beside the open one, as in Claude Code.
                      onClick={(e) => onOpen(session.id, e.altKey)}
                      onContextMenu={(e) => {
                        e.preventDefault();
                        setMenu({ session, x: e.clientX, y: e.clientY });
                      }}
                    >
                      <span className="session-title">{title(session)}</span>
                      {label === 'working' && (
                        <span className="mark working" role="img" aria-label="working" title="Working" />
                      )}
                      {label === 'needs you' && <span className="badge needs-you">needs you</span>}
                    </button>
                    <MenuButton label="Session actions" entries={entriesFor(session)} className="row-menu" right>
                      <span aria-hidden>⋯</span>
                    </MenuButton>
                  </li>
                );
              })}
            </ul>
          </section>
        ))}
        {sessions.nextCursor && !wanted && (
          <button className="more" onClick={onMore}>
            Show more
          </button>
        )}
      </div>
      {menu && (
        <div ref={menuAt}>
          <MenuList
            entries={entriesFor(menu.session)}
            label="Session actions"
            className="menu"
            style={{ left: menu.x, top: menu.y }}
            close={() => setMenu(null)}
          />
        </div>
      )}
      <div className="sidebar-bottom">
        {connectedTo ? (
          <p className="sidebar-foot" data-testid="connected">
            Connected to Koda {connectedTo}
          </p>
        ) : (
          <span />
        )}
        <button className="icon-button" aria-label="Settings" title="Settings (⌘,)" onClick={onSettings}>
          <span aria-hidden>⚙</span>
        </button>
      </div>
    </nav>
  );
}
