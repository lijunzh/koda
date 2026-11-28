// A session's goal, its background commands and forking it, driven as a
// user would against a real `koda serve` and a scripted model.
import { mkdirSync, realpathSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { scratch } from '../helpers';
import { koda, type OpenApp, openApp, shutDown } from './app';
import { gate, type Model, type Reply, scriptedModel, useModel } from './model';

let open: OpenApp | undefined;
let model: Model | undefined;
const homes: string[] = [];

afterEach(async () => {
  await shutDown(open, homes.splice(0));
  open = undefined;
  model?.close();
  model = undefined;
});

async function session(replies: Reply[]) {
  const home = scratch();
  homes.push(home);
  model = await scriptedModel(replies);
  useModel(home, model);
  const project = path.join(home, 'project');
  mkdirSync(project);
  open = await openApp(home);
  const { app, page } = open;
  await page.getByRole('button', { name: 'New session' }).waitFor({ timeout: 30_000 });
  await app.evaluate(({ dialog }, picked) => {
    dialog.showOpenDialog = (async () => ({ canceled: false, filePaths: [picked] })) as typeof dialog.showOpenDialog;
  }, realpathSync(project));
  await page.getByRole('button', { name: 'New session' }).click();
  await page.getByRole('textbox', { name: 'Prompt' }).waitFor({ timeout: 30_000 });
  return page;
}

describe.skipIf(!koda)('session extras', () => {
  it('lists a background command and stops it', async () => {
    const page = await session([{ tool: 'Bash', args: { command: 'sleep 120', background: true } }, { text: 'Started it.' }]);
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill('Start the watcher');
    await box.press('Enter');
    const background = page.getByTestId('background');
    await background.waitFor({ timeout: 30_000 });
    await background.getByRole('button', { name: '1 in the background' }).click();
    expect(await background.textContent()).toContain('sleep 120');
    await background.getByRole('button', { name: 'Stop' }).click();
    await expect.poll(() => page.getByTestId('background').count(), { timeout: 30_000 }).toBe(0);
  });

  it('sets a goal, pauses and clears it', async () => {
    // The first turn Koda starts on the goal waits at its model call until the
    // test is done clicking: no turn ends, and redraws the goal, meanwhile.
    const goalTurn = gate();
    const page = await session([{ text: 'All done.' }, { text: 'All done.', after: goalTurn }]);
    // What the app hears about the goal and the turns, and when the test
    // clicked, for the failure message: this test failed once on CI (#1859)
    // and in none of 115 runs after, so a recurrence has to say why itself.
    const t0 = Date.now();
    await open!.app.evaluate((_e, t0) => {
      type Message = { method?: string; params?: unknown };
      const g = globalThis as unknown as {
        goalLog: string[];
        kodaTestConnection: { on(event: string, f: (batch: Message[]) => void): void };
      };
      g.goalLog = [];
      g.kodaTestConnection.on('messages', (batch) => {
        for (const m of batch) {
          if (!m.method || /delta|tokenUsage|item\//.test(m.method)) continue;
          const p = m.params as { goal?: { status?: string; objective?: string } } | undefined;
          g.goalLog.push(`${Date.now() - t0}ms ${m.method} ${p?.goal ? `${p.goal.status} "${p.goal.objective}"` : ''}`);
        }
      });
    }, t0);
    const mark = (what: string) => open!.app.evaluate((_e, [what, t0]) => (globalThis as unknown as { goalLog: string[] }).goalLog.push(`${Date.now() - (t0 as number)}ms TEST ${what}`), [what, t0] as const);
    // A session starts with its first prompt.
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill('Hello');
    await box.press('Enter');
    await page.getByTestId('agent-message').filter({ hasText: 'All done.' }).first().waitFor({ timeout: 30_000 });
    // The first turn has ended (Send is back in place of Stop): the goal is set
    // on an idle session, and no turn of the user's is counted toward it later.
    const send = page.getByRole('button', { name: 'Send', exact: true });
    await send.waitFor({ timeout: 30_000 });
    await page.getByRole('button', { name: 'Set a goal' }).click();
    const goal = page.getByTestId('goal');
    // The form the goal is typed in is not the goal: nothing is set yet.
    const form = page.getByTestId('goal-form');
    await form.getByRole('textbox', { name: 'Goal' }).fill('Keep the tests green');
    expect(await goal.count()).toBe(0);
    await form.getByRole('button', { name: 'Set goal' }).click();
    await mark('set clicked');
    // The goal's line is there once the server has set the goal and said so.
    await expect.poll(() => goal.textContent(), { timeout: 30_000 }).toContain('Keep the tests green');
    const pause = goal.getByRole('button', { name: 'Pause' });
    await pause.waitFor({ timeout: 30_000 });
    await pause.click();
    await mark('pause clicked');
    // Resume is there once the server has paused it: the line has been redrawn, and Clear is where it stays.
    await goal.getByRole('button', { name: 'Resume' }).waitFor({ timeout: 30_000 });
    expect(await goal.textContent()).toMatch(/paused/);
    await goal.getByRole('button', { name: 'Clear' }).click();
    await mark('clear clicked');
    const setAGoal = page.getByRole('button', { name: 'Set a goal' });
    try {
      await setAGoal.waitFor({ timeout: 30_000 });
      // The turn that waited ends now, with its goal gone: it doesn't bring the goal back.
      goalTurn.open();
      await send.waitFor({ timeout: 30_000 });
      expect(await setAGoal.count()).toBe(1);
    } catch (error) {
      const bar = await page.getByTestId('details').textContent();
      const log = await open!.app.evaluate(() => (globalThis as unknown as { goalLog: string[] }).goalLog.join('\n'));
      throw new Error(`"Set a goal" didn't come back after Clear, or didn't stay: ${(error as Error).message}\nThe details panel: ${bar}\nWhat the app heard:\n${log}`);
    } finally {
      goalTurn.open();
    }
  });

  it('forks the session into a copy it opens', async () => {
    const page = await session([{ text: 'First answer.' }]);
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill('Original question');
    await box.press('Enter');
    await page.getByTestId('agent-message').filter({ hasText: 'First answer.' }).waitFor({ timeout: 30_000 });
    await page.getByRole('button', { name: 'Session menu' }).click();
    await page.getByRole('menuitem', { name: /Fork/ }).click();
    await page.getByRole('navigation').getByRole('button', { name: /Fork of/ }).waitFor({ timeout: 30_000 });
    // The list hears of the fork in a broadcast; the pane opens it when the
    // request is answered, and shows its conversation a moment after that.
    await expect.poll(() => page.getByRole('heading', { level: 1 }).textContent(), { timeout: 30_000 }).toMatch(/Fork of/);
    await page.getByTestId('agent-message').filter({ hasText: 'First answer.' }).waitFor({ timeout: 30_000 });
    expect(await page.getByTestId('agent-message').filter({ hasText: 'First answer.' }).count()).toBe(1);
  });
});
