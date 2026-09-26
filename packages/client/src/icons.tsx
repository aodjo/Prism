/**
 * The small shapes the window points at things with.
 *
 * Drawn inline for the same reason the operating-system marks are: they take the colour of the
 * text beside them, and a file behind `src` resolves `currentColor` against its own document.
 */

import type { JSX } from 'react';

/** How big to draw one, and nothing else — the colour comes from the text around it. */
interface Glyph {
  /** How many pixels across. */
  readonly size?: number;
}

/**
 * Two letters, one of them from another alphabet: the language this window is in.
 *
 * @param {Glyph} props - How big to draw it.
 * @returns {JSX.Element} The glyph.
 */
export function Language({ size = 16 }: Glyph): JSX.Element {
  return (
    <svg
      viewBox="0 0 512 512"
      width={size}
      height={size}
      fill="currentColor"
      aria-hidden
      className="block flex-none"
    >
      <path d="m478.33 433.6-90-218a22 22 0 0 0-40.67 0l-90 218a22 22 0 1 0 40.67 16.79L316.66 406h102.67l18.33 44.39A22 22 0 0 0 458 464a22 22 0 0 0 20.32-30.4zM334.83 362 368 281.65 401.17 362zm-66.99-19.08a22 22 0 0 0-4.89-30.7c-.2-.15-15-11.13-36.49-34.73 39.65-53.68 62.11-114.75 71.27-143.49H330a22 22 0 0 0 0-44H214V70a22 22 0 0 0-44 0v20H54a22 22 0 0 0 0 44h197.25c-9.52 26.95-27.05 69.5-53.79 108.36-31.41-41.68-43.08-68.65-43.17-68.87a22 22 0 0 0-40.58 17c.58 1.38 14.55 34.23 52.86 83.93.92 1.19 1.83 2.35 2.74 3.51-39.24 44.35-77.74 71.86-93.85 80.74a22 22 0 1 0 21.07 38.63c2.16-1.18 48.6-26.89 101.63-85.59 22.52 24.08 38 35.44 38.93 36.1a22 22 0 0 0 30.75-4.9z" />
    </svg>
  );
}

/**
 * The cog everything else is behind.
 *
 * @param {Glyph} props - How big to draw it.
 * @returns {JSX.Element} The glyph.
 */
export function Gear({ size = 16 }: Glyph): JSX.Element {
  return (
    <svg
      viewBox="0 0 512 512"
      width={size}
      height={size}
      fill="currentColor"
      aria-hidden
      className="block flex-none"
    >
      <path d="M256 176a80 80 0 1 0 80 80 80.24 80.24 0 0 0-80-80m172.72 80a165.5 165.5 0 0 1-1.64 22.34l48.69 38.12a11.59 11.59 0 0 1 2.63 14.78l-46.06 79.52a11.64 11.64 0 0 1-14.14 4.93l-57.25-23a176.6 176.6 0 0 1-38.82 22.67l-8.56 60.78a11.93 11.93 0 0 1-11.51 9.86h-92.12a12 12 0 0 1-11.51-9.53l-8.56-60.78A169.3 169.3 0 0 1 151.05 393L93.8 416a11.64 11.64 0 0 1-14.14-4.92L33.6 331.57a11.59 11.59 0 0 1 2.63-14.78l48.69-38.12A175 175 0 0 1 83.28 256a165.5 165.5 0 0 1 1.64-22.34l-48.69-38.12a11.59 11.59 0 0 1-2.63-14.78l46.06-79.52a11.64 11.64 0 0 1 14.14-4.93l57.25 23a176.6 176.6 0 0 1 38.82-22.67l8.56-60.78A11.93 11.93 0 0 1 209.94 26h92.12a12 12 0 0 1 11.51 9.53l8.56 60.78A169.3 169.3 0 0 1 361 119l57.2-23a11.64 11.64 0 0 1 14.14 4.92l46.06 79.52a11.59 11.59 0 0 1-2.63 14.78l-48.69 38.12a175 175 0 0 1 1.64 22.66" />
    </svg>
  );
}

/**
 * A door with an arrow through it: leaving the account.
 *
 * @param {Glyph} props - How big to draw it.
 * @returns {JSX.Element} The glyph.
 */
export function SignOut({ size = 16 }: Glyph): JSX.Element {
  return (
    <svg
      viewBox="0 0 512 512"
      width={size}
      height={size}
      fill="currentColor"
      aria-hidden
      className="block flex-none"
    >
      <path d="M160 256a16 16 0 0 1 16-16h144V136c0-32-33.79-56-64-56H104a56.06 56.06 0 0 0-56 56v240a56.06 56.06 0 0 0 56 56h160a56.06 56.06 0 0 0 56-56V272H176a16 16 0 0 1-16-16m299.31-11.31-80-80a16 16 0 0 0-22.62 22.62L409.37 240H320v32h89.37l-52.68 52.69a16 16 0 1 0 22.62 22.62l80-80a16 16 0 0 0 0-22.62" />
    </svg>
  );
}

/**
 * Four panes: the board, and so the way into arranging it.
 *
 * The blocks on the home screen, shrunk. Nothing in an icon set says "rearrange this screen" as
 * plainly as a small picture of the screen being rearranged.
 *
 * @param {Glyph} props - How big to draw it.
 * @returns {JSX.Element} The glyph.
 */
export function Panes({ size = 16 }: Glyph): JSX.Element {
  const gap = size * 0.16;
  const pane = (size - gap) / 2;

  return (
    <svg
      viewBox={`0 0 ${size} ${size}`}
      width={size}
      height={size}
      fill="currentColor"
      aria-hidden
      className="block flex-none"
    >
      <rect x={0} y={0} width={pane} height={pane} rx={1.5} />
      <rect x={pane + gap} y={0} width={pane} height={pane} rx={1.5} />
      <rect x={0} y={pane + gap} width={pane} height={pane} rx={1.5} />
      <rect x={pane + gap} y={pane + gap} width={pane} height={pane} rx={1.5} />
    </svg>
  );
}
