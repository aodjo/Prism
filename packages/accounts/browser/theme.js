/**
 * Which way round the page is drawn, decided before anything is painted.
 *
 * Its own file loaded from the head rather than a line inside the page, because the content
 * policy this dashboard is served under allows scripts from this origin and nothing inline. It
 * has to run before the first paint either way: a page that opens dark and turns light a moment
 * later has told somebody it was the wrong colour, which is worse than being the wrong colour.
 *
 * Dark is what the product is, so it is what this is when nobody has said otherwise and the
 * system has no opinion either.
 */

/** Where the choice is kept. Per browser, because it is about this screen in this room. */
const KEY = 'prism.theme';

/**
 * Applies a theme and remembers it.
 *
 * @param {string} theme - `dark` or `light`.
 * @returns {void}
 */
function wear(theme) {
  document.documentElement.dataset.theme = theme;

  try {
    localStorage.setItem(KEY, theme);
  } catch {
    // A browser that refuses storage still gets the theme; it just asks again next time.
  }
}

/**
 * The theme now, whether it was chosen or merely inherited from the system.
 *
 * @returns {string} `dark` or `light`.
 */
function worn() {
  return document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
}

let chosen = null;

try {
  chosen = localStorage.getItem(KEY);
} catch {
  // Same as never having chosen.
}

document.documentElement.dataset.theme =
  chosen === 'light' || chosen === 'dark'
    ? chosen
    : window.matchMedia('(prefers-color-scheme: light)').matches
      ? 'light'
      : 'dark';

// Read by `ui.js`, which loads as a module long after this has run. A global rather than an
// export because a module cannot be waited for in the head, and being in the head is the whole
// reason this file exists.
window.prismTheme = { wear, worn };
