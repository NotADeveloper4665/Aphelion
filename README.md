# Aphelion

Aphelion is an independently developed, local-first Linux game-streaming host. It selectively inherits and progressively replaces components from Syzygy while maintaining a separate project, roadmap, and release lifecycle.

**Project status:** CI-verified Rust control-plane prototype with bounded worker restart/backoff and versioned local IPC. The maintainer has observed a real phone Moonlight stream through the supervisor-managed Syzygy process. Aphelion still delegates all media handling to Syzygy, and managed-process shutdown on the real host remains to be confirmed.

## Initial direction

- Preserve Moonlight client compatibility as the first protocol target.
- Begin with a Rust supervisor and a Thick Worker retaining the existing Syzygy streaming pipeline.
- Keep media data inside the worker; use local IPC for versioned control and health only.
- Preserve verified shared-capture and per-session encoding behavior.
- Replace components incrementally, only after tests demonstrate compatibility and a real architectural benefit.
- Require no cloud account, subscription, or vendor service for core streaming.

## Rust control-plane prototype

The executable Milestone 1 slice is on the [`prototype/rust-control-plane`](https://github.com/NotADeveloper4665/Aphelion/tree/prototype/rust-control-plane) branch. It builds a Rust supervisor, a mock-worker, and a separate Syzygy process adapter that exchange versioned JSON over a local Unix socket. The supervisor bounds worker restarts with exponential backoff, validates worker identity and response correlation, and cleans up child processes. Exact request retries replay their cached response; host start and stop remain idempotent, while Moonlight client sessions are observed as worker-originated events. See [build and run instructions](docs/CONTROL-PLANE-PROTOTYPE.md).

The Syzygy adapter can launch and stop an existing Syzygy binary and report process health plus observed client counts from Syzygy logs. It does not modify Syzygy or claim stream readiness, apply unverified stream settings, or implement media handling inside Aphelion. See the adapter boundary document for current limits.

The Actions workflow also produces a temporary Linux x86_64 prototype bundle on successful branch pushes. See [runtime prototype instructions](docs/PROTOTYPE-RUNTIME-TEST.md) for download and first-run steps. The first real Moonlight stream is now observed; broader Fedora, hardware, lifecycle, and regression coverage remain pending.

The first maintainer-observed Moonlight phone stream through the supervisor is recorded in [Milestone 1 runtime results](docs/MILESTONE-1-RUNTIME-RESULTS.md). Full lifecycle, regression, and Milestone 0 baseline checks remain open.

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
