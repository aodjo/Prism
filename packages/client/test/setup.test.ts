import { describe, expect, it, vi } from 'vitest';
import { nodes, setup, settle } from './setup-harness.js';

describe('signup recovery', () => {
  it('rejects a short password before sending a confirmation email', async () => {
    const app = setup();
    app.button('Begin').props.onClick();
    app.fill('a');
    app.button('Create account').props.onClick();
    await settle();
    expect(app.api.accountChallenge).not.toHaveBeenCalled();
    expect(app.state).toContain('A password needs at least 8 characters.');
    expect(nodes(app.render(), (node) => node.type === 'input' && node.props.type === 'password')).toHaveLength(2);
  });

  it('allows a failed email code to be resent or the signup details corrected', async () => {
    const app = setup();
    app.button('Begin').props.onClick();
    app.fill();
    app.button('Create account').props.onClick();
    await settle();
    app.api.accountRegister.mockRejectedValueOnce(new Error('code expired'));
    const code = nodes(app.render(), (node) => node.type?.name === 'CodeBoxes')[0];
    code.props.onComplete('123456');
    await settle();
    expect(app.state).toContain('code expired');
    app.button('Send a new code').props.onClick();
    await settle();
    expect(app.api.accountChallenge).toHaveBeenCalledTimes(2);
    expect(nodes(app.render(), (node) => node.type?.name === 'CodeBoxes')[0].props.refused).toBe(0);
    app.button('Edit signup details').props.onClick();
    expect(nodes(app.render(), (node) => node.type === 'input' && node.props.type === 'email')).toHaveLength(1);
    expect(app.button('Create account')).toBeDefined();
  });

  it('prevents a keyboard submission from duplicating an in-flight signup', () => {
    const app = setup();
    app.button('Begin').props.onClick();
    app.fill();
    const create = app.button('Create account');
    create.props.onClick();
    create.props.onClick();
    expect(app.api.accountChallenge).toHaveBeenCalledOnce();
    expect(app.button('I already have an account').props.disabled).toBe(true);
  });
});


describe('setup startup recovery', () => {
  it('resumes permissions despite an identity failure and offers a working retry', async () => {
    const app = setup();
    app.api.identity.mockRejectedValueOnce(new Error('identity unavailable'));
    app.api.accountState.mockResolvedValue({ email: 'test@example.invalid' });
    app.effect('prism.identity')();
    await settle();
    expect(app.state).toContain('identity unavailable');
    expect(app.button('Allow')).toBeDefined();
    app.button('Retry').props.onClick();
    app.render();
    app.effect('prism.identity')();
    await settle();
    expect(app.api.identity).toHaveBeenCalledTimes(2);
    expect(app.button('Retry')).toBeUndefined();
  });

  it('does not apply late startup replies after cleanup', async () => {
    const app = setup();
    let resolve!: (value: { email: string }) => void;
    app.api.accountState.mockReturnValue(new Promise((done) => { resolve = done; }));
    const stop = app.effect('prism.identity')();
    stop();
    const before = [...app.state];
    resolve({ email: 'test@example.invalid' });
    await settle();
    expect(app.state).toEqual(before);
  });
});


describe('permission failure recovery', () => {
  it('keeps Allow available after a failed request and uses a successful native reply directly', async () => {
    const app = setup();
    app.api.accountState.mockResolvedValue({ email: 'test@example.invalid' });
    app.effect('prism.identity')();
    await settle();
    app.render();
    app.effect("step !== 'permissions'")();
    await settle();
    app.api.requestPermission.mockRejectedValueOnce(new Error('request refused'));
    app.button('Allow').props.onClick();
    await settle();
    expect(app.state).toContain('request refused');
    expect(app.button('Restart PRISM')).toBeUndefined();
    const reads = app.api.permissions.mock.calls.length;
    app.button('Allow').props.onClick();
    await settle();
    expect(app.api.permissions).toHaveBeenCalledTimes(reads);
    expect(app.button('Restart PRISM')).toBeDefined();
    expect(app.button('Open System Settings')).toBeDefined();
  });

  it('reports a failed permission read and allows retrying it', async () => {
    const app = setup();
    app.api.accountState.mockResolvedValue({ email: 'test@example.invalid' });
    app.effect('prism.identity')();
    await settle();
    app.api.permissions.mockRejectedValueOnce(new Error('permission lookup failed'));
    app.render();
    app.effect("step !== 'permissions'")();
    await settle();
    expect(app.state).toContain('permission lookup failed');
    app.button('Retry').props.onClick();
    app.render();
    app.effect("step !== 'permissions'")();
    await settle();
    expect(app.state).not.toContain('permission lookup failed');
  });

  it('ignores permission replies after leaving the step', async () => {
    const app = setup();
    app.api.accountState.mockResolvedValue({ email: 'test@example.invalid' });
    app.effect('prism.identity')();
    await settle();
    let resolve!: (value: any) => void;
    app.api.permissions.mockReturnValue(new Promise((done) => { resolve = done; }));
    app.render();
    const stop = app.effect("step !== 'permissions'")();
    stop();
    const before = [...app.state];
    resolve({ screen: true, input: true, missing: [] });
    await settle();
    expect(app.state).toEqual(before);
  });
});


describe('setup completion', () => {
  it('reports a completion failure and allows retrying without duplicate requests', async () => {
    const app = setup();
    app.api.accountState.mockResolvedValue({ email: 'test@example.invalid' });
    app.effect('prism.identity')();
    await settle();
    app.button('Continue').props.onClick();
    app.api.finishSetup.mockRejectedValueOnce(new Error('home window failed'));
    const enter = app.button('Enter PRISM');
    enter.props.onClick();
    enter.props.onClick();
    expect(app.button('Enter PRISM').props.disabled).toBe(true);
    await settle();
    expect(app.api.finishSetup).toHaveBeenCalledOnce();
    expect(app.state).toContain('home window failed');
    expect(app.button('Enter PRISM').props.disabled).toBe(false);
    app.button('Enter PRISM').props.onClick();
    await settle();
    expect(app.api.finishSetup).toHaveBeenCalledTimes(2);
    expect(app.state).not.toContain('home window failed');
  });
});


describe('platform permission settings', () => {
  it.each([true, false])('offers the local-network settings pane only when supported=%s', async (supported) => {
    const app = setup();
    app.api.accountState.mockResolvedValue({ email: 'test@example.invalid' });
    app.api.permissions.mockResolvedValue({ networkSettings: supported, screen: true, input: true, missing: [] });
    app.effect('prism.identity')();
    await settle();
    app.render();
    app.effect("step !== 'permissions'")();
    await settle();
    expect(Boolean(app.button('Open System Settings'))).toBe(supported);
  });
});


describe('setup language', () => {
  it('saves the language from setup without submitting or clearing the account form', async () => {
    const app = setup('ko');
    app.button('시작').props.onClick();
    app.fill();
    const tree = app.render();
    const language = nodes(tree, (node) => node.type === 'select' && node.props['aria-label'] === '언어')[0];
    language.props.onChange({ target: { value: 'en' } });
    await settle();
    expect(app.api.setSettings).toHaveBeenCalledWith({ language: 'en' });
    expect(app.api.accountChallenge).not.toHaveBeenCalled();
    expect(nodes(app.render(), (node) => node.type === 'input' && node.props.type === 'email')[0].props.value).toBe('test@example.invalid');
  });

  it('keeps signup headings, instructions, actions and greeting in Korean', async () => {
    vi.useFakeTimers();
    try {
      const app = setup('ko');
      app.button('시작').props.onClick();
      /** Collects rendered copy without inspecting classes or implementation details. */
      const copy = (tree: any): string => Array.isArray(tree)
        ? tree.map(copy).join(' ')
        : typeof tree === 'string' ? tree : tree?.props ? copy(tree.props.children) : '';
      expect(copy(app.render())).toContain('계정 만들기');
      expect(copy(app.render())).toContain('계정을 만들면 내 기기끼리');
      expect(copy(app.render())).not.toMatch(/Create|Password|Continue|account/);
      app.fill();
      app.button('계정 만들기').props.onClick();
      await settle();
      expect(copy(app.render())).toContain('메일을 확인하세요');
      expect(copy(app.render())).toContain('test@example.invalid로 6자리 코드를');
      nodes(app.render(), (node) => node.type?.name === 'CodeBoxes')[0].props.onComplete('123456');
      await settle();
      await vi.runAllTimersAsync();
      app.button('등록했습니다 — 확인하기').props.onClick();
      expect(copy(app.render())).toContain('한 번 확인해 봅시다');
      nodes(app.render(), (node) => node.type?.name === 'CodeBoxes')[0].props.onComplete('123456');
      await settle();
      await vi.runAllTimersAsync();
      expect(copy(app.render())).toContain('test@example.invalid님, 반가워요');
      expect(copy(app.render())).not.toMatch(/Hello|Every machine|Test it/);
    } finally {
      vi.useRealTimers();
    }
  });
});
