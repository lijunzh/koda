// The open session's controls, under its title: model, effort and mode (as
// the TUI's /model, /effort and Shift+Tab), how much context is left, and
// compacting it.
import { useEffect, useState } from 'react';
import type { Model } from '../protocol/types.gen';
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
  const [models, setModels] = useState<Model[]>([]);
  const [error, setError] = useState<string | null>(null);
  const provider = settings?.modelProvider;

  useEffect(() => {
    if (!provider) return;
    let live = true;
    call('model/list', { modelProvider: provider }).then(
      (answer) => live && setModels(answer.data),
      () => live && setModels([]),
    );
    return () => {
      live = false;
    };
  }, [provider]);

  if (!settings) return null;
  const update = (change: { model?: string; effort?: string }) =>
    call('session/settings/update', { sessionId, ...change }).then(
      () => setError(null),
      (e: Error) => setError(e.message),
    );
  const current = models.find((m) => m.id === settings.model || (settings.alias && m.alias === settings.alias));
  const efforts = current?.supportedReasoningEfforts ?? [];
  // The session's own model leads the list even when the provider doesn't list it.
  const choices = current || !settings.model ? models : [{ id: settings.model, isDefault: false, supportedReasoningEfforts: [] } as Model, ...models];

  return (
    <div className="session-bar" data-testid="session-bar">
      <label>
        <span className="meta">Model</span>
        <select
          aria-label="Model"
          value={current ? (current.alias ?? current.id) : settings.model}
          onChange={(e) => void update({ model: e.target.value })}
        >
          {choices.map((m) => (
            <option key={m.alias ?? m.id} value={m.alias ?? m.id}>
              {m.alias ? `${m.alias} (${m.id})` : m.id}
            </option>
          ))}
        </select>
      </label>
      {efforts.length > 0 && (
        <label>
          <span className="meta">Effort</span>
          <select
            aria-label="Effort"
            value={settings.effort ?? current?.defaultReasoningEffort ?? ''}
            onChange={(e) => void update({ effort: e.target.value })}
          >
            {efforts.map((effort) => (
              <option key={effort}>{effort}</option>
            ))}
          </select>
        </label>
      )}
      <div className="views mode" role="group" aria-label="Mode">
        {(['auto', 'plan'] as const).map((mode) => (
          <button
            key={mode}
            aria-pressed={settings.mode === mode}
            title={mode === 'auto' ? 'Edits and sandboxed commands without asking' : 'Read only: Koda plans, and changes nothing'}
            onClick={() => {
              if (settings.mode !== mode) void window.koda.setMode(sessionId, mode).catch((e: Error) => setError(e.message));
            }}
          >
            {mode === 'auto' ? 'Auto' : 'Plan'}
          </button>
        ))}
      </div>
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
