import { describe, expect, it } from 'vitest';
import { parseDiff, reviewMessage, turnDiff, withDiffs } from '../src/renderer/diffs';
import type { TurnView } from '../src/renderer/transcript';

const DIFF = `diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -3,3 +3,4 @@ fn main() {
 let a = 1;
-let b = 2;
+let b = 3;
+let c = 4;
 println!();
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello`;

describe('parseDiff', () => {
  it('numbers each line in the old and new file', () => {
    const [a, added] = parseDiff(DIFF);
    expect(a.path).toBe('src/a.rs');
    expect([a.added, a.removed]).toEqual([2, 1]);
    expect(a.lines.map((l) => [l.kind, l.oldLine, l.newLine])).toEqual([
      ['hunk', null, null],
      ['context', 3, 3],
      ['del', 4, null],
      ['add', null, 4],
      ['add', null, 5],
      ['context', 5, 6],
    ]);
    expect(added.path).toBe('new.txt');
    expect(added.lines[1]).toMatchObject({ kind: 'add', text: 'hello', newLine: 1 });
  });

  it('names a deleted file by its old path', () => {
    const [file] = parseDiff('--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye');
    expect(file.path).toBe('gone.txt');
    expect(file.removed).toBe(1);
  });
});

describe('withDiffs', () => {
  it("keeps the open session's latest diff of each turn", () => {
    const sent = (sessionId: string, turnId: string, diff: string) => ({
      jsonrpc: '2.0' as const,
      method: 'turn/diff/updated',
      params: { sessionId, turnId, diff },
    });
    const diffs = withDiffs(new Map(), 's1', [sent('s1', 't1', 'one'), sent('s2', 't1', 'other'), sent('s1', 't1', 'two')]);
    expect([...diffs]).toEqual([['t1', 'two']]);
    const same = withDiffs(diffs, 's1', [{ jsonrpc: '2.0', method: 'turn/started', params: { sessionId: 's1' } }]);
    expect(same).toBe(diffs);
  });
});

describe('turnDiff', () => {
  const edit = (id: string, status: string, path: string, kind: string, diff: string) => ({
    type: 'toolCall',
    id,
    status,
    changes: [{ path, kind: { type: kind }, diff }],
  });
  const turn = {
    id: 't1',
    items: [
      edit('c1', 'completed', '/p/a.txt', 'update', '@@ -1 +1 @@\n-a\n+b\n'),
      { type: 'agentMessage', id: 'm1', text: 'done' },
      edit('c2', 'failed', '/p/x.txt', 'update', '@@ -1 +1 @@\n-x\n+y\n'),
      edit('c3', 'completed', '/p/new.txt', 'add', 'one\ntwo\n'),
    ],
  } as unknown as TurnView;

  it("uses the server's diff when it sent one", () => {
    expect(turnDiff(turn, new Map([['t1', 'sent']]))).toBe('sent');
  });

  it("makes one of the turn's finished edits otherwise", () => {
    const diff = turnDiff(turn, new Map(), '/p');
    expect(diff).toBe(
      '--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+b\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n',
    );
    expect(parseDiff(diff).map((f) => [f.path, f.added, f.removed])).toEqual([
      ['a.txt', 1, 1],
      ['new.txt', 2, 0],
    ]);
  });
});

describe('reviewMessage', () => {
  it('quotes each line with its comment', () => {
    const message = reviewMessage([
      { path: 'src/a.rs', line: 4, side: 'new', text: 'let b = 3;', comment: 'Why 3?' },
      { path: 'src/a.rs', line: 4, side: 'old', text: 'let b = 2;', comment: ' Keep this. ' },
    ]);
    expect(message).toBe(
      'Comments on your changes:\n\nsrc/a.rs:4\n> let b = 3;\nWhy 3?\n\nsrc/a.rs:4 (removed line)\n> let b = 2;\nKeep this.',
    );
  });
});
