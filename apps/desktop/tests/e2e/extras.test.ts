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
  await page.getByTestId('session-bar').waitFor({ timeout: 30_000 });
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
    await page.getByRole('button', { name: 'Set a goal' }).click();
    await page.getByRole('textbox', { name: 'Goal' }).fill('Keep the tests green');
    await page.getByRole('button', { name: 'Set goal' }).click();
    const goal = page.getByTestId('goal');
    await expect.poll(() => goal.textContent(), { timeout: 30_000 }).toContain('Keep the tests green');
    const pause = goal.getByRole('button', { name: 'Pause' });
    if (await pause.count()) {
      await pause.click();
      await expect.poll(() => goal.textContent(), { timeout: 30_000 }).toMatch(/paused|done|blocked/);
    }
    await goal.getByRole('button', { name: 'Clear' }).click();
    await page.getByRole('button', { name: 'Set a goal' }).waitFor({ timeout: 30_000 });
  });

  it('forks the session into a copy it opens', async () => {
    const page = await session([{ text: 'First answer.' }]);
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill('Original question');
    await box.press('Enter');
    await page.getByTestId('agent-message').filter({ hasText: 'First answer.' }).waitFor({ timeout: 30_000 });
    await page.getByRole('button', { name: 'Fork' }).click();
    await page.getByRole('navigation').getByRole('button', { name: /Fork of/ }).waitFor({ timeout: 30_000 });
    expect(await page.getByRole('heading', { level: 1 }).textContent()).toMatch(/Fork of/);
    expect(await page.getByTestId('agent-message').filter({ hasText: 'First answer.' }).count()).toBe(1);
  });
});
