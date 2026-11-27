// Electron's binary, for the tests that open the app: npm 11 no longer runs
// Electron's install script on its own, and CI skips the download for the
// jobs that don't need it.
import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const electron = join(dirname(fileURLToPath(import.meta.url)), '../node_modules/electron');
if (!existsSync(join(electron, 'dist', 'version')) || !existsSync(join(electron, 'path.txt'))) {
  execFileSync(process.execPath, [join(electron, 'install.js')], {
    stdio: 'inherit',
    env: { ...process.env, ELECTRON_SKIP_BINARY_DOWNLOAD: '' },
  });
}
