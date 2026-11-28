// The open session's controls, driven as a user would against a real
// `koda serve` and a scripted model: the plan panel while a turn runs (and
// Ctrl+T), the context meter, the mode (Auto only after the system dialog),
// and renaming the session.
import { mkdirSync, realpathSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { scratch } from '../helpers';
import { koda, type OpenApp, openApp, shutDown } from './app';
import { type Model, scriptedModel, useModel } from './model';

let open: OpenApp | undefined;
let model: Model | undefined;
const homes: string[] = [];

afterEach(async () => {
  await shutDown(open, homes.splice(0));
  open = undefined;
  model?.close();
  model = undefined;
});

describe.skipIf(!koda)('session controls', () => {
  it('shows the plan while a turn runs, the context left, the mode under the prompt box, and renames the session', async () => {
    const home = scratch();
    homes.push(home);
    model = await scriptedModel([
      {
        tool: 'TodoWrite',
        args: {
          todos: [
            { content: 'Read the code', status: 'completed', priority: 'high' },
            { content: 'Write the fix', status: 'in_progress', priority: 'high' },
          ],
        },
      },
      { text: 'Working on the fix. '.repeat(30), pieces: 30, delayMs: 100 },
    ]);
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

    // The plan, while the turn runs; Ctrl+T hides and shows it.
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill('Fix the bug');
    await box.press('Enter');
    const plan = page.getByTestId('plan');
    await expect.poll(() => plan.textContent(), { timeout: 30_000 }).toContain('Write the fix');
    await page.keyboard.press('Control+T');
    await expect.poll(() => plan.textContent()).toContain('1 step left');
    await page.keyboard.press('Control+T');
    await expect.poll(() => plan.textContent()).toContain('Read the code');
    // Done: the plan goes, and the meter says how much context is left.
    await expect.poll(() => page.getByTestId('plan').count(), { timeout: 30_000 }).toBe(0);
    await expect.poll(() => page.getByTestId('context-left').textContent()).toMatch(/\d+% context left/);

    // The mode, under the prompt box: Plan at once, Auto only after the system dialog says so.
    const mode = page.getByRole('button', { name: 'Mode', exact: true });
    const pick = async (name: RegExp) => {
      await mode.click();
      await page.getByRole('menuitemradio', { name }).click();
    };
    await pick(/Plan/);
    await expect.poll(() => mode.textContent()).toBe('Plan');
    await app.evaluate(({ dialog }) => {
      dialog.showMessageBox = (async () => ({ response: 1, checkboxChecked: false })) as typeof dialog.showMessageBox;
    });
    await pick(/Auto/);
    await new Promise((r) => setTimeout(r, 500));
    expect(await mode.textContent()).toBe('Plan');
    await app.evaluate(({ dialog }) => {
      dialog.showMessageBox = (async () => ({ response: 0, checkboxChecked: false })) as typeof dialog.showMessageBox;
    });
    await pick(/Auto/);
    await expect.poll(() => mode.textContent()).toBe('Auto');
    // A number key picks a mode while the menu is open.
    await mode.click();
    await page.keyboard.press('2');
    await expect.poll(() => mode.textContent()).toBe('Plan');

    // Renaming, from the title.
    await page.getByRole('heading', { level: 1 }).click();
    const name = page.getByRole('textbox', { name: 'Session name' });
    await name.fill('Bug fix');
    await name.press('Enter');
    await page.getByRole('navigation').getByRole('button', { name: /Bug fix/ }).waitFor();
    expect(await page.getByRole('heading', { level: 1 }).textContent()).toBe('Bug fix');
  });
});
