// Text added to a running turn: it waits above the prompt box until the model
// takes it, and a turn stopped before that gives it back to the box. The
// scripted model holds each reply, so the turn runs for as long as a step needs.
import { mkdirSync, realpathSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { scratch } from '../helpers';
import { koda, type OpenApp, openApp, shutDown } from './app';
import { gate, type Model, scriptedModel, useModel } from './model';

let open: OpenApp | undefined;
let model: Model | undefined;
const homes: string[] = [];

afterEach(async () => {
  await shutDown(open, homes.splice(0));
  open = undefined;
  model?.close();
  model = undefined;
});

describe.skipIf(!koda)('text added to a running turn', () => {
  it('waits until the model takes it, and comes back to the box when the turn is stopped first', async () => {
    const home = scratch();
    homes.push(home);
    const first = gate();
    const third = gate();
    model = await scriptedModel([
      { text: 'First answer.', after: first },
      { text: 'Second answer.' },
      { text: 'Never said.', after: third },
    ]);
    const scripted = model;
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

    const box = page.getByRole('textbox', { name: 'Prompt' });
    const waiting = page.getByTestId('steers-waiting');
    const reply = (text: string) => page.getByTestId('agent-message').filter({ hasText: text });
    const send = page.getByRole('button', { name: 'Send', exact: true });
    const working = page.getByTestId('working');

    // A turn whose reply is held; text added to it waits above the box.
    await box.fill('First question');
    await box.press('Enter');
    await expect.poll(() => scripted.calls(), { timeout: 30_000 }).toBe(1);
    // The model has the request, but the window may not have heard the turn
    // start yet: until it has, Enter starts a turn in place of steering one.
    await working.waitFor({ timeout: 30_000 });
    await box.fill('Also this');
    await box.press('Enter');
    await waiting.getByText('Also this').waitFor({ timeout: 30_000 });
    expect(await box.inputValue()).toBe('');

    // The reply ends; the turn goes on with the added text, which leaves the list.
    first.open();
    await reply('Second answer.').waitFor({ timeout: 30_000 });
    await send.waitFor({ timeout: 30_000 });
    expect(await waiting.count()).toBe(0);
    expect(await page.getByTestId('user-message').filter({ hasText: 'Also this' }).count()).toBe(1);
    expect(await box.inputValue()).toBe('');
    expect(scripted.calls()).toBe(2);

    // A turn stopped before the model took the added text: the box has it back,
    // in front of what was typed meanwhile, and nothing was sent for it.
    await box.fill('Third question');
    await box.press('Enter');
    await expect.poll(() => scripted.calls(), { timeout: 30_000 }).toBe(3);
    await working.waitFor({ timeout: 30_000 });
    await box.fill('Not this time');
    await box.press('Enter');
    await waiting.getByText('Not this time').waitFor({ timeout: 30_000 });
    await box.fill('a draft');
    await page.getByRole('button', { name: 'Stop' }).click();
    await send.waitFor({ timeout: 30_000 });
    await expect.poll(() => box.inputValue(), { timeout: 30_000 }).toBe('Not this time\n\na draft');
    expect(await waiting.count()).toBe(0);
    expect(scripted.calls()).toBe(3);
    third.open();
  });
});
