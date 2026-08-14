#!/usr/bin/env sh
# ab-wallclock: profile one hotl -p run. Usage: ab-wallclock.sh <hotl-binary> "<prompt>"
# Compare two binaries by running once each (fresh sessions) and diffing the tables.
#
# Reading it: many samples/short gaps -> round-trip inflation; few samples with
# long gaps and cache ~0 -> a cache-busting bug (drop everything, bisect);
# nonzero denial count -> sandbox friction (see [sandbox].writable).
set -eu
BIN="$1"; PROMPT="$2"
SESSDIR="${XDG_DATA_HOME:-$HOME/.local/share}/hotl/sessions"
OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT
START=$(date +%s)
# `-p "$PROMPT" --json`, in that order: a flag right after -p means stdin.
"$BIN" -p "$PROMPT" --json >"$OUT"
WALL=$(( $(date +%s) - START ))
LOG=$(ls -t "$SESSDIR"/*.jsonl | head -1)
echo "log: $LOG   wall-clock: ${WALL}s"
jq -rs '
  [.[] | select(.payload.kind=="usage")] as $u
  | "samples: \($u | length)",
    "output tokens: \([$u[].payload.usage.output_tokens] | add)",
    "cache hit % per sample: \([$u[] | .payload.usage
      | (100 * .cache_read_input_tokens
         / (1 + .input_tokens + .cache_read_input_tokens + .cache_creation_input_tokens))
      | floor])",
    "sample gaps (s): \([$u[].ts_ms] | . as $t
      | [range(1; length) | (($t[.] - $t[.-1]) / 1000 | floor)])"
' "$LOG"
grep -c '"tool_use"' "$LOG" | sed 's/^/tool calls: /'
grep -cE 'ermission denied|not permitted' "$LOG" | sed 's/^/denial-smelling results: /' || true
# Frames ride bare on stdout (one json_frame per line, no envelope).
echo "client spans (ms, p50/p99 per turn):"
jq -r 'select(.type=="ledger_report") | .phase_deltas[]
  | "\(.from)->\(.to): \(.p50_ns/1000000 | floor)/\(.p99_ns/1000000 | floor)"' "$OUT"
