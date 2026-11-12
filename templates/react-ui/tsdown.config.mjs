import css from '@tsdown/css';
import { defineConfig } from 'tsdown';

export default defineConfig({
  entry: ['src/index.ts'],
  format: ['esm'],
  dts: { isolatedDeclarations: true },
  clean: true,
  plugins: [css()],
  deps: {
    neverBundle: [/^[^./]/],
  },
});
