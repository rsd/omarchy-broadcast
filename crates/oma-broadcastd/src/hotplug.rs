//! Camera hotplug: an inotify watch on `/dev` for `video*` nodes being
//! created or deleted. After a burst of events the watcher waits
//! [`SETTLE`] (udev sets the node's permissions and its `/dev/v4l` links
//! just after creating it), asks for a rescan, and asks once more
//! [`SECOND_SCAN`] later in case udev was slower.

use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::inotify::{
    IN_CREATE, IN_DELETE, IN_MOVED_FROM, IN_MOVED_TO, IN_Q_OVERFLOW, Inotify, Waker, poll2,
};
use crate::{debug, warning};

/// Delay from the last `video*` event to the first rescan.
pub const SETTLE: Duration = Duration::from_millis(500);
/// Delay from the last event to the second rescan.
pub const SECOND_SCAN: Duration = Duration::from_secs(3);

/// The watcher thread; `on_change` is called for each rescan it asks for.
pub struct CameraWatcher {
    stop: Arc<AtomicBool>,
    waker: Arc<Waker>,
    thread: Option<JoinHandle<()>>,
}

impl CameraWatcher {
    /// Watches `dev` (`/dev` outside tests).
    pub fn spawn(
        dev: PathBuf,
        settle: Duration,
        second: Duration,
        on_change: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        let inotify = Inotify::new()?;
        inotify.add_watch(&dev, IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO)?;
        let waker = Arc::new(Waker::new()?);
        let stop = Arc::new(AtomicBool::new(false));
        let (w, s) = (Arc::clone(&waker), Arc::clone(&stop));
        let thread = std::thread::Builder::new()
            .name("obc-hotplug".into())
            .spawn(move || watch_loop(&inotify, &w, &s, settle, second, &on_change))?;
        Ok(CameraWatcher {
            stop,
            waker,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.waker.wake();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CameraWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn is_video_node(name: Option<&std::ffi::OsStr>) -> bool {
    name.and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("video"))
}

fn watch_loop(
    inotify: &Inotify,
    waker: &Waker,
    stop: &AtomicBool,
    settle: Duration,
    second: Duration,
    on_change: &dyn Fn(),
) {
    let mut first_at: Option<Instant> = None;
    let mut second_at: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        let next = [first_at, second_at].into_iter().flatten().min();
        let timeout = next.map(|t| t.saturating_duration_since(now));
        let (events, woke) = match poll2(Some(inotify.as_raw_fd()), waker.as_raw_fd(), timeout) {
            Ok(r) => r,
            Err(e) => {
                warning!("camera watcher: poll failed: {e}");
                std::thread::sleep(Duration::from_secs(1));
                (false, false)
            }
        };
        if woke {
            waker.drain();
        }
        if events {
            match inotify.read_events() {
                Ok(evs) => {
                    let relevant = evs
                        .iter()
                        .any(|e| e.mask & IN_Q_OVERFLOW != 0 || is_video_node(e.name.as_deref()));
                    if relevant {
                        debug!("video nodes changed: rescanning soon");
                        let now = Instant::now();
                        first_at = Some(now + settle);
                        second_at = Some(now + second);
                    }
                }
                Err(e) => warning!("camera watcher: reading inotify: {e}"),
            }
        }
        let now = Instant::now();
        if first_at.is_some_and(|t| now >= t) {
            first_at = None;
            on_change();
        }
        if second_at.is_some_and(|t| now >= t) {
            second_at = None;
            on_change();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn video_nodes_trigger_two_rescans_and_others_none() {
        let dir = tempfile::tempdir().unwrap();
        let hits: Arc<Mutex<Vec<Instant>>> = Arc::default();
        let h = Arc::clone(&hits);
        let w = CameraWatcher::spawn(
            dir.path().to_path_buf(),
            Duration::from_millis(100),
            Duration::from_millis(300),
            move || h.lock().unwrap().push(Instant::now()),
        )
        .unwrap();
        std::fs::write(dir.path().join("ttyS9"), b"").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(hits.lock().unwrap().is_empty());

        let t0 = Instant::now();
        std::fs::write(dir.path().join("video12"), b"").unwrap();
        std::fs::remove_file(dir.path().join("video12")).unwrap();
        // Event-driven: wait for the second rescan (the limit only bounds a
        // failing run), then check the lower bounds.
        let deadline = Instant::now() + Duration::from_secs(30);
        while hits.lock().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let got = hits.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0] >= t0 + Duration::from_millis(100));
        assert!(got[1] >= t0 + Duration::from_millis(300));
        w.stop();
    }
}
