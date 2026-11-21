//! Video backgrounds of `replace` (ADR-0035, "CUDA and system memory"): what
//! the controller asks of a video chosen as the background, and what it
//! keeps of one.
//!
//! A background whose content is a video ([`is_video_file`]) becomes a
//! [`VideoChoice`]. The controller thread never runs the video check (it
//! can take seconds): `SetBackgroundImage` hands it the check made on the
//! D-Bus side, a check made earlier in the run is looked up by the file's
//! identity ([`media::known_video_info`]), and any other (a configured
//! file at a camera start) is made on the poster's thread and comes with
//! the poster ([`Command::VideoPoster`]). The effect element is then given
//! two things: the video to play (`EffectRun::background_video`) and its
//! load-time still (`EffectRun::background_image`): the video's poster,
//! made off the controller thread ([`VideoService::request_poster`]), and
//! until it exists the still in force when the video was chosen (dropped
//! at a camera start, and when its file is gone: black behind the person
//! then).
//!
//! What plays depends on the session's memory. A CUDA session plays the
//! original (NVDEC). A system-memory session plays the original when it is
//! CPU-ready ([`crate::media::is_cpu_ready`]); until the check is known,
//! and for any other video, only the still is shown. The daemon converts
//! nothing: `Warning` says the video is shown as a still and points at
//! `omarchy-broadcast media convert-command`, which prints the command
//! that converts it into the user's gallery ([`still_clause`]; the
//! command itself is `GetConvertCommand`'s).
//! With no still at all the original is given all the same, and the
//! element's own clause says it cannot play (black behind the person, D4).

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::media::{self, MediaStore, VideoInfo};
use crate::{debug, warning};

use super::Command;

/// What the controller asks of video backgrounds. The daemon's is
/// [`GalleryVideos`].
pub trait VideoService: Send {
    /// Asks for the check and the poster of `path`, made off the
    /// controller thread and answered by [`Command::VideoPoster`] (the
    /// poster `None` when there is nowhere to keep one or it cannot be
    /// made).
    fn request_poster(&mut self, path: &Path);

    /// Asks for the cache to be pruned ([`media::cache::prune`]: the
    /// playback copies of earlier versions, and the posters no gallery
    /// video needs), keeping what `outside` (a video outside the gallery
    /// the settings name) needs; for the daemon's start.
    fn prune(&mut self, outside: Option<&Path>);

    /// The background the `replace` effect starts with when none is set:
    /// the first bundled background, a still before a video
    /// ([`MediaStore::default_background`]); `None` when there is none.
    fn default_background(&self) -> Option<PathBuf> {
        None
    }
}

/// No video service (until the daemon gives the controller its own): no
/// check, no poster, no gallery.
pub struct NoGallery;

impl VideoService for NoGallery {
    fn request_poster(&mut self, _: &Path) {}
    fn prune(&mut self, _: Option<&Path>) {}
}

/// The daemon's video backgrounds: the gallery's store (its cache for
/// posters), answering through the controller's command channel.
/// Dropping it stops a prune in progress (between two reads of a file)
/// and joins its thread.
pub struct GalleryVideos {
    store: Option<MediaStore>,
    commands: Sender<Command>,
    prune: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl GalleryVideos {
    /// The service of `store` (none: no gallery), sending its answers to
    /// `commands`.
    pub fn new(store: Option<MediaStore>, commands: Sender<Command>) -> GalleryVideos {
        GalleryVideos {
            store,
            commands,
            prune: None,
        }
    }
}

impl Drop for GalleryVideos {
    fn drop(&mut self) {
        if let Some((stop, thread)) = self.prune.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
        }
    }
}

impl VideoService for GalleryVideos {
    fn default_background(&self) -> Option<PathBuf> {
        self.store.as_ref().and_then(MediaStore::default_background)
    }

    fn request_poster(&mut self, path: &Path) {
        let store = self.store.clone();
        let tx = self.commands.clone();
        let source = path.to_path_buf();
        let _ = std::thread::Builder::new()
            .name("obc-poster".into())
            .spawn(move || {
                let (info, poster) = match media::video_poster(store.as_ref(), &source) {
                    Ok((info, poster)) => (Ok(info), poster),
                    Err(e) => (Err(e), None),
                };
                let _ = tx.send(Command::VideoPoster {
                    source,
                    info,
                    poster,
                });
            })
            .map_err(|e| warning!("media: the poster thread did not start: {e}"));
    }

    fn prune(&mut self, outside: Option<&Path>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let Some(cache) = store.cache().cloned() else {
            return;
        };
        if self.prune.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let outside = outside.map(Path::to_path_buf);
        self.prune = std::thread::Builder::new()
            .name("obc-cache-prune".into())
            .spawn(move || {
                let report = media::cache::prune(&cache, &store, outside.as_deref(), &flag);
                debug!(
                    "media: the cache prune removed {} files",
                    report.removed.len()
                );
            })
            .map_err(|e| warning!("media: the cache prune did not start: {e}"))
            .ok()
            .map(|t| (stop, t));
    }
}

/// Whether the file at `path` is a video by its content (the first bytes
/// of an MP4, MOV, MKV or WebM file), as the effect element tells it.
pub fn is_video_file(path: &Path) -> bool {
    gst_obc::video_background::check_video_file(path).is_ok()
}

/// A video chosen as the background, and what is known of it.
#[derive(Debug, Clone)]
pub struct VideoChoice {
    /// The file chosen, as named.
    pub source: PathBuf,
    /// Its check; `None` until it is known (`checked`), or when the check
    /// refused it (a configured file: the element is given it all the
    /// same and says why it cannot play).
    pub info: Option<VideoInfo>,
    /// Whether the check is known (`info` then says what it found).
    pub checked: bool,
    /// Its poster, once known.
    pub poster: Option<PathBuf>,
    /// The still in force when it was chosen: shown until the poster
    /// exists.
    pub still_before: Option<PathBuf>,
}

impl VideoChoice {
    /// Whether a system-memory session shows it only as a still (checked,
    /// and not CPU-ready).
    pub fn not_cpu_ready(&self) -> bool {
        self.info.as_ref().is_some_and(|i| !media::is_cpu_ready(i))
    }

    /// The still the element is given: the poster, else the still in
    /// force when it was chosen.
    pub fn still(&self) -> Option<PathBuf> {
        self.poster.clone().or_else(|| self.still_before.clone())
    }

    /// Forgets the still in force when it was chosen once its file is
    /// gone (a `stat`).
    pub fn drop_gone_still(&mut self) {
        if self.still_before.as_ref().is_some_and(|p| !p.is_file()) {
            self.still_before = None;
        }
    }

    /// What plays in a session of `system` memory (else CUDA): the
    /// original, or in system memory, once the check is known, the
    /// original when CPU-ready (nothing otherwise, but when there is no
    /// still at all: the original then, which the element says it cannot
    /// play).
    pub fn playing(&self, system: bool) -> Option<PathBuf> {
        let play =
            (!system || (self.checked && !self.not_cpu_ready())).then(|| self.source.clone());
        play.or_else(|| self.still().is_none().then(|| self.source.clone()))
    }
}

/// The command `Warning` points at for the conversion of a video.
pub const CONVERT_COMMAND: &str = "omarchy-broadcast media convert-command";

/// The `Warning` clause of a video a system-memory session does not play
/// (not CPU-ready), with a still shown (`still`) or none, pointing at
/// [`CONVERT_COMMAND`] for the command that converts it, in its two forms:
/// naming the video `source` single-quoted as a shell word (POSIX: a `'`
/// is written `'\''`; `None` when the path is not UTF-8), and naming
/// `<file>`, for when the path would not fit in `Warning` or not show
/// unchanged (a path is never cut).
pub fn still_clause(source: &Path, still: bool) -> (Option<String>, String) {
    let start = if still {
        "The video background is shown as a still: it can't play without an NVIDIA GPU"
    } else {
        "The video background plays only with an NVIDIA GPU"
    };
    let clause =
        |file: &str| format!("{start}; run {CONVERT_COMMAND} {file} for the conversion command");
    (
        source.to_str().map(|p| clause(&media::ffmpeg::quote(p))),
        clause("<file>"),
    )
}
