#!/usr/bin/env bash
# Record the hotl demo and trim the leading wait, in one step.
#   scripts/demo/make-recording.sh [trim_seconds]   (default 4)
#
# Runs the VHS tape (which builds a hotl-demo tmux session, drives the demo,
# and tears the session down), then strips the first N seconds - the settle
# time before the interaction - into the final file.
#
# Outputs:
#   scripts/demo/hotl.mp4  raw recording (with the leading wait, intermediate)
#   docs/hotl.mp4          trimmed recording, wait removed
#   docs/hotl.gif          the embeddable demo (GIFs render inline on GitHub
#                          and crates.io; mp4 via ![]() does not)
#
# Local only - never runs `vhs publish`.
set -euo pipefail

# Run from the repo root so the tape's `Output scripts/demo/hotl.mp4` and the
# docs/ output paths resolve consistently no matter where this is invoked from.
cd "$(dirname "$0")/../.."

SKIP="${1:-4}"
RAW=scripts/demo/hotl.mp4   # raw recording (tape's Output)
MP4=docs/hotl.mp4           # trimmed recording
GIF=docs/hotl.gif           # embeddable demo

command -v vhs    >/dev/null || { echo "error: vhs not on PATH." >&2; exit 1; }
command -v ffmpeg >/dev/null || { echo "error: ffmpeg not on PATH." >&2; exit 1; }

echo "recording (vhs scripts/demo/hotl.tape)..."
vhs scripts/demo/hotl.tape

echo "trimming first ${SKIP}s -> $MP4..."
mkdir -p docs
ffmpeg -y -loglevel error -ss "$SKIP" -i "$RAW" \
    -c:v libx264 -pix_fmt yuv420p -movflags +faststart -an "$MP4"

# Build the embeddable GIF from the trimmed mp4. Two-pass palette (generate then
# apply) at 10fps and 900px wide keeps it sharp without a huge file.
echo "building $GIF..."
palette=$(mktemp -t hotl-palette).png
ffmpeg -y -loglevel error -i "$MP4" \
    -vf "fps=10,scale=900:-1:flags=lanczos,palettegen" "$palette"
ffmpeg -y -loglevel error -i "$MP4" -i "$palette" \
    -lavfi "fps=10,scale=900:-1:flags=lanczos [x]; [x][1:v] paletteuse" "$GIF"
rm -f "$palette"

# The raw is just an intermediate; docs/ holds the keepers.
rm -f "$RAW"

echo "done: $GIF (and $MP4)"
