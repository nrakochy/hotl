#!/usr/bin/env bash
# Cut a release: promote the changelog, bump the workspace version, commit,
# tag, and push. The vX.Y.Z tag triggers the crates.io publish + prebuilt-
# binary workflows.
#
# The changelog is part of the release, not an afterthought: `## [Unreleased]`
# becomes `## [X.Y.Z] - <today>` with a fresh empty `[Unreleased]` above it.
# v0.3.0 shipped without its section and had to be backfilled by hand
# (c0dd281) — hence the hard check below rather than a reminder in a doc.
#
# Usage:
#   scripts/release.sh patch      # 0.1.0 -> 0.1.1
#   scripts/release.sh minor      # 0.1.0 -> 0.2.0   (breaking, while pre-1.0)
#   scripts/release.sh major      # 0.1.0 -> 1.0.0
#   scripts/release.sh 0.4.2      # set an explicit version
set -euo pipefail

cd "$(dirname "$0")/.."

[ $# -eq 1 ] || { echo "usage: $0 <patch|minor|major|X.Y.Z>" >&2; exit 1; }

# Clean tree required — a release must reflect committed state.
if [ -n "$(git status --porcelain)" ]; then
  echo "error: working tree is dirty; commit or stash first." >&2
  exit 1
fi

current="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "(.*)"/\1/')"
IFS=. read -r major minor patch <<<"$current"

case "$1" in
  patch) new="$major.$minor.$((patch + 1))" ;;
  minor) new="$major.$((minor + 1)).0" ;;
  major) new="$((major + 1)).0.0" ;;
  [0-9]*.[0-9]*.[0-9]*) new="$1" ;;
  *) echo "error: expected patch|minor|major|X.Y.Z, got '$1'" >&2; exit 1 ;;
esac

tag="v$new"
if git rev-parse "$tag" >/dev/null 2>&1; then
  echo "error: tag $tag already exists." >&2
  exit 1
fi

# --- changelog checks, before anything is edited --------------------------
# Everything below this point mutates files, so every reason to refuse has
# to be established first: a half-bumped tree is worse than no release.

if ! grep -q '^## \[Unreleased\]' CHANGELOG.md; then
  echo "error: CHANGELOG.md has no '## [Unreleased]' heading to promote." >&2
  echo "hint: add one above the newest version section." >&2
  exit 1
fi

# "Has content" = a non-blank line between [Unreleased] and the next `## [`.
if ! awk '
    /^## \[Unreleased\]/ { inside = 1; next }
    /^## \[/             { inside = 0 }
    inside && NF         { found = 1 }
    END                  { exit !found }
  ' CHANGELOG.md; then
  echo "error: '## [Unreleased]' is empty — refusing to tag a release with no notes." >&2
  echo "hint: describe the change under it. For a no-op bump, say so explicitly," >&2
  echo "      e.g. '- Maintenance release: dependency bumps only.'" >&2
  exit 1
fi

# A section for this version must not already exist (re-running after a
# partial failure, or an entry written by hand).
if grep -q "^## \[$new\]" CHANGELOG.md; then
  echo "error: CHANGELOG.md already has a '## [$new]' section." >&2
  exit 1
fi

echo "releasing $current -> $new"

# Bump the first bare `version = "..."` line — the [workspace.package] one,
# leaving dependency version fields untouched. awk for portable in-place edit.
awk -v v="$new" '
  !done && /^version = "/ { sub(/"[^"]*"/, "\"" v "\""); done=1 }
  { print }
' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml

# Sync every internal path-dep pin (`{ path = "../x", version = "…" }`) to the
# same version — the workspace publishes in lockstep, and a stale pin makes
# cargo reject the local crate (or worse, resolve an old one from crates.io).
for f in crates/*/Cargo.toml; do
  sed -E 's#(path = "\.\./[^"]+", version = ")[0-9]+\.[0-9]+\.[0-9]+(")#\1'"$new"'\2#g' \
    "$f" > "$f.tmp" && mv "$f.tmp" "$f"
done

# The docs hero's crates.io button carries the version literally.
sed -E 's/(text: crates\.io v)[0-9]+\.[0-9]+\.[0-9]+/\1'"$new"'/' \
  site/src/content/docs/index.mdx > site/src/content/docs/index.mdx.tmp \
  && mv site/src/content/docs/index.mdx.tmp site/src/content/docs/index.mdx

# Promote the changelog: what was unreleased is now this version, and a fresh
# empty [Unreleased] takes its place. The entries themselves are untouched —
# they simply end up under a dated heading.
awk -v v="$new" -v d="$(date +%F)" '
  !done && /^## \[Unreleased\]/ {
    print "## [Unreleased]"
    print ""
    print "## [" v "] - " d
    done = 1
    next
  }
  { print }
' CHANGELOG.md > CHANGELOG.md.tmp && mv CHANGELOG.md.tmp CHANGELOG.md

cargo build --quiet            # sync Cargo.lock to the new version
git commit -aqm "release: $tag"
git tag -a "$tag" -m "$tag"
git push --follow-tags

echo "pushed $tag — crates.io publish and release binaries are building."
