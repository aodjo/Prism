/**
 * The settings, with no window around them.
 *
 * Drawn inside whatever is showing them: a sheet over the home window, or a window of their
 * own. Which is why nothing here draws a backdrop, a title bar or a wordmark — a component
 * that decided those would be deciding for both of its callers.
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import type { JSX, ReactNode } from 'react';

import type {
  AccountEnrolmentView,
  AccountState,
  Available,
  Build,
  PrismApi,
  Settings,
} from './api.js';
import { speak, t } from './i18n.js';
import { Trouble, reason, short } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** Nothing is known yet. */
const UNKNOWN: AccountState = {
  server: '',
  email: null,
  publicKey: '',
  devices: [],
  relayAllowed: false,
  error: null,
};

/**
 * A band of the window, separated from the next by a hairline and nothing else.
 *
 * Boxing each one would give them all the same weight, and they do not have the same weight.
 *
 * @param {object} props - What to draw.
 * @param {string} props.title - What the band is for.
 * @param {ReactNode} props.children - What is in it.
 * @returns {JSX.Element} The band.
 */
function Band({ title, children }: { title: string; children: ReactNode }): JSX.Element {
  return (
    <section className="border-b border-line-1 px-5 py-4 last:border-b-0">
      <h2 className="m-0 mb-2.5 text-note font-semibold text-ink-3">{title}</h2>
      {children}
    </section>
  );
}

/**
 * A labelled control on its own line.
 *
 * @param {object} props - What to draw.
 * @param {string} props.label - What it sets.
 * @param {ReactNode} props.children - The control.
 * @returns {JSX.Element} The row.
 */
function Row({ label, children }: { label: string; children: ReactNode }): JSX.Element {
  return (
    <div className="flex min-h-[32px] items-center justify-between gap-4">
      <span className="text-note-2 text-muted">{label}</span>
      <div className="flex-none">{children}</div>
    </div>
  );
}

/** The shape every text box in this window has, before it is given a width. */
const FIELD =
  'rounded-tile border border-line-2 bg-base px-2.5 py-1.5 text-fine text-ink placeholder:text-dim focus:border-[rgba(124,92,255,0.6)] focus:outline-none';

/** Wide enough for an address, which is the longest thing typed here. */
const WIDE = `${FIELD} w-[210px]`;

/**
 * Narrow, right aligned and tabular, for the two fields that hold a quantity.
 *
 * A number in a box built for a URL reads as a fragment of something longer. Sizing the box to
 * what goes in it is what says a frame rate is expected rather than an address.
 */
const NUMBER =
  `${FIELD} w-[74px] text-right tabular-nums ` +
  // The browser's own steppers, which arrive grey, square and sized for a form on a web page.
  // Nothing else in this window came from a stylesheet nobody wrote, and these should not
  // either — the value is typed, and a pair of arrows is not what makes it changeable.
  '[appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none ' +
  '[&::-webkit-outer-spin-button]:appearance-none';

/** The one shape every switch in this window has. */
const TOGGLE = 'size-[15px] accent-violet';

/**
 * The settings.
 *
 * @param {object} props - How the thing showing these wants to be told about them.
 * @param {(height: number) => void} [props.onResize] - Called with the content's height as it
 *   changes, for a window that sizes itself to what is in it. A sheet inside a window that is
 *   already the right size passes nothing.
 * @returns {JSX.Element} Every band of them.
 */
export function Preferences({ onResize }: { onResize?: (height: number) => void }): JSX.Element {
  const [account, setAccount] = useState<AccountState>(UNKNOWN);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [enrolment, setEnrolment] = useState<AccountEnrolmentView | null>(null);
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [code, setCode] = useState('');
  /** The code sent to the address, while an account is being made. */
  const [proof, setProof] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [trouble, setTrouble] = useState<string | null>(null);
  const [build, setBuild] = useState<Build | null>(null);
  /** What a manual check found, or the sentence saying it found nothing. */
  const [update, setUpdate] = useState<Available | 'current' | 'checking' | null>(null);


  const body = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    void (async () => {
      const [known, stored, made] = await Promise.all([
        prism.accountState(),
        prism.getSettings(),
        prism.buildInfo(),
      ]);

      speak(stored.language);
      setAccount(known);
      setSettings(stored);
      setBuild(made);
    })();
  }, []);

  // A window is as tall as what is in it. Measured rather than calculated, because the account
  // section changes height by a form's worth when somebody signs in.
  useEffect(() => {
    const measured = body.current;

    if (!measured || !onResize) {
      return;
    }

    const watch = new ResizeObserver(() => {
      onResize(measured.scrollHeight);
    });

    watch.observe(measured);

    return () => {
      watch.disconnect();
    };
  }, [onResize]);

  /** What this machine calls itself when it tells an account about itself. */
  const label = useCallback(
    (): string => `${navigator.platform || 'This machine'} (${new Date().getFullYear()})`,
    [],
  );

  const save = (next: Partial<Settings>): void => {
    void (async () => {
      setSettings(await prism.setSettings(next));
    })();
  };

  const signIn = (): void => {
    void (async () => {
      setBusy(true);
      setTrouble(null);

      try {
        setAccount(await prism.accountSignIn(email.trim(), password, code.trim(), label()));
        setPassword('');
        setCode('');
      } catch (error) {
        setTrouble(reason(error));
      } finally {
        setBusy(false);
      }
    })();
  };

  const create = (): void => {
    void (async () => {
      setBusy(true);
      setTrouble(null);

      try {
        // Nothing is created until the code sent to the address comes back. Asking first is
        // what stops somebody registering an address that is not theirs.
        if (proof === null) {
          if (await prism.accountChallenge(email.trim())) {
            setProof('');
            return;
          }

          setEnrolment(await prism.accountRegister(email.trim(), password, ''));
          return;
        }

        setEnrolment(await prism.accountRegister(email.trim(), password, proof.trim()));
        setProof(null);
      } catch (error) {
        setTrouble(reason(error));
      } finally {
        setBusy(false);
      }
    })();
  };

  const configured = account.server.trim() !== '';

  return (
    <div ref={body}>
        <Band title={t('General')}>
          <Row label={t('Language')}>
            <select
              className="rounded-lg border border-line-2 bg-wash-1 px-2 py-1 text-note-2 text-ink-2"
              value={settings?.language ?? ''}
              onChange={(event) => {
                void (async () => {
                  await prism.setSettings({ language: event.target.value });
                  speak(event.target.value);

                  // Drawn again from the top, because every sentence already on screen was
                  // built with the language before this one and React has no reason to ask for
                  // any of them a second time. A window that is not this one keeps what it has
                  // until it is opened again.
                  location.reload();
                })();
              }}
            >
              <option value="">{t('Follow the system')}</option>
              <option value="en">English</option>
              <option value="ko">한국어</option>
            </select>
          </Row>
        </Band>

        {configured && (
          <Band title={t('Account')}>
            {enrolment ? (
              <div className="text-center">
                <p className="mx-auto mb-3 max-w-[42ch] text-tiny leading-normal text-dim">
                  Scan this with an authenticator app. It is shown once — the server keeps only
                  enough to check codes, which is not enough to show it again.
                </p>

                <img
                  src={enrolment.qr}
                  alt=""
                  width={200}
                  height={200}
                  className="mx-auto mb-3 block rounded-lg bg-white p-2"
                />
                <Row label="Or type">
                  <code className="select-all font-mono text-fine tracking-[0.06em] text-ink">
                    {enrolment.secret}
                  </code>
                </Row>
                <Row label="">
                  <button
                    type="button"
                    className="btn-primary-sm"
                    onClick={() => {
                      setEnrolment(null);
                    }}
                  >
                    Done
                  </button>
                </Row>
              </div>
            ) : account.email === null ? (
              <>
                <p className="mb-3 max-w-[42ch] text-tiny leading-normal text-dim">
                  Sign in and your machines find each other. Without an account they still pair,
                  by reading a code off one screen.
                </p>
                <Row label="Email">
                  <input
                    type="email"
                    spellCheck={false}
                    autoComplete="username"
                    placeholder="you@example.com"
                    className={FIELD}
                    value={email}
                    onChange={(event) => {
                      setEmail(event.target.value);
                    }}
                  />
                </Row>
                <Row label="Password">
                  <input
                    type="password"
                    className={FIELD}
                    value={password}
                    onChange={(event) => {
                      setPassword(event.target.value);
                    }}
                  />
                </Row>
                <Row label="Code">
                  <input
                    type="text"
                    inputMode="numeric"
                    maxLength={6}
                    placeholder="123456"
                    className={FIELD}
                    value={code}
                    onChange={(event) => {
                      setCode(event.target.value);
                    }}
                  />
                </Row>
                {/* Only once a code has been sent. Before that there is nothing to type, and
                    a box for a code nobody has been sent reads as a step somebody missed. */}
                {proof !== null && (
                  <Row label="Emailed code">
                    <input
                      type="text"
                      inputMode="numeric"
                      maxLength={6}
                      placeholder="123456"
                      className={FIELD}
                      value={proof}
                      onChange={(event) => {
                        setProof(event.target.value);
                      }}
                    />
                  </Row>
                )}
                <div className="mt-2.5 flex justify-end gap-2">
                  <button type="button" className="btn-secondary" disabled={busy} onClick={create}>
                    {proof === null ? 'Create account' : 'Confirm code'}
                  </button>
                  <button
                    type="button"
                    className="btn-primary-sm"
                    disabled={busy}
                    onClick={signIn}
                  >
                    Sign in
                  </button>
                </div>
              </>
            ) : (
              <>
                <Row label={account.email}>
                  <button
                    type="button"
                    className="btn-secondary"
                    onClick={() => {
                      void (async () => {
                        setAccount(await prism.accountSignOut());
                      })();
                    }}
                  >
                    Sign out
                  </button>
                </Row>
                  <div className="mt-2 flex flex-col gap-1">
                  {account.devices.map((device) => (
                    <div
                      key={device.publicKey}
                      className="flex min-h-[30px] items-center justify-between gap-3"
                    >
                      <span title={device.publicKey} className="truncate text-note-2 text-ink">
                        {device.label || short(device.publicKey)}
                      </span>
                      {device.isThisMachine ? (
                        // Named rather than made removable. A machine that removed itself would
                        // still be running, still trusted by everything else, and no longer
                        // listed anywhere.
                        <span className="flex-none text-tiny text-dim">this machine</span>
                      ) : (
                        <button
                          type="button"
                          className="btn-secondary"
                          onClick={() => {
                            void (async () => {
                              try {
                                setAccount(await prism.accountForgetDevice(device.publicKey));
                              } catch (error) {
                                setTrouble(reason(error));
                              }
                            })();
                          }}
                        >
                          Forget
                        </button>
                      )}
                    </div>
                  ))}
                </div>
              </>
            )}

            <Trouble message={trouble ?? account.error} className="mt-2.5" />
          </Band>
        )}

        {/* What this machine does when it is the one watching. Separate from what it does when
            it is the one being watched, because they are answers to different questions and a
            person is usually here about one of them. */}
        {/* Version and build number are two answers, not one. A person reads the version to
            know what they have; they report the build number when something is wrong with it,
            and it is the only one of the two that moves between two builds of a release. */}
        <Band title={t('Updates')}>
          <Row label={t('Version')}>
            <span className="text-note-2 text-ink-3">
              {build ? `${build.version} · build ${build.build}` : '—'}
            </span>
          </Row>
          <Row label={t('Automatic')}>
            <input
              type="checkbox"
              className={TOGGLE}
              checked={settings?.autoUpdate ?? true}
              onChange={(event) => {
                save({ autoUpdate: event.target.checked });
              }}
            />
          </Row>
          <Row label={t('Builds')}>
            <select
              className="rounded-lg border border-line-2 bg-wash-1 px-2 py-1 text-note-2 text-ink-2"
              value={settings?.updateChannel || build?.channel || 'production'}
              onChange={(event) => {
                save({ updateChannel: event.target.value });
                setUpdate(null);
              }}
            >
              <option value="production">{t('Released')}</option>
              <option value="development">{t('Every build')}</option>
            </select>
          </Row>
          <Row label="">
            <div className="flex items-center gap-3">
              <span className="text-note-2 text-muted">
                {update === 'checking'
                  ? t('Checking')
                  : update === 'current'
                    ? t('Up to date')
                    : update
                      ? `${update.version} is available`
                      : ''}
              </span>
              <button
                type="button"
                className="rounded-full border border-line-2 bg-wash-2 px-3 py-1 text-note-2 text-ink-3"
                onClick={() => {
                  void (async () => {
                    setUpdate('checking');

                    try {
                      const found = await prism.checkForUpdate();
                      setUpdate(found ?? 'current');

                      if (found) {
                        await prism.installUpdate();
                      }
                    } catch (error) {
                      setUpdate(null);
                      setTrouble(error instanceof Error ? error.message : String(error));
                    }
                  })();
                }}
              >
                {update && update !== 'checking' && update !== 'current' ? t('Install') : t('Check now')}
              </button>
            </div>
          </Row>
        </Band>

        <Band title={t('Watching')}>
          <Row label={t('Send input')}>
            <input
              type="checkbox"
              className={TOGGLE}
              checked={settings?.control ?? true}
              onChange={(event) => {
                save({ control: event.target.checked });
              }}
            />
          </Row>
          <Row label={t('Smooth playback')}>
            <input
              type="checkbox"
              className={TOGGLE}
              checked={settings?.smooth ?? false}
              onChange={(event) => {
                save({ smooth: event.target.checked });
              }}
            />
          </Row>
        </Band>

    </div>
  );
}

/**
 * What this machine gives out while it is shared.
 *
 * Not in the settings with the rest, because these are not settings about the application —
 * they are the terms of one particular action, and they belong beside the switch that starts
 * it. Somebody changing the frame rate is deciding how to share this machine, not how Prism
 * should behave.
 *
 * @returns {JSX.Element} The three figures that decide what goes out.
 */
export function SharingTerms(): JSX.Element {
  const [settings, setSettings] = useState<Settings | null>(null);

  useEffect(() => {
    void (async () => {
      setSettings(await prism.getSettings());
    })();
  }, []);

  const save = (next: Partial<Settings>): void => {
    void (async () => {
      setSettings(await prism.setSettings(next));
    })();
  };

  /**
   * What has been typed into a field that commits when it loses focus, but has not yet.
   *
   * The two text fields wait for focus to leave rather than writing every keystroke, because
   * the name is also sent to the account and that would be a request per letter. But a sheet
   * that closes does not blur its fields — it removes them — so without this, everything typed
   * into one and not tabbed out of goes with it.
   */
  const pending = useRef<Partial<Settings>>({});

  /**
   * Writes whatever is waiting, and forgets it.
   *
   * Called both when a field loses focus and when the sheet goes away, so that the two cannot
   * disagree about what was saved.
   *
   * @returns {void}
   */
  const flush = useCallback((): void => {
    const waiting = pending.current;
    pending.current = {};

    if (Object.keys(waiting).length === 0) {
      return;
    }

    // Deliberately not through `save`: this also runs as the sheet is being taken apart, and
    // a component that sets state on its way out is a warning in the console and nothing else.
    void prism.setSettings(waiting);

    // The account carries the name every other machine reads, so the one typed here is sent
    // there too — two names for one machine would be two answers to the same question. A
    // machine nobody has signed in on has nowhere to send it, and that is not a failure worth
    // interrupting anybody over.
    if (waiting.nickname !== undefined && waiting.nickname !== '') {
      void prism.accountRename(waiting.nickname).catch(() => {});
    }
  }, []);

  useEffect(() => flush, [flush]);

  return (
    <div className="flex flex-col">
      <Row label={t('Name')}>
        <input
          type="text"
          spellCheck={false}
          placeholder="This machine"
          className={WIDE}
          value={settings?.nickname ?? ''}
          onChange={(event) => {
            const typed = event.target.value;

            setSettings((was) => (was ? { ...was, nickname: typed } : was));
            pending.current = { ...pending.current, nickname: typed.trim() };
          }}
          onBlur={flush}
        />
      </Row>
      <Row label={t('Frame rate')}>
        <input
          type="number"
          min={1}
          max={480}
          step={1}
          className={NUMBER}
          value={settings?.fps ?? 60}
          onChange={(event) => {
            save({ fps: Number(event.target.value) });
          }}
        />
      </Row>
      <Row label={t('Bitrate')}>
        <input
          type="number"
          min={1}
          max={200}
          step={1}
          className={NUMBER}
          value={settings ? Math.round(settings.bitrateBps / 1e6) : 24}
          onChange={(event) => {
            save({ bitrateBps: Number(event.target.value) * 1e6 });
          }}
        />
      </Row>
      <Row label={t('Listen on')}>
        <input
          type="text"
          spellCheck={false}
          placeholder="0.0.0.0:47200"
          className={WIDE}
          value={settings?.bind ?? ''}
          onChange={(event) => {
            const typed = event.target.value;

            setSettings((was) => (was ? { ...was, bind: typed } : was));
            pending.current = { ...pending.current, bind: typed.trim() };
          }}
          onBlur={flush}
        />
      </Row>
    </div>
  );
}
