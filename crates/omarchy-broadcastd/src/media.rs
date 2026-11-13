//! The media gallery: the background and away images the daemon offers,
//! bundled with the package or imported by the user.
//!
//! The gallery has two roots, each with one directory per [`MediaKind`]
//! (`backgrounds`, `away`):
//!
//! * the **bundled** root, read only and packaged
//!   (`/usr/share/omarchy-broadcast/media`), whose kind directories carry a
//!   `SOURCES.toml` naming the title, author, licence and source of every
//!   file;
//! * the **user** root (`$XDG_DATA_HOME/omarchy-broadcast/media`, else
//!   `$HOME/.local/share/omarchy-broadcast/media`), written only by the
//!   daemon through [`MediaStore::import`] and [`MediaStore::remove`].
//!
//! Listing ([`MediaStore::list`]) reads only the regular files directly in
//! a kind directory, with an accepted extension; the size of an image is
//! read from its header (a read that starts small and grows up to the file
//! size limit, never a decode), and a file whose header cannot be read, or
//! whose size is outside the dimension limit, is skipped: the same header
//! checks an import makes. A symbolic link in the user root is never
//! followed. Labels are made from the file stem, sanitised as stored names
//! are.
//!
//! Importing copies a file into the user root after the checks the
//! `replace` effect makes on a background (`gst_obc::background`): a
//! bounded read of a regular file, the format from the content's signature
//! (never from the extension), the size from the header within the
//! dimension limit, a complete first image, and a decoder for the format
//! (WebP needs the optional `webpdec`). The stored name is the source's
//! name, sanitised (control and Unicode format characters removed), with
//! the extension of the detected format; an image file already in the
//! gallery with the same bytes is returned instead of being copied again,
//! and an existing name is never overwritten (a numeric suffix is added).
//! The copy is written to a temporary file in the same directory, synced,
//! linked to its final name, and the directory synced, so a reader never
//! sees a partial file. A temporary file left by an interrupted import is
//! removed by a later import once it is ten minutes old. Imports take no
//! lock: two imports of the same file running at the same time may both
//! store a copy.
//!
//! Videos ([`MediaType::Video`]) are recognised from their content and
//! checked with GStreamer without decoding ([`probe::probe_video`]: MP4,
//! M4V, MOV, MKV or WebM holding H.264, HEVC, VP9 or AV1, at most 512 MiB
//! and 3840x2160, 8-bit 4:2:0; audio is ignored). A background video is
//! stored as it is, with the extension of its container, and found again
//! by its content hash (SHA-256) instead of its bytes. An animated GIF,
//! WebP or PNG is transcoded at import with ffmpeg into a CPU-ready VP9
//! WebM ([`ffmpeg::vp9_args`]), which is the stored item; the source's
//! hash is recorded in the kind directory's hidden `.sources/` so the same
//! source is not stored twice; without ffmpeg it is refused. Away images
//! are stills only. A poster (the first frame as a JPEG) is made for every
//! video at import in the cache ([`cache::MediaCache`]). Listing a video
//! reads its size and duration with the same check (remembered per file
//! for the daemon's run, [`check_video`]) and finds its poster, never
//! decoding: a bundled video ships its poster beside it
//! (`<stem>.poster.jpg`, a derived file, not an item), copied into the
//! cache the first time it is listed; a video with no poster yet gets one
//! from [`posters::PosterMaker`], off the listing. Removing a video also
//! removes its poster and playback copies ([`playback::PlaybackCopies`])
//! and cancels a copy job for it.

pub mod cache;
pub mod ffmpeg;
mod hash;
pub mod playback;
pub mod posters;
pub mod probe;

pub use cache::MediaCache;
pub use playback::{CopyEvent, CopyState, PlaybackCopies, PruneReport, is_cpu_ready};
pub use posters::PosterMaker;
pub use probe::{Codec, Container, VideoInfo, probe_video};

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use gst_obc::background::{self, MAX_FILE_BYTES};
use serde::Deserialize;

use self::ffmpeg::{Ffmpeg, FfmpegJob};
use self::hash::FileKey;
use self::probe::MAX_VIDEO_BYTES;
use crate::{debug, warning};

/// The bundled root of the packaged gallery.
pub const BUNDLED_ROOT: &str = "/usr/share/omarchy-broadcast/media";

/// The name of the file describing the bundled items of a kind directory.
pub const SOURCES_FILE: &str = "SOURCES.toml";

/// The extensions of the image files listed (lower case).
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp"];

/// The extensions of the video files listed (lower case).
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm"];

/// The end of the name of a bundled video's shipped poster
/// (`<stem>.poster.jpg` beside `<stem>.<ext>`): a derived file, never
/// listed as an item.
pub const SHIPPED_POSTER_SUFFIX: &str = ".poster.jpg";

/// The longest stored file stem, in bytes (before the suffix and the
/// extension).
pub const MAX_STEM_BYTES: usize = 96;

/// The most bytes read from a file to find its image header when listing:
/// the whole of the largest file an import accepts, so listing and import
/// agree on which files are images.
const MAX_HEADER_BYTES: usize = MAX_FILE_BYTES as usize;

/// The first read when looking for an image header; doubled until the
/// header is found or [`MAX_HEADER_BYTES`] is reached.
const FIRST_HEADER_READ: usize = 64 * 1024;

/// The largest `SOURCES.toml` read.
const MAX_SOURCES_BYTES: u64 = 1024 * 1024;

/// The highest numeric suffix tried for a free name (`name-2` onwards).
const MAX_SUFFIX: u32 = 10_000;

/// The age past which a temporary file of an import is taken as left by an
/// interrupted one, and removed.
const STALE_TEMP_AGE: Duration = Duration::from_secs(10 * 60);

/// The label of a file whose stem sanitises to nothing.
const UNTITLED: &str = "Untitled";

/// What a gallery item is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// A background of the `replace` effect.
    Background,
    /// An image shown while the camera is away.
    Away,
}

impl MediaKind {
    /// Every kind, in a fixed order.
    pub const ALL: [MediaKind; 2] = [MediaKind::Background, MediaKind::Away];

    /// The kind's name on the wire: `background` or `away`.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Background => "background",
            MediaKind::Away => "away",
        }
    }

    /// The kind's directory under a gallery root: `backgrounds` or `away`.
    pub fn dir_name(self) -> &'static str {
        match self {
            MediaKind::Background => "backgrounds",
            MediaKind::Away => "away",
        }
    }

    /// The name a stored file gets when its source's name sanitises to
    /// nothing.
    fn fallback_stem(self) -> &'static str {
        self.as_str()
    }
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MediaKind {
    type Err = MediaError;

    /// Parses `background` or `away`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "background" => Ok(MediaKind::Background),
            "away" => Ok(MediaKind::Away),
            other => Err(MediaError::UnknownKind(other.to_string())),
        }
    }
}

/// Whether a gallery item is a still image or a video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaType {
    /// A still image (PNG, JPEG or WebP).
    Image,
    /// A video (a background; also the transcode of an animated image).
    Video,
}

impl MediaType {
    /// The type's name on the wire: `image` or `video`.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaType::Image => "image",
            MediaType::Video => "video",
        }
    }

    /// The type a file name's extension (any case) stands for, if it is
    /// one the gallery lists.
    pub fn from_path(path: &Path) -> Option<MediaType> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            Some(MediaType::Image)
        } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
            Some(MediaType::Video)
        } else {
            None
        }
    }
}

/// One file of the gallery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItem {
    /// The file's absolute path.
    pub path: PathBuf,
    /// The name shown for it: the bundled title, else one made from the
    /// file's stem.
    pub label: String,
    /// Whether it is part of the packaged gallery (read only).
    pub bundled: bool,
    /// Whether it is a still image or a video.
    pub media_type: MediaType,
    /// The width in pixels, from the image's header or the video's check.
    pub width: u32,
    /// The height in pixels, as [`MediaItem::width`].
    pub height: u32,
    /// The duration in milliseconds; 0 for a still image or when unknown.
    pub duration_ms: u64,
    /// The poster image of a video (a JPEG in the cache), when there is
    /// one; `None` for a still.
    pub poster: Option<PathBuf>,
}

/// Why a gallery operation was refused or failed. Every message names the
/// path concerned and the reason.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// Neither `XDG_DATA_HOME` nor `HOME` is an absolute path.
    #[error("neither XDG_DATA_HOME nor HOME is set: the user's media gallery has no directory")]
    NoDataDir,
    /// A kind name other than `background` or `away`.
    #[error("unknown media kind {0:?}: expected \"background\" or \"away\"")]
    UnknownKind(String),
    /// A relative path where an absolute one is required.
    #[error("{} is not an absolute path", .0.display())]
    NotAbsolute(PathBuf),
    /// A video or animated image given to import as an away image.
    #[error(
        "cannot import {}: an away image must be a still image (PNG, JPEG or WebP)",
        .0.display()
    )]
    VideoUnsupported(PathBuf),
    /// The source is neither an accepted image nor an accepted video, or a
    /// video the check refuses.
    #[error("cannot import {}: {reason}", path.display())]
    NotAVideo {
        /// The source file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// An animated image, which needs ffmpeg to be transcoded.
    #[error(
        "cannot import {}: an animated image needs ffmpeg, which is not installed",
        .0.display()
    )]
    NeedsFfmpeg(PathBuf),
    /// The transcode of an animated image failed.
    #[error("cannot import {}: converting the animation to a video failed: {reason}", path.display())]
    Transcode {
        /// The source file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// The source file could not be read under the checks (its reason
    /// names the path).
    #[error("cannot import: {0}")]
    Unreadable(String),
    /// The source's content is not an accepted image.
    #[error("cannot import {}: {reason}", path.display())]
    NotAnImage {
        /// The source file.
        path: PathBuf,
        /// What is wrong with its content.
        reason: String,
    },
    /// The image's format has no decoder installed.
    #[error("cannot import {}: {reason}", path.display())]
    NoDecoder {
        /// The source file.
        path: PathBuf,
        /// Which decoder is missing.
        reason: String,
    },
    /// Every candidate name up to the highest suffix is taken.
    #[error("cannot import {}: no free name left in {}", src.display(), dir.display())]
    NoFreeName {
        /// The source file.
        src: PathBuf,
        /// The kind directory of the user root.
        dir: PathBuf,
    },
    /// A file system operation of the gallery failed.
    #[error("cannot {action} {}: {source}", path.display())]
    Io {
        /// What was being done (`create`, `write`, `remove`, …).
        action: &'static str,
        /// The path concerned.
        path: PathBuf,
        /// The error.
        source: std::io::Error,
    },
    /// A path to remove that is not a regular file directly in a kind
    /// directory of the user root.
    #[error("{} is not an item of the user's media gallery", .0.display())]
    NotAUserItem(PathBuf),
}

/// The entries of a `SOURCES.toml`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Sources {
    /// The `[[item]]` tables, in file order.
    #[serde(default)]
    pub(crate) item: Vec<SourceEntry>,
}

/// One `[[item]]` of a `SOURCES.toml`: the provenance of a bundled file.
/// The daemon reads only `file` and `title`; the rest is checked by the
/// bundled-assets test.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct SourceEntry {
    /// The file's name in the kind directory.
    pub(crate) file: Option<String>,
    /// The title shown for it.
    pub(crate) title: Option<String>,
    /// Who made it.
    pub(crate) author: Option<String>,
    /// Its SPDX licence expression.
    pub(crate) licence: Option<String>,
    /// Where it comes from (a URL, or the script generating it).
    pub(crate) source: Option<String>,
    /// When it was retrieved.
    pub(crate) retrieved: Option<String>,
    /// Any edit made to the original.
    pub(crate) edits: Option<String>,
}

/// The gallery's two roots, and the cache of files derived from its
/// videos (posters, playback copies) when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStore {
    bundled_root: PathBuf,
    user_root: PathBuf,
    cache: Option<MediaCache>,
}

/// How [`video_facts`] treats posters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Posters {
    /// Look up, and make a missing one with ffmpeg.
    Make,
    /// Look up only (a shipped poster is copied into the cache).
    Find,
}

impl MediaStore {
    /// A store over `bundled_root` (read only) and `user_root` (read and
    /// write), each holding one directory per [`MediaKind`], without a
    /// cache (no posters; see [`MediaStore::with_cache`]).
    pub fn new(bundled_root: impl Into<PathBuf>, user_root: impl Into<PathBuf>) -> Self {
        MediaStore {
            bundled_root: bundled_root.into(),
            user_root: user_root.into(),
            cache: None,
        }
    }

    /// The store with its derived files in `cache`.
    pub fn with_cache(mut self, cache: MediaCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// The store of this session: [`BUNDLED_ROOT`], and
    /// `$XDG_DATA_HOME/omarchy-broadcast/media`, else
    /// `$HOME/.local/share/omarchy-broadcast/media`; the cache in
    /// `$XDG_CACHE_HOME/omarchy-broadcast`, else
    /// `$HOME/.cache/omarchy-broadcast`.
    pub fn from_env() -> Result<Self, MediaError> {
        Self::from_env_in(|k| std::env::var_os(k))
    }

    /// [`MediaStore::from_env`] with the environment looked up through
    /// `env`. A variable holding a relative path is ignored, as the XDG
    /// specification says.
    pub fn from_env_in(env: impl Fn(&str) -> Option<OsString>) -> Result<Self, MediaError> {
        let absolute = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
        let data = absolute("XDG_DATA_HOME")
            .or_else(|| absolute("HOME").map(|h| h.join(".local/share")))
            .ok_or(MediaError::NoDataDir)?;
        let cache =
            absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|h| h.join(".cache")));
        let store = Self::new(BUNDLED_ROOT, data.join("omarchy-broadcast").join("media"));
        Ok(match cache {
            Some(c) => store.with_cache(MediaCache::new(c.join("omarchy-broadcast"))),
            None => store,
        })
    }

    /// The bundled root.
    pub fn bundled_root(&self) -> &Path {
        &self.bundled_root
    }

    /// The user root.
    pub fn user_root(&self) -> &Path {
        &self.user_root
    }

    /// The cache of derived files, if the store has one.
    pub fn cache(&self) -> Option<&MediaCache> {
        self.cache.as_ref()
    }

    /// The directory of `kind` in the bundled root.
    pub fn bundled_dir(&self, kind: MediaKind) -> PathBuf {
        self.bundled_root.join(kind.dir_name())
    }

    /// The directory of `kind` in the user root.
    pub fn user_dir(&self, kind: MediaKind) -> PathBuf {
        self.user_root.join(kind.dir_name())
    }

    /// The items of `kind`: the bundled ones first (in `SOURCES.toml`
    /// order, then the rest by name), then the user's (by name, ignoring
    /// case). A missing directory lists nothing; a file whose header cannot
    /// be read, or a video the check refuses, is skipped and logged. A
    /// video gets its size and duration from the check (remembered per
    /// file: the first listing of a new video takes a moment), and its
    /// poster when the cache has one; a bundled video's shipped poster is
    /// copied into the cache first. No poster is made here (nothing is
    /// decoded): [`PosterMaker`] makes the missing ones.
    pub fn list(&self, kind: MediaKind) -> Vec<MediaItem> {
        let mut items = self.list_bundled(kind);
        items.extend(self.list_user(kind));
        items
    }

    /// The video files of `kind` in both roots (by their name: no check,
    /// no read), for a prune.
    pub(crate) fn video_paths(&self, kind: MediaKind) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (dir, follow) in [(self.bundled_dir(kind), true), (self.user_dir(kind), false)] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_name().to_string_lossy().starts_with('.')
                    || MediaType::from_path(&path) != Some(MediaType::Video)
                {
                    continue;
                }
                let meta = if follow {
                    std::fs::metadata(&path)
                } else {
                    std::fs::symlink_metadata(&path)
                };
                if meta.is_ok_and(|m| m.is_file()) {
                    out.push(path);
                }
            }
        }
        out
    }

    fn list_bundled(&self, kind: MediaKind) -> Vec<MediaItem> {
        let dir = self.bundled_dir(kind);
        let sources = read_sources(&dir);
        let mut found = scan_dir(&dir, true, self.cache.as_ref());
        // SOURCES.toml order first, then by name.
        let rank = |name: &str| {
            sources
                .item
                .iter()
                .position(|e| e.file.as_deref() == Some(name))
                .unwrap_or(usize::MAX)
        };
        found.sort_by(|a, b| {
            (rank(&a.0), a.0.to_lowercase(), &a.0).cmp(&(rank(&b.0), b.0.to_lowercase(), &b.0))
        });
        found
            .into_iter()
            .map(|(name, mut item)| {
                if let Some(title) = sources
                    .item
                    .iter()
                    .find(|e| e.file.as_deref() == Some(name.as_str()))
                    .and_then(|e| e.title.as_deref())
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    item.label = title.to_string();
                }
                item
            })
            .collect()
    }

    fn list_user(&self, kind: MediaKind) -> Vec<MediaItem> {
        let mut found = scan_dir(&self.user_dir(kind), false, self.cache.as_ref());
        found.sort_by(|a, b| (a.0.to_lowercase(), &a.0).cmp(&(b.0.to_lowercase(), &b.0)));
        found.into_iter().map(|(_, item)| item).collect()
    }

    /// Copies the image or video `src` (an absolute path) into the user
    /// directory of `kind` and returns the stored file's absolute path;
    /// when a file of that directory already has the same content, returns
    /// its path without copying. What it is comes from the content, never
    /// the extension:
    ///
    /// * a still image is checked as a background is: refused when
    ///   [`background::read_image_file`] refuses it (not a regular file,
    ///   over [`MAX_FILE_BYTES`]), when the content is not an accepted image
    ///   or its first image is incomplete, for sizes outside
    ///   [`background::MAX_DIMENSION`], and when the format's decoder is not
    ///   installed;
    /// * an animated GIF, WebP or PNG (a background only) is transcoded
    ///   with ffmpeg into a CPU-ready VP9 WebM, which is stored; refused
    ///   without ffmpeg or when the transcode fails;
    /// * a video (a background only) is checked by [`probe::probe_video`]
    ///   and stored as it is, with the extension of its container.
    ///
    /// A video gets its poster in the cache (a failure there is logged,
    /// not a refusal). Never overwrites a file.
    pub fn import(&self, kind: MediaKind, src: &Path) -> Result<PathBuf, MediaError> {
        self.import_stored(kind, src).map(|(path, _)| path)
    }

    /// [`MediaStore::import`], also saying whether a new file was stored
    /// (`false`: an identical file of the gallery was returned).
    pub fn import_stored(
        &self,
        kind: MediaKind,
        src: &Path,
    ) -> Result<(PathBuf, bool), MediaError> {
        if !src.is_absolute() {
            return Err(MediaError::NotAbsolute(src.to_path_buf()));
        }
        // Named as the background checks name an image; a background may
        // be a video too.
        let subject = match kind {
            MediaKind::Away => "the image file",
            MediaKind::Background => "the file",
        };
        let (mut file, _) = open_limited(src, true, MAX_VIDEO_BYTES).map_err(|reason| {
            let reason = reason.strip_prefix("it ").unwrap_or(&reason);
            MediaError::Unreadable(format!("{subject} {} {reason}", src.display()))
        })?;
        let mut head = [0u8; 16];
        let head_len = read_head(&mut file, &mut head).map_err(|e| {
            MediaError::Unreadable(format!("{subject} {} is unreadable: {e}", src.display()))
        })?;
        let head = &head[..head_len];
        if is_image_signature(head) {
            let data = background::read_image_file(src).map_err(MediaError::Unreadable)?;
            if let Some(anim) = animated_format(&data) {
                if kind != MediaKind::Background {
                    return Err(MediaError::VideoUnsupported(src.to_path_buf()));
                }
                return self.import_animated(kind, src, &data, &file, anim);
            }
            return self.import_still(kind, src, &data);
        }
        if !is_container_signature(head) {
            return Err(MediaError::NotAVideo {
                path: src.to_path_buf(),
                reason: "it is not an accepted image (PNG, JPEG, WebP; GIF and APNG as \
                         animations) or video (MP4, M4V, MOV, MKV, WebM)"
                    .into(),
            });
        }
        if kind != MediaKind::Background {
            return Err(MediaError::VideoUnsupported(src.to_path_buf()));
        }
        self.import_video(kind, src, file)
    }

    fn import_still(
        &self,
        kind: MediaKind,
        src: &Path,
        data: &[u8],
    ) -> Result<(PathBuf, bool), MediaError> {
        let not_image = |reason: String| MediaError::NotAnImage {
            path: src.to_path_buf(),
            reason,
        };
        let (format, width, height) = background::image_header(data).map_err(not_image)?;
        background::check_dimensions("it", width, height).map_err(not_image)?;
        background::image_length(format, data).map_err(not_image)?;
        background::decoder_available(format).map_err(|reason| MediaError::NoDecoder {
            path: src.to_path_buf(),
            reason,
        })?;
        let ext = format.extension();

        let dir = self.prepare_dir(kind)?;
        if let Some(existing) = find_identical(&dir, data) {
            debug!(
                "media: {} is already in the gallery as {}",
                src.display(),
                existing.display()
            );
            return Ok((existing, false));
        }
        let temp = TempFile::create(&dir, data)?;
        let target = link_unique(&dir, &temp.path, &self.stem_of(kind, src), ext, src)?;
        Ok((target, true))
    }

    /// The user directory of `kind`, created, with stale temporary files of
    /// interrupted imports removed.
    fn prepare_dir(&self, kind: MediaKind) -> Result<PathBuf, MediaError> {
        let dir = self.user_dir(kind);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|source| MediaError::Io {
                action: "create",
                path: dir.clone(),
                source,
            })?;
        remove_stale_temps(&dir);
        Ok(dir)
    }

    fn stem_of(&self, kind: MediaKind, src: &Path) -> String {
        sanitise_stem(
            &src.file_stem().unwrap_or_default().to_string_lossy(),
            kind.fallback_stem(),
        )
    }

    /// Stores the video `file` (opened from `src`) as it is.
    fn import_video(
        &self,
        kind: MediaKind,
        src: &Path,
        mut file: File,
    ) -> Result<(PathBuf, bool), MediaError> {
        let refused = |reason: String| MediaError::NotAVideo {
            path: src.to_path_buf(),
            reason,
        };
        let info = probe::probe_open(&file).map_err(refused)?;
        let dir = self.prepare_dir(kind)?;
        let rewind = |file: &mut File| {
            file.rewind().map_err(|source| MediaError::Io {
                action: "read",
                path: src.to_path_buf(),
                source,
            })
        };
        // The duplicate check first, from the source's hash: an item
        // already there costs no copy.
        rewind(&mut file)?;
        let source_hash = hash::hash_file(&mut file, MAX_VIDEO_BYTES, None).map_err(|e| {
            let e = e.strip_prefix("it ").unwrap_or(&e);
            MediaError::Unreadable(format!("the file {} {e}", src.display()))
        })?;
        if let Some(existing) = find_identical_video(&dir, &source_hash) {
            debug!(
                "media: {} is already in the gallery as {}",
                src.display(),
                existing.display()
            );
            return Ok((existing, false));
        }
        rewind(&mut file)?;
        // The copy's own hash: the source may have changed since.
        let (temp, content_hash) = TempFile::copy_from(&dir, &mut file, MAX_VIDEO_BYTES)?;
        let target = link_unique(
            &dir,
            &temp.path,
            &self.stem_of(kind, src),
            info.container.extension(),
            src,
        )?;
        drop(temp);
        if let Ok(meta) = std::fs::symlink_metadata(&target) {
            hash::remember(FileKey::of(&meta), &content_hash);
        }
        remember_info(&target, &info);
        self.make_poster(&target, &info, &content_hash);
        Ok((target, true))
    }

    /// Transcodes the animated image `data` (read from `src`, opened as
    /// `file`) into a CPU-ready WebM and stores it.
    fn import_animated(
        &self,
        kind: MediaKind,
        src: &Path,
        data: &[u8],
        file: &File,
        anim: Animated,
    ) -> Result<(PathBuf, bool), MediaError> {
        let dir = self.prepare_dir(kind)?;
        let source_hash = hash::to_hex(&hash::sha256(data));
        if let Some(existing) = transcoded_from(&dir, &source_hash) {
            debug!(
                "media: {} is already in the gallery as {}",
                src.display(),
                existing.display()
            );
            return Ok((existing, false));
        }
        let ffmpeg = Ffmpeg::locate().map_err(|_| MediaError::NeedsFfmpeg(src.to_path_buf()))?;
        let failed = |reason: String| MediaError::Transcode {
            path: src.to_path_buf(),
            reason,
        };
        let pending = cache::PendingFile::new_in(&dir, ANIMATED_TEMP_PREFIX);
        let job = FfmpegJob {
            input: file,
            input_format: anim.ffmpeg_format(),
            output_args: ffmpeg::vp9_args(),
            output: &pending.path,
            timeout: ffmpeg::ANIMATED_TIMEOUT,
        };
        ffmpeg
            .run(&job, &AtomicBool::new(false))
            .map_err(|e| failed(e.to_string()))?;
        let info = probe::probe_video(&pending.path).map_err(failed)?;
        if !is_cpu_ready(&info) {
            return Err(failed(format!(
                "ffmpeg made a {} {} video of {}x{}, not a CPU-ready VP9 WebM",
                info.container.as_str(),
                info.codec.as_str(),
                info.width,
                info.height
            )));
        }
        cache::sync_file(&pending.path).map_err(failed)?;
        let target = link_unique(&dir, &pending.path, &self.stem_of(kind, src), "webm", src)?;
        drop(pending);
        record_transcode(&dir, &source_hash, &target);
        remember_info(&target, &info);
        if let Ok(mut f) = probe::open_video(&target)
            && let Ok(h) = hash::hash_file(&mut f, MAX_VIDEO_BYTES, None)
        {
            self.make_poster(&target, &info, &h);
        }
        Ok((target, true))
    }

    /// Makes the poster of the stored video `path`; a failure is logged.
    fn make_poster(&self, path: &Path, info: &VideoInfo, content_hash: &str) {
        let Some(cache) = &self.cache else {
            return;
        };
        let Ok(ffmpeg) = Ffmpeg::locate() else {
            debug!(
                "media: no poster for {}: ffmpeg is not installed",
                path.display()
            );
            return;
        };
        let made = probe::open_video(path)
            .and_then(|f| cache::ensure_poster(cache, &ffmpeg, &f, info, content_hash));
        if let Err(e) = made {
            warning!("media: no poster for {}: {e}", path.display());
        }
    }

    /// Reads the content hash of the gallery video `path` now, so a later
    /// [`MediaStore::remove`] of it (on the controller thread) finds it
    /// remembered instead of reading the file. Nothing for other files.
    pub fn warm_hash(&self, path: &Path) {
        if MediaType::from_path(path) == Some(MediaType::Video)
            && let Ok((mut f, _)) = open_limited(path, false, MAX_VIDEO_BYTES)
        {
            let _ = hash::hash_file(&mut f, MAX_VIDEO_BYTES, None);
        }
    }

    /// Removes `path`, which must be a regular file (not a symbolic link)
    /// directly in a kind directory of the user root. A bundled item, or
    /// any other path, is [`MediaError::NotAUserItem`]. The file removed is
    /// the one checked: its name in the canonical kind directory, so a
    /// directory link in `path` swapped meanwhile cannot redirect it.
    /// For a video, a playback copy job for it is cancelled and its poster
    /// and playback copies are removed from the cache (its content hash is
    /// read before the removal: remembered when the file was listed or
    /// imported in this run, see [`MediaStore::warm_hash`]; a failure there
    /// is logged, the files then go at the next prune).
    /// Whether the item is in use is the caller's concern.
    pub fn remove(&self, path: &Path) -> Result<(), MediaError> {
        if !path.is_absolute() {
            return Err(MediaError::NotAbsolute(path.to_path_buf()));
        }
        let not_user = || MediaError::NotAUserItem(path.to_path_buf());
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(not_user());
        };
        let checked = std::fs::canonicalize(parent)
            .map_err(|_| not_user())?
            .join(name);
        match self.is_gallery_item(&checked) {
            Some((_, false)) => {}
            _ => return Err(not_user()),
        }
        let video = MediaType::from_path(&checked) == Some(MediaType::Video);
        let content_hash = if video {
            playback::forget_everywhere(path);
            playback::forget_everywhere(&checked);
            open_limited(&checked, false, MAX_VIDEO_BYTES)
                .and_then(|(mut f, _)| hash::hash_file(&mut f, MAX_VIDEO_BYTES, None))
                .inspect_err(|e| {
                    warning!(
                        "media: the derived files of {} stay until the next prune: {e}",
                        checked.display()
                    )
                })
                .ok()
        } else {
            None
        };
        std::fs::remove_file(&checked).map_err(|source| MediaError::Io {
            action: "remove",
            path: path.to_path_buf(),
            source,
        })?;
        if video {
            if let Some(dir) = checked.parent() {
                forget_transcode(dir, &checked);
            }
            if let (Some(cache), Some(h)) = (&self.cache, content_hash) {
                cache.remove_derived(&h);
            }
        }
        Ok(())
    }

    /// The kind of `path` and whether it is bundled, when it is a regular
    /// file directly in a kind directory of either root (the directories
    /// compared canonically). A file in the user root must not be a
    /// symbolic link; a bundled one may be.
    pub fn is_gallery_item(&self, path: &Path) -> Option<(MediaKind, bool)> {
        let parent = path.parent()?;
        path.file_name()?;
        let parent = std::fs::canonicalize(parent).ok()?;
        for kind in MediaKind::ALL {
            for (dir, bundled) in [(self.bundled_dir(kind), true), (self.user_dir(kind), false)] {
                let Ok(dir) = std::fs::canonicalize(&dir) else {
                    continue;
                };
                if dir != parent {
                    continue;
                }
                let meta = if bundled {
                    std::fs::metadata(path)
                } else {
                    std::fs::symlink_metadata(path)
                };
                return meta.ok().filter(Metadata::is_file).map(|_| (kind, bundled));
            }
        }
        None
    }
}

impl MediaError {
    /// The D-Bus error of a gallery operation: the store's refusals are
    /// `InvalidArgument`, a gallery that cannot be used or written
    /// `Failed`; the message is the store's.
    pub fn to_ipc(self) -> obc_ipc::IpcError {
        use obc_ipc::IpcError;
        match self {
            MediaError::NoDataDir | MediaError::Io { .. } | MediaError::NoFreeName { .. } => {
                IpcError::Failed(self.to_string())
            }
            MediaError::UnknownKind(_)
            | MediaError::NotAbsolute(_)
            | MediaError::VideoUnsupported(_)
            | MediaError::NotAVideo { .. }
            | MediaError::NeedsFfmpeg(_)
            | MediaError::Transcode { .. }
            | MediaError::Unreadable(_)
            | MediaError::NotAnImage { .. }
            | MediaError::NoDecoder { .. }
            | MediaError::NotAUserItem(_) => IpcError::InvalidArgument(self.to_string()),
        }
    }
}

/// Whether `a` and `b` name the same file: the same text, or the same
/// canonical path.
pub fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// A file name stem made safe to store: whitespace runs (line breaks and
/// tabs included) collapsed to one space, other control characters, format
/// characters (Unicode category Cf: soft hyphen, zero-width and
/// bidirectional formatting characters, byte order mark, …) and `/`
/// removed, leading dots and surrounding whitespace stripped, then cut to
/// [`MAX_STEM_BYTES`] on a character boundary; `fallback` when nothing is
/// left.
pub fn sanitise_stem(stem: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(stem.len().min(MAX_STEM_BYTES));
    let mut space = false;
    for c in stem.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if c.is_control() || is_format_char(c) || c == '/' {
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        if out.is_empty() && c == '.' {
            continue;
        }
        out.push(c);
    }
    if out.len() > MAX_STEM_BYTES {
        let mut cut = MAX_STEM_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    let out = out.trim_end();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out.to_string()
    }
}

/// The format characters (Unicode general category Cf, as of Unicode 15):
/// invisible, they can hide part of a name or, for the bidirectional ones
/// (marks, embeddings, overrides and isolates), make it display other than
/// it reads.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// A label made from a file stem: sanitised as a stored stem is
/// ([`sanitise_stem`]), dashes and underscores become spaces, whitespace is
/// collapsed, and the first letter is upper case (`warm-bokeh` gives `Warm
/// bokeh`). A stem with nothing left is labelled `Untitled`.
pub fn label_from_stem(stem: &str) -> String {
    let clean = sanitise_stem(stem, UNTITLED);
    let words = clean.replace(['-', '_'], " ");
    let words: Vec<&str> = words.split_whitespace().collect();
    let joined = words.join(" ");
    let mut chars = joined.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => clean,
    }
}

/// The `SOURCES.toml` of a bundled kind directory; empty when there is
/// none or it cannot be read (logged).
fn read_sources(dir: &Path) -> Sources {
    let path = dir.join(SOURCES_FILE);
    let text = match File::open(&path) {
        Ok(f) => {
            let mut text = String::new();
            if let Err(e) = f.take(MAX_SOURCES_BYTES).read_to_string(&mut text) {
                warning!("media: {} is unreadable: {e}", path.display());
                return Sources::default();
            }
            text
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Sources::default(),
        Err(e) => {
            warning!("media: {} is unreadable: {e}", path.display());
            return Sources::default();
        }
    };
    toml::from_str(&text).unwrap_or_else(|e| {
        warning!("media: {} is not valid: {e}", path.display());
        Sources::default()
    })
}

/// The listable files directly in `dir`, each with its file name. Hidden
/// files (a leading dot, such as an import in progress) are skipped;
/// symbolic links are followed only when `follow` (the bundled root, whose
/// shipped posters, `<stem>.poster.jpg`, are not items but the posters of
/// its videos).
fn scan_dir(dir: &Path, follow: bool, cache: Option<&MediaCache>) -> Vec<(String, MediaItem)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warning!("media: cannot list {}: {e}", dir.display());
            return Vec::new();
        }
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            debug!("media: skipping {}: not a UTF-8 name", path.display());
            continue;
        };
        if name.starts_with('.') || (follow && is_shipped_poster(&name)) {
            continue;
        }
        let Some(media_type) = MediaType::from_path(&path) else {
            continue;
        };
        let stem = Path::new(&name)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let (width, height, duration_ms, poster) = match media_type {
            MediaType::Image => match image_size(&path, follow) {
                Ok((w, h)) => (w, h, 0, None),
                Err(reason) => {
                    debug!("media: skipping {}: {reason}", path.display());
                    continue;
                }
            },
            MediaType::Video => match video_facts(
                &path,
                follow,
                cache,
                Posters::Find,
                follow
                    .then(|| shipped_poster_of(&path))
                    .flatten()
                    .as_deref(),
            ) {
                Ok((info, poster)) => (info.width, info.height, info.duration_ms, poster),
                Err(reason) => {
                    debug!("media: skipping {}: {reason}", path.display());
                    continue;
                }
            },
        };
        found.push((
            name,
            MediaItem {
                path,
                label: label_from_stem(&stem),
                bundled: follow,
                media_type,
                width,
                height,
                duration_ms,
                poster,
            },
        ));
    }
    found
}

/// Opens `path` for reading without blocking (and, unless `follow`,
/// without following a symbolic link), and checks on the handle that it is
/// a regular file of at most [`MAX_FILE_BYTES`].
fn open_regular(path: &Path, follow: bool) -> Result<(File, Metadata), String> {
    open_limited(path, follow, MAX_FILE_BYTES)
}

/// [`open_regular`] with the size limit `limit`.
fn open_limited(path: &Path, follow: bool, limit: u64) -> Result<(File, Metadata), String> {
    let mut flags = libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
    if !follow {
        flags |= libc::O_NOFOLLOW;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(|e| format!("it is unreadable: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("it is unreadable: {e}"))?;
    if !meta.is_file() {
        return Err("it is not a regular file".into());
    }
    if meta.len() > limit {
        return Err(format!(
            "it is {} bytes, more than the {} MiB limit",
            meta.len(),
            limit / (1024 * 1024)
        ));
    }
    Ok((file, meta))
}

/// The size of the image at `path`, read from its header within the
/// first [`MAX_HEADER_BYTES`] (read from [`FIRST_HEADER_READ`] up, doubling,
/// so the common case stays cheap) and checked against the dimension limit.
fn image_size(path: &Path, follow: bool) -> Result<(u32, u32), String> {
    let (file, meta) = open_regular(path, follow)?;
    let total = usize::try_from(meta.len()).unwrap_or(usize::MAX);
    let mut reader = file.take(MAX_HEADER_BYTES as u64);
    let mut data = Vec::new();
    let mut want = FIRST_HEADER_READ;
    loop {
        let need = want.saturating_sub(data.len()) as u64;
        (&mut reader)
            .take(need)
            .read_to_end(&mut data)
            .map_err(|e| format!("it is unreadable: {e}"))?;
        match background::image_header(&data) {
            Ok((_, w, h)) => {
                background::check_dimensions("it", w, h)?;
                return Ok((w, h));
            }
            Err(e) if data.len() >= total.min(MAX_HEADER_BYTES) || data.len() < want => {
                return Err(e);
            }
            Err(_) => want *= 2,
        }
    }
}

/// A regular file directly in `dir`, listed as an image (not hidden, with
/// an image extension), whose content is `data`, if any.
fn find_identical(dir: &Path, data: &[u8]) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| MediaType::from_path(p) == Some(MediaType::Image))
        .collect();
    names.sort();
    names.into_iter().find(|path| {
        let Ok((file, meta)) = open_regular(path, false) else {
            return false;
        };
        if meta.len() != data.len() as u64 {
            return false;
        }
        let mut content = Vec::with_capacity(data.len());
        file.take(data.len() as u64 + 1)
            .read_to_end(&mut content)
            .is_ok()
            && content == data
    })
}

/// The prefix of the temporary output of an animated image's transcode
/// (an `.import-*.tmp` name, so an interrupted one is cleaned up as an
/// interrupted copy is).
const ANIMATED_TEMP_PREFIX: &str = ".import-anim-";

/// The hidden directory of a kind directory recording, per source hash,
/// the file an animated image was transcoded into.
const TRANSCODED_DIR: &str = ".sources";

/// An animated image format, transcoded at import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Animated {
    /// GIF (every GIF, a single frame included).
    Gif,
    /// APNG: a PNG with an `acTL` chunk before its image data.
    Apng,
    /// An animated WebP (the `VP8X` animation flag, or an `ANIM`/`ANMF`
    /// chunk).
    Webp,
}

impl Animated {
    /// The ffmpeg demuxer forced for it.
    pub(crate) fn ffmpeg_format(self) -> &'static str {
        match self {
            Animated::Gif => "gif",
            Animated::Apng => "apng",
            Animated::Webp => "webp_anim",
        }
    }
}

/// Reads up to `buf.len()` bytes from the start of `file`.
fn read_head(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Whether `head` starts like an image the gallery takes (PNG, JPEG,
/// WebP, GIF).
fn is_image_signature(head: &[u8]) -> bool {
    head.starts_with(b"\x89PNG\r\n\x1a\n")
        || head.starts_with(&[0xFF, 0xD8, 0xFF])
        || (head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP")
        || head.starts_with(b"GIF87a")
        || head.starts_with(b"GIF89a")
}

/// Whether `head` starts like an ISO base media / QuickTime file (a box
/// type the format starts with at offset 4) or an EBML (Matroska, WebM)
/// one. The check proper is GStreamer's typefind; this only tells a video
/// from a file that is neither image nor video, for the reason given.
fn is_container_signature(head: &[u8]) -> bool {
    head.starts_with(cache::EBML_MAGIC)
        || (head.len() >= 8
            && matches!(
                &head[4..8],
                b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot"
            ))
}

/// The animated format of the image `data`, if it is one.
pub(crate) fn animated_format(data: &[u8]) -> Option<Animated> {
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some(Animated::Gif);
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        // Chunks: length (4, big endian), type (4), data, CRC (4).
        let mut at = 8usize;
        while at + 8 <= data.len() {
            let len = u32::from_be_bytes(data[at..at + 4].try_into().ok()?) as usize;
            match &data[at + 4..at + 8] {
                b"acTL" => return Some(Animated::Apng),
                b"IDAT" | b"IEND" => return None,
                _ => {}
            }
            at = at.checked_add(12)?.checked_add(len)?;
        }
        return None;
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        // Chunks: FourCC (4), size (4, little endian), data padded to even.
        let mut at = 12usize;
        while at + 8 <= data.len() {
            let kind = &data[at..at + 4];
            let len = u32::from_le_bytes(data[at + 4..at + 8].try_into().ok()?) as usize;
            match kind {
                b"VP8X" if data.get(at + 8).is_some_and(|f| f & 0x02 != 0) => {
                    return Some(Animated::Webp);
                }
                b"ANIM" | b"ANMF" => return Some(Animated::Webp),
                _ => {}
            }
            at = at.checked_add(8)?.checked_add(len)?.checked_add(len % 2)?;
        }
    }
    None
}

/// Links the complete file `temp` in `dir` to the first free name of
/// `<stem>.<ext>`, `<stem>-2.<ext>`, …, then syncs the directory (the
/// name is freed again when that fails). Never overwrites.
fn link_unique(
    dir: &Path,
    temp: &Path,
    stem: &str,
    ext: &str,
    src: &Path,
) -> Result<PathBuf, MediaError> {
    for n in 1..=MAX_SUFFIX {
        let name = if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        let target = dir.join(&name);
        match std::fs::hard_link(temp, &target) {
            Ok(()) => {
                if let Err(source) = sync_dir(dir) {
                    // The import fails as a whole: the name is freed
                    // again (the temporary file goes with its guard).
                    let _ = std::fs::remove_file(&target);
                    return Err(MediaError::Io {
                        action: "sync",
                        path: dir.to_path_buf(),
                        source,
                    });
                }
                return Ok(target);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(MediaError::Io {
                    action: "write",
                    path: target,
                    source,
                });
            }
        }
    }
    Err(MediaError::NoFreeName {
        src: src.to_path_buf(),
        dir: dir.to_path_buf(),
    })
}

/// A regular video file directly in `dir` (not hidden, with a video
/// extension, not a link) whose content hash is `content_hash`, if any.
fn find_identical_video(dir: &Path, content_hash: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| MediaType::from_path(p) == Some(MediaType::Video))
        .collect();
    names.sort();
    names.into_iter().find(|path| {
        open_limited(path, false, MAX_VIDEO_BYTES)
            .ok()
            .and_then(|(mut f, _)| hash::hash_file(&mut f, MAX_VIDEO_BYTES, None).ok())
            .is_some_and(|h| h == content_hash)
    })
}

/// The stored transcode of the animated source of hash `source_hash` in
/// `dir`, when its record names a regular file there.
fn transcoded_from(dir: &Path, source_hash: &str) -> Option<PathBuf> {
    let record = dir.join(TRANSCODED_DIR).join(source_hash);
    let (file, _) = open_limited(&record, false, 4096).ok()?;
    let mut name = String::new();
    file.take(4096).read_to_string(&mut name).ok()?;
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return None;
    }
    let target = dir.join(name);
    std::fs::symlink_metadata(&target)
        .ok()
        .filter(Metadata::is_file)
        .map(|_| target)
}

/// Records that the animated source of hash `source_hash` was stored as
/// `target` (in `dir`); a failure is logged (the next import of the same
/// source then stores another copy).
fn record_transcode(dir: &Path, source_hash: &str, target: &Path) {
    let records = dir.join(TRANSCODED_DIR);
    let Some(name) = target.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let written = cache::create_dir(&records).and_then(|()| {
        let pending = cache::PendingFile::new_in(&records, cache::temp_prefix());
        std::fs::write(&pending.path, name)
            .map_err(|e| format!("cannot write {}: {e}", pending.path.display()))?;
        pending.commit(&records.join(source_hash))
    });
    if let Err(e) = written {
        warning!(
            "media: cannot record the source of {}: {e}",
            target.display()
        );
    }
}

/// Removes the records of transcodes stored as `removed` (in `dir`).
fn forget_transcode(dir: &Path, removed: &Path) {
    let Some(name) = removed.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let records = dir.join(TRANSCODED_DIR);
    let Ok(entries) = std::fs::read_dir(&records) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let names_it = open_limited(&path, false, 4096)
            .ok()
            .and_then(|(f, _)| {
                let mut text = String::new();
                f.take(4096).read_to_string(&mut text).ok().map(|_| text)
            })
            .is_some_and(|t| t.trim() == name);
        if names_it {
            cache::remove_logged(&path);
        }
    }
}

/// The checks of listed videos, remembered per file for the daemon's run.
fn infos() -> &'static Mutex<HashMap<FileKey, VideoInfo>> {
    static INFOS: OnceLock<Mutex<HashMap<FileKey, VideoInfo>>> = OnceLock::new();
    INFOS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remembers `info` as the check of the file at `path`.
fn remember_info(path: &Path, info: &VideoInfo) {
    if let (Ok(meta), Ok(mut m)) = (std::fs::symlink_metadata(path), infos().lock()) {
        if m.len() >= 4096 {
            m.clear();
        }
        m.insert(FileKey::of(&meta), info.clone());
    }
}

/// Whether `name` is a shipped poster's ([`SHIPPED_POSTER_SUFFIX`]).
fn is_shipped_poster(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(SHIPPED_POSTER_SUFFIX)
}

/// The shipped poster beside the bundled video `path`, if there is one.
fn shipped_poster_of(path: &Path) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_str()?;
    let poster = path.with_file_name(format!("{stem}{SHIPPED_POSTER_SUFFIX}"));
    poster.is_file().then_some(poster)
}

/// The check of the video at `path` (in or outside the gallery, a
/// symbolic link followed) and its poster in `store`'s cache: the existing
/// one, a bundled video's shipped one, else one made now with ffmpeg;
/// `None` when it cannot be made (no cache, no ffmpeg, a failed job:
/// logged). Probes and hashes the file and may run ffmpeg: never on the
/// controller thread. The reason when the video's check refuses it.
pub fn video_poster(
    store: Option<&MediaStore>,
    path: &Path,
) -> Result<(VideoInfo, Option<PathBuf>), String> {
    let shipped = store
        .filter(|s| matches!(s.is_gallery_item(path), Some((_, true))))
        .and_then(|_| shipped_poster_of(path));
    video_facts(
        path,
        true,
        store.and_then(MediaStore::cache),
        Posters::Make,
        shipped.as_deref(),
    )
    .map_err(|e| format!("the video file {}: {e}", path.display()))
}

/// The check of the video at `path` ([`probe::probe_video`]), remembered
/// per file for the daemon's run, so a second check of the same file
/// (the listing, a choice of it, a camera start) reads nothing. Never on
/// the controller thread.
pub fn check_video(path: &Path) -> Result<VideoInfo, String> {
    let shown = path.display();
    let (file, meta) = open_limited(path, true, MAX_VIDEO_BYTES)
        .map_err(|e| format!("the video file {shown}: {e}"))?;
    let key = FileKey::of(&meta);
    if let Some(i) = infos().lock().ok().and_then(|m| m.get(&key).cloned()) {
        return Ok(i);
    }
    let info = probe::probe_open(&file).map_err(|e| format!("the video file {shown}: {e}"))?;
    remember_key(key, &info);
    Ok(info)
}

/// The check of the video at `path` when it was made in this run for the
/// file as it is now ([`check_video`]): a `stat` and a lookup, for the
/// controller thread.
pub fn known_video_info(path: &Path) -> Option<VideoInfo> {
    let key = FileKey::of(&std::fs::metadata(path).ok()?);
    infos().lock().ok()?.get(&key).cloned()
}

/// Remembers `info` as the check of the file at `path` as it is now (a
/// symbolic link followed): what a caller that checked it elsewhere hands
/// on (a `stat`, for the controller thread).
pub fn remember_video_info(path: &Path, info: &VideoInfo) {
    if let Ok(meta) = std::fs::metadata(path) {
        remember_key(FileKey::of(&meta), info);
    }
}

fn remember_key(key: FileKey, info: &VideoInfo) {
    if let Ok(mut m) = infos().lock() {
        if m.len() >= 4096 {
            m.clear();
        }
        m.insert(key, info.clone());
    }
}

/// When `path` starts like a video (MP4, MOV, MKV or WebM) but is larger
/// than a video may be: the reason, naming the video limit. A `stat` and
/// the first bytes.
pub fn oversized_video(path: &Path) -> Option<String> {
    let (mut file, meta) = open_limited(path, true, u64::MAX).ok()?;
    if meta.len() <= MAX_VIDEO_BYTES {
        return None;
    }
    let mut head = [0u8; 16];
    let n = read_head(&mut file, &mut head).ok()?;
    is_container_signature(&head[..n]).then(|| {
        format!(
            "the video file {} is {} bytes, more than the {} MiB limit",
            path.display(),
            meta.len(),
            MAX_VIDEO_BYTES / (1024 * 1024)
        )
    })
}

/// The check of the video at `path` (remembered per file) and its poster:
/// the cached one, else `shipped` (a bundled video's poster) copied into
/// the cache, else, with [`Posters::Make`] and ffmpeg, one made now.
fn video_facts(
    path: &Path,
    follow: bool,
    cache: Option<&MediaCache>,
    posters: Posters,
    shipped: Option<&Path>,
) -> Result<(VideoInfo, Option<PathBuf>), String> {
    let (mut file, meta) = open_limited(path, follow, MAX_VIDEO_BYTES)?;
    let key = FileKey::of(&meta);
    let known = infos().lock().ok().and_then(|m| m.get(&key).cloned());
    let info = match known {
        Some(i) => i,
        None => {
            let i = probe::probe_open(&file)?;
            if let Ok(mut m) = infos().lock() {
                if m.len() >= 4096 {
                    m.clear();
                }
                m.insert(key, i.clone());
            }
            i
        }
    };
    let Some(cache) = cache else {
        return Ok((info, None));
    };
    let content_hash = match hash::hash_file(&mut file, MAX_VIDEO_BYTES, None) {
        Ok(h) => h,
        Err(e) => {
            debug!("media: no poster for {}: {e}", path.display());
            return Ok((info, None));
        }
    };
    let poster = cache.poster_path(&content_hash);
    if cache::has_magic(&poster, cache::JPEG_MAGIC) {
        return Ok((info, Some(poster)));
    }
    if let Some(shipped) = shipped {
        match cache::install_poster(cache, shipped, &content_hash) {
            Ok(p) => return Ok((info, Some(p))),
            Err(e) => warning!("media: the shipped poster of {}: {e}", path.display()),
        }
    }
    if posters == Posters::Find {
        return Ok((info, None));
    }
    let Ok(ffmpeg) = Ffmpeg::locate() else {
        return Ok((info, None));
    };
    match cache::ensure_poster(cache, &ffmpeg, &file, &info, &content_hash) {
        Ok(p) => Ok((info, Some(p))),
        Err(e) => {
            warning!("media: no poster for {}: {e}", path.display());
            Ok((info, None))
        }
    }
}

/// Syncs the directory `dir`, so a name just linked in it survives a crash.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(dir)?
        .sync_all()
}

/// Removes the temporary files of imports (`.import-*.tmp`) in `dir` older
/// than [`STALE_TEMP_AGE`]: left by an import that was interrupted (the
/// daemon killed mid-copy). A younger one may belong to an import running
/// now, and is kept. Failures are logged.
fn remove_stale_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(".import-") && name.ends_with(".tmp")) {
            continue;
        }
        let path = entry.path();
        let stale = std::fs::symlink_metadata(&path)
            .ok()
            .filter(Metadata::is_file)
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > STALE_TEMP_AGE);
        if !stale {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => debug!("media: removed the stale {}", path.display()),
            Err(e) => warning!("media: cannot remove {}: {e}", path.display()),
        }
    }
}

/// A synced temporary copy in the destination directory, unlinked when
/// dropped (its content then lives on under the name it was linked to).
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn create(dir: &Path, data: &[u8]) -> Result<TempFile, MediaError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let io = |action, path: &Path| {
            let path = path.to_path_buf();
            move |source| MediaError::Io {
                action,
                path,
                source,
            }
        };
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(".import-{}-{n}.tmp", std::process::id()));
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&path)
            {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(MediaError::Io {
                        action: "create",
                        path,
                        source,
                    });
                }
            };
            let temp = TempFile { path };
            file.write_all(data).map_err(io("write", &temp.path))?;
            file.sync_all().map_err(io("write", &temp.path))?;
            return Ok(temp);
        }
    }
}

impl TempFile {
    /// A synced temporary copy in `dir` of what `reader` gives (at most
    /// `limit` bytes; more is a refusal), with its content hash.
    fn copy_from(
        dir: &Path,
        reader: &mut impl Read,
        limit: u64,
    ) -> Result<(TempFile, String), MediaError> {
        let temp = TempFile::create(dir, &[])?;
        let io = |action, source| MediaError::Io {
            action,
            path: temp.path.clone(),
            source,
        };
        let mut out = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temp.path)
            .map_err(|e| io("write", e))?;
        let mut sha = hash::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let n = reader.read(&mut buf).map_err(|e| io("read", e))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > limit {
                return Err(io(
                    "copy",
                    std::io::Error::other(format!("the source grew past {limit} bytes")),
                ));
            }
            sha.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(|e| io("write", e))?;
        }
        out.sync_all().map_err(|e| io("write", e))?;
        Ok((temp, hash::to_hex(&sha.finish())))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            warning!("media: cannot remove {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod video_tests;
