// The prompt box: Enter sends (or adds to the running turn, as the TUI
// steers), Shift+Enter starts a new line, Stop or Esc stops the turn. Enter
// while the session is still opening sends once it is open. Text added to a
// turn that was stopped or failed before the model took it comes back here.
import { type ReactNode, useEffect, useRef, useState } from 'react';
import { call } from './bridge';
import { type SentSteer, settle, steerId, withDropped } from './steers';
import { statusLine, type TurnView, workedFor } from './transcript';

interface Props {
  sessionId: string;
  running: TurnView | undefined;
  /** The session's turns: they show what became of text added to one. */
  turns: readonly TurnView[];
  /** Whether the session is open on the server and can take a prompt. */
  ready: boolean;
  /** The session couldn't be opened: nothing typed here can be sent. */
  failed?: boolean;
  /** What the server waits on the user for, if anything: the status line says so. */
  waiting: string | null;
  /** The controls under the box: the model and the mode. */
  foot?: ReactNode;
}

/** The status line while Koda waits on you, in the TUI's words. */
function waitingLabel(method: string): string {
  switch (method) {
    case 'item/toolCall/requestApproval':
      return 'Waiting for your approval';
    case 'item/tool/requestUserInput':
      return 'Waiting for your answer';
    case 'turn/loopCap/requestDecision':
      return 'Waiting for you to go on or stop';
    default:
      return 'Waiting for you';
  }
}

export function Composer({ sessionId, running, turns, ready, failed, waiting, foot }: Props) {
  const [text, setText] = useState('');
  const [error, setError] = useState<string | null>(null);
  // Enter came while the session was opening: the prompt goes once it is open.
  const [pending, setPending] = useState(false);
  // Text added to a running turn that the model hasn't taken yet.
  const [steers, setSteers] = useState<SentSteer[]>([]);
  const box = useRef<HTMLTextAreaElement>(null);
  const now = useClock(running !== undefined);

  useEffect(() => box.current?.focus(), [sessionId]);

  const stop = () => {
    if (running) void call('turn/interrupt', { sessionId, turnId: running.id }).catch(() => {});
  };

  const deliver = async (prompt: string) => {
    setText('');
    setError(null);
    const input = [{ type: 'text' as const, text: prompt }];
    try {
      if (running) {
        try {
          const id = steerId(running.id);
          await call('turn/steer', { sessionId, expectedTurnId: running.id, input, clientUserMessageId: id });
          // Kept once the server has it: one it refused starts the next turn below.
          setSteers((sent) => [...sent, { id, turnId: running.id, text: prompt }]);
          return;
        } catch {
          // The turn ended meanwhile: this starts the next one.
        }
      }
      await call('turn/start', { sessionId, input });
    } catch (e) {
      setText(prompt);
      setError((e as Error).message);
    }
  };

  // A turn that was stopped or failed gives back the text it never took.
  useEffect(() => {
    if (!steers.length) return;
    const { waiting, dropped } = settle(steers, turns);
    if (waiting.length !== steers.length) setSteers(waiting);
    if (dropped.length) setText((draft) => withDropped(dropped, draft));
  }, [steers, turns]);

  const send = () => {
    const prompt = text.trim();
    if (!prompt) return;
    if (!ready) {
      // Kept, and sent when the session is open; one that failed to open keeps the text.
      if (!failed) setPending(true);
      return;
    }
    void deliver(prompt);
  };

  // The session is open: what was sent while it opened goes now, once. Editing
  // the text or Esc took `pending` back, so the text is the one Enter was pressed on.
  useEffect(() => {
    if (!pending) return;
    if (failed) setPending(false);
    else if (ready) {
      setPending(false);
      void deliver(text.trim());
    }
    // `deliver` and `text` are this render's: the one in which the session opened.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pending, ready, failed]);

  // Shown above the box until the model takes them, as Codex's TUI previews
  // pending steers above its composer (`pending_input_preview.rs`).
  const waitingSteers = running ? steers.filter((steer) => steer.turnId === running.id) : [];

  return (
    <div className="composer">
      {pending && !running && !failed && (
        <div className="working" data-testid="send-pending">
          <span className="spinner" aria-hidden>
            ✻
          </span>{' '}
          Opening the session… your message is sent when it's open <span className="meta">(Esc to take it back)</span>
        </div>
      )}
      {running && (
        <div className="working" data-testid="working">
          <span className="spinner" aria-hidden>
            ✻
          </span>{' '}
          {waiting ? waitingLabel(waiting) : statusLine(running)}
          {running.startedAt ? <span className="meta"> {workedFor(Math.max(0, now - running.startedAt * 1000))}</span> : null}
        </div>
      )}
      {waitingSteers.length > 0 && (
        <ul className="steers-waiting" data-testid="steers-waiting" aria-label="Koda reads these at its next step">
          {waitingSteers.map((steer) => (
            <li key={steer.id}>{steer.text}</li>
          ))}
        </ul>
      )}
      {error && <p className="problem">{error}</p>}
      <div className="composer-row">
        <textarea
          ref={box}
          value={text}
          rows={3}
          placeholder={running ? 'Add to what Koda is doing…' : 'Ask Koda…'}
          aria-label="Prompt"
          onChange={(e) => {
            setText(e.target.value);
            // Changed since Enter: it isn't the prompt that was sent any more.
            setPending(false);
          }}
          onKeyDown={(e) => {
            // Enter that confirms an input method's candidate doesn't send.
            if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing && e.keyCode !== 229) {
              e.preventDefault();
              send();
            } else if (e.key === 'Escape' && pending) {
              e.preventDefault();
              setPending(false);
            } else if (e.key === 'Escape' && running) {
              e.preventDefault();
              stop();
            }
          }}
        />
        {running ? (
          <button className="send stop" aria-label="Stop" title="Stop (Esc)" onClick={stop}>
            ■
          </button>
        ) : (
          <button className="send" aria-label="Send" onClick={send} disabled={!ready || !text.trim()}>
            ↑
          </button>
        )}
      </div>
      {foot && (
        <div className="composer-foot">
          <div className="foot-left" />
          <div className="foot-right">{foot}</div>
        </div>
      )}
    </div>
  );
}

/** The time now, ticking each second while `on`. */
function useClock(on: boolean): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!on) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [on]);
  return now;
}
