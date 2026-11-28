// A session's goal, its background commands and forking it, driven as a
// user would against a real `koda serve` and a scripted model.
import { mkdirSync, realpathSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { scratch } from '../helpers';
import { koda, type OpenApp, openApp, shutDown } from './app';
import { type Model, type Reply, scriptedModel, useModel } from './model';

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
    const page = await session([]);
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
    await page.getByRole('button', { name: 'Set a goal' }).click();
    await page.getByRole('textbox', { name: 'Goal' }).fill('Keep the tests green');
    await page.getByRole('button', { name: 'Set goal' }).click();
    await mark('set clicked');
    const goal = page.getByTestId('goal');
    await expect.poll(() => goal.textContent(), { timeout: 30_000 }).toContain('Keep the tests green');
    const pause = goal.getByRole('button', { name: 'Pause' });
    if (await pause.count()) {
      await pause.click();
      await mark('pause clicked');
      await expect.poll(() => goal.textContent(), { timeout: 30_000 }).toMatch(/paused|done|blocked/);
    }
    await goal.getByRole('button', { name: 'Clear' }).click();
    await mark('clear clicked');
    try {
      await page.getByRole('button', { name: 'Set a goal' }).waitFor({ timeout: 30_000 });
    } catch {
      const bar = await page.getByTestId('details').textContent();
      const log = await open!.app.evaluate(() => (globalThis as unknown as { goalLog: string[] }).goalLog.join('\n'));
      throw new Error(`"Set a goal" didn't come back after Clear.\nThe details panel: ${bar}\nWhat the app heard:\n${log}`);
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
    expect(await page.getByRole('heading', { level: 1 }).textContent()).toMatch(/Fork of/);
    expect(await page.getByTestId('agent-message').filter({ hasText: 'First answer.' }).count()).toBe(1);
  });
});
