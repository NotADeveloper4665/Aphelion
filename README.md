# Aphelion

Aphelion is an independently developed, local-first Linux game-streaming host. It selectively inherits and progressively replaces components from Syzygy while maintaining a separate project, roadmap, and release lifecycle.

**Project status:** CI-verified Rust control-plane prototype with bounded worker restart/backoff, structured lifecycle diagnostics, bounded control-health RTT reporting, and versioned local IPC. A real phone Moonlight stream and a successful Ctrl+C stop through the supervisor-managed Syzygy process were observed by the maintainer. Aphelion still delegates all media handling to Syzygy; Milestone 2 media measurements and repeated target-host acceptance remain open.

## Initial direction

- Preserve Moonlight client compatibility as the first protocol target.
- Begin with a Rust supervisor and a Thick Worker retaining the existing Syzygy streaming pipeline.
- Keep media data inside the worker; use local IPC for versioned control and health only.
- Preserve verified shared-capture and per-session encoding behavior.
- Replace components incrementally, only after tests demonstrate compatibility and a real architectural benefit.
- Require no cloud account, subscription, or vendor service for core streaming.

## Rust control-plane prototype

The executable Milestone 1 slice is on the [`prototype/rust-control-plane`](https://github.com/NotADeveloper4665/Aphelion/tree/prototype/rust-control-plane) branch. It builds a Rust supervisor, a mock-worker, and a separate Syzygy process adapter that exchange versioned JSON over a local Unix socket. The supervisor bounds worker restarts with exponential backoff, validates worker identity and response correlation, and cleans up child processes. Exact request retries replay their cached response; host start and stop remain idempotent, while Moonlight client sessions are observed as worker-originated events. See [build and run instructions](docs/CONTROL-PLANE-PROTOTYPE.md).

The Syzygy adapter can launch and stop an existing Syzygy binary and report process health, measured host uptime, observed client counts from Syzygy logs, and separate subsystem telemetry states. The supervisor summarizes idempotent client-session lifecycle counts and bounded health-IPC round-trip measurements. These are control-plane signals, not stream latency. The adapter does not modify Syzygy or claim stream readiness, apply unverified stream settings, or implement media handling inside Aphelion. See the adapter boundary document for current limits.

An isolated [`aphelion-streamer`](streamer/) crate now has a permission-scoped PipeWire/GStreamer BGRA capture adapter, a low-latency FFmpeg/libx264 encoder adapter, RTSP framing and session lifecycle primitives, plus experimental RTP output, generic FEC, and video AES-GCM primitives. Capture/encoding are not connected to the supervisor worker; the RTSP state machine omits pairing, authentication, key derivation, and Moonlight capability negotiation; standard RTP output does not emit Moonlight packet metadata. This is active Rust streamer groundwork, not yet a Moonlight-compatible Rust stream. See the [Rust streamer prototype notes](docs/RUST-STREAMER-PROTOTYPE.md).

The target Rust media path and its design principles are documented in [Rust Streamer Architecture](docs/RUST-STREAMER-ARCHITECTURE.md) and [Rust Streamer Philosophy](docs/RUST-STREAMER-PHILOSOPHY.md). They define the capture/encoder/session/packet ownership boundaries, Moonlight compatibility and security requirements, rollout gates, and implementation sequence.

The Actions workflow also produces a temporary Linux x86_64 prototype bundle on successful branch pushes. See [runtime prototype instructions](docs/PROTOTYPE-RUNTIME-TEST.md) for download and first-run steps. One Fedora desktop run confirmed a real Moonlight stream and Ctrl+C shutdown; broader hardware, repeated real-client lifecycle, input, and direct-versus-managed regression coverage remain pending.

The maintainer-observed Moonlight stream, disconnect, and Ctrl+C shutdown are recorded in [Milestone 1 runtime results](docs/MILESTONE-1-RUNTIME-RESULTS.md). Milestone 2 software observability and stubbed restart/recovery checks are in place. Real media telemetry, profile-combination support, repeated real-client/orphan checks, direct-versus-managed comparison, and the incomplete Milestone 0 baseline remain open. The adapter reports honest unavailable states, and CI covers repeated lifecycle cleanup with a stub.

## Architecture documents

- [Founding architecture](docs/FOUNDING-ARCHITECTURE.md) — principles, process model, control contracts, extraction gates, and milestones.
- [Initial Syzygy inheritance audit](docs/SYZYGY-INHERITANCE-AUDIT.md) — observed capture, encoding, audio, session, and network ownership in the current Syzygy source.
- [Licensing and attribution audit](docs/LICENSING-AND-ATTRIBUTION-AUDIT.md) — initial GPL and dependency findings; not a complete dependency audit.
- [Source ownership and lifecycle audit](docs/SYZYGY-OWNERSHIP-AND-LIFECYCLE-AUDIT.md) — Linux input, display capture, GPU resources, and audio ownership at a pinned Syzygy commit.
- [Syzygy worker adapter boundary](docs/SYZYGY-WORKER-ADAPTER.md) — verified launch semantics, process-tree ownership, session identity limits, and integration tests required before connecting a real worker.
- [Milestone 0 test plan](docs/MILESTONE-0-TEST-PLAN.md) — baseline, compatibility, hardware, lifecycle, and performance verification required before media-path implementation.
- [Milestone 0 baseline results](docs/MILESTONE-0-BASELINE-RESULTS.md) — recorded CI evidence for pinned Syzygy commit `c50d417`, including passed checks and remaining gaps.

## Project boundaries

Syzygy remains independently maintained. Aphelion may reuse code only with its license, notices, and upstream obligations respected. No Syzygy release, branch, or roadmap is being replaced by this project.

## Development policy

The solo maintainer is the sole approver for merges and releases. AI coding assistants may research, propose, implement, and test changes, but must not publish, merge, tag, or release without explicit approval. Prefer small changes with reproducible tests and measured results.
