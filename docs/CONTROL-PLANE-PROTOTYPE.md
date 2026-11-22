# Rust control-plane prototype

**Status:** Initial executable prototype; GitHub Actions build, unit tests, and supervisor/worker IPC smoke run pass. Fedora-host verification is still pending.

This prototype proves a narrow Milestone 1 slice without modifying or building Syzygy:

- the Rust supervisor creates a private, per-run Unix socket directory;
- the supervisor launches a separate mock-worker process;
- the worker connects back and identifies its instance and generation;
- both processes exchange versioned JSON messages using newline-delimited framing;
- the supervisor sends start, health, stop, and shutdown requests and checks correlated replies;
- the supervisor waits for and reaps the worker process.

The mock worker maintains an in-memory session set. Its subsystem health deliberately reports `not_implemented`; it does not claim that capture, encoding, input, or networking works.

## Build and run

From the repository root on Linux:

```bash
cargo build --workspace --release
./target/release/supervisor
```

The supervisor locates `mock-worker` beside its own executable. Build the entire workspace rather than only the supervisor binary.

Expected log sequence includes:

- `handshake complete`
- `start_session` with state `running`
- `get_health` with worker state and explicit unimplemented subsystem states
- `stop_session` with state `stopped`
- `shutdown acknowledged`
- worker process exit status

## Deliberate prototype limits

- One worker and one demonstration session per run.
- `SOCK_STREAM` with newline-delimited JSON; this is a prototype choice, not the final transport decision.
- 64 KiB maximum message size.
- No external client API, restart policy, heartbeat, peer-credential verification, session recovery, or real media subsystem.
- The worker is a Rust mock, not the Syzygy C++ worker.
- No media payload crosses the control socket.

## Verification status

GitHub Actions currently performs a workspace build, IPC envelope unit tests, and an end-to-end supervisor/worker smoke run. The smoke run has confirmed handshake, start, health, stop, shutdown acknowledgement, and child exit status 0.

This is not yet a full contract test suite. The next verification work is malformed/oversized messages, version mismatch, duplicate requests, worker crash, shutdown timeout, and supervisor loss. Then run the same build and smoke sequence on the target Fedora host before treating the prototype as stable.
