import { byFolder, folderName, type SessionsState, statusLabel, title } from './sessions';

interface Props {
  sessions: SessionsState;
  selectedId: string | null;
  onOpen(id: string): void;
  onNew(): void;
  onMore(): void;
  /** The server's version while connected; `null` while reconnecting. */
  connectedTo: string | null;
}

/** Every folder's sessions, the most recently active first, with what each is doing. */
export function Sidebar({ sessions, selectedId, onOpen, onNew, onMore, connectedTo }: Props) {
  const groups = byFolder(sessions);
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
                return (
                  <li key={session.id}>
                    <button
                      className={session.id === selectedId ? 'session selected' : 'session'}
                      aria-current={session.id === selectedId ? 'true' : undefined}
                      onClick={() => onOpen(session.id)}
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
      {connectedTo && (
        <p className="sidebar-foot" data-testid="connected">
          Connected to Koda {connectedTo}
        </p>
      )}
    </nav>
  );
}
