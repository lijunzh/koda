// The session list's state, from the server's answers and notifications.
import { describe, expect, it } from 'vitest';
import type { Session } from '../src/protocol/types.gen';
import { byFolder, EMPTY, folderName, listed, listedPage, statusLabel, title, withMessages, withPage, withSession } from '../src/renderer/sessions';

function session(id: string, extra: Partial<Session> = {}): Session {
  return { id, createdAt: 1, updatedAt: 1, ephemeral: false, preview: '', turns: [], ...extra } as Session;
}
const note = (method: string, params: object) => ({ jsonrpc: '2.0' as const, method, params });

describe('session list', () => {
  it('lists what a person started, as session/list does', () => {
    expect(listed(session('a', { source: 'cli' }))).toBe(true);
    expect(listed(session('a', { source: 'appServer' }))).toBe(true);
    expect(listed(session('a'))).toBe(true);
    expect(listed(session('a', { source: 'exec' }))).toBe(false);
    expect(listed(session('a', { source: 'subAgent' }))).toBe(false);
    expect(listed(session('a', { parentSessionId: 'p' }))).toBe(false);
    expect(listed(session('a', { ephemeral: true }))).toBe(false);
  });

  it('groups by folder, the most recently active folder and session first', () => {
    const state = withPage(
      EMPTY,
      {
        data: [
          session('a1', { cwd: '/work/a', updatedAt: 10 }),
          session('b1', { cwd: '/work/b', updatedAt: 30 }),
          session('a2', { cwd: '/work/a', updatedAt: 20 }),
          session('old', { updatedAt: 5 }),
        ],
        nextCursor: 'c1',
      },
      true,
    );
    expect(byFolder(state).map((g) => [g.folder, g.sessions.map((s) => s.id)])).toEqual([
      ['/work/b', ['b1']],
      ['/work/a', ['a2', 'a1']],
      [null, ['old']],
    ]);
    expect(state.nextCursor).toBe('c1');
    // A next page adds; a fresh one replaces.
    const more = withPage(state, { data: [session('c', { cwd: '/c' })], nextCursor: null }, false);
    expect([...more.byId.keys()]).toContain('a1');
    expect(more.nextCursor).toBeNull();
    expect([...withPage(state, { data: [], nextCursor: null }, true).byId.keys()]).toEqual([]);
  });

  it('follows sessions other clients start, rename, run and delete', () => {
    let state = withPage(EMPTY, { data: [session('a', { cwd: '/a', updatedAt: 1 })] }, true);
    state = withMessages(state, [
      note('session/started', { session: session('b', { cwd: '/b', updatedAt: 2, source: 'cli' }) }),
      note('session/started', { session: session('x', { source: 'exec' }) }),
      note('session/name/updated', { sessionId: 'a', sessionName: 'Fix the parser' }),
      note('session/status/changed', { sessionId: 'a', status: { type: 'active', activeFlags: [] } }),
    ], 100);
    expect([...state.byId.keys()].sort()).toEqual(['a', 'b']);
    expect(state.byId.get('a')).toMatchObject({ name: 'Fix the parser', updatedAt: 100 });
    expect(statusLabel(state.byId.get('a')!.status)).toBe('working');
    // Activity moves the folder to the top.
    expect(byFolder(state)[0].folder).toBe('/a');
    state = withMessages(state, [note('session/deleted', { sessionId: 'b' })]);
    expect([...state.byId.keys()]).toEqual(['a']);
  });

  it("names a session by its first prompt until the server names it", () => {
    let state = withPage(EMPTY, { data: [session('a')] }, true);
    const turn = { id: 't', status: 'inProgress', items: [{ type: 'userMessage', id: 'u', content: [{ type: 'text', text: 'Fix the parser' }] }] };
    state = withMessages(state, [note('turn/started', { sessionId: 'a', turn })]);
    expect(title(state.byId.get('a')!)).toBe('Fix the parser');
    // Or from the prompt's own item, when the turn's start doesn't carry it.
    let other = withPage(EMPTY, { data: [session('b')] }, true);
    other = withMessages(other, [note('item/started', { sessionId: 'b', turnId: 't', item: turn.items[0] })]);
    expect(title(other.byId.get('b')!)).toBe('Fix the parser');
    state = withMessages(state, [note('session/name/updated', { sessionId: 'a', sessionName: 'Parser fix' })]);
    expect(title(state.byId.get('a')!)).toBe('Parser fix');
  });

  it('puts an archived session away, and lists one brought back', () => {
    let state = withPage(EMPTY, { data: [session('a', { cwd: '/a' }), session('b', { cwd: '/b' })] }, true);
    state = withMessages(state, [note('session/archived', { sessionId: 'a' })]);
    expect([...state.byId.keys()]).toEqual(['b']);
    // `session/unarchived` names the session only: the app reads it, then lists it.
    expect(withMessages(state, [note('session/unarchived', { sessionId: 'a' })])).toBe(state);
    state = withSession(state, session('a', { cwd: '/a', name: 'Back again' }));
    expect(title(state.byId.get('a')!)).toBe('Back again');
    // A sub-agent's session comes back with its parent, and isn't listed.
    expect(withSession(state, session('child', { parentSessionId: 'a' }))).toBe(state);
  });

  it('shows the Archived list what the session list would show', () => {
    const page = { data: [session('a', { name: 'Kept' }), session('x', { source: 'exec' as const }), session('b')] };
    expect(listedPage(page).map((s) => s.id)).toEqual(['a', 'b']);
  });

  it('leaves the state alone for other messages', () => {
    const state = withPage(EMPTY, { data: [session('a')] }, true);
    expect(withMessages(state, [note('turn/started', {}), note('session/deleted', { sessionId: 'zz' })])).toBe(state);
  });

  it('says what a session is doing and what it is called', () => {
    expect(statusLabel(null)).toBe('');
    expect(statusLabel({ type: 'notLoaded' })).toBe('');
    expect(statusLabel({ type: 'idle' })).toBe('open');
    expect(statusLabel({ type: 'active', activeFlags: ['waitingOnApproval'] })).toBe('needs you');
    expect(title({ name: 'Named', preview: 'first' })).toBe('Named');
    expect(title({ name: null, preview: 'first prompt' })).toBe('first prompt');
    expect(title({})).toBe('New session');
    expect(folderName('/Users/me/repo/koda')).toBe('koda');
    expect(folderName(null)).toBe('No folder');
  });
});
