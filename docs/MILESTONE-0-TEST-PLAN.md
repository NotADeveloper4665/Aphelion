# Aphelion Milestone 0 — Baseline and Verification Test Plan

**Status:** Test plan; tests have not yet been executed as part of this audit  
**Date:** 2026-11-22  
**Syzygy baseline:** `c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d`  
**Purpose:** Establish reproducible behavior, compatibility, resource-lifetime, and performance baselines before Aphelion implementation changes the inherited streaming path.

Milestone 0 is an audit and measurement milestone. It does not require an Aphelion supervisor, a new protocol, or a media-path rewrite. Tests should first run against the pinned Syzygy baseline so later Aphelion behavior can be compared to something real.

## 1. Rules for evidence

Every test result must record:
- Syzygy commit and submodule gitlink revisions;
- OS distribution, kernel, compositor/session type, and display server;
- GPU model, PCI identity, driver version, and available encoder APIs;
- capture backend and selected display;
- encoder backend, codec, profile, resolution, frame rate, and pixel format;
- client application/version and connection type;
- test command, start/end time, exit status, logs, and artifacts;
- whether the test was automated, manually observed, skipped, or unavailable.

A test that cannot run on the available machine is **not passed**. Mark it blocked or not applicable and state why. A hardware-specific test must not be represented as a generic CI pass.

Do not invent latency targets before measuring the inherited baseline. Record the observed distribution and define product targets after the baseline is available.

## 2. Existing Syzygy test coverage

The current repository already has a GoogleTest-based test build and several relevant tests:

| Existing test | What it currently covers | Known limitation |
|---|---|---|
| `tests/unit/test_audio.cpp` | Exercises audio capture/encoding across stereo/high-quality stereo, 5.1, 7.1, and custom surround configurations | Does not by itself validate multi-client sink arbitration or default-sink restoration |
| `tests/unit/test_video.cpp` | Validates whether NVENC, VA-API, software, and platform encoders are available | Test body contains a TODO; it is not a frame encode/decode or latency test |
| `tests/unit/test_mouse.cpp` | Mouse movement and absolute movement tests | Linux fixture skips the tests due to the Inputtino mouse-location limitation |
| `tests/unit/test_display_device.cpp` | Display configuration, HDR, resolution, and refresh-rate parsing/selection logic | Does not exercise physical display capture or driver behavior |
| `tests/test_pipewire_memory.cpp` | Owned memory-frame copy, stride/offset handling, and malformed buffer bounds | Does not exercise DMA-BUF import/export or real PipeWire lifecycle |
| `tests/headless_capture_smoke.cpp` | Private KWin headless output, PipeWire frame arrival, and fake-input binding | Hardcodes 640x480 and sends a pointer event without verifying its visible effect |
| `tests/syzygy_smoke.cpp` | CLI, pairing, key derivation, virtual-display option validation, and related smoke checks | Does not establish full streaming or GPU compatibility |
| `tests/integration/*` | Configuration, external command, and locale consistency | Not a media-path integration suite |

The existence of these tests does not mean they have passed in the current environment. Run them and save the actual output as the baseline.

## 3. M0 test suites and acceptance criteria

### M0-01 — Reproducible source and build baseline

**Goal:** Prove that the exact audited source can be checked out, configured, built, and tested.

Tests:
1. Check out the exact Syzygy commit.
2. Initialize every submodule at its exact gitlink commit; record each revision.
3. Perform a clean configure and build with the intended Linux options.
4. Build the test target and run the full registered test suite.
5. Run the CLI and Syzygy smoke tests independently so failures are easy to identify.
6. Repeat from a clean build directory.

**Pass criteria:**
- Build and test commands, dependencies, options, and exact revisions are recorded.
- No unexplained test failures remain.
- Skipped tests and platform-dependent tests are explicitly listed.
- A second clean build produces the same success/failure result.

### M0-02 — Moonlight compatibility baseline

**Goal:** Establish that the inherited streaming path works before wrapping it.

Tests:
1. Pair a supported Moonlight client with the Syzygy host.
2. Start and stop a normal H.264 stream.
3. Repeat with HEVC and AV1 only where both host encoder and client support are confirmed.
4. Verify audio, keyboard, mouse, and gamepad input independently.
5. Verify reconnect and client disconnect behavior.
6. Record the exact client and host configuration.

**Pass criteria:**
- Pairing and stream startup complete without protocol errors.
- Video decodes and remains stable for a recorded test interval.
- Supported input and audio functions work.
- Stop/disconnect returns the host to a clean state.
- Unsupported combinations fail clearly rather than silently claiming support.

### M0-03 — Capture backend matrix

Run only the rows supported by the test machine. A backend that is unavailable must be marked blocked/not applicable, not passed.

| Backend | Required cases |
|---|---|
| X11/XShm | SHM capture; XGetImage fallback; monitor selection; resize/reconfigure; cursor capture; repeated start/stop |
| Wayland wlroots | Required protocol present/missing; DMA-BUF path; RAM/readback path; selected output; timeout; output resize/reinit; FD cleanup |
| Desktop portal/PipeWire | Permission granted/denied/cancelled; portal session reuse; restore token; memory-buffer negotiation; DMA-BUF negotiation; PipeWire disconnect/reconnect; cursor behavior |
| DRM/KMS | DRM access allowed/denied; multiple cards; monitor/plane selection; framebuffer disappearance; hotplug/modeset; cursor plane; HDR metadata change; RAM and VRAM paths |
| NVIDIA NvFBC | Handle creation; capture-session creation; direct-capture path; fallback copy path; cursor-mode change; MUST_RECREATE handling; teardown |
| Private headless KWin | Headless output creation; expected geometry; screencast node; PipeWire frame arrival; fake keyboard/mouse input; shutdown and restart |

For each backend, also test:
- start → stream → stop → start again;
- compositor/display server restart where practical;
- output resolution change while streaming;
- selected display disappears;
- capture timeout and error propagation;
- one client stops while another client still uses the same capture source.

**Pass criteria:**
- Every attempted backend has a recorded result and source of failure if it fails.
- Frame geometry and selected display are correct.
- Expected reinit/timeout/error conditions do not become hangs.
- FDs and backend objects are released after stop.
- One consumer stopping does not terminate a shared source still in use.

### M0-04 — Input ownership and multi-client behavior

**Goal:** Verify global and per-client input state is cleaned up correctly.

Tests:
1. Verify host-global keyboard and mouse virtual devices can be created with the documented device permissions.
2. Verify key-down/key-up, modifier handling, Unicode text, mouse relative/absolute motion, buttons, and vertical/horizontal scroll.
3. Verify permission denial for keyboard, mouse, controller, touch, and pen events.
4. Connect two clients and hold distinct keys/buttons from each.
5. Disconnect one client while the other continues holding and using input.
6. Verify key/button release on disconnect, session stop, and worker shutdown.
7. Connect/disconnect gamepads repeatedly; verify virtual device creation/removal.
8. Verify rumble and LED feedback returns to the correct client.
9. Verify DualSense-only motion, touchpad, battery, and adaptive-trigger paths where supported.
10. Verify each client has independent touchscreen and pen state.
11. In headless KWin mode, verify keyboard and mouse fake-input paths; separately test touch/pen/gamepad because they currently use different device paths.

**Pass criteria:**
- One client's disconnect does not release another client's held state.
- No stuck keys, mouse buttons, controller buttons, or touch contacts remain.
- Virtual devices and feedback threads are removed after their actual final owner exits.
- Per-client touch/pen events do not cross between clients.
- Permission checks prevent unauthorized input from reaching the host.

**Known coverage gap:** The existing Linux mouse unit test is skipped. M0 must add a real integration method or manual verification record; the skipped test cannot count as coverage.

### M0-05 — GPU, DMA-BUF, and encoder resource lifetime

**Goal:** Determine which capture-to-encoder paths actually work and establish their ownership behavior.

Test matrix:
- software/system-memory encoding;
- VA-API with system-memory input;
- VA-API with DMA-BUF/EGL input where available;
- NVENC/CUDA with system-memory input;
- NVENC/CUDA with GL/CUDA interop where available;
- NvFBC-to-CUDA where available.

For every supported combination:
1. Record GPU identity, driver, device node, format, modifier, dimensions, and encoder profile.
2. Capture and encode frames at the chosen baseline resolutions/frame rates.
3. Verify output decodes and has correct dimensions, color, and frame timestamps.
4. Repeatedly start/stop the capture and encoder objects.
5. Exercise failure during initialization, frame import, encode setup, and teardown where safely reproducible.
6. Monitor open FD count and process memory across repeated runs.
7. Verify that every DMA-BUF FD has one owner and is closed exactly once.
8. Verify CUDA stream/GL registration/EGL teardown order and VA surface/FFmpeg device-context teardown order.
9. Test hybrid GPU configurations separately from single-GPU configurations.

**Pass criteria:**
- Each supported combination has a reproducible result and recorded configuration.
- Unsupported combinations fail explicitly or use a documented fallback.
- No FD growth, stale device handles, GPU-context leaks, or teardown hangs across repeated runs.
- No zero-copy claim is made unless the complete path is measured and shown not to copy.

### M0-06 — Audio lifecycle and sink behavior

**Goal:** Establish how the shared audio context behaves with one and multiple clients.

Tests:
1. Run video with audio enabled and disabled; video must continue when audio initialization fails.
2. Test stereo, 5.1, 7.1, and custom channel mapping.
3. Test host audio enabled/disabled and virtual null-sink selection.
4. Test configured sink, default sink, and missing/unavailable sink.
5. Start two sessions with audio; stop the first, then the second.
6. Confirm the original host default sink is restored only after the shared audio context's final owner exits.
7. Disconnect/restart the PulseAudio-compatible server during a stream.
8. Remove the selected sink during a stream and observe recovery/failure behavior.
9. Trigger capture reinitialization and confirm capture/Opus threads terminate cleanly.
10. Confirm no audio packet queue or encoder thread remains after session shutdown.

**Pass criteria:**
- Audio-enabled streams deliver decodable audio for supported layouts.
- Audio failure degrades to video-only without hanging session shutdown.
- Shared sink selection and restoration behavior is documented and repeatable.
- Per-session capture/Opus resources stop independently.
- No audio thread, mainloop worker, or virtual sink is left behind.

**Known limitation to resolve or document:** Linux `is_sink_available()` currently returns true without checking the requested sink. M0 must record this limitation and decide whether a testable implementation is required before the supervisor depends on sink validation.

### M0-07 — Shared capture, per-session encoder, and profile behavior

**Goal:** Confirm actual multi-client behavior before choosing Aphelion's initial profile policy.

Tests:
1. Connect two clients to the same display with the same profile.
2. Connect two clients to the same display with different resolutions.
3. If supported, vary frame rate, codec, and quality independently.
4. Observe whether one capture source feeds both sessions.
5. Observe whether each session has its own encoder context and output.
6. Stop one client while the other remains active.
7. Disconnect/reconnect one client while the other continues.
8. Record resource use and any explicit rejection/fallback behavior.

**Pass criteria:**
- The source-sharing and encoder-sharing behavior is established by observation and code tracing.
- No profile is declared compatible merely because it starts.
- A client's stop/reinit does not corrupt another client's stream.
- Aphelion's initial policy preserves verified Syzygy behavior unless the maintainer explicitly approves a change.

### M0-08 — Lifecycle and failure containment

**Goal:** Map the resource release order and identify failure paths before process isolation.

Tests:
1. Stop during capture initialization.
2. Stop during encoder initialization.
3. Stop during audio initialization.
4. Stop while a client is connected and input is held.
5. Stop one of multiple sessions.
6. Stop the final session.
7. Force a capture backend to report reinit/error.
8. Force a PipeWire or PulseAudio connection failure.
9. Force GPU encoder initialization failure.
10. Repeat start/stop cycles and inspect process/thread/FD counts.

**Pass criteria:**
- Each resource has an identifiable owner and teardown trigger.
- Failures are surfaced rather than silently converted to success.
- No unrelated session is stopped by another session's failure.
- All final-owner resources are released.
- Any known teardown timeout/hang is documented before designing the supervisor restart policy.

### M0-09 — Performance baseline

**Goal:** Capture a trustworthy baseline before changing process boundaries or media code.

For each hardware/backend combination that is intended to be supported:
- choose one repeatable 1080p60 baseline and at least one additional supported resolution/frame rate;
- warm up before collecting results;
- run repeated samples for a fixed interval;
- record capture FPS, encode FPS, dropped frames, queue depths where measurable, CPU utilization, GPU utilization, VRAM, memory, network bitrate, and audio underruns where measurable;
- record p50/p95/p99 for each latency stage only when timestamps actually exist;
- record end-to-end latency using a documented measurement method;
- repeat enough runs to identify variance and outliers.

Do not compare measurements taken with different clients, drivers, capture modes, or encoder settings as if they were equivalent. Do not invent missing stage metrics. The baseline informs later targets; M0 does not set arbitrary numeric performance promises.

**Pass criteria:**
- The benchmark procedure can be repeated.
- Raw results and machine configuration are retained.
- Measurements distinguish capture, encode, network, decode, and presentation only where each stage is actually observable.
- Any missing instrumentation is recorded as a gap, not estimated.

### M0-10 — License and dependency provenance

**Goal:** Ensure the inherited baseline can be reproduced and redistributed responsibly.

Tests/checks:
1. Record the exact Syzygy commit and every submodule gitlink SHA.
2. Inventory direct and transitive dependencies, license identifiers, notices, and bundled assets.
3. Confirm the inherited GPL-3.0-only project metadata and required notices.
4. Identify any incompatible or unclear dependency terms before copying code.
5. Record the provenance of generated/bundled assets and third-party patches.

**Pass criteria:**
- A dependency/provenance inventory is checked into Aphelion.
- Unresolved license questions are explicitly blocked from redistribution.
- No dependency revision is recorded only by a moving branch name.

## 4. Execution order

Recommended order:
1. M0-01 source/build baseline.
2. M0-02 Moonlight compatibility baseline.
3. M0-03 capture matrix.
4. M0-04 input behavior.
5. M0-05 GPU/encoder matrix.
6. M0-06 audio lifecycle.
7. M0-07 multi-client profile behavior.
8. M0-08 lifecycle/failure cases.
9. M0-09 performance baseline.
10. M0-10 dependency/license provenance.

Some tests can run in parallel on different hardware, but do not change the audited source while collecting the baseline. If a baseline bug is fixed, record the before/after commits and rerun affected tests.

## 5. M0 exit gate

Milestone 0 is complete only when all of the following are true:

- [ ] Exact source and submodule revisions are recorded.
- [ ] Clean build and existing tests have recorded results.
- [ ] Moonlight compatibility baseline is reproducible.
- [ ] Every intended Linux capture backend has a result or an explicit blocked/not-applicable reason.
- [ ] Input ownership and multi-client disconnect behavior have been tested.
- [ ] GPU/encoder combinations and DMA-BUF lifetime have recorded results.
- [ ] Audio sink selection/restoration and multi-session lifecycle have recorded results.
- [ ] Shared capture/per-session encoder behavior is measured and documented.
- [ ] Repeated start/stop and failure tests show no unexplained leaks or hangs.
- [ ] Performance baseline and measurement limitations are documented.
- [ ] Dependency and licensing provenance is sufficiently complete for the intended next development step.

**No Aphelion media-path implementation should begin until the maintainer reviews the M0 results and accepts the baseline.** The Rust supervisor/Thick Worker control-plane prototype may be planned separately, but it must not change the media path or assume unverified capture/GPU/audio contracts.
