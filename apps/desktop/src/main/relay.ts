// What the page may ask Koda's server for. The server treats every client as
// the user (as Codex's does): `session/start` can name an MCP server's
// command, `baseUrl` sends the stored key to that address, and the answer to
// an approval runs a command. So the main process checks each request from
// the page rather than passing it on, and anything not listed is refused.
//
// Grows with the app: a pull request that needs a method adds it here, with
// its checks and a test.

/** Methods the page may call with any parameters. */
const PAGE_METHODS: ReadonlySet<string> = new Set([
  'state/read',
  'session/list',
  'session/read',
  'session/turns/list',
  'session/name/set',
  // Archiving is undone with unarchive, so the page may ask for both; a
  // delete, which isn't, is the main process's to confirm.
  'session/archive',
  'session/unarchive',
  'session/goal/get',
  'session/goal/set',
  'session/goal/clear',
  'session/compact/start',
  'session/revert',
  'session/unsubscribe',
  'session/backgroundTerminals/list',
  'session/backgroundTerminals/clean',
  'session/backgroundTerminals/terminate',
  'session/memory/read',
  'turn/interrupt',
  'modelProvider/list',
  'mcpServerStatus/list',
  'agent/list',
  'skills/list',
]);

/** Methods the page may call when its parameters pass a check. */
const CHECKED: Record<string, (params: Record<string, unknown>) => string | null> = {
  // `baseUrl` would send the provider's stored key to that address.
  'model/list': (params) => (params.baseUrl === undefined ? null : 'model/list takes no baseUrl from the page'),
  // Opening a session by its id alone: the server opens it in the folder it
  // was started in. A `cwd` would move where its commands may write, and
  // `config`, `baseUrl` or `approvalPolicy` would change what it runs with.
  'session/resume': (params) => {
    const extra = Object.keys(params).filter((key) => key !== 'sessionId');
    if (extra.length) return `session/resume takes only sessionId from the page, not ${extra.join(', ')}`;
    return typeof params.sessionId === 'string' ? null : 'session/resume needs a sessionId';
  },
};

/**
 * Why a prompt from the page can't go: only typed text for now. Images,
 * mentioned files and skills come with checks of their own (a mentioned
 * file must be in the session's folder).
 */
function promptRefusal(method: string, params: Record<string, unknown>, keys: readonly string[]): string | null {
  const extra = Object.keys(params).filter((key) => !keys.includes(key));
  if (extra.length) return `${method} takes ${keys.join(', ')} from the page, not ${extra.join(', ')}`;
  if (typeof params.sessionId !== 'string') return `${method} needs a sessionId`;
  const input = params.input;
  if (!Array.isArray(input) || input.length === 0) return `${method} needs input`;
  for (const part of input) {
    const p = part as Record<string, unknown> | null;
    if (!p || p.type !== 'text' || typeof p.text !== 'string') return `${method} takes only text from the page`;
  }
  return null;
}

// A session's model and effort may change from the page, and its mode may
// go down to Plan; raising it to Auto lets Koda edit and run commands
// without asking, so that goes through the main process, which asks first
// (`setMode` in sessions.ts).
CHECKED['session/settings/update'] = (params) => {
  const extra = Object.keys(params).filter((key) => !['sessionId', 'model', 'effort', 'mode'].includes(key));
  if (extra.length) return `session/settings/update takes sessionId, model, effort and mode, not ${extra.join(', ')}`;
  if (typeof params.sessionId !== 'string') return 'session/settings/update needs a sessionId';
  if (params.mode !== undefined && params.mode !== null && params.mode !== 'plan') {
    return 'the page may only change the mode to plan; Auto is confirmed by the app';
  }
  return null;
};

// A fork is a copy in the same folder with the same settings: the page names
// the session, and nothing else.
CHECKED['session/fork'] = (params) => {
  const extra = Object.keys(params).filter((key) => key !== 'sessionId');
  if (extra.length) return `session/fork takes only sessionId from the page, not ${extra.join(', ')}`;
  return typeof params.sessionId === 'string' ? null : 'session/fork needs a sessionId';
};

// Marking a session confidential, or unmarking it: the page names the
// session and says which, and nothing else. Unmarking lets other sessions'
// models read it again, which gives the page nothing it hasn't: it reads
// every session already (`session/read`), and the mark is the user's own,
// set from a menu, with no dialog to confirm as a delete has.
CHECKED['session/confidential/set'] = (params) => {
  const extra = Object.keys(params).filter((key) => key !== 'sessionId' && key !== 'confidential');
  if (extra.length) return `session/confidential/set takes sessionId and confidential from the page, not ${extra.join(', ')}`;
  if (typeof params.sessionId !== 'string') return 'session/confidential/set needs a sessionId';
  return typeof params.confidential === 'boolean' ? null : 'session/confidential/set needs confidential, true or false';
};

// A confidential session's brief, the one line models on remote providers
// are told of it: the page names the session and gives the text the user
// accepted or typed, or `null` for none, and nothing else. The server
// checks the text's length and that the session is confidential.
CHECKED['session/confidential/brief/set'] = (params) => {
  const extra = Object.keys(params).filter((key) => key !== 'sessionId' && key !== 'brief');
  if (extra.length) return `session/confidential/brief/set takes sessionId and brief from the page, not ${extra.join(', ')}`;
  if (typeof params.sessionId !== 'string') return 'session/confidential/brief/set needs a sessionId';
  return typeof params.brief === 'string' || params.brief === null ? null : 'session/confidential/brief/set needs brief, a text or null';
};

// Searching what was said in the stored sessions: the page sends the text,
// whether to look in the archive instead, and how many to return, in which
// order. It reads no more than `session/read` gives the page already; a
// search of tool output, or one narrowed some other way, isn't the page's
// to ask for until it has a use for it.
CHECKED['session/search'] = (params) => {
  const keys = ['searchTerm', 'archived', 'limit', 'sortKey'];
  const extra = Object.keys(params).filter((key) => !keys.includes(key));
  if (extra.length) return `session/search takes ${keys.join(', ')} from the page, not ${extra.join(', ')}`;
  return typeof params.searchTerm === 'string' ? null : 'session/search needs a searchTerm';
};

CHECKED['turn/start'] = (params) =>
  // No `mode`: a turn can't raise its own trust; nor an `outputSchema`.
  promptRefusal('turn/start', params, ['sessionId', 'input', 'clientUserMessageId']);
CHECKED['turn/steer'] = (params) =>
  promptRefusal('turn/steer', params, ['sessionId', 'expectedTurnId', 'input', 'clientUserMessageId']);

/** Every method the relay knows: for the test that each still exists in the protocol. */
export const RELAYED_METHODS: readonly string[] = [...PAGE_METHODS, ...Object.keys(CHECKED)];

/** Why the page may not send `method` with `params`, or `null` if it may. */
export function refusal(method: unknown, params: unknown): string | null {
  if (typeof method !== 'string') return 'not a method name';
  if (params !== undefined && (typeof params !== 'object' || params === null || Array.isArray(params))) {
    return `${method}: params must be an object`;
  }
  if (PAGE_METHODS.has(method)) return null;
  const check = CHECKED[method];
  if (check) return check((params ?? {}) as Record<string, unknown>);
  return `${method} isn't open to the page`;
}
