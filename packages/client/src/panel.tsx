/**
 * The settings, in a window of their own.
 *
 * Kept for the one case the sheet cannot serve: a screenshot of the settings taken without the
 * home window behind them, which is how this project checks what it drew. Everyday use opens
 * them over the home window instead, because a second window for four rows of switches is a
 * second thing to find and close.
 */

import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

import type { PrismApi } from './api.js';
import { Preferences } from './preferences.js';
import { Backdrop, HOME_SKY, Wordmark } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Backdrop sky={HOME_SKY} />

    <div className="relative z-[1]">
      <div className="drag h-[34px]" />

      <header className="px-5 pb-4">
        <Wordmark size="sm" />
      </header>

      {/* The same white panel the sheet draws, so that a screenshot taken here is a picture of
          what the settings actually look like rather than of a second design. */}
      <div className="on-white mx-4 mb-4 overflow-hidden rounded-card">
        <Preferences onResize={prism.fit} />
      </div>
    </div>
  </StrictMode>,
);
