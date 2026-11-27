import type { Selected } from './App';
import { Composer } from './Composer';
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
}

/** The open session: its conversation, and the prompt box. */
export function SessionView({ selected, listed, transcript, view, onView }: Props) {
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
          <h1>{name}</h1>
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
      {selected.error && <p className="problem">{selected.error}</p>}
      {!session && !selected.error && <p className="detail">Opening…</p>}
      {transcript && <Conversation transcript={transcript} view={view} />}
      <Composer sessionId={selected.id} running={running} ready={Boolean(session)} />
    </main>
  );
}
