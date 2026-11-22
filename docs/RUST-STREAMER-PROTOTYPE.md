# Rust streamer prototype

**Status:** First media-plane component implemented; experimental standard RTP over UDP. No Moonlight compatibility claim.

The `aphelion-streamer` crate accepts an already encoded H.264 access unit as separate NAL units, packetizes single NALs and large NALs (FU-A), assigns RTP sequence/timestamp/SSRC fields, and sends the resulting datagrams to a configured UDP destination. RTP packetization follows the general RTP header and H.264 payload conventions in [RFC 3550](https://www.rfc-editor.org/rfc/rfc3550.html) and [RFC 6184](https://www.rfc-editor.org/rfc/rfc6184.html). The caller supplies a 90 kHz media timestamp; the crate does not infer media timing.

This is a real, testable Rust packet-and-send path, but not a complete streaming host. It does not capture frames, encode video, negotiate a client, implement RTSP, use Moonlight's protocol-specific packet format, encrypt packets, add FEC, pace traffic, or measure client delivery. UDP send success means only that the local socket accepted a datagram. No media passes over the supervisor's control socket, and the existing Syzygy-backed stream remains unchanged.

The implementation intentionally starts after the encoder boundary. It consumes NAL units without Annex-B start codes and does not take ownership of capture or GPU buffers. Access-unit ordering and timestamps belong to the upstream encoder adapter. The sender currently has no queue or stale-frame policy: there is not yet an attached real-time producer from which to measure queue age or justify a drop policy.

## Verification

GitHub Actions builds and tests the new crate through `cargo build --workspace` and `cargo test --workspace`. Unit coverage checks RTP headers, marker placement, sequence rollover, FU-A reassembly and size bounds, malformed NAL inputs, and UDP delivery to a local test socket. These tests establish packet formatting and local transmission only; they do not establish decoding or network/client compatibility.

## Next extraction work

1. Add an encoder-output adapter with explicit ownership and timestamp rules, initially using test vectors rather than changing Syzygy.
2. Define the Moonlight compatibility layer from inspected Syzygy behavior: discovery/RTSP, packet headers, AES-GCM, FEC, pacing, and per-client state. Keep standard RTP behind a separate experimental target.
3. Add bounded queues and stale-frame handling only alongside a live producer and measured frame deadlines.
4. Add capture/encoder integrations one backend at a time, preserving the Syzygy fallback until measured comparison and real Moonlight tests pass.

The current prototype must not be used as a Moonlight host. The next usable milestone requires real encoded input, the protocol compatibility layer, and a real-client verification run.
