# Rust streamer prototype

**Status:** Capture and software-encoding adapters, a conservative RTSP lifecycle state machine, a Moonlight RTP/NV_VIDEO_PACKET data-shard packetizer, standard RTP experiment, generic Reed-Solomon FEC, and a Moonlight video AES-GCM envelope. No complete Moonlight session or client compatibility claim.

The `aphelion-streamer` crate accepts an already encoded H.264 access unit as separate NAL units, packetizes single NALs and large NALs (FU-A), assigns RTP sequence/timestamp/SSRC fields, and sends the resulting datagrams to a configured UDP destination. RTP packetization follows the general RTP header and H.264 payload conventions in [RFC 3550](https://www.rfc-editor.org/rfc/rfc3550.html) and [RFC 6184](https://www.rfc-editor.org/rfc/rfc6184.html). The caller supplies a 90 kHz media timestamp; the crate does not infer media timing.

`PipeWireCapture` now reads fixed-size BGRA frames from a caller-selected PipeWire node through GStreamer. The node must come from an authorized desktop portal session; this crate does not request permission or enumerate displays. `FfmpegH264Encoder` feeds those frames to a separate FFmpeg `libx264` process and parses Annex-B access units using AUD boundaries. The encoder uses a bounded two-unit output channel and keeps capture timestamps. These are synchronous media-thread adapters, not yet connected to the supervisor worker lifecycle or user-facing display chooser. FFmpeg/GStreamer binaries and codec plugins are runtime dependencies and are not bundled.

The bounded RTSP decoder now feeds a conservative session state machine with ANNOUNCE, SETUP, PLAY, GET_PARAMETER, and TEARDOWN ordering, CSeq correlation, exact duplicate replay, and duplicate-conflict rejection. It does not authenticate or pair clients, parse SDP capabilities, negotiate Moonlight key material, associate UDP ping ports, or prove Moonlight interoperability. `MoonlightVideoPacketizer` emits the packed RTP extension, NV_VIDEO_PACKET metadata, short frame header, and encrypted data-shard envelope when supplied a negotiated cipher; `MoonlightUdpSender` transmits those datagrams to a caller-provided peer. It currently sets FEC percentage to zero and emits no parity. Frames that exceed one 255-data-shard block are rejected unless the caller explicitly supplies negotiated multi-FEC capability. Generic Reed-Solomon shards exist separately; matching Moonlight's FEC block prefix and parity metadata remains open. The AES-GCM primitive does not derive session keys. UDP send success means only that the local socket accepted a datagram. No media passes over supervisor control IPC, and the existing Syzygy-backed stream remains unchanged.

Capture currently copies each BGRA frame through a process pipe, and software encoding copies it again into FFmpeg. There is no DMA-BUF import, hardware encoder backend, audio or input path, bounded capture queue, stale-frame drop policy, pacing, or measured end-to-end latency. The sender remains a separate standard-RTP experiment and is not wired to the new capture/encoder/session modules.

## Verification

GitHub Actions installs FFmpeg, builds/tests the workspace, and exercises an actual two-frame `libx264` encode and final flush. Unit coverage checks RTP headers, marker placement, sequence rollover, FU-A reassembly, malformed NAL inputs, UDP delivery to local test sockets, bounded incremental RTSP parsing, RTSP state ordering/replay, capture limits, Annex-B access-unit boundaries, Moonlight data-shard metadata and sequence/block boundaries, Reed-Solomon recovery, AES-GCM known-answer output, nonce uniqueness, and authentication failure on tampering. These tests establish code-level boundaries only; they do not establish portal operation, complete Moonlight parity compatibility, pairing/key derivation, or client interoperability.

## Next extraction work

1. Add authenticated pairing, key derivation, Moonlight SDP/capability parsing, and UDP stream association to the RTSP state machine against reviewed protocol traces.
2. Implement Moonlight's FEC prefix/parity block layout and differential golden vectors; the new video packetizer currently emits no parity.
3. Connect portal session acquisition and capture lifecycle to the worker; validate PipeWire/GStreamer format negotiation and robust cancellation.
4. Integrate capture, encoding, Moonlight packetization, and UDP with a bounded stale-frame policy; then add capability-gated hardware encoders and explicit DMA-BUF ownership where supported.
5. Wire authenticated session, encoded frames, Moonlight packetization, FEC, encryption, pacing, UDP, and measured lifecycle/health as a single isolated Rust path; preserve the Syzygy fallback until real Moonlight and hardware comparisons pass.

The current prototype must not be used as a Moonlight host. The next usable milestone requires real encoded input, the protocol compatibility layer, and a real-client verification run.
