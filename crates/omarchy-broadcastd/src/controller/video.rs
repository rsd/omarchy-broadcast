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
//! original (NVDEC) and never asks for a playback copy. A system-memory
//! session plays the original when it is CPU-ready
//! ([`crate::media::is_cpu_ready`]), else its playback copy
//! ([`VideoService::ensure_copy`]): until the check is known, while the
//! copy is looked for or prepared, or when it cannot be made, only the
//! still is shown, and `Warning` says why ([`copy_clause`]). With no still
//! at all the original is given all the same, and the element's own clause
//! says it cannot play (black behind the person, D4). A copy job that
//! failed runs once more at the next camera start, never in a loop.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use crate::media::{self, CopyEvent, CopyState, MediaStore, PlaybackCopies, VideoInfo};
use crate::warning;

use super::Command;

/// What the controller asks of video backgrounds. The daemon's is
/// [`GalleryVideos`].
pub trait VideoService: Send {
    /// Asks for the check and the poster of `path`, made off the
    /// controller thread and answered by [`Command::VideoPoster`] (the
    /// poster `None` when there is nowhere to keep one or it cannot be
    /// made).
    fn request_poster(&mut self, path: &Path);

    /// The state of the playback copy of `path`, starting its job when
    /// there is none ([`PlaybackCopies::ensure`]; with `retry`, a job that
    /// failed runs once more, [`PlaybackCopies::retry`]); never blocks.
    /// The steps of a job are [`Command::PlaybackCopy`]s.
    fn ensure_copy(&mut self, path: &Path, retry: bool) -> CopyState;

    /// Cancels the copy job running or queued, if any.
    fn cancel_copy(&mut self);

    /// Asks for the cache's orphans to be pruned, keeping what `outside`
    /// (a video outside the gallery the settings name) needs; for the
    /// daemon's start.
    fn prune(&mut self, outside: Option<&Path>);
}

/// No video service (until the daemon gives the controller its own): no
/// check, no poster, no playback copy.
pub struct NoGallery;

/// Why there is no playback copy without a cache.
pub const NO_CACHE: &str = "there is no cache directory ($XDG_CACHE_HOME or $HOME)";

impl VideoService for NoGallery {
    fn request_poster(&mut self, _: &Path) {}
    fn ensure_copy(&mut self, _: &Path, _: bool) -> CopyState {
        CopyState::Failed(NO_CACHE.into())
    }
    fn cancel_copy(&mut self) {}
    fn prune(&mut self, _: Option<&Path>) {}
}

/// The daemon's video backgrounds: the gallery's store (its cache for
/// posters) and its playback copies, answering through the controller's
/// command channel.
pub struct GalleryVideos {
    store: Option<MediaStore>,
    copies: Option<PlaybackCopies>,
    commands: Sender<Command>,
}

impl GalleryVideos {
    /// The service of `store` (none: no gallery), sending its answers to
    /// `commands`: the playback copy service of its cache is started, and a
    /// thread forwards each [`CopyEvent`] as a [`Command::PlaybackCopy`]
    /// (it ends with the service).
    pub fn new(store: Option<MediaStore>, commands: Sender<Command>) -> GalleryVideos {
        let copies = store
            .as_ref()
            .and_then(PlaybackCopies::for_store)
            .and_then(|(c, rx)| {
                let tx = commands.clone();
                std::thread::Builder::new()
                    .name("obc-copy-events".into())
                    .spawn(move || {
                        for event in rx {
                            if tx.send(Command::PlaybackCopy(event)).is_err() {
                                break;
                            }
                        }
                    })
                    .map_err(|e| warning!("media: the playback copy events are lost: {e}"))
                    .ok()
                    .map(|_| c)
            });
        GalleryVideos {
            store,
            copies,
            commands,
        }
    }
}

impl VideoService for GalleryVideos {
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

    fn ensure_copy(&mut self, path: &Path, retry: bool) -> CopyState {
        match &self.copies {
            Some(c) if retry => c.retry(path),
            Some(c) => c.ensure(path),
            None => CopyState::Failed(NO_CACHE.into()),
        }
    }

    fn cancel_copy(&mut self) {
        if let Some(c) = &self.copies {
            c.cancel();
        }
    }

    fn prune(&mut self, outside: Option<&Path>) {
        if let (Some(c), Some(s)) = (&self.copies, &self.store) {
            c.request_prune(s, outside);
        }
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
    /// In a system-memory session, for a video that is not CPU-ready: the
    /// state of its playback copy (`None`: not asked yet).
    pub copy: Option<CopyState>,
}

impl VideoChoice {
    /// Whether a system-memory session plays it only through a playback
    /// copy (checked, and not CPU-ready).
    pub fn needs_copy(&self) -> bool {
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
    /// original when CPU-ready and else the copy once ready (nothing while
    /// it is not, but when there is no still at all: the original then,
    /// which the element says it cannot play).
    pub fn playing(&self, system: bool) -> Option<PathBuf> {
        let play = if !system {
            Some(self.source.clone())
        } else if !self.checked {
            None
        } else if !self.needs_copy() {
            Some(self.source.clone())
        } else {
            match &self.copy {
                Some(CopyState::Ready(p)) => Some(p.clone()),
                _ => None,
            }
        };
        play.or_else(|| self.still().is_none().then(|| self.source.clone()))
    }

    /// Takes a step of a copy job: whether it was this video's and
    /// changed its state.
    pub fn copy_step(&mut self, event: &CopyEvent) -> bool {
        if event.source != self.source || self.copy.as_ref() == Some(&event.state) {
            return false;
        }
        self.copy = Some(event.state.clone());
        true
    }
}

/// The `Warning` clause of a copy that is not ready, with a still shown
/// meanwhile (`still`) or none; `None` once it is, and while an existing
/// copy is looked for (a moment). Without a still, a copy that cannot be
/// made has no clause: the element's says why nothing plays.
pub fn copy_clause(copy: &CopyState, still: bool) -> Option<String> {
    match copy {
        CopyState::Ready(_) | CopyState::Checking => None,
        CopyState::Preparing if still => Some(
            "The video background is being prepared for playback on the CPU; a still is shown \
             until it is ready"
                .into(),
        ),
        CopyState::Preparing => Some(
            "The video background is being prepared for playback on the CPU; the background is \
             black until it is ready"
                .into(),
        ),
        CopyState::NoFfmpeg if still => {
            Some("The video background is shown as a still: ffmpeg is not installed".into())
        }
        CopyState::Failed(why) if still => Some(format!(
            "The video background is shown as a still: preparing it for playback on the CPU \
             failed ({why})"
        )),
        _ => None,
    }
}
