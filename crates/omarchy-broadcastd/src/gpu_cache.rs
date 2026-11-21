//! What the daemon learnt about the GPU and keeps across restarts: the
//! CUDA compute capability, which picks the inference models made for this
//! GPU. It is learnt only while a session has the GPU up (never at idle:
//! the GPU may be parked), so
//! without this file a freshly started daemon would not know it until its
//! first session, and would count the models of another GPU as installed.
//!
//! The file is `<cache dir>/compute-capability` (the cache directory is
//! `$XDG_CACHE_HOME/omarchy-broadcast`, [`obc_core::config::cache_dir`]),
//! one line `major.minor`, e.g. `8.6`. It is a cache, not a setting and not
//! the state file: a missing, unreadable or garbled file means "unknown",
//! never an error, and a file that cannot be written is only logged.

use std::path::{Path, PathBuf};

/// The file's name in the cache directory.
pub const FILE: &str = "compute-capability";

/// The file in `dir`.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The compute capability stored in `dir`; `None` when there is none or it
/// cannot be read as one.
pub fn load(dir: &Path) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(path(dir)).ok()?;
    parse(&text)
}

/// `major.minor`, with surrounding whitespace.
fn parse(text: &str) -> Option<(u32, u32)> {
    let (major, minor) = text.trim().split_once('.')?;
    let num = |s: &str| {
        (!s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    Some((num(major)?, num(minor)?))
}

/// Stores `cc` in `dir` (created if missing), replacing the file whole.
pub fn save(dir: &Path, (major, minor): (u32, u32)) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let file = path(dir);
    let tmp = dir.join(format!(".{FILE}.tmp"));
    std::fs::write(&tmp, format!("{major}.{minor}\n"))?;
    std::fs::rename(&tmp, &file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_value_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache").join("omarchy-broadcast");
        assert_eq!(load(&cache), None, "no file: unknown");
        save(&cache, (8, 6)).unwrap();
        assert_eq!(std::fs::read_to_string(path(&cache)).unwrap(), "8.6\n");
        assert_eq!(load(&cache), Some((8, 6)));
        save(&cache, (12, 0)).unwrap();
        assert_eq!(load(&cache), Some((12, 0)));
    }

    #[test]
    fn anything_but_major_dot_minor_is_unknown() {
        assert_eq!(parse(" 12.0\n"), Some((12, 0)));
        for bad in [
            "", "8", "8.", ".6", "8.6.1", "a.b", "-8.6", "8.+6", "8 .6", "1234.0", "\u{0}",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(dir.path()), [0xff, 0xfe, b'.', b'1']).unwrap();
        assert_eq!(load(dir.path()), None, "not text");
        // A directory where the file should be: unknown, and not writable.
        let blocked = tempfile::tempdir().unwrap();
        std::fs::create_dir(path(blocked.path())).unwrap();
        assert_eq!(load(blocked.path()), None);
        assert!(save(blocked.path(), (8, 6)).is_err());
    }
}
