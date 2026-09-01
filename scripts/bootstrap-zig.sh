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
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  else
    echo "required command not found: sha256sum or shasum" >&2
    exit 1
  fi
}

required_command awk

expected_zig=$(metadata_value zig_version)
download_base_url=$(metadata_value zig_download_base_url)
if [ -z "$expected_zig" ] || [ -z "$download_base_url" ]; then
  echo "Zig download metadata is incomplete: $metadata" >&2
  exit 1
fi

host_os=$(uname -s)
host_arch=$(uname -m)
case "$host_os:$host_arch" in
  Darwin:arm64)
    zig_platform='aarch64-macos'
    sha_key='zig_aarch64_macos_sha256'
    ;;
  Darwin:x86_64)
    zig_platform='x86_64-macos'
    sha_key='zig_x86_64_macos_sha256'
    ;;
  Linux:aarch64 | Linux:arm64)
    zig_platform='aarch64-linux'
    sha_key='zig_aarch64_linux_sha256'
    ;;
  Linux:x86_64 | Linux:amd64)
    zig_platform='x86_64-linux'
    sha_key='zig_x86_64_linux_sha256'
    ;;
  *)
    echo "unsupported host for the pinned Zig toolchain: $host_os $host_arch" >&2
    echo "Supported hosts are macOS and Linux on arm64 or x86-64." >&2
    exit 1
    ;;
esac

expected_sha256=$(metadata_value "$sha_key")
if [ -z "$expected_sha256" ]; then
  echo "missing $sha_key in $metadata" >&2
  exit 1
fi

if [ -n "${STACKHAND_ZIG:-}" ]; then
  override_zig=$(command -v "$STACKHAND_ZIG" 2>/dev/null || true)
  if [ -z "$override_zig" ] || [ ! -x "$override_zig" ]; then
    echo "STACKHAND_ZIG is not an executable Zig binary: $STACKHAND_ZIG" >&2
    exit 1
  fi
  actual_zig=$("$override_zig" version 2>/dev/null || true)
  if [ "$actual_zig" != "$expected_zig" ]; then
    echo "STACKHAND_ZIG version mismatch: expected $expected_zig, found $actual_zig" >&2
    exit 1
  fi
  printf '%s\n' "$override_zig"
  exit 0
fi

archive_root="zig-$zig_platform-$expected_zig"
archive_name="$archive_root.tar.xz"
archive_url="$download_base_url/$expected_zig/$archive_name"

if [ -z "${HOME:-}" ] && [ -z "${STACKHAND_TOOL_CACHE:-}" ]; then
  echo "HOME is not set; set STACKHAND_TOOL_CACHE to choose the Zig cache directory" >&2
  exit 1
fi

if [ "$host_os" = 'Darwin' ]; then
  default_cache="${HOME:-}/Library/Caches/stackhand"
else
  default_cache="${XDG_CACHE_HOME:-${HOME:-}/.cache}/stackhand"
fi
cache_root=${STACKHAND_TOOL_CACHE:-$default_cache}
archive="$cache_root/downloads/$archive_name"
zig_dir="$cache_root/zig/$archive_root"
zig_bin="$zig_dir/zig"

if [ -x "$zig_bin" ]; then
  actual_zig=$("$zig_bin" version 2>/dev/null || true)
  if [ "$actual_zig" = "$expected_zig" ]; then
    printf '%s\n' "$zig_bin"
    exit 0
  fi
  rm -rf "$zig_dir"
fi

if command -v zig >/dev/null 2>&1; then
  system_zig=$(command -v zig)
  actual_zig=$("$system_zig" version 2>/dev/null || true)
  if [ "$actual_zig" = "$expected_zig" ]; then
    printf '%s\n' "$system_zig"
    exit 0
  fi
fi

mkdir -p "$cache_root/downloads" "$cache_root/zig"

if [ -f "$archive" ]; then
  actual_sha256=$(sha256 "$archive")
  if [ "$actual_sha256" != "$expected_sha256" ]; then
    echo "discarding checksum-mismatched Zig archive: $archive" >&2
    rm -f "$archive"
  fi
fi

if [ ! -f "$archive" ]; then
  required_command curl
  temporary_archive="$archive.tmp.$$"
  rm -f "$temporary_archive"
  echo "Downloading Zig $expected_zig for $zig_platform" >&2
  curl -fL --retry 3 --output "$temporary_archive" "$archive_url"
  actual_sha256=$(sha256 "$temporary_archive")
  if [ "$actual_sha256" != "$expected_sha256" ]; then
    rm -f "$temporary_archive"
    echo "Zig archive checksum mismatch" >&2
    echo "  expected: $expected_sha256" >&2
    echo "  actual:   $actual_sha256" >&2
    exit 1
  fi
  mv "$temporary_archive" "$archive"
fi

if [ ! -x "$zig_bin" ]; then
  required_command tar
  extract_dir="$cache_root/zig/.${archive_root}.tmp.$$"
  rm -rf "$extract_dir"
  mkdir -p "$extract_dir"
  tar -xJf "$archive" -C "$extract_dir"
  if [ ! -x "$extract_dir/$archive_root/zig" ]; then
    rm -rf "$extract_dir"
    echo "Zig archive did not contain $archive_root/zig" >&2
    exit 1
  fi
  rm -rf "$zig_dir"
  mv "$extract_dir/$archive_root" "$zig_dir"
  rm -rf "$extract_dir"
fi

actual_zig=$("$zig_bin" version 2>/dev/null || true)
if [ "$actual_zig" != "$expected_zig" ]; then
  echo "downloaded Zig version mismatch: expected $expected_zig, found $actual_zig" >&2
  exit 1
fi

printf '%s\n' "$zig_bin"
