# SPEC: exclusive tiling

Source of truth distilled 2026-09-29 from `.scratch/exclusive-tiling/map.md` + tickets 00–12, `docs/adr/0001-*`,
`CONTEXT.md` (glossary: Exclusive tiling, Tiling allowlist, Tile limit, Else-action, Rule-floated window, Pin).
Two repos: daemon = this repo (Rust). fork = `~/Projects/Opensources/AeroSpace` branch `local-use` (Swift).

## §G goal

On named workspaces, only windows the rule's tiling allowlist admits (within tile limits) tile; every other window
arriving tiled gets else-action (float). Motivating case: kitty only tiled app on `T`.

## §C constraints

- C1 Hybrid (ADR 0001): fork adds `window-moved` + `window-closed` to `subscribe`; daemon owns rule, config (tool
  TOML), rule-floated memory, surfaces. Exclusive tiling never writes `aerospace.toml` (V19). A `[gaps]` block may
  rewrite `gaps.outer.top` only (V23). Permanent fork delta.
- C2 Never poll (README promise). No periodic sweep, no open-ended timer.
- C3 Without events → rule inert + warn. No lazy fallback, no config-load refusal.
- C4 One serial worker (`src/worker.rs`); `aero.rs` only door to AeroSpace (CLI subprocess).
- C5 Rule never fights user: acts on arrival only; manual tile/float respected; only undoes own floats.
- C6 `on-window-detected` always wins: arrival signal emitted after its callbacks.
- C7 v1 else-action set = `float` only.
- C8 Uninstall reverses only what install added; runtime effects stay.
- C9 Repo public; fork events published as own branch on `noomz/AeroSpace` (e.g. `feat/window-events`).
- C10 Out of scope: general per-app layout rules; upstreaming events.

## §I interfaces

- I.cfg — tool TOML:
  ```toml
  [[exclusive-tiling]]
  workspaces = ["T"]
  else-action = "float"        # optional, default; other value = load error listing valid set
  tiling-allowlist = [
    { app = "net.kovidgoyal.kitty", title = "Settings", max-tiled = 0 },
    { app = "net.kovidgoyal.kitty", max-tiled = 1 },
  ]
  ```
  Also `[[profile.exclusive-tiling]]`. kebab-case, `deny_unknown_fields`. Entry = `app` (exact bundle id, required),
  `title?` (regex, case-insensitive, unanchored; `regex` crate), `max-tiled?` (absent = unlimited, 0 = carve-out).
- I.fork.events — `subscribe` gains:
  `window-moved {windowId, workspace, prevWorkspace?, appBundleId, appName}` (`prevWorkspace` absent = first
  appearance, after callbacks); `window-closed {windowId, workspace, appBundleId}`. Both in `subscribe --all`. No title.
- I.aero — new daemon commands: `layout floating --window-id N`, `layout tiling --window-id N`;
  reads `list-windows ... --format` incl. `%{window-layout}` (`floating` when floating), `%{app-bundle-id}`,
  `%{window-title}`. Child process: `aerospace subscribe window-moved window-closed`.
- I.state — `state.json` gains: rule-floated set keyed (window id, bundle id) per workspace; per-workspace
  fingerprint of last converged effective rule. Existing `pins`, `lock` untouched.
- I.cli.explain — new `exclusive-tiling:` section (human + `--json`):
  ```
  exclusive-tiling:
    T   else-action float        [top-level #1]
        tiling-allowlist         [profile desk3 #1]
          1 net.kovidgoyal.kitty  title~"Settings"  max-tiled 0
          2 net.kovidgoyal.kitty                    max-tiled 1  (1/1 tiled)
        converge: up to date
        windows:
          4211 kitty   "noomz: ~"      tiled     entry 2 (1/1)
          4380 kitty   "noomz: htop"   floating  rule-floated: entry 2 full
          5012 Safari  "GitHub"        floating  rule-floated: not allowlisted
          5120 Preview "x.pdf"         floating  yours (rule ignores)
          5230 Finder  "Downloads"     tiled     tiled by you, not allowlisted
  ```
  `converge:` ∈ `up to date` | `pending: first run` | `pending: rule changed: <fields>` |
  `pending: rule removed, re-tiles N`. Verdicts (closed set): `entry N (k/limit)`; `rule-floated: not allowlisted |
  entry N full | entry N carve-out`; `yours (rule ignores)`; `tiled by you, not allowlisted | over entry N limit`.
  Inert → section still renders, header carries inert reason. `explain --config F` compares F's rule vs record.
- I.cli.dryrun — `reconcile --dry-run` adds block between placement table and commands:
  ```
  exclusive-tiling converge:
    T    rule changed: tiling-allowlist (profile desk3 #1)
  commands:
    move-workspace-to-monitor --workspace T -- 2
    layout floating --window-id 5230
    layout tiling --window-id 4380
    layout --root --workspace T horizontal
  ```
- I.cli.status — one line, from memory, omitted when no rule: `exclusive tiling: live|inert|observe; rule-floated T:2`.
  Inert → pending warning (exit 4) with child exit code + stderr + hint "exclusive tiling needs an AeroSpace with
  window-moved/window-closed (restart AeroSpace after installing)".
- I.cli.install — no new step. With rule configured, `reconcile` step confirm detail adds "and floats windows your
  exclusive-tiling rules don't admit"; after pass prints status line (+ hint if inert). Inert warns, never fails.
- I.docs — README Requirements links fork events branch, notes stock = inert + warn, restart AeroSpace after install;
  rollback notes rule-floated windows + `state.json` stay. Example config gains commented kitty-on-T block.

## §V invariants

- V1 Rule acts only on windows arriving **tiled**. Arriving floating (dialog, sticky, `on-window-detected` float, user
  float) → untouched, never rule-floated, even if allowlisted.
- V2 Arrival verdict: first matching entry governs. No match → else-action. Match with limit full (incl. 0) →
  else-action. Match with room → nothing. Both else-action cases → rule-floated.
- V3 Tile limit counts entry's windows tiled on workspace **now**, whoever tiled them, by current title (first entry
  each matches now). Manual tiling over limit respected. No promotion when slot frees.
- V4 Titles read at decision time (arrival, converge); title change alone triggers nothing.
- V5 Leave of rule-floated window: destination decides. Dest not exclusive → re-tile. Dest exclusive + admitted with
  room → re-tile. Dest exclusive + not admitted or limit full → stays floating, becomes rule-floated on dest.
  Non-rule-floated windows untouched on leave.
- V6 User-tiled rule-floated window: noticed lazily; at leave already tiled → entry dropped. Tiled then re-floated by
  user before leave → still rule-floated (accepted).
- V7 Converge only on: first run (no record), rule edit/profile switch changing effective rule, rule removal.
  Never on plain daemon/AeroSpace restart. Converge = every window on workspace treated as arrival; more candidates than
  limit → lowest window id keeps slot. Edit → rule-floated now admitted re-tile; tiled now disallowed float (incl.
  hand-tiled). Removal → rule-floated re-tile, record dropped.
- V8 Rule record (fingerprint of last converged effective rule per workspace) persisted in `state.json`; updated only
  after that workspace's converge completes. Startup/pass: missing → converge; equal → nothing; differs → converge;
  record w/o rule → re-tile + drop.
- V9 Rule-floated memory persisted keyed (window id, bundle id). Load/reconnect drops entries whose window gone or bundle
  id differs (recycled `CGWindowID`). `window-closed` prunes eagerly.
- V10 Pass order in `run_pass`: placement → converge → flatten + layout → diff. Abort flag checked before each converge
  cmd. Diff never checks rule. Failed converge cmd = pass error (existing one-repass budget).
- V11 Pin ⟂ rule: pin skips layout only; arrivals + converge run on pinned workspaces.
- V12 Window events = own `Msg::Window`, handled at next `recv`: no debounce, never arms/extends `Pending`, never sets
  abort. Mid-pass events queue, run after. Decision resolves effective rule vs last gated display set (stale profile
  healed by V7).
- V13 Display passes never change float state or membership (fork fact: `move-workspace-to-monitor`,
  `layout --root`, `flatten-workspace-tree`, normalization, monitor disconnect) → emit no `window-moved`.
- V14 Subscribe child runs only while ≥1 rule configured; killed when last removed. Child exit = probe → inert +
  warning. Retry 1s/5s/30s via worker `recv_timeout`, then respawn only on `AeroSocket(true)`, config reload, unlock,
  Startup, explicit `reconcile`. Each (re)connect: one `list-windows --all`, reconcile memory only (prune; apply missed
  leave per V5); no arrivals.
- V15 `--observe`: subscribes, runs rule, logs would-be `layout floating|tiling --window-id`; memory + record
  in-process only, never written to `state.json`.
- V16 Config: per workspace per field, last block setting it wins (top-level file order, then active profile);
  `tiling-allowlist` replaced whole. Rule identity = workspace name. No off switch. Load errors: empty `workspaces`;
  bad `else-action`; entry w/o `app`; invalid `title` regex; negative `max-tiled`; workspace whose effective rule lacks
  `tiling-allowlist` (checked top-level alone + under each profile). `tiling-allowlist = []` valid.
- V17 Fork diff: snapshot `[windowId: (workspace, appBundleId, appName)]` diffed at end of `refreshModel_nonCancellable`
  and heavy refresh session (after callbacks + `normalizeLayoutReason`). Within-workspace rebinds invisible. Frozen
  while `isScreenLocked` (no burst after unlock). No `window-moved` while `isSticky` (entry updates silently);
  `window-closed` still fires. First diff after AeroSpace start seeds silently.
- V18 Daemon ignores `window-detected` for the rule; every `window-moved` = arrival on `workspace` + leave from
  `prevWorkspace` when set.
- V19 Uninstall leaves rule-floated windows + `state.json` untouched. `aerospace.toml` is never changed for exclusive
  tiling. Top gap (V23) is the only runtime writer, and it writes `gaps.outer.top` only.
- V20 Removal converge (record w/o rule) needs no events: runs every pass whether rule live or inert, incl. pass right
  after last rule removed + child killed. Live gate (C3) applies only to converge of configured rules.
- V21 Fork diff frozen from lock until end of first heavy refresh session after unlock (windows GC'd behind lock
  re-register only there). No `refreshModel` diff in between emits.
- V22 Arrival order wins: arrival (and leave dest check) counts only tiled windows daemon already knew on that
  workspace (seen at connect resync or via earlier handled event). Burst: later, not-yet-judged arrival never takes
  earlier arrival's slot. Known map in-process, rebuilt each (re)connect.
- V37 Draw waits for the verdict. A refresh that has `window-moved` asks the daemon `pre-layout` (census + moved
  ids) and applies returned `layout floating|tiling` in the tree before it draws, then broadcasts. No daemon, no
  answer in 100ms, or `--observe` → broadcast and draw as before. Handler never calls `aerospace` (caller is inside
  the refresh; that call deadlocks). A float handed out is recorded at once (a late answer still leaves a tiled window, and its
  event floats it as usual); every other memory or known-map change waits for the window's own `window-moved`, which
  sees whether the answer landed (a landed retile leaves a tiled window: the leave drops the record, no command). Census
  windows outside the batch count only when known there (V22). No subscription → no answers (C3). A moved id already known where the
  census puts it was judged by its event first: skipped (stale request). Census `""` app = NULL-APP-BUNDLE-ID. See B7.

## §T tasks

id|status|task|cites
-|-|-|-
T1|x|fork: snapshot diff as pure fn `(old,new,isLocked,sticky,isFirstDiff)->[ServerEvent]` + unit tests (first appearance, move, close, silent rebind, lock freeze, sticky, startup seed)|V17,I.fork.events
T2|x|fork: wire diff at end of `refreshModel_nonCancellable` + heavy session; `ServerEventType` cases, factories, `subscriptions.swift` arms, docs/grammar; `SubscribeCmdArgsTest` + `ClientServerTest`; `./test.sh` green|V17,I.fork.events
T3|x|fork: push `feat/window-events` to `noomz/AeroSpace`; rebuild + `aerospace-release-install` into local-use; restart AeroSpace|C9
T4|x|daemon config: `[[exclusive-tiling]]` + profile variant, entry parse, `regex` dep, cascade resolve w/ per-field source, load errors|V16,I.cfg
T5|x|daemon state: rule-floated set + rule record in `state.json` (serde default), load pruning|V8,V9,I.state
T6|x|testutil: `FakeAero` windows gain `{app,title,floating}`; `Cmd` float/tile `--window-id` recorded + applied; `CliAero` list-windows fields|I.aero
T7|x|rule core: pure arrival verdict, limit count, leave decision, converge plan (lowest id) + tests for arrival matrix, first match, carve-out, limit/no promotion, leave|V1,V2,V3,V4,V5,V6,V7
T8|x|subscribe child + reader thread → `Msg::Window`; line parser tests; lifecycle (rule-gated spawn/kill, retry deadlines via `tick(later())`, respawn triggers, reconnect resync) + inert warning|V12,V14,V18,C3
T9|x|worker: handle `Msg::Window` immediately (arrival/leave/close prune, memory save) + worker tests|V12,V9,V5
T10|x|pass: converge step between placement and layout; record compare/update; abort check; pinned ws included; tests (first run, edit, profile switch, removal, abort redo, order, startup record cases)|V7,V8,V10,V11
T11|x|observe mode: log would-be cmds, in-process memory/record + test|V15
T12|x|surfaces: explain section + verdicts (one list-windows read), dry-run converge block, status line, `--json` views + render tests|I.cli.explain,I.cli.dryrun,I.cli.status
T13|x|install: reconcile step detail + status line/hint; inert never fails|I.cli.install,V19
T14|x|docs: README requirements/rollback/commands, example config kitty-on-T block|I.docs
T15|x|e2e script (manual run, not CI): scratch ws `X`, TextEdit `max-tiled = 1`, `open -a`, `move-node-to-workspace --window-id`, assert `%{window-layout}` + `subscribe --all` log; covers arrival, limit, leave re-tile, close prune, unfocused move|V1,V3,V5,V9
T16|x|runnable checklist (style `docs/cutover.md`): mouse drag, lock/unlock no burst, AeroSpace restart silent seed, stock client inert warning, kitty-on-T real use; logs under `.scratch/`|V17,C3
T17|x|acceptance gate before live deploy: `cargo test`, fork `./test.sh`, T15 passes, T16 fully ticked|T1-T16,T18,T19,T20
T18|x|daemon: removal converge runs while inert / after child kill; worker test (remove last rule → rule-floated re-tile, record dropped)|V20,V7,V14
T19|x|fork: freeze diff until first heavy session after unlock; pure freeze-state fn + unit tests|V21,V17
T20|x|daemon: known-placement map (resync + events), arrival/leave judged vs known windows; burst worker test; e2e waits for daemon before slot check|V22,V3

## §B bugs

id|date|cause|fix
-|-|-|-
B1|2026-09-30|last rule removed → child killed (V14) → pass inert (C3) → removal converge skipped; rule-floated stay floating, record kept forever|V20
B2|2026-09-30|V17 unfroze at unlock; windows GC'd behind lock return only in first heavy session after; light-session diff between = closed+arrival burst|V21
B3|2026-09-30|burst: 2 windows of limited entry land before first event handled; V3 count-now sees unjudged later one → first arrival floats itself, later keeps slot (T9 misfiled as test bug)|V22
B7|2026-11-11|window-moved broadcast, then layout draws the arrival tiled (siblings shrink), then daemon `layout floating`|V37

# SPEC: top gap

Shipped 2026-10-30 in `8606d4a`, `037e47b` (0.3.0), `e5abd4c`. Ids continue the exclusive-tiling spec (last used V22, T20, B3).

## §G goal

When the tool config has a `[gaps]` block, this tool owns AeroSpace's `gaps.outer.top` and takes the measured notch back off it. The same configured number then means the same clearance on a notched panel and on one whose visible area starts at the physical top. No `[gaps]` block: that key is never touched.

## §C constraints

- C11 AeroSpace lays out inside `visibleFrame` and adds `outer.top` on top of it (`visibleRectPaddedByOuterGaps`). A notched panel already lost that height from `visibleFrame`, so an uncompensated gap sits notch-height lower there.
- C12 Only `gaps.outer.top` is written. `inner.*` and `outer.left` / `right` / `bottom` stay the user's. Install's edit of `aerospace.toml` is unchanged (persistent workspaces, force-assignment, reconcile poke). Uninstall does not revert a gap write.
- C13 The notch is measured from AppKit, never from a name or from `builtin`. A plain menu bar compensates nothing.
- C14 Rewrite is format-preserving. Refuse, and write nothing, when the file does not round-trip, when `gaps` or `gaps.outer` is not a table, or when the edit does not read back as the wanted value.
- C15 A steady state writes nothing and does not run `reload-config`.
- C16 Comments riding on the `outer.top` key survive the rewrite.
- C17 Patterns go out as `monitor.<id>`, AeroSpace's 1-based `%{monitor-id}`. A monitor name may be empty or a positional `" (n)"`.
- C18 Gap problems warn as `Class::World`, not `Class::Pass`. `after_pass` clears `Class::Pass` at the end of a clean pass, which would hide a warning raised mid-pass from `status`.

## §I interfaces

- I.gaps.cfg — tool TOML, top-level only (`[[profile]]` has no gaps field; `deny_unknown_fields` rejects one):
  ```toml
  [gaps]
  top-default = 1                 # required; every monitor no [[gaps.top]] names
  notch-compensation = true       # optional, default true
  add-menu-bar-height-when-single-monitor = false # optional, default false
  [[gaps.top]]
  monitor = { main = true }       # Pred; `nth` is a load error, same as [[layout]]
  value = 42                      # first match wins, file order; else top-default
  ```
- I.gaps.aero — the line written into AeroSpace's config. Bare int when every attached monitor resolves to the same number. Otherwise `[{ monitor.<id> = <n> }, …, <default>]`, the shape `parseDynamicValue` accepts (at least one pattern, plain int last). On change: write, then `aerospace reload-config --no-gui`.
- I.gaps.state — `state.json` `gap_written`: the `TopValue` this tool last recorded as wanted (`None` / absent when no `[gaps]` block). `serde` default, so an older file still loads.
- I.gaps.cli — `monitors` appends `notch Npt` when the measured notch is > 0. `explain` prints the wanted line and the arithmetic, or `gaps: not managed (no [gaps] block): outer.top is left alone`. `reconcile --dry-run` JSON gains `gaps` (the per-monitor decisions). `--observe` logs the would-be line and does not write the file or reload.

## §V invariants

- V23 Ownership. Resolved gaps is one `GapDecision` per attached display when `[gaps]` is present, empty otherwise. Empty → `desired` is `None` → the pass returns before opening AeroSpace's config. No read, no write, no reload, `gap_written` stays unset.
- V24 Per monitor, in `aero_id` order. First matching `[[gaps.top]]`, else `top-default`. Notch subtracted only when `notch-compensation` is on. Add AppKit `NSMenu.menuBarHeight` only when `add-menu-bar-height-when-single-monitor` is on and the joined set has exactly one main display: `(base as f64 - notch + menu_bar_add).round().max(0) as u32`. The daemon checks for height changes every 10 seconds while enabled; a changed height arms a verify pass.
- V25 Notch. `auxiliaryTopLeftArea` and `auxiliaryTopRightArea` are the menu-bar strips beside it. Height is `min(left height, right height)`. Result is `0` when that height is ≤ 0, or when `frame.width - (left width + right width) ≤ 1` (the strips meet: a plain menu bar, or no aux area). Otherwise `min(height, top_inset)`, where `top_inset` is what `visibleFrame` keeps off the top of `frame`. A notch AppKit does not carve out of `visibleFrame` compensates nothing, so a gap is never reduced for height it did not double-count.
- V26 Encoding. Default is the value the most monitors share. A tie goes to the value held by the highest monitor id, so the line is the same every pass. Patterns are the monitors whose result differs, in monitor-id order. No patterns → a bare int.
- V27 Read-back. `current` is `Some` only for a non-negative int, or an array of length ≥ 2 whose last item is a non-negative int and whose earlier items are each a one-key inline table `monitor.<id ≥ 1> = <non-negative int>`. A name or `main` pattern, a negative, a one-element array, or a non-int is `None`: foreign, and the point of a takeover. Invalid TOML is an error and the file is left alone.
- V28 Edit. If the parsed value already equals the wanted value, `changed` is false and the original text is returned, round-trip check skipped. Otherwise `DocumentMut::to_string()` must equal the file bytes; if not, refuse. The wanted expression is rendered in a scratch document first. `gaps` and `gaps.outer` are created as tables when absent. An inline `gaps` or `gaps.outer` refuses (`is not a table`). The `top` key's decor is captured before the insert — leaf decor when its prefix is set, otherwise dotted decor — and put back on the new key. The new text is parsed again and must equal the wanted value; any other result refuses. The caller's file is written only after `plan` returns `changed`.
- V29 Apply, on every gated pass, before any window read (an empty read at wake must not skip the gap). Failures warn `Class::World` / `gaps` and return: cannot locate the config, cannot read it, `plan` refused, cannot write, reload failed after a write that did land. When `gap_written` is `Some` and the on-disk value differs, warn `Class::World` / `gaps-writer` (`outer.top in <path> is <now>, not the <prev> this tool wrote: another writer owns that line`) and still take the line back. `now` is the on-disk TOML, or `a shape this tool does not write`.
- V30 Record. When `gap_written` differs from the wanted value it is set and `save_state` runs before the write. That includes observe, and it includes a write that then fails. See B5.
- V31 Steady state. A later pass whose wanted value parses back as already on disk does not write and does not reload.
- V32 Comments. The decor restore keeps a commented-out alternative on the line above `outer.top`, and leaves neighbouring keys (`inner.horizontal`, `outer.right`, the file header) as they were. A dotted `gaps.outer.top` is rewritten so the file contains one `top` definition, not two.
- V33 Surfaces. `monitors` prints `notch Npt` only when notch > 0. `explain` prints `gaps: outer.top = <toml>` and one row per monitor: id, name, base, source (`gaps.top[i]` or `top-default`), `notch -N` when a notch was subtracted, `menu bar +N` when added, and the value AeroSpace is told. No block prints the not-managed line. Dry-run JSON includes `gaps`.
- V34 Observe. The AeroSpace file is not written and `reload-config` is not run. The would-be line is logged. `save_state` in observe puts exclusive-tiling memory back to what was on disk and writes the rest of `Persisted`, including `gap_written` (B5).
- V35 Load. `[gaps]` requires `top-default`. `notch-compensation` defaults true. `add-menu-bar-height-when-single-monitor` defaults false. Unknown keys (`notch = 32`) are a load error. `nth` on `[[gaps.top]].monitor` is a load error. The commented example block parses when the `# ` prefixes are stripped.
- V36 Deadline. Every one-shot `aerospace` call (reads, commands, `reload-config`) goes through `aero::aerospace`, bounded by `CALL_DEADLINE` (5s). Past it the child is killed and reaped, and the call fails like any other read or command error: the gate retries after the debounce, verify and resync log, a pass records the error, the backstop still fires. `subscribe` is not bounded; its exit already respawns (V14). See B6.

## §T tasks

id|status|task|cites
-|-|-|-
T21|x|config: `[gaps]` / `[[gaps.top]]`, `notch-compensation` default, `nth` rejected, example block round-trip|V35,I.gaps.cfg
T22|x|notch: AppKit aux strips + `top_inset` clamp; fixture built-in 32pt; plain menu bar and uncarved notch are 0|V25,C13
T23|x|`desired` / `current` / `plan`: bare int, exception array, tie-break, foreign takeover, steady state, dotted key, inline table refused, decor kept|V26,V27,V28,V31,V32,C14,C16
T24|x|worker: compensate once then go quiet; another writer warned and taken back; no block never touches the file; observe does not write the file; `Class::World`|V23,V29,V31,V34,C15,C18
T25|x|`explain` arithmetic + not-managed line; `monitors` prints `notch Npt`|V33,I.gaps.cli
T26|x|`state.json` `gap_written`, serde default|V30,I.gaps.state
T27|x|README "Top gap and the notch"; example config commented block; version 0.3.0|I.gaps.cfg,C11
T28|x|live check recorded in `8606d4a`: built-in 42 → 10 (42 − 32); docked MateView keeps 42; no-bar default 1 clamps to 0. `e5abd4c`: commented alternative survived two forced writes|V25,V32,C16
T29|x|optional single-monitor menu bar height; automatic refresh; explain and tests|V24,V33,V35,I.gaps.cfg
T30|x|`aero::aerospace` deadline + kill; `bounded_output` tests: sleeping child, pipes closed but running, 1 MiB stdout drained|V36

## §B bugs

id|date|cause|fix
-|-|-|-
B4|2026-10-30|toml_edit hangs comment lines on the key decor; replacing `outer.top` dropped `#outer.top = [{ monitor.main = 50 }, 10]` from the live config|V32
B5|2026-10-30|`gap_written` is saved before a successful write, and observe's `save_state` persists it (only exclusive-tiling memory is substituted back). A later live pass then warns `gaps-writer` even though this tool never wrote the file, or the write failed|open
B6|2026-11-03|AeroSpace restart: resync's `list-windows --all` reached the dying server and was never answered; `Command::output` had no deadline, so the worker blocked and `reconcile` / `status` hung behind it until `launchctl bootout` (issue #1)|V36

# SPEC: workspace template

Drafted 2026-11-12. Not built. Ids continue the specs above (last used C18, V37, T30, B7).
Terms (add to `CONTEXT.md`, T40): **Workspace template** = a per-workspace rule that names the tiling tree to
build. **Slot** = one leaf of that tree; it names an app, optionally narrowed by title. **Rest windows** = tiled
windows on the workspace that no slot matches. **Rebuild** = flatten the workspace, then build the tree again.

## §G goal

A workspace template rebuilds a nested tiling tree from config, so a hand-made layout comes back by itself.
Motivating case: `C` on the built-in display = `h[ LINE, v[ IGCL, Messenger ] ]`. Today a `[[layout]]` rule sets
the root only, and a pin only protects what was arranged by hand.

## §C constraints

- C19 Stock commands only: `flatten-workspace-tree --workspace`, `move --window-id`, `join-with --window-id`,
  `layout --root --workspace`. No new fork delta: T31 passed, so C19 holds. Read from fork source 2026-11-12,
  confirmed live by T31 the same day: `join-with` wraps the window and its neighbour in a new tiles container of the
  opposite orientation; `move` toward a window sibling swaps; `move` toward a container sibling enters it next
  to that container's most recent child; every command resolves `--window-id` / `--workspace` without focus.
- C20 The daemon cannot read a tree. `list-windows` gives each window's parent layout only and sorts by app
  name, then title; flatten keeps depth-first order, but nothing reports it. So a template never
  compares the live tree with the wanted one. It rebuilds on triggers (V40) and trusts its record (V41).
- C21 Never fights the user (C5 again). Between triggers a hand-made change stays. A trigger rebuilds the whole
  workspace and overwrites it. Pin is the opt-out (V46).
- C22 Nesting alternates. A nested container always has the orientation opposite to its parent, because
  `join-with` builds nothing else. Config cannot ask for `h` inside `h`.
- C23 Tiled windows only. A floating window is never moved, tiled or counted. Exclusive tiling decides float
  state first (V45).
- C24 Never poll (C2). Arrival triggers need the fork's `window-moved` (I.fork.events). Without events, arrival
  triggers are inert and `status` warns; pass triggers and `arrange` still work. The subscribe child runs for
  a template alone (V53).
- C25 Out of scope v1: container sizes and weights (a rebuild resets them); accordion inside the tree; a slot
  that places rest windows; capturing a live tree into config (C20); a per-slot window limit; matching by
  anything but app + title.

## §I interfaces

- I.tpl.cfg — tool TOML:
  ```toml
  [[workspace-template]]
  workspaces = ["C"]
  when = { builtin = true }       # optional monitor Pred; absent = every monitor; `nth` is a load error
  root = "h"                      # required: "h" | "v"
  tree = [                        # required; item = slot table, or array = container (opposite orientation)
    { app = "jp.naver.line.mac" },
    [ { app = "com.igcl.app" },
      { app = "com.google.Chrome.app.bbdeiblfgdokhlblpgeaokenkfknecgl" } ],
  ]
  ```
  Also `[[profile.workspace-template]]`. kebab-case, `deny_unknown_fields`. Slot = `app` (exact bundle id,
  required), `title?` (regex, case-insensitive, unanchored: same as a `tiling-allowlist` entry, no `max-tiled`).
- I.tpl.aero — new daemon commands: `move --window-id N --boundaries workspace --boundaries-action stop <dir>`,
  `join-with --window-id N <dir>`. Reused: `Cmd::Flatten`, `Cmd::Layout`, `Cmd::LayoutKind` (root → tiles).
  Read: one `list-windows --workspace W --format` with id, bundle id, title, `%{window-layout}`.
- I.tpl.state — `state.json` gains per-workspace template record: fingerprint of the effective template + one
  window-id list per slot path (empty slots kept) + the rest list (V41). `serde` default.
- I.tpl.cli.arrange — `aerospace-responsive arrange [ws]` (default: focused workspace). Rebuilds now, even when
  the record is equal. No template for `ws` on its monitor → error, exit non-zero, names why (`no template` |
  `when does not match <monitor>`). Pinned → error `C is pinned; unpin it first`. `--dry-run` prints commands.
- I.tpl.cli.explain — new `workspace-template:` section (human + `--json`):
  ```
  workspace-template:
    C   root h   when builtin        [top-level #1]   on Built-in Retina Display: applies
        record: up to date
        tree:
          1 jp.naver.line.mac                 44 LINE
          2 v
            2.1 com.igcl.app                  10 IGCL
            2.2 com.google.Chrome.app.bbde…   49 Messenger
        rest: none
  ```
  `record:` ∈ `up to date` | `pending: first run` | `pending: template changed` | `pending: windows changed` |
  `not applied: when does not match <monitor>` | `not applied: pinned`. Empty slot prints `(no window)`.
- I.tpl.cli.dryrun — `reconcile --dry-run` adds a `workspace-template rebuild:` block (workspace + reason) after
  the exclusive-tiling block; its commands sit where layout commands sit.
- I.tpl.cli.status — one line, omitted when no template: `workspace template: live|inert; C applied`.
- I.tpl.docs — README config reference + example block (commented, parses when uncommented, V35 style).

## §V invariants

Reviewed by codex 2026-11-12 (R1–R10 below name its findings). V51–V54 and the R-marked text came from it.

- V38 Assignment. Input = the workspace's tiled windows, one read. Each window goes to the first slot (depth-first
  tree order) whose `app` equals its bundle id and whose `title`, if set, matches now. A slot holds every window
  it matches, lowest window id first, side by side in the slot's container. No match → rest window. Rest windows
  go to the end of the root, lowest id first.
- V39 Canonical shape (R1). After assignment, repeat until nothing changes: (1) drop a container with no window;
  (2) replace a one-child container by its child; (3) when a container's child is a container of the same axis,
  merge the child's children into it in place; (4) when the root's only child is a container, the root takes
  that container's axis and children. So the built tree has no one-child container and no same-axis nesting, and
  both AeroSpace normalization settings leave it alone. Root kind = tiles. Root axis = `root`, except after (4).
  A root with one window is valid.
- V40 Triggers (closed set): (a) a pass where there is no record, or the record differs from the wanted record
  (V41); (b) `window-moved` whose `workspace` has an applying template and whose window is tiled once every
  queued window event is judged (V45); (c) `arrange`. Not immediate triggers: `window-closed`, a title change, a
  daemon restart, a hand-made move, float or tile. A hand-made float or tile, or a title change that moves a
  window to another slot, changes the wanted record, so the next pass rebuilds (R8).
- V41 Record (R2, R5). Wanted record = template fingerprint + one window-id list per slot path (empty slots
  included) + the rest list. Before the first command of a rebuild the workspace's record is removed and
  `state.json` saved; the new record is saved only after the last command succeeds. So a rebuild that fails or is
  aborted halfway leaves no record, the partial tree stays, and the next pass rebuilds. Pass: no record →
  rebuild; equal → nothing; differs → rebuild. `window-closed` removes the id from the record, no commands; the
  tree may then hold a one-child container until the next rebuild (same picture, sizes may differ: accepted).
- V42 Applies = effective template exists for the workspace AND (`when` absent OR `when` matches the monitor the
  workspace is on after placement) AND not pinned. While it applies, `plan_layout` and the layout checks in
  `diff` skip that workspace, in a verify-only pass too (R4): `orientation`, `flatten` and `kind` of the matching
  `[[layout]]` rule do nothing there. Placement checks in `diff` stay.
- V43 Rebuild = one command list from a pure fn `(template, tiled windows) -> Vec<Cmd>`: flatten, root kind
  tiles, root axis, order the root, then join and move windows into containers, outermost first. The plan must
  not depend on the order windows have after flatten (the daemon cannot read it, C20) or on which window is
  most recent. Empty workspace → no commands, no record. Running it twice gives the same tree. Recipe (fixed by
  T31; leaves L1..Ln depth-first; pos = right|down, neg = left|up of an axis):
  (1) `flatten-workspace-tree`, `layout --root` tiles, `layout --root` root axis.
  (2) Order: for each of L2..Ln in order, `move --window-id Li --boundaries workspace --boundaries-action stop
  pos(root)` n-1 times. A flat root has only window siblings, so a move swaps; at the edge it is a no-op success.
  L1 ends first.
  (3) Nest top-down: per container K (parent axis p) whose leaves are contiguous flat siblings,
  `join-with --window-id <K's 2nd leaf> neg(p)`. The joiner is bound last, so it is K's most-recent child.
  Then each further leaf: `move --window-id <leaf> neg(p)`. It enters K after its most-recent child, which is
  the previous leaf (`bind` marks most-recent). Recurse into K's sub-containers.
  Join direction matters (from source): joining toward pos leaves the first leaf most-recent, so the next move
  lands in the wrong slot. Not isolated live: the negative control reversed joins and moves together.
  No intermediate step has a one-child container or same-axis nesting, so neither normalization setting changes
  anything between commands. Commands = 3 + (n-1)² + Σ containers (leaves − 1); 6 windows = 33.
  Evidence: 96/96 live builds. mac_jr: all four normalization settings, workspace focused and hidden. Desk
  (off/off): 12 focused + 12 hidden (a first desk hidden batch ran visible by mistake; rerun). Focus same before
  and after every build. Desk: ~36 ms/cmd, 0.26 s (3 windows) to 1.2 s (6 windows). A focused workspace redraws
  once per command; flicker not yet judged by eye (T41). Record `.scratch/workspace-template/RESULT.md`, script
  `scripts/spike-workspace-template.py`.
- V44 Pass order in `run_pass`: placement → converge (V10) → template rebuild → flatten + layout (non-template
  workspaces) → diff. Abort flag checked before each rebuild command. A failed command = pass error (existing
  one-repass budget); V41 makes the repass rebuild.
- V45 Exclusive tiling first (R3). Converge in a pass runs before the rebuild. On window events the worker first
  judges every queued `Msg::Window` (V12), then reads the workspace once and rebuilds. A rule-floated arrival
  triggers nothing by itself.
- V46 Pin ⟂ template: pin skips the rebuild like it skips layout (V11). `unpin` removes that workspace's record
  and arms a pass, so the template rebuilds (R7).
- V47 One rebuild per workspace per drained queue: window events that arrive during a rebuild wait, are judged
  together, and cause at most one more rebuild. Never debounced, never arm `Pending`.
- V48 Config: rule identity = workspace name. Per workspace the last block wins whole (top-level file order, then
  active profile); no per-field merge. Load errors: empty `workspaces`; missing or bad `root`; missing or empty
  `tree`; empty nested array; slot w/o `app`; invalid `title` regex; `nth` in `when`; unknown key in a block or a
  slot. A nested array with one item is valid (V39 collapses it). `tree` item parses as untagged
  `Slot | Vec<Node>`.
- V49 `--observe`: logs the would-be commands; records kept in-process only. `save_state` in observe puts the
  on-disk template records back, like exclusive-tiling memory (V34), also when a pin or gap save runs (R10).
- V50 Uninstall and template removal send no template commands. After removal the `[[layout]]` rule owns the
  workspace again and may flatten it on the next pass (R8).
- V51 Server lifetime (R6). Fork window ids are a counter that restarts with AeroSpace, so a record is valid for
  one AeroSpace server run only. When the daemon sees the server come back (`AeroSocket(true)`, V14) it removes
  every template record; the next pass rebuilds. A daemon restart alone keeps records.
- V52 A float or tile the daemon itself sends for a window on an applying-template workspace (arrival verdict,
  late pre-layout answer, converge) removes that workspace's record, so the queue drain or the next pass
  rebuilds (R3).
- V53 Subscription (R9, amends V14 for this feature). The subscribe child runs while ≥ 1 exclusive-tiling rule
  OR ≥ 1 workspace template is configured. Retry, respawn and inert warning are shared. Each (re)connect resync
  also compares template records with the windows listed, and removes a record that differs.
- V54 Rebuild targets work by id: every command carries `--window-id` or `--workspace`, so a workspace that is
  not focused or not visible is rebuilt the same way, and focus does not move.

## §T tasks

T31 gates T34. T33 needs nothing live and may run beside T31.

id|status|task|cites
-|-|-|-
T31|x|spike, live, scratch ws `X` (TextEdit windows, style T15): prove a recipe builds `h[A, v[B, C]]`, `v[A, h[B, C, D]]`, a 3-level tree and a 6-window tree, from any start order and any most-recent window, with `X` focused and with `X` hidden, under both normalization settings; ordering by push-to-edge moves (`--boundaries-action stop`); focus unchanged; count commands, wall time, visible flicker; record in `.scratch/workspace-template/`. Fails → amend C19 (fork command)|C19,C20,V43,V54
T32|x|config: `[[workspace-template]]` + profile variant, untagged `tree` parse, last-block-wins resolve w/ source, load errors incl. unknown key in slot|V48,I.tpl.cfg
T33|x|core: pure assignment + canonical shape fns + tests (first slot wins, title, many windows in a slot, rest, empty container, one-child collapse, same-axis merge, root promotion, single window)|V38,V39
T34|.|core: pure rebuild plan `-> Vec<Cmd>` per T31 recipe; `Cmd::MoveWindow`, `Cmd::JoinWith`; `FakeAero` gains a tree model that applies them with fork semantics (swap, enter container at most-recent child, join = opposite axis) + tests: canonical tree reached from every start order and most-recent choice, twice = same|V43,V54,I.tpl.aero
T35|.|state: per-slot-path record in `state.json`, serde default; wanted-record compare; remove-before-rebuild, save-after-success|V41,I.tpl.state
T36|.|pass: rebuild step, `plan_layout` + `diff` skip, verify-only skip, `when` vs landed monitor, pin skip, abort, tests (first run, template edit, dock → undock → dock for `C`, daemon restart = no commands, pinned, command fails halfway → repass rebuilds, no mismatch warning for an applied template)|V40,V41,V42,V44
T37|.|worker: judge all queued window events then rebuild once; close prunes record; daemon float/tile drops record; worker tests (burst of arrivals, arrival then rule-float, event during rebuild)|V40,V45,V47,V52
T38|.|worker lifecycle: subscribe child gated on rule OR template; template-only config test; resync record check; `AeroSocket(true)` drops records; `unpin` drops record + arms pass; inert warning|V46,V51,V53,C24
T39|.|surfaces: `arrange` command + ipc, explain section, dry-run block, status line, `--json`; observe keeps disk records through pin / gap saves; render tests|I.tpl.cli.arrange,I.tpl.cli.explain,I.tpl.cli.dryrun,I.tpl.cli.status,V49
T40|.|docs: README section + commented example block guarded by parse test; `CONTEXT.md` terms|I.tpl.docs,V50
T41|.|acceptance at the desk: `C` template on built-in; LINE / IGCL / Messenger quit + reopen; dock to DELL (flatten rule wins) and back; AeroSpace restart rebuilds; pin keeps a hand-made tree, unpin rebuilds; `cargo test` + clippy green; then live deploy|T31-T40

## §B bugs

id|date|cause|fix
-|-|-|-
