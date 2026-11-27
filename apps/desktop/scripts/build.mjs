// Build the app into out/: the protocol's types, a type check, the page
// (Vite) and the main process and preload (esbuild, everything bundled, so
// the app needs no node_modules at run time). Packaging comes later.
import { build } from 'esbuild';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const app = join(dirname(fileURLToPath(import.meta.url)), '..');
const run = (command, args) => execFileSync(command, args, { cwd: app, stdio: 'inherit' });

run(process.execPath, ['scripts/gen-types.mjs']);
run(process.execPath, [join(app, 'node_modules/typescript/bin/tsc'), '-p', 'tsconfig.json']);
run(process.execPath, [join(app, 'node_modules/vite/bin/vite.js'), 'build', '--logLevel', 'warn']);
for (const [entry, outdir] of [
  ['src/main/main.ts', 'out/main'],
  ['src/preload/preload.ts', 'out/preload'],
]) {
  await build({
    absWorkingDir: app,
    entryPoints: [entry],
    outdir,
    bundle: true,
    platform: 'node',
    format: 'cjs',
    target: 'node22',
    external: ['electron'],
    logLevel: 'warning',
  });
}
