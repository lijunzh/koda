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
  'turn/steer',
  'modelProvider/list',
  'mcpServerStatus/list',
  'agent/list',
  'skills/list',
]);

/** Methods the page may call when its parameters pass a check. */
const CHECKED: Readonly<Record<string, (params: Record<string, unknown>) => string | null>> = {
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
