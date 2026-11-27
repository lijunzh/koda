// The open session's controls: settings, the context meter and the plan.
import { describe, expect, it } from 'vitest';
import { refusal } from '../src/main/relay';
import { contextLeft, controlsFor, withControls } from '../src/renderer/controls';

const note = (method: string, params: object) => ({ jsonrpc: '2.0' as const, method, params: { sessionId: 's1', ...params } });

describe('controls', () => {
  it('reads the context meter as the TUI and Codex do, leaving out the baseline', () => {
    expect(contextLeft(12_000, 100_000)).toBe(100);
    expect(contextLeft(56_000, 100_000)).toBe(50);
    expect(contextLeft(200_000, 100_000)).toBe(0);
    expect(contextLeft(5_000, 10_000)).toBe(0);
  });

  it('follows the settings, the meter and the plan, and drops the plan when the turn ends', () => {
    let state = controlsFor('s1', null);
    const settings = { model: 'm', modelProvider: 'p', baseUrl: 'u', contextWindow: 100_000, mode: 'plan' };
    state = withControls(state, [
      note('session/settings/updated', { sessionSettings: settings }),
      note('session/tokenUsage/updated', {
        turnId: 't',
        tokenUsage: { last: { totalTokens: 56_000 }, total: {}, modelContextWindow: 100_000 },
      }),
      note('turn/plan/updated', { turnId: 't', plan: [{ step: 'Read', status: 'completed' }, { step: 'Fix', status: 'inProgress' }] }),
    ]);
    expect(state.settings).toEqual(settings);
    expect(state.contextLeft).toBe(50);
    expect(state.plan.map((s) => s.step)).toEqual(['Read', 'Fix']);
    state = withControls(state, [note('turn/completed', { turn: {} })]);
    expect(state.plan).toEqual([]);
    const other = { jsonrpc: '2.0' as const, method: 'turn/plan/updated', params: { sessionId: 'x', plan: [] } };
    expect(withControls(state, [other])).toBe(state);
  });

  it("lets the page change model and effort, and lower the mode, but not raise it", () => {
    expect(refusal('session/settings/update', { sessionId: 's', model: 'claude-opus', effort: 'high' })).toBeNull();
    expect(refusal('session/settings/update', { sessionId: 's', mode: 'plan' })).toBeNull();
    expect(refusal('session/settings/update', { sessionId: 's', mode: 'auto' })).toMatch(/Auto is confirmed/);
    expect(refusal('session/settings/update', { sessionId: 's', baseUrl: 'https://x' })).toMatch(/baseUrl/);
    expect(refusal('session/settings/update', {})).toMatch(/sessionId/);
  });
});
