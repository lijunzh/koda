// More of a session's controls: its goal (as the TUI's /goal), the commands
// and agents running in the background, and forking it.
import { useCallback, useEffect, useState } from 'react';
import type { BackgroundTerminalsListResponse, Goal } from '../protocol/types.gen';
import { call, type RpcMessage } from './bridge';
import { workedFor } from './transcript';

const GOAL_STATUS: Record<Goal['status'], string> = {
  active: 'working on it',
  paused: 'paused',
  blocked: 'blocked',
  budgetLimited: 'out of budget',
  complete: 'done',
};

/** The goal, and setting, pausing, resuming or clearing it. */
export function GoalLine({ sessionId, goal }: { sessionId: string; goal: Goal | null }) {
  const [editing, setEditing] = useState<string | null>(null);
  const [budget, setBudget] = useState('');
  const [error, setError] = useState<string | null>(null);
  const run = (p: Promise<unknown>) => p.then(() => setError(null), (e: Error) => setError(e.message));

  if (editing !== null) {
    return (
      <div className="goal editing" data-testid="goal-form">
        <textarea
          aria-label="Goal"
          rows={2}
          autoFocus
          placeholder="What Koda should keep working on until it's done"
          value={editing}
          onChange={(e) => setEditing(e.target.value)}
        />
        <input aria-label="Token budget" inputMode="numeric" placeholder="Token budget (optional)" value={budget} onChange={(e) => setBudget(e.target.value)} />
        <div className="actions">
          <button
            className="primary"
            disabled={!editing.trim() || (budget !== '' && !/^\d+$/.test(budget))}
            onClick={() =>
              void run(
                call('session/goal/set', {
                  sessionId,
                  objective: editing.trim(),
                  ...(budget ? { tokenBudget: Number(budget) } : {}),
                }),
              ).then(() => setEditing(null))
            }
          >
            Set goal
          </button>
          <button onClick={() => setEditing(null)}>Cancel</button>
          {error && <span className="problem">{error}</span>}
        </div>
      </div>
    );
  }
  if (!goal) {
    return (
      <button className="goal-set" onClick={() => setEditing('')}>
        Set a goal
      </button>
    );
  }
  const used = goal.tokenBudget ? `${goal.tokensUsed.toLocaleString()} of ${goal.tokenBudget.toLocaleString()} tokens` : `${goal.tokensUsed.toLocaleString()} tokens`;
  return (
    <div className="goal" data-testid="goal">
      <span title={goal.objective}>
        🎯 <strong>{goal.objective.length > 80 ? `${goal.objective.slice(0, 79)}…` : goal.objective}</strong>
      </span>
      <span className="meta">
        {' '}
        · {GOAL_STATUS[goal.status]} · {used} · {workedFor(goal.timeUsedSeconds * 1000)}
      </span>
      {goal.status === 'active' && <button onClick={() => void run(call('session/goal/set', { sessionId, status: 'paused' }))}>Pause</button>}
      {(goal.status === 'paused' || goal.status === 'blocked' || goal.status === 'budgetLimited') && (
        <button onClick={() => void run(call('session/goal/set', { sessionId, status: 'active' }))}>Resume</button>
      )}
      <button onClick={() => void run(call('session/goal/clear', { sessionId }))}>Clear</button>
      {error && <span className="problem">{error}</span>}
    </div>
  );
}

/** What runs in the background: the model's commands and agents, each with Stop. */
export function Background({ sessionId, pane }: { sessionId: string; pane?: boolean }) {
  const [list, setList] = useState<BackgroundTerminalsListResponse | null>(null);
  // As a pane the list is always open, and says so when nothing runs.
  const [open, setOpen] = useState(Boolean(pane));
  const refresh = useCallback(() => {
    call('session/backgroundTerminals/list', { sessionId }).then(setList, () => setList(null));
  }, [sessionId]);

  useEffect(() => {
    refresh();
    // What starts or ends them comes as these; the list is read again after each.
    return window.koda.onMessages((batch: RpcMessage[]) => {
      const mine = batch.some((m) => {
        const p = m.params as { sessionId?: string } | undefined;
        return (
          p?.sessionId === sessionId &&
          (m.method === 'item/completed' || m.method === 'turn/completed' || m.method === 'session/backgroundTerminals/exited')
        );
      });
      if (mine) refresh();
    });
  }, [sessionId, refresh]);

  const agents = list?.agents ?? [];
  const count = (list?.data.length ?? 0) + agents.length;
  if (!count) return pane ? <p className="review-empty">Nothing is running in the background.</p> : null;
  return (
    <span className={pane ? 'background as-pane' : 'background'} data-testid={pane ? 'background-pane' : 'background'}>
      {!pane && <button onClick={() => setOpen(!open)}>{count} in the background</button>}
      {open && (
        <ul className="background-list">
          {list!.data.map((t) => (
            <li key={t.processId}>
              <code>{t.command}</code> <span className="meta">{workedFor(Date.now() - t.startedAt)}</span>{' '}
              <button onClick={() => void call('session/backgroundTerminals/terminate', { sessionId, processId: t.processId }).finally(refresh)}>
                Stop
              </button>
            </li>
          ))}
          {agents.map((a) => (
            <li key={a.agentId}>
              🤖 {a.agentName} <span className="meta">{a.prompt.length > 60 ? `${a.prompt.slice(0, 59)}…` : a.prompt}</span>{' '}
              <button onClick={() => void call('session/backgroundTerminals/terminate', { sessionId, processId: a.agentId }).finally(refresh)}>
                Stop
              </button>
            </li>
          ))}
        </ul>
      )}
    </span>
  );
}
