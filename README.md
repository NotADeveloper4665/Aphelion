# Aphelion

Aphelion is an independently developed, local-first Linux game-streaming host. It selectively inherits and progressively replaces components from Syzygy while maintaining a separate project, roadmap, and release lifecycle.

**Project status:** Architecture and source-audit phase. No streaming implementation is present yet.

## Initial direction

- Preserve Moonlight client compatibility as the first protocol target.
- Begin with a Rust supervisor and a Thick Worker retaining the existing Syzygy streaming pipeline.
- Keep media data inside the worker; use local IPC for versioned control and health only.
- Preserve verified shared-capture and per-session encoding behavior.
- Replace components incrementally, only after tests demonstrate compatibility and a real architectural benefit.
- Require no cloud account, subscription, or vendor service for core streaming.

## Architecture documents

- [Founding architecture](docs/FOUNDING-ARCHITECTURE.md) — principles, process model, control contracts, extraction gates, and milestones.
- [Initial Syzygy inheritance audit](docs/SYZYGY-INHERITANCE-AUDIT.md) — observed capture, encoding, audio, session, and network ownership in the current Syzygy source.
- [Licensing and attribution audit](docs/LICENSING-AND-ATTRIBUTION-AUDIT.md) — initial GPL and dependency findings; not a complete dependency audit.

## Project boundaries

Syzygy remains independently maintained. Aphelion may reuse code only with its license, notices, and upstream obligations respected. No Syzygy release, branch, or roadmap is being replaced by this project.

## Development policy

The solo maintainer is the sole approver for merges and releases. AI coding assistants may research, propose, implement, and test changes, but must not publish, merge, tag, or release without explicit approval. Prefer small changes with reproducible tests and measured results.
