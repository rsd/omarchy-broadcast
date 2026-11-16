//! Where the model files are, and the check that a file is the one the
//! catalogue names.
//!
//! The directories are searched in this order, the first one holding the
//! file wins:
//!
//! 1. the module configuration's `model_dir`;
//! 2. `$OBC_ONNX_MODEL_DIR`;
//! 3. `$XDG_DATA_HOME/omarchy-broadcast/models/onnx/` (by default
//!    `~/.local/share/omarchy-broadcast/models/onnx/`);
//! 4. `/usr/share/omarchy-onnx-models/` (the per-model packages
//!    `omarchy-onnx-model-<id>`);
//! 5. `/usr/share/omarchy-broadcast/models/onnx/`.
//!
//! The file found is read once, and its bytes checked against the
//! catalogue's SHA-256 and handed to ONNX Runtime; a file that differs fails the load (it is not skipped for a
//! later directory: a wrong file where the user put it is an error to fix,
//! not something to work around silently).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::catalogue::ModelSpec;

/// The environment variable naming a model directory.
pub const MODEL_DIR_ENV: &str = "OBC_ONNX_MODEL_DIR";
/// The directory the per-model packages `omarchy-onnx-model-<id>` install
/// into.
pub const MODELS_PACKAGE_DIR: &str = "/usr/share/omarchy-onnx-models";
/// The model directory of the omarchy-broadcast package.
pub const SYSTEM_MODEL_DIR: &str = "/usr/share/omarchy-broadcast/models/onnx";
/// The model directory under the user's data directory.
pub const USER_MODEL_SUBDIR: &str = "omarchy-broadcast/models/onnx";

/// The directories searched for model files, in order, from the configured
/// directory and the environment `env` (`std::env::var_os` in production).
pub fn search_dirs(
    configured: Option<&Path>,
    env: impl Fn(&str) -> Option<OsString>,
) -> Vec<PathBuf> {
    let set = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(d) = configured.filter(|d| !d.as_os_str().is_empty()) {
        dirs.push(d.to_path_buf());
    }
    if let Some(d) = set(MODEL_DIR_ENV) {
        dirs.push(d);
    }
    let data = set("XDG_DATA_HOME")
        .filter(|d| d.is_absolute())
        .or_else(|| set("HOME").map(|h| h.join(".local/share")));
    if let Some(d) = data {
        dirs.push(d.join(USER_MODEL_SUBDIR));
    }
    dirs.push(PathBuf::from(MODELS_PACKAGE_DIR));
    dirs.push(PathBuf::from(SYSTEM_MODEL_DIR));
    dirs
}

fn list(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first `file` in `dirs`; the error names the file and the
/// directories searched.
pub fn locate(file: &str, dirs: &[PathBuf]) -> Result<PathBuf, String> {
    dirs.iter()
        .map(|d| d.join(file))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            format!(
                "The model file {file} is not installed (searched: {})",
                list(dirs)
            )
        })
}

/// Whether some model of `models` has its file in `dirs` (no checksum: a
/// filesystem check for the probe).
pub fn any_installed<'a>(
    models: impl IntoIterator<Item = &'a ModelSpec>,
    dirs: &[PathBuf],
) -> bool {
    models.into_iter().any(|m| locate(m.file, dirs).is_ok())
}

/// The SHA-256 of `bytes`, lower-case hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// A model file found and checked: where it is, and its bytes, which are
/// what was hashed and what the session is made from (the file is read
/// once, so there is no time between the check and the use for it to
/// change).
pub struct Checked {
    /// The file.
    pub path: PathBuf,
    /// Its contents.
    pub bytes: Vec<u8>,
}

/// Reads the file at `path` and checks that it has the SHA-256 `expected`.
pub fn read_checked(path: &Path, expected: &str) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("The model file {} cannot be read: {e}", path.display()))?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(format!(
            "The model file {} is not the expected one (SHA-256 {actual}, expected {expected}): \
             reinstall it",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Finds the file of `model` in `dirs`, reads it and checks it.
pub fn resolve(model: &ModelSpec, dirs: &[PathBuf]) -> Result<Checked, String> {
    let path = locate(model.file, dirs).map_err(|e| {
        format!(
            "{e}; install the package omarchy-onnx-model-{} ({MODELS_PACKAGE_DIR}/{})",
            model.id, model.file
        )
    })?;
    let bytes =
        read_checked(&path, model.sha256).map_err(|e| format!("{e} (searched: {})", list(dirs)))?;
    Ok(Checked { path, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::{Geometry, Layout, Norm, Output};

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn the_search_order_is_config_env_user_then_system() {
        let dirs = search_dirs(
            Some(Path::new("/cfg")),
            env(&[
                (MODEL_DIR_ENV, "/env"),
                ("XDG_DATA_HOME", "/data"),
                ("HOME", "/home/u"),
            ]),
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from("/cfg"),
                PathBuf::from("/env"),
                PathBuf::from("/data/omarchy-broadcast/models/onnx"),
                PathBuf::from(MODELS_PACKAGE_DIR),
                PathBuf::from(SYSTEM_MODEL_DIR),
            ]
        );
        // No XDG_DATA_HOME (or a relative one, which the spec says to
        // ignore): ~/.local/share. Empty values count as unset.
        let dirs = search_dirs(
            Some(Path::new("")),
            env(&[
                (MODEL_DIR_ENV, ""),
                ("XDG_DATA_HOME", "rel"),
                ("HOME", "/home/u"),
            ]),
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from("/home/u/.local/share/omarchy-broadcast/models/onnx"),
                PathBuf::from(MODELS_PACKAGE_DIR),
                PathBuf::from(SYSTEM_MODEL_DIR),
            ]
        );
        assert_eq!(
            search_dirs(None, env(&[])),
            [
                PathBuf::from(MODELS_PACKAGE_DIR),
                PathBuf::from(SYSTEM_MODEL_DIR)
            ]
        );
    }

    #[test]
    fn the_per_model_package_directory_is_just_before_the_broadcast_one() {
        let dirs = search_dirs(None, env(&[("HOME", "/home/u")]));
        let at = |d: &str| dirs.iter().position(|p| p == Path::new(d)).unwrap();
        assert_eq!(MODELS_PACKAGE_DIR, "/usr/share/omarchy-onnx-models");
        assert_eq!(at(MODELS_PACKAGE_DIR) + 1, at(SYSTEM_MODEL_DIR));
        assert_eq!(at(SYSTEM_MODEL_DIR), dirs.len() - 1);
    }

    fn spec(file: &'static str, sha256: &'static str) -> ModelSpec {
        ModelSpec {
            id: "t",
            label: "T",
            file,
            sha256,
            layout: Layout::Nchw,
            norm: Norm::Unit,
            output: Output::Alpha,
            geometry: Geometry::Fixed {
                height: 1,
                width: 1,
            },
        }
    }

    // SHA-256 of "abc".
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn the_first_directory_with_the_file_wins_and_its_checksum_is_checked() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("m.onnx"), b"abc").unwrap();
        let dirs = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        assert_eq!(sha256_hex(b"abc"), ABC);
        let found = resolve(&spec("m.onnx", ABC), &dirs).unwrap();
        assert_eq!(found.path, b.path().join("m.onnx"));
        assert_eq!(found.bytes, b"abc");
        assert!(any_installed([&spec("m.onnx", ABC)], &dirs));

        // A missing file names the file and every directory searched.
        let e = resolve(&spec("x.onnx", ABC), &dirs).err().unwrap();
        assert!(e.contains("x.onnx"), "{e}");
        assert!(e.contains(&a.path().display().to_string()), "{e}");
        assert!(e.contains(&b.path().display().to_string()), "{e}");
        assert!(!any_installed([&spec("x.onnx", ABC)], &dirs));

        // A file in the first directory that differs is an error, even
        // though the second has the right one.
        std::fs::write(a.path().join("m.onnx"), b"abd").unwrap();
        let e = resolve(&spec("m.onnx", ABC), &dirs).err().unwrap();
        assert!(e.contains("not the expected one"), "{e}");
        assert!(
            e.contains(&a.path().join("m.onnx").display().to_string()),
            "{e}"
        );
        assert!(e.contains("searched"), "{e}");
    }

    #[test]
    fn a_missing_model_names_its_package_and_the_system_path() {
        let d = tempfile::tempdir().unwrap();
        let e = resolve(&spec("m.onnx", ABC), &[d.path().to_path_buf()])
            .err()
            .unwrap();
        assert!(
            e.contains("install the package omarchy-onnx-model-t"),
            "{e}"
        );
        assert!(e.contains("/usr/share/omarchy-onnx-models/m.onnx"), "{e}");
    }

    #[test]
    fn a_changed_file_is_read_again() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("m.onnx");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(read_checked(&p, ABC).unwrap(), b"abc");
        // Same size, other bytes, the same time stamp at a coarse clock:
        // still refused, as every load reads and hashes the file.
        std::fs::write(&p, b"abd").unwrap();
        assert!(read_checked(&p, ABC).is_err());
        assert!(read_checked(&d.path().join("none"), ABC).is_err());
    }
}
