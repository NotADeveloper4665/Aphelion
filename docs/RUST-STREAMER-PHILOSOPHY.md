# Rust Streamer Design Philosophy

Aphelion is meant to become its own Rust streaming host through measured replacement, not by renaming a Syzygy launcher or by rewriting every subsystem before the replacement works. The existing Syzygy stream is the compatibility baseline and safety net while Rust stages are built.

## Preserve what works while changing what we can prove

Each Rust component replaces one named boundary. Keep the old path runnable until the new path passes the same protocol and runtime checks. A failed prototype must be easy to disable without losing Syzygy's capture, encoding, audio, input, pairing, or Moonlight behavior.

## Keep the architecture process-agnostic

The supervisor describes desired behavior and owns lifecycle state. It does not know FFmpeg flags, PipeWire object IDs, NVENC handles, C++ structs, or packet queue internals. Those belong behind worker-owned interfaces. The control socket is for control and health; media uses an explicitly designed data path.

## Keep ownership close to the resource

Capture owns display authorization and frame references. Encoders own codec sessions and their GPU objects. A client session owns negotiated ports, keys, sequence counters, FEC state, and pacing state. Shutdown follows the same ownership graph in reverse. Shared resources are shared only when source inspection and tests demonstrate safe lifetimes.

## Compatibility and security precede convenience

Pairing, certificate identity, control-channel association, encryption, FEC, and frame metadata are protocol behavior. Never bypass authentication, reuse nonces, assume a packet is delivered because `send_to` succeeded, or mark a profile supported because an encoder accepts its parameters. Reject unsupported requests with an observable reason.

## Measure before tuning

Bound memory and queue age; expose dropped-frame and packet counts; measure stage latency with a defined clock. Tune queue sizes, pacing, FEC percentage, and stale-frame thresholds from repeatable hardware/client measurements. Control IPC timings say nothing about end-to-end stream latency.

## Use the right execution model

Tokio is appropriate for supervision, control requests, and session orchestration. Capture callbacks, encoder submission, and packet scheduling need bounded work and suitable dedicated execution contexts. Do not block a general async executor with GPU or capture operations, and do not put logging, allocation, or unbounded waits in real-time callbacks.

## Keep the media contract narrow

The encoded access unit is the first intended Rust boundary: codec/profile, timestamp, frame identity, keyframe state, and owned NAL data. No generic abstraction is added until at least one real backend uses it. DMA-BUF sharing, shared-encoder fan-out, separate gateway processes, and a new client protocol are future decisions gated by hardware evidence.

## Be truthful about maturity

An RTP packetizer is not a Moonlight host. A successful encoder initialization is not proof of a stream. CI can verify parsers, cryptographic vectors, packet format, failure cleanup, and deterministic test transports; real display permissions, hardware encoding, Moonlight pairing, decoder behavior, latency, and recovery require target-host testing. Documentation and health output must make those limits visible.

## Respect project and license boundaries

Syzygy remains a separate project and its source is not changed by Aphelion work. Reuse only after tracing exact provenance and license obligations. Keep notices and dependency inventory current. Independently implementing protocol behavior from specifications does not authorize copying source code or distributing inherited dependencies without their required terms.
