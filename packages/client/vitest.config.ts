import { defineConfig } from 'vitest/config';

/** Runs client regressions without including generated renderer bundles. */
export default defineConfig({ test: { include: ['test/**/*.test.ts'] } });
