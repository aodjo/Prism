/**
 * The tray panel.
 *
 * Whether this machine is hosting is the whole question somebody opens this for, so that is
 * the one loud thing on it; everything under it is a setting. It shares the client's design
 * system, because these are two halves of one product and a person runs both.
 *
 * Everything it can do comes from `window.prism`, which the preload defines. There is no Node
 * in this context and no way to reach a key.
 */

import { StrictMode, useEffect, useRef, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { createRoot } from 'react-dom/client';

import type { HostPermissions, HostSnapshot, PrismApi, Settings } from './api.js';

declare global {
  interface Window {
    /** The surface the preload exposes. */
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** The phases in which a session is running and the button should offer to stop it. */
const RUNNING: ReadonlySet<string> = new Set(['opening', 'waiting', 'streaming']);

/** How long the word "Paired" stays on screen before the code is put away. */
const PAIRED_LINGER_MS = 1500;

/**
 * What each phase is called in the panel.
 *
 * Written out rather than derived from the phase string, because these are the words a person
 * reads and they should not change because a name changed in Rust.
 */
const PHASE_LABELS: Record<string, string> = {
  opening: 'Opening',
  waiting: 'Waiting for a client',
  streaming: 'Streaming',
  stopped: 'Not hosting',
  failed: 'Failed',
  idle: 'Not hosting',
};

/** The colour each phase paints the state band's edge. */
const PHASE_EDGE: Record<string, string> = {
  opening: 'border-l-cyan bg-[rgba(53,214,255,0.09)]',
  waiting: 'border-l-cyan bg-[rgba(53,214,255,0.09)]',
  streaming: 'border-l-mint bg-[rgba(77,232,176,0.09)]',
  failed: 'border-l-danger bg-[rgba(255,92,110,0.09)]',
};

/** What an idle machine's band looks like, which is as quiet as the rest of the panel. */
const IDLE_EDGE = 'border-l-dim-2 bg-wash-1';

/**
 * The colour the grip carries for each phase.
 *
 * The whole reason the shelf is at the edge of the screen rather than in the menu bar: a
 * glance at the grip says whether this machine is still handing its screen to somebody, with
 * nothing to open and nothing to read.
 */
const GRIP_TONE: Record<string, string> = {
  opening: 'text-cyan',
  waiting: 'text-cyan',
  streaming: 'text-mint',
  failed: 'text-danger-ink',
};

/**
 * Shortens a public key for display.
 *
 * A key is sixty-four hex characters and nobody reads all of them. The first and last six are
 * enough to tell two machines apart at a glance and to check against what the other end shows.
 *
 * @param {string} key - The key as hex.
 * @returns {string} An abbreviated form, or the key itself when it is already short.
 */
function short(key: string): string {
  return key.length <= 16 ? key : `${key.slice(0, 6)}…${key.slice(-6)}`;
}

/**
 * Turns whatever was thrown into the sentence somebody should read.
 *
 * @param {unknown} error - Whatever it was.
 * @returns {string} The message.
 */
function reason(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }

  return typeof error === 'string' ? error : String(error);
}

/**
 * Formats a rate for display.
 *
 * @param {bigint} bps - Bits per second.
 * @returns {string} Megabits a second, to one decimal.
 */
function rate(bps: bigint): string {
  return `${(Number(bps) / 1e6).toFixed(1)} Mbps`;
}

/**
 * A band of the panel, separated from the next by a hairline and nothing else.
 *
 * @param {object} props - What to draw.
 * @param {string} props.title - What the band is for.
 * @param {ReactNode} props.children - What is in it.
 * @returns {JSX.Element} The band.
 */
function Band({ title, children }: { title: string; children: ReactNode }): JSX.Element {
  return (
    <section className="border-b border-line-1 px-4 py-3.5 last:border-b-0">
      <h2 className="m-0 mb-2 text-note font-semibold text-ink-3">{title}</h2>
      {children}
    </section>
  );
}

/**
 * A labelled value or control on its own line.
 *
 * @param {object} props - What to draw.
 * @param {ReactNode} props.label - What it is.
 * @param {ReactNode} props.children - The value or control.
 * @returns {JSX.Element} The row.
 */
function Row({ label, children }: { label: ReactNode; children: ReactNode }): JSX.Element {
  return (
    <div className="flex min-h-[26px] items-center justify-between gap-2.5">
      <span className="text-note-2 text-muted">{label}</span>
      {children}
    </div>
  );
}

/** The one shape every box in this panel has. */
const FIELD =
  'w-[150px] rounded-tile border border-line-2 bg-base px-2 py-1.5 text-fine text-ink placeholder:text-dim focus:border-[rgba(124,92,255,0.6)] focus:outline-none';

/**
 * The tray panel.
 *
 * @returns {JSX.Element} The whole of it.
 */
function Panel(): JSX.Element {
  const [identity, setIdentity] = useState<{ publicKey: string; peers: readonly string[] }>({
    publicKey: '',
    peers: [],
  });
  const [snapshot, setSnapshot] = useState<HostSnapshot | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [held, setHeld] = useState<HostPermissions | null>(null);

  const [code, setCode] = useState<string | null>(null);
  const [hint, setHint] = useState('Type this on the other machine');
  const [pairing, setPairing] = useState(false);
  const [pairTrouble, setPairTrouble] = useState<string | null>(null);
  const [trouble, setTrouble] = useState<string | null>(null);

  const [open, setOpen] = useState(false);
  const body = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    void (async () => {
      const [who, stored, grants, first] = await Promise.all([
        prism.identity(),
        prism.getSettings(),
        prism.permissions(),
        prism.snapshot(),
      ]);

      setIdentity({ publicKey: who.publicKey, peers: who.peers });
      setSettings(stored);
      setHeld(grants);
      setSnapshot(first);
    })();
  }, []);

  useEffect(() => {
    prism.onSnapshot(setSnapshot);
  }, []);

  useEffect(() => {
    prism.onFold(setOpen);
  }, []);

  // The window is shaped from whether the panel is out and how tall it is. Measured rather
  // than calculated, because a missing grant or a pairing code adds a section that was not
  // there a moment ago.
  useEffect(() => {
    const measured = body.current;

    if (!measured) {
      prism.shelf(open, 0);
      return;
    }

    const report = (): void => {
      prism.shelf(open, measured.scrollHeight);
    };

    report();

    const watch = new ResizeObserver(report);
    watch.observe(measured);

    return () => {
      watch.disconnect();
    };
  }, [open]);

  const phase = snapshot?.phase ?? 'idle';
  const running = RUNNING.has(phase);
  const streaming = phase === 'streaming';

  const save = (change: Partial<Settings>): void => {
    setSettings((was) => (was ? { ...was, ...change } : was));
    void prism.setSettings(change);
  };

  const toggle = (): void => {
    void (async () => {
      setTrouble(null);

      try {
        setSnapshot(running ? await prism.stopHosting() : await prism.startHosting());
      } catch (error) {
        setTrouble(reason(error));
      }
    })();
  };

  const pair = (): void => {
    if (pairing || !settings) {
      return;
    }

    void (async () => {
      setPairing(true);
      setPairTrouble(null);

      try {
        const shown = await prism.pairingCode();
        setCode(shown);
        setHint('Type this on the other machine');

        const { peers } = await prism.awaitPairing(settings.bind, shown);

        setIdentity((was) => ({ ...was, peers }));
        setHint('Paired');
        setTimeout(() => {
          setCode(null);
        }, PAIRED_LINGER_MS);
      } catch (error) {
        setCode(null);
        setPairTrouble(reason(error));
      } finally {
        setPairing(false);
      }
    })();
  };

  const [only] = identity.peers;
  const peers =
    identity.peers.length === 0
      ? 'None yet'
      : identity.peers.length === 1 && only
        ? short(only)
        : `${identity.peers.length} devices`;

  const grip = (
    <button
      type="button"
      aria-label={open ? 'Fold the panel away' : 'Pull the panel out'}
      className={`no-drag flex w-[30px] flex-none items-center justify-center self-stretch border-y border-l border-line-2 bg-sidebar text-ui backdrop-blur-md transition-colors hover:bg-wash-3 ${
        GRIP_TONE[phase] ?? 'text-dim'
      } ${open ? 'rounded-l-none' : 'rounded-l-xl'}`}
      onClick={() => {
        setOpen(!open);
      }}
      onContextMenu={(event) => {
        event.preventDefault();
        prism.shelfMenu();
      }}
    >
      {open ? '›' : '‹'}
    </button>
  );

  // Folded, the window is the grip and nothing else, so the screen gives up thirty pixels of
  // its edge rather than a strip somebody's clicks have to get past.
  if (!open) {
    return <div className="flex h-full">{grip}</div>;
  }

  return (
    // No height of its own: the row is as tall as the panel's content, which is the figure
    // reported back so the window can be made that tall too. Giving it the window's height
    // instead would make the measurement circular and the panel would never shrink.
    <div className="flex items-stretch">
      <div
        ref={body}
        className="min-w-0 flex-1 overflow-hidden rounded-l-xl border-y border-l border-line-2 bg-base/95 backdrop-blur-md"
      >
      <div className="drag h-3.5" />

      <header className="flex items-baseline gap-2.5 px-4 pb-3">
        <div className="flex items-center gap-2">
          <img src="assets/mark-home.svg" alt="" className="block h-[13px] w-[15px]" />
          <span className="text-note font-semibold tracking-[3px] text-ink">PRISM</span>
        </div>
        <code
          title={identity.publicKey}
          className="font-mono text-tiny-2 select-text text-dim"
        >
          {identity.publicKey ? short(identity.publicKey) : '…'}
        </code>
      </header>

      {/* The one loud thing. Unlike the client's, this band is always here: whether a machine
          is hosting is the whole question somebody opens this panel to answer. */}
      <div
        className={`border-y border-line-1 border-l-[3px] py-3 pr-4 pl-3.5 transition-colors ${
          PHASE_EDGE[phase] ?? IDLE_EDGE
        }`}
      >
        <div className="flex items-center justify-between gap-3">
          <span className="text-ui font-semibold tracking-[-0.01em]">
            {PHASE_LABELS[phase] ?? phase}
          </span>
          <button type="button" className="btn-primary-sm no-drag" onClick={toggle}>
            {running ? 'Stop' : 'Start hosting'}
          </button>
        </div>

        {/* Hidden until there is a session, because a panel full of zeroes says a machine is
            doing something badly rather than nothing. */}
        {snapshot && (
          <div className="mt-2">
            {snapshot.peer && (
              <Row label="Client">
                <code className="font-mono text-tiny select-text text-ink-3">
                  {short(snapshot.peer)}
                </code>
              </Row>
            )}
            {/* Shown while waiting rather than only once a client is on, because waiting is
                exactly when somebody needs to read it off and type it into the other machine. */}
            {snapshot.local && (
              <Row label="Listening on">
                <code className="font-mono text-tiny select-text text-ink-3">
                  {snapshot.local}
                </code>
              </Row>
            )}
            {snapshot.observed && (
              <Row label="Reachable at">
                <code className="font-mono text-tiny select-text text-ink-3">
                  {snapshot.observed}
                </code>
              </Row>
            )}
            {streaming && (
              <>
                <Row label="Sending">
                  <span className="text-note-2 tabular-nums text-ink-3">
                    {rate(snapshot.bitrateBps)}
                  </span>
                </Row>
                <Row label="Frames">
                  <span className="text-note-2 tabular-nums text-ink-3">
                    {String(snapshot.frames)}
                  </span>
                </Row>
              </>
            )}
          </div>
        )}

        {(trouble ?? snapshot?.error) && (
          <p className="mt-1.5 whitespace-pre-wrap text-fine-2 text-danger-ink">
            {trouble ?? snapshot?.error}
          </p>
        )}
      </div>

      {/* A missing grant is a warning rather than a failure: the host still runs, it just
          cannot do the thing the grant covers. */}
      {held && held.missing.length > 0 && (
        <Band title="Permissions">
          {held.missing.map((grant) => (
            <div key={grant.id} className="flex min-h-[26px] items-center justify-between gap-2.5">
              <span className="flex min-w-0 flex-col">
                <span className="text-note-2 text-ink">{grant.name}</span>
                <span className="text-tiny text-dim">Needed {grant.purpose}.</span>
              </span>
              <button
                type="button"
                className="btn-secondary no-drag"
                onClick={() => {
                  void (async () => {
                    // The system prompts once. If it has already been answered, the main
                    // process opens the settings pane instead, and the panel is redrawn when
                    // the window is next shown — a grant changed in Settings does not reach a
                    // running application.
                    setHeld(await prism.requestPermission(grant.id));
                  })();
                }}
              >
                Allow
              </button>
            </div>
          ))}
        </Band>
      )}

      <Band title="Paired devices">
        <Row label={peers}>
          <button type="button" className="btn-secondary no-drag" disabled={pairing} onClick={pair}>
            Pair a device
          </button>
        </Row>
        {code !== null && (
          <div className="mt-2.5 text-center">
            {/* The one number in this application a person reads out loud to somebody else, so
                it is set at the size that survives being read off a screen from a step away. */}
            <div className="font-mono text-[30px] font-semibold tracking-[0.16em] select-text text-violet">
              {code}
            </div>
            <div className="mt-1 text-tiny text-dim">{hint}</div>
          </div>
        )}
        {pairTrouble !== null && (
          <p className="mt-2 whitespace-pre-wrap text-fine-2 text-danger-ink">{pairTrouble}</p>
        )}
      </Band>

      <Band title="Settings">
        <Row label="Rendezvous">
          <input
            type="text"
            spellCheck={false}
            placeholder="host:47300"
            className={FIELD}
            value={settings?.rendezvous ?? ''}
            onChange={(event) => {
              setSettings((was) => (was ? { ...was, rendezvous: event.target.value } : was));
            }}
            onBlur={(event) => {
              save({ rendezvous: event.target.value.trim() });
            }}
          />
        </Row>
        <Row label="Frame rate">
          <input
            type="number"
            min={1}
            max={480}
            step={1}
            className={FIELD}
            value={settings?.fps ?? 60}
            onChange={(event) => {
              save({ fps: Number(event.target.value) });
            }}
          />
        </Row>
        <Row label="Bitrate (Mbps)">
          <input
            type="number"
            min={1}
            max={200}
            step={1}
            className={FIELD}
            value={settings ? Math.round(settings.bitrateBps / 1e6) : 24}
            onChange={(event) => {
              save({ bitrateBps: Number(event.target.value) * 1e6 });
            }}
          />
        </Row>
        <Row label="Allow control">
          <input
            type="checkbox"
            className="size-[15px] accent-violet"
            checked={settings?.injectInput ?? true}
            onChange={(event) => {
              save({ injectInput: event.target.checked });
            }}
          />
        </Row>
        <Row label="Host on launch">
          <input
            type="checkbox"
            className="size-[15px] accent-violet"
            checked={settings?.autoStart ?? false}
            onChange={(event) => {
              save({ autoStart: event.target.checked });
            }}
          />
        </Row>
      </Band>

        <div className="flex items-center justify-between px-4 py-3 text-tiny text-dim">
          <span>Right-click the grip for more</span>
          <button
            type="button"
            className="no-drag text-tiny text-dim transition-colors hover:text-danger-ink"
            onClick={prism.shelfMenu}
          >
            Menu
          </button>
        </div>
      </div>
      {grip}
    </div>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Panel />
  </StrictMode>,
);
