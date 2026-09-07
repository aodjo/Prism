/**
 * The contract between the client window and the main process.
 *
 * A declaration file rather than a module: it is the only thing both sides compile against, it
 * emits nothing, and it makes the surface a stated contract the preload has to satisfy rather
 * than whatever shape the preload happens to have that day.
 */

/** Who this machine is and which hosts it has paired with. */
export interface Identity {
  /** The core's version string. */
  readonly version: string;
  /** The wire format revision this build speaks. */
  readonly wireFormat: number;
  /** This machine's public key as hex. */
  readonly publicKey: string;
  /** Every host this machine has paired with, as hex. */
  readonly hosts: readonly string[];
}

/** What this machine is configured to do. */
/**
 * What is known about the account, if any.
 *
 * A machine with no account is not a lesser one: it pairs by reading six digits off the host's
 * screen, which is what every machine did before accounts existed and what one still does when
 * there is no server to sign in to.
 */
export interface AccountState {
  /** Where the account server is, or empty when none is configured. */
  readonly server: string;
  /** The address signed in as, or `null`. */
  readonly email: string | null;
  /** This machine's own public key, as hex. */
  readonly publicKey: string;
  /** Every machine on the account, this one included. */
  readonly devices: readonly AccountDeviceView[];
  /** Whether the account may use the relay. */
  readonly relayAllowed: boolean;
  /** What went wrong the last time something was tried, if anything. */
  readonly error: string | null;
}

/** One machine on the account, as the window shows it. */
export interface AccountDeviceView {
  /** Its long-term public key, as hex. */
  readonly publicKey: string;
  /** What its owner calls it. */
  readonly label: string;
  /** Whether it is the machine this window is running on. */
  readonly isThisMachine: boolean;
}

/**
 * What creating an account produced, shown once and never again.
 *
 * The server keeps only enough to check codes, which is not enough to show the secret a second
 * time. Somebody who closes this without scanning it has to start over.
 */
export interface AccountEnrolmentView {
  /** A QR code of the provisioning link, as a data URI. */
  readonly qr: string;
  /** The same secret as text, for typing in when a camera is not to hand. */
  readonly secret: string;
}

export interface Settings {
  /** Rendezvous server to find hosts through, or empty to connect directly. */
  rendezvous: string;
  /**
   * Account server to sign in to, or empty to pair by code instead.
   *
   * Signing in is what makes a machine's own machines follow it: the account knows which
   * public keys are its own, so two of them trust each other without anybody reading digits
   * off a screen.
   */
  accountServer: string;

  /**
   * Whether the setup flow has been through once.
   *
   * Stored rather than inferred from whether anything is paired, because somebody who skipped
   * setup deliberately should not be asked again every time they open the application with no
   * machines added.
   */
  setupDone: boolean;
  /** Whether to send input to the host, or only watch. */
  control: boolean;
  /** Whether to even out arrival jitter at the cost of a little latency. */
  smooth: boolean;
  /**
   * Where each paired host can be reached directly, keyed by its public key.
   *
   * What a machine on the same network needs and a rendezvous server otherwise supplies. The
   * host shows the address it is listening on; this is where it gets typed. Kept per host
   * because a person with two machines has two answers, and retyping the right one every time
   * is how the wrong one gets used.
   */
  addresses: Record<string, string>;
}

/**
 * What the stream process is doing.
 *
 * The stream runs in a process of its own rather than in this one. On macOS a window has to
 * be driven from the main thread and Electron already owns that thread, so a stream window
 * inside this process could not exist — and putting the frame path in a separate process is
 * the better arrangement anyway.
 */
/**
 * What the stream agreed to carry, said once when the session opens.
 *
 * Fixed for the life of a session: both sides negotiated it and neither can change it without
 * opening a new one.
 */
export interface StreamTerms {
  /** The video codec, as the two sides named it. */
  readonly codec: string;
  /** Pixels across, or zero when this side asked for whatever the host's screen is. */
  readonly width: number;
  /** Pixels down, or zero for the same reason. */
  readonly height: number;
  /** Frames a second the host will send. */
  readonly fps: number;
}

/**
 * What the stream is doing right now, refreshed once a second.
 *
 * Numbers only. This is the whole of what a running stream tells the interface, and it is why
 * the interface can show a figure without a frame ever reaching it.
 */
export interface StreamStats {
  /** Best round trip seen so far, in milliseconds. */
  readonly rttMs: number;
  /** Frames arriving a second, over the last second. */
  readonly fps: number;
  /** What is actually arriving, in megabits a second. */
  readonly mbps: number;
  /** Frames reassembled since the stream opened. */
  readonly frames: number;
}

export interface StreamState {
  /** `idle`, `connecting`, `streaming`, `stopped` or `failed`. */
  readonly phase: string;
  /** The host being watched, as hex, while a stream is running. */
  readonly host: string | null;
  /** What the two sides agreed to, once they have. */
  readonly terms: StreamTerms | null;
  /** What is happening, as of the last second. */
  readonly stats: StreamStats | null;
  /** The last few lines the stream process wrote, which is what explains a failure. */
  readonly log: readonly string[];
}

/** One system grant this machine has not given yet. */
export interface MissingGrant {
  /** What to pass back to ask for it. */
  readonly id: string;
  /** What the system's own settings call it. */
  readonly name: string;
  /** Why it is needed, in one sentence. */
  readonly purpose: string;
  /** A link that opens the settings pane holding it. */
  readonly settingsUrl: string;
}

/**
 * What this machine currently allows.
 *
 * A client that only watches another screen needs neither of these. They are asked for during
 * setup because the machine somebody sets up is usually also one they will want to reach from
 * somewhere else, and the moment to ask is once, at the start.
 */
export interface HostPermissions {
  /** Whether the screen may be recorded. */
  readonly screen: boolean;
  /** Whether this machine may be controlled. */
  readonly input: boolean;
  /** What is still missing, ready to show. */
  readonly missing: readonly MissingGrant[];
}

/** What one pairing exchange produced. */
export interface Paired {
  /** The host that was just paired with, as hex. */
  readonly peer: string;
  /** Every paired host, including the new one. */
  readonly hosts: readonly string[];
}

/**
 * The whole surface between the window and the machine.
 *
 * Every entry is a control call. No frame and no private key crosses this boundary.
 */
export interface PrismApi {
  /**
   * Returns who this machine is and which hosts it has paired with.
   *
   * @async
   * @returns {Promise<Identity>} This machine's key and its hosts.
   */
  identity(): Promise<Identity>;

  /**
   * Returns what this machine allows, without prompting for anything.
   *
   * @returns {Promise<HostPermissions>} What is held and what is missing.
   */
  permissions(): Promise<HostPermissions>;

  /**
   * Asks the system for one grant, opening its settings pane if the answer is already no.
   *
   * @param {string} id - The grant's id, from [`MissingGrant`].
   * @returns {Promise<HostPermissions>} What is held afterwards.
   */
  requestPermission(id: string): Promise<HostPermissions>;

  /**
   * Closes setup and opens the home window.
   *
   * Called once, at the end of the flow or when somebody skips it. Setup does not run again
   * unless this machine forgets everything it knows.
   *
   * @returns {void}
   */
  finishSetup(): void;

  /**
   * Opens the settings window, or brings it forward if it is already open.
   *
   * @returns {void}
   */
  openSettings(): void;

  /**
   * Returns what is known about the account.
   *
   * @returns {Promise<AccountState>} The current state, signed in or not.
   */
  accountState(): Promise<AccountState>;

  /**
   * Creates an account and returns the second factor to set up, once.
   *
   * @param {string} email - The address to sign in with.
   * @param {string} password - The password, which never leaves this machine.
   * @returns {Promise<AccountEnrolmentView>} What to put into an authenticator app.
   * @throws {Error} If the name is taken, or the server cannot be reached.
   */
  accountRegister(email: string, password: string): Promise<AccountEnrolmentView>;

  /**
   * Signs in, registers this machine, and trusts every other machine on the account.
   *
   * @param {string} email - The address the account is under.
   * @param {string} password - The password, which never leaves this machine.
   * @param {string} code - Six digits from an authenticator app.
   * @param {string} label - What to call this machine.
   * @returns {Promise<AccountState>} The state afterwards.
   * @throws {Error} If any of the three is wrong, which is reported as one failure.
   */
  accountSignIn(
    email: string,
    password: string,
    code: string,
    label: string,
  ): Promise<AccountState>;

  /**
   * Forgets the session on this machine.
   *
   * Machines already trusted stay trusted: they were paired, and signing out is not the same
   * as saying they are not yours.
   *
   * @returns {Promise<AccountState>} The state afterwards.
   */
  accountSignOut(): Promise<AccountState>;

  /**
   * Removes a machine from the account.
   *
   * @param {string} publicKey - The machine's public key, as hex.
   * @returns {Promise<AccountState>} The state afterwards.
   * @throws {Error} If the session has expired.
   */
  accountForgetDevice(publicKey: string): Promise<AccountState>;

  /**
   * Returns the stored settings.
   *
   * @async
   * @returns {Promise<Settings>} What this machine is configured to do.
   */
  getSettings(): Promise<Settings>;

  /**
   * Changes some settings and stores the result.
   *
   * @async
   * @param {Partial<Settings>} next - The fields to change.
   * @returns {Promise<Settings>} The settings as they now stand.
   */
  setSettings(next: Partial<Settings>): Promise<Settings>;

  /**
   * Pairs with a host that is showing a code.
   *
   * @async
   * @param {string} host - The address the host is waiting on.
   * @param {string} code - The six digits it is showing.
   * @returns {Promise<Paired>} The host's key and the new list.
   * @throws {Error} If the code was not accepted or the host did not answer.
   */
  pair(host: string, code: string): Promise<Paired>;

  /**
   * Opens a stream window onto a paired host.
   *
   * @async
   * @param {string} host - The host's public key as hex.
   * @param {string} address - Its address, or empty to find it through the rendezvous server.
   * @returns {Promise<StreamState>} What the stream is doing a moment after starting.
   * @throws {Error} If neither an address nor a rendezvous server is configured.
   */
  connect(host: string, address: string): Promise<StreamState>;

  /**
   * Closes the stream window.
   *
   * @async
   * @returns {Promise<StreamState>} Once the stream process has ended.
   */
  disconnect(): Promise<StreamState>;

  /**
   * Returns what the stream is doing right now.
   *
   * @async
   * @returns {Promise<StreamState>} The current state.
   */
  streamState(): Promise<StreamState>;

  /**
   * Tells the main process how tall the window's content is.
   *
   * @param {number} height - The content's height in CSS pixels.
   * @returns {void}
   */
  fit(height: number): void;

  /**
   * Registers a listener for stream state the main process pushes.
   *
   * @param {(state: StreamState) => void} listener - Called whenever the state changes.
   * @returns {void}
   */
  onStream(listener: (state: StreamState) => void): void;
}
