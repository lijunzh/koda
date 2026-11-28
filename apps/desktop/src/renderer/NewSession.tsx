// A new session, before it exists: the prompt box, and under it the folder,
// the model and the mode it will start with, as Claude Code's desktop app
// opens "New session". The session starts with the first prompt. The folder
// is one that already has a session or one picked in the system's dialog;
// the main process checks that, so the page never sends a path of its own.
import { useEffect, useRef, useState } from 'react';
import type { Session } from '../protocol/types.gen';
import { call } from './bridge';
import { FolderIcon } from './Icons';
import { type Entry, MenuButton } from './Menu';
import { type Mode, ModeMenu, ModelMenu } from './SessionControls';
import { byFolder, folderName, type SessionsState } from './sessions';

interface Props {
  sessions: SessionsState;
  /** The session is started: open it, and send `prompt` as its first turn. */
  onStarted(session: Session, prompt: string): void;
  /** Close the pane, when there are others. */
  onClose?(): void;
}

/** What the server starts a session with, and what the user changed of it. */
interface Choice {
  provider: string;
  model: string;
  effort: string | null;
  mode: Mode;
}

export function NewSession({ sessions, onStarted, onClose }: Props) {
  const folders = byFolder(sessions)
    .map((group) => group.folder)
    .filter((folder): folder is string => Boolean(folder));
  // The folder worked in last leads, until the user picks one.
  const [picked, setPicked] = useState<string | null>(null);
  const folder = picked ?? folders[0] ?? null;
  const [defaults, setDefaults] = useState<Choice | null>(null);
  const [choice, setChoice] = useState<Choice | null>(null);
  const [text, setText] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const box = useRef<HTMLTextAreaElement>(null);

  useEffect(() => box.current?.focus(), []);
  useEffect(() => {
    let live = true;
    void call('state/read', {}).then(
      (state) => {
        if (!live) return;
        const start: Choice = {
          provider: state.modelProvider,
          model: state.model,
          effort: null,
          mode: state.mode === 'plan' ? 'plan' : 'auto',
        };
        setDefaults(start);
        setChoice((now) => now ?? start);
      },
      () => {},
    );
    return () => {
      live = false;
    };
  }, []);

  const folderEntries: Entry[] = [
    ...folders.map(
      (f): Entry => ({ type: 'item', id: f, label: folderName(f), detail: f, checked: f === folder, onPick: () => setPicked(f) }),
    ),
    ...(folders.length ? [{ type: 'separator' } as Entry] : []),
    {
      type: 'item',
      id: 'open',
      label: 'Open folder…',
      onPick: () => void window.koda.pickFolder().then((f) => f && setPicked(f)),
    },
  ];

  const send = async () => {
    const prompt = text.trim();
    if (!prompt || starting) return;
    setStarting(true);
    setError(null);
    try {
      const where = folder ?? (await window.koda.pickFolder());
      if (!where) return;
      setPicked(where);
      const changed = choice && defaults && (choice.provider !== defaults.provider || choice.model !== defaults.model);
      const session = await window.koda.startSession({
        folder: where,
        ...(changed ? { provider: choice.provider, model: choice.model } : {}),
      });
      // The effort and the mode aren't part of starting a session: set them
      // before its first turn. Auto asks in the system's dialog, as ever.
      if (choice?.effort) await call('session/settings/update', { sessionId: session.id, effort: choice.effort });
      const mode = session.settings?.mode === 'plan' ? 'plan' : 'auto';
      if (choice && choice.mode !== mode) await window.koda.setMode(session.id, choice.mode);
      onStarted(session, prompt);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setStarting(false);
    }
  };

  return (
    <main className="session-view new-session-view" data-testid="new-session">
      <header>
        <div className="heading">
          <h1>New session</h1>
        </div>
        {onClose && (
          <button className="close-pane" aria-label="Close pane" onClick={onClose}>
            ✕
          </button>
        )}
      </header>
      <div className="new-session-body">
        <p className="meta">Describe a task or ask a question. Koda works in the folder below.</p>
      </div>
      <div className="bottom">
        {error && <p className="problem">{error}</p>}
        <div className="composer">
          <div className="composer-row">
            <textarea
              ref={box}
              value={text}
              rows={3}
              placeholder="Describe a task or ask a question"
              aria-label="Prompt"
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                // Enter that confirms an input method's candidate doesn't send.
                if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing && e.keyCode !== 229) {
                  e.preventDefault();
                  void send();
                }
              }}
            />
            <button className="send" aria-label="Send" onClick={() => void send()} disabled={!text.trim() || starting}>
              ↑
            </button>
          </div>
          <div className="composer-foot">
            <div className="foot-left">
              <MenuButton label="Folder" entries={folderEntries} up>
                <FolderIcon />
                <span data-testid="new-session-folder">{folder ? folderName(folder) : 'Choose a folder'}</span>
                <span className="caret" aria-hidden>
                  ⌄
                </span>
              </MenuButton>
            </div>
            <div className="foot-right">
              {choice && (
                <>
                  <ModelMenu
                    provider={choice.provider}
                    model={choice.model}
                    effort={choice.effort}
                    onModel={(model) => setChoice({ ...choice, model, effort: null })}
                    onEffort={(effort) => setChoice({ ...choice, effort })}
                    onOther={(provider, model) => setChoice({ ...choice, provider, model, effort: null })}
                  />
                  <ModeMenu mode={choice.mode} onMode={(mode) => setChoice({ ...choice, mode })} />
                </>
              )}
            </div>
          </div>
        </div>
      </div>
    </main>
  );
}
