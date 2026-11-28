// Archiving a session, as Claude Code's desktop app does: Archive in a
// session's menu puts it away, the Archived list shows it, an archived
// session opens to read with Unarchive, and Unarchive brings it back.
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

describe.skipIf(!koda)('archive', () => {
  it('puts a session away, shows it under Archived to read, and brings it back', async () => {
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

    const reply = (text: string) => page.getByTestId('agent-message').filter({ hasText: text });
    const ask = async (prompt: string, answer: string) => {
      await page.getByRole('button', { name: 'New session' }).click();
      await expect.poll(() => page.getByTestId('agent-message').count()).toBe(0);
      const box = page.getByRole('textbox', { name: 'Prompt' });
      await box.fill(prompt);
      await box.press('Enter');
      await reply(answer).waitFor({ timeout: 30_000 });
    };
    await ask('Alpha question', 'First answer.');
    await ask('Beta question', 'Second answer.');

    const list = page.getByRole('navigation');
    const sessions = list.getByTestId('folder-group');
    const archivedList = list.getByTestId('archived-list');

    // Archive from a row's menu: Alpha leaves the list; Beta, open, stays.
    const row = sessions.getByRole('listitem').filter({ hasText: 'Alpha question' });
    await row.hover();
    await row.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Archive' }).click();
    // Gone once the server has archived it and said so: a request and a broadcast away.
    await expect.poll(() => sessions.getByRole('button', { name: /Alpha question/ }).count(), { timeout: 30_000 }).toBe(0);
    expect(await sessions.getByRole('button', { name: /Beta question/ }).count()).toBe(1);
    await reply('Second answer.').waitFor();

    // The Archived list has it. Opened, it reads, with Unarchive where the prompt box is.
    await list.getByRole('button', { name: 'Archived', exact: true }).click();
    await archivedList.getByRole('button', { name: /Alpha question/ }).click();
    await reply('First answer.').waitFor({ timeout: 30_000 });
    const bar = page.getByTestId('archived-bar');
    await bar.waitFor();
    expect(await page.getByRole('textbox', { name: 'Prompt' }).count()).toBe(0);
    expect(await page.getByTestId('session-status').textContent()).toContain('archived');

    // Unarchive: the session opens for work again, and is listed again.
    await bar.getByRole('button', { name: 'Unarchive' }).click();
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.waitFor({ timeout: 30_000 });
    // The prompt box is there before the server has opened the session: the
    // conversation shows once it is open. (Enter before that waits for it; see opening.test.ts.)
    await reply('First answer.').waitFor({ timeout: 30_000 });
    // The Archived list is asked for again after the server says it was unarchived.
    await expect.poll(() => archivedList.getByRole('button', { name: /Alpha question/ }).count(), { timeout: 30_000 }).toBe(0);
    await list.getByRole('button', { name: 'Archived', exact: true }).click();
    await sessions.getByRole('button', { name: /Alpha question/ }).waitFor();
    await box.fill('Alpha again');
    await box.press('Enter');
    await reply('Third answer.').waitFor({ timeout: 30_000 });

    // Archive from the title's menu: the open session is put away and its pane is a new session.
    await page.getByRole('button', { name: 'Session menu' }).click();
    await page.getByRole('menuitem', { name: 'Archive' }).click();
    await page.getByTestId('new-session').waitFor({ timeout: 30_000 });
    await expect.poll(() => sessions.getByRole('button', { name: /Alpha question/ }).count()).toBe(0);

    // Unarchive from the Archived list's menu, without opening it.
    await list.getByRole('button', { name: 'Archived', exact: true }).click();
    const archivedRow = archivedList.getByRole('listitem').filter({ hasText: 'Alpha question' });
    await archivedRow.hover();
    await archivedRow.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Unarchive' }).click();
    await archivedList.getByText('No archived sessions.').waitFor();
    await list.getByRole('button', { name: 'Archived', exact: true }).click();
    await sessions.getByRole('button', { name: /Alpha question/ }).waitFor();
  });
});
