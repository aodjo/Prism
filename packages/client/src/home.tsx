/**
 * The home window.
 *
 * A pegboard with blocks on it. What each block shows is its own business; this file decides
 * which blocks there are, where they sit, and what happens when one is pressed. Nothing here
 * draws a screen: a picture of a remote machine would have had to cross into JavaScript to
 * arrive, and it never does — the stream is decoded and drawn by a process of its own.
 *
 * The arrangement is kept in the settings, so a board somebody built is the board they open
 * tomorrow. A machine that has never been arranged gets one laid out from whatever the account
 * has, which is a better first screen than an empty grid with an invitation on it.
 */

import { StrictMode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { createRoot } from 'react-dom/client';

import type {
  AccountDeviceView,
  Available,
  Block,
  HostSnapshot,
  PrismApi,
  Session,
  Settings,
  StreamState,
} from './api.js';
import { Body, titleOf } from './blocks.js';
import type { Ground } from './blocks.js';
import { STEP, holes, startingBoard } from './board.js';
import { AddPanel, BlockPanel, Frame, Pegboard } from './editor.js';
import { Gear, Language, Panes, SignOut } from './icons.js';
import { Preferences } from './preferences.js';
import { speak, t } from './i18n.js';
import { Backdrop, GRANTS, HOME_SKY, Trouble, Wordmark, reason, short } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

/**
 * What a machine sends before anybody has changed the settings.
 *
 * Stated here as well as in the shell because this window draws the figures before the settings
 * have arrived, and a block that says nothing for a moment and then something is worse than one
 * that says the truth immediately.
 */
const DEFAULT_FPS = 60;

/** And at what rate. Kept in step with the shell's own default in `settings.rs`. */
const DEFAULT_BITRATE_BPS = 40_000_000;

const prism = window.prism;

/** The phases where a stream is running or on its way to running. */
const RUNNING: ReadonlySet<string> = new Set(['connecting', 'streaming']);

/** Nothing is happening, and nothing has happened yet. */
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

/** The languages the menu offers, in the order it cycles through them. */
const LANGUAGES: readonly { id: string; name: string }[] = [
  { id: '', name: 'Follow the system' },
  { id: 'ko', name: '한국어' },
  { id: 'en', name: 'English' },
];

/**
 * The machines this one can watch: the account's others that are shared right now.
 *
 * One that is not shared is not somewhere to connect to, whatever the reason — switched off,
 * never turned on in Prism, or missing a permission sharing needs — so it is not offered.
 *
 * @param {readonly AccountDeviceView[]} devices - Every machine on the account.
 * @param {string} mine - This machine's public key.
 * @returns {string[]} The public keys of the ones to list.
 */
function watchable(devices: readonly AccountDeviceView[], mine: string): string[] {
  return devices
    .filter((device) => device.shared && device.publicKey !== mine)
    .map((device) => device.publicKey);
}

/**
 * The home window.
 *
 * @returns {JSX.Element} The whole of it.
 */
function Home(): JSX.Element {
  const [machines, setMachines] = useState<readonly string[]>([]);
  const [devices, setDevices] = useState<readonly AccountDeviceView[]>([]);
  const [account, setAccount] = useState<{ email: string | null }>({ email: null });
  const [settings, setSettings] = useState<Settings | null>(null);
  const [stream, setStream] = useState<StreamState>(NOTHING);
  const [history, setHistory] = useState<readonly Session[]>([]);
  /** What this machine's own session is doing, or `null` when it is not shared. */
  const [mine, setMine] = useState<HostSnapshot | null>(null);
  const [trouble, setTrouble] = useState<string | null>(null);

  /** The blocks, as they stand. */
  const [board, setBoard] = useState<readonly Block[]>([]);
  /** Whether the board is being arranged. */
  const [editing, setEditing] = useState(false);
  /** What it looked like when arranging started, which is what undoing goes back to. */
  const [before, setBefore] = useState<readonly Block[] | null>(null);
  /** Which block is being worked on. */
  const [picked, setPicked] = useState<string | null>(null);
  /** Which block's panel is open. */
  const [opened, setOpened] = useState<string | null>(null);
  /** Whether the block picker is open. */
  const [adding, setAdding] = useState(false);
  /** Whether the menu under the face is open. */
  const [menu, setMenu] = useState(false);
  /** How many holes fit across the board, measured rather than assumed. */
  const [across, setAcross] = useState(36);
  const field = useRef<HTMLDivElement | null>(null);

  /**
   * The grants sharing is waiting on, while the window is showing them, by id.
   *
   * Asked when somebody turns sharing on and something is missing, rather than said as an error
   * afterwards: the switch does nothing until these are allowed.
   */
  const [asking, setAsking] = useState<readonly string[] | null>(null);
  /** Which of those have already been sent to System Settings. */
  const [asked, setAsked] = useState<ReadonlySet<string>>(new Set());
  /** How the host went, when a stream window closed because the machine it showed went away. */
  const [gone, setGone] = useState<'left' | 'silent' | null>(null);
  /** Whether the settings are open over the window. */
  const [tuning, setTuning] = useState(false);
  /** The newer build the shell found at launch, until it is installed or waved away. */
  const [update, setUpdate] = useState<Available | null>(null);
  const [updateError, setUpdateError] = useState('');
  const [installing, setInstalling] = useState(false);

  const nameOf = useCallback(
    (key: string): string =>
      devices.find((device) => device.publicKey === key)?.label || short(key),
    [devices],
  );

  const platformOf = useCallback(
    (key: string): string => devices.find((device) => device.publicKey === key)?.platform ?? '',
    [devices],
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
      setMachines(watchable(signedIn.devices, identity.publicKey));
      setBoard(stored.board);
    })();
  }, []);

  useEffect(() => {
    prism.onSharing(setMine);
    prism.onSessions(setHistory);
    prism.onStream((state) => {
      setStream(state);

      if (state.departed !== null && !RUNNING.has(state.phase)) {
        setGone(state.departed);
      }
    });

    prism.onAccount((state) => {
      setDevices(state.devices);
      setAccount({ email: state.email });
      setMachines(watchable(state.devices, state.publicKey));
    });

    prism.onUpdate(setUpdate);
  }, []);

  // How wide the board is decides how many holes there are, so it is measured rather than
  // guessed. A window dragged narrower has fewer holes across, and a block wider than what is
  // left is held to the edge instead of hanging off it.
  useEffect(() => {
    const element = field.current;

    if (!element) {
      return;
    }

    const measure = (): void => {
      setAcross(holes(element.clientWidth));
    };

    measure();

    const watcher = new ResizeObserver(measure);
    watcher.observe(element);

    return () => {
      watcher.disconnect();
    };
  }, []);

  // A machine nobody has arranged gets a board laid out from what the account has. Done once
  // the account and the settings have both arrived, and only when nothing is saved: a board
  // somebody built is theirs, including one they emptied.
  useEffect(() => {
    if (settings === null || settings.board.length > 0 || board.length > 0) {
      return;
    }

    setBoard(startingBoard(machines, across));
  }, [settings, machines, across, board.length]);

  /**
   * Changes one of the terms this machine is shared on.
   *
   * Shown before it is written, so a slider being dragged moves under the hand rather than in
   * steps behind it. What comes back replaces it, which is what corrects a value the shell
   * refused or rounded.
   */
  const retune = useCallback((patch: Partial<Settings>): void => {
    setSettings((was) => (was ? { ...was, ...patch } : was));
    void prism.setSettings(patch).then(setSettings, (error: unknown) => {
      setTrouble(reason(error));
    });
  }, []);

  /** Writes the arrangement down, and keeps the window's copy in step with it. */
  const commit = useCallback((next: readonly Block[]): void => {
    setBoard(next);
    void prism.setSettings({ board: next }).then(setSettings, (error: unknown) => {
      setTrouble(reason(error));
    });
  }, []);

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

  /**
   * Shows the grants sharing is waiting on, or starts sharing when there are none.
   *
   * @async
   * @returns {Promise<boolean>} Whether sharing started.
   */
  const shareOrAsk = useCallback(async (): Promise<boolean> => {
    const grants = await prism.permissions();

    if (grants.missing.length > 0) {
      setAsking(grants.missing.map((grant) => grant.id));

      return false;
    }

    setAsking(null);
    setMine(await prism.startSharing());

    return true;
  }, []);

  /** Whether this machine is handing its screen out, or on its way to. */
  const shared = mine !== null && mine.phase !== 'stopped' && mine.phase !== 'failed';

  /** Whether somebody is actually watching it. */
  const watched = mine?.phase === 'streaming';

  /** Starts or stops handing this machine's screen out. */
  const flip = useCallback((): void => {
    void (async () => {
      setTrouble(null);

      try {
        if (shared) {
          setMine(await prism.stopSharing());
        } else {
          setAsked(new Set());
          await shareOrAsk();
        }
      } catch (error) {
        setTrouble(reason(error));
      }
    })();
  }, [shared, shareOrAsk]);

  // Allowing happens in System Settings, which is to say while this window is not the one being
  // looked at. Asked again when it comes back, so the list shrinks as switches are turned on.
  useEffect(() => {
    if (!asking) {
      return;
    }

    const again = (): void => {
      void shareOrAsk().catch((error: unknown) => {
        setTrouble(reason(error));
      });
    };

    window.addEventListener('focus', again);

    return () => {
      window.removeEventListener('focus', again);
    };
  }, [asking, shareOrAsk]);

  useEffect(() => {
    const close = (event: KeyboardEvent): void => {
      if (event.key !== 'Escape') {
        return;
      }

      setTuning(false);
      setMenu(false);
      setAdding(false);
      setOpened(null);
    };

    window.addEventListener('keydown', close);

    return () => {
      window.removeEventListener('keydown', close);
    };
  }, []);

  /**
   * What this machine would send, as one line.
   *
   * The display it is on rather than one it was told about, because the thing being shared is
   * the screen this window is on. Multiplied by the backing scale, since a Mac reports the size
   * it draws at and the encoder is handed the pixels behind it.
   */
  const specs = useMemo(() => {
    const across2 = Math.round(window.screen.width * window.devicePixelRatio);
    const down = Math.round(window.screen.height * window.devicePixelRatio);
    const rate = settings?.fps ?? DEFAULT_FPS;
    const megabits = Math.round((settings?.bitrateBps ?? DEFAULT_BITRATE_BPS) / 1e6);

    return `${across2} × ${down}  |  ${rate} fps  |  ${megabits} Mbps`;
  }, [settings?.fps, settings?.bitrateBps]);

  const ground: Ground = useMemo(
    () => ({
      devices,
      machines,
      stream,
      mine,
      history,
      settings,
      specs,
      nameOf,
      platformOf,
      watch,
      flip,
      tune: () => {
        setTuning(true);
      },
      retune,
    }),
    [
      devices,
      machines,
      stream,
      mine,
      history,
      settings,
      specs,
      nameOf,
      platformOf,
      watch,
      flip,
      retune,
    ],
  );

  /** How tall the board has to be to hold everything on it. */
  const down = board.reduce((low, one) => Math.max(low, one.y + one.h), 12) + 2;

  const working = board.find((one) => one.id === opened) ?? null;
  const chosen = board.find((one) => one.id === picked) ?? null;
  const language = LANGUAGES.find((one) => one.id === (settings?.language ?? '')) ?? LANGUAGES[0];

  return (
    <div className="relative h-full w-full overflow-x-hidden overflow-y-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
      <Backdrop sky={HOME_SKY} />

      {/* Room at the bottom for the bar that floats over it, so the last row of blocks can be
          scrolled clear of it rather than ending underneath. */}
      <div className="relative z-[1] flex min-h-full w-full flex-col px-8 pt-[62px] pb-[108px]">
        {/* The bar the window is carried by, and the bar its controls sit on — one element,
            because two meant the strip that drags lay over the controls that do not. The shell
            moves the window for the element under the pointer and never for its children, so
            everything in here is clickable and every gap between them drags. */}
        <header
          data-tauri-drag-region
          onDoubleClick={(event) => {
            // Only the bar itself. A double-click that landed on the wordmark or a button is
            // somebody hitting that thing twice, not somebody reaching for the title bar.
            if (event.target === event.currentTarget) {
              prism.titleBarDoubleClick();
            }
          }}
          className="fixed inset-x-0 top-0 z-[5] flex h-[62px] items-center gap-4 px-8"
        >
          <Wordmark size="sm" />
          {/* The empty middle. Deaf to the pointer, so that dragging and double-clicking there
              reach the bar itself rather than stopping at a spacer that does neither. */}
          <div className="pointer-events-none h-full flex-1" />

          {editing ? (
            <>
              <button
                type="button"
                className="text-note font-medium text-dim transition-colors hover:text-ink-3"
                onClick={() => {
                  if (before) {
                    commit(before);
                  }

                  setEditing(false);
                  setPicked(null);
                }}
              >
                {t('Undo')}
              </button>
              <button
                type="button"
                className="rounded-pill bg-white px-[18px] py-2 text-note font-medium text-on-light"
                onClick={() => {
                  commit(board);
                  setEditing(false);
                  setPicked(null);
                }}
              >
                {t('Done')}
              </button>
            </>
          ) : (
            <>
              <span className="text-fine text-dim">{account.email ?? ''}</span>
              <button
                type="button"
                aria-haspopup="menu"
                aria-expanded={menu}
                aria-label={account.email ? `Signed in as ${account.email}` : 'Not signed in'}
                onClick={() => {
                  setMenu(!menu);
                }}
                className="-m-1.5 flex flex-none items-center justify-center p-1.5"
              >
                {/* The target is larger than the mark, because the mark is 28 pixels across and
                    a 28-pixel target is one somebody misses. */}
                <span
                  className={`block size-7 rounded-pill bg-violet transition-shadow ${
                    menu ? 'ring-2 ring-white' : ''
                  }`}
                />
              </button>
            </>
          )}

          {menu && (
            <>
              <button
                type="button"
                aria-hidden
                tabIndex={-1}
                className="fixed inset-0 z-[4] cursor-default"
                onClick={() => {
                  setMenu(false);
                }}
              />
              <div
                role="menu"
                className="absolute top-[54px] right-8 z-[5] w-[252px] overflow-hidden rounded-panel border border-line-4 bg-[rgba(20,20,26,0.98)] py-1.5 shadow-[0_18px_48px_rgba(0,0,0,0.6)]"
              >
                <div className="flex items-center gap-[11px] px-3.5 py-3">
                  <span className="size-8 flex-none rounded-pill bg-violet" />
                  <span className="flex min-w-0 flex-col gap-1">
                    <span className="truncate text-note font-medium text-ink">
                      {settings?.nickname || t('This machine')}
                    </span>
                    <span className="truncate text-tiny text-dim">{account.email ?? ''}</span>
                  </span>
                </div>

                <div className="border-t border-line-1" />

                <button
                  type="button"
                  role="menuitem"
                  className="flex w-full items-center gap-[11px] px-3.5 py-2.5 text-note font-medium text-ink hover:bg-wash-2"
                  onClick={() => {
                    setBefore(board);
                    setEditing(true);
                    setMenu(false);
                  }}
                >
                  <Panes size={16} />
                  {t('Arrange this screen')}
                </button>

                <div className="border-t border-line-1" />

                <button
                  type="button"
                  role="menuitem"
                  className="flex w-full items-center gap-[11px] px-3.5 py-2.5 text-note font-medium text-ink-3 hover:bg-wash-2"
                  onClick={() => {
                    const at = LANGUAGES.findIndex((one) => one.id === (settings?.language ?? ''));
                    const next = LANGUAGES[(at + 1) % LANGUAGES.length] as { id: string };

                    speak(next.id);
                    void prism.setSettings({ language: next.id }).then(setSettings);
                  }}
                >
                  <Language size={16} />
                  <span className="flex-1 text-left">{t('Language')}</span>
                  <span className="text-tiny text-muted-2">{t(language?.name ?? '')}</span>
                </button>

                <button
                  type="button"
                  role="menuitem"
                  className="flex w-full items-center gap-[11px] px-3.5 py-2.5 text-note font-medium text-ink-3 hover:bg-wash-2"
                  onClick={() => {
                    setMenu(false);
                    setTuning(true);
                  }}
                >
                  <Gear size={16} />
                  {t('Settings')}
                </button>

                <div className="border-t border-line-1" />

                <button
                  type="button"
                  role="menuitem"
                  className="flex w-full items-center gap-[11px] px-3.5 py-2.5 text-note font-medium text-rose hover:bg-wash-2"
                  onClick={() => {
                    setMenu(false);
                    void prism.accountSignOut().catch((error: unknown) => {
                      setTrouble(reason(error));
                    });
                  }}
                >
                  <SignOut size={16} />
                  {t('Sign out')}
                </button>
              </div>
            </>
          )}
        </header>

        <Trouble
          message={
            trouble ??
            (stream.phase === 'failed' && stream.log.length > 0
              ? (stream.log.at(-1) ?? null)
              : null)
          }
          className="mt-3 flex-none"
        />

        {/* The board. Its height follows what is on it rather than the window, so a block
            dragged past the bottom takes the page with it instead of being clipped. */}
        <div
          ref={field}
          className="relative mt-11 min-h-0 flex-1"
          style={{ minHeight: down * STEP }}
          onPointerDown={(event) => {
            if (editing && event.target === event.currentTarget) {
              setPicked(null);
            }
          }}
        >
          <Pegboard shown={editing} />

          {board.map((one) => (
            <Frame
              key={one.id}
              block={one}
              editing={editing}
              picked={picked === one.id}
              across={across}
              onPick={() => {
                setPicked(one.id);
              }}
              onEdit={() => {
                setPicked(one.id);
                setOpened(one.id);
              }}
              onChange={(next) => {
                commit(board.map((other) => (other.id === next.id ? next : other)));
              }}
            >
              <Body block={one} ground={ground} />
            </Frame>
          ))}

          {board.length === 0 && (
            <div className="flex h-full flex-col items-start justify-center gap-3">
              <h2 className="m-0 text-heading font-semibold text-ink-2">
                {t('Nothing on this screen yet')}
              </h2>
              <p className="m-0 max-w-[46ch] text-note text-muted-2">
                {t('Arrange this screen, then add the blocks you want on it.')}
              </p>
            </div>
          )}
        </div>

        {/* The bar along the bottom. In view it is this machine and what it is doing; while the
            board is being arranged it is what arranging can do. */}
        {/* Held against the window rather than against the page. What it carries — whether this
            machine is shared, and the one switch that changes that — is not something to have to
            scroll back to, and a board taller than the window is exactly when somebody is
            furthest from it. */}
        <div className="fixed inset-x-8 bottom-[26px] z-[4] flex items-center gap-3.5 rounded-pill border border-line-4 bg-[rgba(5,5,7,0.92)] py-[9px] pr-4 pl-[30px] backdrop-blur-[6px]">
          {editing ? (
            <>
              <button
                type="button"
                className="inline-flex items-center gap-2 rounded-pill bg-violet px-4 py-2.5 text-note font-medium text-white"
                onClick={() => {
                  setAdding(true);
                }}
              >
                <span aria-hidden className="text-ui leading-none">
                  +
                </span>
                {t('Add a block')}
              </button>
              <span className="text-fine text-dim">
                {t('Drag a block to move it, and its corner to resize it.')}
              </span>
              <span className="flex-1" />
              {chosen && (
                <span className="text-fine text-ink-3 font-mono tabular-nums">
                  {titleOf(chosen, ground)} | {chosen.w} × {chosen.h}
                </span>
              )}
              <button
                type="button"
                className="btn-secondary"
                onClick={() => {
                  commit(startingBoard(machines, across));
                  setPicked(null);
                }}
              >
                {t('Default layout')}
              </button>
            </>
          ) : (
            <>
              <span className="truncate text-note font-medium text-ink">
                {watched
                  ? t('{name} is watching this machine', { name: nameOf(mine?.peer ?? '') })
                  : shared
                    ? t('Sharing this machine')
                    : t('Not shared')}
              </span>
              <span className="truncate text-fine text-dim font-mono tabular-nums">{specs}</span>
              <span className="flex-1" />
              <button type="button" className="btn-secondary" onClick={prism.openTransfers}>
                {t('Files')}
                {stream.moving.length > 0 && (
                  <span className="ml-2 rounded-pill bg-mint px-1.5 text-label font-semibold text-on-light font-mono tabular-nums">
                    {stream.moving.length}
                  </span>
                )}
              </button>
              {watched && (
                <button
                  type="button"
                  className="btn-secondary"
                  onClick={() => {
                    void prism.disconnectViewer().catch((error: unknown) => {
                      setTrouble(reason(error));
                    });
                  }}
                >
                  {t('Disconnect')}
                </button>
              )}
              <button
                type="button"
                className={shared ? 'btn-danger px-4 py-2.5 text-note' : 'btn-primary-sm'}
                onClick={flip}
              >
                {shared ? t('Stop sharing') : t('Share this machine')}
              </button>
            </>
          )}
        </div>
      </div>

      {/* While a session is opening. The stream draws itself in a window of its own and that
          window does not exist yet, so the wait belongs here — where the machine being opened
          is already named and there is something to press to give up on it. */}
      {stream.phase === 'connecting' && (
        <div className="fixed inset-0 z-[7] grid place-items-center bg-[rgba(5,5,7,0.96)]">
          <div
            aria-hidden
            className="pointer-events-none absolute top-1/2 left-1/2 h-[460px] w-[720px] -translate-x-1/2 -translate-y-1/2 opacity-60 blur-[120px]"
            style={{
              background:
                'linear-gradient(90deg, rgba(53,214,255,0.3), rgba(124,92,255,0.34), rgba(255,92,168,0.24))',
            }}
          />

          <button
            type="button"
            className="btn-secondary absolute top-6 right-8"
            onClick={() => {
              void prism.disconnect().catch((error: unknown) => {
                setTrouble(reason(error));
              });
            }}
          >
            {t('Disconnect')}
          </button>

          <div className="relative flex flex-col items-center gap-3.5">
            <span className="text-[34px] leading-none font-semibold tracking-[-0.8px] text-ink">
              {nameOf(stream.host ?? '')}
            </span>
            <span className="text-control font-medium text-muted-2">{t('Opening')}</span>
            <span className="relative mt-6 block h-1 w-[360px] overflow-hidden rounded-pill bg-wash-4">
              <span className="absolute inset-y-0 left-0 block w-1/3 rounded-pill bg-gradient-to-r from-transparent via-violet to-transparent motion-safe:animate-[sweep_1.6s_ease-in-out_infinite]" />
            </span>
          </div>

          <p className="absolute bottom-16 m-0 text-fine text-dim">
            {t('If this sits here, check that sharing is on over there.')}
          </p>
        </div>
      )}

      {adding && (
        <AddPanel
          board={board}
          across={across}
          machines={machines.map((key) => ({
            key,
            name: nameOf(key),
            platform: platformOf(key),
          }))}
          onAdd={(block) => {
            commit([...board, block]);
            setPicked(block.id);
            setAdding(false);
          }}
          onClose={() => {
            setAdding(false);
          }}
        />
      )}

      {working && (
        <BlockPanel
          block={working}
          title={titleOf(working, ground)}
          onChange={(next) => {
            setBoard(board.map((other) => (other.id === next.id ? next : other)));
          }}
          onRemove={() => {
            commit(board.filter((other) => other.id !== working.id));
            setOpened(null);
            setPicked(null);
          }}
          onClose={() => {
            commit(board);
            setOpened(null);
          }}
        />
      )}

      {/* Asked rather than done quietly. Installing replaces the application under a process
          that is running it, and the last step is a restart. */}
      {update && (
        <div className="fixed inset-0 z-[8] grid place-items-center bg-[rgba(6,6,10,0.62)] p-6 backdrop-blur-[3px]">
          <div className="w-full max-w-[420px] rounded-card border border-line-4 bg-[rgba(20,20,26,0.97)] px-5 pt-4 pb-5 shadow-[0_24px_60px_rgba(0,0,0,0.5)]">
            <h2 className="m-0 text-[17px] leading-none font-semibold tracking-[-0.2px] text-ink">
              새로운 빌드가 있습니다
            </h2>
            <p className="mt-3 mb-0 text-ui text-dim">
              {update.version} 을 설치하면 Prism이 다시 시작됩니다.
            </p>
            {update.notes.trim() !== '' && (
              <p className="mt-2 mb-0 max-h-24 overflow-y-auto text-ui text-dim">{update.notes}</p>
            )}
            {updateError !== '' && <p className="mt-3 mb-0 text-ui text-danger-ink">{updateError}</p>}
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
                  void prism
                    .installUpdate()
                    .catch((error: unknown) => {
                      setUpdateError(error instanceof Error ? error.message : String(error));
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

      {/* What sharing is waiting on, one row a grant, each with the way to it. */}
      {asking && (
        <div className="fixed inset-0 z-[8] grid place-items-center bg-[rgba(6,6,10,0.62)] p-6 backdrop-blur-[3px]">
          <div className="w-full max-w-[460px] rounded-card border border-line-4 bg-[rgba(20,20,26,0.97)] px-5 pt-4 pb-5 shadow-[0_24px_60px_rgba(0,0,0,0.5)]">
            <h2 className="m-0 text-[17px] leading-none font-semibold tracking-[-0.2px] text-ink">
              {t('Allow these to share this machine')}
            </h2>
            <p className="mt-3 mb-0 text-ui text-dim">
              {t('Turn each one on in System Settings, then come back.')}
            </p>

            <div className="mt-4">
              {GRANTS.filter((grant) => asking.includes(grant.id)).map((grant) => {
                const restart = grant.id === 'screen' && asked.has(grant.id);

                return (
                  <div
                    key={grant.id}
                    className="flex items-center gap-3 py-3 not-first:border-t not-first:border-line-1"
                  >
                    <span
                      className={`flex size-[34px] flex-none items-center justify-center rounded-badge border text-ui font-medium ${grant.tint}`}
                    >
                      {grant.glyph}
                    </span>
                    <span className="flex min-w-0 flex-1 flex-col gap-0.5">
                      <span className="text-ui font-medium text-ink">{t(grant.name)}</span>
                      <span className="text-fine text-muted-2">{t(grant.why)}</span>
                    </span>
                    <button
                      type="button"
                      className="btn-secondary flex-none"
                      onClick={() => {
                        void (async () => {
                          if (restart) {
                            await prism.restart();

                            return;
                          }

                          try {
                            await prism.requestPermission(grant.id);
                            setAsked((was) => new Set(was).add(grant.id));
                            await shareOrAsk();
                          } catch (error) {
                            setTrouble(reason(error));
                          }
                        })();
                      }}
                    >
                      {restart ? t('Restart PRISM') : t('Open System Settings')}
                    </button>
                  </div>
                );
              })}
            </div>

            <div className="mt-4 flex justify-end">
              <button
                type="button"
                className="btn-secondary"
                onClick={() => {
                  setAsking(null);
                }}
              >
                {t('Close')}
              </button>
            </div>
          </div>
        </div>
      )}

      {gone && (
        <div className="fixed inset-0 z-[8] grid place-items-center bg-[rgba(6,6,10,0.62)] p-6 backdrop-blur-[3px]">
          <div
            role="alertdialog"
            aria-labelledby="gone-title"
            className="w-full max-w-[400px] rounded-card border border-line-4 bg-[rgba(20,20,26,0.97)] px-5 pt-4 pb-5 shadow-[0_24px_60px_rgba(0,0,0,0.5)]"
          >
            <h2
              id="gone-title"
              className="m-0 text-[17px] leading-none font-semibold tracking-[-0.2px] text-ink"
            >
              {gone === 'left'
                ? t('The host disconnected')
                : t('The connection to the host was lost')}
            </h2>
            {gone === 'silent' && (
              <p className="mt-3 mb-0 text-ui text-dim">
                {t('Check that the host is on and connected to the network.')}
              </p>
            )}

            <div className="mt-5 flex justify-end">
              <button
                type="button"
                className="btn-secondary"
                autoFocus
                onClick={() => {
                  setGone(null);
                }}
              >
                {t('OK')}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Over the window rather than beside it. Settings are a detour from what somebody came
          to do, and a detour that dims what it interrupts is one they can see their way back
          from. */}
      {tuning && (
        <div
          className="fixed inset-0 z-[7] grid place-items-center bg-[rgba(6,6,10,0.62)] p-6 backdrop-blur-[3px]"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget) {
              setTuning(false);
            }
          }}
        >
          {/* The scrollbar is left showing here, unlike everywhere else in this window. The
              settings are taller than the sheet on a short screen, and a panel that scrolls
              with nothing to say so is one somebody reads the top of and takes for all of it. */}
          <div className="max-h-full w-full max-w-[520px] overflow-y-auto overscroll-contain rounded-card border border-line-4 bg-[rgba(20,20,26,0.96)] shadow-[0_24px_60px_rgba(0,0,0,0.45)]">
            <div className="flex items-center justify-between px-5 pt-4 pb-1">
              <h2 className="m-0 text-[17px] leading-none font-semibold tracking-[-0.2px] text-ink">
                {t('Settings')}
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
