//! Whether CUDA is usable on this machine, found out without using it.
//!
//! [`probe`] decides from what the system shows, in this order (the first
//! check that fails gives the reason):
//!
//! 1. `CUDA_VISIBLE_DEVICES` does not hide every GPU (set and empty, `-1`
//!    or `NoDevFiles`);
//! 2. the NVIDIA kernel driver is loaded and lists a GPU: the directory
//!    `/proc/driver/nvidia/gpus` has an entry (the listing only, no file in
//!    it is read);
//! 3. the CUDA driver library (`libcuda.so.1`) can be opened;
//! 4. GStreamer's CUDA elements (`cudaupload`, `cudadownload`,
//!    `cudaconvertscale`) are registered. This crate does not link
//!    GStreamer: the caller looks them up in the registry
//!    (`gst::ElementFactory::find`, which reads the registry cache and
//!    loads no plugin) and passes the answer in [`ProbeEnv`].
//!
//! None of it creates a CUDA context, calls `cuInit` or any other driver
//! entry point, or opens a `/dev/nvidia*` device: listing procfs touches no
//! device, and opening the driver library runs its initialisers only (as
//! [`crate::park`] does to find its entry points). It is therefore safe while
//! the process is parked and while a runtime-suspended GPU sleeps
//! (`docs/adr/0006-on-demand-gpu-lifecycle.md`,
//! `docs/adr/0017-cuda-checkpoint-parking.md`). The caller must not
//! make a CUDA element (`gst::ElementFactory::make`) to answer step 4:
//! loading the `nvcodec` plugin calls `cuInit`.
//!
//! What it cannot see is a driver that is present but whose `cuInit` fails
//! (library and kernel module of different versions, a GPU in a bad state,
//! `CUDA_VISIBLE_DEVICES` naming no existing GPU): that shows only when CUDA
//! is first used.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::DRIVER_LIBRARY;

/// The NVIDIA kernel driver's procfs directory.
pub const NVIDIA_PROC_DIR: &str = "/proc/driver/nvidia";

/// The GStreamer elements the CUDA video pipeline needs, which the caller
/// looks up for [`ProbeEnv::cuda_elements_registered`].
pub const CUDA_ELEMENTS: [&str; 3] = ["cudaupload", "cudadownload", "cudaconvertscale"];

/// The reason when `CUDA_VISIBLE_DEVICES` hides every GPU.
pub const REASON_HIDDEN: &str = "CUDA_VISIBLE_DEVICES hides every GPU";
/// The reason when the NVIDIA kernel driver is not loaded.
pub const REASON_NO_DRIVER: &str = "no NVIDIA driver is loaded";
/// The reason when the NVIDIA kernel driver lists no GPU.
pub const REASON_NO_GPU: &str = "the NVIDIA driver reports no GPU";
/// The reason when the CUDA driver library cannot be opened.
pub const REASON_NO_LIBRARY: &str = "the CUDA driver library is not installed";
/// The reason when GStreamer's CUDA elements are not registered.
pub const REASON_NO_ELEMENTS: &str =
    "GStreamer's CUDA elements (gst-plugins-bad, nvcodec) are not installed";

/// Whether CUDA is usable, as [`probe`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// Every check passed.
    Usable,
    /// A check failed: why, in words a user reads (one of the `REASON_*`
    /// constants).
    Unusable(String),
}

impl Presence {
    /// Whether every check passed.
    pub fn is_usable(&self) -> bool {
        matches!(self, Presence::Usable)
    }

    /// Why CUDA is not usable; `None` when it is. The argument
    /// `obc_core::config::resolve_memory` takes.
    pub fn unusable_reason(&self) -> Option<&str> {
        match self {
            Presence::Usable => None,
            Presence::Unusable(r) => Some(r),
        }
    }
}

/// What [`probe`] looks at. [`ProbeEnv::from_process`] fills it from this
/// process and machine; tests build it by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeEnv {
    /// The value of `CUDA_VISIBLE_DEVICES`; `None` when not set.
    pub cuda_visible_devices: Option<OsString>,
    /// The NVIDIA driver's procfs directory ([`NVIDIA_PROC_DIR`]); its
    /// `gpus` subdirectory is listed.
    pub nvidia_proc_dir: PathBuf,
    /// The CUDA driver library's name or path ([`DRIVER_LIBRARY`]).
    pub driver_library: OsString,
    /// Whether every one of [`CUDA_ELEMENTS`] is in GStreamer's registry.
    pub cuda_elements_registered: bool,
}

impl ProbeEnv {
    /// This process's environment and the machine's paths, with the
    /// caller's answer for GStreamer's CUDA elements.
    pub fn from_process(cuda_elements_registered: bool) -> ProbeEnv {
        ProbeEnv {
            cuda_visible_devices: std::env::var_os("CUDA_VISIBLE_DEVICES"),
            nvidia_proc_dir: PathBuf::from(NVIDIA_PROC_DIR),
            driver_library: OsString::from(DRIVER_LIBRARY),
            cuda_elements_registered,
        }
    }
}

/// Whether a `CUDA_VISIBLE_DEVICES` value hides every GPU: set and empty
/// (or blank), `-1` or `NoDevFiles`.
pub fn cuda_hidden(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| {
        let v = v.to_string_lossy();
        let v = v.trim();
        v.is_empty() || v == "-1" || v == "NoDevFiles"
    })
}

/// Whether CUDA is usable according to `env`; see the module
/// documentation for the checks. Creates no CUDA context.
pub fn probe(env: &ProbeEnv) -> Presence {
    let unusable = |r: &str| Presence::Unusable(r.to_string());
    if cuda_hidden(env.cuda_visible_devices.as_deref()) {
        return unusable(REASON_HIDDEN);
    }
    match std::fs::read_dir(env.nvidia_proc_dir.join("gpus")) {
        Err(_) => return unusable(REASON_NO_DRIVER),
        Ok(mut entries) => {
            if !entries.any(|e| e.is_ok()) {
                return unusable(REASON_NO_GPU);
            }
        }
    }
    if !driver_library_opens(&env.driver_library) {
        return unusable(REASON_NO_LIBRARY);
    }
    if !env.cuda_elements_registered {
        return unusable(REASON_NO_ELEMENTS);
    }
    Presence::Usable
}

/// [`probe`] of [`ProbeEnv::from_process`].
pub fn probe_process(cuda_elements_registered: bool) -> Presence {
    probe(&ProbeEnv::from_process(cuda_elements_registered))
}

/// Whether the library `name` opens (lazy binding, local symbols). An
/// opened library stays loaded: the CUDA driver library is not meant to be
/// unloaded and loaded again, and a process that uses CUDA opens it anyway.
fn driver_library_opens(name: &OsStr) -> bool {
    use libloading::os::unix::{Library, RTLD_LAZY, RTLD_LOCAL};
    // SAFETY: opening the NVIDIA driver library runs its initialisers, which
    // have no preconditions and open no device; any other library named here
    // is the caller's choice (tests). Nothing is resolved or called.
    match unsafe { Library::open(Some(name), RTLD_LAZY | RTLD_LOCAL) } {
        Ok(lib) => {
            std::mem::forget(lib);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A scratch directory under the system's temporary directory, removed
    /// on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "obc-gpu-presence-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A procfs tree with `gpus` holding `n` GPU directories.
    fn proc_with_gpus(n: usize) -> Scratch {
        let s = Scratch::new();
        fs::create_dir_all(s.0.join("gpus")).unwrap();
        for i in 0..n {
            fs::create_dir_all(s.0.join(format!("gpus/0000:0{i}:00.0"))).unwrap();
        }
        s
    }

    /// An environment where every check passes: a GPU listed, a library
    /// every Linux process has loaded, the elements registered.
    fn usable(proc_dir: &Path) -> ProbeEnv {
        ProbeEnv {
            cuda_visible_devices: None,
            nvidia_proc_dir: proc_dir.to_path_buf(),
            driver_library: "libc.so.6".into(),
            cuda_elements_registered: true,
        }
    }

    #[test]
    fn every_check_passing_is_usable() {
        let p = proc_with_gpus(1);
        let r = probe(&usable(&p.0));
        assert_eq!(r, Presence::Usable);
        assert!(r.is_usable());
        assert_eq!(r.unusable_reason(), None);
        // A visible-devices list that names GPUs hides nothing.
        for v in ["0", "1,0", "GPU-8f1e"] {
            let env = ProbeEnv {
                cuda_visible_devices: Some(v.into()),
                ..usable(&p.0)
            };
            assert_eq!(probe(&env), Presence::Usable, "{v}");
        }
    }

    #[test]
    fn hidden_devices_are_unusable_before_anything_else_is_looked_at() {
        for v in ["", "  ", "-1", " -1 ", "NoDevFiles"] {
            // Nothing else would pass either: the first check decides.
            let env = ProbeEnv {
                cuda_visible_devices: Some(v.into()),
                nvidia_proc_dir: "/nonexistent/obc-gpu".into(),
                driver_library: "libobc-no-such-library.so.0".into(),
                cuda_elements_registered: false,
            };
            let r = probe(&env);
            assert_eq!(r.unusable_reason(), Some(REASON_HIDDEN), "{v:?}");
            assert!(!r.is_usable());
        }
        assert!(!cuda_hidden(None));
        assert!(!cuda_hidden(Some(OsStr::new("0"))));
    }

    #[test]
    fn a_missing_driver_and_a_driver_without_gpus_are_told_apart() {
        let none = Scratch::new();
        let r = probe(&usable(&none.0));
        assert_eq!(r.unusable_reason(), Some(REASON_NO_DRIVER));

        let empty = proc_with_gpus(0);
        let r = probe(&usable(&empty.0));
        assert_eq!(r.unusable_reason(), Some(REASON_NO_GPU));

        // `gpus` a file, not a directory: no listing, no driver.
        let odd = Scratch::new();
        fs::write(odd.0.join("gpus"), "").unwrap();
        assert_eq!(
            probe(&usable(&odd.0)).unusable_reason(),
            Some(REASON_NO_DRIVER)
        );
    }

    #[test]
    fn a_library_that_does_not_open_is_unusable() {
        let p = proc_with_gpus(2);
        let env = ProbeEnv {
            driver_library: "libobc-no-such-library.so.0".into(),
            ..usable(&p.0)
        };
        assert_eq!(probe(&env).unusable_reason(), Some(REASON_NO_LIBRARY));
    }

    #[test]
    fn missing_gstreamer_elements_are_unusable() {
        let p = proc_with_gpus(1);
        let env = ProbeEnv {
            cuda_elements_registered: false,
            ..usable(&p.0)
        };
        assert_eq!(probe(&env).unusable_reason(), Some(REASON_NO_ELEMENTS));
    }

    /// The `/dev/nvidia*` nodes this process holds open, and the lines of
    /// its memory map that map one.
    fn nvidia_device_use() -> (Vec<PathBuf>, Vec<String>) {
        let fds = fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|fd| fs::read_link(fd.path()).ok())
            .filter(|t| t.to_string_lossy().starts_with("/dev/nvidia"))
            .collect();
        let maps = fs::read_to_string("/proc/self/maps")
            .unwrap()
            .lines()
            .filter(|l| l.contains("/dev/nvidia"))
            .map(str::to_string)
            .collect();
        (fds, maps)
    }

    #[test]
    fn the_real_probe_opens_and_maps_no_nvidia_device() {
        // Runs on any machine: without NVIDIA hardware the probe stops at an
        // earlier check, with it every check runs (the driver library is
        // opened and stays mapped, which is allowed). Either way no device
        // node may be opened or mapped. No other test in this binary uses
        // CUDA, so a device seen here was opened by the probe.
        for registered in [true, false] {
            let r = probe_process(registered);
            assert_eq!(r.is_usable(), r.unusable_reason().is_none());
            let (fds, maps) = nvidia_device_use();
            assert!(fds.is_empty(), "the probe opened {fds:?} ({r:?})");
            assert!(maps.is_empty(), "the probe mapped {maps:?} ({r:?})");
        }
    }

    #[test]
    fn the_process_environment_names_the_real_paths() {
        let env = ProbeEnv::from_process(true);
        assert_eq!(env.nvidia_proc_dir, Path::new(NVIDIA_PROC_DIR));
        assert_eq!(env.driver_library, OsStr::new(DRIVER_LIBRARY));
        assert!(env.cuda_elements_registered);
        assert_eq!(
            env.cuda_visible_devices,
            std::env::var_os("CUDA_VISIBLE_DEVICES")
        );
    }
}
