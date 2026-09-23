# aerospace-responsive

Reshapes [AeroSpace](https://github.com/nikitabobko/AeroSpace) when displays change: moves
workspaces to the monitors your rules name, and sets each workspace's root orientation (optionally
flattening it) from the monitor it lands on. A resident daemon reacts to CoreGraphics display
reconfiguration — no polling — and every command is a client of that one daemon.

Design record: `.scratch/responsive-layout/map.md` (decisions D1-D26).

## Install

```sh
cargo install --path .
mkdir -p ~/.config/aerospace-responsive
cp aerospace-responsive.example.toml ~/.config/aerospace-responsive/aerospace-responsive.toml
# edit it for your monitors first
aerospace-responsive install            # asks before each step; --yes, --only, --skip for scripts
```

`install` steps: `config` (adds your placement workspaces to `persistent-workspaces`, removes
`workspace-to-monitor-force-assignment`, adds `exec-and-forget aerospace-responsive reconcile` to
`after-startup-command` — format-preserving, backed up, or refused), `launchd` (user LaunchAgent,
logs in `~/Library/Logs/aerospace-responsive/`), `reload` (`aerospace reload-config`, verified),
`reconcile` (first pass). Re-running converges. `uninstall` reverses exactly what `install` added.

## Commands

| Command | Does |
|---|---|
| `reconcile [--dry-run] [--config F]` | Run a pass now, or print the exact `aerospace` commands it would run |
| `explain [--config F]` | Every rule considered and why it won or lost |
| `monitors` | Attached displays as predicates see them (roles, pixels, serial, labels) |
| `status` | Profile, last pass, pins, pending warnings — exits 4 while warnings are pending |
| `pin [ws]` / `unpin [ws\|--all]` / `pins` | Protect a hand-tuned workspace's layout (placement still follows) |
| `profile [lock <name> \| unlock]` | Show, lock or unlock the setup profile |
| `reload` | Reload the config now (it is also watched) |

`--json` on `status`, `explain`, `monitors`, `pins`, `profile`, `reconcile --dry-run`.
Exit codes: 0 ok, 1 error, 2 usage, 3 daemon not running, 4 warnings pending.

`daemon --observe` runs the daemon without executing anything: every pass is logged as the
commands it would have run. This is how to try a config against the live system safely.

## Config

See `aerospace-responsive.example.toml`. Layout rules: first match wins; no match means orientation
from aspect ratio and **never** flatten. Placement rules: first *satisfiable* match wins, so a list
reads as a preference chain; nothing satisfiable leaves the workspace where it is and warns.
Profiles (predicates over the attached set) are consulted before the base rules.

## Rollback

```sh
aerospace-responsive uninstall --yes
```

## Things it will never do

- Read an empty workspace's root layout (that would create and freeze it).
- Flatten unless your rule says so **and** the root axis is about to change.
- Poll. Or touch a pinned workspace's layout.
