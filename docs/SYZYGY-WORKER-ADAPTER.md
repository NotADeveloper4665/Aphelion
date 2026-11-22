# Syzygy Worker Adapter Boundary

**Status:** Process-control adapter implemented; Moonlight client-session control is not implemented  
**Audited source:** NotADeveloper4665/Syzygy at `c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d`  
**Scope:** Process launch, signal ownership, headless child processes, and session identity

This document records what the pinned Syzygy source actually exposes so Aphelion does not mistake the Rust mock protocol for an existing Syzygy API. Aphelion and Syzygy remain separate projects. This audit makes no Syzygy source changes.

## Verified launch behavior

- On Linux, `syzygy -s` delegates to the Python service manager unless `-foreground` is supplied or `SYZYGY_MANAGED_CHILD=1` is set. The decision is in [src/main.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/main.cpp#L165-L181); the foreground option is parsed in [src/syzygy/cli.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/syzygy/cli.cpp#L19-L25).
- The service manager runs the C++ process in a new session/process group, captures its output, handles termination, and escalates cleanup. It sets `SYZYGY_MANAGED_CHILD=1` for the child. See [syzygy-service.py](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src_assets/linux/assets/scripts/syzygy-service.py#L98-L154).
- `-headless` starts a Python-managed private desktop. That launcher owns D-Bus, KWin, PipeWire, WirePlumber, and the Syzygy process, and cleans up its process group. See [syzygy-headless.py](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src_assets/linux/assets/scripts/syzygy-headless.py#L99-L160) and [its session cleanup](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src_assets/linux/assets/scripts/syzygy-headless.py#L177-L216).

An Aphelion adapter must own the top-level launcher and its descendants. It must not launch `syzygy -s` as an ordinary child and assume the C++ process remains attached. Signal, timeout, and process-group behavior needs an integration test for both desktop and headless modes.

## Session identity and control mismatch

Syzygy creates a client session after incoming streaming-protocol work. The source logs `SYZYGY_SESSION_START <address-of-session> <codec>` and `SYZYGY_SESSION_STOP <address-of-session>` from [src/stream.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/stream.cpp#L2168-L2170). The value is a process-local pointer. It may be reused and must not become an Aphelion `session_id`. The service manager currently consumes these lines to maintain an active-client count and codec list; it does not expose a durable client identity or a Unix-socket event API.

The prototype now separates worker host lifecycle commands (`host.start` / `host.stop`) from client-session events (`session.started` / `session.stopped`). Syzygy's Moonlight protocol path creates and ends client sessions; no existing Syzygy control socket accepts per-client start/stop commands. The supervisor event registry remains covered independently, and the adapter reports aggregate client activity from logs until safe per-client correlation is established.

## Adapter contract for the next implementation step

The Aphelion-owned adapter should be a separate executable that:

1. Implements the versioned Unix-socket envelope and required capability handshake.
2. Owns one foreground Syzygy process tree, including the private desktop launcher when selected.
3. Starts/stops the Syzygy host process without treating that lifecycle as a client session. A future process-agnostic stream intent must remain unapplied until a verified Syzygy configuration mapping exists; it must not place Syzygy-specific fields in the supervisor schema.
4. Reports process health separately from capture, encoder, audio, input, and network health. It reports a subsystem as unavailable until Syzygy exposes a measured value with defined semantics.
5. Implements shutdown with bounded signal escalation and confirms descendants are gone before acknowledging process teardown.
6. Reports observed client activity using adapter-generated IDs scoped to a worker generation only if the adapter can correlate start and stop safely. The Rust contract now provides a mapper that keeps an adapter-private source token out of the public ID and issues a new ID if that token is reused. Until correlation is confirmed, report aggregate counts and do not fabricate stable session identities.

The worker contract has been revised to distinguish host lifecycle from client-session events. A separate per-client command bridge remains future work and must be demonstrated against real Moonlight behavior before adding such commands. This avoids changing or bypassing Syzygy's protocol behavior and keeps media payloads off the control socket.

## Implemented adapter slice

The separate Rust `syzygy-worker` executable now implements the existing versioned worker handshake and lifecycle requests. The supervisor selects it with `APHELION_WORKER_KIND=syzygy`; the worker reads the binary path from `APHELION_SYZYGY_EXECUTABLE` and optional launch arguments from `APHELION_SYZYGY_ARGS` as a JSON string array. It starts the existing binary with `-s -foreground` and `SYZYGY_MANAGED_CHILD=1`, without invoking a shell. Existing Syzygy configuration remains in effect.

The prototype's `host.start` and `host.stop` messages manage one Syzygy host process per worker. They do not start or stop an individual Moonlight client. The start response explicitly reports `stream_ready=false`: Syzygy's current CLI does not expose a verified mapping for Aphelion's display, codec, resolution, frame-rate, and audio intent. The adapter does not alter those settings or claim that a client stream is active.

Health reports whether the Syzygy process is running and an observed active-client count from Syzygy's `SYZYGY_SESSION_START/STOP` output tokens. It does not publish the process-local tokens, fabricate stable client IDs, or claim capture, encoder, audio, input, network, or latency health. If Syzygy stops, health includes its exit status. Child stdout and stderr are drained continuously.

The adapter places Syzygy in its own process group. Stop and shutdown send SIGTERM, wait up to two seconds, then send SIGKILL and reap the direct child. The CI adapter scenario uses a stub executable to verify arguments, environment, process health, and teardown; this is not an integration run against a built Syzygy binary.

`APHELION_SYZYGY_ARGS` must be a JSON string array. The adapter rejects supervisor-owned lifecycle flags in that array. For example: `["-auto", "-nvenc"]`. Headless launcher cleanup and real Syzygy runtime behavior still require integration testing.

## Required integration tests

- Launch Syzygy in foreground mode and verify the adapter owns/reaps the expected process.
- Launch headless mode and verify KWin, audio services, and Syzygy descendants are cleaned up after normal shutdown, timeout, and adapter crash.
- Confirm the client session start/stop event source and its correlation limitations.
- Verify Moonlight pairing, stream startup, and disconnect behavior remain unchanged.
- Verify that only process and measured subsystem health is reported; no synthetic latency or fabricated client IDs.
- Verify that no frame, audio, or encoded media payload crosses the control socket.

## Licensing boundary

This audit references Syzygy source but copies no Syzygy implementation code. Any future inherited or modified C++ code must preserve required notices and satisfy the repository’s GPL-3.0-only metadata and applicable dependency terms. See the [licensing audit](LICENSING-AND-ATTRIBUTION-AUDIT.md).
