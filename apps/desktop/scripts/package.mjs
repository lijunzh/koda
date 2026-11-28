// Package the app as Koda.app (macOS, Apple Silicon), with the matching
// `koda` inside it, so the app and the server it starts are one version.
//
//   KODA_BIN=../../target/release/koda node scripts/package.mjs
//
// Writes release/Koda.app and release/Koda.app.zip. Electron's fuses are set as its security
// guidance asks for an app that runs no Node code of its own: no
// ELECTRON_RUN_AS_NODE, no NODE_OPTIONS, no --inspect, and the app loads
// only from its asar archive, whose integrity is checked. The app is signed
// ad hoc (no Apple Developer ID): it opens when it isn't marked as
// downloaded, as with a Homebrew formula or a local build.
import { flipFuses, FuseV1Options, FuseVersion } from '@electron/fuses';
import { packager } from '@electron/packager';
import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync } from 'node:fs';
import os from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

if (process.platform !== 'darwin' || process.arch !== 'arm64') {
  console.error('Koda.app is built on an Apple Silicon Mac.');
  process.exit(1);
}
const app = join(dirname(fileURLToPath(import.meta.url)), '..');
const koda = resolve(process.env.KODA_BIN ?? join(app, '../../target/release/koda'));
if (!existsSync(koda)) {
  console.error(`No koda at ${koda}: build it (cargo build --release -p koda-cli) or set KODA_BIN.`);
  process.exit(1);
}
const version = JSON.parse(readFileSync(join(app, 'package.json'), 'utf8')).version;
const kodaVersion = execFileSync(koda, ['--version'], { encoding: 'utf8' }).trim().split(/\s+/).pop();
if (kodaVersion !== version) {
  console.error(`koda at ${koda} is ${kodaVersion}; the app is ${version}. Koda runs one version.`);
  process.exit(1);
}

execFileSync(process.execPath, [join(app, 'scripts/build.mjs')], { cwd: app, stdio: 'inherit' });

// The app's Resources get bin/koda.
const staging = mkdtempSync(join(os.tmpdir(), 'koda-app-'));
mkdirSync(join(staging, 'bin'));
copyFileSync(koda, join(staging, 'bin', 'koda'));

const out = join(app, 'release');
rmSync(out, { recursive: true, force: true });
const [built] = await packager({
  dir: app,
  name: 'Koda',
  appBundleId: 'dev.koda.app',
  appCategoryType: 'public.app-category.developer-tools',
  appVersion: version,
  buildVersion: version,
  // Koda's own icon in place of Electron's (assets/icon.svg; scripts/make-icon.sh builds this).
  icon: join(app, 'assets', 'icon.icns'),
  platform: 'darwin',
  arch: 'arm64',
  out,
  asar: true,
  prune: false,
  overwrite: true,
  quiet: true,
  // Only the built app goes in: everything it runs is bundled into out/.
  ignore: (file) => file !== '' && !/^\/(out|package\.json)($|\/)/.test(file),
  extraResource: [join(staging, 'bin')],
});
rmSync(staging, { recursive: true, force: true });

const bundle = join(out, 'Koda.app');
renameSync(join(built, 'Koda.app'), bundle);
// Electron's and Chromium's licenses ship with the app.
mkdirSync(join(out, 'licenses'));
renameSync(join(built, 'LICENSE'), join(out, 'licenses', 'electron-LICENSE'));
renameSync(join(built, 'LICENSES.chromium.html'), join(out, 'licenses', 'LICENSES.chromium.html'));
rmSync(built, { recursive: true, force: true });

await flipFuses(join(bundle, 'Contents/MacOS/Koda'), {
  version: FuseVersion.V1,
  resetAdHocDarwinSignature: true,
  [FuseV1Options.RunAsNode]: false,
  [FuseV1Options.EnableNodeOptionsEnvironmentVariable]: false,
  [FuseV1Options.EnableNodeCliInspectArguments]: false,
  [FuseV1Options.OnlyLoadAppFromAsar]: true,
  [FuseV1Options.EnableEmbeddedAsarIntegrityValidation]: true,
  [FuseV1Options.EnableCookieEncryption]: true,
});

// One ad-hoc signature over the whole bundle, after everything in it changed.
execFileSync('codesign', ['--force', '--deep', '--sign', '-', bundle], { stdio: 'inherit' });
execFileSync('codesign', ['--verify', '--deep', '--strict', bundle], { stdio: 'inherit' });

// The release ships the app zipped: Homebrew rewrites the library IDs of
// every Mach-O file a formula installs, which breaks the app's signature,
// but leaves an archive alone. `koda app` unzips it into ~/Applications.
execFileSync('ditto', ['-c', '-k', '--sequesterRsrc', '--keepParent', bundle, join(out, 'Koda.app.zip')]);
console.log(`Built ${bundle} and Koda.app.zip (Koda ${version})`);
