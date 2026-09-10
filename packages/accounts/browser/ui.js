/**
 * What the dashboard's screens are built out of: elements, formatting, the API, and the map.
 *
 * Separated from `app.js` so that the screens read as screens. Nothing here knows what any page
 * looks like; nothing there builds a `<div>` by hand.
 */

/** Where the API lives, which is this page's own origin. */
const API = '';

/** How long a stored session is worth trying before the page asks for a password again. */
const TOKEN_KEY = 'prism.operator';

/** The five colours, matching `packages/design/theme.css`. */
const MINT = '#4de8b0';
const MUTED = '#8a8a99';

/* ── Small helpers ─────────────────────────────────────────────────────────── */

/**
 * Builds an element.
 *
 * @param {string} tag - The tag name, optionally with `.class.names` appended.
 * @param {object} [attributes] - Properties to assign. `text` sets textContent, `html` sets
 *   innerHTML, `on` takes a map of event names to handlers, anything else becomes an attribute.
 * @param {Array<Node|string|null|undefined|false>} [children] - What to append.
 * @returns {HTMLElement} The element.
 */
function el(tag, attributes = {}, children = []) {
  const [name, ...classes] = tag.split('.');
  const node = document.createElement(name || 'div');

  if (classes.length > 0) {
    node.className = classes.join(' ');
  }

  for (const [key, value] of Object.entries(attributes)) {
    if (value === null || value === undefined || value === false) {
      continue;
    }

    if (key === 'text') {
      node.textContent = value;
    } else if (key === 'html') {
      node.innerHTML = value;
    } else if (key === 'on') {
      for (const [event, handler] of Object.entries(value)) {
        node.addEventListener(event, handler);
      }
    } else if (key === 'class') {
      node.className = `${node.className} ${value}`.trim();
    } else {
      node.setAttribute(key, value === true ? '' : String(value));
    }
  }

  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) {
      continue;
    }

    node.append(child);
  }

  return node;
}

/**
 * The icons this page draws, as Lucide's path data.
 *
 * Inlined rather than fetched: the content policy forbids this page loading anything from
 * anywhere, which is the property that makes it safe to open. Lucide is ISC licensed and its
 * icons are a single 24×24 stroke path each, so carrying the eleven that are used costs less
 * than a request would.
 *
 * @see https://lucide.dev
 */
const ICONS = {
  users:
    '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/>' +
    '<path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>',
  monitor:
    '<rect width="20" height="14" x="2" y="3" rx="2"/><path d="M8 21h8"/><path d="M12 17v4"/>',
  history:
    '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/>' +
    '<path d="M12 7v5l4 2"/>',
  globe:
    '<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/>' +
    '<path d="M2 12h20"/>',
  radio:
    '<path d="M4.9 19.1C1 15.2 1 8.8 4.9 4.9"/><path d="M7.8 16.2c-2.3-2.3-2.3-6.1 0-8.5"/>' +
    '<circle cx="12" cy="12" r="2"/><path d="M16.2 7.8c2.3 2.3 2.3 6.1 0 8.5"/>' +
    '<path d="M19.1 4.9C23 8.8 23 15.1 19.1 19"/>',
  refresh:
    '<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/>' +
    '<path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>',
  search: '<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>',
  package:
    '<path d="m7.5 4.27 9 5.15"/><path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/>' +
    '<path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>',
  folder:
    '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  file: '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/>',
  download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M7 10l5 5 5-5"/><path d="M12 15V3"/>',
  trash:
    '<path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/>' +
    '<path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
  chevronDown: '<path d="m6 9 6 6 6-6"/>',
  arrowLeft: '<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>',
  arrowRight: '<path d="M5 12h14"/><path d="m12 5 7 7-7 7"/>',
};

/**
 * The Prism mark, as `crates/prism-tauri/icons/icon.svg` draws it.
 *
 * Copied from the application's own icon rather than approximated: the triangle is not
 * equilateral and the stroke runs through the three colours a prism splits light into, and a
 * hand-drawn stand-in got both wrong. The gradient is given an id of its own because two of
 * these on one page would otherwise share one definition and the second would find nothing.
 *
 * @param {number} [size] - Its height in pixels; the width follows the shape.
 * @returns {HTMLElement} A span holding the SVG.
 */
export function mark(size = 15) {
  const id = `prism-mark-${Math.round(size * 100)}`;

  return el('span.icon', {
    html:
      `<svg height="${size}" viewBox="0.5 0.7 10.6 9.3" fill="none" ` +
      'xmlns="http://www.w3.org/2000/svg" aria-hidden="true">' +
      `<path d="M10.3574 9.25H1.26855L5.53809 1.48535L10.3574 9.25Z" stroke="url(#${id})" ` +
      'stroke-width="1.5" stroke-linejoin="round"/>' +
      `<defs><linearGradient id="${id}" x1="0.4" y1="5.4" x2="11.3" y2="5.4" ` +
      'gradientUnits="userSpaceOnUse">' +
      '<stop stop-color="#35D6FF"/><stop offset="0.5" stop-color="#7C5CFF"/>' +
      '<stop offset="1" stop-color="#FF5CA8"/></linearGradient></defs></svg>',
  });
}

/**
 * One icon, at the size asked for.
 *
 * @param {keyof ICONS} name - Which.
 * @param {number} [size] - Its side, in pixels.
 * @param {string} [colour] - A CSS colour, or `currentColor`.
 * @returns {HTMLElement} A span holding the SVG.
 */
export function icon(name, size = 16, colour = 'currentColor') {
  return el('span.icon', {
    html:
      `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" ` +
      `stroke="${colour}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ` +
      `aria-hidden="true">${ICONS[name]}</svg>`,
  });
}

/**
 * Picks the particle a word takes.
 *
 * Korean chooses between 이/가, 은/는 and 을/를 by whether the word ends in a consonant. Every
 * name on these screens is data — a region an operator named, an address somebody registered —
 * so the sentence around it cannot be written with one of them already in it.
 *
 * @param {string} word - What the particle follows.
 * @param {string} after - The form for a word ending in a consonant.
 * @param {string} otherwise - The form for one ending in a vowel.
 * @returns {string} Whichever fits.
 */
export function particle(word, after, otherwise) {
  const last = word.trim().at(-1) ?? '';
  const code = last.charCodeAt(0);

  // Hangul syllables are laid out so that the final consonant is what is left after dividing
  // out the initial and the vowel. Zero means the syllable ends on its vowel.
  if (code >= 0xac00 && code <= 0xd7a3) {
    return (code - 0xac00) % 28 === 0 ? otherwise : after;
  }

  // Anything else — a Latin name, a number — is read aloud in Korean, and there is no reliable
  // rule for that here. The sentences that would need one are written to avoid it.
  return after;
}

/**
 * Formats a moment as how long ago it was, in Korean.
 *
 * @param {number} unix - Seconds since the epoch.
 * @returns {string} Something like `2시간 전`, or a date once it is older than a week.
 */
/**
 * Words a length of time that has already passed.
 *
 * The same wording as {@link ago}, for a value that arrives as a duration rather than as a
 * moment — a region reports how long it has been running, not when it started.
 *
 * @param {number|null} seconds - How long ago, or null if it never happened.
 * @returns {string} The wording, in Korean.
 *
 * @example
 * since(90); // '1분 전'
 */
function since(seconds) {
  if (seconds === null || seconds === undefined) {
    return '없음';
  }

  return ago(Math.floor(Date.now() / 1000) - Math.max(0, seconds));
}

function ago(unix) {
  if (!unix) {
    return '없음';
  }

  const seconds = Math.max(0, Math.floor(Date.now() / 1000) - unix);

  if (seconds < 60) {
    return '방금';
  }

  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}분 전`;
  }

  if (seconds < 86400) {
    return `${Math.floor(seconds / 3600)}시간 전`;
  }

  if (seconds < 7 * 86400) {
    return `${Math.floor(seconds / 86400)}일 전`;
  }

  const when = new Date(unix * 1000);

  return `${when.getMonth() + 1}월 ${when.getDate()}일`;
}

/**
 * Formats a byte count the way an operator reads one.
 *
 * @param {number} bytes - How many.
 * @returns {string} With a unit, to one decimal place above a kilobyte.
 */
function size(bytes) {
  if (!bytes) {
    return '0 B';
  }

  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const step = Math.min(units.length - 1, Math.floor(Math.log10(bytes) / 3));
  const scaled = bytes / 1000 ** step;

  return `${step === 0 ? scaled : scaled.toFixed(1)} ${units[step]}`;
}

/**
 * An arrow between two machine names.
 *
 * @param {string} from - The host end.
 * @param {string} to - The client end.
 * @returns {HTMLElement} The pair, with the arrow between them.
 */
function between(from, to) {
  return el('span.between', {}, [
    el('span', { text: from }),
    icon('arrowRight', 14, '#4e4e5c'),
    el('span', { text: to }),
  ]);
}

/**
 * A coloured dot beside a word.
 *
 * @param {'good'|'busy'|'bad'|'op'|''} kind - Which colour.
 * @param {string} label - The word.
 * @param {string} [tone] - A class for the label's colour.
 * @returns {HTMLElement} The pair.
 */
function state(kind, label, tone = '') {
  return el('span.state', {}, [
    el('i', { class: `dot ${kind}`.trim() }),
    el('span', { class: tone, text: label }),
  ]);
}

/* ── Deriving the authentication secret ────────────────────────────────────── */

/** The WebAssembly module, once it has been fetched. */
let argon = null;

/**
 * Loads the derivation module, once per page.
 *
 * @async
 * @returns {Promise<WebAssembly.Instance>} Its exports.
 */
async function loadArgon() {
  if (!argon) {
    argon = WebAssembly.instantiateStreaming(fetch('/admin/argon2.wasm')).then(
      (loaded) => loaded.instance,
    );
  }

  return argon;
}

/**
 * Turns a password and the account's salt into the secret the server recognises.
 *
 * Spends sixty-four mebibytes and about a tenth of a second. The wrapping secret is derived
 * alongside it inside the module and never crosses back — this page has no use for a private
 * key and therefore no way to reach one.
 *
 * @async
 * @param {string} password - What was typed.
 * @param {Uint8Array} salt - Sixteen bytes the server handed out.
 * @returns {Promise<string>} The secret, as hex.
 * @throws {Error} If the password is too short or the module refused it.
 */
async function deriveAuth(password, salt) {
  const wasm = await loadArgon();
  const { memory, prism_alloc: alloc, prism_free: free, prism_auth: auth } = wasm.exports;

  const typed = new TextEncoder().encode(password);
  const passwordAt = alloc(typed.length);
  const saltAt = alloc(salt.length);
  const outAt = alloc(32);

  try {
    new Uint8Array(memory.buffer, passwordAt, typed.length).set(typed);
    new Uint8Array(memory.buffer, saltAt, salt.length).set(salt);

    const code = auth(passwordAt, typed.length, saltAt, outAt);

    if (code === -1) {
      throw new Error('비밀번호는 8자 이상이어야 합니다.');
    }

    if (code !== 0) {
      throw new Error('비밀번호를 처리하지 못했습니다.');
    }

    return [...new Uint8Array(memory.buffer, outAt, 32)]
      .map((byte) => byte.toString(16).padStart(2, '0'))
      .join('');
  } finally {
    free(passwordAt, typed.length);
    free(saltAt, salt.length);
    free(outAt, 32);
  }
}

/* ── Talking to the server ─────────────────────────────────────────────────── */

/**
 * The signed-in operator's session.
 *
 * In `sessionStorage` rather than a cookie or `localStorage`: closing the tab is signing out,
 * and a shared machine keeps nothing. A reload does not ask for the password again, which
 * matters when deriving it costs a tenth of a second and sixty-four mebibytes.
 */
export const session = { token: sessionStorage.getItem(TOKEN_KEY) ?? '', email: '', you: null };

/**
 * Remembers a session, or forgets it.
 *
 * @param {string} token - What the server issued, or an empty string to sign out.
 * @param {string} [email] - Whose it is.
 * @returns {void}
 */
export function remember(token, email = '') {
  session.token = token;
  session.email = email;

  if (token) {
    sessionStorage.setItem(TOKEN_KEY, token);
  } else {
    sessionStorage.removeItem(TOKEN_KEY);
    session.you = null;
  }
}

/**
 * Calls the API with the operator's session.
 *
 * @async
 * @param {string} path - The route, from the origin.
 * @param {object} [options] - `method` and `body`, which is sent as JSON.
 * @returns {Promise<object>} What the server answered.
 * @throws {Error} With the server's own message, or a plain one if it sent none.
 */
async function call(path, options = {}) {
  const answer = await fetch(`${API}${path}`, {
    method: options.method ?? 'GET',
    headers: {
      ...(session.token ? { authorization: `Bearer ${session.token}` } : {}),
      ...(options.body ? { 'content-type': 'application/json' } : {}),
    },
    body: options.body ? JSON.stringify(options.body) : undefined,
  });

  const body = await answer.json().catch(() => ({}));

  if (!answer.ok) {
    const error = new Error(body.error ?? '서버가 응답하지 않았습니다.');
    error.status = answer.status;

    throw error;
  }

  return body;
}

/* ── The map ───────────────────────────────────────────────────────────────── */

/**
 * The world's land as runs of dots, one line per row of a 120×45 grid.
 *
 * Generated from the 110m land polygons, rasterised once rather than shipped as coastlines: at
 * this size the map places two servers against each other and is not read for its geography.
 */
const LAND =
  '22:1,25:3,29:1,31:4,36:17,64:2,93:2|21:4,26:1,28:5,40:13,79:2,90:8,106:2|18:2,21:4,26:3,30:3,34:1,42:11,77:2,83:1,87:16,106:3|6:8,15:5,21:5,27:3,31:2,34:4,43:9,66:5,80:1,82:2,85:31,117:2|3:61,71:1,73:2|3:1,6:24,31:1,36:2,43:3,63:3,67:53|4:7,12:16,34:3,45:1,62:4,67:45,114:3|7:2,15:14,34:5,58:1,63:3,67:39,112:2|16:17,34:7,57:1,59:1,63:1,65:41,107:1,112:1|17:24,59:1,61:47|18:20,40:2,58:50|19:21,60:4,65:5,71:5,77:29|19:17,57:4,64:1,66:3,74:2,78:25|19:16,57:3,65:1,67:1,69:7,78:21,100:1|19:16,60:4,72:28,102:1,105:2|21:13,57:7,72:28,103:1|21:9,31:2,57:44|22:6,56:15,72:4,78:22|24:3,55:17,73:6,82:17,100:1|25:3,30:1,34:1,54:18,73:7,83:6,91:5|26:5,34:1,55:18,74:5,84:4,91:4,100:1|29:3,54:19,74:3,85:2,93:3,100:1|31:1,55:19,85:2,94:2|32:2,35:5,56:21|34:8,57:3,62:14,93:1|34:9,63:13,92:1,97:2|33:10,63:11,93:2,96:3|33:14,63:10,94:1,97:2,100:1,104:3,110:1|33:15,64:9,95:1,106:3|34:14,64:9,101:1,107:1,109:1|34:13,65:8,104:2,107:1|35:12,64:10,76:1,102:3,107:1|36:11,64:8,75:1,101:8|37:9,65:7,75:1,99:11|36:8,65:7,75:1,98:13|36:8,65:6,98:13|36:7,66:4,98:13|36:6,66:3,99:2,105:6|36:5,107:3|36:3,118:1|35:4,108:1,117:1|35:3,116:1|35:3|35:2,40:1|36:2';

/** Columns in the grid the runs above describe. */
const MAP_COLUMNS = 120;

/** Rows in it. */
const MAP_ROWS = 45;

/** The northern edge the grid was sampled at. */
const MAP_NORTH = 80;

/** The southern edge. Antarctica is below it and holds no servers. */
const MAP_SOUTH = -56;

/**
 * Draws the land as one SVG of small circles.
 *
 * @returns {string} The markup.
 */
function landSvg() {
  const step = 9;
  let circles = '';

  LAND.split('|').forEach((row, rowIndex) => {
    for (const run of row.split(',').filter(Boolean)) {
      const [start, length] = run.split(':').map(Number);

      for (let i = 0; i < length; i += 1) {
        circles += `<circle cx="${(start + i) * step + 4.5}" cy="${rowIndex * step + 4.5}" r="1.7"/>`;
      }
    }
  });

  return (
    `<svg viewBox="0 0 ${MAP_COLUMNS * step} ${MAP_ROWS * step}" aria-hidden="true">` +
    `<g fill="#32323b">${circles}</g></svg>`
  );
}

/**
 * Where a place falls on the map, as percentages of its box.
 *
 * @param {number} lat - Degrees north.
 * @param {number} lon - Degrees east.
 * @returns {{left: string, top: string}} Percentages, ready for a style.
 */
function place(lat, lon) {
  return {
    left: `${((lon + 180) / 360) * 100}%`,
    top: `${((MAP_NORTH - lat) / (MAP_NORTH - MAP_SOUTH)) * 100}%`,
  };
}

/**
 * Where each region sits, by name.
 *
 * A lookup rather than a column, because a rendezvous server does not know its own latitude and
 * asking an operator to type one to see a dot would be asking for the wrong thing.
 */
const PLACES = {
  오사카: [34.69, 135.5],
  Osaka: [34.69, 135.5],
  암스테르담: [52.37, 4.9],
  Amsterdam: [52.37, 4.9],
  서울: [37.57, 126.98],
  Seoul: [37.57, 126.98],
  도쿄: [35.68, 139.65],
  Tokyo: [35.68, 139.65],
  싱가포르: [1.35, 103.82],
  Singapore: [1.35, 103.82],
  프랑크푸르트: [50.11, 8.68],
  Frankfurt: [50.11, 8.68],
  런던: [51.51, -0.13],
  London: [51.51, -0.13],
  뉴욕: [40.71, -74.01],
  'New York': [40.71, -74.01],
};

export { el, ago, since, size, between, state, deriveAuth, call, landSvg, place, PLACES };

