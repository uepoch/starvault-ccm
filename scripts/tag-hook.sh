#!/bin/sh
# Blocks `git tag v*` when CHANGELOG.md has no entry for that version.
# Releases without changelog entries are silently-broken releases.
tag="$1"
case "$tag" in
  v*) ;;
  *) exit 0 ;; # non-release tags are unconstrained
esac
if ! grep -q "## \[${tag#v}\]" CHANGELOG.md 2>/dev/null; then
  echo "error: CHANGELOG.md has no '## [${tag#v}]' entry." >&2
  echo "Add the entry before tagging (or use a non-v tag)." >&2
  exit 1
fi
