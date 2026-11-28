// Search's content results, from the query and the server's answers.
import { describe, expect, it } from 'vitest';
import type { Session, SessionSearchResponse } from '../src/protocol/types.gen';
import { answered, asking, emptyLine, failed, IDLE, marked, searchTerm, unshown } from '../src/renderer/search';

function session(id: string, extra: Partial<Session> = {}): Session {
  return { id, createdAt: 1, updatedAt: 1, ephemeral: false, preview: '', turns: [], ...extra } as Session;
}
const result = (id: string, snippet: string, extra: Partial<Session> = {}) => ({
  session: session(id, extra),
  snippet,
  turnId: 't',
  itemId: `item-${id}`,
  matched: 'assistant' as const,
});
const response = (...data: SessionSearchResponse['data']): SessionSearchResponse => ({ data });

describe('content search', () => {
  it('searches from two characters on, without the spaces around them', () => {
    const cases: Array<[string | null, string | null]> = [
      [null, null],
      ['', null],
      ['r', null],
      [' r ', null],
      ['re', 're'],
      ['  retry budget ', 'retry budget'],
    ];
    for (const [query, term] of cases) expect(searchTerm(query), JSON.stringify(query)).toBe(term);
  });

  it('waits for the answer to the latest search and drops one for an earlier query', () => {
    const first = { term: 'ret', archived: false };
    const second = { term: 'retry', archived: false };
    let state = asking(first);
    expect(state).toMatchObject({ waiting: true, found: [] });
    state = asking(second);
    // The earlier search answers late: nothing of it shows.
    state = answered(state, first, response(result('old', 'a ret here')));
    expect(state).toMatchObject({ asked: second, waiting: true, found: [] });
    state = failed(state, first, 'gone');
    expect(state.error).toBeNull();
    state = answered(state, second, response(result('a', 'the retry budget')));
    expect(state.waiting).toBe(false);
    expect(state.found).toEqual([{ session: session('a'), snippet: 'the retry budget', itemId: 'item-a' }]);
    // The same text in the archive is another search.
    expect(answered(asking({ term: 'retry', archived: true }), second, response(result('a', 'x'))).found).toEqual([]);
    // A query too short for a search shows none of the last one.
    expect(asking(null)).toBe(IDLE);
    expect(answered(IDLE, second, response(result('a', 'x')))).toBe(IDLE);
  });

  it('keeps what a failed search said, and no results', () => {
    const asked = { term: 'retry', archived: false };
    expect(failed(asking(asked), asked, 'server gone')).toEqual({ asked, waiting: false, found: [], error: 'server gone' });
  });

  it('lists the sessions a person started, and not one already shown as a title match', () => {
    const asked = { term: 'retry', archived: false };
    const state = answered(
      asking(asked),
      asked,
      response(result('a', 'one'), result('exec', 'two', { source: 'exec' }), result('b', 'three'), {
        ...result('legacy', 'four'),
        itemId: undefined,
      }),
    );
    expect(state.found.map((f) => [f.session.id, f.itemId])).toEqual([
      ['a', 'item-a'],
      ['b', 'item-b'],
      ['legacy', null],
    ]);
    expect(unshown(state.found, new Set(['a'])).map((f) => f.session.id)).toEqual(['b', 'legacy']);
  });

  it('says the list is empty in the words for why', () => {
    const asked = { term: 'retry', archived: false };
    const waiting = asking(asked);
    const none = answered(waiting, asked, response());
    const broken = failed(waiting, asked, 'server gone');
    const cases: Array<[string, string | null, number, number, typeof IDLE, string | null]> = [
      ['no sessions at all', null, 0, 0, IDLE, 'No sessions yet.'],
      ['a short query with no title match', 'r', 0, 0, IDLE, 'No session matches.'],
      ['a search still waiting', 'retry', 0, 0, waiting, null],
      ['nothing by title or in conversations', 'retry', 0, 0, none, 'No conversation mentions it.'],
      ['title matches only', 'retry', 2, 0, none, null],
      ['content results only', 'retry', 0, 1, none, null],
      ['a failed search with no title match', 'retry', 0, 0, broken, 'No session matches.'],
    ];
    for (const [what, query, titles, results, state, line] of cases) {
      expect(emptyLine(query, titles, results, state), what).toBe(line);
    }
  });

  it('marks each place the term occurs in a snippet, in either case', () => {
    expect(marked('... so the Retry budget is spent; retry later ...', 'retry')).toEqual([
      { text: '... so the ', hit: false },
      { text: 'Retry', hit: true },
      { text: ' budget is spent; ', hit: false },
      { text: 'retry', hit: true },
      { text: ' later ...', hit: false },
    ]);
    expect(marked('retry', 'RETRY')).toEqual([{ text: 'retry', hit: true }]);
    // The server joins runs of spaces, so a term may not be in its snippet as typed.
    expect(marked('a b', 'a  b')).toEqual([{ text: 'a b', hit: false }]);
  });
});
