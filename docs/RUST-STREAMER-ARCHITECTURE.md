# Aphelion Rust Streamer Architecture

**Status:** Implementation architecture for the progressive Rust media-path replacement. The current `aphelion-streamer` crate implements only standard H.264/RTP packetization and UDP transmission. This document is the target design, not a claim that the target is complete.

## Goals and invariants

1. Keep Moonlight compatibility as the first production client path while allowing an Aphelion-native protocol later.
2. Keep the Rust supervisor as owner of desired host state, worker lifecycle, session identity, and recovery policy.
3. Keep all video/audio media outside the supervisor's JSON control socket. The control socket carries commands and measured health only.
4. Preserve Syzygy's shared display capture and per-session encoder behavior where verified. Never silently collapse distinct client profiles.
5. Make resource ownership, timestamps, ordering, queue bounds, buffer lifetime, and shutdown explicit at every new boundary.
6. Retain the Syzygy worker as the default/fallback until the Rust path passes compatibility and hardware gates.
7. Do not copy Syzygy or submodule code into Aphelion until provenance, license, notices, and distribution obligations are resolved. Prefer independently written implementations from protocol specifications and public test vectors.

## Target topology

Initially, the Rust media pipeline runs inside a dedicated Rust streaming worker process launched and monitored by the Rust supervisor. The first implementation may keep capture, encoder, RTSP/session logic, and packet sender in one worker to avoid moving GPU buffers across processes. Internal Rust modules have explicit interfaces; separate processes are introduced only when measurements justify them.

| Layer | Responsibility | Ownership / contract |
|---|---|---|
| Supervisor | Config, desired host state, worker generation, restart/backoff, health and lifecycle | Never receives media bytes; supervises either Syzygy or Rust worker |
| Capture | Obtain desktop frames through user-authorized backend; attach monotonic capture timestamps and display identity | Shared per capture source; owns portal session, PipeWire stream, queues, and frame/buffer references |
| Encoder | Convert/submit frames and produce encoded access units for each supported profile | Initially per-session encoder contexts; reports codec, dimensions, frame type, timestamp, and ownership |
| Session setup | Pair/authenticate client, negotiate stream parameters/ports, associate RTSP and control channels | Per Moonlight client; pairing keys and session secrets never enter normal logs |
| Frame adaptation | Map encoded access units to Moonlight frame headers and packet metadata | Per-session sequence/frame counters; stable frame ordering |
| FEC | Split frame packet data into bounded blocks, produce parity shards, and describe block/shard indices | Per frame/session; bounded shard counts and explicit parity policy |
| Encryption | Authenticate/encrypt each required packet with unique per-session IV/nonce state | Per session and stream; no nonce reuse; fail closed on authentication/crypto errors |
| Pacer/network | Respect per-session pacing, datagram limits, fairness, and teardown | Bounded queues; observable queue age/bytes; never let one client stall all others |

The current standard RTP experiment is a separate opt-in target for packetization research. It is not the production Moonlight sender and must not be placed behind a compatibility claim.

## Capture and frame ownership

The first Linux desktop backend is a portal-authorized PipeWire screencast. The user selects or authorizes a source through the desktop portal. Aphelion must not bypass that permission flow by opening an arbitrary monitor node. Capture callbacks do minimal real-time work: acquire the PipeWire buffer, attach monotonic timestamp/format metadata, enqueue a bounded frame reference, and return the buffer promptly.

Capture output must describe pixel format, width, height, plane offsets/strides, DMA-BUF or mapped-memory handles, modifier, synchronization state, timestamp clock, and release callback. A DMA-BUF FD alone does not define safe GPU import or lifetime. The first encoder adapter may use CPU-mappable frames; GPU interop is a separate measured implementation with explicit fences and destruction ordering.

One capture source may fan out to multiple session encoders only while retained references keep the underlying frame alive. Each session's encoder owns its own profile and state until concurrent profile support is measured. Queue capacity and stale-frame behavior are configurable and reported; old frames may be discarded only at a documented boundary and never while an encoder still owns them.

## Encoder contract

`EncodedAccessUnit` is the boundary between encoding and network adaptation. It contains:

- codec/profile, width, height, and frame rate;
- one or more codec NAL units without container start codes;
- capture and encode timestamps with documented clock domains;
- keyframe/reference-invalidation metadata;
- a session/profile identity and monotonically increasing frame number;
- ownership that remains valid until packetization/encryption/FEC finishes.

The first backend must be selectable rather than assumed: software H.264 is the baseline; VA-API and NVENC are optional backends chosen only after capability discovery. Encoder initialization must fail clearly when the requested codec/profile is unavailable. Do not report zero-copy unless buffer import, conversion, encode, and release have all been measured on the target hardware.

## Moonlight session setup

Session setup is a security-sensitive state machine, not a generic RTSP listener. The eventual Rust implementation must cover the existing host's discovery and pairing/authentication flow, certificate identity and persistence, client capability negotiation, RTSP CSeq/session validation, launch/session state, UDP port negotiation, control-channel association, ping/loss feedback, and clean disconnect. It must keep existing pairing and crypto requirements; it must not weaken them to simplify streaming.

Implement in independently testable stages:

1. Bounded incremental RTSP framing and strict request parsing.
2. Deterministic request/response state transitions with CSeq, session, and supported-option checks.
3. Pairing/certificate integration and key derivation with compatibility vectors from the maintained protocol implementation.
4. Client negotiation with an explicit supported profile matrix; reject unsupported combinations rather than silently coercing them.
5. UDP stream endpoint setup and association with the authenticated RTSP launch session.

Before the complete pairing and negotiation path passes vectors and a real client, the Syzygy-backed host remains the usable path.

## Moonlight video packet path

The inspected inherited sender combines these operations in order: codec parameter replacement; 8-byte short-frame metadata; per-packet Moonlight/RTP metadata; FEC block planning; Reed-Solomon parity; per-client AES-GCM packet encryption; pacing; batched UDP sends. The Rust replacement must define each field and its byte order from protocol behavior and compatibility vectors. It must not assume that standard RTP alone is sufficient.

For each access unit, the sender must preserve one frame timestamp, packet order, start/end flags, frame index, data/parity shard indices, block count, and final-payload length semantics. FEC must be bounded by protocol shard/block limits. Encryption must use a fresh nonce per encrypted packet under each session key and authenticate exactly the bytes required by the negotiated protocol. FEC/encryption ordering and parity coverage must match the selected Moonlight protocol version.

## Audio and input

Video is the first Rust media path. Audio and input remain on Syzygy until their global/per-session ownership, permission, device, timing, and teardown semantics have separate designs and test coverage. The Rust worker may initially delegate them to the existing Syzygy process; it must not claim those subsystems are Rust-managed.

## Observability and recovery

Health reports capture state/last-frame age, encoder state/encode duration, queue age/bytes/drops, packet/FEC counts, encryption failures, send errors, and per-session negotiated profile only when each field is measured. Unknown values are null or explicitly unavailable. Control IPC RTT is never labeled stream latency.

On worker or device failure, stop admitting work, release queued frames and GPU resources in dependency order, close session sockets, report the failed stage, and let the supervisor apply bounded restart policy. Seamless client recovery, keyframe recovery time, and stale-frame thresholds remain unclaimed until measured.

## Verification and rollout gates

1. CI: parser fuzz/property vectors, malformed/oversized/truncated RTSP, state-machine negatives, packet golden vectors, crypto known-answer and nonce-reuse tests, FEC encode/recovery vectors, queue overflow/teardown, and worker-crash cleanup.
2. Differential compatibility: compare supported session traces and emitted datagrams against the pinned Syzygy/Moonlight test vectors without copying implementation code.
3. Hardware: record GPU, driver, kernel, compositor, portal, PipeWire, encoder, codec, resolution, FPS, and Moonlight client version; verify capture authorization and encoder operation.
4. Runtime: pair a real Moonlight client, connect/disconnect repeatedly, test input/audio when implemented, exercise worker/device failures, and check that no host process remains after stop.
5. Performance: compare capture-to-encode, encode, queue, packetize/FEC/encrypt, and network timings against the Syzygy baseline under the same profile.
6. Rollout: keep the Syzygy path available behind an explicit backend selection until compatibility, reliability, licensing, and performance gates pass. Do not merge or publish a replacement claim from CI-only evidence.

## Immediate implementation sequence

1. Architecture and philosophy documents; bounded RTSP parser; generic Reed-Solomon block encoder; video AES-GCM envelope primitive.
2. Portal-authorized PipeWire/GStreamer BGRA capture and low-latency software H.264 encoding adapters, with explicit timestamps and bounded encoder output.
3. Conservative RTSP session ordering, CSeq correlation, and duplicate replay; pairing, authentication, SDP capability parsing, and key derivation remain open.
4. Moonlight RTP/NV_VIDEO_PACKET data-shard framing and UDP send are implemented in no-parity mode; next add Moonlight's FEC prefix/parity layout with differential golden vectors.
5. Complete authenticated session and negotiated UDP stream association; then join capture, encoding, packetization, FEC, encryption, and socket lifecycle in the Rust worker.
6. Add hardware encoders and zero-copy buffers only after capability and ownership tests; run hardware/client acceptance before defaulting to the Rust path.
