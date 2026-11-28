// Reviewing a turn's changes: the Changes panel shows the edit Koda made,
// line by line; a comment on a line goes back to Koda in one message with
// the others, as Claude Code's desktop app sends review comments.
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

describe.skipIf(!koda)('reviewing changes', () => {
  it('shows the edit and sends a comment on a line back to Koda', async () => {
    const home = scratch();
    homes.push(home);
    const project = path.join(home, 'project');
    mkdirSync(project);
    const file = path.join(realpathSync(project), 'greeting.txt');
    writeFileSync(file, 'one\nhello\nthree\n');
    model = await scriptedModel([
      { tool: 'Read', args: { file_path: file } },
      { tool: 'Edit', args: { file_path: file, old_str: 'hello', new_str: 'hello, world' } },
      { text: 'Changed the greeting.' },
      { text: 'Kept it short.' },
    ]);
    useModel(home, model);
    open = await openApp(home);
    const { app, page } = open;
    await page.getByRole('button', { name: 'New session' }).waitFor({ timeout: 30_000 });
    await app.evaluate(({ dialog }, picked) => {
      dialog.showOpenDialog = (async () => ({ canceled: false, filePaths: [picked] })) as typeof dialog.showOpenDialog;
    }, realpathSync(project));
    await page.getByRole('button', { name: 'New session' }).click();
    const prompt = page.getByRole('textbox', { name: 'Prompt' });
    await prompt.fill('Make the greeting friendlier');
    await prompt.press('Enter');
    // An edit may ask first, depending on the mode.
    const done = page.getByTestId('agent-message').filter({ hasText: 'Changed the greeting.' });
    const asked = page.getByTestId('request');
    await expect.poll(async () => (await done.count()) + (await asked.count()), { timeout: 30_000 }).toBeGreaterThan(0);
    if (await asked.count()) await asked.getByRole('button', { name: 'Yes', exact: true }).click();
    await done.waitFor({ timeout: 30_000 });

    await page.getByRole('button', { name: 'Changes' }).click();
    const review = page.getByTestId('review');
    await review.getByTestId('review-file').filter({ hasText: 'greeting.txt' }).waitFor({ timeout: 10_000 });
    const added = review.locator('.review-line.add');
    expect(await added.count()).toBe(1);
    expect(await added.textContent()).toContain('hello, world');
    expect(await review.locator('.review-line.del').textContent()).toContain('hello');

    // Unchanged lines above the change are folded to a count.
    expect(await review.locator('.review-skip').first().textContent()).toBe('\u00a0');

    // Hovering a line shows the "+" beside its number; Enter adds the comment.
    await added.hover();
    await added.getByRole('button', { name: 'Comment on line 2' }).click();
    const comment = review.getByRole('textbox', { name: 'Comment' });
    await comment.fill('Drop the comma');
    await comment.press('Enter');
    expect(await review.getByTestId('review-comment').textContent()).toContain('Drop the comma');
    await review.getByRole('button', { name: /Send 1 comment/ }).waitFor();
    // Cmd+Enter sends every comment.
    await page.keyboard.press('Meta+Enter');

    await page.getByTestId('agent-message').filter({ hasText: 'Kept it short.' }).waitFor({ timeout: 30_000 });
    const sent = page.getByTestId('user-message').last();
    expect(await sent.textContent()).toContain('Comments on your changes');
    expect(model.bodies().at(-1)).toContain('greeting.txt:2');
    expect(model.bodies().at(-1)).toContain('Drop the comma');
    expect(await review.getByTestId('review-comment').count()).toBe(0);
  });
});
