/**
 * The setup flow.
 *
 * Six screens in one window, shown once. Everything on them is real: the permissions are the
 * ones this machine actually holds, the code pairs, the connection is a connection, and the
 * numbers on the last screen came off the stream. A flow that showed a plausible picture
 * instead would be a flow that passes when the thing behind it is broken.
 */

import { StrictMode, useCallback, useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import { createRoot } from 'react-dom/client';

import type {
  AccountDeviceView,
  AccountEnrolmentView,
  HostPermissions,
  PrismApi,
  Settings,
  StreamState,
} from './api.js';
import { latency } from './format.js';
import { Backdrop, Primary, STEP_SKY, Trouble, WELCOME_SKY, Wordmark, reason, short } from './ui.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** The screens, in the order somebody sees them. */
const STEPS = [
  'welcome',
  'account',
  'intro',
  'permissions',
  'device',
  'connecting',
  'ready',
] as const;

type Step = (typeof STEPS)[number];

/** The screens that are counted. The welcome screen comes before the count starts. */
const COUNTED = STEPS.slice(1);

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
    <div className="relative h-[276px] w-[360px]">
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

/* ── 05 · Connecting ──────────────────────────────────────────────────────────────────── */

/**
 * The rings that travel outward while a connection is being made.
 *
 * The only motion in the flow, and it is here because this is the only screen that is waiting
 * for something.
 *
 * @returns {JSX.Element} The illustration.
 */
function Pulse(): JSX.Element {
  return (
    <div className="relative size-[320px]">
      <img
        src="assets/ring-1.svg"
        alt=""
        className="absolute left-0 top-0 block size-[320px] origin-center animate-[ripple_3s_ease-out_infinite_0.8s]"
      />
      <img
        src="assets/ring-2.svg"
        alt=""
        className="absolute left-10 top-10 block size-[240px] origin-center animate-[ripple_3s_ease-out_infinite_0.4s]"
      />
      <img
        src="assets/ring-3.svg"
        alt=""
        className="absolute left-[78px] top-[78px] block size-[164px] origin-center animate-[ripple_3s_ease-out_infinite]"
      />
      <div className="absolute left-[60px] top-[60px] size-[200px] mix-blend-screen">
        <img
          src="assets/core-glow.svg"
          alt=""
          className="absolute inset-[-27.5%] block max-w-none"
        />
      </div>
      {/* The wordmark's own triangle, at the size this screen gives it. The same file the top
          left corner uses: a mark rendered larger is still the mark, and drawing a second one
          for this spot is how two versions of a logo start to drift apart. */}
      <img
        src="assets/mark.svg"
        alt=""
        className="absolute left-[126px] top-[132px] block h-[58.1px] w-[68px]"
      />
    </div>
  );
}

/** How far along one of the three things a connection does has got. */
type StageState = 'waiting' | 'doing' | 'done';

/**
 * One line of the connection's progress.
 *
 * @param {object} props - What to draw.
 * @param {StageState} props.state - How far along it is.
 * @param {string} props.what - What it is.
 * @param {string} props.detail - What it settled on, once it has.
 * @returns {JSX.Element} The row.
 */
function Stage({
  state,
  what,
  detail,
}: {
  state: StageState;
  what: string;
  detail: string;
}): JSX.Element {
  return (
    <div className="flex items-center gap-3 px-[18px] py-3.5 not-first:border-t not-first:border-wash-3">
      {/* A dashed ring rather than a spinner: it says this one is open without implying a
          proportion of it is finished, which nothing here can honestly report. */}
      <span
        className={
          state === 'done'
            ? 'flex size-5 flex-none items-center justify-center rounded-pill bg-[rgba(77,232,176,0.16)] text-tiny font-medium text-mint'
            : state === 'doing'
              ? 'size-5 flex-none rounded-pill border-2 border-dashed border-[rgba(124,92,255,0.85)]'
              : 'size-5 flex-none rounded-pill border-2 border-line-4'
        }
      >
        {state === 'done' ? '✓' : ''}
      </span>
      <span
        className={`text-ui font-medium ${state === 'doing' ? 'text-ink' : 'text-ink-3'}`}
      >
        {what}
      </span>
      <span className="ml-auto text-fine-2 text-dim">{detail}</span>
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
  const [devices, setDevices] = useState<readonly AccountDeviceView[]>([]);
  const [known, setKnown] = useState<readonly string[]>([]);
  const [held, setHeld] = useState<HostPermissions | null>(null);
  const [stream, setStream] = useState<StreamState>({
    phase: 'idle',
    host: null,
    terms: null,
    stats: null,
    log: [],
  });
  const [target, setTarget] = useState<string | null>(null);
  const [connectError, setConnectError] = useState<string | null>(null);

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

  /**
   * Returns what to call a machine.
   *
   * The account is asked first, because a person named their machines and a public key is what
   * is left when nobody has.
   */
  const machineName = useCallback(
    (key: string): string =>
      devices.find((device) => device.publicKey === key)?.label || short(key),
    [devices],
  );

  useEffect(() => {
    void (async () => {
      const [identity, account, stored] = await Promise.all([
        prism.identity(),
        prism.accountState(),
        prism.getSettings(),
      ]);

      setVersion(identity.version);
      setSettings(stored);
      setDevices(account.devices);
      setSignedIn(account.email);
      setArrivedSignedIn(account.email !== null);

      // Both sources, minus this machine. A machine arrives here either by having been paired
      // with or by being on the account, and setup should offer whichever is already true.
      setKnown(
        [
          ...new Set([...account.devices.map((device) => device.publicKey), ...identity.hosts]),
        ].filter((key) => key !== identity.publicKey),
      );
    })();
  }, []);

  useEffect(() => {
    prism.onStream(setStream);
  }, []);

  // The last screen is reached by the connection working, not by anybody pressing anything.
  useEffect(() => {
    if (step === 'connecting' && stream.phase === 'streaming' && (stream.stats?.frames ?? 0) > 0) {
      go('ready');
    }

    if (step === 'connecting' && stream.phase === 'failed' && stream.log.length > 0) {
      setConnectError(stream.log.slice(-6).join('\n'));
    }
  }, [step, stream]);

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
        setDevices(state.devices);
        setKnown((was) =>
          [...new Set([...state.devices.map((device) => device.publicKey), ...was])].filter(
            (key) => key !== state.publicKey,
          ),
        );
        setPassword('');
        setConfirm('');
        setAskingCode(false);
        setGreeted(true);
      } catch (error) {
        setAccountTrouble(reason(error));
      } finally {
        setWorking(false);
      }
    })();
  };

  /**
   * Creates an account and shows the second factor, once.
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
        setEnrolment(await prism.accountRegister(email.trim(), password));
      } catch (error) {
        setAccountTrouble(reason(error));
      } finally {
        setWorking(false);
      }
    })();
  };

  /**
   * Opens a stream onto a machine.
   */
  const open = async (host: string, where: string): Promise<void> => {
    setConnectError(null);
    setTarget(host);
    go('connecting');

    try {
      setStream(await prism.connect(host, where));
    } catch (error) {
      setConnectError(reason(error));
    }
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
                ? 'Enter your code'
                : enrolment
                  ? 'One more thing'
                  : joining
                    ? 'Create your account'
                    : 'Welcome back'}
          </h2>
          <p className="mt-3.5 max-w-[min(560px,38.9vw)] text-body-2 text-muted">
            {greeted
              ? 'Every machine on this account now knows about this one, and this one knows about them.'
              : askingCode
                ? `Six digits from your authenticator, for ${email.trim()}.`
                : enrolment
                  ? 'Set up the second factor now. It is the only time it is shown.'
                  : joining
                    ? 'An account is how your machines find each other, and how this one is recognised when it asks.'
                    : 'Sign in and every machine on your account finds this one.'}
          </p>

          {greeted ? null : askingCode ? (
            <CodeBoxes
              disabled={working}
              onComplete={(code) => {
                signIn(code);
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
              {/* Straight to the code. The account exists, the address and password are still
                  in hand, and the only thing left is the six digits that were just set up. */}
              <button
                type="button"
                className="btn-primary-sm no-drag mx-auto mt-5 block"
                onClick={() => {
                  setEnrolment(null);
                  setJoining(false);
                  setAskingCode(true);
                }}
              >
                I have it — sign in
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
                  against but a second reading of the same keystrokes. */}
              {joining && (
                <label className="flex flex-col gap-1.5">
                  <span className="text-fine-2 text-dim">Password again</span>
                  <input
                    type="password"
                    autoComplete="new-password"
                    placeholder="••••••••"
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
              )}

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

      {which === 'device' && (
        <section className={cls}>
          <h2 className="max-w-[min(700px,48.6vw)] text-title font-semibold">
            Your machines
          </h2>
          <p className="mt-3.5 max-w-[min(580px,40.3vw)] text-body-2 text-muted">
            Install PRISM on the machine you want to reach, sign in to the same account, and
            press Share on it. It appears here.
          </p>

          <div className="mt-11 flex w-[min(620px,43.1vw)] flex-col gap-2.5 text-left">
            {known.length === 0 ? (
              <div className="py-[18px] text-center text-note text-dim">
                Nothing yet — the next machine you sign in on shows up here
              </div>
            ) : (
              known.map((key) => {
                const where = settings?.addresses[key] ?? '';

                return (
                  <div
                    key={key}
                    className="flex items-center gap-3.5 rounded-panel border border-line-2 bg-wash-1 py-3.5 pr-3.5 pl-[18px]"
                  >
                    <img
                      src={where ? 'assets/status-live-04.svg' : 'assets/status-idle-04.svg'}
                      alt=""
                      className="block size-2 flex-none overflow-visible"
                    />
                    <span className="flex min-w-0 flex-1 flex-col gap-[3px]">
                      <span className="text-row font-medium">{machineName(key)}</span>
                      <span className="text-fine-2 leading-tight text-muted-2">
                        {where || 'through the rendezvous server'}
                      </span>
                    </span>
                    <button
                      type="button"
                      className="btn-secondary no-drag"
                      onClick={() => {
                        void open(key, where);
                      }}
                    >
                      Connect
                    </button>
                  </div>
                );
              })
            )}
          </div>

          {/* A second machine may not be to hand yet, and nothing else in the flow is waiting
              on one. This is a step to come back to rather than a way out of setup. */}
          <button type="button" className="btn-ghost no-drag mt-7" onClick={finish}>
            Not now
          </button>
        </section>
      )}

      {which === 'connecting' && (
        <section className={cls}>
          <Pulse />
          <h2 className="mt-5 max-w-[min(700px,48.6vw)] text-title font-semibold">
            Connecting to {machineName(target ?? '')}
          </h2>
          <p className="mt-2.5 whitespace-pre-wrap text-fine text-muted-2">
            {settings?.addresses[target ?? '']
              ? `${settings.addresses[target ?? '']}  ·  direct on your LAN  ·  no relay`
              : 'through the rendezvous server'}
          </p>
          <div className="mt-[59px] w-[min(500px,34.7vw)] overflow-hidden rounded-panel border border-line-2 bg-wash-1 text-left">
            <Stage
              state={stream.terms !== null || stream.phase === 'streaming' ? 'done' : 'doing'}
              what="Secure handshake"
              detail="Ed25519"
            />
            <Stage
              state={
                stream.terms !== null
                  ? 'done'
                  : stream.phase === 'streaming'
                    ? 'doing'
                    : 'waiting'
              }
              what="Negotiating codec"
              detail={
                stream.terms
                  ? stream.stats
                    ? `${stream.terms.codec} · ${stream.stats.mbps.toFixed(0)} Mbps`
                    : stream.terms.codec
                  : '—'
              }
            />
            <Stage
              state={
                (stream.stats?.frames ?? 0) > 0
                  ? 'done'
                  : stream.terms !== null
                    ? 'doing'
                    : 'waiting'
              }
              what="Opening video stream"
              detail={
                stream.terms
                  ? stream.terms.width
                    ? `${stream.terms.width} × ${stream.terms.height} @ ${stream.terms.fps} Hz`
                    : `the host's screen @ ${stream.terms.fps} Hz`
                  : '—'
              }
            />
          </div>
          <button
            type="button"
            className="btn-ghost no-drag mt-[22px]"
            onClick={() => {
              void (async () => {
                setStream(await prism.disconnect());
                go('device');
              })();
            }}
          >
            Cancel
          </button>
          <Trouble message={connectError} className="mt-4" />
        </section>
      )}

      {which === 'ready' && (
        <section className={cls}>
          <span className="inline-flex items-center gap-2 rounded-pill border border-[rgba(77,232,176,0.24)] bg-[rgba(77,232,176,0.12)] py-2 pr-4 pl-3.5 text-note-2 font-medium tracking-[0.3px] text-mint">
            <img src="assets/dot-ready.svg" alt="" className="block size-[7px] overflow-visible" />
            Connected
          </span>
          <h2 className="mt-[19px] max-w-[min(760px,52.8vw)] text-triumph font-semibold">
            You&rsquo;re all set.
          </h2>
          <p className="mt-3 max-w-[min(640px,44.4vw)] text-lead-2 text-muted">
            {machineName(stream.host ?? target ?? '')} is live. Press ⌘↵ from anywhere to
            jump straight back in.
          </p>
          <div className="card mt-[26px] flex w-[min(560px,38.9vw)]">
            <Figure
              label="LATENCY"
              tone="text-mint"
              value={stream.stats ? `${latency(stream.stats.rttMs)} ms` : '—'}
            />
            <Figure label="CODEC" tone="text-cyan" value={stream.terms?.codec ?? '—'} />
            <Figure
              label="DISPLAY"
              tone="text-violet"
              value={
                stream.terms
                  ? stream.terms.height
                    ? `${stream.terms.height}p · ${stream.terms.fps} Hz`
                    : `${stream.terms.fps} Hz`
                  : '—'
              }
            />
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

  const shownSteps = COUNTED.filter((which) => !answered(which));
  const counted = shownSteps.indexOf(step as (typeof COUNTED)[number]);

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
              back ? 'animate-[slide-in-back_420ms_ease-out_both]' : 'animate-[slide-in-forward_420ms_ease-out_both]'
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
 * One of the three figures on the last screen.
 *
 * @param {object} props - What to draw.
 * @param {string} props.label - What it measures.
 * @param {string} props.value - What it measured.
 * @param {string} props.tone - The colour class for the value.
 * @returns {JSX.Element} The figure.
 */
function Figure({
  label,
  value,
  tone,
}: {
  label: string;
  value: string;
  tone: string;
}): JSX.Element {
  return (
    <div className="flex min-w-0 flex-1 flex-col items-center gap-[7px] py-5 not-first:border-l not-first:border-line-1">
      <span className="text-label font-medium text-dim">{label}</span>
      <span className={`text-[17px] font-medium ${tone}`}>{value}</span>
    </div>
  );
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
  onComplete,
}: {
  disabled: boolean;
  onComplete: (code: string) => void;
}): JSX.Element {
  const [characters, setCharacters] = useState<string[]>(Array<string>(CODE_LENGTH).fill(''));
  const boxes = useRef<(HTMLInputElement | null)[]>([]);

  useEffect(() => {
    boxes.current[0]?.focus();
  }, []);

  const put = (next: string[]): void => {
    setCharacters(next);

    if (next.every((character) => character !== '')) {
      onComplete(next.join(''));
    }
  };

  return (
    <div className="mt-[88px] flex gap-2.5">
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
          disabled={disabled}
          aria-label={`Character ${at + 1}`}
          value={character}
          onChange={(event) => {
            const typed = event.target.value.toUpperCase().slice(0, 1);
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

            const pasted = event.clipboardData.getData('text').toUpperCase().replace(/\s/g, '');
            const next = characters.map((_, index) => pasted[index] ?? '');

            boxes.current[Math.min(pasted.length, CODE_LENGTH - 1)]?.focus();
            put(next);
          }}
          className={`no-drag h-[72px] w-[62px] rounded-panel border p-0 text-center text-digit font-medium text-ink caret-[rgba(124,92,255,0.9)] outline-none ${
            character === '' ? 'border-line-4 bg-wash-1' : 'border-line-4 bg-wash-4'
          } focus:border-[1.6px] focus:border-[rgba(124,92,255,0.85)] focus:shadow-[0_0_18px_rgba(124,92,255,0.35)]`}
        />
      ))}
    </div>
  );
}

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <Setup />
  </StrictMode>,
);
