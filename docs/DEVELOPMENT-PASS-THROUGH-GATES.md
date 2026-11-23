# Development pass-through gates

Aphelion has two kinds of evidence gates. A gap in target hardware access must not stop implementation work that can be built and tested in GitHub Actions, and a hosted CI pass must not be presented as target-hardware evidence.

## Gate A — software development (automated)

A change may continue to the next software task when GitHub Actions passes the applicable checks:

- workspace compilation and Rust unit/integration tests;
- malformed, incompatible, stale-identity, and uncorrelated control-protocol cases;
- worker crash recovery, bounded restart exhaustion, shutdown, and process cleanup using test workers;
- RTSP framing/session failure cases;
- synthetic capture-to-encode-to-packetize/encrypt/FEC/UDP path;
- Linux prototype bundle construction.

The current workflow publishes an Actions summary that states this scope on every run. Failed checks are debugged and fixed before relying on that revision. Hardware-only rows remain pending, rather than being treated as failures for unrelated source changes.

## Gate B — host acceptance (manual/target-bound)

These checks remain open until observed on the target system or an equivalent real host:

- direct Syzygy versus Aphelion-managed behavior comparison;
- repeated real Moonlight connect/disconnect cycles and process-list cleanup evidence;
- supported GPU/capture backend and encoder combinations;
- actual portal permissions, audio/input lifecycle, and multi-client resource ownership;
- reproducible end-to-end latency/performance measurements.

A CI stub, synthetic frame, successful socket write, or compile result cannot close these rows.

## Milestone handling

Continue Milestone 2 reliability/observability implementation and the separate Rust streamer prototype under Gate A. Keep Milestone 0 and Milestone 1 formal acceptance status partial until their applicable Gate B evidence is recorded. The Rust streamer remains experimental until authenticated pairing, client compatibility, and real-host acceptance are demonstrated. This policy authorizes development throughput; it does not redefine milestone exit criteria.
