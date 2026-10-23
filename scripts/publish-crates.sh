#!/usr/bin/env bash
# Publish every workspace crate whose manifest version is not on crates.io yet.
#
# `cargo publish --workspace` orders the crates and waits for each to reach the
# index. Crates already published at this version are excluded, so a re-run
# after a partial publish picks up where it stopped.
#
# Usage:
#   scripts/publish-crates.sh [cargo publish flags, e.g. --dry-run]
set -euo pipefail

command -v jq >/dev/null 2>&1 || { echo "error: jq is required." >&2; exit 1; }

total=0
excludes=()
while IFS=$'\t' read -r name version; do
  total=$((total + 1))
  lower="$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')"
  # Sparse-index path for names of four or more characters; every crate here is.
  url="https://index.crates.io/${lower:0:2}/${lower:2:2}/$lower"
  # A 404 (never published) or a network error leaves the crate in; cargo then
  # publishes it or refuses on a duplicate, and either is loud.
  if curl -fsS "$url" 2>/dev/null | jq -e --arg v "$version" 'select(.vers == $v)' >/dev/null; then
    echo "$name $version is already on crates.io; skipping"
    excludes+=(--exclude "$name")
  fi
done < <(cargo metadata --no-deps --format-version 1 --locked \
  | jq -r '.packages[] | select(.publish != []) | [.name, .version] | @tsv')

if [ "${#excludes[@]}" -eq $((total * 2)) ]; then
  echo "all $total crates are already on crates.io; nothing to publish"
  exit 0
fi

cargo publish --workspace --locked "${excludes[@]}" "$@"
