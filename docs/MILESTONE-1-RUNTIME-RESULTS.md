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

- Capture the supervisor's observed `Stopped` event when the Moonlight client disconnects, then verify a clean host shutdown.
- Repeat connection and disconnect cycles and check for orphan processes or stuck sessions.
- Compare the same Syzygy configuration when launched directly and through Aphelion for behavior regressions.
- Record the exact Syzygy revision, host configuration, capture path, and encoder details for a reproducible result.
- Complete the remaining Milestone 0 baseline rows; this smoke test does not establish the pinned baseline, hardware matrix, performance results, or long-run reliability.

Milestone 1 has passed its first real-stream smoke check, but its full exit criteria are not yet met.
