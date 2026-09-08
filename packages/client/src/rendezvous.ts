/**
 * How this machine finds the other one.
 *
 * Shared by the window that offers the choice and the process that acts on it, because the
 * address the session uses and the address the interface shows have to be the same one.
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

/** Which of the three answers a stored rendezvous address amounts to. */
export type Finding = 'automatic' | 'custom' | 'off';

/**
 * Works out which answer an address is.
 *
 * Read from the address rather than stored beside it, because two fields that mean one thing
 * are two fields that can disagree — and the address is the one the session actually uses.
 *
 * @param {string} address - The rendezvous setting as it is stored.
 * @returns {Finding} Which of the three the address amounts to.
 *
 * @example
 * findingOf('');                  // 'off'
 * findingOf('rv.example.com:47300'); // 'custom'
 */
export function findingOf(address: string): Finding {
  if (address === PRISM_RENDEZVOUS) {
    return 'automatic';
  }

  return address.trim() === '' ? 'off' : 'custom';
}
