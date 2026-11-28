import { useCallback, useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import { call, type RpcMessage, type ServerStatus } from './bridge';
import { type Listener, Pane } from './Pane';
import { forSession, type Pending, withRequests, without } from './requests';
import { Logo } from './Logo';
import { ServerLine } from './ServerLine';
import { back, canGoBack, canGoForward, forget, forward, here, type History, START, visit } from './history';
import { NewSession } from './NewSession';
import { EMPTY, type SessionsState, withMessages, withPage, withSession } from './sessions';
import { applyPrefs, loadPrefs, type Prefs, savePrefs } from './prefs';
import { Settings } from './Settings';
import { Sidebar } from './Sidebar';
import { type View, VIEWS } from './transcript';
import { WindowTools } from './WindowTools';

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
  /** An archived session, shown to read. */
  archived?: boolean;
  /** Where a search result opened it: the conversation shows that item. */
  at?: Place;
}

/** An item of a conversation to show; `n` tells one click on a search result from the next. */
export interface Place {
  item: string;
  n: number;
}

const PAGE = 50;
/** Live events are applied at most this often, so a fast stream doesn't redraw for every word. */
const FLUSH_MS = 50;
/** Sessions side by side, at most. */
export const MAX_PANES = 4;

let nextKey = 1;
let nextPlace = 1;

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
  // Where the window has been, for Back and Forward.
  const [history, setHistory] = useState<History>(START);
  // On a Mac the window's own buttons sit in the app's top bar, except in
  // full screen, which has none: the stylesheet leaves them room.
  useEffect(() => {
    const root = document.documentElement;
    root.dataset.platform = window.koda.platform;
    return window.koda.onFullScreen((full) => {
      if (full) root.dataset.fullscreen = '';
      else delete root.dataset.fullscreen;
    });
  }, []);
  // The theme and the width are attributes the stylesheet reads.
  useEffect(() => {
    applyPrefs(document.documentElement, prefs);
    savePrefs(window.localStorage, prefs);
    void window.koda.setAppPrefs({ notifications: prefs.notifications, keepAwake: prefs.keepAwake }).catch(() => {});
  }, [prefs]);
  // `open` runs after awaits (a new session's dialog): it reads these, not a render's.
  const current = useRef({ panes, focused, history });
  current.current = { panes, focused, history };
  // Whatever the focused pane shows is where the window is: opening a
  // session, starting one and moving to another pane all count.
  const shown = panes[focused]?.id ?? null;
  useEffect(() => setHistory((h) => visit(h, shown)), [shown]);
  // The sessions this window knows are archived: one opened from the
  // Archived list, or archived while the app ran. Opening one reads it.
  const archivedIds = useRef(new Set<string>());
  // Goes up when the archive changes, for the Archived list to ask again.
  const [archiveTick, setArchiveTick] = useState(0);
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
        if (!params?.sessionId) continue;
        const id = params.sessionId;
        if (message.method === 'session/deleted') {
          archivedIds.current.delete(id);
          closeSession(id);
          setArchiveTick((n) => n + 1);
        }
        // An archived session is closed on the server: its pane goes, as
        // Claude Code's desktop app puts the session away.
        if (message.method === 'session/archived') {
          archivedIds.current.add(id);
          closeSession(id);
          setArchiveTick((n) => n + 1);
        }
        // Brought back: it is listed again, and a pane reading it opens it.
        if (message.method === 'session/unarchived') {
          archivedIds.current.delete(id);
          setArchiveTick((n) => n + 1);
          setPanes((all) => all.map((p) => (p.id === id && p.archived ? { key: nextKey++, id } : p)));
          void call('session/read', { sessionId: id }).then(
            (answer) => setSessions((state) => withSession(state, answer.session)),
            () => {},
          );
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
    setHistory((h) => forget(h, id));
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
    (
      id: string,
      options: { session?: Session; beside?: boolean; pane?: number; firstPrompt?: string; archived?: boolean; item?: string | null } = {},
    ) => {
      const { panes, focused } = current.current;
      const place = options.item ? { item: options.item, n: nextPlace++ } : undefined;
      const at = panes.findIndex((p) => p.id === id);
      if (at >= 0) {
        // Open already: a search result still shows where it matched.
        if (place) setPanes(panes.map((p, i) => (i === at ? { ...p, at: place } : p)));
        setFocused(at);
        return;
      }
      if (options.archived) archivedIds.current.add(id);
      const next: PaneState = {
        key: nextKey++,
        id,
        initial: options.session,
        firstPrompt: options.firstPrompt,
        archived: archivedIds.current.has(id),
        at: place,
      };
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

  /** Back or Forward: to the session shown before or after, in its pane if it has one still. */
  const travel = useCallback(
    (step: (history: History) => History) => {
      const next = step(current.current.history);
      if (next === current.current.history) return;
      setHistory(next);
      const place = here(next);
      if (place) open(place);
      else start();
    },
    [open, start],
  );
  const toggleSidebar = useCallback(() => setPrefs((p) => ({ ...p, sidebar: !p.sidebar })), []);

  // Cmd+N starts a session and Cmd+Ctrl+W takes the focused one out of
  // split view, as in Claude Code's desktop app; so do Cmd+B for the
  // sidebar and Cmd+[ and Cmd+] for Back and Forward.
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
      if (e.metaKey && !e.shiftKey && !e.altKey && !e.ctrlKey) {
        const act = e.key.toLowerCase() === 'b' ? toggleSidebar : e.key === '[' ? () => travel(back) : e.key === ']' ? () => travel(forward) : null;
        if (act) {
          e.preventDefault();
          act();
        }
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
  }, [start, travel, toggleSidebar]);

  const answer = async (id: number | string, result: unknown) => {
    await window.koda.respond(id, result);
    setPending((state) => without(state, id));
  };

  // Once there's a list, it stays while the app gets back to the server.
  const reconnecting = status?.state === 'reconnecting' && sessions.loaded;
  if (!connected && !reconnecting) {
    return (
      <main className="status">
        <div className="window-band" />
        <Logo size={56} />
        <h1>Koda</h1>
        <ServerLine status={status} />
      </main>
    );
  }
  const openIds = panes.flatMap((p) => (p.id ? [p.id] : []));
  return (
    <div className={prefs.sidebar ? 'layout' : 'layout no-sidebar'}>
      <div className="window-band" />
      <WindowTools
        sidebarShown={prefs.sidebar}
        onSidebar={toggleSidebar}
        canGoBack={canGoBack(history)}
        canGoForward={canGoForward(history)}
        onBack={() => travel(back)}
        onForward={() => travel(forward)}
      />
      <Sidebar
        hidden={!prefs.sidebar}
        sessions={sessions}
        openIds={openIds}
        selectedId={panes[focused]?.id ?? null}
        canSplit={panes.length < MAX_PANES}
        onOpen={(id, beside) => open(id, { beside })}
        onOpenSession={(session) => open(session.id, { session })}
        onOpenArchived={(id) => open(id, { archived: true })}
        onOpenFound={({ session, itemId }, how) => {
          // One further down than the list has loaded joins it, as the open session it now is.
          if (!how.archived) setSessions((state) => (state.byId.has(session.id) ? state : withSession(state, session)));
          open(session.id, { beside: how.beside, archived: how.archived, item: itemId });
        }}
        archiveTick={archiveTick}
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
                  archived={pane.archived}
                  at={pane.at}
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
