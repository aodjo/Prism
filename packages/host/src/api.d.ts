/**
 * The contract between the panel and the main process.
 *
 * A declaration file rather than a module, for two reasons. It is the only thing both sides
 * compile against, and a declaration file emits nothing — so the renderer's build produces
 * exactly one script rather than dragging the preload into the window's directory as well.
 * It also makes the surface a stated contract that the preload has to satisfy, rather than
 * whatever shape the preload happens to have that day.
 */

/**
 * What a host session is doing, as the panel sees it.
 *
 * The counters are `bigint` because a session that runs for hours sends more packets than a
 * double counts exactly, and a byte total that quietly starts rounding is a statistic that
 * lies rather than one that is merely imprecise.
 */
/**
 * One thing the system still has to allow.
 *
 * Reported rather than assumed because both grants fail quietly: capture without Screen
 * Recording delivers black frames and silence, injection without Accessibility posts events
 * that go nowhere, and neither raises an error. A host that did not ask first looks like one
 * that is working and is not.
 */
export interface MissingGrant {
  /** Which grant this is: `screen` or `input`. */
  readonly id: string;
  /** What the system's own settings call it. */
  readonly name: string;
  /** Why a host needs it, in one sentence. */
  readonly purpose: string;
  /** A link that opens the settings pane holding it. */
  readonly settingsUrl: string;
}

/** What this machine currently allows a host to do. */
export interface HostPermissions {
  /** Whether the screen may be recorded, which also covers capturing its audio. */
  readonly screen: boolean;
  /** Whether this machine may be controlled. */
  readonly input: boolean;
  /** What is still missing, ready to show. */
  readonly missing: readonly MissingGrant[];
}

export interface HostSnapshot {
  /** `opening`, `waiting`, `streaming`, `stopped` or `failed`. */
  readonly phase: string;
  /** Where the rendezvous server sees this machine, once it has said. */
  readonly observed: string | null;
  /**
   * The address this machine is actually listening on.
   *
   * What somebody on the same network has to be told, and the only way to reach this host
   * when no rendezvous server is configured.
   */
  readonly local: string | null;
  /** The connected client's public key as hex, once one has connected. */
  readonly peer: string | null;
  /** Frames captured, encoded and sent. */
  readonly frames: bigint;
  /** Packets put on the wire, parity included. */
  readonly packets: bigint;
  /** Bytes put on the wire, headers included. */
  readonly bytes: bigint;
  /** What sending has worked out to so far, in bits per second. */
  readonly bitrateBps: bigint;
  /** What went wrong, when the phase is `failed`. */
  readonly error: string | null;
}

/** Who this machine is and what it has paired with. */
export interface Identity {
  /** The core's version string. */
  readonly version: string;
  /** The wire format revision this build speaks. */
  readonly wireFormat: number;
  /** This machine's public key as hex. */
  readonly publicKey: string;
  /** Every machine this one has paired with, as hex. */
  readonly peers: readonly string[];
}

/**
 * What this machine is configured to do.
 *
 * Deliberately small. Anything that can be derived — the identity, the list of paired
 * machines — is derived, because two copies of one fact drift apart and the copy a person is
 * looking at is then the wrong one.
 */
export interface Settings {
  /** Rendezvous server to register with, or empty to be reachable only directly. */
  rendezvous: string;
  /** Address to listen on. Port zero lets the operating system choose. */
  bind: string;
  /** Frames per second to capture at. */
  fps: number;
  /** Target bitrate in bits per second. */
  bitrateBps: number;
  /** Whether a connected client may control this machine. */
  injectInput: boolean;
  /** Whether to start hosting as soon as the application launches. */
  autoStart: boolean;
}

/** What one pairing exchange produced. */
export interface Paired {
  /** The machine that just paired, as hex. */
  readonly peer: string;
  /** Every paired machine, including the new one. */
  readonly peers: readonly string[];
}

/**
 * The whole surface between the panel and the machine.
 *
 * Everything here is a control call. No frame, no packet and no private key crosses this
 * boundary, and the reason the list is short enough to read in one go is that keeping it short
 * is what keeps that true.
 */
export interface PrismApi {
  /**
   * Returns who this machine is and what it has paired with.
   *
   * @async
   * @returns {Promise<Identity>} The core's version, this machine's public key, and its peers.
   */
  identity(): Promise<Identity>;

  /**
   * Returns what the system allows, without prompting for anything.
   *
   * Safe to call whenever the panel redraws.
   *
   * @returns {Promise<HostPermissions>} What is held and what is missing.
   */
  permissions(): Promise<HostPermissions>;

  /**
   * Asks the system for one grant, opening its settings pane if the answer is already no.
   *
   * The system prompts at most once in the life of an application; after that only a person
   * can change the answer, so a refusal here sends them where they can.
   *
   * @param {string} id - Which grant, as `MissingGrant.id`.
   * @returns {Promise<HostPermissions>} What is held afterwards.
   */
  requestPermission(id: string): Promise<HostPermissions>;

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
   * Generates a six digit pairing code.
   *
   * Separate from waiting for it to be used, because the code has to be on screen before the
   * waiting starts and the waiting does not end until somebody has used it.
   *
   * @async
   * @returns {Promise<string>} Six digits.
   */
  pairingCode(): Promise<string>;

  /**
   * Waits for one client to pair using a code that is already on screen.
   *
   * @async
   * @param {string} bind - Address to listen on while pairing.
   * @param {string} code - The six digits being shown.
   * @returns {Promise<Paired>} The client's key and the new peer list.
   * @throws {Error} If nobody pairs before the code expires or the code was mistyped.
   */
  awaitPairing(bind: string, code: string): Promise<Paired>;

  /**
   * Starts hosting with the stored settings.
   *
   * @async
   * @returns {Promise<HostSnapshot | null>} What the session is doing a moment after starting.
   * @throws {Error} If no client has been paired or an address cannot be parsed.
   */
  startHosting(): Promise<HostSnapshot | null>;

  /**
   * Ends the running session.
   *
   * @async
   * @returns {Promise<null>} Once the session's thread has finished.
   */
  stopHosting(): Promise<null>;

  /**
   * Returns what the session is doing right now.
   *
   * @async
   * @returns {Promise<HostSnapshot | null>} The snapshot, or `null` when not hosting.
   */
  snapshot(): Promise<HostSnapshot | null>;

  /**
   * Tells the main process how tall the panel's content is.
   *
   * The window is sized to its content rather than to a number chosen once, because the
   * content grows: a machine paired with six devices needs more room than one paired with
   * none, and a fixed height is either too short for one or padded with nothing for the other.
   *
   * @param {number} height - The content's height in CSS pixels.
   * @returns {void}
   */
  shelf(open: boolean, height: number): void;

  /**
   * Shows what the handle offers on a right click.
   *
   * @returns {void}
   */
  shelfMenu(): void;

  /**
   * Listens for the main process folding or unfolding the shelf.
   *
   * Clicking away folds it, and the handle's own menu can ask for it back. Both happen outside
   * this window, so the panel is told rather than asked.
   *
   * @param {(open: boolean) => void} listener - Called with the new state.
   * @returns {void}
   */
  onFold(listener: (open: boolean) => void): void;

  /**
   * Registers a listener for snapshots the main process pushes while the panel is open.
   *
   * Pushed rather than polled from here so a hidden panel costs nothing: the main process
   * knows whether anybody is looking and a renderer does not.
   *
   * @param {(snapshot: HostSnapshot | null) => void} listener - Called on each snapshot.
   * @returns {void}
   */
  onSnapshot(listener: (snapshot: HostSnapshot | null) => void): void;
}
