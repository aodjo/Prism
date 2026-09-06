# Prism

Low-latency remote desktop, built for playing games rather than reading documents.

**Target:** p99 glass-to-glass added latency under 25 ms at 1440p120 on a LAN, and
under 25 ms plus RTT over the internet.

## Design

The whole project rests on one rule:

> A video frame never crosses into JavaScript.

Electron and TypeScript run the control plane — pairing, signalling, settings, policy,
stats. A Rust core owns the entire data plane: capture, encode, packetise, send,
receive, decode, present, input. The Node-API surface between them carries control
calls and stats at 10 Hz, never pixels.

See `docs/plan.md` for the full design and milestones, and `docs/wire-format.md` for the
protocol.

## Layout

| Path | Contents |
|---|---|
| `crates/prism-core` | Data plane: capture, encode, transport, decode, present, input |
| `crates/prism-napi` | Node-API surface — control and stats only |
| `crates/prism-cli` | Headless host and client, used for latency measurement and CI |
| `packages/protocol` | Wire format, shared by Rust and TypeScript |
| `packages/native` | Built addon, consumed by the Electron shells |
| `packages/host` | Electron tray UI |
| `packages/client` | Electron UI shell; the stream window is Rust and SDL3 |
| `packages/signaling` | Cloudflare Worker and Durable Object |

## Development

```sh
pnpm install
pnpm build:native     # build the addon into packages/native
pnpm test             # cargo test --workspace, then vitest
pnpm smoke:node       # addon loads in Node
pnpm smoke:electron   # addon loads in Electron
pnpm lint             # clippy and rustfmt
```

`packages/protocol/vectors.json` is the single source of truth for the wire format. Both
`cargo test` and `vitest` assert against it, so a layout change applied to only one
implementation fails CI. Change the vectors first.

## Status

M0 complete: workspaces, the wire format with cross-language conformance tests, and a
Node-API addon that loads in both Node and Electron on all three platforms.

Next is M1, the vertical slice — Windows WGC capture through NVENC to a macOS
VideoToolbox client over LAN, targeting p99 under 40 ms at 1080p60.
