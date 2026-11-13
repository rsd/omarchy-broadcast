//! The daemon's cache of files derived from gallery videos:
//! `$XDG_CACHE_HOME/omarchy-broadcast/` (else `$HOME/.cache/...`) with
//!
//! * `posters/<content hash>.jpg`: the first frame of a video, made by
//!   ffmpeg at import or by the poster maker for a listed video that has
//!   none ([`super::posters::PosterMaker`]), or copied from a bundled
//!   video's shipped poster ([`install_poster`]);
//! * `playback/<content hash>-v<settings version>.webm`: a CPU-ready VP9
//!   copy of a video for playback in system memory
//!   ([`super::playback::PlaybackCopies`]).
//!
//! The content hash is the SHA-256 of the source file, so the same bytes
//! share their derived files wherever they live, and an edited file gets
//! new ones. Every file is written to a hidden temporary file in its
//! directory, synced, renamed to its name and the directory synced, so a
//! name always holds a complete file. Everything here can be deleted at
//! any time: it is rebuilt when needed.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::ffmpeg::{self, ENCODE_SETTINGS_VERSION, Ffmpeg, FfmpegJob};
use super::probe::VideoInfo;
use crate::{debug, warning};

/// The age past which a temporary file of this process is taken as left by
/// an interrupted job, and removed by a prune.
const STALE_TEMP_AGE: Duration = Duration::from_secs(10 * 60);

/// The age past which a temporary file of another process is removed by a
/// prune: that process may be a daemon of another session, or one starting
/// while the last one ends, still writing it.
const FOREIGN_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// The prefix of the cache's temporary files.
const TEMP_PREFIX: &str = ".part-";

/// The cache's directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaCache {
    root: PathBuf,
}

impl MediaCache {
    /// A cache rooted at `root` (normally
    /// `$XDG_CACHE_HOME/omarchy-broadcast`).
    pub fn new(root: impl Into<PathBuf>) -> MediaCache {
        MediaCache { root: root.into() }
    }

    /// The root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory of posters.
    pub fn posters_dir(&self) -> PathBuf {
        self.root.join("posters")
    }

    /// The directory of playback copies.
    pub fn playback_dir(&self) -> PathBuf {
        self.root.join("playback")
    }

    /// The poster of the content `hash`.
    pub fn poster_path(&self, hash: &str) -> PathBuf {
        self.posters_dir().join(format!("{hash}.jpg"))
    }

    /// The playback copy of the content `hash` under the current encode
    /// settings ([`ENCODE_SETTINGS_VERSION`]).
    pub fn copy_path(&self, hash: &str) -> PathBuf {
        self.playback_dir()
            .join(format!("{hash}-v{ENCODE_SETTINGS_VERSION}.webm"))
    }

    /// Removes the poster and every playback copy (any settings version)
    /// of the content `hash`; returns what was removed.
    pub fn remove_derived(&self, hash: &str) -> Vec<PathBuf> {
        let mut removed = Vec::new();
        let poster = self.poster_path(hash);
        if remove_logged(&poster) {
            removed.push(poster);
        }
        for (path, h, _) in copies_in(&self.playback_dir()) {
            if h == hash && remove_logged(&path) {
                removed.push(path);
            }
        }
        removed
    }
}

/// The playback copies in `dir`: each path with its hash and settings
/// version (`<hash>-v<n>.webm`); other names are left out.
pub(crate) fn copies_in(dir: &Path) -> Vec<(PathBuf, String, u32)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".webm")?;
            let (hash, version) = stem.split_once("-v")?;
            let version: u32 = version.parse().ok()?;
            super::hash::is_hash(hash).then(|| (e.path(), hash.to_string(), version))
        })
        .collect()
}

/// The posters in `dir`: each path with its hash.
pub(crate) fn posters_in(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let hash = name.strip_suffix(".jpg")?;
            super::hash::is_hash(hash).then(|| (e.path(), hash.to_string()))
        })
        .collect()
}

/// Removes `path`, logging a failure other than its absence; whether it
/// was removed.
pub(crate) fn remove_logged(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => {
            debug!("media: removed {}", path.display());
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warning!("media: cannot remove {}: {e}", path.display());
            false
        }
    }
}

/// Creates `dir` (and its parents) private to the user.
pub(crate) fn create_dir(dir: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// A path in `dir` for a temporary file that does not exist yet, removed
/// when dropped unless it was [`PendingFile::commit`]ted.
pub(crate) struct PendingFile {
    pub(crate) path: PathBuf,
    done: bool,
}

impl PendingFile {
    /// A fresh name in `dir`, starting with `prefix`
    /// (`.part-<pid>-<n>.tmp` for the cache).
    pub(crate) fn new_in(dir: &Path, prefix: &str) -> PendingFile {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{prefix}{}-{n}.tmp", std::process::id()));
            if std::fs::symlink_metadata(&path).is_err() {
                return PendingFile { path, done: false };
            }
        }
    }

    /// Syncs the written file and renames it to `target` (replacing it),
    /// then syncs the directory.
    pub(crate) fn commit(mut self, target: &Path) -> Result<(), String> {
        sync_file(&self.path)?;
        std::fs::rename(&self.path, target)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
        self.done = true;
        if let Some(dir) = target.parent() {
            sync_dir(dir).map_err(|e| format!("cannot sync {}: {e}", dir.display()))?;
        }
        Ok(())
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Syncs the regular file at `path` (opened without following a link).
pub(crate) fn sync_file(path: &Path) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("cannot sync {}: {e}", path.display()))
}

/// Syncs the directory `dir`.
pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(dir)?
        .sync_all()
}

/// Whether `path` is a regular file (not a link) of at least `min` bytes
/// starting with `magic`.
pub(crate) fn has_magic(path: &Path, magic: &[u8]) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = vec![0u8; magic.len()];
    let mut f = file;
    f.read_exact(&mut head).is_ok() && head == magic
}

/// The start of a JPEG file.
pub(crate) const JPEG_MAGIC: &[u8] = &[0xFF, 0xD8, 0xFF];

/// The start of an EBML (Matroska, WebM) file.
pub(crate) const EBML_MAGIC: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];

/// The poster of the video `input` (opened and checked, of content `hash`,
/// described by `info`): the existing one when it is a JPEG, else one made
/// now with `ffmpeg`. The reason when it cannot be made.
pub fn ensure_poster(
    cache: &MediaCache,
    ffmpeg: &Ffmpeg,
    input: &File,
    info: &VideoInfo,
    hash: &str,
) -> Result<PathBuf, String> {
    let target = cache.poster_path(hash);
    if has_magic(&target, JPEG_MAGIC) {
        return Ok(target);
    }
    remove_logged(&target);
    let dir = cache.posters_dir();
    create_dir(&dir)?;
    let pending = PendingFile::new_in(&dir, TEMP_PREFIX);
    let job = FfmpegJob {
        input,
        input_format: info.container.ffmpeg_format(),
        output_args: ffmpeg::poster_args(),
        output: &pending.path,
        timeout: ffmpeg::POSTER_TIMEOUT,
    };
    ffmpeg
        .run(&job, &AtomicBool::new(false))
        .map_err(|e| e.to_string())?;
    if !has_magic(&pending.path, JPEG_MAGIC) {
        return Err("ffmpeg made no JPEG".into());
    }
    pending.commit(&target)?;
    Ok(target)
}

/// The largest shipped poster copied into the cache.
const MAX_SHIPPED_POSTER_BYTES: u64 = 8 * 1024 * 1024;

/// Copies `shipped` (a bundled video's poster, `<stem>.poster.jpg`) into
/// the cache as the poster of the content `hash`, without ffmpeg: a
/// regular JPEG file of at most 8 MiB, written as every cache file is. The
/// reason when it is not one or cannot be written.
pub fn install_poster(cache: &MediaCache, shipped: &Path, hash: &str) -> Result<PathBuf, String> {
    let shown = shipped.display();
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(shipped)
        .map_err(|e| format!("{shown} is unreadable: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("{shown} is unreadable: {e}"))?;
    if !meta.is_file() || meta.len() > MAX_SHIPPED_POSTER_BYTES {
        return Err(format!(
            "{shown} is not a regular file of at most {} MiB",
            MAX_SHIPPED_POSTER_BYTES / (1024 * 1024)
        ));
    }
    let mut data = Vec::new();
    (&mut file)
        .take(MAX_SHIPPED_POSTER_BYTES)
        .read_to_end(&mut data)
        .map_err(|e| format!("{shown} is unreadable: {e}"))?;
    if !data.starts_with(JPEG_MAGIC) {
        return Err(format!("{shown} is not a JPEG"));
    }
    let dir = cache.posters_dir();
    create_dir(&dir)?;
    let pending = PendingFile::new_in(&dir, TEMP_PREFIX);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&pending.path)
        .and_then(|mut f| f.write_all(&data))
        .map_err(|e| format!("cannot write {}: {e}", pending.path.display()))?;
    let target = cache.poster_path(hash);
    pending.commit(&target)?;
    Ok(target)
}

/// Removes the cache's temporary files in `dir` that an interrupted job
/// left: those of this process older than [`STALE_TEMP_AGE`], and those of
/// another process older than [`FOREIGN_TEMP_AGE`].
pub(crate) fn remove_stale_parts(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let ours = format!("{TEMP_PREFIX}{}-", std::process::id());
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(TEMP_PREFIX) && name.ends_with(".tmp")) {
            continue;
        }
        let path = entry.path();
        let limit = if name.starts_with(&ours) {
            STALE_TEMP_AGE
        } else {
            FOREIGN_TEMP_AGE
        };
        let old = std::fs::symlink_metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > limit);
        if old && remove_logged(&path) {
            removed.push(path);
        }
    }
    removed
}

/// The prefix of the cache's temporary files, for the other modules.
pub(crate) fn temp_prefix() -> &'static str {
    TEMP_PREFIX
}
