#!/bin/sh
# Full gate: everything CI runs, locally. Fails fast on the first miss.
# Usage: scripts/check.sh [--no-webui]   (skip the webui half, e.g. no vp)
set -e
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --check --all

echo "==> cargo clippy (both crates, -D warnings)"
cargo clippy -p svccm-core -p svccm-app --all-targets -- -D warnings

echo "==> cargo test"
cargo test -p svccm-core

if [ "${1:-}" = "--no-webui" ]; then
  echo "==> webui skipped (--no-webui)"
  exit 0
fi

# vp lives in ~/.vite-plus/bin unless globally on PATH.
PATH="$HOME/.vite-plus/bin:$PATH"
command -v vp >/dev/null 2>&1 || {
  echo "error: vp not found (install: curl -fsSL https://vite.plus | sh)" >&2
  exit 1
}

echo "==> vp check (webui fmt + lint + types)"
(cd crates/app/ui && vp check)

echo "==> vp build (webui)"
(cd crates/app/ui && vp build)

echo "All gates green."
