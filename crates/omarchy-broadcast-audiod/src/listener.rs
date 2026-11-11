//! The worker's socket: the one systemd hands over (socket activation,
//! `LISTEN_FDS`/`LISTEN_PID`, descriptor 3), else one the worker binds
//! itself at [`obc_audio::default_socket_path`] when started directly.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::path::{Path, PathBuf};

use obc_audio::server::Listener;

/// The first descriptor systemd passes.
const SD_LISTEN_FDS_START: RawFd = 3;

/// The listening socket, and the path to remove at exit when the worker
/// bound it itself.
#[derive(Debug)]
pub struct Socket {
    pub listener: Listener,
    pub bound: Option<PathBuf>,
    /// Whether systemd handed it over.
    pub activated: bool,
}

impl Drop for Socket {
    fn drop(&mut self) {
        if let Some(p) = &self.bound {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The descriptor systemd passed, when `LISTEN_PID` is this process and
/// `LISTEN_FDS` is at least 1; it must be a socket.
fn activated_fd() -> Option<OwnedFd> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.trim().parse().ok()?;
    if pid != std::process::id() {
        return None;
    }
    let n: u32 = std::env::var("LISTEN_FDS").ok()?.trim().parse().ok()?;
    if n < 1 {
        return None;
    }
    // SAFETY: fstat on a descriptor number; a closed one fails.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(SD_LISTEN_FDS_START, &mut st) } != 0
        || st.st_mode & libc::S_IFMT != libc::S_IFSOCK
    {
        return None;
    }
    // SAFETY: systemd passed this descriptor to this process, which owns
    // it from now on; nothing else in the worker uses descriptor 3.
    unsafe {
        libc::fcntl(SD_LISTEN_FDS_START, libc::F_SETFD, libc::FD_CLOEXEC);
        Some(OwnedFd::from_raw_fd(SD_LISTEN_FDS_START))
    }
}

/// Whether a worker listens at `path`.
fn someone_listens(path: &Path) -> bool {
    obc_audio::sys::SeqPacket::connect_nonblocking(path).is_ok()
}

/// Binds `path`: its directory is created (mode 0700) and a stale socket
/// file (nobody listening) replaced. Another worker listening there is an
/// error.
pub fn bind(path: &Path) -> io::Result<Socket> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if someone_listens(path) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another audio worker listens at {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    Ok(Socket {
        listener: Listener::bind(path)?,
        bound: Some(path.to_path_buf()),
        activated: false,
    })
}

/// The socket handed over by systemd, else [`bind`] of `path`.
pub fn open(path: Option<&Path>) -> io::Result<Socket> {
    if let Some(fd) = activated_fd() {
        return Ok(Socket {
            listener: Listener::from_fd(fd),
            bound: None,
            activated: true,
        });
    }
    let path = path.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no socket was handed over and neither OMARCHY_BROADCAST_AUDIO_SOCKET nor \
             XDG_RUNTIME_DIR is set",
        )
    })?;
    bind(path)
}
