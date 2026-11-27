// The conversation, driven as a user would against a real `koda serve` and
// a scripted model: a prompt runs a turn whose lookups fold into one row and
// whose answer streams in as Markdown; the finished turn folds; Stop ends a
// running turn.
import { mkdirSync, realpathSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { scratch } from '../helpers';
import { closeApp, koda, type OpenApp, openApp, stopServer } from './app';
import { type Model, scriptedModel, useModel } from './model';

let open: OpenApp | undefined;
let model: Model | undefined;
const homes: string[] = [];

afterEach(async () => {
  for (const home of homes.splice(0)) stopServer(home);
  if (open) await closeApp(open);
  open = undefined;
  model?.close();
  model = undefined;
});

/** The app on a scratch home whose sessions run on `replies`, with a new session open in a project folder. */
async function session(replies: Parameters<typeof scriptedModel>[0]) {
  const home = scratch();
  homes.push(home);
  model = await scriptedModel(replies);
  useModel(home, model);
  const project = path.join(home, 'project');
  mkdirSync(project);
  writeFileSync(path.join(project, 'README.md'), '# Demo\n');
  open = await openApp(home);
  const { app, page } = open;
  await page.getByRole('button', { name: 'New session' }).waitFor({ timeout: 30_000 });
  await app.evaluate(({ dialog }, picked) => {
    dialog.showOpenDialog = (async () => ({ canceled: false, filePaths: [picked] })) as typeof dialog.showOpenDialog;
  }, realpathSync(project));
  await page.getByRole('button', { name: 'New session' }).click();
  await page.getByRole('textbox', { name: 'Prompt' }).waitFor();
  return page;
}

describe.skipIf(!koda)('the conversation', () => {
  it('runs a turn: lookups fold to one row, the answer streams as Markdown, the finished turn folds', async () => {
    const page = await session([
      { tool: 'Bash', args: { command: 'ls' } },
      { tool: 'Bash', args: { command: 'cat README.md' } },
      { text: 'The project has **one file**:\n\n| File | Kind |\n|---|---|\n| README.md | Markdown |\n', pieces: 6, delayMs: 30 },
    ]);
    const prompt = page.getByRole('textbox', { name: 'Prompt' });
    await prompt.fill('What is in this project?');
    await prompt.press('Enter');

    await page.getByTestId('user-message').filter({ hasText: 'What is in this project?' }).waitFor();
    const answer = page.getByTestId('agent-message').last();
    await expect.poll(() => answer.locator('strong').textContent(), { timeout: 30_000 }).toBe('one file');
    await expect.poll(() => answer.locator('td').first().textContent()).toBe('README.md');
    // The list names the session by its first prompt.
    await page.getByRole('navigation').getByRole('button', { name: /What is in this project\?/ }).waitFor();

    // Done: the steps fold behind one line; opening it shows the lookups' row.
    const worked = page.getByTestId('worked');
    await worked.waitFor({ timeout: 30_000 });
    expect(await worked.textContent()).toMatch(/Worked for .* · 2 tool calls/);
    await worked.getByRole('button').click();
    expect(await page.getByTestId('lookups').textContent()).toContain('Read 1 file, listed 1 directory');

    // The full view shows each call.
    await page.getByRole('button', { name: 'Show details' }).click();
    expect(await page.getByTestId('tool-call').count()).toBe(2);
  });

  it('stops a running turn', async () => {
    const page = await session([{ text: 'word '.repeat(400), pieces: 400, delayMs: 50 }]);
    const prompt = page.getByRole('textbox', { name: 'Prompt' });
    await prompt.fill('Write a lot');
    await prompt.press('Enter');
    await page.getByTestId('working').waitFor({ timeout: 30_000 });
    await page.getByRole('button', { name: 'Stop' }).click();
    await page.getByTestId('turn-note').filter({ hasText: 'Stopped' }).waitFor({ timeout: 30_000 });
    expect(await page.getByTestId('working').count()).toBe(0);
  });
});
