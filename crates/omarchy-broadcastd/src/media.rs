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
//! Videos ([`MediaType::Video`]) are listed when found but not imported yet.

use std::ffi::OsString;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use gst_obc::background::{self, MAX_FILE_BYTES};
use serde::Deserialize;

use crate::{debug, warning};

/// The bundled root of the packaged gallery.
pub const BUNDLED_ROOT: &str = "/usr/share/omarchy-broadcast/media";

/// The name of the file describing the bundled items of a kind directory.
pub const SOURCES_FILE: &str = "SOURCES.toml";

/// The extensions of the image files listed (lower case).
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp"];

/// The extensions of the video files listed (lower case).
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm"];

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
    /// A video; listed when found, not imported yet.
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
    /// The width in pixels from the header; 0 when unknown (videos).
    pub width: u32,
    /// The height in pixels from the header; 0 when unknown (videos).
    pub height: u32,
    /// The duration in milliseconds; 0 for a still image or when unknown.
    pub duration_ms: u64,
    /// The poster image of a video, when there is one.
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
    /// A video given to import.
    #[error("cannot import {}: videos are not supported yet", .0.display())]
    VideoUnsupported(PathBuf),
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

/// The gallery's two roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStore {
    bundled_root: PathBuf,
    user_root: PathBuf,
}

impl MediaStore {
    /// A store over `bundled_root` (read only) and `user_root` (read and
    /// write), each holding one directory per [`MediaKind`].
    pub fn new(bundled_root: impl Into<PathBuf>, user_root: impl Into<PathBuf>) -> Self {
        MediaStore {
            bundled_root: bundled_root.into(),
            user_root: user_root.into(),
        }
    }

    /// The store of this session: [`BUNDLED_ROOT`], and
    /// `$XDG_DATA_HOME/omarchy-broadcast/media`, else
    /// `$HOME/.local/share/omarchy-broadcast/media`.
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
        Ok(Self::new(
            BUNDLED_ROOT,
            data.join("omarchy-broadcast").join("media"),
        ))
    }

    /// The bundled root.
    pub fn bundled_root(&self) -> &Path {
        &self.bundled_root
    }

    /// The user root.
    pub fn user_root(&self) -> &Path {
        &self.user_root
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
    /// be read is skipped and logged.
    pub fn list(&self, kind: MediaKind) -> Vec<MediaItem> {
        let mut items = self.list_bundled(kind);
        items.extend(self.list_user(kind));
        items
    }

    fn list_bundled(&self, kind: MediaKind) -> Vec<MediaItem> {
        let dir = self.bundled_dir(kind);
        let sources = read_sources(&dir);
        let mut found = scan_dir(&dir, true);
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
        let mut found = scan_dir(&self.user_dir(kind), false);
        found.sort_by(|a, b| (a.0.to_lowercase(), &a.0).cmp(&(b.0.to_lowercase(), &b.0)));
        found.into_iter().map(|(_, item)| item).collect()
    }

    /// Copies the image `src` (an absolute path) into the user directory of
    /// `kind` and returns the stored file's absolute path; when a file of
    /// that directory already has the same bytes, returns its path without
    /// copying. Refuses videos, anything [`background::read_image_file`]
    /// refuses (not a regular file, over [`MAX_FILE_BYTES`]), content that
    /// is not an accepted image or whose first image is incomplete, sizes
    /// outside [`background::MAX_DIMENSION`], and a format whose decoder is
    /// not installed. Never overwrites a file.
    pub fn import(&self, kind: MediaKind, src: &Path) -> Result<PathBuf, MediaError> {
        if !src.is_absolute() {
            return Err(MediaError::NotAbsolute(src.to_path_buf()));
        }
        if MediaType::from_path(src) == Some(MediaType::Video) {
            return Err(MediaError::VideoUnsupported(src.to_path_buf()));
        }
        let data = background::read_image_file(src).map_err(MediaError::Unreadable)?;
        let not_image = |reason: String| MediaError::NotAnImage {
            path: src.to_path_buf(),
            reason,
        };
        let (format, width, height) = background::image_header(&data).map_err(not_image)?;
        background::check_dimensions("it", width, height).map_err(not_image)?;
        background::image_length(format, &data).map_err(not_image)?;
        background::decoder_available(format).map_err(|reason| MediaError::NoDecoder {
            path: src.to_path_buf(),
            reason,
        })?;
        let ext = format.extension();

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
        if let Some(existing) = find_identical(&dir, &data) {
            debug!(
                "media: {} is already in the gallery as {}",
                src.display(),
                existing.display()
            );
            return Ok(existing);
        }

        let stem = sanitise_stem(
            &src.file_stem().unwrap_or_default().to_string_lossy(),
            kind.fallback_stem(),
        );
        let temp = TempFile::create(&dir, &data)?;
        for n in 1..=MAX_SUFFIX {
            let name = if n == 1 {
                format!("{stem}.{ext}")
            } else {
                format!("{stem}-{n}.{ext}")
            };
            let target = dir.join(&name);
            match std::fs::hard_link(&temp.path, &target) {
                Ok(()) => {
                    if let Err(source) = sync_dir(&dir) {
                        // The import fails as a whole: the name is freed
                        // again (the temporary file goes with `temp`).
                        let _ = std::fs::remove_file(&target);
                        return Err(MediaError::Io {
                            action: "sync",
                            path: dir,
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
            dir,
        })
    }

    /// Removes `path`, which must be a regular file (not a symbolic link)
    /// directly in a kind directory of the user root. A bundled item, or
    /// any other path, is [`MediaError::NotAUserItem`]. The file removed is
    /// the one checked: its name in the canonical kind directory, so a
    /// directory link in `path` swapped meanwhile cannot redirect it.
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
        std::fs::remove_file(&checked).map_err(|source| MediaError::Io {
            action: "remove",
            path: path.to_path_buf(),
            source,
        })
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
/// symbolic links are followed only when `follow`.
fn scan_dir(dir: &Path, follow: bool) -> Vec<(String, MediaItem)> {
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
        if name.starts_with('.') {
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
        let (width, height) = match media_type {
            MediaType::Image => match image_size(&path, follow) {
                Ok(size) => size,
                Err(reason) => {
                    debug!("media: skipping {}: {reason}", path.display());
                    continue;
                }
            },
            MediaType::Video => {
                if let Err(reason) = open_regular(&path, follow) {
                    debug!("media: skipping {}: {reason}", path.display());
                    continue;
                }
                (0, 0)
            }
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
                duration_ms: 0,
                poster: None,
            },
        ));
    }
    found
}

/// Opens `path` for reading without blocking (and, unless `follow`,
/// without following a symbolic link), and checks on the handle that it is
/// a regular file of at most [`MAX_FILE_BYTES`].
fn open_regular(path: &Path, follow: bool) -> Result<(File, Metadata), String> {
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
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "it is {} bytes, more than the {} MiB limit",
            meta.len(),
            MAX_FILE_BYTES / (1024 * 1024)
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

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            warning!("media: cannot remove {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests;
