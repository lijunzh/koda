// The session list, as Claude Code's desktop app has it: New session and
// Search at the top; every folder's sessions, the most recently active
// first; a mark on a session that is working or waiting for you; and a "…"
// menu on each (also on a right-click) to open it beside another, rename,
// fork, mark confidential, archive or delete it; a confidential session
// carries a lock. Option-click opens a session in split view.
// "Archived" shows the sessions put away, to read, unarchive or delete.
// Search narrows the list by title as you type and, under those, lists the
// sessions in which the text was said (`search.ts`).
import { useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import { call } from './bridge';
import { ArchiveIcon, ConfidentialMark, GearIcon, PlusIcon, SearchIcon } from './Icons';
import { type Entry, MenuButton, MenuList } from './Menu';
import { answered, asking, emptyLine, failed as searchFailed, type Found, IDLE, marked, SEARCH_DELAY_MS, SEARCH_LIMIT, type SearchState, searchTerm, unshown } from './search';
import { byFolder, confidentialTip, folderName, type ListedSession, listedPage, type SessionsState, statusLabel, title } from './sessions';

/** How many archived sessions the list asks for: the most recent ones. */
const ARCHIVED_SHOWN = 100;

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
  /** Open an archived session, to read. */
  onOpenArchived(id: string): void;
  /** Open a session search found, at what matched: `beside`, in a new pane; an `archived` one, to read. */
  onOpenFound(found: Found, how: { beside: boolean; archived: boolean }): void;
  /** Goes up when a session is archived, unarchived or deleted. */
  archiveTick: number;
  onNew(): void;
  onMore(): void;
  onSettings(): void;
  /** The server's version while connected; `null` while reconnecting. */
  connectedTo: string | null;
  /** Hidden with Cmd+B: it keeps its search and scroll for when it's back. */
  hidden: boolean;
}

export function Sidebar({ sessions, openIds, selectedId, canSplit, onOpen, onOpenSession, onOpenArchived, onOpenFound, archiveTick, onNew, onMore, onSettings, connectedTo, hidden }: Props) {
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

  // The Archived list, asked for while it shows and again when it changes.
  const [archivedShown, setArchivedShown] = useState(false);
  const [archived, setArchived] = useState<ListedSession[] | null>(null);
  useEffect(() => {
    if (!archivedShown) return;
    let current = true;
    call('session/list', { archived: true, limit: ARCHIVED_SHOWN, sortKey: 'updated_at' }).then(
      (page) => current && setArchived(listedPage(page)),
      (e: Error) => current && setError(e.message),
    );
    return () => {
      current = false;
    };
  }, [archivedShown, archiveTick]);

  const wanted = query?.trim().toLowerCase() ?? '';
  const failed = (e: Error) => setError(e.message);

  // What was said in sessions, asked for once the typing rests; with the
  // Archived list showing, in the archive. Each change of the query starts
  // over, and an answer to an earlier one is dropped (`answered`).
  const term = searchTerm(query);
  const [search, setSearch] = useState<SearchState>(IDLE);
  useEffect(() => {
    const asked = term ? { term, archived: archivedShown } : null;
    setSearch(asking(asked));
    if (!asked) return;
    const timer = setTimeout(() => {
      call('session/search', {
        searchTerm: asked.term,
        limit: SEARCH_LIMIT,
        sortKey: 'updated_at',
        ...(asked.archived ? { archived: true } : {}),
      }).then(
        (answer) => setSearch((state) => answered(state, asked, answer)),
        (e: Error) => setSearch((state) => searchFailed(state, asked, e.message)),
      );
    }, SEARCH_DELAY_MS);
    return () => clearTimeout(timer);
  }, [term, archivedShown, archiveTick]);
  // Mark a session confidential, or unmark it: in every session's menu.
  const confidentialEntry = (session: ListedSession): Entry => ({
    type: 'item',
    id: 'confidential',
    label: session.confidential ? 'Unmark confidential' : 'Mark confidential',
    onPick: () =>
      void call('session/confidential/set', { sessionId: session.id, confidential: !session.confidential }).catch(failed),
  });
  const archivedEntries = (session: ListedSession): Entry[] => [
    { type: 'item', id: 'open', label: 'Open', onPick: () => onOpenArchived(session.id) },
    confidentialEntry(session),
    {
      type: 'item',
      id: 'unarchive',
      label: 'Unarchive',
      onPick: () => void call('session/unarchive', { sessionId: session.id }).catch(failed),
    },
    { type: 'separator' },
    { type: 'item', id: 'delete', label: 'Delete', danger: true, onPick: () => void window.koda.deleteSession(session.id) },
  ];
  const groups = byFolder(sessions)
    .map((group) => ({
      ...group,
      sessions: wanted ? group.sessions.filter((s) => title(s).toLowerCase().includes(wanted)) : group.sessions,
    }))
    .filter((group) => group.sessions.length);
  const archivedMatches = (archived ?? []).filter((s) => !wanted || title(s).toLowerCase().includes(wanted));
  // "In conversations": not the sessions the title filter shows already.
  const titleMatches = archivedShown ? archivedMatches : groups.flatMap((group) => group.sessions);
  const results = unshown(search.found, new Set(titleMatches.map((s) => s.id)));
  const empty = emptyLine(query, titleMatches.length, results.length, search);
  const conversations = term !== null && (
    <>
      {search.waiting && (
        <p className="empty searching" data-testid="searching">
          Searching…
        </p>
      )}
      {search.error && <p className="problem empty">{search.error}</p>}
      {results.length > 0 && (
        <section className="group" data-testid="content-results">
          <h2>In conversations</h2>
          <ul>
            {results.map((found) => (
              <li key={found.session.id} className="session-row">
                <button
                  className={found.session.id === selectedId ? 'session found selected' : 'session found'}
                  title={found.session.cwd ?? undefined}
                  onClick={(e) => onOpenFound(found, { beside: e.altKey && !archivedShown, archived: archivedShown })}
                >
                  <span className="found-head">
                    <span className="session-title">{title(found.session)}</span>
                    {found.session.confidential && <ConfidentialMark tip={confidentialTip(found.session.confidentialBrief)} />}
                    <span className="session-folder">{folderName(found.session.cwd ?? null)}</span>
                  </span>
                  <span className="snippet">
                    {marked(found.snippet, term).map((part, i) => (part.hit ? <mark key={i}>{part.text}</mark> : part.text))}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}
    </>
  );

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
    confidentialEntry(session),
    { type: 'separator' },
    {
      type: 'item',
      id: 'archive',
      label: 'Archive',
      onPick: () => void call('session/archive', { sessionId: session.id }).catch(failed),
    },
    { type: 'item', id: 'delete', label: 'Delete', danger: true, onPick: () => void window.koda.deleteSession(session.id) },
  ];

  return (
    <nav className="sidebar" aria-label="Sessions" hidden={hidden}>
      <div className="sidebar-top">
        <button className="side-action" onClick={onNew}>
          <PlusIcon />
          New session
        </button>
        {query === null ? (
          <button className="side-action" onClick={() => setQuery('')}>
            <SearchIcon />
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
        <button className="side-action" aria-pressed={archivedShown} onClick={() => setArchivedShown((shown) => !shown)}>
          <ArchiveIcon />
          Archived
        </button>
      </div>
      {archivedShown && (
        <div className="groups" data-testid="archived-list">
          {error && <p className="problem empty">{error}</p>}
          <section className="group">
            <h2>Archived</h2>
            {archived && archived.length === 0 && <p className="empty">No archived sessions.</p>}
            {archived && archived.length > 0 && wanted && empty && <p className="empty">{empty}</p>}
            <ul>
              {archivedMatches.map((session) => (
                <li key={session.id} className="session-row">
                  <button
                    className={session.id === selectedId ? 'session selected' : 'session'}
                    title={session.cwd ?? undefined}
                    onClick={() => onOpenArchived(session.id)}
                  >
                    <span className="session-title">{title(session)}</span>
                    {session.confidential && <ConfidentialMark tip={confidentialTip(session.brief)} />}
                    <span className="session-folder">{folderName(session.cwd)}</span>
                  </button>
                  <MenuButton label="Session actions" entries={archivedEntries(session)} className="row-menu" right>
                    <span aria-hidden>⋯</span>
                  </MenuButton>
                </li>
              ))}
            </ul>
          </section>
          {conversations}
        </div>
      )}
      <div className="groups" hidden={archivedShown}>
        {sessions.loaded && !archivedShown && empty && <p className="empty">{empty}</p>}
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
                      {session.confidential && <ConfidentialMark tip={confidentialTip(session.brief)} />}
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
        {!archivedShown && conversations}
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
          <GearIcon />
        </button>
      </div>
    </nav>
  );
}
