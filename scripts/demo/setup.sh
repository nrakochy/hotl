#!/usr/bin/env bash
# Setup for the hotl VHS demo. Builds a `hotl-demo` tmux session laid out as
# hotl on the left and three claude panes stacked on the right, then leaves it
# ready to attach. Only ever touches the `hotl-demo` session — never other
# sessions or the server. Idempotent: recreates hotl-demo if it exists.
#
# Assumes `hotl` and `claude` are on PATH.
set -euo pipefail

SESSION=hotl-demo

cd ~/sources/hotl

tmux start-server
tmux kill-session -t "$SESSION" 2>/dev/null || true

# Left pane (0) will run hotl; created detached so we can lay it out first.
# Size roughly matches the VHS window (1200x700 @ 18pt) so nothing overflows.
tmux new-session -d -s "$SESSION" -x 130 -y 34

# Split left | right (pane 1), then stack the right into three panes (1,2,3).
tmux split-window -h -t "$SESSION.0"
tmux split-window -v -t "$SESSION.1"
tmux split-window -v -t "$SESSION.2"
tmux select-layout -t "$SESSION" main-vertical

# Let each new pane's interactive zsh finish loading its rc/prompt before we
# type — otherwise send-keys races the shell and the Enter is dropped.
sleep 2

# Run a command in a pane: send the text, then Enter as a SEPARATE key after a
# beat, which is far more reliable than "<cmd> Enter" against a busy shell.
run_in() {
    local pane="$1" cmd="$2"
    tmux send-keys -t "$pane" "$cmd"
    sleep 0.4
    tmux send-keys -t "$pane" Enter
}

# Right panes run claude from different dirs (top: repo root, middle: crates/,
# bottom: docs/); left pane runs hotl. The tape's `Wait` handles readiness.
run_in "$SESSION.1" "claude"
run_in "$SESSION.2" "cd crates/ && claude"
run_in "$SESSION.3" "cd docs && claude"
run_in "$SESSION.0" "hotl"
tmux select-pane -t "$SESSION.0"
