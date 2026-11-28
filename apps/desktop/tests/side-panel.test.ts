// The side panel's panes: opened once, and the tab before takes a closed one's place.
import { describe, expect, it } from 'vitest';
import { type SideState, withoutPane, withPane } from '../src/renderer/SidePanel';

describe('side panel', () => {
  const empty: SideState = { tabs: [], active: null };

  it('opens a pane once, and goes to it when it is open', () => {
    const one = withPane(empty, 'changes');
    expect(one).toEqual({ tabs: ['changes'], active: 'changes' });
    const two = withPane(one, 'background');
    expect(withPane(two, 'changes')).toEqual({ tabs: ['changes', 'background'], active: 'changes' });
  });

  it('closes a pane, leaving the one before it in front, or the list of panes', () => {
    const two = withPane(withPane(empty, 'changes'), 'background');
    expect(withoutPane(two, 'background')).toEqual({ tabs: ['changes'], active: 'changes' });
    expect(withoutPane(withoutPane(two, 'background'), 'changes')).toEqual(empty);
    // Closing a tab that isn't in front leaves the front one.
    expect(withoutPane(two, 'changes')).toEqual({ tabs: ['background'], active: 'background' });
  });
});
