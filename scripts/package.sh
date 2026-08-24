#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
metadata="$repo_root/packaging/build-metadata.toml"

if [ ! -f "$metadata" ]; then
  echo "missing build metadata: $metadata" >&2
  exit 1
fi

metadata_value() {
  key=$1
  awk -F '"' -v key="$key" '$0 ~ "^" key "[[:space:]]*=" { print $2; exit }' "$metadata"
}

required_command() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "required command not found: $1" >&2
    exit 1
  }
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

required_command cargo
required_command rustc
required_command zig
required_command tar
required_command gzip

expected_zig=$(metadata_value zig_version)
actual_zig=$(zig version)
if [ "$actual_zig" != "$expected_zig" ]; then
  echo "wrong Zig version: expected $expected_zig, found $actual_zig" >&2
  echo "Put the pinned Zig toolchain first on PATH and retry." >&2
  exit 1
fi

expected_rust=$(metadata_value rust_version)
actual_rust=$(rustc --version | awk '{ print $2 }')
if [ "$actual_rust" != "$expected_rust" ]; then
  echo "wrong Rust version: expected $expected_rust, found $actual_rust" >&2
  echo "The repository rust-toolchain.toml pins the expected toolchain." >&2
  exit 1
fi

target=${STACKHAND_TARGET:-$(rustc -vV | awk '/^host:/ { print $2 }')}
out_dir=${STACKHAND_PACKAGE_DIR:-"$repo_root/dist"}
target_dir=${CARGO_TARGET_DIR:-"$repo_root/target"}
case "$target_dir" in
  /*) ;;
  *) target_dir="$repo_root/$target_dir" ;;
esac

case "$target" in
  *-apple-darwin) native_glob='libghostty-vt*.dylib' ;;
  *-linux-*) native_glob='libghostty-vt.so*' ;;
  *)
    echo "unsupported packaging target: $target" >&2
    echo "Supported prototype targets are *-apple-darwin and *-linux-*." >&2
    exit 1
    ;;
esac

build_started=$(date +%s)
(cd "$repo_root" && cargo build --locked --release --target "$target")
build_finished=$(date +%s)
build_seconds=$((build_finished - build_started))

release_dir="$target_dir/$target/release"
binary="$release_dir/stackhand"
if [ ! -f "$binary" ]; then
  echo "release binary not found: $binary" >&2
  exit 1
fi

build_root="$release_dir/build"
native_lib=''
for candidate in "$build_root"/libghostty-vt-sys-*/out/ghostty-install/lib/$native_glob; do
  if [ -f "$candidate" ]; then
    native_lib=$candidate
    break
  fi
done
if [ -z "$native_lib" ]; then
  echo "native Ghostty library not found below: $build_root" >&2
  exit 1
fi

ghostty_stamp=$(dirname "$native_lib")/../../ghostty-src/.ghostty-commit
if [ ! -f "$ghostty_stamp" ]; then
  echo "Ghostty source stamp not found beside native build: $ghostty_stamp" >&2
  exit 1
fi
expected_ghostty=$(metadata_value ghostty_revision)
actual_ghostty=$(tr -d '[:space:]' < "$ghostty_stamp")
if [ "$actual_ghostty" != "$expected_ghostty" ]; then
  echo "wrong Ghostty revision: expected $expected_ghostty, found $actual_ghostty" >&2
  exit 1
fi

mkdir -p "$out_dir"
package_name="stackhand-$target"
package_root="$out_dir/$package_name"
rm -rf "$package_root"
mkdir -p "$package_root/bin" "$package_root/lib" "$package_root/share/licenses"

cp "$binary" "$package_root/bin/stackhand-bin"
cp "$native_lib" "$package_root/lib/$(basename "$native_lib")"

# The sys crate's macOS library has an @rpath install name without the
# versioned filename. Keep both names in the bundle. Linux gets the common
# SONAME aliases as plain files so the bundle does not depend on symlink
# preservation by an archive or file transfer tool.
case "$target" in
  *-apple-darwin)
    cp "$native_lib" "$package_root/lib/libghostty-vt.dylib"
    ;;
  *-linux-*)
    cp "$native_lib" "$package_root/lib/libghostty-vt.so"
    cp "$native_lib" "$package_root/lib/libghostty-vt.so.0"
    ;;
esac

cp "$repo_root/packaging/build-metadata.toml" "$package_root/share/build-metadata.toml"
cp "$repo_root/docs/implementation/native-dependency-licenses.md" \
  "$package_root/share/licenses/native-dependencies.md"

cat > "$package_root/bin/stackhand" <<'LAUNCHER'
#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
case "$(uname -s)" in
  Darwin)
    if [ -n "${DYLD_LIBRARY_PATH:-}" ]; then
      DYLD_LIBRARY_PATH="$root/lib:$DYLD_LIBRARY_PATH"
    else
      DYLD_LIBRARY_PATH="$root/lib"
    fi
    export DYLD_LIBRARY_PATH
    ;;
  Linux)
    if [ -n "${LD_LIBRARY_PATH:-}" ]; then
      LD_LIBRARY_PATH="$root/lib:$LD_LIBRARY_PATH"
    else
      LD_LIBRARY_PATH="$root/lib"
    fi
    export LD_LIBRARY_PATH
    ;;
  *) echo "unsupported packaged host: $(uname -s)" >&2; exit 1 ;;
esac
exec "$root/bin/stackhand-bin" "$@"
LAUNCHER
chmod 755 "$package_root/bin/stackhand"

# Normalize package file times before creating the archive. Build metrics are
# written outside the package, so the payload has stable metadata.
find "$package_root" -type f -exec touch -t 197001010000 {} +

manifest="$package_root/share/SHA256SUMS"
(
  cd "$package_root"
  find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort | while IFS= read -r file; do
    printf '%s  %s\n' "$(sha256 "$file")" "$file"
  done
) > "$manifest"
touch -t 197001010000 "$manifest"

archive="$out_dir/$package_name.tar.gz"
rm -f "$archive"
(
  cd "$out_dir"
  find "$package_name" -type f -print | LC_ALL=C sort | tar -cf - -T -
) | gzip -n > "$archive"

binary_bytes=$(wc -c < "$binary" | tr -d '[:space:]')
native_bytes=$(wc -c < "$native_lib" | tr -d '[:space:]')
package_bytes=$(wc -c < "$archive" | tr -d '[:space:]')
metrics="$out_dir/$package_name.metrics.txt"
{
  printf '%s\n' "target=$target"
  printf '%s\n' "rust_version=$actual_rust"
  printf '%s\n' "zig_version=$actual_zig"
  printf '%s\n' "ghostty_revision=$actual_ghostty"
  printf '%s\n' "native_build_options=$(metadata_value native_build_options)"
  printf '%s\n' "native_build_seconds=$build_seconds"
  printf '%s\n' "binary_size_bytes=$binary_bytes"
  printf '%s\n' "native_library_size_bytes=$native_bytes"
  printf '%s\n' "package_archive_size_bytes=$package_bytes"
  printf '%s\n' "package_archive_sha256=$(sha256 "$archive")"
  printf '%s\n' "startup_time=not measured by package command; use the manual PTY smoke test in packaging-evidence.md"
} > "$metrics"

printf '%s\n' "Packaged: $archive"
printf '%s\n' "Metrics:   $metrics"
