#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

command -v cargo >/dev/null 2>&1 || {
  echo "required command not found: cargo" >&2
  echo "Install Rust with rustup: https://rustup.rs" >&2
  exit 1
}

zig_bin=$("$repo_root/scripts/bootstrap-zig.sh")
zig_version=$("$zig_bin" version)
zig_dir=$(CDPATH= cd -- "$(dirname -- "$zig_bin")" && pwd)
zig_path="$zig_dir"

shell_quote() {
  quoted_value=$(printf '%s' "$1" | sed "s/'/'\\\\''/g")
  printf "'%s'" "$quoted_value"
}

macos_sdk_supports_host() {
  sdk_path=$1
  system_tbd="$sdk_path/usr/lib/libSystem.tbd"
  [ -f "$system_tbd" ] || return 1
  system_targets=$(awk '/^[[:space:]]*targets:/ { print; getline; print; exit }' "$system_tbd")

  case "$(uname -m)" in
    arm64 | aarch64)
      printf '%s\n' "$system_targets" | grep -q 'arm64-macos'
      ;;
    x86_64 | amd64)
      printf '%s\n' "$system_targets" | grep -q 'x86_64-macos'
      ;;
    *)
      return 1
      ;;
  esac
}

configure_macos_zig_sdk() {
  # Newer macOS SDKs use arm64e-only target names in libSystem.tbd. Zig
  # 0.15.2 cannot read those names, so use an older installed SDK for Zig.
  [ "$(uname -s)" = 'Darwin' ] || return 0
  command -v xcrun >/dev/null 2>&1 || return 0

  current_sdk=$(xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)
  if [ -n "$current_sdk" ] && macos_sdk_supports_host "$current_sdk"; then
    return 0
  fi

  selected_developer_dir=$(xcode-select -p 2>/dev/null || true)
  compatible_sdk=''
  for sdk_root in \
    "$selected_developer_dir/Platforms/MacOSX.platform/Developer/SDKs" \
    '/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs' \
    '/Library/Developer/CommandLineTools/SDKs'
  do
    [ -d "$sdk_root" ] || continue
    for sdk_path in "$sdk_root"/MacOSX*.sdk; do
      [ -d "$sdk_path" ] || continue
      if macos_sdk_supports_host "$sdk_path"; then
        compatible_sdk=$sdk_path
        break 2
      fi
    done
  done

  if [ -z "$compatible_sdk" ]; then
    if [ -n "$current_sdk" ]; then
      echo "warning: the selected macOS SDK is not compatible with Zig $zig_version" >&2
      echo "warning: no installed compatible SDK was found; Zig will use the selected SDK" >&2
    fi
    return 0
  fi

  real_xcrun=$(command -v xcrun)
  shim_dir="$repo_root/.zig-cache/xcrun"
  zig_wrapper_dir="$repo_root/.zig-cache/bin"
  mkdir -p "$shim_dir" "$zig_wrapper_dir"

  quoted_sdk=$(shell_quote "$compatible_sdk")
  quoted_real_xcrun=$(shell_quote "$real_xcrun")
  quoted_shim_dir=$(shell_quote "$shim_dir")
  quoted_zig=$(shell_quote "$zig_bin")

  cat > "$shim_dir/xcrun" <<EOF
#!/bin/sh
if [ "\$#" -eq 3 ] && [ "\$1" = '--sdk' ] && [ "\$2" = 'macosx' ] && [ "\$3" = '--show-sdk-path' ]; then
  printf '%s\\n' $quoted_sdk
  exit 0
fi
exec $quoted_real_xcrun "\$@"
EOF
  chmod 755 "$shim_dir/xcrun"

  cat > "$zig_wrapper_dir/zig" <<EOF
#!/bin/sh
PATH=$quoted_shim_dir:\$PATH
export PATH
exec $quoted_zig "\$@"
EOF
  chmod 755 "$zig_wrapper_dir/zig"
  zig_path="$zig_wrapper_dir:$zig_dir"
}

configure_macos_zig_sdk
PATH="$zig_path:$PATH"
export PATH

# Keep Zig's global cache separate from other Zig releases. Zig can reuse
# cached native objects across compiler versions, which can break macOS links.
ZIG_GLOBAL_CACHE_DIR=${STACKHAND_ZIG_GLOBAL_CACHE_DIR:-"$repo_root/.zig-cache/global"}
mkdir -p "$ZIG_GLOBAL_CACHE_DIR"
export ZIG_GLOBAL_CACHE_DIR

exec cargo "$@"
