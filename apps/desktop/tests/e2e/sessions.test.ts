// The session list, driven as a user would against a real `koda serve`:
// sessions another client makes appear live, grouped by folder; opening one
// shows where it works; a new one starts in the folder picked; deleting asks
// first. The system dialogs are answered from the test, in the main process.
import { mkdirSync, realpathSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { KodaClient } from '../../src/main/koda-client';
import { scratch } from '../helpers';
import { koda, type OpenApp, openApp, socketOf, shutDown } from './app';
import { type Model, scriptedModel, useModel } from './model';

let open: OpenApp | undefined;
let client: KodaClient | undefined;
let model: Model | undefined;
const homes: string[] = [];

afterEach(async () => {
  client?.close();
  client = undefined;
  await shutDown(open, homes.splice(0));
  open = undefined;
  model?.close();
  model = undefined;
});

/** A folder under `home`, by its real path (macOS's /var is /private/var). */
function folder(home: string, name: string): string {
  const dir = path.join(home, name);
  mkdirSync(dir, { recursive: true });
  return realpathSync(dir);
}

describe.skipIf(!koda)('the session list', () => {
  it('shows every folder’s sessions live, opens, starts and deletes them', async () => {
    const home = scratch();
    homes.push(home);
    model = await scriptedModel([]);
    useModel(home, model);
    open = await openApp(home);
    const { app, page } = open;
    await page.getByRole('button', { name: 'New session' }).waitFor({ timeout: 30_000 });
    await page.getByText('No sessions yet.').waitFor();

    // Another client (a TUI, say) starts two sessions in two folders.
    client = new KodaClient();
    await client.connect(socketOf(home));
    await client.request('initialize', { clientInfo: { name: 'other-client', version: '0' } });
    const alpha = folder(home, 'alpha');
    const beta = folder(home, 'beta');
    const a = (await client.request<{ session: { id: string } }>('session/start', { cwd: alpha })).session.id;
    await client.request('session/name/set', { sessionId: a, name: 'Alpha work' });
    const b = (await client.request<{ session: { id: string } }>('session/start', { cwd: beta })).session.id;
    await client.request('session/name/set', { sessionId: b, name: 'Beta work' });

    await page.getByRole('button', { name: /Beta work/ }).waitFor();
    await page.getByRole('button', { name: /Alpha work/ }).waitFor();
    const headings = await page.getByTestId('folder-group').locator('h2').allTextContents();
    expect(headings.sort()).toEqual(['alpha', 'beta']);

    // Opening one joins it and shows where it works.
    await page.getByRole('button', { name: /Alpha work/ }).click();
    await expect.poll(() => page.getByTestId('session-folder').textContent()).toBe(alpha);
    expect(await page.getByRole('heading', { level: 1 }).textContent()).toBe('Alpha work');

    // A new session, in the folder the user picks.
    const gamma = folder(home, 'gamma');
    await app.evaluate(({ dialog }, picked) => {
      dialog.showOpenDialog = (async () => ({ canceled: false, filePaths: [picked] })) as typeof dialog.showOpenDialog;
    }, gamma);
    // The new session offers the folder worked in last; "Open folder…" asks for another.
    await page.getByRole('button', { name: 'New session' }).click();
    await page.getByTestId('new-session').waitFor();
    expect(['alpha', 'beta']).toContain(await page.getByTestId('new-session-folder').textContent());
    await page.getByRole('button', { name: 'Folder' }).click();
    await page.getByRole('menuitem', { name: 'Open folder…' }).click();
    await expect.poll(() => page.getByTestId('new-session-folder').textContent()).toBe('gamma');
    // It starts with its first prompt, in that folder.
    expect(await page.getByRole('navigation').getByRole('button', { name: /Gamma work/ }).count()).toBe(0);
    const prompt = page.getByRole('textbox', { name: 'Prompt' });
    await prompt.fill('Gamma work');
    await prompt.press('Enter');
    await expect.poll(() => page.getByTestId('session-folder').textContent(), { timeout: 30_000 }).toBe(gamma);
    await page.getByTestId('agent-message').filter({ hasText: 'All done.' }).waitFor({ timeout: 30_000 });
    await expect.poll(async () => (await page.getByTestId('folder-group').locator('h2').allTextContents()).sort()).toEqual([
      'alpha',
      'beta',
      'gamma',
    ]);

    // Deleting asks first; "Cancel" keeps it, "Delete" removes it everywhere.
    await page.getByRole('button', { name: /Beta work/ }).click();
    await expect.poll(() => page.getByTestId('session-folder').textContent()).toBe(beta);
    const answer = async (response: number) =>
      app.evaluate(({ dialog }, r) => {
        const g = globalThis as { deleteDialogs?: number };
        dialog.showMessageBox = (async () => {
          g.deleteDialogs = (g.deleteDialogs ?? 0) + 1;
          return { response: r, checkboxChecked: false };
        }) as typeof dialog.showMessageBox;
      }, response);
    const asked = () => app.evaluate(() => (globalThis as { deleteDialogs?: number }).deleteDialogs ?? 0);
    await answer(1);
    await page.getByRole('button', { name: 'Session menu' }).click();
    await page.getByRole('menuitem', { name: 'Delete' }).click();
    // The dialog has been shown and answered "Cancel": only then does the wait
    // below say anything, and only then may the next answer replace this one.
    await expect.poll(asked, { timeout: 30_000 }).toBe(1);
    await new Promise((r) => setTimeout(r, 500));
    expect(await page.getByRole('button', { name: /Beta work/ }).count()).toBe(1);
    await answer(0);
    await page.getByRole('button', { name: 'Session menu' }).click();
    await page.getByRole('menuitem', { name: 'Delete' }).click();
    // Gone once the server has deleted it and said so: two requests, the dialog and a broadcast away.
    await expect.poll(() => page.getByRole('button', { name: /Beta work/ }).count(), { timeout: 30_000 }).toBe(0);
    // The pane it was in goes back to a new session.
    await page.getByTestId('new-session').waitFor();
    const list = await client.request<{ data: Array<{ id: string }> }>('session/list', {});
    expect(list.data.map((s) => s.id)).not.toContain(b);
  });
});
