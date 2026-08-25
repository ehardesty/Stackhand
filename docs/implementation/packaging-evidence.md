# Packaging evidence

This is prototype evidence for GitHub issue #10. It records the current build
boundary. It does not declare a released or supported platform.

## Pinned inputs

The repository pins the contributor Rust toolchain in
[`rust-toolchain.toml`](../../rust-toolchain.toml). Native build inputs are in
[`packaging/build-metadata.toml`](../../packaging/build-metadata.toml):

- Rust `1.93.0`;
- Zig `0.15.2`;
- Ghostty commit `a887df42c56f6de86c0fe6da9c4eeca37931e083`;
- `libghostty-vt` and `libghostty-vt-sys` crate `0.2.1`, from upstream revision
  `46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0`;
- native options `-Demit-lib-vt=true`, `-Demit-xcframework=false`, and
  `-Dapp-runtime=none`;
- the release profile uses `strip = true` and `lto = "thin"`.

`Cargo.lock` pins the complete Rust dependency graph. The native build script
records the Ghostty commit in its build output, and `scripts/package.sh`
rejects a different commit.

## Build and package command

Use the pinned Zig toolchain first on `PATH`:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" ./scripts/package.sh
```

The script runs `cargo build --locked --release` and copies the binary into one
archive. The 0.2.1 binding links the vendored Ghostty static library into that
binary. The launcher does not call Zig and does not search for a separately
installed Ghostty library.

The package contains a SHA-256 manifest. The script also writes platform
metrics next to the archive. The metrics include native build time, binary
size, static native archive size, package archive size, and package hash.

## Current platform evidence

| Target | Result | Evidence boundary |
| --- | --- | --- |
| macOS arm64 (`aarch64-apple-darwin`) | Build and package path passed on 2026-08-24 with pinned Rust `1.93.0`, Zig `0.15.2`, and `libghostty-vt` `0.2.1`. | Current incremental build: 4 s; binary: 2,297,408 bytes; static native archive: 8,857,784 bytes; final package archive: 953,727 bytes; archive SHA-256: `de7ccb78dda1895641de867947546ff3575d1a0c38d0db63daba36deca8d9257`. Two consecutive package runs had the same payload hash. A clean-`PATH` real-PTY mouse fixture passed, and `otool -L` reported no Ghostty shared library. |
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
contains `bin/stackhand-bin` and a launcher in `bin/stackhand`. A runtime smoke
test should use a clean `PATH`
without `zig` and without `GHOSTTY_SOURCE_DIR`:

```sh
env -i PATH="/usr/bin:/bin:/usr/sbin:/sbin" TERM=xterm-256color \
  SHELL=/bin/sh ./dist/stackhand-aarch64-apple-darwin/bin/stackhand
```

Press `Ctrl-Q` to leave the prototype. `otool -L` on macOS or `ldd` on Linux
must not report a Ghostty shared library. No Zig or external Ghostty library is
used at runtime.

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
