# Syzygy Worker Adapter Boundary

**Status:** Source-backed integration contract; adapter not implemented  
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

The mock prototype still models the supervisor issuing `session.start` and `session.stop`, but it now sends separate worker-originated `session.started` and `session.stopped` events and the supervisor tracks those events while awaiting correlated replies. This exercises the event direction needed by an adapter, but it does not make the mock command direction match Syzygy’s current Moonlight-driven session lifecycle. No existing Syzygy control socket accepts those commands.

## Adapter contract for the next implementation step

The Aphelion-owned adapter should be a separate executable that:

1. Implements the versioned Unix-socket envelope and required capability handshake.
2. Owns one foreground Syzygy process tree, including the private desktop launcher when selected.
3. Translates Aphelion's process-agnostic `SessionIntent` into validated Syzygy configuration and launch arguments; it must not place Syzygy-specific fields in the supervisor schema.
4. Reports process health separately from capture, encoder, audio, input, and network health. It reports a subsystem as unavailable until Syzygy exposes a measured value with defined semantics.
5. Implements shutdown with bounded signal escalation and confirms descendants are gone before acknowledging process teardown.
6. Reports observed client activity using adapter-generated IDs scoped to a worker generation only if the adapter can correlate start and stop safely. The Rust contract now provides a mapper that keeps an adapter-private source token out of the public ID and issues a new ID if that token is reused. Until correlation is confirmed, report aggregate counts and do not fabricate stable session identities.

The current `session.start` and `session.stop` commands should not be wired to fake per-client actions. Before the real adapter is considered complete, Aphelion must either add and test an Aphelion-side bridge for these operations or revise the external contract to distinguish worker start/shutdown from client-session events. That decision must preserve Moonlight behavior and keep media payloads off the control socket.

## Required integration tests

- Launch Syzygy in foreground mode and verify the adapter owns/reaps the expected process.
- Launch headless mode and verify KWin, audio services, and Syzygy descendants are cleaned up after normal shutdown, timeout, and adapter crash.
- Confirm the client session start/stop event source and its correlation limitations.
- Verify Moonlight pairing, stream startup, and disconnect behavior remain unchanged.
- Verify that only process and measured subsystem health is reported; no synthetic latency or fabricated client IDs.
- Verify that no frame, audio, or encoded media payload crosses the control socket.

## Licensing boundary

This audit references Syzygy source but copies no Syzygy implementation code. Any future inherited or modified C++ code must preserve required notices and satisfy the repository’s GPL-3.0-only metadata and applicable dependency terms. See the [licensing audit](LICENSING-AND-ATTRIBUTION-AUDIT.md).
