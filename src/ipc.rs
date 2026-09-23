//! Client ↔ daemon: one JSON line each way over a UNIX socket the daemon owns (D24 P3).

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "kebab-case")]
pub enum Request {
    Reconcile { dry_run: bool, config: Option<PathBuf> },
    Explain { config: Option<PathBuf> },
    Monitors,
    Status,
    Pins,
    Profile,
    ProfileUnlock,
    Reload,
    Pin { workspace: Option<String> },
    Unpin { workspace: Option<String>, all: bool },
    ProfileLock { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub exit: i32,
    pub text: String,
    #[serde(default)]
    pub json: serde_json::Value,
}

impl Response {
    pub fn ok(text: impl Into<String>, json: serde_json::Value) -> Self {
        Response { exit: 0, text: text.into(), json }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Response { exit: 1, text: text.into(), json: serde_json::Value::Null }
    }
}

/// A request plus where its single response goes.
pub type Envelope = (Request, Sender<Response>);

/// Bind (refusing if another daemon answers on `path`) and accept on a background thread.
pub fn serve(path: &Path, deliver: impl Fn(Envelope) + Send + Sync + 'static) -> io::Result<()> {
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("another daemon is listening on {}", path.display())));
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let deliver = Arc::new(deliver);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let deliver = deliver.clone();
            std::thread::spawn(move || {
                if let Err(e) = handle(conn, &*deliver) {
                    crate::log!("client connection: {e}");
                }
            });
        }
    });
    Ok(())
}

fn handle(conn: UnixStream, deliver: &dyn Fn(Envelope)) -> io::Result<()> {
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line)?;
    let resp = match serde_json::from_str::<Request>(&line) {
        Err(e) => Response { exit: 2, text: format!("bad request: {e}"), json: serde_json::Value::Null },
        Ok(req) => {
            let (tx, rx) = mpsc::channel();
            deliver((req, tx));
            rx.recv().unwrap_or_else(|_| Response::err("daemon worker stopped"))
        }
    };
    let mut w = &conn;
    serde_json::to_writer(&mut w, &resp)?;
    w.write_all(b"\n")
}

#[derive(Debug)]
pub enum CallError {
    NoDaemon,
    Io(io::Error),
}

pub fn call(path: &Path, req: &Request) -> Result<Response, CallError> {
    let conn = UnixStream::connect(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => CallError::NoDaemon,
        _ => CallError::Io(e),
    })?;
    let mut w = &conn;
    serde_json::to_writer(&mut w, req).map_err(|e| CallError::Io(e.into()))?;
    w.write_all(b"\n").map_err(CallError::Io)?;
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line).map_err(CallError::Io)?;
    serde_json::from_str(&line).map_err(|e| CallError::Io(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        serve(&path, |(req, reply): Envelope| {
            let _ = reply.send(Response::ok(format!("{req:?}"), serde_json::Value::Null));
        })
        .unwrap();
        let resp = call(&path, &Request::Pin { workspace: Some("1".into()) }).unwrap();
        assert_eq!(resp.exit, 0);
        assert_eq!(resp.text, "Pin { workspace: Some(\"1\") }");
    }

    #[test]
    fn missing_socket_means_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(call(&dir.path().join("none.sock"), &Request::Status), Err(CallError::NoDaemon)));
    }

    #[test]
    fn second_server_on_a_live_socket_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        serve(&path, |_| {}).unwrap();
        assert!(serve(&path, |_| {}).is_err());
    }
}
