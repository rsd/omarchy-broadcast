//! Playback copies: a CPU-ready VP9 copy of a video, made in the cache for
//! playback in system memory (ADR-0035, "The playback copy"; the
//! gallery item itself is never replaced).
//!
//! "CPU-ready" ([`is_cpu_ready`]) is VP9 profile 0 in WebM or Matroska, at
//! most 1920x1080 and 30 fps: what `vp9dec` plays cheaply. Any other video
//! the gallery accepts gets a copy the first time a system-memory session
//! needs it: [`PlaybackCopies::ensure`] answers at once (never blocking the
//! caller on a hash, a check or ffmpeg) and, when there is no copy known,
//! queues a job on the service's own thread. A source not hashed yet in
//! this run is [`CopyState::Checking`]: the job hashes it and finds an
//! existing copy (then [`CopyState::Ready`]), or says it starts the
//! transcode ([`CopyState::Preparing`]). One job runs at a time; a newer
//! request replaces a queued one and cancels a running one for another
//! source (a cancelled job never answers a later request for its source:
//! that request queues a new job). The steps of a job are [`CopyEvent`]s
//! on the channel [`PlaybackCopies::new`] returns (a cancelled job sends
//! none). A job that failed is remembered for that file's content until
//! [`PlaybackCopies::remove_for`] or a change of the file, so asking again
//! does not loop on ffmpeg; [`PlaybackCopies::retry`] (a camera start)
//! runs it once more, once per failure.
//!
//! ffmpeg not found when the service started is looked up again at each
//! request ([`Ffmpeg::locate`]), so an install while the daemon runs is
//! seen without a restart.
//!
//! The copy is `playback/<content hash>-v<settings version>.webm`
//! ([`MediaCache::copy_path`]), written to a temporary file, synced and
//! renamed. A copy that is missing, or whose start is not a WebM, is made
//! again. A file outside the gallery follows the same rule under the same
//! key.
//!
//! [`PlaybackCopies::prune_orphans`] (meant for the daemon's start, off the
//! controller thread: it hashes every gallery video, once per run; through
//! [`PlaybackCopies::request_prune`] it is cancelled, removing nothing, when
//! the service is dropped) removes
//! posters and copies that no gallery video needs: a copy or poster is kept
//! when its hash is a gallery video's, or the outside file's the caller
//! names; when the caller names none, the newest unreferenced copy of the
//! current settings is kept as well (the copy of an outside file the
//! configuration may still name). Copies of older settings versions, and
//! temporary files of interrupted jobs, always go.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;

use super::MediaStore;
use super::cache::{self, EBML_MAGIC, MediaCache, PendingFile};
use super::ffmpeg::{
    self, CPU_MAX_FPS, CPU_MAX_HEIGHT, CPU_MAX_WIDTH, ENCODE_SETTINGS_VERSION, Ffmpeg, FfmpegError,
    FfmpegJob,
};
use super::hash::{self, FileKey};
use super::probe::{self, Codec, Container, MAX_VIDEO_BYTES, VideoInfo};
use crate::{debug, info, warning};

/// Whether `info` is a video the CPU plays directly: VP9 profile 0 in
/// WebM or Matroska, at most 1920x1080 and 30 fps (a rate that is not
/// stated is not ready).
pub fn is_cpu_ready(info: &VideoInfo) -> bool {
    matches!(info.container, Container::WebM | Container::Matroska)
        && info.codec == Codec::Vp9
        && (info.profile.is_empty() || info.profile == "0")
        && info.width <= CPU_MAX_WIDTH
        && info.height <= CPU_MAX_HEIGHT
        && info.fps_num > 0
        && u64::from(info.fps_num) <= u64::from(CPU_MAX_FPS) * u64::from(info.fps_den.max(1))
}

/// What [`PlaybackCopies::ensure`] says about a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyState {
    /// The copy is at this path and complete (or the source itself, when
    /// the job found it CPU-ready).
    Ready(PathBuf),
    /// The source is not hashed yet in this run: a job hashes it and looks
    /// for its copy; a [`CopyEvent`] follows (`Ready`, or `Preparing` when
    /// the copy must be made).
    Checking,
    /// The copy is being made; a [`CopyEvent`] follows.
    Preparing,
    /// A job for this content failed; the reason.
    Failed(String),
    /// ffmpeg is not installed: no copy can be made.
    NoFfmpeg,
}

/// A step of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyEvent {
    /// The source as it was asked for.
    pub source: PathBuf,
    /// [`CopyState::Preparing`] when the job starts the transcode (no copy
    /// existed), then its end: [`CopyState::Ready`] with the path to play
    /// (the copy, or the source itself when the job found it CPU-ready),
    /// or [`CopyState::Failed`] with why there is none.
    pub state: CopyState,
}

/// A request for a copy.
#[derive(Debug, Clone)]
struct Request {
    source: PathBuf,
    key: FileKey,
    /// Whether it runs again a job that failed ([`PlaybackCopies::retry`]).
    retried: bool,
}

/// The job running now.
#[derive(Debug)]
struct Running {
    source: PathBuf,
    key: FileKey,
    cancel: Arc<AtomicBool>,
    /// Whether it found no copy and transcodes (else it is checking).
    transcoding: bool,
}

/// A job that failed.
#[derive(Debug, Clone)]
struct Failure {
    source: PathBuf,
    reason: String,
    /// Whether it was already run again once.
    retried: bool,
}

/// A prune asked of the worker.
#[derive(Debug, Clone)]
struct PruneRequest {
    store: MediaStore,
    outside: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct State {
    queued: Option<Request>,
    running: Option<Running>,
    prune: Option<PruneRequest>,
    /// Failed jobs by content.
    failed: HashMap<FileKey, Failure>,
    stop: bool,
}

struct Shared {
    cache: MediaCache,
    /// The ffmpeg in use; when not found and there is a `relocate`, looked
    /// up again with it at each request.
    ffmpeg: Mutex<Result<Ffmpeg, FfmpegError>>,
    relocate: Option<fn() -> Result<Ffmpeg, FfmpegError>>,
    state: Mutex<State>,
    wake: Condvar,
    events: Sender<CopyEvent>,
    /// Set when the service is dropped: a prune in progress stops.
    stopping: AtomicBool,
}

/// The playback copy service: a worker thread making one copy at a time.
/// Dropping it cancels the running job (its ffmpeg is killed) or prune
/// (between two reads of a file), and joins the thread.
pub struct PlaybackCopies {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

/// What a prune removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneReport {
    /// The files removed.
    pub removed: Vec<PathBuf>,
}

/// The live services, so a removal of a gallery item through
/// [`MediaStore::remove`] cancels their jobs for it.
fn services() -> &'static Mutex<Vec<Weak<Shared>>> {
    static S: OnceLock<Mutex<Vec<Weak<Shared>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

/// Cancels and forgets every live service's job for `source`.
pub(crate) fn forget_everywhere(source: &Path) {
    let live: Vec<Arc<Shared>> = services()
        .lock()
        .map(|v| v.iter().filter_map(Weak::upgrade).collect())
        .unwrap_or_default();
    for s in live {
        s.forget(source);
    }
}

impl PlaybackCopies {
    /// A service writing into `cache` with `ffmpeg`, used as given, and
    /// the channel of its [`CopyEvent`]s.
    pub fn new(
        cache: MediaCache,
        ffmpeg: Result<Ffmpeg, FfmpegError>,
    ) -> (PlaybackCopies, Receiver<CopyEvent>) {
        Self::start(cache, ffmpeg, None)
    }

    /// [`PlaybackCopies::new`] with the ffmpeg of `PATH`
    /// ([`Ffmpeg::locate`]), looked up again at each request while it is
    /// not found.
    pub fn located(cache: MediaCache) -> (PlaybackCopies, Receiver<CopyEvent>) {
        Self::start(cache, Ffmpeg::locate(), Some(Ffmpeg::locate))
    }

    /// [`PlaybackCopies::located`] with `locate` instead of
    /// [`Ffmpeg::locate`] (tests).
    #[cfg(test)]
    pub(crate) fn with_locator(
        cache: MediaCache,
        locate: fn() -> Result<Ffmpeg, FfmpegError>,
    ) -> (PlaybackCopies, Receiver<CopyEvent>) {
        Self::start(cache, locate(), Some(locate))
    }

    fn start(
        cache: MediaCache,
        ffmpeg: Result<Ffmpeg, FfmpegError>,
        relocate: Option<fn() -> Result<Ffmpeg, FfmpegError>>,
    ) -> (PlaybackCopies, Receiver<CopyEvent>) {
        let (events, rx) = channel();
        let shared = Arc::new(Shared {
            cache,
            ffmpeg: Mutex::new(ffmpeg),
            relocate,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            events,
            stopping: AtomicBool::new(false),
        });
        if let Ok(mut v) = services().lock() {
            v.retain(|w| w.strong_count() > 0);
            v.push(Arc::downgrade(&shared));
        }
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("obc-playback-copy".into())
            .spawn(move || worker_shared.work())
            .map_err(|e| warning!("media: the playback copy thread did not start: {e}"))
            .ok();
        (PlaybackCopies { shared, worker }, rx)
    }

    /// The service of `store`'s cache with the ffmpeg of `PATH`; `None`
    /// when the store has no cache.
    pub fn for_store(store: &MediaStore) -> Option<(PlaybackCopies, Receiver<CopyEvent>)> {
        Some(Self::located(store.cache()?.clone()))
    }

    /// The cache it writes into.
    pub fn cache(&self) -> &MediaCache {
        &self.shared.cache
    }

    /// The copy path of `source`: hashes it (once per daemon run; a large
    /// file takes a moment, so not on the controller thread unless
    /// [`PlaybackCopies::ensure`] said `Ready`). The reason when it cannot
    /// be read.
    pub fn copy_path(&self, source: &Path) -> Result<PathBuf, String> {
        let mut file = probe::open_video(source)?;
        let h = hash::hash_file(&mut file, MAX_VIDEO_BYTES, None)
            .map_err(|e| format!("the video file {}: {e}", source.display()))?;
        Ok(self.shared.cache.copy_path(&h))
    }

    /// The state of the copy of `source`, starting a job when there is
    /// none. Never blocks: a stat of `source` and a lookup; a source never
    /// hashed in this run is `Checking` (the job hashes it and looks for
    /// its copy). A request for another source than the running job's
    /// cancels that job; a queued one is replaced. A job that failed for
    /// this content is `Failed` without a new job. The caller decides
    /// whether `source` needs a copy ([`is_cpu_ready`]); a job that finds
    /// it CPU-ready ends with the source itself as its result.
    pub fn ensure(&self, source: &Path) -> CopyState {
        self.request(source, false)
    }

    /// [`PlaybackCopies::ensure`], except that a job that failed for this
    /// content runs once more (once per failure: a second failure is then
    /// `Failed` until the file changes or is forgotten). For a camera
    /// start.
    pub fn retry(&self, source: &Path) -> CopyState {
        self.request(source, true)
    }

    fn request(&self, source: &Path, retry: bool) -> CopyState {
        if self.shared.current_ffmpeg().is_err() {
            return CopyState::NoFfmpeg;
        }
        let meta = match std::fs::metadata(source) {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                return CopyState::Failed(format!(
                    "the video file {} is not a regular file",
                    source.display()
                ));
            }
            Err(e) => {
                return CopyState::Failed(format!(
                    "the video file {} is unreadable: {e}",
                    source.display()
                ));
            }
        };
        let key = FileKey::of(&meta);
        // Known hash, no copy: the job transcodes at once.
        let mut waiting = CopyState::Checking;
        if let Some(h) = hash::known(&key) {
            let path = self.shared.cache.copy_path(&h);
            if cache::has_magic(&path, EBML_MAGIC) {
                return CopyState::Ready(path);
            }
            waiting = CopyState::Preparing;
        }
        let Ok(mut st) = self.shared.state.lock() else {
            return CopyState::Failed("the playback copy service failed".into());
        };
        let mut retried = false;
        if let Some(f) = st.failed.get(&key) {
            if !retry || f.retried {
                return CopyState::Failed(f.reason.clone());
            }
            st.failed.remove(&key);
            retried = true;
        }
        if let Some(r) = &st.running {
            // A cancelled job ends with no event: it answers nothing.
            if r.key == key && r.source == source && !r.cancel.load(Ordering::Relaxed) {
                return if r.transcoding {
                    CopyState::Preparing
                } else {
                    CopyState::Checking
                };
            }
            r.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(q) = st.queued.as_mut()
            && q.key == key
            && q.source == source
        {
            q.retried |= retried;
            return waiting;
        }
        st.queued = Some(Request {
            source: source.to_path_buf(),
            key,
            retried,
        });
        self.shared.wake.notify_all();
        waiting
    }

    /// Cancels the job for `source` (queued or running), forgets a failure
    /// of it, and removes its copy (and its poster) from the cache. Used
    /// when a gallery item is removed: [`MediaStore::remove`] does the
    /// same for every live service.
    pub fn remove_for(&self, source: &Path) {
        self.shared.forget(source);
        if let Ok(meta) = std::fs::metadata(source)
            && let Some(h) = hash::known(&FileKey::of(&meta))
        {
            self.shared.cache.remove_derived(&h);
        }
    }

    /// Cancels the running job, if any, and drops a queued one.
    pub fn cancel(&self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.queued = None;
            if let Some(r) = &st.running {
                r.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Asks the worker to prune the cache (see the module documentation)
    /// before its next job; for the daemon's start.
    pub fn request_prune(&self, store: &MediaStore, outside: Option<&Path>) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.prune = Some(PruneRequest {
                store: store.clone(),
                outside: outside.map(Path::to_path_buf),
            });
            self.shared.wake.notify_all();
        }
    }

    /// Prunes the cache now, on the caller's thread (see the module
    /// documentation; it hashes every gallery video not hashed yet).
    pub fn prune_orphans(&self, store: &MediaStore, outside: Option<&Path>) -> PruneReport {
        prune(&self.shared.cache, store, outside, &AtomicBool::new(false))
    }

    /// Whether a job is running or queued (for tests and diagnostics).
    pub fn busy(&self) -> bool {
        self.shared
            .state
            .lock()
            .is_ok_and(|st| st.running.is_some() || st.queued.is_some() || st.prune.is_some())
    }
}

impl Drop for PlaybackCopies {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        if let Ok(mut st) = self.shared.state.lock() {
            st.stop = true;
            st.queued = None;
            st.prune = None;
            if let Some(r) = &st.running {
                r.cancel.store(true, Ordering::Relaxed);
            }
            self.shared.wake.notify_all();
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// What the worker takes next.
enum Task {
    Prune(PruneRequest),
    Copy(Request, Arc<AtomicBool>),
}

impl Shared {
    /// The ffmpeg in use, looked up again when not found (and `relocate`).
    fn current_ffmpeg(&self) -> Result<Ffmpeg, FfmpegError> {
        let mut f = self
            .ffmpeg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if f.is_err()
            && let Some(locate) = self.relocate
        {
            *f = locate();
        }
        f.clone()
    }

    fn forget(&self, source: &Path) {
        if let Ok(mut st) = self.state.lock() {
            if st.queued.as_ref().is_some_and(|q| q.source == source) {
                st.queued = None;
            }
            if let Some(r) = &st.running
                && r.source == source
            {
                r.cancel.store(true, Ordering::Relaxed);
            }
            st.failed.retain(|_, f| f.source != source);
        }
    }

    fn next_task(&self) -> Option<Task> {
        let mut st = self.state.lock().ok()?;
        loop {
            if st.stop {
                return None;
            }
            if let Some(p) = st.prune.take() {
                return Some(Task::Prune(p));
            }
            if let Some(req) = st.queued.take() {
                let cancel = Arc::new(AtomicBool::new(false));
                st.running = Some(Running {
                    source: req.source.clone(),
                    key: req.key,
                    cancel: Arc::clone(&cancel),
                    transcoding: false,
                });
                return Some(Task::Copy(req, cancel));
            }
            st = self.wake.wait(st).ok()?;
        }
    }

    fn work(&self) {
        while let Some(task) = self.next_task() {
            match task {
                Task::Prune(p) => {
                    let report = prune(&self.cache, &p.store, p.outside.as_deref(), &self.stopping);
                    debug!(
                        "media: pruned {} file(s) from the cache",
                        report.removed.len()
                    );
                }
                Task::Copy(req, cancel) => {
                    let result = self.make_copy(&req, &cancel);
                    let Ok(mut st) = self.state.lock() else {
                        return;
                    };
                    st.running = None;
                    if cancel.load(Ordering::Relaxed) {
                        debug!(
                            "media: the playback copy of {} was cancelled",
                            req.source.display()
                        );
                        continue;
                    }
                    let state = match result {
                        Ok(path) => {
                            info!(
                                "media: the playback copy of {} is ready: {}",
                                req.source.display(),
                                path.display()
                            );
                            CopyState::Ready(path)
                        }
                        Err(reason) => {
                            warning!(
                                "media: the playback copy of {} failed: {reason}",
                                req.source.display()
                            );
                            st.failed.insert(
                                req.key,
                                Failure {
                                    source: req.source.clone(),
                                    reason: reason.clone(),
                                    retried: req.retried,
                                },
                            );
                            CopyState::Failed(reason)
                        }
                    };
                    drop(st);
                    let _ = self.events.send(CopyEvent {
                        source: req.source,
                        state,
                    });
                }
            }
        }
    }

    /// Marks the running job as transcoding and says so (unless it was
    /// cancelled meanwhile).
    fn transcoding(&self, req: &Request, cancel: &AtomicBool) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if let Some(r) = st.running.as_mut() {
            r.transcoding = true;
        }
        drop(st);
        let _ = self.events.send(CopyEvent {
            source: req.source.clone(),
            state: CopyState::Preparing,
        });
    }

    /// One job: check, hash, and transcode `req.source` into its copy. A
    /// reason is what failed (`Warning` puts it after "preparing it for
    /// playback on the CPU failed").
    fn make_copy(&self, req: &Request, cancel: &AtomicBool) -> Result<PathBuf, String> {
        let ffmpeg = self.current_ffmpeg().map_err(|e| e.to_string())?;
        let shown = req.source.display();
        let mut file = probe::open_video(&req.source)?;
        let h = hash::hash_file(&mut file, MAX_VIDEO_BYTES, Some(cancel))
            .map_err(|e| format!("the video file {shown}: {e}"))?;
        let target = self.cache.copy_path(&h);
        if cache::has_magic(&target, EBML_MAGIC) {
            return Ok(target);
        }
        cache::remove_logged(&target);
        let info = probe::probe_open(&file).map_err(|e| format!("the video file {shown}: {e}"))?;
        if is_cpu_ready(&info) {
            return Ok(req.source.clone());
        }
        self.transcoding(req, cancel);
        let dir = self.cache.playback_dir();
        cache::create_dir(&dir)?;
        let pending = PendingFile::new_in(&dir, cache::temp_prefix());
        let job = FfmpegJob {
            input: &file,
            input_format: info.container.ffmpeg_format(),
            output_args: ffmpeg::vp9_args(),
            output: &pending.path,
            timeout: ffmpeg::copy_timeout(info.duration_ms),
        };
        ffmpeg.run(&job, cancel).map_err(|e| e.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if !cache::has_magic(&pending.path, EBML_MAGIC) {
            return Err("ffmpeg made no WebM".into());
        }
        // The source removed or changed meanwhile: the copy is of nothing.
        let same = std::fs::metadata(&req.source).is_ok_and(|m| FileKey::of(&m) == req.key);
        if !same {
            return Err(format!(
                "the video file {shown} changed while it was copied"
            ));
        }
        pending.commit(&target)?;
        Ok(target)
    }
}

/// The content hashes of every video of `store` (both kinds, both roots);
/// `stop` set abandons the reads.
fn gallery_hashes(store: &MediaStore, stop: &AtomicBool) -> HashSet<String> {
    let mut keep = HashSet::new();
    for kind in super::MediaKind::ALL {
        for path in store.video_paths(kind) {
            if stop.load(Ordering::Relaxed) {
                return keep;
            }
            if let Some(h) = hash_of(&path, stop) {
                keep.insert(h);
            }
        }
    }
    keep
}

fn hash_of(path: &Path, stop: &AtomicBool) -> Option<String> {
    let mut f = probe::open_video(path).ok()?;
    hash::hash_file(&mut f, MAX_VIDEO_BYTES, Some(stop)).ok()
}

/// The prune; `stop` set (the service dropped) abandons it before it
/// removes anything, since what to keep is then not known in full.
pub(super) fn prune(
    cache: &MediaCache,
    store: &MediaStore,
    outside: Option<&Path>,
    stop: &AtomicBool,
) -> PruneReport {
    let mut keep = gallery_hashes(store, stop);
    if let Some(h) = outside.and_then(|p| hash_of(p, stop)) {
        keep.insert(h);
    }
    if stop.load(Ordering::Relaxed) {
        return PruneReport::default();
    }
    let mut removed = Vec::new();
    let playback = cache.playback_dir();
    removed.extend(cache::remove_stale_parts(&playback));
    let mut copies = cache::copies_in(&playback);
    // The newest unreferenced copy of the current settings, kept when no
    // outside file is named.
    let spare = if outside.is_none() {
        copies
            .iter()
            .filter(|(_, h, v)| *v == ENCODE_SETTINGS_VERSION && !keep.contains(h))
            .max_by_key(|(p, _, _)| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .map(|(_, h, _)| h.clone())
    } else {
        None
    };
    if let Some(h) = &spare {
        keep.insert(h.clone());
    }
    copies.retain(|(_, h, v)| *v != ENCODE_SETTINGS_VERSION || !keep.contains(h));
    for (path, _, _) in copies {
        if cache::remove_logged(&path) {
            removed.push(path);
        }
    }
    let posters = cache.posters_dir();
    removed.extend(cache::remove_stale_parts(&posters));
    for (path, h) in cache::posters_in(&posters) {
        if !keep.contains(&h) && cache::remove_logged(&path) {
            removed.push(path);
        }
    }
    PruneReport { removed }
}
