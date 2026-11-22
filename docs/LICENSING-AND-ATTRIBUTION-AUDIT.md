# Licensing and Attribution Audit — Initial Findings

**Status:** Partial source audit; not legal advice  
**Date:** 2026-11-22  
**Source:** NotADeveloper4665/Syzygy, current main branch

## Verified findings

1. Syzygy's repository contains a GPL version 3 license text in LICENSE.
2. Syzygy's top-level CMakeLists.txt sets PROJECT_LICENSE to GPL-3.0-only.
3. Syzygy's README states that the project is distributed under GPLv3 and that upstream copyright notices and dependency licenses are retained.
4. The repository uses Git submodules for numerous dependencies and supporting projects. The current .gitmodules includes, among others:
   - moonlight-common-c (ClassicOldSong fork);
   - inputtino;
   - libdisplaydevice;
   - nv-codec-headers;
   - nvapi-open-source-sdk;
   - Simple-Web-Server;
   - TPCircularBuffer;
   - tray;
   - wayland-protocols and wlr-protocols;
   - googletest;
   - Flatpak build tools/shared modules.
5. Syzygy's README attributes its adapted word list to the EFF Large Wordlist under CC BY 3.0.

## Consequences for Aphelion

- Treat copied or modified Syzygy code as subject to the applicable GPLv3 obligations. The top-level project metadata specifically says GPL-3.0-only; do not assume a different GPL variant.
- Preserve copyright headers, upstream notices, and license texts for inherited code.
- Do not copy a dependency or submodule into Aphelion merely because Syzygy uses it. Determine its exact upstream revision, license, notices, and compatibility with Aphelion's distribution model.
- Keep a software bill of materials or equivalent dependency inventory, including transitive dependencies and generated/bundled assets.
- Preserve required attribution for the EFF-derived word list if that asset or derivative is inherited.
- Do not add an Aphelion LICENSE file or claim a final licensing policy until the maintainer has chosen the project's distribution plan and the inherited-code/dependency audit is complete.

## Not yet verified

This is not a complete dependency-license audit. The individual license texts and exact pinned revisions of all submodules and transitive dependencies have not been reviewed. Packaging assets, generated files, fonts, icons, documentation images, and platform-specific bundled components may have additional terms. The current submodule branch names in .gitmodules are not immutable revision pins; inspect the actual gitlink commits in the source tree before recording dependency versions.

## Rust streamer dependencies added

- `aes-gcm` 0.11.x (RustCrypto): upstream declares Apache-2.0 OR MIT. The crate's security notes describe one NCC Group audit; Aphelion still needs its own usage review and key-lifecycle tests.
- `reed-solomon-erasure` 6.x: upstream declares MIT. This is used as a generic FEC primitive; Aphelion has not copied Syzygy's implementation.

Cargo currently uses exact direct-dependency versions for these two crates. Transitive dependency inventory and a checked-in lockfile remain follow-up work before distribution.

## Required follow-up before redistribution

1. Record the exact Syzygy source commit and all submodule gitlink commits used as the inheritance baseline.
2. Inventory each inherited dependency and bundled asset with its exact version/revision, license identifier, required notices, and source location.
3. Verify whether any files contain additional upstream copyright or license headers that affect redistribution.
4. Decide Aphelion's license only after the maintainer confirms the intended code provenance and distribution model.
5. Include required license texts, notices, attribution, and corresponding source availability in any distribution where required.

## Source references

- [Syzygy LICENSE](https://github.com/NotADeveloper4665/Syzygy/blob/main/LICENSE)
- [Syzygy CMakeLists.txt](https://github.com/NotADeveloper4665/Syzygy/blob/main/CMakeLists.txt)
- [Syzygy README — upstream and license](https://github.com/NotADeveloper4665/Syzygy/blob/main/README.md#upstream-and-license)
- [Syzygy .gitmodules](https://github.com/NotADeveloper4665/Syzygy/blob/main/.gitmodules)
