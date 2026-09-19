/**
 * Arranging the home board.
 *
 * The board is a pegboard: while it is being edited the holes show, and a block can only ever
 * land on them. Dragging reports where a block would go rather than moving it as the pointer
 * moves, so what is seen during a drag is the arrangement that will exist when it ends — there
 * is no moment where a block sits between two holes and then jumps.
 *
 * None of this talks to the machine. It changes an arrangement and hands it back; the window
 * decides when that arrangement is worth writing down.
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import type { JSX, PointerEvent as ReactPointerEvent, ReactNode } from 'react';

import type { Block, BlockKind } from './api.js';
import { ACCENTS, DOT, GUTTER, KINDS, OFFERED, STEP, held, make, wash } from './board.js';
import { t } from './i18n.js';
import { Platform } from './marks.js';

/** What a drag is doing to a block. */
type Handle = 'move' | 'size';

/**
 * The holes, drawn once and repeated.
 *
 * A background image rather than a thousand elements: a board twelve hundred pixels wide has
 * close to a thousand holes in it, and a thousand nodes that do nothing but sit there is a
 * thousand nodes the browser lays out on every frame of a drag.
 *
 * @param {object} props - What to draw.
 * @param {boolean} props.shown - Whether the board is being arranged.
 * @returns {JSX.Element} The lattice.
 */
export function Pegboard({ shown }: { shown: boolean }): JSX.Element {
  return (
    <div
      aria-hidden
      className={`pointer-events-none absolute inset-0 transition-opacity duration-200 ${
        shown ? 'opacity-100' : 'opacity-0'
      }`}
      style={{
        backgroundImage: `radial-gradient(rgba(255,255,255,0.32) ${DOT / 2}px, transparent ${DOT / 2}px)`,
        backgroundSize: `${STEP}px ${STEP}px`,
        backgroundPosition: `${-DOT / 2}px ${-DOT / 2}px`,
      }}
    />
  );
}

/**
 * One block on the board, placed and — while the board is being arranged — draggable.
 *
 * @param {object} props - What to draw and what it may do.
 * @param {Block} props.block - Where it is and what it is.
 * @param {boolean} props.editing - Whether the board is being arranged.
 * @param {boolean} props.picked - Whether this is the one being worked on.
 * @param {number} props.across - The last hole's number.
 * @param {(block: Block) => void} props.onChange - Called with where the drag ended.
 * @param {() => void} props.onPick - Called when it is taken hold of.
 * @param {() => void} props.onEdit - Called when its own button is pressed.
 * @param {ReactNode} props.children - What it draws inside.
 * @returns {JSX.Element} The block.
 */
export function Frame({
  block,
  editing,
  picked,
  across,
  onChange,
  onPick,
  onEdit,
  children,
}: {
  block: Block;
  editing: boolean;
  picked: boolean;
  across: number;
  onChange: (block: Block) => void;
  onPick: () => void;
  onEdit: () => void;
  children: ReactNode;
}): JSX.Element {
  /** Where the block would land if the drag ended now, while one is happening. */
  const [going, setGoing] = useState<Block | null>(null);
  const from = useRef<{ x: number; y: number; block: Block; handle: Handle } | null>(null);

  const take = useCallback(
    (event: ReactPointerEvent<HTMLElement>, handle: Handle): void => {
      if (!editing) {
        return;
      }

      event.preventDefault();
      event.stopPropagation();
      onPick();
      from.current = { x: event.clientX, y: event.clientY, block, handle };
      setGoing(block);
      (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    },
    [block, editing, onPick],
  );

  useEffect(() => {
    if (going === null) {
      return;
    }

    const moved = (event: PointerEvent): void => {
      const start = from.current;

      if (!start) {
        return;
      }

      const dx = Math.round((event.clientX - start.x) / STEP);
      const dy = Math.round((event.clientY - start.y) / STEP);

      setGoing(
        held(
          start.handle === 'move'
            ? { ...start.block, x: start.block.x + dx, y: start.block.y + dy }
            : { ...start.block, w: start.block.w + dx, h: start.block.h + dy },
          across,
        ),
      );
    };

    const dropped = (): void => {
      setGoing((last) => {
        if (last) {
          onChange(last);
        }

        return null;
      });
      from.current = null;
    };

    window.addEventListener('pointermove', moved);
    window.addEventListener('pointerup', dropped);
    window.addEventListener('pointercancel', dropped);

    return () => {
      window.removeEventListener('pointermove', moved);
      window.removeEventListener('pointerup', dropped);
      window.removeEventListener('pointercancel', dropped);
    };
  }, [going !== null, across, onChange]);

  const at = going ?? block;
  const box = {
    left: block.x * STEP,
    top: block.y * STEP,
    width: block.w * STEP,
    height: block.h * STEP,
  };

  return (
    <>
      <div
        style={box}
        onPointerDown={(event) => {
          take(event, 'move');
        }}
        className={`absolute overflow-hidden rounded-card border transition-[border-color,opacity] ${
          editing
            ? picked
              ? 'z-[2] border-2 border-violet'
              : 'z-[1] border border-dashed border-[rgba(255,255,255,0.25)]'
            : 'border-line-4'
        } ${editing ? 'cursor-grab bg-[rgba(5,5,7,0.94)] active:cursor-grabbing' : 'bg-[rgba(5,5,7,0.6)]'} ${
          going ? 'opacity-45' : ''
        }`}
      >
        <div className={editing ? 'pointer-events-none h-full' : 'h-full'}>{children}</div>

        {editing && (
          <button
            type="button"
            onPointerDown={(event) => {
              event.stopPropagation();
            }}
            onClick={(event) => {
              event.stopPropagation();
              onEdit();
            }}
            className={`absolute top-4 right-4 z-[2] rounded-pill px-3 py-1.5 text-tiny font-medium transition-colors ${
              picked
                ? 'bg-violet text-white'
                : 'border border-[rgba(255,255,255,0.18)] bg-[rgba(0,0,0,0.5)] text-ink-3 hover:text-ink'
            }`}
          >
            {t('Edit')}
          </button>
        )}

        {editing && picked && (
          <span
            onPointerDown={(event) => {
              take(event, 'size');
            }}
            className="absolute right-0 bottom-0 z-[2] size-8 cursor-nwse-resize"
          >
            <span className="absolute right-2 bottom-2 block h-[3px] w-6 rounded-[2px] bg-violet" />
            <span className="absolute right-2 bottom-2 block h-6 w-[3px] rounded-[2px] bg-violet" />
          </span>
        )}
      </div>

      {/* Where it would land. Drawn as its own element rather than by moving the block, so that
          what is under the pointer during a drag is the arrangement and not a block halfway to
          somewhere. */}
      {going && (
        <div
          aria-hidden
          style={{
            left: at.x * STEP,
            top: at.y * STEP,
            width: at.w * STEP,
            height: at.h * STEP,
          }}
          className="pointer-events-none absolute z-[3] rounded-card border-2 border-dashed border-violet bg-[rgba(124,92,255,0.16)]"
        >
          <span className="absolute -bottom-9 left-1/2 -translate-x-1/2 rounded-badge bg-violet px-3 py-1.5 text-tiny font-medium text-white font-mono tabular-nums">
            {at.w} × {at.h}
          </span>
        </div>
      )}
    </>
  );
}

/**
 * A colour as a hex string, from one turn around the wheel.
 *
 * @param {number} hue - Where on the wheel, between 0 and 360.
 * @returns {string} A `#rrggbb` colour at the saturation and lightness the palette uses.
 */
export function hueToHex(hue: number): string {
  const saturation = 0.78;
  const lightness = 0.66;
  const chroma = (1 - Math.abs(2 * lightness - 1)) * saturation;
  const second = chroma * (1 - Math.abs(((hue / 60) % 2) - 1));
  const low = lightness - chroma / 2;
  const sixth = Math.floor(hue / 60) % 6;
  const wheel: readonly [number, number, number][] = [
    [chroma, second, 0],
    [second, chroma, 0],
    [0, chroma, second],
    [0, second, chroma],
    [second, 0, chroma],
    [chroma, 0, second],
  ];
  const [red, green, blue] = wheel[sixth] as [number, number, number];

  return `#${[red, green, blue]
    .map((part) => Math.round((part + low) * 255).toString(16).padStart(2, '0'))
    .join('')}`;
}

/**
 * The colours a block is painted in: one of them, or several across a gradient.
 *
 * @param {object} props - What to draw.
 * @param {readonly string[]} props.accent - The colours as they stand.
 * @param {(accent: readonly string[]) => void} props.onChange - Called with the new ones.
 * @returns {JSX.Element} The picker.
 */
function Accent({
  accent,
  onChange,
}: {
  accent: readonly string[];
  onChange: (accent: readonly string[]) => void;
}): JSX.Element {
  const many = accent.length > 1;
  const [at, setAt] = useState(0);
  const point = Math.min(at, accent.length - 1);

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-3">
        <span className="text-tiny font-medium text-muted-2">{t('Accent')}</span>
        <div className="flex items-center gap-0.5 rounded-pill bg-wash-3 p-[3px]">
          <button
            type="button"
            aria-pressed={!many}
            onClick={() => {
              onChange([accent[point] ?? ACCENTS[0]] as readonly string[]);
              setAt(0);
            }}
            className={`rounded-pill px-3 py-1 text-tiny font-medium ${
              many ? 'text-dim' : 'bg-wash-4 text-ink'
            }`}
          >
            {t('Flat')}
          </button>
          <button
            type="button"
            aria-pressed={many}
            onClick={() => {
              if (!many) {
                onChange([accent[0] ?? ACCENTS[0], ACCENTS[4]] as readonly string[]);
              }
            }}
            className={`rounded-pill px-3 py-1 text-tiny font-medium ${
              many ? 'bg-wash-4 text-ink' : 'text-dim'
            }`}
          >
            {t('Gradient')}
          </button>
        </div>
      </div>

      <div
        className="h-9 w-full rounded-badge border border-line-4"
        style={{ background: wash(accent) }}
      />

      {many && (
        <div className="flex items-center gap-2">
          {accent.map((colour, index) => (
            <button
              key={`${colour}-${index}`}
              type="button"
              aria-pressed={index === point}
              aria-label={t('Point {n}', { n: String(index + 1) })}
              onClick={() => {
                setAt(index);
              }}
              className={`size-6 flex-none rounded-badge border-2 ${
                index === point ? 'border-violet' : 'border-white/80'
              }`}
              style={{ background: colour }}
            />
          ))}

          <button
            type="button"
            onClick={() => {
              const next = [...accent];
              next.splice(point + 1, 0, ACCENTS[(point + 1) % ACCENTS.length] as string);
              onChange(next);
              setAt(point + 1);
            }}
            className="rounded-pill bg-violet px-3 py-1.5 text-tiny font-medium text-white"
          >
            {t('Add point')}
          </button>

          <button
            type="button"
            disabled={accent.length <= 2}
            onClick={() => {
              const next = accent.filter((_, index) => index !== point);
              onChange(next);
              setAt(Math.max(0, point - 1));
            }}
            className="rounded-pill border border-[rgba(255,92,168,0.4)] px-3 py-1.5 text-tiny font-medium text-rose disabled:opacity-40"
          >
            {t('Remove point')}
          </button>
        </div>
      )}

      {/* The wheel, for the point being worked on. The five named colours sit under it because
          a board where every block took its colour from the same five reads as one design, and
          anything else on the wheel is there for the person who wants it. */}
      <input
        type="range"
        min={0}
        max={359}
        aria-label={t('Colour')}
        onChange={(event) => {
          const next = [...accent];
          next[point] = hueToHex(Number(event.target.value));
          onChange(next);
        }}
        className="h-4 w-full cursor-pointer appearance-none rounded-pill"
        style={{
          background:
            'linear-gradient(90deg, #ff2f59, #ffb05c, #4de8b0, #35d6ff, #7c5cff, #ff5ca8, #ff2f59)',
        }}
      />

      <div className="flex items-center gap-2">
        {ACCENTS.map((colour) => (
          <button
            key={colour}
            type="button"
            aria-label={colour}
            onClick={() => {
              const next = [...accent];
              next[point] = colour;
              onChange(next);
            }}
            className={`size-[26px] flex-none rounded-pill border-2 ${
              accent[point] === colour ? 'border-white' : 'border-transparent'
            }`}
            style={{ background: colour }}
          />
        ))}
      </div>
    </div>
  );
}

/**
 * What one block shows, and what it is called.
 *
 * @param {object} props - What to draw.
 * @param {Block} props.block - The block being worked on.
 * @param {string} props.title - What it is called when it has no name of its own.
 * @param {(block: Block) => void} props.onChange - Called with the block as it now is.
 * @param {() => void} props.onRemove - Called to take it off the board.
 * @param {() => void} props.onClose - Called to put the panel away.
 * @returns {JSX.Element} The panel.
 */
export function BlockPanel({
  block,
  title,
  onChange,
  onRemove,
  onClose,
}: {
  block: Block;
  title: string;
  onChange: (block: Block) => void;
  onRemove: () => void;
  onClose: () => void;
}): JSX.Element {
  const shape = KINDS[block.kind];

  return (
    <div
      className="fixed inset-0 z-[6] grid place-items-center bg-[rgba(6,6,10,0.62)] p-6 backdrop-blur-[3px]"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div className="max-h-full w-full max-w-[404px] overflow-y-auto overscroll-contain rounded-card border border-line-4 bg-[rgba(17,17,22,0.98)] shadow-[0_24px_60px_rgba(0,0,0,0.6)] [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
        <div className="flex items-center justify-between gap-3 px-5 pt-[18px] pb-4">
          <span className="flex min-w-0 flex-col gap-1">
            <span className="text-tiny font-medium text-muted-2">{t('Edit block')}</span>
            <span className="truncate text-heading font-semibold text-ink">{title}</span>
          </span>
          <button type="button" className="btn-secondary flex-none" onClick={onClose}>
            {t('Close')}
          </button>
        </div>

        <div className="flex flex-col gap-2.5 border-t border-line-1 px-5 py-4">
          <span className="text-tiny font-medium text-muted-2">{t('Name')}</span>
          <input
            type="text"
            value={block.label}
            placeholder={title}
            spellCheck={false}
            onChange={(event) => {
              onChange({ ...block, label: event.target.value });
            }}
            className="w-full rounded-panel border border-line-4 bg-wash-2 px-3 py-2.5 text-control text-ink placeholder:text-dim focus:border-violet focus:outline-none"
          />
        </div>

        <div className="flex items-center justify-between gap-3 border-t border-line-1 px-5 py-4">
          <span className="text-tiny font-medium text-muted-2">{t('Size')}</span>
          <span className="text-control text-ink font-mono tabular-nums">
            {block.w} × {block.h}
          </span>
        </div>

        {shape.fields.length > 0 && (
          <div className="flex flex-col gap-1 border-t border-line-1 px-5 py-4">
            <span className="mb-1.5 text-tiny font-medium text-muted-2">{t('What it shows')}</span>
            {shape.fields.map((field) => {
              const on = block.fields.includes(field);

              return (
                <button
                  key={field}
                  type="button"
                  role="switch"
                  aria-checked={on}
                  onClick={() => {
                    onChange({
                      ...block,
                      fields: on
                        ? block.fields.filter((one) => one !== field)
                        : [...block.fields, field],
                    });
                  }}
                  className="flex items-center justify-between gap-3 py-[7px]"
                >
                  <span className={`text-note ${on ? 'text-ink-3' : 'text-dim'}`}>
                    {t(FIELD_NAMES[field] ?? field)}
                  </span>
                  <span
                    className={`relative block h-[21px] w-9 flex-none rounded-pill transition-colors ${
                      on ? 'bg-violet' : 'bg-wash-4'
                    }`}
                  >
                    <span
                      className={`absolute top-[3px] block size-[15px] rounded-pill bg-white transition-[left] ${
                        on ? 'left-[18px]' : 'left-[3px]'
                      }`}
                    />
                  </span>
                </button>
              );
            })}
          </div>
        )}

        <div className="border-t border-line-1 px-5 py-4">
          <Accent
            accent={block.accent}
            onChange={(accent) => {
              onChange({ ...block, accent });
            }}
          />
        </div>

        <div className="flex items-center justify-between gap-3 border-t border-line-1 px-5 py-4">
          <button type="button" className="btn-danger px-3.5 py-2.5 text-note" onClick={onRemove}>
            {t('Remove this block')}
          </button>
          <button type="button" className="btn-primary-sm" onClick={onClose}>
            {t('Done')}
          </button>
        </div>
      </div>
    </div>
  );
}

/** What each figure a block can show is called. */
const FIELD_NAMES: Readonly<Record<string, string>> = {
  fps: 'Frame rate',
  mbps: 'Bandwidth',
  rtt: 'Round trip',
  seen: 'Last seen',
  state: 'State',
  screen: 'Screen',
  bitrate: 'Bitrate',
  bind: 'Listen on',
  started: 'Started',
  length: 'Length',
  frames: 'Frames',
};

/**
 * The blocks there are to add, and which machine a machine block is about.
 *
 * @param {object} props - What to draw.
 * @param {readonly Block[]} props.board - What is already on the board.
 * @param {number} props.across - The last hole's number.
 * @param {readonly AccountLike[]} props.machines - The machines a tile could be about.
 * @param {(block: Block) => void} props.onAdd - Called with the block to put down.
 * @param {() => void} props.onClose - Called to put the panel away.
 * @returns {JSX.Element} The panel.
 */
export function AddPanel({
  board,
  across,
  machines,
  onAdd,
  onClose,
}: {
  board: readonly Block[];
  across: number;
  machines: readonly { key: string; name: string; platform: string }[];
  onAdd: (block: Block) => void;
  onClose: () => void;
}): JSX.Element {
  const [kind, setKind] = useState<BlockKind>('machine');
  const [host, setHost] = useState(machines[0]?.key ?? '');
  const shape = KINDS[kind];

  return (
    <div
      className="fixed inset-0 z-[6] grid place-items-center bg-[rgba(6,6,10,0.64)] p-6 backdrop-blur-[3px]"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div className="max-h-full w-full max-w-[788px] overflow-y-auto overscroll-contain rounded-card border border-line-4 bg-[rgba(17,17,22,0.98)] shadow-[0_28px_70px_rgba(0,0,0,0.6)] [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
        <div className="flex flex-col gap-1.5 px-[26px] pt-6 pb-5">
          <h2 className="m-0 text-[20px] leading-none font-semibold tracking-[-0.3px] text-ink">
            {t('Add a block')}
          </h2>
          <p className="m-0 text-fine text-dim">{t('It goes wherever there is room for it.')}</p>
        </div>

        <div className="grid grid-cols-[repeat(auto-fill,minmax(210px,1fr))] gap-3.5 px-[26px] pb-6">
          {OFFERED.map((one) => {
            const on = one === kind;

            return (
              <button
                key={one}
                type="button"
                aria-pressed={on}
                onClick={() => {
                  setKind(one);
                }}
                className={`flex flex-col gap-3 rounded-panel border p-3.5 text-left transition-colors ${
                  on ? 'border-2 border-violet bg-wash-3' : 'border-line-2 bg-wash-1 hover:bg-wash-2'
                }`}
              >
                <span
                  aria-hidden
                  className="block h-[72px] w-full rounded-badge border border-line-1"
                  style={{
                    background: `linear-gradient(130deg, ${ACCENTS[OFFERED.indexOf(one) % ACCENTS.length]}55, transparent)`,
                  }}
                />
                <span className="flex items-baseline justify-between gap-2">
                  <span className="truncate text-control font-medium text-ink">
                    {t(KINDS[one].name)}
                  </span>
                  <span className="flex-none text-tiny text-muted-2 font-mono tabular-nums">
                    {KINDS[one].size[0]} × {KINDS[one].size[1]}
                  </span>
                </span>
                <span className="text-fine text-muted-2">{t(KINDS[one].about)}</span>
              </button>
            );
          })}
        </div>

        {shape.ofMachine && (
          <div className="flex flex-col gap-2.5 border-t border-line-1 px-[26px] py-4">
            <span className="text-tiny font-medium text-muted-2">{t('Which machine')}</span>
            {machines.length === 0 ? (
              <p className="m-0 text-note text-dim">{t('Nothing to watch yet')}</p>
            ) : (
              <div className="flex flex-wrap gap-2">
                {machines.map((one) => (
                  <button
                    key={one.key}
                    type="button"
                    aria-pressed={one.key === host}
                    onClick={() => {
                      setHost(one.key);
                    }}
                    className={`inline-flex items-center gap-2 rounded-pill border px-3.5 py-2 text-note font-medium ${
                      one.key === host
                        ? 'border-violet bg-wash-3 text-ink'
                        : 'border-line-2 text-ink-3 hover:text-ink'
                    }`}
                  >
                    <Platform platform={one.platform} size={14} />
                    <span className="truncate">{one.name}</span>
                  </button>
                ))}
              </div>
            )}
          </div>
        )}

        <div className="flex items-center justify-between gap-3 border-t border-line-1 px-[26px] py-4">
          <span className="text-note font-medium text-muted-2 font-mono tabular-nums">
            {t(shape.name)} · {shape.size[0]} × {shape.size[1]}
          </span>
          <span className="flex gap-2.5">
            <button type="button" className="btn-secondary" onClick={onClose}>
              {t('Cancel')}
            </button>
            <button
              type="button"
              className="btn-primary-sm"
              disabled={shape.ofMachine && host === ''}
              onClick={() => {
                onAdd(make(board, across, kind, shape.ofMachine ? host : ''));
              }}
            >
              {t('Add')}
            </button>
          </span>
        </div>
      </div>
    </div>
  );
}

/** The gap the board leaves around itself, in pixels. */
export const EDGE = GUTTER * STEP;
