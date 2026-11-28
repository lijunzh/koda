// The session list's state, from the server's answers and notifications.
import { describe, expect, it } from 'vitest';
import type { Session } from '../src/protocol/types.gen';
import { briefAfter, briefOf, byFolder, confidentialAfter, confidentialTip, CONFIDENTIAL_TIP, EMPTY, folderName, listed, listedPage, NO_BRIEF, statusLabel, title, withMessages, withPage, withSession } from '../src/renderer/sessions';

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

  it('follows a session marked confidential and unmarked, by the user or by Koda', () => {
    // The server leaves `confidential` out for a session that isn't.
    let state = withPage(EMPTY, { data: [session('a'), session('b', { confidential: true })] }, true);
    const marks = () => [...state.byId.values()].map((s) => [s.id, s.confidential]);
    expect(marks()).toEqual([['a', false], ['b', true]]);
    state = withMessages(state, [
      note('session/confidential/updated', { sessionId: 'a', confidential: true }),
      note('session/confidential/updated', { sessionId: 'b', confidential: false }),
      note('session/confidential/updated', { sessionId: 'elsewhere', confidential: true }),
    ]);
    expect(marks()).toEqual([['a', true], ['b', false]]);
    // Told what it already shows, the list is left as it is.
    expect(withMessages(state, [note('session/confidential/updated', { sessionId: 'a', confidential: true })])).toBe(state);
    // A fresh listing decides: one that no longer says it is confidential isn't.
    state = withPage(state, { data: [session('a')] }, false);
    expect(marks()).toEqual([['a', false], ['b', false]]);
    // The Archived list, and a session brought back, carry the mark too.
    expect(listedPage({ data: [session('c', { confidential: true })] })[0].confidential).toBe(true);
    expect(withSession(state, session('b', { confidential: true })).byId.get('b')!.confidential).toBe(true);
  });

  it("gives a pane its session's mark: the last word about that session", () => {
    const told = (sessionId: string, confidential: boolean) => note('session/confidential/updated', { sessionId, confidential });
    expect(confidentialAfter(false, 'a', [])).toBe(false);
    expect(confidentialAfter(true, 'a', [note('session/name/updated', { sessionId: 'a', sessionName: 'x' })])).toBe(true);
    expect(confidentialAfter(false, 'a', [told('a', true)])).toBe(true);
    expect(confidentialAfter(true, 'a', [told('a', false)])).toBe(false);
    expect(confidentialAfter(false, 'a', [told('a', true), told('a', false), told('b', true)])).toBe(false);
    expect(confidentialAfter(false, 'a', [told('b', true)])).toBe(false);
  });

  it("gives a pane its session's brief and draft: the last word about them, and none once the mark is off", () => {
    const told = (sessionId: string, brief: string | null, draft: string | null) =>
      note('session/confidential/brief/updated', { sessionId, brief, draft });
    const unmarked = (sessionId: string) => note('session/confidential/updated', { sessionId, confidential: false });
    expect(briefOf(session('a'))).toEqual(NO_BRIEF);
    expect(briefOf(session('a', { confidentialBrief: 'Planning a trip', confidentialBriefDraft: 'Travel' }))).toEqual({
      brief: 'Planning a trip',
      draft: 'Travel',
    });
    const drafted = briefAfter(NO_BRIEF, 'a', [told('a', null, 'Planning a trip'), told('b', 'Elsewhere', null)]);
    expect(drafted).toEqual({ brief: null, draft: 'Planning a trip' });
    // Accepted: the draft is gone. Cleared: both are.
    expect(briefAfter(drafted, 'a', [told('a', 'Planning a trip', null)])).toEqual({ brief: 'Planning a trip', draft: null });
    expect(briefAfter(drafted, 'a', [told('a', null, null)])).toEqual(NO_BRIEF);
    // Marking says nothing of the brief; unmarking takes it and the draft.
    expect(briefAfter(drafted, 'a', [note('session/confidential/updated', { sessionId: 'a', confidential: true })])).toBe(drafted);
    expect(briefAfter(drafted, 'a', [unmarked('b')])).toBe(drafted);
    expect(briefAfter({ brief: 'Planning a trip', draft: null }, 'a', [unmarked('a')])).toEqual(NO_BRIEF);
    expect(briefAfter(drafted, 'a', [unmarked('a'), told('a', null, 'Again')])).toEqual({ brief: null, draft: 'Again' });
  });

  it("keeps a listed session's brief for its lock to say, never a draft", () => {
    let state = withPage(EMPTY, { data: [session('a', { confidential: true, confidentialBriefDraft: 'A draft' })] }, true);
    const brief = () => state.byId.get('a')!.brief;
    expect(brief()).toBeNull();
    state = withMessages(state, [note('session/confidential/brief/updated', { sessionId: 'a', brief: null, draft: 'Another draft' })]);
    expect(brief()).toBeNull();
    state = withMessages(state, [
      note('session/confidential/brief/updated', { sessionId: 'a', brief: 'Planning a trip', draft: null }),
      note('session/confidential/brief/updated', { sessionId: 'elsewhere', brief: 'Not listed', draft: null }),
    ]);
    expect(brief()).toBe('Planning a trip');
    expect(confidentialTip(brief())).toBe(`${CONFIDENTIAL_TIP}. Models on remote providers are told: "Planning a trip"`);
    expect(confidentialTip(null)).toBe(CONFIDENTIAL_TIP);
    // The mark coming off takes the brief with it.
    state = withMessages(state, [note('session/confidential/updated', { sessionId: 'a', confidential: false })]);
    expect(brief()).toBeNull();
    expect(listedPage({ data: [session('c', { confidential: true, confidentialBrief: 'Taxes' })] })[0].brief).toBe('Taxes');
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
