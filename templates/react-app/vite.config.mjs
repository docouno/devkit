import { env } from 'node:process';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';
import RunnableTestsReporter from '@docouno/env/vitest-reporter';

export default defineConfig({
  plugins: [react()],
  cacheDir: 'node_modules/.cache/devkit-vite',
  server: {
    host: '0.0.0.0',
    port: 5173,
    strictPort: true,
    ...(env.DEVKIT_API_TARGET ? { proxy: { '/api': env.DEVKIT_API_TARGET } } : {}),
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.{ts,tsx}'],
    globals: false,
    passWithNoTests: false,
    reporters: ['default', new RunnableTestsReporter()],
    restoreMocks: true,
    clearMocks: true,
    maxWorkers: 2,
  },
});
