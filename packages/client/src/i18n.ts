/**
 * What the windows say, in the language of the machine they are on.
 *
 * The English sentence is the key. There is no table of invented names to keep in step with the
 * markup, a screen still reads as the sentence it shows, and a string nobody has translated yet
 * comes out in English rather than as `home.devices.title`.
 *
 * One language beside English so far. A third would want the dictionaries in files of their own;
 * two of them fit here, and splitting them before that is filing an empty drawer.
 */

/**
 * Everything the windows say in Korean.
 *
 * Written rather than translated: an interface says what happens or what to do, and the sentence
 * that does that in Korean is rarely the same sentence with Korean words in it. Where the English
 * explains itself in a clause, the Korean names the subject and says it directly.
 */
const KO: Record<string, string> = {
  /* ── Setup ─────────────────────────────────────────────────────────────── */
  'Your desktop.': '내 컴퓨터를',
  'Everywhere.': '어디서나.',
  Everywhere: '어디서나',
  'Low-latency remote access for macOS, Windows, and Linux.':
    'macOS · Windows · Linux를 지연 없이 원격으로 씁니다.',
  Begin: '시작',
  'Already using PRISM? Sign in': '이미 쓰고 있다면 로그인',
  'One machine, every screen.': '한 대의 컴퓨터, 모든 화면.',
  'PRISM streams your desktop to any other device you own — with latency low enough that you stop noticing it’s remote.':
    'PRISM은 내 컴퓨터 화면을 내가 가진 다른 기기로 보냅니다. 원격이라는 걸 잊을 만큼 빠릅니다.',
  'Scan this with an authenticator app. It is shown once — the server keeps only enough to check codes, which is not enough to show it again.':
    '인증 앱으로 스캔하세요. 이 화면은 한 번만 보여집니다.',
  'I have it — test it': '등록했습니다 — 확인하기',
  'A few permissions first': '먼저 권한이 필요합니다',
  'PRISM needs these to capture and control this machine.':
    '이 컴퓨터의 화면을 보내고 조작하려면 필요합니다.',
  'Nothing is sent outside your own network.': '내 네트워크 밖으로 나가지 않습니다.',
  Granted: '허용됨',
  'Screen Recording': '화면 기록',
  Accessibility: '손쉬운 사용',
  'Local Network': '로컬 네트워크',
  'Capture this display so it can be streamed.': '이 화면을 캡처해서 보냅니다.',
  'Pass keyboard and mouse input to this machine.':
    '다른 기기의 키보드와 마우스 입력을 이 컴퓨터에 전달합니다.',
  'Discover your other devices on this network.': '같은 네트워크에 있는 내 기기를 찾습니다.',
  'Allow these to share this machine': '이 컴퓨터를 공유하려면 허용하세요',
  'Turn each one on in System Settings, then come back.':
    '시스템 설정에서 각각 켠 뒤 돌아오세요.',
  Close: '닫기',
  'Restart PRISM': 'PRISM 다시 시작',
  Allow: '허용',
  'Open System Settings': '시스템 설정 열기',
  'You can change these later in Settings → Privacy.':
    '나중에 설정 → 개인정보 보호에서 바꿀 수 있습니다.',
  'You’re all set.': '준비됐습니다.',
  'Add another machine': '다른 컴퓨터 추가하기',
  'Install PRISM on it and sign in to the same account. The two find each other.':
    '그 컴퓨터에 PRISM을 설치하고 같은 계정으로 로그인하세요. 서로 알아서 찾습니다.',
  'Share the one you want to watch': '보고 싶은 컴퓨터에서 공유 켜기',
  'Press Share on it, and it turns up on the home screen of the other.':
    '그 컴퓨터에서 공유를 누르면 다른 컴퓨터 홈 화면에 나타납니다.',
  'Enter PRISM': 'PRISM 시작',
  Continue: '다음',
  'Enter your code': '코드를 입력하세요',
  'Use a different email': '다른 주소 쓰기',
  'I already have an account': '이미 계정이 있습니다',
  'I need an account': '계정 만들기',
  'Create your account': '계정 만들기',
  'Welcome back': '다시 오셨네요',
  'Check your email': '메일을 확인하세요',
  'One more thing': '하나만 더',
  'Test it once': '한 번 확인해 봅시다',

  /* ── Home ──────────────────────────────────────────────────────────────── */
  Devices: '기기',
  'Add device': '기기 추가',
  'Stop sharing': '공유 중지',
  'Share this machine': '이 컴퓨터 공유',
  'Sharing this machine': '이 컴퓨터를 공유하는 중',
  'Not shared': '공유 안 함',
  'This machine': '이 컴퓨터',
  'Nothing to watch yet': '아직 볼 컴퓨터가 없습니다',
  'Turn on sharing on the machine you want to watch, and it turns up here.':
    '보고 싶은 컴퓨터에서 공유를 켜면 여기에 나타납니다.',
  'Install PRISM on the machine you want to watch, sign in to the same account, and turn on sharing.':
    '보고 싶은 컴퓨터에 PRISM을 설치하고 같은 계정으로 로그인한 뒤 공유를 켜세요.',
  'Other devices': '다른 기기',
  'Recent sessions': '최근 세션',
  'Every session you end is listed here, with what it came to.':
    '끝난 세션이 결과와 함께 여기에 쌓입니다.',
  'Search devices, sessions, files': '기기, 세션, 파일 검색',
  'The host disconnected': '호스트가 연결 해제되었습니다',
  'The connection to the host was lost': '호스트와 연결이 끊어졌습니다',
  'Check that the host is on and connected to the network.':
    '호스트가 켜져 있고 네트워크에 연결되어 있는지 확인하세요.',
  OK: '확인',
  'Sharing terms': '공유 설정',
  'Frame rate, bitrate and where it listens': '프레임 속도, 비트레이트, 대기 주소',
  'Close sharing terms': '공유 설정 닫기',
  'Close settings': '설정 닫기',
  All: '전체',
  Online: '연결됨',
  Pinned: '고정됨',
  Settings: '설정',
  Done: '완료',

  /* ── Settings ──────────────────────────────────────────────────────────── */
  General: '일반',
  Language: '언어',
  'Follow the system': '시스템 설정 따르기',
  Account: '계정',
  'Sign in and your machines find each other. Without an account they still pair, by reading a code off one screen.':
    '로그인하면 내 컴퓨터들이 서로 찾습니다. 계정 없이도 화면의 코드를 읽어 연결할 수 있습니다.',
  'Sign in': '로그인',
  'Sign out': '로그아웃',
  Forget: '해제',
  Email: '주소',
  Password: '비밀번호',
  'Password again': '비밀번호 확인',
  Code: '코드',
  'Emailed code': '메일로 받은 코드',
  'Or type': '또는 직접 입력',
  Updates: '업데이트',
  '{count} machines on this account': '이 계정의 기기 {count}대',
  'build {build} · {channel}': '빌드 {build} · {channel}',
  '{version} is available': '{version} 있음',
  Version: '버전',
  Automatic: '자동 확인',
  Builds: '받을 빌드',
  Released: '정식',
  'Every build': '개발',
  'Built here': '이 컴퓨터에서 만든 것',
  Checking: '확인 중',
  'Up to date': '최신입니다',
  Install: '설치',
  'Check now': '지금 확인',
  Watching: '볼 때',
  'Send input': '키보드·마우스 보내기',
  'Smooth playback': '부드럽게 재생',
  Name: '이름',
  'Frame rate': '프레임 속도',
  Bitrate: '비트레이트',
  'Listen on': '대기 주소',
};

/** The dictionaries, by the language they are. */
const LANGUAGES: Record<string, Record<string, string>> = { ko: KO };

/**
 * The language the windows are in.
 *
 * Held rather than read on every call: it is decided once, and a `t` that consulted the settings
 * every time would be a settings lock taken thousands of times to draw one screen.
 */
let chosen = '';

/**
 * Sets the language, or clears it to follow the machine.
 *
 * @param {string} language - `en`, `ko`, or an empty string to follow the system.
 * @returns {void}
 */
export function speak(language: string): void {
  chosen =
    language ||
    // What the system is set to, which is what the webview reports. Only the part before the
    // region: `ko-KR` and `ko` are the same language and there is one Korean here.
    (navigator.language || 'en').split('-')[0] ||
    'en';
}

/**
 * Returns which language the windows are in.
 *
 * @returns {string} A language tag.
 */
export function speaking(): string {
  return chosen;
}

/**
 * Says something in the language the windows are in.
 *
 * The English is the key, so a string with no translation comes out as itself. That is the right
 * failure: a screen in one language with a sentence in another is legible, and a screen showing
 * a key is not.
 *
 * @param {string} english - What it says in English.
 * @param {Record<string, string | number>} [values] - What to put in `{braces}`, if any.
 * @returns {string} The sentence.
 *
 * @example
 * t('Share this machine'); // '이 컴퓨터 공유'
 * t('Signed in as {email}', { email: 'me@junx.dev' });
 */
export function t(english: string, values?: Record<string, string | number>): string {
  const said = LANGUAGES[chosen]?.[english] ?? english;

  if (!values) {
    return said;
  }

  return said.replace(/\{(\w+)\}/gu, (whole, name: string) =>
    name in values ? String(values[name]) : whole,
  );
}

speak('');
