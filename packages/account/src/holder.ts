/**
 * The account, as an application's main process holds it.
 *
 * Both applications do the same four things with an account — resume a kept session, sign in,
 * sign out, and act on what the account says about the machines it knows — and the two ends of
 * one session have to agree about all four or the product breaks in ways that look like a
 * network fault. So this is written once and used twice rather than copied.
 *
 * Everything here happens at human speed. None of it is on the frame path and none of it may
 * ever be.
 */

import { AccountClient, AccountError } from './index.js';
import type { AccountDevice, AccountEnrolment } from './index.js';
import { forgetSession, keepSession, storedSession } from './stored-session.js';

/** One machine on the account, as a window shows it. */
export interface DeviceView {
  /** Its long-term public key, as hex. */
  readonly publicKey: string;
  /** What its owner calls it. */
  readonly label: string;
  /** Whether it is the machine showing this. */
  readonly isThisMachine: boolean;
}

/** What is known about the account right now. */
export interface AccountView {
  /** Where the account server is, or empty when none is configured. */
  readonly server: string;
  /** The address signed in as, or `null` when nobody is. */
  readonly email: string | null;
  /** This machine's own public key, as hex. */
  readonly publicKey: string;
  /** Every machine on the account, this one included. */
  readonly devices: readonly DeviceView[];
  /** Whether the account may use the relay. */
  readonly relayAllowed: boolean;
  /** What went wrong the last time something was tried, if anything. */
  readonly error: string | null;
}

/** The parts of the native addon an account needs. */
export interface Native {
  /** Derives the sign-in secret from a password. Memory-hard, so it is native. */
  accountAuth(password: string, salt: string): Promise<string>;
  /** This machine's long-term public key, as hex. */
  identityPublicKey(): string;
  /** Records every machine the account named as one this machine will talk to. */
  accountTrustDevices(publicKeys: string[]): number;
}

/** What the holder needs from the application's settings, whatever shape they are in. */
export interface Store {
  /** Where the account server is. */
  server(): string;
  /** The signalling address configured by hand, or empty. */
  rendezvous(): string;
  /** Records the signalling address the account handed over. */
  setRendezvous(address: string): void;
}

/**
 * Holds one machine's account.
 */
export class Holder {
  /** The native calls this needs. */
  private readonly native: Native;

  /** Where the settings this reads and writes live. */
  private readonly store: Store;

  /** The client, rebuilt whenever the server address changes. */
  private client: AccountClient | null = null;

  /** The address signed in as, or `null`. */
  private email: string | null = null;

  /** Every machine the account knows, as of the last time it said. */
  private devices: readonly DeviceView[] = [];

  /** Whether the account may use the relay. */
  private relayAllowed = false;

  /** What went wrong the last time the account was asked something. */
  private trouble: string | null = null;

  /**
   * The attempt to reuse a token kept from a previous run.
   *
   * Held so that a window can wait for it. Without that, the first thing drawn after opening
   * an application is a sign-in form, replaced a moment later by the account that was signed
   * in all along — which reads as having been signed out.
   */
  private resuming: Promise<void> | null = null;

  /**
   * Builds a holder over an addon and a settings store.
   *
   * @param {Native} native - The addon calls this needs.
   * @param {Store} store - Where to read the server address and write the signalling one.
   */
  constructor(native: Native, store: Store) {
    this.native = native;
    this.store = store;
  }

  /**
   * Starts the attempt to reuse a kept session.
   *
   * @returns {void}
   */
  start(): void {
    this.resuming = this.resume();
  }

  /**
   * Describes the account for a window, once any resume has finished.
   *
   * @async
   * @returns {Promise<AccountView>} What is known right now.
   */
  async view(): Promise<AccountView> {
    await this.resuming;

    return this.snapshot();
  }

  /**
   * Creates an account and returns what to put into an authenticator app.
   *
   * @async
   * @param {string} email - The address to register.
   * @param {string} password - The password, which is never sent.
   * @param {string} code - The six digits sent to that address, or empty when none was sent.
   * @returns {Promise<AccountEnrolment>} The second factor, once.
   * @throws {Error} If no server is configured, or the server refused.
   */
  async register(email: string, password: string, code: string): Promise<AccountEnrolment> {
    const client = this.reach();
    this.trouble = null;

    return client.register(email, password, code);
  }

  /**
   * Asks the server to send a signup code to an address.
   *
   * @async
   * @param {string} email - The address to prove.
   * @returns {Promise<boolean>} Whether a code was sent and has to be typed back in.
   * @throws {Error} If no server is configured, or the server refused.
   */
  async challenge(email: string): Promise<boolean> {
    const client = this.reach();
    this.trouble = null;

    return client.challenge(email);
  }

  /**
   * Signs in, tells the account about this machine, and trusts every machine it names.
   *
   * @async
   * @param {string} email - The address to sign in with.
   * @param {string} password - The password, which is never sent.
   * @param {string} code - The six digits from an authenticator app.
   * @param {string} label - What to call this machine on the account.
   * @returns {Promise<AccountView>} What is known afterwards.
   * @throws {Error} If any of the three is wrong, reported as one failure.
   */
  async signIn(
    email: string,
    password: string,
    code: string,
    label: string,
  ): Promise<AccountView> {
    const client = this.reach();

    try {
      const session = await client.signIn(email, password, code);
      this.email = email;
      this.relayAllowed = session.relayAllowed;

      // This machine tells the account about itself before reading the list, so that the list
      // it reads already has it in — otherwise the first sign-in on a machine shows every
      // computer except the one in front of you.
      this.adopt(await client.registerDevice(this.native.identityPublicKey(), label));
      this.adoptRendezvous(session.rendezvous);
      this.trouble = null;

      // Kept only once both halves have worked. A token stored before this machine had been
      // registered would come back to a list that does not have it in.
      keepSession({ email, token: session.token });
    } catch (error) {
      this.email = null;
      this.trouble = message(error);
      throw new Error(this.trouble);
    }

    return this.snapshot();
  }

  /**
   * Ends the session, here and on the server.
   *
   * @async
   * @returns {Promise<AccountView>} What is known afterwards.
   */
  async signOut(): Promise<AccountView> {
    await this.client?.signOut();
    forgetSession();

    this.email = null;
    this.trouble = null;

    // The machines stay trusted. They were on the account, and signing out is not a statement
    // that they are not yours.
    this.devices = [];

    return this.snapshot();
  }

  /**
   * Renames this machine on the account.
   *
   * The same call that registered it: the server keeps one entry per key, so registering a key
   * it already has is how a label is changed. Which means renaming needs no endpoint of its
   * own, and cannot leave a machine listed twice under two names.
   *
   * @param {string} label - What to call it from now on.
   * @returns {Promise<AccountView>} The account as it stands afterwards.
   * @throws {Error} If nobody is signed in, or the server refuses.
   */
  async rename(label: string): Promise<AccountView> {
    const client = this.reach();

    try {
      this.adopt(await client.registerDevice(this.native.identityPublicKey(), label));
      this.trouble = null;
    } catch (error) {
      this.trouble = message(error);
      throw new Error(this.trouble);
    }

    return this.snapshot();
  }

  /**
   * Removes a machine from the account.
   *
   * @async
   * @param {string} publicKey - The machine's public key, as hex.
   * @returns {Promise<AccountView>} What is known afterwards.
   * @throws {Error} If no server is configured, or the server refused.
   */
  async forget(publicKey: string): Promise<AccountView> {
    const client = this.reach();

    try {
      this.adopt(await client.forgetDevice(publicKey));
      this.trouble = null;
    } catch (error) {
      this.trouble = message(error);
      throw new Error(this.trouble);
    }

    return this.snapshot();
  }

  /**
   * Whether this machine has a session it can resume.
   *
   * Read from the file rather than from what has been resumed, because a launch asks this
   * before it has had time to reach the server — and the question is whether somebody signed
   * in here, not whether the server can be reached right now.
   *
   * @returns {boolean} Whether a session is stored.
   */
  static signedInBefore(): boolean {
    return storedSession() !== null;
  }

  /**
   * Asks the account again who its machines are, and records the answer.
   *
   * The list is not a thing this machine decides, so it goes stale the moment somebody signs in
   * somewhere else. Cheap enough to do whenever a window comes forward, which is the moment
   * somebody is about to look at the list and expect it to be right.
   *
   * @async
   * @returns {Promise<boolean>} Whether the machines are different from what was known before.
   */
  async refresh(): Promise<boolean> {
    const before = this.devices.map((device) => device.publicKey).join(',');

    await this.resume();

    return this.devices.map((device) => device.publicKey).join(',') !== before;
  }

  /**
   * Signs in with the token an earlier run kept, if there is one and it is still good.
   *
   * A server that cannot be reached is not the same as a token that has expired: the first is
   * temporary and the token stays, the second is permanent and it goes. Treating them alike
   * would sign somebody out of their own account because their network was down for a minute.
   *
   * @async
   * @returns {Promise<void>}
   */
  private async resume(): Promise<void> {
    const stored = storedSession();

    if (!stored) {
      return;
    }

    let client: AccountClient;
    try {
      client = this.reach();
    } catch {
      return;
    }

    try {
      const session = await client.resume(stored.token);

      if (!session) {
        forgetSession();
        return;
      }

      this.email = session.email;
      this.relayAllowed = session.relayAllowed;
      this.adopt(session.devices);
      this.adoptRendezvous(session.rendezvous);
      this.trouble = null;
    } catch (error) {
      this.trouble = message(error);
    }
  }

  /**
   * Returns a client for the configured server, building one if the address has changed.
   *
   * Rebuilt rather than reconfigured, because a client holds a session and a session belongs to
   * the server that issued it. Carrying one across a change of address would send somebody's
   * token to a machine that never gave it to them.
   *
   * @returns {AccountClient} The client.
   * @throws {Error} If no server is configured.
   */
  private reach(): AccountClient {
    const server = this.store.server().trim();

    if (server === '') {
      this.client = null;
      this.email = null;
      throw new Error('set an account server first');
    }

    if (!this.client || this.client.base !== server) {
      // A token is only worth anything to the server that issued it, so pointing a machine at a
      // different one throws it away rather than offering it to a stranger. Building the first
      // client of a run is not that: there is nothing to point away from, and the token waiting
      // on disk is the one this client is about to use.
      if (this.client) {
        forgetSession();
      }

      this.client = new AccountClient(server, this.native.accountAuth);
      this.email = null;
    }

    return this.client;
  }

  /**
   * Records what the account said, and trusts every machine it named.
   *
   * Trusting is the point of the whole arrangement: two machines signed in to the same account
   * are told about each other, and that is the only thing that makes one willing to talk to
   * the other.
   *
   * @param {readonly AccountDevice[]} devices - What the account listed.
   * @returns {void}
   */
  private adopt(devices: readonly AccountDevice[]): void {
    const mine = this.native.identityPublicKey();

    this.native.accountTrustDevices(devices.map((device) => device.publicKey));

    this.devices = devices.map((device) => ({
      publicKey: device.publicKey,
      label: device.label,
      isThisMachine: device.publicKey === mine,
    }));
  }

  /**
   * Takes the signalling address the account handed over.
   *
   * Anything already configured by hand wins: somebody who typed an address meant it, and an
   * account should not quietly replace it.
   *
   * @param {string} address - Where to register, or empty when the server does not say.
   * @returns {void}
   */
  private adoptRendezvous(address: string): void {
    if (address === '' || this.store.rendezvous().trim() !== '') {
      return;
    }

    this.store.setRendezvous(address);
  }

  /**
   * Describes the account without waiting for anything.
   *
   * @returns {AccountView} What is known right now.
   */
  private snapshot(): AccountView {
    return {
      server: this.store.server(),
      email: this.email,
      publicKey: this.native.identityPublicKey(),
      devices: this.devices,
      relayAllowed: this.relayAllowed,
      error: this.trouble,
    };
  }
}

/**
 * Turns a failure into the sentence a person should read.
 *
 * @param {unknown} error - Whatever was thrown.
 * @returns {string} The message.
 */
function message(error: unknown): string {
  if (error instanceof AccountError) {
    return error.message;
  }

  return error instanceof Error ? error.message : String(error);
}
