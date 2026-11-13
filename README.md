# aerospace-responsive

Reshapes [AeroSpace](https://github.com/nikitabobko/AeroSpace) when displays change: moves
workspaces to the monitors your rules name, and sets each workspace's root orientation (optionally
flattening it) from the monitor it lands on. A resident daemon reacts to CoreGraphics display
reconfiguration — no polling — and every command is a client of that one daemon.

## Requirements

- macOS
- [AeroSpace](https://github.com/nikitabobko/AeroSpace) 0.21.0-Beta or later (for `layout --root`)
- For exclusive tiling, and for a workspace template to rebuild when a window arrives: an AeroSpace whose `subscribe` has `window-moved` and `window-closed`,
  from the [`feat/window-events`](https://github.com/noomz/AeroSpace/tree/feat/window-events) branch
  of the noomz/AeroSpace fork. Restart AeroSpace after installing it (client and server must match).
  On stock AeroSpace the exclusive-tiling rule is inert, a template rebuilds only on a pass or
  `arrange`, and `status` warns; nothing else changes.

## Install

```sh
cargo install aerospace-responsive
mkdir -p ~/.config/aerospace-responsive
curl -fsSL https://raw.githubusercontent.com/noomz/aerospace-responsive/main/aerospace-responsive.example.toml \
  -o ~/.config/aerospace-responsive/aerospace-responsive.toml
# edit it for your monitors first
aerospace-responsive install            # asks before each step; --yes, --only, --skip for scripts
```

`install` steps: `config` (adds your placement workspaces to `persistent-workspaces`, removes
`workspace-to-monitor-force-assignment`, adds `exec-and-forget aerospace-responsive reconcile` to
`after-startup-command` — format-preserving, backed up, or refused), `launchd` (user LaunchAgent,
logs in `~/Library/Logs/aerospace-responsive/`), `reload` (`aerospace reload-config`, verified),
`reconcile` (first pass). Re-running converges. `uninstall` reverses exactly what `install` added.

After updating the binary with `cargo install`, restart the agent with `launchctl bootout gui/$UID/com.noomz.aerospace-responsive` then `launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.noomz.aerospace-responsive.plist`, not `kickstart`: launchd caches the code requirement of the binary it last ran and refuses a replacement one (`last exit reason = OS_REASON_CODESIGNING`, `job state = spawn failed`).

## Commands

| Command | Does |
|---|---|
| `reconcile [--dry-run] [--config F]` | Run a pass now, or print the exact `aerospace` commands it would run |
| `explain [--config F]` | Every rule considered and why it won or lost; each exclusive-tiling workspace with its windows' verdicts; each workspace template with the window in every slot |
| `monitors` | Attached displays as predicates see them (roles, pixels, notch, serial, labels) |
| `status` | Profile, last pass, pins, exclusive tiling and workspace template (live/inert), pending warnings — exits 4 while warnings are pending |
| `arrange [ws] [--dry-run]` | Rebuild a workspace's template now (default: the focused workspace); errors when none applies there |
| `pin [ws]` / `unpin [ws\|--all]` / `pins` | Protect a hand-tuned workspace's layout (placement still follows) |
| `profile [lock <name> \| unlock]` | Show, lock or unlock the setup profile |
| `reload` | Reload the config now (it is also watched) |

`--json` on `status`, `explain`, `monitors`, `pins`, `profile`, `reconcile --dry-run`, `arrange`.
Exit codes: 0 ok, 1 error, 2 usage, 3 daemon not running, 4 warnings pending.

After placement, cosmetic restore issues at most one workspace-focus command on the originally
focused monitor, and skips restore when fresh reads show a change in focus or that monitor's visible
workspace. Other monitors keep the workspaces placement left visible, avoiding intermediate focus
changes across the displays.

`daemon --observe` runs the daemon without executing anything: every pass is logged as the
commands it would have run. This is how to try a config against the live system safely.

## Config

See `aerospace-responsive.example.toml`. Layout rules: first match wins; no match means orientation
from aspect ratio and **never** flatten. A layout rule may set the root kind with
`kind = "tiles" | "accordion"`. Without it, a flatten rule means tiles and any other rule leaves the
kind alone (an axis change keeps an accordion). Put `kind` on a profile's layout rule to make it
setup-specific, for example accordion on the built-in screen only while it is the only monitor. Placement rules: first *satisfiable* match wins, so a list
reads as a preference chain; nothing satisfiable leaves the workspace where it is and warns.
Profiles (predicates over the attached set) are consulted before the base rules.

Predicate fields (every present field must hold): `name`, `label` (a `[[monitor]]` block),
`builtin`, `external`, `portrait`, `landscape`, `main`, `vendor`, `model`, `serial`, and — in a
`[[placement]]` `monitor` only — `nth`: the 1-based ordinal, in monitor-id order, among the
monitors matching the other fields. `{ main = false, nth = 2 }` is the second non-main monitor;
fewer matches make the rule unsatisfiable (`explain` says `nth is 2, only 1 match`). `nth = 0`, or
`nth` in `[[layout]]`, `[[monitor]]` or a profile `when`, is a load error.

### Top gap and the notch

AeroSpace adds `outer.top` on top of a monitor's **visible** area (`visibleRectPaddedByOuterGaps`),
and a notched panel already has its notch height removed from that area. A top gap sized to clear a
bar therefore lands notch-height lower on the built-in than on a display whose visible area starts at
the physical top edge. A `[gaps]` block makes this tool own that one key and take the notch back, so
the same number means the same clearance on every monitor:

```toml
[gaps]
top-default = 1                    # every monitor no rule below names
notch-compensation = true          # the default: subtract the measured notch, clamped at 0
add-menu-bar-height-when-single-monitor = true # opt in; default false

[[gaps.top]]
monitor = { main = true }          # any monitor predicate except `nth`; first match wins
value = 42
```

No `[gaps]` block ⇒ `outer.top` is never touched. `inner.*` and `outer.left/right/bottom` are never
written, notch or not.

The notch is measured, not guessed from a name: AppKit's two menu-bar strips beside the notch give
its height, and a plain 24 pt menu bar (the strips meet, no gap) compensates nothing — so an older
Retina panel with no notch keeps its configured number. `monitors` prints what was measured
(`notch 32pt`), and `explain` prints the arithmetic and the exact line:

```
gaps: outer.top = [{ monitor.2 = 10 }, 1]
  monitor 2   Built-in Retina Display    base  42 [gaps.top[0]  notch -32  ->  10]
```

When `add-menu-bar-height-when-single-monitor` is enabled, one attached main monitor gets the
AppKit `NSMenu.menuBarHeight` added after notch compensation, even when the menu bar is hidden.
Multiple monitors keep the normal gap. The daemon checks for height changes every 10 seconds while
this option is enabled; display changes also trigger reconciliation. `explain` shows the addition.

On the pass that wants a different number, the tool rewrites that key in AeroSpace's config
(format-preserving, refused if the file does not round-trip) and runs `aerospace reload-config
--no-gui`. A steady state writes nothing and never reloads. If something else writes the line — a
gap script in `after-startup-command`, a hand edit — the next pass warns and takes it back.

### Exclusive tiling

On the workspaces a rule names, only windows its **tiling allowlist** admits may tile — e.g. kitty
the only tiled app on `T`. Needs the fork events (see Requirements); no `aerospace.toml` change.

```toml
[[exclusive-tiling]]
workspaces = ["T"]
else-action = "float"            # optional; the default and only value
tiling-allowlist = [
  { app = "net.kovidgoyal.kitty", title = "Settings", max-tiled = 0 },  # kitty's settings never tile
  { app = "net.kovidgoyal.kitty", max-tiled = 1 },                      # one other kitty may tile
]
```

| Field | Meaning |
|---|---|
| `workspaces` | Workspaces the rule applies to (non-empty) |
| `else-action` | What happens to a window the allowlist does not admit: `"float"` (default; the only value) |
| `tiling-allowlist` | Ordered entries; a window is governed by the **first** entry it matches. `[]` = nothing tiles |
| `app` | Exact bundle id (required). Find one with `aerospace list-apps --format '%{app-bundle-id} %{app-name}'` |
| `title` | Optional regex on the window title, case-insensitive and unanchored (as AeroSpace's `window-title-regex-substring`) |
| `max-tiled` | Optional tile limit for the entry: absent = unlimited, `0` = matches but never tiles |

What the rule does:

- It acts on **arrivals**: a window that lands on the workspace **tiled** (opened there, moved by keyboard,
  mouse or a callback) and is not admitted, or whose entry's `max-tiled` is full, is floated. A window
  arriving floating (dialogs, sticky windows, your floats, `on-window-detected` floats) is never touched.
- The limit counts the entry's windows tiled there now, whoever tiled them. When two windows arrive
  together, the first to arrive keeps the slot. A freed slot does not re-tile anything.
- It never fights you: tile or float a window yourself and it stays that way. It only undoes its own
  floats — a window it floated re-tiles when it leaves for a workspace where it may tile.
- On the first run, and whenever a workspace's rule changes (including by a profile switch), it
  **converges** that workspace once: every window judged as an arrival, the lowest window id keeping a
  limited slot. Removing a rule re-tiles the windows it floated. A plain restart converges nothing.
- Blocks cascade per workspace and per field, last block wins: top-level `[[exclusive-tiling]]` in file
  order, then the active profile's `[[profile.exclusive-tiling]]`. `tiling-allowlist` is replaced
  whole, never merged. Every workspace with a rule needs a `tiling-allowlist` (top-level alone and under
  each profile); a bad regex, an unknown `else-action` or a negative `max-tiled` is a load error.
- Moves are watched through `aerospace subscribe window-moved window-closed window-detected`, a child
  the daemon runs only while a rule exists — events, not polling. On stock AeroSpace the child exits,
  the rule does nothing, and `status` warns (exit 4) with the reason.
- While that child runs, a new window also re-checks placement. Placement skips a workspace that has no
  windows, and right after a wake AeroSpace re-registers windows one app at a time, so a pass can see a
  workspace as empty and leave it on the wrong monitor. When its windows come back, the re-check moves it.

`status` gets one line (`exclusive tiling: live; rule-floated T:2`, or `inert` / `observe`). `explain`
shows each rule, where every field came from, and a verdict for every window on the workspace:

```
exclusive-tiling:
  T    else-action float        [default]
       tiling-allowlist         [top-level #1]
         1 net.kovidgoyal.kitty  title~"Settings"  max-tiled 0
         2 net.kovidgoyal.kitty                    max-tiled 1  (1/1 tiled)
       converge: up to date
       windows:
         4211   kitty    "noomz: ~"     tiled     entry 2 (1/1)
         4380   kitty    "noomz: htop"  floating  rule-floated: entry 2 full
         5120   Preview  "x.pdf"        floating  yours (rule ignores)
```

`reconcile --dry-run` lists a pending converge and its `layout floating|tiling --window-id` commands;
`daemon --observe` logs the rule's would-be commands without running them.

### Workspace template

A `[[layout]]` rule sets a workspace's root only. A workspace template names the whole tiling tree,
so a nested layout you would otherwise arrange by hand comes back by itself, e.g. `C` on the
built-in display as `h[ LINE, v[ IGCL, Messenger ] ]`:

```toml
[[workspace-template]]
workspaces = ["C"]
when = { builtin = true }        # optional; absent = on every monitor
root = "h"
tree = [
  { app = "jp.naver.line.mac" },
  [ { app = "com.igcl.app" },
    { app = "com.google.Chrome.app.bbdeiblfgdokhlblpgeaokenkfknecgl" } ],
]
```

| Field | Meaning |
|---|---|
| `workspaces` | Workspaces the template applies to (non-empty) |
| `when` | Optional monitor predicate, judged against the monitor the workspace lands on after placement. Any field except `nth` |
| `root` | Root orientation: `"h"` or `"v"` (required) |
| `tree` | The root's children in order (non-empty). A table is a **slot**; an array is a container with the opposite orientation of its parent |
| `app` | Slot: exact bundle id (required) |
| `title` | Slot: optional regex on the window title, case-insensitive and unanchored (as in exclusive tiling) |

How windows are placed:

- Only tiled windows count. A floating window is never moved, and exclusive tiling decides what floats
  before the template runs.
- Each window goes to the first slot, in tree order, that matches it. A slot holds every window it
  matches, lowest window id first, side by side. Windows no slot matches (**rest windows**) go to the
  end of the root, lowest id first.
- Empty slots and containers are dropped, and a container left with one child is replaced by that
  child: with IGCL and Messenger closed, `C` above is just `h[ LINE ]`.
- Nesting always alternates (`h` inside `v` inside `h`). A rebuild resets container sizes.

When it rebuilds — flatten the workspace, then build the tree again with `move` and `join-with` by
window id, so focus does not move and a hidden workspace is rebuilt the same way:

- On a pass, the first time, and whenever the template or the workspace's tiled windows differ from
  what was last built (a tiled window opened, floated, tiled, or retitled into another slot).
  A closed window only leaves a gap in the record; it rebuilds nothing.
- When a tiled window arrives on the workspace (needs the fork events, see Requirements). A burst of
  arrivals causes one rebuild.
- On `aerospace-responsive arrange [ws]`, always (default: the focused workspace).
- After AeroSpace restarts (its window ids restart). A daemon restart alone rebuilds nothing.

Between triggers your hand-made changes stay; a trigger overwrites them. `pin` the workspace to keep a
hand-made tree; `unpin` rebuilds it.
While a template applies, the matching `[[layout]]` rule's `orientation`, `flatten` and `kind` do
nothing on that workspace; placement still applies. When `when` does not match — say `C` docked to an
external monitor — the `[[layout]]` rule owns the workspace there, and docking back rebuilds the tree.
Removing a template sends no commands; the `[[layout]]` rule takes the workspace back on the next pass.

Per workspace, the last block wins whole: top-level `[[workspace-template]]` in file order, then the
active profile's `[[profile.workspace-template]]`. Load errors: empty `workspaces`, missing or bad
`root`, missing or empty `tree`, an empty nested array, a slot without `app`, a bad `title` regex,
`nth` in `when`, and unknown keys.

Each command takes about 36 ms: a 3-window tree builds in ~0.3 s, a 6-window tree in ~1.2 s. A
focused workspace redraws once per command.

`status` gets one line (`workspace template: live; C applied`, or `inert`). `explain` shows each
template, whether it applies on the monitor its workspace is on, and the window in every slot:

```
workspace-template:
  C    root h   when builtin   [top-level #1]   on Built-in Retina Display: applies
       record: up to date
       tree:
         1 jp.naver.line.mac                 44 LINE
         2 v
           2.1 com.igcl.app                  10 IGCL
           2.2 com.google.Chrome.app.bbdei…  49 Messenger
       rest: none
```

`record:` says what the next pass would do: `up to date`, `pending: first run | template changed |
windows changed`, or `not applied: pinned | when does not match <monitor>`. `reconcile --dry-run`
lists pending rebuilds and their commands; `arrange --dry-run` prints one workspace's commands; and
`daemon --observe` logs them without running them.

## Rollback

```sh
aerospace-responsive uninstall --yes
```

Windows exclusive tiling floated stay floating, a workspace a template built keeps its tree, and
`state.json` (pins, the lock, the rule's memory of what it floated, what each template last built)
stays where it is; a reinstall picks it up.

## Things it will never do

- Read an empty workspace's root layout (that would create and freeze it).
- Flatten unless your rule says so **and** the root needs fixing: the axis or the kind is wrong.
- Change a root's kind (tiles/accordion) unless the rule sets `kind` or flattens (flatten means tiles).
- Poll. Or touch a pinned workspace's layout.
- Write `outer.top` (or any other gap) when your config has no `[gaps]` block.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
