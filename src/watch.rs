//! kqueue watcher thread (D11: event-driven, no polling).
//! - config file + its directory → `Trigger::Config` (atomic-save editors replace the file;
//!   the worker ignores events whose text is unchanged)
//! - AeroSpace's socket directory → `AeroSocket(bool)` on existence edges (D24 P5), and a
//!   replaced socket as gone-then-back (V55)

use crate::worker::{Msg, Trigger};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

fn open_evt(path: &Path) -> Option<RawFd> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY) };
    (fd >= 0).then_some(fd)
}

fn watch(kq: RawFd, fd: RawFd, fflags: u32) {
    let ev = libc::kevent {
        ident: fd as usize,
        filter: libc::EVFILT_VNODE,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
}

/// The socket's identity: a server that unlinks and binds it between two wakeups gives a new inode.
type SocketId = Option<(u64, u64)>;

fn socket_id(path: &Path) -> SocketId {
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// V55: the `AeroSocket` edges between two reads. Another identity means the old server went away
/// and a new one came, even when both reads see a socket.
fn socket_edges(before: SocketId, now: SocketId) -> &'static [bool] {
    match (before, now) {
        (None, Some(_)) => &[true],
        (Some(_), None) => &[false],
        (Some(a), Some(b)) if a != b => &[false, true],
        _ => &[],
    }
}

const FILE_FLAGS: u32 = libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_EXTEND | libc::NOTE_ATTRIB;

pub fn spawn(tx: Sender<Msg>, config: PathBuf, aero_socket: PathBuf) -> std::io::Result<()> {
    let config_dir = config.parent().expect("config path has a directory").to_path_buf();
    std::fs::create_dir_all(&config_dir)?;
    let sock_dir = aero_socket.parent().expect("socket path has a directory").to_path_buf();
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let close = |fds: &[RawFd]| {
        fds.iter().for_each(|fd| {
            unsafe { libc::close(*fd) };
        })
    };
    let Some(dir_fd) = open_evt(&config_dir) else {
        close(&[kq]);
        return Err(std::io::Error::last_os_error());
    };
    let Some(sock_dir_fd) = open_evt(&sock_dir) else {
        close(&[kq, dir_fd]);
        return Err(std::io::Error::last_os_error());
    };
    watch(kq, dir_fd, libc::NOTE_WRITE);
    watch(kq, sock_dir_fd, libc::NOTE_WRITE);
    let mut file_fd = open_evt(&config).unwrap_or(-1);
    if file_fd >= 0 {
        watch(kq, file_fd, FILE_FLAGS);
    }
    let mut sock = socket_id(&aero_socket);
    let _ = tx.send(Msg::AeroSocket(sock.is_some()));

    std::thread::spawn(move || {
        loop {
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) };
            if n <= 0 {
                continue; // EINTR
            }
            let fd = ev.ident as RawFd;
            if fd == sock_dir_fd {
                let now = socket_id(&aero_socket);
                for &up in socket_edges(sock, now) {
                    if tx.send(Msg::AeroSocket(up)).is_err() {
                        return;
                    }
                }
                sock = now;
                continue;
            }
            if fd == file_fd && ev.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0 {
                unsafe { libc::close(file_fd) };
                file_fd = -1;
            }
            if file_fd < 0
                && let Some(f) = open_evt(&config)
            {
                file_fd = f;
                watch(kq, file_fd, FILE_FLAGS);
            }
            if tx.send(Msg::Trigger(Trigger::Config)).is_err() {
                return;
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    fn socket_msgs(rx: &Receiver<Msg>) -> Vec<bool> {
        let mut out = vec![];
        while let Ok(m) = rx.recv_timeout(Duration::from_millis(300)) {
            if let Msg::AeroSocket(up) = m {
                out.push(up);
            }
        }
        out
    }

    #[test]
    fn v55_a_socket_replaced_in_one_step_is_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("aero.sock");
        std::fs::write(&sock, "").unwrap();
        let (tx, rx) = channel();
        spawn(tx, dir.path().join("cfg/config.toml"), sock.clone()).unwrap();
        assert_eq!(socket_msgs(&rx), [true], "first report: already up");
        // A new server's socket lands over the old one: it exists at every instant.
        let next = dir.path().join("aero.sock.new");
        std::fs::write(&next, "").unwrap();
        std::fs::rename(&next, &sock).unwrap();
        assert_eq!(socket_msgs(&rx), [false, true], "V55: a new socket identity = gone, then back");
    }

    #[test]
    fn v55_edges() {
        let (a, b) = (Some((1, 1)), Some((1, 2)));
        assert_eq!(socket_edges(None, a), [true]);
        assert_eq!(socket_edges(a, None), [false]);
        assert_eq!(socket_edges(a, b), [false, true]);
        assert!(socket_edges(a, a).is_empty());
        assert!(socket_edges(None, None).is_empty());
    }
}
