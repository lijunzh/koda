// The open session's conversation, in rows (`transcript.ts`): grouped and
// folded in the normal view, everything in the full one. A click opens or
// closes a folded row, as in the TUI.
import { memo, useLayoutEffect, useRef, useState } from 'react';
import { DiffText, Markdown } from './Markdown';
import { callTitle, type Row, shownOutput, type TranscriptState, turnRows, type View, workedFor } from './transcript';

interface Props {
  transcript: TranscriptState;
  view: View;
}

export function Conversation({ transcript, view }: Props) {
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const toggle = (key: string) =>
    setOpen((before) => {
      const next = new Set(before);
      if (!next.delete(key)) next.add(key);
      return next;
    });
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  // Follow new output while the view is at the bottom; reading further up stays put.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  });
  return (
    <div
      className="transcript"
      ref={scroller}
      data-testid="transcript"
      onScroll={(e) => {
        const el = e.currentTarget;
        pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
      }}
    >
      {transcript.turns.map((turn) => (
        <section key={turn.id} className="turn">
          {turnRows(turn, view, true).map((row) => (
            <RowView key={row.key} row={row} open={open} toggle={toggle} full={view === 'verbose'} />
          ))}
        </section>
      ))}
    </div>
  );
}

interface RowProps {
  row: Row;
  open: ReadonlySet<string>;
  toggle(key: string): void;
  full: boolean;
}

const RowView = memo(function RowView({ row, open, toggle, full }: RowProps) {
  const opened = full || open.has(row.key);
  switch (row.kind) {
    case 'user':
      return (
        <div className="row user" data-testid="user-message">
          <div className="user-text">{row.text}</div>
        </div>
      );
    case 'message':
      return (
        <div className={`row message${row.streaming ? ' streaming' : ''}`} data-testid="agent-message">
          <Markdown text={row.text} />
        </div>
      );
    case 'thought':
      return (
        <div className="row thought">
          <button className="fold" onClick={() => toggle(row.key)}>
            ∴ {row.running ? 'Thinking…' : row.ms !== null ? `Thought for ${workedFor(row.ms)}` : 'Thought'}
          </button>
          {opened && <div className="thought-text">{row.text}</div>}
        </div>
      );
    case 'lookups':
      return (
        <div className="row lookups" data-testid="lookups">
          <button className="fold" onClick={() => toggle(row.key)}>
            <span className="dot read">●</span> <strong>{row.label}</strong>
            {row.failed > 0 && <span className="failed"> · {row.failed} failed</span>}
          </button>
          {opened && (
            <ul className="calls">
              {row.thoughts.map((text, i) => (
                <li key={`t${i}`} className="thought-text">
                  {text}
                </li>
              ))}
              {row.calls.map((call) => (
                <li key={call.id}>
                  <code>{callTitle(call)}</code>
                </li>
              ))}
            </ul>
          )}
        </div>
      );
    case 'tool':
      return <ToolRow row={row} opened={opened} toggle={toggle} full={full} />;
    case 'compaction':
      return <div className="row note">{row.text}</div>;
    case 'worked':
      return (
        <div className="row worked" data-testid="worked">
          <button className="fold" onClick={() => toggle(row.key)}>
            {opened ? '▾' : '▸'} {row.label}
          </button>
          {opened && (
            <div className="worked-rows">
              {row.rows.map((inner) => (
                <RowView key={inner.key} row={inner} open={open} toggle={toggle} full={full} />
              ))}
            </div>
          )}
        </div>
      );
    case 'note':
      return (
        <div className={`row note ${row.tone}`} data-testid="turn-note">
          {row.text}
        </div>
      );
  }
});

function ToolRow({
  row,
  opened,
  toggle,
  full,
}: {
  row: Extract<Row, { kind: 'tool' }>;
  opened: boolean;
  toggle(key: string): void;
  full: boolean;
}) {
  const call = row.call;
  const { lines, hidden, exitCode } = shownOutput(call, opened);
  const ms = call.durationMs ?? null;
  const failed = call.status === 'failed' || (exitCode !== null && exitCode !== 0);
  return (
    <div className={`row tool status-${call.status}`} data-testid="tool-call">
      <div className="tool-head">
        <span className={`dot ${call.effect ?? ''}`}>●</span> <code>{callTitle(call)}</code>
        {ms !== null && ms >= 1000 && <span className="meta"> · {workedFor(ms)}</span>}
        {failed && exitCode !== null && exitCode !== 0 && <span className="failed"> · exit {exitCode}</span>}
        {call.status === 'declined' && <span className="failed"> · declined</span>}
      </div>
      {call.progress && call.status === 'inProgress' && <div className="meta">{call.progress}</div>}
      {row.children.length > 0 && call.status !== 'inProgress' && (
        <div className="meta">
          Done ({row.children.length} tool use{row.children.length === 1 ? '' : 's'}
          {ms !== null ? ` · ${workedFor(ms)}` : ''})
        </div>
      )}
      {(call.changes ?? []).map((change) => (
        <pre key={change.path} className="tool-diff">
          <DiffText text={change.diff} />
        </pre>
      ))}
      {lines.length > 0 && (
        <pre className="tool-out">
          {lines.map((line, i) => (
            <span key={i} className={line.stderr ? 'stderr' : undefined}>
              {line.text}
              {'\n'}
            </span>
          ))}
        </pre>
      )}
      {!full && (hidden > 0 || (opened && lines.length > 0)) && (
        <button className="fold more-lines" onClick={() => toggle(row.key)}>
          {opened ? 'Show less' : `… +${hidden} lines`}
        </button>
      )}
    </div>
  );
}
