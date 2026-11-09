//! The live preview's bookkeeping (interface revision 5): the leases, keyed
//! by the holder's unique bus name, and the private directory the frame
//! file is written to.
//!
//! A lease is taken with `AcquirePreview(u fps)`, 15 or 30 frames per
//! second, and ends with `ReleasePreview`, when its holder leaves the bus
//! (`NameOwnerChanged` with no new owner) or when the daemon exits. A
//! second `AcquirePreview` from the same name renews its lease at the new
//! rate: one lease per name. The frames are encoded at the highest rate
//! asked by a lease held ([`Leases::fps`]). Every lease is a consumer of
//! its own kind (`PreviewConsumers`), counted in `Consumers` with the
//! others.
//!
//! The frames go to `$XDG_RUNTIME_DIR/omarchy-broadcast/preview.jpg`
//! ([`PreviewDir`]): the directory is created mode 0700 if missing; if
//! present it must be a directory (not a symbolic link) owned by the
//! daemon's user, and its mode is forced to 0700. The encoder
//! (`obc_pipeline::preview`) writes each frame beside it and renames it
//! over it; the files are removed when the last lease ends, at daemon exit
//! and at daemon start (left by a daemon that crashed).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub use obc_ipc::PREVIEW_FPS;
use obc_pipeline::preview::{FRAME_FILE, remove_frames};

/// The daemon's directory under `$XDG_RUNTIME_DIR`.
pub const DIR_NAME: &str = "omarchy-broadcast";

/// Why a preview frame rate is refused: it is not one of [`PREVIEW_FPS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadFps(pub u32);

impl std::fmt::Display for BadFps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the preview frame rate must be 15 or 30 frames per second, not {}",
            self.0
        )
    }
}

/// Checks a frame rate asked for by `AcquirePreview`.
pub fn check_fps(fps: u32) -> Result<(), BadFps> {
    if PREVIEW_FPS.contains(&fps) {
        Ok(())
    } else {
        Err(BadFps(fps))
    }
}

/// The preview leases held, by unique bus name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leases {
    held: BTreeMap<String, u32>,
}

impl Leases {
    /// Records the lease of `name` at `fps`, replacing its previous one
    /// (the same name holds one lease). Nothing changes on error.
    pub fn acquire(&mut self, name: &str, fps: u32) -> Result<(), BadFps> {
        check_fps(fps)?;
        self.held.insert(name.to_string(), fps);
        Ok(())
    }

    /// Ends the lease of `name`; whether it held one.
    pub fn release(&mut self, name: &str) -> bool {
        self.held.remove(name).is_some()
    }

    /// Ends every lease.
    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// Whether `name` holds a lease.
    pub fn holds(&self, name: &str) -> bool {
        self.held.contains_key(name)
    }

    /// The number of leases (`PreviewConsumers`).
    pub fn count(&self) -> u32 {
        u32::try_from(self.held.len()).unwrap_or(u32::MAX)
    }

    /// The rate the frames are encoded at: the highest asked by a lease
    /// held (`None`: no lease).
    pub fn fps(&self) -> Option<u32> {
        self.held.values().copied().max()
    }
}

/// The preview directory, `$XDG_RUNTIME_DIR/omarchy-broadcast`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewDir(PathBuf);

impl PreviewDir {
    /// The directory under the runtime directory `runtime`
    /// (`$XDG_RUNTIME_DIR`), which must be set and absolute.
    pub fn under(runtime: Option<&Path>) -> Result<PreviewDir, String> {
        match runtime {
            Some(r) if r.is_absolute() => Ok(PreviewDir(r.join(DIR_NAME))),
            Some(r) => Err(format!(
                "XDG_RUNTIME_DIR ({}) is not an absolute path: the preview has nowhere to go",
                r.display()
            )),
            None => Err("XDG_RUNTIME_DIR is not set: the preview has nowhere to go".into()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The frame file `AcquirePreview` returns.
    pub fn frame(&self) -> PathBuf {
        self.0.join(FRAME_FILE)
    }

    /// Makes the directory safe to write the frames to: created mode 0700
    /// if missing; else it must be a real directory (a symbolic link is
    /// refused) owned by this process's user, and its mode is set to 0700.
    pub fn prepare(&self) -> Result<(), String> {
        let dir = &self.0;
        let shown = dir.display();
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create the preview directory {shown}: {e}")),
        }
        let meta =
            fs::symlink_metadata(dir).map_err(|e| format!("the preview directory {shown}: {e}"))?;
        if !meta.file_type().is_dir() {
            return Err(format!(
                "the preview directory {shown} is not a directory (a symbolic link or a file is \
                 in its place)"
            ));
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        if meta.uid() != uid {
            return Err(format!(
                "the preview directory {shown} belongs to uid {}, not to this user ({uid})",
                meta.uid()
            ));
        }
        if meta.mode() & 0o7777 != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("cannot make the preview directory {shown} private: {e}"))?;
        }
        Ok(())
    }

    /// Removes the frame files, if the directory is a real directory owned
    /// by this user (nothing is touched through a symbolic link or in a
    /// directory someone else owns).
    pub fn remove_files(&self) {
        let Ok(meta) = fs::symlink_metadata(&self.0) else {
            return;
        };
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        if meta.file_type().is_dir() && meta.uid() == uid {
            remove_frames(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_pipeline::preview::TMP_FILE;

    #[test]
    fn only_15_and_30_are_accepted() {
        for fps in [0, 1, 10, 14, 16, 29, 31, 60, u32::MAX] {
            assert_eq!(check_fps(fps), Err(BadFps(fps)), "{fps}");
        }
        for fps in [15, 30] {
            assert_eq!(check_fps(fps), Ok(()));
        }
        assert_eq!(
            BadFps(10).to_string(),
            "the preview frame rate must be 15 or 30 frames per second, not 10"
        );
    }

    #[test]
    fn acquire_release_and_the_effective_rate() {
        let mut l = Leases::default();
        assert_eq!((l.count(), l.fps()), (0, None));
        l.acquire(":1.7", 15).unwrap();
        assert_eq!((l.count(), l.fps()), (1, Some(15)));
        assert!(l.holds(":1.7"));
        // The same name again: one lease, at the new rate.
        l.acquire(":1.7", 30).unwrap();
        assert_eq!((l.count(), l.fps()), (1, Some(30)));
        l.acquire(":1.7", 15).unwrap();
        assert_eq!((l.count(), l.fps()), (1, Some(15)));
        // Two holders: counted twice, encoded at the higher rate.
        l.acquire(":1.9", 30).unwrap();
        assert_eq!((l.count(), l.fps()), (2, Some(30)));
        // A refused rate changes nothing, for a holder or a newcomer.
        for bad in [0, 10, 16, 60, u32::MAX] {
            assert_eq!(l.acquire(":1.7", bad), Err(BadFps(bad)));
            assert_eq!(l.acquire(":1.11", bad), Err(BadFps(bad)));
        }
        assert_eq!((l.count(), l.fps()), (2, Some(30)));
        assert!(!l.holds(":1.11"));
        // Releasing without a lease does nothing.
        assert!(!l.release(":1.11"));
        assert_eq!(l.count(), 2);
        assert!(l.release(":1.9"));
        assert_eq!((l.count(), l.fps()), (1, Some(15)));
        assert!(!l.release(":1.9"), "released once");
        assert!(l.release(":1.7"));
        assert_eq!((l.count(), l.fps()), (0, None));
        l.acquire(":1.7", 15).unwrap();
        l.clear();
        assert_eq!(l.count(), 0);
    }

    #[test]
    fn the_directory_needs_an_absolute_runtime_directory() {
        assert!(PreviewDir::under(None).unwrap_err().contains("not set"));
        assert!(
            PreviewDir::under(Some(Path::new("run/user/1000")))
                .unwrap_err()
                .contains("not an absolute path")
        );
        let d = PreviewDir::under(Some(Path::new("/run/user/1000"))).unwrap();
        assert_eq!(d.path(), Path::new("/run/user/1000/omarchy-broadcast"));
        assert_eq!(
            d.frame(),
            Path::new("/run/user/1000/omarchy-broadcast/preview.jpg")
        );
    }

    #[test]
    fn the_directory_is_made_private_and_a_link_refused() {
        let runtime = tempfile::tempdir().unwrap();
        let d = PreviewDir::under(Some(runtime.path())).unwrap();
        d.prepare().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(d.path()), 0o700);
        // Present with a loose mode: tightened.
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o755)).unwrap();
        d.prepare().unwrap();
        assert_eq!(mode(d.path()), 0o700);
        // Stale files are removed; the directory stays.
        fs::write(d.frame(), b"old").unwrap();
        fs::write(d.path().join(TMP_FILE), b"old").unwrap();
        d.remove_files();
        assert!(!d.frame().exists());
        assert!(!d.path().join(TMP_FILE).exists());
        assert!(d.path().is_dir());

        // A symbolic link in its place: refused, nothing done through it.
        let other = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), runtime.path().join(DIR_NAME)).unwrap();
        fs::write(other.path().join(FRAME_FILE), b"not ours").unwrap();
        let d = PreviewDir::under(Some(runtime.path())).unwrap();
        let e = d.prepare().unwrap_err();
        assert!(e.contains("not a directory"), "{e}");
        d.remove_files();
        assert!(other.path().join(FRAME_FILE).exists());
        // A file in its place: refused too.
        let runtime = tempfile::tempdir().unwrap();
        fs::write(runtime.path().join(DIR_NAME), b"").unwrap();
        let d = PreviewDir::under(Some(runtime.path())).unwrap();
        assert!(d.prepare().unwrap_err().contains("not a directory"));
        // A runtime directory that does not exist: cannot be created.
        let d = PreviewDir::under(Some(Path::new("/nonexistent-obc-test/run"))).unwrap();
        assert!(d.prepare().unwrap_err().contains("cannot create"));
    }
}
