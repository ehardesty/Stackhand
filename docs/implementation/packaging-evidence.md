# Packaging evidence

This is prototype evidence for GitHub issue #10. It records the current build
boundary. It does not declare a released or supported platform.

## Pinned inputs

The repository pins the contributor Rust toolchain in
[`rust-toolchain.toml`](../../rust-toolchain.toml). Native build inputs are in
[`packaging/build-metadata.toml`](../../packaging/build-metadata.toml):

- Rust `1.93.0`;
- Zig `0.15.2`;
- Ghostty commit `bebca84668947bfc92b9a30ed58712e1c34eee1d`;
- `libghostty-vt` and `libghostty-vt-sys` crate `0.1.1`, from upstream revision
  `86338c17d1926c7e863f3c94d70202d3d1d60172`;
- `ratatui-ghostty` crate `0.2.0`, from upstream revision
  `be6d2d89105ca8cb867e7963888f82ba58b52316`;
- native option `-Demit-lib-vt`;
- the release profile uses `strip = true` and `lto = "thin"`.

`Cargo.lock` pins the complete Rust dependency graph. The native build script
records the Ghostty commit in its build output, and `scripts/package.sh`
rejects a different commit.

## Build and package command

Use the pinned Zig toolchain first on `PATH`:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" ./scripts/package.sh
```

The script runs `cargo build --locked --release`, copies the binary and the
native library into one archive, and writes a launcher that sets the platform
library path to that bundled library. The launcher does not call Zig. It does
not search for a separately installed Ghostty library.

The package contains a SHA-256 manifest. The script also writes platform
metrics next to the archive. The metrics include native build time, binary
size, native library size, archive size, and archive hash.

## Current platform evidence

| Target | Result | Evidence boundary |
| --- | --- | --- |
| macOS arm64 (`aarch64-apple-darwin`) | Build and package path passed on 2026-08-24 with the pinned Rust `1.93.0` and Zig `0.15.2`. | First native build: 28 s; incremental package build: 3 s; binary: 1,106,736 bytes; native library: 5,378,440 bytes; final archive: 3,868,182 bytes; archive SHA-256: `e7284255b0b5bee021d8ec4d855db65cf882022d7582638e5db7cd5546d4a392`. Launch-to-quit PTY smoke timing: 2.05 s (three identical runs). This timing includes PTY setup, first frame, and Ctrl-Q shutdown. |
| Linux x86-64 (`x86_64-unknown-linux-gnu`) | Not verified on this macOS arm64 host. | A Linux x86-64 clean-checkout build must run in Linux CI or a Linux machine before this target can be called supported. The script accepts the target through `STACKHAND_TARGET`. |
| macOS x86-64 (`x86_64-apple-darwin`) | Not verified. | Requires a cross-target toolchain and a separate runtime smoke test. Do not describe it as supported. |
| Linux arm64 (`aarch64-unknown-linux-gnu`) | Not verified. | Requires a Linux arm64 runtime smoke test. Do not describe it as supported. |

The current macOS environment used Rust `1.94.0` and Zig `0.15.2` before the
repository toolchain pin was added. That is useful exploratory evidence, not a
fresh-checkout acceptance result for the pinned Rust toolchain.

The macOS archive hash was identical across two consecutive package runs. This
checks the package script's normalized archive metadata and payload manifest;
it does not replace a clean-checkout build on another machine.

## Runtime dependency check

The packaged artifact is a directory and a `.tar.gz` archive. The archive
contains `bin/stackhand-bin`, the matching `libghostty-vt` library, and a
launcher in `bin/stackhand`. A runtime smoke test should use a clean `PATH`
without `zig` and without `GHOSTTY_SOURCE_DIR`:

```sh
env -i PATH="/usr/bin:/bin:/usr/sbin:/sbin" TERM=xterm-256color \
  SHELL=/bin/sh ./dist/stackhand-aarch64-apple-darwin/bin/stackhand
```

Press `Ctrl-Q` to leave the prototype. If the bundled library is absent, the
smoke test must fail. With the package present, no Zig or external Ghostty
library is used.

## Known friction and remaining proof

- The upstream crate fetches the pinned Ghostty source during the first build.
  A clean online checkout is reproducible by revision. An offline build still
  needs a local checkout supplied through `GHOSTTY_SOURCE_DIR`.
- Linux x86-64 cannot be proven on this macOS arm64 host. A Linux runner must
  record build time, startup time, binary size, native library size, and
  archive size before the platform is considered validated.
- The package script records a manual startup-time boundary, not a synthetic
  benchmark. Use the PTY smoke test and record launch-to-first-frame timing in
  the metrics file when measuring a platform.
- This is prototype packaging evidence. It does not set a release boundary or
  a supported-product platform list.
