# Aphelion Linux Runtime Prototype

This bundle is an early, installable integration harness for the Rust supervisor and an existing Syzygy host binary. It does not contain Syzygy and does not change its behavior. The supervisor owns the Syzygy process lifecycle; Syzygy continues to own Moonlight pairing, capture, encoding, audio, input, and media networking.

## Obtain the bundle

After a successful push build on `prototype/rust-control-plane`, open the repository's **Actions** tab, select the latest **Rust control-plane prototype** run, and download the artifact named `aphelion-linux-x86_64-<commit>`. GitHub downloads the artifact as a ZIP; inside it is the `.tar.gz` runtime bundle shown below. The artifact is retained for 30 days and is not a release.

The CI bundle is built on Ubuntu x86_64. Fedora and other Linux distributions have not yet been verified. The package includes `supervisor`, `syzygy-worker`, the `aphelion-syzygy` launcher, and this document.

## Run against an installed Syzygy binary

Stop any Syzygy instance or service that is already running first. The launcher owns and stops only the Syzygy process it starts; it does not take over or stop a separately launched process. Extract the archive, point the launcher at the existing Syzygy executable, and start Aphelion:

```sh
unzip aphelion-linux-x86_64-<commit>.zip
tar -xzf aphelion-linux-x86_64.tar.gz
cd aphelion-linux-x86_64
APHELION_SYZYGY_EXECUTABLE=/absolute/path/to/syzygy ./bin/aphelion-syzygy
```

To pass additional Syzygy options, provide a JSON array. The adapter always adds `-s -foreground` because it must own the host process:

```sh
APHELION_SYZYGY_EXECUTABLE=/absolute/path/to/syzygy \
APHELION_SYZYGY_ARGS='["-config","/path/to/syzygy.conf"]' \
./bin/aphelion-syzygy
```

Use the existing Syzygy configuration, pairing procedure, and Moonlight client to attempt a stream. Stop the host with Ctrl+C. The launcher reports host lifecycle and periodic worker health in the terminal. Session log tokens are converted to opaque IDs before they reach Aphelion logs or control messages.

If Moonlight still sees the host after Aphelion exits, check for a separately running host before restarting the machine:

```sh
pgrep -a -f '[s]yzygy|[a]phelion'
```

Aphelion's managed child should be gone after a graceful Ctrl+C. Any remaining Syzygy command line is a separate process or service and must be stopped through the way it was started.

## Current boundaries and acceptance

- The Unix socket carries versioned control and health messages only. Media payloads remain in Syzygy.
- `host.start` reports `stream_ready=false`; stream intent settings are not yet mapped from Aphelion into Syzygy.
- Session start/stop is observed from Syzygy output; it is not a per-client control interface.
- CI verifies supervisor-worker IPC, adapter launch, lifecycle events, graceful shutdown, restart behavior, and packaging. It cannot verify GPU capture, audio, pairing, or a real Moonlight stream.
- The Syzygy hardware/Moonlight baseline remains incomplete. Milestone 0's repository source, ownership, licensing, contract, and test-plan artifacts exist, while hardware and runtime checks remain pending. No hardware pass is claimed.
- Milestone 1 is not complete until the packaged supervisor starts and stops the real host and a Moonlight client establishes and ends a stream through it.

The first target-hardware run should record the Syzygy commit and configuration used, whether the host started, Moonlight pairing and stream outcome, capture/encoder/audio path, stop behavior, and any regressions compared with running Syzygy directly. Do not interpret a successful CI run as that acceptance evidence.
