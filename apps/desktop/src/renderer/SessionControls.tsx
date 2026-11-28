// The controls under the prompt box, as Claude Code's desktop app has them:
// the model with its effort, and the mode. The model menu lists the
// session's provider; other providers' models are under "More models", and
// picking one opens a new session on it, since a session keeps the provider
// it started with (as Codex's threads do, and as the TUI's /provider goes).
import { useEffect, useState } from 'react';
import type { Model, ModelProviderSummary, Session } from '../protocol/types.gen';
import { call } from './bridge';
import type { ControlsState } from './controls';
import { type Entry, MenuButton } from './Menu';

export type Mode = 'auto' | 'plan';

const MODES: Array<{ mode: Mode; label: string; description: string }> = [
  { mode: 'auto', label: 'Auto', description: 'Koda edits and runs sandboxed commands without asking' },
  { mode: 'plan', label: 'Plan', description: 'Read only: Koda plans, and changes nothing' },
];

export function ModeMenu({ mode, onMode }: { mode: Mode; onMode(mode: Mode): void }) {
  const entries: Entry[] = [
    { type: 'heading', label: 'Mode' },
    ...MODES.map(
      (m, i): Entry => ({
        type: 'item',
        id: m.mode,
        label: m.label,
        description: m.description,
        hint: String(i + 1),
        checked: m.mode === mode,
        onPick: () => m.mode !== mode && onMode(m.mode),
      }),
    ),
  ];
  return (
    <MenuButton label="Mode" entries={entries} up right hotkey="m">
      {mode === 'plan' ? 'Plan' : 'Auto'}
    </MenuButton>
  );
}

interface ModelProps {
  /** The provider the models are of. */
  provider: string;
  /** The model in use: its id, or the alias it was picked by. */
  model: string;
  alias?: string | null;
  effort?: string | null;
  onModel(model: string): void;
  onEffort(effort: string): void;
  /** A model of another provider was picked. */
  onOther(provider: string, model: string): void;
}

/** The models a provider offers; none if it can't be asked (a local server that isn't running). */
function modelsOf(provider: string): Promise<Model[]> {
  return call('model/list', { modelProvider: provider }).then(
    (answer) => answer.data,
    () => [],
  );
}

export function ModelMenu({ provider, model, alias, effort, onModel, onEffort, onOther }: ModelProps) {
  const [models, setModels] = useState<Model[]>([]);
  useEffect(() => {
    let live = true;
    void modelsOf(provider).then((list) => live && setModels(list));
    return () => {
      live = false;
    };
  }, [provider]);
  const current = models.find((m) => m.id === model || (alias && m.alias === alias) || m.alias === model);
  // The model in use leads the list even when the provider doesn't list it.
  const choices = current || !model ? models : [{ id: model } as Model, ...models];
  const efforts = current?.supportedReasoningEfforts ?? [];
  const shownEffort = effort ?? current?.defaultReasoningEffort ?? null;
  const entries: Entry[] = choices.map(
    (m, i): Entry => ({
      type: 'item',
      id: m.alias ?? m.id,
      label: m.alias ?? m.id,
      detail: m.alias ? m.id : undefined,
      hint: i < 9 ? String(i + 1) : undefined,
      checked: m === current || (!current && m.id === model),
      onPick: () => m !== current && onModel(m.alias ?? m.id),
    }),
  );
  if (efforts.length) {
    entries.push({ type: 'separator' });
    entries.push({
      type: 'submenu',
      id: 'effort',
      label: 'Effort',
      detail: shownEffort ?? undefined,
      entries: efforts.map(
        (e): Entry => ({ type: 'item', id: e, label: e, checked: e === shownEffort, onPick: () => e !== shownEffort && onEffort(e) }),
      ),
    });
  }
  entries.push({ type: 'separator' });
  entries.push({
    type: 'submenu',
    id: 'more',
    label: 'More models',
    // The other providers that can be used now: a key set, or none needed.
    entries: async () => {
      const providers = (await call('modelProvider/list', {})).data as ModelProviderSummary[];
      const usable = providers.filter((p) => p.id !== provider && (p.hasApiKey || !p.requiresApiKey));
      const lists = await Promise.all(usable.map(async (p) => ({ p, models: await modelsOf(p.id) })));
      return lists
        .filter((l) => l.models.length)
        .flatMap((l): Entry[] => [
          { type: 'heading', label: l.p.name },
          ...l.models.map(
            (m): Entry => ({
              type: 'item',
              id: `${l.p.id}/${m.alias ?? m.id}`,
              label: m.alias ?? m.id,
              detail: m.alias ? m.id : undefined,
              onPick: () => onOther(l.p.id, m.alias ?? m.id),
            }),
          ),
        ]);
    },
  });
  return (
    <MenuButton label="Model" entries={entries} up right hotkey="i">
      {alias ?? current?.alias ?? model}
      {shownEffort && <span className="meta"> {shownEffort}</span>}
    </MenuButton>
  );
}

interface Props {
  sessionId: string;
  controls: ControlsState;
  /** The session's folder, where a session on another provider starts too. */
  folder: string | null;
  /** Open the session started on another provider. */
  onOpen(session: Session): void;
  onError(message: string | null): void;
}

/** The open session's model, effort and mode, under its prompt box. */
export function SessionControls({ sessionId, controls, folder, onOpen, onError }: Props) {
  const settings = controls.settings;
  if (!settings) return null;
  const update = (change: { model?: string; effort?: string }) =>
    call('session/settings/update', { sessionId, ...change }).then(
      () => onError(null),
      (e: Error) => onError(e.message),
    );
  return (
    <>
      <ModelMenu
        provider={settings.modelProvider}
        model={settings.model}
        alias={settings.alias}
        effort={settings.effort}
        onModel={(model) => void update({ model })}
        onEffort={(effort) => void update({ effort })}
        onOther={(provider, model) => {
          if (!folder) return;
          void window.koda.startSession({ folder, provider, model }).then(onOpen, (e: Error) => onError(e.message));
        }}
      />
      <ModeMenu
        mode={settings.mode === 'plan' ? 'plan' : 'auto'}
        onMode={(mode) => void window.koda.setMode(sessionId, mode).catch((e: Error) => onError(e.message))}
      />
    </>
  );
}
