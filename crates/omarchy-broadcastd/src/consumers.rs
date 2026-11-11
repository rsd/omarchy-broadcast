//! Who holds the virtual camera (`docs/adr/0011-daemon-lifecycle-and-dbus-api.md`,
//! "Consumer detection").
//!
//! An inotify watch on the loopback node (`IN_OPEN`, `IN_CLOSE_WRITE`,
//! `IN_CLOSE_NOWRITE`) says *when* to look; [`count_holders`] says *who*:
//! every process other than the daemon with a `/proc/<pid>/fd` entry that
//! resolves to the node. Each burst of events is counted at once and again
//! [`RECOUNT_AFTER`] later, which catches a close that raced the first
//! count. If the watch cannot be set the watcher counts every
//! [`POLL_FALLBACK`]. A `/proc/<pid>/fd` that cannot be read (another
//! user's process) is skipped. A queue overflow (events lost) is counted
//! at once, like a burst.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::inotify::{
    Event, IN_CLOSE_NOWRITE, IN_CLOSE_WRITE, IN_DELETE_SELF, IN_IGNORED, IN_OPEN, IN_Q_OVERFLOW,
    Inotify, Waker, poll2,
};
use crate::{debug, warning};

/// The second count after a burst of events.
pub const RECOUNT_AFTER: Duration = Duration::from_millis(250);
/// The counting period when the node cannot be watched.
pub const POLL_FALLBACK: Duration = Duration::from_secs(1);

/// What identifies the node: its canonical path and, for a device, its
/// device number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeId {
    path: PathBuf,
    rdev: Option<u64>,
}

impl NodeId {
    pub fn of(node: &Path) -> NodeId {
        let path = fs::canonicalize(node).unwrap_or_else(|_| node.to_path_buf());
        let rdev = fs::metadata(&path)
            .ok()
            .filter(|m| m.file_type().is_char_device() || m.file_type().is_block_device())
            .map(|m| m.rdev());
        NodeId { path, rdev }
    }

    /// Whether the open file `fd` (a `/proc/<pid>/fd/<n>` entry) is this
    /// node: its link names the path or, for a device seen under another
    /// name (a container's `/dev`), the same device number.
    fn is(&self, fd: &Path) -> bool {
        let Ok(target) = fs::read_link(fd) else {
            return false;
        };
        if target == self.path {
            return true;
        }
        match self.rdev {
            Some(rdev) if target.starts_with("/dev/") => {
                fs::metadata(fd).is_ok_and(|m| m.file_type().is_char_device() && m.rdev() == rdev)
            }
            _ => false,
        }
    }
}

/// The number of processes other than `exclude` that hold `node` open,
/// from the `/proc` tree at `proc_dir`.
pub fn count_holders(proc_dir: &Path, node: &NodeId, exclude: u32) -> u32 {
    let Ok(entries) = fs::read_dir(proc_dir) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == exclude {
            continue;
        }
        let Ok(fds) = fs::read_dir(e.path().join("fd")) else {
            continue; // another user's process, or it just exited
        };
        if fds.flatten().any(|fd| node.is(&fd.path())) {
            n += 1;
        }
    }
    n
}

/// The process names (`/proc/<pid>/comm`, trimmed; the pid when it
/// cannot be read) of the processes other than `exclude` that hold `node`
/// open, from the `/proc` tree at `proc_dir`, one per process.
pub fn holder_names(proc_dir: &Path, node: &NodeId, exclude: u32) -> Vec<String> {
    let Ok(entries) = fs::read_dir(proc_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == exclude {
            continue;
        }
        let Ok(fds) = fs::read_dir(e.path().join("fd")) else {
            continue;
        };
        if fds.flatten().any(|fd| node.is(&fd.path())) {
            let name = fs::read_to_string(e.path().join("comm"))
                .map(|s| s.trim().to_string())
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("process {pid}"));
            out.push(name);
        }
    }
    out
}

/// Whether inotify event `e` asks for a count: an event of the watch `mine`,
/// or a queue overflow (`wd` -1: events were lost, the count may be stale).
fn wants_count(e: &Event, mine: Option<i32>) -> bool {
    e.mask & IN_Q_OVERFLOW != 0 || (mine.is_some() && Some(e.wd) == mine)
}

enum Control {
    Watch(Option<PathBuf>),
    Stop,
}

/// The watcher thread (see the module docs). It reports every change of the
/// count through the callback, and the count right after each
/// [`Self::watch`].
pub struct ConsumerWatcher {
    control: Sender<Control>,
    waker: Arc<Waker>,
    thread: Option<JoinHandle<()>>,
}

impl ConsumerWatcher {
    /// Starts the thread, watching nothing yet. `report` receives counts.
    /// `proc_dir` is `/proc` outside tests; `exclude` the daemon's pid.
    pub fn spawn(
        proc_dir: PathBuf,
        exclude: u32,
        report: impl Fn(u32) + Send + 'static,
    ) -> io::Result<Self> {
        let waker = Arc::new(Waker::new()?);
        let (control, rx) = channel();
        let w = Arc::clone(&waker);
        let thread = std::thread::Builder::new()
            .name("obc-consumers".into())
            .spawn(move || watch_loop(&proc_dir, exclude, &rx, &w, &report))?;
        Ok(ConsumerWatcher {
            control,
            waker,
            thread: Some(thread),
        })
    }

    /// Watches `node` from now on (`None`: nothing).
    pub fn watch(&self, node: Option<PathBuf>) {
        let _ = self.control.send(Control::Watch(node));
        self.waker.wake();
    }

    /// Stops and joins the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.control.send(Control::Stop);
        self.waker.wake();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ConsumerWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Watched {
    node: PathBuf,
    id: NodeId,
    /// The inotify watch; `None`: counting every [`POLL_FALLBACK`].
    wd: Option<i32>,
}

fn watch_loop(
    proc_dir: &Path,
    exclude: u32,
    control: &Receiver<Control>,
    waker: &Waker,
    report: &dyn Fn(u32),
) {
    let inotify = match Inotify::new() {
        Ok(i) => Some(i),
        Err(e) => {
            warning!("inotify unavailable ({e}): counting consumers every second");
            None
        }
    };
    let mut watched: Option<Watched> = None;
    let mut last: Option<u32> = None;
    let mut recount_at: Option<Instant> = None;
    let mut poll_at: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let next = [recount_at, poll_at].into_iter().flatten().min();
        let timeout = next.map(|t| t.saturating_duration_since(now));
        let (events, woke) = match poll2(
            inotify.as_ref().map(AsRawFd::as_raw_fd),
            waker.as_raw_fd(),
            timeout,
        ) {
            Ok(r) => r,
            Err(e) => {
                warning!("consumer watcher: poll failed: {e}");
                std::thread::sleep(POLL_FALLBACK);
                (false, false)
            }
        };
        let mut count_now = false;
        if woke {
            waker.drain();
            loop {
                match control.try_recv() {
                    Ok(Control::Watch(node)) => {
                        if let (Some(i), Some(w)) = (inotify.as_ref(), watched.take())
                            && let Some(wd) = w.wd
                        {
                            i.rm_watch(wd);
                        }
                        watched = node.map(|node| {
                            let wd = inotify.as_ref().and_then(|i| {
                                i.add_watch(&node, IN_OPEN | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE)
                                    .inspect_err(|e| {
                                        warning!(
                                            "cannot watch {} ({e}): counting its consumers every second",
                                            node.display()
                                        )
                                    })
                                    .ok()
                            });
                            Watched {
                                id: NodeId::of(&node),
                                node,
                                wd,
                            }
                        });
                        if watched.is_some() {
                            last = None;
                        }
                        count_now = true;
                    }
                    Ok(Control::Stop) | Err(TryRecvError::Disconnected) => return,
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
        if events && let Some(i) = inotify.as_ref() {
            match i.read_events() {
                Ok(evs) => {
                    let mine = watched.as_ref().and_then(|w| w.wd);
                    for e in evs.iter().filter(|e| wants_count(e, mine)) {
                        if e.mask & IN_Q_OVERFLOW != 0 {
                            debug!("inotify queue overflow: counting the consumers again");
                        } else if e.mask & (IN_IGNORED | IN_DELETE_SELF) != 0
                            && let Some(w) = watched.as_mut()
                        {
                            debug!("{} is gone: counting every second", w.node.display());
                            w.wd = None;
                        }
                        count_now = true;
                    }
                    if count_now {
                        recount_at = Some(Instant::now() + RECOUNT_AFTER);
                    }
                }
                Err(e) => warning!("consumer watcher: reading inotify: {e}"),
            }
        }
        let now = Instant::now();
        if recount_at.is_some_and(|t| now >= t) {
            recount_at = None;
            count_now = true;
        }
        if poll_at.is_some_and(|t| now >= t) {
            count_now = true;
        }
        let Some(w) = watched.as_ref() else {
            poll_at = None;
            recount_at = None;
            if count_now && last != Some(0) {
                last = Some(0);
                report(0);
            }
            continue;
        };
        poll_at = w.wd.is_none().then(|| now + POLL_FALLBACK);
        if count_now {
            let n = count_holders(proc_dir, &w.id, exclude);
            if last != Some(n) {
                last = Some(n);
                report(n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    /// A child process holding `file` open (as its stdin) until killed.
    struct Holder(Child);

    impl Holder {
        fn spawn(file: &Path) -> Holder {
            let f = fs::File::open(file).unwrap();
            Holder(
                Command::new("sleep")
                    .arg("60")
                    .stdin(Stdio::from(f))
                    .spawn()
                    .expect("spawn `sleep`"),
            )
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        done()
    }

    #[test]
    fn a_queue_overflow_asks_for_a_count_like_an_event_of_the_watch() {
        let ev = |wd: i32, mask: u32| Event {
            wd,
            mask,
            name: None,
        };
        assert!(wants_count(&ev(-1, IN_Q_OVERFLOW), Some(3)));
        assert!(wants_count(&ev(-1, IN_Q_OVERFLOW), None));
        assert!(wants_count(&ev(3, IN_OPEN), Some(3)));
        assert!(!wants_count(&ev(4, IN_OPEN), Some(3)));
        assert!(!wants_count(&ev(-1, IN_OPEN), None));
    }

    #[test]
    fn a_child_holding_the_node_is_counted_and_we_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("video99");
        fs::write(&node, b"").unwrap();
        let id = NodeId::of(&node);
        let me = std::process::id();
        let proc_dir = Path::new("/proc");
        assert_eq!(count_holders(proc_dir, &id, me), 0);

        // Our own descriptor is excluded by pid.
        let mine = fs::File::open(&node).unwrap();
        assert_eq!(count_holders(proc_dir, &id, me), 0);
        assert_eq!(
            count_holders(proc_dir, &id, 0),
            1,
            "counted without the exclusion"
        );

        let a = Holder::spawn(&node);
        assert!(wait_for(Duration::from_secs(5), || count_holders(
            proc_dir, &id, me
        ) == 1));
        let b = Holder::spawn(&node);
        assert!(wait_for(Duration::from_secs(5), || count_holders(
            proc_dir, &id, me
        ) == 2));
        drop(a);
        assert_eq!(count_holders(proc_dir, &id, me), 1);
        drop(b);
        assert_eq!(count_holders(proc_dir, &id, me), 0);
        drop(mine);
        // Another file is not this node.
        let other = dir.path().join("other");
        fs::write(&other, b"").unwrap();
        let _c = Holder::spawn(&other);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count_holders(proc_dir, &id, me), 0);
    }

    #[test]
    fn the_holders_are_named_by_their_process_name() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("video98");
        fs::write(&node, b"").unwrap();
        let id = NodeId::of(&node);
        let me = std::process::id();
        let proc_dir = Path::new("/proc");
        assert!(holder_names(proc_dir, &id, me).is_empty());
        let _a = Holder::spawn(&node);
        let _b = Holder::spawn(&node);
        assert!(wait_for(Duration::from_secs(5), || holder_names(
            proc_dir, &id, me
        )
        .len()
            == 2));
        assert_eq!(holder_names(proc_dir, &id, me), ["sleep", "sleep"]);
    }

    #[test]
    fn unreadable_entries_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // A fake /proc: one pid without an fd directory, one non-pid entry.
        fs::create_dir_all(dir.path().join("123")).unwrap();
        fs::create_dir_all(dir.path().join("self")).unwrap();
        let id = NodeId::of(Path::new("/nonexistent/video1"));
        assert_eq!(count_holders(dir.path(), &id, 1), 0);
        assert_eq!(count_holders(&dir.path().join("missing"), &id, 1), 0);
    }

    #[test]
    fn inotify_events_trigger_recounts() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("video98");
        fs::write(&node, b"").unwrap();
        let seen: Arc<Mutex<Vec<u32>>> = Arc::default();
        let s = Arc::clone(&seen);
        let w = ConsumerWatcher::spawn(PathBuf::from("/proc"), std::process::id(), move |n| {
            s.lock().unwrap().push(n)
        })
        .unwrap();
        w.watch(Some(node.clone()));
        let counts = || seen.lock().unwrap().clone();
        assert!(
            wait_for(Duration::from_secs(2), || counts() == [0]),
            "{:?}",
            counts()
        );

        // Opening the node ourselves fires events but changes nothing.
        drop(fs::File::open(&node).unwrap());
        std::thread::sleep(RECOUNT_AFTER * 2);
        assert_eq!(counts(), [0]);

        let h = Holder::spawn(&node);
        assert!(
            wait_for(Duration::from_secs(3), || counts() == [0, 1]),
            "{:?}",
            counts()
        );
        drop(h);
        assert!(
            wait_for(Duration::from_secs(3), || counts() == [0, 1, 0]),
            "{:?}",
            counts()
        );

        // Watching nothing reports nothing more; the thread stops cleanly.
        w.watch(None);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(counts(), [0, 1, 0]);
        w.stop();
    }

    #[test]
    fn without_a_watch_it_counts_every_second() {
        // A node that does not exist cannot be watched: polling instead.
        let dir = tempfile::tempdir().unwrap();
        let node = fs::canonicalize(dir.path()).unwrap().join("video97");
        let seen: Arc<Mutex<Vec<u32>>> = Arc::default();
        let s = Arc::clone(&seen);
        let w = ConsumerWatcher::spawn(PathBuf::from("/proc"), std::process::id(), move |n| {
            s.lock().unwrap().push(n)
        })
        .unwrap();
        w.watch(Some(node.clone()));
        let counts = || seen.lock().unwrap().clone();
        assert!(wait_for(Duration::from_secs(2), || counts() == [0]));
        // The node appears and a process holds it: the next poll sees it.
        fs::write(&node, b"").unwrap();
        let h = Holder::spawn(&node);
        assert!(
            wait_for(POLL_FALLBACK * 3, || counts() == [0, 1]),
            "{:?}",
            counts()
        );
        drop(h);
        drop(w);
    }
}
