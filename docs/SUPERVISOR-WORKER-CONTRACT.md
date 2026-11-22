# Aphelion Supervisor–Worker Contract

**Status:** Architecture contract draft 0.1 — proposed for maintainer review  
**Date:** 2026-11-22  
**Scope:** Milestone 1 Rust supervisor and Syzygy-derived Thick Worker  
**Decision owner:** Solo maintainer

This document turns the founding architecture into an operational process and control-plane contract. It is a design proposal, not an implementation claim. Items marked **provisional** require a prototype or maintainer decision before they become compatibility commitments.

## 1. Contract goals and non-goals

The contract must:

- isolate worker crashes from the supervisor;
- preserve Moonlight behavior by keeping the inherited streaming pipeline intact;
- identify logical sessions independently of processes and operating-system PIDs;
- make lifecycle operations correlated, bounded, and safe to retry;
- expose truthful worker and subsystem health;
- make shutdown and resource ownership observable;
- keep media payloads out of the Milestone 1 control channel.

It does not define the Moonlight wire protocol, expose Syzygy C++ types, promise seamless recovery, or select a future cross-process media transport. It does not require capture, encoding, audio, input, and networking to become separate processes.

## 2. Process and ownership model

### 2.1 Roles

**Supervisor (Rust)** owns:

- desired host/session state and the authoritative session registry;
- worker creation, launch configuration, process monitoring, and restart policy;
- control IPC endpoints and request correlation;
- worker-to-session assignments;
- health aggregation, diagnostics, and bounded shutdown escalation;
- cleanup of supervisor-owned sockets, child handles, and runtime metadata.

**Worker (initially Syzygy-derived Thick Worker)** owns:

- the actual streaming runtime assigned to it;
- capture, encoding, audio, input, Moonlight protocol, packetization, encryption, and network resources as inherited;
- worker-local session state and all media resources it creates;
- reporting its state and measured subsystem health to the supervisor.

The worker must not become the authority for host policy, persistent configuration, or global restart decisions. The supervisor must not reach into worker internals or manipulate media resources behind the worker's back.

### 2.2 Worker granularity

A worker process is not synonymous with a client session. The intended initial grouping is one worker per capture source, with one or more logical sessions hosted by that worker when the inherited implementation supports it. This is a target to validate against source behavior and tests, not permission to invent new sharing semantics.

The supervisor maintains an explicit mapping:

- capture-source assignment → worker instance;
- logical session → worker instance;
- logical session → its own requested/negotiated stream profile and client state.

Shared capture must not imply shared encoder state. Preserve the audited per-session encoder behavior and supported profile combinations. If the initial wrapper cannot safely host more than one session, enforce that limitation explicitly and document it rather than silently changing resource ownership.

## 3. Identity model

All IDs are opaque strings on the wire. UUIDs are the proposed representation; the exact UUID version is not externally significant.

| Identifier | Created by | Lifetime and meaning |
|---|---|---|
| `session_id` | Supervisor | Stable logical stream/session identity. Survives worker restart while that logical session is being recovered. Never derived from PID. |
| `worker_instance_id` | Supervisor | New unique ID for every process launch. Never reused, including after crash/restart. |
| `worker_generation` | Supervisor | Monotonically increasing generation for a worker assignment/replacement. Fences stale messages from an earlier process. |
| `request_id` | Request sender | Unique correlation ID for one request/reply exchange. |
| `event_id` | Event sender | Unique ID for an asynchronous event, if event delivery is added. |
| `capture_source_id` | Supervisor registry | Stable identity for a configured capture source, not a raw device pointer or backend handle. |

The supervisor is authoritative for `session_id`, `worker_instance_id`, and `worker_generation`. The worker echoes them. Every worker-originated message must carry its worker instance and generation; session-scoped messages must also carry `session_id`.

A message from an unknown worker instance, an old generation, or a session not assigned to that worker is rejected and logged as stale/unauthorized. A PID is diagnostic metadata only and must never be used as identity or authorization.

A session ID remains stable across a worker replacement only if the supervisor is recovering the same logical session. A deliberate new client connection or new logical stream receives a new session ID. Session IDs are not authentication secrets.

## 4. Control transport and wire envelope

### 4.1 Transport

The control plane uses a local Unix-domain socket. The exact socket type is **provisional**: prototype stream sockets with explicit framing and `SOCK_SEQPACKET` before choosing. The implementation must test message boundaries, peer credentials, disconnect detection, backpressure, and cleanup on the supported Linux environments.

The socket lives in a supervisor-owned private runtime directory (for example, under `$XDG_RUNTIME_DIR`), with restrictive directory/socket permissions. The supervisor validates peer credentials using Linux peer-credential facilities and accepts only the expected worker process identity/credentials. Do not treat possession of a socket path as sufficient authentication.

No TCP listener, remote control endpoint, or cloud dependency is introduced by this contract.

### 4.2 Framing

For a stream socket, each message is framed as a 4-byte unsigned big-endian payload length followed by exactly that many UTF-8 JSON bytes. The receiver must reject zero-length, truncated, malformed, or oversized frames before dispatch. **Provisional limit:** 1 MiB per control message; normal messages should be far smaller. The limit is configurable only through a documented protocol revision, not silently per call.

If `SOCK_SEQPACKET` is selected instead, one packet is one message and packet truncation must be detected. The JSON envelope remains the same.

### 4.3 Envelope

Every request, response, and event uses a common envelope:

```json
{
  "protocol": { "major": 1, "minor": 0 },
  "kind": "request",
  "type": "session.start",
  "request_id": "opaque-id",
  "worker_instance_id": "opaque-worker-id",
  "worker_generation": 1,
  "session_id": "opaque-session-id",
  "sent_at_unix_ms": 0,
  "body": {}
}
```

Fields:

- `protocol.major`: incompatible wire/schema generation. A major mismatch is fatal to the connection.
- `protocol.minor`: backward-compatible additions. A receiver ignores unknown optional fields; a sender must not require a newer minor field unless negotiated.
- `kind`: `request`, `response`, or `event`.
- `type`: namespaced message name.
- `request_id`: required for requests and their responses; absent for unsolicited events.
- `worker_instance_id` and `worker_generation`: required after handshake; supervisor-originated launch/handshake messages may establish them.
- `session_id`: required for session-scoped operations and reports; omitted for worker-scoped operations.
- `sent_at_unix_ms`: diagnostic timestamp only, not an ordering or timeout authority.
- `body`: message-specific object.

The supervisor and worker perform a handshake immediately after connection. The handshake exchanges supported major/minor versions, worker identity/generation, worker build/version metadata, and supported capabilities. The connection is not considered ready until the supervisor validates the identity and selects a mutually supported protocol version. Do not infer feature support from version numbers alone; negotiate named capabilities where needed.

### 4.4 Request/reply and errors

Every accepted request receives exactly one terminal response: success or structured error. A response echoes `request_id`, message type, worker identity/generation, and session identity when applicable.

Error shape:

```json
{
  "code": "SESSION_NOT_FOUND",
  "message": "The requested session is not owned by this worker.",
  "retryable": false,
  "details": {}
}
```

Error codes are stable machine-readable identifiers; messages are diagnostic and may change. Initial codes include:

- `UNSUPPORTED_VERSION`
- `UNSUPPORTED_MESSAGE`
- `INVALID_ARGUMENT`
- `UNAUTHORIZED_PEER`
- `STALE_WORKER_GENERATION`
- `SESSION_NOT_FOUND`
- `SESSION_ALREADY_EXISTS`
- `SESSION_STATE_CONFLICT`
- `RESOURCE_UNAVAILABLE`
- `BUSY`
- `SHUTTING_DOWN`
- `INTERNAL_ERROR`

Never return success before the requested state transition has actually been accepted. For asynchronous operations, distinguish “accepted/in progress” from “completed”; do not overload an acknowledgement as proof that a stream is already running.

## 5. Message families

The initial protocol is intentionally small.

| Message | Direction | Scope | Purpose |
|---|---|---|---|
| `worker.hello` | Worker → Supervisor | Worker | Establish identity, protocol, build, and capabilities |
| `worker.ready` | Worker → Supervisor | Worker | Report that initialization is complete and commands can be accepted |
| `worker.health` | Worker → Supervisor / Supervisor → Worker | Worker | Health snapshot or health request |
| `session.start` | Supervisor → Worker | Session | Request creation of a logical streaming session |
| `session.stop` | Supervisor → Worker | Session | Request orderly stop and cleanup |
| `session.status` | Supervisor → Worker | Session | Query session state and negotiated profile |
| `worker.shutdown` | Supervisor → Worker | Worker | Stop accepting work and begin process shutdown |
| `worker.event` | Worker → Supervisor | Worker/session | Report state transition or asynchronous failure |

Do not add arbitrary setters for internal Syzygy implementation details. Extend the protocol only for a concrete user-visible or operational requirement.

### 5.1 Start request

The start request describes desired behavior, not implementation choices:

```json
{
  "protocol": { "major": 1, "minor": 0 },
  "kind": "request",
  "type": "session.start",
  "request_id": "req-1",
  "worker_instance_id": "worker-1",
  "worker_generation": 1,
  "session_id": "session-1",
  "body": {
    "client": { "kind": "moonlight", "client_id": "opaque-client-id" },
    "display": { "source_id": "display-primary" },
    "video": {
      "codec": "h264",
      "width": 1920,
      "height": 1080,
      "fps_num": 60,
      "fps_den": 1
    },
    "audio": { "enabled": true },
    "input": { "enabled": true }
  }
}
```

This is illustrative, not a finalized Moonlight configuration schema. Credentials, pairing secrets, private keys, and certificate material must not be copied into routine control messages or logs. Define a separate protected provisioning mechanism if the worker requires secret material.

The worker validates the complete request before allocating resources. It returns a structured rejection without leaving partial resources, or an accepted/in-progress response followed by a state event. The final state becomes `running` only after the worker has reached the defined operational readiness point. That point must be specified against the actual inherited stream startup path.

### 5.2 Stop request

`session.stop` is idempotent. Stopping an already stopped session returns a successful terminal result. An unknown session returns `SESSION_NOT_FOUND`; it must not stop another session or the whole worker.

A successful stop response means session-owned resources have been released and the session is terminal. If cleanup fails or times out, return a failure with diagnostic details and report the remaining resource state; do not claim clean shutdown.

## 6. Lifecycle state machines

### 6.1 Worker process

The supervisor tracks:

`created → starting → handshaking → ready → draining → stopping → exited`

Failure transitions may occur from any non-terminal state:

`starting | handshaking | ready | draining | stopping → failed`

Rules:

- `ready` is entered only after a valid handshake and `worker.ready`.
- `draining` means no new sessions are accepted; existing sessions may be stopped.
- `stopping` means shutdown is in progress.
- `exited` is confirmed by OS process exit and exit status.
- `failed` is a supervisor-observed failure state, not a worker self-declaration.
- A replacement process always receives a new `worker_instance_id` and incremented generation.
- The supervisor never assumes a worker is dead solely because one health response is late; use a defined heartbeat/timeout policy and confirm process state.

### 6.2 Logical session

The supervisor tracks:

`requested → starting → running → stopping → stopped`

Failure may transition a non-terminal session to `failed`. A session can also be `recovering` when the supervisor is attempting to re-establish the same logical session after worker loss; this state is supervisor-owned and must not be confused with a worker claiming that its old resources survived.

A start request for an existing non-terminal `session_id` is not a second start. It returns the existing state or `SESSION_ALREADY_EXISTS`. A retry with the same `request_id` returns the recorded result where the deduplication record remains available. A new request ID does not make a conflicting state transition valid.

The supervisor is authoritative for desired state; the worker is authoritative for observed runtime state. The supervisor reconciles them and reports disagreement explicitly.

## 7. Idempotency, ordering, and reconnect

- The sender creates a unique `request_id` for every logical operation.
- The receiver keeps a bounded deduplication cache for the connection/worker lifetime. A repeated request ID with the same type and payload returns the original result; the same ID with a different payload is rejected.
- `session.start` is additionally idempotent by `session_id`: it must not create duplicate streams.
- `session.stop` is idempotent by `session_id`.
- Requests for one session are processed in a deterministic order. Conflicting transitions return `SESSION_STATE_CONFLICT`; they are not silently reordered.
- Responses are correlated by `request_id`, not by arrival order.
- On IPC disconnect, the supervisor marks worker control as unavailable and checks process liveness. It does not assume the worker has stopped merely because the socket closed.
- A reconnected worker must prove the same `worker_instance_id` and generation. A newly launched process cannot impersonate an earlier instance.
- Worker events include a monotonically increasing per-process `event_sequence`; duplicate or out-of-order events are ignored or recorded as protocol anomalies.
- No automatic replay of a non-idempotent operation is allowed. Only explicitly idempotent operations may be retried automatically.

## 8. Health and observability

Health is a snapshot of observed state, not a readiness promise for every future operation. The supervisor combines process liveness with worker-reported health.

Minimum worker-level fields:

- worker state and readiness;
- worker instance ID and generation;
- process uptime;
- last successful heartbeat age;
- active session IDs/count;
- build/version and negotiated protocol;
- last error/failure summary.

Minimum per-session subsystem groups:

- capture;
- encoder;
- protocol/network;
- audio;
- input.

Each subsystem reports a state such as `initializing`, `available`, `degraded`, `unavailable`, or `stopped`, plus optional metrics only when genuinely measured.

Metric rules:

- include units and sampling window in the schema/documentation;
- distinguish measured, estimated, and unavailable values;
- use `null`/unavailable rather than zero when unsupported;
- never synthesize FPS, latency, bitrate, queue depth, packet loss, or underrun counts;
- include a monotonic sample sequence/time for freshness;
- do not put high-frequency per-frame telemetry on the control channel.

The worker sends a lightweight heartbeat at a configured interval. The supervisor tracks missed heartbeats with a grace period and also checks OS process state. Exact intervals and restart thresholds are operational configuration to be measured, not hard-coded product guarantees. A healthy process may still host a degraded session; a live worker is not equivalent to a healthy stream.

Logs include `session_id`, `worker_instance_id`, `worker_generation`, and `request_id` when applicable. Secrets, pairing material, private keys, and raw media are excluded.

## 9. Shutdown, crash, and restart behavior

### 9.1 Normal shutdown

1. Supervisor changes worker state to `draining` and stops assigning new sessions.
2. Supervisor sends `worker.shutdown` with a deadline and reason.
3. Worker stops accepting new work, stops each active session, releases session resources, then process-wide resources.
4. Worker emits final state/health and acknowledges shutdown.
5. Supervisor waits for process exit and records exit status.
6. Supervisor removes its socket endpoint and runtime metadata after the process is confirmed exited.

Session cleanup order must respect actual resource dependencies. In particular, shared capture/audio/input resources are released only by their true final owner; stopping one session must not tear down resources still used by another.

### 9.2 Timeout escalation

Use bounded stages:

- **Graceful phase:** wait for worker acknowledgement and clean exit until the configured graceful deadline.
- **Terminate phase:** send SIGTERM and wait for a shorter termination grace period.
- **Kill phase:** send SIGKILL only after the previous deadline expires.
- **Reap phase:** always wait/reap the child and record the final exit status.
- **Cleanup phase:** remove stale socket paths and supervisor-owned metadata only after verifying ownership and process identity.

Deadlines are configurable and measured. Do not use an unbounded wait. Do not unlink a socket path that may belong to a newer worker generation.

### 9.3 Worker crash

On unexpected exit, the supervisor:

1. records exit code/signal and last known health;
2. marks affected sessions failed or recovering according to explicit policy;
3. cleans resources it owns;
4. invalidates the old worker generation;
5. applies bounded restart/backoff to avoid crash loops;
6. launches a new worker with a new instance ID/generation if policy permits.

Do not promise seamless stream continuation. Re-establishing a logical session requires revalidation of client state, pairing/session credentials, display availability, and negotiated profile. If recovery cannot be proven safe, fail the session clearly and require a new client connection.

### 9.4 Supervisor crash/restart

Workers must not become permanent orphans. The initial design should use OS process supervision and a private runtime directory with an ownership/instance record so a restarted supervisor can identify and reconcile only its own workers. A worker must detect supervisor control loss and enter a bounded self-shutdown path after a documented grace interval. The exact mechanism—parent-death signal, control-socket lease, or both—must be prototyped; parent-death signal alone is insufficient for all supervisor restart scenarios.

Never kill processes based only on a stale PID file. Verify process identity using a per-launch token/instance identity and OS process metadata before cleanup.

## 10. Media-interface boundaries

### 10.1 Milestone 1 invariant

No raw frames, DMA-BUF payloads, encoded video packets, audio samples, or media queues cross the supervisor control socket. The worker owns the complete inherited media path.

The supervisor may send declarative configuration and receive state/metrics. It must not receive frame callbacks or become a media scheduler.

### 10.2 Internal interfaces

Within the worker, keep conceptual boundaries explicit even if they remain in one process:

- **Capture source:** produces frames with defined timestamps, geometry, pixel format, color metadata, and ownership/lifetime.
- **Frame resource:** identifies memory domain and synchronization requirements; a CPU pointer or GPU handle is valid only while its owner guarantees lifetime.
- **Encoder session:** consumes frames under a negotiated profile and produces encoded access units with timestamps, keyframe/configuration metadata, and explicit ownership.
- **Audio session:** owns capture/format conversion/encoding state and reports sink/capture failure separately from video.
- **Protocol/network session:** owns client-specific packetization, FEC, encryption, pacing, and socket state.

Do not create empty Rust traits solely to mirror every current C++ class. Define an extraction interface only when a real replacement or independent lifecycle requires it.

### 10.3 Future extraction contract

Before any media boundary crosses a process, specify and test:

- format/capability negotiation;
- timestamp clock domain, units, monotonicity, and discontinuity handling;
- buffer ownership, reference lifetime, and exactly-once FD closure;
- GPU device identity, modifiers, synchronization fences, and import failure;
- ordering, bounded queue capacity, backpressure, and stale-frame policy;
- reconfiguration and resolution/profile changes;
- cancellation, crash, restart, and cleanup semantics;
- security/authorization for transferred handles;
- measured copies, latency, CPU/GPU use, and memory cost.

SCM_RIGHTS, DMA-BUF, memfd/shared memory, and zero-copy are future options, not Milestone 1 dependencies or performance claims. A media transport must be a separately versioned interface; it must not be smuggled into the control protocol.

## 11. Security requirements

- Control IPC is local-only and peer-authenticated.
- The supervisor creates and owns the runtime directory/socket with restrictive permissions.
- Validate all message sizes, versions, enums, numeric ranges, and state transitions.
- Reject unknown required capabilities and malformed identity combinations.
- Do not trust client-supplied session IDs or worker IDs.
- Keep secrets out of routine messages, errors, and logs.
- Do not weaken inherited Moonlight pairing, authentication, or encryption.
- Treat worker command-line arguments and environment as potentially observable; do not place secrets there.
- Worker privileges and sandboxing remain a separate least-privilege design task; the contract does not assume root.

## 12. Initial acceptance tests

These tests validate the contract before media extraction. They can be implemented with a fake worker before integrating Syzygy.

1. Handshake succeeds for supported protocol versions and rejects a major mismatch.
2. Invalid JSON, truncated frames, zero-length frames, and oversized frames are rejected without crashing either process.
3. A peer with incorrect credentials is rejected.
4. Worker instance/generation mismatch and stale events are rejected.
5. Start is correlated to its response; duplicate request IDs do not duplicate work.
6. Duplicate `session.start` does not create a second session.
7. Stop is idempotent; stopping an unknown session does not affect other sessions.
8. Conflicting start/stop operations produce explicit state-conflict errors.
9. Health distinguishes process liveness from subsystem/session degradation and never invents metrics.
10. Worker crash is detected, reaped, and recorded without crashing the supervisor.
11. Graceful shutdown completes within its deadline; escalation is bounded and testable.
12. Supervisor loss does not leave an indefinitely running unmanaged worker.
13. A stale worker cannot remove or overwrite a newer worker's socket/runtime record.
14. Multi-session tests prove stopping one session does not destroy shared resources still owned by another.
15. No media payload appears on the control channel.

The fake-worker tests establish process and protocol semantics; they do not replace Moonlight compatibility, GPU, audio, or capture tests against the real worker.

## 13. Decisions and unresolved questions

| Topic | Proposed status |
|---|---|
| Supervisor is authoritative for desired state and worker lifecycle | Accepted direction |
| Worker reports observed runtime state and owns its media resources | Accepted direction |
| Session ID is logical and independent of PID/process | Contract invariant |
| Every launch gets a new worker instance ID and generation | Contract invariant |
| One worker per capture source, potentially multiple sessions | Initial target; validate against inherited behavior |
| Local Unix-domain socket | Accepted direction |
| JSON envelope with major/minor versioning | Proposed v1 |
| 4-byte length framing and 1 MiB cap | Provisional; validate |
| SOCK_STREAM vs SOCK_SEQPACKET | Open prototype decision |
| Peer authentication via Linux peer credentials | Required; implementation to validate |
| Heartbeat interval and timeout/backoff values | Open; measure and configure |
| Exact stream-ready point for `session.start` | Open; trace inherited startup path |
| Worker self-shutdown after supervisor loss | Required behavior; mechanism open |
| Automatic session recovery after worker crash | Not promised; policy requires tests |
| Cross-process media transport / FD passing | Deferred; separate design and benchmark |

## 14. Next implementation sequence

1. Review this contract against the founding architecture and pinned source ownership audit.
2. Resolve the open semantics that affect the first fake-worker prototype: worker grouping, readiness point, socket type/framing, and supervisor-loss behavior.
3. Define language-neutral JSON schemas and golden message fixtures.
4. Build a Rust supervisor plus a small fake worker; test identity, framing, state transitions, health, crash, and shutdown.
5. Only then wrap the unmodified Syzygy worker behind the same contract.
6. Keep the media path unchanged until the supervisor can start, observe, stop, and cleanly reap a real Moonlight-compatible worker.

**Change rule:** Any change to identity, message semantics, lifecycle guarantees, or media ownership requires updating this document and its compatibility fixtures. Provisional choices must remain visibly provisional until evidence and maintainer approval resolve them.
