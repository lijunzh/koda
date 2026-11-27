// A session's transcript: its turns as the server sends them (stored turns
// from `session/resume`, then live events), and the rows the window shows,
// in Claude Code's desktop app's three views (its docs: "Tool calls
// collapsed into summaries, with full text responses"):
//
// - Normal: lookups one after another (reads, searches, listings) are one
//   row in Claude Code's words, `Read 3 files, searched 2 patterns`, with no
//   output; other calls are a row each with their output folded to 5 lines
//   (and the last 5 when they failed); an edit shows its diff; thinking isn't
//   shown. A finished turn's steps stay as they are, as in Claude Code.
// - Thinking: the same, with the model's thinking where it came.
// - Verbose: every item and all its output.
//
// The grouping and wording are the TUI's (koda-tui's `tui_render.rs`).
import type { CommandAction, Item, Session, Turn } from '../protocol/types.gen';
import type { RpcMessage } from './bridge';

/** An item and what the live events added: its times, a running command's output, its progress. */
export type ItemView = Item & {
  startedAtMs?: number;
  completedAtMs?: number;
  /** The sub-agent call it belongs to. */
  parentItemId?: string | null;
  /** A running command's output so far, from `item/toolCall/outputDelta`. */
  liveOutput?: string;
};

export interface TurnView {
  id: string;
  status: Turn['status'];
  startedAt: number | null;
  durationMs: number | null;
  error: string | null;
  items: ItemView[];
}

export interface TranscriptState {
  sessionId: string;
  turns: TurnView[];
}

/** A running command's output kept for the window, at most. */
const LIVE_OUTPUT_CHARS = 64_000;

function turnView(turn: Turn, before?: TurnView): TurnView {
  return {
    id: turn.id,
    status: turn.status,
    startedAt: turn.startedAt ?? before?.startedAt ?? null,
    durationMs: turn.durationMs ?? before?.durationMs ?? null,
    error: turn.error?.message ?? null,
    // A turn's items as the server has them; the live ones if it sent none.
    items: turn.items?.length ? turn.items.map((item) => merge(item, before)) : (before?.items ?? []),
  };
}

/** An item as stored or completed, keeping what the live events knew of it. */
function merge(item: Item, turn?: TurnView): ItemView {
  const live = turn?.items.find((i) => i.id === item.id);
  if (!live) return { ...item };
  return {
    ...item,
    startedAtMs: live.startedAtMs,
    completedAtMs: live.completedAtMs,
    parentItemId: live.parentItemId,
  };
}

/** The transcript of a session the server just opened. */
export function fromSession(session: Session): TranscriptState {
  return { sessionId: session.id, turns: (session.turns ?? []).map((turn) => turnView(turn)) };
}

/** Apply this session's events from `batch`; the same state if none was its. */
export function withEvents(state: TranscriptState, batch: readonly RpcMessage[]): TranscriptState {
  let turns: TurnView[] | null = null;
  const edit = () => (turns ??= state.turns.slice());
  const find = (turnId: string) => (turns ?? state.turns).findIndex((t) => t.id === turnId);
  const turnFor = (turnId: string): number => {
    const at = find(turnId);
    if (at >= 0) return at;
    // A turn whose start this window didn't see (it joined mid-turn).
    edit().push({ id: turnId, status: 'inProgress', startedAt: null, durationMs: null, error: null, items: [] });
    return edit().length - 1;
  };
  const changeItem = (turnId: string, itemId: string, change: (item: ItemView | undefined) => ItemView) => {
    const at = turnFor(turnId);
    const all = edit();
    const turn = all[at];
    const items = turn.items.slice();
    const index = items.findIndex((i) => i.id === itemId);
    const next = change(index >= 0 ? items[index] : undefined);
    if (index >= 0) items[index] = next;
    else items.push(next);
    all[at] = { ...turn, items };
  };

  for (const message of batch) {
    const p = message.params as Record<string, unknown> | undefined;
    if (!p || p.sessionId !== state.sessionId) continue;
    const turnId = p.turnId as string;
    switch (message.method) {
      case 'turn/started':
      case 'turn/completed': {
        const turn = p.turn as Turn;
        const at = find(turn.id);
        if (at >= 0) edit()[at] = turnView(turn, (turns ?? state.turns)[at]);
        else edit().push(turnView(turn));
        break;
      }
      case 'item/started':
        changeItem(turnId, (p.item as Item).id, (before) => ({
          ...(p.item as Item),
          startedAtMs: p.startedAtMs as number,
          parentItemId: (p.parentItemId as string | null) ?? null,
          liveOutput: before?.liveOutput,
        }));
        break;
      case 'item/completed':
        changeItem(turnId, (p.item as Item).id, (before) => ({
          ...(p.item as Item),
          startedAtMs: before?.startedAtMs,
          completedAtMs: p.completedAtMs as number,
          parentItemId: (p.parentItemId as string | null) ?? before?.parentItemId ?? null,
        }));
        break;
      case 'item/agentMessage/delta':
      case 'item/reasoning/textDelta': {
        const type = message.method === 'item/agentMessage/delta' ? 'agentMessage' : 'reasoning';
        changeItem(turnId, p.itemId as string, (before) => {
          const text = (before && 'text' in before ? before.text : '') + (p.delta as string);
          return before ? ({ ...before, text } as ItemView) : ({ type, id: p.itemId as string, text } as ItemView);
        });
        break;
      }
      case 'item/toolCall/outputDelta':
        changeItem(turnId, p.itemId as string, (before) => {
          const output = ((before?.liveOutput ?? '') + (p.delta as string)).slice(-LIVE_OUTPUT_CHARS);
          return { ...(before as ItemView), liveOutput: output };
        });
        break;
      case 'item/toolCall/progress':
        changeItem(turnId, p.itemId as string, (before) => ({ ...(before as ItemView), progress: p.message } as ItemView));
        break;
    }
  }
  return turns ? { ...state, turns } : state;
}

/** The turn running now, if one is. */
export function runningTurn(state: TranscriptState): TurnView | undefined {
  const last = state.turns.at(-1);
  return last?.status === 'inProgress' ? last : undefined;
}

// ── Rows ─────────────────────────────────────────────────────────────────

type ToolCall = Extract<ItemView, { type: 'toolCall' }>;
type Lookup = 'read' | 'search' | 'list' | 'command';

/**
 * What a call looks up, as the TUI counts it; `null` if it isn't a lookup.
 * A command counts as what its `commandActions` say it does, whatever its
 * effect (since 0.8.0 the host rates no command read-only), as Codex's
 * `Explored` cell reads them; one with a part they can't read keeps its own
 * row. Other tools fold when the host says they only read.
 */
export function lookups(call: ToolCall): Lookup[] | null {
  const readOnly = call.effect === 'readOnly';
  if (call.tool === 'Bash') {
    const actions: CommandAction[] = call.commandActions ?? [];
    if (actions.length && !actions.some((a) => a.type === 'unknown')) {
      return actions.map((a) => (a.type === 'read' ? 'read' : a.type === 'listFiles' ? 'list' : 'search'));
    }
    // Stored when the host still rated commands: a read-only one is a command in the row.
    return readOnly ? ['command'] : null;
  }
  if (!readOnly) return null;
  switch (call.tool) {
    case 'Read':
      return ['read'];
    case 'Grep':
    case 'Glob':
      return ['search'];
    case 'List':
      return ['list'];
    default:
      return null;
  }
}

export interface Counts {
  read: number;
  search: number;
  list: number;
  command: number;
}

/** A lookup row in Claude Code's words: `Read 3 files, searched 5 patterns`; `Reading …` while one runs. */
export function lookupLabel(counts: Counts, running: boolean): string {
  const parts = (
    [
      [counts.read, 'read', 'reading', 'file', 'files'],
      [counts.search, 'searched', 'searching', 'pattern', 'patterns'],
      [counts.list, 'listed', 'listing', 'directory', 'directories'],
      [counts.command, 'ran', 'running', 'command', 'commands'],
    ] as const
  )
    .filter(([n]) => n > 0)
    .map(([n, done, doing, one, many]) => `${running ? doing : done} ${n} ${n === 1 ? one : many}`);
  const words = parts.join(', ');
  return (words.charAt(0).toUpperCase() + words.slice(1)) + (running ? '…' : '');
}

/** A duration as Codex's TUI writes it: `<1s`, `12s`, `3m 4s`, `1h 2m 3s`. */
export function workedFor(ms: number): string {
  const secs = Math.floor(ms / 1000);
  const [h, m, s] = [Math.floor(secs / 3600), Math.floor((secs % 3600) / 60), secs % 60];
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m > 0) return `${m}m ${s}s`;
  return s === 0 ? '<1s' : `${s}s`;
}

export type Row =
  | { kind: 'user'; key: string; text: string }
  | { kind: 'thought'; key: string; ms: number | null; text: string; running: boolean }
  | { kind: 'lookups'; key: string; label: string; failed: number; calls: ToolCall[] }
  | { kind: 'tool'; key: string; call: ToolCall; children: ToolCall[] }
  | { kind: 'message'; key: string; text: string; streaming: boolean }
  | { kind: 'compaction'; key: string; text: string }
  | { kind: 'note'; key: string; text: string; tone: 'stopped' | 'error' };

/** Claude Code's desktop views, which Ctrl+O cycles. */
export type View = 'normal' | 'thinking' | 'verbose';

export const VIEWS: readonly View[] = ['normal', 'thinking', 'verbose'];

function userText(item: Extract<ItemView, { type: 'userMessage' }>): string {
  return item.content
    .map((part) => {
      switch (part.type) {
        case 'text':
          return part.text;
        case 'mention':
          return `@${part.path}`;
        case 'skill':
          return `$${part.name}`;
        default:
          return '[image]';
      }
    })
    .join(' ');
}

function itemMs(item: ItemView): number | null {
  return item.startedAtMs && item.completedAtMs ? item.completedAtMs - item.startedAtMs : null;
}

/** The rows of one turn, in the view asked for. */
export function turnRows(turn: TurnView, view: View): Row[] {
  const top = turn.items.filter((item) => !item.parentItemId);
  const children = (id: string) =>
    turn.items.filter((item): item is ToolCall => item.parentItemId === id && item.type === 'toolCall');
  const running = turn.status === 'inProgress';
  const rows: Row[] = [];

  if (view === 'verbose') {
    for (const item of top) rows.push(...verboseRows(item, children));
  } else {
    let group: Extract<Row, { kind: 'lookups' }> | null = null;
    let counts: Counts = { read: 0, search: 0, list: 0, command: 0 };
    let groupRunning = false;
    const closeGroup = () => {
      if (group) group.label = lookupLabel(counts, groupRunning);
      group = null;
    };
    for (const item of top) {
      if (item.type === 'reasoning') {
        // Thinking shows in the Thinking view only, where it came; in Normal
        // it's left out and doesn't split a row of lookups.
        if (view === 'thinking') {
          closeGroup();
          rows.push({ kind: 'thought', key: item.id, ms: itemMs(item), text: item.text, running: running && !item.completedAtMs });
        }
        continue;
      }
      if (item.type === 'toolCall') {
        const looked = lookups(item);
        if (looked) {
          if (!group) {
            group = { kind: 'lookups', key: item.id, label: '', failed: 0, calls: [] };
            counts = { read: 0, search: 0, list: 0, command: 0 };
            groupRunning = false;
            rows.push(group);
          }
          const g = group as Extract<Row, { kind: 'lookups' }>;
          g.calls.push(item);
          for (const l of looked) counts[l]++;
          if (item.status === 'inProgress') groupRunning = true;
          if (item.status === 'failed') g.failed++;
          continue;
        }
        closeGroup();
        rows.push({ kind: 'tool', key: item.id, call: item, children: children(item.id) });
        continue;
      }
      closeGroup();
      if (item.type === 'userMessage') rows.push({ kind: 'user', key: item.id, text: userText(item) });
      else if (item.type === 'agentMessage') {
        rows.push({ kind: 'message', key: item.id, text: item.text, streaming: running && !item.completedAtMs });
      } else if (item.type === 'contextCompaction') {
        rows.push({ kind: 'compaction', key: item.id, text: 'Context compacted' });
      }
    }
    closeGroup();
  }

  if (turn.status === 'interrupted') rows.push({ kind: 'note', key: `${turn.id}-end`, text: 'Stopped', tone: 'stopped' });
  if (turn.status === 'failed') {
    rows.push({ kind: 'note', key: `${turn.id}-end`, text: turn.error ?? 'The turn failed', tone: 'error' });
  }
  return rows;
}

function verboseRows(item: ItemView, children: (id: string) => ToolCall[]): Row[] {
  switch (item.type) {
    case 'userMessage':
      return [{ kind: 'user', key: item.id, text: userText(item) }];
    case 'reasoning':
      return [{ kind: 'thought', key: item.id, ms: itemMs(item), text: item.text, running: !item.completedAtMs }];
    case 'agentMessage':
      return [{ kind: 'message', key: item.id, text: item.text, streaming: !item.completedAtMs }];
    case 'toolCall':
      return [{ kind: 'tool', key: item.id, call: item, children: children(item.id) }];
    case 'contextCompaction':
      return [{ kind: 'compaction', key: item.id, text: 'Context compacted' }];
  }
}

// ── Tool output ──────────────────────────────────────────────────────────

/** Terminal escape sequences, which a command's output may carry; the window shows text. */
// eslint-disable-next-line no-control-regex
const ESCAPES = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;

export function plain(text: string): string {
  return text.replace(ESCAPES, '');
}

export interface OutputLine {
  text: string;
  stderr: boolean;
}

/**
 * What a command printed, from the result Koda's shell tool writes for the
 * model (`Exit code: 0 | stdout: 9 lines | …`, a `--- stdout (9 lines) ---`
 * header over each stream), as koda-tui's `command_output` reads it. `null`
 * for any other text.
 */
export function commandOutput(output: string): { code: number; lines: OutputLine[] } | null {
  const rows = output.split('\n');
  const first = rows[0]?.startsWith('Exit code: ') ? rows[0].slice('Exit code: '.length) : null;
  if (first === null) return null;
  const code = Number.parseInt(first.split(' |')[0].trim(), 10);
  if (Number.isNaN(code)) return null;
  const lines: OutputLine[] = [];
  let stderr = false;
  for (const row of rows.slice(1)) {
    const header = /^--- (stdout|stderr)(?: \(.*\))? ---$/.exec(row);
    if (header) {
      stderr = header[1] === 'stderr';
      while (lines.length && lines.at(-1)!.text === '') lines.pop();
      continue;
    }
    if (
      row.startsWith('Warning: truncated output') ||
      row.startsWith('Total output lines: ') ||
      row.startsWith('To see what is missing, narrow the command')
    ) {
      continue;
    }
    if (row === '' && lines.length === 0) continue;
    lines.push({ text: row, stderr });
  }
  while (lines.length && lines.at(-1)!.text === '') lines.pop();
  return { code, lines };
}

/** Lines shown folded: the first and, when the call failed, the last. */
export const FOLD_HEAD = 5;
export const FOLD_TAIL = 5;

/** A tool's output as the window shows it, and how many lines folding hides. */
export function shownOutput(call: ToolCall, full: boolean): { lines: OutputLine[]; hidden: number; exitCode: number | null } {
  const raw = plain(call.output ?? call.liveOutput ?? '');
  const parsed = call.tool === 'Bash' ? commandOutput(raw) : null;
  let lines: OutputLine[] = parsed
    ? parsed.lines
    : raw
        .split('\n')
        .filter((_, i, all) => !(i === all.length - 1 && all[i] === ''))
        .map((text) => ({ text, stderr: false }));
  const exitCode = parsed ? parsed.code : (call.exitCode ?? null);
  // A read's content is in the full view only.
  if (call.tool === 'Read' && !full) return { lines: [], hidden: 0, exitCode };
  if (full) return { lines, hidden: 0, exitCode };
  const failed = call.status === 'failed';
  const keep = FOLD_HEAD + (failed ? FOLD_TAIL : 0);
  if (lines.length <= keep) return { lines, hidden: 0, exitCode };
  const hidden = lines.length - keep;
  lines = failed ? [...lines.slice(0, FOLD_HEAD), ...lines.slice(-FOLD_TAIL)] : lines.slice(0, FOLD_HEAD);
  return { lines, hidden, exitCode };
}

/** A call's row heading: the tool and what it was given. */
export function callTitle(call: ToolCall): string {
  const args = (call.arguments ?? {}) as Record<string, unknown>;
  const what =
    (typeof args.command === 'string' && args.command) ||
    (typeof args.file_path === 'string' && args.file_path) ||
    (typeof args.path === 'string' && args.path) ||
    (typeof args.url === 'string' && args.url) ||
    (typeof args.agent_name === 'string' && args.agent_name) ||
    call.detail ||
    '';
  const oneLine = what.split('\n')[0];
  return what ? `${call.tool}(${oneLine.length > 120 ? `${oneLine.slice(0, 119)}…` : oneLine})` : call.tool;
}

/** What a running turn is on, in the TUI's status-line words. */
export function statusLine(turn: TurnView): string {
  const top = turn.items.filter((item) => !item.parentItemId);
  const live = [...top].reverse().find((item) => !item.completedAtMs && item.type !== 'userMessage');
  if (!live) return 'Working…';
  if (live.type === 'reasoning') return 'Thinking…';
  if (live.type === 'agentMessage') return 'Writing…';
  if (live.type === 'contextCompaction') return 'Compacting the conversation…';
  if (live.type !== 'toolCall') return 'Working…';
  const action = live.commandActions?.[0];
  const args = (live.arguments ?? {}) as Record<string, unknown>;
  const base = (p: unknown) => (typeof p === 'string' ? p.split('/').pop() : undefined);
  if (action?.type === 'read') return `Reading ${action.name}`;
  if (action?.type === 'search' && action.query) return `Searching for "${action.query}"`;
  if (action?.type === 'listFiles') return `Listing ${action.path ?? 'files'}`;
  if (live.tool === 'Read' && base(args.file_path)) return `Reading ${base(args.file_path)}`;
  if ((live.tool === 'Edit' || live.tool === 'Write') && base(args.file_path)) return `Editing ${base(args.file_path)}`;
  if (live.tool === 'InvokeAgent' && typeof args.agent_name === 'string') return `Waiting on ${args.agent_name}`;
  if (live.tool === 'Bash') return 'Running a command';
  return `Running ${live.tool}`;
}
