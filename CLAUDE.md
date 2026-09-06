# Prism

저지연 원격 데스크톱. 게임 플레이가 가능한 수준의 지연(LAN p99 < 25ms @ 1440p120)이 목표.

전체 설계는 `docs/plan.md` 참조.

## 절대 규칙

> **비디오 프레임은 절대 V8(JavaScript)을 통과하지 않는다.**

napi 표면은 제어 호출과 통계만 노출한다. 통계는 최대 10Hz. 프레임 버퍼·텍스처·패킷을
JS로 넘기는 코드는 리뷰에서 무조건 반려한다.

- **제어 평면 (TypeScript/Electron):** 페어링, 시그널링, 설정, 정책 결정, 통계 표시
- **데이터 평면 (Rust):** 캡처 → 인코딩 → 송신 → 수신 → 디코딩 → 표시 → 입력

## 코드 주석 컨벤션

### TypeScript / JavaScript

**모든 함수와 메서드에 JSDoc을 영문으로 작성한다.** 형식은 아래를 따른다.

```js
/**
 * Short description of the function or method.
 *
 * Longer explanation of what the function does, how it works,
 * and any important edge cases or side effects.
 *
 * @async (Optional)
 * @param {Type} paramName - Description of the parameter.
 * @param {Type} [optionalParamName=defaultValue] - Description of an optional parameter with default.
 * @returns {ReturnType} Description of the return value.
 * @throws {ErrorType} Description of errors that can be thrown.
 *
 * @example
 * const result = myFunction('example');
 * console.log(result); // Expected output
 */
```

상수에도 작성한다. 주석은 **항상 선언 위**에 둔다.

```js
/** Maximum UDP payload size in bytes, kept under the safe PMTU floor. */
export const MAX_PACKET_SIZE = 1200;
```

**자잘한 주석은 작성하지 않는다.** 코드가 이미 말하고 있는 것을 반복하는 라인 주석은 금지.

### Rust

동일한 원칙을 rustdoc으로 적용한다. 모든 공개 항목에 영문 `///` 문서 주석을 작성하고,
`# Errors` / `# Panics` / `# Safety` / `# Examples` 섹션을 해당될 때 포함한다.
`unsafe` 블록에는 안전성 불변식을 설명하는 `// SAFETY:` 주석을 반드시 단다.
그 외 자잘한 라인 주석은 작성하지 않는다.

## 레이아웃

```
crates/prism-core/   데이터 평면 전체 (캡처·인코드·네트워크·디코드·표시·입력)
crates/prism-napi/   napi-rs 표면 → .node (제어 + 통계 전용)
crates/prism-cli/    헤드리스 host/client (M1~M4 검증 및 CI 회귀)
crates/amf-shim/     AMD AMF C 심
packages/protocol/   와이어 포맷 — Rust와 TS가 공유하는 단일 진실 소스
packages/host/       Electron 트레이 UI
packages/client/     Electron UI 셸 (스트림 창은 Rust/SDL3)
crates/prism-rendezvous/  자체 호스팅 서버: 페어링·시그널링·주소 발견·릴레이 폴백
```

## 와이어 포맷

`packages/protocol/vectors.json`이 유일한 진실 소스다. Rust(`cargo test`)와
TS(`vitest`) 양쪽이 같은 벡터로 테스트하며, 포맷을 바꾸면 벡터를 먼저 고친다.

## 푸시 전 검사

`#[cfg(target_os = ...)]` 뒤에 있는 코드는 macOS에서만 빌드해서는 절대 검증되지 않는다.
한쪽에서만 쓰이는 상수·import는 다른 OS에서 dead code가 되고, CI의 `-D warnings`에 걸린다.
플랫폼별 코드를 건드렸다면 푸시 전에 반드시 실행한다:

```sh
pnpm lint         # macOS clippy + rustfmt
pnpm lint:cross   # linux, windows 타깃 clippy (링커 없이 clippy만 수행하므로 로컬에서 동작)
```

최초 1회 `rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-msvc` 필요.

## 핫패스 금지 사항

- 핫패스에 `tokio` 사용 금지 (제어 평면·랑데부 서버에만 허용)
- 프레임 경로에 힙 할당 금지 — 버퍼는 풀에서 재사용
- 캡처된 GPU 텍스처를 CPU로 내리지 않는다 (제로카피 필수)


## README

`README.md`는 **저장소 소유자가 직접 작성한다.** 에이전트는 생성하지도, 수정하지도 않는다.
프로젝트 설명이 필요하면 `docs/` 아래에 쓴다.

## 브랜치 전략 (git flow)

| 브랜치 | 역할 | 분기 출발 | 병합 대상 |
|---|---|---|---|
| `main` | 릴리스만. 모든 커밋에 `v*` 태그 | — | — |
| `develop` | 통합 브랜치. 평소 작업의 기준점 | `main` | — |
| `feature/*` | 기능 개발 | `develop` | `develop` |
| `bugfix/*` | `develop`의 버그 수정 | `develop` | `develop` |
| `release/*` | 릴리스 준비 (버전 범프, 안정화) | `develop` | `main` + `develop` |
| `hotfix/*` | 배포본 긴급 수정 | `main` | `main` + `develop` |

- **`main`과 `develop`에 직접 푸시하지 않는다.** PR로만 병합한다.
- 마일스톤 단위 작업은 `feature/m1-vertical-slice` 처럼 마일스톤 번호를 붙인다.
- 태그는 `v0.1.0` 형식. `release/*`를 `main`에 병합할 때만 붙인다.
- CI는 `main`·`develop` 푸시와 두 브랜치를 향한 PR에서 돈다.
- GitHub 기본 브랜치는 `main`이다. PR은 반드시 `--base develop`을 명시해서 만든다.

`git flow` CLI 없이 순수 git으로도 동일하게 운용 가능하다. CLI를 쓰려면
`brew install git-flow-avh` 후 `git flow init -d` (설정은 이미 `.git/config`에 있음).

## 커밋 메시지 컨벤션

Conventional Commits를 따른다. **영문 한 줄**로만 작성한다.

```
feat: add NVENC slice-level bitstream streaming
fix: reject video packets with reserved flag bits set
docs: document the feedback packet layout
```

타입: `feat` `fix` `docs` `refactor` `perf` `test` `build` `ci` `chore`

- 본문·푸터를 쓰지 않는다. 제목 한 줄로 끝낸다.
- `Claude-Session:` 등 도구 관련 트레일러를 절대 넣지 않는다.
- 명령형 현재시제를 쓴다 (`add`, `fix` — `added`, `fixes` 아님).
