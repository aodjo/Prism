import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { PrismApi } from '../src/api.js';

/** A controllable IPC response for testing registration and teardown races. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

/** Drains IPC continuations without advancing polling timers. */
async function settle(): Promise<void> {
  for (let pass = 0; pass < 10; pass++) {
    await Promise.resolve();
  }
}

describe('the shell bridge', () => {
  let api: PrismApi;
  let invoke: ReturnType<typeof vi.fn>;
  let unregister: ReturnType<typeof vi.fn>;
  let unregisterListener: ReturnType<typeof vi.fn>;
  let handlers: Map<number, (message: unknown) => void>;

  beforeEach(async () => {
    vi.resetModules();
    vi.useFakeTimers();
    handlers = new Map();
    unregister = vi.fn((id: number) => { handlers.delete(id); });
    unregisterListener = vi.fn();
    invoke = vi.fn(async () => 42);
    vi.stubGlobal('window', {
      addEventListener: vi.fn(),
      __TAURI_INTERNALS__: {
        invoke,
        transformCallback(callback: (message: unknown) => void) {
          const id = handlers.size + 1;
          handlers.set(id, callback);
          return id;
        },
        unregisterCallback: unregister,
      },
      __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener },
    });
    await import('../src/bridge.js');
    api = window.prism;
  });

  afterEach(() => {
    vi.clearAllTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('sends only changed settings so concurrent writes can merge on the shell', async () => {
    let settings = { fps: 60, bitrateBps: 40_000_000 };
    invoke.mockImplementation(async (command, args) => {
      expect(command).toBe('set_settings');
      settings = { ...settings, ...args.next };
      return settings;
    });
    await Promise.all([
      api.setSettings({ fps: 120 }),
      api.setSettings({ bitrateBps: 80_000_000 }),
    ]);
    expect(invoke.mock.calls).toEqual([
      ['set_settings', { next: { fps: 120 } }],
      ['set_settings', { next: { bitrateBps: 80_000_000 } }],
    ]);
    expect(settings).toEqual({ fps: 120, bitrateBps: 80_000_000 });
  });

  it('delivers saved language changes and unregisters the subscription', async () => {
    const receive = vi.fn();
    const stop = api.onSettings(receive);
    await settle();
    expect(invoke).toHaveBeenCalledWith('plugin:event|listen', expect.objectContaining({ event: 'settings:changed' }));
    const handler = [...handlers.values()][0];
    handler({ payload: { language: 'ko' } });
    expect(receive).toHaveBeenCalledWith({ language: 'ko' });
    stop();
    await settle();
    expect(unregister).toHaveBeenCalledOnce();
  });

  it('returns setup completion failures to the screen', async () => {
    invoke.mockRejectedValueOnce(new Error('home window failed'));
    await expect(api.finishSetup()).rejects.toThrow('home window failed');
  });

  it('reports asynchronous registration failures and releases their callbacks', async () => {
    const failure = new Error('registration refused');
    invoke.mockRejectedValueOnce(failure);
    const report = vi.fn();
    const stop = api.onStream(vi.fn(), report);
    await settle();
    expect(report).toHaveBeenCalledWith(failure);
    expect(handlers.size).toBe(0);
    stop();
  });

  it('removes both event records and the callback when unsubscribed', async () => {
    const receive = vi.fn();
    const stop = api.onStream(receive);
    const handler = handlers.get(1)!;
    await settle();
    handler({ payload: { phase: 'streaming' } });
    expect(receive).toHaveBeenCalledOnce();
    stop();
    stop();
    handler({ payload: { phase: 'stopped' } });
    expect(receive).toHaveBeenCalledOnce();
    expect(unregister).toHaveBeenCalledExactlyOnceWith(1);
    expect(unregisterListener).toHaveBeenCalledExactlyOnceWith('stream:state', 42);
    expect(invoke).toHaveBeenLastCalledWith('plugin:event|unlisten', {
      event: 'stream:state', eventId: 42,
    });
  });

  it('unsubscribes even when stopped before registration resolves', async () => {
    const registration = deferred<number>();
    invoke.mockReturnValueOnce(registration.promise);
    const stop = api.onAccount(vi.fn());
    stop();
    registration.resolve(99);
    await settle();
    expect(handlers.size).toBe(0);
    expect(invoke).toHaveBeenLastCalledWith('plugin:event|unlisten', {
      event: 'account:state', eventId: 99,
    });
  });

  it('reports cleanup failures without an unhandled rejection', async () => {
    const report = vi.fn();
    const stop = api.onUpdate(vi.fn(), report);
    await settle();
    invoke.mockRejectedValueOnce(new Error('cleanup refused'));
    stop();
    await settle();
    expect(report).toHaveBeenCalledWith(expect.objectContaining({ message: 'cleanup refused' }));
  });

  it('reports polling failures and can stop the polling timer', async () => {
    invoke.mockRejectedValue(new Error('snapshot unavailable'));
    const report = vi.fn();
    const stop = api.onSharing(vi.fn(), report);
    await vi.advanceTimersByTimeAsync(200);
    expect(report).toHaveBeenCalledOnce();
    stop();
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(1000);
    expect(invoke).toHaveBeenCalledOnce();
  });

  it('does not overlap polls or deliver a late response after cleanup', async () => {
    const snapshot = deferred<null>();
    invoke.mockReturnValue(snapshot.promise);
    const receive = vi.fn();
    const stop = api.onSharing(receive);
    await vi.advanceTimersByTimeAsync(1000);
    expect(invoke).toHaveBeenCalledOnce();
    stop();
    snapshot.resolve(null);
    await settle();
    expect(receive).not.toHaveBeenCalled();
  });
});
