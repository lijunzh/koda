import type { Selected } from './App';
import { useState } from 'react';
import { call } from './bridge';
import { Composer } from './Composer';
import type { ControlsState } from './controls';
import { PlanPanel, SessionBar } from './SessionBar';
import type { Pending } from './requests';
import { Requests } from './RequestCards';
import { type ListedSession, statusLabel, title } from './sessions';
import { Conversation } from './Conversation';
import { runningTurn, type TranscriptState, type View, VIEWS } from './transcript';

/** Claude Code's names for its desktop views. */
const VIEW_NAMES: Record<View, string> = { normal: 'Normal', thinking: 'Thinking', verbose: 'Verbose' };

interface Props {
  selected: Selected | null;
  listed?: ListedSession;
  transcript: TranscriptState | null;
  view: View;
  onView(view: View): void;
  /** What the server waits on the user for in this session. */
  requests: Pending[];
  onAnswer(id: number | string, result: unknown): Promise<void>;
  controls: ControlsState | null;
  planShown: boolean;
}

/** The open session: its conversation, and the prompt box. */
export function SessionView({ selected, listed, transcript, view, onView, requests, onAnswer, controls, planShown }: Props) {
  const [renaming, setRenaming] = useState<string | null>(null);
  if (!selected) {
    return (
      <main className="session-view empty-view">
        <p>Open a session from the list, or start a new one.</p>
      </main>
    );
  }
  const session = selected.session;
  const firstPrompt = transcript?.turns
    .flatMap((turn) => turn.items)
    .find((item) => item.type === 'userMessage');
  const prompted =
    firstPrompt?.type === 'userMessage' ? (firstPrompt.content.find((p) => p.type === 'text') as { text?: string })?.text : '';
  const name = title({
    name: listed?.name ?? session?.name,
    preview: listed?.preview || session?.preview || prompted || '',
  });
  const folder = session?.cwd ?? listed?.cwd ?? null;
  const label = statusLabel(listed?.status ?? session?.status ?? null);
  const running = transcript ? runningTurn(transcript) : undefined;
  return (
    <main className="session-view" data-testid="session-view">
      <header>
        <div className="heading">
          {renaming === null ? (
            <h1 title="Click to rename" onClick={() => setRenaming(listed?.name ?? session?.name ?? '')}>
              {name}
            </h1>
          ) : (
            <input
              className="rename"
              aria-label="Session name"
              autoFocus
              maxLength={100}
              value={renaming}
              onChange={(e) => setRenaming(e.target.value)}
              onBlur={() => setRenaming(null)}
              onKeyDown={(e) => {
                if (e.key === 'Escape') setRenaming(null);
                if (e.key === 'Enter' && renaming.trim() && !e.nativeEvent.isComposing) {
                  void call('session/name/set', { sessionId: selected.id, name: renaming.trim() }).finally(() => setRenaming(null));
                }
              }}
            />
          )}
          <p className="subtitle">
            {folder && <code data-testid="session-folder">{folder}</code>}
            {session?.model && <span> · {session.model}</span>}
            {label && <span data-testid="session-status"> · {label}</span>}
          </p>
        </div>
        <div className="views" role="group" aria-label="Transcript view (Ctrl+O)">
          {VIEWS.map((v) => (
            <button key={v} aria-pressed={view === v} onClick={() => onView(v)}>
              {VIEW_NAMES[v]}
            </button>
          ))}
        </div>
        <button className="delete" onClick={() => void window.koda.deleteSession(selected.id)}>
          Delete
        </button>
      </header>
      {controls && <SessionBar sessionId={selected.id} controls={controls} />}
      {selected.error && <p className="problem">{selected.error}</p>}
      {!session && !selected.error && <p className="detail">Opening…</p>}
      {transcript && <Conversation transcript={transcript} view={view} />}
      <div className="bottom">
        {controls && <PlanPanel controls={controls} shown={planShown} />}
        <Requests requests={requests} onAnswer={onAnswer} />
        <Composer sessionId={selected.id} running={running} ready={Boolean(session)} waiting={requests[0]?.method ?? null} />
      </div>
    </main>
  );
}
