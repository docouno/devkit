import RunnableTestsReporter from '@docouno/env/vitest-reporter';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  cacheDir: 'node_modules/.cache/devkit-vitest',
  resolve: { dedupe: ['react', 'react-dom'] },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.{ts,tsx}', 'test/**/*.test.{ts,tsx}'],
    globals: false,
    passWithNoTests: false,
    reporters: ['default', new RunnableTestsReporter()],
    restoreMocks: true,
    clearMocks: true,
    maxWorkers: 2,
  },
});
