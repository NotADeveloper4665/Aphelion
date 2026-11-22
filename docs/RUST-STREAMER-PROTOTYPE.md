# Rust streamer prototype

**Status:** Standard RTP experiment plus bounded RTSP framing, generic Reed-Solomon FEC, and a Moonlight video AES-GCM envelope primitive. No complete Moonlight session or client compatibility claim.

The `aphelion-streamer` crate accepts an already encoded H.264 access unit as separate NAL units, packetizes single NALs and large NALs (FU-A), assigns RTP sequence/timestamp/SSRC fields, and sends the resulting datagrams to a configured UDP destination. RTP packetization follows the general RTP header and H.264 payload conventions in [RFC 3550](https://www.rfc-editor.org/rfc/rfc3550.html) and [RFC 6184](https://www.rfc-editor.org/rfc/rfc6184.html). The caller supplies a 90 kHz media timestamp; the crate does not infer media timing.

This is a real, testable Rust packet-and-send path, but not a complete streaming host. It does not capture frames, encode video, pair/authenticate a client, negotiate a Moonlight stream, emit the full Moonlight video packet/FEC metadata, pace traffic, or measure client delivery. The RTSP parser is only a bounded framing primitive. The FEC module creates generic Reed-Solomon shards; the AES-GCM module creates the video-shard encryption envelope but does not derive session keys. UDP send success means only that the local socket accepted a datagram. No media passes over the supervisor's control socket, and the existing Syzygy-backed stream remains unchanged.

The implementation intentionally starts after the encoder boundary. It consumes NAL units without Annex-B start codes and does not take ownership of capture or GPU buffers. Access-unit ordering and timestamps belong to the upstream encoder adapter. The sender currently has no queue or stale-frame policy: there is not yet an attached real-time producer from which to measure queue age or justify a drop policy.

## Verification

GitHub Actions builds and tests the new crate through `cargo build --workspace` and `cargo test --workspace`. Unit coverage checks RTP headers, marker placement, sequence rollover, FU-A reassembly, malformed NAL inputs, UDP delivery to a local test socket, bounded incremental RTSP parsing, Reed-Solomon recovery, AES-GCM known-answer output, nonce uniqueness, and authentication failure on tampering. These tests establish primitives only; they do not establish Moonlight session, key-derivation, or client compatibility.

## Next extraction work

1. Implement the authenticated Moonlight RTSP session state machine and pairing/key derivation against reviewed protocol vectors.
2. Add the exact Moonlight frame/shard metadata around the tested FEC and encryption primitives, with golden packet vectors.
3. Implement a permissioned PipeWire capture backend and a software H.264 encoder, then add hardware backends behind capability checks.
4. Integrate bounded capture/encoded queues, measured timestamps, pacing and lifecycle reporting; preserve the Syzygy fallback until real Moonlight and hardware comparisons pass.

The current prototype must not be used as a Moonlight host. The next usable milestone requires real encoded input, the protocol compatibility layer, and a real-client verification run.
