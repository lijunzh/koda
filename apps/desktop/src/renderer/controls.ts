// What the header and the plan panel show for the open session: its
// settings (model, effort, mode), how full its context is, and the model's
// plan, kept current from the server's notifications.
import type { Goal, PlanStep, SessionSettings, TokenUsage } from '../protocol/types.gen';
import type { RpcMessage } from './bridge';

export interface ControlsState {
  sessionId: string;
  settings: SessionSettings | null;
  /** Percent of the context window left, once a model call has said. */
  contextLeft: number | null;
  /** The plan of the running turn; gone when the turn ends, as the TUI's panel. */
  plan: PlanStep[];
  /** The session's goal, if it has one. */
  goal: Goal | null;
}

export function controlsFor(sessionId: string, settings: SessionSettings | null | undefined): ControlsState {
  return { sessionId, settings: settings ?? null, contextLeft: null, plan: [], goal: null };
}

/** Tokens always in the context (system prompt, tools), which Codex leaves out of both sides. */
const BASELINE_TOKENS = 12_000;

/** How much of the context window is left, in percent, as the TUI's (and Codex's) meter reads it. */
export function contextLeft(tokens: number, window: number): number {
  if (window <= BASELINE_TOKENS) return 0;
  const effective = window - BASELINE_TOKENS;
  const used = Math.max(0, tokens - BASELINE_TOKENS);
  const left = Math.max(0, effective - used);
  return Math.round(Math.min(100, Math.max(0, (left / effective) * 100)));
}

/** Apply this session's notifications from `batch`; the same state if none was its. */
export function withControls(state: ControlsState, batch: readonly RpcMessage[]): ControlsState {
  let next: ControlsState | null = null;
  const edit = () => (next ??= { ...state });
  for (const message of batch) {
    const p = message.params as Record<string, unknown> | undefined;
    if (!p || p.sessionId !== state.sessionId) continue;
    switch (message.method) {
      case 'session/settings/updated':
        edit().settings = p.sessionSettings as SessionSettings;
        break;
      case 'session/tokenUsage/updated': {
        const usage = p.tokenUsage as TokenUsage;
        if (usage.modelContextWindow) edit().contextLeft = contextLeft(usage.last.totalTokens, usage.modelContextWindow);
        break;
      }
      case 'turn/plan/updated':
        edit().plan = p.plan as PlanStep[];
        break;
      case 'turn/completed':
        if ((next ?? state).plan.length) edit().plan = [];
        break;
      case 'session/goal/updated':
        edit().goal = p.goal as Goal;
        break;
      case 'session/goal/cleared':
        edit().goal = null;
        break;
    }
  }
  return next ?? state;
}
