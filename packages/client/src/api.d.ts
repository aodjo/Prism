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
export interface Settings {
  /** Rendezvous server to find hosts through, or empty to connect directly. */
  rendezvous: string;
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
export interface StreamState {
  /** `idle`, `connecting`, `streaming`, `stopped` or `failed`. */
  readonly phase: string;
  /** The host being watched, as hex, while a stream is running. */
  readonly host: string | null;
  /** The last few lines the stream process wrote, which is what explains a failure. */
  readonly log: readonly string[];
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
