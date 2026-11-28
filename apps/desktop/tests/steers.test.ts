// Text added to a running turn: what became of it, and what the prompt box
// gets back when the turn dropped it.
import { describe, expect, it } from 'vitest';
import { type SentSteer, settle, steerId, withDropped } from '../src/renderer/steers';
import type { ItemView, TurnView } from '../src/renderer/transcript';

const taken = (ids: string[]): ItemView => ({ type: 'userMessage', id: `user-${ids.join()}`, content: [], clientUserMessageIds: ids });
const turn = (id: string, status: TurnView['status'], items: ItemView[] = []): TurnView => ({
  id,
  status,
  startedAt: null,
  durationMs: null,
  error: null,
  items,
});
const steer = (id: string, text: string, turnId = 't1'): SentSteer => ({ id, turnId, text });

describe('steers', () => {
  it('waits while the turn runs and has not taken the text', () => {
    const sent = [steer('a', 'one')];
    expect(settle(sent, [turn('t1', 'inProgress')])).toEqual({ waiting: sent, dropped: [] });
  });

  it('forgets text the model has taken, in a running turn or an ended one', () => {
    const sent = [steer('a', 'one'), steer('b', 'two')];
    expect(settle(sent, [turn('t1', 'inProgress', [taken(['a'])])])).toEqual({ waiting: [sent[1]], dropped: [] });
    expect(settle(sent, [turn('t1', 'completed', [taken(['a', 'b'])])])).toEqual({ waiting: [], dropped: [] });
  });

  it('gives back, oldest first, what a stopped or failed turn never took', () => {
    const sent = [steer('a', 'one'), steer('b', 'two'), steer('c', 'three')];
    expect(settle(sent, [turn('t1', 'interrupted', [taken(['a'])])])).toEqual({ waiting: [], dropped: ['two', 'three'] });
    expect(settle(sent, [turn('t1', 'failed')])).toEqual({ waiting: [], dropped: ['one', 'two', 'three'] });
  });

  it("keeps a steer whose turn the transcript doesn't show", () => {
    const sent = [steer('a', 'one', 'elsewhere')];
    expect(settle(sent, [turn('t1', 'interrupted')])).toEqual({ waiting: sent, dropped: [] });
  });

  it('puts the text back in front of what is typed', () => {
    expect(withDropped(['one', 'two'], '')).toBe('one\n\ntwo');
    expect(withDropped(['one'], 'a draft')).toBe('one\n\na draft');
    expect(withDropped(['one'], '  \n')).toBe('one');
  });

  it("names each steer after its turn, no two alike", () => {
    const a = steerId('t1');
    expect(a.startsWith('t1-steer-')).toBe(true);
    expect(steerId('t1')).not.toBe(a);
  });
});
