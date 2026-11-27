// The page: built by Vite into out/renderer, served to the window from app://koda/.
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  root: 'src/renderer',
  base: '/',
  plugins: [react()],
  build: {
    outDir: '../../out/renderer',
    emptyOutDir: true,
    target: 'chrome140',
    sourcemap: false,
    modulePreload: { polyfill: false },
  },
});
