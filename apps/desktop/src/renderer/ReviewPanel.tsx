// The Changes pane, in the side panel, laid out as Claude Code's
// desktop app lays out a diff: one turn's changes (the latest first), file
// by file, each with its counts; unchanged lines between the changes folded
// to a count; old and new line numbers; a bar at the left of each added or
// removed line. Hovering a line shows a "+" beside its number, which opens a
// comment; Enter adds it, and Cmd+Enter (or the button) sends them all to
// Koda as one message.
import { useEffect, useMemo, useState } from 'react';
import { call } from './bridge';
import { type DiffFile, type DiffLine, type LineComment, parseDiff, reviewMessage, type TurnDiffs, turnDiff } from './diffs';
import { type Entry, MenuButton } from './Menu';
import { runningTurn, type TranscriptState } from './transcript';

interface Props {
  sessionId: string;
  /** The session's folder, which the changes' paths are relative to. */
  cwd: string | null;
  transcript: TranscriptState;
  diffs: TurnDiffs;
}

type Draft = { key: string; at: Omit<LineComment, 'comment'>; text: string };

export function ReviewPanel({ sessionId, cwd, transcript, diffs }: Props) {
  // The turns that changed something, the latest first.
  const changed = useMemo(
    () =>
      transcript.turns
        .map((turn, i) => ({ turn, index: i + 1, diff: turnDiff(turn, diffs, cwd) }))
        .filter((t) => t.diff.trim())
        .reverse(),
    [transcript, diffs, cwd],
  );
  const [turnId, setTurnId] = useState<string | null>(null);
  const shown = changed.find((t) => t.turn.id === turnId) ?? changed[0];
  const files = useMemo(() => (shown ? parseDiff(shown.diff) : []), [shown]);
  const [comments, setComments] = useState<LineComment[]>([]);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);

  const send = async (all: LineComment[]) => {
    if (!all.length) return;
    const input = [{ type: 'text' as const, text: reviewMessage(all) }];
    const running = runningTurn(transcript);
    try {
      if (running) {
        try {
          await call('turn/steer', { sessionId, expectedTurnId: running.id, input });
          setComments([]);
          return;
        } catch {
          // The turn ended meanwhile: the comments start the next one.
        }
      }
      await call('turn/start', { sessionId, input });
      setComments([]);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  /** The comments with the one being typed, if it says anything. */
  const withDraft = (): LineComment[] => (draft?.text.trim() ? [...comments, { ...draft.at, comment: draft.text }] : comments);

  // Cmd+Enter sends every comment, the one being typed included.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
        const all = withDraft();
        if (!all.length) return;
        e.preventDefault();
        setDraft(null);
        void send(all);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  const turns: Entry[] = changed.map(
    (t): Entry => ({
      type: 'item',
      id: t.turn.id,
      label: `Turn ${t.index}`,
      detail: t === changed[0] ? 'latest' : undefined,
      checked: t === shown,
      onPick: () => setTurnId(t.turn.id),
    }),
  );

  return (
    <div className="review" data-testid="review">
      {shown && (
        <header>
          <MenuButton label="Turn" entries={turns}>
            Turn {shown.index}
            <span className="caret" aria-hidden>
              ⌄
            </span>
          </MenuButton>
        </header>
      )}
      {!shown && <p className="review-empty">No changes to show. What a command changed isn't shown here.</p>}
      <div className="review-files">
        {files.map((file) => (
          <FileDiff
            key={file.path}
            file={file}
            comments={comments.filter((c) => c.path === file.path)}
            draft={draft}
            onLine={(key, at) => setDraft({ key, at, text: '' })}
            onDraft={(text) => setDraft((d) => (d ? { ...d, text } : d))}
            onCancel={() => setDraft(null)}
            onAdd={() => {
              setComments(withDraft());
              setDraft(null);
            }}
            onRemove={(c) => setComments(comments.filter((x) => x !== c))}
          />
        ))}
      </div>
      {comments.length > 0 && (
        <footer>
          <button className="primary" onClick={() => void send(comments)}>
            Send {comments.length} comment{comments.length === 1 ? '' : 's'}
            <span className="key-hint"> ⌘↵</span>
          </button>
          <button onClick={() => setComments([])}>Discard</button>
          {error && <span className="problem">{error}</span>}
        </footer>
      )}
    </div>
  );
}

interface FileProps {
  file: DiffFile;
  comments: LineComment[];
  draft: Draft | null;
  onLine(key: string, at: Omit<LineComment, 'comment'>): void;
  onDraft(text: string): void;
  onCancel(): void;
  onAdd(): void;
  onRemove(comment: LineComment): void;
}

/** How many unchanged lines a hunk header stands for: those since the hunk before, or the file's start. */
function skipped(lines: DiffLine[], at: number): number | null {
  const header = /^@@ -(\d+)/.exec(lines[at].text);
  if (!header) return null;
  const start = Number(header[1]);
  let before = 0;
  for (let i = at - 1; i >= 0; i--) {
    const old = lines[i].oldLine;
    if (old !== null) {
      before = old;
      break;
    }
  }
  return Math.max(0, start - before - 1);
}

function FileDiff({ file, comments, draft, onLine, onDraft, onCancel, onAdd, onRemove }: FileProps) {
  const [open, setOpen] = useState(true);
  const slash = file.path.lastIndexOf('/');
  return (
    <section className="review-file" data-testid="review-file">
      <button className="review-path" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="chevron" aria-hidden>
          {open ? '⌄' : '›'}
        </span>
        <span className="review-name">
          {file.path.slice(slash + 1)}
          {slash > 0 && <span className="meta"> {file.path.slice(0, slash)}</span>}
        </span>
        <span className="diff-add">+{file.added}</span>
        <span className="diff-del">−{file.removed}</span>
      </button>
      {open && (
        <table className="review-lines">
          <tbody>
            {file.lines.map((line, i) => {
              const key = `${file.path}:${i}`;
              if (line.kind === 'hunk') {
                const n = skipped(file.lines, i);
                return (
                  <tr key={key} className="review-skip">
                    <td colSpan={3}>{n ? `${n} unmodified line${n === 1 ? '' : 's'}` : ' '}</td>
                  </tr>
                );
              }
              const number = line.newLine ?? line.oldLine ?? 0;
              const side = line.newLine !== null ? 'new' : 'old';
              const here = comments.filter((c) => c.line === number && c.side === side);
              return [
                <tr key={key} className={`review-line ${line.kind}`}>
                  <td className="num">
                    <button
                      className="comment-add"
                      aria-label={`Comment on line ${number}`}
                      title="Comment on this line"
                      onClick={() => onLine(key, { path: file.path, line: number, side, text: line.text })}
                    >
                      +
                    </button>
                    {line.oldLine ?? ''}
                  </td>
                  <td className="num">{line.newLine ?? ''}</td>
                  <td className="code">{line.text}</td>
                </tr>,
                ...here.map((c, j) => (
                  <tr key={`${key}-c${j}`} className="review-comment" data-testid="review-comment">
                    <td colSpan={3}>
                      <span className="comment-text">{c.comment}</span>
                      <button onClick={() => onRemove(c)}>Remove</button>
                    </td>
                  </tr>
                )),
                draft?.key === key ? (
                  <tr key={`${key}-draft`} className="review-draft">
                    <td colSpan={3}>
                      <textarea
                        autoFocus
                        rows={2}
                        aria-label="Comment"
                        placeholder="Comment on this line. Enter adds it; ⌘↵ sends every comment."
                        value={draft.text}
                        onChange={(e) => onDraft(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.nativeEvent.isComposing) {
                            e.preventDefault();
                            onAdd();
                          }
                          if (e.key === 'Escape') onCancel();
                        }}
                      />
                      <div className="actions">
                        <button className="primary" onClick={onAdd} disabled={!draft.text.trim()}>
                          Add comment
                        </button>
                        <button onClick={onCancel}>Cancel</button>
                      </div>
                    </td>
                  </tr>
                ) : null,
              ];
            })}
          </tbody>
        </table>
      )}
    </section>
  );
}
