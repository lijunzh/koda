// Starting and deleting sessions, which the page asks for but the main
// process carries out: a new session's folder decides where its commands may
// write, so it comes only from the system's folder picker, never from the
// page; and deleting one is for good, so the user confirms it in a system
// dialog that names the session as the server stores it.

/** What these actions need: the server, and the system dialogs. */
export interface SessionActionDeps {
  request(method: string, params: unknown): Promise<unknown>;
  /** The folder the user picked, or `null` if they cancelled. */
  pickFolder(): Promise<string | null>;
  /** Whether the user confirmed deleting the session called `label`. */
  confirmDelete(label: string): Promise<boolean>;
}

interface SessionShape {
  id: string;
  name?: string | null;
  preview?: string;
}

/** A new session in a folder the user picks; `null` if they cancelled. */
export async function newSession(deps: SessionActionDeps): Promise<unknown> {
  const folder = await deps.pickFolder();
  if (!folder) return null;
  const started = (await deps.request('session/start', { cwd: folder })) as { session: SessionShape };
  return started.session;
}

/** Delete session `id` once the user confirms it. Whether it was deleted. */
export async function deleteSession(deps: SessionActionDeps, id: unknown): Promise<boolean> {
  if (typeof id !== 'string' || !id) throw new Error('refused: a session id');
  const read = (await deps.request('session/read', { sessionId: id })) as { session: SessionShape };
  const label = read.session.name || read.session.preview || 'this session';
  if (!(await deps.confirmDelete(label))) return false;
  await deps.request('session/delete', { sessionId: id });
  return true;
}
