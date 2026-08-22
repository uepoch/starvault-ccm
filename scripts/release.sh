#!/bin/sh
# Release a version: verifies the changelog entry, then dispatches the
# release workflow on main (it creates the tag and GitHub release itself).
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
git push origin HEAD:main
gh workflow run release.yml --ref main -f version="$tag"
echo "Dispatched release $tag — watch it with 'gh run watch' or the Actions tab."
