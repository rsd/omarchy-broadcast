//! ONNX Runtime, opened at run time.
//!
//! The library is found, in order: `$ORT_DYLIB_PATH` when it is set; else
//! the module configuration's `ort_library`; else `libonnxruntime.so.1`,
//! then `libonnxruntime.so`, from the system's library path. The probe
//! looks for the file only ([`find_library`]); the runtime is opened by the
//! module's `check` or the first load ([`runtime`]), once per process: ONNX
//! Runtime allows one environment per process, so the first library opened
//! stays for the life of the daemon.
//!
//! The runtime's log goes to the daemon's log (warnings and errors only),
//! and the recent warnings are kept ([`take_warnings`]) so that the reason
//! of an engine that did not start can quote what ONNX Runtime said.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use obc_module_sdk::log;
use ort::environment::Environment;
use ort::logging::LogLevel;

/// The environment variable naming the ONNX Runtime library.
pub const DYLIB_ENV: &str = "ORT_DYLIB_PATH";
/// The library names tried on the system's library path, in order.
pub const SYSTEM_NAMES: [&str; 2] = ["libonnxruntime.so.1", "libonnxruntime.so"];
/// The reason given when no library is found.
pub const REASON_NOT_INSTALLED: &str = "ONNX Runtime not installed (libonnxruntime.so not found)";

/// Where the library is to be looked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibrarySource {
    /// A path from `$ORT_DYLIB_PATH`.
    Env(PathBuf),
    /// A path from the module configuration.
    Config(PathBuf),
    /// The system's library path.
    System,
}

/// Where to look, from the environment's `ORT_DYLIB_PATH` (`env`) and the
/// configured path.
pub fn library_source(env: Option<std::ffi::OsString>, configured: Option<&Path>) -> LibrarySource {
    if let Some(p) = env.filter(|v| !v.is_empty()) {
        return LibrarySource::Env(PathBuf::from(p));
    }
    match configured.filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => LibrarySource::Config(p.to_path_buf()),
        None => LibrarySource::System,
    }
}

/// The directories the dynamic loader searches, as far as a file check can
/// tell: `$LD_LIBRARY_PATH`, those of `/etc/ld.so.conf` (and the files it
/// includes from `/etc/ld.so.conf.d`), then the default ones.
pub fn loader_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("LD_LIBRARY_PATH")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mut conf = vec![PathBuf::from("/etc/ld.so.conf")];
    if let Ok(rd) = std::fs::read_dir("/etc/ld.so.conf.d") {
        let mut more: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "conf"))
            .collect();
        more.sort();
        conf.extend(more);
    }
    for c in conf {
        if let Ok(text) = std::fs::read_to_string(&c) {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if line.starts_with('/') {
                    dirs.push(PathBuf::from(line));
                }
            }
        }
    }
    for d in ["/usr/lib", "/usr/lib64", "/lib", "/lib64", "/usr/local/lib"] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// The library file `source` names, by a filesystem check only: the path
/// given, or the first system name found in `dirs`. The error is the
/// reason the stage cannot run.
pub fn find_library(source: &LibrarySource, dirs: &[PathBuf]) -> Result<PathBuf, String> {
    match source {
        LibrarySource::Env(p) | LibrarySource::Config(p) => {
            if p.is_file() {
                Ok(p.clone())
            } else {
                let from = match source {
                    LibrarySource::Env(_) => DYLIB_ENV.to_string(),
                    _ => "the module configuration's ort_library".to_string(),
                };
                Err(format!(
                    "ONNX Runtime not installed (libonnxruntime.so not found at {}, from {from})",
                    p.display()
                ))
            }
        }
        LibrarySource::System => SYSTEM_NAMES
            .iter()
            .flat_map(|n| dirs.iter().map(move |d| d.join(n)))
            .find(|p| p.is_file())
            .ok_or_else(|| REASON_NOT_INSTALLED.to_string()),
    }
}

/// The loaded runtime: the process's environment, the library, its
/// version and the execution providers it was built with.
pub struct Runtime {
    /// The environment every session is created in.
    pub env: Environment,
    /// The library opened.
    pub library: PathBuf,
    /// The runtime's version (`1.31.1`).
    pub version: String,
    /// The execution providers of this build, as ONNX Runtime names them
    /// (`CUDAExecutionProvider`), from `GetAvailableProviders`.
    pub providers: Vec<String>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("library", &self.library)
            .field("version", &self.version)
            .field("providers", &self.providers)
            .finish()
    }
}

impl Runtime {
    /// Whether this build has the provider `name`.
    pub fn has_provider(&self, name: &str) -> bool {
        self.providers.iter().any(|p| p == name)
    }
}

static RUNTIME: OnceLock<Result<Arc<Runtime>, String>> = OnceLock::new();
static OPEN_LOCK: Mutex<()> = Mutex::new(());

/// The process's runtime, opened from `source` the first time; a failure
/// is not remembered (a later call tries again, for a library installed
/// meanwhile), a success is.
pub fn runtime(source: &LibrarySource) -> Result<Arc<Runtime>, String> {
    if let Some(Ok(rt)) = RUNTIME.get() {
        return Ok(rt.clone());
    }
    let _serial = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(Ok(rt)) = RUNTIME.get() {
        return Ok(rt.clone());
    }
    let rt = open(source)?;
    let rt = Arc::new(rt);
    // Only this function sets it, under the lock, and only on success.
    let _ = RUNTIME.set(Ok(rt.clone()));
    Ok(rt)
}

/// The runtime if it is already open.
pub fn opened() -> Option<Arc<Runtime>> {
    match RUNTIME.get() {
        Some(Ok(rt)) => Some(rt.clone()),
        _ => None,
    }
}

fn candidates(source: &LibrarySource) -> Vec<PathBuf> {
    match source {
        LibrarySource::Env(p) | LibrarySource::Config(p) => vec![p.clone()],
        LibrarySource::System => SYSTEM_NAMES.iter().map(PathBuf::from).collect(),
    }
}

fn open(source: &LibrarySource) -> Result<Runtime, String> {
    if let LibrarySource::Env(_) | LibrarySource::Config(_) = source {
        find_library(source, &[])?;
    }
    let mut errors = Vec::new();
    for path in candidates(source) {
        match ort::init_from(&path) {
            Ok(builder) => {
                let logger: ort::logging::LoggerFunction = Arc::new(ort_log);
                let env = builder
                    .with_name("omarchy-broadcast")
                    .with_telemetry(false)
                    .with_logger(logger)
                    .build()
                    .map_err(|e| format!("ONNX Runtime ({}) did not start: {e}", path.display()))?;
                // `ort` creates a custom-logger environment at the verbose
                // level; only warnings and errors are wanted.
                set_env_log_level(&env);
                let version = version_string(&path).unwrap_or_else(|| "unknown".into());
                let providers = available_providers().map_err(|e| {
                    format!("ONNX Runtime ({}) lists no providers: {e}", path.display())
                })?;
                log::info!(
                    "ONNX Runtime {version} from {}; its execution providers: {}",
                    path.display(),
                    providers.join(", ")
                );
                return Ok(Runtime {
                    env,
                    library: path,
                    version,
                    providers,
                });
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
    match source {
        LibrarySource::System if errors.iter().all(|e| e.contains("failed to load")) => {
            Err(format!("{REASON_NOT_INSTALLED}: {}", errors.join("; ")))
        }
        _ => Err(format!(
            "ONNX Runtime could not be loaded: {}",
            errors.join("; ")
        )),
    }
}

fn set_env_log_level(env: &Environment) {
    use ort::AsPointer;
    let f = ort::api().UpdateEnvWithCustomLogLevel;
    // SAFETY: a live environment of the loaded runtime; the status, if any,
    // is released.
    let status = unsafe {
        f(
            env.ptr().cast_mut(),
            ort::sys::OrtLoggingLevel::ORT_LOGGING_LEVEL_WARNING,
        )
    };
    release_status(status);
}

/// Releases an `OrtStatus`, returning its message when there was one.
fn release_status(status: ort::sys::OrtStatusPtr) -> Option<String> {
    if status.0.is_null() {
        return None;
    }
    let api = ort::api();
    // SAFETY: a status returned by the runtime, read then released once.
    unsafe {
        let msg = (api.GetErrorMessage)(status.0);
        let text = if msg.is_null() {
            "unknown error".to_string()
        } else {
            CStr::from_ptr(msg).to_string_lossy().into_owned()
        };
        (api.ReleaseStatus)(status.0);
        Some(text)
    }
}

/// `GetAvailableProviders`: the providers compiled into the loaded
/// runtime. Asks nothing of any device.
fn available_providers() -> Result<Vec<String>, String> {
    let api = ort::api();
    let mut list: *mut *mut c_char = std::ptr::null_mut();
    let mut n: c_int = 0;
    // SAFETY: valid out-pointers; the list is released below.
    let status = unsafe { (api.GetAvailableProviders)(&mut list, &mut n) };
    if let Some(e) = release_status(status) {
        return Err(e);
    }
    if list.is_null() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n.max(0) as usize {
        // SAFETY: `n` entries, each a NUL-terminated string.
        let p = unsafe { *list.add(i) };
        if !p.is_null() {
            out.push(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
        }
    }
    // SAFETY: the list returned above, released once.
    release_status(unsafe { (api.ReleaseAvailableProviders)(list, n) });
    Ok(out)
}

/// The runtime's version, from `OrtGetApiBase()->GetVersionString` of the
/// library `ort` opened (opening it again returns the same handle).
fn version_string(path: &Path) -> Option<String> {
    // SAFETY: the library is already loaded by `ort`; this takes another
    // reference to it and calls a function without preconditions.
    unsafe {
        let lib = libloading::Library::new(path).ok()?;
        let get: libloading::Symbol<unsafe extern "C" fn() -> *const ort::sys::OrtApiBase> =
            lib.get(b"OrtGetApiBase").ok()?;
        let base = get();
        if base.is_null() {
            return None;
        }
        let v = ((*base).GetVersionString)();
        if v.is_null() {
            return None;
        }
        Some(CStr::from_ptr(v).to_string_lossy().into_owned())
    }
}

/// The TensorRT library names tried, newest first.
const TENSORRT_NAMES: [&str; 2] = ["libnvinfer.so.11", "libnvinfer.so.10"];

/// glibc's `RTLD_NOLOAD` (not among `libloading`'s constants): open a
/// library only if it is loaded already.
const RTLD_NOLOAD: c_int = 0x4;

/// The TensorRT library names `provider` (ONNX Runtime's TensorRT
/// provider library) is linked against, newest first: their soname
/// strings in the file, which is small (about 1 MB). Every name when the
/// file cannot be read or names none.
pub fn tensorrt_names_linked(provider: &Path) -> Vec<&'static str> {
    let Ok(bytes) = std::fs::read(provider) else {
        return TENSORRT_NAMES.to_vec();
    };
    let linked: Vec<&str> = TENSORRT_NAMES
        .into_iter()
        .filter(|name| {
            let needle = [name.as_bytes(), b"\0"].concat();
            bytes.windows(needle.len()).any(|w| w == needle.as_slice())
        })
        .collect();
    if linked.is_empty() {
        TENSORRT_NAMES.to_vec()
    } else {
        linked
    }
}

/// TensorRT's version (`10.16.1`), from `getInferLibVersion` of the
/// `libnvinfer` ONNX Runtime's TensorRT provider (beside `library`) uses.
/// `None` when no TensorRT library is found.
///
/// A `libnvinfer` already in the process (the provider's, once a TensorRT
/// session was made) is read without loading anything. Before that, the
/// one the provider is linked against is opened by its soname, newest
/// first, and kept: the provider would load it at its first session
/// anyway, and the loader then reuses this one (same soname). That is the
/// library the provider's own lookup finds: ONNX Runtime's provider
/// libraries carry no `RUNPATH`, so both lookups search the same path
/// (`LD_LIBRARY_PATH`, then the loader's cache).
pub fn tensorrt_version(library: &Path) -> Option<String> {
    static VERSION: OnceLock<Option<String>> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            let read = |lib: libloading::Library| {
                // SAFETY: `getInferLibVersion` takes nothing and returns an
                // int. The library is kept for the process.
                unsafe {
                    let get: libloading::Symbol<unsafe extern "C" fn() -> c_int> =
                        lib.get(b"getInferLibVersion").ok()?;
                    let v = get();
                    std::mem::forget(lib);
                    Some(tensorrt_version_string(v))
                }
            };
            let loaded = TENSORRT_NAMES.iter().find_map(|name| {
                // SAFETY: opens only a library already loaded (its
                // initialisers have run); takes another reference to it.
                let lib = unsafe {
                    libloading::os::unix::Library::open(
                        Some(*name),
                        libloading::os::unix::RTLD_LAZY | RTLD_NOLOAD,
                    )
                }
                .ok()?;
                read(lib.into())
            });
            if loaded.is_some() {
                return loaded;
            }
            let dir = library.parent().unwrap_or(Path::new("/"));
            let provider = dir.join("libonnxruntime_providers_tensorrt.so");
            tensorrt_names_linked(&provider)
                .into_iter()
                .find_map(|name| {
                    // SAFETY: loading TensorRT runs its initialisers, which
                    // have no preconditions.
                    let lib = unsafe { libloading::Library::new(name) }.ok()?;
                    read(lib)
                })
        })
        .clone()
}

/// `getInferLibVersion`'s number as a version: TensorRT 10 and later give
/// `major * 10000 + minor * 100 + patch`.
pub fn tensorrt_version_string(v: c_int) -> String {
    format!("{}.{}.{}", v / 10000, (v / 100) % 100, v % 100)
}

const KEPT_WARNINGS: usize = 16;
static WARNINGS: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Whether a runtime warning is noise about the model file, not about
/// running it (said of every load of some of the models).
fn is_noise(message: &str) -> bool {
    message.contains("Removing initializer") || message.contains("is not used by any node")
}

fn ort_log(level: LogLevel, _category: &str, _id: &str, _location: &str, message: &str) {
    if is_noise(message) {
        log::debug!("ONNX Runtime: {message}");
        return;
    }
    match level {
        LogLevel::Warning => {
            log::warn!("ONNX Runtime: {message}");
            let mut w = WARNINGS.lock().unwrap_or_else(|e| e.into_inner());
            if w.len() == KEPT_WARNINGS {
                w.pop_front();
            }
            w.push_back(message.to_string());
        }
        LogLevel::Error | LogLevel::Fatal => {
            log::error!("ONNX Runtime: {message}");
            let mut w = WARNINGS.lock().unwrap_or_else(|e| e.into_inner());
            if w.len() == KEPT_WARNINGS {
                w.pop_front();
            }
            w.push_back(message.to_string());
        }
        LogLevel::Info | LogLevel::Verbose => {}
    }
}

/// The warnings and errors ONNX Runtime logged since the last call
/// (at most the last few), oldest first.
pub fn take_warnings() -> Vec<String> {
    WARNINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_path_wins_over_the_configured_one() {
        let cfg = Path::new("/cfg/libonnxruntime.so");
        assert_eq!(
            library_source(Some("/env/lib.so".into()), Some(cfg)),
            LibrarySource::Env("/env/lib.so".into())
        );
        assert_eq!(
            library_source(Some("".into()), Some(cfg)),
            LibrarySource::Config(cfg.into())
        );
        assert_eq!(library_source(None, None), LibrarySource::System);
        assert_eq!(
            library_source(None, Some(Path::new(""))),
            LibrarySource::System
        );
    }

    #[test]
    fn a_missing_library_is_the_documented_reason() {
        let d = tempfile::tempdir().unwrap();
        let dirs = vec![d.path().to_path_buf()];
        assert_eq!(
            find_library(&LibrarySource::System, &dirs).unwrap_err(),
            REASON_NOT_INSTALLED
        );
        std::fs::write(d.path().join("libonnxruntime.so"), b"").unwrap();
        assert_eq!(
            find_library(&LibrarySource::System, &dirs).unwrap(),
            d.path().join("libonnxruntime.so")
        );
        // The versioned name first.
        std::fs::write(d.path().join("libonnxruntime.so.1"), b"").unwrap();
        assert_eq!(
            find_library(&LibrarySource::System, &dirs).unwrap(),
            d.path().join("libonnxruntime.so.1")
        );
        let e = find_library(&LibrarySource::Env(d.path().join("nope.so")), &dirs).unwrap_err();
        assert!(
            e.starts_with("ONNX Runtime not installed (libonnxruntime.so not found at"),
            "{e}"
        );
        assert!(e.contains(DYLIB_ENV), "{e}");
        let e = find_library(&LibrarySource::Config(d.path().join("nope.so")), &dirs).unwrap_err();
        assert!(e.contains("ort_library"), "{e}");
    }

    #[test]
    fn the_tensorrt_version_is_read_from_its_number() {
        assert_eq!(tensorrt_version_string(101601), "10.16.1");
        assert_eq!(tensorrt_version_string(110002), "11.0.2");
    }

    #[test]
    fn the_tensorrt_names_are_the_ones_the_provider_links() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("libonnxruntime_providers_tensorrt.so");
        std::fs::write(
            &p,
            b"\x7fELF..libcublas.so.13\0libnvinfer.so.10\0libc.so.6\0",
        )
        .unwrap();
        assert_eq!(tensorrt_names_linked(&p), ["libnvinfer.so.10"]);
        std::fs::write(&p, b"libnvinfer.so.11\0libnvinfer.so.10\0").unwrap();
        assert_eq!(tensorrt_names_linked(&p), TENSORRT_NAMES);
        // A prefix of another name is not that name.
        std::fs::write(&p, b"libnvinfer.so.100\0").unwrap();
        assert_eq!(tensorrt_names_linked(&p), TENSORRT_NAMES);
        assert_eq!(
            tensorrt_names_linked(&dir.path().join("missing.so")),
            TENSORRT_NAMES
        );
    }

    #[test]
    fn loader_dirs_end_with_the_defaults() {
        let dirs = loader_dirs();
        assert!(dirs.contains(&PathBuf::from("/usr/lib")));
    }
}
