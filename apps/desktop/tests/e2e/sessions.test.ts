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

let open: OpenApp | undefined;
let client: KodaClient | undefined;
const homes: string[] = [];

afterEach(async () => {
  client?.close();
  client = undefined;
  await shutDown(open, homes.splice(0));
  open = undefined;
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
    await page.getByRole('button', { name: 'New session' }).click();
    await expect.poll(() => page.getByTestId('session-folder').textContent()).toBe(gamma);
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
        dialog.showMessageBox = (async () => ({ response: r, checkboxChecked: false })) as typeof dialog.showMessageBox;
      }, response);
    await answer(1);
    await page.getByRole('button', { name: 'Delete' }).click();
    await new Promise((r) => setTimeout(r, 500));
    expect(await page.getByRole('button', { name: /Beta work/ }).count()).toBe(1);
    await answer(0);
    await page.getByRole('button', { name: 'Delete' }).click();
    await expect.poll(() => page.getByRole('button', { name: /Beta work/ }).count()).toBe(0);
    await page.getByText('Open a session from the list').waitFor();
    const list = await client.request<{ data: Array<{ id: string }> }>('session/list', {});
    expect(list.data.map((s) => s.id)).not.toContain(b);
  });
});
