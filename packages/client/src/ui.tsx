/**
 * The pieces both windows are built from.
 *
 * The aurora backdrop is assembled from SVGs exported from the design rather than reproduced
 * with gradients. They are large blurred shapes whose falloff is the whole effect, and a
 * hand-made approximation of one reads as a different product.
 */

import type { JSX, ReactNode } from 'react';

/** One blurred shape in the backdrop, placed as a share of the window. */
export interface BlobShape {
  /** The file under `assets/`. */
  readonly src: string;
  /** Where its own bounds sit, as Tailwind arbitrary values. */
  readonly box: string;
  /** How far the blur spills past those bounds, which is what the export expects. */
  readonly bleed: string;
}

/** The four shapes and the horizon behind the welcome screen. */
export const WELCOME_SKY: readonly BlobShape[] = [
  {
    src: 'aurora-w1.svg',
    box: 'left-[14.58%] top-[13.33%] w-[70.83%] h-[95.56%]',
    bleed: 'inset-y-[-13.95%] inset-x-[-11.76%]',
  },
  {
    src: 'aurora-w2.svg',
    box: 'left-[-12.5%] top-[26.67%] w-[55.56%] h-[82.22%]',
    bleed: 'inset-y-[-20.27%] inset-x-[-18.75%]',
  },
  {
    src: 'aurora-w3.svg',
    box: 'left-[57.64%] top-[28.89%] w-[59.72%] h-[84.44%]',
    bleed: 'inset-y-[-19.74%] inset-x-[-17.44%]',
  },
  {
    src: 'aurora-w4.svg',
    box: 'left-[26.39%] top-[51.11%] w-[51.39%] h-[62.22%]',
    bleed: 'inset-y-[-32.14%] inset-x-[-24.32%]',
  },
  {
    src: 'aurora-horizon.svg',
    box: 'left-[9.03%] top-[47.78%] w-[81.94%] h-[33.33%]',
    bleed: 'inset-y-[-36.67%] inset-x-[-9.32%]',
  },
];

/** The calmer three behind every screen after it. */
export const STEP_SKY: readonly BlobShape[] = [
  {
    src: 'aurora-violet.svg',
    box: 'left-[16.67%] top-[21.11%] w-[67.36%] h-[90%]',
    bleed: 'inset-y-[-20.37%] inset-x-[-17.01%]',
  },
  {
    src: 'aurora-cyan.svg',
    box: 'left-[-11.81%] top-[33.33%] w-[54.86%] h-[78.89%]',
    bleed: 'inset-y-[-26.06%] inset-x-[-23.42%]',
  },
  {
    src: 'aurora-rose.svg',
    box: 'left-[56.94%] top-[27.78%] w-[59.03%] h-[83.33%]',
    bleed: 'inset-y-[-24.67%] inset-x-[-21.76%]',
  },
];

/** The two behind the home window, which has content edge to edge and no vignette. */
export const HOME_SKY: readonly BlobShape[] = [
  {
    src: 'aurora-home1.svg',
    box: 'left-[33.33%] top-[-46.67%] w-[69.44%] h-[80%]',
    bleed: 'inset-y-[-33.33%] inset-x-[-24%]',
  },
  {
    src: 'aurora-home2.svg',
    box: 'left-[-20.83%] top-[62.22%] w-[55.56%] h-[71.11%]',
    bleed: 'inset-y-[-39.06%] inset-x-[-31.25%]',
  },
];

/**
 * Draws the aurora behind a window.
 *
 * Positioned in shares of the window rather than the pixels the design was drawn at, so the
 * composition holds together at any size instead of only at 1440 × 900.
 *
 * @param {object} props - What to draw.
 * @param {readonly BlobShape[]} props.sky - Which arrangement of shapes.
 * @param {boolean} [props.vignette] - Whether the light falls off at the edges.
 * @returns {JSX.Element} The backdrop.
 */
export function Backdrop({
  sky,
  vignette = false,
}: {
  sky: readonly BlobShape[];
  vignette?: boolean;
}): JSX.Element {
  return (
    <div className="dither pointer-events-none fixed inset-0 z-0 overflow-hidden">
      {sky.map((blob) => (
        <div key={blob.src} className={`absolute mix-blend-screen ${blob.box}`}>
          <img src={`assets/${blob.src}`} alt="" className={`absolute block max-w-none ${blob.bleed}`} />
        </div>
      ))}
      {vignette && <div className="vignette absolute inset-0" />}
    </div>
  );
}

/**
 * The product's name, set the way the design sets it.
 *
 * @param {object} props - What to draw.
 * @param {'lg' | 'sm'} [props.size] - `lg` in the setup flow, `sm` in the home sidebar.
 * @returns {JSX.Element} The wordmark.
 */
export function Wordmark({ size = 'lg' }: { size?: 'lg' | 'sm' }): JSX.Element {
  const large = size === 'lg';

  return (
    <div className="flex items-center gap-2.5">
      <img
        src={large ? 'assets/mark-01.svg' : 'assets/mark-home.svg'}
        alt=""
        className={large ? 'block h-[10px] w-[11.71px]' : 'block h-[13px] w-[15px]'}
      />
      <span
        className={
          large
            ? 'text-[15px] font-semibold tracking-[3.6px] text-ink'
            : 'text-note font-semibold tracking-[3px] text-ink'
        }
      >
        PRISM
      </span>
    </div>
  );
}

/**
 * The light button that carries the one action a screen is for.
 *
 * @param {object} props - What to draw.
 * @param {ReactNode} props.children - The label.
 * @param {string} [props.trailing] - The key or arrow shown in a chip after it.
 * @param {boolean} [props.small] - The home window's size rather than the flow's.
 * @param {boolean} [props.disabled] - Whether it can be pressed.
 * @param {() => void} props.onClick - What it does.
 * @returns {JSX.Element} The button.
 */
export function Primary({
  children,
  trailing,
  small = false,
  disabled = false,
  onClick,
}: {
  children: ReactNode;
  trailing?: string;
  small?: boolean;
  disabled?: boolean;
  onClick: () => void;
}): JSX.Element {
  return (
    <button
      type="button"
      className={small ? 'btn-primary-sm no-drag' : 'btn-primary no-drag'}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
      {trailing !== undefined && <span className="btn-trailing">{trailing}</span>}
    </button>
  );
}

/**
 * Reports a failure, or nothing when there is none.
 *
 * @param {object} props - What to draw.
 * @param {string | null} props.message - What went wrong.
 * @param {string} [props.className] - Anything the surrounding screen wants to add.
 * @returns {JSX.Element | null} The line, or nothing.
 */
export function Trouble({
  message,
  className = '',
}: {
  message: string | null;
  className?: string;
}): JSX.Element | null {
  if (message === null) {
    return null;
  }

  return (
    <p className={`whitespace-pre-wrap text-note-2 text-danger-ink ${className}`}>{message}</p>
  );
}

/**
 * Turns whatever was thrown into the sentence somebody should read.
 *
 * @param {unknown} error - Whatever it was.
 * @returns {string} The message.
 */
export function reason(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }

  return typeof error === 'string' ? error : String(error);
}

/**
 * Shortens a public key to something a person can compare at a glance.
 *
 * @param {string} key - The key, as hex.
 * @returns {string} The first and last few characters.
 */
export function short(key: string): string {
  return key.length <= 16 ? key : `${key.slice(0, 6)}…${key.slice(-6)}`;
}
