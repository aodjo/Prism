/**
 * The home window.
 *
 * A list of machines and everything about the one that is picked. The picture of that machine
 * is not here — it is decoded and drawn by a process of its own, which is what keeps a video
 * frame from ever having to become a JavaScript value. What this window has is the numbers
 * that process reports, once a second, and the controls that start and stop it.
 */

import { StrictMode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { createRoot } from 'react-dom/client';

import type { AccountDeviceView, PrismApi, Settings, StreamState } from './api.js';
import { latency } from './format.js';
import { Backdrop, HOME_SKY, Trouble, Wordmark, reason, short } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** How many seconds of history a sparkline holds. */
const HISTORY = 26;

/** How tall a sparkline's tallest bar is, in pixels. */
const SPARK_HEIGHT = 28;

/**
 * The ceiling the round-trip sparkline is drawn against, in milliseconds.
 *
 * Fixed rather than taken from the run, because latency is the one figure here where lower is
 * better: scaled to its own maximum, a connection that never left half a millisecond would
 * draw full-height bars and read as a warning. Forty is a little over the point where this
 * stops being usable for a game, so a run that stays near the floor looks like what it is.
 */
const RTT_CEILING = 40;

/** The phases where a stream is running or on its way to running. */
const RUNNING: ReadonlySet<string> = new Set(['connecting', 'streaming']);

/** Nothing is happening, and nothing has happened yet. */
const NOTHING: StreamState = { phase: 'idle', host: null, terms: null, stats: null, log: [] };

/**
 * One measurement, its recent history, and what it is called.
 *
 * @param {object} props - What to draw.
 * @param {string} props.label - What it measures.
 * @param {string} props.unit - What it is measured in.
 * @param {string} props.value - The figure, or empty when there is none.
 * @param {string} props.tone - The colour class for the unit.
 * @param {string} props.rgb - The colour's channels, for the fade along the run.
 * @param {readonly number[]} props.history - The last half minute, oldest first.
 * @param {number | null} props.ceiling - What full height means, or `null` to take it from the run.
 * @returns {JSX.Element} The card.
 */
function Metric({
  label,
  unit,
  value,
  tone,
  rgb,
  history,
  ceiling,
}: {
  label: string;
  unit: string;
  value: string;
  tone: string;
  rgb: string;
  history: readonly number[];
  ceiling: number | null;
}): JSX.Element {
  const top = ceiling ?? Math.max(...history, 0);

  return (
    <div className="flex min-w-0 flex-1 flex-col gap-2.5 rounded-2xl border border-line-2 bg-wash-1 px-5 py-[18px]">
      <span className="text-label-2 font-medium text-dim">{label}</span>
      <span className="flex items-baseline gap-[5px]">
        {/* No figure yet. An em dash set at thirty pixels reads as a rule rather than as an
            absence, so the placeholder is smaller and the colour of something not there. */}
        {value === '' ? (
          <b className="text-[20px] font-medium text-dim">—</b>
        ) : (
          <b className="text-figure font-semibold tabular-nums">{value}</b>
        )}
        <span className={`text-note font-medium ${tone}`}>{unit}</span>
      </span>
      {/* Half a minute of history, one bar a second, oldest on the left. The bars fade towards
          the past rather than being one flat colour, so which end is now needs no label. */}
      <div className="flex h-7 items-end gap-[3px]">
        {history.length > 0 &&
          Array.from({ length: HISTORY }, (_, at) => {
            const sample = history[history.length - HISTORY + at];
            const share = top > 0 ? Math.min((sample ?? 0) / top, 1) : 0;

            return (
              <i
                // A fixed-length run of bars that never reorders: position is what identifies
                // one, and there is nothing else stable to key on.
                      key={at}
                className="block w-1 flex-none rounded-[2px]"
                style={{
                  height: `${Math.round(share * SPARK_HEIGHT)}px`,
                  minHeight: '2px',
                  background: `rgba(${rgb}, ${(0.25 + (at / HISTORY) * 0.53).toFixed(2)})`,
                }}
              />
            );
          })}
      </div>
    </div>
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
  const [account, setAccount] = useState<{ name: string | null; relay: boolean }>({
    name: null,
    relay: false,
  });
  const [settings, setSettings] = useState<Settings | null>(null);
  const [stream, setStream] = useState<StreamState>(NOTHING);
  const [picked, setPicked] = useState<string | null>(null);
  const [query, setQuery] = useState('');
  const [trouble, setTrouble] = useState<string | null>(null);
  const search = useRef<HTMLInputElement | null>(null);

  /** A minute of each figure, oldest first. */
  const [history, setHistory] = useState<{
    rtt: number[];
    fps: number[];
    rate: number[];
  }>({ rtt: [], fps: [], rate: [] });

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

      return settings?.rendezvous ? 'through the rendezvous server' : 'no address yet';
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

  useEffect(() => {
    void (async () => {
      const [identity, signedIn, stored, state] = await Promise.all([
        prism.identity(),
        prism.accountState(),
        prism.getSettings(),
        prism.streamState(),
      ]);

      setOwnKey(identity.publicKey);
      setSettings(stored);
      setDevices(signedIn.devices);
      setAccount({ name: signedIn.name, relay: signedIn.relayAllowed });

      // Both sources, minus this machine: one arrives by pairing and the other by signing in,
      // and which of the two brought a machine here is not something anybody wants to read two
      // lists to find out.
      const all = [
        ...new Set([...signedIn.devices.map((device) => device.publicKey), ...identity.hosts]),
      ].filter((key) => key !== identity.publicKey);

      setMachines(all);
      setStream(state);
      setPicked(state.host ?? all[0] ?? null);
    })();
  }, []);

  useEffect(() => {
    prism.onStream((state) => {
      setStream(state);

      // Picking follows the stream when a stream is what changed, so that starting one from
      // somewhere else does not leave this window looking at a different machine.
      if (state.host && RUNNING.has(state.phase)) {
        setPicked(state.host);
      }

      if (state.phase === 'streaming' && state.stats) {
        const stats = state.stats;

        setHistory((was) => ({
          rtt: [...was.rtt, stats.rttMs].slice(-HISTORY),
          fps: [...was.fps, stats.fps].slice(-HISTORY),
          rate: [...was.rate, stats.mbps].slice(-HISTORY),
        }));
      }
    });
  }, []);

  // The same seam the setup flow has, for the same reason: a window whose interesting states
  // all need a second machine is a window nobody can look at while building it.
  useEffect(() => {
    Object.defineProperty(window, 'prismHome', {
      value: { draw: setStream, pick: setPicked },
      configurable: true,
    });
  }, []);

  useEffect(() => {
    const onKey = (event: KeyboardEvent): void => {
      if (event.key === 'k' && (event.metaKey || event.ctrlKey)) {
        event.preventDefault();
        search.current?.focus();
      }
    };

    document.addEventListener('keydown', onKey);

    return () => {
      document.removeEventListener('keydown', onKey);
    };
  }, []);

  const shown = useMemo(
    () =>
      machines.filter(
        (key) => query.trim() === '' || machineName(key).toLowerCase().includes(query.trim().toLowerCase()),
      ),
    [machines, query, machineName],
  );

  const watching = picked !== null && stream.host === picked && RUNNING.has(stream.phase);
  const live = watching && stream.phase === 'streaming';
  const stats = live ? stream.stats : null;

  const watch = (): void => {
    if (picked === null) {
      return;
    }

    void (async () => {
      setTrouble(null);

      try {
        setStream(await prism.connect(picked, settings?.addresses[picked] ?? ''));
      } catch (error) {
        setTrouble(reason(error));
      }
    })();
  };

  return (
    <>
      <Backdrop sky={HOME_SKY} />

      <aside className="drag relative z-[1] flex h-full w-[268px] flex-none flex-col border-r border-line-1 bg-sidebar px-[22px] pt-[62px] pb-5">
        <div className="flex-none">
          <Wordmark size="sm" />
        </div>

        <div className="mt-[29px] flex flex-none items-center gap-[9px] rounded-tile border border-line-1 bg-wash-2 py-[9px] pr-2.5 pl-3">
          <span className="text-ui text-dim">⌕</span>
          <input
            ref={search}
            type="text"
            spellCheck={false}
            placeholder="Search devices"
            value={query}
            onChange={(event) => {
              setQuery(event.target.value);
            }}
            className="no-drag min-w-0 flex-1 border-0 bg-transparent p-0 text-note text-ink placeholder:text-dim focus:outline-none"
          />
          <kbd className="font-sans text-tiny-2 text-dim-2">⌘K</kbd>
        </div>

        <div className="mt-[30px] flex-none text-label-2 font-medium text-dim-2">DEVICES</div>

        <div className="mt-2.5 flex min-h-0 shrink flex-col gap-1 overflow-y-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
          {shown.length === 0 ? (
            <div className="px-2.5 py-[9px] text-note-2 text-dim">
              {machines.length === 0 ? 'None yet' : 'Nothing matches'}
            </div>
          ) : (
            shown.map((key) => {
              const state = machineState(key);

              // A row is a target, so the whole of it answers. The one being watched is the one
              // that is lit; the others are as quiet as the sidebar they sit in.
              return (
                <button
                  key={key}
                  type="button"
                  aria-current={key === picked}
                  onClick={() => {
                    setPicked(key);
                  }}
                  className={`no-drag flex w-full flex-none items-center gap-2.5 rounded-tile border px-2.5 py-[9px] text-left ${
                    key === picked
                      ? 'border-line-3 bg-wash-4'
                      : 'border-transparent bg-transparent hover:bg-wash-1'
                  }`}
                >
                  <img
                    src={`assets/status-${state}.svg`}
                    alt=""
                    className="block size-[7px] flex-none overflow-visible"
                  />
                  <span className="flex min-w-0 flex-1 flex-col gap-0.5">
                    <span
                      title={key}
                      className={`truncate text-note font-medium ${state === 'off' ? 'text-muted-2' : 'text-ink'}`}
                    >
                      {machineName(key)}
                    </span>
                    <span className="truncate text-tiny leading-tight text-dim">
                      {machineWhere(key)}
                    </span>
                  </span>
                  <span className="flex-none text-tiny-2 text-dim">
                    {state === 'live' && stats ? `${latency(stats.rttMs)} ms` : '—'}
                  </span>
                </button>
              );
            })
          )}
        </div>

        <button
          type="button"
          onClick={prism.openSettings}
          className="no-drag mt-2.5 flex w-full flex-none items-center gap-2 rounded-tile border border-dashed border-line-4 px-3 py-2.5 text-left text-note font-medium text-muted-2 hover:border-[rgba(124,92,255,0.5)] hover:text-ink-2"
        >
          <span className="text-ui">+</span>
          <span>Add device</span>
        </button>

        {/* Pushed to the floor of the sidebar rather than following the list, so that adding a
            machine does not move the account somebody is used to finding in one place. */}
        <button
          type="button"
          onClick={prism.openSettings}
          className="no-drag mt-auto flex w-full flex-none items-center gap-2.5 rounded-tile bg-wash-1 px-2.5 py-[9px] text-left hover:bg-wash-3"
        >
          <img src="assets/avatar.svg" alt="" className="block size-[26px] flex-none" />
          <span className="flex min-w-0 flex-1 flex-col gap-px">
            <span className="truncate text-note-2 font-medium text-ink">
              {account.name ?? 'Not signed in'}
            </span>
            <span className="text-tiny-2 text-dim">
              {account.name ? (account.relay ? 'Relay allowed' : 'Direct only') : 'This machine only'}
            </span>
          </span>
          <span className="flex-none text-note text-dim">⚙</span>
        </button>
      </aside>

      <main className="drag relative z-[1] flex h-full min-w-0 flex-1 flex-col px-10 pt-[52px] pb-10">
        <div className="flex min-h-10 flex-none items-center gap-3.5">
          <div className="flex min-w-0 flex-col gap-[5px]">
            <h1 className="m-0 truncate text-heading font-semibold">
              {picked ? machineName(picked) : 'No device'}
            </h1>
            <span className="text-tiny text-dim">
              {picked
                ? `${machineWhere(picked)}${live ? '  ·  direct LAN' : ''}`
                : 'Add one to get started'}
            </span>
          </div>
          <div className="flex-1" />
          {live && (
            <span className="inline-flex items-center gap-[7px] rounded-pill border border-[rgba(77,232,176,0.22)] bg-[rgba(77,232,176,0.12)] py-2 pr-3.5 pl-3 text-fine-2 font-medium text-mint">
              <img src="assets/dot-live.svg" alt="" className="block size-1.5 overflow-visible" />
              {stats ? `Live · ${latency(stats.rttMs)} ms` : 'Live'}
            </span>
          )}
          <button type="button" className="btn-secondary no-drag" disabled title="One display for now">
            Displays
          </button>
          {watching && (
            <button
              type="button"
              className="btn-danger no-drag"
              onClick={() => {
                void (async () => {
                  setStream(await prism.disconnect());
                })();
              }}
            >
              Disconnect
            </button>
          )}
          {!watching && (
            <button
              type="button"
              className="btn-primary-sm no-drag"
              disabled={picked === null}
              onClick={watch}
            >
              Connect
            </button>
          )}
        </div>

        {/* What the remote machine looks like. The picture itself is not here and never will
            be: it is decoded and drawn by a process of its own, in a window of its own, because
            a frame that reached this one would have had to cross into JavaScript to get here.
            So this panel carries the numbers, the controls, and a likeness — and says which. */}
        <div
          className="relative mt-[26px] min-h-0 flex-1 overflow-hidden rounded-stage border border-line-4 shadow-[0_24px_60px_-10px_rgba(0,0,0,0.55)]"
          style={{
            backgroundImage:
              'linear-gradient(153deg, #151a2e 0%, #0f1424 35.7%, #1a1430 71.4%)',
          }}
        >
          <div className="absolute left-[9.07%] top-[-21.92%] h-[126.81%] w-[82.42%] mix-blend-screen">
            <img
              src="assets/wallpaper-glow.svg"
              alt=""
              className="absolute inset-y-[-17.14%] inset-x-[-13.33%] block max-w-none"
            />
          </div>
          {/* Sized from the width alone so the height follows the artwork's own proportions. A
              stage that is not the shape it was drawn at would otherwise stretch the windows in
              it, and a stretched window reads as a rendering fault rather than as a likeness. */}
          <img
            src="assets/mock-window-1.svg"
            alt=""
            className="absolute left-[47.53%] top-[16.49%] block w-[43.04%] max-w-none"
          />
          <img
            src="assets/mock-window-2.svg"
            alt=""
            className="absolute left-[10.9%] top-[27%] block w-[47.62%] max-w-none"
          />
          <div className="absolute bottom-[10%] left-1/2 flex -translate-x-1/2 gap-2.5 rounded-2xl border border-line-4 bg-wash-4 px-3 py-2.5">
            {[
              'rgba(124,92,255,0.55)',
              'rgba(53,214,255,0.55)',
              'rgba(77,232,176,0.55)',
              'rgba(255,176,92,0.55)',
              'rgba(255,92,168,0.55)',
              'rgba(138,138,153,0.55)',
            ].map((tint) => (
              <i key={tint} className="block size-[30px] rounded-lg" style={{ background: tint }} />
            ))}
          </div>

          {live && stream.terms && (
            <div className="absolute left-[21px] top-[21px] flex gap-3.5 rounded-tile border border-line-3 bg-[rgba(6,6,10,0.55)] px-3.5 py-[9px] text-tiny tabular-nums text-ink-3">
              <span>
                {stream.terms.width
                  ? `${stream.terms.width} × ${stream.terms.height}`
                  : "the host's screen"}
              </span>
              <span>{stream.terms.fps} fps</span>
              <span>{stats ? `${stats.mbps.toFixed(0)} Mbps` : '—'}</span>
              <span>{stream.terms.codec}</span>
            </div>
          )}

          {/* Over the likeness while nothing is running, so that a still picture is never
              mistaken for a live one. Once a stream is up the dimming lifts and the sentence
              moves to the corner — the caption still has to be there, because what is under it
              is a likeness either way and the real picture is in another window. */}
          <div
            className={`absolute inset-0 flex flex-col gap-3.5 p-6 text-center transition-colors ${
              live
                ? 'items-end justify-start bg-[rgba(6,6,10,0.12)]'
                : 'items-center justify-center bg-[rgba(6,6,10,0.45)] backdrop-blur-[1px]'
            }`}
          >
            <p
              className={
                live
                  ? 'm-0 rounded-pill border border-line-3 bg-[rgba(6,6,10,0.66)] px-3.5 py-[7px] text-fine-2 text-ink-3'
                  : 'm-0 max-w-[380px] text-note leading-[21px] text-muted'
              }
            >
              {live
                ? `${machineName(picked ?? '')} is on screen in its own window.`
                : watching
                  ? 'Opening…'
                  : picked === null
                    ? 'Pick a machine, and its screen opens in a window of its own.'
                    : `Connect, and ${machineName(picked)} opens in a window of its own.`}
            </p>
            <Trouble
              message={
                trouble ??
                (stream.phase === 'failed' && stream.log.length > 0
                  ? stream.log.slice(-4).join('\n')
                  : null)
              }
              className="max-w-[620px] text-left text-fine-2"
            />
          </div>
        </div>

        <div className="mt-4 flex flex-none gap-4">
          <Metric
            label="ROUND TRIP"
            unit="ms"
            tone="text-mint"
            rgb="77, 232, 176"
            value={stats ? latency(stats.rttMs) : ''}
            history={history.rtt}
            ceiling={RTT_CEILING}
          />
          <Metric
            label="FRAME RATE"
            unit="fps"
            tone="text-cyan"
            rgb="53, 214, 255"
            value={stats ? stats.fps.toFixed(0) : ''}
            history={history.fps}
            ceiling={null}
          />
          <Metric
            label="BITRATE"
            unit="Mbps"
            tone="text-violet"
            rgb="124, 92, 255"
            value={stats ? stats.mbps.toFixed(0) : ''}
            history={history.rate}
            ceiling={null}
          />
        </div>
      </main>
    </>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Home />
  </StrictMode>,
);
