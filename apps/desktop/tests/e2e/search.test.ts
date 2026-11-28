// Search, against a real `koda serve`: besides narrowing the list by title,
// it lists the sessions in which the text was said, each with the words
// around it, and a click opens the session at that place.
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

describe.skipIf(!koda)('search', () => {
  it('finds a session by what was said in it, and opens it at the match', async () => {
    const home = scratch();
    homes.push(home);
    // Long enough that Alpha's first answer is far above the bottom of its conversation.
    const long = Array.from({ length: 80 }, (_, i) => `Filler paragraph ${i + 1}.`).join('\n\n');
    model = await scriptedModel([
      { text: 'The zebra crossing is the safe place.' },
      { text: long },
      { text: 'Cats sleep most of the day.' },
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

    const reply = (text: string) => page.getByTestId('agent-message').filter({ hasText: text });
    const say = async (prompt: string, answer: string) => {
      const box = page.getByRole('textbox', { name: 'Prompt' });
      await box.fill(prompt);
      await box.press('Enter');
      await reply(answer).waitFor({ timeout: 30_000 });
    };
    const start = async () => {
      await page.getByRole('button', { name: 'New session' }).click();
      await expect.poll(() => page.getByTestId('agent-message').count()).toBe(0);
    };
    await start();
    await say('Alpha question', 'zebra crossing');
    await say('Go on', 'Filler paragraph 80.');
    await start();
    await say('Beta question', 'Cats sleep');

    // Neither title has the word: the list empties, and the server is asked.
    const list = page.getByRole('navigation');
    await list.getByRole('button', { name: 'Search' }).click();
    await list.getByRole('textbox', { name: 'Search sessions' }).fill('ZEBRA');
    const results = list.getByTestId('content-results');
    const row = results.getByRole('button', { name: /Alpha question/ });
    await row.waitFor({ timeout: 30_000 });
    expect(await results.locator('h2').textContent()).toBe('In conversations');
    expect(await list.getByTestId('folder-group').count()).toBe(0);
    expect(await results.getByRole('listitem').count()).toBe(1);
    expect(await row.locator('.snippet').textContent()).toContain('The zebra crossing is the safe place.');
    expect(await row.locator('mark').allTextContents()).toEqual(['zebra']);
    expect(await row.locator('.session-folder').textContent()).toBe('project');

    // A click opens it, as a click in the list does, with the answer that
    // matched in view: the bottom, where a session opens, is 80 paragraphs on.
    await row.click();
    await expect.poll(() => page.getByRole('heading', { level: 1 }).textContent(), { timeout: 30_000 }).toBe('Alpha question');
    await page.getByRole('textbox', { name: 'Prompt' }).waitFor();
    const inView = () =>
      page.evaluate(() => {
        const pane = document.querySelector('[data-testid="transcript"]');
        const match = [...document.querySelectorAll('[data-testid="agent-message"]')].find((el) =>
          el.textContent?.includes('zebra crossing'),
        );
        if (!pane || !match) return false;
        const outer = pane.getBoundingClientRect();
        const inner = match.getBoundingClientRect();
        return inner.top >= outer.top && inner.bottom <= outer.bottom;
      });
    await reply('zebra crossing').waitFor({ timeout: 30_000 });
    // Still there once the window has drawn again: hiding the list redraws
    // the conversation, which would go back to the bottom if it followed it.
    await page.getByRole('button', { name: 'Hide sidebar' }).click();
    await page.getByRole('button', { name: 'Show sidebar' }).waitFor();
    expect(await inView()).toBe(true);
    await page.getByRole('button', { name: 'Show sidebar' }).click();

    // A word nobody said: no session by title, none by content.
    await list.getByRole('textbox', { name: 'Search sessions' }).fill('giraffe');
    await list.getByText('No conversation mentions it.').waitFor({ timeout: 30_000 });
    expect(await results.count()).toBe(0);
  });
});
