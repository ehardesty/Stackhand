#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

printf '%s\n' "Stackhand dependency license inventory"
printf '%s\n' "Generated from Cargo.lock by cargo metadata --locked."
printf 'name\tversion\tlicense\tsource\n'

cargo metadata --locked --format-version 1 \
  | jq -r '.packages | sort_by(.name)[] | [.name, .version, (.license // "NO LICENSE FIELD"), (.source // "workspace")] | @tsv'
