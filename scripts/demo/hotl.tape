# hotl demo — run from anywhere:  vhs scripts/demo/hotl.tape
# Outputs scripts/demo/hotl.mp4. Local only; do NOT run `vhs publish`.
#
# Setup (session + layout + launching hotl/claude) lives in setup.sh and runs
# hidden. VHS `Wait`s for claude's UI to appear before recording, so no manual
# cropping is needed. Teardown kills ONLY the hotl-demo session.

Require hotl
Require tmux

Output scripts/demo/hotl.mp4

Set Shell "bash"
Set FontSize 18
Set Width 1200
Set Height 700
Set Padding 20

# --- setup (hidden): build the session, launch panes, then wait for claude ---
Hide
Type "bash ~/sources/hotl/scripts/demo/setup.sh && tmux attach -t hotl-demo" Enter
# Wait (up to 30s) for hotl's status bar to report detected agents. Matching
# claude's `ctx:` is unreliable — it gets truncated in the narrow panes.
Wait+Screen@30s /[0-9]+ agents/
Sleep 1500ms
Show

# --- recording ---
# Focus starts on the hotl pane (left). Move to the top-right claude pane.
Ctrl+l
Sleep 800ms

# Settle before interacting.
Sleep 5s

# Grow this pane leftward: Ctrl-a then h x4 (h is -r/repeatable, so one prefix).
Ctrl+a
Type "hhhh"
Sleep 1s

# Top-right claude: ask the meaning of life.
Type@60ms "what is the meaning of life?"
Sleep 600ms
Enter
Sleep 1500ms

# Down to the middle claude pane: ask it to choose.
Ctrl+j
Sleep 600ms
Type@60ms "Ask me A or B"
Sleep 600ms
Enter
Sleep 1500ms

# Back up to the meaning-of-life pane.
Ctrl+k
Sleep 1s

# Zoom this pane fullscreen (Ctrl-a m), hold, then un-zoom.
Ctrl+a
Type "m"
Sleep 1500ms
Ctrl+a
Type "m"
Sleep 800ms

# Over to the hotl pane, select the middle-claude ("crates") agent, and jump
# to it with gd. Tune the j count if the selected row isn't the crates pane.
Ctrl+h
Sleep 600ms
Type "gg"
Sleep 400ms
Type "j" Sleep 300ms
Type "j" Sleep 300ms
Type "gd"
Sleep 2s

# gd landed us on the "Ask me A or B" pane — answer it, then hold.
Type@60ms "I choose A"
Sleep 600ms
Enter
Sleep 2s

# --- teardown (hidden): detach, then kill only the demo session ---
Hide
Ctrl+a
Type "d"
Sleep 500ms
Type "tmux kill-session -t hotl-demo 2>/dev/null" Enter
Show
