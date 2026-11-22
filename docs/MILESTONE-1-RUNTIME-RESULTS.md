# Milestone 1 Runtime Results

## First real Moonlight client smoke test

**Observed:** 2026-11-22, from the maintainer-provided host screenshot.  
**Aphelion bundle:** commit `42b70c0870f81653a93e4c486ab5e07f4ee50dac`.  
**Test type:** Manual runtime observation on the maintainer's Linux desktop with a Moonlight client on a phone. This is not a CI test.

The screenshot shows the Aphelion supervisor managing the existing Syzygy process while a client is connected:

- Syzygy logged a new streaming session and one active session.
- The supervisor observed a `Started` session event and subsequent health responses reported `client_session_count: 1` while `process_running: true`.
- Syzygy initialized the `hevc_nvenc` encoder and connected its audio path to PipeWire.
- The phone displayed the live desktop stream with Moonlight's on-screen controls.

This confirms the first real client connection and active stream through the supervisor-managed Syzygy process. The adapter still reports `stream_ready: false` for `host.start`; that field means the host start request alone cannot guarantee a stream. Client session events and the observed health count provide the stream activity evidence instead.

## Remaining acceptance work

- Current adapter shutdown invokes `-stop` on the configured Syzygy binary, then signals and reaps the foreground process group. GitHub Actions now sends SIGINT and verifies this sequence with a stub. The real Syzygy shutdown path remains unconfirmed on the target host.
- The later runtime attempt observed the supervisor's `Stopped` event when the Moonlight client disconnected, but the user then terminated matching processes by name; that is manual cleanup, not a graceful-shutdown result.
- Repeat connection and disconnect cycles and check for orphan processes or stuck sessions.
- Compare the same Syzygy configuration when launched directly and through Aphelion for behavior regressions.
- Record the exact Syzygy revision, host configuration, capture path, and encoder details for a reproducible result.
- Complete the remaining Milestone 0 baseline rows; this smoke test does not establish the pinned baseline, hardware matrix, performance results, or long-run reliability.

Milestone 1 has passed its first real-stream smoke check, but its full exit criteria are not yet met.

## Follow-up runtime attempt

**Observed:** 2026-11-22, from the maintainer-provided terminal output and clarification.  
**Aphelion bundle:** commit `d88766f89ebffa83438663badb6c0ae305b3d2ca`.  
**Syzygy:** version `0.0.9`, commit `2f1fb5ea140ab517fc7413655e9d0b3076d3c825`.

The phone connected and streamed at 1920x1080 using `hevc_nvenc`; Syzygy initialized PipeWire/Opus audio. Aphelion observed the client `Started` and `Stopped` events. Syzygy reported screen-cast-only portal mode after device selection failed, so input behavior was not established. The user stopped remaining matching processes with a name-based kill command after the run. The terminal output did not establish that Ctrl+C caused Aphelion's graceful shutdown path or that all descendants were reaped by the supervisor.

This confirms streaming and client-disconnect observation through the managed host. It does not confirm clean real-host shutdown. GitHub Actions verifies the SIGINT shutdown sequence against a stub process; the next target-host check should stop Aphelion with Ctrl+C and inspect whether any Syzygy process remains.
