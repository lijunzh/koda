import { useCallback, useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import { call, type RpcMessage, type ServerStatus } from './bridge';
import { type Listener, Pane } from './Pane';
import { forSession, type Pending, withRequests, without } from './requests';
import { ServerLine } from './ServerLine';
import { NewSession } from './NewSession';
import { EMPTY, type SessionsState, withMessages, withPage } from './sessions';
import { applyPrefs, loadPrefs, type Prefs, savePrefs } from './prefs';
import { Settings } from './Settings';
import { Sidebar } from './Sidebar';
import { type View, VIEWS } from './transcript';

/** The open session: its id at once, the session once the server has opened it. */
export interface Selected {
  id: string;
  session?: Session;
  error?: string;
}

/** A pane of the window: the session it shows, if any. */
interface PaneState {
  /** Stays the pane's while it shows other sessions. */
  key: number;
  id: string | null;
  /** The session as the server just made it, for a fork. */
  initial?: Session;
  /** A new session's first prompt. */
  firstPrompt?: string;
}

const PAGE = 50;
/** Live events are applied at most this often, so a fast stream doesn't redraw for every word. */
const FLUSH_MS = 50;
/** Sessions side by side, at most. */
export const MAX_PANES = 4;

let nextKey = 1;

export function App() {
  const [status, setStatus] = useState<ServerStatus | null>(null);
  const [sessions, setSessions] = useState<SessionsState>(EMPTY);
  const [panes, setPanes] = useState<PaneState[]>([{ key: 0, id: null }]);
  const [focused, setFocused] = useState(0);
  const [pending, setPending] = useState<readonly Pending[]>([]);
  const [planShown, setPlanShown] = useState(true);
  const [detailsShown, setDetailsShown] = useState(true);
  const [prefs, setPrefs] = useState<Prefs>(() => loadPrefs(window.localStorage));
  // The view the app opens in is a setting; Ctrl+O changes it for now.
  const [view, setView] = useState<View>(prefs.view);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // The theme and the width are attributes the stylesheet reads.
  useEffect(() => {
    applyPrefs(document.documentElement, prefs);
    savePrefs(window.localStorage, prefs);
    void window.koda.setAppPrefs({ notifications: prefs.notifications, keepAwake: prefs.keepAwake }).catch(() => {});
  }, [prefs]);
  // `open` runs after awaits (a new session's dialog): it reads these, not a render's.
  const current = useRef({ panes, focused });
  current.current = { panes, focused };
  const listeners = useRef(new Set<Listener>());
  const subscribe = useCallback((listener: Listener) => {
    listeners.current.add(listener);
    return () => void listeners.current.delete(listener);
  }, []);

  // Ctrl+O cycles Normal, Thinking and Verbose, as in Claude Code's desktop app.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey && !e.metaKey && !e.altKey && e.key.toLowerCase() === 'o') {
        e.preventDefault();
        setView((v) => VIEWS[(VIEWS.indexOf(v) + 1) % VIEWS.length]);
      }
      // Ctrl+T hides or shows the plan, as in Claude Code.
      if (e.ctrlKey && !e.metaKey && !e.altKey && e.key.toLowerCase() === 't') {
        e.preventDefault();
        setPlanShown((shown) => !shown);
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
      setPending((state) => withRequests(state, batch));
      for (const listener of listeners.current) listener(batch);
      for (const message of batch) {
        const params = message.params as { sessionId?: string } | undefined;
        if (message.method === 'session/deleted' && params?.sessionId) closeSession(params.sessionId);
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
  // The list, afresh each time the app (re)connects; requests from before are
  // gone with the connection, and opening a session sends its own again.
  useEffect(() => {
    if (!connected) return;
    setPending([]);
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

  /** A deleted session's panes close; the last one is left empty. */
  const closeSession = (id: string) => {
    setPanes((all) => {
      const left = all.filter((p) => p.id !== id);
      if (left.length === all.length) return all;
      return left.length ? left : [{ key: nextKey++, id: null }];
    });
    setFocused(0);
  };

  /**
   * Open session `id`: in the pane that has it already, else in the focused
   * pane, or, `beside`, in a new pane next to it (Claude Code's split view).
   */
  const open = useCallback(
    (id: string, options: { session?: Session; beside?: boolean; pane?: number; firstPrompt?: string } = {}) => {
      const { panes, focused } = current.current;
      const at = panes.findIndex((p) => p.id === id);
      if (at >= 0) {
        setFocused(at);
        return;
      }
      const next: PaneState = { key: nextKey++, id, initial: options.session, firstPrompt: options.firstPrompt };
      if (options.beside && panes.length < MAX_PANES) {
        const where = (options.pane ?? focused) + 1;
        setPanes([...panes.slice(0, where), next, ...panes.slice(where)]);
        setFocused(where);
        return;
      }
      const where = Math.min(options.pane ?? focused, panes.length - 1);
      setPanes(panes.map((p, i) => (i === where ? { ...next, key: p.key } : p)));
      setFocused(where);
    },
    [],
  );

  const closePane = (index: number) => {
    setPanes((all) => all.filter((_, i) => i !== index));
    setFocused((f) => Math.max(0, f >= index ? f - 1 : f));
  };

  // "New session" turns the focused pane into a new one: nothing starts
  // until its first prompt.
  const start = useCallback(() => {
    const { focused } = current.current;
    setPanes((all) => all.map((p, i) => (i === focused ? { key: nextKey++, id: null } : p)));
  }, []);

  // Cmd+N starts a session and Cmd+Ctrl+W takes the focused one out of
  // split view, as in Claude Code's desktop app.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey && !e.shiftKey && !e.altKey && !e.ctrlKey && e.key.toLowerCase() === 'n') {
        e.preventDefault();
        start();
      }
      // Cmd+, opens Settings, as everywhere on a Mac.
      if (e.metaKey && !e.shiftKey && !e.altKey && !e.ctrlKey && e.key === ',') {
        e.preventDefault();
        setSettingsOpen(true);
      }
      if (e.metaKey && e.ctrlKey && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'w') {
        const { panes, focused } = current.current;
        if (panes.length > 1) {
          e.preventDefault();
          setPanes((all) => all.filter((_, i) => i !== focused));
          setFocused((f) => Math.max(0, f - 1));
        }
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [start]);

  const answer = async (id: number | string, result: unknown) => {
    await window.koda.respond(id, result);
    setPending((state) => without(state, id));
  };

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
  const openIds = panes.flatMap((p) => (p.id ? [p.id] : []));
  return (
    <div className="layout">
      <Sidebar
        sessions={sessions}
        openIds={openIds}
        selectedId={panes[focused]?.id ?? null}
        canSplit={panes.length < MAX_PANES}
        onOpen={(id, beside) => open(id, { beside })}
        onOpenSession={(session) => open(session.id, { session })}
        onNew={start}
        onMore={more}
        onSettings={() => setSettingsOpen(true)}
        connectedTo={connected ? status.serverVersion : null}
      />
      {settingsOpen && (
        <Settings
          prefs={prefs}
          onPrefs={(next) => {
            // A new default view shows at once.
            if (next.view !== prefs.view) setView(next.view);
            setPrefs(next);
          }}
          appVersion={status?.state === 'connected' ? status.appVersion : null}
          version={connected ? status.serverVersion : null}
          onClose={() => setSettingsOpen(false)}
        />
      )}
      <div className="main-column">
        {reconnecting && <ServerLine status={status} />}
        <div
          className="panes"
          // A session dragged from the list opens beside the pane it's dropped on.
          onDragOver={(e) => {
            if (e.dataTransfer.types.includes('application/x-koda-session')) e.preventDefault();
          }}
          onDrop={(e) => {
            const id = e.dataTransfer.getData('application/x-koda-session');
            const pane = Number((e.target as HTMLElement).closest('[data-pane]')?.getAttribute('data-pane') ?? focused);
            if (id) open(id, { beside: true, pane });
          }}
        >
          {panes.map((pane, i) => (
            <div
              key={pane.key}
              data-pane={i}
              data-testid="pane"
              className={panes.length > 1 && i === focused ? 'pane focused' : 'pane'}
              onMouseDownCapture={() => setFocused(i)}
              onFocusCapture={() => setFocused(i)}
            >
              {pane.id ? (
                <Pane
                  key={pane.id}
                  id={pane.id}
                  initial={pane.initial}
                  firstPrompt={pane.firstPrompt}
                  subscribe={subscribe}
                  listed={sessions.byId.get(pane.id)}
                  requests={forSession([...pending], pane.id)}
                  onAnswer={answer}
                  view={view}
                  onView={setView}
                  planShown={planShown}
                  detailsShown={detailsShown}
                  onDetails={setDetailsShown}
                  onOpen={(session) => open(session.id, { session, pane: i })}
                  onClose={panes.length > 1 ? () => closePane(i) : undefined}
                />
              ) : (
                <NewSession
                  sessions={sessions}
                  onStarted={(session, prompt) => open(session.id, { pane: i, firstPrompt: prompt })}
                  onClose={panes.length > 1 ? () => closePane(i) : undefined}
                />
              )}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
