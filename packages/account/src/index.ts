/**
 * Talking to the account server.
 *
 * The network half of signing in. The cryptographic half is native, and the split is not
 * arbitrary: the password becomes two secrets, and only one of them belongs on this side of
 * the boundary. `accountAuth` in the addon returns that one and keeps the other, so the value
 * that unlocks a private key is never a JavaScript string that could be logged, serialised
 * into an error report, or sent to the wrong place.
 *
 * Everything here happens at human speed. None of it is on the frame path and none of it may
 * ever be.
 */

/** One machine on an account. */
export interface AccountDevice {
  /** Its long-term public key, as hex. */
  readonly publicKey: string;
  /** What its owner calls it. */
  readonly label: string;
  /** When it was added, in seconds since the epoch. */
  readonly addedUnix: number;
}

/** What signing in produced. */
export interface AccountSession {
  /** Proves later requests are this account's, until the server restarts or it expires. */
  readonly token: string;
  /** Every machine the account knows, this one included once it has registered. */
  readonly devices: readonly AccountDevice[];
  /** Whether the relay may be used, which costs bandwidth somebody pays for. */
  readonly relayAllowed: boolean;
}

/** What creating an account produced, and will not produce again. */
export interface AccountEnrolment {
  /** The link an authenticator app reads from a QR code. */
  readonly totpUri: string;
  /** The same secret as text, for typing in by hand when a camera is not to hand. */
  readonly totpSecret: string;
}

/**
 * A request the account server refused.
 *
 * Carries the status because the three that matter are told apart by it and by nothing else:
 * the server deliberately says the same thing for a wrong password, a wrong code and an
 * address with no account behind it.
 */
export class AccountError extends Error {
  /** The HTTP status the server answered with. */
  readonly status: number;

  /**
   * Builds an error from what the server said.
   *
   * @param {number} status - The HTTP status.
   * @param {string} message - What to show.
   */
  constructor(status: number, message: string) {
    super(message);
    this.name = 'AccountError';
    this.status = status;
  }
}

/**
 * Derives the sign-in secret from a password.
 *
 * Supplied by the caller rather than imported, because this module is shared by two
 * applications and neither of them should be reaching into the other's copy of the addon.
 */
export type DeriveAuth = (password: string, salt: string) => Promise<string>;

/** How long to wait for the server before giving up. */
const TIMEOUT_MS = 20_000;

/**
 * The account server, as the thing an application talks to.
 *
 * Holds the session token so that no caller has to remember to attach it, which is the way
 * that eventually gets forgotten on exactly one endpoint.
 */
export class AccountClient {
  /** Where the server is, without a trailing slash. */
  readonly base: string;

  /** Derives the sign-in secret; native, because it is a memory-hard hash. */
  private readonly deriveAuth: DeriveAuth;

  /** The current session, or `null` when nobody is signed in. */
  private token: string | null = null;

  /**
   * Points a client at a server.
   *
   * @param {string} base - The server's base URL, such as `https://rv.example.com`.
   * @param {DeriveAuth} deriveAuth - The native password derivation.
   */
  constructor(base: string, deriveAuth: DeriveAuth) {
    this.base = base.replace(/\/+$/, '');
    this.deriveAuth = deriveAuth;
  }

  /** Whether a session is currently held. */
  get signedIn(): boolean {
    return this.token !== null;
  }

  /**
   * Ends the session, here and on the server.
   *
   * The server is told rather than left to expire the token on its own, because sessions
   * survive a restart now: a token this client merely forgot would go on working for the rest
   * of its twelve hours in the hands of anybody who had read it.
   *
   * Forgetting happens either way. Somebody signing out on a machine they are about to hand
   * over should not stay signed in on it because the network was down.
   *
   * @async
   * @returns {Promise<void>}
   */
  async signOut(): Promise<void> {
    const token = this.token;
    this.token = null;

    if (token === null) {
      return;
    }

    try {
      await this.sendWith(token, 'DELETE', '/v1/session');
    } catch {
      // Nothing to do about it and nothing to say: the token is gone from this machine, and
      // the server drops it when it expires.
    }
  }

  /**
   * Creates an account and returns what to put into an authenticator app.
   *
   * The second factor is shown once. There is no way to ask for it again: the server keeps
   * only what it needs to check codes, which is not enough to show the secret a second time.
   *
   * @async
   * @param {string} email - The address to sign in with.
   * @param {string} password - The password, which is never sent.
   * @returns {Promise<AccountEnrolment>} The second factor, once.
   * @throws {AccountError} If the address already has an account or is malformed, or the
   *   server cannot be reached.
   */
  async register(email: string, password: string): Promise<AccountEnrolment> {
    const salt = await this.saltFor(email);
    const auth = await this.deriveAuth(password, salt);

    const body = await this.send<{ totp_uri: string; totp_secret: string }>(
      'POST',
      '/v1/accounts',
      { email, salt, auth, sealed_key: '' },
    );

    return { totpUri: body.totp_uri, totpSecret: body.totp_secret };
  }

  /**
   * Signs in, and remembers the session for later calls.
   *
   * @async
   * @param {string} email - The address the account is under.
   * @param {string} password - The password, which is never sent.
   * @param {string} code - The six digits from an authenticator app.
   * @returns {Promise<AccountSession>} The session and the machines on the account.
   * @throws {AccountError} If any of the three is wrong, which is reported as one failure.
   */
  async signIn(email: string, password: string, code: string): Promise<AccountSession> {
    const salt = await this.saltFor(email);
    const auth = await this.deriveAuth(password, salt);

    const body = await this.send<{
      token: string;
      devices: { public_key: string; label: string; added_unix: number }[];
      relay_allowed: boolean;
    }>('POST', '/v1/sessions', { email, auth, code: Number(code) });

    this.token = body.token;

    return {
      token: body.token,
      devices: body.devices.map(toDevice),
      relayAllowed: body.relay_allowed,
    };
  }

  /**
   * Signs in with a token kept from a previous run.
   *
   * The token is checked by being used, which is the only check worth anything: a token that
   * looks well-formed and has expired is indistinguishable from a good one until the server
   * says otherwise. A refusal clears it, so a stale token is discarded rather than retried on
   * every later call.
   *
   * @async
   * @param {string} token - What was stored the last time somebody signed in.
   * @returns {Promise<AccountSession & {email: string} | null>} The session, or `null` if the
   *   token is no longer good.
   * @throws {AccountError} If the server could not be reached, which is not the same as the
   *   token being bad and must not throw the token away.
   */
  async resume(token: string): Promise<(AccountSession & { email: string }) | null> {
    this.token = token;

    try {
      const body = await this.send<{
        email: string;
        devices: { public_key: string; label: string; added_unix: number }[];
        relay_allowed: boolean;
      }>('GET', '/v1/session');

      return {
        email: body.email,
        token,
        devices: body.devices.map(toDevice),
        relayAllowed: body.relay_allowed,
      };
    } catch (error) {
      if (error instanceof AccountError && error.status === 401) {
        return null;
      }

      this.token = null;
      throw error;
    }
  }

  /**
   * Tells the account about this machine, and returns every machine it now knows.
   *
   * Signing in again on a machine already listed renames it rather than listing it twice.
   *
   * @async
   * @param {string} publicKey - This machine's long-term public key, as hex.
   * @param {string} label - What to call it.
   * @returns {Promise<readonly AccountDevice[]>} Every machine on the account.
   * @throws {AccountError} If the session has expired or the key is malformed.
   */
  async registerDevice(publicKey: string, label: string): Promise<readonly AccountDevice[]> {
    const body = await this.send<{ devices: { public_key: string; label: string; added_unix: number }[] }>(
      'POST',
      '/v1/devices',
      { public_key: publicKey, label },
    );

    return body.devices.map(toDevice);
  }

  /**
   * Returns every machine on the account.
   *
   * @async
   * @returns {Promise<readonly AccountDevice[]>} The machines.
   * @throws {AccountError} If the session has expired.
   */
  async devices(): Promise<readonly AccountDevice[]> {
    const body = await this.send<{ devices: { public_key: string; label: string; added_unix: number }[] }>(
      'GET',
      '/v1/devices',
    );

    return body.devices.map(toDevice);
  }

  /**
   * Removes a machine from the account.
   *
   * @async
   * @param {string} publicKey - The machine's public key, as hex.
   * @returns {Promise<readonly AccountDevice[]>} What is left.
   * @throws {AccountError} If the session has expired.
   */
  async forgetDevice(publicKey: string): Promise<readonly AccountDevice[]> {
    const body = await this.send<{ devices: { public_key: string; label: string; added_unix: number }[] }>(
      'DELETE',
      `/v1/devices/${encodeURIComponent(publicKey)}`,
    );

    return body.devices.map(toDevice);
  }

  /**
   * Fetches the salt a password must be hashed with.
   *
   * Every address gets an answer, including one with no account — otherwise this call would be
   * a way to find out which addresses are registered, and an address is half of what somebody
   * guessing needs.
   *
   * @async
   * @param {string} email - The address the account is under.
   * @returns {Promise<string>} The salt, as hex.
   * @throws {AccountError} If the server cannot be reached.
   */
  private async saltFor(email: string): Promise<string> {
    const body = await this.send<{ salt: string }>(
      'GET',
      `/v1/salt?email=${encodeURIComponent(email)}`,
    );

    return body.salt;
  }

  /**
   * Sends one request and reads what came back.
   *
   * @async
   * @param {string} method - The HTTP method.
   * @param {string} path - The path, including any query.
   * @param {unknown} [body] - What to send, when there is anything.
   * @returns {Promise<T>} The parsed reply.
   * @throws {AccountError} If the server refused, or could not be reached in time.
   */
  private async send<T>(method: string, path: string, body?: unknown): Promise<T> {
    return this.sendWith(this.token, method, path, body);
  }

  /**
   * Sends one request under a named token rather than the current one.
   *
   * Exists for signing out, which has to use a token it has already given up.
   *
   * @async
   * @param {string | null} token - What to authorise with, if anything.
   * @param {string} method - The HTTP method.
   * @param {string} path - The path, including any query.
   * @param {unknown} [body] - What to send, when there is anything.
   * @returns {Promise<T>} The parsed reply.
   * @throws {AccountError} If the server refused, or could not be reached in time.
   */
  private async sendWith<T>(
    token: string | null,
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const headers: Record<string, string> = {};
    if (body !== undefined) {
      headers['content-type'] = 'application/json';
    }
    if (token) {
      headers['authorization'] = `Bearer ${token}`;
    }

    let response: Response;
    try {
      // Built up rather than written out, because `exactOptionalPropertyTypes` draws the
      // distinction between a field that is absent and one that is present and undefined, and
      // a GET carrying an explicitly undefined body is not the same request.
      const init: RequestInit = {
        method,
        headers,
        signal: AbortSignal.timeout(TIMEOUT_MS),
      };
      if (body !== undefined) {
        init.body = JSON.stringify(body);
      }

      response = await fetch(`${this.base}${path}`, init);
    } catch (error) {
      // A name that does not resolve, a machine that is not there, a certificate that is not
      // trusted. None of them is something the caller can tell apart, and all of them mean the
      // same thing to somebody looking at a window.
      throw new AccountError(0, `the account server could not be reached: ${String(error)}`);
    }

    if (response.status === 204) {
      return undefined as T;
    }

    const text = await response.text();
    const parsed: unknown = text === '' ? {} : safeParse(text);

    if (!response.ok) {
      const message =
        typeof parsed === 'object' && parsed !== null && 'error' in parsed
          ? String((parsed as { error: unknown }).error)
          : `the account server answered ${response.status}`;

      // The session is gone rather than merely refused for this call, so holding onto it would
      // have every later request fail the same way with no sign of why.
      if (response.status === 401) {
        this.token = null;
      }

      throw new AccountError(response.status, message);
    }

    return parsed as T;
  }
}

/**
 * Parses JSON without throwing on something that is not JSON.
 *
 * @param {string} text - What the server sent.
 * @returns {unknown} The parsed value, or an empty object.
 */
function safeParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return {};
  }
}

/**
 * Turns the wire's shape into the one this module offers.
 *
 * @param {object} device - The server's representation.
 * @returns {AccountDevice} The same thing, named the way the rest of this code names things.
 */
function toDevice(device: { public_key: string; label: string; added_unix: number }): AccountDevice {
  return {
    publicKey: device.public_key,
    label: device.label,
    addedUnix: device.added_unix,
  };
}
