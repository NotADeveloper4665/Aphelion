# Syzygy Source Ownership and Lifecycle Audit

**Status:** Source-level audit, not runtime validation  
**Audited repository:** NotADeveloper4665/Syzygy  
**Audited ref:** `main` at `c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d`  
**Audit date:** 2026-11-22  
**Scope:** Linux input, display capture, GPU/graphics resources, audio lifecycle, and their ownership implications for Aphelion.

All source links below are pinned to the audited commit so future changes to Syzygy do not silently alter the evidence. This audit describes source structure; it does not claim that hardware paths were executed or that every cleanup/error path is correct.

## 1. Ownership overview

The current implementation has several different ownership scopes. They must not be collapsed into a single generic "session owns everything" model.

| Resource | Observed owner | Lifetime / sharing |
|---|---|---|
| Host keyboard and mouse virtual devices | Process-wide `platf_input` | Shared across client sessions |
| Virtual gamepad devices | Process-wide platform input object; per-session gamepad slots map to global IDs | Device is created for a client controller arrival and removed on controller/session teardown |
| Virtual touchscreen and pen devices | Per-client input context | Separate device handles for each client |
| Headless KWin fake-input connection | Process-wide singleton | Shared by headless capture/input path |
| Display capture source | Shared capture context for sessions targeting the same source in the current video paths | One capture source can feed multiple per-session encoder contexts |
| Encoder session | Per capture/session context | Separate encoder state per session in the inspected paths |
| EGL/GL context and imported image objects | Display or encoder-device object | Must outlive GL/EGL resources that depend on it |
| CUDA stream and registered GL resources | CUDA/GL encoder device | Stream and interop registrations must be torn down before dependent EGL/GL objects |
| VA display, DRM FD, VA surfaces and FFmpeg hardware context | VAAPI encoder device / FFmpeg device context | Explicit reference and destruction ordering |
| Portal D-Bus session and remote FD | Process-wide portal session cache | Cached and shared; each consumer receives a duplicated FD |
| PipeWire loop/context/core/stream | Per portal capture object | Owns its duplicated remote FD and PipeWire resources |
| PulseAudio control connection and virtual sinks | Shared audio context | Created once while audio context is held; default sink may be changed by the first stream and restored when the shared context ends |
| Audio capture and Opus encoder thread | Per streaming session | Session shutdown stops capture, drains/stops the sample queue, and joins encoder thread |
| Video/audio packet transport state | Per client session plus shared broadcaster | Per-client crypto/sequence/FEC/pacing state is consumed by shared sender loops |

## 2. Input ownership

### 2.1 Host-global devices versus per-client devices

The Linux Inputtino backend defines `input_raw_t` with:
- one virtual mouse;
- one virtual keyboard;
- a host-wide collection of virtual gamepad slots.

The process-level input module keeps a static `platf_input` object and a static gamepad ID mask. `input::init()` initializes the platform input object; its deinitializer resets it.

Each logical client input context contains:
- a per-client `client_input_raw_t` created from the shared host input object;
- its own controller-slot mapping and feedback queue;
- a per-client input-message queue and shortcut/touch-port state.

The per-client platform context creates separate virtual touchscreen and pen devices. The source comment explicitly says these are per-client because clients can send independent touch/pen events.

Gamepads are created in the shared platform input object when a client reports controller arrival. Each session's gamepad slot stores the allocated global ID. The virtual gamepad is selected as Xbox One, DualSense, or Switch Pro based on configuration and client capabilities. Rumble/LED callbacks capture that client's feedback queue. Freeing the gamepad destroys its Inputtino object; the source comment notes that destruction stops its background rumble/LED threads and removes the virtual device.

**Aphelion implication:** Input ownership is mixed by design. Preserve host-global keyboard/mouse devices, per-client touch/pen devices, and explicit controller allocation/feedback ownership unless a tested redesign intentionally changes those semantics.

### 2.2 Shared input state and disconnect behavior

The input module also has process-static key-press and mouse-button state. Session reset releases keys and mouse buttons through the shared host-global devices. The platform input object is not recreated for each client.

Input messages are checked against each client's permission bits before being queued. The inspected code distinguishes controller, mouse, keyboard, touch, and pen permissions.

**Concurrency risk to test:** Because the keyboard/mouse devices and key/button tracking are process-wide, disconnect/reset behavior with two active clients must be tested. A client's teardown must not release another client's still-held input or leave a stuck key/button. The source layout alone does not prove this behavior is race-free.

### 2.3 Headless input path

When the private headless KWin session is active:
- keyboard events use the KWin fake-input keyboard protocol;
- mouse movement, absolute movement, buttons, and scroll use the KWin fake-input pointer protocol;
- the fake-input interface is authenticated by the headless session;
- the KWin connection is held by a process-wide singleton protected by a mutex.

Touch and pen handling in the inspected Inputtino source still uses the per-client Inputtino touchscreen/pen objects; it does not use the same KWin fake-input branch. Gamepad emulation also remains on the Inputtino path.

**Aphelion implication:** Do not describe headless input as one unified backend. Keyboard/mouse and touch/pen/gamepad currently have different implementation and device ownership paths.

### 2.4 Input extraction boundary

A future input service needs explicit ownership for:
- host-global keyboard/mouse state;
- per-client permissions and event ordering;
- per-client touch/pen device lifetime;
- gamepad global ID allocation and per-client mapping;
- rumble, LED, adaptive-trigger, motion, battery, and touchpad feedback;
- release-all behavior on disconnect and process shutdown;
- headless KWin versus uinput/Inputtino routing.

It must also define how multiple clients are arbitrated when they send overlapping keyboard/mouse state. Do not split input into a separate process before those semantics are captured in tests.

## 3. Display capture ownership

### 3.1 Shared capture and per-session encoding

The inspected `src/video.cpp` paths already separate capture from per-session encoding internally:
- the asynchronous capture context owns a capture thread and a set of active capture contexts for a display; captured image objects are fanned out to those contexts;
- the synchronous capture path owns one display capture loop and iterates over active session contexts, converting/encoding the same captured image for each session;
- each session context owns its own `encode_session_t`.

The asynchronous image pool is bounded, and shared image ownership controls when a slot can be reused. This is shared capture with per-session encoding contexts; it is not proof of one shared hardware encoder session or one common encoded bitstream.

**Aphelion implication:** Preserve the existing shared-capture/per-session-encoder behavior until profile combinations and lifecycle are measured. Do not replace it with one capture per client, and do not claim shared encoding without evidence.

### 3.2 Linux capture backends

The Linux source contains several independent capture implementations. They have different compositor, permission, memory, and resource requirements.

| Backend | Main implementation | Key ownership / constraints |
|---|---|---|
| X11 | `x11grab.cpp` | X display connection and root window; XRandR monitor selection; XShm path uses XCB shared memory and a SysV shared-memory segment; fallback uses XGetImage; cursor may be blended separately |
| Wayland wlroots | `wayland.cpp`, `wlgrab.cpp` | Wayland display/registry/output objects; requires xdg-output and wlr-export-dmabuf support; screencopy frame and DMA-BUF objects have explicit cleanup |
| Desktop portal | `portalgrab.cpp` | D-Bus portal session and PipeWire remote FD; portal session cache is process-wide; PipeWire capture loop/context/core/stream are owned by each capture object |
| DRM/KMS | `kmsgrab.cpp` | Opens DRM card nodes, enumerates planes/CRTCs/connectors, exports framebuffer handles to DMA-BUF FDs, and imports through EGL; requires access to DRM resources and can fail when framebuffer handles are unavailable |
| NVIDIA NvFBC | `cuda.cpp` NvFBC section | NVIDIA-specific capture handle and capture session, bound to a thread context; uses NvFBC-to-CUDA interop and may reinitialize on driver-reported conditions |
| Private headless KWin | `kwin_headless.h` plus portal/PipeWire path | Process-wide private KWin session creates a virtual output and screencast node; PipeWire consumes that node; fake input is provided by the same KWin session |

### 3.3 X11

The X11 backend opens an X display and captures the root window. It uses XRandR to select a monitor and determine its dimensions/offset. The preferred XShm path allocates a SysV shared-memory segment, attaches it to XCB, and copies each frame into an owned image buffer. If the SHM path cannot initialize, the factory falls back to XGetImage.

The SHM display object schedules delayed refresh work and cancels that task in its destructor. The X11 capture loop checks for display geometry changes and requests reinitialization when the X server dimensions change.

**Tests required:** SHM available/unavailable fallback, selected-monitor geometry, resize/reconfiguration, cursor blending, and repeated start/stop with shared-memory cleanup.

### 3.4 Wayland wlroots screencopy

The wlroots backend connects to the Wayland display, binds output metadata, screencopy, and DMA-BUF export interfaces, and selects a monitor. It requests a new screencopy frame and dispatches Wayland events until the frame is ready or times out.

Two memory paths exist:
- The RAM path imports the exported DMA-BUF through EGL/GL and reads pixels back into a CPU-owned image buffer.
- The VRAM path transfers the frame's surface descriptor and file-descriptor ownership into an image descriptor for downstream GPU import. The source explicitly invalidates the source frame's FD slots after transferring ownership so the DMA-BUF object does not close the transferred descriptors.

The VRAM path's image descriptor closes its owned FDs on reset/destruction. The Wayland DMA-BUF manager destroys frame objects and GBM resources during teardown.

**Important:** The RAM path is not a zero-copy path; it performs a texture readback into CPU memory. The VRAM path enables DMA-BUF import but still depends on compatible EGL/driver support and correct FD lifetime.

**Tests required:** compositor interface absence, DMA-BUF versus RAM fallback, FD transfer/close exactly once, frame timeout, output resize/reinit, and repeated capture teardown.

### 3.5 Desktop portal and PipeWire

The portal backend has a process-wide cached session object. It owns the D-Bus portal session and a PipeWire remote FD, caches node/geometry, and returns a duplicated FD to each consumer. It can persist a portal restore token for ordinary capture; virtual-display mode intentionally does not use the same restore-token path and requires the portal to advertise virtual-monitor support.

Each portal capture object owns a PipeWire thread loop, context, core, stream, and a remote FD. Its destructor locks the thread loop, deactivates/disconnects/destroys the stream, disconnects the core, destroys the context, unlocks, stops the loop, closes its FD, and destroys the loop.

The PipeWire process callback retains the newest dequeued buffer and returns the previous one to PipeWire. `fill_img` locks the PipeWire loop:
- for DMA-BUF data it duplicates each buffer FD into the image descriptor and copies format/stride/offset metadata;
- for memory data it copies the required frame rows into an owned vector, then exposes that vector as the image data.

The portal code negotiates DMA-BUF only for supported GPU combinations. Its comments explicitly state that DMA-BUF from an Intel display GPU cannot be imported into CUDA on an Intel+NVIDIA hybrid system, so that case falls back to memory buffers.

The cursor argument is currently ignored by the portal snapshot path (the source contains a FIXME).

**Tests required:** portal permission denial/cancel, restore-token reuse/invalidation, multiple consumers of the cached portal session, duplicated FD lifetime, PipeWire stream error/reconnect, DMA-BUF and memory-buffer negotiation, hybrid GPU fallback, buffer recycling, and cursor behavior.

### 3.6 DRM/KMS

The KMS backend scans `/dev/dri/card*`, opens a card node read/write, enables universal planes and atomic capabilities, and enumerates active planes. It correlates DRM card/CRTC information with monitor geometry, reads framebuffer metadata, and exports framebuffer handles to PRIME FDs.

The capture path re-queries the plane/framebuffer each refresh because the display may be reconfigured while streaming. It returns timeout when a framebuffer disappears during reconfiguration, error when required framebuffer handles cannot be accessed, and reinit when dimensions or HDR metadata change.

The RAM variant imports the DMA-BUF into EGL and reads pixels into CPU memory. The VRAM variant creates an EGL/GL image descriptor for downstream VA-API or CUDA interop. Cursor handling uses a separate cursor plane and may map framebuffer data to CPU memory for cursor composition.

**Tests required:** no DRM permissions, missing framebuffer handles, multiple DRM cards, hybrid GPU selection, display modeset, hotplug, cursor plane visibility, HDR metadata change, framebuffer FD lifetime, and RAM/VRAM paths.

### 3.7 NvFBC

NvFBC uses a handle object that owns the NvFBC session handle and capture session. A separate context object binds the NvFBC handle to the current thread and releases it on destruction. The display capture loop binds the context, creates the capture session, and resets the handle on exit. It can reinitialize capture when NvFBC reports that the session must be recreated or when cursor-capture mode changes.

The implementation requests shared CUDA capture and can attempt direct capture before falling back to an extra-copy path. It is NVIDIA-specific and cannot be treated as a generic capture backend.

**Tests required:** supported/unsupported GPU and driver behavior, handle/context cleanup on every error path, direct-capture fallback, cursor-mode reinitialization, modeset/recreate behavior, and CUDA frame ownership.

## 4. GPU and graphics resource ownership

### 4.1 EGL/GL

The Linux graphics layer can create EGL displays from GBM, Wayland, or X11 native displays. It initializes EGL and requires:
- EGL_KHR_create_context;
- EGL_KHR_surfaceless_context;
- EGL_EXT_image_dma_buf_import;
- EGL_EXT_image_dma_buf_import_modifiers.

It creates an OpenGL context and makes it current. DMA-BUF image import creates EGLImages and GL textures. The image descriptor type owns up to four DMA-BUF FDs and closes them when reset or destroyed.

**Ownership rule:** EGLImages, textures, framebuffers, and GL objects must be destroyed while the relevant EGL/GL context is still valid and current as required by the implementation. The source uses RAII wrappers, but a process split must not assume that an EGL context or GL object can be transferred by sending only an FD.

### 4.2 CUDA and GL interop

The CUDA GL encoder path owns:
- a DRM file descriptor selected for the CUDA device;
- a GBM device;
- an EGL display and EGL context;
- GL textures and framebuffers used for conversion/scaling;
- a CUDA stream;
- CUDA graphics-resource registrations for the GL Y and UV textures;
- an FFmpeg CUDA hardware frame.

The source explicitly comments that the CUDA stream must be destroyed before the EGL display. It registers GL textures with CUDA, maps them for CUDA access, copies the texture planes into the destination CUDA frame with `cuMemcpy2DAsync`, then unmaps the resources.

**Important correction to any zero-copy claim:** This path is GPU-resident interop, but it explicitly copies the converted GL texture planes into the CUDA hardware frame. Do not describe it as end-to-end zero-copy.

### 4.3 VA-API and EGL

The VA-API encoder device owns a DRM FD, GBM device, EGL display/context, VA display, and VA surfaces/FFmpeg hardware frames. It exports VA surfaces as DRM PRIME descriptors and imports the exported planes into EGLImages/GL textures. The source explicitly notes that the FFmpeg hardware frame must be destroyed before the VA display so the GPU driver is still loaded when VA surfaces are destroyed.

The FFmpeg VAAPI hardware-device context duplicates the DRM FD and takes ownership of the VA display/FD through its custom free callback.

### 4.4 Cross-process consequence

The current GPU path relies on process-local state and strict destruction ordering:
- CUDA contexts/streams and registered graphics resources;
- EGL display/context and GL object namespaces;
- VA display and FFmpeg hardware-device context;
- DMA-BUF FDs and their metadata;
- GBM device lifetime;
- frame and image references held by capture/encoder queues.

Passing a DMA-BUF FD to another process is not enough to transfer all this state safely. Any future GPU process boundary needs a documented handle contract covering device identity, format/modifier, dimensions, plane count, offsets/strides, synchronization/fences, FD ownership, import failure, and teardown order. It must be prototyped on real hardware before adoption.

## 5. Audio ownership and lifecycle

### 5.1 Shared audio control context

The audio module holds a shared audio context with start/stop lifecycle callbacks. It owns:
- one platform audio-control object;
- the original host/default sink information;
- a flag indicating whether the first stream has changed the default sink;
- whether the default sink should be restored.

The Linux platform audio-control implementation uses the PulseAudio client API and a PulseAudio mainloop worker thread. On PipeWire desktops this generally means the PulseAudio compatibility server, not the same native PipeWire API used by the screen-capture backend.

The control object discovers or creates virtual null sinks for stereo, 5.1, and 7.1. It records module indices and unloads the virtual sinks during destruction. The destructor disconnects the PulseAudio context, waits for termination/failure, quits the mainloop, and joins its worker thread.

### 5.2 Per-session capture and Opus encoding

Each streaming session's audio thread:
1. obtains a reference to the shared audio context;
2. chooses a sink according to virtual-sink, configured-sink, and host-audio policy;
3. allows only the first active stream to change the global default sink;
4. creates a microphone capture object for the monitor source of the selected sink;
5. samples audio on the capture thread;
6. pushes samples into a bounded queue of 30 entries;
7. starts a separate Opus encoding thread, which creates a low-delay Opus multistream encoder and emits packets to the shared audio packet queue.

On shutdown, the capture thread stops the sample queue and joins the encoder thread. If capture reports reinit and auto-capture is enabled, it recreates the microphone object. If audio initialization fails, the stream is allowed to continue without audio.

### 5.3 Shared sink behavior and limitations

The default sink change is global, not per-client. The first active audio stream selects the default sink; the shared context restores the original host sink when the last reference is released if a change was made.

The Linux implementation's `is_sink_available()` currently logs that it is unimplemented and returns `true`. It therefore does not verify that a requested sink exists.

**Aphelion implication:** Do not model audio routing as a simple per-session sink field without preserving or intentionally replacing the current global sink behavior. Multi-client streams requesting different sinks need an explicit policy; current source behavior does not provide independent per-client default-sink switching.

### 5.4 Audio extraction boundary

Before extracting audio, define:
- whether audio control is process-global, host-global, or session-scoped;
- who owns the original default sink and when restoration occurs;
- virtual sink creation and module cleanup;
- per-session capture source and Opus encoder state;
- queue bounds, sample timestamps, underrun/overrun behavior;
- audio packet lifetime and per-client encryption/FEC state;
- behavior when the audio server disappears or the selected sink is removed;
- whether audio failure is fatal to the session or degrades to video-only.

## 6. Lifecycle risks to verify before extraction

These are test requirements, not confirmed bugs:

1. **Concurrent input reset:** One client's disconnect must not release another client's held keyboard/mouse state.
2. **Input teardown:** All virtual gamepads, touchscreens, pen devices, feedback threads, and global devices must disappear after the appropriate final owner exits.
3. **Capture sharing:** Removing one session must not stop a display capture source still used by another session.
4. **Encoder independence:** A failed or stopped per-session encoder must not corrupt another session consuming the shared capture source.
5. **DMA-BUF ownership:** Each FD must have one clear owner and be closed exactly once, including error/reinit paths.
6. **GPU destruction order:** CUDA stream/registrations, VA surfaces/frames, EGL/GL objects, GBM devices, and driver handles must be destroyed in valid dependency order.
7. **Portal cache:** One capture consumer exiting must not close the cached portal FD used by another consumer.
8. **PipeWire buffers:** Producer buffers must be returned/recycled correctly; any memory frame retained by the encoder must be an owned snapshot.
9. **Audio restoration:** The original default sink must be restored after the last audio stream, including partial initialization failure and overlapping streams.
10. **Audio server loss:** A PulseAudio/PipeWire-Pulse restart must not leave a hung audio thread or prevent video-only operation.
11. **Last-session behavior:** Display configuration, application lifecycle, input state, audio state, and shared broadcaster resources must be cleaned only when their actual final owner exits.

## 7. Recommended logical subsystem ownership map for Aphelion

This is the ownership model to document and test inside the Thick Worker. It does not require creating separate processes in Milestone 1.

| Logical subsystem | Owns | Must not implicitly own |
|---|---|---|
| Input service | Device state, permission-filtered input, per-client mappings, feedback routing, release/reset | Display capture or encoder state |
| Capture source | Display/portal/DRM/Wayland/X11/NvFBC handles, frame acquisition, source geometry, source buffer references | Client crypto or packetization |
| Frame/buffer manager | Frame metadata, buffer/FD ownership, synchronization, queue/reference lifetime | Client session identity hidden in opaque pointers |
| Encoder session | Codec configuration, encoder context, reference frames, per-session output packets | Capture-source lifetime unless explicitly shared/ref-counted |
| Audio control | Host sink discovery, virtual sinks, default-sink policy/restoration | Per-client audio packet encryption |
| Audio session | Capture source, sample queue, Opus encoder, audio timestamps | Global sink ownership unless explicitly delegated |
| Client gateway/protocol adapter | Client authentication, protocol state, per-client encryption, FEC, packetization, pacing, network sockets | GPU capture handles or global host input device ownership |
| Supervisor (future Rust process) | Worker lifecycle, configuration, health, restart/backoff, local control IPC | Direct ownership of C++/GPU objects in the Thick Worker |

## 8. Source references (pinned to audited commit)

- [input.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/input.cpp) — process-global platform input, per-client contexts, permissions, reset and gamepad allocation.
- [inputtino_common.h](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/input/inputtino_common.h) — global mouse/keyboard/gamepad and per-client touch/pen ownership.
- [inputtino_gamepad.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/input/inputtino_gamepad.cpp) — controller selection, feedback callbacks, and destruction.
- [kwin_headless.h](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/kwin_headless.h) — private headless session and fake input.
- [video.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/video.cpp) — shared capture and per-session encode contexts.
- [x11grab.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/x11grab.cpp) — X11/XShm capture.
- [wayland.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/wayland.cpp) and [wlgrab.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/wlgrab.cpp) — Wayland screencopy and DMA-BUF ownership.
- [portalgrab.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/portalgrab.cpp) — portal session cache, PipeWire lifecycle and buffer ownership.
- [kmsgrab.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/kmsgrab.cpp) — DRM/KMS capture and framebuffer export.
- [graphics.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/graphics.cpp) and [graphics.h](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/graphics.h) — EGL/GL objects and DMA-BUF image descriptors.
- [cuda.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/cuda.cpp) — CUDA/GL interop and NvFBC.
- [vaapi.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/vaapi.cpp) — VAAPI/DRM/EGL ownership and FFmpeg hardware context.
- [audio.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/audio.cpp) and [Linux audio.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/platform/linux/audio.cpp) — shared audio context, per-session capture/Opus, PulseAudio control and sink lifecycle.
- [stream.cpp](https://github.com/NotADeveloper4665/Syzygy/blob/c50d417c774320632cbe6fa6fc1f1a6bdfc8c58d/src/stream.cpp) — per-client session state and shared broadcaster lifecycle.

## 9. Audit completion boundary

This completes the source-level ownership map for the requested Linux input, display capture, GPU resource, and audio areas at the pinned commit. It does **not** complete:
- runtime validation on physical hardware;
- a line-by-line audit of every platform capture implementation and error path;
- a full input concurrency/race audit;
- a GPU-driver compatibility matrix;
- a complete security review of device permissions and portal tokens;
- a complete dependency/license audit.

Those are assigned to the Milestone 0 verification plan. Do not start subsystem extraction solely on the strength of this source audit.
