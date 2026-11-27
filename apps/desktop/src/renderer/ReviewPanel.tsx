// The changes a session's turns made, beside the conversation: one turn at a
// time (the latest first), file by file, with line numbers. Clicking a line
// adds a comment to it; "Send comments" sends them all to Koda as one
// message, as Claude Code's desktop app sends review comments together.
import { useMemo, useState } from 'react';
import { call } from './bridge';
import { type DiffFile, type LineComment, parseDiff, reviewMessage, type TurnDiffs, turnDiff } from './diffs';
import { runningTurn, type TranscriptState } from './transcript';

interface Props {
  sessionId: string;
  /** The session's folder, which the changes' paths are relative to. */
  cwd: string | null;
  transcript: TranscriptState;
  diffs: TurnDiffs;
  onClose(): void;
}

export function ReviewPanel({ sessionId, cwd, transcript, diffs, onClose }: Props) {
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
  const [draft, setDraft] = useState<{ key: string; at: Omit<LineComment, 'comment'>; text: string } | null>(null);
  const [error, setError] = useState<string | null>(null);

  const send = async () => {
    const input = [{ type: 'text' as const, text: reviewMessage(comments) }];
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

  return (
    <aside className="review" data-testid="review" aria-label="Changes">
      <header>
        <strong>Changes</strong>
        {changed.length > 1 && (
          <select aria-label="Turn" value={shown?.turn.id} onChange={(e) => setTurnId(e.target.value)}>
            {changed.map((t) => (
              <option key={t.turn.id} value={t.turn.id}>
                Turn {t.index}
              </option>
            ))}
          </select>
        )}
        <button aria-label="Close changes" onClick={onClose}>
          ✕
        </button>
      </header>
      {!shown && <p className="meta">No file changes yet. Changes a command made aren't shown here.</p>}
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
              if (draft?.text.trim()) setComments([...comments, { ...draft.at, comment: draft.text }]);
              setDraft(null);
            }}
            onRemove={(c) => setComments(comments.filter((x) => x !== c))}
          />
        ))}
      </div>
      {comments.length > 0 && (
        <footer>
          <button className="primary" onClick={() => void send()}>
            Send {comments.length} comment{comments.length === 1 ? '' : 's'}
          </button>
          <button onClick={() => setComments([])}>Discard</button>
          {error && <span className="problem">{error}</span>}
        </footer>
      )}
    </aside>
  );
}

interface FileProps {
  file: DiffFile;
  comments: LineComment[];
  draft: { key: string; text: string } | null;
  onLine(key: string, at: Omit<LineComment, 'comment'>): void;
  onDraft(text: string): void;
  onCancel(): void;
  onAdd(): void;
  onRemove(comment: LineComment): void;
}

function FileDiff({ file, comments, draft, onLine, onDraft, onCancel, onAdd, onRemove }: FileProps) {
  return (
    <section className="review-file" data-testid="review-file">
      <div className="review-path">
        <code>{file.path}</code>{' '}
        <span className="diff-add">+{file.added}</span> <span className="diff-del">−{file.removed}</span>
      </div>
      <table className="review-lines">
        <tbody>
          {file.lines.map((line, i) => {
            const key = `${file.path}:${i}`;
            if (line.kind === 'hunk') {
              return (
                <tr key={key} className="diff-hunk">
                  <td colSpan={3}>{line.text}</td>
                </tr>
              );
            }
            const number = line.newLine ?? line.oldLine ?? 0;
            const side = line.newLine !== null ? 'new' : 'old';
            const here = comments.filter((c) => c.line === number && c.side === side);
            return [
              <tr
                key={key}
                className={`review-line ${line.kind}`}
                title="Click to comment on this line"
                onClick={() => onLine(key, { path: file.path, line: number, side, text: line.text })}
              >
                <td className="num">{line.oldLine ?? ''}</td>
                <td className="num">{line.newLine ?? ''}</td>
                <td className="code">
                  {line.kind === 'add' ? '+' : line.kind === 'del' ? '−' : ' '}
                  {line.text}
                </td>
              </tr>,
              ...here.map((c, j) => (
                <tr key={`${key}-c${j}`} className="review-comment" data-testid="review-comment">
                  <td colSpan={3}>
                    💬 {c.comment} <button onClick={() => onRemove(c)}>Remove</button>
                  </td>
                </tr>
              )),
              draft?.key === key ? (
                <tr key={`${key}-draft`}>
                  <td colSpan={3}>
                    <textarea
                      autoFocus
                      rows={2}
                      aria-label="Comment"
                      value={draft.text}
                      onChange={(e) => onDraft(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
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
    </section>
  );
}
