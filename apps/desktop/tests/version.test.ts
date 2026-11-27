// The app runs Koda's version: the server refuses nothing for it, but the
// app uses only a server of its own version, so a release must bump both.
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { expect, it } from 'vitest';

it("has the workspace's version", () => {
  const app = JSON.parse(readFileSync(path.join(__dirname, '../package.json'), 'utf8')) as { version: string };
  const cargo = readFileSync(path.join(__dirname, '../../../Cargo.toml'), 'utf8');
  const workspace = /\[workspace\.package\][^[]*?\nversion = "([^"]+)"/.exec(cargo)?.[1];
  expect(workspace).toBeDefined();
  expect(app.version).toBe(workspace);
});
