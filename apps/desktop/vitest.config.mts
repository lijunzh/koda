// Unit tests run in Node: the socket client, the relay's checks, and, when
// KODA_BIN names a koda binary, a real `koda serve` on a scratch socket.
import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['tests/*.test.ts'],
    environment: 'node',
    testTimeout: 30_000,
  },
});
