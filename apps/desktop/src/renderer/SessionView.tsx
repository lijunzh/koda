import type { Selected } from './App';
import { type ListedSession, statusLabel, title } from './sessions';

/** The open session: what it is, where it works, and deleting it. Its conversation comes next. */
export function SessionView({ selected, listed }: { selected: Selected | null; listed?: ListedSession }) {
  if (!selected) {
    return (
      <main className="session-view empty-view">
        <p>Open a session from the list, or start a new one.</p>
      </main>
    );
  }
  const session = selected.session;
  const name = title({ name: listed?.name ?? session?.name, preview: listed?.preview ?? session?.preview });
  const folder = session?.cwd ?? listed?.cwd ?? null;
  const label = statusLabel(listed?.status ?? session?.status ?? null);
  return (
    <main className="session-view" data-testid="session-view">
      <header>
        <h1>{name}</h1>
        <button className="delete" onClick={() => void window.koda.deleteSession(selected.id)}>
          Delete
        </button>
      </header>
      <dl className="facts">
        {folder && (
          <>
            <dt>Folder</dt>
            <dd data-testid="session-folder">
              <code>{folder}</code>
            </dd>
          </>
        )}
        {session?.model && (
          <>
            <dt>Model</dt>
            <dd>{session.model}</dd>
          </>
        )}
        {label && (
          <>
            <dt>Status</dt>
            <dd data-testid="session-status">{label}</dd>
          </>
        )}
        {session && (
          <>
            <dt>Turns</dt>
            <dd>{session.turns?.length ?? 0}</dd>
          </>
        )}
      </dl>
      {selected.error && <p className="problem">{selected.error}</p>}
      {!session && !selected.error && <p className="detail">Opening…</p>}
    </main>
  );
}
