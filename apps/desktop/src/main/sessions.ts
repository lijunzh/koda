// Starting and deleting sessions, which the page asks for but the main
// process carries out: a new session's folder decides where its commands may
// write, so it is one the user picked in the system's folder picker, or one
// that already has a session, never a path the page made up; its provider is
// one the server lists, and never comes with an endpoint; and deleting one is
// for good, so the user confirms it in a system dialog that names the session
// as the server stores it.

/** What these actions need: the server, and the system dialogs. */
export interface SessionActionDeps {
  request(method: string, params: unknown): Promise<unknown>;
  /** The folder the user picked, or `null` if they cancelled. */
  pickFolder(): Promise<string | null>;
  /** Whether the user confirmed deleting the session called `label`. */
  confirmDelete(label: string): Promise<boolean>;
  /** Whether the user confirmed letting session `label` work in Auto. */
  confirmAuto(label: string): Promise<boolean>;
}

interface SessionShape {
  id: string;
  name?: string | null;
  preview?: string;
}

/** The folders the user picked in the system's dialog since the app started. */
export type PickedFolders = Set<string>;

/** Ask the user for a folder, and remember it as one a session may start in. */
export async function pickFolder(deps: SessionActionDeps, picked: PickedFolders): Promise<string | null> {
  const folder = await deps.pickFolder();
  if (folder) picked.add(folder);
  return folder;
}

/**
 * A new session in `folder`, on `provider` and `model` when the page names
 * them. Refused unless the user picked the folder in the system's dialog or a
 * session already works in it, and unless the provider is one the server
 * lists: the page chooses among what the user and the server already allow.
 */
export async function startSession(deps: SessionActionDeps, picked: PickedFolders, options: unknown): Promise<unknown> {
  const o = (options ?? {}) as Record<string, unknown>;
  const extra = Object.keys(o).filter((key) => !['folder', 'provider', 'model'].includes(key));
  if (extra.length) throw new Error(`refused: a new session takes folder, provider and model, not ${extra.join(', ')}`);
  const { folder, provider, model } = o;
  if (typeof folder !== 'string' || !folder) throw new Error('refused: a folder');
  if (!picked.has(folder)) {
    const known = (await deps.request('session/list', { cwd: folder, limit: 1 })) as { data?: unknown[] };
    if (!known.data?.length) throw new Error('refused: a folder the user picked, or one that has a session');
  }
  const params: Record<string, unknown> = { cwd: folder };
  if (provider !== undefined) {
    if (typeof provider !== 'string') throw new Error('refused: a provider id');
    const listed = (await deps.request('modelProvider/list', {})) as { data?: Array<{ id: string }> };
    if (!listed.data?.some((p) => p.id === provider)) throw new Error('refused: a provider the server lists');
    params.modelProvider = provider;
  }
  if (model !== undefined) {
    if (typeof model !== 'string' || !model || model.length > 200) throw new Error('refused: a model id');
    params.model = model;
  }
  const started = (await deps.request('session/start', params)) as { session: SessionShape };
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

/**
 * Put session `id` in `mode`. Plan only lowers what Koda may do; Auto lets it
 * edit and run commands without asking (inside the sandbox), so the user
 * confirms it first in a system dialog. Whether the mode changed.
 */
export async function setMode(deps: SessionActionDeps, id: unknown, mode: unknown): Promise<boolean> {
  if (typeof id !== 'string' || !id) throw new Error('refused: a session id');
  if (mode !== 'auto' && mode !== 'plan') throw new Error('refused: auto or plan');
  if (mode === 'auto') {
    const read = (await deps.request('session/read', { sessionId: id })) as { session: SessionShape };
    const label = read.session.name || read.session.preview || 'this session';
    if (!(await deps.confirmAuto(label))) return false;
  }
  await deps.request('session/settings/update', { sessionId: id, mode });
  return true;
}
