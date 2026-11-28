// Marking a session confidential: "Mark confidential" in a session's menu
// puts a lock on its row and beside its title, in every list it is in, and
// "Unmark confidential" takes it off. The lock follows what the server says
// (`session/confidential/updated`), not the click. A marked session's
// model, a local one, drafts its brief, which waits under the title to be
// accepted, edited or discarded.
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

describe.skipIf(!koda)('confidential', () => {
  it('locks a session marked from its menu, in the list, its title and the archive, until it is unmarked', async () => {
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
    const row = (name: string) => sessions.getByRole('listitem').filter({ hasText: name });
    const title = page.getByTestId('session-view').locator('.title-row');
    const locks = (where: { getByTestId(id: string): { count(): Promise<number> } }) => where.getByTestId('confidential').count();
    const pick = async (from: 'row' | 'title', entry: string) => {
      if (from === 'row') {
        await row('Beta question').hover();
        await row('Beta question').getByRole('button', { name: 'Session actions' }).click();
      } else {
        await page.getByRole('button', { name: 'Session menu' }).click();
      }
      await page.getByRole('menuitem', { name: entry }).click();
    };
    expect(await locks(list)).toBe(0);
    expect(await locks(title)).toBe(0);

    // From the row's menu: Beta, the open session, gets a lock in the list
    // and beside its title once the server says it is marked. Alpha gets none.
    await pick('row', 'Mark confidential');
    await expect.poll(() => locks(row('Beta question')), { timeout: 30_000 }).toBe(1);
    await expect.poll(() => locks(title), { timeout: 30_000 }).toBe(1);
    expect(await locks(row('Alpha question'))).toBe(0);
    expect(await title.getByTestId('confidential').getAttribute('title')).toBe(
      'Confidential: only a local model can read this session from another session',
    );
    expect(await title.getByRole('img', { name: 'Confidential' }).count()).toBe(1);

    // The menus now offer to unmark it. From the title's menu: both locks go.
    await pick('title', 'Unmark confidential');
    await expect.poll(() => locks(row('Beta question')), { timeout: 30_000 }).toBe(0);
    await expect.poll(() => locks(title), { timeout: 30_000 }).toBe(0);

    // Marked again from the title's menu, then archived: the Archived list
    // shows the lock, and its menu takes it off where the session is.
    await pick('title', 'Mark confidential');
    await expect.poll(() => locks(row('Beta question')), { timeout: 30_000 }).toBe(1);
    await pick('row', 'Archive');
    await expect.poll(() => row('Beta question').count(), { timeout: 30_000 }).toBe(0);
    await list.getByRole('button', { name: 'Archived', exact: true }).click();
    const archivedRow = list.getByTestId('archived-list').getByRole('listitem').filter({ hasText: 'Beta question' });
    await expect.poll(() => locks(archivedRow), { timeout: 30_000 }).toBe(1);
    await archivedRow.hover();
    await archivedRow.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Unmark confidential' }).click();
    await expect.poll(() => locks(archivedRow), { timeout: 30_000 }).toBe(0);
    expect(await archivedRow.count()).toBe(1);
  });
  it("offers the brief a session's local model drafted, to accept, edit or discard, and says the accepted one on the lock", async () => {
    const home = scratch();
    homes.push(home);
    // Each session's one turn, then the draft Koda asks its model for when
    // the session is marked.
    model = await scriptedModel([
      { text: 'First answer.' },
      { text: '"Planning a trip"' },
      { text: 'Second answer.' },
      { text: 'Reviewing a contract' },
      { text: 'Third answer.' },
      { text: 'Doing the accounts' },
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

    const view = page.getByTestId('session-view');
    const draft = view.getByTestId('brief-draft');
    const lock = view.locator('.title-row').getByTestId('confidential');
    const box = view.getByRole('textbox', { name: 'Brief for models on remote providers' });
    const tip = 'Confidential: only a local model can read this session from another session';
    const told = (brief: string) => `${tip}. Models on remote providers are told: "${brief}"`;
    const fromMenu = async (entry: string) => {
      await page.getByRole('button', { name: 'Session menu' }).click();
      await page.getByRole('menuitem', { name: entry }).click();
    };
    // A new session with one turn, marked: its draft comes under the title.
    const marked = async (prompt: string, answer: string, drafted: string) => {
      await page.getByRole('button', { name: 'New session' }).click();
      await expect.poll(() => page.getByTestId('agent-message').count()).toBe(0);
      const prompts = page.getByRole('textbox', { name: 'Prompt' });
      await prompts.fill(prompt);
      await prompts.press('Enter');
      await page.getByTestId('agent-message').filter({ hasText: answer }).waitFor({ timeout: 30_000 });
      expect(await draft.count()).toBe(0);
      await fromMenu('Mark confidential');
      await draft.waitFor({ timeout: 30_000 });
      expect(await draft.locator('.brief-text').textContent()).toBe(`Brief for sessions on remote models: "${drafted}"`);
      // A draft is no brief yet: the lock says nothing of it.
      expect(await lock.getAttribute('title')).toBe(tip);
    };

    // Edit: the draft in a one-line box. Esc leaves it waiting; Enter
    // saves what was typed as the brief, and the line goes.
    await marked('Alpha question', 'First answer.', 'Planning a trip');
    await draft.getByRole('button', { name: 'Edit' }).click();
    expect(await box.inputValue()).toBe('Planning a trip');
    await box.fill('Something else');
    await box.press('Escape');
    await draft.waitFor();
    await draft.getByRole('button', { name: 'Edit' }).click();
    await box.fill('  Planning travel ');
    await box.press('Enter');
    await expect.poll(() => lock.getAttribute('title'), { timeout: 30_000 }).toBe(told('Planning travel'));
    expect(await draft.count()).toBe(0);
    // "Edit brief…" opens the box on the brief; saved empty, it is cleared.
    await fromMenu('Edit brief…');
    expect(await box.inputValue()).toBe('Planning travel');
    await box.fill('');
    await box.press('Enter');
    await expect.poll(() => lock.getAttribute('title'), { timeout: 30_000 }).toBe(tip);

    // Accept: the draft as it is.
    await marked('Beta question', 'Second answer.', 'Reviewing a contract');
    await draft.getByRole('button', { name: 'Accept' }).click();
    await expect.poll(() => lock.getAttribute('title'), { timeout: 30_000 }).toBe(told('Reviewing a contract'));
    expect(await draft.count()).toBe(0);
    // The session's row in the list says it too.
    const row = page.getByRole('navigation').getByTestId('folder-group').getByRole('listitem').filter({ hasText: 'Beta question' });
    expect(await row.getByTestId('confidential').getAttribute('title')).toBe(told('Reviewing a contract'));

    // Discard: no brief, and the line goes.
    await marked('Gamma question', 'Third answer.', 'Doing the accounts');
    await draft.getByRole('button', { name: 'Discard' }).click();
    await expect.poll(() => draft.count(), { timeout: 30_000 }).toBe(0);
    expect(await lock.getAttribute('title')).toBe(tip);
    expect(model.calls()).toBe(6);
    // What the drafts were asked with: no stream, and the session's own words.
    const asked = model.bodies().map((body) => JSON.parse(body) as { stream?: boolean; messages: Array<{ content: string }> });
    const drafts = asked.filter((request) => request.stream !== true);
    expect(drafts.map((request) => /\[user\]\n(\w+) question\n/.exec(request.messages[0].content)?.[1])).toEqual(['Alpha', 'Beta', 'Gamma']);
  });
});
