//! The OpenCL drivers OpenVINO's GPU device is given: the Intel ones only.
//!
//! OpenVINO's GPU plugin (`libopenvino_intel_gpu_plugin.so`, loaded only
//! for the `GPU` device: the `NPU` and `CPU` devices do not load it) finds
//! its GPU through the OpenCL loader (`libOpenCL.so.1`), which opens every
//! driver named in `/etc/OpenCL/vendors/` to list the platforms. NVIDIA's
//! (`libnvidia-opencl.so.1`) opens `/dev/nvidia*` and so wakes a
//! runtime-suspended NVIDIA GPU, and the process keeps the device files
//! open until it exits, though OpenVINO then runs on the Intel GPU.
//!
//! So before the first `openvino-gpu` session (an explicit choice, or
//! `auto` reaching it), [`limit_once`] points `OCL_ICD_VENDORS` at a
//! directory holding links to the Intel GPU drivers' `.icd` files only
//! ([`intel_gpu_icds`]: those naming `libigdrcl*`, Intel's compute
//! runtime), `$XDG_RUNTIME_DIR/omarchy-broadcast/opencl-vendors/` (else
//! the module's cache directory). The value ends with `/`: the loader the
//! CUDA toolkit installs (`/opt/cuda/lib64/libOpenCL.so.1`, ahead of
//! ocl-icd's in the loader's cache when `/opt/cuda/lib64` is in
//! `ld.so.conf`) joins the directory and the file name without a
//! separator, and takes a directory only. ocl-icd takes the same value.
//! `OCL_ICD_FILENAMES` would not do: both loaders add its drivers to the
//! directory's instead of replacing them.
//!
//! Left alone, with the reason logged: when any `OCL_ICD_*` variable is
//! set already (the user's choice), and when no Intel GPU driver is listed
//! (OpenVINO's GPU device could not run anyway). Done once per process, the
//! outcome logged once.
//!
//! The loader reads the variable once, at the first OpenCL call of the
//! process; nothing else in the daemon makes one (the other modules use
//! CUDA, not OpenCL), so the first `openvino-gpu` session is that call. If
//! the loader is already in the process when the variable is set, a warning
//! says the limit may come too late.
//!
//! The variable is set in the process's environment (no OpenCL loader
//! takes its configuration any other way), which is the residual hazard:
//! see the SAFETY comment in [`limit_once`]. It is inherited by the
//! processes the daemon starts afterwards.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use obc_module_sdk::log;

/// Where the OpenCL loaders find the drivers by default.
pub const VENDORS_DIR: &str = "/etc/OpenCL/vendors";
/// The variable that points the loaders at another directory.
pub const VENDORS_ENV: &str = "OCL_ICD_VENDORS";
/// The prefix of the loaders' variables: any of them set is the user's
/// configuration of the loader, left alone.
pub const ENV_PREFIX: &str = "OCL_ICD_";
/// The directory of links made, under the runtime (or cache) directory.
pub const LINKS_DIR: &str = "omarchy-broadcast/opencl-vendors";

/// What is to be done about the OpenCL drivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// A loader variable is set already (`NAME=value`): left alone.
    UserSet(String),
    /// No Intel GPU driver among the vendors: left alone.
    NoIntelGpu,
    /// Only these `.icd` files (in the vendors directory) are to be seen.
    Limit(Vec<PathBuf>),
}

/// The plan, from the environment's variables (`vars`) and the vendors
/// directory `vendors`.
pub fn plan(vars: impl IntoIterator<Item = (OsString, OsString)>, vendors: &Path) -> Plan {
    let mut set: Vec<String> = vars
        .into_iter()
        .filter(|(k, _)| k.as_encoded_bytes().starts_with(ENV_PREFIX.as_bytes()))
        .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
        .collect();
    if !set.is_empty() {
        set.sort();
        return Plan::UserSet(set.join(" "));
    }
    let icds = intel_gpu_icds(vendors);
    if icds.is_empty() {
        Plan::NoIntelGpu
    } else {
        Plan::Limit(icds)
    }
}

/// The `.icd` files of `vendors` that name an Intel GPU driver (a library
/// whose file name starts with `libigdrcl`, Intel's compute runtime, the
/// legacy one's `libigdrcl_legacy1.so` among them), sorted.
pub fn intel_gpu_icds(vendors: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(vendors) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension() == Some(OsStr::new("icd")))
        .filter(|p| {
            std::fs::read_to_string(p).is_ok_and(|text| {
                let lib = text.lines().map(str::trim).find(|l| !l.is_empty());
                lib.and_then(|l| Path::new(l).file_name())
                    .is_some_and(|n| n.as_encoded_bytes().starts_with(b"libigdrcl"))
            })
        })
        .collect();
    out.sort();
    out
}

/// Makes `dir` hold a link to each of `icds` (by its file name) and
/// nothing else, and returns the variable's value: `dir` with a trailing
/// `/`. A link already right is kept, so that another process doing the
/// same at the same time never sees a driver missing.
pub fn populate(dir: &Path, icds: &[PathBuf]) -> std::io::Result<OsString> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // The loader opens what the links name: the directory is this user's.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let wanted: Vec<(OsString, &PathBuf)> = icds
        .iter()
        .filter_map(|p| Some((p.file_name()?.to_os_string(), p)))
        .collect();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let keep = wanted.iter().any(|(n, t)| {
            *n == e.file_name() && std::fs::read_link(e.path()).ok().as_ref() == Some(*t)
        });
        if !keep {
            std::fs::remove_file(e.path())?;
        }
    }
    for (name, target) in &wanted {
        match std::os::unix::fs::symlink(target, dir.join(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            r => r?,
        }
    }
    let mut value = dir.as_os_str().to_os_string();
    if !value.as_encoded_bytes().ends_with(b"/") {
        value.push("/");
    }
    Ok(value)
}

/// The directory of links: under `$XDG_RUNTIME_DIR` when it is set and
/// absolute, else under `cache` (the module's cache directory).
pub fn links_dir(runtime: Option<OsString>, cache: Option<&Path>) -> Option<PathBuf> {
    runtime
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|r| r.join(LINKS_DIR))
        .or_else(|| cache.map(|c| c.join("opencl-vendors")))
}

/// glibc's `RTLD_NOLOAD`: open a library only if it is loaded already.
const RTLD_NOLOAD: std::ffi::c_int = 0x4;

/// Whether an OpenCL loader is in the process already.
fn loader_loaded() -> bool {
    // SAFETY: opens only a library already loaded (its initialisers have
    // run); the reference taken is dropped at once.
    unsafe {
        libloading::os::unix::Library::open(
            Some("libOpenCL.so.1"),
            libloading::os::unix::RTLD_LAZY | RTLD_NOLOAD,
        )
    }
    .is_ok()
}

/// Limits the OpenCL drivers to the Intel GPU ones ([`plan`]), once per
/// process, before the first session on OpenVINO's GPU device. `cache` is
/// the module's cache directory, used when `$XDG_RUNTIME_DIR` is not set.
pub fn limit_once(cache: Option<&Path>) {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| limit(cache));
}

fn limit(cache: Option<&Path>) {
    let icds = match plan(std::env::vars_os(), Path::new(VENDORS_DIR)) {
        Plan::UserSet(vars) => {
            log::info!(
                "ONNX cut-out: OpenCL drivers left as configured ({vars}); OpenVINO's GPU device may wake the NVIDIA GPU"
            );
            return;
        }
        Plan::NoIntelGpu => {
            log::info!(
                "ONNX cut-out: OpenCL drivers not limited: no Intel GPU driver in {VENDORS_DIR}"
            );
            return;
        }
        Plan::Limit(icds) => icds,
    };
    let Some(dir) = links_dir(std::env::var_os("XDG_RUNTIME_DIR"), cache) else {
        log::warn!(
            "ONNX cut-out: OpenCL drivers not limited: neither XDG_RUNTIME_DIR nor a cache directory is set; OpenVINO's GPU device may wake the NVIDIA GPU"
        );
        return;
    };
    let value = match populate(&dir, &icds) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "ONNX cut-out: OpenCL drivers not limited: {} cannot be made: {e}; OpenVINO's GPU device may wake the NVIDIA GPU",
                dir.display()
            );
            return;
        }
    };
    let late = loader_loaded();
    // SAFETY: `setenv` is not synchronised with a `getenv` or a walk of
    // `environ` in another thread that does not go through Rust's
    // environment lock (C code: GStreamer, ONNX Runtime, OpenVINO). This
    // runs once per process (`limit_once`), only on the thread loading an
    // `openvino-gpu` session, and sets one variable no other code of the
    // process reads before the OpenCL loader's first call, which this
    // precedes; the residual hazard is another thread reading some other
    // variable at the instant this one is added (glibc may move the array
    // of variables then). There is no other way to configure the loader.
    unsafe { std::env::set_var(VENDORS_ENV, &value) };
    let names: Vec<String> = icds.iter().map(|p| p.display().to_string()).collect();
    log::info!(
        "ONNX cut-out: OpenCL drivers limited to {} ({VENDORS_ENV}={}), so that OpenVINO's GPU device leaves the other GPUs alone",
        names.join(", "),
        value.to_string_lossy()
    );
    if late {
        log::warn!(
            "ONNX cut-out: the OpenCL loader was in the process already; if it had been used, the limit comes too late"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vendors(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(d.path().join(name), text).unwrap();
        }
        d
    }

    fn vars(v: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        v.iter().map(|(k, v)| (k.into(), v.into())).collect()
    }

    #[test]
    fn only_the_intel_gpu_drivers_are_kept() {
        let d = vendors(&[
            ("nvidia.icd", "libnvidia-opencl.so.1\n"),
            ("intel.icd", "/usr/lib/intel-opencl/libigdrcl.so\n"),
            (
                "intel-legacy1.icd",
                "\n/usr/lib/intel-opencl-legacy1/libigdrcl_legacy1.so",
            ),
            ("intel-cpu.icd", "/opt/intel/oclcpu/libintelocl.so\n"),
            ("rusticl.icd", "libRusticlOpenCL.so.1\n"),
            ("igdrcl.txt", "/usr/lib/intel-opencl/libigdrcl.so\n"),
        ]);
        let p = d.path();
        assert_eq!(
            plan(vars(&[("HOME", "/home/u")]), p),
            Plan::Limit(vec![p.join("intel-legacy1.icd"), p.join("intel.icd")])
        );
    }

    #[test]
    fn a_loader_variable_the_user_set_is_left_alone() {
        let d = vendors(&[("intel.icd", "/usr/lib/intel-opencl/libigdrcl.so\n")]);
        for (k, v) in [
            ("OCL_ICD_VENDORS", "/etc/OpenCL/vendors"),
            ("OCL_ICD_FILENAMES", "/usr/lib/libfoo.so"),
            ("OCL_ICD_VENDORS", ""),
        ] {
            assert_eq!(
                plan(vars(&[("HOME", "/h"), (k, v)]), d.path()),
                Plan::UserSet(format!("{k}={v}"))
            );
        }
        // A name that only starts like one is not one.
        assert!(matches!(
            plan(vars(&[("OCL_ICDX", "1")]), d.path()),
            Plan::Limit(_)
        ));
    }

    #[test]
    fn without_an_intel_gpu_driver_nothing_is_done() {
        let d = vendors(&[("nvidia.icd", "libnvidia-opencl.so.1\n"), ("empty.icd", "")]);
        assert_eq!(plan(vars(&[]), d.path()), Plan::NoIntelGpu);
        assert_eq!(plan(vars(&[]), &d.path().join("missing")), Plan::NoIntelGpu);
    }

    #[test]
    fn the_links_directory_holds_the_chosen_drivers_only() {
        let src = vendors(&[
            ("intel.icd", "/usr/lib/intel-opencl/libigdrcl.so\n"),
            ("other.icd", "/usr/lib/intel-opencl/libigdrcl.so\n"),
        ]);
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("rt").join(LINKS_DIR);
        let intel = src.path().join("intel.icd");
        let v = populate(&dir, std::slice::from_ref(&intel)).unwrap();
        assert_eq!(v, OsString::from(format!("{}/", dir.display())));
        // A stale link, a wrong one and a file are replaced or removed.
        std::fs::write(dir.join("nvidia.icd"), "libnvidia-opencl.so.1\n").unwrap();
        std::fs::remove_file(dir.join("intel.icd")).unwrap();
        std::os::unix::fs::symlink(src.path().join("other.icd"), dir.join("intel.icd")).unwrap();
        populate(&dir, std::slice::from_ref(&intel)).unwrap();
        // Again, with everything right: nothing changes.
        populate(&dir, std::slice::from_ref(&intel)).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["intel.icd"]);
        assert_eq!(std::fs::read_link(dir.join("intel.icd")).unwrap(), intel);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn the_links_go_under_the_runtime_directory_else_the_cache() {
        let cache = Path::new("/home/u/.cache/omarchy-broadcast");
        assert_eq!(
            links_dir(Some("/run/user/1000".into()), Some(cache)),
            Some(PathBuf::from(
                "/run/user/1000/omarchy-broadcast/opencl-vendors"
            ))
        );
        assert_eq!(
            links_dir(Some("relative".into()), Some(cache)),
            Some(cache.join("opencl-vendors"))
        );
        assert_eq!(
            links_dir(None, Some(cache)),
            Some(cache.join("opencl-vendors"))
        );
        assert_eq!(links_dir(None, None), None);
    }
}
