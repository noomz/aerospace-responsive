//! kqueue watcher thread (D11: event-driven, no polling).
//! - config file + its directory → `Trigger::Config` (atomic-save editors replace the file;
//!   the worker ignores events whose text is unchanged)
//! - AeroSpace's socket directory → `AeroSocket(bool)` on existence edges (D24 P5)

use crate::worker::{Msg, Trigger};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
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
    let mut sock_up = aero_socket.exists();
    let _ = tx.send(Msg::AeroSocket(sock_up));

    std::thread::spawn(move || {
        loop {
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) };
            if n <= 0 {
                continue; // EINTR
            }
            let fd = ev.ident as RawFd;
            if fd == sock_dir_fd {
                let up = aero_socket.exists();
                if up != sock_up {
                    sock_up = up;
                    if tx.send(Msg::AeroSocket(up)).is_err() {
                        return;
                    }
                }
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
