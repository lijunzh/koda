// One open session, in a pane of its own: its conversation, controls and
// changes, kept from the server's events. Several panes show several
// sessions side by side, as Claude Code's split view does; each joins its
// session on the server when it opens and leaves it when it closes.
import { useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import type { Selected } from './App';
import { call, type RpcMessage } from './bridge';
import { type ControlsState, controlsFor, withControls } from './controls';
import { type TurnDiffs, withDiffs } from './diffs';
import type { Pending } from './requests';
import type { ListedSession } from './sessions';
import { SessionView } from './SessionView';
import { fromSession, type TranscriptState, type View, withEvents } from './transcript';

export type Listener = (batch: readonly RpcMessage[]) => void;

interface Props {
  id: string;
  /** The session, when the server just made it (a fork). */
  initial?: Session;
  /** A new session's first prompt, sent once the pane follows the session. */
  firstPrompt?: string;
  /** Hear the server's events; returns the way to stop. */
  subscribe(listener: Listener): () => void;
  listed?: ListedSession;
  requests: Pending[];
  onAnswer(id: number | string, result: unknown): Promise<void>;
  view: View;
  onView(view: View): void;
  planShown: boolean;
  /** Open a session the server just made (a fork) in this pane. */
  onOpen(session: Session): void;
  /** Close this pane; absent when it's the only one. */
  onClose?(): void;
}

export function Pane({ id, initial, firstPrompt, subscribe, listed, requests, onAnswer, view, onView, planShown, onOpen, onClose }: Props) {
  const [selected, setSelected] = useState<Selected>({ id });
  const [transcript, setTranscript] = useState<TranscriptState | null>(null);
  const [controls, setControls] = useState<ControlsState | null>(null);
  /** The turns' diffs, as the server sends them while they run. */
  const [diffs, setDiffs] = useState<TurnDiffs>(new Map());
  /** Events from before the session's snapshot came, kept to apply after it. */
  const early = useRef<RpcMessage[]>([]);

  useEffect(
    () =>
      subscribe((batch) => {
        setControls((state) => (state ? withControls(state, batch) : state));
        setDiffs((state) => withDiffs(state, id, batch));
        setTranscript((state) => {
          if (state) return withEvents(state, batch);
          // The snapshot hasn't come yet: keep the events for it.
          early.current.push(...batch);
          return state;
        });
      }),
    [id, subscribe],
  );

  // Opening joins the session on the server; closing the pane leaves it (a
  // turn running in it goes on, as when a window closes).
  useEffect(() => {
    let open = true;
    const opened = (session: Session) => {
      if (!open) return;
      setSelected({ id, session });
      setControls(controlsFor(id, session.settings));
      void call('session/goal/get', { sessionId: id }).then((answer) => {
        if (open && answer.goal) setControls((c) => (c ? { ...c, goal: answer.goal ?? null } : c));
      }, () => {});
      // The snapshot, then what came while it was on its way: the server
      // answers at one point in the session's stream, so nothing repeats.
      setTranscript(withEvents(fromSession(session), early.current));
      early.current = [];
      // Only now, so the pane sees the turn from its start.
      if (firstPrompt) {
        call('turn/start', { sessionId: id, input: [{ type: 'text', text: firstPrompt }] }).catch(
          (error: Error) => open && setSelected({ id, session, error: error.message }),
        );
      }
    };
    if (initial) opened(initial);
    else {
      call('session/resume', { sessionId: id }).then(
        (answer) => opened(answer.session),
        (error: Error) => open && setSelected({ id, error: error.message }),
      );
    }
    return () => {
      open = false;
      void call('session/unsubscribe', { sessionId: id }).catch(() => {});
    };
    // `initial` and `firstPrompt` are only the session's first state.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  return (
    <SessionView
      selected={selected}
      listed={listed}
      transcript={transcript}
      view={view}
      onView={onView}
      requests={requests}
      controls={controls}
      diffs={diffs}
      planShown={planShown}
      onOpen={onOpen}
      onAnswer={onAnswer}
      onClose={onClose}
    />
  );
}
