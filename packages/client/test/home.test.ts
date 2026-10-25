import fs from 'node:fs';
import vm from 'node:vm';
import ts from 'typescript';
import { describe, expect, it, vi } from 'vitest';

/** Transpiles the actual renderer source for an isolated hook and IPC harness. */
function compile(name: string): string {
  return ts.transpileModule(fs.readFileSync(new URL(`../src/${name}`, import.meta.url), 'utf8'), {
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2023,
      jsx: ts.JsxEmit.ReactJSX,
    },
  }).outputText;
}

/** Drains startup reads without starting native processes or touching a profile. */
async function settle(): Promise<void> {
  for (let pass = 0; pass < 10; pass++) {
    await Promise.resolve();
  }
}

/** A JSX node whose children are inspected without running visual components. */
interface Element {
  type: string | ((props?: unknown) => Element);
  props: Record<string, any>;
}

/** Finds a component or control in the home screen's rendered tree. */
function nodes(tree: any, predicate: (node: Element) => boolean): Element[] {
  if (Array.isArray(tree)) {
    return tree.flatMap((node) => nodes(node, predicate));
  }
  if (!tree || typeof tree !== 'object' || !tree.props) {
    return [];
  }
  return [ ...(predicate(tree) ? [tree] : []), ...nodes(tree.props.children, predicate) ];
}

/** Runs Home's real effects while replacing OS calls and visual-only components. */
function home() {
  const settings = { language: 'en', board: [], boardInitialized: false };
  const stops = Array.from({ length: 5 }, () => vi.fn());
  const api = {
    identity: vi.fn(async () => ({ publicKey: 'mine' })),
    accountState: vi.fn(async () => ({ devices: [], email: 'test@example.invalid' })),
    getSettings: vi.fn(async () => settings),
    streamState: vi.fn(async () => ({
      phase: 'idle', host: null, terms: null, stats: null, departed: null,
      log: [], moving: [], offered: [], offeredMore: false, arrived: [],
    })),
    sharing: vi.fn(async () => null),
    sessions: vi.fn(async () => []),
    onSharing: vi.fn(() => stops[0]),
    onSessions: vi.fn(() => stops[1]),
    onStream: vi.fn(() => stops[2]),
    onAccount: vi.fn(() => stops[3]),
    onUpdate: vi.fn(() => stops[4]),
  };
  const state: any[] = [];
  let cursor = 0;
  let effects: (() => void | (() => void))[] = [];
  let component!: () => Element;
  const board = {};
  vm.runInNewContext(compile('board.ts'), { exports: board, crypto: globalThis.crypto });
  const visual = new Proxy({
    t: (text: string) => text,
    speak: () => {},
    useLanguage: () => 'en',
    short: (text: string) => text,
    reason: (error: Error) => error.message,
  }, { get: (target, key) => key in target ? target[key as keyof typeof target] : String(key) });
  const jsx = (type: Element['type'], props: Element['props']): Element => ({ type, props });
  vm.runInNewContext(compile('home.tsx'), {
    exports: {},
    require(name: string) {
      if (name === 'react') {
        return {
          StrictMode: 'StrictMode',
          useState(initial: unknown) {
            const index = cursor++;
            if (!(index in state)) state[index] = initial;
            return [state[index], (next: unknown) => {
              state[index] = typeof next === 'function' ? next(state[index]) : next;
            }];
          },
          useEffect(effect: () => void) { effects.push(effect); },
          useRef: (current: unknown) => ({ current }),
          useMemo: (calculate: () => unknown) => calculate(),
          useCallback: (callback: unknown) => callback,
        };
      }
      if (name === 'react/jsx-runtime') return { jsx, jsxs: jsx };
      if (name === 'react-dom/client') return {
        createRoot: () => ({ render: (element: Element) => {
          component = element.props.children.type;
        } }),
      };
      if (name === './board.js') return board;
      return visual;
    },
    window: { prism: api, screen: { width: 1280, height: 720 }, devicePixelRatio: 1 },
    document: { getElementById: () => ({}) },
  });

  /** Renders again with the state written by completed effects or user actions. */
  const render = (): Element => {
    cursor = 0;
    effects = [];
    return component();
  };

  /** Selects an actual effect by the API it exercises, avoiding hook position assumptions. */
  const effect = (contains: string) => {
    const found = effects.find((run) => String(run).includes(contains));
    if (!found) throw new Error(`missing home effect: ${contains}`);
    return found;
  };

  render();
  return { api, settings, state, stops, render, effect };
}

describe('home startup and board persistence', () => {
  it('keeps successful reads when session history fails, and retries from the screen', async () => {
    const app = home();
    app.api.sessions.mockRejectedValueOnce(new Error('history unavailable'));
    app.effect('prism.identity')();
    await settle();
    expect(app.state).toContain(app.settings);
    expect(app.state).toContain('history unavailable');
    const screen = app.render();
    const retry = nodes(screen, (node) => node.type === 'button' && node.props.children === 'Retry')[0];
    expect(retry).toBeDefined();
    retry.props.onClick();
    app.render();
    app.effect('prism.identity')();
    await settle();
    expect(app.api.sessions).toHaveBeenCalledTimes(2);
    expect(app.state).not.toContain('history unavailable');
  });

  it('does not apply reads that finish after the startup effect is cleaned up', async () => {
    const app = home();
    let finish!: (value: typeof app.settings) => void;
    app.api.getSettings.mockReturnValue(new Promise((resolve) => { finish = resolve; }));
    const cleanup = app.effect('prism.identity')() as () => void;
    cleanup();
    const before = [...app.state];
    finish(app.settings);
    await settle();
    expect(app.state).toEqual(before);
  });

  it('cleans up every home subscription', () => {
    const app = home();
    const cleanup = app.effect('prism.onSharing')() as () => void;
    cleanup();
    app.stops.forEach((stop) => { expect(stop).toHaveBeenCalledOnce(); });
  });

  it('reports a synchronous subscription failure while keeping the other subscriptions', () => {
    const app = home();
    app.api.onStream.mockImplementationOnce(() => { throw new Error('bridge unavailable'); });
    const cleanup = app.effect('prism.onSharing')() as () => void;
    expect(app.state).toContain('bridge unavailable');
    expect(app.api.onAccount).toHaveBeenCalledOnce();
    expect(app.api.onUpdate).toHaveBeenCalledOnce();
    cleanup();
    expect(app.stops[0]).toHaveBeenCalledOnce();
    expect(app.stops[4]).toHaveBeenCalledOnce();
  });

  it.each([false, true])('handles an empty board with saved=%s', async (saved) => {
    const app = home();
    app.settings.boardInitialized = saved;
    app.effect('prism.identity')();
    await settle();
    app.render();
    app.effect('startingBoard')();
    const frames = nodes(app.render(), (node) => node.type === 'Frame');
    expect(frames.length > 0).toBe(!saved);
  });

  it('preserves older nonempty boards without the new initialization marker', async () => {
    const app = home();
    const block = { id: 'old', kind: 'mine', x: 1, y: 1, w: 10, h: 6 };
    app.api.getSettings.mockResolvedValue({ language: 'en', board: [block] } as any);
    app.effect('prism.identity')();
    await settle();
    app.render();
    app.effect('startingBoard')();
    const frames = nodes(app.render(), (node) => node.type === 'Frame');
    expect(frames).toHaveLength(1);
    expect(frames[0].props.block).toBe(block);
  });
});
