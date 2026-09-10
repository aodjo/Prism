/**
 * The operator dashboard's screens.
 *
 * One page, drawn from what the server answers. It signs in with an ordinary Prism account —
 * the same address and password a machine uses — and opens only for accounts the server has
 * marked as operators. There is no shared token: a token is one secret that opens everything
 * for everybody who was ever told it, and it cannot be taken away from one person.
 *
 * The password becomes the authentication secret in this tab, through `/admin/argon2.wasm`.
 * Sending it to the server would let the server derive the wrapping secret too, and with it
 * open every sealed private key it stores.
 */

import {
  ago,
  between,
  call,
  deriveAuth,
  el,
  icon,
  landSvg,
  mark,
  particle,
  place,
  PLACES,
  remember,
  session,
  since,
  size,
  state,
} from './ui.js';

/** Where the whole page is drawn. */
const root = document.getElementById('root');

/**
 * Which way round the page is drawn.
 *
 * Set up by `/admin/theme.js`, which runs in the head so that the first paint is already the
 * right colour. The fallback is for a page opened with that file missing: dark, and a toggle
 * that changes nothing rather than one that throws.
 */
const theme = window.prismTheme ?? { wear: () => {}, worn: () => 'dark' };

/** The pages the rail offers, in the order it offers them. */
const PAGES = [
  { id: 'accounts', label: '계정', glyph: 'users', group: null },
  { id: 'sessions', label: '세션', glyph: 'monitor', group: null },
  { id: 'audit', label: '감사 로그', glyph: 'history', group: null },
  { id: 'regions', label: '리전', glyph: 'globe', group: '서버' },
  { id: 'relay', label: '릴레이', glyph: 'radio', group: '서버' },
  { id: 'local', label: '로컬 빌드', glyph: 'laptop', group: '빌드' },
  { id: 'builds', label: '개발 빌드', glyph: 'package', group: '빌드' },
  { id: 'releases', label: '정식 빌드', glyph: 'tag', group: '빌드' },
];

/** Which page is showing, and which account is open on the accounts page. */
const view = { page: 'accounts', account: null, search: '' };

/** The last overview the server gave, so the strip does not blank on every navigation. */
let overview = null;

/* ── Sign in ───────────────────────────────────────────────────────────────── */

/**
 * Draws the sign-in screen and runs it.
 *
 * Three states live here rather than in three functions: what changes between them is one
 * message and whether the button is spinning, and splitting that produces three copies of a
 * form that must stay identical.
 *
 * @returns {void}
 */
function drawSignIn() {
  const address = el('input.box', { type: 'email', autocomplete: 'username', value: '' });
  const password = el('input.box', { type: 'password', autocomplete: 'current-password' });
  const digits = [...Array(6)].map(() =>
    el('input', { inputmode: 'numeric', maxlength: '1', autocomplete: 'one-time-code' }),
  );

  // Typing moves along; a paste of six digits fills the row. Both strip anything that is not a
  // digit, because a code arriving through a password manager brings spaces with it.
  digits.forEach((box, index) => {
    box.addEventListener('input', (event) => {
      const cleaned = event.target.value.replace(/\D/gu, '').slice(0, 1);
      event.target.value = cleaned;

      if (cleaned && index < digits.length - 1) {
        digits[index + 1].focus();
      }
    });

    box.addEventListener('keydown', (event) => {
      if (event.key === 'Backspace' && !event.target.value && index > 0) {
        digits[index - 1].focus();
      }
    });

    box.addEventListener('paste', (event) => {
      event.preventDefault();
      const pasted = (event.clipboardData?.getData('text') ?? '').replace(/\D/gu, '');

      digits.forEach((each, at) => {
        each.value = pasted[at] ?? '';
      });

      digits[Math.min(pasted.length, digits.length - 1)].focus();
    });
  });

  const trouble = el('p.trouble', { hidden: true }, [el('i.dot.bad'), el('span')]);
  const progress = el('i', { style: 'width:0%' });
  const waiting = el('div.progress', { hidden: true }, [progress]);
  const button = el('button.solid', { type: 'submit', text: '대시보드 열기' });

  /**
   * Says what went wrong, in one message for all three causes.
   *
   * Which of the address, the password and the code was wrong is not said, because saying it
   * tells somebody guessing that the other two were right.
   *
   * @param {string} message - What to show, or an empty string to clear it.
   * @returns {void}
   */
  const say = (message) => {
    trouble.hidden = !message;
    trouble.lastChild.textContent = message;
  };

  const form = el('form.gate-card', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        say('');

        const email = address.value.trim().toLowerCase();
        const code = digits.map((box) => box.value).join('');

        if (!email || !password.value || code.length < 6) {
          say('주소, 비밀번호, 인증 앱의 6자리 코드를 모두 입력하세요.');

          return;
        }

        button.disabled = true;
        button.textContent = '비밀번호 확인 중';
        waiting.hidden = false;

        // The bar is honest about being an estimate: the derivation cannot report progress, so
        // this counts up to the time it usually takes and stops there until the answer lands.
        const started = Date.now();
        const ticking = setInterval(() => {
          progress.style.width = `${Math.min(92, ((Date.now() - started) / 400) * 92)}%`;
        }, 40);

        try {
          const { salt } = await call(`/v1/salt?email=${encodeURIComponent(email)}`);
          const bytes = Uint8Array.from(salt.match(/../gu).map((pair) => parseInt(pair, 16)));
          const auth = await deriveAuth(password.value, bytes);
          const opened = await call('/v1/sessions', {
            method: 'POST',
            body: { email, auth, code },
          });

          progress.style.width = '100%';
          remember(opened.token, email);
          session.you = opened;

          if (!opened.operator) {
            drawNotOperator(email);

            return;
          }

          await start();
        } catch (error) {
          say(error.message);
        } finally {
          clearInterval(ticking);
          button.disabled = false;
          button.textContent = '대시보드 열기';
          waiting.hidden = true;
          progress.style.width = '0%';
        }
      },
    },
  }, [
    el('div.wordmark', { style: 'padding:0' }, [mark(15), el('b', { text: 'PRISM' })]),
    el('h1.heading', { text: '이 서버를 관리하려면 로그인하세요.', style: 'margin:26px 0 28px' }),
    el('div.field', {}, [el('div.field-cap', {}, [el('span', { text: '주소' })]), address]),
    el('div.field', {}, [
      el('div.field-cap', {}, [
        el('span', { text: '비밀번호' }),
        el('a', { href: '#', text: '비밀번호 찾기' }),
      ]),
      password,
    ]),
    el('div.field', { style: 'padding-bottom:24px' }, [
      // Named after where it comes from, not what it looks like. Asked twice what this was,
      // which is twice more than a label that worked would have been.
      el('div.field-cap', {}, [el('span', { text: '인증 앱 6자리 코드' })]),
      el('div.digits', {}, digits),
    ]),
    trouble,
    button,
    waiting,
  ]);

  root.replaceChildren(el('div.gate', {}, [form]));
  address.focus();
}

/**
 * Draws what an account without the operator right sees.
 *
 * Not an error: the sign-in worked. It says what is true and offers the two ways out.
 *
 * @param {string} email - Who signed in.
 * @returns {void}
 */
function drawNotOperator(email) {
  root.replaceChildren(
    el('div.gate', {}, [
      el('div.gate-card.wide', {}, [
        el('div.wordmark', { style: 'padding:0' }, [
          mark(15),
          el('b', { text: 'PRISM' }),
        ]),
        el('h1.heading', { text: '이 계정은 접근할 수 없습니다.', style: 'margin:26px 0 10px' }),
        el('p.note.muted', { style: 'margin:0 0 22px' }, [
          el('span', {
            text:
              `${email} 계정은 정상입니다. 서버 관리는 별개의 권한이고, 이 계정에는 없습니다.`,
          }),
        ]),
        el('div', { style: 'display:flex;gap:10px' }, [
          el('button.solid', {
            style: 'width:auto;padding:12px 17px',
            text: '다른 계정으로 로그인',
            on: {
              click: async () => {
                await call('/v1/session', { method: 'DELETE' }).catch(() => {});
                remember('');
                drawSignIn();
              },
            },
          }),
          el('button.pill', { style: 'padding:12px 17px', text: 'Prism 열기', on: { click: () => { location.href = '/'; } } }),
        ]),
      ]),
    ]),
  );
}

/* ── Shell ─────────────────────────────────────────────────────────────────── */

/**
 * The navigation rail.
 *
 * @returns {HTMLElement} The rail.
 */
function drawRail() {
  const items = [];
  let group = null;

  for (const page of PAGES) {
    if (page.group !== group) {
      group = page.group;

      if (group) {
        items.push(el('div.group', { text: group }));
      }
    }

    items.push(
      el('button.nav', {
        type: 'button',
        'aria-current': view.page === page.id ? 'page' : null,
        on: {
          click: () => {
            view.page = page.id;
            view.account = null;
            drawPage();
          },
        },
      }, [icon(page.glyph), el('span', { text: page.label })]),
    );
  }

  return el('nav.rail', {}, [
    el('div.wordmark', {}, [
      mark(15),
      el('b', { text: 'PRISM' }),
      overview && el('span.build', { text: overview.version }),
    ]),
    ...items,
  ]);
}

/**
 * One reading in the status strip.
 *
 * @param {string} label - What it is.
 * @param {Array<Node|string|false|null>} parts - The reading itself.
 * @returns {HTMLElement} The item.
 */
function reading(label, parts) {
  return el('div.live-item', {}, [el('span.label', { text: label }), ...parts]);
}

/**
 * The one card the strip opens, moved and refilled rather than built per reading.
 *
 * Only ever one is open, and a card per item would be four subtrees to keep in step with
 * numbers that change on every refresh.
 *
 * @type {HTMLElement|null}
 */
let strip = null;

/**
 * Opens a reading up when somebody looks at it.
 *
 * @param {HTMLElement} item - The reading in the strip.
 * @param {() => Array<Node|string|false|null>} build - What the card says.
 * @returns {HTMLElement} The same reading, so this can wrap one in place.
 */
function explains(item, build) {
  // Reachable by keyboard, because a number nobody can read without a mouse is a number half
  // the operators cannot read.
  item.tabIndex = 0;
  item.classList.add('asks');

  /**
   * Fills the card and puts it under the reading.
   *
   * @returns {void}
   */
  const show = () => {
    if (!strip?.parentElement) {
      return;
    }

    strip.replaceChildren(...build());
    strip.hidden = false;

    // Placed against the window rather than against the strip: the strip is narrower than the
    // card, so clamping to it would push a card opened on the last reading off the screen.
    const bar = strip.parentElement.getBoundingClientRect();
    const box = item.getBoundingClientRect();
    const rightmost = window.innerWidth - 16 - strip.offsetWidth;

    strip.style.left = `${Math.max(16, Math.min(rightmost, box.left)) - bar.left}px`;
    strip.style.top = `${box.bottom - bar.top + 10}px`;
  };

  /**
   * Closes it.
   *
   * @returns {void}
   */
  const hide = () => {
    if (strip) {
      strip.hidden = true;
    }
  };

  item.addEventListener('mouseenter', show);
  item.addEventListener('focus', show);
  item.addEventListener('mouseleave', hide);
  item.addEventListener('blur', hide);

  return item;
}

/**
 * A count, grouped so that five figures can be read at a glance.
 *
 * @param {number} value - How many.
 * @param {string} unit - The counter that follows it.
 * @returns {string} The number and its counter.
 */
function counted(value, unit) {
  return `${(value ?? 0).toLocaleString('ko-KR')}${unit}`;
}

/**
 * The head of a detail card: what it is, and one line saying so.
 *
 * @param {string} title - What the reading is.
 * @param {string} blurb - The sentence under it.
 * @returns {HTMLElement} The head.
 */
function about(title, blurb) {
  return el('div.detail-head', {}, [el('b', { text: title }), el('span', { text: blurb })]);
}

/**
 * A row with a bar under it, for a number that runs against a limit.
 *
 * @param {string} label - What is being used.
 * @param {string} value - How much of it, written out.
 * @param {number} share - How full, from 0 to 1.
 * @param {boolean} [tight] - Whether it is close enough to the limit to say so in red.
 * @returns {HTMLElement} The row.
 */
function meter(label, value, share, tight = false) {
  return el('div.meter', {}, [
    el('span.meter-label', { text: label }),
    el('span.meter-value', { class: tight ? 'is-bad' : '', text: value }),
    el('div.gauge.wide', {}, [
      el('i', {
        style: `width:${Math.min(100, Math.max(0, share * 100))}%${tight ? ';background:#ff5c6e' : ''}`,
      }),
    ]),
  ]);
}

/**
 * The strip along the top, and the operator's own chip.
 *
 * Every number in it is measured when it is asked for, so a stale reading is not possible —
 * only a slow one.
 *
 * @returns {HTMLElement} The bar.
 */
function drawStatusBar() {
  const database = overview?.database ?? {};
  const regions = overview?.regions ?? [];
  const up = regions.filter((region) => region.up).length;
  const carrying = regions.reduce((total, region) => total + (region.now_mbps ?? 0), 0);
  const capacity = regions.reduce((total, region) => total + (region.link_mbps ?? 0), 0);
  const healthy = up === regions.length;

  const peak = regions.reduce((total, region) => total + (region.peak_mbps ?? 0), 0);

  strip = el('div.hover-card.detail', { hidden: true });

  return el('div.statusbar', {}, [
    el('div.live', {}, [
      strip,
      explains(
        reading('', [
          el('i', { class: `dot ${healthy ? 'good' : 'busy'}` }),
          el('span.ink-3', { text: healthy ? '정상' : `리전 ${regions.length - up}곳 응답 없음` }),
        ]),
        () => [
          about('전체 상태', '리전과 데이터베이스를 한 번에 본 것입니다.'),
          el('dl', {}, [
            el('dt', { text: '리전' }),
            el('dd', {
              class: healthy ? '' : 'is-bad',
              text: healthy ? `${up}곳 모두 정상` : `${regions.length - up}곳 응답 없음`,
            }),
            el('dt', { text: 'D1 응답' }),
            el('dd', { text: `${database.latency_ms ?? '—'}ms` }),
            el('dt', { text: '릴레이' }),
            el('dd', { text: `${carrying} Mbps` }),
            el('dt', { text: '빌드' }),
            el('dd', { text: overview?.version ?? '—' }),
          ]),
        ],
      ),
      el('i.live-divider'),
      explains(
        reading('리전', [
          el('span.mono', { class: healthy ? '' : 'is-bad', text: `${up}/${regions.length}` }),
        ]),
        () => [
          about('리전', '각 리전이 마지막으로 보고한 때입니다.'),
          el(
            'dl',
            {},
            regions.flatMap((region) => [
              el('dt', { text: region.name }),
              el('dd', {
                class: region.up ? '' : 'is-bad',
                text: region.reports ? since(region.heard_seconds) : '보고 없음',
              }),
            ]),
          ),
        ],
      ),
      el('i.live-divider'),
      explains(
        reading('D1', [el('span.mono.ink-2', { text: `${database.latency_ms ?? '—'}ms` })]),
        () => [
          about('D1 데이터베이스', '계정과 기기, 열린 세션이 들어 있습니다.'),
          el('dl', {}, [
            el('dt', { text: '쿼리 응답 시간' }),
            el('dd', { text: `${database.latency_ms ?? '—'}ms` }),
            el('dt', { text: '계정' }),
            el('dd', { text: counted(database.accounts, '개') }),
            el('dt', { text: '메일 미인증' }),
            el('dd', { text: counted(database.unverified, '개') }),
            el('dt', { text: '기기' }),
            el('dd', { text: counted(database.devices, '대') }),
            el('dt', { text: '열린 세션' }),
            el('dd', { text: counted(database.sessions, '개') }),
          ]),
        ],
      ),
      el('i.live-divider'),
      explains(
        reading('릴레이', [
          el('div.gauge', {}, [
            el('i', {
              style: `width:${capacity ? Math.min(100, (carrying / capacity) * 100) : 0}%`,
            }),
          ]),
          el('span.mono.ink-2', { text: `${carrying} Mbps` }),
          capacity > 0 &&
            el('span.fine.muted', { text: `회선의 ${((carrying / capacity) * 100).toFixed(1)}%` }),
        ]),
        () => [
          about('릴레이', '리전들이 지금 나르고 있는 트래픽입니다.'),
          el('dl', {}, [
            el('dt', { text: '지금' }),
            el('dd', { text: `${carrying} Mbps` }),
            el('dt', { text: '회선 합계' }),
            el('dd', { text: capacity ? `${capacity} Mbps` : '—' }),
            el('dt', { text: '최고' }),
            el('dd', { text: `${peak} Mbps` }),
          ]),
          // Only the metered ones. A machine on an unmetered plan has nothing to run out of,
          // and a bar drawn against no limit would invent one.
          regions.some((region) => region.limit_gb) &&
            el('div.detail-part', {}, [
              el('span.detail-heading', { text: '트래픽 허용량' }),
              ...regions
                .filter((region) => region.limit_gb)
                .map((region) => {
                  const used = region.carried_bytes ?? 0;
                  const allowed = region.limit_gb * 1e9;
                  const share = Math.min(1, used / allowed);

                  return meter(
                    region.name,
                    `${(used / 1e9).toFixed(0)} / ${region.limit_gb} GB`,
                    share,
                    share > 0.8,
                  );
                }),
            ]),
        ],
      ),
      el('button.refresh', { type: 'button', title: '새로고침', on: { click: () => refresh() } }, [
        icon('refresh', 15),
      ]),
    ]),
    el('div.statusbar-right', {}, [
      // Beside the operator's own chip rather than in the strip of readings to its left,
      // because that strip is what this server is doing and this is about the screen it is
      // being read on.
      el('button.chrome-button', {
        type: 'button',
        title: theme.worn() === 'light' ? '어둡게' : '밝게',
        'aria-label': theme.worn() === 'light' ? '어둡게' : '밝게',
        on: {
          click: () => {
            theme.wear(theme.worn() === 'light' ? 'dark' : 'light');
            void drawPage();
          },
        },
      }, [icon(theme.worn() === 'light' ? 'moon' : 'sun', 15)]),
      el('button.operator', {
        type: 'button',
        title: '로그아웃',
        on: {
          click: async () => {
            await call('/v1/session', { method: 'DELETE' }).catch(() => {});
            remember('');
            drawSignIn();
          },
        },
      }, [
        el('span.avatar', { text: (session.email[0] ?? '?').toLowerCase() }),
        el('span', { text: session.email }),
        icon('chevronDown', 13),
      ]),
    ]),
  ]);
}

/**
 * A page header: a title, and whatever belongs on the right of it.
 *
 * @param {string} title - The page's name.
 * @param {Array<Node|false|null>} [right] - Controls.
 * @param {Node} [above] - A row above the title, for the way back.
 * @returns {HTMLElement} The header.
 */
function header(title, right = [], above = null) {
  return el('header.header', {}, [
    el('div.who', {}, [above, el('h1.title', { text: title })]),
    ...right,
  ]);
}

/**
 * A table, from column names and already-built rows.
 *
 * @param {Array<{label: string, width?: string, right?: boolean}>} columns - The head.
 * @param {HTMLElement[]} rows - `<tr>` elements.
 * @returns {HTMLElement} The table.
 */
function table(columns, rows) {
  return el('table.table', {}, [
    el('thead', {}, [
      el('tr', {}, columns.map((column) =>
        el('th', {
          class: column.right ? 'right' : '',
          style: column.width ? `width:${column.width}` : null,
          text: column.label,
        }),
      )),
    ]),
    el('tbody', {}, rows),
  ]);
}

/**
 * What a page shows when the region servers cannot yet report what it is about.
 *
 * Said plainly rather than drawn as an empty table, because an empty table reads as "nothing is
 * happening" and the truth is "nobody asked the machine that knows".
 *
 * @param {string} what - What is missing.
 * @returns {HTMLElement} The notice.
 */
function notReported(what) {
  return el('div.nothing', {}, [
    el('p.row-text.ink-3', { style: 'margin:0', text: `${what}을 보고하는 리전이 없습니다.` }),
    el('p.note.dim', { style: 'margin:0', text: '리전 서버가 이 값을 아직 내보내지 않습니다.' }),
  ]);
}

/* ── Accounts ──────────────────────────────────────────────────────────────── */

/**
 * The accounts page.
 *
 * @async
 * @param {HTMLElement} column - Where to draw.
 * @returns {Promise<void>}
 */
async function drawAccounts(column) {
  const { accounts, you } = await call('/v1/admin/accounts');
  const term = view.search.trim().toLowerCase();
  const shown = term ? accounts.filter((account) => account.email.toLowerCase().includes(term)) : accounts;

  const search = el('input', {
    type: 'search',
    placeholder: '주소 검색',
    value: view.search,
    on: {
      input: (event) => {
        view.search = event.target.value;
        drawPage();
      },
    },
  });

  const rows = shown.map((account) => {
    const yours = account.email === you;

    return el('tr.clickable', {
      on: {
        click: (event) => {
          if (event.target.closest('button')) {
            return;
          }

          view.account = account.email;
          drawPage();
        },
      },
    }, [
      el('td', {}, [
        el('span.row-text.ink-2', { text: account.email }),
        !account.verified && el('span.tag', { style: 'margin-left:10px', text: '미인증' }),
      ]),
      el('td', {}, [el('span.mono', { class: account.devices ? 'ink-3' : 'dim', text: String(account.devices) })]),
      el('td', {}, [
        account.operator
          ? state('op', yours ? '운영자 — 나' : '운영자', 'ink-3')
          : el('span.note.dim', { text: '스트리밍만' }),
      ]),
      el('td', {}, [
        el('span.note', {
          class: 'muted',
          text: account.last_seen ? ago(account.last_seen) : '로그인한 적 없음',
        }),
      ]),
      el('td.right', {}, [
        el('span.actions', {}, [
          el('button.pill', {
            type: 'button',
            text: account.operator ? '운영자 해제' : '운영자 지정',
            on: { click: () => setOperator(account.email, !account.operator) },
          }),
          !yours &&
            el('button.pill.bare.danger', {
              type: 'button',
              text: '삭제',
              on: { click: () => askDelete(account) },
            }),
        ]),
      ]),
    ]);
  });

  column.append(
    header('계정', [
      el('button.pill.danger', { type: 'button', text: '전부 로그아웃', on: { click: askSignOutEverybody } }),
    ]),
    el('div.toolbar', {}, [
      el('div.search', {}, [icon('search', 15), search]),
      el('span.note.muted', { text: '최신순' }),
    ]),
    el('div.page-body', { style: 'padding-top:22px' }, [
      table(
        [
          { label: '주소' },
          { label: '기기', width: '120px' },
          { label: '역할', width: '150px' },
          { label: '마지막 접속', width: '170px' },
          { label: '', width: '250px', right: true },
        ],
        rows,
      ),
      shown.length === 0 &&
        el('div.nothing', {}, [
          el('p.row-text.ink-3', { style: 'margin:0', text: `“${view.search}”으로 찾은 계정이 없습니다.` }),
          el('p.note.dim', { style: 'margin:0', text: '주소의 일부만 입력해도 됩니다. 대소문자는 구분하지 않습니다.' }),
          el('button.pill', {
            type: 'button',
            style: 'margin-top:8px',
            text: '검색 지우기',
            on: {
              click: () => {
                view.search = '';
                drawPage();
              },
            },
          }),
        ]),
    ]),
  );
}

/**
 * One account, its machines and what this server introduced for it.
 *
 * @async
 * @param {HTMLElement} column - Where to draw.
 * @returns {Promise<void>}
 */
async function drawAccount(column) {
  const email = view.account;
  const { account, devices, activity } = await call(
    `/v1/admin/accounts/${encodeURIComponent(email)}`,
  );

  const back = el('button.back', {
    type: 'button',
    on: {
      click: () => {
        view.account = null;
        drawPage();
      },
    },
  }, [icon('arrowLeft', 14), el('span', { text: '계정' })]);

  column.append(
    header(email, [
      el('button.pill', {
        type: 'button',
        text: account.operator ? '운영자 해제' : '운영자 지정',
        on: { click: () => setOperator(email, !account.operator) },
      }),
      account.email !== session.email &&
        el('button.pill.danger', { type: 'button', text: '계정 삭제', on: { click: () => askDelete(account) } }),
    ], back),
    el('div.page-body', {}, [
      el('div.card.facts', {}, [
        el('div.fact', {}, [
          el('span.label', { text: '가입' }),
          el('span.row-text.ink-2', { text: new Date(account.created_unix * 1000).toLocaleDateString('ko-KR') }),
          el('span.fine.dim', { text: ago(account.created_unix) }),
        ]),
        el('div.fact', {}, [
          el('span.label', { text: '기기' }),
          el('span.row-text.ink-2', { text: `${devices.length}대` }),
          el('span.fine.dim', { text: account.verified ? '주소 인증됨' : '주소 미인증' }),
        ]),
        el('div.fact', {}, [
          el('span.label', { text: '역할' }),
          el('span.row-text.ink-2', { text: account.operator ? '운영자' : '스트리밍만' }),
          el('span.fine.dim', { text: account.operator ? '모든 계정을 읽고 지울 수 있음' : '' }),
        ]),
      ]),
      el('div.section', {}, [el('h2', { text: '기기' })]),
      table(
        [
          { label: '이름' },
          { label: '추가', width: '210px' },
          { label: '키', width: '210px' },
          { label: '', width: '110px', right: true },
        ],
        devices.map((device) =>
          el('tr', {}, [
            el('td', {}, [el('span.row-text.ink-2', { text: device.label })]),
            el('td', {}, [el('span.note.muted', { text: new Date(device.added_unix * 1000).toLocaleDateString('ko-KR') })]),
            el('td', {}, [el('span.mono.dim', { text: `${device.public_key.slice(0, 4)}…${device.public_key.slice(-4)}` })]),
            el('td.right', {}, [
              el('button.pill.bare.danger', {
                type: 'button',
                text: '제거',
                on: { click: () => removeDevice(email, device.public_key) },
              }),
            ]),
          ]),
        ),
      ),
      devices.length === 0 && el('div.nothing', {}, [el('p.note.dim', { style: 'margin:0', text: '등록된 기기가 없습니다.' })]),
      el('div.section', {}, [el('h2', { text: '최근 중개' })]),
      activity?.reports
        ? table(
            [
              { label: '구간' },
              { label: '경로', width: '220px' },
              { label: '리전', width: '170px' },
              { label: '중개 시각', width: '200px' },
            ],
            (activity.introduced ?? []).map((entry) =>
              el('tr', {}, [
                el('td', {}, [between(entry.from, entry.to)]),
                el('td', {}, [
                  entry.relayed ? state('busy', '릴레이로 전환', 'ink-3') : state('', '릴레이 요청 없음', 'muted'),
                ]),
                el('td', {}, [el('span.note.muted', { text: entry.region })]),
                el('td', {}, [el('span.note.muted', { text: ago(entry.at_unix) })]),
              ]),
            ),
          )
        : notReported('중개 기록'),
    ]),
  );
}

/* ── Sessions ──────────────────────────────────────────────────────────────── */

/**
 * What the server is carrying, and what it only introduced.
 *
 * These are two tables because they are two different kinds of knowledge. A relayed session
 * passes through this server, so its liveness and its bytes are counted. An introduction ends
 * when the address is handed over.
 *
 * @async
 * @param {HTMLElement} column - Where to draw.
 * @returns {Promise<void>}
 */
async function drawSessions(column) {
  const activity = await call('/v1/admin/activity');

  column.append(
    header('세션'),
    el('div.page-body', { style: 'padding-top:20px' }, [
      el('div.section', { style: 'padding-top:6px' }, [el('h2', { text: '운반 중' })]),
      activity.reports
        ? table(
            [
              { label: '계정' },
              { label: '구간', width: '250px' },
              { label: '리전', width: '150px' },
              { label: '경과', width: '140px' },
              { label: '운반량', width: '160px' },
              { label: '', width: '92px', right: true },
            ],
            (activity.carrying ?? []).map((entry) =>
              el('tr', {}, [
                el('td', {}, [el('span.row-text.ink-2', { text: entry.email })]),
                el('td', {}, [between(entry.from, entry.to)]),
                el('td', {}, [el('span.note.muted', { text: entry.region })]),
                el('td', {}, [state('busy', ago(entry.since_unix).replace(' 전', '째'), 'ink-2')]),
                el('td', {}, [el('span.mono.ink-3', { text: size(entry.bytes) })]),
                el('td.right', {}, [
                  el('button.pill.bare.quiet', {
                    type: 'button',
                    text: '종료',
                    on: { click: () => endSession(entry.token) },
                  }),
                ]),
              ]),
            ),
          )
        : notReported('운반 중인 세션'),
      activity.reports &&
        (activity.carrying ?? []).length === 0 &&
        el('div.nothing', {}, [el('p.note.dim', { style: 'margin:0', text: '지금 운반 중인 세션이 없습니다.' })]),
      el('div.section', {}, [el('h2', { text: '최근 중개' })]),
      activity.reports
        ? table(
            [
              { label: '계정' },
              { label: '구간', width: '250px' },
              { label: '리전', width: '150px' },
              { label: '중개 시각', width: '300px' },
              { label: '이후', width: '240px' },
            ],
            (activity.introduced ?? []).map((entry) =>
              el('tr', {}, [
                el('td', {}, [el('span.row-text.ink-2', { text: entry.email })]),
                el('td', {}, [between(entry.from, entry.to)]),
                el('td', {}, [el('span.note.muted', { text: entry.region })]),
                el('td', {}, [el('span.note.muted', { text: ago(entry.at_unix) })]),
                el('td', {}, [
                  entry.relayed ? state('busy', '릴레이로 전환', 'ink-3') : state('', '릴레이 요청 없음', 'muted'),
                ]),
              ]),
            ),
          )
        : notReported('중개 기록'),
    ]),
  );
}

/* ── Regions ───────────────────────────────────────────────────────────────── */

/**
 * The regions page: a map, and a row per region with its traffic allowance.
 *
 * @async
 * @param {HTMLElement} column - Where to draw.
 * @returns {Promise<void>}
 */
async function drawRegions(column) {
  const { regions } = await call('/v1/admin/regions');
  const down = regions.filter((region) => !region.up);

  const map = el('div.map', { html: landSvg() });
  const card = el('div.hover-card', { hidden: true });
  map.append(card);

  for (const region of regions) {
    const at = PLACES[region.name];

    if (!at) {
      continue;
    }

    const spot = place(at[0], at[1]);
    const marker = el('button', {
      class: `marker ${region.up ? '' : 'bad'}`.trim(),
      type: 'button',
      style: `left:${spot.left};top:${spot.top}`,
      title: region.name,
    });

    const name = el('span.marker-name', {
      style: `left:calc(${spot.left} + 20px);top:${spot.top}`,
      text: region.name,
    });

    /**
     * Fills the hovering card with everything this region knows.
     *
     * @returns {void}
     */
    const show = () => {
      card.replaceChildren(
        el('div', { style: 'display:flex;align-items:center;gap:10px' }, [
          el('i', { class: `dot ${region.up ? 'good' : 'bad'}` }),
          el('span.row-text', { text: region.name }),
          region.build && el('span.build', { text: region.build }),
        ]),
        el('dl', {}, [
          el('dt', { text: '주소' }),
          el('dd', { text: region.url }),
          el('dt', { text: '마지막 보고' }),
          el('dd', {
            class: region.up ? '' : 'is-bad',
            text: region.reports ? since(region.heard_seconds) : '보고 없음',
          }),
          el('dt', { text: '대기 중인 호스트' }),
          el('dd', { text: region.reports ? String(region.hosts) : '—' }),
          el('dt', { text: '운반 중' }),
          el('dd', { text: region.reports ? `세션 ${region.carrying}개, ${region.now_mbps} Mbps` : '—' }),
          el('dt', { text: '운반한 트래픽' }),
          el('dd', {
            text: region.limit_gb
              ? `${(region.carried_bytes / 1e9).toFixed(0)} / ${region.limit_gb} GB`
              : '무제한',
          }),
        ]),
      );

      const box = map.getBoundingClientRect();
      const left = parseFloat(spot.left) / 100 * box.width;
      const top = parseFloat(spot.top) / 100 * box.height;

      card.style.left = `${Math.max(8, Math.min(box.width - 276, left + 30))}px`;
      card.style.top = `${Math.max(8, Math.min(box.height - 200, top - 90))}px`;
      card.hidden = false;
    };

    marker.addEventListener('mouseenter', show);
    marker.addEventListener('focus', show);
    marker.addEventListener('mouseleave', () => {
      card.hidden = true;
    });
    marker.addEventListener('blur', () => {
      card.hidden = true;
    });

    map.append(marker, name);
  }

  column.append(
    header('리전', [
      el('button.pill', { type: 'button', text: '리전 추가', on: { click: () => askRegion(null) } }),
    ]),
    el('div.page-body', { style: 'padding-top:22px' }, [
      down.length > 0 &&
        el('div.alarm', {}, [
          el('i.dot.bad'),
          el('div', {}, [
            el('p.row-text.ink-2', {
              style: 'margin:0',
              text:
                `${down.map((region) => region.name).join(', ')}` +
                `${particle(down[down.length - 1].name, '이', '가')} 응답하지 않습니다.`,
            }),
            el('p.fine.muted', {
              style: 'margin:4px 0 0',
              text: '그 리전으로 중개되던 기기는 다른 리전으로 넘어갑니다. 이미 이어진 세션은 끊기지 않습니다.',
            }),
          ]),
        ]),
      el('div.map-card', {}, [map]),
      el('div.chips', {}, regions.map((region) => drawRegionChip(region))),
      regions.length === 0 &&
        el('div.nothing', {}, [
          el('p.row-text.ink-3', { style: 'margin:0', text: '등록된 리전이 없습니다.' }),
          el('p.note.dim', { style: 'margin:0', text: '리전을 추가하면 이 페이지가 그 서버에 상태를 물어봅니다.' }),
        ]),
    ]),
  );
}

/**
 * One region's line under the map, carrying its traffic allowance.
 *
 * The allowance is the one number on this page an operator sets rather than reads: some of
 * these machines are on plans that meter traffic and some are not, and only the person paying
 * the bill knows which.
 *
 * @param {object} region - As the server described it.
 * @returns {HTMLElement} The chip.
 */
function drawRegionChip(region) {
  const used = region.carried_bytes ?? 0;
  const allowed = region.limit_gb ? region.limit_gb * 1e9 : 0;
  const share = allowed ? Math.min(1, used / allowed) : 0;
  const tight = allowed > 0 && share > 0.8;

  return el('div.chip', {}, [
    el('i', { class: `dot ${region.up ? 'good' : 'bad'}` }),
    el('span.row-text.ink-2', { text: region.name }),
    el('span.mono.dim', { text: region.url }),
    el('span.push'),
    allowed > 0
      ? el('span', { style: 'display:flex;align-items:center;gap:10px' }, [
          el('span.mono', {
            class: tight ? 'is-bad' : 'ink-3',
            text: `${(used / 1e9).toFixed(0)} / ${region.limit_gb} GB`,
          }),
          el('span.share', {}, [
            el('i', { style: `width:${share * 100}%;background:${tight ? '#ff5c6e' : '#4de8b0'}` }),
          ]),
        ])
      : el('span.note.muted', { text: '트래픽 무제한' }),
    el('span.note', {
      class: region.up ? 'ink-3' : 'is-bad',
      text: region.reports ? since(region.heard_seconds) : '보고 없음',
    }),
    el('button.pill.bare.quiet', { type: 'button', text: '설정', on: { click: () => askRegion(region) } }),
  ]);
}

/* ── Relay ─────────────────────────────────────────────────────────────────── */

/**
 * How much of each region's link is in use, and how much has gone through.
 *
 * @async
 * @param {HTMLElement} column - Where to draw.
 * @returns {Promise<void>}
 */
async function drawRelay(column) {
  const relay = await call('/v1/admin/relay');
  const peak = Math.max(1, ...(relay.days ?? []).map((day) => day.bytes));

  column.append(
    header('릴레이'),
    el('div.page-body', { style: 'padding-top:22px' }, [
      ...(relay.regions ?? []).map((region) =>
        el('div.card', { style: 'margin-bottom:14px' }, [
          el('div', { style: 'display:flex;align-items:center;justify-content:space-between;gap:16px' }, [
            el('div', { style: 'display:flex;align-items:baseline;gap:14px' }, [
              el('span.row-text.ink-2', { text: region.name }),
              el('span.figure', { text: `${region.now_mbps} Mbps` }),
              el('span.note.muted', { text: `/ ${(region.link_mbps / 1000).toFixed(0)} Gbps` }),
            ]),
            el('span.tag', {
              text: `회선의 ${((region.now_mbps / region.link_mbps) * 100).toFixed(1)}%, ` +
                (region.carrying ? `세션 ${region.carrying}개 운반 중` : '지금은 운반 없음'),
            }),
          ]),
          el('div.track', { style: 'margin-top:16px' }, [
            el('i', { class: 'peak', style: `width:${(region.peak_mbps / region.link_mbps) * 100}%` }),
            el('i', { class: 'now', style: `width:${(region.now_mbps / region.link_mbps) * 100}%` }),
          ]),
          el('div.legend', { style: 'margin-top:16px' }, [
            el('span', {}, [
              el('i.swatch', { style: 'background:#4de8b0' }),
              el('span', { text: `현재 ${region.now_mbps} Mbps` }),
            ]),
            el('span', {}, [
              el('i.swatch', { style: 'background:rgba(255,255,255,.08)' }),
              el('span', { text: `최대 ${region.peak_mbps} Mbps` }),
            ]),
          ]),
        ]),
      ),
      (relay.days ?? []).length > 0 &&
        el('div.card', { style: 'margin-top:14px' }, [
          el('div', { style: 'display:flex;align-items:center;justify-content:space-between' }, [
            el('div', { style: 'display:flex;align-items:baseline;gap:10px' }, [
              el('span.figure', { text: size(relay.days.reduce((total, day) => total + day.bytes, 0)) }),
              el('span.note.muted', { text: '최근 30일 운반량' }),
            ]),
          ]),
          el('div.chart', { style: 'margin-top:22px' },
            relay.days.map((day) =>
              el('i', {
                class: day.bytes === peak ? 'top' : '',
                style: `height:${Math.max(3, (day.bytes / peak) * 118)}px`,
                title: `${day.date} ${size(day.bytes)}`,
              }),
            ),
          ),
          el('div.axis', { style: 'margin-top:22px' }, [
            el('span', { text: relay.days[0]?.date ?? '' }),
            el('span', { text: '오늘' }),
          ]),
        ]),
      (relay.regions ?? []).length === 0 && notReported('릴레이 사용량'),
    ]),
  );
}

/* ── Builds ────────────────────────────────────────────────────────────────── */

/**
 * The three lines, and what each is.
 *
 * Written out rather than branched on at four call sites, because what differs between them is
 * four sentences and a branch per sentence is where the third one gets forgotten.
 */
const LINES = {
  local: {
    name: '로컬 빌드',
    about:
      '이 저장소에서 직접 만들어 올린 빌드입니다. CI를 거치지 않으므로 만든 사람의 기계에서만 확인된 것이고, 로컬 채널로 옮긴 기계만 받습니다.',
    empty: '저장소에서 pnpm publish-local 을 실행하면 여기에 나타납니다.',
  },
  development: {
    name: '개발 빌드',
    about: 'develop에 올라간 것을 CI가 만든 빌드입니다. 개발 채널을 따르는 기계가 맨 위의 것을 받습니다.',
    empty: 'develop에 푸시하면 CI가 만들어 여기에 올립니다.',
  },
  production: {
    name: '정식 빌드',
    about: '태그를 붙여 낸 릴리즈입니다. 기본 채널을 따르는 모든 기계가 이 중 맨 위의 것을 받습니다.',
    empty: 'release 브랜치를 main에 병합하고 v 태그를 붙이면 여기에 나타납니다.',
  },
};

/** Which version's files are open, kept across redraws so a refresh does not close it. */
let openVersion = null;


/**
 * What a target and architecture are called where somebody reads them.
 *
 * The updater asks in the names Rust uses for a target. Those are the right thing on the wire
 * and the wrong thing in a list a person is scanning.
 *
 * @param {string} target - `darwin`, `windows` or `linux`.
 * @param {string} arch - `aarch64` or `x86_64`.
 * @returns {string} What to show.
 */
function platformName(target, arch) {
  const system = { darwin: 'macOS', windows: 'Windows', linux: 'Linux' }[target] ?? target;
  const chip = { aarch64: 'Apple Silicon', x86_64: 'Intel' }[arch] ?? arch;

  return target === 'darwin' ? `${system} · ${chip}` : `${system} · ${arch}`;
}

/**
 * The development line, as the folders and files it is stored as.
 *
 * A version is a folder because that is what it is in the bucket: one version holds one file per
 * platform, published one at a time by whoever had that machine in front of them. Which is also
 * why a version with one file in it is the normal case rather than a half-finished one.
 *
 * @async
 * @param {HTMLElement} column - Where to draw.
 * @returns {Promise<void>}
 */
async function drawBuilds(column) {
  // Which of the two the rail is on. One function draws both, because they are one list read
  // against two lines, and splitting it would be two copies of the same explorer kept in step
  // by hand.
  const line = { releases: 'production', local: 'local' }[view.page] ?? 'development';
  const { builds } = await call(`/v1/admin/builds?channel=${line}`);

  // Grouped in the order the rows arrived, which is newest first: the table is ordered by when
  // each was published and a version's folder belongs where its newest file puts it.
  const versions = new Map();

  for (const build of builds ?? []) {
    const found = versions.get(build.version) ?? { version: build.version, files: [], bytes: 0 };

    found.files.push(build);
    found.bytes += build.bytes;
    versions.set(build.version, found);
  }

  const folders = [...versions.values()];

  if (openVersion === null || !versions.has(openVersion)) {
    openVersion = folders[0]?.version ?? null;
  }

  const files = el('div.explorer-files');

  /**
   * Draws the files inside whichever version is open.
   *
   * @returns {void}
   */
  const showFiles = () => {
    const folder = versions.get(openVersion);

    if (!folder) {
      files.replaceChildren(
        el('div.nothing', {}, [
          el('p.row-text.muted', { style: 'margin:0', text: `${LINES[line].name}가 없습니다.` }),
          el('p.note.dim', { style: 'margin:8px 0 0', text: LINES[line].empty }),
        ]),
      );

      return;
    }

    files.replaceChildren(
      // Column names, because the two figures on the right are a size and an age and neither
      // says which it is. One row of them at the top costs less than a unit on every line.
      el('div.explorer-head', {}, [
        el('span'),
        el('span.label', { text: '파일' }),
        el('span.label', { text: '출처' }),
        el('span.label', { text: '크기' }),
        el('span.label', { text: '올린 때' }),
        el('span'),
      ]),
      ...folder.files.map((build) =>
        el('div.explorer-file', {}, [
          el('span.explorer-glyph', {}, [icon('file', 15)]),
          el('div.explorer-what', {}, [
            // The name the file actually has, not one assembled from the columns beside it. A
            // name built out of a version and a platform is a guess that happens to be right,
            // and the moment the bundler renames something it is a guess that is wrong with
            // nothing saying so.
            el('span.explorer-file-name', {
              text: build.filename || `${build.version}-${build.target}-${build.arch}`,
            }),
            el('span.note.muted', { text: build.notes || platformName(build.target, build.arch) }),
          ]),
          el('span.explorer-source', {
            class: build.source === 'github' ? 'is-github' : '',
            text: build.source === 'github' ? 'GitHub' : '직접 올림',
          }),
          el('span.mono.ink-3.explorer-figure', { text: size(build.bytes) }),
          el('span.note.muted.explorer-figure', {
            text: since(Math.max(0, Math.floor(Date.now() / 1000) - build.uploaded_unix)),
          }),
          el('span.explorer-actions', {}, [
            el('a.icon-button', {
              href:
                build.source === 'github'
                  ? build.url
                  : `/v1/builds/${build.version}/${build.target}/${build.arch}`,
              title: '내려받기',
              download: '',
            }, [icon('download', 15)]),
            // Only what this server is keeping. A release belongs to the repository that cut it
            // and a button here that appeared to delete one would be lying about what it does.
            build.source !== 'github' &&
              el('button.icon-button', {
                type: 'button',
                title: '내리기',
                on: {
                  click: () => {
                    void withdraw(build);
                  },
                },
              }, [icon('trash', 15)]),
          ]),
        ]),
      ),
    );
  };

  showFiles();

  column.append(
    header(
      LINES[line].name,
      [
        el('span.note.muted', {
          text: folders.length
            ? `버전 ${folders.length}개 · 파일 ${(builds ?? []).length}개`
            : '',
        }),
      ],
    ),
    el('div.page-body', { style: 'padding-top:22px' }, [
      // What this line is for, said once. Somebody who lands here having only ever cut releases
      // would otherwise be looking at an empty folder with no idea what fills it.
      el('p.note.muted', { style: 'margin:0 0 18px', text: LINES[line].about }),
      el('div.explorer', {}, [
        el('div.explorer-tree', {}, [
          el('div.explorer-tree-head', {}, [el('span.label', { text: '버전' })]),
          ...(folders.length
            ? folders.map((folder, at) =>
                el('button', {
                  class: `explorer-folder${folder.version === openVersion ? ' is-open' : ''}`,
                  type: 'button',
                  on: {
                    click: () => {
                      openVersion = folder.version;

                      for (const each of column.querySelectorAll('.explorer-folder')) {
                        each.classList.toggle('is-open', each.dataset.version === openVersion);
                      }

                      showFiles();
                    },
                  },
                  'data-version': folder.version,
                }, [
                  el('span.explorer-glyph.small', {}, [icon('folder', 14)]),
                  el('span.explorer-folder-what', {}, [
                    el('span.explorer-name', { text: folder.version }),
                    el('span.fine.dim', {
                      text: `파일 ${folder.files.length}개 · ${size(folder.bytes)}`,
                    }),
                  ]),
                  // Only the newest, and only once. Which build is going out is the question
                  // this page exists to answer, and answering it on every row answers nothing.
                  at === 0 && el('span.tag-now', { text: '배포 중' }),
                ]),
              )
            : [el('p.note.dim', { style: 'margin:12px 14px', text: '비어 있습니다.' })]),
        ]),
        files,
      ]),
    ]),
  );
}

/**
 * Takes one build off the line, once somebody has said so twice.
 *
 * @async
 * @param {object} build - The row being withdrawn.
 * @returns {Promise<void>}
 */
async function withdraw(build) {
  const yes = await confirmed(
    build.filename || `${build.version} · ${platformName(build.target, build.arch)}`,
    '이 빌드를 내립니다. 이미 받은 기계는 그대로 두고, 앞으로 제안되지 않습니다.',
    '내리기',
  );

  if (!yes) {
    return;
  }

  await call(`/v1/admin/builds/${build.version}/${build.target}/${build.arch}`, {
    method: 'DELETE',
  });

  await drawPage();
}

/* ── Audit ─────────────────────────────────────────────────────────────────── */

/** How an action token reads, and how loud it is. */
const ACTIONS = {
  'operator.grant': { kind: 'op', say: (entry) => `${entry.subject}를 운영자로 지정` },
  'operator.revoke': { kind: 'op', say: (entry) => `${entry.subject} 운영자 해제` },
  'account.delete': { kind: 'bad', say: (entry) => `${entry.subject} 삭제` },
  'device.remove': { kind: 'bad', say: (entry) => `${entry.subject} 기기 제거` },
  'sessions.clear': { kind: 'bad', say: () => '모든 기기 로그아웃' },
  'session.open': { kind: '', say: () => '로그인' },
  'session.refuse': { kind: '', say: (entry) => `${entry.subject} 로그인 거부` },
  'region.add': { kind: 'op', say: (entry) => `${entry.subject} 리전 추가` },
  'region.change': { kind: 'op', say: (entry) => `${entry.subject} 리전 설정 변경` },
  'region.remove': { kind: 'bad', say: (entry) => `${entry.subject} 리전 제거` },
};

/**
 * Everything an operator did here, and when.
 *
 * @async
 * @param {HTMLElement} column - Where to draw.
 * @returns {Promise<void>}
 */
async function drawAudit(column) {
  const { entries } = await call('/v1/admin/audit');

  column.append(
    header('감사 로그'),
    el('div.page-body', { style: 'padding-top:24px' }, [
      el('div', { style: 'display:flex;justify-content:flex-end;padding-bottom:20px' }, [
        el('span.note.dim', { text: '90일 보관' }),
      ]),
      ...entries.map((entry) => {
        const known = ACTIONS[entry.action] ?? { kind: '', say: () => entry.action };

        return el('div.entry', {}, [
          el('div.kind', {}, [el('i', { class: `dot ${known.kind}`.trim() })]),
          el('div.words', {}, [
            el('span.row-text.ink-2', { text: known.say(entry) }),
            entry.detail && el('span.fine.dim', { text: entry.detail }),
          ]),
          el('div.stamp', {}, [
            el('span.note.muted', { text: ago(entry.at_unix) }),
            el('span.fine.dim', { text: entry.actor || '서버' }),
          ]),
        ]);
      }),
      entries.length === 0 &&
        el('div.nothing', {}, [el('p.note.dim', { style: 'margin:0', text: '아직 기록이 없습니다.' })]),
    ]),
  );
}

/* ── Things that change something ──────────────────────────────────────────── */

/**
 * Opens a modal and resolves when it closes.
 *
 * @param {(close: () => void) => HTMLElement} build - Given a way to close, returns the card.
 * @param {() => void} [closed] - Run whichever way it closes, including the veil and Escape.
 *   Somebody who dismisses a sheet has answered it, and a caller waiting on that answer would
 *   otherwise wait forever.
 * @returns {void}
 */
function modal(build, closed = () => {}) {
  const veil = el('div.veil');
  const close = () => {
    veil.remove();
    closed();
  };

  veil.addEventListener('click', (event) => {
    if (event.target === veil) {
      close();
    }
  });

  document.addEventListener(
    'keydown',
    (event) => {
      if (event.key === 'Escape') {
        close();
      }
    },
    { once: true },
  );

  veil.append(build(close));
  document.body.append(veil);
}

/**
 * Asks once, and answers whether it was agreed to.
 *
 * For a change that is worth stopping over and not worth typing a name out for. Removing an
 * account is the other kind and has its own sheet, where the address has to be typed: what makes
 * that one different is that nothing brings it back.
 *
 * @async
 * @param {string} title - What is about to happen to.
 * @param {string} detail - What happens, in one line.
 * @param {string} verb - What the button says.
 * @returns {Promise<boolean>} Whether to go ahead.
 */
function confirmed(title, detail, verb) {
  return new Promise((settle) => {
    let answer = false;

    modal(
      (close) => {
        /**
         * Records what was chosen and closes, which is what settles this.
         *
         * @param {boolean} yes - What was chosen.
         * @returns {void}
         */
        const done = (yes) => {
          answer = yes;
          close();
        };

        return el('div.modal', {}, [
          el('h2.heading', { text: title }),
          el('p.note.muted', { style: 'margin:14px 0 0', text: detail }),
          el('div.modal-actions', { style: 'padding-top:24px' }, [
            el('button.pill', { type: 'button', text: '취소', on: { click: () => done(false) } }),
            el('button.pill.danger', {
              type: 'button',
              text: verb,
              on: { click: () => done(true) },
            }),
          ]),
        ]);
      },
      // Whichever way it closed. Dismissing it with the veil or with Escape leaves `answer`
      // where it started, which is no.
      () => settle(answer),
    );
  });
}

/**
 * Asks before deleting an account, and makes the address be typed.
 *
 * Typed rather than clicked, because everything the account holds goes with it and there is no
 * copy anywhere this server could ask for it back from.
 *
 * @param {object} account - The one to delete.
 * @returns {void}
 */
function askDelete(account) {
  modal((close) => {
    const typed = el('input.box.mono', { autocomplete: 'off', spellcheck: 'false' });
    const confirm = el('button.pill.danger', { type: 'button', text: '삭제', disabled: true });

    typed.addEventListener('input', () => {
      confirm.disabled = typed.value.trim().toLowerCase() !== account.email;
    });

    confirm.addEventListener('click', async () => {
      confirm.disabled = true;

      try {
        await call(`/v1/admin/accounts/${encodeURIComponent(account.email)}`, { method: 'DELETE' });
        close();
        view.account = null;
        await refresh();
      } catch (error) {
        confirm.disabled = false;
        alert(error.message);
      }
    });

    return el('div.modal', {}, [
      el('h2.heading', { text: `${account.email} 계정을 삭제할까요?` }),
      el('p.note.muted', { style: 'margin:0', text: '이 실행은 되돌릴 수 없습니다.' }),
      el('div.losses', {}, [
        el('div', {}, [el('i.bullet'), el('span.fine.muted', { text: '아무도 갖고 있지 않은, 봉인된 개인키' })]),
        el('div', {}, [el('i.bullet'), el('span.fine.muted', { text: '2단계 인증과, 그것을 설정한 기기' })]),
        el('div', {}, [
          el('i.bullet'),
          el('span.fine.muted', { text: `등록된 기기 ${account.devices}대` }),
        ]),
      ]),
      el('div.field', { style: 'padding-top:22px' }, [
        el('div.field-cap', {}, [el('span', { text: '삭제를 켜려면 주소를 입력하세요' })]),
        typed,
      ]),
      el('div.modal-actions', {}, [
        el('button.pill', { type: 'button', text: '그대로 두기', on: { click: close } }),
        confirm,
      ]),
    ]);
  });
}

/**
 * Asks before ending every session on the server.
 *
 * @returns {void}
 */
function askSignOutEverybody() {
  modal((close) =>
    el('div.modal', {}, [
      el('h2.heading', { text: '모든 기기를 로그아웃할까요?' }),
      el('p.note.muted', { style: 'margin:0' }, [
        el('span', { text: '계정은 아무도 잃지 않습니다. 모든 기기가 다시 로그인해야 합니다.' }),
      ]),
      el('div.alarm', { style: 'margin:22px 0 0;background:rgba(255,176,92,.08);border-color:rgba(255,176,92,.24)' }, [
        el('i.dot.busy'),
        el('span.fine.muted', { text: '이 브라우저도 로그아웃됩니다.' }),
      ]),
      el('div.modal-actions', {}, [
        el('button.pill', { type: 'button', text: '그대로 두기', on: { click: close } }),
        el('button.pill.danger', {
          type: 'button',
          text: '전부 로그아웃',
          on: {
            click: async () => {
              await call('/v1/admin/sessions', { method: 'DELETE' });
              remember('');
              close();
              drawSignIn();
            },
          },
        }),
      ]),
    ]),
  );
}

/**
 * Adds a region, or changes the one given.
 *
 * @param {object|null} region - The one to change, or null to add.
 * @returns {void}
 */
function askRegion(region) {
  modal((close) => {
    const name = el('input.box', {
      value: region?.name ?? '',
      readonly: Boolean(region),
      placeholder: '오사카',
    });
    const url = el('input.box.mono', {
      value: region?.url ?? '',
      placeholder: '129.225.129.149:47300',
    });

    const metered = el('input', {
      type: 'checkbox',
      checked: Boolean(region?.limit_gb),
      'aria-label': '트래픽 상한이 있는 요금제',
    });
    const limit = el('input', { type: 'number', min: '1', value: region?.limit_gb ?? 500 });

    // Hidden rather than greyed out when the plan does not meter: a number nobody may edit is
    // still a number somebody reads, and this one would be a limit that does not exist.
    const allowance = el('div.field', { hidden: !region?.limit_gb }, [
      el('div.amount', {}, [limit, el('span', { text: 'GB / 월' })]),
    ]);

    metered.addEventListener('change', () => {
      allowance.hidden = !metered.checked;

      if (metered.checked) {
        limit.focus();
        limit.select();
      }
    });

    const save = el('button.pill.primary', { type: 'button', text: '저장' });

    save.addEventListener('click', async () => {
      save.disabled = true;

      try {
        const body = {
          url: url.value.trim(),
          limit_gb: metered.checked ? Number(limit.value) : null,
        };

        await (region
          ? call(`/v1/admin/regions/${encodeURIComponent(region.name)}`, { method: 'PATCH', body })
          : call('/v1/admin/regions', { method: 'POST', body: { name: name.value.trim(), ...body } }));

        close();
        await refresh();
      } catch (error) {
        save.disabled = false;
        alert(error.message);
      }
    });

    return el('div.modal', {}, [
      el('h2.heading', { text: region ? `${region.name} 설정` : '리전 추가' }),
      el('div.field', { style: 'padding-top:24px' }, [
        el('div.field-cap', {}, [el('span', { text: '이름' })]),
        name,
      ]),
      el('div.field', {}, [el('div.field-cap', {}, [el('span', { text: '주소' })]), url]),
      el('label.control-row', {}, [
        el('span', { text: '트래픽 상한이 있는 요금제' }),
        el('span.switch', {}, [metered, el('i')]),
      ]),
      allowance,
      el('div.modal-actions', {}, [
        region &&
          el('button.pill.bare.danger.apart', {
            type: 'button',
            text: '리전 제거',
            on: {
              click: async () => {
                await call(`/v1/admin/regions/${encodeURIComponent(region.name)}`, {
                  method: 'DELETE',
                });
                close();
                await refresh();
              },
            },
          }),
        el('button.pill', { type: 'button', text: '취소', on: { click: close } }),
        save,
      ]),
    ]);
  });
}

/**
 * Grants or withdraws the right to look after this server.
 *
 * @async
 * @param {string} email - Whose.
 * @param {boolean} wanted - Whether they should hold it.
 * @returns {Promise<void>}
 */
async function setOperator(email, wanted) {
  try {
    await call(`/v1/admin/accounts/${encodeURIComponent(email)}`, {
      method: 'PATCH',
      body: { operator: wanted },
    });
    await refresh();
  } catch (error) {
    alert(error.message);
  }
}

/**
 * Takes a machine off an account.
 *
 * @async
 * @param {string} email - Whose account.
 * @param {string} key - The machine's public key.
 * @returns {Promise<void>}
 */
async function removeDevice(email, key) {
  try {
    await call(`/v1/admin/accounts/${encodeURIComponent(email)}/devices/${key}`, {
      method: 'DELETE',
    });
    await refresh();
  } catch (error) {
    alert(error.message);
  }
}

/**
 * Stops one relayed session.
 *
 * @async
 * @param {string} token - Which relay the region gave it.
 * @returns {Promise<void>}
 */
async function endSession(token) {
  try {
    await call(`/v1/admin/activity/${encodeURIComponent(token)}`, { method: 'DELETE' });
    await refresh();
  } catch (error) {
    alert(error.message);
  }
}

/* ── Drawing ───────────────────────────────────────────────────────────────── */

/**
 * Draws the shell and whichever page is showing.
 *
 * @async
 * @returns {Promise<void>}
 */
async function drawPage() {
  const column = el('div.column', {}, [drawStatusBar()]);
  root.replaceChildren(el('div.shell', {}, [drawRail(), column]));

  const draw = {
    accounts: view.account ? drawAccount : drawAccounts,
    sessions: drawSessions,
    regions: drawRegions,
    relay: drawRelay,
    local: drawBuilds,
    builds: drawBuilds,
    releases: drawBuilds,
    audit: drawAudit,
  }[view.page];

  try {
    await draw(column);
  } catch (error) {
    if (error.status === 401 || error.status === 403) {
      remember('');
      drawSignIn();

      return;
    }

    column.append(
      el('div.page-body', {}, [
        el('div.nothing', {}, [
          el('p.row-text.is-bad', { style: 'margin:0', text: error.message }),
          el('button.pill', { type: 'button', style: 'margin-top:8px', text: '다시 시도', on: { click: refresh } }),
        ]),
      ]),
    );
  }
}

/**
 * Asks the server for the strip's numbers and redraws.
 *
 * @async
 * @returns {Promise<void>}
 */
async function refresh() {
  overview = await call('/v1/admin/overview').catch(() => overview);
  await drawPage();
}

/**
 * Opens the dashboard for a session that is already good.
 *
 * @async
 * @returns {Promise<void>}
 */
async function start() {
  await refresh();
}

/**
 * Decides which screen the page opens on.
 *
 * @async
 * @returns {Promise<void>}
 */
async function boot() {
  if (!session.token) {
    drawSignIn();

    return;
  }

  try {
    const resumed = await call('/v1/session');
    session.email = resumed.email;
    session.you = resumed;

    if (!resumed.operator) {
      drawNotOperator(resumed.email);

      return;
    }

    await start();
  } catch {
    remember('');
    drawSignIn();
  }
}

boot();
