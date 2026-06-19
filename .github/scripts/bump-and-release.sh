#!/usr/bin/env bash
# Bump a workspace crate's semver, refresh Cargo.lock, update its changelog,
# commit on main, and push the release tag (synapse-<crate>-vX.Y.Z).
set -euo pipefail

CRATE="${1:?crate name required (e.g. synapse-gateway)}"
BUMP="${2:?bump type required (patch|minor|major)}"

case "$BUMP" in
  patch | minor | major) ;;
  *)
    echo "::error::invalid bump type: $BUMP (expected patch, minor, or major)"
    exit 1
    ;;
esac

CARGO_TOML="crates/${CRATE}/Cargo.toml"
CHANGELOG="crates/${CRATE}/CHANGELOG.md"

if [[ ! -f "$CARGO_TOML" ]]; then
  echo "::error::missing $CARGO_TOML"
  exit 1
fi

git pull --rebase origin main

CURRENT="$(grep -E '^version\s*=' "$CARGO_TOML" | head -1 | sed -E 's/^version\s*=\s*"([^"]+)".*/\1/')"
IFS='.' read -r major minor patch <<< "${CURRENT%%-*}"

case "$BUMP" in
  major)
    major=$((major + 1))
    minor=0
    patch=0
    ;;
  minor)
    minor=$((minor + 1))
    patch=0
    ;;
  patch)
    patch=$((patch + 1))
    ;;
esac

NEW_VERSION="${major}.${minor}.${patch}"
TAG="${CRATE}-v${NEW_VERSION}"

if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "::error::tag $TAG already exists"
  exit 1
fi

sed -i "s/^version = \".*\"/version = \"${NEW_VERSION}\"/" "$CARGO_TOML"

TODAY="$(date -u +%Y-%m-%d)"
if [[ "$CRATE" == "synapse-gateway" ]]; then
  sed -i "/^## \[Unreleased\]/a\\
\\
## [${NEW_VERSION}] - ${TODAY}" "$CHANGELOG"
else
  sed -i "2a\\
\\
## ${NEW_VERSION}\\
" "$CHANGELOG"
fi

cargo check -p "$CRATE" -q

git add "$CARGO_TOML" Cargo.lock "$CHANGELOG"
git commit -m "chore(${CRATE}): release v${NEW_VERSION}"
git tag "$TAG"
git push origin HEAD:main "$TAG"

echo "Released ${CRATE} v${NEW_VERSION} (tag ${TAG})"
