#!/usr/bin/env python3
"""SPEC T31 spike: prove a stock-command recipe that rebuilds a nested tiling tree on a workspace.

Run by hand, not in CI. Opens TextEdit documents A.txt..F.txt, uses scratch workspace X and staging
workspace Y, and for each test tree, start order and visibility:
  scramble X (random order, random joins, so the start tree and most-recent window are arbitrary),
  run the recipe, check focus did not move, show X, `balance-sizes`, read every window's frame
  (`debug-windows` AXFrame) and rebuild the tree from the frames (guillotine cuts). Equal sizes after
  balance-sizes rule out hidden same-axis nesting, which frames alone cannot show.

  python3 -I scripts/spike-workspace-template.py --seeds 2 --log .scratch/workspace-template/desk.jsonl
  python3 -I scripts/spike-workspace-template.py --ssh admin@IP --ssh-opt -i --ssh-opt KEY \
      --aerospace '~/rt/bin/aerospace' --docdir '~/rt/tpl' --log .scratch/workspace-template/vm.jsonl

On the live desk pass --pin, so a daemon pass cannot flatten or re-orient X mid-run.
"""
import argparse, json, random, re, shlex, subprocess, sys, time

WS, STAGE = "X", "Y"
LABELS = "ABCDEF"

# Trees: (axis, [children]); a leaf is a label. All are canonical per SPEC V39.
TREES = {
    "h[A,v[B,C]]": ("h", ["A", ("v", ["B", "C"])]),
    "v[A,h[B,C,D]]": ("v", ["A", ("h", ["B", "C", "D"])]),
    "h[A,v[B,h[C,D]],E]": ("h", ["A", ("v", ["B", ("h", ["C", "D"])]), "E"]),
    "v[h[A,B,C],h[D,v[E,F]]]": ("v", [("h", ["A", "B", "C"]), ("h", ["D", ("v", ["E", "F"])])]),
}

POS = {"h": "right", "v": "down"}
NEG = {"h": "left", "v": "up"}
OTHER = {"h": "v", "v": "h"}


def leaves(node):
    return [node] if isinstance(node, str) else [l for c in node[1] for l in leaves(c)]


def plan(tree, ids, ws=WS):
    """The recipe. ids: label -> window id. Returns argv lists (without the aerospace binary)."""
    axis, _ = tree
    order = leaves(tree)
    cmds = [["flatten-workspace-tree", "--workspace", ws],
            ["layout", "--root", "--workspace", ws, "tiles"],
            ["layout", "--root", "--workspace", ws, "horizontal" if axis == "h" else "vertical"]]
    n = len(order)
    # Order the flat root: push leaves 2..n to the far edge, in order; leaf 1 is left at the front.
    for label in order[1:]:
        for _ in range(n - 1):
            cmds.append(["move", "--window-id", str(ids[label]), "--boundaries", "workspace",
                         "--boundaries-action", "stop", POS[axis]])

    # Top-down nesting: a container's leaves are contiguous flat siblings in its parent (axis p).
    def nest(node, p):
        ls = leaves(node)
        cmds.append(["join-with", "--window-id", str(ids[ls[1]]), NEG[p]])  # joiner bound last = MRU
        for label in ls[2:]:
            cmds.append(["move", "--window-id", str(ids[label]), NEG[p]])  # lands after MRU window
        for child in node[1]:
            if not isinstance(child, str):
                nest(child, node[0])

    for child in tree[1]:
        if not isinstance(child, str):
            nest(child, axis)
    return cmds


class Host:
    def __init__(self, a):
        self.ssh = (["ssh", *a.ssh_opt, a.ssh] if a.ssh else None)
        self.aero_bin = a.aerospace
        self.docdir = a.docdir

    def sh(self, line, check=True):
        argv = [*self.ssh, line] if self.ssh else ["sh", "-c", line]
        r = subprocess.run(argv, capture_output=True, text=True)
        if check and r.returncode != 0:
            raise RuntimeError(f"{line}: {r.stderr.strip()}")
        return r

    def aero(self, *args, check=True):
        line = " ".join([self.aero_bin, *map(shlex.quote, args)])
        t = time.monotonic()
        r = self.sh(line, check=False)
        ms = (time.monotonic() - t) * 1000
        if check and r.returncode != 0:
            raise RuntimeError(f"aerospace {' '.join(args)}: rc {r.returncode} {r.stderr.strip()}")
        return r, ms


def windows(h):
    r, _ = h.aero("list-windows", "--all", "--format",
                  "%{window-id}|%{app-bundle-id}|%{workspace}|%{window-layout}|%{window-title}")
    out = []
    for line in r.stdout.splitlines():
        wid, app, ws, layout, title = line.split("|", 4)
        out.append(dict(id=int(wid), app=app, ws=ws, layout=layout, title=title))
    return out


def setup_windows(h):
    h.sh(f"mkdir -p {h.docdir} && cd {h.docdir} && touch " + " ".join(f"{l}.txt" for l in LABELS))
    h.aero("workspace", STAGE)
    ids = {}
    for label in LABELS:
        found = [w for w in windows(h) if w["app"] == "com.apple.TextEdit" and w["title"] in (f"{label}.txt", label)]
        if not found:
            h.sh(f"open -a TextEdit {h.docdir}/{label}.txt")
            for _ in range(40):
                time.sleep(0.25)
                found = [w for w in windows(h) if w["app"] == "com.apple.TextEdit" and w["title"] in (f"{label}.txt", label)]
                if found:
                    break
        if not found:
            raise RuntimeError(f"no TextEdit window for {label}")
        ids[label] = found[0]["id"]
    for wid in ids.values():
        h.aero("layout", "tiling", "--window-id", str(wid), check=False)
        h.aero("move-node-to-workspace", "--window-id", str(wid), STAGE)
    return ids


def focus_state(h):
    r, _ = h.aero("list-windows", "--focused", "--format", "%{window-id}", check=False)
    ws, _ = h.aero("list-workspaces", "--focused")
    return dict(window=r.stdout.strip(), workspace=ws.stdout.strip())


FRAME = re.compile(r'"AXFrame" : "<AXValue[^{]*\{value = x:([-\d.]+) y:([-\d.]+) w:([-\d.]+) h:([-\d.]+)')


def frame(h, wid):
    r, _ = h.aero("debug-windows", "--window-id", str(wid))
    m = FRAME.search(r.stdout)
    if not m:
        raise RuntimeError(f"no AXFrame for {wid}")
    x, y, w, hh = map(float, m.groups())
    return (x, y, w, hh)


def decompose(items, tol=4.0):
    """items: [(label, (x,y,w,h))] -> (tree, size_errors). Guillotine cuts; all cuts on one axis at once."""
    if len(items) == 1:
        return items[0][0], []
    for axis, lo, ext in (("h", 0, 2), ("v", 1, 3)):
        s = sorted(items, key=lambda it: it[1][lo])
        groups, cur, edge = [], [], None
        for it in s:
            start, end = it[1][lo], it[1][lo] + it[1][ext]
            if cur and start >= edge - tol:
                groups.append(cur)
                cur = []
                edge = None
            cur.append(it)
            edge = end if edge is None else max(edge, end)
        groups.append(cur)
        if len(groups) > 1:
            sizes = [max(i[1][lo] + i[1][ext] for i in g) - min(i[1][lo] for i in g) for g in groups]
            errs = []
            if max(sizes) - min(sizes) > 0.15 * max(sizes):
                errs.append(f"{axis} children unequal after balance-sizes: {[round(x) for x in sizes]}")
            kids = []
            for g in groups:
                t, e = decompose(g, tol)
                kids.append(t)
                errs += e
            return (axis, kids), errs
    return ("?", [i[0] for i in items]), ["windows overlap: no guillotine cut"]


def show(t):
    return t if isinstance(t, str) else f"{t[0]}[{','.join(show(c) for c in t[1])}]"


def scramble(h, ids, labels, rng, visible):
    order = labels[:]
    rng.shuffle(order)
    n = len(order)
    h.aero("flatten-workspace-tree", "--workspace", WS)
    h.aero("layout", "--root", "--workspace", WS, rng.choice(["horizontal", "vertical"]))
    for label in order[1:]:
        for _ in range(n - 1):
            h.aero("move", "--window-id", str(ids[label]), "--boundaries", "workspace",
                   "--boundaries-action", "stop", "right", check=False)
            h.aero("move", "--window-id", str(ids[label]), "--boundaries", "workspace",
                   "--boundaries-action", "stop", "down", check=False)
    joins = []
    for _ in range(rng.choice([1, 2])):
        label, d = rng.choice(labels), rng.choice(["left", "right", "up", "down"])
        r, _ = h.aero("join-with", "--window-id", str(ids[label]), d, check=False)
        joins.append(f"{label} {d} rc{r.returncode}")
    mru = rng.choice(labels)
    if visible:
        h.aero("focus", "--window-id", str(ids[mru]))
    else:
        # bind marks MRU: a swap toward any neighbour makes `mru` the most recent window
        h.aero("move", "--window-id", str(ids[mru]), "--boundaries", "workspace", "--boundaries-action", "stop",
               rng.choice(["left", "right", "up", "down"]), check=False)
    return dict(start_order=order, joins=joins, mru=mru)


def run_case(h, ids, name, tree, visible, seed, home):
    rng = random.Random(f"{name}/{visible}/{seed}")
    want = leaves(tree)
    for label in LABELS:
        h.aero("move-node-to-workspace", "--window-id", str(ids[label]), WS if label in want else STAGE)
    h.aero("workspace", WS if visible else home)
    start = scramble(h, ids, want, rng, visible)
    before = focus_state(h)
    cmds = plan(tree, ids)
    t0 = time.monotonic()
    per = []
    failed = None
    for c in cmds:
        r, ms = h.aero(*c, check=False)
        per.append(round(ms))
        if r.returncode != 0:
            failed = f"{' '.join(c)}: rc {r.returncode} {r.stderr.strip()}"
            break
    wall = round((time.monotonic() - t0) * 1000)
    after = focus_state(h)
    if not visible:
        h.aero("workspace", WS)
    h.aero("balance-sizes", "--workspace", WS)
    time.sleep(0.6)
    frames = [(label, frame(h, ids[label])) for label in want]
    got, errs = decompose(frames)
    layouts = {w["title"]: w["layout"] for w in windows(h) if w["ws"] == WS}
    if not visible:
        h.aero("workspace", home)
    ok = failed is None and show(got) == show(tree) and not errs and before == after
    return dict(tree=name, visible=visible, seed=seed, ok=ok, got=show(got), size_errors=errs,
                focus_before=before, focus_after=after, failed=failed, commands=len(cmds),
                wall_ms=wall, per_cmd_ms=per, layouts=layouts, **start)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ssh")
    ap.add_argument("--ssh-opt", action="append", default=[])
    ap.add_argument("--aerospace", default="aerospace")
    ap.add_argument("--docdir", default="$PWD/.scratch/workspace-template/docs")
    ap.add_argument("--seeds", type=int, default=2)
    ap.add_argument("--trees", default=";".join(TREES))
    ap.add_argument("--visibility", default="focused,hidden")
    ap.add_argument("--log", required=True)
    ap.add_argument("--keep-windows", action="store_true")
    ap.add_argument("--pin", action="store_true", help="live desk: pin X via aerospace-responsive for the run")
    a = ap.parse_args()
    h = Host(a)
    home = focus_state(h)["workspace"]
    cfg = h.aero("config", "--config-path")[0].stdout.strip()
    norm = h.sh(f"grep -h '^enable-normalization' {shlex.quote(cfg)}", check=False).stdout.split("\n")[:-1]
    textedit_was_running = h.sh("pgrep -x TextEdit", check=False).returncode == 0
    ids = setup_windows(h)
    if a.pin:
        h.aero("move-node-to-workspace", "--window-id", str(ids["A"]), WS)
        subprocess.run(["aerospace-responsive", "pin", WS], check=True)
    fails = 0
    with open(a.log, "a") as log:
        log.write(json.dumps(dict(run=time.strftime("%Y-%m-%dT%H:%M:%S"), host=a.ssh or "local",
                                  normalization=norm, ids=ids, home=home)) + "\n")
        try:
            for name in a.trees.split(";"):
                for vis in a.visibility.split(","):
                    for seed in range(a.seeds):
                        res = run_case(h, ids, name, TREES[name], vis == "focused", seed, home)
                        log.write(json.dumps(res) + "\n")
                        log.flush()
                        fails += not res["ok"]
                        print(f"{'ok  ' if res['ok'] else 'FAIL'} {name:26} {vis:7} seed {seed}  "
                              f"{res['commands']:3} cmds {res['wall_ms']:5} ms  got {res['got']}"
                              + (f"  {res['failed']}" if res["failed"] else "")
                              + (f"  {res['size_errors']}" if res["size_errors"] else "")
                              + ("" if res["focus_before"] == res["focus_after"] else
                                 f"  focus {res['focus_before']} -> {res['focus_after']}"))
        finally:
            h.aero("workspace", home, check=False)
            if a.pin:
                subprocess.run(["aerospace-responsive", "unpin", WS])
            if not a.keep_windows:
                for wid in ids.values():
                    h.aero("close", "--window-id", str(wid), check=False)
                if not textedit_was_running:
                    h.sh("osascript -e 'quit app \"TextEdit\"'", check=False)
    print(f"{fails} failure(s); normalization {norm}")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
