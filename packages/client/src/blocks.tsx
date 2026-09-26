/**
 * What each kind of block on the home board draws.
 *
 * Every figure here comes from something the machine actually reports. A block cannot show a
 * number nothing measures, so there is no packet loss and no direct-or-relayed line: neither
 * crosses into this window, and a panel that showed them would be showing a guess.
 *
 * Nothing here decides anything. The home window holds the state and passes it down; a block
 * reads it and says what it is for.
 */

import type { JSX, ReactNode } from 'react';

import type { AccountDeviceView, Block, HostSnapshot, Session, Settings, StreamState } from './api.js';
import { KINDS, fade } from './board.js';
import { ago, latency, span, when } from './format.js';
import { t } from './i18n.js';
import { Platform } from './marks.js';

/** Everything a block may read, gathered once by the window rather than fetched block by block. */
export interface Ground {
  /** Every machine on the account. */
  readonly devices: readonly AccountDeviceView[];
  /** The ones that can be watched right now, in the order they are listed. */
  readonly machines: readonly string[];
  /** What the stream process is doing. */
  readonly stream: StreamState;
  /** What this machine's own session is doing, or `null` when it is not shared. */
  readonly mine: HostSnapshot | null;
  /** What has been watched, newest first. */
  readonly history: readonly Session[];
  /** What somebody has chosen. */
  readonly settings: Settings | null;
  /** What this machine would send, as one line. */
  readonly specs: string;
  /** What a machine is called. */
  readonly nameOf: (key: string) => string;
  /** Which operating system a machine runs, or empty. */
  readonly platformOf: (key: string) => string;
  /** Opens a session with a machine. */
  readonly watch: (key: string) => void;
  /** Turns this machine's sharing on or off. */
  readonly flip: () => void;
  /** Opens the settings. */
  readonly tune: () => void;
  /** Changes one of the terms this machine is shared on. */
  readonly retune: (patch: Partial<Settings>) => void;
}

/** The three states a machine is ever in, as far as this window can tell. */
type Standing = 'watching' | 'shared' | 'off';

/**
 * How a machine is doing.
 *
 * Three states and no more, because three is all the account knows: the one being watched right
 * now, the ones that say they are shared, and the rest.
 *
 * @param {string} key - The machine's public key.
 * @param {Ground} ground - What the window knows.
 * @returns {Standing} Which of the three it is in.
 */
export function standing(key: string, ground: Ground): Standing {
  if (ground.stream.host === key && ground.stream.phase === 'streaming') {
    return 'watching';
  }

  return ground.machines.includes(key) ? 'shared' : 'off';
}

/** What each standing is called. */
const SAID: Readonly<Record<Standing, string>> = {
  watching: 'Watching now',
  shared: 'Shared',
  off: 'Off',
};

/** The colour each standing is said in. */
const TONE: Readonly<Record<Standing, string>> = {
  watching: 'text-mint',
  shared: 'text-mint',
  off: 'text-dim',
};

/**
 * The line under a machine's name: what it is doing, and the figures for it.
 *
 * @param {string} key - The machine.
 * @param {Ground} ground - What the window knows.
 * @param {readonly string[]} fields - Which figures the block was told to show.
 * @returns {string} One line, the parts divided by bars.
 */
function machineLine(key: string, ground: Ground, fields: readonly string[]): string {
  const how = standing(key, ground);
  const parts: string[] = [t(SAID[how])];
  const stats = how === 'watching' ? ground.stream.stats : null;
  const seen = ground.history.find((one) => one.host === key) ?? null;

  if (stats) {
    if (fields.includes('fps')) {
      parts.push(`${stats.fps.toFixed(0)} fps`);
    }

    if (fields.includes('mbps')) {
      parts.push(`${stats.mbps.toFixed(0)} Mbps`);
    }

    if (fields.includes('rtt')) {
      parts.push(`${latency(stats.rttMs)} ms`);
    }
  } else if (seen && fields.includes('seen')) {
    parts.push(t('Last seen {when}', { when: ago(seen.endedAt) }));
  }

  return parts.join('  |  ');
}

/**
 * A block's heading, which every kind but the machine tile wears.
 *
 * @param {object} props - What to draw.
 * @param {string} props.title - What the block is.
 * @param {ReactNode} [props.trailing] - The count or figure at the far end.
 * @returns {JSX.Element} The row.
 */
function Head({ title, trailing }: { title: string; trailing?: ReactNode }): JSX.Element {
  return (
    <div className="flex flex-none items-center justify-between gap-3">
      <span className="truncate text-fine font-medium text-muted-2">{title}</span>
      {trailing !== undefined && (
        <span className="flex-none text-tiny text-dim font-mono tabular-nums">{trailing}</span>
      )}
    </div>
  );
}

/**
 * One labelled figure, as the smaller blocks stack them.
 *
 * @param {object} props - What to draw.
 * @param {string} props.name - What it is.
 * @param {ReactNode} props.value - What it reads.
 * @param {string} [props.tone] - The colour for the value.
 * @returns {JSX.Element} The row.
 */
function Pair({
  name,
  value,
  tone = 'text-ink',
}: {
  name: string;
  value: ReactNode;
  tone?: string;
}): JSX.Element {
  return (
    <div className="flex items-center justify-between gap-3">
      <span className="truncate text-note text-ink-3">{name}</span>
      <span className={`flex-none text-note font-mono tabular-nums ${tone}`}>{value}</span>
    </div>
  );
}

/**
 * One machine, given a tile of its own.
 *
 * The whole tile is the way in, because the one thing somebody does with a machine is open it.
 *
 * @param {object} props - What to draw.
 * @param {Block} props.block - Which block this is.
 * @param {Ground} props.ground - What the window knows.
 * @returns {JSX.Element} The tile.
 */
function Machine({ block, ground }: { block: Block; ground: Ground }): JSX.Element {
  const key = block.host;
  const how = standing(key, ground);
  const name = block.label || ground.nameOf(key);
  const face =
    block.accent.length > 1
      ? `linear-gradient(130deg, ${block.accent
          .map((colour, at) => fade(colour, 0.52 - at * 0.09))
          .join(', ')})`
      : `linear-gradient(130deg, ${fade(block.accent[0] ?? '#7c5cff', 0.5)}, ${fade(
          block.accent[0] ?? '#7c5cff',
          0.1,
        )})`;

  return (
    <button
      type="button"
      disabled={how === 'off'}
      aria-label={t('Watch {name}', { name })}
      onClick={() => {
        ground.watch(key);
      }}
      className="group relative block h-full w-full overflow-hidden text-left disabled:cursor-default"
    >
      <span
        aria-hidden
        className={`pointer-events-none absolute inset-0 block transition-opacity ${
          how === 'off' ? 'opacity-25' : 'opacity-100 group-hover:opacity-90'
        }`}
        style={{ background: face }}
      />

      <span className="relative flex h-full flex-col justify-end gap-[7px] p-[22px]">
        <span
          className={`flex items-center gap-[11px] truncate text-[26px] leading-none font-semibold tracking-[-0.5px] ${
            how === 'off' ? 'text-muted-2' : 'text-ink'
          }`}
        >
          <Platform platform={ground.platformOf(key)} size={24} />
          <span className="truncate">{name}</span>
        </span>
        <span
          className={`truncate text-note font-mono tabular-nums ${how === 'off' ? 'text-dim' : 'text-ink-3'}`}
        >
          {machineLine(key, ground, block.fields)}
        </span>
      </span>
    </button>
  );
}

/**
 * Every machine on the account, a line each.
 *
 * @param {object} props - What to draw.
 * @param {Block} props.block - Which block this is.
 * @param {Ground} props.ground - What the window knows.
 * @returns {JSX.Element} The list.
 */
function Machines({ block, ground }: { block: Block; ground: Ground }): JSX.Element {
  const others = ground.devices.filter((device) => !device.isThisMachine);

  return (
    <div className="flex h-full flex-col p-5">
      <Head
        title={block.label || t('Devices')}
        trailing={t('{count} machines', { count: String(others.length) })}
      />

      <div className="mt-3 min-h-0 flex-1 overflow-y-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
        {others.length === 0 ? (
          <p className="m-0 py-4 text-note text-dim">{t('Nothing to watch yet')}</p>
        ) : (
          others.map((device) => {
            const how = standing(device.publicKey, ground);
            const seen = ground.history.find((one) => one.host === device.publicKey) ?? null;
            const stats = how === 'watching' ? ground.stream.stats : null;

            return (
              <button
                key={device.publicKey}
                type="button"
                disabled={how === 'off'}
                onClick={() => {
                  ground.watch(device.publicKey);
                }}
                className="flex w-full items-center gap-3 border-t border-line-1 py-[11px] text-left first:border-t-0 enabled:hover:bg-wash-1 disabled:cursor-default"
              >
                <span
                  className={`flex min-w-0 flex-1 items-center gap-[9px] truncate text-row font-medium ${
                    how === 'off' ? 'text-muted-2' : 'text-ink'
                  }`}
                >
                  <Platform platform={device.platform} size={15} />
                  <span className="truncate">{device.label || device.publicKey.slice(0, 8)}</span>
                </span>
                <span
                  className={`flex-none text-fine font-mono tabular-nums ${TONE[how]} ${
                    block.fields.includes('rtt') ? '' : 'hidden'
                  }`}
                >
                  {t(SAID[how])}
                  {stats && `  |  ${latency(stats.rttMs)} ms`}
                  {!stats && how === 'off' && seen && `  |  ${ago(seen.endedAt)}`}
                </span>
              </button>
            );
          })
        )}
      </div>
    </div>
  );
}

/**
 * This computer, and the one switch somebody came to flip.
 *
 * @param {object} props - What to draw.
 * @param {Block} props.block - Which block this is.
 * @param {Ground} props.ground - What the window knows.
 * @returns {JSX.Element} The block.
 */
function Mine({ block, ground }: { block: Block; ground: Ground }): JSX.Element {
  const on = ground.mine !== null && ground.mine.phase !== 'stopped' && ground.mine.phase !== 'failed';
  const opening = on && ground.mine?.local === null;

  return (
    <div className="flex h-full flex-col justify-between p-5">
      <div className="flex flex-col gap-2">
        <Head title={block.label || t('This machine')} />
        <span
          className={`truncate text-[26px] leading-none font-semibold tracking-[-0.5px] ${
            on ? 'text-mint' : 'text-muted-2'
          }`}
        >
          {ground.mine?.phase === 'failed'
            ? t('Sharing failed')
            : opening
              ? t('Opening')
              : on
                ? t('Shared')
                : t('Not shared')}
        </span>
      </div>

      <div className="flex items-center justify-between gap-3">
        <span className="min-w-0 flex-1 truncate text-fine text-dim font-mono tabular-nums">
          {block.fields.includes('screen') ? ground.specs : ''}
        </span>
        <button
          type="button"
          role="switch"
          aria-checked={on}
          aria-label={on ? t('Stop sharing') : t('Share this machine')}
          onClick={ground.flip}
          className={`relative h-[26px] w-11 flex-none rounded-pill transition-colors ${
            on ? 'bg-violet' : 'bg-wash-4'
          }`}
        >
          <span
            className={`absolute top-1 block size-[18px] rounded-pill bg-white transition-[left] ${
              on ? 'left-[22px]' : 'left-1'
            }`}
          />
        </button>
      </div>
    </div>
  );
}

/**
 * What has been watched lately.
 *
 * How a session ended is not among the columns, because nothing records it: what is kept is
 * when it started, when it stopped and what the round trip averaged.
 *
 * @param {object} props - What to draw.
 * @param {Block} props.block - Which block this is.
 * @param {Ground} props.ground - What the window knows.
 * @returns {JSX.Element} The list.
 */
function Sessions({ block, ground }: { block: Block; ground: Ground }): JSX.Element {
  return (
    <div className="flex h-full flex-col p-5">
      <Head
        title={block.label || t('Recent sessions')}
        trailing={String(ground.history.length)}
      />

      <div className="mt-3 min-h-0 flex-1 overflow-y-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
        {ground.history.length === 0 ? (
          <p className="m-0 py-4 text-note text-dim">
            {t('Every session you end is listed here, with what it came to.')}
          </p>
        ) : (
          ground.history.map((one) => (
            <button
              key={`${one.host}-${one.startedAt}`}
              type="button"
              disabled={!ground.machines.includes(one.host)}
              onClick={() => {
                ground.watch(one.host);
              }}
              className="flex w-full items-center gap-3 border-t border-line-1 py-[11px] text-left first:border-t-0 enabled:hover:bg-wash-1 disabled:cursor-default"
            >
              <span className="flex min-w-0 flex-1 items-center gap-[9px] truncate text-row font-medium text-ink-3">
                <Platform platform={ground.platformOf(one.host)} size={15} />
                <span className="truncate">{ground.nameOf(one.host)}</span>
              </span>
              <span className="flex-none text-fine text-dim font-mono tabular-nums">
                {block.fields.includes('started') && when(one.endedAt)}
                {block.fields.includes('length') && `  |  ${span(one.endedAt - one.startedAt)}`}
                {block.fields.includes('rtt') && `  |  ${latency(one.rttMs)} ms`}
              </span>
            </button>
          ))
        )}
      </div>
    </div>
  );
}

/**
 * What this machine is shared on.
 *
 * Reads the settings and says so. Changing them is one click away rather than in here: a slider
 * that rewrites the encoder's terms belongs where the rest of the settings are, beside the
 * warnings about what each one costs.
 *
 * @param {object} props - What to draw.
 * @param {Block} props.block - Which block this is.
 * @param {Ground} props.ground - What the window knows.
 * @returns {JSX.Element} The block.
 */
function Terms({ block, ground }: { block: Block; ground: Ground }): JSX.Element {
  const megabits = Math.round((ground.settings?.bitrateBps ?? 40_000_000) / 1e6);
  const share = Math.min(100, Math.max(0, ((megabits - BITRATE_LEAST) / (BITRATE_MOST - BITRATE_LEAST)) * 100));

  return (
    <div className="flex h-full flex-col gap-[18px] p-5">
      <Head title={block.label || t('Sharing terms')} />

      {block.fields.includes('fps') && (
        <Pair name={t('Frame rate')} value={`${ground.settings?.fps ?? 60} fps`} />
      )}

      {/* Dragged here rather than only read here. Bitrate is the one of these that gets moved
          while somebody is watching the picture it changes — too soft, push it up; the link
          cannot hold it, pull it down — and a figure you have to open the settings to touch is
          one you tune once and then live with. */}
      {block.fields.includes('bitrate') && (
        <div className="flex flex-col gap-3">
          <Pair name={t('Bitrate')} value={`${megabits} Mbps`} />
          <span className="relative block h-[18px] w-full">
            <span className="pointer-events-none absolute top-1.5 block h-1.5 w-full rounded-pill bg-wash-4" />
            <span
              className="pointer-events-none absolute top-1.5 block h-1.5 rounded-pill bg-violet"
              style={{ width: `${share}%` }}
            />
            <span
              className="pointer-events-none absolute top-0 block size-[18px] -translate-x-1/2 rounded-pill border-2 border-white bg-violet"
              style={{ left: `${share}%` }}
            />
            {/* The control itself, laid over the drawing of it and invisible. Styling a range
                input to look like this means styling three pseudo-elements per engine, and the
                fill cannot be one of them because its width is a value. */}
            <input
              type="range"
              min={BITRATE_LEAST}
              max={BITRATE_MOST}
              step={1}
              aria-label={t('Bitrate')}
              value={megabits}
              onChange={(event) => {
                ground.retune({ bitrateBps: Number(event.target.value) * 1e6 });
              }}
              className="absolute inset-0 w-full cursor-pointer opacity-0"
            />
          </span>
        </div>
      )}

      {block.fields.includes('bind') && (
        <Pair name={t('Listen on')} value={ground.settings?.bind ?? ''} tone="text-muted-2" />
      )}

      <button
        type="button"
        onClick={ground.tune}
        className="mt-auto self-start text-fine text-dim transition-colors hover:text-ink-3"
      >
        {t('All settings')}
      </button>
    </div>
  );
}

/** The least this machine may be asked to spend, in megabits a second. */
const BITRATE_LEAST = 1;

/**
 * And the most.
 *
 * The same ceiling the settings offer, so the slider and the field beside it cannot disagree
 * about what the range is.
 */
const BITRATE_MOST = 200;

/**
 * How the session that is open is doing.
 *
 * @param {object} props - What to draw.
 * @param {Block} props.block - Which block this is.
 * @param {Ground} props.ground - What the window knows.
 * @returns {JSX.Element} The block.
 */
function Link({ block, ground }: { block: Block; ground: Ground }): JSX.Element {
  const stats = ground.stream.phase === 'streaming' ? ground.stream.stats : null;

  return (
    <div className="flex h-full flex-col gap-3 p-5">
      <Head title={block.label || t('Connection')} />

      <div className="flex items-baseline gap-2.5">
        <span className="text-[38px] leading-none font-semibold tracking-[-1px] text-ink font-mono tabular-nums">
          {stats ? latency(stats.rttMs) : '—'}
        </span>
        <span className="text-note font-medium text-muted-2">{t('ms round trip')}</span>
      </div>

      <div className="mt-auto flex flex-col gap-2.5">
        {block.fields.includes('fps') && (
          <Pair
            name={t('Frame rate')}
            value={stats ? `${stats.fps.toFixed(0)} fps` : '—'}
            tone={stats ? 'text-mint' : 'text-dim'}
          />
        )}
        {block.fields.includes('mbps') && (
          <Pair
            name={t('Arriving')}
            value={stats ? `${stats.mbps.toFixed(0)} Mbps` : '—'}
            tone={stats ? 'text-ink' : 'text-dim'}
          />
        )}
        {block.fields.includes('frames') && (
          <Pair
            name={t('Frames')}
            value={stats ? stats.frames.toLocaleString() : '—'}
            tone={stats ? 'text-ink' : 'text-dim'}
          />
        )}
      </div>
    </div>
  );
}

/**
 * Whatever the block is.
 *
 * @param {object} props - What to draw.
 * @param {Block} props.block - Which block this is.
 * @param {Ground} props.ground - What the window knows.
 * @returns {JSX.Element} Its contents.
 */
export function Body({ block, ground }: { block: Block; ground: Ground }): JSX.Element {
  if (block.kind === 'machine') {
    return <Machine block={block} ground={ground} />;
  }

  if (block.kind === 'machines') {
    return <Machines block={block} ground={ground} />;
  }

  if (block.kind === 'mine') {
    return <Mine block={block} ground={ground} />;
  }

  if (block.kind === 'sessions') {
    return <Sessions block={block} ground={ground} />;
  }

  if (block.kind === 'terms') {
    return <Terms block={block} ground={ground} />;
  }

  return <Link block={block} ground={ground} />;
}

/**
 * What a block is called where somebody is choosing or editing one.
 *
 * @param {Block} block - The block.
 * @param {Ground} ground - What the window knows.
 * @returns {string} Its name.
 */
export function titleOf(block: Block, ground: Ground): string {
  if (block.label !== '') {
    return block.label;
  }

  if (block.kind === 'machine') {
    return ground.nameOf(block.host);
  }

  return t(KINDS[block.kind].name);
}
