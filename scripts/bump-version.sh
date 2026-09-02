#!/usr/bin/env bash
# bump-version.sh — move every version-bearing file to one new version.
#
# The reality check of 2026-09-01 found README.md, .claude-plugin/plugin.json,
# and the packaging manifests at 0.5.2 while Cargo.toml was 0.5.7. This script
# is the single place a release bump happens, and tests/package_manifests.rs
# (`test_version_metadata_matches_cargo`) fails when the files drift again.
#
# Usage:
#   scripts/bump-version.sh <new-version> [--dry-run]
#
# Touches, by exact anchored line replacement (no regex sweeps over code):
#   Cargo.toml                    version = "…"           (the [package] line)
#   Cargo.lock                    via `cargo update -p beads_rust --offline`
#   README.md                     the `# br <version>` line under "Verify Installation"
#   .claude-plugin/plugin.json    "version": "…"
#   packaging/homebrew/br.rb      version "…"
#   packaging/scoop/br.json       "version": "…" and the release download URLs
#   packaging/aur/PKGBUILD        pkgver=…
#   CHANGELOG.md                  inserts a "## v<version> -- <date> (Unreleased)" stub
#                                 above the newest entry when none exists for it
#
# Package-manager manifests also carry per-asset checksums that only exist after
# the release assets are published; those stay as they are and are refreshed by
# the Update Package Manifests workflow / DSR. Exit codes: 0 ok, 2 usage, 3 a
# file did not contain its anchor (nothing is written in that case).
set -euo pipefail

NEW=${1:-}
DRY=${2:-}
if [ -z "$NEW" ] || ! printf '%s' "$NEW" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.]+)?$'; then
  echo "usage: scripts/bump-version.sh <new-version> [--dry-run]" >&2
  exit 2
fi
root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$root"

OLD=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
[ -n "$OLD" ] || { echo "bump-version: could not read the current version from Cargo.toml" >&2; exit 3; }
echo "bump-version: $OLD -> $NEW${DRY:+ (dry run)}"

# replace_line FILE ANCHOR_PATTERN NEW_LINE  — exactly one line must match the anchor.
replace_line() {
  local file=$1 pattern=$2 new_line=$3
  local count
  count=$(grep -cE "$pattern" "$file" || true)
  if [ "$count" -ne 1 ]; then
    echo "bump-version: expected exactly one line matching /$pattern/ in $file, found $count" >&2
    exit 3
  fi
  if [ -n "$DRY" ]; then
    echo "  $file: $(grep -E "$pattern" "$file" | sed 's/^[[:space:]]*//') -> $new_line"
    return 0
  fi
  local tmp
  tmp=$(mktemp "$file.bump.XXXXXX")
  awk -v pat="$pattern" -v repl="$new_line" '
    $0 ~ pat && !done { match($0, /^[ \t]*/); print substr($0, 1, RLENGTH) repl; done = 1; next }
    { print }
  ' "$file" > "$tmp"
  mv "$tmp" "$file"
  echo "  $file: updated"
}

replace_line Cargo.toml '^version = "[^"]+"$' "version = \"$NEW\""
replace_line README.md '^# br [0-9]+\.[0-9]+\.[0-9]+' "# br $NEW"
replace_line .claude-plugin/plugin.json '^[[:space:]]*"version": "[^"]+",?$' "\"version\": \"$NEW\","
replace_line packaging/homebrew/br.rb '^[[:space:]]*version "[^"]+"$' "version \"$NEW\""
replace_line packaging/aur/PKGBUILD '^pkgver=' "pkgver=$NEW"
replace_line packaging/scoop/br.json '^[[:space:]]*"version": "[^"]+",?$' "\"version\": \"$NEW\","

# Scoop URLs embed the tag and asset version literally. The manifest may lag
# Cargo.toml (it is refreshed after release assets exist), so the version to
# replace is read from the manifest's own download URL, not from Cargo.toml.
SCOOP_OLD=$(grep -oE 'releases/download/v[0-9]+\.[0-9]+\.[0-9]+[^/]*/' packaging/scoop/br.json | head -1 | sed 's#releases/download/v##; s#/$##')
if [ -n "$SCOOP_OLD" ] && [ "$SCOOP_OLD" != "$NEW" ]; then
  if [ -z "$DRY" ]; then
    tmp=$(mktemp packaging/scoop/br.json.bump.XXXXXX)
    sed "s#releases/download/v$SCOOP_OLD/#releases/download/v$NEW/#g; s#br-$SCOOP_OLD-#br-$NEW-#g" packaging/scoop/br.json > "$tmp"
    mv "$tmp" packaging/scoop/br.json
    echo "  packaging/scoop/br.json: download URLs moved from v$SCOOP_OLD to v$NEW"
  else
    echo "  packaging/scoop/br.json: download URLs would move from v$SCOOP_OLD to v$NEW"
  fi
fi

# CHANGELOG stub above the newest versioned entry.
if ! grep -qE "^## v$NEW( |$)" CHANGELOG.md; then
  if [ -z "$DRY" ]; then
    today=$(date -u +%F)
    tmp=$(mktemp CHANGELOG.md.bump.XXXXXX)
    awk -v ver="$NEW" -v day="$today" '
      /^## v[0-9]+\.[0-9]+\.[0-9]+/ && !done { print "## v" ver " -- " day " (Unreleased)"; print ""; print "- (describe the changes in this release)"; print ""; done = 1 }
      { print }
    ' CHANGELOG.md > "$tmp"
    mv "$tmp" CHANGELOG.md
    echo "  CHANGELOG.md: stub entry added for v$NEW"
  else
    echo "  CHANGELOG.md: would add a stub entry for v$NEW"
  fi
fi

if [ -z "$DRY" ]; then
  if cargo update -p beads_rust --offline >/dev/null 2>&1; then
    echo "  Cargo.lock: updated"
  else
    echo "bump-version: 'cargo update -p beads_rust --offline' failed; run it manually before committing" >&2
  fi
fi

echo "bump-version: done. Review with: git diff -- Cargo.toml Cargo.lock README.md .claude-plugin/plugin.json packaging CHANGELOG.md"
