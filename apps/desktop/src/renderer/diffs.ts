// A turn's changes for review: the unified diff the server sends as the
// turn edits (`turn/diff/updated`, Codex's), or, for a stored turn, its
// edits' own diffs; parsed into files and lines with their numbers, so a
// line can carry a comment. Changes a command or a sub-agent made aren't in
// it, as the protocol says of `turn/diff/updated`.
import type { FileUpdateChange } from '../protocol/types.gen';
import type { RpcMessage } from './bridge';
import type { TurnView } from './transcript';

export interface DiffLine {
  kind: 'add' | 'del' | 'context' | 'hunk';
  text: string;
  oldLine: number | null;
  newLine: number | null;
}

export interface DiffFile {
  path: string;
  added: number;
  removed: number;
  lines: DiffLine[];
}

/** Parse a unified diff (git's format) into files and numbered lines. */
export function parseDiff(diff: string): DiffFile[] {
  const files: DiffFile[] = [];
  let file: DiffFile | null = null;
  let oldLine = 0;
  let newLine = 0;
  const lines = diff.split('\n');
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (line.startsWith('diff --git ')) {
      file = null;
      continue;
    }
    if (line.startsWith('--- ') && lines[i + 1]?.startsWith('+++ ')) {
      const next = lines[i + 1].slice(4).trim();
      const before = line.slice(4).trim();
      const path = (next === '/dev/null' ? before : next).replace(/^[ab]\//, '');
      file = { path, added: 0, removed: 0, lines: [] };
      files.push(file);
      i++;
      continue;
    }
    if (!file) continue;
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (hunk) {
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[2]);
      file.lines.push({ kind: 'hunk', text: line, oldLine: null, newLine: null });
    } else if (line.startsWith('+')) {
      file.lines.push({ kind: 'add', text: line.slice(1), oldLine: null, newLine: newLine++ });
      file.added++;
    } else if (line.startsWith('-')) {
      file.lines.push({ kind: 'del', text: line.slice(1), oldLine: oldLine++, newLine: null });
      file.removed++;
    } else if (line.startsWith(' ')) {
      file.lines.push({ kind: 'context', text: line.slice(1), oldLine: oldLine++, newLine: newLine++ });
    }
  }
  return files;
}

/** The latest diff of each turn, by turn id, from `turn/diff/updated`. */
export type TurnDiffs = ReadonlyMap<string, string>;

export function withDiffs(diffs: TurnDiffs, sessionId: string, batch: readonly RpcMessage[]): TurnDiffs {
  let next: Map<string, string> | null = null;
  for (const message of batch) {
    const p = message.params as { sessionId?: string; turnId?: string; diff?: string } | undefined;
    if (message.method !== 'turn/diff/updated' || p?.sessionId !== sessionId || !p.turnId) continue;
    (next ??= new Map(diffs)).set(p.turnId, p.diff ?? '');
  }
  return next ?? diffs;
}

/**
 * A turn's changes: the server's diff of it, else, for a stored turn, its
 * finished edits' own diffs, which have no file lines of their own. `cwd`
 * shortens the edits' paths as the server's diff does.
 */
export function turnDiff(turn: TurnView, diffs: TurnDiffs, cwd?: string | null): string {
  const sent = diffs.get(turn.id);
  if (sent !== undefined) return sent;
  return turn.items
    .flatMap((item) => (item.type === 'toolCall' && item.status === 'completed' ? (item.changes ?? []) : []))
    .map((change) => fileDiff(change, cwd))
    .join('');
}

/** One edit's change as a file's unified diff, with the file lines git writes. */
function fileDiff(change: FileUpdateChange, cwd?: string | null): string {
  const path = cwd && change.path.startsWith(`${cwd}/`) ? change.path.slice(cwd.length + 1) : change.path;
  const body = change.diff.endsWith('\n') ? change.diff : `${change.diff}\n`;
  if (change.kind.type === 'add') {
    // A new file's change is its content.
    const lines = change.diff.split('\n');
    if (lines.at(-1) === '') lines.pop();
    return `--- /dev/null\n+++ b/${path}\n@@ -0,0 +1,${lines.length} @@\n${lines.map((l) => `+${l}\n`).join('')}`;
  }
  if (change.diff.startsWith('--- ')) return body;
  return `--- a/${path}\n+++ b/${path}\n${body}`;
}

/** A comment on one line of a turn's changes. */
export interface LineComment {
  path: string;
  /** The line's number in the new file, or in the old one for a removed line. */
  line: number;
  side: 'new' | 'old';
  /** The line itself, quoted in the message. */
  text: string;
  comment: string;
}

/** The comments as one message to Koda, as Claude Code sends review comments together. */
export function reviewMessage(comments: readonly LineComment[]): string {
  const parts = comments.map((c) => {
    const where = `${c.path}:${c.line}${c.side === 'old' ? ' (removed line)' : ''}`;
    return `${where}\n> ${c.text.trim()}\n${c.comment.trim()}`;
  });
  return `Comments on your changes:\n\n${parts.join('\n\n')}`;
}
