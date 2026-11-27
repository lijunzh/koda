// The app itself, driven with Playwright against a real `koda serve` on a
// scratch config folder. Needs a built app (`npm run build`), Electron's
// binary, and KODA_BIN naming a koda built from this checkout:
//   KODA_BIN=../../target/debug/koda npm run test:e2e
import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['tests/e2e/*.test.ts'],
    environment: 'node',
    testTimeout: 90_000,
    hookTimeout: 60_000,
    // One app at a time: each test owns a window and a server.
    fileParallelism: false,
  },
});
