#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

command -v cargo >/dev/null 2>&1 || {
  echo "required command not found: cargo" >&2
  echo "Install Rust with rustup: https://rustup.rs" >&2
  exit 1
}

zig_bin=$("$repo_root/scripts/bootstrap-zig.sh")
zig_dir=$(CDPATH= cd -- "$(dirname -- "$zig_bin")" && pwd)
PATH="$zig_dir:$PATH"
export PATH

# Keep Zig's global cache separate from other Zig releases. Zig can reuse
# cached native objects across compiler versions, which can break macOS links.
ZIG_GLOBAL_CACHE_DIR=${STACKHAND_ZIG_GLOBAL_CACHE_DIR:-"$repo_root/.zig-cache/global"}
mkdir -p "$ZIG_GLOBAL_CACHE_DIR"
export ZIG_GLOBAL_CACHE_DIR

exec cargo "$@"
