/**
 * Where this machine looks for everything it cannot find by itself.
 *
 * Both addresses are fixed rather than settings. What a person is deciding when they install
 * this is not which servers to use — it is whether to use it — and a field asking them to
 * name one is a question with no good answer for almost everybody. Anyone who does want to
 * run their own has the server's own documentation and a settings file to point at it.
 */

/**
 * The rendezvous this project runs, which is what a fresh installation uses.
 *
 * A name rather than an address, and one name rather than a list. Every record it resolves to
 * is a region: the machine being shared registers with all of them and the machine connecting
 * asks all of them at once, so whichever answers first is both the nearest and the one the two
 * will relay through if they cannot reach each other directly. Adding a region is therefore a
 * server and a zone file, not a release.
 *
 * Nothing here has to be trusted — the two machines prove who they are to each other, and a
 * hostile server can refuse to introduce them but cannot watch or join. Which is why replacing
 * it with your own is a setting rather than a fork.
 */
export const PRISM_RENDEZVOUS = 'rv.presm.kr:47300';

/**
 * The account server this project runs.
 *
 * What a machine signs in to and how it learns about the others on the same account. It also
 * says where the rendezvous is, which is why a machine that has signed in needs to be told
 * nothing else.
 *
 * TLS is the reverse proxy's, not the server's: what crosses it is the value that signs
 * somebody in, and never anything that opens a private key — that stays sealed under a secret
 * derived from the password and is not sent anywhere.
 */
export const PRISM_ACCOUNT_SERVER = 'https://rv.presm.kr';
