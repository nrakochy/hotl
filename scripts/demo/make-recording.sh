#!/usr/bin/env bash
# Record the hotl demo and trim the leading wait, in one step.
#   scripts/demo/make-recording.sh [trim_seconds]   (default 4)
#
# Runs the VHS tape (which builds a hotl-demo tmux session, drives the demo,
# and tears the session down), then strips the first N seconds - the settle
# time before the interaction - into the final file.
#
# Outputs:
#   scripts/demo/hotl.mp4  raw recording (with the leading wait)
#   docs/hotl.mp4          final, wait removed - the committed demo
#
# Local only - never runs `vhs publish`.
set -euo pipefail

# Run from the repo root so the tape's `Output scripts/demo/hotl.mp4` and the
# docs/ output paths resolve consistently no matter where this is invoked from.
cd "$(dirname "$0")/../.."

SKIP="${1:-4}"
RAW=scripts/demo/hotl.mp4   # raw recording (tape's Output)
OUT=docs/hotl.mp4           # final trimmed recording

command -v vhs    >/dev/null || { echo "error: vhs not on PATH." >&2; exit 1; }
command -v ffmpeg >/dev/null || { echo "error: ffmpeg not on PATH." >&2; exit 1; }

echo "recording (vhs scripts/demo/hotl.tape)..."
vhs scripts/demo/hotl.tape

echo "trimming first ${SKIP}s -> $OUT..."
mkdir -p docs
ffmpeg -y -loglevel error -ss "$SKIP" -i "$RAW" \
    -c:v libx264 -pix_fmt yuv420p -movflags +faststart -an "$OUT"

# The raw is just an intermediate; the trimmed docs/hotl.mp4 is the only keeper.
rm -f "$RAW"

echo "done: $OUT"
