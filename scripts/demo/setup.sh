#!/usr/bin/env bash
# Setup for the hotl VHS demo. Builds a `hotl-demo` tmux session laid out as
# hotl on the left and three claude panes stacked on the right, then leaves it
# ready to attach.
#
# Runs on its own tmux server (socket `hotl-demo`) rather than the default one,
# and only ever touches the `hotl-demo` session on it — never your sessions, and
# never any server. The private socket is also what makes the demo reproducible:
# hotl discovers agents with `list-panes -a`, which spans every session on the
# server it is attached to. On the default socket the agent list would include
# whatever else you have running, so the rows the tape drives would shift with
# your machine's state. On a private socket the list is exactly these three.
#
# Assumes `hotl` and `claude` are on PATH.
set -euo pipefail

SOCKET=hotl-demo
SESSION=hotl-demo

# Every tmux call in this script (and in the tape) must target the private
# server, so wrap it once rather than repeating -L at each call site.
tm() { tmux -L "$SOCKET" "$@"; }

cd ~/sources/hotl

# Clear a leftover demo session so the run starts clean. Scoped to the session
# by name, never `kill-server`: killing a server is not this script's business,
# and a name-scoped kill stays harmless even if the socket ever resolved wrong.
tm kill-session -t "$SESSION" 2>/dev/null || true

# Left pane (0) will run hotl; created detached so we can lay it out first.
# Size roughly matches the VHS window (1200x700 @ 18pt) so nothing overflows.
tm new-session -d -s "$SESSION" -x 130 -y 34

# Split left | right (pane 1), then stack the right into three panes (1,2,3).
tm split-window -h -t "$SESSION.0"
tm split-window -v -t "$SESSION.1"
tm split-window -v -t "$SESSION.2"
tm select-layout -t "$SESSION" main-vertical

# Let each new pane's interactive zsh finish loading its rc/prompt before we
# type — otherwise send-keys races the shell and the Enter is dropped.
sleep 2

# Run a command in a pane: send the text, then Enter as a SEPARATE key after a
# beat, which is far more reliable than "<cmd> Enter" against a busy shell.
run_in() {
    local pane="$1" cmd="$2"
    tm send-keys -t "$pane" "$cmd"
    sleep 0.4
    tm send-keys -t "$pane" Enter
}

# Right panes run claude from different dirs (top: repo root, middle: crates/,
# bottom: docs/); left pane runs hotl. The tape's `Wait` handles readiness.
# This order also fixes the agent rows hotl shows: hotl / crates / docs.
run_in "$SESSION.1" "claude"
run_in "$SESSION.2" "cd crates/ && claude"
run_in "$SESSION.3" "cd docs && claude"
run_in "$SESSION.0" "hotl"

# `select-pane -R` out of the hotl pane has three candidates (1, 2, 3), and tmux
# breaks the tie by picking the most recently active one — after the splits above
# that is pane 3 (docs), so the tape's first Ctrl-l would land at the bottom.
# Touch the stack bottom-to-top so pane 1 wins the tie and Ctrl-l goes to the top.
tm select-pane -t "$SESSION.3"
tm select-pane -t "$SESSION.2"
tm select-pane -t "$SESSION.1"
tm select-pane -t "$SESSION.0"
