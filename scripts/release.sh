#!/bin/sh
# Release a version: verifies the changelog entry, tags, pushes the tag.
# Usage: scripts/release.sh v1.2.3
set -e
tag="${1:?usage: scripts/release.sh vX.Y.Z}"
case "$tag" in v*) ;; *)
  echo "error: tag must start with 'v' (got '$tag')" >&2
  exit 1
  ;;
esac
if ! grep -q "## \[${tag#v}\]" CHANGELOG.md; then
  echo "error: CHANGELOG.md has no '## [${tag#v}]' entry." >&2
  echo "Write the changelog entry first; releases without one are blocked." >&2
  exit 1
fi
git tag "$tag"
git push origin "$tag"
echo "Tagged and pushed $tag — the release workflow builds and attaches the installer."
