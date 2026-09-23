use aerospace_responsive::ipc::{self, CallError, Request};
use aerospace_responsive::{daemon, install, paths};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "aerospace-responsive", version, about = "Reshape AeroSpace workspaces when displays change")]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Run the daemon (the LaunchAgent's entry point)
    Daemon {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Log every pass as a dry run; execute nothing
        #[arg(long, hide = true)]
        observe: bool,
    },
    /// Run a reconcile pass now, or print it with --dry-run
    Reconcile {
        #[arg(long)]
        dry_run: bool,
        /// Evaluate this config instead of the loaded one (with --dry-run)
        #[arg(long, requires = "dry_run")]
        config: Option<PathBuf>,
        #[arg(long, requires = "dry_run")]
        json: bool,
    },
    /// Show every rule considered and why it won or lost
    Explain {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Show attached displays as predicates see them
    Monitors {
        #[arg(long)]
        json: bool,
    },
    /// Profile, last pass, pins, pending warnings (exit 4 while any are pending)
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Protect a workspace's layout (default: the focused one)
    Pin { workspace: Option<String> },
    /// Remove a pin (default: the focused workspace)
    Unpin {
        workspace: Option<String>,
        #[arg(long, conflicts_with = "workspace")]
        all: bool,
    },
    /// List pins
    Pins {
        #[arg(long)]
        json: bool,
    },
    /// Show the current profile, or lock / unlock one
    Profile {
        #[command(subcommand)]
        action: Option<ProfileAction>,
        #[arg(long)]
        json: bool,
    },
    /// Reload the config now
    Reload,
    /// Set up ~/.aerospace.toml, the LaunchAgent, and run a first pass
    Install(install::Opts),
    /// Undo what install did
    Uninstall(install::Opts),
}

#[derive(Subcommand)]
enum ProfileAction {
    Lock { name: String },
    Unlock,
}

fn main() -> ExitCode {
    let (req, json) = match Cli::parse().verb {
        Verb::Daemon { config, observe } => return finish(daemon::run(config.unwrap_or_else(paths::config_file), observe).map(|()| 0)),
        Verb::Install(o) => return finish(install::install(&o)),
        Verb::Uninstall(o) => return finish(install::uninstall(&o)),
        Verb::Reconcile { dry_run, config, json } => (Request::Reconcile { dry_run, config: absolute(config) }, json),
        Verb::Explain { config, json } => (Request::Explain { config: absolute(config) }, json),
        Verb::Monitors { json } => (Request::Monitors, json),
        Verb::Status { json } => (Request::Status, json),
        Verb::Pin { workspace } => (Request::Pin { workspace }, false),
        Verb::Unpin { workspace, all } => (Request::Unpin { workspace, all }, false),
        Verb::Pins { json } => (Request::Pins, json),
        Verb::Profile { action: None, json } => (Request::Profile, json),
        Verb::Profile { action: Some(ProfileAction::Lock { name }), .. } => (Request::ProfileLock { name }, false),
        Verb::Profile { action: Some(ProfileAction::Unlock), .. } => (Request::ProfileUnlock, false),
        Verb::Reload => (Request::Reload, false),
    };
    match ipc::call(&paths::daemon_socket(), &req) {
        Err(CallError::NoDaemon) => {
            eprintln!("daemon not running. Start it with: launchctl kickstart gui/$(id -u)/{}", paths::LABEL);
            ExitCode::from(3)
        }
        Err(CallError::Io(e)) => {
            eprintln!("error talking to the daemon: {e}");
            ExitCode::from(1)
        }
        Ok(r) => {
            if json && !r.json.is_null() {
                println!("{}", serde_json::to_string_pretty(&r.json).unwrap_or_default());
            } else if r.exit == 0 || r.exit == 4 {
                print!("{}", r.text);
            } else {
                eprint!("{}", if r.text.ends_with('\n') { r.text } else { r.text + "\n" });
            }
            ExitCode::from(r.exit as u8)
        }
    }
}

/// The daemon resolves paths itself, so send it an absolute one.
fn absolute(p: Option<PathBuf>) -> Option<PathBuf> {
    p.map(|p| std::path::absolute(&p).unwrap_or(p))
}

fn finish(r: anyhow::Result<i32>) -> ExitCode {
    match r {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
