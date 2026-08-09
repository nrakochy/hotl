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
#
# Env:
#   HOTL_SKIP_LINUX_CHECK=1   accept that a Linux-only break may reach the tag
#   HOTL_SKIP_CI_WAIT=1       tag without waiting for green CI on the commit
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

# The tag is what makes a release public, and the wait below is what holds it
# back — so a missing `gh` has to be a refusal here, not a surprise after the
# changelog has already been promoted.
if [ "${HOTL_SKIP_CI_WAIT:-0}" != 1 ]; then
  command -v gh >/dev/null 2>&1 || {
    echo "error: gh is required to wait for CI before tagging." >&2
    echo "hint: install the GitHub CLI, or re-run with HOTL_SKIP_CI_WAIT=1 to" >&2
    echo "      accept a tag that no green CI run vouches for." >&2
    exit 1
  }
  gh auth status >/dev/null 2>&1 || {
    echo "error: gh is not authenticated, so CI status cannot be read." >&2
    echo "hint: run 'gh auth login', or re-run with HOTL_SKIP_CI_WAIT=1 to" >&2
    echo "      accept a tag that no green CI run vouches for." >&2
    exit 1
  }
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

# Run the same gate the publish workflow will: a test failure found here costs
# seconds; found after the push it strands the release half-out (GitHub release
# and Nix tag built, crates.io refused — v0.5.0 through v0.7.0 all did this).
echo "running the workspace tests (the publish workflow's gate) ..."
cargo test --workspace --locked --quiet

# Those tests only prove the release builds on *this* machine. Every release
# target but macOS is Linux, and the local Nix toolchain carries no Linux std
# to cross-check against — so a BSD-only libc call compiles clean here and
# fails every Linux job minutes after the tag is public (v0.8.0:
# `libc::getpeereid`). Compile for Linux in a container while refusing is still
# free.
if [ "${HOTL_SKIP_LINUX_CHECK:-0}" = 1 ]; then
  echo "skipping the Linux cross-check (HOTL_SKIP_LINUX_CHECK=1)."
elif ! docker info >/dev/null 2>&1; then
  echo "error: Docker is unavailable, so the Linux build cannot be verified." >&2
  echo "hint: start Docker, or re-run with HOTL_SKIP_LINUX_CHECK=1 to accept" >&2
  echo "      that a Linux-only break may reach the tag." >&2
  exit 1
else
  echo "checking the workspace builds on Linux ..."
  # --all-targets covers test code too; check (not test) keeps the linker out
  # of it, which is what a small Docker VM runs out of memory on.
  docker run --rm \
    -v "$PWD":/src:ro \
    -v hotl-linux-check-target:/target \
    -v hotl-linux-check-registry:/usr/local/cargo/registry \
    -w /src rust:slim \
    cargo check --workspace --locked --all-targets --target-dir /target
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

# So does llms.txt's summary line — the file agents read instead of the docs
# site. Nothing kept it in step until now, and it sat at 0.6.2 through two
# releases; a version an agent quotes back at a user has to be the real one.
sed -E 's/(^> hotl is .*Version )[0-9]+\.[0-9]+\.[0-9]+(;)/\1'"$new"'\2/' \
  site/public/llms.txt > site/public/llms.txt.tmp \
  && mv site/public/llms.txt.tmp site/public/llms.txt

if ! grep -q "Version $new;" site/public/llms.txt; then
  echo "error: llms.txt version line did not update to $new — its wording" >&2
  echo "       probably drifted from the sed above. Fix before tagging." >&2
  exit 1
fi

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

# The tag triggers publish.yml, release.yml and nix-tag.yml — all three, none
# of which waits for CI. So the commit and the tag are two separate pushes:
# send the commit, wait for CI to go green on it, and only then tag.
branch="$(git rev-parse --abbrev-ref HEAD)"
git push origin "$branch"

if [ "${HOTL_SKIP_CI_WAIT:-0}" = 1 ]; then
  echo "skipping the CI wait (HOTL_SKIP_CI_WAIT=1) — publish.yml will refuse if CI is red."
else
  bash scripts/wait-for-ci.sh "$(git rev-parse HEAD)"
fi

git tag -a "$tag" -m "$tag"
git push origin "$tag"

echo "pushed $tag — crates.io publish and release binaries are building."
