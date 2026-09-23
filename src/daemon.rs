//! `aerospace-responsive daemon`: threads = main (AppKit loop, CG callback, unlock
//! notification), kqueue watcher, socket server, one serial worker (D24).

use crate::aero::CliAero;
use crate::worker::{Msg, Trigger, Worker};
use crate::{ipc, macos, paths, watch};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

pub fn run(config: PathBuf, observe: bool) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let aero_sock = paths::aerospace_socket();
    let worker = Worker::new(
        CliAero,
        macos::screens,
        config.clone(),
        paths::state_dir().join("state.json"),
        aero_sock.exists(),
        observe,
    );
    let abort = worker.abort.clone();

    let to_worker = tx.clone();
    ipc::serve(&paths::daemon_socket(), move |(req, reply)| {
        let _ = to_worker.send(Msg::Request(req, reply));
    })
    .context("cannot open the daemon socket")?;
    watch::spawn(tx.clone(), config, aero_sock).context("cannot start the file watcher")?;

    let on_display = tx.clone();
    macos::install_display_callback(move || {
        abort.store(true, Ordering::SeqCst); // re-entrancy: stop an in-flight pass (ticket 03)
        let _ = on_display.send(Msg::Trigger(Trigger::Display));
    });
    let on_unlock = tx.clone();
    macos::observe_unlock(move || {
        let _ = on_unlock.send(Msg::Trigger(Trigger::Unlock));
    });

    std::thread::spawn(move || worker.run(rx));
    let _ = tx.send(Msg::Trigger(Trigger::Startup));
    crate::log!("daemon started (pid {}){}", std::process::id(), if observe { " in observe mode" } else { "" });
    macos::run_app()
}
