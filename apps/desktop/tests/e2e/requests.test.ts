// What Koda waits on you for, answered in the window, against a real
// `koda serve` and a scripted model: an approval (yes, and no with what to
// do instead) and the model's question.
import { existsSync, mkdirSync, realpathSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { scratch } from '../helpers';
import { closeApp, koda, type OpenApp, openApp, stopServer } from './app';
import { type Model, type Reply, scriptedModel, useModel } from './model';

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

/** A new session in a project with `notes.txt`, its model scripted with `replies`, and `prompt` sent. */
async function turn(replies: Reply[], prompt: string) {
  const home = scratch();
  homes.push(home);
  model = await scriptedModel(replies);
  useModel(home, model);
  const project = path.join(home, 'project');
  mkdirSync(project);
  writeFileSync(path.join(project, 'notes.txt'), 'keep me\n');
  open = await openApp(home);
  const { app, page } = open;
  await page.getByRole('button', { name: 'New session' }).waitFor({ timeout: 30_000 });
  await app.evaluate(({ dialog }, picked) => {
    dialog.showOpenDialog = (async () => ({ canceled: false, filePaths: [picked] })) as typeof dialog.showOpenDialog;
  }, realpathSync(project));
  await page.getByRole('button', { name: 'New session' }).click();
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill(prompt);
  await box.press('Enter');
  return { page, notes: path.join(project, 'notes.txt') };
}

describe.skipIf(!koda)('requests', () => {
  it('asks before a forced rm, says so everywhere, and runs it on Yes', async () => {
    const { page, notes } = await turn([{ tool: 'Bash', args: { command: 'rm -f notes.txt' } }, { text: 'Removed it.' }], 'Remove the notes');
    const card = page.getByTestId('request');
    await card.waitFor({ timeout: 30_000 });
    expect(await card.textContent()).toContain('rm -f notes.txt');
    expect(await page.getByTestId('working').textContent()).toContain('Waiting for your approval');
    await expect.poll(() => page.getByRole('navigation').textContent()).toContain('needs you');
    expect(existsSync(notes)).toBe(true);

    await card.getByRole('button', { name: 'Yes', exact: true }).click();
    await page.getByTestId('agent-message').filter({ hasText: 'Removed it.' }).waitFor({ timeout: 30_000 });
    expect(existsSync(notes)).toBe(false);
    expect(await page.getByTestId('request').count()).toBe(0);
  });

  it('refuses with what to do instead, which reaches the model', async () => {
    const { page, notes } = await turn([{ tool: 'Bash', args: { command: 'rm -f notes.txt' } }, { text: 'Understood.' }], 'Remove the notes');
    const card = page.getByTestId('request');
    await card.waitFor({ timeout: 30_000 });
    await card.getByRole('button', { name: /tell Koda what to do instead/ }).click();
    await card.getByRole('textbox', { name: 'What Koda should do instead' }).fill('Move it to old/ instead');
    await card.getByRole('button', { name: 'Send' }).click();
    await page.getByTestId('agent-message').filter({ hasText: 'Understood.' }).waitFor({ timeout: 30_000 });
    expect(existsSync(notes)).toBe(true);
    expect(model!.bodies().at(-1)).toContain('Move it to old/ instead');
  });

  it("answers the model's question with a choice", async () => {
    const { page } = await turn(
      [{ tool: 'AskUser', args: { question: 'Which color?', options: ['red', 'blue'] } }, { text: 'Blue it is.' }],
      'Paint it',
    );
    const card = page.getByTestId('request');
    await card.waitFor({ timeout: 30_000 });
    expect(await card.textContent()).toContain('Which color?');
    expect(await page.getByTestId('working').textContent()).toContain('Waiting for your answer');
    await card.getByRole('button', { name: 'blue' }).click();
    await page.getByTestId('agent-message').filter({ hasText: 'Blue it is.' }).waitFor({ timeout: 30_000 });
    expect(model!.bodies().at(-1)).toContain('blue');
  });
});
