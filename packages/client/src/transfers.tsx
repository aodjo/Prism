/**
 * The files moving between this machine and the one being watched.
 *
 * A window of its own, and that is the point of it. Files used to be a menu inside the stream
 * window — which meant choosing one covered the desktop somebody was working in, and watching a
 * large file move meant leaving that menu open over it. What is moving belongs beside the
 * picture rather than on top of it.
 *
 * It holds nothing. Every row here is read from the stream state the shell announces, so a
 * window opened halfway through a transfer shows the same thing as one that was open all along,
 * and closing it stops nothing.
 */

import { StrictMode, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';

import type { MovingFile, PrismApi, StreamState } from './api.js';
import { bytes } from './format.js';
import { t } from './i18n.js';
import { Backdrop, HOME_SKY, Trouble, Wordmark, reason } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** Nothing is moving, because nothing is connected. */
const NOTHING: StreamState = {
  phase: 'idle',
  host: null,
  terms: null,
  stats: null,
  departed: null,
  log: [],
  moving: [],
  offered: [],
  offeredMore: false,
  arrived: [],
};

/**
 * How far along a transfer is, as a percentage.
 *
 * A file of no bytes is finished the moment it is offered, and dividing by its size would
 * otherwise put a bar at `NaN%`.
 *
 * @param {MovingFile} file - The transfer.
 * @returns {number} Between 0 and 100.
 */
function portion(file: MovingFile): number {
  if (file.size === 0) {
    return 100;
  }

  return Math.min(100, Math.round((file.moved / file.size) * 100));
}

/**
 * One file on its way, with a bar that fills as it moves.
 *
 * @param {object} props - The component's properties.
 * @param {MovingFile} props.file - What is moving.
 * @returns {JSX.Element} The row.
 */
function Row({ file }: { file: MovingFile }): JSX.Element {
  const share = portion(file);

  return (
    <li className="border-t border-line-1 px-4 py-3 first:border-t-0">
      <div className="flex items-baseline gap-3">
        <span className="min-w-0 flex-1 truncate text-row text-ink" title={file.name}>
          {file.name}
        </span>
        <span className="flex-none text-note text-muted-2 tabular-nums">
          {file.done ? bytes(file.size) : `${bytes(file.moved)} / ${bytes(file.size)}`}
        </span>
      </div>

      <div className="mt-2 flex items-center gap-3">
        <span
          className={`flex-none rounded-badge border px-2 py-0.5 text-label tracking-[1.4px] uppercase ${
            file.sending
              ? 'border-[rgba(53,214,255,0.28)] bg-[rgba(53,214,255,0.16)] text-cyan'
              : 'border-[rgba(77,232,176,0.28)] bg-[rgba(77,232,176,0.16)] text-mint'
          }`}
        >
          {t(file.sending ? 'Sending' : 'Receiving')}
        </span>

        <span className="h-1.5 min-w-0 flex-1 overflow-hidden rounded-pill bg-wash-2">
          <span
            className={`block h-full rounded-pill ${file.sending ? 'bg-cyan' : 'bg-mint'}`}
            style={{ width: `${share}%` }}
          />
        </span>

        <span className="w-10 flex-none text-right text-note text-dim tabular-nums">
          {file.done ? t('Done') : `${share}%`}
        </span>
      </div>
    </li>
  );
}

/**
 * The window.
 *
 * @returns {JSX.Element} Everything in it.
 */
function Transfers(): JSX.Element {
  const [stream, setStream] = useState<StreamState>(NOTHING);
  const [trouble, setTrouble] = useState<string | null>(null);

  useEffect(() => {
    void (async () => {
      try {
        setStream(await prism.streamState());
      } catch (error: unknown) {
        setTrouble(reason(error));
      }
    })();

    prism.onStream(setStream);
  }, []);

  const connected = stream.phase === 'streaming';

  /**
   * Asks the far machine what it has, reporting a refusal where somebody will see it.
   *
   * @returns {void}
   */
  const ask = (): void => {
    setTrouble(null);
    void prism.askListing().catch((error: unknown) => {
      setTrouble(reason(error));
    });
  };

  /**
   * Puts the chooser up, and lets the stream offer whatever comes back.
   *
   * @returns {void}
   */
  const send = (): void => {
    setTrouble(null);
    void prism.chooseFile().catch((error: unknown) => {
      setTrouble(reason(error));
    });
  };

  /**
   * Asks for one of the files the far machine named.
   *
   * @param {string} name - The file, as its listing gave it.
   * @returns {void}
   */
  const fetch = (name: string): void => {
    setTrouble(null);
    void prism.fetchFile(name).catch((error: unknown) => {
      setTrouble(reason(error));
    });
  };

  return (
    <>
      <Backdrop sky={HOME_SKY} />

      <div className="relative z-[1] flex h-full flex-col">
        <div data-tauri-drag-region className="h-[34px] flex-none" />

        <header className="flex-none px-5 pb-4">
          <Wordmark size="sm" />
          <div className="mt-3 flex items-end gap-3">
            <div className="min-w-0 flex-1">
              <h1 className="m-0 text-heading font-semibold text-ink">{t('Files')}</h1>
              <p className="mt-1 mb-0 text-note text-dim">
                {connected
                  ? t('Files move between this machine and the one you are watching.')
                  : t('Nothing is connected.')}
              </p>
            </div>

            <button
              type="button"
              className="btn-primary-sm flex-none"
              disabled={!connected}
              onClick={send}
            >
              {t('Send a file')}
            </button>
          </div>
        </header>

        <div className="min-h-0 flex-1 overflow-y-auto px-5 pb-5">
          <Trouble message={trouble} className="mb-3" />

          <section className="card">
            <h2 className="border-b border-line-1 px-4 py-3 text-label tracking-[1.4px] text-muted-2 uppercase">
              {t('Moving now')}
            </h2>

            {stream.moving.length === 0 ? (
              <p className="m-0 px-4 py-5 text-note text-dim">{t('Nothing is moving.')}</p>
            ) : (
              <ul className="m-0 list-none p-0">
                {stream.moving.map((file) => (
                  <Row key={`${file.sending ? 'up' : 'down'}:${file.name}`} file={file} />
                ))}
              </ul>
            )}
          </section>

          <section className="card mt-4">
            <div className="flex items-center gap-3 border-b border-line-1 px-4 py-3">
              <h2 className="m-0 flex-1 text-label tracking-[1.4px] text-muted-2 uppercase">
                {t('On the other machine')}
              </h2>
              <button type="button" className="btn-secondary" disabled={!connected} onClick={ask}>
                {t('Refresh')}
              </button>
            </div>

            {stream.offered.length === 0 ? (
              <p className="m-0 px-4 py-5 text-note text-dim">
                {connected ? t('Nothing listed yet.') : t('Connect to see what it offers.')}
              </p>
            ) : (
              <ul className="m-0 list-none p-0">
                {stream.offered.map((file) => (
                  <li
                    key={file.name}
                    className="flex items-center gap-3 border-t border-line-1 px-4 py-3 first:border-t-0"
                  >
                    <span className="min-w-0 flex-1 truncate text-row text-ink" title={file.name}>
                      {file.name}
                    </span>
                    <span className="flex-none text-note text-muted-2 tabular-nums">
                      {bytes(file.size)}
                    </span>
                    <button
                      type="button"
                      className="btn-secondary"
                      onClick={() => {
                        fetch(file.name);
                      }}
                    >
                      {t('Get')}
                    </button>
                  </li>
                ))}
              </ul>
            )}

            {stream.offeredMore && (
              <p className="m-0 border-t border-line-1 px-4 py-3 text-note text-dim">
                {t('It has more than fits here.')}
              </p>
            )}
          </section>

          {stream.arrived.length > 0 && (
            <section className="card mt-4">
              <h2 className="border-b border-line-1 px-4 py-3 text-label tracking-[1.4px] text-muted-2 uppercase">
                {t('Arrived')}
              </h2>

              <ul className="m-0 list-none p-0">
                {stream.arrived.map((file) => (
                  <li
                    key={file.path}
                    className="border-t border-line-1 px-4 py-3 first:border-t-0"
                  >
                    <div className="truncate text-row text-ink" title={file.name}>
                      {file.name}
                    </div>
                    <div className="mt-1 truncate text-fine text-dim select-text" title={file.path}>
                      {file.path}
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          )}
        </div>
      </div>
    </>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Transfers />
  </StrictMode>,
);
