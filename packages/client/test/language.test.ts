import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { Settings } from '../src/api.js';
import { speaking, speak, t } from '../src/i18n.js';
import { useLanguage } from '../src/language.js';

/** Captures the hook's lifecycle without mounting a native webview. */
const hooks = vi.hoisted(() => ({ effect: null as null | (() => () => void), changed: vi.fn() }));
vi.mock('react', () => ({
  useState: () => ['', hooks.changed],
  useEffect: (effect: () => () => void) => { hooks.effect = effect; },
}));

/** Drains settings continuations in their actual promise order. */
async function settle(): Promise<void> {
  for (let pass = 0; pass < 5; pass++) await Promise.resolve();
}

describe('window language synchronization', () => {
  let receive!: (settings: Settings) => void;
  let resolve!: (settings: Settings) => void;
  let stop: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    speak('en');
    hooks.changed.mockClear();
    stop = vi.fn();
    vi.stubGlobal('window', { prism: {
      onSettings: (listener: (settings: Settings) => void) => { receive = listener; return stop; },
      getSettings: () => new Promise<Settings>((done) => { resolve = done; }),
    } });
    vi.stubGlobal('document', { documentElement: { lang: 'en' } });
  });

  afterEach(() => { vi.unstubAllGlobals(); speak('en'); });

  it('uses the saved language when an independent file or banner window opens', async () => {
    useLanguage();
    const cleanup = hooks.effect!();
    resolve({ language: 'ko' } as Settings);
    await settle();
    expect(t('Files')).toBe('파일');
    expect(document.documentElement.lang).toBe('ko');
    receive({ language: 'en' } as Settings);
    expect(t('Files')).toBe('Files');
    expect(hooks.changed).toHaveBeenLastCalledWith('en');
    cleanup();
    expect(stop).toHaveBeenCalledOnce();
  });

  it('keeps a newer language announcement when the initial read resolves late', async () => {
    useLanguage();
    const cleanup = hooks.effect!();
    receive({ language: 'ko' } as Settings);
    resolve({ language: 'en' } as Settings);
    await settle();
    expect(speaking()).toBe('ko');
    cleanup();
    receive({ language: 'en' } as Settings);
    expect(speaking()).toBe('ko');
  });

  it('ignores an initial response after its window has closed', async () => {
    useLanguage();
    hooks.effect!()();
    resolve({ language: 'ko' } as Settings);
    await settle();
    expect(speaking()).toBe('en');
    expect(hooks.changed).not.toHaveBeenCalled();
  });
});
