//! Finding module manifests: every `*.toml` file of an ordered list of
//! `modules.d` directories.
//!
//! Discovery reads manifests and nothing else: no library is opened, so a
//! broken module cannot stop the daemon from starting. A manifest that
//! cannot be read or parsed becomes an entry with the error as its reason,
//! never an error of the scan. A later directory overrides an earlier one
//! by module id, so a user install replaces the system's.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::manifest::{Manifest, valid_module_id};

/// The system directory of module manifests.
pub const SYSTEM_DIR: &str = "/usr/lib/omarchy-broadcast/modules.d";

/// The environment variable with extra manifest directories, separated by
/// `:` (development and tests). They come last, so they override all
/// others.
pub const MODULE_PATH_ENV: &str = "OBC_MODULE_PATH";

/// The directory of user manifests under the XDG data directory.
const USER_SUBDIR: &str = "omarchy-broadcast/modules.d";

/// One discovered manifest file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleEntry {
    /// The module id: the manifest's, or for a manifest that does not
    /// parse, its `id` key when that is a valid module id, else the file
    /// name without `.toml`.
    pub id: String,
    /// The manifest file.
    pub manifest_path: PathBuf,
    /// The manifest, or why it could not be read or parsed.
    pub manifest: Result<Manifest, String>,
}

/// What a scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// One entry per module id, by id.
    pub entries: Vec<ModuleEntry>,
    /// The manifests another one overrode (same id, later), with the id:
    /// for the daemon's log.
    pub overridden: Vec<(String, PathBuf)>,
}

impl Discovery {
    /// The entry of module `id`.
    pub fn entry(&self, id: &str) -> Option<&ModuleEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

/// The manifest directories, in order (later overrides earlier): the
/// system's ([`SYSTEM_DIR`]), the user's (`$XDG_DATA_HOME` or
/// `~/.local/share`, then `omarchy-broadcast/modules.d`), `extra` (the
/// configuration's), then [`MODULE_PATH_ENV`].
pub fn default_dirs(extra: &[PathBuf]) -> Vec<PathBuf> {
    dirs_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        extra,
        std::env::var_os(MODULE_PATH_ENV).as_deref(),
    )
}

/// [`default_dirs`] from explicit values: an `xdg_data_home` that is not an
/// absolute path is ignored, as the XDG specification says; without it
/// and without `home` there is no user directory. Empty elements of
/// `module_path` are skipped.
pub fn dirs_from(
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
    extra: &[PathBuf],
    module_path: Option<&OsStr>,
) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(SYSTEM_DIR)];
    let data = xdg_data_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            home.filter(|h| !h.is_empty())
                .map(|h| Path::new(h).join(".local/share"))
        });
    if let Some(data) = data {
        dirs.push(data.join(USER_SUBDIR));
    }
    dirs.extend(extra.iter().cloned());
    if let Some(path) = module_path {
        dirs.extend(std::env::split_paths(path).filter(|p| !p.as_os_str().is_empty()));
    }
    dirs
}

/// The id of an entry whose manifest did not parse.
fn fallback_id(text: Option<&str>, path: &Path) -> String {
    text.and_then(|t| toml::from_str::<toml::Table>(t).ok())
        .and_then(|t| t.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .filter(|id| valid_module_id(id))
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

/// Reads one manifest file into an entry.
pub fn read_entry(path: &Path) -> ModuleEntry {
    let text = std::fs::read_to_string(path);
    match &text {
        Ok(t) => match Manifest::parse(t, path) {
            Ok(m) => ModuleEntry {
                id: m.id.clone(),
                manifest_path: path.to_path_buf(),
                manifest: Ok(m),
            },
            Err(e) => ModuleEntry {
                id: fallback_id(Some(t), path),
                manifest_path: path.to_path_buf(),
                manifest: Err(e),
            },
        },
        Err(e) => ModuleEntry {
            id: fallback_id(None, path),
            manifest_path: path.to_path_buf(),
            manifest: Err(format!(
                "the manifest {} could not be read: {e}",
                path.display()
            )),
        },
    }
}

/// The `*.toml` files of `dir`, by file name; none when it cannot be read.
fn manifests_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension() == Some(OsStr::new("toml")) && !p.is_dir())
        .collect();
    files.sort();
    files
}

/// Scans `dirs` in order. A directory that does not exist is skipped.
/// Within a directory files are read in file-name order; a later manifest
/// with the same module id (in the same or a later directory) overrides an
/// earlier one, which is listed in [`Discovery::overridden`].
pub fn scan(dirs: &[PathBuf]) -> Discovery {
    let mut by_id: BTreeMap<String, ModuleEntry> = BTreeMap::new();
    let mut overridden = Vec::new();
    for dir in dirs {
        for path in manifests_in(dir) {
            let entry = read_entry(&path);
            if let Some(old) = by_id.insert(entry.id.clone(), entry) {
                overridden.push((old.id, old.manifest_path));
            }
        }
    }
    Discovery {
        entries: by_id.into_values().collect(),
        overridden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, name: &str) -> String {
        format!(
            r#"abi = 1
id = "{id}"
name = "{name}"
version = "1"
licence = "MIT"
kind = "video-stage"
library = "lib.so"
[[stage]]
id = "hello"
slot = "post-composite"
rank = 1
device = "cpu"
frames = ["host"]
formats = ["nv12"]
"#
        )
    }

    #[test]
    fn the_default_dirs_are_system_user_extra_then_the_variable() {
        let extra = [PathBuf::from("/srv/modules")];
        let dirs = dirs_from(
            Some(OsStr::new("/data")),
            Some(OsStr::new("/home/u")),
            &extra,
            Some(OsStr::new("/a::/b")),
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from(SYSTEM_DIR),
                PathBuf::from("/data/omarchy-broadcast/modules.d"),
                PathBuf::from("/srv/modules"),
                PathBuf::from("/a"),
                PathBuf::from("/b"),
            ]
        );
        // A relative XDG_DATA_HOME is ignored; HOME gives the default.
        let dirs = dirs_from(
            Some(OsStr::new("data")),
            Some(OsStr::new("/home/u")),
            &[],
            None,
        );
        assert_eq!(
            dirs[1],
            PathBuf::from("/home/u/.local/share/omarchy-broadcast/modules.d")
        );
        assert_eq!(
            dirs_from(None, None, &[], None),
            [PathBuf::from(SYSTEM_DIR)]
        );
    }

    #[test]
    fn later_dirs_override_earlier_ones_by_id() {
        let sys = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        std::fs::write(sys.path().join("a.toml"), manifest("org.x.a", "System A")).unwrap();
        std::fs::write(sys.path().join("b.toml"), manifest("org.x.b", "B")).unwrap();
        std::fs::write(sys.path().join("notes.txt"), "not a manifest").unwrap();
        // Another file name, the same id: overrides.
        std::fs::write(user.path().join("mine.toml"), manifest("org.x.a", "User A")).unwrap();
        let missing = sys.path().join("missing");
        let d = scan(&[sys.path().to_path_buf(), missing, user.path().to_path_buf()]);
        let ids: Vec<&str> = d.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["org.x.a", "org.x.b"]);
        let a = d.entry("org.x.a").unwrap();
        assert_eq!(a.manifest.as_ref().unwrap().name, "User A");
        assert_eq!(a.manifest_path, user.path().join("mine.toml"));
        assert_eq!(
            d.overridden,
            [("org.x.a".to_string(), sys.path().join("a.toml"))]
        );
    }

    #[test]
    fn a_malformed_manifest_is_an_entry_with_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("typo.toml"),
            manifest("org.x.typo", "T").replace("licence", "license"),
        )
        .unwrap();
        std::fs::write(dir.path().join("garbage.toml"), "= = =").unwrap();
        std::fs::write(dir.path().join("good.toml"), manifest("org.x.good", "G")).unwrap();
        let d = scan(&[dir.path().to_path_buf()]);
        let ids: Vec<&str> = d.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["garbage", "org.x.good", "org.x.typo"]);
        let typo = d.entry("org.x.typo").unwrap();
        let reason = typo.manifest.as_ref().unwrap_err();
        assert!(reason.contains("license"), "{reason}");
        assert!(d.entry("garbage").unwrap().manifest.is_err());
        assert!(d.entry("org.x.good").unwrap().manifest.is_ok());
    }

    #[test]
    fn an_unreadable_manifest_is_an_entry_too() {
        let dir = tempfile::tempdir().unwrap();
        // A directory named like a manifest is skipped; a dangling link is
        // an entry that cannot be read.
        std::fs::create_dir(dir.path().join("dir.toml")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("gone.toml"))
            .unwrap();
        let d = scan(&[dir.path().to_path_buf()]);
        assert_eq!(d.entries.len(), 1);
        let gone = &d.entries[0];
        assert_eq!(gone.id, "gone");
        assert!(
            gone.manifest
                .as_ref()
                .unwrap_err()
                .contains("could not be read")
        );
    }
}
