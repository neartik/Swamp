#!/usr/bin/env bash
# Manual tmux smoke test for `swamp board` (docs/BOARD.md section 7, item 7). Not part of CI:
# it launches a real chat, a real brain and real workers, so it is run by hand and read by eye.
#
#   scripts/board-tmux.sh            # run the whole sequence, leave the captures in $OUT
#   OUT=/somewhere scripts/board-tmux.sh
#
# It opens a detached 120x40 session in a scratch git repo, splits -h -l 46, runs `swamp chat`
# on the left and `swamp board` on the right, asks the brain for two tiny low-tier tasks, and
# captures the board pane every few seconds. Then it moves the selection, resizes the split to
# 60 and back to 46, checks `swamp board --once --json`, quits both panes and asserts the
# terminal is restored and board.pid is gone.
set -euo pipefail

SESSION=${SESSION:-swamp-board}
REPO=${REPO:-/tmp/swamp-board}
OUT=${OUT:-/tmp/swamp-board-captures}
WIDE=${WIDE:-60}
NARROW=${NARROW:-46}
SWAMP=${SWAMP:-swamp}
PROMPT=${PROMPT:-'Dispatch two low tier tasks in parallel, one swamp_dispatch call with both. Task one: append the line "one" to notes.txt. Task two: append the line "two" to notes2.txt. Do not do the edits yourself.'}

command -v tmux >/dev/null || { echo "tmux is not installed" >&2; exit 1; }
command -v "$SWAMP" >/dev/null || { echo "$SWAMP is not on PATH" >&2; exit 1; }

step() { printf '\n== %s\n' "$*"; }
snap() { # snap <name> <pane>
  tmux capture-pane -p -t "$2" >"$OUT/$1.txt"
  printf '   %s %s\n' "$(date -u +%H:%M:%S)" "$1"
}

step "scratch repo $REPO"
tmux kill-session -t "$SESSION" 2>/dev/null || true
rm -rf "$REPO"
mkdir -p "$REPO" "$OUT"
git -C "$REPO" init -q -b main
printf '# swamp board smoke\n' >"$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" -c user.name=swamp -c user.email=swamp@example.invalid commit -qm "Add the readme"

step "120x40 session, split -h -l $NARROW"
tmux new-session -d -s "$SESSION" -x 120 -y 40 -c "$REPO"
CHAT=$(tmux list-panes -t "$SESSION" -F '#{pane_id}' | head -1)
tmux send-keys -t "$CHAT" "cd $REPO && $SWAMP chat" Enter
sleep 5
# No board is attached yet, so this capture is where the welcome hint has to show.
snap 00-startup-chat "$CHAT"
grep -q 'swamp board' "$OUT/00-startup-chat.txt" && echo "   welcome hint: present" || echo "   welcome hint: MISSING"

BOARD=$(tmux split-window -h -l "$NARROW" -t "$CHAT" -c "$REPO" -P -F '#{pane_id}' "$SWAMP board")
sleep 4
snap 01-startup-board "$BOARD"

step "dispatch two low-tier tasks"
tmux send-keys -t "$CHAT" "$PROMPT"
sleep 1
tmux send-keys -t "$CHAT" Enter
# One capture a second: the spec's claim is that a dispatch shows up within about two, so the
# journal timestamp and the first capture that names the node have to be that close.
for i in $(seq 1 60); do
  sleep 1
  snap "$(printf '1-t%03ds-board' "$i")" "$BOARD"
done

step "selection footer: down arrow"
for i in 1 2 3 4 5 6 7 8; do
  tmux send-keys -t "$BOARD" Down
  sleep 2
  snap "$(printf '3%01d-selection-board' "$i")" "$BOARD"
done
grep -lE 'score \.[0-9]+' "$OUT"/3*-selection-board.txt >/dev/null 2>&1 \
  && echo "   score terms: present" || echo "   score terms: MISSING"

step "resize the split to $WIDE and back to $NARROW"
tmux resize-pane -t "$BOARD" -x "$WIDE"
sleep 3
snap 40-wide-board "$BOARD"
tmux resize-pane -t "$BOARD" -x "$NARROW"
sleep 3
snap 41-narrow-board "$BOARD"

step "let the nodes finish"
for i in 1 2 3; do
  sleep 10
  snap "$(printf '5%01d-late-board' "$i")" "$BOARD"
done

step "swamp board --once --json"
(cd "$REPO" && "$SWAMP" board --once --json) >"$OUT/60-once.json" 2>"$OUT/60-once.err" || true
if command -v python3 >/dev/null; then
  python3 -c 'import json,sys; json.load(open(sys.argv[1])); print("   valid JSON")' "$OUT/60-once.json"
fi
(cd "$REPO" && "$SWAMP" board --once) >"$OUT/61-once.txt" 2>&1 || true

step "quit both panes"
tmux send-keys -t "$BOARD" q
sleep 2
tmux list-panes -t "$SESSION" -F '#{pane_id}' | grep -q "$BOARD" \
  && echo "   board pane STILL OPEN" || echo "   board pane closed on q"
tmux send-keys -t "$CHAT" Escape
tmux send-keys -t "$CHAT" Escape
sleep 2
tmux send-keys -t "$CHAT" C-c
sleep 1
tmux send-keys -t "$CHAT" C-c
sleep 8
snap 70-after-quit-chat "$CHAT"
tmux list-panes -t "$SESSION" -F '#{pane_id} #{pane_current_command}' >"$OUT/71-panes.txt" || true
cat "$OUT/71-panes.txt"
grep -q ' swamp$' "$OUT/71-panes.txt" && echo "   chat DID NOT exit" || echo "   chat exited, shell restored"

step "board.pid"
PIDFILE="${SWAMP_HOME:-$HOME/.swamp}/board.pid"
if [ -e "$PIDFILE" ]; then echo "   STILL THERE: $PIDFILE"; cat "$PIDFILE"; else echo "   gone"; fi

step "cleanup"
cp "$REPO"/.swamp/runs/*/journal.jsonl "$OUT/90-journal.jsonl" 2>/dev/null || true
tmux kill-session -t "$SESSION" 2>/dev/null || true
rm -rf "$REPO"
"$SWAMP" doctor --reap >"$OUT/80-reap.txt" 2>&1 || true
tail -3 "$OUT/80-reap.txt"
echo
echo "captures in $OUT"
