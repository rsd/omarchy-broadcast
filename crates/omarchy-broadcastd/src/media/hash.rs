//! Content hashes of gallery files: SHA-256 (the `sha2` crate), and a
//! process-wide memo keyed by the file's identity, so a large video is
//! read once per daemon run however often it is listed, imported or
//! removed.
//!
//! The hash names derived files (posters, playback copies) in the cache:
//! the same bytes give the same name wherever the file lives, and a file
//! edited in place gets a new name.

use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// The identity of a file's content as far as the file system tells:
/// device, inode, size and modification and change times. Any write moves
/// the times, so a memo entry under a key is never stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileKey {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileKey {
    /// The key of the file `meta` describes.
    pub(crate) fn of(meta: &Metadata) -> FileKey {
        FileKey {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// The most entries the memo keeps; past that it is emptied (a gallery is
/// far smaller, so this only bounds a pathological run).
const MEMO_CAP: usize = 4096;

fn memo() -> &'static Mutex<HashMap<FileKey, String>> {
    static MEMO: OnceLock<Mutex<HashMap<FileKey, String>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The memoised hash of the file of `key`, if it was hashed in this run.
pub(crate) fn known(key: &FileKey) -> Option<String> {
    memo().lock().ok()?.get(key).cloned()
}

/// Records `hash` as the hash of the file of `key`.
pub(crate) fn remember(key: FileKey, hash: &str) {
    if let Ok(mut m) = memo().lock() {
        if m.len() >= MEMO_CAP {
            m.clear();
        }
        m.insert(key, hash.to_string());
    }
}

/// The hex SHA-256 of the open file `file` (read from its current offset
/// to the end, at most `limit` bytes; more is an error), memoised under its
/// key. `cancel`, when set, abandons the read (`Err("cancelled")`).
pub(crate) fn hash_file(
    file: &mut File,
    limit: u64,
    cancel: Option<&AtomicBool>,
) -> Result<String, String> {
    let meta = file
        .metadata()
        .map_err(|e| format!("it is unreadable: {e}"))?;
    let key = FileKey::of(&meta);
    if let Some(h) = known(&key) {
        return Ok(h);
    }
    let mut sha = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err("cancelled".into());
        }
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("it is unreadable: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            return Err(format!("it grew past {limit} bytes while being read"));
        }
        sha.update(&buf[..n]);
    }
    let hex = to_hex(&sha.finish());
    remember(key, &hex);
    Ok(hex)
}

/// Lower-case hexadecimal of `bytes`.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Whether `s` is a hash as [`hash_file`] writes it (64 lower-case hex
/// digits), so a cache file name can be told from a stranger's.
pub(crate) fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The SHA-256 of `data`.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut s = Sha256::new();
    s.update(data);
    s.finish()
}

/// An incremental SHA-256.
pub(crate) struct Sha256(sha2::Sha256);

impl Sha256 {
    pub(crate) fn new() -> Sha256 {
        Sha256(sha2::Digest::new())
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        sha2::Digest::update(&mut self.0, data);
    }

    pub(crate) fn finish(self) -> [u8; 32] {
        sha2::Digest::finalize(self.0).into()
    }
}
