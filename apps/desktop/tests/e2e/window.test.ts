// The window's own buttons, as Claude Code's desktop app has them at its top
// left: hide or show the sidebar (Cmd+B), and Back and Forward (Cmd+[ and
// Cmd+]) through the sessions the window has shown.
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

describe.skipIf(!koda)('the window', () => {
  it('goes back and forward through the sessions shown, and hides the sidebar', async () => {
    const home = scratch();
    homes.push(home);
    model = await scriptedModel([{ text: 'First answer.' }, { text: 'Second answer.' }]);
    useModel(home, model);
    const project = path.join(home, 'project');
    mkdirSync(project);
    open = await openApp(home);
    const { app, page } = open;
    await page.getByRole('button', { name: 'New session' }).waitFor({ timeout: 30_000 });
    await app.evaluate(({ dialog }, picked) => {
      dialog.showOpenDialog = (async () => ({ canceled: false, filePaths: [picked] })) as typeof dialog.showOpenDialog;
    }, realpathSync(project));

    const back = page.getByRole('button', { name: 'Back', exact: true });
    const forward = page.getByRole('button', { name: 'Forward', exact: true });
    // Nowhere to go yet.
    expect(await back.isDisabled()).toBe(true);
    expect(await forward.isDisabled()).toBe(true);

    const ask = async (prompt: string, reply: string) => {
      await page.getByRole('button', { name: 'New session' }).click();
      await expect.poll(() => page.getByTestId('agent-message').count()).toBe(0);
      const box = page.getByRole('textbox', { name: 'Prompt' });
      await box.fill(prompt);
      await box.press('Enter');
      await page.getByTestId('agent-message').filter({ hasText: reply }).waitFor({ timeout: 30_000 });
    };
    await ask('Alpha question', 'First answer.');
    await ask('Beta question', 'Second answer.');
    const shows = (reply: string) => page.getByTestId('agent-message').filter({ hasText: reply }).waitFor({ timeout: 30_000 });

    // Back: the new-session screen Beta was started from, then Alpha.
    await back.click();
    await page.getByTestId('new-session').waitFor();
    await back.click();
    await shows('First answer.');
    expect(await forward.isDisabled()).toBe(false);

    // Forward, by its keys, to Beta again; then there is nothing ahead.
    await page.keyboard.press('Meta+]');
    await page.getByTestId('new-session').waitFor();
    await page.keyboard.press('Meta+]');
    await shows('Second answer.');
    expect(await forward.isDisabled()).toBe(true);

    // Going somewhere else from the past drops what was ahead.
    await page.keyboard.press('Meta+[');
    await page.keyboard.press('Meta+[');
    await shows('First answer.');
    await page.getByRole('navigation').getByRole('button', { name: /Beta question/ }).click();
    await shows('Second answer.');
    expect(await forward.isDisabled()).toBe(true);
    await back.click();
    await shows('First answer.');

    // The sidebar hides and comes back, by the button and by Cmd+B; the
    // session stays open, and the choice is kept for the next launch.
    const list = page.getByRole('navigation');
    await page.getByRole('button', { name: 'Hide sidebar' }).click();
    await expect.poll(() => list.isVisible()).toBe(false);
    expect(await page.getByTestId('agent-message').filter({ hasText: 'First answer.' }).isVisible()).toBe(true);
    expect(await page.evaluate(() => JSON.parse(localStorage.getItem('koda.prefs') ?? '{}').sidebar)).toBe(false);
    await page.keyboard.press('Meta+b');
    await expect.poll(() => list.isVisible()).toBe(true);
    await page.getByRole('button', { name: 'Hide sidebar' }).waitFor();
  });
});
