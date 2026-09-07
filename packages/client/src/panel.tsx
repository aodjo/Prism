/**
 * The settings window.
 *
 * What used to be the whole application. The home window owns the machines and the streaming
 * now, so what is left here is the three things that are settings rather than use: who this
 * machine signs in as, how it pairs with another one, and where it looks for both.
 *
 * It sizes itself to its content, because the account section is three lines when somebody is
 * signed in and a form when they are not.
 */

import { StrictMode, useCallback, useEffect, useRef, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { createRoot } from 'react-dom/client';

import type { AccountEnrolmentView, AccountState, PrismApi, Settings } from './api.js';
import { Backdrop, HOME_SKY, Trouble, Wordmark, reason, short } from './ui.js';

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
    <div className="flex min-h-[30px] items-center justify-between gap-3">
      <span className="text-note-2 text-muted">{label}</span>
      {children}
    </div>
  );
}

/** The one shape every text box in this window has. */
const FIELD =
  'w-[190px] rounded-tile border border-line-2 bg-base px-2.5 py-1.5 text-fine text-ink placeholder:text-dim focus:border-[rgba(124,92,255,0.6)] focus:outline-none';

/**
 * The settings window.
 *
 * @returns {JSX.Element} The whole of it.
 */
function Panel(): JSX.Element {
  const [account, setAccount] = useState<AccountState>(UNKNOWN);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [enrolment, setEnrolment] = useState<AccountEnrolmentView | null>(null);
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [code, setCode] = useState('');
  const [busy, setBusy] = useState(false);
  const [trouble, setTrouble] = useState<string | null>(null);


  const body = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    void (async () => {
      const [known, stored] = await Promise.all([prism.accountState(), prism.getSettings()]);

      setAccount(known);
      setSettings(stored);
    })();
  }, []);

  // The window is as tall as what is in it. Measured rather than calculated, because the
  // account section changes height by a form's worth when somebody signs in.
  useEffect(() => {
    const measured = body.current;

    if (!measured) {
      return;
    }

    const watch = new ResizeObserver(() => {
      prism.fit(measured.scrollHeight);
    });

    watch.observe(measured);

    return () => {
      watch.disconnect();
    };
  }, []);

  /** What this machine calls itself when it tells an account about itself. */
  const label = useCallback(
    (): string => `${navigator.platform || 'This machine'} (${new Date().getFullYear()})`,
    [],
  );

  const save = (next: Partial<Settings>): void => {
    void (async () => {
      setSettings(await prism.setSettings(next));

      // The account client belongs to the server that issued its session, so a change of
      // address is a change of who is signed in.
      if (next.accountServer !== undefined) {
        setAccount(await prism.accountState());
      }
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
        setEnrolment(await prism.accountRegister(email.trim(), password));
      } catch (error) {
        setTrouble(reason(error));
      } finally {
        setBusy(false);
      }
    })();
  };

  const configured = account.server.trim() !== '';

  return (
    <>
      <Backdrop sky={HOME_SKY} />

      <div ref={body} className="relative z-[1]">
        <div className="drag h-[34px]" />

        <header className="flex items-baseline gap-2.5 px-5 pb-3.5">
          <Wordmark size="sm" />
          <code
            title={account.publicKey}
            className="select-text font-mono text-tiny-2 text-dim"
          >
            {account.publicKey ? short(account.publicKey) : '…'}
          </code>
        </header>

        {configured && (
          <Band title="Account">
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
                <div className="mt-2.5 flex justify-end gap-2">
                  <button type="button" className="btn-secondary" disabled={busy} onClick={create}>
                    Create account
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
                <Row label="Relay">
                  <span className="text-note-2 text-ink-3">
                    {account.relayAllowed ? 'allowed' : 'not allowed'}
                  </span>
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

        <Band title="Settings">
          <Row label="Account server">
            <input
              type="text"
              spellCheck={false}
              placeholder="https://rv.example.com"
              className={FIELD}
              value={settings?.accountServer ?? ''}
              onChange={(event) => {
                setSettings((was) => (was ? { ...was, accountServer: event.target.value } : was));
              }}
              onBlur={(event) => {
                save({ accountServer: event.target.value.trim() });
              }}
            />
          </Row>
          <Row label="Rendezvous">
            <input
              type="text"
              spellCheck={false}
              placeholder="host:47300"
              className={FIELD}
              value={settings?.rendezvous ?? ''}
              onChange={(event) => {
                setSettings((was) => (was ? { ...was, rendezvous: event.target.value } : was));
              }}
              onBlur={(event) => {
                save({ rendezvous: event.target.value.trim() });
              }}
            />
          </Row>
          <Row label="Send input">
            <input
              type="checkbox"
              className="size-[15px] accent-violet"
              checked={settings?.control ?? true}
              onChange={(event) => {
                save({ control: event.target.checked });
              }}
            />
          </Row>
          <Row label="Smooth playback">
            <input
              type="checkbox"
              className="size-[15px] accent-violet"
              checked={settings?.smooth ?? false}
              onChange={(event) => {
                save({ smooth: event.target.checked });
              }}
            />
          </Row>
        </Band>

        {/* What this machine gives out rather than what it takes in. Separate from the settings
            above because they answer a different question — one is about watching, this is
            about being watched. */}
        <Band title="Sharing this machine">
          <Row label="Frame rate">
            <input
              type="number"
              min={1}
              max={480}
              step={1}
              className={FIELD}
              value={settings?.fps ?? 60}
              onChange={(event) => {
                save({ fps: Number(event.target.value) });
              }}
            />
          </Row>
          <Row label="Bitrate (Mbps)">
            <input
              type="number"
              min={1}
              max={200}
              step={1}
              className={FIELD}
              value={settings ? Math.round(settings.bitrateBps / 1e6) : 24}
              onChange={(event) => {
                save({ bitrateBps: Number(event.target.value) * 1e6 });
              }}
            />
          </Row>
          <Row label="Listen on">
            <input
              type="text"
              spellCheck={false}
              placeholder="0.0.0.0:47200"
              className={FIELD}
              value={settings?.bind ?? ''}
              onChange={(event) => {
                setSettings((was) => (was ? { ...was, bind: event.target.value } : was));
              }}
              onBlur={(event) => {
                save({ bind: event.target.value.trim() });
              }}
            />
          </Row>
          <Row label="Share on launch">
            <input
              type="checkbox"
              className="size-[15px] accent-violet"
              checked={settings?.shareOnLaunch ?? false}
              onChange={(event) => {
                save({ shareOnLaunch: event.target.checked });
              }}
            />
          </Row>
        </Band>
      </div>
    </>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Panel />
  </StrictMode>,
);
