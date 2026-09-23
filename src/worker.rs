//! The one writer (D17, D24): a single thread that owns config, state and every mutation.
//! Triggers funnel through a trailing debounce; requests are answered in between passes.

use crate::aero::Aero;
use crate::config::{self, Config};
use crate::display::{Display, Screen};
use crate::ipc::{Request, Response};
use crate::pass::{self, PassReport};
use crate::render::{self, LastPass, MonitorView, PinView, StatusView};
use crate::restore::{self, RestoreCtx};
use crate::rules::{self, ProfileMode, Resolved};
use crate::state::{self, Class, Persisted, Pin, Warnings};
use crate::world;
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
}

pub enum Msg {
    Trigger(Trigger),
    AeroSocket(bool),
    Request(Request, Sender<Response>),
}

const BACKSTOP_DELAY: Duration = Duration::from_secs(10); // P-13
const GATE_TRIES: u32 = 3; // P-2

struct Pending {
    due: Instant,
    /// false = verify-only (unlock, D23).
    full: bool,
    /// This run is the one budgeted re-pass; a fresh trigger merged in resets it.
    repass: bool,
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
    aero_up: bool,
    restore: Option<RestoreCtx>,
    gate_failures: u32,
    last_pass: Option<LastPass>,
    observe: bool,
}

fn reply_all(waiters: Vec<Sender<Response>>, r: Response) {
    for w in waiters {
        let _ = w.send(r.clone());
    }
}

fn fmt_err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

impl<A: Aero, S: FnMut() -> Vec<Screen>> Worker<A, S> {
    pub fn new(aero: A, screens: S, config_path: PathBuf, state_path: PathBuf, aero_up: bool, observe: bool) -> Self {
        let persisted = Persisted::load(&state_path);
        let mut w = Worker {
            aero,
            screens,
            config_path,
            state_path,
            config: Config::default(),
            config_text: None,
            persisted,
            warnings: Warnings::default(),
            abort: Arc::new(AtomicBool::new(false)),
            debounce: Duration::from_millis(config::TRAILING_FLOOR_MS),
            pending: None,
            backstop: None,
            aero_up,
            restore: None,
            gate_failures: 0,
            last_pass: None,
            observe,
        };
        let _ = w.load_config();
        w
    }

    pub fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let due = [self.pending.as_ref().map(|p| p.due), self.backstop].into_iter().flatten().min();
            let msg = match due {
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
            };
            match msg {
                Ok(m) => self.handle(m),
                Err(RecvTimeoutError::Timeout) => self.tick(Instant::now()),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Trigger(Trigger::Config) => {
                if let Ok(true) = self.load_config() {
                    self.arm(Trigger::Config, true, None);
                }
            }
            Msg::Trigger(Trigger::Unlock) => self.arm(Trigger::Unlock, false, None),
            Msg::Trigger(t) => self.arm(t, true, None),
            Msg::AeroSocket(up) => self.aero_socket(up),
            Msg::Request(req, reply) => self.request(req, reply),
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if self.backstop.is_some_and(|b| b <= now) {
            self.backstop = None;
            self.arm(Trigger::Backstop, true, None);
        }
        if self.pending.as_ref().is_some_and(|p| p.due <= now) {
            self.fire();
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
                if full && !p.full {
                    p.trigger = trigger;
                }
                p.full |= full;
                p.waiters.extend(waiter);
            }
            None => self.pending = Some(Pending { due, full, repass, trigger, waiters: waiter.into_iter().collect() }),
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
        match pass::run_pass(&mut self.aero, &r, &ds, &pinned, &self.abort) {
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

    fn after_pass(&mut self, rep: PassReport, p: Pending) {
        for e in &rep.executed {
            match &e.error {
                None => crate::log!("  {}", e.cmd),
                Some(err) => crate::log!("  {}  -> FAILED: {err}", e.cmd),
            }
        }
        if rep.aborted || rep.empty_snapshot {
            let why = if rep.aborted { "aborted by a display change" } else { "list-windows came back empty" };
            if rep.empty_snapshot && p.repass {
                self.restore = None; // the disturbance is over; do not replay it later
                self.record(p.trigger, why.into());
                return reply_all(p.waiters, Response::err(why));
            }
            crate::log!("pass {why}; restarting through the debounce");
            // An empty read is a retry of the same run, and ticket 12 allows exactly one (the
            // second lands in the give-up return above). An abort restarts a full reconcile,
            // which ticket 03 says owns the next run — so it gets its budget back.
            let repass = rep.empty_snapshot;
            self.pending = Some(Pending { due: Instant::now() + self.debounce, repass, ..p });
            return;
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
            format!("ok, {} command(s)", rep.executed.len())
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
                    crate::log!("verify: model matches");
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
        let text = match self.aero.windows() {
            Ok(s) => {
                let cmds = pass::dry_run(r, &s, ds, pinned);
                crate::log!("observe: would run {} command(s)", cmds.len());
                for c in &cmds {
                    crate::log!("  would run: {c}");
                }
                render::dry_run(r, &cmds)
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
        if let Err(e) = self.persisted.save(&self.state_path) {
            crate::log!("cannot save state: {e:#}");
        }
    }

    // ---------- requests ----------

    fn request(&mut self, req: Request, reply: Sender<Response>) {
        let resp = match req {
            Request::Reconcile { dry_run: false, .. } => return self.arm(Trigger::Poke, true, Some(reply)),
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

    fn explain(&mut self, path: Option<PathBuf>) -> Result<Response, String> {
        let cfg = self.candidate(path)?;
        let ds = self.world()?;
        let r = rules::resolve(&cfg, &ds, self.persisted.lock.as_deref());
        Ok(Response::ok(render::explain(&r), json!(r)))
    }

    fn dry_run(&mut self, path: Option<PathBuf>) -> Result<Response, String> {
        let cfg = self.candidate(path)?;
        let ds = self.world()?;
        let s = self.aero.windows().map_err(fmt_err)?;
        let r = rules::resolve(&cfg, &ds, self.persisted.lock.as_deref());
        let cmds = pass::dry_run(&r, &s, &ds, &self.persisted.pinned_names());
        let lines: Vec<String> = cmds.iter().map(|c| c.to_string()).collect();
        Ok(Response::ok(render::dry_run(&r, &cmds), json!({ "placement": r.placement, "commands": lines })))
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
        self.arm(Trigger::Config, true, None);
        Ok(Response::ok("config reloaded\n", Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aero::Cmd;
    use crate::config::Axis;
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
    fn a_persistently_empty_read_retries_once_then_gives_up() {
        // Ticket 12: an empty list-windows right at wake is a retry, not a state to act on —
        // and exactly one retry, or a genuinely empty model would re-arm forever.
        let mut fake = docked();
        fake.ws.iter_mut().for_each(|w| w.windows.clear());
        let (mut w, _d) = worker(fake);
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        assert!(w.pending.as_ref().is_some_and(|p| p.repass), "the first empty read retries");
        w.tick(later());
        assert!(w.pending.is_none(), "the second gives up instead of re-arming");
        assert!(w.aero.log.is_empty(), "an empty model is never acted on");
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
    fn restore_runs_after_a_clean_pass() {
        let (mut w, _d) = worker(docked());
        w.handle(Msg::Trigger(Trigger::Display));
        w.tick(later());
        // before: portrait showed A, landscape showed 2, T focused on the built-in.
        let tail: Vec<&Cmd> = w.aero.log.iter().rev().take(3).collect();
        assert_eq!(tail, vec![&Cmd::Focus { ws: "T".into() }, &Cmd::Focus { ws: "2".into() }, &Cmd::Focus { ws: "A".into() }]);
        assert_eq!(w.aero.focused, "T");
    }

    #[test]
    fn dry_run_and_explain_do_not_mutate() {
        let (mut w, _d) = worker(docked());
        let r = ask(&mut w, Request::Reconcile { dry_run: true, config: None });
        assert!(r.text.contains("move-workspace-to-monitor --workspace 1"));
        assert_eq!(ask(&mut w, Request::Explain { config: None }).exit, 0);
        assert!(w.aero.log.is_empty());
    }
}
