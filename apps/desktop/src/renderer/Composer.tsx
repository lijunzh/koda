// The prompt box: Enter sends (or adds to the running turn, as the TUI
// steers), Shift+Enter starts a new line, Stop or Esc stops the turn.
import { type ReactNode, useEffect, useRef, useState } from 'react';
import { call } from './bridge';
import { statusLine, type TurnView, workedFor } from './transcript';

interface Props {
  sessionId: string;
  running: TurnView | undefined;
  /** Whether the session is open on the server and can take a prompt. */
  ready: boolean;
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

export function Composer({ sessionId, running, ready, waiting, foot }: Props) {
  const [text, setText] = useState('');
  const [error, setError] = useState<string | null>(null);
  const box = useRef<HTMLTextAreaElement>(null);
  const now = useClock(running !== undefined);

  useEffect(() => box.current?.focus(), [sessionId]);

  const stop = () => {
    if (running) void call('turn/interrupt', { sessionId, turnId: running.id }).catch(() => {});
  };

  const send = async () => {
    const prompt = text.trim();
    if (!prompt || !ready) return;
    setText('');
    setError(null);
    const input = [{ type: 'text' as const, text: prompt }];
    try {
      if (running) {
        try {
          await call('turn/steer', { sessionId, expectedTurnId: running.id, input });
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

  return (
    <div className="composer">
      {running && (
        <div className="working" data-testid="working">
          <span className="spinner" aria-hidden>
            ✻
          </span>{' '}
          {waiting ? waitingLabel(waiting) : statusLine(running)}
          {running.startedAt ? <span className="meta"> {workedFor(Math.max(0, now - running.startedAt * 1000))}</span> : null}
        </div>
      )}
      {error && <p className="problem">{error}</p>}
      <div className="composer-row">
        <textarea
          ref={box}
          value={text}
          rows={3}
          placeholder={running ? 'Add to what Koda is doing…' : 'Ask Koda…'}
          aria-label="Prompt"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            // Enter that confirms an input method's candidate doesn't send.
            if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing && e.keyCode !== 229) {
              e.preventDefault();
              void send();
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
          <button className="send" aria-label="Send" onClick={() => void send()} disabled={!ready || !text.trim()}>
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
