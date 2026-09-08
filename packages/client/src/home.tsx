/**
 * The home window.
 *
 * Every machine this one can reach, the one it would reach for next, and what has been watched
 * lately. The picture of a machine is not here and never will be — it is decoded and drawn by
 * a process of its own, which is what keeps a video frame from ever becoming a JavaScript
 * value. What the previews here draw is a likeness: four bars and two rectangles, tinted per
 * machine so that a list of them can be told apart at a glance.
 */

import { StrictMode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { createRoot } from 'react-dom/client';

import type {
  AccountDeviceView,
  HostSnapshot,
  PrismApi,
  Session,
  Settings,
  StreamState,
} from './api.js';
import { ago, latency, span, when } from './format.js';
import { Backdrop, HOME_SKY, Trouble, Wordmark, reason, short } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** The phases where a stream is running or on its way to running. */
const RUNNING: ReadonlySet<string> = new Set(['connecting', 'streaming']);

/** Nothing is happening, and nothing has happened yet. */
const NOTHING: StreamState = { phase: 'idle', host: null, terms: null, stats: null, log: [] };

/** How many sessions the list shows before somebody asks for the rest. */
const RECENT = 3;

/** Which machines the grid is showing. */
type Which = 'all' | 'online' | 'pinned';

/** The three filters, in the order they are offered. */
const WHICH: readonly { id: Which; label: string }[] = [
  { id: 'all', label: 'All' },
  { id: 'online', label: 'Online' },
  { id: 'pinned', label: 'Pinned' },
];

/** The colours a machine's likeness is drawn in. */
interface Tint {
  /** The top-left of the gradient behind the likeness. */
  readonly from: string;
  /** The bottom-right of it. */
  readonly to: string;
  /** The same machine's mark in a list, where there is no room for a gradient. */
  readonly mark: string;
}

/**
 * One tint per machine, assigned by key rather than by position.
 *
 * A colour that moved when the list reordered would be worse than no colour at all: the whole
 * of what it is for is recognising the same machine in the grid and in the history without
 * reading either label.
 */
const TINTS: readonly Tint[] = [
  { from: '#1a2140', to: '#221a3c', mark: 'rgba(124, 92, 255, 0.85)' },
  { from: '#16281f', to: '#1b2a3a', mark: 'rgba(77, 232, 176, 0.85)' },
  { from: '#152738', to: '#1a2440', mark: 'rgba(53, 214, 255, 0.85)' },
  { from: '#2c1a2e', to: '#241a3a', mark: 'rgba(255, 92, 168, 0.85)' },
];

/** What a machine nobody can reach is drawn in. */
const UNLIT: Tint = { from: '#131317', to: '#17171c', mark: 'rgba(98, 98, 111, 0.7)' };

/** The frame every card in the grid has. */
const CARD = 'group relative flex h-[214px] flex-col overflow-hidden rounded-card bg-wash-3';

/**
 * Picks a machine's tint from its key.
 *
 * @param {string} key - The machine's public key, as hex.
 * @returns {Tint} Its colours, the same ones every time.
 */
function tintOf(key: string): Tint {
  let sum = 0;

  for (let at = 0; at < key.length; at += 1) {
    sum = (sum * 31 + key.charCodeAt(at)) % 65_536;
  }

  return TINTS[sum % TINTS.length] as Tint;
}

/**
 * A machine's screen, as a likeness rather than a picture of it.
 *
 * Drawn rather than captured, because a frame cannot cross into this window and a still one
 * from a minute ago would be a lie either way. What it carries is the tint, which is the part
 * that identifies the machine.
 *
 * @param {object} props - What to draw.
 * @param {Tint} props.tint - The machine's colours.
 * @param {boolean} props.dark - Whether the machine is unreachable, in which case there is
 *   nothing to suggest and the likeness is dropped for the word.
 * @param {string | null} props.over - A word set over the likeness, for a machine that is on
 *   but is not handing its screen out.
 * @returns {JSX.Element} The banner.
 */
function Preview({
  tint,
  dark,
  over = null,
}: {
  tint: Tint;
  dark: boolean;
  over?: string | null;
}): JSX.Element {
  return (
    <div
      className={`relative h-[118px] w-full flex-none overflow-hidden ${dark ? 'opacity-55' : ''}`}
      style={{ backgroundImage: `linear-gradient(164deg, ${tint.from} 0%, ${tint.to} 71.4%)` }}
    >
      {!dark && (
        <>
          <div className="absolute top-[30px] left-[60px] h-24 w-[190px] overflow-hidden rounded-[7px] border border-line-4 bg-[rgba(14,17,32,0.82)]">
            {[70, 92, 114, 136].map((width, at) => (
              <i
                key={width}
                className="absolute left-[11px] block h-[5px] rounded-[2.5px]"
                style={{
                  top: `${15 + at * 14}px`,
                  width: `${width}px`,
                  background: `rgba(255, 255, 255, ${at < 2 ? 0.13 : 0.09})`,
                }}
              />
            ))}
          </div>
          <div className="absolute top-[18px] left-[240px] h-20 w-[150px] rounded-[7px] border border-line-4 bg-[rgba(14,17,32,0.6)] opacity-80" />
          <div className="absolute top-[100px] left-[130px] h-3 w-40 rounded-md bg-[rgba(255,255,255,0.12)]" />
        </>
      )}
      <div className="absolute inset-0 bg-gradient-to-b from-transparent to-[rgba(8,8,11,0.55)]" />
      {over !== null && (
        <span className="absolute inset-0 flex items-center justify-center bg-[rgba(8,8,11,0.4)] text-note font-medium text-dim-2">
          {over}
        </span>
      )}
    </div>
  );
}

/**
 * The same likeness at the size the featured machine gets, which is one window and a dock.
 *
 * @param {object} props - What to draw.
 * @param {Tint} props.tint - The machine's colours.
 * @returns {JSX.Element} The tile.
 */
function Thumbnail({ tint }: { tint: Tint }): JSX.Element {
  return (
    <div
      className="relative h-[136px] w-60 flex-none overflow-hidden rounded-xl border border-line-4"
      style={{ backgroundImage: `linear-gradient(150deg, ${tint.from} 0%, ${tint.to} 71.4%)` }}
    >
      <div className="absolute top-[29px] left-[25px] h-21 w-[150px] overflow-hidden rounded-md border border-line-4 bg-[rgba(14,17,32,0.85)]">
        {[60, 78, 96, 114].map((width, at) => (
          <i
            key={width}
            className="absolute left-[9px] block h-1 rounded-[2px]"
            style={{
              top: `${13 + at * 12}px`,
              width: `${width}px`,
              background: `rgba(255, 255, 255, ${at < 2 ? 0.13 : 0.09})`,
            }}
          />
        ))}
      </div>
      <div className="absolute top-[115px] left-[59px] h-3 w-30 rounded-md bg-[rgba(255,255,255,0.13)]" />
    </div>
  );
}

/**
 * One measurement, said in the colour it is measured in.
 *
 * @param {object} props - What to draw.
 * @param {string} props.tone - The Tailwind text colour.
 * @param {string} props.wash - The tint behind it.
 * @param {ReactNode} props.children - The figure and its unit.
 * @returns {JSX.Element} The chip.
 */
function Chip({
  tone,
  wash,
  children,
}: {
  tone: string;
  wash: string;
  children: ReactNode;
}): JSX.Element {
  return (
    <span
      className={`inline-flex items-center rounded-pill px-[11px] py-1.5 text-fine font-medium ${tone}`}
      style={{ background: wash }}
    >
      {children}
    </span>
  );
}

/**
 * The home window.
 *
 * @returns {JSX.Element} The whole of it.
 */
function Home(): JSX.Element {
  const [ownKey, setOwnKey] = useState('');
  const [machines, setMachines] = useState<readonly string[]>([]);
  const [devices, setDevices] = useState<readonly AccountDeviceView[]>([]);
  const [account, setAccount] = useState<{ email: string | null; relay: boolean }>({
    email: null,
    relay: false,
  });
  const [settings, setSettings] = useState<Settings | null>(null);
  const [stream, setStream] = useState<StreamState>(NOTHING);
  const [history, setHistory] = useState<readonly Session[]>([]);
  /** What this machine's own session is doing, or `null` when it is not shared. */
  const [mine, setMine] = useState<HostSnapshot | null>(null);
  const [query, setQuery] = useState('');
  const [which, setWhich] = useState<Which>('all');
  const [everything, setEverything] = useState(false);
  const [trouble, setTrouble] = useState<string | null>(null);
  const search = useRef<HTMLInputElement | null>(null);

  const machineName = useCallback(
    (key: string): string =>
      devices.find((device) => device.publicKey === key)?.label || short(key),
    [devices],
  );

  /** Where a machine is, as far as this one knows. */
  const machineWhere = useCallback(
    (key: string): string => {
      const address = settings?.addresses[key];

      if (address) {
        return address;
      }

      return settings?.rendezvous ? 'Through the rendezvous server' : 'No address yet';
    },
    [settings],
  );

  /** How a machine is doing: watched, reachable, or neither. */
  const machineState = useCallback(
    (key: string): 'live' | 'idle' | 'off' => {
      if (stream.host === key && RUNNING.has(stream.phase)) {
        return 'live';
      }

      return settings?.addresses[key] || settings?.rendezvous ? 'idle' : 'off';
    },
    [settings, stream],
  );

  /** The most recent session on a machine, or `null` if it has never been watched. */
  const lastOn = useCallback(
    (key: string): Session | null => history.find((one) => one.host === key) ?? null,
    [history],
  );

  useEffect(() => {
    void (async () => {
      const [identity, signedIn, stored, state, own, past] = await Promise.all([
        prism.identity(),
        prism.accountState(),
        prism.getSettings(),
        prism.streamState(),
        prism.sharing(),
        prism.sessions(),
      ]);

      setOwnKey(identity.publicKey);
      setSettings(stored);
      setDevices(signedIn.devices);
      setAccount({ email: signedIn.email, relay: signedIn.relayAllowed });
      setMine(own);
      setStream(state);
      setHistory(past);

      // Both sources, minus this machine: one arrives by having been trusted through the
      // account and the other by having been reached before, and which of the two brought a
      // machine here is not something anybody wants to read two lists to find out.
      setMachines(
        [
          ...new Set([...signedIn.devices.map((device) => device.publicKey), ...identity.hosts]),
        ].filter((key) => key !== identity.publicKey),
      );
    })();
  }, []);

  useEffect(() => {
    prism.onSharing(setMine);
    prism.onSessions(setHistory);
    prism.onStream(setStream);
  }, []);

  const pinned = useMemo(
    () => new Set(settings?.pinned ?? []),
    [settings],
  );

  /** What the grid is showing, pinned machines first and each group in its own order. */
  const shown = useMemo(() => {
    const wanted = query.trim().toLowerCase();

    return machines
      .filter((key) => wanted === '' || machineName(key).toLowerCase().includes(wanted))
      .filter((key) => {
        if (which === 'online') {
          return machineState(key) !== 'off';
        }

        if (which === 'pinned') {
          return pinned.has(key);
        }

        return true;
      })
      .sort((one, two) => Number(pinned.has(two)) - Number(pinned.has(one)));
  }, [machines, query, which, pinned, machineName, machineState]);

  /**
   * The machine the top of the window is about.
   *
   * Whatever is being watched, else whatever was watched last, else the first one there is.
   * All three are the same question — which machine is somebody here for — answered by the
   * strongest evidence available.
   */
  const featured = useMemo((): string | null => {
    if (stream.host !== null && RUNNING.has(stream.phase)) {
      return stream.host;
    }

    const recent = history.find((one) => machines.includes(one.host));

    return recent?.host ?? shown[0] ?? machines[0] ?? null;
  }, [stream, history, machines, shown]);

  const live = featured !== null && stream.host === featured && stream.phase === 'streaming';
  const watching = featured !== null && stream.host === featured && RUNNING.has(stream.phase);
  const stats = live ? stream.stats : null;
  const shared = mine !== null && mine.phase !== 'stopped' && mine.phase !== 'failed';

  const watch = useCallback(
    (key: string): void => {
      void (async () => {
        setTrouble(null);

        try {
          setStream(await prism.connect(key, settings?.addresses[key] ?? ''));
        } catch (error) {
          setTrouble(reason(error));
        }
      })();
    },
    [settings],
  );

  /** Adds a machine to the front of the list, or takes it back out. */
  const pin = (key: string): void => {
    void (async () => {
      const next = pinned.has(key)
        ? [...pinned].filter((one) => one !== key)
        : [...pinned, key];

      setSettings(await prism.setSettings({ pinned: next }));
    })();
  };

  useEffect(() => {
    const onKey = (event: KeyboardEvent): void => {
      if (!event.metaKey && !event.ctrlKey) {
        return;
      }

      if (event.key === 'k') {
        event.preventDefault();
        search.current?.focus();
      }

      // The one action the window is for, from the keyboard. Only when nothing is running:
      // there is no second screen to open, and ending one is not something to do by reflex.
      if (event.key === 'Enter' && featured !== null && !watching) {
        event.preventDefault();
        watch(featured);
      }
    };

    document.addEventListener('keydown', onKey);

    return () => {
      document.removeEventListener('keydown', onKey);
    };
  }, [featured, watching, watch]);

  /** How long a machine has been watched today. */
  const todayOn = (key: string): number => {
    const midnight = new Date();
    midnight.setHours(0, 0, 0, 0);

    return history
      .filter((one) => one.host === key && one.endedAt >= midnight.getTime())
      .reduce((sum, one) => sum + (one.endedAt - one.startedAt), 0);
  };

  const last = featured === null ? null : lastOn(featured);
  const today = featured === null ? 0 : todayOn(featured);
  const listed = everything ? history : history.slice(0, RECENT);

  return (
    <div className="relative h-full w-full overflow-x-hidden overflow-y-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
      <Backdrop sky={HOME_SKY} />

      <div className="relative z-[1] mx-auto flex w-full max-w-[1440px] flex-col px-[72px] pt-[46px] pb-[52px]">
        <header className="drag flex h-10 flex-none items-center gap-4">
          <Wordmark size="sm" />
          <div className="flex-1" />
          <div className="no-drag flex w-[460px] min-w-0 shrink items-center gap-[9px] rounded-xl border border-line-1 bg-wash-3 py-2.5 pr-3 pl-3.5">
            <span className="flex-none text-ui text-dim">⌕</span>
            <input
              ref={search}
              type="text"
              spellCheck={false}
              placeholder="Search devices, sessions, files"
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
              }}
              className="min-w-0 flex-1 border-0 bg-transparent p-0 text-note text-ink placeholder:text-dim focus:outline-none"
            />
            <kbd className="flex-none font-sans text-tiny font-medium text-dim-2">⌘K</kbd>
          </div>
          {/* The gear and the face are one drawing in the design, and one control here: both
              of them open the only place there is to change anything. */}
          <button
            type="button"
            className="no-drag flex-none rounded-pill"
            title={account.email ?? 'Not signed in'}
            aria-label={account.email ? `Signed in as ${account.email}` : 'Not signed in'}
            onClick={prism.openSettings}
          >
            <img src="assets/account.svg" alt="" className="block h-7 w-14" />
          </button>
        </header>

        <div className="drag mt-[30px] flex h-9 flex-none items-center gap-3">
          <h1 className="m-0 text-[26px] leading-none font-semibold tracking-[-0.5px] text-ink">
            Devices
          </h1>
          <span className="rounded-pill bg-[rgba(255,255,255,0.09)] px-[9px] py-1 text-fine font-medium text-muted-2">
            {machines.length + 1}
          </span>
          <div className="flex-1" />
          <div className="no-drag flex items-center gap-0.5 rounded-pill border border-line-1 bg-wash-3 p-[3px]">
            {WHICH.map((one) => (
              <button
                key={one.id}
                type="button"
                aria-pressed={which === one.id}
                onClick={() => {
                  setWhich(one.id);
                }}
                className={`rounded-pill px-3.5 py-1.5 text-[12.5px] font-medium transition-colors ${
                  which === one.id
                    ? 'bg-[rgba(255,255,255,0.13)] text-ink'
                    : 'text-muted-2 hover:text-ink-3'
                }`}
              >
                {one.label}
              </button>
            ))}
          </div>
          <button
            type="button"
            onClick={prism.openSettings}
            className="no-drag inline-flex items-center gap-[7px] rounded-pill border border-line-4 bg-wash-3 py-[9px] pr-4 pl-[15px] text-note font-medium text-ink-2 transition-colors hover:bg-[rgba(255,255,255,0.1)]"
          >
            <span className="text-ui">+</span>
            <span>Add device</span>
          </button>
        </div>

        {/* The machine somebody is most likely here for, and the one action it takes. Given
            the whole width because on most days it is the only thing on this page anybody
            touches. */}
        <div className="relative mt-6 h-44 flex-none overflow-hidden rounded-card border border-line-4 bg-gradient-to-r from-[rgba(255,255,255,0.08)] to-[rgba(255,255,255,0.03)]">
          <div className="pointer-events-none absolute top-[-151px] left-[59%] h-[400px] w-[700px] mix-blend-screen">
            <img
              src="assets/resume-glow.svg"
              alt=""
              className="absolute inset-y-[-22.5%] inset-x-[-12.86%] block max-w-none"
            />
          </div>

          {featured === null ? (
            <div className="relative flex h-full items-center gap-6 py-5 pr-6 pl-5">
              <Thumbnail tint={UNLIT} />
              <div className="flex min-w-0 flex-1 flex-col gap-2.5">
                <p className="m-0 truncate text-[30px] leading-none font-semibold tracking-[-0.7px] text-ink">
                  No devices yet
                </p>
                <p className="m-0 text-[13.5px] text-muted-2">
                  Sign in on another machine and it turns up here on its own.
                </p>
              </div>
              <button type="button" className="btn-primary-md" onClick={prism.openSettings}>
                Add device
              </button>
            </div>
          ) : (
            <div className="relative flex h-full items-center gap-6 py-5 pr-6 pl-5">
              <Thumbnail tint={machineState(featured) === 'off' ? UNLIT : tintOf(featured)} />

              <div className="flex min-w-0 flex-1 flex-col gap-2.5">
                <p
                  title={featured}
                  className="m-0 truncate text-[30px] leading-none font-semibold tracking-[-0.7px] text-ink"
                >
                  {machineName(featured)}
                </p>
                <p className="m-0 truncate text-[13.5px] text-muted-2">
                  {live && stream.terms
                    ? [
                        stream.terms.width
                          ? `${stream.terms.width} × ${stream.terms.height}`
                          : "the host's screen",
                        `${stream.terms.fps} fps`,
                        stream.terms.codec,
                      ].join(' · ')
                    : machineWhere(featured)}
                </p>
                <p className="m-0 truncate text-[12.5px] text-dim">
                  {last === null
                    ? 'Not watched from this machine yet'
                    : today > 0
                      ? `Last session ${ago(last.endedAt)} · ${span(today)} today`
                      : `Last session ${ago(last.endedAt)}`}
                </p>
              </div>

              <div className="flex flex-none flex-col items-end gap-3.5">
                <div className="flex gap-2">
                  {stats ? (
                    <>
                      <Chip tone="text-mint" wash="rgba(77, 232, 176, 0.13)">
                        {latency(stats.rttMs)} ms
                      </Chip>
                      <Chip tone="text-cyan" wash="rgba(53, 214, 255, 0.13)">
                        {stats.fps.toFixed(0)} fps
                      </Chip>
                      <Chip tone="text-violet" wash="rgba(124, 92, 255, 0.13)">
                        {stats.mbps.toFixed(0)} Mbps
                      </Chip>
                    </>
                  ) : (
                    last !== null && (
                      <Chip tone="text-mint" wash="rgba(77, 232, 176, 0.13)">
                        {latency(last.rttMs)} ms avg
                      </Chip>
                    )
                  )}
                </div>

                {watching ? (
                  <button
                    type="button"
                    className="btn-danger px-6 py-3.5 text-[15px]"
                    onClick={() => {
                      void (async () => {
                        setStream(await prism.disconnect());
                      })();
                    }}
                  >
                    {live ? 'End session' : 'Cancel'}
                  </button>
                ) : (
                  <button
                    type="button"
                    className="btn-primary-md"
                    onClick={() => {
                      watch(featured);
                    }}
                  >
                    {last === null ? 'Start session' : 'Resume session'}
                    <span className="btn-key">⌘↵</span>
                  </button>
                )}
              </div>
            </div>
          )}
        </div>

        <Trouble
          message={
            trouble ??
            (stream.phase === 'failed' && stream.log.length > 0
              ? stream.log.slice(-3).join('\n')
              : null)
          }
          className="mt-3 flex-none"
        />

        <h2 className="mt-10 flex-none text-ui font-medium tracking-[0.2px] text-muted-2">
          All devices
        </h2>

        {/* Three across at the width the design was drawn at, two when the window is narrow
            enough that a third would be a column of thumbnails rather than of machines. */}
        <div className="mt-2.5 grid flex-none grid-cols-[repeat(auto-fill,minmax(290px,1fr))] gap-[18px]">
          {/* This machine first, because it is the one card that answers what this machine
              gives out rather than what it takes in, and because the switch is the reason
              somebody came looking. */}
          <div className={`${CARD} border ${shared ? 'border-line-4' : 'border-line-1'}`}>
            {/* Drawn whether or not anybody may watch it: this machine is on, and its screen
                is the one thing here that certainly exists. What sharing changes is who can
                see it, which is what the word over the top says. */}
            <Preview tint={tintOf(ownKey)} dark={false} over={shared ? null : 'Not shared'} />
            <div className="flex min-h-0 flex-1 items-center gap-3 py-4 pr-4 pl-[18px]">
              <img
                src={`assets/status-${shared ? 'live' : 'off'}.svg`}
                alt=""
                className="block size-[7px] flex-none overflow-visible"
              />
              <span className="flex min-w-0 flex-1 flex-col gap-1">
                <span className="truncate text-[15.5px] font-semibold tracking-[-0.2px] text-ink">
                  This machine
                </span>
                <span className="truncate text-fine text-dim">
                  {mine?.error ??
                    (mine?.peer
                      ? `${machineName(mine.peer)} is watching`
                      : shared
                        ? `Waiting · ${mine?.local ?? 'opening'}`
                        : 'Nobody can watch it')}
                </span>
              </span>
              <button
                type="button"
                className={shared ? 'btn-secondary' : 'btn-primary-sm'}
                onClick={() => {
                  void (async () => {
                    setTrouble(null);

                    try {
                      setMine(shared ? await prism.stopSharing() : await prism.startSharing());
                    } catch (error) {
                      setTrouble(reason(error));
                    }
                  })();
                }}
              >
                {shared ? 'Stop' : 'Share'}
              </button>
            </div>
          </div>

          {shown.map((key) => {
            const state = machineState(key);
            const seen = lastOn(key);
            const held = pinned.has(key);

            return (
              <div
                key={key}
                className={`${CARD} border ${state === 'off' ? 'border-line-1' : 'border-line-4'}`}
              >
                {/* The whole card is the target. Laid over it rather than wrapped around it,
                    because the star is a control of its own and a button inside a button is
                    not a thing a browser will build. */}
                <button
                  type="button"
                  aria-label={`Watch ${machineName(key)}`}
                  onClick={() => {
                    watch(key);
                  }}
                  className="absolute inset-0 z-0 rounded-card transition-colors hover:bg-[rgba(255,255,255,0.03)]"
                />

                <div className="pointer-events-none relative z-[1] flex min-h-0 flex-1 flex-col">
                  <Preview
                    tint={state === 'off' ? UNLIT : tintOf(key)}
                    dark={state === 'off'}
                    over={state === 'off' ? 'Offline' : null}
                  />
                  <div className="flex min-h-0 flex-1 items-center gap-3 py-4 pr-4 pl-[18px]">
                    {/* Two states, as the design has them: a machine there is some way to
                        reach, and one there is not. Whether it is being watched right now is
                        the line underneath, where it can be said rather than encoded. */}
                    <img
                      src={`assets/status-${state === 'off' ? 'off' : 'live'}.svg`}
                      alt=""
                      className="block size-[7px] flex-none overflow-visible"
                    />
                    <span className="flex min-w-0 flex-1 flex-col gap-1">
                      <span
                        title={key}
                        className={`truncate text-[15.5px] font-semibold tracking-[-0.2px] ${
                          state === 'off' ? 'text-muted-2' : 'text-ink'
                        }`}
                      >
                        {machineName(key)}
                      </span>
                      <span title={machineWhere(key)} className="truncate text-fine text-dim">
                        {state === 'live'
                          ? 'Streaming now'
                          : seen
                            ? `Last seen ${ago(seen.endedAt)}`
                            : machineWhere(key)}
                      </span>
                    </span>

                    {(stats && state === 'live' ? true : seen !== null) && (
                      <span className="flex-none rounded-pill bg-[rgba(255,255,255,0.07)] px-2.5 py-[5px] text-fine-2 font-medium text-muted">
                        {stats && state === 'live'
                          ? `${latency(stats.rttMs)} ms`
                          : `${latency(seen?.rttMs ?? 0)} ms`}
                      </span>
                    )}

                    <button
                      type="button"
                      aria-pressed={held}
                      aria-label={held ? `Unpin ${machineName(key)}` : `Pin ${machineName(key)}`}
                      onClick={() => {
                        pin(key);
                      }}
                      className={`pointer-events-auto flex-none rounded-pill px-2 py-[5px] text-tiny font-medium transition-opacity ${
                        held
                          ? 'bg-[rgba(255,176,92,0.14)] text-amber'
                          : 'text-dim-2 opacity-0 group-hover:opacity-100 focus-visible:opacity-100'
                      }`}
                    >
                      ★
                    </button>
                  </div>
                </div>
              </div>
            );
          })}

          {shown.length === 0 && machines.length > 0 && (
            <div className="col-span-full py-6 text-note text-dim">
              {query.trim() === ''
                ? `No ${which} devices.`
                : `Nothing here is called “${query.trim()}”.`}
            </div>
          )}
        </div>

        <div className="mt-10 flex flex-none items-center gap-2.5">
          <h2 className="m-0 text-ui font-medium tracking-[0.2px] text-muted-2">
            Recent sessions
          </h2>
          <div className="flex-1" />
          {history.length > RECENT && (
            <button
              type="button"
              className="text-[12.5px] text-dim transition-colors hover:text-ink-3"
              onClick={() => {
                setEverything(!everything);
              }}
            >
              {everything ? 'Show fewer' : 'View all'}
            </button>
          )}
        </div>

        <div className="mt-2.5 flex flex-none flex-col gap-2">
          {listed.length === 0 ? (
            <p className="m-0 py-3 text-note-2 text-dim">
              Every session you end is listed here, with what it came to.
            </p>
          ) : (
            listed.map((one) => (
              <button
                key={`${one.host}-${one.startedAt}`}
                type="button"
                disabled={!machines.includes(one.host)}
                onClick={() => {
                  watch(one.host);
                }}
                className="flex w-full items-center gap-3.5 rounded-xl border border-line-1 bg-[rgba(255,255,255,0.04)] px-4 py-[13px] text-left transition-colors enabled:hover:bg-wash-3 disabled:cursor-default"
              >
                <i
                  className="block size-1.5 flex-none rounded-[2px]"
                  style={{ background: tintOf(one.host).mark }}
                />
                <span className="truncate text-control font-medium text-ink-2">
                  {machineName(one.host)}
                </span>
                <span className="flex-none text-[12.5px] text-dim">{when(one.endedAt)}</span>
                <span className="flex-1" />
                <span className="flex-none text-fine text-dim">
                  {latency(one.rttMs)} ms avg
                </span>
                <span className="flex-none text-[12.5px] font-medium text-muted">
                  {span(one.endedAt - one.startedAt)}
                </span>
                <span className="flex-none text-[15px] text-dim-2">›</span>
              </button>
            ))
          )}
        </div>
      </div>
    </div>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Home />
  </StrictMode>,
);
