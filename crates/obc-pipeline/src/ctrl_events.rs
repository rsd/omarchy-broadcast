//! A watcher for V4L2 control events (`V4L2_EVENT_CTRL`).
//!
//! [`CtrlWatcher::start`] opens its own read-only file descriptor on a
//! node, subscribes it to the control events of the given controls
//! (`VIDIOC_SUBSCRIBE_EVENT`), and runs a thread that waits in `poll()` for
//! an event (`POLLPRI`) or for its stop signal (an eventfd), dequeues each
//! event (`VIDIOC_DQEVENT`) and hands it to a callback as a
//! [`CtrlNotice::Changed`]: the control, what changed (value, flags,
//! range) and its new value and flags. A driver sends an event when a
//! control is changed through another file descriptor, when the device
//! changes it by itself, and when another control's change flips its flags
//! (the inactive flag of a manual control whose automatic mode was turned
//! on or off). Changes made through the watcher's own descriptor would not
//! be reported, but it never writes.
//!
//! Dropping the watcher (or [`CtrlWatcher::stop`]) wakes the thread and
//! joins it at once; there is no timed polling. When the node goes away
//! (the camera is unplugged) or reports an error, the thread ends by itself
//! after one [`CtrlNotice::Ended`]. So it does when no subscription
//! succeeded: then it closes the descriptor at once and only sends that
//! [`CtrlNotice::Ended`]. A panic in the callback ends the thread too, and
//! is reported as a [`CtrlNotice::Ended`]. An open descriptor keeps a UVC
//! camera out of autosuspend, so a caller holds a watcher only while it has
//! the camera open anyway.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::controls::open_node;

/// `V4L2_EVENT_CTRL`
pub const EVENT_CTRL: u32 = 3;
/// `V4L2_EVENT_CTRL_CH_VALUE`: the control's value changed.
pub const CTRL_CHANGE_VALUE: u32 = 0x0001;
/// `V4L2_EVENT_CTRL_CH_FLAGS`: its flags changed (inactive, grabbed, ...).
pub const CTRL_CHANGE_FLAGS: u32 = 0x0002;
/// `V4L2_EVENT_CTRL_CH_RANGE`: its minimum, maximum, step or default
/// changed.
pub const CTRL_CHANGE_RANGE: u32 = 0x0004;

/// `struct v4l2_event_subscription`
#[repr(C)]
struct V4l2EventSubscription {
    type_: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

const _: () = assert!(std::mem::size_of::<V4l2EventSubscription>() == 32);

/// `struct v4l2_event`. The union `u` holds an `__s64` (in `struct
/// v4l2_event_ctrl`), so it is 8-aligned and starts at offset 8; with the
/// `struct timespec` the whole is 136 bytes on 64-bit.
#[repr(C)]
struct V4l2Event {
    type_: u32,
    u: [u64; 8],
    pending: u32,
    sequence: u32,
    timestamp: libc::timespec,
    id: u32,
    reserved: [u32; 8],
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<V4l2Event>() == 136);

/// `_IOW('V', 90, struct v4l2_event_subscription)`
const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = (1 << 30)
    | ((std::mem::size_of::<V4l2EventSubscription>() as libc::c_ulong) << 16)
    | ((b'V' as libc::c_ulong) << 8)
    | 90;
/// `_IOR('V', 89, struct v4l2_event)`
const VIDIOC_DQEVENT: libc::c_ulong = (2 << 30)
    | ((std::mem::size_of::<V4l2Event>() as libc::c_ulong) << 16)
    | ((b'V' as libc::c_ulong) << 8)
    | 89;

/// One control event (`struct v4l2_event_ctrl` with the event's id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtrlEvent {
    /// The control id.
    pub id: u32,
    /// What changed: [`CTRL_CHANGE_VALUE`], [`CTRL_CHANGE_FLAGS`],
    /// [`CTRL_CHANGE_RANGE`], or'ed.
    pub changes: u32,
    /// The control's `V4L2_CTRL_TYPE_*`.
    pub type_: u32,
    /// Its value now.
    pub value: i32,
    /// Its flags now (`V4L2_CTRL_FLAG_*`).
    pub flags: u32,
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub default_value: i32,
}

/// What the watcher hands its callback.
#[derive(Debug)]
pub enum CtrlNotice {
    /// A control changed.
    Changed(CtrlEvent),
    /// The watcher ended by itself, for this reason (no subscription
    /// succeeded, the node went away, reported an error or could not be
    /// waited on, or the callback panicked). Sent at most once, as the last
    /// notice, and never after a stop.
    Ended(io::Error),
}

/// The control event in a dequeued event, or `None` for another type.
fn ctrl_event(ev: &V4l2Event) -> Option<CtrlEvent> {
    if ev.type_ != EVENT_CTRL {
        return None;
    }
    let raw: Vec<u8> = ev.u.iter().flat_map(|w| w.to_ne_bytes()).collect();
    let word = |i: usize| [raw[i], raw[i + 1], raw[i + 2], raw[i + 3]];
    // struct v4l2_event_ctrl: changes, type, union { __s32 value; __s64
    // value64 } at 8, flags at 16, minimum, maximum, step, default_value.
    Some(CtrlEvent {
        id: ev.id,
        changes: u32::from_ne_bytes(word(0)),
        type_: u32::from_ne_bytes(word(4)),
        value: i32::from_ne_bytes(word(8)),
        flags: u32::from_ne_bytes(word(16)),
        minimum: i32::from_ne_bytes(word(20)),
        maximum: i32::from_ne_bytes(word(24)),
        step: i32::from_ne_bytes(word(28)),
        default_value: i32::from_ne_bytes(word(32)),
    })
}

fn subscribe(file: &fs::File, id: u32) -> io::Result<()> {
    let mut sub = V4l2EventSubscription {
        type_: EVENT_CTRL,
        id,
        flags: 0,
        reserved: [0; 5],
    };
    // SAFETY: `sub` is a valid `struct v4l2_event_subscription` that
    // outlives the call; the fd is open for the duration.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_SUBSCRIBE_EVENT as _, &mut sub) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Dequeues one event; `Ok(None)` when none is pending (`ENOENT` on the
/// non-blocking descriptor).
fn dequeue(file: &fs::File) -> io::Result<Option<V4l2Event>> {
    // SAFETY: an all-zero `struct v4l2_event` is a valid value (plain
    // integers and a timespec).
    let mut ev: V4l2Event = unsafe { std::mem::zeroed() };
    // SAFETY: `ev` is a valid `struct v4l2_event` that outlives the call;
    // the fd is open for the duration.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_DQEVENT as _, &mut ev) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(e)
        };
    }
    Ok(Some(ev))
}

fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall without pointers.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A running control-event watcher ([`CtrlWatcher::start`]). Dropping it
/// stops the thread and waits for it.
#[derive(Debug)]
pub struct CtrlWatcher {
    wake: Arc<OwnedFd>,
    thread: Option<JoinHandle<()>>,
    subscribed: Vec<u32>,
    refused: Vec<(u32, io::Error)>,
}

impl CtrlWatcher {
    /// Opens `path` read-only, subscribes to the control events of every
    /// control in `ids`, and starts the thread that calls `on_notice` for
    /// each event (on the watcher thread). A control whose subscription
    /// fails (one the device does not have: `EINVAL`) is left out and
    /// listed in [`CtrlWatcher::refused`]; the watcher runs with the
    /// others. When none succeeded (or `ids` is empty) the descriptor is
    /// closed before this returns, and the thread only sends one
    /// [`CtrlNotice::Ended`] saying so and ends. `Err` when the node cannot
    /// be opened or the thread not started.
    pub fn start(
        path: &Path,
        ids: &[u32],
        on_notice: impl FnMut(CtrlNotice) + Send + 'static,
    ) -> io::Result<CtrlWatcher> {
        let file = open_node(path, false)?;
        let mut subscribed = Vec::new();
        let mut refused = Vec::new();
        for &id in ids {
            match subscribe(&file, id) {
                Ok(()) => subscribed.push(id),
                Err(e) => refused.push((id, e)),
            }
        }
        let source = if !subscribed.is_empty() {
            Ok(file)
        } else {
            drop(file);
            Err(match refused.first() {
                None => io::Error::other("no control to watch"),
                Some((id, e)) => io::Error::other(format!(
                    "none of the {} controls takes event subscriptions \
                     (control {id:#010x}: {e})",
                    refused.len()
                )),
            })
        };
        CtrlWatcher::spawn(source, subscribed, refused, on_notice)
    }

    /// Starts the watcher thread on `source`: the subscribed descriptor it
    /// waits on, or the reason it ends at once (sent as the one
    /// [`CtrlNotice::Ended`]).
    fn spawn(
        source: Result<fs::File, io::Error>,
        subscribed: Vec<u32>,
        refused: Vec<(u32, io::Error)>,
        mut on_notice: impl FnMut(CtrlNotice) + Send + 'static,
    ) -> io::Result<CtrlWatcher> {
        let wake = Arc::new(eventfd()?);
        let thread_wake = Arc::clone(&wake);
        let thread = std::thread::Builder::new()
            .name("obc-ctrl-events".into())
            .spawn(move || {
                run_thread(&mut on_notice, move |on_notice| {
                    let file = match source {
                        Ok(file) => file,
                        Err(why) => return Some(why),
                    };
                    // The descriptor is closed when the loop ends, before
                    // the Ended notice.
                    watch(&file, &thread_wake, on_notice)
                })
            });
        // `source` moved into a thread that failed to start is dropped with
        // it: no descriptor stays open.
        let thread = thread?;
        Ok(CtrlWatcher {
            wake,
            thread: Some(thread),
            subscribed,
            refused,
        })
    }

    /// The controls whose events are watched.
    pub fn subscribed(&self) -> &[u32] {
        &self.subscribed
    }

    /// The controls whose subscription failed, with the error.
    pub fn refused(&self) -> &[(u32, io::Error)] {
        &self.refused
    }

    /// Whether the thread has ended (by itself, after a
    /// [`CtrlNotice::Ended`]).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Stops the thread and waits for it; the same as dropping the
    /// watcher. No notice is sent after it returns.
    pub fn stop(self) {}

    fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes 8 bytes from a live buffer to the eventfd we own.
        // A failure (the counter cannot overflow from 1s) leaves the thread
        // running; join would then hang, so it is checked.
        let n = unsafe { libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
        if n != one.len() as isize {
            eprintln!(
                "could not stop the control-event watcher: {}",
                io::Error::last_os_error()
            );
            return;
        }
        // A panic in the callback was caught and reported by the thread.
        let _ = thread.join();
    }
}

impl Drop for CtrlWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The watcher thread's body: runs `body` (the event loop, which hands each
/// notice to `on_notice`), then sends the one [`CtrlNotice::Ended`]: the
/// error the loop ended with, or the panic it raised (a panic of the
/// callback included). Nothing when it was stopped. A panic of the callback
/// while it takes that last notice is caught and dropped: nothing follows
/// it.
fn run_thread<F: FnMut(CtrlNotice)>(
    on_notice: &mut F,
    body: impl FnOnce(&mut F) -> Option<io::Error>,
) {
    let ended = match panic::catch_unwind(AssertUnwindSafe(|| body(&mut *on_notice))) {
        Ok(ended) => ended,
        Err(payload) => Some(io::Error::other(format!(
            "the control-event thread panicked: {}",
            panic_text(payload.as_ref())
        ))),
    };
    if let Some(e) = ended {
        let _ = panic::catch_unwind(AssertUnwindSafe(|| on_notice(CtrlNotice::Ended(e))));
    }
}

/// The message of a panic payload, when it has one.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)")
}

/// The watcher thread's loop: waits for an event on `file` or a stop on
/// `wake`, and hands each control event to `on_notice`. `None` when
/// stopped; the error that ended it otherwise.
fn watch(
    file: &fs::File,
    wake: &OwnedFd,
    on_notice: &mut impl FnMut(CtrlNotice),
) -> Option<io::Error> {
    loop {
        let mut fds = [
            libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLPRI,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two pollfd for the call; both
        // descriptors stay open (the file and the Arc'ed eventfd are owned
        // by this thread's caller frame and the watcher).
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Some(e);
        }
        if fds[1].revents != 0 {
            return None;
        }
        let revents = fds[0].revents;
        if revents & libc::POLLNVAL != 0 {
            return Some(io::Error::other("the node's descriptor is not valid"));
        }
        if revents & libc::POLLPRI != 0 {
            loop {
                match dequeue(file) {
                    Ok(Some(ev)) => {
                        if let Some(c) = ctrl_event(&ev) {
                            on_notice(CtrlNotice::Changed(c));
                        }
                    }
                    Ok(None) => break,
                    // A vanished node answers ENODEV.
                    Err(e) => return Some(e),
                }
            }
        }
        if revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return Some(io::Error::other("the node reported an error or hung up"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn ioctl_numbers_and_sizes() {
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT, 0x4020_565a);
        assert_eq!(std::mem::size_of::<V4l2EventSubscription>(), 32);
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
            assert_eq!(std::mem::size_of::<V4l2Event>(), 136);
            assert_eq!(std::mem::offset_of!(V4l2Event, u), 8);
            assert_eq!(std::mem::offset_of!(V4l2Event, pending), 72);
            assert_eq!(std::mem::offset_of!(V4l2Event, timestamp), 80);
            assert_eq!(std::mem::offset_of!(V4l2Event, id), 96);
        }
    }

    fn event(type_: u32, id: u32, ctrl: [i32; 9]) -> V4l2Event {
        // The struct v4l2_event_ctrl words: changes, type, value, (value
        // high), flags, minimum, maximum, step, default_value.
        let mut bytes = [0u8; 64];
        for (i, w) in ctrl.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
        }
        let mut u = [0u64; 8];
        for (i, w) in u.iter_mut().enumerate() {
            let mut b = [0; 8];
            b.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
            *w = u64::from_ne_bytes(b);
        }
        // SAFETY: all-zero is a valid value of the struct.
        let mut ev: V4l2Event = unsafe { std::mem::zeroed() };
        ev.type_ = type_;
        ev.u = u;
        ev.id = id;
        ev
    }

    #[test]
    fn a_control_event_is_decoded() {
        let ev = event(
            EVENT_CTRL,
            0x009a_0902,
            [3, 1, 1250, 0, 0x10, 3, 2047, 1, 156],
        );
        assert_eq!(
            ctrl_event(&ev),
            Some(CtrlEvent {
                id: 0x009a_0902,
                changes: CTRL_CHANGE_VALUE | CTRL_CHANGE_FLAGS,
                type_: 1,
                value: 1250,
                flags: 0x10,
                minimum: 3,
                maximum: 2047,
                step: 1,
                default_value: 156,
            })
        );
        let neg = event(
            EVENT_CTRL,
            0x009a_0909,
            [1, 1, -14400, -1, 0, -72000, 72000, 3600, 0],
        );
        assert_eq!(ctrl_event(&neg).unwrap().value, -14400);
        // Another event type (V4L2_EVENT_SOURCE_CHANGE = 5) is not one.
        assert_eq!(ctrl_event(&event(5, 0, [1; 9])), None);
    }

    /// Whether this process holds a descriptor open on `path`.
    fn holds_open(path: &Path) -> bool {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|e| fs::read_link(e.ok()?.path()).ok())
            .any(|target| target == path)
    }

    fn wait_finished(w: &CtrlWatcher) {
        let t = Instant::now();
        while !w.is_finished() {
            assert!(t.elapsed() < Duration::from_secs(5), "still running");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_watcher_without_a_subscription_ends_at_once_and_says_so() {
        // A regular file takes no subscription (ENOTTY).
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("node");
        fs::write(&node, b"").unwrap();
        let node = fs::canonicalize(&node).unwrap();
        for ids in [&[0x0098_0900, 0x0098_0913][..], &[][..]] {
            let (tx, rx) = mpsc::channel();
            let w = CtrlWatcher::start(&node, ids, move |n| {
                tx.send(n).unwrap();
            })
            .unwrap();
            // The descriptor is closed before start returns.
            assert!(!holds_open(&node), "{ids:?}");
            assert!(w.subscribed().is_empty());
            let refused: Vec<(u32, Option<i32>)> = w
                .refused()
                .iter()
                .map(|(id, e)| (*id, e.raw_os_error()))
                .collect();
            let expected: Vec<(u32, Option<i32>)> =
                ids.iter().map(|id| (*id, Some(libc::ENOTTY))).collect();
            assert_eq!(refused, expected);
            // One Ended saying why, then the thread is gone.
            let notice = rx.recv_timeout(Duration::from_secs(5)).expect("a notice");
            let CtrlNotice::Ended(why) = notice else {
                panic!("{notice:?}");
            };
            let why = why.to_string();
            if ids.is_empty() {
                assert_eq!(why, "no control to watch");
            } else {
                assert!(why.contains("none of the 2 controls"), "{why}");
                assert!(why.contains("0x00980900"), "{why}");
            }
            wait_finished(&w);
            drop(w);
            assert!(matches!(
                rx.recv_timeout(Duration::from_secs(1)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ));
        }
    }

    #[test]
    fn a_running_watcher_stops_at_once_without_a_notice() {
        // /dev/null never signals POLLPRI: the thread waits until it is
        // stopped.
        let (tx, rx) = mpsc::channel();
        let file = fs::File::open("/dev/null").unwrap();
        let w = CtrlWatcher::spawn(Ok(file), vec![0x0098_0900], Vec::new(), move |n| {
            tx.send(format!("{n:?}")).unwrap();
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!w.is_finished());
        let t = Instant::now();
        w.stop();
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        // Stopped: no notice at all.
        assert!(rx.try_recv().is_err());
    }

    fn fifo(dir: &Path) -> std::path::PathBuf {
        let fifo = dir.join("node");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path for the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        fifo
    }

    #[test]
    fn a_node_that_hangs_up_ends_the_watcher_once() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = fifo(dir.path());
        let (tx, rx) = mpsc::channel();
        let file = open_node(&fifo, false).unwrap();
        let w = CtrlWatcher::spawn(Ok(file), vec![0x0098_0900], Vec::new(), move |n| {
            tx.send(n).unwrap();
        })
        .unwrap();
        // A writer that comes and goes: the reader sees POLLHUP.
        drop(fs::OpenOptions::new().write(true).open(&fifo).unwrap());
        let notice = rx.recv_timeout(Duration::from_secs(5)).expect("a notice");
        assert!(matches!(notice, CtrlNotice::Ended(_)), "{notice:?}");
        // Ended is the thread's last act: joining returns at once.
        drop(w);
        // Exactly once; the sender is gone with the thread.
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    fn some_event() -> CtrlEvent {
        ctrl_event(&event(
            EVENT_CTRL,
            0x0098_0900,
            [1, 1, 5, 0, 0, 0, 10, 1, 5],
        ))
        .unwrap()
    }

    #[test]
    fn a_panicking_callback_is_reported_as_ended() {
        let (tx, rx) = mpsc::channel();
        let mut on_notice = move |n: CtrlNotice| {
            if let CtrlNotice::Changed(_) = n {
                panic!("the callback broke");
            }
            tx.send(n).unwrap();
        };
        let mut after_panic = false;
        run_thread(&mut on_notice, |f| {
            f(CtrlNotice::Changed(some_event()));
            after_panic = true;
            None
        });
        assert!(!after_panic, "the loop goes on after the panic");
        let notices: Vec<CtrlNotice> = rx.try_iter().collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        let CtrlNotice::Ended(why) = &notices[0] else {
            panic!("{notices:?}");
        };
        assert!(why.to_string().contains("panicked"), "{why}");
        assert!(why.to_string().contains("the callback broke"), "{why}");

        // A panic while taking the Ended itself: caught, nothing follows.
        let mut calls = 0;
        let mut always = |_: CtrlNotice| {
            calls += 1;
            panic!("again");
        };
        run_thread(&mut always, |_| Some(io::Error::other("gone")));
        assert_eq!(calls, 1);
        // A stopped loop: no notice.
        let mut calls = 0;
        run_thread(&mut |_: CtrlNotice| calls += 1, |_| None);
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_watcher_whose_callback_panics_ends_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = fifo(dir.path());
        let file = open_node(&fifo, false).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let w = CtrlWatcher::spawn(Ok(file), vec![0x0098_0900], Vec::new(), move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("the callback broke on Ended");
        })
        .unwrap();
        drop(fs::OpenOptions::new().write(true).open(&fifo).unwrap());
        wait_finished(&w);
        let t = Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_missing_node_is_an_error() {
        assert!(CtrlWatcher::start(Path::new("/nonexistent/obc-node"), &[1], |_| {}).is_err());
    }
}
