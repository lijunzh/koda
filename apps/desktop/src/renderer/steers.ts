// Text added to a running turn (`turn/steer`), kept until the model has it.
// A turn that is stopped or fails drops the steering text it hadn't taken
// yet; the prompt box gets that text back, as the TUI's does.
import type { TurnView } from './transcript';

export interface SentSteer {
  /** The `clientUserMessageId` it went with. */
  id: string;
  turnId: string;
  text: string;
}

export interface Settled {
  /** Still on their way: the turn runs and hasn't taken them. */
  waiting: SentSteer[];
  /** The turn ended without them, oldest first. */
  dropped: string[];
}

/** Sort the steers sent by what `turns` shows became of them. */
export function settle(sent: readonly SentSteer[], turns: readonly TurnView[]): Settled {
  const waiting: SentSteer[] = [];
  const dropped: string[] = [];
  for (const steer of sent) {
    const turn = turns.find((t) => t.id === steer.turnId);
    // A turn this transcript doesn't show (another session's) says nothing yet.
    if (!turn) waiting.push(steer);
    else if (turn.items.some((item) => item.type === 'userMessage' && item.clientUserMessageIds.includes(steer.id))) continue;
    else if (turn.status === 'inProgress') waiting.push(steer);
    else dropped.push(steer.text);
  }
  return { waiting, dropped };
}

/** The prompt box with `dropped` back in front of what is typed in it. */
export function withDropped(dropped: readonly string[], draft: string): string {
  const joined = dropped.join('\n\n');
  return draft.trim() ? `${joined}\n\n${draft}` : joined;
}

/** An id for a steer, the server's to echo. */
export function steerId(turnId: string): string {
  return `${turnId}-steer-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}
