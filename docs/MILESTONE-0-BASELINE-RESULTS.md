# Syzygy Milestone 0 — Baseline Execution Record

**Record date:** 2026-11-22  
**Target source:** `NotADeveloper4665/Syzygy`  
**Pinned source commit:** `c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d`  
**Commit message:** `Fix WAN pacing backlog at 5 Mbps with bounded sender fallback`  
**Commit date:** 2026-10-13  
**Execution evidence:** GitHub Actions runs attached to the exact pinned SHA  
**Important:** This is a partial CI baseline, not a complete Milestone 0 pass and not a run on the maintainer's T1600.

## 1. Evidence inspected

### Linux build and key checks

- Workflow: [Syzygy Linux build and key checks](https://github.com/NotADeveloper4665/Syzygy/actions/runs/38464393269)
- Run ID: `38464393269`
- Commit: exact pinned SHA above
- Event: push
- Runner: Ubuntu 24.04.5 hosted GitHub Actions runner
- Started: 2026-10-13 20:38:12 UTC
- Completed: 2026-10-13 20:47:35 UTC
- Job: `linux`, job ID `115527457514`
- Conclusion: **success**
- Submodules: recursively initialized. The workflow log records their checked-out commits.

The workflow checked out the exact commit and initialized recursive submodules. Its CMake configuration used:
- `CMAKE_BUILD_TYPE=Release`
- `SUNSHINE_ENABLE_CUDA=OFF`
- `SUNSHINE_ENABLE_TRAY=OFF`
- `BUILD_TESTS=OFF`
- RPM packaging and installation checks

Therefore, this run verifies a Linux software-encoder-capable package build path, not CUDA/NVENC, tray, or the GoogleTest unit suite.

### Development RPM publication

- Workflow: [Publish Syzygy development RPM](https://github.com/NotADeveloper4665/Syzygy/actions/runs/38465624247)
- Run ID: `38465624247`
- Commit: exact pinned SHA above
- Job: `publish`, job ID `115531560383`
- Conclusion: **success**

This workflow published the newest previously validated development RPM. It is not an independent build/test run and is not counted as additional test coverage.

## 2. Tests and checks evidenced as passing

The Linux workflow executed the following checks successfully:

| Check | Result | Evidence / scope |
|---|---|---|
| Dev RPM updater Python tests | PASS — 9 tests | `test_dev_rpm_updater.py` |
| Headless launcher Python tests | PASS — 8 tests | `test_headless_launcher.py` |
| Service manager Python tests | PASS — 2 tests | `test_service_manager.py` |
| Syzygy CLI/key-store/pairing smoke | PASS | CLI, key persistence and permissions, concurrent creation, unsafe-file rejection, pairing proofs |
| WAN sender smoke | PASS | WAN sender smoke executable completed |
| WAN pacing smoke | PASS | 5 Mbps pacing bounded with wire overhead and FEC minima; logged original backlog after 100 frames as 346 ms |
| Tailscale pairing smoke | PASS | Tailnet identity, local interface, sharing, expiry, daemon state, malformed-data rejection |
| PipeWire memory smoke | PASS | Owned frame lifetime, padded stride, offsets, malformed buffer bounds |
| Headless KWin capture smoke | PASS | Real headless KWin capture, virtual geometry, PipeWire frame, native input binding |
| RPM build | PASS | CMake/Ninja Release build and CPack RPM generation completed |
| RPM install | PASS | Package installed in CI environment |
| Installed assets check | PASS | Shaders, Web UI HTML and JavaScript assets found from unrelated working directory |
| Passkey pairing integration | PASS | Wrong passkeys, one-use challenges, two clients, host confirmation, TLS client auth, retry, persistence, log redaction |
| Headless startup integration | PASS | Installed `-headless` CLI, native capture/CPU encoder probe, Web UI without desktop session |
| WAN integration | PASS | Real WAN session, software encoder, small UDP, isolated telemetry, congestion/loss adaptation, stale fallback |
| Key output smoke | PASS | Standalone key output, persistence, redaction |

The Python test counts are directly shown in the job log:
- 9 tests in 2.333 seconds — OK
- 8 tests in 0.003 seconds — OK
- 2 tests in 0.879 seconds — OK

The headless capture smoke explicitly logged:
`PASS: real headless KWin capture, virtual geometry, PipeWire frame and native input binding`.

The WAN integration explicitly logged:
`PASS: real WAN session, software encoder, small UDP, isolated telemetry, congestion/loss adaptation and stale fallback`.

## 3. Warnings and environmental limitations in the passing run

The workflow passed, but its logs contain environment warnings that should not be hidden:

- The hosted runner lacked access to `/dev/dri/renderD128` for the attempted hardware device path; the KWin log reported permission denial and a Vulkan incompatible-driver error. The job intentionally used `LIBGL_ALWAYS_SOFTWARE=1` for headless capture.
- PipeWire's real-time module could not connect to the system D-Bus in the container.
- The xdg document portal could not mount its FUSE document path.
- Qt reported that its Wayland shell integration failed in a helper invocation.
- Package post-install emitted a read-only sysfs uinput uevent warning.
- The CMake build emitted a GCC atomic warning and CPack warned that the RPM would not be relocatable under the selected packaging flags.

These messages did not fail the workflow. They do mean the result must not be interpreted as proof of physical GPU capture, hardware encoding, or a fully functional desktop audio environment.

## 4. Milestone 0 coverage still missing

The following Milestone 0 areas are **not verified by these CI runs**:

| Test-plan area | Current result |
|---|---|
| M0-01 exact build and registered unit suite | Partial: Linux RPM build passed; `BUILD_TESTS=OFF`, no full GoogleTest/CTest run evidenced |
| M0-02 Moonlight client compatibility | Partial: pairing and WAN protocol smoke/integration passed; no complete real Moonlight client matrix evidenced |
| M0-03 capture backend matrix | Partial: private headless KWin/PipeWire smoke passed; X11, wlroots screencopy, portal permissions, DRM/KMS, and NvFBC matrix not evidenced |
| M0-04 input ownership/multi-client teardown | Partial: native headless input binding and pairing with two clients passed; concurrent held-key/button disconnect, gamepad lifecycle/rumble, touch and pen isolation not evidenced |
| M0-05 GPU/DMA-BUF/encoder matrix | Not verified: CUDA disabled; hardware device unavailable in runner; no NVENC/CUDA or VAAPI interop matrix |
| M0-06 audio lifecycle | Not verified: no audio/Opus, sink selection, restoration, server-loss, or multi-session audio test evidenced |
| M0-07 shared capture/per-session encoder/profile behavior | Not verified: no same-display multi-client profile test evidenced |
| M0-08 lifecycle/failure containment | Partial: some launcher/service/pairing and headless startup checks passed; resource leak, forced failure, and repeated multi-session teardown matrix not evidenced |
| M0-09 performance baseline | Partial: WAN pacing and adaptation checks passed; no reproducible capture/encode/CPU/GPU/end-to-end latency benchmark dataset |
| M0-10 dependency/license provenance | Not verified by these workflow runs |

The source tree contains GoogleTest tests for audio, video encoder availability, mouse, and display-device logic, plus PipeWire memory and headless smoke tests. The passing Linux workflow explicitly configured `BUILD_TESTS=OFF`; the existence of those test files must not be mistaken for executed test results. The video test's current scope also does not establish successful frame encode/decode.

## 5. Local execution attempt in this environment

A fresh clone with recursive submodules at the pinned SHA was attempted from the current execution environment. It could not resolve `github.com` (`Could not resolve host: github.com`). No local build or test process was started from that failed checkout.

This is a limitation of the current execution environment, not evidence of a Syzygy build failure. The CI logs above are the available executed evidence. No claim is made that tests were run on the user's Fedora T1600.

## 6. Baseline disposition

**Disposition: PARTIAL PASS — NOT READY TO EXIT MILESTONE 0.**

Confirmed:
- The exact pinned commit completed the hosted Linux CI build/package workflow.
- Existing targeted Python and C++ smoke checks listed above passed.
- Headless KWin/PipeWire capture and a software-encoder WAN integration path passed in CI.

Still required before Aphelion media-path implementation:
1. Run the registered GoogleTest suite with `BUILD_TESTS=ON` on a clean checkout of the pinned commit.
2. Run a Fedora 44 build/test on the T1600, recording its actual kernel, GPU, driver, compositor, and encoder capabilities.
3. Exercise the T1600's NVIDIA NVENC/CUDA path and relevant capture backend.
4. Execute audio lifecycle and multi-client input/capture teardown tests.
5. Record real Moonlight-client compatibility and performance results.
6. Complete the dependency/license provenance check.

Do not convert the missing rows to PASS based on successful RPM packaging or generic CI smoke checks. The maintainer must review the remaining evidence before approving the Milestone 0 exit gate.
