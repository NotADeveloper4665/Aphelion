# Syzygy Inheritance Audit — Initial Source Findings

**Status:** Initial source audit; not a complete security, licensing, or hardware audit  
**Audited repository:** NotADeveloper4665/Syzygy  
**Audited branch:** main  
**Audit date:** 2026-11-22  
**Purpose:** Identify real component ownership and migration constraints before Aphelion implementation.

This document records observations from the current source files listed below. It distinguishes observed code structure from architectural recommendations. It does not establish runtime performance, GPU zero-copy behavior, multi-client reliability, or cross-platform compatibility.

## 1. Executive findings

The current Syzygy implementation is already more internally partitioned than a simple one-session/one-pipeline model:

- A shared broadcast context owns the control server, video/audio UDP sockets, receive loop, and broadcast threads.
- Video capture is shared across active capture contexts for a selected display in both the synchronous and asynchronous capture paths.
- Encoding state is per session/capture context, not one shared encoder context.
- Encoded video packets carry an opaque channel_data pointer that is set to the session object and later used by the shared broadcaster to recover per-client state.
- The shared video broadcaster performs protocol framing, FEC, encryption, pacing, and network transmission in the same loop.
- Audio capture/encoding has per-session work, while audio transmission is handled by a shared broadcaster.
- The session object combines several kinds of per-client state: video peer/crypto/sequence state, audio peer/crypto/FEC state, control crypto/input-feedback state, and lifecycle/configuration state.

**Architecture implication:** Aphelion must not assume that Syzygy is currently a fully independent media pipeline per client, nor should it assume that one shared encoder is already implemented. The source shows shared capture with per-session encoding contexts and a shared per-client-aware sender. The initial migration should preserve those semantics before changing ownership.

## 2. Video capture and encoding

### 2.1 Shared capture in the asynchronous path

In src/video.cpp:
- capture_thread_async_ctx_t owns one capture thread, a queue of capture contexts, a display reference, and a reinitialization signal (around lines 460–485).
- captureThread maintains a list of active capture contexts and captures from one display (around lines 1147–1320).
- For each captured frame, it fans the same shared image object out to every active capture context (around lines 1300–1320).
- The image pool is bounded to 12 shared image slots (around lines 1195–1205), and image ownership/use_count controls reuse (around lines 1205–1288).
- Each capture context is registered through capture_async (around lines 2420–2440).

This is evidence of shared capture and shared image-buffer distribution within this code path. It is not proof that every platform backend is zero-copy or that every client can safely consume the same GPU allocation indefinitely.

### 2.2 Synchronous capture path

The synchronous path has a capture_thread_sync_ctx_t with an encode-session context queue. captureThreadSync owns a vector of active session contexts and runs encode_run_sync (around lines 450–485 and 2385–2415).

encode_run_sync:
- opens one display;
- creates an encode session for each active context;
- captures a frame;
- iterates over active sessions and converts/encodes that same captured image for each session;
- removes contexts when their shutdown event is raised;
- admits newly queued contexts while capture continues.

See src/video.cpp around lines 2236–2390.

### 2.3 Per-session encoder contexts

sync_session_t contains a pointer to its context and a unique_ptr<encode_session_t> (src/video.cpp around lines 450–458). The encoder interface is abstracted through encode_session_t methods such as convert, request_idr_frame, request_normal_frame, and invalidate_ref_frames (src/video.h around lines 200–220).

The async path also creates an encode device and encode session for its capture context before entering encode_run (src/video.cpp around lines 2440–2490).

The encoder implementation is selected through a global chosen_encoder pointer, while individual encoding sessions are created for the active contexts. Therefore:
- the source demonstrates a common selected encoder backend and separate per-session encoding state;
- it does not demonstrate a shared NVENC/VAAPI encoder session producing one bitstream for many clients;
- it does not yet prove which combinations of different resolutions, frame rates, codecs, or quality settings are supported concurrently.

**Aphelion implication:** Do not prematurely force a single encoding profile per capture source if that would regress existing behavior. First establish the actual supported profile combinations with source tracing and tests. Do not claim shared encoding until a real shared encoder context and bitstream fan-out are demonstrated.

## 3. Encoded video packet boundary

src/video.h defines packet_raw_t with:
- frame metadata and payload accessors;
- optional codec parameter replacements;
- opaque channel_data;
- reference-frame invalidation state;
- optional frame timestamp.

The encode paths enqueue packets with channel_data supplied by the caller. The caller passes the session pointer through the capture path (src/video.cpp around lines 1959–2025, 2480–2510; src/stream.cpp around lines 1327–1360).

The shared video broadcaster retrieves session_t from packet->channel_data. It then reads per-session configuration, peer, sequence number, encryption state, and pacing configuration. It applies codec parameter replacements, adds the Moonlight short-frame header, calculates FEC, constructs RTP/Moonlight packet metadata, encrypts shards per client, and sends batches (src/stream.cpp around lines 1327–1665).

**Critical finding:** The encoded packet queue is not a clean public bitstream boundary. Packets are tagged with an opaque pointer to session-owned state, and the downstream broadcaster immediately uses that state to create per-client wire packets. Extracting a gateway requires replacing this implicit pointer contract with an explicit, lifetime-safe message contract. It cannot be done by merely redirecting a socket.

## 4. Shared network and broadcast ownership

broadcast_ctx_t owns:
- shared message-queue routing;
- receive, video, audio, and control threads;
- one IO context;
- video and audio UDP sockets;
- one control server.

A process-wide shared broadcast object is created from this context (src/stream.cpp around lines 300–340 and 480–490). start_broadcast binds the control, video, and audio sockets and starts the shared broadcast threads (around lines 1799–1860). end_broadcast stops queues/sockets and joins those threads (around lines 1860–1895).

The video broadcaster is a single shared thread consuming the shared video packet queue. Each packet's channel_data selects the client session. Thus client-specific packet construction occurs in one sender loop rather than in a separate gateway process per client.

The source explicitly disables its WAN pacing mode when more than one session is active, warning that multiple clients share the video sender; this avoids slow pacing for one client stalling another (src/stream.cpp around lines 1400–1435).

**Aphelion implication:** Network extraction must account for shared sender scheduling, per-client pacing, queue fairness, packet ordering, and failure isolation. Moving the current loop into a separate process without defining these semantics risks regressions.

## 5. Per-session state observed in stream.cpp

session_t includes:
- config, mail, input object, audio/video threads, ping timeout, broadcast reference, and local address;
- video peer, ping payload, low sequence number, optional AES-GCM cipher, IV counter, IDR/reference invalidation events, and QoS resource;
- audio cipher, sequence number, timestamp, peer, FEC buffers/shards, FEC packet, and QoS resource;
- control cipher and IV state, connection/session matching data, peer, sequence number, feedback queue, and HDR queue;
- launch/session identifiers, device identity, permissions, do/undo commands, shutdown event, and session state.

See src/stream.cpp around lines 300–395.

This confirms that the existing session object is a mixed ownership container. It is not yet a clean boundary between capture, encoder, gateway, audio, and input lifecycles.

## 6. Audio path observations

src/audio.cpp:
- capture() obtains an audio context reference, chooses a sink, and creates a microphone capture object (around lines 130–205).
- It creates a dedicated encodeThread for the session, with an Opus multistream encoder and a high-priority thread (around lines 75–125 and 195–220).
- Encoded audio packets are placed on the shared audio packet queue with channel_data (around lines 75–125).
- The first session to acquire the sink flag may change the default sink; the code tracks whether the prior sink should be restored (around lines 165–190).

src/stream.cpp defines per-session audio cipher, sequence, timestamp, peer, FEC buffers, and QoS state in session_t. Audio transmission is handled by the shared audio broadcaster.

**Aphelion implication:** Audio cannot be treated as merely a device name passed at launch. Its capture context, sink selection/restoration, per-session Opus state, encoded packet ownership, per-client packetization/encryption, and shutdown behavior need to be mapped before extraction.

## 7. What this audit changes in the Aphelion plan

1. Preserve shared capture fan-out where the existing implementation supports it. Do not regress to one independent display capture per client without evidence.
2. Preserve per-session encoder contexts initially. The current source does not show one shared encoder session for multiple clients.
3. Treat capture, encoder, and network as separate logical ownership domains even while the Thick Worker hosts them.
4. Replace the opaque packet channel_data pointer only when designing a real extraction boundary; document its current lifetime assumptions first.
5. Do not split the video broadcaster until per-client encryption, FEC, packetization, pacing, ordering, and queue isolation have explicit contracts and compatibility tests.
6. Audit audio separately. The audio sink and capture lifecycle has shared/global behavior that must not be accidentally duplicated or torn down per client.
7. Revisit the initial one-profile-per-source assumption against measured and tested Syzygy behavior. The conservative initial Aphelion policy should preserve supported Syzygy configurations, not silently narrow them.

## 8. Remaining source-audit work

This audit is intentionally incomplete. Before implementation:
- trace the packet_t definition and queue implementation, including ownership and shutdown guarantees;
- trace encode() and codec packet ownership through every encoder backend;
- inspect platform capture/display and GPU buffer allocation/import/export paths;
- map input/control message routing and process/application launch lifecycle;
- trace audio context reference counting and sink restoration across overlapping sessions;
- inspect all relevant build options and platform-specific branches;
- audit license files and third-party notices;
- build and run tests on actual supported hardware.

No latency, zero-copy, multi-client scaling, or hardware concurrency claims are made by this document.

## 9. Source references

- [src/stream.cpp — shared session and broadcaster implementation](https://github.com/NotADeveloper4665/Syzygy/blob/main/src/stream.cpp)
- [src/video.cpp — capture and encoding implementation](https://github.com/NotADeveloper4665/Syzygy/blob/main/src/video.cpp)
- [src/video.h — encoder and packet interfaces](https://github.com/NotADeveloper4665/Syzygy/blob/main/src/video.h)
- [src/audio.cpp — audio capture and Opus encoding](https://github.com/NotADeveloper4665/Syzygy/blob/main/src/audio.cpp)
- [src/audio.h — audio declarations](https://github.com/NotADeveloper4665/Syzygy/blob/main/src/audio.h)
