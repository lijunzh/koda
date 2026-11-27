import { useCallback, useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import { call, type ServerStatus } from './bridge';
import { ServerLine } from './ServerLine';
import { SessionView } from './SessionView';
import { EMPTY, type SessionsState, withMessages, withPage } from './sessions';
import { Sidebar } from './Sidebar';

/** The open session: its id at once, the session once the server has opened it. */
export interface Selected {
  id: string;
  session?: Session;
  error?: string;
}

const PAGE = 50;

export function App() {
  const [status, setStatus] = useState<ServerStatus | null>(null);
  const [sessions, setSessions] = useState<SessionsState>(EMPTY);
  const [selected, setSelected] = useState<Selected | null>(null);
  const selectedId = useRef<string | null>(null);

  useEffect(() => {
    const stop = window.koda.onStatus(setStatus);
    void window.koda.status().then(setStatus);
    return stop;
  }, []);

  useEffect(
    () =>
      window.koda.onMessages((batch) => {
        setSessions((state) => withMessages(state, batch));
        for (const message of batch) {
          const params = message.params as { sessionId?: string } | undefined;
          if (message.method === 'session/deleted' && params?.sessionId === selectedId.current) {
            selectedId.current = null;
            setSelected(null);
          }
        }
      }),
    [],
  );

  const connected = status?.state === 'connected';
  // The list, afresh each time the app (re)connects.
  useEffect(() => {
    if (!connected) return;
    void call('session/list', { limit: PAGE, sortKey: 'updated_at' }).then((page) =>
      setSessions((state) => withPage(state, page, true)),
    );
  }, [connected]);

  const more = useCallback(() => {
    if (!sessions.nextCursor) return;
    void call('session/list', { limit: PAGE, sortKey: 'updated_at', cursor: sessions.nextCursor }).then((page) =>
      setSessions((state) => withPage(state, page, false)),
    );
  }, [sessions.nextCursor]);

  // Opening a session joins it on the server; the one left stops being
  // followed (a turn running in it goes on, as when a window closes).
  const open = useCallback((id: string, session?: Session) => {
    const before = selectedId.current;
    if (before === id) return;
    if (before) void call('session/unsubscribe', { sessionId: before }).catch(() => {});
    selectedId.current = id;
    if (session) {
      setSelected({ id, session });
      return;
    }
    setSelected({ id });
    call('session/resume', { sessionId: id }).then(
      (answer) => selectedId.current === id && setSelected({ id, session: answer.session }),
      (error: Error) => selectedId.current === id && setSelected({ id, error: error.message }),
    );
  }, []);

  const start = useCallback(async () => {
    const session = await window.koda.newSession();
    if (session) open(session.id, session);
  }, [open]);

  // Once there's a list, it stays while the app gets back to the server.
  const reconnecting = status?.state === 'reconnecting' && sessions.loaded;
  if (!connected && !reconnecting) {
    return (
      <main className="status">
        <h1>Koda</h1>
        <ServerLine status={status} />
      </main>
    );
  }
  return (
    <div className="layout">
      <Sidebar
        sessions={sessions}
        selectedId={selected?.id ?? null}
        onOpen={(id) => open(id)}
        onNew={() => void start()}
        onMore={more}
        connectedTo={connected ? status.serverVersion : null}
      />
      <div className="main-column">
        {reconnecting && <ServerLine status={status} />}
        <SessionView selected={selected} listed={selected ? sessions.byId.get(selected.id) : undefined} />
      </div>
    </div>
  );
}
