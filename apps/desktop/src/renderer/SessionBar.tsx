// The open session's bar, under its title: how much context is left,
// compacting it, forking, what runs in the background and the goal. The
// model, effort and mode are under the prompt box (`SessionControls`).
import { useState } from 'react';
import { call } from './bridge';
import type { ControlsState } from './controls';
import { Background, GoalLine } from './SessionExtras';

export function SessionBar({
  sessionId,
  controls,
  onForked,
}: {
  sessionId: string;
  controls: ControlsState;
  /** Open the fork once the server has made it. */
  onForked(session: import('../protocol/types.gen').Session): void;
}) {
  const settings = controls.settings;
  const [error, setError] = useState<string | null>(null);

  if (!settings) return null;
  return (
    <div className="session-bar" data-testid="session-bar">
      {controls.contextLeft !== null && (
        <span className="meta" data-testid="context-left" title="Of the model's context window">
          {controls.contextLeft}% context left
        </span>
      )}
      <button
        title="Summarize the conversation so far, to free context"
        onClick={() => void call('session/compact/start', { sessionId }).catch((e: Error) => setError(e.message))}
      >
        Compact
      </button>
      <button
        title="A copy of this conversation to go on in, leaving this one as it is"
        onClick={() =>
          void call('session/fork', { sessionId }).then(
            (answer) => onForked(answer.session),
            (e: Error) => setError(e.message),
          )
        }
      >
        Fork
      </button>
      <Background sessionId={sessionId} />
      {error && <span className="problem">{error}</span>}
      <div className="goal-row">
        <GoalLine sessionId={sessionId} goal={controls.goal} />
      </div>
    </div>
  );
}

/** The model's plan for the running turn, as Claude Code's checklist; Ctrl+T hides or shows it. */
export function PlanPanel({ controls, shown }: { controls: ControlsState; shown: boolean }) {
  const left = controls.plan.filter((step) => step.status !== 'completed').length;
  if (!controls.plan.length || left === 0) return null;
  if (!shown) {
    return (
      <div className="plan meta" data-testid="plan">
        Plan: {left} step{left === 1 ? '' : 's'} left (Ctrl+T to show)
      </div>
    );
  }
  return (
    <ol className="plan" data-testid="plan" aria-label="Plan (Ctrl+T to hide)">
      {controls.plan.map((step, i) => (
        <li key={i} className={`plan-step ${step.status}`}>
          <span aria-hidden>{step.status === 'completed' ? '✓' : step.status === 'inProgress' ? '◐' : '○'}</span> {step.step}
        </li>
      ))}
    </ol>
  );
}
