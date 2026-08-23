#!/bin/sh
# Verify one release version across the dispatch input, Cargo workspace, and Tauri.
set -eu

requested="${1:?usage: scripts/check-version.sh vX.Y.Z}"
requested="${requested#v}"

cargo_version=$(
  awk '
    /^\[workspace\.package\]$/ { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && /^version[[:space:]]*=/ {
      value = $0
      sub(/^[^=]*=[[:space:]]*"/, "", value)
      sub(/"[[:space:]]*$/, "", value)
      print value
      exit
    }
  ' Cargo.toml
)
tauri_version=$(
  sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    crates/app/tauri.conf.json | sed -n '1p'
)

if [ -z "$cargo_version" ] || [ -z "$tauri_version" ]; then
  echo "error: could not read Cargo or Tauri version" >&2
  exit 1
fi

if [ "$requested" != "$cargo_version" ] || [ "$requested" != "$tauri_version" ]; then
  echo "error: version mismatch" >&2
  echo "  requested: $requested" >&2
  echo "  Cargo.toml: $cargo_version" >&2
  echo "  tauri.conf.json: $tauri_version" >&2
  exit 1
fi

echo "Version $requested matches Cargo and Tauri."
