//! A minimal inotify and eventfd wrapper (libc) for the watcher threads.

use std::ffi::{CString, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::time::Duration;

pub use libc::{
    IN_CLOSE_NOWRITE, IN_CLOSE_WRITE, IN_CREATE, IN_DELETE, IN_DELETE_SELF, IN_IGNORED,
    IN_MOVED_FROM, IN_MOVED_TO, IN_OPEN, IN_Q_OVERFLOW,
};

/// One inotify event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub wd: i32,
    pub mask: u32,
    /// The entry's name, for a watch on a directory.
    pub name: Option<OsString>,
}

/// A non-blocking inotify instance.
#[derive(Debug)]
pub struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    pub fn new() -> io::Result<Self> {
        // SAFETY: inotify_init1 has no preconditions; the result is checked.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor this instance now owns.
        Ok(Inotify {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Adds (or replaces) the watch on `path` for `mask`.
    pub fn add_watch(&self, path: &Path, mask: u32) -> io::Result<i32> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `c` is a valid NUL-terminated string for the call.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), mask) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(wd)
    }

    /// Removes a watch (errors ignored: the watch may already be gone).
    pub fn rm_watch(&self, wd: i32) {
        // SAFETY: plain syscall on our descriptor.
        unsafe {
            libc::inotify_rm_watch(self.fd.as_raw_fd(), wd);
        }
    }

    /// Every event queued now (none if there is none).
    pub fn read_events(&self) -> io::Result<Vec<Event>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(out),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(e),
                };
            }
            if n == 0 {
                return Ok(out);
            }
            parse_events(&buf[..n as usize], &mut out);
        }
    }
}

impl AsRawFd for Inotify {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Parses a buffer of `struct inotify_event` records.
fn parse_events(mut buf: &[u8], out: &mut Vec<Event>) {
    const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
    while buf.len() >= HEADER {
        let int = |at: usize| i32::from_ne_bytes(buf[at..at + 4].try_into().unwrap_or([0; 4]));
        let uint = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap_or([0; 4]));
        // struct inotify_event { int wd; uint32_t mask, cookie, len; char name[]; }
        let (wd, mask, len) = (int(0), uint(4), uint(12) as usize);
        let end = (HEADER + len).min(buf.len());
        let raw = &buf[HEADER..end];
        let name = raw.split(|&b| b == 0).next().filter(|n| !n.is_empty());
        out.push(Event {
            wd,
            mask,
            name: name.map(|n| OsString::from_vec(n.to_vec())),
        });
        buf = &buf[end..];
    }
}

/// An eventfd used to wake a thread blocked in [`poll2`].
#[derive(Debug)]
pub struct Waker {
    fd: OwnedFd,
}

impl Waker {
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd has no preconditions; the result is checked.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor this waker now owns.
        Ok(Waker {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writes 8 bytes from a valid u64.
        unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                (&one as *const u64).cast::<libc::c_void>(),
                8,
            );
        }
    }

    /// Clears pending wake-ups.
    pub fn drain(&self) {
        let mut v: u64 = 0;
        // SAFETY: reads 8 bytes into a valid u64.
        unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                (&mut v as *mut u64).cast::<libc::c_void>(),
                8,
            );
        }
    }
}

impl AsRawFd for Waker {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Waits until `a` or `b` is readable, or `timeout` (`None`: forever)
/// passes. `a` may be `None` (only `b` and the timeout). Returns which
/// were readable.
pub fn poll2(a: Option<RawFd>, b: RawFd, timeout: Option<Duration>) -> io::Result<(bool, bool)> {
    let mut fds = [
        libc::pollfd {
            fd: a.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: b,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let ms = timeout.map_or(-1, |t| {
        i32::try_from(t.as_millis().max(u128::from(!t.is_zero()))).unwrap_or(i32::MAX)
    });
    loop {
        // SAFETY: `fds` is a valid array of 2 pollfd; a negative fd is ignored.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let ready =
            |p: &libc::pollfd| p.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0;
        return Ok((a.is_some() && ready(&fds[0]), ready(&fds[1])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_closes_of_a_file_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("node");
        std::fs::write(&f, b"x").unwrap();
        let ino = Inotify::new().unwrap();
        let wd = ino
            .add_watch(&f, IN_OPEN | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE)
            .unwrap();
        assert!(ino.read_events().unwrap().is_empty());
        drop(std::fs::File::open(&f).unwrap());
        let (ready, _) = poll2(
            Some(ino.as_raw_fd()),
            Waker::new().unwrap().as_raw_fd(),
            Some(Duration::from_secs(2)),
        )
        .unwrap();
        assert!(ready);
        let ev = ino.read_events().unwrap();
        assert!(ev.iter().all(|e| e.wd == wd && e.name.is_none()), "{ev:?}");
        assert!(ev.iter().any(|e| e.mask & IN_OPEN != 0), "{ev:?}");
        assert!(ev.iter().any(|e| e.mask & IN_CLOSE_NOWRITE != 0), "{ev:?}");
    }

    #[test]
    fn a_directory_watch_names_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let ino = Inotify::new().unwrap();
        ino.add_watch(dir.path(), IN_CREATE | IN_DELETE).unwrap();
        std::fs::write(dir.path().join("video7"), b"").unwrap();
        std::fs::remove_file(dir.path().join("video7")).unwrap();
        let ev = ino.read_events().unwrap();
        let names: Vec<(bool, Option<&std::ffi::OsStr>)> = ev
            .iter()
            .map(|e| (e.mask & IN_CREATE != 0, e.name.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                (true, Some(std::ffi::OsStr::new("video7"))),
                (false, Some(std::ffi::OsStr::new("video7")))
            ]
        );
    }

    #[test]
    fn the_waker_wakes_a_poll_and_drains() {
        let w = Waker::new().unwrap();
        let (_, woke) = poll2(None, w.as_raw_fd(), Some(Duration::ZERO)).unwrap();
        assert!(!woke);
        w.wake();
        let (_, woke) = poll2(None, w.as_raw_fd(), Some(Duration::from_secs(1))).unwrap();
        assert!(woke);
        w.drain();
        let (_, woke) = poll2(None, w.as_raw_fd(), Some(Duration::ZERO)).unwrap();
        assert!(!woke);
    }
}
