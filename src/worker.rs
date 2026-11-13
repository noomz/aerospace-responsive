//! The one writer (D17, D24): a single thread that owns config, state and every mutation.
//! Triggers funnel through a trailing debounce; requests are answered in between passes.

use crate::aero::{self, Aero, Cmd, SubLine, Subscription, WindowEvent};
use crate::config::{self, Config};
use crate::display::{Display, Screen};
use crate::gaps;
use crate::ipc::{Request, Response};
use crate::paths;
use crate::pass::{self, PassReport};
use crate::render::{self, LastPass, MonitorView, PinView, StatusView, TilingStatus};
use crate::restore::{self, RestoreCtx};
use crate::rules::{self, ProfileMode, Resolved};
use crate::state::{self, Class, Persisted, Pin, TemplateRecords, TilingMemory, Warnings};
use crate::template;
use crate::tiling::{self, Leave, Plan, Rule};
use crate::world::{self, WinInfo};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Display,
    Startup,
    Config,
    Unpin,
    Lock,
    Poke,
    Backstop,
    Unlock,
    Repass,
    /// A window was registered. At wake AeroSpace registers windows app by app, so a pass can
    /// read a workspace as empty and skip it; this verify catches it once its windows return.
    WindowDetected,
}

pub enum Msg {
    Trigger(Trigger),
    AeroSocket(bool),
    Request(Request, Sender<Response>),
    /// From the subscribe child: handled at the next `recv`, never debounced, never arms a
    /// pass or sets the abort flag (V12). `window-detected` is the exception: it arms a
    /// debounced verify (`Trigger::WindowDetected`).
    Window(WindowEvent),
    /// The subscribe child ended on its own: the capability probe (V14). `child` names the
    /// child, so a stale report is ignored.
    SubscribeExit { child: u64, code: Option<i32>, stderr: String },
}

const BACKSTOP_DELAY: Duration = Duration::from_secs(10); // P-13
const MENU_BAR_POLL: Duration = Duration::from_secs(10);
const GATE_TRIES: u32 = 3; // P-2
/// V14: bounded respawn after the child exits; after these only existing triggers respawn.
const SUB_RETRY: [Duration; 3] = [Duration::from_secs(1), Duration::from_secs(5), Duration::from_secs(30)];
/// An empty `list-windows` is retried on this schedule, then given up on (ticket 12). Wake
/// needed one retry; login needs ~30s, since apps open their windows after AeroSpace starts.
const EMPTY_RETRY: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
];
pub const INERT_HINT: &str = "exclusive tiling needs an AeroSpace with window-moved/window-closed (restart AeroSpace after installing)";

struct Pending {
    due: Instant,
    /// false = verify-only (unlock, D23).
    full: bool,
    /// This run is the one budgeted re-pass; a fresh trigger merged in resets it.
    repass: bool,
    /// Empty reads this run has retried (`EMPTY_RETRY`); a fresh trigger merged in resets it.
    empty_tries: usize,
    trigger: Trigger,
    waiters: Vec<Sender<Response>>,
}

pub struct Worker<A: Aero, S: FnMut() -> Vec<Screen>> {
    aero: A,
    screens: S,
    config_path: PathBuf,
    state_path: PathBuf,
    config: Config,
    config_text: Option<String>,
    persisted: Persisted,
    warnings: Warnings,
    pub abort: Arc<AtomicBool>,
    debounce: Duration,
    pending: Option<Pending>,
    backstop: Option<Instant>,
    menu_bar_poll: Instant,
    aero_up: bool,
    restore: Option<RestoreCtx>,
    gate_failures: u32,
    last_pass: Option<LastPass>,
    observe: bool,
    /// The worker's own inbox, handed to the subscribe reader thread.
    tx: Option<Sender<Msg>>,
    /// The subscribe child; `Some` = exclusive tiling is live (V14).
    sub: Option<Subscription>,
    sub_gen: u64,
    sub_retries: usize,
    sub_retry_at: Option<Instant>,
    /// Why the rule is inert, while it is (`aerospace subscribe exited 2`).
    inert: Option<String>,
    /// The display set of the last gated pass: window events resolve the rule against it (V12).
    last_ds: Option<Vec<Display>>,
    /// Where the daemon has seen each window (connect resync, then every event): an arrival is
    /// judged only against windows it already knew there, so arrival order wins a burst (V22).
    known: std::collections::BTreeMap<u32, String>,
    /// Where AeroSpace's config lives: its own lookup in production (`paths::aerospace_config`),
    /// a temp file in a test.
    aero_config: Box<dyn Fn() -> Result<PathBuf, String> + Send>,
    /// Exclusive tiling's memory as `state.json` had it: observe mode keeps its own changes
    /// in-process and writes this back instead (V15).
    tiling_on_disk: TilingMemory,
    /// Arrivals (`window-moved`) per workspace since the queue was last drained: a template
    /// rebuild waits until every queued event is judged (V45, V47).
    arrivals: std::collections::BTreeMap<String, BTreeSet<u32>>,
}

fn reply_all(waiters: Vec<Sender<Response>>, r: Response) {
    for w in waiters {
        let _ = w.send(r.clone());
    }
}

fn fmt_err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// (workspace, why it converges) for the dry-run block.
fn reasons(plans: &[Plan]) -> Vec<(String, String)> {
    plans.iter().map(|p| (p.workspace.clone(), p.describe())).collect()
}

impl<A: Aero, S: FnMut() -> Vec<Screen>> Worker<A, S> {
    pub fn new(aero: A, screens: S, config_path: PathBuf, state_path: PathBuf, aero_up: bool, observe: bool) -> Self {
        let persisted = Persisted::load(&state_path);
        let tiling_on_disk = persisted.exclusive_tiling.clone();
        let mut w = Worker {
            aero,
            screens,
            config_path,
            state_path,
            aero_config: Box::new(paths::aerospace_config),
            config: Config::default(),
            config_text: None,
            persisted,
            warnings: Warnings::default(),
            abort: Arc::new(AtomicBool::new(false)),
            debounce: Duration::from_millis(config::TRAILING_FLOOR_MS),
            pending: None,
            backstop: None,
            menu_bar_poll: Instant::now() + MENU_BAR_POLL,
            aero_up,
            restore: None,
            gate_failures: 0,
            last_pass: None,
            observe,
            tx: None,
            sub: None,
            sub_gen: 0,
            sub_retries: 0,
            sub_retry_at: None,
            inert: None,
            last_ds: None,
            known: Default::default(),
            tiling_on_disk,
            arrivals: Default::default(),
        };
        let _ = w.load_config();
        w
    }

    /// Must be called before `run`: the subscribe child reports into this channel.
    pub fn set_sender(&mut self, tx: Sender<Msg>) {
        self.tx = Some(tx);
    }

    pub fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let menu_poll = self.config.gaps.as_ref().filter(|g| g.add_menu_bar_height_when_single_monitor).map(|_| self.menu_bar_poll);
            let due = [self.pending.as_ref().map(|p| p.due), self.backstop, self.sub_retry_at, menu_poll].into_iter().flatten().min();
            let msg = match due {
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
            };
            match msg {
                // A steady stream of window events must not postpone a due pass or retry.
                Ok(m) => {
                    self.handle(m);
                    // V45, V47: judge everything already queued, then rebuild once.
                    while let Ok(m) = rx.try_recv() {
                        self.handle(m);
                    }
                    self.queue_drained();
                    self.tick(Instant::now());
                }
                Err(RecvTimeoutError::Timeout) => self.tick(Instant::now()),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Trigger(Trigger::Config) => {
                if let Ok(true) = self.load_config() {
                    self.ensure_subscription();
                    self.arm(Trigger::Config, true, None);
                }
            }
            Msg::Trigger(Trigger::Unlock) => {
                self.ensure_subscription();
                self.arm(Trigger::Unlock, false, None);
            }
            Msg::Trigger(Trigger::Startup) => {
                self.ensure_subscription();
                self.arm(Trigger::Startup, true, None);
            }
            Msg::Trigger(t) => self.arm(t, true, None),
            Msg::AeroSocket(up) => {
                self.aero_socket(up);
                if up {
                    self.ensure_subscription();
                }
            }
            Msg::Request(req, reply) => self.request(req, reply),
            Msg::Window(WindowEvent::WindowDetected {}) => self.arm(Trigger::WindowDetected, false, None),
            Msg::Window(ev) => self.window_event(ev),
            Msg::SubscribeExit { child, code, stderr } => {
                if child == self.sub_gen && self.sub.take().is_some() {
                    let how = code.map_or("aerospace subscribe was killed".into(), |c| format!("aerospace subscribe exited {c}"));
                    self.sub_failed(how, stderr);
                }
            }
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if self.config.gaps.as_ref().is_some_and(|g| g.add_menu_bar_height_when_single_monitor) && self.menu_bar_poll <= now {
            self.menu_bar_poll = now + MENU_BAR_POLL;
            if self.aero_up && let Some([d]) = self.last_ds.as_deref() && d.screen.main {
                let previous = (d.screen.cg_id, d.screen.menu_bar_height);
                let screens = (self.screens)();
                if let [screen] = screens.as_slice()
                    && screen.cg_id == previous.0
                    && screen.menu_bar_height > 0.0
                    && screen.menu_bar_height != previous.1
                {
                    self.arm(Trigger::Unlock, false, None);
                }
            }
        }
        if self.backstop.is_some_and(|b| b <= now) {
            self.backstop = None;
            self.arm(Trigger::Backstop, true, None);
        }
        if self.pending.as_ref().is_some_and(|p| p.due <= now) {
            self.fire();
        }
        if self.sub_retry_at.is_some_and(|t| t <= now) {
            self.sub_retry_at = None;
            if self.sub.is_none() && self.config.has_exclusive_tiling() {
                self.spawn_subscription();
            }
        }
    }

    /// Trailing debounce: every trigger pushes the deadline out; there is no cap (D17).
    fn arm(&mut self, trigger: Trigger, full: bool, waiter: Option<Sender<Response>>) {
        let due = Instant::now() + self.debounce;
        let repass = trigger == Trigger::Repass;
        match &mut self.pending {
            Some(p) => {
                p.due = due;
                p.repass &= repass;
                p.empty_tries = 0;
                if full && !p.full {
                    p.trigger = trigger;
                }
                p.full |= full;
                p.waiters.extend(waiter);
            }
            None => self.pending = Some(Pending { due, full, repass, empty_tries: 0, trigger, waiters: waiter.into_iter().collect() }),
        }
    }

    fn aero_socket(&mut self, up: bool) {
        self.aero_up = up;
        if up {
            crate::log!("AeroSpace socket present; backstop pass in {}s", BACKSTOP_DELAY.as_secs());
            self.backstop = Some(Instant::now() + BACKSTOP_DELAY);
        } else {
            crate::log!("AeroSpace socket gone; idling until it returns");
            self.backstop = None;
        }
    }

    fn fire(&mut self) {
        let p = self.pending.take().expect("fire only with a pending run");
        self.backstop = None; // any pass satisfies the backstop
        if !self.aero_up {
            crate::log!("pass skipped: AeroSpace is not running");
            return reply_all(p.waiters, Response::err("AeroSpace is not running"));
        }
        self.abort.store(false, Ordering::SeqCst);
        let ds = match self.gate() {
            Ok(ds) => {
                self.gate_failures = 0;
                self.last_ds = Some(ds.clone());
                ds
            }
            Err(why) => {
                self.gate_failures += 1;
                if self.gate_failures < GATE_TRIES {
                    crate::log!("gate: {why}; retrying after the debounce");
                    self.pending = Some(Pending { due: Instant::now() + self.debounce, ..p });
                } else {
                    self.gate_failures = 0;
                    self.warn(Class::Pass, "gate", &format!("AeroSpace and AppKit disagree about the monitors: {why}"));
                    reply_all(p.waiters, Response::err(why));
                }
                return;
            }
        };
        let r = self.resolve_live(&ds);
        let pinned = self.persisted.pinned_names();
        self.apply_gaps(&r); // ahead of any window read: an empty read at wake must not strand the gap
        if self.observe {
            return self.observe_pass(&r, &ds, &pinned, p);
        }
        if !p.full {
            return self.verify_only(&r, &pinned, p);
        }
        if self.restore.is_none() {
            match (self.aero.monitors(), self.aero.workspaces()) {
                (Ok(m), Ok(rows)) => self.restore = Some(RestoreCtx::capture(m, &rows)),
                _ => crate::log!("restore: could not capture visibility; no restore for this disturbance"),
            }
        }
        let before = self.persisted.exclusive_tiling.clone();
        let live = self.sub.is_some(); // inert: only removals converge (C3, V20)
        // V41 saves template records mid-pass, from a pre-pass copy for the rest; the save below
        // brings converged tiling memory up to date.
        let (base, path) = (self.persisted.clone(), self.state_path.clone());
        let mut save = |recs: &TemplateRecords| {
            if let Err(e) = (Persisted { template_records: recs.clone(), ..base.clone() }).save(&path) {
                crate::log!("cannot save state: {e:#}");
            }
        };
        let mem = pass::Memory { tiling: &mut self.persisted.exclusive_tiling, templates: &mut self.persisted.template_records, save: &mut save };
        let result = pass::run_pass(&mut self.aero, &r, &ds, &pinned, &self.abort, Some(mem), live);
        if self.persisted.exclusive_tiling != before {
            self.save_state(); // also after an error: memory follows each executed command
        }
        match result {
            Ok(rep) => self.after_pass(rep, p),
            Err(e) => {
                // The pass may have moved workspaces before dying, so the layout phase and the
                // post-pass diff never ran. Drop the stale visibility baseline (it belongs to a
                // disturbance that is over), warn, and give it the one budgeted retry.
                let msg = format!("pass failed: {e:#}");
                self.restore = None;
                self.warn(Class::Pass, "pass-failed", &msg);
                if !p.repass {
                    self.arm(Trigger::Repass, true, None);
                }
                self.record(p.trigger, msg.clone());
                reply_all(p.waiters, Response::err(msg));
            }
        }
    }

    /// The top gap step. AeroSpace adds `outer.top` on top of a `visibleFrame` that already
    /// excludes a notch, so a notched panel gets that height taken back; the same configured
    /// number then means the same distance from the usable edge on every monitor. No `[gaps]`
    /// block means the line is not ours and is never touched. A steady state writes nothing.
    ///
    /// Every problem here warns as `Class::World`: it is re-derived from the file on
    /// every pass, so it clears itself as soon as the cause goes, and a clean pass cannot
    /// swallow it the way it swallows a `Class::Pass` warning raised mid-pass.
    fn apply_gaps(&mut self, r: &Resolved) {
        let Some(want) = gaps::desired(&r.gaps) else { return };
        let path = match (self.aero_config)() {
            Ok(p) => p,
            Err(e) => {
                self.warn(Class::World, "gaps", &e);
                return;
            }
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.warn(Class::World, "gaps", &format!("cannot read {}: {e}", path.display()));
                return;
            }
        };
        let plan = match gaps::plan(&text, &want) {
            Ok(p) => p,
            Err(e) => {
                self.warn(Class::World, "gaps", &format!("outer.top left alone: {e:#}",));
                return;
            }
        };
        if let Some(prev) = self.persisted.gap_written.clone() {
            if plan.on_disk.as_ref() != Some(&prev) {
                let now = plan.on_disk.as_ref().map_or_else(|| "a shape this tool does not write".to_string(), |v| v.to_toml());
                self.warn(
                    Class::World,
                    "gaps-writer",
                    &format!("outer.top in {} is {now}, not the {prev:?} this tool wrote: another writer owns that line", path.display()),
                );
            }
        }
        if self.persisted.gap_written.as_ref() != Some(&want) {
            self.persisted.gap_written = Some(want.clone());
            self.save_state();
        }
        if !plan.changed {
            return;
        }
        if self.observe {
            crate::log!("gaps: would write outer.top = {} to {}", want.to_toml(), path.display());
            return;
        }
        if let Err(e) = std::fs::write(&path, &plan.new_text) {
            self.warn(Class::World, "gaps", &format!("cannot write {}: {e}", path.display()));
            return;
        }
        let from = plan.on_disk.map_or_else(|| "-".to_string(), |v| v.to_toml());
        crate::log!("gaps: outer.top {from} -> {} in {}", want.to_toml(), path.display());
        if let Err(e) = self.aero.run(&Cmd::ReloadConfig) {
            self.warn(Class::World, "gaps", &format!("wrote outer.top = {} but AeroSpace did not reload: {e}", want.to_toml()));
        } else {
            // Worth one line: a reload is the moment AeroSpace re-reads every gap, and a silent
            // one is hard to attribute afterwards.
            crate::log!("  {}", Cmd::ReloadConfig);
        }
    }

    fn after_pass(&mut self, rep: PassReport, p: Pending) {
        for e in &rep.executed {
            match &e.error {
                None => crate::log!("  {}  ({} ms)", e.cmd, e.ms),
                Some(err) => crate::log!("  {}  ({} ms) -> FAILED: {err}", e.cmd, e.ms),
            }
        }
        for d in &rep.drifted {
            crate::log!("drift: {d} during the pass; this pass did not move it");
        }
        if rep.aborted || rep.empty_snapshot {
            // An abort restarts a full reconcile, which ticket 03 says owns the next run — so it
            // gets its budgets back. An empty read is a retry of the same run, on a bounded
            // schedule, or a genuinely empty model would re-arm forever.
            let (why, retry, empty_tries) = if rep.aborted {
                ("aborted by a display change", Some(self.debounce), 0)
            } else {
                ("list-windows came back empty", EMPTY_RETRY.get(p.empty_tries).copied(), p.empty_tries + 1)
            };
            let Some(delay) = retry else {
                self.restore = None; // the disturbance is over; do not replay it later
                self.record(p.trigger, why.into());
                return reply_all(p.waiters, Response::err(why));
            };
            crate::log!("pass {why}; retrying in {}s", delay.as_secs());
            self.pending = Some(Pending { due: Instant::now() + delay, repass: false, empty_tries, ..p });
            return;
        }
        for ws in &rep.converged {
            crate::log!("exclusive tiling: {ws} converged");
        }
        let cleared = self.persisted.drop_empty(&rep.nonempty);
        if !cleared.is_empty() {
            crate::log!("pin cleared (workspace empty): {}", cleared.join(" "));
            self.save_state();
        }
        for (ws, err) in &rep.refused {
            let (key, msg) = (format!("refused:{ws}"), format!("move of workspace {ws} was refused: {err}"));
            self.warn(Class::Pass, &key, &msg);
        }
        if let Some(ctx) = &mut self.restore {
            ctx.moves.extend(rep.moves.iter().cloned());
            ctx.left_visible = rep.left_visible.clone();
        }
        if !rep.mismatches.is_empty() && !p.repass {
            crate::log!("post-pass diff: {}; one re-pass", rep.mismatches.join("; "));
            self.arm(Trigger::Repass, true, None);
            if let Some(next) = &mut self.pending {
                next.waiters.extend(p.waiters);
            }
            return;
        }
        let result = if rep.mismatches.is_empty() {
            if rep.refused.is_empty() {
                self.warnings.clear(Class::Pass);
            }
            format!("ok, {} command(s) in {:.1}s", rep.executed.len(), rep.ms as f64 / 1000.0)
        } else {
            let msg = format!("state still differs after one re-pass: {}", rep.mismatches.join("; "));
            self.warn(Class::Pass, "mismatch", &msg);
            "mismatch after re-pass".to_string()
        };
        self.restore_visibility();
        self.record(p.trigger, result.clone());
        reply_all(p.waiters, Response::ok(format!("pass {result}\n"), json!(rep)));
    }

    fn verify_only(&mut self, r: &Resolved, pinned: &BTreeSet<String>, p: Pending) {
        match self.aero.windows() {
            Ok(s) if s.windows.is_empty() && !p.repass => {
                crate::log!("verify: empty read at wake; retrying once");
                self.arm(Trigger::Repass, false, None);
            }
            Ok(s) => {
                let m = pass::diff(r, &s, pinned, &BTreeSet::new());
                if m.is_empty() {
                    if p.trigger != Trigger::WindowDetected {
                        crate::log!("verify: model matches"); // every new window would log it
                    }
                } else if !p.repass {
                    crate::log!("verify: {}; one re-pass", m.join("; "));
                    self.arm(Trigger::Repass, true, None);
                } else {
                    let msg = format!("state still differs after one re-pass: {}", m.join("; "));
                    self.warn(Class::Pass, "mismatch", &msg);
                }
            }
            Err(e) => crate::log!("verify failed: {e:#}"),
        }
        reply_all(p.waiters, Response::ok("verified\n", Value::Null));
    }

    fn observe_pass(&mut self, r: &Resolved, ds: &[Display], pinned: &BTreeSet<String>, p: Pending) {
        // V15: the converge is logged, and taken as done in-process only.
        let plans = self.converge_plans(&r.exclusive_tiling);
        let text = match self.aero.windows() {
            Ok(s) => {
                let cmds = pass::dry_run(r, &s, ds, pinned, &plans);
                crate::log!("observe: would run {} command(s)", cmds.len());
                for c in &cmds {
                    crate::log!("  would run: {c}");
                }
                for plan in &plans {
                    tiling::commit(&mut self.persisted.exclusive_tiling, plan);
                }
                render::dry_run(r, &cmds, &reasons(&plans))
            }
            Err(e) => format!("observe: {e:#}\n"),
        };
        self.record(p.trigger, "observed (nothing executed)".into());
        reply_all(p.waiters, Response::ok(text, Value::Null));
    }

    /// Cosmetic (D26 S1): never verified, never warns, never re-passes.
    fn restore_visibility(&mut self) {
        let Some(ctx) = self.restore.take() else { return };
        let (Ok(mons), Ok(rows), Ok(snap)) = (self.aero.monitors(), self.aero.workspaces(), self.aero.windows()) else {
            crate::log!("restore: fresh read failed; skipped");
            return;
        };
        let (cmds, skips) = restore::plan_restore(&ctx, &mons, &rows, &snap.nonempty());
        for s in skips {
            crate::log!("restore: {s}");
        }
        for c in cmds {
            if let Err(e) = self.aero.run(&c) {
                crate::log!("restore: {c} failed: {e}");
            }
        }
    }

    /// D17 gate steps 2-3 (P-1): a probe command, then two identical, joinable reads.
    fn gate(&mut self) -> Result<Vec<Display>, String> {
        self.aero.monitors().map_err(fmt_err)?; // probe: any command refreshes AeroSpace's model
        let a = (self.aero.monitors().map_err(fmt_err)?, (self.screens)());
        let b = (self.aero.monitors().map_err(fmt_err)?, (self.screens)());
        if a != b {
            return Err("the monitor view changed between two reads".into());
        }
        world::join(&a.0, &a.1)
    }

    /// Resolve against the live config: apply the lock's auto-release (02 M1) and recompute
    /// world-dependent warnings (P-10).
    fn resolve_live(&mut self, ds: &[Display]) -> Resolved {
        let r = rules::resolve(&self.config, ds, self.persisted.lock.as_deref());
        if r.lock_released {
            crate::log!("profile lock {:?} released: the attached monitors no longer match", self.persisted.lock);
            self.persisted.lock = None;
            self.save_state();
        }
        let mut fresh = vec![];
        if self.config.settings.warn_on_no_match {
            for n in &r.no_match {
                fresh.push((format!("no-match:{n}"), n.clone()));
            }
            for p in r.placement.iter().filter(|p| p.target.is_none()) {
                fresh.push((
                    format!("unsatisfiable:{}", p.workspace),
                    format!("no placement rule for workspace {} is satisfiable; leaving it where it is", p.workspace),
                ));
            }
        }
        for m in self.warnings.replace(Class::World, fresh) {
            crate::log!("warning: {m}");
        }
        for i in &r.info {
            crate::log!("info: {i}");
        }
        r
    }

    /// Ok(true) = a new config was adopted; Ok(false) = unchanged; Err = kept the old one (V4).
    fn load_config(&mut self) -> Result<bool, String> {
        let text = match std::fs::read_to_string(&self.config_path) {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("cannot read {}: {e}", self.config_path.display());
                self.set_config_warnings(vec![("config".into(), msg.clone())]);
                return Err(msg);
            }
        };
        if self.config_text.as_deref() == Some(text.as_str()) {
            return Ok(false);
        }
        match config::parse(&text) {
            Ok(loaded) => {
                self.debounce = Duration::from_millis(loaded.config.settings.debounce.trailing_ms);
                self.config = loaded.config;
                self.config_text = Some(text);
                self.set_config_warnings(loaded.warnings.into_iter().map(|w| ("floor".to_string(), w)).collect());
                crate::log!("config loaded from {}", self.config_path.display());
                Ok(true)
            }
            Err(e) => {
                let msg = format!("config not loaded, keeping the previous one: {e:#}");
                self.config_text = None; // so reverting the file re-parses and clears this
                self.set_config_warnings(vec![("parse".into(), msg.clone())]);
                Err(msg)
            }
        }
    }

    fn set_config_warnings(&mut self, fresh: Vec<(String, String)>) {
        for m in self.warnings.replace(Class::Config, fresh) {
            crate::log!("warning: {m}");
        }
    }

    fn warn(&mut self, class: Class, key: &str, msg: &str) {
        if self.warnings.raise(class, key, msg) {
            crate::log!("warning: {msg}");
        }
    }

    fn record(&mut self, trigger: Trigger, result: String) {
        crate::log!("pass ({trigger:?}): {result}");
        self.last_pass = Some(LastPass { at: state::now(), trigger: format!("{trigger:?}").to_lowercase(), result });
    }

    fn save_state(&mut self) {
        let saved = if self.observe {
            &Persisted { exclusive_tiling: self.tiling_on_disk.clone(), ..self.persisted.clone() }
        } else {
            &self.persisted
        };
        if let Err(e) = saved.save(&self.state_path) {
            crate::log!("cannot save state: {e:#}");
        }
    }

    // ---------- exclusive tiling: the subscribe child (V14) ----------

    /// On a trigger that may have fixed the connection (Startup, config reload, unlock,
    /// AeroSpace's socket appearing, explicit reconcile): run the child iff a rule exists.
    fn ensure_subscription(&mut self) {
        if !self.config.has_exclusive_tiling() {
            if self.sub.take().is_some() {
                crate::log!("exclusive tiling: no rule left; subscribe child stopped");
            }
            self.sub_retry_at = None;
            self.inert = None;
            self.warnings.clear(Class::ExclusiveTiling);
            return;
        }
        self.sub_retries = 0;
        if self.sub.is_none() {
            self.spawn_subscription();
        }
    }

    fn spawn_subscription(&mut self) {
        self.sub_retry_at = None;
        let Some(tx) = self.tx.clone() else { return };
        if !self.aero_up {
            return; // its socket appearing respawns
        }
        self.sub_gen += 1;
        let child = self.sub_gen;
        let sink = Box::new(move |l: SubLine| match l {
            SubLine::Line(line) => {
                for ev in aero::parse_events(&line) {
                    let _ = tx.send(Msg::Window(ev));
                }
            }
            SubLine::Exit { code, stderr } => {
                let _ = tx.send(Msg::SubscribeExit { child, code, stderr });
            }
        });
        match self.aero.subscribe(sink) {
            Ok(s) => {
                self.sub = Some(s);
                self.inert = None;
                self.warnings.clear(Class::ExclusiveTiling);
                crate::log!("exclusive tiling: subscribe child started");
                self.resync();
            }
            Err(e) => self.sub_failed("aerospace subscribe could not start".into(), format!("{e:#}")),
        }
    }

    /// The child is gone (or never started): inert, warn, and retry a bounded number of times.
    fn sub_failed(&mut self, how: String, stderr: String) {
        let detail = if stderr.is_empty() { String::new() } else { format!(" ({stderr})") };
        self.warn(Class::ExclusiveTiling, "exclusive-tiling", &format!("exclusive tiling is inert: {how}{detail}. Hint: {INERT_HINT}"));
        self.inert = Some(how);
        if let Some(d) = SUB_RETRY.get(self.sub_retries) {
            self.sub_retry_at = Some(Instant::now() + *d);
            self.sub_retries += 1;
        }
    }

    /// V14: on each (re)connect, one read reconciles memory only — prune gone and recycled
    /// windows, apply the leaves missed while disconnected. No arrivals.
    fn resync(&mut self) {
        let wins = match self.aero.window_details() {
            Ok(w) => w,
            Err(e) => return crate::log!("exclusive tiling: resync read failed: {e:#}"),
        };
        self.known = wins.iter().map(|w| (w.id, w.workspace.clone())).collect();
        let live: Vec<(u32, &str)> = wins.iter().map(|w| (w.id, w.app.as_str())).collect();
        let mut changed = self.persisted.exclusive_tiling.prune(&live);
        let rules = self.current_rules();
        let mem = &self.persisted.exclusive_tiling;
        let left: Vec<&WinInfo> = wins.iter().filter(|w| mem.floated_on(w.id, &w.app).is_some_and(|ws| ws != w.workspace)).collect();
        for w in left {
            self.apply_leave(w, &wins, &rules);
            changed = true;
        }
        if changed {
            self.save_state();
        }
    }

    /// V5, V6: a rule-floated window is now on `w.workspace`; memory follows the decision.
    fn apply_leave(&mut self, w: &WinInfo, wins: &[WinInfo], rules: &[Rule]) -> Leave {
        let decision = tiling::leave(w, rules.iter().find(|r| r.workspace == w.workspace), wins);
        match &decision {
            Leave::Drop => {
                self.persisted.exclusive_tiling.remove(w.id);
            }
            Leave::Retile => {
                if self.exec(&Cmd::Tile { id: w.id }, &format!("left for {}", w.workspace)) {
                    self.persisted.exclusive_tiling.remove(w.id);
                    self.drop_template_record(&w.workspace);
                }
            }
            Leave::StayFloated(why) => {
                crate::log!("exclusive tiling: window {} stays floating on {}: {why}", w.id, w.workspace);
                self.persisted.exclusive_tiling.insert(&w.workspace, w.id, &w.app);
            }
        }
        decision
    }

    /// V12, V18: `window-moved` = arrival on `workspace`, plus a leave when the window is
    /// rule-floated elsewhere; `window-closed` prunes memory eagerly (V9).
    fn window_event(&mut self, ev: WindowEvent) {
        self.sub_retries = 0; // a line proves the connection works
        let (id, workspace, app) = match ev {
            WindowEvent::WindowClosed { id, app, .. } => {
                self.known.remove(&id);
                let floated = self.persisted.exclusive_tiling.floated_on(id, &app).is_some() && self.persisted.exclusive_tiling.remove(id);
                if template::forget(&mut self.persisted.template_records, id) | floated {
                    self.save_state();
                }
                return;
            }
            WindowEvent::WindowMoved { id, workspace, app, .. } => (id, workspace, app),
            WindowEvent::WindowDetected {} => unreachable!("handle routes window-detected to a verify"),
        };
        self.known.insert(id, workspace.clone()); // judged below; later arrivals count it
        self.arrivals.entry(workspace.clone()).or_default().insert(id);
        let left = self.persisted.exclusive_tiling.floated_on(id, &app).is_some_and(|ws| ws != workspace);
        let rules = self.current_rules();
        let rule = rules.iter().find(|r| r.workspace == workspace).cloned();
        if rule.is_none() && !left {
            return; // no rule here, nothing of ours leaving: no read
        }
        self.decide_moved(id, &workspace, rule.as_ref(), left, &rules);
    }

    fn decide_moved(&mut self, id: u32, workspace: &str, rule: Option<&Rule>, left: bool, rules: &[Rule]) {
        let wins = match self.aero.window_details() {
            Ok(w) => w,
            Err(e) => return crate::log!("exclusive tiling: window read failed, window {id} not decided: {e:#}"),
        };
        let Some(w) = wins.iter().find(|w| w.id == id) else {
            if self.persisted.exclusive_tiling.remove(id) {
                self.save_state();
            }
            return; // closed meanwhile
        };
        if w.workspace != workspace {
            return; // moved on again: its next event decides
        }
        // V22: a window whose own arrival here is still queued does not count yet.
        let wins: Vec<WinInfo> = wins.iter().filter(|x| x.id == id || self.known.get(&x.id) == Some(&x.workspace)).cloned().collect();
        let Some(w) = wins.iter().find(|x| x.id == id) else { return };
        let mut changed = false;
        if left {
            changed = true;
            if self.apply_leave(w, &wins, rules) != Leave::Drop {
                return self.save_state();
            }
        }
        // The arrival: only a window arriving tiled is the rule's business (V1, V2).
        match rule.map(|rule| tiling::arrival(rule, w, &wins)) {
            Some(tiling::Arrival::Float(why)) => {
                if self.exec(&Cmd::Float { id }, &format!("arrived on {workspace}: {why}")) {
                    self.persisted.exclusive_tiling.insert(workspace, id, &w.app);
                    self.drop_template_record(workspace);
                    changed = true;
                }
            }
            // Admitted tiled, yet recorded here: a pre-layout float that never landed (V37). The
            // rule does not own a tiled window, so the record goes.
            Some(tiling::Arrival::Admit) if self.persisted.exclusive_tiling.floated_on(id, &w.app) == Some(workspace) => {
                self.persisted.exclusive_tiling.remove(id);
                changed = true;
            }
            _ => {}
        }
        if changed {
            self.save_state();
        }
    }

    /// V52: a float or tile the daemon sent changed a template workspace's tiled set (an
    /// arrival verdict, which also catches a late pre-layout answer, or a retile on a leave). Dropping
    /// its record makes the queue drain or the next pass rebuild. A pinned workspace keeps its
    /// record (V46). The caller saves.
    fn drop_template_record(&mut self, ws: &str) {
        if !self.persisted.pins.iter().any(|p| p.workspace == ws) && self.persisted.template_records.remove(ws).is_some() {
            crate::log!("workspace template: {ws} record dropped (the daemon changed its tiled windows)");
        }
    }

    /// V40 (b), V45, V47: every queued window event is judged; now each workspace an arrival
    /// landed on rebuilds once, if its template applies and an arrived window is still tiled
    /// there. One read of the monitors' windows, one of window details. Never debounced.
    pub fn queue_drained(&mut self) {
        let arrivals = std::mem::take(&mut self.arrivals);
        // Before the first gated pass, that pass decides (V40 a).
        let (false, Some(ds), true) = (arrivals.is_empty(), self.last_ds.clone(), self.aero_up) else { return };
        let r = rules::resolve(&self.config, &ds, self.persisted.lock.as_deref());
        if !r.templates.iter().any(|d| arrivals.contains_key(&d.template.workspace)) {
            return; // no read for a workspace without a template
        }
        let (s, details) = match (self.aero.windows(), self.aero.window_details()) {
            (Ok(s), Ok(d)) => (s, d),
            (Err(e), _) | (_, Err(e)) => return crate::log!("workspace template: window read failed; the next pass decides: {e:#}"),
        };
        let applying = r.applying(&s, &self.persisted.pinned_names());
        let due = r.templates.iter().map(|d| &d.template).filter(|t| {
            applying.contains(&t.workspace)
                && arrivals.get(&t.workspace).is_some_and(|ids| details.iter().any(|w| ids.contains(&w.id) && w.workspace == t.workspace && w.tiled()))
        });
        let due: Vec<template::Template> = due.cloned().collect();
        for t in &due {
            self.rebuild(t, &details);
        }
    }

    /// One forced rebuild outside a pass (V41: the record only after the last command). A
    /// failure leaves no record, so the next pass rebuilds. Observe logs, records in-process.
    fn rebuild(&mut self, t: &template::Template, details: &[WinInfo]) {
        let (observe, base, path) = (self.observe, self.persisted.clone(), self.state_path.clone());
        let mut save = |recs: &TemplateRecords| {
            if observe {
                return;
            }
            if let Err(e) = (Persisted { template_records: recs.clone(), ..base.clone() }).save(&path) {
                crate::log!("cannot save state: {e:#}");
            }
        };
        let mut executed = vec![];
        let result = if observe {
            let wins: Vec<WinInfo> = details.iter().filter(|w| w.workspace == t.workspace).cloned().collect();
            for c in template::rebuild(t, &wins) {
                crate::log!("workspace template: would run: {c}");
            }
            self.persisted.template_records.insert(t.workspace.clone(), template::wanted(t, &wins));
            Ok(pass::Rebuilt::Built)
        } else {
            pass::rebuild_template(&mut self.aero, t, details, &mut self.persisted.template_records, &mut save, &self.abort, true, &mut executed)
        };
        for e in &executed {
            crate::log!("  {}  ({} ms)", e.cmd, e.ms);
        }
        let why = match result {
            Ok(pass::Rebuilt::Built) => return crate::log!("workspace template: {} rebuilt (window arrived)", t.workspace),
            Ok(pass::Rebuilt::Nothing) => return,
            Ok(pass::Rebuilt::Aborted) => "aborted".to_string(),
            Err(e) => format!("failed: {e:#}"),
        };
        // V47 exception (B8): the partial tree must not wait for an unrelated trigger.
        crate::log!("workspace template: {} rebuild {why}; one re-pass", t.workspace);
        self.arm(Trigger::Repass, true, None);
    }

    /// The converge plans a pass would run now, against a fresh window read: while inert only
    /// removals (C3, V20); none when nothing is ruled.
    fn converge_plans(&mut self, rules: &[Rule]) -> Vec<Plan> {
        if rules.is_empty() && self.persisted.exclusive_tiling.records.is_empty() {
            return vec![];
        }
        match self.aero.window_details() {
            Ok(wins) => {
                let live = self.sub.is_some();
                tiling::plan_converge(rules, &self.persisted.exclusive_tiling, &wins).into_iter().filter(|p| tiling::runnable(p, live)).collect()
            }
            Err(e) => {
                crate::log!("exclusive tiling: window read failed: {e:#}");
                vec![]
            }
        }
    }

    /// Effective rules against the last gated display set (V12), or a fresh read before the
    /// first pass.
    fn current_rules(&mut self) -> Vec<Rule> {
        let ds = match self.last_ds.clone() {
            Some(ds) => ds,
            None => self.world().unwrap_or_default(),
        };
        rules::resolve(&self.config, &ds, self.persisted.lock.as_deref()).exclusive_tiling
    }

    /// The refresh asks before it draws. Answer from the census it sent. Never call AeroSpace
    /// here: the caller is inside that refresh, and a CLI call waits for it to finish.
    fn pre_layout(&mut self, mut windows: Vec<WinInfo>, moved: Vec<u32>) -> Response {
        if self.observe {
            return Response::ok("", json!([]));
        }
        // No subscription: the rule is inert (C3), and no event would follow up.
        let (Some(ds), true) = (self.last_ds.clone(), self.sub.is_some()) else {
            return Response::ok("", json!([]));
        };
        let rules = rules::resolve(&self.config, &ds, self.persisted.lock.as_deref()).exclusive_tiling;
        // The fork's census sends "" for an app without a bundle id; events and CLI reads say
        // NULL-APP-BUNDLE-ID. One identity, or a record made here is never matched again.
        for w in windows.iter_mut().filter(|w| w.app.is_empty()) {
            w.app = crate::aero::null_app();
        }
        // An arrival already known where the census puts it was judged by its own event (or a
        // resync) first: this request is stale for it (AeroSpace stopped waiting). Skip it.
        let moved: Vec<u32> = moved
            .into_iter()
            .filter(|id| windows.iter().find(|w| w.id == *id).is_none_or(|w| self.known.get(id) != Some(&w.workspace)))
            .collect();
        // Judge against copies (batch order needs them), then keep only the floats handed out.
        // A float recorded at once is safe either way: if AeroSpace stopped waiting and drew the
        // window tiled, its event floats it as usual. Everything else (retile, leave, the known
        // map) waits for the window's own event, which sees whether the answer landed: a retile
        // that landed leaves a tiled window, and its leave drops the record with no command.
        let (mut memory, mut known) = (self.persisted.exclusive_tiling.clone(), self.known.clone());
        let cmds = tiling::pre_layout(&rules, &mut memory, &mut known, &windows, &moved);
        let mut changed = false;
        for cmd in &cmds {
            if let Cmd::Float { id } = cmd
                && let Some(w) = windows.iter().find(|w| w.id == *id)
            {
                self.persisted.exclusive_tiling.insert(&w.workspace, *id, &w.app);
                changed = true;
            }
        }
        if changed {
            self.save_state();
        }
        Response::ok("", json!(cmds))
    }

    /// One rule command, or its log line in observe mode. True when it took effect.
    fn exec(&mut self, cmd: &Cmd, why: &str) -> bool {
        if self.observe {
            crate::log!("exclusive tiling: would run: {cmd}  ({why})");
            return true;
        }
        match self.aero.run(cmd) {
            Ok(()) => {
                crate::log!("exclusive tiling: {cmd}  ({why})");
                true
            }
            Err(e) => {
                crate::log!("exclusive tiling: {cmd} failed: {e}");
                false
            }
        }
    }

    // ---------- requests ----------

    fn request(&mut self, req: Request, reply: Sender<Response>) {
        let resp = match req {
            Request::Reconcile { dry_run: false, .. } => {
                self.ensure_subscription();
                return self.arm(Trigger::Poke, true, Some(reply));
            }
            Request::Reconcile { dry_run: true, config } => self.dry_run(config),
            Request::Explain { config } => self.explain(config),
            Request::Monitors => self.monitors(),
            Request::Status => self.status(),
            Request::Pin { workspace } => self.pin(workspace),
            Request::Unpin { workspace, all } => self.unpin(workspace, all),
            Request::Pins => Ok(self.pins()),
            Request::Profile => self.profile(),
            Request::ProfileLock { name } => self.lock(name),
            Request::ProfileUnlock => Ok(self.unlock()),
            Request::Reload => self.reload(),
            Request::PreLayout { windows, moved } => Ok(self.pre_layout(windows, moved)),
        };
        let _ = reply.send(resp.unwrap_or_else(Response::err));
    }

    fn world(&mut self) -> Result<Vec<Display>, String> {
        let mons = self.aero.monitors().map_err(fmt_err)?;
        world::join(&mons, &(self.screens)())
    }

    /// V3: a candidate config is evaluated against the live view without being loaded.
    fn candidate(&self, path: Option<PathBuf>) -> Result<Config, String> {
        match path {
            None => Ok(self.config.clone()),
            Some(p) => config::load(&p).map(|l| l.config).map_err(fmt_err),
        }
    }

    fn focused_ws(&mut self) -> Result<String, String> {
        let rows = self.aero.workspaces().map_err(fmt_err)?;
        world::focused(&rows).map(|r| r.workspace.clone()).ok_or_else(|| "no focused workspace".to_string())
    }

    /// `live` | `inert` | `observe` (I.cli.status).
    fn tiling_state(&self) -> &'static str {
        match (&self.sub, self.observe) {
            (None, _) => "inert",
            (Some(_), true) => "observe",
            (Some(_), false) => "live",
        }
    }

    fn explain(&mut self, path: Option<PathBuf>) -> Result<Response, String> {
        let cfg = self.candidate(path)?;
        let ds = self.world()?;
        let r = rules::resolve(&cfg, &ds, self.persisted.lock.as_deref());
        let mut text = render::explain(&r);
        let mut j = json!(r);
        if !r.exclusive_tiling.is_empty() || !self.persisted.exclusive_tiling.records.is_empty() {
            let wins = self.aero.window_details().ok(); // one read, answered between passes
            let views = tiling::view(&r.exclusive_tiling, &self.persisted.exclusive_tiling, wins.as_deref());
            let state = self.tiling_state();
            let note = match state {
                "inert" => Some(format!("inert: {}", self.inert.as_deref().unwrap_or("not subscribed to window events"))),
                "observe" => Some("observe".to_string()),
                _ => None,
            };
            text.push_str(&render::exclusive_tiling(&views, note.as_deref()));
            j["exclusive_tiling"] = json!({ "state": state, "inert": self.inert, "workspaces": views });
        }
        Ok(Response::ok(text, j))
    }

    fn dry_run(&mut self, path: Option<PathBuf>) -> Result<Response, String> {
        let cfg = self.candidate(path)?;
        let ds = self.world()?;
        let s = self.aero.windows().map_err(fmt_err)?;
        let r = rules::resolve(&cfg, &ds, self.persisted.lock.as_deref());
        let plans = self.converge_plans(&r.exclusive_tiling);
        let cmds = pass::dry_run(&r, &s, &ds, &self.persisted.pinned_names(), &plans);
        let lines: Vec<String> = cmds.iter().map(|c| c.to_string()).collect();
        let converge: Vec<Value> = reasons(&plans).into_iter().map(|(ws, why)| json!({ "workspace": ws, "reason": why })).collect();
        let text = format!("{}{}", render::dry_run(&r, &cmds, &reasons(&plans)), render::gaps(&r));
        Ok(Response::ok(text, json!({ "placement": r.placement, "converge": converge, "commands": lines, "gaps": r.gaps })))
    }

    fn monitors(&mut self) -> Result<Response, String> {
        let ds = self.world()?;
        let views: Vec<MonitorView> = ds
            .into_iter()
            .map(|d| MonitorView {
                labels: self.config.monitor.iter().filter(|l| rules::matches(&l.matches, &d, &self.config)).map(|l| l.label.clone()).collect(),
                portrait: d.portrait(),
                external: d.external(),
                pixel_area: d.pixel_area(),
                display: d,
            })
            .collect();
        Ok(Response::ok(render::monitors(&views), json!(views)))
    }

    fn status(&mut self) -> Result<Response, String> {
        let (profile, profile_mode) = match self.world() {
            Ok(ds) => {
                let r = rules::resolve(&self.config, &ds, self.persisted.lock.as_deref());
                (r.profile, r.profile_mode)
            }
            Err(_) => (None, ProfileMode::None),
        };
        let view = StatusView {
            profile,
            profile_mode,
            lock: self.persisted.lock.clone(),
            last_pass: self.last_pass.clone(),
            pins: self.pin_views(),
            warnings: self.warnings.list(),
            aerospace_running: self.aero_up,
            observe: self.observe,
            exclusive_tiling: self.config.has_exclusive_tiling().then(|| TilingStatus {
                state: self.tiling_state().into(),
                inert: self.sub.is_none().then(|| self.inert.clone().unwrap_or_else(|| "not subscribed to window events".into())),
                rule_floated: self.persisted.exclusive_tiling.counts(),
            }),
        };
        let mut resp = Response::ok(render::status(&view), json!(view));
        if !self.warnings.is_empty() {
            resp.exit = 4; // D25 V7
        }
        Ok(resp)
    }

    fn pin_views(&mut self) -> Vec<PinView> {
        let here: Vec<(String, u32)> = match (self.aero.windows(), self.world()) {
            (Ok(s), Ok(ds)) => self
                .persisted
                .pins
                .iter()
                .filter_map(|p| {
                    let m = s.monitor_of(&p.workspace)?;
                    let d = ds.iter().find(|d| d.aero_id == m)?;
                    Some((p.workspace.clone(), d.screen.cg_id))
                })
                .collect(),
            _ => vec![],
        };
        self.persisted
            .pins
            .iter()
            .map(|p| PinView {
                workspace: p.workspace.clone(),
                monitor: p.monitor_name.clone(),
                suspended: here.iter().any(|(w, cg)| *w == p.workspace && *cg != p.cg_id),
            })
            .collect()
    }

    fn pin(&mut self, ws: Option<String>) -> Result<Response, String> {
        let ws = match ws {
            Some(w) => w,
            None => self.focused_ws()?,
        };
        let snap = self.aero.windows().map_err(fmt_err)?;
        let Some(m) = snap.monitor_of(&ws) else {
            return Err(format!("workspace {ws} is empty; there is no layout to protect"));
        };
        let d = self.world()?.into_iter().find(|d| d.aero_id == m).ok_or("the workspace's monitor vanished")?;
        self.persisted.pin(Pin { workspace: ws.clone(), cg_id: d.screen.cg_id, monitor_name: d.aero_name.clone() });
        self.save_state();
        Ok(Response::ok(format!("pinned {ws} on {}\n", d.aero_name), Value::Null))
    }

    fn unpin(&mut self, ws: Option<String>, all: bool) -> Result<Response, String> {
        let names: Vec<String> = if all {
            self.persisted.pins.iter().map(|p| p.workspace.clone()).collect()
        } else {
            vec![match ws {
                Some(w) => w,
                None => self.focused_ws()?,
            }]
        };
        let removed: Vec<String> = names.into_iter().filter(|w| self.persisted.unpin(w)).collect();
        if removed.is_empty() {
            return Err("nothing to unpin".into());
        }
        self.save_state();
        self.arm(Trigger::Unpin, true, None); // D17: unpin is a trigger
        Ok(Response::ok(format!("unpinned {}\n", removed.join(" ")), Value::Null))
    }

    fn pins(&mut self) -> Response {
        let views = self.pin_views();
        Response::ok(render::pins(&views), json!(views))
    }

    fn profile(&mut self) -> Result<Response, String> {
        let ds = self.world()?;
        let r = rules::resolve(&self.config, &ds, self.persisted.lock.as_deref());
        let text = match (&r.profile, &r.profile_mode) {
            (Some(n), ProfileMode::Locked) => format!("{n} (locked)\n"),
            (Some(n), _) => format!("{n} (auto)\n"),
            (None, _) => "none (no profile matches)\n".into(),
        };
        Ok(Response::ok(text, json!({ "profile": r.profile, "mode": r.profile_mode, "profiles": r.profiles })))
    }

    fn lock(&mut self, name: String) -> Result<Response, String> {
        let ds = self.world()?;
        let p = self.config.profile.iter().find(|p| p.name == name).ok_or_else(|| format!("no profile named {name:?}"))?;
        if let Some(why) = rules::set_mismatch(&p.when, &ds, &self.config) {
            return Err(format!("profile {name:?} does not match the attached monitors: {why}"));
        }
        self.persisted.lock = Some(name.clone());
        self.save_state();
        self.arm(Trigger::Lock, true, None); // P-9
        Ok(Response::ok(format!("profile locked to {name}\n"), Value::Null))
    }

    fn unlock(&mut self) -> Response {
        self.persisted.lock = None;
        self.save_state();
        self.arm(Trigger::Lock, true, None);
        Response::ok("profile unlocked (auto)\n", Value::Null)
    }

    fn reload(&mut self) -> Result<Response, String> {
        self.config_text = None; // explicit: always re-parse
        self.load_config()?;
        self.ensure_subscription();
        self.arm(Trigger::Config, true, None);
        Ok(Response::ok("config reloaded\n", Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aero::{Aero, Cmd};
    use crate::config::Axis;
    use crate::template::Tree;
    use crate::testutil::{FakeAero, desk3};
    use std::sync::mpsc;

    type TestWorker = Worker<FakeAero, Box<dyn FnMut() -> Vec<Screen>>>;

    const EXAMPLE: &str = include_str!("../aerospace-responsive.example.toml");

    fn docked() -> FakeAero {
        FakeAero::new(
            &desk3(),
            &[
                ("1", 2, false, 2, Axis::H),
                ("2", 3, true, 1, Axis::H),
                ("3", 2, false, 1, Axis::H),
                ("C", 2, false, 0, Axis::H),
                ("T", 2, true, 1, Axis::H),
                ("A", 1, true, 1, Axis::H),
            ],
            "T",
        )
    }

    #[test]
    fn changed_menu_bar_height_reconciles_and_unchanged_height_stays_quiet() {
        use std::cell::Cell;
        use std::rc::Rc;
        let displays = vec![crate::testutil::builtin(1)];
        let (mut w, dir) = worker_cfg(
            FakeAero::new(&displays, &[("1", 1, true, 1, Axis::H)], "1"),
            "[gaps]\ntop-default = 42\nadd-menu-bar-height-when-single-monitor = true\n",
            None,
        );
        let height = Rc::new(Cell::new(33.0));
        let read_height = height.clone();
        w.screens = Box::new(move || {
            let mut s = crate::testutil::builtin(1).screen;
            s.menu_bar_height = read_height.get();
            vec![s]
        });
        let aero_cfg = dir.path().join("aerospace.toml");
        std::fs::write(&aero_cfg, "[gaps]\nouter.top = 10\n").unwrap();
        let p = aero_cfg.clone();
        w.aero_config = Box::new(move || Ok(p.clone()));
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert_eq!(std::fs::read_to_string(&aero_cfg).unwrap(), "[gaps]\nouter.top = 43\n");
        let count = w.aero.log.len();
        w.menu_bar_poll = Instant::now();
        w.tick(Instant::now());
        assert!(w.pending.is_none());
        assert_eq!(w.aero.log.len(), count);
        height.set(40.0);
        w.menu_bar_poll = Instant::now();
        w.tick(Instant::now());
        assert!(w.pending.is_some());
        w.tick(later());
        assert_eq!(std::fs::read_to_string(&aero_cfg).unwrap(), "[gaps]\nouter.top = 50\n");
    }

    #[test]
    fn the_gap_step_compensates_the_notch_once_then_says_nothing() {
        let (mut w, dir) = worker_cfg(
            FakeAero::new(&desk3(), &[("1", 2, true, 1, Axis::H)], "1"),
            "[gaps]\ntop-default = 1\n[[gaps.top]]\nmonitor = { builtin = true }\nvalue = 52\n",
            None,
        );
        let aero_cfg = dir.path().join("aerospace.toml");
        std::fs::write(&aero_cfg, "[gaps]\ninner.horizontal = 8   # tight\nouter.top = [{ monitor.main = 42 }, 1]\n").unwrap();
        let p = aero_cfg.clone();
        w.aero_config = Box::new(move || Ok(p.clone()));
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        // desk3 puts the built-in at monitor 2 with a 32 pt notch: 52 - 32 = 20 there, 1 elsewhere.
        let text = std::fs::read_to_string(&aero_cfg).unwrap();
        assert!(text.contains("outer.top = [{ monitor.2 = 20 }, 1]"), "{text}");
        assert!(text.contains("inner.horizontal = 8   # tight"), "the rest of the file is untouched: {text}");
        assert!(w.aero.log.iter().any(|c| *c == Cmd::ReloadConfig), "AeroSpace must be told to reread");
        // Steady state: a second pass writes nothing and never reloads.
        let n = w.aero.log.len();
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert_eq!(std::fs::read_to_string(&aero_cfg).unwrap(), text);
        assert!(!w.aero.log.iter().skip(n).any(|c| *c == Cmd::ReloadConfig), "no change, no reload");
    }

    #[test]
    fn the_gap_step_takes_the_line_over_and_says_who_else_writes_it() {
        let (mut w, dir) = worker_cfg(
            FakeAero::new(&desk3(), &[("1", 2, true, 1, Axis::H)], "1"),
            "[gaps]\ntop-default = 1\n[[gaps.top]]\nmonitor = { builtin = true }\nvalue = 52\n",
            None,
        );
        let aero_cfg = dir.path().join("aerospace.toml");
        std::fs::write(&aero_cfg, "[gaps]\nouter.top = 42\n").unwrap();
        let p = aero_cfg.clone();
        w.aero_config = Box::new(move || Ok(p.clone()));
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert_eq!(w.persisted.gap_written.as_ref().map(|v| v.to_toml()), Some("[{ monitor.2 = 20 }, 1]".to_string()));
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        // A hand edit (or a gap script) now shows up instead of being silently overwritten.
        std::fs::write(&aero_cfg, "[gaps]\nouter.top = 42\n").unwrap();
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        let r = ask(&mut w, Request::Status);
        assert!(serde_json::to_string(&r.json).unwrap().contains("outer.top in"), "{r:?}");
        // and it is taken back, so the next pass starts from our value again.
        assert_eq!(std::fs::read_to_string(&aero_cfg).unwrap(), "[gaps]\nouter.top = [{ monitor.2 = 20 }, 1]\n");
    }

    #[test]
    fn no_gaps_block_means_outer_top_is_never_touched() {
        let (mut w, dir) = worker_cfg(FakeAero::new(&desk3(), &[("1", 2, true, 1, Axis::H)], "1"), "", None);
        let aero_cfg = dir.path().join("aerospace.toml");
        std::fs::write(&aero_cfg, "[gaps]\nouter.top = 42\n").unwrap();
        let p = aero_cfg.clone();
        w.aero_config = Box::new(move || Ok(p.clone()));
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert_eq!(std::fs::read_to_string(&aero_cfg).unwrap(), "[gaps]\nouter.top = 42\n");
        assert!(!w.aero.log.iter().any(|c| *c == Cmd::ReloadConfig));
        assert!(w.persisted.gap_written.is_none());
    }

    #[test]
    fn observe_mode_shows_the_gap_line_without_writing_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg.toml");
        std::fs::write(&cfg, format!("{EXAMPLE}[gaps]\ntop-default = 1\n")) .unwrap();
        let aero_cfg = dir.path().join("aerospace.toml");
        std::fs::write(&aero_cfg, "[gaps]\nouter.top = 42\n").unwrap();
        let p = aero_cfg.clone();
        let mut w = Worker::new(FakeAero::new(&desk3(), &[("1", 2, true, 1, Axis::H)], "1"), all_screens(), cfg, dir.path().join("state.json"), true, true);
        w.set_sender(mpsc::channel().0);
        w.aero_config = Box::new(move || Ok(p.clone()));
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert_eq!(std::fs::read_to_string(&aero_cfg).unwrap(), "[gaps]\nouter.top = 42\n", "observe writes nothing");
    }

    fn all_screens() -> Box<dyn FnMut() -> Vec<Screen>> {
        Box::new(|| desk3().into_iter().map(|d| d.screen).collect())
    }

    fn worker_with(fake: FakeAero, screens: Box<dyn FnMut() -> Vec<Screen>>, aero_up: bool) -> (TestWorker, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg.toml");
        std::fs::write(&cfg, EXAMPLE).unwrap();
        let w = Worker::new(fake, screens, cfg, dir.path().join("state.json"), aero_up, false);
        (w, dir)
    }

    fn worker(fake: FakeAero) -> (TestWorker, tempfile::TempDir) {
        worker_with(fake, all_screens(), true)
    }

    /// kitty-on-T: one kitty may tile on T.
    const RULE: &str = "\n[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [{ app = \"net.kovidgoyal.kitty\", max-tiled = 1 }]\n";

    /// A worker on the example config plus `extra`, wired to a channel like the daemon's,
    /// optionally over a pre-existing `state.json`.
    fn worker_cfg(fake: FakeAero, extra: &str, state: Option<&Persisted>) -> (TestWorker, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg.toml");
        std::fs::write(&cfg, format!("{EXAMPLE}{extra}")).unwrap();
        if let Some(p) = state {
            p.save(&dir.path().join("state.json")).unwrap();
        }
        let mut w = Worker::new(fake, all_screens(), cfg, dir.path().join("state.json"), true, false);
        w.set_sender(mpsc::channel().0);
        (w, dir)
    }

    fn set_config(w: &mut TestWorker, d: &tempfile::TempDir, extra: &str) {
        std::fs::write(d.path().join("cfg.toml"), format!("{EXAMPLE}{extra}")).unwrap();
        w.handle(Msg::Trigger(Trigger::Config));
    }

    fn exit(w: &mut TestWorker, code: i32) {
        let child = w.sub_gen;
        w.handle(Msg::SubscribeExit { child, code: Some(code), stderr: "ERROR: Unknown event 'window-moved'".into() });
    }

    #[test]
    fn subscribe_child_runs_only_while_a_rule_is_configured() {
        let (mut w, d) = worker_cfg(docked(), "", None);
        w.handle(Msg::Trigger(Trigger::Startup));
        assert!(w.aero.subs.is_empty(), "no rule, no child");
        set_config(&mut w, &d, RULE);
        assert_eq!(w.aero.live_subs(), 1);
        set_config(&mut w, &d, RULE.replace("max-tiled = 1", "max-tiled = 2").as_str());
        assert_eq!((w.aero.subs.len(), w.aero.live_subs()), (1, 1), "an edit keeps the running child");
        set_config(&mut w, &d, "");
        assert_eq!(w.aero.live_subs(), 0, "the last rule removed kills the child");
    }

    #[test]
    fn child_exit_is_inert_with_three_bounded_retries_then_triggers_only() {
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        assert_eq!(w.aero.subs.len(), 1);
        exit(&mut w, 2);
        let st = ask(&mut w, Request::Status);
        assert_eq!(st.exit, 4, "inert is a pending warning");
        assert!(st.text.contains("aerospace subscribe exited 2 (ERROR: Unknown event 'window-moved')"), "{}", st.text);
        assert!(st.text.contains(INERT_HINT), "{}", st.text);
        for n in 2..=4 {
            assert!(w.sub_retry_at.is_some());
            w.tick(later());
            assert_eq!(w.aero.subs.len(), n, "retry {}", n - 1);
            exit(&mut w, 2);
        }
        assert!(w.sub_retry_at.is_none(), "1s, 5s, 30s, then no open-ended timer");
        w.tick(later());
        assert_eq!(w.aero.subs.len(), 4);
        w.handle(Msg::AeroSocket(true));
        assert_eq!(w.aero.live_subs(), 1, "an existing trigger respawns");
        assert!(w.warnings.list().iter().all(|x| x.key != "exclusive-tiling"), "a running child clears the warning");
        let stale = w.sub_gen - 1;
        w.handle(Msg::SubscribeExit { child: stale, code: Some(1), stderr: String::new() });
        assert_eq!(w.aero.live_subs(), 1, "a stale child's exit is ignored");
    }

    #[test]
    fn a_child_that_cannot_spawn_is_inert_too() {
        let mut fake = docked();
        fake.fail_subscribe = true;
        let (mut w, _d) = worker_cfg(fake, RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        assert!(ask(&mut w, Request::Status).text.contains("aerospace subscribe could not start"));
    }

    const KITTY: &str = "net.kovidgoyal.kitty";
    /// Rules on T and C, both kitty-only with one slot.
    const RULE_TC: &str = "\n[[exclusive-tiling]]\nworkspaces = [\"T\", \"C\"]\ntiling-allowlist = [{ app = \"net.kovidgoyal.kitty\", max-tiled = 1 }]\n";

    fn moved(id: u32, ws: &str, prev: Option<&str>, app: &str) -> Msg {
        Msg::Window(WindowEvent::WindowMoved { id, workspace: ws.into(), prev_workspace: prev.map(Into::into), app: app.into() })
    }

    /// A live worker (child running) on `extra`, the fake as given.
    fn live(fake: FakeAero, extra: &str) -> (TestWorker, tempfile::TempDir) {
        let (mut w, d) = worker_cfg(fake, extra, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.pending = None; // the startup pass is not under test here
        (w, d)
    }

    fn floated(w: &TestWorker, ws: &str) -> Vec<u32> {
        w.persisted.exclusive_tiling.rule_floated.get(ws).map_or(vec![], |s| s.iter().map(|f| f.window_id).collect())
    }

    #[test]
    fn arrivals_float_what_the_rule_does_not_admit() {
        let (mut w, d) = live(docked(), RULE);
        // One at a time, as windows open: the limit counts what is tiled at each arrival (V3).
        for (id, app, floating) in [(500, "com.apple.Safari", false), (501, KITTY, false), (502, KITTY, false), (503, "com.apple.Preview", true)] {
            w.aero.add_window("T", id, app, "", floating);
            w.handle(moved(id, "T", None, app));
        }
        assert_eq!(w.aero.log, [Cmd::Float { id: 500 }, Cmd::Float { id: 502 }], "Safari not allowlisted; second kitty over the limit");
        assert_eq!(floated(&w, "T"), [500, 502]);
        assert_eq!(Persisted::load(&d.path().join("state.json")).exclusive_tiling, w.persisted.exclusive_tiling, "memory saved");
        assert!(w.pending.is_none() && !w.abort.load(Ordering::SeqCst), "events never arm a pass or abort one (V12)");
    }

    #[test]
    fn pre_layout_answers_without_calling_aerospace_and_the_event_does_not_repeat_it() {
        let (mut w, d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.aero.log.clear();
        w.aero.add_window("T", 700, "com.apple.Safari", "GitHub", false);
        let wins = w.aero.window_details().unwrap();
        let r = ask(&mut w, Request::PreLayout { windows: wins, moved: vec![700] });
        assert_eq!(r.exit, 0, "{}", r.text);
        assert_eq!(r.json, serde_json::json!([{ "kind": "float", "id": 700 }]));
        assert!(w.aero.log.is_empty(), "pre-layout must not call aerospace: {:?}", w.aero.log);
        assert!(floated(&w, "T").contains(&700), "{:?}", floated(&w, "T"));
        assert_eq!(Persisted::load(&d.path().join("state.json")).exclusive_tiling, w.persisted.exclusive_tiling);
        // The fork applies the float, then broadcasts. The event sees a floating window and stops.
        w.aero.set_floating(700, true);
        w.handle(moved(700, "T", None, "com.apple.Safari"));
        assert!(w.aero.log.is_empty(), "already judged: {:?}", w.aero.log);
    }

    #[test]
    fn a_late_float_answer_is_floated_by_the_event() {
        // AeroSpace stopped waiting and drew the arrival tiled; the event must still float it.
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.aero.log.clear();
        w.aero.add_window("T", 700, "com.apple.Safari", "GitHub", false);
        let wins = w.aero.window_details().unwrap();
        ask(&mut w, Request::PreLayout { windows: wins, moved: vec![700] });
        w.handle(moved(700, "T", None, "com.apple.Safari"));
        assert_eq!(w.aero.log, [Cmd::Float { id: 700 }]);
        assert!(floated(&w, "T").contains(&700));
    }

    #[test]
    fn a_float_that_moved_on_before_its_events_is_retiled() {
        // Applied on T, then the window went to 1 (no rule) before either event was handled.
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.aero.log.clear();
        w.aero.add_window("T", 700, "com.apple.Safari", "GitHub", false);
        let wins = w.aero.window_details().unwrap();
        ask(&mut w, Request::PreLayout { windows: wins, moved: vec![700] });
        w.aero.set_floating(700, true);
        w.aero.move_window(700, "1");
        w.handle(moved(700, "T", None, "com.apple.Safari"));
        w.handle(moved(700, "1", Some("T"), "com.apple.Safari"));
        assert_eq!(w.aero.log, [Cmd::Tile { id: 700 }]);
        assert!(w.persisted.exclusive_tiling.floated_on(700, "com.apple.Safari").is_none());
    }

    #[test]
    fn no_answers_while_the_subscription_is_down() {
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        exit(&mut w, 2);
        w.aero.add_window("T", 700, "com.apple.Safari", "GitHub", false);
        let wins = w.aero.window_details().unwrap();
        let r = ask(&mut w, Request::PreLayout { windows: wins, moved: vec![700] });
        assert_eq!(r.json, serde_json::json!([]));
    }

    #[test]
    fn a_late_retile_answer_keeps_the_record_so_the_event_retiles() {
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.aero.add_window("T", 700, "com.apple.Safari", "GitHub", false);
        w.handle(moved(700, "T", None, "com.apple.Safari")); // floated by the rule, recorded
        assert!(floated(&w, "T").contains(&700));
        w.aero.log.clear();
        w.aero.move_window(700, "1");
        let wins = w.aero.window_details().unwrap();
        let r = ask(&mut w, Request::PreLayout { windows: wins, moved: vec![700] });
        assert_eq!(r.json, serde_json::json!([{ "kind": "tile", "id": 700 }]));
        // Not applied: the window is still floating when its event arrives.
        w.handle(moved(700, "1", Some("T"), "com.apple.Safari"));
        assert_eq!(w.aero.log, [Cmd::Tile { id: 700 }]);
        assert!(w.persisted.exclusive_tiling.floated_on(700, "com.apple.Safari").is_none());
    }

    #[test]
    fn a_landed_retile_is_settled_by_the_event_without_a_second_command() {
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.aero.add_window("T", 700, "com.apple.Safari", "GitHub", false);
        w.handle(moved(700, "T", None, "com.apple.Safari"));
        w.aero.log.clear();
        w.aero.move_window(700, "1");
        let wins = w.aero.window_details().unwrap();
        ask(&mut w, Request::PreLayout { windows: wins, moved: vec![700] });
        assert!(w.persisted.exclusive_tiling.floated_on(700, "com.apple.Safari").is_some(), "the record waits for the event");
        w.aero.set_floating(700, false); // the fork applied `layout tiling`
        w.handle(moved(700, "1", Some("T"), "com.apple.Safari"));
        assert!(w.aero.log.is_empty(), "{:?}", w.aero.log);
        assert!(w.persisted.exclusive_tiling.floated_on(700, "com.apple.Safari").is_none());
    }

    #[test]
    fn a_late_float_answer_admitted_after_all_leaves_no_record() {
        // Slot taken: pre-layout floats 301, AeroSpace stopped waiting and drew it tiled. The
        // incumbent closes before 301's event, so the event admits it: the rule must not own it.
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.aero.add_window("T", 300, KITTY, "incumbent", false);
        w.handle(moved(300, "T", None, KITTY));
        w.aero.add_window("T", 301, KITTY, "late", false);
        let wins = w.aero.window_details().unwrap();
        let r = ask(&mut w, Request::PreLayout { windows: wins, moved: vec![301] });
        assert_eq!(r.json, serde_json::json!([{ "kind": "float", "id": 301 }]));
        w.aero.close_window(300);
        w.handle(Msg::Window(WindowEvent::WindowClosed { id: 300, workspace: "T".into(), app: KITTY.into() }));
        w.aero.log.clear();
        w.handle(moved(301, "T", None, KITTY));
        assert!(w.aero.log.is_empty(), "admitted: {:?}", w.aero.log);
        assert!(w.persisted.exclusive_tiling.floated_on(301, KITTY).is_none(), "no false ownership");
        // You float it by hand and move it: the daemon leaves it alone.
        w.aero.set_floating(301, true);
        w.aero.move_window(301, "1");
        w.handle(moved(301, "1", Some("T"), KITTY));
        assert!(w.aero.log.is_empty(), "{:?}", w.aero.log);
    }

    #[test]
    fn a_pre_layout_request_after_its_event_is_stale() {
        // AeroSpace stopped waiting; the event for 301 was handled first and admitted it.
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.aero.add_window("T", 300, KITTY, "incumbent", false);
        w.aero.add_window("T", 301, KITTY, "late", false);
        let stale = w.aero.window_details().unwrap(); // census taken while 300 was still tiled
        w.aero.set_floating(300, true); // you float the incumbent: the slot is free
        w.handle(moved(300, "T", None, KITTY));
        w.handle(moved(301, "T", None, KITTY));
        w.aero.log.clear();
        let r = ask(&mut w, Request::PreLayout { windows: stale, moved: vec![301] });
        assert_eq!(r.json, serde_json::json!([]), "already judged by its event");
        assert!(w.persisted.exclusive_tiling.floated_on(301, KITTY).is_none(), "no false ownership");
    }

    #[test]
    fn a_census_app_without_a_bundle_id_matches_its_events() {
        let (mut w, _d) = worker_cfg(docked(), RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.aero.add_window("T", 700, "NULL-APP-BUNDLE-ID", "", false);
        let mut wins = w.aero.window_details().unwrap();
        wins.iter_mut().filter(|x| x.id == 700).for_each(|x| x.app = String::new()); // as the fork sends it
        let r = ask(&mut w, Request::PreLayout { windows: wins, moved: vec![700] });
        assert_eq!(r.json, serde_json::json!([{ "kind": "float", "id": 700 }]));
        assert!(w.persisted.exclusive_tiling.floated_on(700, "NULL-APP-BUNDLE-ID").is_some());
    }

    #[test]
    fn in_a_burst_the_first_arrival_keeps_the_slot() {
        // B3: both kitties land on T before the daemon handles the first event. The first to
        // arrive keeps the one slot, even with the higher id (V22).
        let (mut w, _d) = live(docked(), RULE);
        w.aero.add_window("1", 520, KITTY, "first", false);
        w.aero.add_window("1", 510, KITTY, "second", false);
        w.handle(moved(520, "1", None, KITTY));
        w.handle(moved(510, "1", None, KITTY));
        w.aero.move_window(520, "T");
        w.aero.move_window(510, "T");
        w.handle(moved(520, "T", Some("1"), KITTY));
        w.handle(moved(510, "T", Some("1"), KITTY));
        assert_eq!(w.aero.log, [Cmd::Float { id: 510 }]);
        assert_eq!(floated(&w, "T"), [510]);
    }

    #[test]
    fn a_workspace_without_a_rule_is_left_alone() {
        let mut fake = docked();
        fake.add_window("1", 500, "com.apple.Safari", "", false);
        let (mut w, _d) = live(fake, RULE);
        w.handle(moved(500, "1", Some("2"), "com.apple.Safari"));
        assert!(w.aero.log.is_empty());
    }

    #[test]
    fn leave_retiles_unless_the_destination_would_float_it_too() {
        let mut fake = docked();
        fake.add_window("1", 600, "com.apple.Safari", "", true);
        fake.add_window("C", 601, "com.apple.Preview", "", true);
        fake.add_window("C", 602, KITTY, "", true);
        fake.add_window("C", 603, KITTY, "vim", false);
        let (mut w, _d) = live(fake, RULE_TC);
        for (id, app) in [(600, "com.apple.Safari"), (601, "com.apple.Preview"), (602, KITTY)] {
            w.persisted.exclusive_tiling.insert("T", id, app);
        }
        w.handle(moved(600, "1", Some("T"), "com.apple.Safari"));
        w.handle(moved(601, "C", Some("T"), "com.apple.Preview"));
        w.handle(moved(602, "C", Some("T"), KITTY));
        assert_eq!(w.aero.log, [Cmd::Tile { id: 600 }], "no rule on 1: re-tile");
        assert_eq!(floated(&w, "C"), [601, 602], "C floats Preview (not allowlisted) and kitty (603 holds the slot)");
        assert!(floated(&w, "T").is_empty());
    }

    #[test]
    fn a_rule_floated_window_you_tiled_is_forgotten_then_judged_as_an_arrival() {
        let mut fake = docked();
        fake.add_window("C", 700, "com.apple.Safari", "", false); // you tiled it on T, then moved it
        let (mut w, _d) = live(fake, RULE_TC);
        w.persisted.exclusive_tiling.insert("T", 700, "com.apple.Safari");
        w.handle(moved(700, "C", Some("T"), "com.apple.Safari"));
        assert_eq!(w.aero.log, [Cmd::Float { id: 700 }], "V6 drop, then V18 arrival on C");
        assert_eq!(floated(&w, "C"), [700]);
    }

    #[test]
    fn close_prunes_memory_eagerly() {
        let mut fake = docked();
        fake.add_window("T", 800, "com.apple.Safari", "", false);
        let (mut w, d) = live(fake, RULE);
        w.handle(moved(800, "T", None, "com.apple.Safari"));
        assert_eq!(floated(&w, "T"), [800]);
        w.aero.close_window(800);
        w.handle(Msg::Window(WindowEvent::WindowClosed { id: 800, workspace: "T".into(), app: "com.apple.Safari".into() }));
        assert!(w.persisted.exclusive_tiling.rule_floated.is_empty());
        assert!(Persisted::load(&d.path().join("state.json")).exclusive_tiling.rule_floated.is_empty());
    }

    #[test]
    fn the_record_survives_a_restart_so_a_plain_restart_converges_nothing() {
        let mut fake = docked();
        fake.add_window("T", 400, KITTY, "", false);
        let (mut w, d) = live(fake, RULE);
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert!(w.aero.log.contains(&Cmd::Float { id: 105 }), "first run floats T's non-kitty window");
        let saved = Persisted::load(&d.path().join("state.json"));
        assert!(saved.exclusive_tiling.records.contains_key("T"));
        // You tile it back by hand; the daemon restarts with the same rule.
        let mut fake = std::mem::replace(&mut w.aero, docked());
        fake.run(&Cmd::Tile { id: 105 }).unwrap();
        fake.log.clear();
        let mut w2 = Worker::new(fake, all_screens(), d.path().join("cfg.toml"), d.path().join("state.json"), true, false);
        w2.set_sender(mpsc::channel().0);
        w2.handle(Msg::Trigger(Trigger::Startup));
        w2.tick(later());
        assert!(!w2.aero.log.iter().any(|c| matches!(c, Cmd::Float { .. } | Cmd::Tile { .. })), "equal record: your tile survives");
    }

    #[test]
    fn removing_the_last_rule_kills_the_child_and_still_retiles_its_floats() {
        let mut fake = docked();
        fake.add_window("T", 400, KITTY, "", false);
        let (mut w, d) = live(fake, RULE);
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert!(w.aero.win(105).unwrap().floating, "first run floated T's non-kitty window");
        set_config(&mut w, &d, "");
        assert_eq!(w.aero.live_subs(), 0);
        w.tick(later());
        assert!(!w.aero.win(105).unwrap().floating, "V20: the removal converge ran without the child");
        let saved = Persisted::load(&d.path().join("state.json"));
        assert_eq!(saved.exclusive_tiling, TilingMemory::default(), "record and memory dropped");
        assert!(!ask(&mut w, Request::Status).text.contains("exclusive tiling"), "no rule, no status line");
    }

    #[test]
    fn an_inert_rule_does_not_converge() {
        let mut fake = docked();
        fake.add_window("T", 400, KITTY, "", false);
        let (mut w, _d) = worker_cfg(fake, RULE, None);
        w.handle(Msg::Trigger(Trigger::Startup));
        exit(&mut w, 2);
        w.sub_retry_at = None;
        w.tick(later());
        assert!(!w.aero.log.iter().any(|c| matches!(c, Cmd::Float { .. })));
        assert!(w.persisted.exclusive_tiling.records.is_empty(), "the first live pass converges later");
    }

    #[test]
    fn observe_logs_the_rule_and_keeps_its_memory_in_process() {
        let mut fake = docked();
        fake.add_window("T", 400, KITTY, "", false);
        let mut st = Persisted::default();
        st.exclusive_tiling.insert("X", 1, "on-disk");
        let (mut w, d) = worker_cfg(fake, RULE, Some(&st));
        w.observe = true;
        w.handle(Msg::Trigger(Trigger::Startup));
        assert_eq!(w.aero.live_subs(), 1, "observe subscribes");
        w.tick(later());
        assert!(w.persisted.exclusive_tiling.records.contains_key("T"), "the converge counts as done in-process");
        w.aero.add_window("T", 401, "com.apple.Safari", "", false);
        w.handle(moved(401, "T", None, "com.apple.Safari"));
        assert!(w.persisted.exclusive_tiling.floated_on(401, "com.apple.Safari").is_some());
        assert!(!w.aero.log.iter().any(|c| matches!(c, Cmd::Float { .. } | Cmd::Tile { .. })), "nothing executed");
        // Anything else that saves state (a pin) must not leak the in-process memory.
        assert_eq!(ask(&mut w, Request::Pin { workspace: Some("T".into()) }).exit, 0);
        let disk = Persisted::load(&d.path().join("state.json"));
        assert_eq!(disk.exclusive_tiling, st.exclusive_tiling);
        assert_eq!(disk.pins.len(), 1);
    }

    #[test]
    fn surfaces_show_the_rule_live_and_inert() {
        let mut fake = docked();
        fake.add_window("T", 400, KITTY, "noomz: ~", false);
        let (mut w, _d) = live(fake, RULE);
        let dry = ask(&mut w, Request::Reconcile { dry_run: true, config: None });
        assert!(dry.text.contains("exclusive-tiling converge:\n  T    first run\n"), "{}", dry.text);
        assert!(dry.text.contains("aerospace layout floating --window-id 105"), "{}", dry.text);
        assert_eq!(dry.json["converge"][0]["reason"], "first run");
        assert!(w.aero.log.is_empty(), "dry-run executes nothing");

        let ex = ask(&mut w, Request::Explain { config: None });
        assert!(ex.text.contains("exclusive-tiling:\n  T "), "{}", ex.text);
        assert!(ex.text.contains("converge: pending: first run"), "{}", ex.text);
        assert!(ex.text.contains("tiled by you, not allowlisted"), "{}", ex.text);
        assert_eq!(ex.json["exclusive_tiling"]["state"], "live");
        assert_eq!(ex.json["exclusive_tiling"]["workspaces"][0]["workspace"], "T");

        let st = ask(&mut w, Request::Status);
        assert!(st.text.contains("exclusive tiling: live; rule-floated none"), "{}", st.text);
        assert_eq!(st.json["exclusive_tiling"]["state"], "live");

        exit(&mut w, 2);
        let ex = ask(&mut w, Request::Explain { config: None });
        assert!(ex.text.contains("exclusive-tiling: (inert: aerospace subscribe exited 2)"), "{}", ex.text);
        assert!(ask(&mut w, Request::Status).text.contains("exclusive tiling: inert;"));
        let dry = ask(&mut w, Request::Reconcile { dry_run: true, config: None });
        assert!(!dry.text.contains("--window-id"), "an inert rule converges nothing: {}", dry.text);
    }

    #[test]
    fn reconnect_prunes_memory_and_applies_missed_leaves_but_no_arrivals() {
        let mut fake = docked();
        fake.add_window("1", 900, "com.apple.Safari", "", true); // left T while disconnected
        fake.add_window("T", 902, "com.apple.Preview", "", true); // still rule-floated on T
        fake.add_window("T", 903, "com.apple.Finder", "", false); // moved in meanwhile: stays tiled
        let mut st = Persisted::default();
        st.exclusive_tiling.insert("T", 900, "com.apple.Safari");
        st.exclusive_tiling.insert("T", 901, "com.apple.TextEdit"); // closed meanwhile
        st.exclusive_tiling.insert("T", 902, "com.apple.Preview");
        let (mut w, d) = worker_cfg(fake, RULE, Some(&st));
        w.handle(Msg::AeroSocket(true));
        assert_eq!(w.aero.log, [Cmd::Tile { id: 900 }], "only the missed leave acts");
        let saved = Persisted::load(&d.path().join("state.json"));
        let ids: Vec<u32> = saved.exclusive_tiling.rule_floated["T"].iter().map(|f| f.window_id).collect();
        assert_eq!(ids, [902]);
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    fn ask(w: &mut TestWorker, req: Request) -> Response {
        let (tx, rx) = mpsc::channel();
        w.handle(Msg::Request(req, tx));
        rx.try_recv().expect("synchronous reply")
    }

    #[test]
    fn trailing_debounce_pushes_the_deadline() {
        let (mut w, _d) = worker(docked());
        w.handle(Msg::Trigger(Trigger::Display));
        let first = w.pending.as_ref().unwrap().due;
        std::thread::sleep(Duration::from_millis(5));
        w.handle(Msg::Trigger(Trigger::Display));
        assert!(w.pending.as_ref().unwrap().due > first);
        assert!(w.aero.log.is_empty(), "nothing runs before the deadline");
    }

    #[test]
    fn display_change_runs_one_pass_and_a_repeat_is_a_no_op() {
        let (mut w, _d) = worker(docked());
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert!(w.aero.log.iter().any(|c| matches!(c, Cmd::Move { ws, .. } if ws == "1")));
        assert!(w.pending.is_none());
        let n = w.aero.log.len();
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        let mutating = w.aero.log[n..].iter().filter(|c| !matches!(c, Cmd::Focus { .. })).count();
        assert_eq!(mutating, 0);
    }

    #[test]
    fn mismatch_gets_one_repass_then_warns() {
        let mut fake = docked();
        fake.ignore_layout = true;
        let (mut w, _d) = worker(fake);
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert!(w.pending.as_ref().is_some_and(|p| p.repass));
        assert!(w.warnings.is_empty());
        w.tick(later());
        assert!(w.pending.is_none());
        assert!(w.warnings.list().iter().any(|x| x.key == "mismatch"));
    }

    #[test]
    fn a_window_registered_after_the_pass_gets_its_workspace_placed() {
        // At wake, list-windows can come back partial: workspace 1's windows are not
        // registered yet, so the pass cannot see it and skips it.
        let mut fake = docked();
        let late = std::mem::take(&mut fake.ws.iter_mut().find(|w| w.name == "1").unwrap().windows);
        let (mut w, _d) = worker(fake);
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert!(!w.aero.log.iter().any(|c| matches!(c, Cmd::Move { ws, .. } if ws == "1")));
        w.aero.ws.iter_mut().find(|w| w.name == "1").unwrap().windows = late;
        w.handle(Msg::Window(WindowEvent::WindowDetected {}));
        assert!(w.pending.as_ref().is_some_and(|p| !p.full), "a detected window arms a verify, not a pass");
        w.tick(later());
        w.tick(later());
        assert!(w.aero.log.iter().any(|c| matches!(c, Cmd::Move { ws, .. } if ws == "1")));
        assert!(w.pending.is_none());
    }

    #[test]
    fn poke_while_aerospace_is_down_replies_with_an_error() {
        let (mut w, _d) = worker_with(docked(), all_screens(), false);
        let (tx, rx) = mpsc::channel();
        w.handle(Msg::Request(Request::Reconcile { dry_run: false, config: None }, tx));
        w.tick(later());
        assert_eq!(rx.try_recv().unwrap().exit, 1);
        assert!(w.aero.log.is_empty());
    }

    #[test]
    fn gate_disagreement_retries_then_warns() {
        let two: Box<dyn FnMut() -> Vec<Screen>> = Box::new(|| desk3().into_iter().take(2).map(|d| d.screen).collect());
        let (mut w, _d) = worker_with(docked(), two, true);
        w.handle(Msg::Trigger(Trigger::Display));
        for _ in 0..GATE_TRIES {
            w.tick(later());
        }
        assert!(w.warnings.list().iter().any(|x| x.key == "gate"));
        assert!(w.aero.log.is_empty());
    }

    #[test]
    fn unlock_verifies_and_re_passes_instead_of_acting_itself() {
        // D23: unlock runs the post-pass diff and, on a mismatch, ONE budgeted re-pass —
        // it never issues commands of its own. Drift the model so a full pass would act.
        let (mut w, _d) = worker(docked());
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        let n = w.aero.log.len();
        let i = w.aero.ws.iter().position(|x| x.name == "1").unwrap();
        w.aero.ws[i].root = Axis::H; // something re-laid 1 while the screen was locked

        w.handle(Msg::Trigger(Trigger::Unlock));
        w.tick(later());
        assert_eq!(w.aero.log.len(), n, "the verify pass itself issues nothing");
        let pending = w.pending.as_ref().expect("a mismatch arms the budgeted re-pass");
        assert!(pending.full && pending.repass);

        w.tick(later()); // the re-pass is a normal full pass and fixes the drift
        assert!(w.aero.log[n..].iter().any(|c| matches!(c, Cmd::Layout { ws, axis } if ws == "1" && *axis == Axis::V)));
    }

    #[test]
    fn a_persistently_empty_read_retries_on_a_schedule_then_gives_up() {
        // Ticket 12: an empty list-windows is a retry, not a state to act on — on a bounded
        // schedule, or a genuinely empty model would re-arm forever.
        let mut fake = docked();
        fake.ws.iter_mut().for_each(|w| w.windows.clear());
        let (mut w, _d) = worker(fake);
        w.handle(Msg::Trigger(Trigger::Display));
        for _ in EMPTY_RETRY {
            w.tick(later());
            assert!(w.pending.is_some(), "an empty read retries");
        }
        w.tick(later());
        assert!(w.pending.is_none(), "the last gives up instead of re-arming");
        assert!(w.aero.log.is_empty(), "an empty model is never acted on");
    }

    #[test]
    fn startup_before_any_window_exists_places_once_windows_appear() {
        // Login: the daemon's first passes read an empty list-windows while apps still open.
        let mut fake = docked();
        fake.ws.iter_mut().for_each(|w| w.windows.clear());
        let (mut w, _d) = worker(fake);
        w.handle(Msg::Trigger(Trigger::Startup));
        w.tick(later());
        w.tick(later());
        assert!(w.aero.log.is_empty() && w.pending.is_some(), "still retrying after two empty reads");
        w.aero.ws = docked().ws; // the apps are up
        w.tick(later());
        assert!(!w.aero.log.is_empty(), "the retry places workspaces");
    }

    #[test]
    fn a_failed_pass_warns_and_retries_once() {
        let (mut w, _d) = worker(docked());
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        w.restore = None;
        w.aero.fail_windows = true; // AeroSpace blips mid-pass
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert!(w.warnings.list().iter().any(|x| x.key == "pass-failed"));
        assert!(w.pending.as_ref().is_some_and(|p| p.repass), "one budgeted retry");
        assert!(w.restore.is_none(), "a stale visibility baseline must not survive");
    }

    #[test]
    fn pin_refuses_an_empty_workspace_and_protects_layout() {
        let (mut w, _d) = worker(docked());
        assert_eq!(ask(&mut w, Request::Pin { workspace: Some("C".into()) }).exit, 1);
        assert_eq!(ask(&mut w, Request::Pin { workspace: Some("A".into()) }).exit, 0);
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert!(!w.aero.log.iter().any(|c| matches!(c, Cmd::Layout { ws, .. } if ws == "A")));
        assert_eq!(ask(&mut w, Request::Pins).text.lines().count(), 1);
    }

    #[test]
    fn bad_config_keeps_the_old_one_and_warns() {
        let (mut w, d) = worker(docked());
        std::fs::write(d.path().join("cfg.toml"), "[[layout]\n").unwrap();
        w.handle(Msg::Trigger(Trigger::Config));
        assert!(w.warnings.list().iter().any(|x| x.key == "parse"));
        assert_eq!(w.config.layout.len(), 2);
        assert_eq!(ask(&mut w, Request::Status).exit, 4);
    }

    #[test]
    fn backstop_arms_a_pass_after_aerospace_appears() {
        let (mut w, _d) = worker_with(docked(), all_screens(), false);
        w.handle(Msg::AeroSocket(true));
        assert!(w.backstop.is_some());
        w.tick(later());
        assert!(!w.aero.log.is_empty());
    }

    #[test]
    fn restore_does_not_switch_other_monitors_after_a_clean_pass() {
        let (mut w, _d) = worker(docked());
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        let focus: Vec<&Cmd> = w.aero.log.iter().filter(|c| matches!(c, Cmd::Focus { .. })).collect();
        assert!(focus.is_empty(), "already-correct focus needs no workspace command");
        assert_eq!(w.aero.focused, "T");
        assert_eq!(crate::world::visible_on(&w.aero.clone_rows(), 1), Some("1"));
        assert_eq!(crate::world::visible_on(&w.aero.clone_rows(), 3), Some("3"));
    }

    #[test]
    fn dry_run_and_explain_do_not_mutate() {
        let (mut w, _d) = worker(docked());
        let r = ask(&mut w, Request::Reconcile { dry_run: true, config: None });
        assert!(r.text.contains("move-workspace-to-monitor --workspace 1"));
        assert_eq!(ask(&mut w, Request::Explain { config: None }).exit, 0);
        assert!(w.aero.log.is_empty());
    }

    // ---------- workspace template: window events (T37) ----------

    /// `C` on the built-in: `h[line, v[igcl, msgr]]`.
    const TPL: &str = "\n[[workspace-template]]\nworkspaces = [\"C\"]\nwhen = { builtin = true }\nroot = \"h\"\ntree = [{ app = \"line\" }, [{ app = \"igcl\" }, { app = \"msgr\" }]]\n";

    /// A worker with LINE 44 and IGCL 10 tiled on `C` (built-in), gated against desk3.
    fn tpl_worker(extra: &str) -> (TestWorker, tempfile::TempDir) {
        let mut f = docked();
        f.add_window("C", 44, "line", "LINE", false);
        f.add_window("C", 10, "igcl", "IGCL", false);
        let (mut w, d) = live(f, &format!("{TPL}{extra}"));
        w.last_ds = Some(desk3());
        (w, d)
    }

    fn flattens(log: &[Cmd]) -> usize {
        log.iter().filter(|c| matches!(c, Cmd::Flatten { ws } if ws == "C")).count()
    }

    #[test]
    fn a_burst_of_arrivals_rebuilds_once_after_the_queue_is_judged() {
        let (mut w, d) = tpl_worker("");
        for id in [49, 50] {
            w.aero.add_window("C", id, "msgr", "Messenger", false);
            w.handle(moved(id, "C", None, "msgr"));
        }
        assert!(w.aero.log.is_empty(), "no rebuild before the queue is drained: {:?}", w.aero.log);
        w.queue_drained();
        assert_eq!(flattens(&w.aero.log), 1, "{:?}", w.aero.log);
        assert_eq!(
            w.aero.tree("C"),
            Tree::Split(Axis::H, vec![Tree::Win(44), Tree::Split(Axis::V, vec![Tree::Win(10), Tree::Win(49), Tree::Win(50)])])
        );
        let rec = &Persisted::load(&d.path().join("state.json")).template_records["C"];
        assert_eq!(rec.slots["2.2"], [49, 50], "record saved after the last command");
        assert!(w.pending.is_none(), "V47: never arms a pass");
        w.aero.log.clear();
        w.queue_drained();
        assert!(w.aero.log.is_empty(), "nothing queued, nothing rebuilt");
    }

    #[test]
    fn arrivals_elsewhere_or_on_a_pinned_template_workspace_send_nothing() {
        let (mut w, _d) = tpl_worker("");
        w.aero.add_window("T", 77, KITTY, "", false);
        w.handle(moved(77, "T", None, KITTY));
        w.queue_drained();
        assert!(w.aero.log.is_empty(), "T has no template: {:?}", w.aero.log);
        w.persisted.pin(Pin { workspace: "C".into(), cg_id: 0, monitor_name: String::new() });
        w.aero.add_window("C", 49, "msgr", "Messenger", false);
        w.handle(moved(49, "C", None, "msgr"));
        w.queue_drained();
        assert!(w.aero.log.is_empty(), "V46: pinned: {:?}", w.aero.log);
    }

    #[test]
    fn an_arrival_the_rule_floats_triggers_no_rebuild_and_drops_the_record() {
        let rule = "\n[[exclusive-tiling]]\nworkspaces = [\"C\"]\ntiling-allowlist = [{ app = \"line\", max-tiled = 1 }, { app = \"igcl\", max-tiled = 1 }]\n";
        let (mut w, d) = tpl_worker(rule);
        w.handle(moved(44, "C", None, "line"));
        w.queue_drained();
        assert!(w.persisted.template_records.contains_key("C"), "first rebuild leaves a record");
        w.aero.log.clear();
        w.aero.add_window("C", 49, "msgr", "Messenger", false);
        w.handle(moved(49, "C", None, "msgr"));
        w.queue_drained();
        assert_eq!(w.aero.log, [Cmd::Float { id: 49 }], "floated, then not tiled: no rebuild (V40 b)");
        assert!(!w.persisted.template_records.contains_key("C"), "V52: the daemon's float drops the record");
        assert!(!Persisted::load(&d.path().join("state.json")).template_records.contains_key("C"), "and saves");
    }

    #[test]
    fn a_closed_window_leaves_the_record_without_commands() {
        let (mut w, d) = tpl_worker("");
        w.aero.add_window("C", 49, "msgr", "Messenger", false);
        w.handle(moved(49, "C", None, "msgr"));
        w.queue_drained();
        w.aero.log.clear();
        w.aero.close_window(49);
        w.handle(Msg::Window(WindowEvent::WindowClosed { id: 49, workspace: "C".into(), app: "msgr".into() }));
        w.queue_drained();
        assert!(w.aero.log.is_empty(), "{:?}", w.aero.log);
        let rec = &Persisted::load(&d.path().join("state.json")).template_records["C"];
        assert!(rec.slots["2.2"].is_empty() && rec.slots["1"] == [44], "{rec:?}");
        // The next pass compares equal: the record follows the close (V41).
        let wins = w.aero.window_details().unwrap();
        let t = &rules::resolve(&w.config, &desk3(), None).templates[0].template;
        assert_eq!(template::pending(Some(rec), &template::wanted(t, &wins.into_iter().filter(|x| x.workspace == "C").collect::<Vec<_>>())), None);
    }

    #[test]
    fn a_retile_on_leave_drops_the_record_and_the_arrival_rebuilds() {
        let (mut w, _d) = tpl_worker(RULE);
        w.handle(moved(44, "C", None, "line"));
        w.queue_drained();
        assert!(w.persisted.template_records.contains_key("C"));
        // Kitty 77 was floated by T's rule; the user moves it to C, where no rule holds it.
        w.aero.add_window("T", 77, KITTY, "", true);
        w.persisted.exclusive_tiling.insert("T", 77, KITTY);
        w.aero.move_window(77, "C");
        w.aero.log.clear();
        w.handle(moved(77, "C", Some("T"), KITTY));
        assert_eq!(w.aero.log, [Cmd::Tile { id: 77 }]);
        assert!(!w.persisted.template_records.contains_key("C"), "V52: the daemon's tile drops the record");
        w.queue_drained();
        assert_eq!(flattens(&w.aero.log), 1, "now tiled on C: the arrival rebuilds");
        assert_eq!(w.persisted.template_records["C"].rest, [77]);
    }

    #[test]
    fn a_drain_rebuild_stopped_halfway_arms_one_repass() {
        let (mut w, _d) = tpl_worker("");
        w.aero.add_window("C", 49, "msgr", "Messenger", false);
        w.handle(moved(49, "C", None, "msgr"));
        w.abort.store(true, Ordering::SeqCst); // a display change mid-rebuild; a failed command takes the same branch
        w.queue_drained();
        assert!(!w.persisted.template_records.contains_key("C"), "no record after a partial rebuild (V41)");
        let p = w.pending.as_ref().expect("B8: a re-pass is armed");
        assert!(p.full && p.repass && p.trigger == Trigger::Repass);
    }

    /// Sends `inject` into the worker's inbox when the first `C` flatten runs: events that
    /// arrive during a rebuild.
    struct Inject {
        f: FakeAero,
        inject: Option<(Sender<Msg>, Vec<Msg>)>,
        log: std::sync::Arc<std::sync::Mutex<Vec<Cmd>>>,
    }

    impl Aero for Inject {
        fn monitors(&mut self) -> anyhow::Result<Vec<crate::world::AeroMonitor>> {
            self.f.monitors()
        }
        fn windows(&mut self) -> anyhow::Result<crate::world::Snapshot> {
            self.f.windows()
        }
        fn workspaces(&mut self) -> anyhow::Result<Vec<crate::world::WorkspaceRow>> {
            self.f.workspaces()
        }
        fn window_details(&mut self) -> anyhow::Result<Vec<WinInfo>> {
            self.f.window_details()
        }
        fn run(&mut self, cmd: &Cmd) -> Result<(), String> {
            self.log.lock().unwrap().push(cmd.clone());
            if matches!(cmd, Cmd::Flatten { ws } if ws == "C")
                && let Some((tx, msgs)) = self.inject.take()
            {
                for id in [50, 51] {
                    self.f.add_window("C", id, "msgr", "Messenger", false);
                }
                for m in msgs {
                    tx.send(m).unwrap();
                }
            }
            self.f.run(cmd)
        }
        fn subscribe(&mut self, sink: crate::aero::Sink) -> anyhow::Result<crate::aero::Subscription> {
            self.f.subscribe(sink)
        }
    }

    #[test]
    fn events_during_a_rebuild_are_judged_together_and_rebuild_once_more() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg.toml");
        std::fs::write(&cfg, format!("{EXAMPLE}{TPL}")).unwrap();
        let mut f = docked();
        f.add_window("C", 44, "line", "LINE", false);
        f.add_window("C", 10, "igcl", "IGCL", false);
        f.add_window("C", 49, "msgr", "Messenger", false);
        let (tx, rx) = mpsc::channel();
        let log = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let inject = Inject { f, inject: Some((tx.clone(), vec![moved(50, "C", None, "msgr"), moved(51, "C", None, "msgr")])), log: log.clone() };
        let mut w = Worker::new(inject, all_screens(), cfg, dir.path().join("state.json"), true, false);
        w.last_ds = Some(desk3());
        tx.send(moved(49, "C", None, "msgr")).unwrap();
        drop(tx);
        w.run(rx); // returns once the inbox is drained and every sender is gone
        assert_eq!(flattens(&log.lock().unwrap()), 2, "one rebuild, then one for both events queued during it: {:?}", log.lock().unwrap());
        let rec = &Persisted::load(&dir.path().join("state.json")).template_records["C"];
        assert_eq!(rec.slots["2.2"], [49, 50, 51]);
    }
}
