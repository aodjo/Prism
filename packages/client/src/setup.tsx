/**
 * The setup flow.
 *
 * Five screens in one window, shown once. Everything on them is real: the account is really
 * made, the codes are really checked, and the permissions are the ones this machine actually
 * holds. A flow that showed a plausible picture instead would be one that passes when the
 * thing behind it is broken.
 *
 * It stops at being set up. Connecting to another machine is not part of it — there is no
 * other machine yet on the first run, and offering to reach one before any exists is a screen
 * that can only ever be empty.
 */

import { StrictMode, useCallback, useEffect, useRef, useState } from 'react';
import type { CSSProperties, JSX } from 'react';
import { createRoot } from 'react-dom/client';

import type {
  AccountEnrolmentView,
  HostPermissions,
  PrismApi,
  Settings,
} from './api.js';
import { Backdrop, Primary, STEP_SKY, Trouble, WELCOME_SKY, Wordmark, reason, short } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** The screens, in the order somebody sees them. */
const STEPS = ['welcome', 'account', 'intro', 'permissions', 'ready'] as const;

type Step = (typeof STEPS)[number];

/**
 * What the dots count, which is screens rather than stages.
 *
 * The account stage is not one screen: it is a form, an address to prove, a second factor to
 * set up and then to try, and a greeting. A single dot standing still through all five reads
 * as a flow that has stopped, so each of them gets its own.
 */
type Marker =
  | 'account'
  | 'account:prove'
  | 'account:enrol'
  | 'account:code'
  | 'account:done'
  | 'intro'
  | 'permissions'
  | 'ready';

/** Everything after the account stage, which is one screen each. */
const AFTER: readonly Marker[] = ['intro', 'permissions', 'ready'];

/**
 * The screens that offer a Continue rather than doing something else with the bottom right.
 *
 * Adding a device is not among them. There is nothing to continue to from that screen until a
 * machine has been added — the screen after it is a connection being made — so what it offers
 * instead is to leave it for later.
 */
const HAS_NEXT: ReadonlySet<Step> = new Set<Step>(['intro', 'permissions']);

/** How the three permission rows read, in the order the design puts them. */
const GRANTS = [
  {
    id: 'screen',
    glyph: '▣',
    name: 'Screen Recording',
    why: 'Capture this display so it can be streamed.',
    tint: 'border-[rgba(124,92,255,0.28)] bg-[rgba(124,92,255,0.16)] text-violet',
  },
  {
    id: 'input',
    glyph: '⌘',
    name: 'Accessibility',
    why: 'Pass keyboard and mouse input to this machine.',
    tint: 'border-[rgba(53,214,255,0.28)] bg-[rgba(53,214,255,0.16)] text-cyan',
  },
  {
    id: 'network',
    glyph: '⇄',
    name: 'Local Network',
    why: 'Discover your other devices on this network.',
    tint: 'border-[rgba(77,232,176,0.28)] bg-[rgba(77,232,176,0.16)] text-mint',
  },
] as const;

/** How many characters a pairing code has. */
const CODE_LENGTH = 6;

/** How far apart the code boxes are, centre to centre: 62 wide with 10 between. */
const CODE_PITCH = 72;

/** How far the leftmost box is from the middle of the row, which the rest step down from. */
const CODE_CENTRE = ((CODE_LENGTH - 1) * CODE_PITCH) / 2;

/**
 * How long a refusal is shown before the row hands itself back.
 *
 * Long enough to read three words and see the row say no; short enough that somebody who
 * already knows they fat-fingered it is not kept waiting to try again.
 */
const REFUSAL_HOLD_MS = 900;

/**
 * How long an accepted code is shown before the flow moves on.
 *
 * Enough for the tick to draw and be read. Without it the screen changes on the same frame the
 * answer arrives, so the only mark anybody ever sees is the one that says no — which leaves
 * the interface looking like it only ever has bad news.
 */
const SUCCESS_HOLD_MS = 620;

/**
 * How the mark inside the circle is drawn on.
 *
 * The dash pattern is the whole trick: a path whose dash is as long as the path itself is
 * either entirely gap or entirely line, so moving the offset from one to the other draws it.
 * The lengths are the paths' own, rounded up.
 */
const MARK_TICK = {
  '--mark-length': '22',
  strokeDasharray: 22,
  animation: 'code-mark 280ms cubic-bezier(0.65, 0, 0.35, 1) both',
} as CSSProperties;

/** The same, for each of the two strokes that say no. */
const MARK_CROSS = {
  '--mark-length': '16',
  strokeDasharray: 16,
  animation: 'code-mark 200ms cubic-bezier(0.65, 0, 0.35, 1) 120ms both',
} as CSSProperties;

/** The one shape every box on the account screen has. */
const ACCOUNT_FIELD =
  'w-full rounded-tile border border-line-2 bg-base px-3 py-2 text-body-2 text-ink placeholder:text-dim focus:border-[rgba(124,92,255,0.6)] focus:outline-none';

/** What a screen is laid out inside: a centred column over the backdrop. */
const SCREEN = 'flex flex-col items-center text-center';

/**
 * How far through the flow somebody is.
 *
 * Drawn here rather than taken from the design's exported image, because that image has one
 * dot per screen baked into it and the number of screens is not a constant. The geometry is
 * the export's: six-pixel dots, an eighteen-pixel pill for the one being shown, seven pixels
 * between them.
 *
 * @param {object} props - What to draw.
 * @param {number} props.at - Which step, counting from zero.
 * @param {number} props.of - How many there are.
 * @returns {JSX.Element} The dots.
 */
function Steps({ at, of }: { at: number; of: number }): JSX.Element {
  return (
    <div className="flex h-1.5 items-center gap-[7px]">
      {Array.from({ length: of }, (_, index) => (
        <span
          // A fixed run that never reorders: position is what identifies one of these.
          key={index}
          className={`block h-1.5 rounded-full bg-white transition-all duration-300 ${
            index === at ? 'w-[18px] opacity-90' : 'w-1.5 opacity-[0.18]'
          }`}
        />
      ))}
    </div>
  );
}

/* ── 02 · Intro ───────────────────────────────────────────────────────────────────────── */

/** The five rays leaving the prism, at the angles and colours the design set. */
const RAYS = [
  { turn: '-21deg', rgb: '124, 92, 255' },
  { turn: '-10.5deg', rgb: '53, 214, 255' },
  { turn: '0deg', rgb: '77, 232, 176' },
  { turn: '10.5deg', rgb: '255, 176, 92' },
  { turn: '21deg', rgb: '255, 92, 168' },
] as const;

/**
 * White light going in one side and coming out as five colours.
 *
 * The rays are gradients on rotated boxes rather than exported assets, because that is what
 * the design file holds too — there is no image to export.
 *
 * @returns {JSX.Element} The illustration.
 */
function PrismArt(): JSX.Element {
  return (
    // Centred on the prism rather than on the drawing. The rays leave to one side only, so the
    // box that holds them all has its middle 38.5px to the right of the prism's, and centring
    // the box puts the prism off to one side of everything written under it.
    <div className="relative h-[276px] w-[360px] -translate-x-[38.5px]">
      <div className="absolute left-9 top-[18px] h-[240px] w-[276px] mix-blend-screen">
        <img
          src="assets/prism-glow.svg"
          alt=""
          className="absolute inset-y-[-22.5%] inset-x-[-19.57%] block max-w-none"
        />
      </div>
      <div className="absolute left-[27px] top-[137px] h-[2px] w-[190px] mix-blend-screen">
        <img
          src="assets/incident-beam.svg"
          alt=""
          className="absolute inset-y-[-36%] inset-x-[-0.38%] block max-w-none"
        />
      </div>
      {RAYS.map((ray) => (
        <div
          key={ray.rgb}
          className="absolute left-[216px] top-[136.8px] h-[1.5px] w-[180px] origin-left blur-[0.75px] mix-blend-screen"
          style={{
            transform: `rotate(${ray.turn})`,
            background: `linear-gradient(to right, rgb(${ray.rgb}), rgba(${ray.rgb}, 0.55) 65%, rgba(${ray.rgb}, 0))`,
          }}
        />
      ))}
      <img
        src="assets/prism-intro.svg"
        alt=""
        className="absolute left-[174px] top-[91px] block h-[75px] w-[89px]"
      />
    </div>
  );
}

/* ── The flow ─────────────────────────────────────────────────────────────────────────── */

/**
 * The setup window.
 *
 * @returns {JSX.Element} The whole of it.
 */
function Setup(): JSX.Element {
  const [step, setStep] = useState<Step>('welcome');
  /** The screen on its way out, kept mounted only as long as it takes to leave. */
  const [leaving, setLeaving] = useState<Step | null>(null);
  /** Whether the move was backwards, which is the side both screens travel towards. */
  const [back, setBack] = useState(false);
  const [version, setVersion] = useState('');
  const [settings, setSettings] = useState<Settings | null>(null);
  const [held, setHeld] = useState<HostPermissions | null>(null);
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  /**
   * Whether the account screen is asking for the six digits rather than the email and password.
   *
   * Its own page, the way a machine asks for a PIN: the code is short, it is typed against a
   * clock, and a field for it sitting under a password somebody has not finished typing is a
   * field that gets filled in with a code that has already expired.
   */
  const [askingCode, setAskingCode] = useState(false);
  /**
   * Whether the code sent to the address is what is being waited for.
   *
   * Before the account exists, not after. An account created before its address is proved is
   * one somebody can park on an address they do not own, holding a password and a second
   * factor of their choosing until the owner does something that turns it on.
   */
  const [proving, setProving] = useState(false);
  /**
   * Whether the second factor was set up a moment ago.
   *
   * The screen that follows asks for six digits either way, but it means two different things:
   * signing in, or checking that the authenticator somebody has just set up actually produces
   * what this account expects. Getting that wrong is worth finding out now rather than the
   * next time they open the application.
   */
  const [justEnrolled, setJustEnrolled] = useState(false);
  /**
   * How many codes have been refused on this screen.
   *
   * Counted rather than flagged, because the row of boxes reacts to it and a flag that was
   * already true the second time would react once and then sit still.
   */
  const [refused, setRefused] = useState(0);
  /**
   * Whether anything has moved yet.
   *
   * The first screen is not arriving from anywhere — there is nothing to its right for it to
   * have come from — so it fades up instead of sliding across. A slide would be the window
   * claiming a history it does not have, on the one screen somebody has no context for.
   */
  const [moved, setMoved] = useState(false);
  /** Whether the code just entered was accepted, while that is still being shown. */
  const [passed, setPassed] = useState(false);
  const [signedIn, setSignedIn] = useState<string | null>(null);
  const [enrolment, setEnrolment] = useState<AccountEnrolmentView | null>(null);
  const [working, setWorking] = useState(false);
  /**
   * Which of the two account screens is showing.
   *
   * Decided by how somebody arrived rather than by a control on the screen itself: Begin is for
   * a new account and the link beside it is for one that already exists, so by the time either
   * screen is drawn the question has been answered.
   */
  const [joining, setJoining] = useState(true);
  /**
   * Whether somebody signed in during this run of the flow.
   *
   * Separate from being signed in at all: arriving already signed in skips the screen, but
   * having just typed a password into it deserves an answer rather than a screen that changes
   * out from under you.
   */
  const [greeted, setGreeted] = useState(false);
  /**
   * Whether this run of the flow started with somebody already signed in.
   *
   * Whether to show the account screen is settled once, on arrival. Asking again after somebody
   * signs in would take the screen they are standing on out of the count while they are still
   * standing on it, and the dots would lose one under them.
   */
  const [arrivedSignedIn, setArrivedSignedIn] = useState(false);
  const [accountTrouble, setAccountTrouble] = useState<string | null>(null);

  useEffect(() => {
    void (async () => {
      const [identity, account, stored] = await Promise.all([
        prism.identity(),
        prism.accountState(),
        prism.getSettings(),
      ]);

      setVersion(identity.version);
      setSettings(stored);
      setSignedIn(account.email);
      setArrivedSignedIn(account.email !== null);

      // Arriving already signed in means this window was opened by a launch that found an
      // account but no finished setup, and there is only one way to be in that state: the
      // permissions step sent somebody to System Settings, and macOS reads a new grant only
      // when the application starts again. They are coming back to that step, so start there
      // rather than walking them through the welcome and the introduction a second time.
      if (account.email !== null) {
        setStep('permissions');
      }
    })();
  }, []);

  useEffect(() => {
    if (step !== 'permissions') {
      return;
    }

    void (async () => {
      try {
        setHeld(await prism.permissions());
      } catch {
        setHeld({ screen: false, input: false, missing: [] });
      }
    })();
  }, [step]);

  /**
   * Moves to another screen, and starts the one being left on its way out.
   *
   * Both are mounted for as long as the move takes, which is what makes it a move rather than
   * a replacement.
   */
  const go = (next: Step): void => {
    if (next === step) {
      return;
    }

    setBack(STEPS.indexOf(next) < STEPS.indexOf(step));
    setLeaving(step);
    setStep(next);
    setMoved(true);
  };

  /**
   * Whether a screen has nothing left to ask.
   *
   * Only the account screen ever does: somebody already signed in has answered it, and showing
   * them a page that says so and offers a button to leave is a page that wastes their time.
   */
  const answered = (which: Step): boolean => which === 'account' && arrivedSignedIn;

  const advance = (): void => {
    for (let at = STEPS.indexOf(step) + 1; at < STEPS.length; at += 1) {
      const next = STEPS[at];

      if (next && !answered(next)) {
        go(next);
        return;
      }
    }
  };

  const finish = (): void => {
    prism.finishSetup();
  };

  /**
   * Signs in, and takes the machines the account knows about with it.
   *
   * The code arrives as an argument rather than off the field it was typed into, because the
   * six boxes call this the moment the last one is filled and the state behind them has not
   * been committed yet.
   *
   * @param {string} code - The six digits.
   */
  const signIn = (code: string): void => {
    void (async () => {
      setWorking(true);
      setAccountTrouble(null);

      try {
        const state = await prism.accountSignIn(
          email.trim(),
          password,
          code,
          `${navigator.platform || 'This machine'} (${new Date().getFullYear()})`,
        );

        setSignedIn(state.email);
        setPassed(true);
        await new Promise((settled) => setTimeout(settled, SUCCESS_HOLD_MS));

        setPassword('');
        setConfirm('');
        setAskingCode(false);
        setGreeted(true);
        setPassed(false);
        setRefused(0);
      } catch (error) {
        setAccountTrouble(reason(error));
        setRefused((was) => was + 1);
      } finally {
        setWorking(false);
      }
    })();
  };

  /**
   * Asks the server to send a code to the address, and waits for it to come back.
   *
   * No account is made here. That is the whole of what this step is for.
   */
  const createAccount = (): void => {
    // Checked here because it cannot be checked anywhere else: the password never leaves this
    // machine, so a mistyped one is only ever two strings in this window that differ.
    if (password !== confirm) {
      setAccountTrouble('Those two passwords are not the same');
      return;
    }

    void (async () => {
      setWorking(true);
      setAccountTrouble(null);

      try {
        if (await prism.accountChallenge(email.trim())) {
          setProving(true);
        } else {
          // A server with no mail configured sends nothing and asks for nothing. The account
          // is made on the spot, which is what this server did before it could send at all.
          setEnrolment(await prism.accountRegister(email.trim(), password, ''));
          setJustEnrolled(true);
        }
      } catch (error) {
        setAccountTrouble(reason(error));
      } finally {
        setWorking(false);
      }
    })();
  };

  /**
   * Creates the account, now that the code sent to the address has come back.
   *
   * @param {string} code - The six digits from the message.
   */
  const proveAddress = (code: string): void => {
    void (async () => {
      setWorking(true);
      setAccountTrouble(null);

      try {
        const made = await prism.accountRegister(email.trim(), password, code);

        setPassed(true);
        await new Promise((settled) => setTimeout(settled, SUCCESS_HOLD_MS));

        setEnrolment(made);
        setProving(false);
        setJustEnrolled(true);
        setPassed(false);
        setRefused(0);
      } catch (error) {
        setAccountTrouble(reason(error));
        setRefused((was) => was + 1);
      } finally {
        setWorking(false);
      }
    })();
  };

  useEffect(() => {
    const onKey = (event: KeyboardEvent): void => {
      if (step === 'ready' && event.key === 'Enter' && (event.metaKey || event.ctrlKey)) {
        finish();
      }
    };

    document.addEventListener('keydown', onKey);

    return () => {
      document.removeEventListener('keydown', onKey);
    };
  }, [step]);

  /**
   * Builds one screen.
   *
   * Takes which screen rather than reading the current one, because during a move there
   * are two of them on the page and only one of them is current.
   *
   * @param {Step} which - The screen to build.
   * @param {string} cls - What to lay it out with, which is how it is told to animate.
   * @returns {JSX.Element} The screen.
   */
  const screenFor = (which: Step, cls: string): JSX.Element => (
    <>
      {which === 'welcome' && (
        <section className={cls}>
          <h1 className="max-w-[min(1040px,72.2vw)] text-hero font-semibold">
            Your desktop.
            <br />
            Everywhere.
          </h1>
          <p className="mt-7 max-w-[min(700px,48.6vw)] text-lead text-muted">
            Low-latency remote access for macOS, Windows, and Linux.
          </p>
          <div className="mt-7">
            <Primary
              trailing="→"
              onClick={() => {
                setJoining(true);
                advance();
              }}
            >
              Begin
            </Primary>
          </div>
          {/* Somebody who already has an account signs in and their machines follow. It opens
              the settings window rather than ending setup: this flow is what pairs this
              machine and asks for the grants it needs, and neither has happened yet however
              many machines the account already knows about. */}
          {/* Directly over the brightest part of the aurora, which is the one place on any of
              these screens where even the muted end of the scale washes out. */}
          {signedIn === null && (
            <button
              type="button"
              className="btn-ghost no-drag mt-7 text-ink-3 hover:text-ink"
              onClick={() => {
                setJoining(false);
                advance();
              }}
            >
              Already using PRISM? Sign in
            </button>
          )}
        </section>
      )}

      {which === 'intro' && (
        <section className={cls}>
          <PrismArt />
          <h2 className="mt-4 max-w-[min(640px,44.4vw)] text-display font-semibold">
            One machine, every screen.
          </h2>
          <p className="mt-4 max-w-[min(520px,36.1vw)] text-body text-muted">
            PRISM streams your desktop to any other device you own — with latency low enough
            that you stop noticing it&rsquo;s remote.
          </p>
        </section>
      )}

      {which === 'account' && (
        <section className={cls}>
          <h2 className="max-w-[min(700px,48.6vw)] text-title font-semibold">
            {greeted
              ? `Hello, ${signedIn ?? ''}`
              : askingCode
                ? justEnrolled
                  ? 'Test it once'
                  : 'Enter your code'
                : enrolment
                  ? 'One more thing'
                  : proving
                    ? 'Check your email'
                    : joining
                      ? 'Create your account'
                      : 'Welcome back'}
          </h2>
          <p className="mt-3.5 max-w-[min(560px,38.9vw)] text-body-2 text-muted">
            {greeted
              ? 'Every machine on this account now knows about this one, and this one knows about them.'
              : askingCode
                ? justEnrolled
                  ? 'Enter what your authenticator shows now, so you know it works before you need it.'
                  : `Six digits from your authenticator, for ${email.trim()}.`
                : enrolment
                  ? 'Set up the second factor now. It is the only time it is shown.'
                  : proving
                    ? `Six digits went to ${email.trim()}. Nothing is created until they come back.`
                    : joining
                      ? 'An account is how your machines find each other, and how this one is recognised when it asks.'
                      : 'Sign in and every machine on your account finds this one.'}
          </p>

          {greeted ? null : askingCode ? (
            <CodeBoxes
              disabled={working}
              refused={refused}
              passed={passed}
              onComplete={(code) => {
                signIn(code);
              }}
            />
          ) : proving ? (
            <CodeBoxes
              disabled={working}
              refused={refused}
              passed={passed}
              onComplete={(code) => {
                proveAddress(code);
              }}
            />
          ) : enrolment ? (
            <div className="card mt-9 w-[min(440px,30.6vw)] p-6">
              <p className="mx-auto max-w-[36ch] text-note leading-normal text-dim">
                Scan this with an authenticator app. It is shown once — the server keeps only
                enough to check codes, which is not enough to show it again.
              </p>
              <img
                src={enrolment.qr}
                alt=""
                width={200}
                height={200}
                className="mx-auto my-4 block rounded-xl bg-white p-2"
              />
              <code className="block text-center text-fine tracking-[0.06em] select-all text-ink-3">
                {enrolment.secret}
              </code>
              <button
                type="button"
                className="btn-primary-sm no-drag mx-auto mt-5 block"
                onClick={() => {
                  setEnrolment(null);
                  setJoining(false);
                  setRefused(0);
                  setAskingCode(true);
                }}
              >
                I have it — test it
              </button>
            </div>
          ) : (
            <div className="card mt-9 flex w-[min(440px,30.6vw)] flex-col gap-3 p-6 text-left">
              <label className="flex flex-col gap-1.5">
                <span className="text-fine-2 text-dim">Email</span>
                <input
                  type="email"
                  autoComplete="username"
                  spellCheck={false}
                  placeholder="you@example.com"
                  className={ACCOUNT_FIELD}
                  value={email}
                  onChange={(event) => {
                    setEmail(event.target.value);
                  }}
                />
              </label>
              <label className="flex flex-col gap-1.5">
                <span className="text-fine-2 text-dim">
                  {joining ? 'Password — at least eight characters' : 'Password'}
                </span>
                <input
                  type="password"
                  autoComplete={joining ? 'new-password' : 'current-password'}
                  placeholder="••••••••"
                  className={ACCOUNT_FIELD}
                  value={password}
                  onChange={(event) => {
                    setPassword(event.target.value);
                  }}
                  onKeyDown={(event) => {
                    if (event.key === 'Enter' && !joining) {
                      setAskingCode(true);
                    }
                  }}
                />
              </label>

              {/* Only when creating one. A password being typed to sign in is checked by the
                  server against what it already has; one being set has nothing to check it
                  against but a second reading of the same keystrokes.

                  Kept in the page and collapsed rather than taken out of it, so that changing
                  which question is being asked is a card that grows rather than one that jumps
                  by a field. The row is sized in `fr` because nothing here knows how tall a
                  field is, and the negative margin swallows the parent's gap while the row is
                  shut — otherwise a closed field still spaces the card as though it were open.
                  Both animate; the reduced-motion rule in the design system turns them off. */}
              <div
                aria-hidden={!joining}
                className={`grid transition-all duration-300 ease-out ${
                  joining ? 'grid-rows-[1fr]' : '-mt-3 grid-rows-[0fr]'
                }`}
              >
                <div className="overflow-hidden">
                  <label className="flex flex-col gap-1.5">
                    <span className="text-fine-2 text-dim">Password again</span>
                    <input
                      type="password"
                      autoComplete="new-password"
                      placeholder="••••••••"
                      tabIndex={joining ? undefined : -1}
                      className={
                        confirm !== '' && confirm !== password
                          ? `${ACCOUNT_FIELD} border-[rgba(255,92,110,0.5)]`
                          : ACCOUNT_FIELD
                      }
                      value={confirm}
                      onChange={(event) => {
                        setConfirm(event.target.value);
                      }}
                      onKeyDown={(event) => {
                        if (event.key === 'Enter') {
                          createAccount();
                        }
                      }}
                    />
                  </label>
                </div>
              </div>

              <button
                type="button"
                className="btn-primary-sm no-drag mt-2 justify-center"
                disabled={working}
                onClick={
                  joining
                    ? createAccount
                    : () => {
                        setAccountTrouble(null);
                        setAskingCode(true);
                      }
                }
              >
                {joining ? 'Create account' : 'Continue'}
              </button>
            </div>
          )}

          <Trouble message={accountTrouble} className="mt-4" />

          {askingCode && (
            <button
              type="button"
              className="btn-ghost no-drag mt-5"
              onClick={() => {
                setAccountTrouble(null);
                setAskingCode(false);
              }}
            >
              Use a different email
            </button>
          )}

          {enrolment === null && !greeted && !askingCode && (
            <button
              type="button"
              className="btn-ghost no-drag mt-5"
              onClick={() => {
                setAccountTrouble(null);
                setConfirm('');
                setJoining(!joining);
              }}
            >
              {joining ? 'I already have an account' : 'I need an account'}
            </button>
          )}
        </section>
      )}

      {which === 'permissions' && (
        <section className={cls}>
          <h2 className="max-w-[min(700px,48.6vw)] text-title font-semibold">
            A few permissions first
          </h2>
          <p className="mt-3.5 max-w-[min(560px,38.9vw)] text-body-2 text-muted">
            PRISM needs these to capture and control this machine.
            <br />
            Nothing is sent outside your own network.
          </p>
          <div className="card mt-[63px] w-[min(620px,43.1vw)] text-left">
            {GRANTS.map((grant) => {
              // Local Network is stated as given rather than checked. There is no
              // interface for asking the system about it, and by the time this screen is
              // on a display the application has already used the network to draw it — so
              // reporting anything else would be reporting a guess.
              const has =
                grant.id === 'screen'
                  ? (held?.screen ?? false)
                  : grant.id === 'input'
                    ? (held?.input ?? false)
                    : true;

              return (
                <div
                  key={grant.id}
                  className="flex items-center gap-4 py-5 pr-[18px] pl-[22px] not-first:border-t not-first:border-line-1"
                >
                  <span
                    className={`flex size-[38px] flex-none items-center justify-center rounded-badge border text-body-2 font-medium ${grant.tint}`}
                  >
                    {grant.glyph}
                  </span>
                  <span className="flex min-w-0 flex-1 flex-col gap-1">
                    <span className="text-body-2 font-medium">{grant.name}</span>
                    <span className="text-note text-muted-2">{grant.why}</span>
                  </span>
                  {has ? (
                    <span className="tag-granted">
                      <span className="text-fine">✓</span>
                      <span>Granted</span>
                    </span>
                  ) : (
                    <button
                      type="button"
                      className="btn-secondary no-drag"
                      onClick={() => {
                        void (async () => {
                          try {
                            await prism.requestPermission(grant.id);
                          } finally {
                            // Redrawn either way. The system may have granted it, refused
                            // it, or opened its own settings pane — and only the check
                            // afterwards says which.
                            setHeld(await prism.permissions());
                          }
                        })();
                      }}
                    >
                      Allow
                    </button>
                  )}
                </div>
              );
            })}
          </div>
          <p className="mt-4 text-note text-dim">
            You can change these later in Settings → Privacy.
          </p>
        </section>
      )}

      {which === 'ready' && (
        <section className={cls}>
          <h2 className="max-w-[min(760px,52.8vw)] text-triumph font-semibold">
            You&rsquo;re all set.
          </h2>
          <p className="mt-3 max-w-[min(640px,44.4vw)] text-lead-2 text-muted">
            {signedIn === null
              ? 'This machine is ready. Sign in to an account to reach it from anywhere else.'
              : `Signed in as ${signedIn}. Every machine you sign in to this account keeps its own screen, and can reach the others.`}
          </p>

          {/* What to do next, rather than a picture of something already happening. Setup can
              honestly say this machine is ready; it cannot say anything is connected, because
              connecting needs a second machine and this is the first one.

              Numbered because they are in fact an order: sharing a machine nothing else can
              reach does nothing, so the second one has to exist first. */}
          <div className="card mt-[30px] w-[min(560px,38.9vw)] text-left">
            <div className="flex items-start gap-4 px-[22px] py-5">
              <span className="flex size-[38px] flex-none items-center justify-center rounded-badge border border-[rgba(124,92,255,0.28)] bg-[rgba(124,92,255,0.16)] text-body-2 font-medium text-violet">
                1
              </span>
              <span className="flex min-w-0 flex-1 flex-col gap-1">
                <span className="text-body-2 font-medium">Add another machine</span>
                <span className="text-note text-muted-2">
                  Install PRISM on it and sign in to the same account. The two find each other.
                </span>
              </span>
            </div>
            <div className="flex items-start gap-4 border-t border-line-1 px-[22px] py-5">
              <span className="flex size-[38px] flex-none items-center justify-center rounded-badge border border-[rgba(53,214,255,0.28)] bg-[rgba(53,214,255,0.16)] text-body-2 font-medium text-cyan">
                2
              </span>
              <span className="flex min-w-0 flex-1 flex-col gap-1">
                <span className="text-body-2 font-medium">Share the one you want to watch</span>
                <span className="text-note text-muted-2">
                  Press Share on it, and it turns up on the home screen of the other.
                </span>
              </span>
            </div>
          </div>

          <div className="mt-[23px]">
            <Primary keys="⌘↵" onClick={finish}>
              Enter PRISM
            </Primary>
          </div>
        </section>
      )}
    </>
  );

  /**
   * Every screen this run will show, decided by the path rather than discovered as it goes.
   *
   * Somebody signed in already never sees the account stage at all. Somebody signing in sees
   * three of its screens and somebody creating an account sees five — a choice they made on
   * the welcome screen, before any of these dots are drawn, so the count is settled by the
   * time anybody can read it.
   */
  // Not `joining` on its own: that flag doubles as which form is showing, and the button
  // under the second factor turns it off so the screen after it signs in. Reading it here
  // would shorten the row of dots halfway through, which is the one thing a count of what is
  // left must never do.
  const creating = joining || justEnrolled;

  const shownSteps: readonly Marker[] = arrivedSignedIn
    ? AFTER
    : creating
      ? ['account', 'account:prove', 'account:enrol', 'account:code', 'account:done', ...AFTER]
      : ['account', 'account:code', 'account:done', ...AFTER];

  /** Which of them is on screen. */
  const marker = ((): Marker | null => {
    if (step === 'welcome') {
      return null;
    }

    if (step !== 'account') {
      return step;
    }

    if (greeted) {
      return 'account:done';
    }
    if (askingCode) {
      return 'account:code';
    }
    if (enrolment) {
      return 'account:enrol';
    }
    if (proving) {
      return 'account:prove';
    }

    return 'account';
  })();

  const counted = marker === null ? -1 : shownSteps.indexOf(marker);

  return (
    <>
      <div className="drag fixed inset-x-0 top-0 z-[3] h-11" />
      {/* Both skies are always on the page and one of them is faded out. Swapping the images
          instead would pop the whole backdrop at the moment the screens are halfway through
          moving, which is the one moment nobody is looking at the aurora. */}
      <Backdrop
        sky={WELCOME_SKY}
        vignette
        className={`transition-opacity duration-700 ${step === 'welcome' ? 'opacity-100' : 'opacity-0'}`}
      />
      <Backdrop
        sky={STEP_SKY}
        vignette
        className={`transition-opacity duration-700 ${step === 'welcome' ? 'opacity-0' : 'opacity-100'}`}
      />

      {/* The comp insets its four corners by different amounts — the wordmark sits further in
          than the step dots, and the skip link further in than the Continue button. The frame
          takes the outermost of each and the pieces make up the rest. */}
      <div className="relative z-[1] flex h-full flex-col pt-[71px] pr-10 pb-16 pl-14">
        <div className="flex h-5 flex-none items-center justify-between">
          <div className="ml-[21px]">
            <Wordmark />
          </div>
        </div>

        {/* Both screens are in the same cell while one is arriving and the other leaving, so
            the move is a move rather than a jump through an empty page. */}
        <div className="grid min-h-0 flex-1 place-items-center overflow-hidden">
          {leaving !== null && (
            <div
              key={`leaving-${leaving}`}
              aria-hidden
              className={`pointer-events-none col-start-1 row-start-1 ${
                back ? 'animate-[slide-out-back_260ms_ease-in_both]' : 'animate-[slide-out-forward_260ms_ease-in_both]'
              }`}
              onAnimationEnd={() => {
                setLeaving(null);
              }}
            >
              {screenFor(leaving, SCREEN)}
            </div>
          )}
          <div
            key={step}
            className={`col-start-1 row-start-1 ${
              moved
                ? back
                  ? 'animate-[slide-in-back_420ms_ease-out_both]'
                  : 'animate-[slide-in-forward_420ms_ease-out_both]'
                : 'animate-[fade-in_620ms_ease-out_both]'
            }`}
          >
            {screenFor(step, `${SCREEN} rise`)}
          </div>
        </div>

        <div
          key={`nav-${step}`}
          className="flex h-14 flex-none animate-[fade-in_420ms_ease-out_both] items-center justify-between"
        >
          {counted >= 0 ? <Steps at={counted} of={shownSteps.length} /> : <span />}
          {step === 'welcome' && (
            <span className="ml-auto text-tiny font-medium text-dim">v{version} · beta</span>
          )}
          {(HAS_NEXT.has(step) || (step === 'account' && greeted)) && (
            <Primary trailing="→" onClick={advance}>
              Continue
            </Primary>
          )}
        </div>
      </div>
    </>
  );
}

/**
 * Puts the caret in a box once that box has stopped moving.
 *
 * The boxes arrive with a transform, and a box focused while that is still running keeps the
 * caret where the box was rather than where it settles — it stays low in the box for as long
 * as the screen is open. Clicking a box has never shown this, because by then nothing is
 * moving; only the focus this screen gives itself lands early enough to catch it.
 *
 * @param {HTMLInputElement | null} [box] - The box to focus, if it is there.
 * @returns {() => void} Undoes the wait, for a screen that leaves before the box settles.
 */
function focusOnceStill(box: HTMLInputElement | null | undefined): () => void {
  if (!box) {
    return () => {};
  }

  if (!box.getAnimations().some((animation) => animation.playState === 'running')) {
    box.focus();

    return () => {};
  }

  const settled = (): void => {
    box.focus();
  };

  box.addEventListener('animationend', settled, { once: true });

  return () => {
    box.removeEventListener('animationend', settled);
  };
}

/**
 * The six characters of a pairing code.
 *
 * One box per character, so that the code is read and typed as six things rather than as a
 * word. Typing moves forward, deleting moves back, and pasting a whole code fills them all —
 * which is what somebody does when the code is on a screen beside them rather than in their
 * head.
 *
 * @param {object} props - What to draw.
 * @param {boolean} props.disabled - Whether the boxes accept anything.
 * @param {(code: string) => void} props.onComplete - Called once all six are filled.
 * @returns {JSX.Element} The boxes.
 */
function CodeBoxes({
  disabled,
  refused,
  passed,
  onComplete,
}: {
  disabled: boolean;
  refused: number;
  passed: boolean;
  onComplete: (code: string) => void;
}): JSX.Element {
  const [characters, setCharacters] = useState<string[]>(Array<string>(CODE_LENGTH).fill(''));
  /**
   * The refusal this row has already answered for.
   *
   * The count only ever goes up, so on its own it cannot say whether *this* attempt was the
   * one refused. Without that distinction a row that was wrong once says so again the instant
   * the sixth character of the right code lands, before the server has been asked.
   *
   * Started from whatever the count already is, so a row that appears after somebody has been
   * refused on an earlier screen does not inherit that refusal and open by saying no.
   */
  const [handled, setHandled] = useState(refused);
  const boxes = useRef<(HTMLInputElement | null)[]>([]);

  // Also after a refusal, which empties the row and animates the six boxes back in: the caret
  // has to wait for them there for the same reason it waits for them on arrival.
  useEffect(() => focusOnceStill(boxes.current[0]), [handled]);

  // A refusal empties the row and puts the caret back at the start, but not until the mark has
  // finished saying no. Clearing underneath the answer would take the answer away before it
  // had been read; leaving six wrong characters in place would ask somebody to clean up after
  // the interface's own bad news.
  useEffect(() => {
    if (refused === handled) {
      return;
    }

    const settling = setTimeout(() => {
      setCharacters(Array<string>(CODE_LENGTH).fill(''));
      setHandled(refused);
    }, REFUSAL_HOLD_MS);

    return () => {
      clearTimeout(settling);
    };
  }, [refused, handled]);

  const put = (next: string[]): void => {
    setCharacters(next);

    if (next.every((character) => character !== '')) {
      onComplete(next.join(''));
    }
  };

  const full = characters.every((character) => character !== '');
  const wrong = full && refused > handled;

  return (
    <div className="relative mt-[88px] flex h-[72px] items-center gap-2.5">
      {characters.map((character, at) => (
        <input
          // The boxes are a fixed row of six that never reorders, so their position is what
          // identifies them; there is nothing else stable to key on.
          key={at}
          ref={(node) => {
            boxes.current[at] = node;
          }}
          type="text"
          inputMode="numeric"
          maxLength={1}
          disabled={disabled || full}
          aria-label={`Character ${at + 1}`}
          value={character}
          onChange={(event) => {
            // Digits, and nothing else. Both codes this row is used for are six digits, so
            // anything else is a keystroke that was never going to be part of one — and on a
            // keyboard that is composing another script it is worse than useless: the typeface
            // is subset to latin, so what lands in the box is a character it cannot draw, and
            // somebody sees an empty rectangle where their code should be.
            const typed = event.target.value.replace(/\D/gu, '').slice(0, 1);
            const next = [...characters];
            next[at] = typed;

            if (typed !== '') {
              boxes.current[at + 1]?.focus();
            }

            put(next);
          }}
          onKeyDown={(event) => {
            if (event.key === 'Backspace' && character === '') {
              const next = [...characters];
              next[at - 1] = '';
              setCharacters(next);
              boxes.current[at - 1]?.focus();
            }
          }}
          onPaste={(event) => {
            event.preventDefault();

            const pasted = event.clipboardData.getData('text').replace(/\D/gu, '');
            const next = characters.map((_, index) => pasted[index] ?? '');

            boxes.current[Math.min(pasted.length, CODE_LENGTH - 1)]?.focus();
            put(next);
          }}
          style={{
            // Each box carries how far it is from the middle of the row, so the six of them
            // draw together into one place rather than merely fading out where they stand.
            // A transition rather than an animation because the way back out is the same
            // movement reversed, and a refusal has to be able to hand the row back.
            transform: full ? `translateX(${CODE_CENTRE - at * CODE_PITCH}px) scale(0.55)` : 'none',
            opacity: full ? 0 : 1,
            animationDelay: `${at * 45}ms`,
          }}
          // The line box is the height of the box it is in, so the digit and the caret sit in
          // the middle of it. Left to the type scale it inherits a line height of one and a
          // half, which in a box this tall puts both of them near the top.
          className={`no-drag h-[72px] w-[62px] rounded-panel border p-0 text-center text-digit leading-[70px] font-medium text-ink caret-[rgba(124,92,255,0.9)] outline-none transition-[transform,opacity,background-color,border-color,box-shadow] duration-[340ms] ease-[cubic-bezier(0.4,0,0.2,1)] focus:border-[1.6px] focus:border-[rgba(124,92,255,0.85)] focus:shadow-[0_0_18px_rgba(124,92,255,0.35)] ${
            character === ''
              ? 'border-line-4 bg-wash-1 animate-[code-in_380ms_cubic-bezier(0.22,1.2,0.36,1)_both]'
              : 'border-[rgba(124,92,255,0.45)] bg-wash-4'
          }`}
        />
      ))}

      {/* What the six of them became: one round mark in the middle they collapsed into. A
          circle rather than another box, because the row of boxes is what somebody was filling
          in and this is no longer a thing to fill in.

          Empty while the server is being asked. A tick drawn before the answer came back would
          be the interface agreeing with somebody about something it has not checked, and the
          one time that matters is the time they typed it wrong. */}
      {full && (
        <span
          key={refused}
          className={`pointer-events-none absolute inset-0 flex items-center justify-center ${
            wrong ? 'animate-[code-refused_420ms_cubic-bezier(0.36,0.07,0.19,0.97)]' : ''
          }`}
        >
          <span
            role="status"
            style={{ animationDelay: wrong ? '0ms' : '190ms' }}
            className={`flex size-[72px] animate-[code-sealed_320ms_cubic-bezier(0.22,1.2,0.36,1)_both] items-center justify-center rounded-full border-2 transition-colors duration-300 ${
              wrong
                ? 'border-[rgba(255,92,110,0.5)] bg-[rgba(255,92,110,0.16)]'
                : passed
                  ? 'border-[rgba(77,232,176,0.6)] bg-[rgba(77,232,176,0.18)]'
                  : 'border-[rgba(124,92,255,0.55)] bg-[rgba(124,92,255,0.16)]'
            } ${!wrong && !passed ? 'animate-[code-checking_1.1s_ease-in-out_infinite]' : ''}`}
          >
            <span className="sr-only">
              {wrong ? 'That code was refused' : passed ? 'That code was accepted' : 'Checking'}
            </span>
            <svg
              viewBox="0 0 28 28"
              className="size-7"
              fill="none"
              stroke={wrong ? '#ff8a96' : '#4de8b0'}
              strokeWidth="2.4"
              strokeLinecap="round"
              strokeLinejoin="round"
              aria-hidden="true"
            >
              {wrong && (
                <>
                  <path d="M8.5 8.5 L19.5 19.5" style={MARK_CROSS} />
                  <path d="M19.5 8.5 L8.5 19.5" style={{ ...MARK_CROSS, animationDelay: '90ms' }} />
                </>
              )}
              {passed && <path d="M6.5 14.5 L11.75 19.75 L21.5 8.75" style={MARK_TICK} />}
            </svg>
          </span>
        </span>
      )}
    </div>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Setup />
  </StrictMode>,
);
