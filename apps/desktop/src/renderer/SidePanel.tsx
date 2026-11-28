// The side panel, beside the conversation, as Claude Code's desktop app has
// it: the panes a session has open, as tabs, with a "+" for another. With
// none open it lists what can be opened. Koda's panes are Changes and
// Background tasks.
import type { ReactNode } from 'react';
import { ChangesIcon, CloseIcon, PlusIcon, TasksIcon } from './Icons';
import { type Entry, MenuButton } from './Menu';

export type PaneKind = 'changes' | 'background';

export const PANES: Array<{ kind: PaneKind; label: string; keys: string; icon: ReactNode }> = [
  { kind: 'changes', label: 'Changes', keys: '⇧⌘D', icon: <ChangesIcon /> },
  { kind: 'background', label: 'Background tasks', keys: '', icon: <TasksIcon /> },
];

export interface SideState {
  /** The panes open, in the order they were opened. */
  tabs: PaneKind[];
  active: PaneKind | null;
}

/** Open `kind`, or go to it if it is open. */
export function withPane(state: SideState, kind: PaneKind): SideState {
  return { tabs: state.tabs.includes(kind) ? state.tabs : [...state.tabs, kind], active: kind };
}

/** Close `kind`; the tab before it takes its place in front. */
export function withoutPane(state: SideState, kind: PaneKind): SideState {
  const tabs = state.tabs.filter((t) => t !== kind);
  return { tabs, active: state.active === kind ? (tabs.at(-1) ?? null) : state.active };
}

interface Props {
  state: SideState;
  onState(state: SideState): void;
  /** What a pane shows. */
  render(kind: PaneKind): ReactNode;
}

export function SidePanel({ state, onState, render }: Props) {
  const closed = PANES.filter((p) => !state.tabs.includes(p.kind));
  const add: Entry[] = closed.map(
    (p): Entry => ({ type: 'item', id: p.kind, label: p.label, hint: p.keys || undefined, onPick: () => onState(withPane(state, p.kind)) }),
  );
  return (
    <aside className="side-panel" data-testid="side-panel" aria-label="Side panel">
      <div className="side-tabs" role="tablist">
        {state.tabs.map((kind) => {
          const pane = PANES.find((p) => p.kind === kind)!;
          return (
            <span key={kind} className={kind === state.active ? 'side-tab active' : 'side-tab'}>
              <button role="tab" aria-selected={kind === state.active} onClick={() => onState({ ...state, active: kind })}>
                {pane.icon}
                {pane.label}
              </button>
              <button className="tab-close" aria-label={`Close ${pane.label}`} onClick={() => onState(withoutPane(state, kind))}>
                <CloseIcon />
              </button>
            </span>
          );
        })}
        {closed.length > 0 && state.tabs.length > 0 && (
          <MenuButton label="Open a pane" entries={add} className="icon-button">
            <PlusIcon />
          </MenuButton>
        )}
      </div>
      {state.active ? (
        <div className="side-body" role="tabpanel">
          {render(state.active)}
        </div>
      ) : (
        // Nothing open: what can be, as Claude Code lists its panes.
        <div className="side-empty">
          <div className="side-menu">
            {PANES.map((p) => (
              <button key={p.kind} onClick={() => onState(withPane(state, p.kind))}>
                {p.icon}
                <span className="menu-label">{p.label}</span>
                <span className="menu-hint">{p.keys}</span>
              </button>
            ))}
          </div>
        </div>
      )}
    </aside>
  );
}
