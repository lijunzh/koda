// Starting and deleting sessions, which the main process does with the
// system's dialogs.
import { describe, expect, it } from 'vitest';
import { deleteSession, newSession, type SessionActionDeps } from '../src/main/sessions';

function deps(over: Partial<SessionActionDeps> = {}) {
  const sent: Array<[string, unknown]> = [];
  const all: SessionActionDeps = {
    request: async (method, params) => {
      sent.push([method, params]);
      if (method === 'session/start') return { session: { id: 'new', cwd: (params as { cwd: string }).cwd } };
      if (method === 'session/read') return { session: { id: 's1', name: null, preview: 'fix the bug' } };
      return {};
    },
    pickFolder: async () => '/work/project',
    confirmDelete: async () => true,
    ...over,
  };
  return { all, sent };
}

describe('session actions', () => {
  it('starts a session in the folder the user picked, and in no other', async () => {
    const { all, sent } = deps();
    expect(await newSession(all)).toMatchObject({ id: 'new', cwd: '/work/project' });
    expect(sent).toEqual([['session/start', { cwd: '/work/project' }]]);
  });

  it('starts nothing when the user cancels the picker', async () => {
    const { all, sent } = deps({ pickFolder: async () => null });
    expect(await newSession(all)).toBeNull();
    expect(sent).toEqual([]);
  });

  it('deletes only what the user confirmed, named as the server stores it', async () => {
    let asked = '';
    const { all, sent } = deps({ confirmDelete: async (label) => ((asked = label), true) });
    expect(await deleteSession(all, 's1')).toBe(true);
    expect(asked).toBe('fix the bug');
    expect(sent.map(([m]) => m)).toEqual(['session/read', 'session/delete']);

    const declined = deps({ confirmDelete: async () => false });
    expect(await deleteSession(declined.all, 's1')).toBe(false);
    expect(declined.sent.map(([m]) => m)).toEqual(['session/read']);
  });

  it('refuses an id that is not one', async () => {
    await expect(deleteSession(deps().all, 42)).rejects.toThrow(/session id/);
    await expect(deleteSession(deps().all, '')).rejects.toThrow(/session id/);
  });
});
