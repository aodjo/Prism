/**
 * What a machine shows while somebody is controlling it.
 *
 * A bar at the top of the screen saying who is in control, with the button that ends it. macOS
 * draws the same bar natively; this is the one for the platforms where the shell has only a web
 * view to draw with.
 *
 * It decides nothing. When the bar is up, and for whom, is the shell's to say — it is the shell
 * that knows a session is open and that the mouse here has just been touched — and it says so by
 * calling `window.__banner`, several times a second, with the whole of what should be showing.
 * Told rather than subscribed, so a page that finishes loading a moment after the session opened
 * is put right by the next call instead of having missed the one that mattered.
 */

import { StrictMode, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';

import type { PrismApi } from './api.js';
import { speak, t } from './i18n.js';

/** What the shell says should be showing. */
interface Said {
  /** Who is in control, by the name their account gives them. */
  readonly name: string;
  /** Whether the bar is up. */
  readonly shown: boolean;
}

declare global {
  interface Window {
    readonly prism: PrismApi;
    /** How the shell tells this page what to show. */
    __banner?: (said: Said) => void;
  }
}

const prism = window.prism;

/** How long the bar takes to appear, in milliseconds. Quick, because it is answering a hand. */
const APPEARING = 150;

/** How long the bar takes to fade, in milliseconds. */
const FADING = 800;

/**
 * The bar.
 *
 * @returns {JSX.Element} The bar, see-through when it is not up.
 */
function Banner(): JSX.Element {
  const [said, setSaid] = useState<Said>({ name: '', shown: false });

  useEffect(() => {
    window.__banner = setSaid;

    void prism
      .getSettings()
      .then((stored) => {
        speak(stored.language);
        setSaid((now) => ({ ...now }));
      })
      .catch(() => undefined);

    return () => {
      delete window.__banner;
    };
  }, []);

  return (
    <div
      className="flex h-full items-center gap-3 rounded-[12px] border border-white/12 bg-[rgba(26,26,26,0.95)] pr-3 pl-[18px]"
      style={{
        opacity: said.shown ? 1 : 0,
        transition: `opacity ${said.shown ? APPEARING : FADING}ms ease`,
      }}
    >
      <span className="min-w-0 flex-1 truncate text-[13px] text-ink">
        {t('{name} is controlling this machine', { name: said.name })}
      </span>
      <button
        type="button"
        className="btn-secondary"
        onClick={() => {
          void prism.disconnectViewer().catch(() => undefined);
        }}
      >
        {t('Disconnect')}
      </button>
    </div>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Banner />
  </StrictMode>,
);
