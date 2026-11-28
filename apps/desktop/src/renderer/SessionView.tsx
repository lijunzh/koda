import { useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import type { Selected } from './App';
import { call } from './bridge';
import { Composer } from './Composer';
import type { ControlsState } from './controls';
import { Conversation } from './Conversation';
import type { TurnDiffs } from './diffs';
import { type Entry, MenuButton } from './Menu';
import { PlanPanel } from './Plan';
import { Requests } from './RequestCards';
import type { Pending } from './requests';
import { ListIcon, PanelIcon } from './Icons';
import { ReviewPanel } from './ReviewPanel';
import { SessionControls } from './SessionControls';
import { SessionDetails } from './SessionDetails';
import { Background } from './SessionExtras';
import { type SideState, SidePanel, withPane } from './SidePanel';
import { type ListedSession, statusLabel, title } from './sessions';
import { runningTurn, type TranscriptState, type TurnView, type View, VIEWS } from './transcript';

/** Claude Code's names for its desktop views. */
/** A session not open yet has no turns; one array, so nothing re-renders for it. */
const NO_TURNS: readonly TurnView[] = [];
const VIEW_NAMES: Record<View, string> = { normal: 'Normal', thinking: 'Thinking', verbose: 'Verbose' };

interface Props {
  selected: Selected;
  /** An archived session: its conversation, and Unarchive in place of the prompt box. */
  archived?: boolean;
  listed?: ListedSession;
  transcript: TranscriptState | null;
  view: View;
  onView(view: View): void;
  /** What the server waits on the user for in this session. */
  requests: Pending[];
  onAnswer(id: number | string, result: unknown): Promise<void>;
  controls: ControlsState | null;
  /** The turns' diffs the server sent while they ran. */
  diffs: TurnDiffs;
  planShown: boolean;
  /** Whether the session details panel shows, in every pane. */
  detailsShown: boolean;
  onDetails(shown: boolean): void;
  /** Open a session the server just made (a fork, or one on another provider). */
  onOpen(session: Session): void;
  /** Close the pane, when there are others. */
  onClose?(): void;
}

/**
 * The open session, laid out as Claude Code's desktop app lays one out: its
 * title with a menu of what can be done with it; the conversation and the
 * prompt box; and, at the right, the Changes pane and the details panel.
 */
export function SessionView({
  selected,
  archived,
  listed,
  transcript,
  view,
  onView,
  requests,
  onAnswer,
  controls,
  diffs,
  planShown,
  detailsShown,
  onDetails,
  onOpen,
  onClose,
}: Props) {
  const [renaming, setRenaming] = useState<string | null>(null);
  // The side panel: whether it shows, and the panes open in it.
  const [sideShown, setSideShown] = useState(false);
  const [side, setSide] = useState<SideState>({ tabs: [], active: null });
  const view$ = useRef<HTMLElement>(null);
  // Cmd+. hides or shows the side panel and Shift+Cmd+D opens Changes in
  // it, as in Claude Code's desktop app; in split view, for the focused pane.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const pane = view$.current?.closest('.pane');
      if (pane && !pane.classList.contains('focused') && document.querySelectorAll('.pane').length > 1) return;
      if (e.metaKey && !e.shiftKey && !e.altKey && !e.ctrlKey && e.key === '.') {
        e.preventDefault();
        setSideShown((shown) => !shown);
      }
      if (e.metaKey && e.shiftKey && !e.altKey && !e.ctrlKey && e.key.toLowerCase() === 'd') {
        e.preventDefault();
        setSide((state) => withPane(state, 'changes'));
        setSideShown(true);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
  const showChanges = () => {
    setSide((state) => withPane(state, 'changes'));
    setSideShown(true);
  };
  const [error, setError] = useState<string | null>(null);
  const session = selected.session;
  const firstPrompt = transcript?.turns.flatMap((turn) => turn.items).find((item) => item.type === 'userMessage');
  const prompted =
    firstPrompt?.type === 'userMessage' ? (firstPrompt.content.find((p) => p.type === 'text') as { text?: string })?.text : '';
  const name = title({
    name: listed?.name ?? session?.name,
    preview: listed?.preview || session?.preview || prompted || '',
  });
  const folder = session?.cwd ?? listed?.cwd ?? null;
  const label = archived ? 'archived' : statusLabel(listed?.status ?? session?.status ?? null);
  const running = transcript ? runningTurn(transcript) : undefined;
  const failed = (e: Error) => setError(e.message);
  const unarchive = () => void call('session/unarchive', { sessionId: selected.id }).catch(failed);

  // What can be done with the session, behind the caret beside its title.
  const menu: Entry[] = [
    { type: 'item', id: 'rename', label: 'Rename', onPick: () => setRenaming(listed?.name ?? session?.name ?? '') },
    {
      type: 'item',
      id: 'fork',
      label: 'Fork',
      description: 'A copy to go on in, leaving this one as it is',
      onPick: () => void call('session/fork', { sessionId: selected.id }).then((answer) => onOpen(answer.session), failed),
    },
    { type: 'separator' },
    {
      type: 'submenu',
      id: 'view',
      label: 'Transcript view',
      detail: VIEW_NAMES[view],
      entries: VIEWS.map((v): Entry => ({ type: 'item', id: v, label: VIEW_NAMES[v], checked: v === view, onPick: () => onView(v) })),
    },
    {
      type: 'item',
      id: 'compact',
      label: 'Compact',
      description: 'Summarize the conversation so far, to free context',
      disabled: archived,
      onPick: () => void call('session/compact/start', { sessionId: selected.id }).catch(failed),
    },
    { type: 'separator' },
    archived
      ? { type: 'item', id: 'unarchive', label: 'Unarchive', onPick: unarchive }
      : {
          type: 'item',
          id: 'archive',
          label: 'Archive',
          description: 'Put it away; a running turn stops',
          onPick: () => void call('session/archive', { sessionId: selected.id }).catch(failed),
        },
    { type: 'item', id: 'delete', label: 'Delete', danger: true, onPick: () => void window.koda.deleteSession(selected.id) },
  ];

  return (
    <main className="session-view" data-testid="session-view" data-view={view} ref={view$}>
      <header>
        <div className="heading">
          <div className="title-row">
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
            <MenuButton label="Session menu" entries={menu} className="icon-button">
              <span aria-hidden>⌄</span>
            </MenuButton>
          </div>
          <p className="subtitle">
            {folder && <code data-testid="session-folder">{folder}</code>}
            {label && <span data-testid="session-status"> · {label}</span>}
          </p>
        </div>
        <button
          className="icon-button"
          aria-label="Activity panel"
          title="Activity panel"
          aria-pressed={detailsShown}
          onClick={() => onDetails(!detailsShown)}
        >
          <ListIcon />
        </button>
        <button
          className="icon-button"
          aria-label="Side panel"
          title={sideShown ? 'Hide side panel (⌘.)' : 'Show side panel (⌘.)'}
          aria-pressed={sideShown}
          disabled={!transcript}
          onClick={() => setSideShown((shown) => !shown)}
        >
          <PanelIcon />
        </button>
        {onClose && (
          <button className="close-pane" aria-label="Close pane" onClick={onClose}>
            ✕
          </button>
        )}
      </header>
      {(selected.error || error) && <p className="problem">{selected.error ?? error}</p>}
      {!session && !selected.error && <p className="detail">Opening…</p>}
      <div className="session-split">
        <div className="conversation-column">
          {transcript && <Conversation transcript={transcript} view={view} />}
          {archived ? (
            <div className="bottom">
              <div className="archived-bar" data-testid="archived-bar">
                <span>This session is archived.</span>
                <button onClick={unarchive}>Unarchive</button>
              </div>
            </div>
          ) : (
          <div className="bottom">
            {controls && <PlanPanel controls={controls} shown={planShown} />}
            <Requests requests={requests} onAnswer={onAnswer} />
            <Composer
              sessionId={selected.id}
              running={running}
              turns={transcript?.turns ?? NO_TURNS}
              ready={Boolean(session)}
              failed={Boolean(selected.error) && !session}
              waiting={requests[0]?.method ?? null}
              foot={
                controls && (
                  <SessionControls
                    sessionId={selected.id}
                    controls={controls}
                    folder={folder}
                    onOpen={onOpen}
                    onError={setError}
                  />
                )
              }
            />
          </div>
          )}
        </div>
        {sideShown && transcript && (
          <SidePanel
            state={side}
            onState={setSide}
            render={(kind) =>
              kind === 'changes' ? (
                <ReviewPanel key={selected.id} sessionId={selected.id} cwd={folder} transcript={transcript} diffs={diffs} />
              ) : (
                <Background sessionId={selected.id} pane />
              )
            }
          />
        )}
        {detailsShown && controls && (
          <SessionDetails
            sessionId={selected.id}
            folder={folder}
            controls={controls}
            transcript={transcript}
            diffs={diffs}
            onChanges={showChanges}
            archived={archived}
          />
        )}
      </div>
    </main>
  );
}
