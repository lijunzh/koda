// The session list's state, as pure functions of the server's answers and
// notifications. The server sends every connection the sessions others start,
// rename, delete, run and mark confidential (`session/started`,
// `session/name/updated`, `session/deleted`, `session/archived`,
// `session/status/changed`, `session/confidential/updated`), so the
// list stays current without opening any session. An archived session leaves
// the list, as Claude Code's desktop app puts it away; the Archived list is
// asked for on its own (`session/list` with `archived`).
import type { Session, SessionListResponse, SessionStatus } from '../protocol/types.gen';
import type { RpcMessage } from './bridge';

export interface ListedSession {
  id: string;
  name: string | null;
  preview: string;
  cwd: string | null;
  updatedAt: number;
  status: SessionStatus | null;
  /** Marked confidential: its row and its title show a lock. */
  confidential: boolean;
}

/** What the lock on a confidential session says. */
export const CONFIDENTIAL_TIP = 'Confidential: only a local model can read this session from another session';

export interface SessionsState {
  byId: ReadonlyMap<string, ListedSession>;
  /** The next page's cursor; `null` when there's no more. */
  nextCursor: string | null;
  loaded: boolean;
}

export const EMPTY: SessionsState = { byId: new Map(), nextCursor: null, loaded: false };

/**
 * Whether the list shows `session`: as `session/list` does, the sessions a
 * person started (the TUI, an app, or from before sources were recorded),
 * not `koda exec` runs, sub-agents' sessions or side conversations.
 */
export function listed(session: Session): boolean {
  if (session.ephemeral || session.parentSessionId) return false;
  return session.source == null || session.source === 'cli' || session.source === 'appServer' || session.source === 'unknown';
}

function entry(session: Session, before?: ListedSession): ListedSession {
  return {
    id: session.id,
    name: session.name ?? before?.name ?? null,
    preview: session.preview || before?.preview || '',
    cwd: session.cwd ?? before?.cwd ?? null,
    updatedAt: Math.max(session.updatedAt, before?.updatedAt ?? 0),
    status: session.status ?? before?.status ?? null,
    // A session always says: the server leaves the field out for one that isn't.
    confidential: session.confidential === true,
  };
}

/**
 * Whether session `id` is confidential after `batch`, having been
 * `confidential` before it: the last `session/confidential/updated` for it
 * decides. For a pane, which shows the mark for sessions the list doesn't
 * have (an archived one).
 */
export function confidentialAfter(confidential: boolean, id: string, batch: readonly RpcMessage[]): boolean {
  for (const message of batch) {
    if (message.method !== 'session/confidential/updated') continue;
    const params = message.params as { sessionId?: string; confidential?: boolean } | undefined;
    if (params?.sessionId === id) confidential = params.confidential === true;
  }
  return confidential;
}

/** Add a page of `session/list`; `fresh` replaces what was there. */
export function withPage(state: SessionsState, page: SessionListResponse, fresh: boolean): SessionsState {
  const byId = new Map(fresh ? [] : state.byId);
  for (const session of page.data) {
    if (listed(session)) byId.set(session.id, entry(session, byId.get(session.id)));
  }
  return { byId, nextCursor: page.nextCursor ?? null, loaded: true };
}

/** Apply the server's notifications about sessions; others are ignored. */
export function withMessages(state: SessionsState, batch: readonly RpcMessage[], now = Date.now() / 1000): SessionsState {
  let byId: Map<string, ListedSession> | null = null;
  const edit = () => (byId ??= new Map(state.byId));
  for (const message of batch) {
    const params = message.params as Record<string, unknown> | undefined;
    switch (message.method) {
      case 'session/started': {
        const session = params?.session as Session | undefined;
        if (session && listed(session)) edit().set(session.id, entry(session, state.byId.get(session.id)));
        break;
      }
      case 'session/name/updated': {
        const current = (byId ?? state.byId).get(params?.sessionId as string);
        if (current) edit().set(current.id, { ...current, name: params?.sessionName as string });
        break;
      }
      case 'session/confidential/updated': {
        const current = (byId ?? state.byId).get(params?.sessionId as string);
        const confidential = params?.confidential === true;
        if (current && current.confidential !== confidential) edit().set(current.id, { ...current, confidential });
        break;
      }
      case 'session/deleted':
      case 'session/archived':
        if ((byId ?? state.byId).has(params?.sessionId as string)) edit().delete(params?.sessionId as string);
        break;
      case 'turn/started':
      case 'item/started': {
        // A session's first prompt is its name in the list until it has one.
        const current = (byId ?? state.byId).get(params?.sessionId as string);
        const turn = message.method === 'turn/started' ? params?.turn : { items: [params?.item] };
        const prompt = current && !current.preview ? firstPrompt(turn) : '';
        if (current && prompt) edit().set(current.id, { ...current, preview: prompt });
        break;
      }
      case 'session/status/changed': {
        const current = (byId ?? state.byId).get(params?.sessionId as string);
        // A change of status is activity: it moves the session up.
        if (current) edit().set(current.id, { ...current, status: params?.status as SessionStatus, updatedAt: now });
        break;
      }
    }
  }
  return byId ? { ...state, byId } : state;
}

/** With `session` in the list: one brought back from the archive. */
export function withSession(state: SessionsState, session: Session): SessionsState {
  if (!listed(session)) return state;
  const byId = new Map(state.byId);
  byId.set(session.id, entry(session, byId.get(session.id)));
  return { ...state, byId };
}

/** The sessions of a `session/list` page that a list shows, in its order. */
export function listedPage(page: SessionListResponse): ListedSession[] {
  return page.data.filter(listed).map((session) => entry(session));
}

export interface FolderGroup {
  /** The folder; `null` for sessions too old to have recorded one. */
  folder: string | null;
  sessions: ListedSession[];
}

/** The sessions by folder: the most recently active folder first, its newest session first. */
export function byFolder(state: SessionsState): FolderGroup[] {
  const groups = new Map<string | null, ListedSession[]>();
  for (const session of state.byId.values()) {
    const list = groups.get(session.cwd) ?? [];
    list.push(session);
    groups.set(session.cwd, list);
  }
  return [...groups.entries()]
    .map(([folder, sessions]) => ({ folder, sessions: sessions.sort((a, b) => b.updatedAt - a.updatedAt) }))
    .sort((a, b) => b.sessions[0].updatedAt - a.sessions[0].updatedAt);
}

/** What a session's status means to the user: nothing when it's only stored. */
export function statusLabel(status: SessionStatus | null): string {
  if (!status || status.type === 'notLoaded') return '';
  if (status.type === 'idle') return 'open';
  return status.activeFlags.length ? 'needs you' : 'working';
}

/** The name a session goes by: its title, else its first prompt, else that it's new. */
export function title(session: { name?: string | null; preview?: string }): string {
  return session.name || session.preview || 'New session';
}

/** A folder's last part, for a group's heading. */
export function folderName(folder: string | null): string {
  if (!folder) return 'No folder';
  return folder.split('/').filter(Boolean).pop() ?? folder;
}

/** The text of a turn's first user message, if it has one. */
function firstPrompt(turn: unknown): string {
  const items = (turn as { items?: Array<{ type: string; content?: Array<{ type: string; text?: string }> }> } | undefined)?.items;
  const message = items?.find((item) => item.type === 'userMessage');
  return message?.content?.find((part) => part.type === 'text')?.text?.trim() ?? '';
}
