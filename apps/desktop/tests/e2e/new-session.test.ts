// A new session, as Claude Code's desktop app starts one: the prompt box
// with the folder, the model and the mode under it, and nothing started
// until the first prompt.
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

describe.skipIf(!koda)('a new session', () => {
  it('asks for a folder once, starts with the first prompt in the mode picked, offers the folder again; Settings set the theme', async () => {
    const home = scratch();
    homes.push(home);
    model = await scriptedModel([{ text: 'First answer.' }, { text: 'Second answer.' }]);
    useModel(home, model);
    const project = path.join(home, 'project');
    mkdirSync(project);
    open = await openApp(home);
    const { app, page } = open;
    // The app opens on a new session: nothing is started, and no folder is known.
    await page.getByTestId('new-session').waitFor({ timeout: 30_000 });
    expect(await page.getByTestId('new-session-folder').textContent()).toBe('Choose a folder');
    await page.getByText('No sessions yet.').waitFor();

    // Plan, picked before the session exists.
    await page.getByRole('button', { name: 'Mode', exact: true }).click();
    await page.getByRole('menuitemradio', { name: /Plan/ }).click();
    expect(await page.getByRole('button', { name: 'Mode', exact: true }).textContent()).toBe('Plan');

    // The first prompt asks for the folder in the system's dialog, then starts the session there.
    let asked = 0;
    await app.evaluate(({ dialog }, picked) => {
      const g = globalThis as { folderDialogs?: number };
      g.folderDialogs = 0;
      dialog.showOpenDialog = (async () => {
        g.folderDialogs = (g.folderDialogs ?? 0) + 1;
        return { canceled: false, filePaths: [picked] };
      }) as typeof dialog.showOpenDialog;
    }, realpathSync(project));
    const prompt = page.getByRole('textbox', { name: 'Prompt' });
    await prompt.fill('Alpha question');
    await prompt.press('Enter');
    await page.getByTestId('agent-message').filter({ hasText: 'First answer.' }).waitFor({ timeout: 30_000 });
    expect(await page.getByTestId('session-folder').textContent()).toBe(realpathSync(project));
    expect(await page.getByRole('button', { name: 'Mode', exact: true }).textContent()).toBe('Plan');
    asked = await app.evaluate(() => (globalThis as { folderDialogs?: number }).folderDialogs ?? 0);
    expect(asked).toBe(1);

    // Cmd+N: another new session, in the same folder without asking again.
    await page.keyboard.press('Meta+n');
    await page.getByTestId('new-session').waitFor();
    expect(await page.getByTestId('new-session-folder').textContent()).toBe('project');
    const again = page.getByRole('textbox', { name: 'Prompt' });
    await again.fill('Beta question');
    await again.press('Enter');
    await page.getByTestId('agent-message').filter({ hasText: 'Second answer.' }).waitFor({ timeout: 30_000 });
    expect(await app.evaluate(() => (globalThis as { folderDialogs?: number }).folderDialogs ?? 0)).toBe(1);
    await page.getByRole('navigation').getByRole('button', { name: /Beta question/ }).waitFor();

    // Settings: the theme follows the system until one is chosen, and the choice is kept.
    const theme = () => page.evaluate(() => document.documentElement.dataset.theme ?? 'system');
    expect(await theme()).toBe('system');
    await page.getByRole('button', { name: 'Settings' }).click();
    const settings = page.getByTestId('settings');
    await settings.getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'Dark' }).click();
    expect(await theme()).toBe('dark');
    await page.emulateMedia({ colorScheme: 'light' });
    expect(await page.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe('rgb(26, 26, 25)');
    await settings.getByRole('group', { name: 'Transcript width' }).getByRole('button', { name: 'Wide' }).click();
    expect(await page.evaluate(() => document.documentElement.dataset.width)).toBe('wide');
    expect(await page.evaluate(() => window.localStorage.getItem('koda.prefs'))).toBe('{"theme":"dark","width":"wide"}');
    await page.keyboard.press('Escape');
    expect(await page.getByTestId('settings').count()).toBe(0);
    // Cmd+, opens it again.
    await page.keyboard.press('Meta+,');
    await page.getByTestId('settings').getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'System' }).click();
    expect(await theme()).toBe('system');
  });
});
