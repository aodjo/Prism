import { defineConfig } from 'vitest/config';

/**
 * Vitest configuration for the repository root.
 *
 * Its only job is to keep the runner out of `.claude/worktrees`, where parallel agent runs
 * leave whole copies of the tree. Those copies contain their own protocol tests, written
 * against whatever the wire format was when the worktree was made, and a stale copy passing
 * is exactly the shape of a regression going unnoticed.
 */
export default defineConfig({
  test: {
    exclude: ['**/node_modules/**', '**/dist/**', '.claude/**'],
  },
});
