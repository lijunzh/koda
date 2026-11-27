// The transcript: turns from the server's events, and the rows the window
// shows, by the TUI's folding rules and in its words.
import { describe, expect, it } from 'vitest';
import type { Session } from '../src/protocol/types.gen';
import {
  commandOutput,
  fromSession,
  lookupLabel,
  plain,
  type Row,
  shownOutput,
  statusLine,
  turnRows,
  type TurnView,
  withEvents,
  workedFor,
} from '../src/renderer/transcript';

const S = 's1';
const ev = (method: string, params: object) => ({ jsonrpc: '2.0' as const, method, params: { sessionId: S, ...params } });

let n = 0;
const id = () => `i${++n}`;
const user = (text: string) => ({ type: 'userMessage', id: id(), content: [{ type: 'text', text }] });
const say = (text: string, phase?: string) => ({ type: 'agentMessage', id: id(), text, ...(phase ? { phase } : {}) });
const think = (text: string, ms?: number) => ({
  type: 'reasoning',
  id: id(),
  text,
  ...(ms !== undefined ? { startedAtMs: 1000, completedAtMs: 1000 + ms } : {}),
});
const call = (tool: string, extra: object = {}) => ({
  type: 'toolCall',
  id: id(),
  tool,
  status: 'completed',
  effect: 'readOnly',
  arguments: {},
  ...extra,
});
const bash = (command: string, actions: object[], extra: object = {}) =>
  call('Bash', { arguments: { command }, commandActions: actions, ...extra });

function turn(items: object[], extra: Partial<TurnView> = {}): TurnView {
  return { id: 't1', status: 'completed', startedAt: 0, durationMs: 125_000, error: null, items: items as TurnView['items'], ...extra };
}
const kinds = (rows: Row[]) => rows.map((r) => (r.kind === 'lookups' ? `${r.kind}:${r.label}` : r.kind));

describe('rows', () => {
  it('folds lookups one after another into one row, counted as Claude Code words them', () => {
    const rows = turnRows(
      turn(
        [
          user('look'),
          call('Read', { arguments: { file_path: '/a/Cargo.toml' } }),
          bash('cat a.rs && rg GUI', [
            { type: 'read', command: 'cat a.rs', name: 'a.rs', path: 'a.rs' },
            { type: 'search', command: 'rg GUI', query: 'GUI' },
          ]),
          bash('ls src', [{ type: 'listFiles', command: 'ls src', path: 'src' }]),
          bash('make', [{ type: 'unknown', command: 'make' }], { effect: 'localMutation' }),
          say('Done.'),
        ],
        { status: 'interrupted' },
      ),
      'normal',
    );
    // `make` can't be read: its own row, ending the group.
    expect(kinds(rows)).toEqual(['user', 'lookups:Read 2 files, searched 1 pattern, listed 1 directory', 'tool', 'message', 'note']);
  });

  it('folds a command by what it does, whatever its effect, as Codex does', () => {
    const ls = bash('ls', [{ type: 'listFiles', command: 'ls', path: '.' }], { effect: 'localMutation' });
    const make = bash('make', [{ type: 'unknown', command: 'make' }], { effect: 'localMutation' });
    // Stored before 0.8.0: a read-only command the host couldn't read is a command in the row.
    const legacy = bash('jq . a.json', [], { effect: 'readOnly' });
    const rows = turnRows(turn([user('a'), ls, legacy, make], { status: 'interrupted' }), 'normal');
    expect(kinds(rows)).toEqual(['user', 'lookups:Listed 1 directory, ran 1 command', 'tool', 'note']);
  });

  it('ends a row at anything else shown; a progress note or an edit splits lookups', () => {
    const rows = turnRows(
      turn(
        [
          user('go'),
          call('Read'),
          call('Read'),
          say('A progress note', 'commentary'),
          call('Read'),
          call('Edit', { effect: 'localMutation', changes: [] }),
          call('Read'),
        ],
        { status: 'interrupted' },
      ),
      'normal',
    );
    expect(kinds(rows)).toEqual(['user', 'lookups:Read 2 files', 'message', 'lookups:Read 1 file', 'tool', 'lookups:Read 1 file', 'note']);
  });

  it("shows thinking in the Thinking view only, where it came; in Normal it doesn't split lookups", () => {
    const items = [user('go'), think('first', 2000), call('Read'), think('between', 1000), call('Read'), say('Done.')];
    expect(kinds(turnRows(turn(items), 'normal'))).toEqual(['user', 'lookups:Read 2 files', 'message']);
    const thinking = turnRows(turn(items), 'thinking');
    expect(kinds(thinking)).toEqual(['user', 'thought', 'lookups:Read 1 file', 'thought', 'lookups:Read 1 file', 'message']);
    expect(thinking[1]).toMatchObject({ text: 'first', ms: 2000 });
  });

  it("keeps a finished turn's steps, as Claude Code does", () => {
    const rows = turnRows(turn([user('fix it'), call('Read'), say('Looking', 'commentary'), call('Edit', { effect: 'localMutation' }), say('Fixed.', 'final_answer')]), 'normal');
    expect(kinds(rows)).toEqual(['user', 'lookups:Read 1 file', 'message', 'tool', 'message']);
  });

  it('says a row is running, and counts failures', () => {
    const rows = turnRows(
      turn([user('go'), call('Read', { status: 'failed' }), call('Read', { status: 'inProgress' })], { status: 'inProgress' }),
      'normal',
    );
    const group = rows[1] as Extract<Row, { kind: 'lookups' }>;
    expect(group.label).toBe('Reading 2 files…');
    expect(group.failed).toBe(1);
  });

  it('shows every item in the Verbose view', () => {
    const rows = turnRows(turn([user('a'), think('t'), call('Read'), call('Read'), say('b')]), 'verbose');
    expect(kinds(rows)).toEqual(['user', 'thought', 'tool', 'tool', 'message']);
  });

  it("keeps a sub-agent's calls under its own", () => {
    const parent = call('InvokeAgent', { effect: 'localMutation', arguments: { agent_name: 'explore' } });
    const child = { ...call('Read'), parentItemId: parent.id };
    const rows = turnRows(turn([user('a'), parent, child], { status: 'interrupted' }), 'normal');
    expect(kinds(rows)).toEqual(['user', 'tool', 'note']);
    expect((rows[1] as Extract<Row, { kind: 'tool' }>).children).toHaveLength(1);
  });

  it('says a turn failed, with its error', () => {
    const rows = turnRows(turn([user('a')], { status: 'failed', error: 'rate limited' }), 'normal');
    expect(rows.at(-1)).toMatchObject({ kind: 'note', tone: 'error', text: 'rate limited' });
  });

  it('words durations and labels as the TUI does', () => {
    expect(workedFor(400)).toBe('<1s');
    expect(workedFor(12_300)).toBe('12s');
    expect(workedFor(184_000)).toBe('3m 4s');
    expect(workedFor(3_723_000)).toBe('1h 2m 3s');
    expect(lookupLabel({ read: 1, search: 2, list: 0, command: 0 }, false)).toBe('Read 1 file, searched 2 patterns');
    expect(lookupLabel({ read: 0, search: 0, list: 2, command: 1 }, true)).toBe('Listing 2 directories, running 1 command…');
  });
});

describe('events', () => {
  const session = { id: S, turns: [] } as unknown as Session;

  it('streams a turn from its start to its end', () => {
    let state = fromSession(session);
    const t = { id: 't1', status: 'inProgress', items: [] };
    state = withEvents(state, [
      ev('turn/started', { turn: t }),
      ev('item/started', { turnId: 't1', item: { type: 'agentMessage', id: 'm1', text: '' }, startedAtMs: 10 }),
      ev('item/agentMessage/delta', { turnId: 't1', itemId: 'm1', delta: 'Hel' }),
      ev('item/agentMessage/delta', { turnId: 't1', itemId: 'm1', delta: 'lo' }),
      ev('item/started', { turnId: 't1', item: call('Bash', { id: 'c1', status: 'inProgress' }), startedAtMs: 20 }),
      ev('item/toolCall/outputDelta', { turnId: 't1', itemId: 'c1', stream: 'stdout', delta: 'line 1\n' }),
      ev('item/toolCall/outputDelta', { turnId: 't1', itemId: 'c1', stream: 'stdout', delta: 'line 2\n' }),
    ]);
    const items = state.turns[0].items;
    expect(items[0]).toMatchObject({ text: 'Hello', startedAtMs: 10 });
    expect(items[1]).toMatchObject({ liveOutput: 'line 1\nline 2\n' });
    expect(statusLine(state.turns[0])).toBe('Running a command');

    state = withEvents(state, [
      ev('item/completed', { turnId: 't1', item: call('Bash', { id: 'c1', output: 'Exit code: 0 | stdout: 2 lines' }), completedAtMs: 90 }),
      ev('turn/completed', { turn: { ...t, status: 'completed', durationMs: 100, items: [] } }),
    ]);
    expect(state.turns[0].status).toBe('completed');
    expect(state.turns[0].items[1]).toMatchObject({ startedAtMs: 20, completedAtMs: 90 });
    expect(state.turns[0].items).toHaveLength(2);
  });

  it("ignores other sessions' events, and keeps a turn joined in the middle", () => {
    const state = fromSession(session);
    expect(withEvents(state, [{ jsonrpc: '2.0', method: 'turn/started', params: { sessionId: 'other', turn: {} } }])).toBe(state);
    const joined = withEvents(state, [ev('item/agentMessage/delta', { turnId: 't9', itemId: 'm', delta: 'x' })]);
    expect(joined.turns[0]).toMatchObject({ id: 't9', status: 'inProgress' });
  });
});

describe('tool output', () => {
  it("reads what a command printed from the shell tool's result", () => {
    const out = commandOutput(
      'Exit code: 1 | stdout: 2 lines | stderr: 1 lines\n\n--- stdout (2 lines) ---\nok\nmore\n\n--- stderr (1 lines) ---\nboom\n',
    );
    expect(out).toEqual({
      code: 1,
      lines: [
        { text: 'ok', stderr: false },
        { text: 'more', stderr: false },
        { text: 'boom', stderr: true },
      ],
    });
    expect(commandOutput('Timed out')).toBeNull();
  });

  it('folds to the first lines, and the last ones too when the call failed', () => {
    const lines = Array.from({ length: 20 }, (_, i) => `l${i}`).join('\n');
    const ok = shownOutput(call('Grep', { output: lines }) as never, false);
    expect(ok.lines.map((l) => l.text)).toEqual(['l0', 'l1', 'l2', 'l3', 'l4']);
    expect(ok.hidden).toBe(15);
    const failed = shownOutput(call('Grep', { output: lines, status: 'failed' }) as never, false);
    expect(failed.lines.map((l) => l.text)).toEqual(['l0', 'l1', 'l2', 'l3', 'l4', 'l15', 'l16', 'l17', 'l18', 'l19']);
    expect(shownOutput(call('Grep', { output: lines }) as never, true).lines).toHaveLength(20);
    expect(shownOutput(call('Read', { output: lines }) as never, false).lines).toEqual([]);
  });

  it('shows text, not terminal escapes', () => {
    expect(plain('\x1b[31mred\x1b[0m and \x1b]8;;http://x\x07link\x1b]8;;\x07')).toBe('red and link');
  });
});
