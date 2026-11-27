// Sessions side by side, as Claude Code's split view: right-click a session
// in the list and open it beside the one open; each pane runs its own
// session, and closing a pane leaves the other.
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

describe.skipIf(!koda)('split view', () => {
  it('opens a session beside another, each pane running its own', async () => {
    const home = scratch();
    homes.push(home);
    model = await scriptedModel([{ text: 'First answer.' }, { text: 'Second answer.' }, { text: 'Third answer.' }]);
    useModel(home, model);
    const project = path.join(home, 'project');
    mkdirSync(project);
    open = await openApp(home);
    const { app, page } = open;
    await page.getByRole('button', { name: 'New session' }).waitFor({ timeout: 30_000 });
    await app.evaluate(({ dialog }, picked) => {
      dialog.showOpenDialog = (async () => ({ canceled: false, filePaths: [picked] })) as typeof dialog.showOpenDialog;
    }, realpathSync(project));

    const ask = async (prompt: string, reply: string) => {
      await page.getByRole('button', { name: 'New session' }).click();
      // The new, empty session has taken the pane.
      await expect.poll(() => page.getByTestId('agent-message').count()).toBe(0);
      const box = page.getByRole('textbox', { name: 'Prompt' });
      await box.fill(prompt);
      await box.press('Enter');
      await page.getByTestId('agent-message').filter({ hasText: reply }).waitFor({ timeout: 30_000 });
    };
    await ask('Alpha question', 'First answer.');
    await ask('Beta question', 'Second answer.');
    const panes = page.getByTestId('pane');
    expect(await panes.count()).toBe(1);

    const list = page.getByRole('navigation');
    await list.getByRole('button', { name: /Alpha question/ }).click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Open in split view' }).click();
    await expect.poll(() => panes.count()).toBe(2);
    await panes.nth(1).getByTestId('agent-message').filter({ hasText: 'First answer.' }).waitFor({ timeout: 30_000 });
    expect(await panes.nth(0).textContent()).toContain('Second answer.');
    expect(await panes.nth(0).textContent()).not.toContain('First answer.');

    // Clicking a session already open goes to its pane rather than opening it twice.
    await list.getByRole('button', { name: /Beta question/ }).click();
    expect(await panes.count()).toBe(2);

    // A prompt in the second pane runs in its session only.
    const second = panes.nth(1).getByRole('textbox', { name: 'Prompt' });
    await second.fill('Alpha again');
    await second.press('Enter');
    await panes.nth(1).getByTestId('agent-message').filter({ hasText: 'Third answer.' }).waitFor({ timeout: 30_000 });
    expect(await panes.nth(0).textContent()).not.toContain('Third answer.');

    await panes.nth(1).getByRole('button', { name: 'Close pane' }).click();
    await expect.poll(() => panes.count()).toBe(1);
    expect(await panes.nth(0).textContent()).toContain('Second answer.');
    expect(await page.getByRole('button', { name: 'Close pane' }).count()).toBe(0);
  });
});
