# Aphelion Founding Architecture

**Status:** Founding draft 0.1  
**Date:** 2026-11-22  
**Decision owner:** Solo maintainer

This document records the initial architectural direction. It is a plan, not a claim that the system has been implemented or performance-tested. Provisional choices require source audits or prototypes before becoming implementation commitments.

## 1. Purpose and relationship to Syzygy

Aphelion is an independently developed Linux game-streaming host that selectively inherits and progressively replaces parts of Syzygy. Syzygy remains an independent project with its own roadmap and releases. Aphelion is not a rename or a commitment to stop Syzygy development.

The initial objective is a reliable, maintainable host with low latency, local operation, hardware choice, user control, and compatibility with existing Moonlight clients. An Aphelion-native protocol is a later objective and must not compromise the initial compatibility path.

AI-assisted contributions are acceptable. Judge code by correctness, security, performance, maintainability, licensing, and reproducible tests—not authorship.

## 2. Founding principles

1. **Local-first:** Core streaming must not require a cloud account, subscription, vendor service, or internet connection.
2. **User ownership:** Configuration, pairing material, logs, and session control remain under user control.
3. **Open compatibility:** Preserve Moonlight compatibility while it is a required client path.
4. **Hardware freedom:** Do not assume one GPU vendor, capture backend, display server, or codec.
5. **Low latency:** Bound queues, measure stage latency, discard stale frames where appropriate, and avoid unnecessary copies.
6. **Progressive replacement:** Keep working components until replacements demonstrate equivalent behavior and pass tests.
7. **Explicit ownership:** Every subsystem has an identifiable owner for state, resources, lifecycle, and failure handling, even when several subsystems share a process.
8. **Evidence over assumptions:** Unknown performance, compatibility, and hardware limits remain unknown until measured.
9. **Maintainable scope:** Build only what a concrete milestone requires.

## 3. Milestone 1 architecture: Thick Worker

### Process model

Milestone 1 uses a Rust supervisor and a Thick Worker that retains the complete existing Syzygy streaming path.

- **Aphelion supervisor (Rust):** Configuration, desired host state, worker lifecycle, observed client-session state, health polling, process monitoring, and local control API.
- **Thick Worker (existing Syzygy C++ implementation):** Capture, encoding, audio, input, Moonlight protocol handling, packetization, FEC, encryption, and network transmission.
- **Control IPC:** Local Unix-domain socket carrying versioned control messages and health responses.
- **Media path:** Remains inside the worker. No video, audio, encoded bitstream, or frame-buffer payload crosses the control socket in Milestone 1.

This is a compatibility-preserving boundary. Syzygy's current video broadcast path couples encoded packet handling with codec parameter-set handling, FEC, RTP/Moonlight headers, per-session encryption, and batched network transmission. Splitting there immediately would force a protocol/network rewrite. The Thick Worker avoids that premature split.

The supervisor must not depend on Syzygy C++ structures or internal implementation details. Its contract describes desired host behavior and observable client-session/health state, not the worker's internal topology. The worker IPC lifecycle is host-scoped; Moonlight client-session lifecycle is initiated by client protocol activity and reported as worker-originated events. A host start request must not masquerade as a client session start.

### Worker granularity and initial profile policy

The intended ownership boundary is one worker per capture source. This is a target to validate, not proof that all capture backends can share a source or that current Syzygy already supports it.

Do not assume one active encoding profile per capture source. The initial Syzygy source audit shows shared capture with separate per-session encoding contexts, but does not yet establish which combinations of resolutions, frame rates, codecs, and quality settings are supported concurrently. Milestone 1 must preserve verified existing Syzygy behavior. Trace and test profile combinations before narrowing support. Do not introduce generalized shared-encoder bitstream fan-out in Milestone 1; keep per-session encoder contexts where that is how the inherited path works. Profile compatibility must be explicit, and materially different profiles must not be silently treated as equivalent.

### Failure isolation

The supervisor owns worker lifecycle. It detects worker exit, reports failure, cleans resources it owns, and applies bounded restart/backoff. A worker crash must not crash the supervisor. A supervisor restart must not leave unmanaged workers or stale socket endpoints. Seamless recovery and keyframe timing are not promised until tested.

## 4. Contracts established in Milestone 1

### 4.1 Process-agnostic, versioned control API

The API describes **what the user wants**, not which internal process performs it. It must not expose Syzygy class names, C++ layouts, implementation-specific encoder controls, or assumptions that capture, encoding, and networking always share one process.

Every request and response includes an API version and request identifier. Unsupported versions and invalid requests fail clearly. Define idempotency and duplicate-request behavior before relying on retries.

Illustrative shape of a possible future user-facing desired-session API, not the current worker IPC schema:

```json
{
  "api_version": 1,
  "request_id": "request-identifier",
  "command": "start_session",
  "session_id": "logical-session-identifier",
  "stream": {
    "display": "requested-display",
    "video": {
      "codec": "h264",
      "width": 1920,
      "height": 1080,
      "fps": 60
    }
  }
}
```

The current worker IPC command families are:
- `host.start` — start the Moonlight-compatible host process; idempotent while running.
- `host.stop` — stop that host process; idempotent while stopped.
- `worker.health` and `worker.shutdown` — report lifecycle/observed health and terminate the worker.

A future local user-facing API may expose desired streaming configuration, but it must not claim to start or stop an individual Moonlight client unless the worker has a tested control bridge for that operation. Moonlight client sessions are created and ended by protocol activity; the worker reports observed `session.started` / `session.stopped` events with IDs scoped to the worker instance and generation where correlation is safe. Add commands only for concrete lifecycle or operational needs. Internal topology changes should not require an external API redesign.

### 4.2 Subsystem-mirrored health

Health responses identify capture, encoder, network/protocol, audio, input, and worker lifecycle separately, even while they execute inside one worker.

Illustrative shape:

```json
{
  "api_version": 1,
  "session_id": "logical-session-identifier",
  "worker": {"state": "running", "uptime_ms": 120000},
  "subsystems": {
    "capture": {"state": "available", "fps": null, "last_frame_age_ms": null},
    "encoder": {"state": "available", "fps": null, "avg_encode_ms": null, "queue_depth": null},
    "network": {"state": "available", "bitrate_kbps": null, "rtt_ms": null, "lost_packets": null},
    "audio": {"state": "available", "sample_rate": null, "buffer_underruns": null},
    "input": {"state": "available"}
  }
}
```

This is a shape example, not a claim that Syzygy currently exposes these measurements. Report only metrics actually measured with defined semantics. Use null or an explicit unavailable state when unsupported. Never fabricate values or infer zero from missing data. Document units, sampling window, source, and whether each value is measured or estimated.

### 4.3 IPC capability and media-plane invariant

The control socket is control-plane only in Milestone 1. Media payloads never travel over it.

Keep future file-descriptor transfer, including DMA-BUF sharing through SCM_RIGHTS, possible in the IPC design. However, do not assume SOCK_SEQPACKET is a trivial or universally supported library substitution. Prototype Rust/Linux support, framing, peer authentication, FD passing, backpressure, and failure behavior before selecting the socket type.

Choose the simplest reliable control transport that meets tested requirements. Any future media-plane transport is a separate interface requiring its own design and benchmarks. Do not add shared memory, memfd, DMA-BUF transfer, or zero-copy complexity to Milestone 1 merely for theoretical future-proofing.

### 4.4 Session identifiers

session_id is the stable, user-facing logical session identifier. It does not mean one process or one encoder.

Internal capture_id, encoder_id, and gateway_id may be introduced when those resources actually exist as independently managed objects. Their future relationships must permit one-to-many and many-to-one mappings, but Milestone 1 must not create empty abstractions merely to imitate a future topology.

Define identifier scope, uniqueness, lifetime, cleanup, and log correlation before multi-client resource sharing.

### 4.5 Subsystem ownership map

Even inside the Thick Worker, document ownership for:
- capture source and display resources;
- raw frame and GPU-buffer lifetime;
- encoder context and profile;
- encoded packet lifetime;
- per-client protocol, encryption, FEC, pacing, and network state;
- audio resources;
- input devices and routing;
- worker lifecycle and cleanup.

Derive this map from source inspection and tests, not from an architecture diagram alone.

## 5. Language and concurrency

Rust is preferred for new control-plane and independently testable infrastructure where it provides a clear benefit. Existing C/C++ may remain where it is the correct boundary, including inherited Syzygy and native multimedia/GPU integration. Do not rewrite solely for language uniformity.

Use narrow, documented FFI boundaries when justified. Do not create Rust traits that mechanically mirror current C++ internals before extraction requirements are known.

Tokio is suitable for supervisor control-plane work and may suit a future gateway. Real-time media work should use dedicated synchronous threads or the execution model required by its media libraries. Do not run blocking capture/encode work on general async executor threads.

## 6. Compatibility, security, and licensing

- Preserve Moonlight compatibility as a first-class initial requirement.
- Treat pairing, certificates, authentication, encryption, protocol parsing, and input handling as security-sensitive. Preserve known-good behavior until replacements pass compatibility and security tests.
- Do not weaken authentication or encryption to simplify process boundaries.
- Define trust boundaries and least-privilege requirements before worker sandboxing or privileged capture operations.
- Audit Syzygy licensing, notices, third-party dependencies, and upstream obligations before copying, modifying, or distributing code. This document does not determine those obligations.
- Do not add cloud dependencies or telemetry uploads by default. Remote features must be opt-in and documented.

## 7. Thick Worker extraction gates

The Thick Worker is transitional, not a declared end state. Extraction reviews become mandatory when:

1. **Native protocol work begins:** Review separating the Moonlight adapter so legacy compatibility and the new protocol can evolve independently.
2. **A second production capture backend is proposed:** Define and test a capture interface before backend-specific logic spreads through the worker.
3. **Encoder resources need independent lifecycle or sharing:** Extract only after profile ownership, resource limits, and GPU-buffer lifetime are demonstrated.
4. **Network lifecycle needs independent isolation:** Prototype the bitstream boundary, timestamps, ordering, backpressure, and failure semantics before extraction.
5. **Operational isolation is required:** Reassess process boundaries when a subsystem's crashes, privileges, or resource contention demonstrably harm unrelated sessions.

These are mandatory architecture reviews, not automatic rewrite triggers. Each review records evidence, alternatives, compatibility impact, resource costs, test plan, rollback plan, and maintainer decision. Arbitrary client-count thresholds must not be the sole reason to extract a subsystem.

## 8. Roadmap

### Milestone 0 — Audit and design validation
- Inspect Syzygy source and map exact integration points.
- Map state and resource ownership across capture, encoder, audio, input, encryption, FEC, packetization, and network.
- Audit licensing and third-party obligations.
- Inventory capture, encoder, audio, and display paths without assuming one backend is universal.
- Establish a reproducible Moonlight compatibility baseline.
- Prototype Rust-to-worker IPC, socket lifecycle, and peer authentication.
- Validate SOCK_SEQPACKET and SCM_RIGHTS requirements rather than assuming them.
- Record baseline tests and measurements before changing the media path.

**Exit:** Source map, license audit, control-contract draft, and test baseline exist.

### Milestone 1 — Thick Worker supervisor
- Build a Rust supervisor that launches and monitors the existing worker.
- Implement versioned, process-agnostic host.start, host.stop, worker.health, and worker.shutdown IPC. Treat Moonlight client sessions as worker-observed lifecycle events until a tested per-client control bridge exists.
- Keep all media and Moonlight protocol work inside the worker.
- Add subsystem health with honest unavailable values.
- Implement bounded restart/backoff, clean shutdown, socket cleanup, and useful logs.
- Test worker crashes, supervisor restarts, malformed messages, duplicate requests, and resource cleanup.

**Exit:** The supervisor starts and stops the real host process; a Moonlight client can establish and end a real stream through the existing protocol path; health distinguishes host lifecycle from observed client sessions; failures are contained; Moonlight compatibility is not regressed. Do not claim that `host.stop` stops one client independently.

### Milestone 2 — Reliability and observability
- Improve metrics based on measurements available from the implementation.
- Add bounded queues and stale-frame handling where evidence justifies them.
- Define and test client/profile compatibility behavior.
- Add recovery and diagnostics without promising seamless behavior until verified.
- Expand hardware testing across available capture and encoder paths.

**Exit:** Reproducible reliability and latency measurements exist, and failure modes are documented.

### Milestone 3 — First justified extraction
- Select one subsystem only after an extraction gate is met.
- Specify ownership transfer, buffer lifetime, timestamps, ordering, backpressure, errors, and restart behavior.
- Prototype against real hardware and Moonlight clients.
- Compare latency, CPU/GPU use, copies, and reliability against the Thick Worker baseline.
- Retain the old path until the new path passes equivalent tests.

The first isolated Rust media-plane prototype is the `aphelion-streamer` crate: it packetizes encoded H.264 NAL units as standard RTP and sends UDP datagrams. This is an interface and transport experiment only. It is not the Moonlight wire protocol and does not replace the Syzygy path; no capture, encoder, FEC, encryption, pacing, or client interoperability is claimed. See `docs/RUST-STREAMER-PROTOTYPE.md` for scope and next extraction gates.

**Exit:** The extracted path demonstrates measurable value, compatibility where required, and acceptable operational complexity.

Later candidates include multi-profile encoding, independent gateways, additional capture backends, native protocol support, improved audio, HDR/color, virtual displays, application launch, gamepad/rumble/haptics, and worker sandboxing. None is a Milestone 1 commitment.

## 9. Engineering practices

- Keep changes small and independently testable.
- Add regression tests that fail before a fix and pass after it wherever practical.
- Test success, failure, and resource cleanup paths.
- Preserve a working Moonlight compatibility test path.
- Record hardware, driver, kernel, capture backend, encoder, codec, resolution, frame rate, and client version for performance results.
- Never present estimates or synthetic benchmarks as real-world measurements.
- The solo maintainer explicitly approves merges, publication, tags, and releases.
- Use automated checks; do not create processes that assume a second human reviewer exists.

## 10. Open questions

1. Which Syzygy APIs can be wrapped without changing streaming behavior?
2. Which session state is global, per capture source, per encoder, and per client?
3. Which GPU buffer paths are actually zero-copy on supported hardware and drivers?
4. What are real concurrent encoder limits for each supported GPU and driver? Do not hardcode an unverified GTX 1060/Pascal limit.
5. Which capture backends are practical across X11, Wayland, DRM/KMS, and headless operation?
6. What PipeWire integration works across supported desktop/session environments?
7. Which socket type and Rust library provide the simplest tested control IPC with required security and future FD-passing capability?
8. What latency and reliability costs arise from each future process boundary?
9. Which Moonlight client behaviors and protocol variants must be preserved?

## 11. Decision record

| Decision | Status |
|---|---|
| Aphelion is independent from Syzygy; inheritance is selective and progressive | Accepted |
| Moonlight compatibility remains an initial requirement | Accepted |
| Rust-first for new control infrastructure; C/C++ retained where justified | Accepted |
| Milestone 1 uses Rust supervisor and complete Syzygy Thick Worker | Accepted direction |
| No media payload crosses Milestone 1 control IPC | Invariant |
| Control schema is versioned and process-agnostic | Invariant |
| Health mirrors subsystem boundaries and reports only real measurements | Invariant |
| session_id identifies a logical client session, not a worker or host process | Invariant |
| Worker IPC host lifecycle is distinct from Moonlight client-session lifecycle | Accepted based on the pinned Syzygy source audit; the initial illustrative per-session worker commands were not supported by Syzygy's current interface |
| A per-client start/stop command requires a tested control bridge; until then client sessions are protocol-driven and observed as events | Invariant |
| SOCK_SEQPACKET is the selected control transport | Provisional; prototype first |
| IPC supports future SCM_RIGHTS if needed | Design requirement to validate |
| Preserve verified shared-capture and per-session encoder behavior; defer generalized shared-encoder fan-out | Audit-led initial direction |
| Gamescope capture or custom PipeWire audio is the universal path | Not accepted; research required |
| Thick Worker extraction follows documented architecture gates | Accepted |

The host/client lifecycle split updates the original illustrative `start_session` / `stop_session` worker commands. Evidence: the pinned Syzygy source audit at `c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d` found Moonlight protocol handlers create client sessions and no existing control socket accepts per-client start/stop commands. The current adapter can own the host process and observe session log activity, but has no verified mapping for the initial stream-intent fields. This changes the Milestone 1 worker IPC boundary; it preserves Moonlight pairing and streaming behavior and leaves any separate local desired-session API provisional until its semantics can be exercised against the worker.

**Document rule:** Update this document when a decision changes. Record why it changed, what evidence supports it, and which compatibility or migration assumptions are affected. Do not silently turn provisional choices into permanent requirements.
