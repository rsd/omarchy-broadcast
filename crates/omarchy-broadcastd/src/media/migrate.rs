//! The one-time move of the user's gallery of earlier versions
//! (`$XDG_DATA_HOME/omarchy-broadcast/media`, else
//! `$HOME/.local/share/omarchy-broadcast/media`, with `backgrounds/` and
//! `away/`) into the user's folders, at the daemon's start, before anything
//! lists the gallery or prunes its cache: each file by its type, a video of
//! `backgrounds/` into the videos root ([`MediaStore::video_dir`]), any
//! other file into the pictures side ([`MediaStore::user_dir`]: the user
//! root for `backgrounds/`, its `away/` for `away/`).
//!
//! Only the regular files directly in an old kind directory move (never a
//! symbolic link, a directory or a hidden file; the stale temporary files
//! of interrupted imports are removed, as an import removes them), with
//! the records of its hidden `.sources/` (the animated sources' hashes)
//! moved to the `.sources/` of the directory the videos go to, rewritten
//! to the names the files get. A file is placed under its own
//! name, or the first free `<stem>-<n>.<ext>` when the name holds other
//! bytes (a name holding the same bytes is the file already moved). On
//! the same file system the old file's own inode is linked to the new
//! name (nothing partial is ever visible, nothing is copied); otherwise it
//! is copied to a hidden temporary file, synced, linked to its name, the
//! directory synced, and the copy read back and compared (size and
//! SHA-256) with the source. The old file is removed only then, and only
//! when nothing still names it: `[effect] background_image` or
//! `fallback_image` of the managed `config.d/50-panel.toml` and the state
//! file's background are rewritten to the new path; one set in
//! `config.toml` or another drop-in keeps the old file (logged, naming the
//! key, the file and the new path). When a file of the configuration or
//! the state cannot be read, every old file is kept. A failure on a file
//! leaves it, logged, and the next start retries; a rerun is idempotent.
//! The old directories are removed when they end empty, never
//! recursively and never through a symbolic link; a symbolic link in
//! place of the old root or a kind directory is not followed (logged).
//! When any new directory (the user root, its `away/`, the videos root) is,
//! holds or sits in the old root or any old kind directory (a symbolic
//! link or a bind mount into the old gallery; by device and inode, and by
//! canonical path), nothing is moved at all (logged). A file is never
//! placed into the old gallery, never taken for its own copy, and its old
//! file is never removed when its new name is a name of the old gallery.
//!
//! The root, its `away/` directory and the videos root are made before any
//! file is placed,
//! and a background is never given the name `away` (an old one so named
//! becomes `away-2`). Where the file system has no hard links (exFAT,
//! vfat), a copy gets its name by a rename that refuses an existing one.
//!
//! The kept files are remembered in the old kind directory's hidden
//! `.migrated` (size, modification time and name per line): while one is
//! unchanged and still named as above (or the configuration still cannot
//! be read), the next starts leave it alone, so a gallery copy the user
//! removed does not come back and nothing is hashed again. One that
//! changed, or that nothing names any more, is placed again (its gallery
//! copy found by its bytes) and removed as a migrated one is.
//!
//! A start stops starting new files after [`BUDGET`] (the daemon takes
//! its bus name only after the move, under systemd's start timeout); the
//! files done are already out of the old directory, the rest wait for the
//! next start (logged). An exclusive `flock` of the old root lets only one
//! of two daemons started at once move the gallery; the other skips it.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{Read, Seek};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use obc_core::config::RuntimeState;
use obc_core::config_d::{self, FileStatus, MANAGED_REL, ManagedEdit, OverridePolicy};

use super::{
    MAX_SUFFIX, MediaKind, MediaStore, MediaType, TRANSCODED_DIR, TempFile, hash, link_new,
    open_limited, remove_stale_temps, same_file, sync_dir,
};
use crate::{debug, info, warning};

/// The keys of the configuration that may name a gallery file.
const KEYS: [&str; 2] = ["effect.background_image", "effect.fallback_image"];

/// The largest configuration file read for its references.
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// The largest `.sources` record read.
const MAX_RECORD_BYTES: u64 = 4096;

/// What a migration did with the old items (the `.sources` records are
/// not counted).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// Placed in the user root, the old file removed.
    pub(crate) migrated: usize,
    /// Placed in the user root, the old file kept (something still names
    /// it).
    pub(crate) kept: usize,
    /// Left where they are after an error (logged).
    pub(crate) failed: usize,
    /// Not started: the start's time ran out ([`BUDGET`]); the next start
    /// continues.
    pub(crate) deferred: usize,
    /// Kept at an earlier start, unchanged and still named: not placed
    /// again.
    pub(crate) remembered: usize,
}

impl Outcome {
    /// Whether anything was done or left to do (worth a log line).
    fn eventful(&self) -> bool {
        self.migrated + self.kept + self.failed + self.deferred > 0
    }
}

/// How long a start goes on starting new files: the daemon requests its
/// bus name only after the move, and systemd stops a start that takes 30 s.
/// The file being moved when it runs out is finished.
pub(crate) const BUDGET: Duration = Duration::from_secs(15);

/// The record, in an old kind directory, of the old files kept there (one
/// line per file: `<size> <mtime s> <mtime ns> <name>`).
pub(crate) const KEPT_RECORD: &str = ".migrated";

/// The largest kept record read.
const MAX_KEPT_RECORD_BYTES: u64 = 1024 * 1024;

/// The old gallery root of `env`: `$XDG_DATA_HOME/omarchy-broadcast/media`
/// (a relative `XDG_DATA_HOME` ignored), else
/// `$HOME/.local/share/omarchy-broadcast/media`; `None` without either.
pub(crate) fn old_root_in(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let absolute = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    absolute("XDG_DATA_HOME")
        .or_else(|| absolute("HOME").map(|h| h.join(".local/share")))
        .map(|d| d.join("omarchy-broadcast").join("media"))
}

/// The daemon's start: moves the old gallery of this session's
/// environment into `store` ([`run`], for at most [`BUDGET`]), logs the
/// counts when there was anything to move, then makes sure the user root,
/// its `away` directory and the videos root exist (so a file can be
/// dropped there before any import; one that cannot be made is logged).
pub(crate) fn at_start(store: &MediaStore, config_toml: &Path, state: &Path) {
    if let Some(old) = old_root_in(|k| std::env::var_os(k)) {
        let deadline = Instant::now() + BUDGET;
        let o = run(store, &old, config_toml, state, &mut || {
            Instant::now() >= deadline
        });
        if o.eventful() {
            info!(
                "media: moved the gallery from {} to {} and {}: {} migrated, {} kept in place \
                 too, {} left after an error, {} left for the next start (out of time)",
                old.display(),
                store.user_root().display(),
                store.video_root().display(),
                o.migrated,
                o.kept,
                o.failed,
                o.deferred
            );
        }
    }
    for kind in MediaKind::ALL {
        if let Err(e) = store.ensure_user_dir(kind) {
            warning!("media: {e}");
        }
        if let Err(e) = store.ensure_video_dir(kind) {
            warning!("media: {e}");
        }
    }
}

/// One old item placed in the user root.
#[derive(Debug)]
struct Placed {
    old: PathBuf,
    new: PathBuf,
    stamp: Stamp,
}

/// What tells an old file unchanged since it was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl Stamp {
    fn of(m: &Metadata) -> Stamp {
        Stamp {
            size: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
        }
    }
}

/// Moves the old gallery at `old_root` into `store`'s user root; the
/// references are read from the configuration set of `config_toml` and
/// the state file `state`. Nothing when `old_root` is not a directory, or
/// while another process holds its lock (another daemon moving it). No
/// new file is started once `out_of_time` says so.
pub(crate) fn run(
    store: &MediaStore,
    old_root: &Path,
    config_toml: &Path,
    state: &Path,
    out_of_time: &mut dyn FnMut() -> bool,
) -> Outcome {
    let mut out = Outcome::default();
    if !is_real_dir(old_root) {
        log_if_link(old_root);
        return out;
    }
    let Some(_lock) = lock(old_root) else {
        return out;
    };
    let refs = References::load(config_toml, state);
    // Every directory first: an old background named `away` must not
    // take the away directory's name.
    for kind in MediaKind::ALL {
        let _ = store.ensure_user_dir(kind);
        let _ = store.ensure_video_dir(kind);
    }
    // A new directory reached through a symbolic link (or a bind mount)
    // may be an old one, hold one or sit in one: a file placed there, or
    // found there as a copy, is a name of the old gallery, and removing
    // the old file would lose it. Every new directory against every old
    // one, before anything moves; any overlap leaves it all as it is.
    if let Some((how, old, new)) = crossing(store, old_root) {
        warning!(
            "media: the old gallery {} {how} the new folder {}, so the old gallery is left as \
             it is: move its files by hand",
            old.display(),
            new.display()
        );
        return out;
    }
    let (old_canonical, old_names) = old_gallery(old_root);
    let mut placed = Vec::new();
    let mut old_dirs = Vec::new();
    // Per old kind directory, the files it keeps (for its record).
    let mut kept: HashMap<PathBuf, Vec<(OsString, Stamp)>> = HashMap::new();
    for kind in MediaKind::ALL {
        let old_dir = old_root.join(kind.dir_name());
        if !is_real_dir(&old_dir) {
            log_if_link(&old_dir);
            continue;
        }
        let new_dir = match store.ensure_user_dir(kind) {
            Ok(d) => d,
            Err(e) => {
                // Left whole, its record of kept files too.
                warning!("media: the old gallery {} stays: {e}", old_dir.display());
                out.failed += count_items(&old_dir);
                continue;
            }
        };
        // Videos go to the videos root (the same directory for away).
        let video_dir = store.video_dir(kind).unwrap_or_else(|| new_dir.clone());
        old_dirs.push(old_dir.clone());
        let reserved: &[&str] = match kind {
            MediaKind::Background => &[super::AWAY_DIR],
            MediaKind::Away => &[],
        };
        remove_stale_temps(&old_dir);
        let remembered = read_kept(&old_dir);
        // Old name -> new name of the files placed in the videos'
        // directory, for the records.
        let mut names: HashMap<OsString, OsString> = HashMap::new();
        for (name, path, stamp) in items_of(&old_dir) {
            if remembered.get(&name) == Some(&stamp) && refs.keeps(&path) {
                debug!("media: {} was kept at an earlier start", path.display());
                kept.entry(old_dir.clone()).or_default().push((name, stamp));
                out.remembered += 1;
                continue;
            }
            if out_of_time() {
                out.deferred += 1;
                continue;
            }
            let video = MediaType::from_path(&path) == Some(MediaType::Video);
            let dest = if video { &video_dir } else { &new_dir };
            match place(&path, dest, reserved, &old_canonical) {
                Ok(new) => {
                    debug!("media: {} is now {}", path.display(), new.display());
                    if (video || video_dir == new_dir)
                        && let Some(n) = new.file_name()
                    {
                        names.insert(name, n.to_os_string());
                    }
                    placed.push(Placed {
                        old: path,
                        new,
                        stamp,
                    });
                }
                Err(e) => {
                    warning!(
                        "media: {} stays in the old gallery (retried at the next start): {e}",
                        path.display()
                    );
                    out.failed += 1;
                }
            }
        }
        move_records(&old_dir, &video_dir, &names);
    }
    let removable = refs.settle(&placed, config_toml, state);
    for (p, remove) in placed.iter().zip(removable) {
        // The new name must be a file of its own: never a name of the old
        // gallery (one still to move, or moved), never in it.
        if let Err(why) = apart(&p.new, &old_canonical, &old_names) {
            warning!(
                "media: {} stays in the old gallery: its copy {} {why}",
                p.old.display(),
                p.new.display()
            );
            out.failed += 1;
            continue;
        }
        let removed = remove
            && match fs::remove_file(&p.old) {
                Ok(()) => true,
                Err(e) => {
                    warning!(
                        "media: {} was copied to {} but cannot be removed: {e}",
                        p.old.display(),
                        p.new.display()
                    );
                    false
                }
            };
        if removed {
            out.migrated += 1;
            continue;
        }
        out.kept += 1;
        if let (Some(dir), Some(name)) = (p.old.parent(), p.old.file_name()) {
            kept.entry(dir.to_path_buf())
                .or_default()
                .push((name.to_os_string(), p.stamp));
        }
    }
    for dir in &old_dirs {
        write_kept(dir, kept.get(dir).map(Vec::as_slice).unwrap_or_default());
        let _ = sync_dir(dir);
        remove_empty(&dir.join(TRANSCODED_DIR));
        remove_empty(dir);
    }
    // Never the old root when the user root or the videos root is in it
    // (or is it).
    for dir in [Some(old_root), old_root.parent()].into_iter().flatten() {
        if holds(dir, store.user_root()) || holds(dir, store.video_root()) {
            continue;
        }
        remove_empty(dir);
    }
    out
}

/// The first overlap of a new directory of `store` (the user root, its
/// `away` directory, the videos root) with the old root `old_root` or one
/// of its kind directories ([`overlap`]): how, the old one and the new one.
/// A new directory that does not exist (not even as a link) is none.
fn crossing(store: &MediaStore, old_root: &Path) -> Option<(&'static str, PathBuf, PathBuf)> {
    let mut new = MediaKind::ALL.map(|k| store.user_dir(k)).to_vec();
    new.push(store.video_root().to_path_buf());
    new.retain(|d| fs::symlink_metadata(d).is_ok());
    let mut old = vec![old_root.to_path_buf()];
    old.extend(
        MediaKind::ALL
            .iter()
            .map(|k| old_root.join(k.dir_name()))
            .filter(|d| is_real_dir(d)),
    );
    old.iter().find_map(|o| {
        new.iter()
            .find_map(|n| overlap(o, n).map(|how| (how, o.clone(), n.clone())))
    })
}

/// The old gallery at `old_root`, canonically: its directories (the root
/// and its kind directories) and the names of the files to move in them.
fn old_gallery(old_root: &Path) -> (Vec<PathBuf>, HashSet<PathBuf>) {
    let mut dirs = Vec::new();
    let mut names = HashSet::new();
    if let Ok(root) = fs::canonicalize(old_root) {
        dirs.push(root);
    }
    for kind in MediaKind::ALL {
        let dir = old_root.join(kind.dir_name());
        if !is_real_dir(&dir) {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&dir) else {
            continue;
        };
        names.extend(
            items_of(&dir)
                .into_iter()
                .map(|(n, _, _)| canonical.join(n)),
        );
        dirs.push(canonical);
    }
    (dirs, names)
}

/// The canonical path of the directory `dir` when it is neither one of
/// the old gallery's directories `old` (canonical) nor inside one; `Err`
/// naming why when it is, or when it cannot be resolved.
fn outside_old(dir: &Path, old: &[PathBuf]) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(dir).map_err(|e| format!("cannot be resolved ({e})"))?;
    if old.iter().any(|o| canonical.starts_with(o)) {
        return Err(format!("is in the old gallery ({})", canonical.display()));
    }
    Ok(canonical)
}

/// Whether the placed file `new` is apart from the old gallery: a regular
/// file, not in one of its directories `old` and not one of its names
/// `old_names` (all canonical). `Err` says why not.
fn apart(new: &Path, old: &[PathBuf], old_names: &HashSet<PathBuf>) -> Result<(), String> {
    let (Some(parent), Some(name)) = (new.parent(), new.file_name()) else {
        return Err("has no name".into());
    };
    let canonical = outside_old(parent, old)?.join(name);
    if old_names.contains(&canonical) {
        return Err(format!(
            "is a name of the old gallery ({})",
            canonical.display()
        ));
    }
    match fs::symlink_metadata(new) {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err("is not a regular file".into()),
        Err(e) => Err(format!("cannot be read ({e})")),
    }
}

/// How the old directory `old` relates to the new one `new`, when
/// they overlap: the same directory (by device and inode, or by canonical
/// path), or one inside the other (by canonical path). Directories that
/// cannot be compared count as overlapping.
fn overlap(old: &Path, new: &Path) -> Option<&'static str> {
    let (Ok(a), Ok(b)) = (fs::metadata(old), fs::metadata(new)) else {
        return Some("cannot be told apart from");
    };
    let (Ok(old), Ok(new)) = (fs::canonicalize(old), fs::canonicalize(new)) else {
        return Some("cannot be told apart from");
    };
    if (a.dev() == b.dev() && a.ino() == b.ino()) || old == new {
        Some("is")
    } else if new.starts_with(&old) {
        Some("holds")
    } else if old.starts_with(&new) {
        Some("is inside")
    } else {
        None
    }
}

/// Whether the directory `dir` is `inner` or holds it (canonical paths;
/// `true` when `dir` exists but cannot be resolved).
fn holds(dir: &Path, inner: &Path) -> bool {
    let Ok(dir) = fs::canonicalize(dir) else {
        return fs::symlink_metadata(dir).is_ok();
    };
    fs::canonicalize(inner).is_ok_and(|inner| inner.starts_with(&dir))
}

/// Logs that `path` is a symbolic link, so not moved (never followed: the
/// old gallery was the daemon's own directory).
fn log_if_link(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) {
        warning!(
            "media: {} is a symbolic link, so it is not moved: move its files by hand",
            path.display()
        );
    }
}

/// An exclusive lock of the directory `dir` (released when the file
/// closes): `None` when another process holds it (logged) or it cannot be
/// taken (logged; nothing is moved then). Two daemons started at once
/// both pass the bus name check; only one moves the gallery.
fn lock(dir: &Path) -> Option<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir);
    let f = match f {
        Ok(f) => f,
        Err(e) => {
            warning!("media: the old gallery {} stays: {e}", dir.display());
            return None;
        }
    };
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Some(f);
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        info!(
            "media: another process is moving the old gallery {}",
            dir.display()
        );
    } else {
        warning!("media: the old gallery {} stays: {e}", dir.display());
    }
    None
}

/// The files the record of the old kind directory `dir` says were kept,
/// by name (an unreadable record is none: they are placed again).
fn read_kept(dir: &Path) -> HashMap<OsString, Stamp> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = HashMap::new();
    let Ok((file, _)) = open_limited(&dir.join(KEPT_RECORD), false, MAX_KEPT_RECORD_BYTES) else {
        return out;
    };
    let mut data = Vec::new();
    if file
        .take(MAX_KEPT_RECORD_BYTES)
        .read_to_end(&mut data)
        .is_err()
    {
        return out;
    }
    for line in data.split(|&b| b == b'\n') {
        let mut parts = line.splitn(4, |&b| b == b' ');
        let mut num = || {
            parts
                .next()
                .and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|p| p.parse::<i64>().ok())
        };
        let (Some(size), Some(mtime), Some(mtime_nsec)) = (num(), num(), num()) else {
            continue;
        };
        let Some(name) = parts.next().filter(|n| !n.is_empty()) else {
            continue;
        };
        let Ok(size) = u64::try_from(size) else {
            continue;
        };
        out.insert(
            OsStr::from_bytes(name).to_os_string(),
            Stamp {
                size,
                mtime,
                mtime_nsec,
            },
        );
    }
    out
}

/// Replaces the record of the old kind directory `dir` with `kept`, or
/// removes it when nothing is kept (a name holding a line break is not
/// recorded: it is placed again at each start). A failure is logged.
fn write_kept(dir: &Path, kept: &[(OsString, Stamp)]) {
    use std::os::unix::ffi::OsStrExt;
    let record = dir.join(KEPT_RECORD);
    if kept.is_empty() {
        match fs::remove_file(&record) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warning!("media: cannot remove {}: {e}", record.display()),
        }
        return;
    }
    let mut data = Vec::new();
    for (name, s) in kept {
        let name = name.as_bytes();
        if name.contains(&b'\n') {
            continue;
        }
        data.extend_from_slice(format!("{} {} {} ", s.size, s.mtime, s.mtime_nsec).as_bytes());
        data.extend_from_slice(name);
        data.push(b'\n');
    }
    let written = TempFile::create(dir, &data)
        .map_err(|e| e.to_string())
        .and_then(|temp| fs::rename(&temp.path, &record).map_err(|e| e.to_string()));
    if let Err(e) = written {
        warning!(
            "media: cannot write {} (the kept files are placed again at the next start): {e}",
            record.display()
        );
    }
}

/// Whether `path` is a directory, not a symbolic link to one.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Removes the directory `dir` when it is empty (never recursively, never
/// through a symbolic link: neither `dir` nor its parent may be one).
fn remove_empty(dir: &Path) {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return;
    };
    let Ok(name) = std::ffi::CString::new(name.as_bytes()) else {
        return;
    };
    let Ok(parent) = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
    else {
        return;
    };
    let fd = parent.as_raw_fd();
    // SAFETY: a zeroed `stat` filled by fstatat, on a descriptor and a C
    // string this function owns.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0
        || st.st_mode & libc::S_IFMT != libc::S_IFDIR
    {
        return;
    }
    // SAFETY: as above. A directory swapped for a link meanwhile is not
    // removed (rmdir refuses a link).
    if unsafe { libc::unlinkat(fd, name.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
        debug!("media: removed the empty {}", dir.display());
    }
}

/// The items to move of the old kind directory `dir`, by name: the
/// regular files directly in it, not hidden.
fn items_of(dir: &Path) -> Vec<(OsString, PathBuf, Stamp)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(OsString, PathBuf, Stamp)> = entries
        .flatten()
        .filter(|e| !e.file_name().as_encoded_bytes().starts_with(b"."))
        .filter_map(|e| {
            let m = fs::symlink_metadata(e.path())
                .ok()
                .filter(Metadata::is_file)?;
            Some((e.file_name(), e.path(), Stamp::of(&m)))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn count_items(dir: &Path) -> usize {
    items_of(dir).len()
}

/// The name `<stem>.<ext>` for `n` = 1, else `<stem>-<n>.<ext>` (no dot
/// without an extension).
fn candidate(stem: &OsStr, ext: Option<&OsStr>, n: u32) -> OsString {
    let mut name = stem.to_os_string();
    if n > 1 {
        name.push(format!("-{n}"));
    }
    if let Some(ext) = ext {
        name.push(".");
        name.push(ext);
    }
    name
}

/// The hex SHA-256 of the regular file `path` (not followed), with its
/// size.
fn hash_of(path: &Path) -> Result<(String, u64), String> {
    let (mut f, meta) = open_limited(path, false, u64::MAX)?;
    let h = hash::hash_file(&mut f, u64::MAX, None)?;
    Ok((h, meta.len()))
}

/// Places the regular file `src` in `dir` under its name or the first
/// free numbered one (never one of `reserved`), unless a name on the way
/// holds the same bytes (that file is returned). Returns the path in
/// `dir`; the source is untouched. Refused when `dir` is in one of the old
/// gallery's directories `old` (canonical): a match there would be a name
/// of the old gallery, a copy there would be in it.
fn place(src: &Path, dir: &Path, reserved: &[&str], old: &[PathBuf]) -> Result<PathBuf, String> {
    let (mut file, meta) = open_limited(src, false, u64::MAX)?;
    outside_old(dir, old).map_err(|why| format!("{} {why}", dir.display()))?;
    // In its own directory the source would be found as its own copy (and
    // then removed): never placed there.
    let same_dir = match (src.parent().map(fs::metadata), fs::metadata(dir)) {
        (Some(Ok(a)), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => true,
    };
    if same_dir {
        return Err(format!("{} is its own directory", dir.display()));
    }
    let path = Path::new(src.file_name().ok_or("it has no name")?);
    let stem = path.file_stem().unwrap_or(path.as_os_str());
    let ext = path.extension();
    let mut src_hash: Option<String> = None;
    for n in 1..=MAX_SUFFIX {
        let name = candidate(stem, ext, n);
        if reserved.iter().any(|r| name == *r) {
            continue;
        }
        let target = dir.join(name);
        match fs::symlink_metadata(&target) {
            Ok(m) => {
                if m.dev() == meta.dev() && m.ino() == meta.ino() {
                    // The source's own inode: a copy only as another name
                    // of it (a link made by an earlier start), never the
                    // source's own entry.
                    if m.is_file() && m.nlink() >= 2 {
                        return Ok(target);
                    }
                    return Err(format!("{} is the file itself", target.display()));
                }
                if m.is_file() && m.len() == meta.len() {
                    let h = match &src_hash {
                        Some(h) => h.clone(),
                        None => {
                            file.rewind()
                                .map_err(|e| format!("it is unreadable: {e}"))?;
                            let h = hash::hash_file(&mut file, u64::MAX, None)?;
                            src_hash = Some(h.clone());
                            h
                        }
                    };
                    if hash_of(&target).is_ok_and(|(t, _)| t == h) {
                        return Ok(target);
                    }
                }
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot read {}: {e}", target.display())),
        }
        match put(src, &mut file, &meta, dir, &target)? {
            true => return Ok(target),
            false => continue,
        }
    }
    Err(format!("no free name left in {}", dir.display()))
}

/// Gives `target` (a free name in `dir`) the content of `src` (opened as
/// `file`, described by `meta`): its own inode linked when both are on
/// one file system, else a verified copy. `false` when the name was taken
/// meanwhile (nothing written).
fn put(
    src: &Path,
    file: &mut File,
    meta: &Metadata,
    dir: &Path,
    target: &Path,
) -> Result<bool, String> {
    let synced = |target: &Path| {
        sync_dir(dir).map_err(|e| {
            let _ = fs::remove_file(target);
            format!("cannot sync {}: {e}", dir.display())
        })
    };
    let link = if force_copy() {
        Err(std::io::Error::from_raw_os_error(libc::EXDEV))
    } else {
        fs::hard_link(src, target)
    };
    match link {
        Ok(()) => {
            let linked = fs::symlink_metadata(target)
                .is_ok_and(|m| m.dev() == meta.dev() && m.ino() == meta.ino());
            if !linked {
                // Never the case: the old directory is the daemon's.
                let _ = fs::remove_file(target);
                return Err(format!(
                    "{} changed while it was being moved",
                    src.display()
                ));
            }
            let _ = fs::set_permissions(target, fs::Permissions::from_mode(0o600));
            synced(target)?;
            return Ok(true);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        // Another file system (or links refused: the old file is never
        // renamed, it stays until nothing names it): a copy.
        Err(e) => debug!("media: copying {} (no link: {e})", src.display()),
    }
    file.rewind()
        .map_err(|e| format!("it is unreadable: {e}"))?;
    let (temp, copied) = TempFile::copy_from(dir, file, u64::MAX).map_err(|e| e.to_string())?;
    match link_new(&temp.path, target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(format!("cannot write {}: {e}", target.display())),
    }
    drop(temp);
    synced(target)?;
    #[cfg(test)]
    if CORRUPT_COPY.with(std::cell::Cell::get) {
        let mut data = fs::read(target).map_err(|e| e.to_string())?;
        if let Some(b) = data.first_mut() {
            *b ^= 0xff;
        }
        fs::write(target, data).map_err(|e| e.to_string())?;
    }
    // Read back from the disk (a new inode: never a remembered hash).
    let verified = hash_of(target);
    match verified {
        Ok((h, len)) if h == copied && len == meta.len() => Ok(true),
        other => {
            let _ = fs::remove_file(target);
            Err(match other {
                Ok(_) => format!("the copy {} differs from it", target.display()),
                Err(e) => format!("the copy {} cannot be read back: {e}", target.display()),
            })
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Copies as across file systems (tests).
    static FORCE_COPY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Changes a copy before it is read back (tests).
    static CORRUPT_COPY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether to copy even where a link would do (tests only).
fn force_copy() -> bool {
    #[cfg(test)]
    return FORCE_COPY.with(std::cell::Cell::get);
    #[cfg(not(test))]
    false
}

/// Moves the records of the old `.sources/` of `old_dir` into that of
/// `new_dir`, each naming the file's new name (`names`). A record naming a
/// file still in the old directory stays with it; a record naming no file
/// is dropped; when the new directory has a record of the same source,
/// that one is kept.
fn move_records(old_dir: &Path, new_dir: &Path, names: &HashMap<OsString, OsString>) {
    let old = old_dir.join(TRANSCODED_DIR);
    let Ok(entries) = fs::read_dir(&old) else {
        return;
    };
    let new = new_dir.join(TRANSCODED_DIR);
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(hash_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !hash::is_hash(&hash_name) {
            continue;
        }
        let Some(named) = read_record(&path) else {
            continue;
        };
        let named = OsString::from(named);
        let new_name = match names.get(&named) {
            Some(n) => n.clone(),
            None if fs::symlink_metadata(old_dir.join(&named)).is_ok() => continue,
            None => {
                debug!("media: dropping the stale record {}", path.display());
                cache_remove(&path);
                continue;
            }
        };
        match write_record(&new, &hash_name, &new_name) {
            Ok(()) => cache_remove(&path),
            Err(e) => warning!(
                "media: the record {} stays in the old gallery: {e}",
                path.display()
            ),
        }
    }
    let _ = sync_dir(&old);
}

fn cache_remove(path: &Path) {
    super::cache::remove_logged(path);
}

/// The file name a record holds, checked as an import reads it.
fn read_record(path: &Path) -> Option<String> {
    let (file, _) = open_limited(path, false, MAX_RECORD_BYTES).ok()?;
    let mut text = String::new();
    file.take(MAX_RECORD_BYTES).read_to_string(&mut text).ok()?;
    let name = text.trim();
    (!name.is_empty() && !name.contains('/') && !name.starts_with('.')).then(|| name.to_string())
}

/// Writes the record `hash_name` naming `name` in `records`, without
/// replacing one there.
fn write_record(records: &Path, hash_name: &str, name: &OsStr) -> Result<(), String> {
    let name = name.to_str().ok_or("the file's new name is not UTF-8")?;
    super::cache::create_dir(records)?;
    let target = records.join(hash_name);
    if fs::symlink_metadata(&target).is_ok() {
        return Ok(());
    }
    let temp = TempFile::create(records, name.as_bytes()).map_err(|e| e.to_string())?;
    match link_new(&temp.path, &target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot write {}: {e}", target.display())),
    }
    drop(temp);
    sync_dir(records).map_err(|e| format!("cannot sync {}: {e}", records.display()))
}

/// A configuration file's value naming a file.
#[derive(Debug)]
struct ConfigRef {
    key: &'static str,
    /// The file setting it, relative to the configuration directory.
    file: String,
    path: PathBuf,
    managed: bool,
}

/// What names gallery files: the configuration set and the state file.
#[derive(Debug)]
struct References {
    /// Every file of the set and the state could be read; else no old
    /// file is removed.
    known: bool,
    config: Vec<ConfigRef>,
    /// The state, when it names a background.
    state: Option<RuntimeState>,
}

impl References {
    fn load(config_toml: &Path, state: &Path) -> References {
        let mut known = true;
        let mut config = Vec::new();
        let (dir, files) = config_d::config_files(config_toml);
        for f in files {
            match f.status {
                FileStatus::Absent => continue,
                FileStatus::Error => {
                    warning!(
                        "media: {} cannot be read ({}): the old gallery's files are kept",
                        dir.join(&f.path).display(),
                        f.reason
                    );
                    known = false;
                    continue;
                }
                FileStatus::Loaded => {}
            }
            let path = dir.join(&f.path);
            let table = read_table(&path);
            let Some(table) = table else {
                warning!(
                    "media: {} cannot be read: the old gallery's files are kept",
                    path.display()
                );
                known = false;
                continue;
            };
            for key in KEYS {
                if let Some(toml::Value::String(v)) = config_d::table_get(&table, key)
                    && !v.is_empty()
                {
                    config.push(ConfigRef {
                        key,
                        file: f.path.clone(),
                        path: PathBuf::from(v),
                        managed: f.managed || f.path == MANAGED_REL,
                    });
                }
            }
        }
        let (st, damaged) = RuntimeState::load(state);
        let state = match damaged {
            Some(why) => {
                warning!("media: {why}: the old gallery's files are kept");
                known = false;
                None
            }
            None => st.background_image.is_some().then_some(st),
        };
        References {
            known,
            config,
            state,
        }
    }

    /// Whether the old file `path` stays named after a move: the set or
    /// the state could not be read, or a configuration file other than
    /// the managed one sets it (the managed file and the state are
    /// rewritten instead).
    fn keeps(&self, path: &Path) -> bool {
        !self.known
            || self
                .config
                .iter()
                .any(|r| !r.managed && same_file(&r.path, path))
    }

    /// Rewrites the managed file and the state where they name a placed
    /// file, and says, per placed file, whether its old file may go.
    fn settle(&self, placed: &[Placed], config_toml: &Path, state_path: &Path) -> Vec<bool> {
        if !self.known {
            return vec![false; placed.len()];
        }
        let mut ok = vec![true; placed.len()];
        let find = |p: &Path| placed.iter().position(|m| same_file(p, &m.old));
        let mut edits = Vec::new();
        let mut by_managed = Vec::new();
        for r in &self.config {
            let Some(i) = find(&r.path) else {
                continue;
            };
            let new = &placed[i].new;
            if r.managed {
                edits.push(ManagedEdit::Set(
                    r.key.to_string(),
                    toml::Value::String(new.to_string_lossy().into_owned()),
                ));
                by_managed.push(i);
            } else {
                warning!(
                    "media: {} sets {} to {}, so that file is kept; the gallery's copy is {}",
                    r.file,
                    r.key,
                    placed[i].old.display(),
                    new.display()
                );
                ok[i] = false;
            }
        }
        if !edits.is_empty() {
            match config_d::persist_managed(config_toml, &edits, OverridePolicy::Ignore) {
                Ok(_) => info!("media: {MANAGED_REL} now names the moved gallery's files"),
                Err(e) => {
                    warning!(
                        "media: {MANAGED_REL} cannot be rewritten ({e}): the old files it names are kept"
                    );
                    for i in by_managed {
                        ok[i] = false;
                    }
                }
            }
        }
        if let Some(st) = &self.state
            && let Some(image) = &st.background_image
            && let Some(i) = find(image)
        {
            let mut st = st.clone();
            st.background_image = Some(placed[i].new.clone());
            if let Err(e) = st.save(state_path) {
                warning!(
                    "media: {} cannot be rewritten ({e}): {} is kept",
                    state_path.display(),
                    placed[i].old.display()
                );
                ok[i] = false;
            }
        }
        ok
    }
}

/// The TOML table of the file `path`, read up to [`MAX_CONFIG_BYTES`].
fn read_table(path: &Path) -> Option<toml::Table> {
    let file = File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return None;
    }
    toml::from_str(&text).ok()
}

#[cfg(test)]
mod tests;
