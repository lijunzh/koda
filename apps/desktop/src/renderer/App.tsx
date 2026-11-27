import { useCallback, useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import { call, type RpcMessage, type ServerStatus } from './bridge';
import { ServerLine } from './ServerLine';
import { SessionView } from './SessionView';
import { EMPTY, type SessionsState, withMessages, withPage } from './sessions';
import { Sidebar } from './Sidebar';
import { fromSession, type TranscriptState, type View, withEvents } from './transcript';

/** The open session: its id at once, the session once the server has opened it. */
export interface Selected {
  id: string;
  session?: Session;
  error?: string;
}

const PAGE = 50;
/** Live events are applied at most this often, so a fast stream doesn't redraw for every word. */
const FLUSH_MS = 50;

export function App() {
  const [status, setStatus] = useState<ServerStatus | null>(null);
  const [sessions, setSessions] = useState<SessionsState>(EMPTY);
  const [selected, setSelected] = useState<Selected | null>(null);
  const [transcript, setTranscript] = useState<TranscriptState | null>(null);
  const [view, setView] = useState<View>('normal');
  const selectedId = useRef<string | null>(null);
  /** Events from before the open session's snapshot came, kept to apply after it. */
  const early = useRef<RpcMessage[]>([]);

  // Ctrl+O switches the normal and full views, as in the TUI and Claude Code.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey && !e.metaKey && !e.altKey && e.key.toLowerCase() === 'o') {
        e.preventDefault();
        setView((v) => (v === 'verbose' ? 'normal' : 'verbose'));
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  useEffect(() => {
    const stop = window.koda.onStatus(setStatus);
    void window.koda.status().then(setStatus);
    return stop;
  }, []);

  useEffect(() => {
    let queued: RpcMessage[] = [];
    let timer: ReturnType<typeof setTimeout> | null = null;
    const flush = () => {
      timer = null;
      const batch = queued;
      queued = [];
      setSessions((state) => withMessages(state, batch));
      setTranscript((state) => {
        if (state) return withEvents(state, batch);
        // The open session's snapshot hasn't come yet: keep its events for it.
        early.current.push(...batch);
        return state;
      });
      for (const message of batch) {
        const params = message.params as { sessionId?: string } | undefined;
        if (message.method === 'session/deleted' && params?.sessionId === selectedId.current) {
          selectedId.current = null;
          setSelected(null);
          setTranscript(null);
        }
      }
    };
    const stop = window.koda.onMessages((batch) => {
      queued.push(...batch);
      timer ??= setTimeout(flush, FLUSH_MS);
    });
    return () => {
      stop();
      if (timer) clearTimeout(timer);
    };
  }, []);

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
    early.current = [];
    setTranscript(null);
    const opened = (session: Session) => {
      if (selectedId.current !== id) return;
      setSelected({ id, session });
      // The snapshot, then what came while it was on its way: the server
      // answers at one point in the session's stream, so nothing repeats.
      setTranscript(withEvents(fromSession(session), early.current));
      early.current = [];
    };
    if (session) {
      opened(session);
      return;
    }
    setSelected({ id });
    call('session/resume', { sessionId: id }).then(
      (answer) => opened(answer.session),
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
        <SessionView
          selected={selected}
          listed={selected ? sessions.byId.get(selected.id) : undefined}
          transcript={transcript}
          view={view}
          onView={setView}
        />
      </div>
    </div>
  );
}
