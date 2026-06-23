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

CURRENT_TOML="$(cargo metadata --no-deps --format-version 1 \
  | jq -r --arg c "$CRATE" '.packages[] | select(.name==$c) | .version')"
LATEST_TAG="$(git tag -l "${CRATE}-v*" --sort=-v:refname | head -1)"
CURRENT_TAG=""
if [[ -n "$LATEST_TAG" ]]; then
  CURRENT_TAG="${LATEST_TAG#${CRATE}-v}"
fi

if [[ -n "$CURRENT_TAG" ]]; then
  CURRENT="$(printf '%s\n%s\n' "$CURRENT_TOML" "$CURRENT_TAG" | sort -V | tail -1)"
else
  CURRENT="$CURRENT_TOML"
fi

if [[ "$CURRENT" != "$CURRENT_TOML" ]]; then
  echo "Note: latest tag (${CRATE}-v${CURRENT_TAG}) is ahead of Cargo.toml (${CURRENT_TOML}); bumping from ${CURRENT}"
fi

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

echo "tag=${TAG}" >> "${GITHUB_OUTPUT:-/dev/null}"
echo "Released ${CRATE} v${NEW_VERSION} (tag ${TAG})"
