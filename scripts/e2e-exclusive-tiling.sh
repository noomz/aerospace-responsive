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
# TextEdit windows must arrive tiled (no on-window-detected rule floating them). Covers: arrival
# via an unfocused move, the tile limit, a non-allowlisted arrival, leave re-tile, close prune.
# Opens two TextEdit documents and a Preview window, and closes them again.
set -euo pipefail

WS=X
AWAY=Y
TEXTEDIT=com.apple.TextEdit
STATE="$HOME/Library/Application Support/aerospace-responsive/state.json"
LOG_DIR="${LOG_DIR:-.scratch/exclusive-tiling/e2e}"
mkdir -p "$LOG_DIR"
EVENTS="$LOG_DIR/subscribe-$(date +%Y%m%dT%H%M%S).log"
failures=0
created=()

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

cleanup() {
    for id in "${created[@]}"; do
        aerospace close --window-id "$id" >/dev/null 2>&1 || true
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
aerospace-responsive status | grep -q '^exclusive tiling: live' || { say "the daemon's exclusive tiling is not live:"; aerospace-responsive status; exit 2; }
aerospace-responsive explain | grep -q "$TEXTEDIT" || { say "no exclusive-tiling rule for $TEXTEDIT; add the block from this script's header"; exit 2; }
HOME_WS=$(aerospace list-workspaces --focused)
[[ "$HOME_WS" != "$WS" ]] || { say "focus a workspace other than $WS first"; exit 2; }
pass "subscribe child alive, daemon live, rule present; logging events to $EVENTS"

say "arrival by an unfocused move, and the tile limit"
first=$(new_window "$TEXTEDIT" osascript -e 'tell application "TextEdit" to make new document')
created+=("$first")
second=$(new_window "$TEXTEDIT" osascript -e 'tell application "TextEdit" to make new document')
created+=("$second")
expect_layout "$first" tiled "TextEdit arrives tiled on $HOME_WS"
aerospace move-node-to-workspace --window-id "$first" "$WS"
expect_layout "$first" tiled "first TextEdit on $WS keeps its slot"
aerospace move-node-to-workspace --window-id "$second" "$WS"
expect_layout "$second" floating "second TextEdit on $WS is over max-tiled = 1"
[[ " $(rule_floated_on "$WS") " == *" $second "* ]] && pass "second TextEdit is rule-floated on $WS" || fail "second TextEdit not in state.json memory for $WS"
[[ "$(aerospace list-workspaces --focused)" == "$HOME_WS" ]] && pass "focus never followed (unfocused moves)" || fail "focus moved"

say "a window the allowlist does not name"
icon=/System/Applications/Calculator.app/Contents/Resources/AppIcon.icns
preview=$(new_window com.apple.Preview open -a Preview "$icon")
created+=("$preview")
aerospace move-node-to-workspace --window-id "$preview" "$WS"
expect_layout "$preview" floating "Preview on $WS is not allowlisted"

say "leave: the destination decides"
aerospace move-node-to-workspace --window-id "$second" "$AWAY"
expect_layout "$second" tiled "rule-floated TextEdit re-tiles on $AWAY (no rule there)"
[[ " $(rule_floated_on "$WS") " != *" $second "* ]] && pass "its memory entry is gone" || fail "still remembered on $WS"

say "close prunes memory"
aerospace close --window-id "$preview"
expect_layout "$preview" gone "Preview closed"
sleep 0.5
[[ " $(rule_floated_on "$WS") " != *" $preview "* ]] && pass "closed window pruned from state.json" || fail "closed window still in state.json"

say "the event stream"
grep -q "\"_event\":\"window-moved\".*\"prevWorkspace\":\"$HOME_WS\".*\"windowId\":$first" "$EVENTS" && pass "window-moved with prevWorkspace for $first" || fail "no window-moved for $first in $EVENTS"
grep -q "\"_event\":\"window-closed\".*\"windowId\":$preview" "$EVENTS" && pass "window-closed for $preview" || fail "no window-closed for $preview in $EVENTS"
[[ "$(workspace_of "$first")" == "$WS" ]] && pass "first TextEdit still on $WS" || fail "first TextEdit left $WS"

say ""
if ((failures)); then
    say "FAILED: $failures check(s); events in $EVENTS, daemon log in ~/Library/Logs/aerospace-responsive/"
    exit 1
fi
say "all exclusive-tiling e2e checks passed"
