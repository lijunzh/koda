// A prompt entered while its session is still opening: Enter isn't lost, the
// prompt goes once the session is open, and Esc or an edit takes it back. The
// app's `session/resume` is held in the main process, so the session stays
// opening for as long as each test needs.
import { mkdirSync, realpathSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { scratch } from '../helpers';
import { holdRequests, koda, type OpenApp, openApp, shutDown } from './app';
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

/** The app on a scratch home whose sessions run on `replies`, and a way to ask in a new session. */
async function app(replies: Reply[]) {
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
  const send = page.getByRole('button', { name: 'Send', exact: true });
  /** A new session whose first prompt is `prompt`, once its turn has ended. */
  const ask = async (prompt: string, reply: string) => {
    await page.getByRole('button', { name: 'New session' }).click();
    await expect.poll(() => page.getByTestId('agent-message').count()).toBe(0);
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill(prompt);
    await box.press('Enter');
    await page.getByTestId('agent-message').filter({ hasText: reply }).waitFor({ timeout: 30_000 });
    // Send is back in place of Stop: the turn has ended.
    await send.waitFor({ timeout: 30_000 });
  };
  return { app, page, ask, model, send };
}

describe.skipIf(!koda)('a prompt entered while the session opens', () => {
  it('is sent once an unarchived session is open, once; an edit takes it back', async () => {
    const { app: electron, page, ask, model, send } = await app([
      { text: 'First answer.' },
      { text: 'Second answer.' },
      { text: 'Third answer.' },
    ]);
    await ask('Alpha question', 'First answer.');
    const reply = (text: string) => page.getByTestId('agent-message').filter({ hasText: text });
    const list = page.getByRole('navigation');

    // Archived, and opened to read.
    await page.getByRole('button', { name: 'Session menu' }).click();
    await page.getByRole('menuitem', { name: 'Archive' }).click();
    await page.getByTestId('new-session').waitFor({ timeout: 30_000 });
    await list.getByRole('button', { name: 'Archived', exact: true }).click();
    await list.getByTestId('archived-list').getByRole('button', { name: /Alpha question/ }).click();
    const bar = page.getByTestId('archived-bar');
    await bar.waitFor({ timeout: 30_000 });

    // Unarchive, with the session held at opening: type and press Enter at once.
    const hold = await holdRequests(electron, 'session/resume');
    await bar.getByRole('button', { name: 'Unarchive' }).click();
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.waitFor({ timeout: 30_000 });
    await box.fill('Alpha');
    await box.press('Enter');
    const pending = page.getByTestId('send-pending');
    await pending.waitFor({ timeout: 30_000 });
    // Changed since Enter: no longer the prompt that was sent.
    await box.fill('Alpha again');
    expect(await pending.count()).toBe(0);
    await box.press('Enter');
    await pending.waitFor({ timeout: 30_000 });
    expect(await box.inputValue()).toBe('Alpha again');
    // The session is still opening: its request is the one held.
    await expect.poll(() => hold.waiting(), { timeout: 30_000 }).toBe(1);

    await hold.release();
    await reply('Second answer.').waitFor({ timeout: 30_000 });
    expect(await box.inputValue()).toBe('');
    expect(await pending.count()).toBe(0);
    // Once: after a later prompt has had its own reply, there is still one of it.
    await send.waitFor({ timeout: 30_000 });
    await box.fill('And then?');
    await box.press('Enter');
    await reply('Third answer.').waitFor({ timeout: 30_000 });
    expect(await page.getByTestId('user-message').filter({ hasText: 'Alpha again' }).count()).toBe(1);
    expect(model.calls()).toBe(3);
  });

  it('waits in its own pane of a split view, and Esc takes it back', async () => {
    const { app: electron, page, ask, model } = await app([
      { text: 'First answer.' },
      { text: 'Second answer.' },
      { text: 'Third answer.' },
      { text: 'Fourth answer.' },
    ]);
    await ask('Alpha question', 'First answer.');
    await ask('Beta question', 'Second answer.');
    const panes = page.getByTestId('pane');
    const [left, right] = [panes.nth(0), panes.nth(1)];
    const reply = (pane: typeof left, text: string) => pane.getByTestId('agent-message').filter({ hasText: text });

    // Alpha opens beside Beta, held at opening.
    const hold = await holdRequests(electron, 'session/resume');
    await page.getByRole('navigation').getByRole('button', { name: /Alpha question/ }).click({ modifiers: ['Alt'] });
    await expect.poll(() => panes.count(), { timeout: 30_000 }).toBe(2);
    const rightBox = right.getByRole('textbox', { name: 'Prompt' });
    await rightBox.fill('Alpha again');
    await rightBox.press('Enter');
    await right.getByTestId('send-pending').waitFor({ timeout: 30_000 });

    // The other pane's session is open: its prompt goes at once, and nothing waits there.
    expect(await left.getByTestId('send-pending').count()).toBe(0);
    const leftBox = left.getByRole('textbox', { name: 'Prompt' });
    await leftBox.fill('Beta again');
    await leftBox.press('Enter');
    await reply(left, 'Third answer.').waitFor({ timeout: 30_000 });
    expect(await right.getByTestId('send-pending').count()).toBe(1);

    // Esc takes the waiting prompt back: the text stays, and opening doesn't send it.
    await rightBox.press('Escape');
    expect(await right.getByTestId('send-pending').count()).toBe(0);
    await hold.release();
    await reply(right, 'First answer.').waitFor({ timeout: 30_000 });
    expect(await rightBox.inputValue()).toBe('Alpha again');
    // Enter sends it now. Had opening sent it too, there would be two of it.
    await rightBox.press('Enter');
    await reply(right, 'Fourth answer.').waitFor({ timeout: 30_000 });
    expect(await right.getByTestId('user-message').filter({ hasText: 'Alpha again' }).count()).toBe(1);
    expect(await left.textContent()).not.toContain('Fourth answer.');
    expect(model.calls()).toBe(4);
  });

  it("keeps the text and says why when the session can't be opened", async () => {
    const { app: electron, page, ask } = await app([{ text: 'First answer.' }]);
    await ask('Alpha question', 'First answer.');
    // Back to a new session, so the list's row opens Alpha again.
    await page.getByRole('button', { name: 'New session' }).click();
    await page.getByTestId('new-session').waitFor({ timeout: 30_000 });

    const hold = await holdRequests(electron, 'session/resume');
    await page.getByRole('navigation').getByRole('button', { name: /Alpha question/ }).click();
    const view = page.getByTestId('session-view');
    await view.waitFor({ timeout: 30_000 });
    const box = view.getByRole('textbox', { name: 'Prompt' });
    await box.fill('Alpha again');
    await box.press('Enter');
    const pending = page.getByTestId('send-pending');
    await pending.waitFor({ timeout: 30_000 });

    // Opening fails once its request is the one held.
    await expect.poll(() => hold.waiting(), { timeout: 30_000 }).toBe(1);
    await hold.fail('the session is locked');
    await view.getByText(/the session is locked/).waitFor({ timeout: 30_000 });
    await pending.waitFor({ state: 'detached', timeout: 30_000 });
    expect(await box.inputValue()).toBe('Alpha again');
  });
});
