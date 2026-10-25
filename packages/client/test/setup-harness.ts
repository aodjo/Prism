import fs from 'node:fs';
import vm from 'node:vm';
import ts from 'typescript';
import { vi } from 'vitest';

/** A JSX tree inspected without running native window code. */
export interface Element {
  type: any;
  props: Record<string, any>;
}

/** Transpiles the real setup and UI modules for controlled IPC and hook tests. */
function compile(name: string): string {
  return ts.transpileModule(fs.readFileSync(new URL(`../src/${name}`, import.meta.url), 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2023, jsx: ts.JsxEmit.ReactJSX },
  }).outputText;
}

/** Finds visible controls, excluding outgoing animation layers and hidden fields. */
export function nodes(tree: any, predicate: (node: Element) => boolean): Element[] {
  if (Array.isArray(tree)) return tree.flatMap((node) => nodes(node, predicate));
  if (!tree?.props || tree.props['aria-hidden']) return [];
  return [...(predicate(tree) ? [tree] : []), ...nodes(tree.props.children, predicate)];
}

/** Drains mocked IPC continuations without waiting for animation delays. */
export async function settle(): Promise<void> {
  for (let pass = 0; pass < 12; pass++) await Promise.resolve();
}

/** Runs Setup's actual event handlers with persistent hook values and isolated OS calls. */
export function setup() {
  const api = {
    identity: vi.fn(async () => ({ version: 'test', publicKey: 'mine' })),
    accountState: vi.fn(async () => ({ email: null as string | null })),
    getSettings: vi.fn(async () => ({ language: 'en' })),
    accountChallenge: vi.fn(async () => true),
    accountRegister: vi.fn(async () => ({ qr: 'data:image/png;base64,test', secret: 'test' })),
    accountSignIn: vi.fn(async () => ({ email: 'test@example.invalid' })),
    permissions: vi.fn(async () => ({ screen: false, input: false, missing: [{ id: 'screen' }, { id: 'input' }] })),
    requestPermission: vi.fn(async () => ({ screen: false, input: false, missing: [{ id: 'screen' }, { id: 'input' }] })),
    restart: vi.fn(async () => null),
    finishSetup: vi.fn(async () => null),
  };
  const state: any[] = [];
  const events = new Map<string, () => void>();
  let cursor = 0;
  let effects: (() => any)[] = [];
  let component!: () => Element;
  const jsx = (type: any, props: Record<string, any>): Element => ({ type, props });
  const actualUi: Record<string, any> = {};
  vm.runInNewContext(compile('ui.tsx'), { exports: actualUi, require: () => ({ jsx, jsxs: jsx }) });
  const ui = new Proxy({
    GRANTS: actualUi.GRANTS, t: (text: string) => text, speak: () => {},
    reason: (error: Error) => error.message, short: (text: string) => text,
  }, { get: (target, key) => key in target ? target[key as keyof typeof target] : String(key) });
  const eventTarget = {
    addEventListener: (name: string, callback: () => void) => { events.set(name, callback); },
    removeEventListener: (name: string) => { events.delete(name); },
  };
  vm.runInNewContext(compile('setup.tsx'), {
    exports: {},
    require(name: string) {
      if (name === 'react') return {
        StrictMode: 'StrictMode',
        useState(initial: unknown) {
          const index = cursor++;
          if (!(index in state)) state[index] = initial;
          return [state[index], (next: any) => { state[index] = typeof next === 'function' ? next(state[index]) : next; }];
        },
        useRef(initial: unknown) {
          const index = cursor++;
          if (!(index in state)) state[index] = { current: initial };
          return state[index];
        },
        useEffect: (effect: () => any) => { effects.push(effect); },
        useCallback: (callback: unknown) => callback,
      };
      if (name === 'react/jsx-runtime') return { jsx, jsxs: jsx };
      if (name === 'react-dom/client') return {
        createRoot: () => ({ render: (element: Element) => { component = element.props.children.type; } }),
      };
      return ui;
    },
    window: { prism: api, ...eventTarget },
    document: { ...eventTarget, getElementById: () => ({}), hidden: false },
    navigator: { platform: 'Test' }, setTimeout, clearTimeout, console,
  });
  /** Renders with the latest values committed by the component's handlers. */
  const render = (): Element => { cursor = 0; effects = []; return component(); };
  /** Selects an effect by the real operation it performs rather than a hook index. */
  const effect = (contains: string) => {
    const found = effects.find((run) => String(run).includes(contains));
    if (!found) throw new Error(`missing setup effect: ${contains}`);
    return found;
  };
  /** Finds a visible button by its user-facing text. */
  const button = (label: string, tree = render()) => nodes(tree, (node) =>
    ['button', 'Primary'].includes(node.type) && node.props.children === label)[0];
  /** Enters matching email and password values using the actual input handlers. */
  const fill = (password = 'long-enough-password') => {
    const tree = render();
    for (const [type, value] of [['email', 'test@example.invalid'], ['password', password]]) {
      nodes(tree, (node) => node.type === 'input' && node.props.type === type).forEach((field) => {
        field.props.onChange({ target: { value } });
      });
    }
  };
  render();
  return { api, state, events, render, effect, button, fill };
}
