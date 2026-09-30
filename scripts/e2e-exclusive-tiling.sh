#!/usr/bin/env bash
# End-to-end check of exclusive tiling on the live desktop (SPEC T15). Run by hand, not in CI.
#
# Needs: AeroSpace with window-moved/window-closed (the noomz/AeroSpace feat/window-events build,
# AeroSpace restarted after install), the aerospace-responsive daemon running, and this block in
# the tool's config (the script never edits your config):
#
#   [[exclusive-tiling]]
#   workspaces = ["X"]
#   tiling-allowlist = [{ app = "com.apple.TextEdit", max-tiled = 1 }]
#
# New windows open wherever AeroSpace puts them (often floating); the script stages each on scratch
# workspace Y (no rule), tiles it there, then moves it to X, so the rule sees a tiled arrival. Covers:
# arrival via an unfocused move, the tile limit, a non-allowlisted arrival, leave re-tile, close
# prune. Launches TextEdit and Terminal first (an app's first window after launch can open
# floating), opens two TextEdit documents and a Terminal window, and closes them again; an app the
# script launched is quit at the end.
set -euo pipefail

WS=X
AWAY=Y
TEXTEDIT=com.apple.TextEdit
TERMINAL=com.apple.Terminal
STATE="$HOME/Library/Application Support/aerospace-responsive/state.json"
LOG_DIR="${LOG_DIR:-.scratch/exclusive-tiling/e2e}"
mkdir -p "$LOG_DIR"
EVENTS="$LOG_DIR/subscribe-$(date +%Y%m%dT%H%M%S).log"
failures=0
created=()
launched=()

say() { printf '%s\n' "$*"; }
pass() { say "  ok    $*"; }
fail() { say "  FAIL  $*"; failures=$((failures + 1)); }

layout_of() { # window id -> %{window-layout}, or "gone"
    aerospace list-windows --all --format '%{window-id} %{window-layout}' | awk -v id="$1" '$1 == id { print $2; f = 1 } END { if (!f) print "gone" }'
}

workspace_of() {
    aerospace list-windows --all --format '%{window-id} %{workspace}' | awk -v id="$1" '$1 == id { print $2 }'
}

# Wait up to 5s for a window's layout class (tiled|floating|gone).
expect_layout() {
    local id=$1 want=$2 what=$3 got=""
    for _ in $(seq 50); do
        got=$(layout_of "$id")
        case "$got" in h_* | v_*) got=tiled ;; esac
        [[ "$got" == "$want" ]] && { pass "$what ($id $want)"; return; }
        sleep 0.1
    done
    fail "$what: window $id is $got, wanted $want"
}

rule_floated_on() { # workspace -> ids the daemon remembers as rule-floated there
    python3 -c 'import json,sys
d = json.load(open(sys.argv[1])).get("exclusive_tiling", {}).get("rule_floated", {})
print(" ".join(str(f["window_id"]) for f in d.get(sys.argv[2], [])))' "$STATE" "$1"
}

ids_of() { aerospace list-windows --all --format '%{window-id} %{app-bundle-id}' | awk -v app="$1" '$2 == app { print $1 }' | sort; }

# Run an opener, print the one new window id of `app` it produced. Runs in a subshell: the caller
# records the id in `created`.
new_window() {
    local app=$1; shift
    local before after id
    before=$(ids_of "$app")
    "$@" >/dev/null
    for _ in $(seq 50); do
        after=$(ids_of "$app")
        id=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | head -1)
        [[ -n "$id" ]] && { printf '%s\n' "$id"; return; }
        sleep 0.1
    done
    say "no new $app window appeared" >&2
    exit 1
}

# Start `name` (bundle `app`) if it is not running, and quit it at the end. Then open one warm-up
# window with the opener (the rest of the arguments): the first window after an app launches can
# open floating, which would make it no arrival for the rule. Every window this opens is tracked.
start_app() {
    local app=$1 name=$2 before
    shift 2
    before=$(ids_of "$app")
    if ! pgrep -xq "$name"; then
        osascript -e "tell application \"$name\" to launch" >/dev/null
        launched+=("$name")
        sleep 2
    fi
    "$@" >/dev/null
    sleep 1
    while read -r id; do [[ -n "$id" ]] && created+=("$id"); done < <(comm -13 <(printf '%s\n' "$before") <(ids_of "$app"))
}

# Move a fresh window to the no-rule workspace and tile it there: the arrival the rule acts on.
stage() {
    aerospace move-node-to-workspace --window-id "$1" "$AWAY"
    aerospace layout tiling --window-id "$1" 2>/dev/null || true # already tiled: a no-op error
    expect_layout "$1" tiled "$2 staged tiled on $AWAY"
}

cleanup() {
    for id in "${created[@]}"; do
        aerospace close --window-id "$id" >/dev/null 2>&1 || true
    done
    for name in "${launched[@]}"; do # everything open in it is ours; TextEdit refuses to quit over open documents
        osascript -e "tell application \"$name\" to close every document saving no" >/dev/null 2>&1 || true
        osascript -e "tell application \"$name\" to quit" >/dev/null 2>&1 || true
    done
    [[ -n "${SUB_PID:-}" ]] && kill "$SUB_PID" 2>/dev/null || true
    [[ -n "${HOME_WS:-}" ]] && aerospace workspace "$HOME_WS" >/dev/null 2>&1 || true
}
trap cleanup EXIT

say "preconditions"
aerospace subscribe --all >"$EVENTS" 2>&1 &
SUB_PID=$!
sleep 1
kill -0 "$SUB_PID" 2>/dev/null || { say "aerospace subscribe exited: $(cat "$EVENTS")"; say "needs AeroSpace with window-moved/window-closed"; exit 2; }
# status exits 4 while any warning is pending, so read its text rather than piping it (pipefail).
status=$(aerospace-responsive status || true)
grep -q '^exclusive tiling: live' <<<"$status" || { say "the daemon's exclusive tiling is not live:"; say "$status"; exit 2; }
explain=$(aerospace-responsive explain || true)
grep -q "$TEXTEDIT" <<<"$explain" || { say "no exclusive-tiling rule for $TEXTEDIT; add the block from this script's header"; exit 2; }
HOME_WS=$(aerospace list-workspaces --focused)
[[ "$HOME_WS" != "$WS" ]] || { say "focus a workspace other than $WS first"; exit 2; }
pass "subscribe child alive, daemon live, rule present; logging events to $EVENTS"
start_app "$TEXTEDIT" TextEdit osascript -e 'tell application "TextEdit" to make new document'
start_app "$TERMINAL" Terminal osascript -e 'tell application "Terminal" to do script ""'

say "arrival by an unfocused move, and the tile limit"
first=$(new_window "$TEXTEDIT" osascript -e 'tell application "TextEdit" to make new document')
created+=("$first")
second=$(new_window "$TEXTEDIT" osascript -e 'tell application "TextEdit" to make new document')
created+=("$second")
stage "$first" "first TextEdit"
stage "$second" "second TextEdit"
aerospace move-node-to-workspace --window-id "$first" "$WS"
expect_layout "$first" tiled "first TextEdit on $WS keeps its slot"
aerospace move-node-to-workspace --window-id "$second" "$WS"
expect_layout "$second" floating "second TextEdit on $WS is over max-tiled = 1"
expect_layout "$first" tiled "first TextEdit still holds the slot after the daemon judged both"
[[ " $(rule_floated_on "$WS") " == *" $second "* ]] && pass "second TextEdit is rule-floated on $WS" || fail "second TextEdit not in state.json memory for $WS"
[[ "$(aerospace list-workspaces --focused)" == "$HOME_WS" ]] && pass "focus never followed (unfocused moves)" || fail "focus moved"

say "a window the allowlist does not name"
other=$(new_window "$TERMINAL" osascript -e 'tell application "Terminal" to do script ""')
created+=("$other")
stage "$other" Terminal
aerospace move-node-to-workspace --window-id "$other" "$WS"
expect_layout "$other" floating "Terminal on $WS is not allowlisted"
[[ " $(rule_floated_on "$WS") " == *" $other "* ]] && pass "Terminal is rule-floated on $WS" || fail "Terminal not in state.json memory for $WS"

say "leave: the destination decides"
aerospace move-node-to-workspace --window-id "$second" "$AWAY"
expect_layout "$second" tiled "rule-floated TextEdit re-tiles on $AWAY (no rule there)"
[[ " $(rule_floated_on "$WS") " != *" $second "* ]] && pass "its memory entry is gone" || fail "still remembered on $WS"

say "close prunes memory"
aerospace close --window-id "$other"
expect_layout "$other" gone "Terminal closed"
sleep 0.5
[[ " $(rule_floated_on "$WS") " != *" $other "* ]] && pass "closed window pruned from state.json" || fail "closed window still in state.json"

say "the event stream"
grep -q "\"_event\":\"window-moved\".*\"prevWorkspace\":\"$AWAY\".*\"windowId\":$first" "$EVENTS" && pass "window-moved with prevWorkspace for $first" || fail "no window-moved for $first in $EVENTS"
grep -q "\"_event\":\"window-closed\".*\"windowId\":$other" "$EVENTS" && pass "window-closed for $other" || fail "no window-closed for $other in $EVENTS"
[[ "$(workspace_of "$first")" == "$WS" ]] && pass "first TextEdit still on $WS" || fail "first TextEdit left $WS"

say ""
if ((failures)); then
    say "FAILED: $failures check(s); events in $EVENTS, daemon log in ~/Library/Logs/aerospace-responsive/"
    exit 1
fi
say "all exclusive-tiling e2e checks passed"
