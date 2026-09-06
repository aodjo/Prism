# Prism — 저지연 원격 데스크톱 (게임 스트리밍급)

## Context

`/Users/aodjo/Documents/prism`은 현재 완전히 빈 디렉토리다. 목표는 **게임 플레이가 가능한 수준의 저지연**과 **간단한 설치**를 동시에 만족하는 원격 데스크톱 "Prism"을 처음부터 만드는 것.

확정된 요구사항:

| 항목 | 결정 |
|---|---|
| 호스트 OS | Windows + macOS + Linux (전부) |
| 클라이언트 | 네이티브 전용 |
| 네트워크 | 처음부터 인터넷 경유 (NAT 트래버설 포함) |
| UI 셸 | TypeScript / Electron |
| 네이티브 코어 | **Rust** → `napi-rs`로 Node 애드온화 |
| 코덱 계층 | 네이티브 SDK 직접 호출 (NVENC/AMF/oneVPL/VideoToolbox/VAAPI) |
| 1차 목표 스펙 | 1440p120, HEVC·AV1까지 |

보유 장비: 이 Mac(Apple Silicon, macOS 26.6) + NVIDIA GPU Windows PC + AMD/Intel 내장 GPU Windows PC + Linux 박스. 이 Mac에는 Node 24 · Python 3.14 · ffmpeg 9가 있고 **Rust 툴체인은 아직 없다** (M0에서 `rustup` 설치).

**성공 기준(측정 가능한 정의):** LAN에서 캡처→표시(glass-to-glass) 추가 지연 **p99 < 25ms @ 1440p120**, 인터넷에서 **< 25ms + RTT**. Moonlight/Parsec와 동급 목표치이며, 이 문서의 모든 설계 결정은 이 숫자에서 역산됐다.

---

## 이 프로젝트를 성립시키는 단 하나의 규칙

> **비디오 프레임은 절대 V8(JavaScript)을 통과하지 않는다.**

Electron/TS를 쓰면서 게임 지연을 달성하는 방법은 이것 하나다. 데이터 평면과 제어 평면을 물리적으로 분리한다:

- **제어 평면 (TypeScript/Electron):** 페어링 UI, 시그널링, 세션 협상, 설정, 코덱 네고 정책, 비트레이트 정책, 통계 표시, 업데이터
- **데이터 평면 (Rust):** 캡처 → 인코딩 → 패킷화 → 송신 → 수신 → 디코딩 → 표시 → 입력. 전용 스레드에서 끝까지 처리하며 JS 이벤트 루프를 한 번도 건드리지 않는다.

napi-rs 표면은 **제어 호출과 통계만** 노출한다. 통계는 `ThreadsafeFunction`으로 **최대 10Hz**. 프레임 버퍼·텍스처·패킷은 절대 넘기지 않는다. napi-rs가 TS 타입 정의(`index.d.ts`)를 자동 생성하므로 제어 평면과 코어의 계약이 컴파일 타임에 맞춰진다.

Node-API 바이너리는 런타임 독립적이라 **`.node` 하나가 Node와 Electron에서 재빌드 없이 그대로 로드**된다.

---

## 왜 Rust인가 (C++ 대비)

| 항목 | 효과 |
|---|---|
| **Cargo** | CMake + cmake-js + vcpkg × 3 OS 조합이 사라진다. 원래 계획에서 가장 큰 숨은 비용이었고, M0·M8·M10이 실질적으로 몇 주 줄어든다 |
| **napi-rs** | Windows/Linux/macOS 프리빌트 CI/CD 기본 제공. SWC·Rolldown이 쓰는 플랫폼별 optional dependency 패턴 그대로 사용 |
| **windows-rs** | Microsoft가 직접 관리하는 WinRT 프로젝션. `Windows.Graphics.Capture` + D3D11이 C++/WinRT보다 깔끔 |
| **objc2 프레임워크 크레이트** | `objc2-screen-capture-kit`, `objc2-video-toolbox`가 Xcode SDK에서 생성되고 Xcode 릴리스에 맞춰 갱신 |
| **`Send`/`Sync`** | 5개 스레드가 프레임 버퍼·통계를 공유하는 파이프라인에서 실제 버그를 잡는다 |
| **순수 Rust 대체** | Noise→`snow`, FEC→`reed-solomon-erasure`, PAKE→`spake2`, AES-GCM→`aes-gcm`. C 의존성이 코덱 SDK·opus·SDL3로만 줄어든다 |

**유일한 마찰: AMF(AMD 인코더).** COM 유사 vtable이라 bindgen이 깔끔히 못 뽑는다. `amf-shim` 크레이트에 ~200줄 C 심을 두고 `cc`로 빌드한다. 나머지 4개 인코더는 문제없다. 부수적으로 Sunshine/Moonlight/OBS 참조 구현이 전부 C++이라 복사가 아닌 번역이 된다.

---

## 아키텍처

```
prism/
├─ Cargo.toml                    Cargo 워크스페이스
├─ crates/
│  ├─ prism-core/                데이터 평면 전체 (라이브러리)
│  │  ├─ capture/    wgc.rs · sck.rs · pipewire.rs
│  │  ├─ encode/     nvenc.rs · amf.rs · vpl.rs · videotoolbox.rs · vaapi.rs
│  │  ├─ decode/     nvdec.rs · d3d11va.rs · videotoolbox.rs · vaapi.rs
│  │  ├─ render/     d3d11.rs · metal.rs · vulkan.rs
│  │  ├─ net/        ice.rs · session.rs(Noise) · packetizer.rs · fec.rs · pacer.rs · cc.rs · clock.rs
│  │  ├─ input/      capture_*(클라이언트) · inject_*(호스트)
│  │  ├─ audio/      wasapi.rs · sck_audio.rs · pipewire_audio.rs · opus.rs
│  │  └─ stats.rs    단계별 타임스탬프 링버퍼 → p50/p99
│  ├─ prism-napi/                napi-rs 얇은 표면 → .node (제어 + 통계만)
│  ├─ prism-cli/                 헤드리스 host/client 실행 파일 (M1 검증·CI 회귀용)
│  └─ amf-shim/                  AMF C 심 (cc 크레이트)
├─ packages/
│  ├─ protocol/      와이어 포맷 스펙 + 테스트 벡터 (Rust와 TS가 공유하는 단일 진실 소스)
│  ├─ host/          Electron 트레이 UI (TS)
│  ├─ client/        Electron UI 셸 (TS) — 스트림 창은 Rust/SDL3
│  └─ signaling/     Cloudflare Worker + Durable Object
└─ pnpm-workspace.yaml
```

### 크레이트 매핑

| 영역 | 크레이트 |
|---|---|
| Windows 캡처/GPU/입력/오디오 | `windows` (WGC, D3D11, DXGI, WASAPI, `SendInput`) |
| NVENC / NVDEC | `bindgen`으로 `nvEncodeAPI.h` / `nvcuvid.h` 직접 (LTR·`NvEncInvalidateRefFrames`가 헤더에 있음) |
| Intel oneVPL | `bindgen` (순수 C API) |
| AMD AMF | `amf-shim` (C) + `bindgen` |
| VAAPI | `libva-sys` / `bindgen` |
| macOS | `objc2`, `objc2-screen-capture-kit`, `objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video`, `objc2-metal`, `core-graphics`(CGEvent) |
| Linux | `pipewire`, `ashpd`(xdg 포털), `evdev`/`uinput` |
| ICE | `webrtc-ice`(순수 Rust, 단독 사용 가능) — 불안정하면 `libjuice` bindgen 폴백 |
| 암호 | `snow`(Noise_IK), `aes-gcm`, `spake2`, `ed25519-dalek` |
| FEC | `reed-solomon-erasure` (SIMD, 순수 Rust) |
| 오디오 코덱 | `audiopus` (libopus) |
| 창/입력/게임패드/오디오출력 | `sdl3` |
| 스레드 간 통신 | `crossbeam-channel`, std 스레드. **핫패스에 tokio 금지** (제어 평면·시그널링에만) |
| Node 바인딩 | `napi`, `napi-derive`, `@napi-rs/cli` |

### 호스트 스레드 모델 (세션당)

| 스레드 | 역할 |
|---|---|
| T1 | 캡처 (OS 콜백 스레드) → GPU 텍스처 핸들을 인코더로 직접 전달 |
| T2 | 인코드 서브밋 (GPU 디바이스 소유) |
| T3 | 인코드 완료 대기(async 이벤트) → **슬라이스 단위** 패킷화 → FEC → 페이싱 송신 |
| T4 | 네트워크 수신 — 입력 이벤트, ACK, 피드백 |
| T5 | 오디오 캡처 → Opus → 송신 |

**제로카피 필수:** 캡처된 GPU 텍스처가 CPU로 내려오지 않고 그대로 인코더 입력이 된다. Windows는 `ID3D11Texture2D` → `NvEncRegisterResource`, macOS는 `IOSurface` → VideoToolbox, Linux는 DMA-BUF → VAAPI.

### 클라이언트 스레드 모델

| 스레드 | 역할 |
|---|---|
| T1 | 네트워크 수신 → 재조립 → FEC 복구 → 디코드 서브밋 |
| T2 | 디코드 완료 → 표시 큐 |
| T3 | 표시 (vblank / CVDisplayLink 구동, 적응형 페이싱) |
| T4 | 입력 캡처 → **즉시** 송신 (배칭·페이싱 금지) |

### 클라이언트 창

Electron은 **UI 셸(설정·페어링·호스트 목록)만** 담당한다. 스트림 화면은 **Rust가 SDL3로 만든 별도 네이티브 창**에서 렌더링한다. Chromium 컴포지터를 완전히 우회해야 하기 때문이다(컴포지터는 1~2프레임을 먹는다). SDL3가 창 관리 + 상대 마우스 캡처 + 게임패드 + 오디오 출력을 한 번에 해결하고, 렌더러는 플랫폼별 백엔드(D3D11 flip-model / Metal / Vulkan)를 직접 붙인다.

---

## 지연을 만드는(혹은 죽이는) 12가지 결정

네이티브 SDK를 직접 호출하기로 한 이유가 바로 아래 항목들이다. ffmpeg 추상화로는 1~5번을 제대로 제어할 수 없다.

1. **인코더가 버퍼링하지 못하게 한다 — 가장 중요.**
   NVENC: `outputDelay=0`, B-프레임 0, lookahead 0, `NV_ENC_PARAMS_RC_CBR`,
   **`vbvBufferSize = averageBitRate / frameRate` (딱 1프레임)**, `vbvInitialDelay` 동일.
   VBV가 크면 인코더가 거대한 프레임을 뱉고 전송에 3프레임이 걸린다. 지연 스파이크의 최대 원인.

2. **슬라이스 단위 스트리밍.** `sliceMode=3, sliceModeData=4`(프레임당 4슬라이스). `NV_ENC_LOCK_BITSTREAM`의 슬라이스 오프셋을 읽어 완성되는 즉시 송신. 프레임 시간의 절반가량을 절약한다.

   > **측정 결과 제약:** Apple Silicon 하드웨어 H.264 인코더는 `kVTCompressionPropertyKey_MaxH264SliceBytes`를
   > 지원하지 않는다 (`kVTPropertyNotSupportedErr`, -12900). 따라서 **macOS 호스트에서는 이 항목을 적용할 수 없고**,
   > 프레임이 통째로 인코딩될 때까지 전송을 시작하지 못해 약 반 프레임(1440p120 기준 ~4ms)을 손해본다.
   > 주 목표 경로인 Windows/NVENC 호스트는 슬라이싱을 지원하므로 영향이 없다.
   > 코드에서는 세션을 실패시키지 않고 `VideoToolboxEncoder::slicing_supported()`로 능력을 노출한다.

3. **Async 인코드 + 이벤트 핸들.** `enableEncodeAsync=1` + 완료 이벤트 대기. 폴링 금지.

4. **IDR 대신 intra-refresh.** `enableIntraRefresh=1`, `intraRefreshPeriod=framerate`, `intraRefreshCnt=framerate/4`. 키프레임 비트레이트 스파이크를 없앤다.

5. **LTR + 참조 무효화로 손실 복구.** 클라이언트가 수신 프레임을 ACK → 손실 발생 시 호스트가 `NvEncInvalidateRefFrames` 호출 후 마지막 ACK된 LTR을 참조해 인코딩. **IDR 히칭이 영구히 사라진다.** 좋은 게임 스트리밍과 그저 그런 것의 차이가 여기서 갈린다.

6. **송신 페이싱.** 프레임 패킷을 회선 속도로 몰아 쏘지 않고 프레임 간격의 ~80%에 걸쳐 분산. 버퍼블로트와 버스트 손실 방지.

7. **클라이언트 표시 페이싱.** 디코드 즉시 표시하면 저더, 2프레임 버퍼링하면 +16ms. 측정된 p99 지터로 구동되는 **적응형 큐**(최소 0)를 쓴다.

8. **커서는 클라이언트가 그린다.** WGC `IsCursorCaptureEnabled=false`로 호스트 캡처에서 커서를 빼고, 커서 모양·위치는 제어 채널로 보내 클라이언트가 네이티브 주사율로 직접 렌더링. **체감 지연에서 가장 큰 단일 승리** — 영상이 30ms 늦어도 커서는 즉각 반응한다.

9. **입력 경로는 모든 것을 우회한다.** 페이싱·배칭 없이 즉시 송신, 호스트에서 전용 고우선순위 스레드로 주입. 마우스는 1000Hz 폴링.

10. **클럭 동기화.** 주기적 핑퐁의 min-RTT 샘플로 오프셋 추정. 단방향 지연 측정(혼잡 제어)과 통계 HUD의 전제 조건.

11. **혼잡 제어는 대역폭보다 지연을 우선한다.** 단방향 지연 기울기 + 손실률 기반 AIMD. 지연이 오르면 공격적으로 백오프 — 게임에서는 화질보다 지연이 먼저다.

12. **표시 시점.** D3D11은 flip-model + waitable object + `ALLOW_TEARING`, macOS는 CVDisplayLink. 렌더 어헤드 0.

---

## 프로토콜

UDP 단일 플로우. ICE로 뚫고, Noise 핸드셰이크로 키 교환 후 AES-GCM으로 패킷 단위 봉인.

```
[1B 채널][... 암호화된 페이로드]
채널: 0=제어  1=비디오  2=오디오  3=입력  4=피드백/ACK
```

비디오 패킷 헤더:
```
frame_id u32 | slice_id u16 | pkt_idx u16 | pkt_count u16 |
flags u8 (idr / last-of-frame / ltr) | capture_ts_us u64 | payload
```
페이로드는 **1200바이트 이하**로 유지 (PMTU 안전).

- **FEC:** 프레임/슬라이스 단위 Reed-Solomon (`reed-solomon-erasure`). 측정 손실률에 따라 패리티 비율 적응(기본 10~20%).
- **보안:** 호스트가 최초 실행 시 Ed25519 장기 키쌍 생성. 페어링은 호스트에 표시된 6자리 PIN을 **SPAKE2**로 처리(오프라인 브루트포스 차단) 후 공개키 상호 고정. 세션은 고정된 키로 **Noise_IK** 핸드셰이크 → 1-RTT + 전방 비밀성. 시그널링 서버는 암호화된 블롭과 ICE 후보만 보고 키는 절대 못 본다.
- **코덱 네고:** 호스트가 인코딩 가능 목록, 클라이언트가 디코딩 가능 목록을 광고 → 상호 최선 선택. **H.264는 항상 폴백으로 보장.** (AV1 인코딩은 NVIDIA Ada / AMD RDNA3 / Intel Arc 이상에서만 가능하므로 반드시 협상 대상.)

### NAT 트래버설 / 시그널링

- **ICE:** `webrtc-ice`를 Rust 코어에 내장. 미디어+입력 플로우는 코어가 자체 ICE 세션으로 소유한다.
- **시그널링:** Cloudflare Worker + Durable Object (호스트 ID당 DO 1개). 호스트가 WebSocket으로 상주, 클라이언트가 오퍼/앤서/후보를 DO 통해 교환. TS(Electron)가 WebSocket을 들고 후보를 napi 호출로 코어에 넘긴다 — 저빈도라 경계 통과 OK.
- **릴레이 폴백:** Cloudflare Realtime TURN (`turn.cloudflare.com`, anycast). Worker에서 단기 자격증명 발급. **비용 주의: 1000GB 무료 후 $0.05/GB. 40Mbps 기준 1000GB ≈ 55시간.** 릴레이는 예외 경로이지 기본 경로가 아니며, 앱 UI에 릴레이 사용 중임을 표시할 것.

---

## 지연 예산 (1440p120, 프레임 주기 8.33ms)

| 단계 | LAN 목표 |
|---|---|
| 캡처 (WGC 제로카피) | 0.5–2 ms |
| 인코드 (NVENC LL, 4슬라이스 → 첫 슬라이스) | ~1 ms (프레임 완료 2–4 ms) |
| 패킷화 + FEC | 0.2–0.5 ms |
| 네트워크 (LAN) | 0.5–2 ms |
| 재조립 (마지막 패킷 대기) | 프레임 바이트 / 회선 속도 |
| 디코드 | 2–5 ms |
| 표시 대기 (120Hz vblank) | 0–8.3 ms (평균 4.2) |
| **합계 (추가 지연)** | **12–25 ms** |

인터넷은 여기에 RTT가 더해진다. 참고로 클릭→광자 전체(게임 렌더 + 모니터 포함)는 45–70ms대가 되며, 로컬 네이티브가 25–40ms다.

macOS 호스트(M9)는 슬라이싱 불가로 인코드 항목이 "첫 슬라이스 ~1ms"가 아니라 "프레임 완료 2–4ms"가 되어
합계가 그만큼 늘어난다.

## 실측 기록

M1 진행 중 이 Mac(M시리즈, macOS 26.6)에서 측정한 값. 마일스톤 통과 판정의 근거가 되므로 갱신하며 유지한다.

| 항목 | 조건 | p50 | p99 | max | 비고 |
|---|---|---|---|---|---|
| 전송 + 재조립 | 루프백, 60fps, 40KB/frame, 19.6 Mbps | 0.20 ms | 0.34 ms | 1.36 ms | 300/300 프레임, 손실 0 |
| 전송 + 재조립 | 루프백, 120fps, 42KB/frame, 41.1 Mbps | 0.20 ms | 0.49 ms | 8.17 ms | 600/600 프레임, 손실 0 |
| VideoToolbox 인코드 | 1080p60, 24 Mbps 목표 | 4.72 ms | 7.76 ms | 52.55 ms | 실측 21.3 Mbps, max는 세션 워밍업 |

- 전송 계층은 1440p120 목표 레이트에서도 예산의 1% 미만을 쓴다. 최적화 우선순위가 아니다.
- 120fps에서 max 8.17ms는 정확히 한 프레임 주기라 프로세스 스케줄링 지연으로 읽힌다. M2 표시 페이싱에서 다룬다.
- 인코더 출력은 ffmpeg으로 교차 검증했다: High profile 1080p, 120프레임 전량 디코드, I 1개 + P 119개(B-프레임 0).

---

## 계측 (M1부터 필수, 나중에 붙이면 늦다)

프레임마다 전 구간 타임스탬프를 찍는다:
`capture_start → capture_done → encode_submit → first_slice_out → encode_done → first_byte_sent → last_byte_sent → first_pkt_recv → frame_complete → decode_submit → decode_done → present_queued → present_done`

클럭 동기화로 호스트/클라이언트 타임스탬프를 비교 가능하게 만들고, 클라이언트에 Moonlight 스타일 통계 HUD를 오버레이한다. **모든 마일스톤의 통과 기준은 이 HUD의 p99 숫자로 판정한다.**

---

## 마일스톤

각 단계는 측정 가능한 결과로 끝난다. 첫 수직 슬라이스를 **Windows(NVENC) 호스트 → macOS 클라이언트**로 잡은 이유: 실제 사용 시나리오(게임 PC를 Mac으로 스트리밍)와 정확히 일치하고, 보유 장비를 그대로 쓰며, 크로스플랫폼 문제를 첫날부터 강제로 드러내 버려지는 코드가 없다. 클라이언트 측 VideoToolbox 디코드는 5개 디코더 중 가장 단순해서 빨리 뜬다.

M1~M4는 `prism-cli`(헤드리스)로 진행한다. Electron은 M5부터 붙인다 — 지연 숫자 확보가 UI보다 먼저다.

| # | 내용 | 통과 기준 | 예상 |
|---|---|---|---|
| **M0** | `rustup` 설치, Cargo 워크스페이스 + pnpm 워크스페이스, napi-rs 최소 애드온(`version()`), `@napi-rs/cli` 3-OS CI, `protocol/` 테스트 벡터 스켈레톤 | 3개 OS에서 `.node` 빌드 + Node/Electron 양쪽 로드 | 1주 |
| **M1** | **수직 슬라이스 (CLI).** WGC 캡처 → NVENC(H.264) → 평문 UDP(LAN) → VideoToolbox 디코드 → SDL3 Metal 표시. 암호화·ICE 없음 | 1080p60, 추가 지연 **p99 < 40ms** | 3주 |
| **M2** | 전 구간 계측, 클럭 동기화, 통계 HUD, 적응형 표시 페이싱, **클라이언트 측 커서 렌더링** | HUD로 단계별 p50/p99 확인, 커서 체감 즉각 | 1.5주 |
| **M3** | 입력: SDL3 상대 마우스·키보드 캡처 → `SendInput` 주입 | 클릭→광자 측정, FPS 게임 조준 가능 | 1.5주 |
| **M4** | **프로토콜 경화.** 슬라이스 스트리밍, FEC, intra-refresh, LTR + 참조 무효화, 송신 페이싱, 적응형 비트레이트 | 5% 패킷 손실에서 **IDR 히칭 0회**, 지연 유지 | 3주 |
| **M5** | **인터넷 + Electron.** CF Worker+DO 시그널링, `webrtc-ice`, SPAKE2 페어링, Noise_IK 세션, TURN 폴백. Electron 호스트 트레이 + 클라이언트 셸이 napi로 코어 구동 | 서로 다른 NAT 뒤 두 지점 연결, 릴레이 폴백 동작, UI에서 페어링→접속 | 3.5주 |
| **M6** | **1440p120 + HEVC/AV1.** 코덱 협상, 고주사율 캡처·표시 경로 | **1440p120 p99 < 25ms** (핵심 목표 달성) | 2주 |
| **M7** | 오디오: WASAPI 루프백 → Opus(2.5–10ms 프레임) → SDL3 출력, 독립 지터 버퍼 | A/V 각각 저지연 유지, 드롭아웃 없음 | 1.5주 |
| **M8** | 나머지 인코더(AMF 심 + oneVPL) + Linux 호스트(PipeWire+VAAPI) + Windows/Linux 클라이언트(D3D11VA/NVDEC/VAAPI) | 보유 장비 4대 전 조합 통과 | 3.5주 |
| **M9** | macOS 호스트: ScreenCaptureKit + VideoToolbox + CGEventPost, TCC 권한 온보딩 | Mac 호스트 동작, 권한 안내 흐름 완성 | 2주 |
| **M10** | 게임패드(SDL3 읽기 → ViGEmBus/uinput 주입), 패키징·코드서명·노타리제이션, 자동 업데이트 | 3개 OS 원클릭 설치, 경고창 없음 | 2주 |

**총 ≈ 24주 (1인 기준 약 6개월).** 게임 가능한 첫 결과물은 M3(약 7주)에 나온다.

---

## 알아둘 제약과 비용

- **"간단한 설치"의 실제 비용은 코드서명이다.** macOS는 Apple Developer($99/년) + 노타리제이션, Windows는 OV/EV 코드서명 인증서(연 $100~400)가 있어야 경고창 없는 설치가 된다. 없으면 Gatekeeper/SmartScreen 경고를 사용자가 뚫어야 하고 "간단한 설치" 목표가 무너진다. M10 전에 미리 발급받아 둘 것.
- **Electron 설치 파일은 150~250MB.** 원클릭이긴 하지만 가볍지는 않다. 나중에 Tauri로 셸만 교체하면 10~15MB가 되고 코어는 그대로다 — 셸이 얇게 유지되도록 napi 표면을 작게 잡는 게 그 선택지를 열어둔다.
- **macOS 호스트는 TCC 권한 3종**(화면 기록 / 손쉬운 사용 / 입력 모니터링)을 요구한다. 온보딩 UX에서 가장 마찰이 큰 지점.
- **게임패드 주입은 v1에서 Windows(ViGEmBus, 드라이버 설치 필요)와 Linux(uinput)만.** macOS는 가상 게임패드 API가 없어 DriverKit 드라이버를 직접 써야 하므로 범위 밖.
- **AV1 인코딩은 하드웨어 게이트**(NVIDIA Ada+ / AMD RDNA3+ / Intel Arc+). 반드시 협상하고 H.264로 폴백.
- **HEVC는 상용 배포 시 라이선스 이슈**가 있다. 개인/오픈소스면 무관하나 상용화 계획이 있으면 확인 필요.
- **Apple Silicon 인코더는 슬라이스 크기 제한을 지원하지 않는다.** 위 지연 항목 2 참조. macOS 호스트 한정 제약이며 Windows/NVENC에는 영향이 없다.
- **네이티브 SDK 직접**을 택했으므로 인코더 5종 × 디코더 4종을 각각 구현해야 한다. M8이 가장 무거운 단계이며, 여기서 일정이 밀릴 가능성이 가장 크다. 리스크가 크면 M8을 "NVENC + VideoToolbox만"으로 잠시 좁히고 나머지를 M10 이후로 미루는 것이 가장 안전한 조정 지점이다.
- **GPU 핸들·SDK 호출은 전부 `unsafe`.** Rust의 소유권이 FFI 너머까지 보호해주지는 않는다. 각 SDK 래퍼를 안전한 타입으로 감싸는 얇은 계층(`encode/nvenc.rs` 안에서만 `unsafe`)을 두고 나머지 코드는 safe로 유지한다.

---

## 검증 방법

- **단계별 지연:** 클라이언트 통계 HUD의 p50/p99 (M2부터 상시 가동). 각 마일스톤 통과 판정의 기준.
- **클릭→광자:** 호스트 화면에 색이 바뀌는 테스트 패턴을 띄우고, 클라이언트에서 클릭 → 240fps 카메라(스마트폰 슬로우모션으로 충분)로 클라이언트 화면 촬영 → 프레임 카운트. M3, M6에서 실측.
- **손실 내성:** `dummynet`(macOS) / `clumsy`(Windows) / `tc netem`(Linux)으로 손실 1/3/5%, 지터 5/20ms를 주입해 M4 통과 판정.
- **NAT 조합:** 서로 다른 회선(집 Wi-Fi + 휴대폰 테더링)으로 M5 검증. 대칭 NAT 강제 시 TURN 폴백 동작 확인.
- **프로토콜 회귀:** `packages/protocol`의 테스트 벡터를 `cargo test`와 `vitest` 양쪽에서 돌려 Rust·TS 구현이 동일 결과를 내는지 CI 검증.
- **`prism-cli` 회귀:** 헤드리스 host↔client를 CI에서 루프백으로 띄워 패킷 손실 주입 시나리오를 자동화.
- **장시간 안정성:** 실제 게임 2시간 연속 세션에서 메모리 누수·지연 드리프트·오디오 싱크 이탈 없음 확인.

---

## 첫 커밋에서 할 일 (M0)

1. `rustup` 설치 (stable), `git init`
2. Cargo 워크스페이스: `crates/prism-core`, `crates/prism-napi`, `crates/prism-cli`, `crates/amf-shim`
3. pnpm 워크스페이스: `packages/protocol`, `packages/host`, `packages/client`, `packages/signaling`
4. `prism-napi`에 `version()` 하나만 노출 → Node 24와 Electron 양쪽에서 `require` 확인
5. `@napi-rs/cli`로 GitHub Actions 3-매트릭스(macOS arm64 / Windows x64 / Linux x64) 빌드 + 플랫폼별 npm 패키지 산출
6. `packages/protocol`에 와이어 포맷 타입과 테스트 벡터 스켈레톤, `cargo test` + `vitest` 양쪽 연결

---

Sources:
- [napi-rs cross build](https://napi.rs/docs/cross-build.en)
- [objc2 — Apple framework bindings](https://github.com/madsmtm/objc2)
- [objc2-screen-capture-kit](https://docs.rs/objc2-screen-capture-kit/latest/objc2_screen_capture_kit/)
- [objc2-video-toolbox](https://docs.rs/objc2-video-toolbox/latest/objc2_video_toolbox/)
- [Cloudflare Realtime TURN Service](https://developers.cloudflare.com/realtime/turn/)
- [Native Node Modules — Electron](https://www.electronjs.org/docs/latest/tutorial/using-native-node-modules)
