import { defineConfig } from 'vitest/config';

/** Runs account API regressions against an in-memory database. */
export default defineConfig({ test: { include: ['test/**/*.test.ts'] } });
