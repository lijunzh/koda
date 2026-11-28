// The conversation, driven as a user would against a real `koda serve` and
// a scripted model: a prompt runs a turn whose lookups fold into one row and
// whose answer streams in as Markdown; the turn's steps stay once it's done,
// as in Claude Code; Stop ends a running turn.
import { mkdirSync, realpathSync, writeFileSync } from 'node:fs';
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
  it('runs a turn: lookups fold to one row, the answer streams as Markdown, the steps stay', async () => {
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

    // Done: the steps stay, as in Claude Code; the lookups are one row.
    await expect.poll(() => page.getByTestId('lookups').textContent(), { timeout: 30_000 }).toContain('Read 1 file, listed 1 directory');
    expect(await page.getByTestId('worked').count()).toBe(0);
    // Clicking the row opens it to its calls and what they printed.
    await page.getByTestId('lookups').getByRole('button').first().click();
    const opened = page.getByTestId('lookup-calls');
    await expect.poll(() => opened.getByTestId('tool-call').count()).toBe(2);
    expect(await opened.textContent()).toContain('README.md');
    expect(await opened.textContent()).toContain('# Demo');
    // Opening the row opens none of its calls' longer output.
    expect(await opened.getByRole('button', { name: 'Show less' }).count()).toBe(0);
    await page.getByTestId('lookups').getByRole('button').first().click();
    expect(await page.getByTestId('lookup-calls').count()).toBe(0);

    // Verbose shows each call; Ctrl+O cycles Normal, Thinking, Verbose.
    // The session's menu holds the views: Verbose shows each call; Ctrl+O cycles Normal, Thinking, Verbose.
    await page.getByRole('button', { name: 'Session menu' }).click();
    await page.getByRole('menuitem', { name: /Transcript view/ }).click();
    await page.getByRole('menuitemradio', { name: 'Verbose' }).click();
    expect(await page.getByTestId('tool-call').count()).toBe(2);
    await page.keyboard.press('Control+O');
    expect(await page.getByTestId('session-view').getAttribute('data-view')).toBe('normal');
    expect(await page.getByTestId('tool-call').count()).toBe(0);
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
