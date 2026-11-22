# Rust control-plane prototype

**Status:** CI-verified supervisor, mock worker, and first Syzygy process adapter. Fedora-host and real Syzygy binary verification are still pending.

This prototype proves a narrow Milestone 1 slice without modifying or building Syzygy:

- the Rust supervisor creates a private, per-run Unix socket directory;
- by default the supervisor launches a separate mock-worker process; with `APHELION_WORKER_KIND=syzygy`, it launches the separate `syzygy-worker` adapter;
- the worker connects back and identifies its instance and generation;
- both processes exchange versioned JSON messages using newline-delimited framing;
- the supervisor checks that the worker advertises host start/stop, health, and shutdown capabilities before accepting the handshake;
- the supervisor sends host start, health, host stop, and worker shutdown requests and checks correlated replies while accepting worker-originated client-session lifecycle events;
- the supervisor waits for and reaps the worker process;
- after an injected worker failure during `host.start`, the supervisor retries with a fresh worker ID and generation, then starts the host.

The mock worker maintains an in-memory session set. Its subsystem health deliberately reports `not_implemented`; it does not claim that capture, encoding, input, or networking works.

## Build and run

From the repository root on Linux:

```bash
cargo build --workspace --release
./target/release/supervisor
```

The supervisor locates the selected worker beside its own executable. Build the entire workspace rather than only the supervisor binary. To use the Syzygy adapter, set `APHELION_WORKER_KIND=syzygy` and `APHELION_SYZYGY_EXECUTABLE=/path/to/syzygy`; optional existing Syzygy CLI settings can be provided as a JSON string array in `APHELION_SYZYGY_ARGS`.

Expected log sequence includes:

- `handshake complete`
- `start_host` with state `process_running`
- `get_health` with worker state and explicit unimplemented subsystem states
- `stop_host` with state `stopped`
- `shutdown acknowledged`
- worker process exit status

## Host lifecycle and client sessions

The worker IPC controls a host process, not an individual Moonlight client. `host.start` and `host.stop` carry no `session_id`; repeated start while running and stop while stopped are idempotent. Moonlight protocol activity creates and ends client sessions. The supervisor tracks worker-originated `session.started` and `session.stopped` events when their identity can be safely correlated.

The design draft's process-agnostic stream intent remains a candidate for a future user-facing desired-session API. It is not sent to the current Syzygy adapter because the audited CLI does not expose a verified mapping for display, codec, resolution, frame rate, and audio settings. The adapter reports that the stream is not ready rather than implying those values were applied. See [Syzygy worker adapter](SYZYGY-WORKER-ADAPTER.md) for evidence and remaining integration work.

## Request and session retry behavior

- `request_id` is scoped to one worker generation. The worker caches the last 128 completed request/response pairs in FIFO order.
- An exact retry with a cached request ID receives the original response without applying the request again. Reusing a cached ID with different content is rejected and closes the worker IPC session.
- Retries older than the bounded cache window may be processed again. Session operations remain idempotent across distinct request IDs: starting an active session returns `created: false`; stopping an inactive session returns `was_active: false`.
- The cache is in-memory and is discarded when the worker exits. It does not provide crash-safe exactly-once execution or restore desired sessions after a restart.

Worker-originated `session.started` and `session.stopped` events update an idempotent active-session registry while the supervisor waits for request-correlated responses. The Syzygy adapter maps private runtime tokens from paired log lines to IDs scoped by worker generation and emits lifecycle events; the source token itself is not exposed in the control protocol. For the source-backed real-worker integration boundary, see [Syzygy worker adapter](SYZYGY-WORKER-ADAPTER.md).

## Deliberate prototype limits

- One worker and one host lifecycle demonstration per run.
- `SOCK_STREAM` with newline-delimited JSON; this is a prototype choice, not the final transport decision.
- 64 KiB maximum message size.
- No external client API, heartbeat, peer-credential verification, durable request deduplication, or real media subsystem. Restart/backoff and host restart are verified only with the mock worker; recovery of a real Syzygy worker and its client sessions is not implemented.
- The mock remains the default; the Syzygy adapter is a Rust process owner around the existing C++ host, not a replacement streaming implementation.
- No media payload crosses the control socket.

## Verification status

GitHub Actions performs a workspace build, IPC envelope and lifecycle unit tests, a normal mock-worker smoke run, a fault-injected restart recovery run, and a Syzygy process-adapter run against a stub executable. The normal smoke run checks handshake, host start, health, host stop, shutdown acknowledgement, and child exit status 0. The restart test requires the first worker to fail during `host.start`, then checks retry logging, generation 2, and successful completion.

CI now covers incompatible protocol versions, missing request IDs, stale worker generations, incorrect response types, request correlation, worker-originated session start/stop event ordering, malformed/oversized/truncated/disconnected IPC input, crash reaping, graceful exit, and SIGTERM-to-SIGKILL escalation. Duplicate request replay and host lifecycle idempotency are covered by worker unit tests. Supervisor-loss cleanup, peer authentication, durable session recovery, integration against a built Syzygy binary, headless descendant cleanup, real Moonlight session behavior, and Fedora-host verification remain open.
