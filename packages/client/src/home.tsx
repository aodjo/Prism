/**
 * The home window.
 *
 * This machine at the top, every machine it can reach below it, and what has been watched
 * lately under that. Nothing here draws a screen: a picture of a remote machine would have had
 * to cross into JavaScript to arrive, and it never does — the stream is decoded and drawn by a
 * process of its own. So this window is names, addresses and figures, which is all it can
 * honestly be.
 */

import { StrictMode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { createRoot } from 'react-dom/client';

import type {
  AccountDeviceView,
  Available,
  HostSnapshot,
  PrismApi,
  Session,
  Settings,
  StreamState,
} from './api.js';
import { ago, latency, span, when } from './format.js';
import { Preferences, SharingTerms } from './preferences.js';
import { speak, t } from './i18n.js';
import { Backdrop, HOME_SKY, Trouble, Wordmark, reason, short } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

/**
 * What a machine sends before anybody has changed the settings.
 *
 * Stated here as well as in the main process because this window draws the figures before the
 * settings have arrived, and a card that says nothing for a moment and then something is worse
 * than one that says the truth immediately.
 */
const DEFAULT_FPS = 60;

/** And at what rate. */
const DEFAULT_BITRATE_BPS = 24_000_000;

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

/**
 * The colour a machine is marked with, assigned by key rather than by position.
 *
 * A colour that moved when the list reordered would be worse than no colour at all: the whole
 * of what it is for is recognising the same machine twice in a list of similar names.
 */
const MARKS: readonly string[] = [
  'rgba(124, 92, 255, 0.85)',
  'rgba(77, 232, 176, 0.85)',
  'rgba(53, 214, 255, 0.85)',
  'rgba(255, 92, 168, 0.85)',
];

/** The frame every machine in the grid has. */
const CARD =
  'group relative flex items-center gap-3 rounded-card border bg-wash-3 py-[18px] pr-4 pl-5';

/**
 * Picks a machine's mark from its key.
 *
 * @param {string} key - The machine's public key, as hex.
 * @returns {string} Its colour, the same one every time.
 */
function markOf(key: string): string {
  let sum = 0;

  for (let at = 0; at < key.length; at += 1) {
    sum = (sum * 31 + key.charCodeAt(at)) % 65_536;
  }

  return MARKS[sum % MARKS.length] as string;
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
  const [machines, setMachines] = useState<readonly string[]>([]);
  const [devices, setDevices] = useState<readonly AccountDeviceView[]>([]);
  const [account, setAccount] = useState<{ email: string | null }>({
    email: null,
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
  /**
   * What this machine still needs before it can be shared, and how far along asking has got.
   *
   * `ask` while there is a settings pane to open, `restart` once it has been opened — screen
   * recording is read once for the life of a process, so a grant given now is one this run goes
   * on calling missing. Null when nothing is in the way.
   */
  const [needsScreen, setNeedsScreen] = useState<'ask' | 'restart' | null>(null);
  /** Whether the settings are open over the window. */
  const [tuning, setTuning] = useState(false);
  /** Whether the terms this machine is shared on are open beside the switch. */
  const [terms, setTerms] = useState(false);
  /** The newer build the shell found at launch, until it is installed or waved away. */
  const [update, setUpdate] = useState<Available | null>(null);
  /** What went wrong installing it, which is the only place that would otherwise be silent. */
  const [updateError, setUpdateError] = useState('');
  const [installing, setInstalling] = useState(false);
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

      speak(stored.language);
      setSettings(stored);
      setDevices(signedIn.devices);
      setAccount({ email: signedIn.email });
      setMine(own);
      setStream(state);
      setHistory(past);

      // The account's machines, and only those. What this machine happens to trust locally is
      // not the same question: that file is a cache of the account's answer, and anything in
      // it that the account does not name is something nobody may reach from here.
      setMachines(
        signedIn.devices
          .map((device) => device.publicKey)
          .filter((key) => key !== identity.publicKey),
      );
    })();
  }, []);

  useEffect(() => {
    prism.onSharing(setMine);
    prism.onSessions(setHistory);
    prism.onStream(setStream);

    // The account is asked again whenever this window comes forward, so a machine signed in
    // somewhere else turns up here without anybody restarting anything.
    prism.onAccount((state) => {
      setDevices(state.devices);
      setAccount({ email: state.email });
      setMachines(
        state.devices
          .map((device) => device.publicKey)
          .filter((key) => key !== state.publicKey),
      );
    });

    prism.onUpdate(setUpdate);
  }, []);

  const pinned = useMemo(() => new Set(settings?.pinned ?? []), [settings]);

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

  /** Whether this machine is handing its screen out, or on its way to. */
  const shared = mine !== null && mine.phase !== 'stopped' && mine.phase !== 'failed';

  /** Whether somebody is actually watching it. */
  const watched = mine?.phase === 'streaming';

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

  /** Starts or stops handing this machine's screen out. */
  const flip = useCallback((): void => {
    void (async () => {
      setTrouble(null);
      setNeedsScreen(null);

      try {
        setMine(shared ? await prism.stopSharing() : await prism.startSharing());
      } catch (error) {
        setTrouble(reason(error));

        // A refusal that names a permission is one somebody can act on, so the window offers
        // the way there rather than the name of a settings pane to go and find. Asked of the
        // system rather than read out of the message, which is text and would tie this to its
        // wording.
        try {
          const held = await prism.permissions();

          setNeedsScreen(held.screen ? null : 'ask');
        } catch {
          setNeedsScreen(null);
        }
      }
    })();
  }, [shared]);

  // Allowing happens in System Settings, which is to say while this window is not the one being
  // looked at. Read again when it comes back, so a machine that may now record its screen stops
  // saying it may not.
  useEffect(() => {
    if (!needsScreen) {
      return;
    }

    const again = (): void => {
      void (async () => {
        try {
          const held = await prism.permissions();

          if (held.screen) {
            setNeedsScreen(null);
            setTrouble(null);
          }
        } catch {
          // Nothing to say. The answer is the one it already had.
        }
      })();
    };

    window.addEventListener('focus', again);

    return () => {
      window.removeEventListener('focus', again);
    };
  }, [needsScreen]);

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

      // Only ever starts. Ending a session somebody else is in the middle of is not something
      // to do by reflex, so the keystroke does nothing once this machine is shared.
      if (event.key === 'Enter' && !shared) {
        event.preventDefault();
        flip();
      }
    };

    document.addEventListener('keydown', onKey);

    return () => {
      document.removeEventListener('keydown', onKey);
    };
  }, [shared, flip]);

  const listed = everything ? history : history.slice(0, RECENT);

  useEffect(() => {
    if (!tuning && !terms) {
      return;
    }

    const close = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        setTuning(false);
        setTerms(false);
      }
    };

    window.addEventListener('keydown', close);

    return () => {
      window.removeEventListener('keydown', close);
    };
  }, [tuning, terms]);

  /**
   * Whether this is the only machine there is.
   *
   * Which is where everybody starts and where most people sit for a while, so it is a state to
   * design rather than the full window with its contents missing. Searching one machine,
   * filtering it, and counting it are all questions that answer themselves.
   */
  const alone = machines.length === 0;

  /** Where this machine can be reached, once it is listening somewhere. */
  const reachable =
    mine?.local === null || mine?.local === undefined
      ? null
      : mine.observed && mine.observed !== mine.local
        ? `${mine.local}  ·  seen at ${mine.observed}`
        : mine.local;

  /**
   * What this machine would send, as one line.
   *
   * The display it is on rather than one it was told about, because the thing being shared is
   * the screen this window is on. Multiplied by the backing scale, since a Mac reports the
   * size it draws at and the encoder is handed the pixels behind it.
   */
  const specs = useMemo(() => {
    const across = Math.round(window.screen.width * window.devicePixelRatio);
    const down = Math.round(window.screen.height * window.devicePixelRatio);
    const rate = settings?.fps ?? DEFAULT_FPS;
    const megabits = Math.round((settings?.bitrateBps ?? DEFAULT_BITRATE_BPS) / 1e6);

    return `${across} × ${down}  ·  ${rate} fps  ·  ${megabits} Mbps`;
  }, [settings?.fps, settings?.bitrateBps]);

  return (
    <div className="relative h-full w-full overflow-x-hidden overflow-y-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
      <Backdrop sky={HOME_SKY} />

      {/* The strip the window is carried by. It has to be a band of its own rather than a
          class on the header, because the shell moves the window for the element under the
          pointer and never for its children — so a header holding a search field and two
          buttons would drag from three narrow gaps. */}
      <div data-tauri-drag-region className="fixed inset-x-0 top-0 z-[3] h-[46px]" />

      <div className="relative z-[1] mx-auto flex w-full max-w-[1440px] flex-col px-[72px] pt-[46px] pb-[52px]">
        <header className="flex h-10 flex-none items-center gap-4">
          <Wordmark size="sm" />
          <div data-tauri-drag-region className="h-full flex-1" />
          <div
            hidden={alone}
            className="flex w-[460px] min-w-0 shrink items-center gap-[9px] rounded-pill border border-line-1 bg-wash-3 py-2.5 pr-4 pl-5"
          >
            <input
              ref={search}
              type="text"
              spellCheck={false}
              placeholder={t('Search devices, sessions, files')}
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
            className="flex-none rounded-pill"
            title={account.email ?? 'Not signed in'}
            aria-label={account.email ? `Signed in as ${account.email}` : 'Not signed in'}
            onClick={() => {
              setTuning(true);
            }}
          >
            <img src="assets/account.svg" alt="" className="block h-7 w-14" />
          </button>
        </header>

        <div className="mt-[30px] flex h-9 flex-none items-center gap-3">
          <h1 className="m-0 text-[26px] leading-none font-semibold tracking-[-0.5px] text-ink">
            {t('Devices')}
          </h1>
          <span
            hidden={alone}
            className="rounded-pill bg-[rgba(255,255,255,0.09)] px-[9px] py-1 text-fine font-medium text-muted-2"
          >
            {machines.length + 1}
          </span>
          <div data-tauri-drag-region className="h-full flex-1" />
          <div
            hidden={alone}
            className="flex items-center gap-0.5 rounded-pill border border-line-1 bg-wash-3 p-[3px]"
          >
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
            hidden={alone}
            onClick={prism.openSettings}
            className="inline-flex items-center gap-[7px] rounded-pill border border-line-4 bg-wash-3 py-[9px] pr-4 pl-[15px] text-note font-medium text-ink-2 transition-colors hover:bg-[rgba(255,255,255,0.1)]"
          >
            <span className="text-ui">+</span>
            <span>{t('Add device')}</span>
          </button>
        </div>

        {/* This machine, given the top of the window because it is the one machine that is
            always here and the one switch somebody came to flip. Everything below it is a
            machine somebody might watch; this is the one they might be watched on. */}
        <div className="relative mt-6 flex-none overflow-hidden rounded-card border border-line-4 bg-gradient-to-r from-[rgba(255,255,255,0.08)] to-[rgba(255,255,255,0.03)]">
          <div className="pointer-events-none absolute top-[-151px] left-[59%] h-[400px] w-[700px] mix-blend-screen">
            <img
              src="assets/resume-glow.svg"
              alt=""
              className="absolute inset-x-[-12.86%] inset-y-[-22.5%] block max-w-none"
            />
          </div>

          <div className="relative flex items-center gap-6 px-7 py-6">
            <div className="flex min-w-0 flex-1 flex-col gap-2.5">
              <span className="truncate text-[30px] leading-none font-semibold tracking-[-0.7px] text-ink">
                This machine
                {/* The name its owner gave it, after the one everybody's machine has. Somebody
                    with two of these is looking at two cards that say the same thing, and the
                    thing that tells them apart is the part they chose. */}
                {settings?.nickname ? (
                  <span className="font-normal text-muted-2"> ({settings.nickname})</span>
                ) : null}
              </span>
              {/* What it is doing, not where it is. Nobody types an address any more — the
                  account is what finds a machine — so putting one here is asking somebody to
                  read a number they will never use. It stays on the hover for the one case
                  that still needs it: a deployment with no rendezvous server, where the other
                  end has to be told by hand. */}
              <span title={reachable ?? undefined} className="truncate text-[13.5px] text-muted-2">
                {mine?.phase === 'failed'
                  ? 'Sharing failed'
                  : shared
                    ? mine?.local === null
                      ? 'Opening'
                      : 'Shared'
                    : t('Not shared')}
              </span>
              <span
                className={`truncate text-[12.5px] ${mine?.error ? 'text-danger-ink' : 'text-dim'}`}
              >
                {/* What this machine would send, rather than a sentence about waiting. The
                    line above already says whether it is shared, so saying it again in prose
                    spent the one line that could have carried something. */}
                {mine?.error ?? (watched ? `${machineName(mine?.peer ?? '')} is watching` : specs)}
              </span>
            </div>

            <div className="flex flex-none flex-col items-end gap-3.5">
              {watched && (
                <Chip tone="text-violet" wash="rgba(124, 92, 255, 0.13)">
                  {(Number(mine?.bitrateBps ?? 0n) / 1e6).toFixed(0)} Mbps
                </Chip>
              )}

              <div className="flex items-center gap-2.5">
                <button
                  type="button"
                  aria-label={t('Sharing terms')}
                  aria-expanded={terms}
                  title={t('Frame rate, bitrate and where it listens')}
                  className={`flex size-9 flex-none items-center justify-center rounded-pill border border-line-4 text-ink transition-colors ${
                    terms ? 'bg-[rgba(255,255,255,0.12)]' : ''
                  }`}
                  onClick={() => {
                    setTerms(!terms);
                  }}
                >
                  {/* The same drawing as the one in the header, taken out of it rather than
                      redrawn, so the two gears cannot drift apart.

                      Stencilled rather than drawn: an SVG behind `src` is its own document, and
                      the `currentColor` in it resolves against that document's black rather
                      than against this button. Masking paints the shape with the button's own
                      colour, which is the thing that was meant all along. */}
                  <span
                    aria-hidden
                    className="block size-[17px] bg-current [mask-image:url(assets/gear.svg)] [mask-position:center] [mask-repeat:no-repeat] [mask-size:contain]"
                  />
                </button>

              {shared ? (
                <button type="button" className="btn-danger px-6 py-3.5 text-[15px]" onClick={flip}>
                  {t('Stop sharing')}
                </button>
              ) : (
                <button type="button" className="btn-primary-md" onClick={flip}>
                  {t('Share this machine')}
                  <span className="btn-key">⌘↵</span>
                </button>
              )}
              </div>
            </div>
          </div>
        </div>

        {/* A modal rather than a popover hanging off the card. What is being set here is
            typed — a name, a rate, an address — and a panel that closes when a click lands
            slightly wrong is a panel that throws away what was being typed into it. */}
        {terms && (
          <div
            className="fixed inset-0 z-[3] grid place-items-center bg-[rgba(6,6,10,0.62)] p-6 backdrop-blur-[3px]"
            onMouseDown={(event) => {
              if (event.target === event.currentTarget) {
                setTerms(false);
              }
            }}
          >
            <div className="max-h-full w-full max-w-[460px] overflow-y-auto overscroll-contain rounded-card border border-line-4 bg-[rgba(20,20,26,0.97)] px-5 pt-4 pb-5 shadow-[0_24px_60px_rgba(0,0,0,0.5)] [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
              <div className="mb-2 flex items-center justify-between">
                <h2 className="m-0 text-[17px] leading-none font-semibold tracking-[-0.2px] text-ink">
                  {t('Sharing this machine')}
                </h2>
                <button
                  type="button"
                  aria-label={t('Close sharing terms')}
                  className="rounded-pill px-2 text-ui text-dim transition-colors hover:text-ink"
                  onClick={() => {
                    setTerms(false);
                  }}
                >
                  ✕
                </button>
              </div>
              <SharingTerms />

              {/* It says Done rather than Save because nothing here is waiting to be saved: a
                  figure applies as it is typed. What the button is for is ending the detour,
                  and having somewhere deliberate to click that is not the corner. */}
              <div className="mt-3 flex justify-end">
                <button
                  type="button"
                  className="btn-primary-sm"
                  onClick={() => {
                    setTerms(false);
                  }}
                >
                  {t('Done')}
                </button>
              </div>
            </div>
          </div>
        )}

        {/* Asked rather than done quietly. Installing replaces the application under a process
            that is running it, and the last step is a restart — which is not something to do to
            somebody who may be watching another machine at that moment. One question, and then
            everything that follows from the answer happens without asking again.

            No dismissing it by clicking away: the answer is one of the two buttons, because a
            modal that vanishes on a stray click is one somebody never decides about. */}
        {update && (
          <div className="fixed inset-0 z-[4] grid place-items-center bg-[rgba(6,6,10,0.62)] p-6 backdrop-blur-[3px]">
            <div className="w-full max-w-[420px] rounded-card border border-line-4 bg-[rgba(20,20,26,0.97)] px-5 pt-4 pb-5 shadow-[0_24px_60px_rgba(0,0,0,0.5)]">
              <h2 className="m-0 text-[17px] leading-none font-semibold tracking-[-0.2px] text-ink">
                새로운 빌드가 있습니다
              </h2>
              <p className="mt-3 mb-0 text-ui text-dim">
                {update.version} 을 설치하면 Prism이 다시 시작됩니다.
              </p>
              {update.notes.trim() !== '' && (
                <p className="mt-2 mb-0 max-h-24 overflow-y-auto text-ui text-dim">
                  {update.notes}
                </p>
              )}
              {updateError !== '' && (
                <p className="mt-3 mb-0 text-ui text-danger-ink">{updateError}</p>
              )}
              <div className="mt-4 flex justify-end gap-2">
                <button
                  type="button"
                  className="btn-secondary"
                  disabled={installing}
                  onClick={() => {
                    setUpdate(null);
                    setUpdateError('');
                  }}
                >
                  나중에
                </button>
                <button
                  type="button"
                  className="btn-primary-sm"
                  disabled={installing}
                  onClick={() => {
                    setInstalling(true);
                    setUpdateError('');
                    // Nothing follows a success: the process is replaced. Reaching the next
                    // line at all means it failed, and then the reason is worth more than a
                    // window that has quietly gone back to how it was.
                    void prism
                      .installUpdate()
                      .catch((error: unknown) => {
                        setUpdateError(
                          error instanceof Error ? error.message : String(error),
                        );
                      })
                      .finally(() => {
                        setInstalling(false);
                      });
                  }}
                >
                  {installing ? '설치 중…' : '설치'}
                </button>
              </div>
            </div>
          </div>
        )}

        <Trouble
          message={
            trouble ??
            (stream.phase === 'failed' && stream.log.length > 0
              ? stream.log.slice(-3).join('\n')
              : null)
          }
          className="mt-3 flex-none"
        />

        {/* Naming the pane and leaving somebody to find it is most of the work still to do, so
            the window does that part. What it cannot do is the last step: screen recording is
            read once for the life of a process, and a grant given to a running Prism is one it
            goes on calling missing until it starts again. */}
        {needsScreen && (
          <div className="mt-3 flex flex-none items-center gap-3">
            <button
              type="button"
              className="btn-secondary"
              onClick={() => {
                void (async () => {
                  if (needsScreen === 'restart') {
                    await prism.restart();

                    return;
                  }

                  try {
                    const held = await prism.requestPermission('screen');

                    setNeedsScreen(held.screen ? null : 'restart');

                    if (held.screen) {
                      setTrouble(null);
                    }
                  } catch (error) {
                    setTrouble(reason(error));
                  }
                })();
              }}
            >
              {needsScreen === 'restart' ? t('Restart PRISM') : t('Open System Settings')}
            </button>
          </div>
        )}

        {alone ? (
          /* The one thing left to do, said once. Every other machine on the account turns up
             here by itself, so what is missing is not a button but a second installation —
             and a window that offered a button instead would be offering the wrong thing. */
          <div className="mt-12 flex-none">
            <h2 className="m-0 text-[19px] leading-none font-semibold tracking-[-0.3px] text-ink-2">
              {t('Nothing to watch yet')}
            </h2>
            <p className="mt-3 mb-0 max-w-[46ch] text-note leading-relaxed text-muted-2">
              Install Prism on the machine you want to watch and sign in
              {account.email ? (
                <>
                  {' as '}
                  <span className="text-ink-3">{account.email}</span>
                </>
              ) : (
                ' to the same account'
              )}
              . It turns up here on its own.
            </p>
          </div>
        ) : (
          <>
        <h2 className="mt-10 flex-none text-ui font-medium tracking-[0.2px] text-muted-2">
          {t('Other devices')}
        </h2>

        {/* Three across at the width the design was drawn at, two when the window is narrow
            enough that a third would be a column of clipped names. */}
        <div className="mt-2.5 grid flex-none grid-cols-[repeat(auto-fill,minmax(290px,1fr))] gap-3">
          {shown.map((key) => {
            const state = machineState(key);
            const seen = lastOn(key);
            const held = pinned.has(key);
            const live = state === 'live' && stream.phase === 'streaming';

            return (
              <div
                key={key}
                className={`${CARD} ${state === 'off' ? 'border-line-1' : 'border-line-4'}`}
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
                  className="absolute inset-0 z-0 rounded-card transition-colors hover:bg-[rgba(255,255,255,0.04)]"
                />

                <div className="pointer-events-none relative z-[1] flex min-w-0 flex-1 items-center gap-3">
                  {/* Two states, as the design has them: a machine there is some way to reach,
                      and one there is not. Whether it is being watched right now is the line
                      underneath, where it can be said rather than encoded. */}
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
                      {live && stream.stats
                        ? `Streaming now · ${stream.stats.fps.toFixed(0)} fps · ${stream.stats.mbps.toFixed(0)} Mbps`
                        : state === 'live'
                          ? 'Connecting'
                          : seen
                            ? `Last seen ${ago(seen.endedAt)}`
                            : machineWhere(key)}
                    </span>
                  </span>

                  {(live && stream.stats ? true : seen !== null) && (
                    <span className="flex-none rounded-pill bg-[rgba(255,255,255,0.07)] px-2.5 py-[5px] text-fine-2 font-medium text-muted">
                      {live && stream.stats
                        ? `${latency(stream.stats.rttMs)} ms`
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
            );
          })}

          {shown.length === 0 && (
            <div className="col-span-full py-6 text-note text-dim">
              {machines.length === 0
                ? 'No other machines yet. Sign in on another one and it turns up here.'
                : query.trim() === ''
                  ? `No ${which} devices.`
                  : `Nothing here is called “${query.trim()}”.`}
            </div>
          )}
        </div>

        <div className="mt-10 flex flex-none items-center gap-2.5">
          <h2 className="m-0 text-ui font-medium tracking-[0.2px] text-muted-2">
            {t('Recent sessions')}
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
              {t('Every session you end is listed here, with what it came to.')}
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
                  style={{ background: markOf(one.host) }}
                />
                <span className="truncate text-control font-medium text-ink-2">
                  {machineName(one.host)}
                </span>
                <span className="flex-none text-[12.5px] text-dim">{when(one.endedAt)}</span>
                <span className="flex-1" />
                <span className="flex-none text-fine text-dim">{latency(one.rttMs)} ms avg</span>
                <span className="flex-none text-[12.5px] font-medium text-muted">
                  {span(one.endedAt - one.startedAt)}
                </span>
                <span className="flex-none text-[15px] text-dim-2">›</span>
              </button>
            ))
          )}
        </div>          </>
        )}

      </div>

      {/* Over the window rather than beside it. Settings are a detour from what somebody came
          to do, and a detour that dims what it interrupts is one they can see their way back
          from — a second window is a second thing to find, raise and close.

          Held against the window rather than against the page: the page scrolls, and a sheet
          positioned inside it opens wherever the scroll happens to be rather than in front of
          the person. Bounded too, so that a sheet taller than the window scrolls within itself
          instead of running off the bottom edge with no way to reach the rest. */}
      {tuning && (
        <div
          className="fixed inset-0 z-[2] grid place-items-center bg-[rgba(6,6,10,0.62)] p-6 backdrop-blur-[3px]"
          onMouseDown={(event) => {
            // Only the backdrop itself. A drag that started inside the sheet and ended out
            // here is somebody selecting text, not somebody dismissing it.
            if (event.target === event.currentTarget) {
              setTuning(false);
            }
          }}
        >
          <div className="max-h-full w-full max-w-[520px] overflow-y-auto overscroll-contain rounded-card border border-line-4 bg-[rgba(20,20,26,0.96)] shadow-[0_24px_60px_rgba(0,0,0,0.45)] [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
            <div className="flex items-center justify-between px-5 pt-4 pb-1">
              <h2 className="m-0 text-[17px] leading-none font-semibold tracking-[-0.2px] text-ink">
                Settings
              </h2>
              <button
                type="button"
                aria-label={t('Close settings')}
                className="rounded-pill px-2 text-ui text-dim transition-colors hover:text-ink"
                onClick={() => {
                  setTuning(false);
                }}
              >
                ✕
              </button>
            </div>

            <Preferences />

            <div className="flex justify-end border-t border-line-1 px-5 py-4">
              <button
                type="button"
                className="btn-primary-sm"
                onClick={() => {
                  setTuning(false);
                }}
              >
                {t('Done')}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Home />
  </StrictMode>,
);
