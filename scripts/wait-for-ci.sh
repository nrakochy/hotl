#!/usr/bin/env bash
# Block until ci.yml is green on an exact commit, or refuse.
#
# The release path's verification artifact is CI's own check-run record: it is
# keyed to the SHA, produced by the only party that can produce it, and cannot
# be forged by the thing being released. Nothing is uploaded or cached — the
# gate is a query.
#
# Usage:
#   scripts/wait-for-ci.sh <sha>
#
# Env:
#   HOTL_CI_WAIT_TIMEOUT    seconds before giving up (default 1800)
#   HOTL_CI_POLL_INTERVAL   seconds between polls (default 15)
set -euo pipefail

# Filtered to a named set, not "everything green": check runs attach to the SHA,
# so publish.yml's own `publish` run shows up here and an all-green gate would
# wait on itself.
#
# ci.yml job ids. The nix legs are deliberately absent: master-only, often
# skipped, chronically red on darwin, and re-verified by nix-tag.yml (non-fatal).
# harness-windows runs on every push and is the only leg that exercises the
# platform's real syscalls, so a red Windows suite must block a release — it was
# missing here, which is how v0.12.0 shipped with it failing.
REQUIRED=(fmt watch harness harness-windows msrv docs audit)

timeout="${HOTL_CI_WAIT_TIMEOUT:-1800}"
interval="${HOTL_CI_POLL_INTERVAL:-15}"

[ $# -eq 1 ] || { echo "usage: $0 <sha>" >&2; exit 1; }
sha="$1"

command -v gh >/dev/null 2>&1 || {
  echo "error: gh is required to read CI status." >&2
  echo "hint: install the GitHub CLI (https://cli.github.com)." >&2
  exit 1
}
command -v jq >/dev/null 2>&1 || {
  echo "error: jq is required to read CI status." >&2
  echo "hint: install jq, or run this on a GitHub-hosted runner where it is preinstalled." >&2
  exit 1
}
gh auth status >/dev/null 2>&1 || {
  echo "error: gh is not authenticated." >&2
  echo "hint: run 'gh auth login', or set GH_TOKEN." >&2
  exit 1
}

# Set on runners (one fewer API call there); the fallback keeps a fork or a
# local clone working.
repo="${GITHUB_REPOSITORY:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"

names_json="$(printf '%s\n' "${REQUIRED[@]}" | jq -R . | jq -sc .)"

# group_by | max_by(.started_at) is latest-run-wins: the API returns superseded
# check runs alongside current ones, so a job that was re-run green still reads
# as failed without this — a gate that refuses forever.
filter="[ .check_runs[] | select(.name as \$n | $names_json | index(\$n)) ]
  | group_by(.name) | map(max_by(.started_at))
  | .[] | [.name, .status, .conclusion // \"\", .html_url] | @tsv"

elapsed_str() {
  printf '%dm%02ds' $((SECONDS / 60)) $((SECONDS % 60))
}

echo "waiting for CI on $sha (${REQUIRED[*]}) ..."

api_failures=0
while :; do
  if ! rows="$(gh api "repos/$repo/commits/$sha/check-runs?per_page=100" --jq "$filter" 2>&1)"; then
    # An unknown SHA is 422 "No commit found", not a blip — polling cannot fix
    # a commit that was never pushed.
    case "$rows" in
      *"No commit found"*)
        echo "error: $repo has no commit $sha — it was never pushed." >&2
        echo "hint: push the branch first; CI cannot report on a local-only commit." >&2
        exit 1
        ;;
    esac
    api_failures=$((api_failures + 1))
    if [ "$api_failures" -ge 3 ]; then
      echo "error: the check-runs API failed 3 times in a row for $sha:" >&2
      echo "$rows" >&2
      exit 1
    fi
    echo "  check-runs API call failed (${api_failures}/3), retrying ..."
    sleep "$interval"
    continue
  fi
  api_failures=0

  green=0
  failed=""
  pending=""
  for name in "${REQUIRED[@]}"; do
    row="$(printf '%s\n' "$rows" | awk -F'\t' -v n="$name" '$1 == n { print; exit }')"
    if [ -z "$row" ]; then
      pending="$pending $name(absent)"
      continue
    fi
    IFS=$'\t' read -r _ status conclusion url <<<"$row"
    if [ "$status" != completed ]; then
      pending="$pending $name($status)"
    elif [ "$conclusion" = success ]; then
      green=$((green + 1))
    else
      # `cancelled` lands here deliberately: ci.yml runs cancel-in-progress, and
      # treating a cancelled run as pending turns this gate into a hang.
      failed="$failed$name	$conclusion	$url
"
    fi
  done

  if [ -n "$failed" ]; then
    echo "error: CI is not green on $sha — refusing." >&2
    printf '%s' "$failed" >&2
    exit 1
  fi

  if [ "$green" -eq "${#REQUIRED[@]}" ]; then
    echo "CI is green on $sha (${#REQUIRED[@]}/${#REQUIRED[@]}) after $(elapsed_str)."
    exit 0
  fi

  if [ "$SECONDS" -ge "$timeout" ]; then
    echo "error: CI never reported$pending for $sha within ${timeout}s." >&2
    echo "hint: ci.yml ignores tags (ci.yml:13), so a commit that was never pushed to a" >&2
    echo "      branch has no CI run and never will. Push the branch first, then finish" >&2
    echo "      the release with 'scripts/release.sh --tag-only'." >&2
    exit 1
  fi

  echo "waiting $(elapsed_str):${pending} — $green/${#REQUIRED[@]} green"
  sleep "$interval"
done
