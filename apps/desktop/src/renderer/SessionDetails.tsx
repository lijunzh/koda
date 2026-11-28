// The session details panel, at the right of the conversation, as Claude
// Code's desktop app has it: cards for where the session works, how full its
// context is, what it changed, what runs in the background, its goal and its
// MCP servers. It replaces the bar that sat above the conversation.
import { useEffect, useMemo, useState } from 'react';
import type { McpServerStatus } from '../protocol/types.gen';
import { call } from './bridge';
import type { ControlsState } from './controls';
import { parseDiff, type TurnDiffs, turnDiff } from './diffs';
import { FolderIcon } from './Icons';
import { Background, GoalLine } from './SessionExtras';
import { folderName } from './sessions';
import type { TranscriptState } from './transcript';

interface Props {
  sessionId: string;
  folder: string | null;
  controls: ControlsState;
  transcript: TranscriptState | null;
  diffs: TurnDiffs;
  /** Open the Changes pane. */
  onChanges(): void;
  /** An archived session isn't open: nothing here acts on it. */
  archived?: boolean;
}

/** 12345 as "12.3K", as Claude Code's Context card counts tokens. */
export function tokenCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(n >= 100_000 ? 0 : 1)}K`;
  return String(n);
}

export function SessionDetails({ sessionId, folder, controls, transcript, diffs, onChanges, archived }: Props) {
  const settings = controls.settings;
  const [error, setError] = useState<string | null>(null);
  const [servers, setServers] = useState<McpServerStatus[]>([]);
  useEffect(() => {
    let live = true;
    call('mcpServerStatus/list', {}).then(
      (answer) => live && setServers(answer.data ?? []),
      () => {},
    );
    return () => {
      live = false;
    };
  }, [sessionId]);

  // The files of the latest turn that changed something.
  const changed = useMemo(() => {
    const turns = transcript?.turns ?? [];
    for (let i = turns.length - 1; i >= 0; i--) {
      const diff = turnDiff(turns[i], diffs, folder);
      if (diff.trim()) return parseDiff(diff);
    }
    return [];
  }, [transcript, diffs, folder]);

  const used = controls.contextLeft === null ? null : 100 - controls.contextLeft;
  return (
    <aside className="details" data-testid="details" aria-label="Activity panel">
      <section className="card">
        <h2>Environment</h2>
        {folder && (
          <p className="card-row" title={folder}>
            <FolderIcon />
            <span className="path">
              {folderName(folder)}
              <span className="meta"> · {folder.slice(0, Math.max(1, folder.lastIndexOf('/')))}</span>
            </span>
          </p>
        )}
        {settings && (
          <p className="card-row meta">
            {settings.modelProvider} · {settings.alias ?? settings.model}
          </p>
        )}
      </section>

      <section className="card">
        <h2>Context</h2>
        {used === null ? (
          <p className="meta">Known after the first reply.</p>
        ) : (
          <>
            <p className="card-row between">
              <span data-testid="context-left">{used}% used</span>
              {controls.tokens !== null && controls.window !== null && (
                <span className="meta">
                  {tokenCount(controls.tokens)} / {tokenCount(controls.window)}
                </span>
              )}
            </p>
            <div className="meter" role="img" aria-label={`${used}% of the context window used`}>
              <span style={{ width: `${used}%` }} />
            </div>
          </>
        )}
        {!archived && (
          <p className="card-row end">
            <button
              title="Summarize the conversation so far, to free context"
              onClick={() => void call('session/compact/start', { sessionId }).then(() => setError(null), (e: Error) => setError(e.message))}
            >
              Compact session
            </button>
          </p>
        )}
        {error && <p className="problem">{error}</p>}
      </section>

      <section className="card">
        <h2>Changes</h2>
        {changed.length === 0 ? (
          <p className="meta">No file changes yet.</p>
        ) : (
          <ul className="card-list">
            {changed.map((file) => (
              <li key={file.path}>
                <button className="card-file" onClick={onChanges} title="Open in Changes">
                  <code>{file.path}</code>
                  <span>
                    <span className="diff-add">+{file.added}</span> <span className="diff-del">−{file.removed}</span>
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {!archived && (
        <section className="card">
          <h2>Goal</h2>
          <GoalLine sessionId={sessionId} goal={controls.goal} />
        </section>
      )}

      {!archived && (
        <section className="card background-card">
          <h2>Background</h2>
          <Background sessionId={sessionId} />
        </section>
      )}

      {servers.length > 0 && (
        <section className="card">
          <h2>MCP servers</h2>
          <ul className="card-list">
            {servers.map((server) => (
              <li key={server.name} className="card-row between">
                <span>{server.name}</span>
                <span className="meta">{typeof server.runtimeStatus === 'string' ? server.runtimeStatus : ''}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </aside>
  );
}
