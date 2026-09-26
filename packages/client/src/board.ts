/**
 * The board the home window's blocks sit on.
 *
 * A pegboard: a lattice of holes, and every block spanning one hole to another. Positions and
 * sizes are counted in holes here and turned into pixels only where they are drawn, so a block
 * is described the same way whatever the window is doing — and two blocks can never end up a
 * few pixels out of line with each other, because there is nowhere between two holes to be.
 *
 * Nothing in here touches the machine. It is arithmetic on an arrangement.
 */

import type { Block, BlockKind } from './api.js';

/** How far apart the holes are, in pixels. */
export const STEP = 32;

/** How many holes are left between two blocks, and around the edge of the board. */
export const GUTTER = 2;

/** How wide a hole's dot is drawn. */
export const DOT = 3;

/** What one kind of block is, apart from where it happens to be. */
export interface Kind {
  /** What it is called where somebody picks one. */
  readonly name: string;
  /** What it shows, in one line, where somebody picks one. */
  readonly about: string;
  /** How big it arrives, in holes. */
  readonly size: readonly [number, number];
  /** The smallest it may be dragged to, in holes. */
  readonly least: readonly [number, number];
  /** Which figures it can be told to show or hide. */
  readonly fields: readonly string[];
  /** Whether it is about one machine, and so needs one chosen. */
  readonly ofMachine: boolean;
}

/**
 * Every kind of block, in the order they are offered.
 *
 * Files are not among them on purpose. What moves between two machines only exists while a
 * session does, so a block for it on the home screen would be an empty frame every time it was
 * looked at — the window opens one of its own when there is something in it.
 */
export const KINDS: Readonly<Record<BlockKind, Kind>> = {
  machine: {
    name: 'One machine',
    about: 'A tile of its own, with its picture behind it',
    size: [17, 9],
    least: [8, 5],
    fields: ['fps', 'mbps', 'rtt', 'seen'],
    ofMachine: true,
  },
  machines: {
    name: 'Every machine',
    about: 'All of them, a line each',
    size: [22, 9],
    least: [10, 4],
    fields: ['state', 'rtt'],
    ofMachine: false,
  },
  mine: {
    name: 'This computer',
    about: 'Whether it is shared, and what it sends',
    size: [10, 6],
    least: [8, 4],
    fields: ['screen', 'fps'],
    ofMachine: false,
  },
  sessions: {
    name: 'Recent sessions',
    about: 'What was watched, when, and for how long',
    size: [16, 8],
    least: [10, 4],
    fields: ['started', 'length', 'rtt'],
    ofMachine: false,
  },
  terms: {
    name: 'Sharing terms',
    about: 'Frame rate, bitrate and where it listens',
    size: [10, 6],
    least: [8, 4],
    fields: ['fps', 'bitrate', 'bind'],
    ofMachine: false,
  },
  link: {
    name: 'Connection',
    about: 'Round trip, frame rate and what is arriving',
    size: [12, 9],
    least: [8, 5],
    fields: ['fps', 'mbps', 'frames'],
    ofMachine: false,
  },
};

/** The kinds, in the order the picker offers them. */
export const OFFERED: readonly BlockKind[] = [
  'machine',
  'machines',
  'mine',
  'sessions',
  'terms',
  'link',
];

/**
 * The colours a block can be marked with.
 *
 * The same five the rest of the window uses. A block may take one of them or several, and
 * several means a gradient across them in the order they are given.
 */
export const ACCENTS: readonly string[] = [
  '#35d6ff',
  '#4de8b0',
  '#ffb05c',
  '#ff5ca8',
  '#7c5cff',
];

/**
 * How many holes fit across a width.
 *
 * @param {number} pixels - How wide the board is.
 * @returns {number} The last hole's number, counting the first as zero.
 */
export function holes(pixels: number): number {
  return Math.max(1, Math.floor(pixels / STEP));
}

/**
 * Whether two blocks are standing on the same holes.
 *
 * @param {Block} one - The first.
 * @param {Block} two - The second.
 * @returns {boolean} Whether they overlap at all.
 */
export function clashes(one: Block, two: Block): boolean {
  return (
    one.x < two.x + two.w &&
    two.x < one.x + one.w &&
    one.y < two.y + two.h &&
    two.y < one.y + one.h
  );
}

/**
 * The first place a block of a given size will stand without touching any of the others.
 *
 * Searched top to bottom and left to right, which is where somebody looks for something that
 * has just been added. Returns the bottom of the board when nothing above it is free, so a
 * block is always placed somewhere rather than silently not added.
 *
 * @param {readonly Block[]} board - What is already there.
 * @param {number} across - The last hole's number.
 * @param {number} w - How many holes wide the new block is.
 * @param {number} h - How many holes tall.
 * @returns {{ x: number; y: number }} Where to put it.
 */
export function room(
  board: readonly Block[],
  across: number,
  w: number,
  h: number,
): { x: number; y: number } {
  const floor = board.reduce((low, one) => Math.max(low, one.y + one.h), 0);

  for (let y = GUTTER / 2; y <= floor; y += 1) {
    for (let x = 1; x + w <= across; x += 1) {
      const want = { x, y, w, h } as Block;

      if (!board.some((one) => clashes(one, want))) {
        return { x, y };
      }
    }
  }

  return { x: 1, y: floor + GUTTER };
}

/**
 * A name nothing else on the board is using.
 *
 * @param {readonly Block[]} board - What is already there.
 * @returns {string} The identifier for a new block.
 */
export function fresh(board: readonly Block[]): string {
  const taken = new Set(board.map((one) => one.id));

  for (let at = 1; ; at += 1) {
    const id = `b${at}`;

    if (!taken.has(id)) {
      return id;
    }
  }
}

/**
 * A block of a kind, sized and coloured the way that kind arrives.
 *
 * @param {readonly Block[]} board - What is already there, so the new one lands beside it.
 * @param {number} across - The last hole's number.
 * @param {BlockKind} kind - What it draws.
 * @param {string} [host] - Which machine, for the kinds that are about one.
 * @returns {Block} The new block.
 */
export function make(
  board: readonly Block[],
  across: number,
  kind: BlockKind,
  host = '',
): Block {
  const shape = KINDS[kind];
  const [w, h] = shape.size;
  const wide = Math.min(w, across - 2);
  const spot = room(board, across, wide, h);

  return {
    id: fresh(board),
    kind,
    host,
    x: spot.x,
    y: spot.y,
    w: wide,
    h,
    label: '',
    fields: [...shape.fields],
    accent: [ACCENTS[board.length % ACCENTS.length] as string],
  };
}

/**
 * The board a machine has before anybody has arranged one.
 *
 * Two machines get tiles across the top because a tile is the fastest thing to recognise and
 * the thing somebody came to click. Everything the account has goes in the list below, so a
 * machine that is not one of the two is still one click away rather than missing. What is left
 * of the row beside the list is how the current session is doing.
 *
 * @param {readonly string[]} hosts - The machines that can be watched, in the order they are listed.
 * @param {number} across - The last hole's number.
 * @returns {Block[]} A board to start from.
 */
export function startingBoard(hosts: readonly string[], across: number): Block[] {
  const inner = across - 2;
  const top = GUTTER / 2;
  const board: Block[] = [];

  if (hosts.length === 0) {
    board.push({
      id: 'b1',
      kind: 'mine',
      host: '',
      x: 1,
      y: top,
      w: Math.min(KINDS.mine.size[0], inner),
      h: KINDS.mine.size[1],
      label: '',
      fields: [...KINDS.mine.fields],
      accent: [ACCENTS[4] as string],
    });

    return board;
  }

  const tall = KINDS.machine.size[1];
  const each = hosts.length === 1 ? inner : Math.floor((inner - GUTTER) / 2);

  hosts.slice(0, 2).forEach((host, at) => {
    board.push({
      id: `b${at + 1}`,
      kind: 'machine',
      host,
      x: 1 + at * (each + GUTTER),
      y: top,
      w: each,
      h: tall,
      label: '',
      fields: [...KINDS.machine.fields],
      accent: [ACCENTS[at % ACCENTS.length] as string],
    });
  });

  const below = top + tall + GUTTER;
  const link = Math.min(KINDS.link.size[0], Math.max(8, Math.floor(inner / 3)));

  board.push({
    id: `b${board.length + 1}`,
    kind: 'machines',
    host: '',
    x: 1,
    y: below,
    w: inner - link - GUTTER,
    h: KINDS.machines.size[1],
    label: '',
    fields: [...KINDS.machines.fields],
    accent: [ACCENTS[2] as string],
  });

  board.push({
    id: `b${board.length + 1}`,
    kind: 'link',
    host: '',
    x: 1 + (inner - link - GUTTER) + GUTTER,
    y: below,
    w: link,
    h: KINDS.link.size[1],
    label: '',
    fields: [...KINDS.link.fields],
    accent: [ACCENTS[1] as string],
  });

  return board;
}

/**
 * A block held inside the board, at least as big as its kind allows.
 *
 * Applied to whatever a drag arrives at, so a block cannot be pushed off an edge or shrunk into
 * a line. Clamping rather than refusing, because a drag that stops responding at the edge reads
 * as the window having frozen.
 *
 * @param {Block} block - Where the drag has got to.
 * @param {number} across - The last hole's number.
 * @returns {Block} The same block, within its bounds.
 */
export function held(block: Block, across: number): Block {
  const [leastW, leastH] = KINDS[block.kind].least;
  const w = Math.max(leastW, Math.min(block.w, across));
  const h = Math.max(leastH, block.h);

  return {
    ...block,
    w,
    h,
    x: Math.max(0, Math.min(block.x, across - w)),
    y: Math.max(0, block.y),
  };
}

/**
 * How a block's colour is painted.
 *
 * One colour is a flat one; several are a gradient across them. Returned as a CSS value so the
 * caller can drop it into whichever property it is painting.
 *
 * @param {readonly string[]} accent - The block's colours.
 * @param {number} [alpha] - How much of it to let through, between 0 and 1.
 * @returns {string} A CSS colour or gradient.
 */
export function wash(accent: readonly string[], alpha = 1): string {
  const tinted = accent.map((colour) => fade(colour, alpha));

  if (tinted.length === 0) {
    return 'transparent';
  }

  if (tinted.length === 1) {
    return tinted[0] as string;
  }

  return `linear-gradient(120deg, ${tinted.join(', ')})`;
}

/**
 * One colour, softened.
 *
 * @param {string} colour - A `#rrggbb` colour.
 * @param {number} alpha - How much to let through, between 0 and 1.
 * @returns {string} The same colour as `rgba(...)`.
 */
export function fade(colour: string, alpha: number): string {
  const hex = colour.replace('#', '');
  const red = parseInt(hex.slice(0, 2), 16);
  const green = parseInt(hex.slice(2, 4), 16);
  const blue = parseInt(hex.slice(4, 6), 16);

  return `rgba(${red}, ${green}, ${blue}, ${alpha})`;
}
