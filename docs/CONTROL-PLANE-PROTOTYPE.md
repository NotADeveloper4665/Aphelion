# Rust control-plane prototype

**Status:** Initial executable prototype; GitHub Actions build, unit tests, and supervisor/worker IPC smoke run pass. Fedora-host verification is still pending.

This prototype proves a narrow Milestone 1 slice without modifying or building Syzygy:

- the Rust supervisor creates a private, per-run Unix socket directory;
- the supervisor launches a separate mock-worker process;
- the worker connects back and identifies its instance and generation;
- both processes exchange versioned JSON messages using newline-delimited framing;
- the supervisor checks that the worker advertises session start/stop, health, and shutdown capabilities before accepting the handshake;
- the supervisor sends start, health, stop, and shutdown requests and checks correlated replies;
- the supervisor waits for and reaps the worker process;
- after an injected worker failure during `session.start`, the supervisor retries with a fresh worker ID and generation, then replays the desired session start.

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

## Process-agnostic session request

A `session.start` body contains desired stream intent: a display selector, codec, width, height, frame rate, and audio-enabled flag. The mock validates non-empty display selection and non-zero video dimensions/frame rate. It contains no capture-backend, encoder-backend, Syzygy class, or C++ layout fields. A future adapter translates this stable intent into its implementation-specific configuration.

```json
{
  "display": "default",
  "video": {"codec": "h264", "width": 1920, "height": 1080, "fps": 60},
  "audio_enabled": false
}
```

This schema describes a request contract only; the mock does not capture or stream media. The handshake also requires the worker to advertise start, stop, health, and shutdown support before the supervisor accepts it.

## Request and session retry behavior

- `request_id` is scoped to one worker generation. The worker caches the last 128 completed request/response pairs in FIFO order.
- An exact retry with a cached request ID receives the original response without applying the request again. Reusing a cached ID with different content is rejected and closes the worker IPC session.
- Retries older than the bounded cache window may be processed again. Session operations remain idempotent across distinct request IDs: starting an active session returns `created: false`; stopping an inactive session returns `was_active: false`.
- The cache is in-memory and is discarded when the worker exits. It does not provide crash-safe exactly-once execution or restore desired sessions after a restart.

For the source-backed real-worker integration boundary, see [Syzygy worker adapter](SYZYGY-WORKER-ADAPTER.md).

## Deliberate prototype limits

- One worker and one demonstration session per run.
- `SOCK_STREAM` with newline-delimited JSON; this is a prototype choice, not the final transport decision.
- 64 KiB maximum message size.
- No external client API, heartbeat, peer-credential verification, durable request deduplication, or real media subsystem. Restart/backoff and desired-session replay are verified only with the mock worker; recovery of a real Syzygy worker and its sessions is not implemented.
- The worker is a Rust mock, not the Syzygy C++ worker.
- No media payload crosses the control socket.

## Verification status

GitHub Actions performs a workspace build, IPC envelope unit tests, a normal end-to-end supervisor/worker smoke run, and a fault-injected restart recovery run. The normal smoke run checks handshake, start, health, stop, shutdown acknowledgement, and child exit status 0. The restart test requires the first worker to fail during `session.start`, then checks retry logging, generation 2, and successful completion.

CI now covers incompatible protocol versions, missing request IDs, stale worker generations, incorrect response types, request correlation, malformed/oversized/truncated/disconnected IPC input, crash reaping, graceful exit, and SIGTERM-to-SIGKILL escalation. Duplicate request replay and session idempotency are covered by worker unit tests. Supervisor-loss cleanup, peer authentication, durable session recovery, integration with a real Syzygy worker, and Fedora-host verification remain open.
