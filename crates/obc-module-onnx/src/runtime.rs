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

/// The process's runtime, its environment among it, kept for the life of
/// the process and never released: releasing ONNX Runtime's environment
/// after the OpenVINO provider was used crashes the process at exit
/// (`SIGSEGV`; ONNX Runtime 1.29 with OpenVINO 2026.4: the release unloads
/// the provider and OpenVINO's plugins while OpenVINO's threads and
/// thread-local destructors still run their code). So this stays a
/// `static` that is never dropped (the host opens modules with
/// `RTLD_NODELETE`, so not even at unload): never a `take()`, never a
/// `Drop` path, never a field the module or an instance owns alone.
/// `tests/exit.rs` checks that a process exits cleanly after OpenVINO ran.
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
                // A bare name (the system path) is not a place: the
                // provider libraries are looked for beside the file the
                // loader found, as ONNX Runtime itself does.
                let path = resolved_library(&path);
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

/// The file of the loaded library `opened` (a path, or a bare name the
/// loader searched for): where ONNX Runtime looks for its provider
/// libraries. A path is kept as given (ONNX Runtime does not resolve
/// symbolic links either); a bare name is the file the loader mapped, by
/// `dladdr` of `OrtGetApiBase`, else the first of the loader's
/// directories that has it, else the name itself.
pub fn resolved_library(opened: &Path) -> PathBuf {
    if opened.components().count() > 1 {
        return opened.to_path_buf();
    }
    let name = opened.to_string_lossy();
    loaded_file(&name, "OrtGetApiBase")
        .or_else(|| {
            loader_dirs()
                .into_iter()
                .map(|d| d.join(opened))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| opened.to_path_buf())
}

/// glibc's `Dl_info`, as `dladdr` fills it.
#[repr(C)]
struct DlInfo {
    fname: *const c_char,
    fbase: *mut std::ffi::c_void,
    sname: *const c_char,
    saddr: *mut std::ffi::c_void,
}

unsafe extern "C" {
    fn dladdr(addr: *const std::ffi::c_void, info: *mut DlInfo) -> c_int;
}

/// The absolute path of the file the already-loaded library `name` was
/// mapped from: `dladdr` of its `symbol`. `None` when it is not loaded,
/// has no such symbol, or the loader gives no absolute path.
pub fn loaded_file(name: &str, symbol: &str) -> Option<PathBuf> {
    // SAFETY: opens only a library already loaded (its initialisers have
    // run) and takes another reference to it, dropped at the end; the
    // symbol's address is only passed to `dladdr`, never called.
    unsafe {
        let lib = libloading::os::unix::Library::open(
            Some(name),
            libloading::os::unix::RTLD_LAZY | RTLD_NOLOAD,
        )
        .ok()?;
        let sym: libloading::os::unix::Symbol<*const std::ffi::c_void> =
            lib.get(symbol.as_bytes()).ok()?;
        let mut info = DlInfo {
            fname: std::ptr::null(),
            fbase: std::ptr::null_mut(),
            sname: std::ptr::null(),
            saddr: std::ptr::null_mut(),
        };
        if dladdr(*sym, &mut info) == 0 || info.fname.is_null() {
            return None;
        }
        let path = PathBuf::from(
            std::ffi::OsStr::from_encoded_bytes_unchecked(CStr::from_ptr(info.fname).to_bytes())
                .to_os_string(),
        );
        path.is_absolute().then_some(path)
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
/// anyway, and the loader then reuses this one (same soname), so the
/// version read is that of the library the provider runs.
///
/// This lookup is the module's, which has no `RUNPATH`: it searches
/// `LD_LIBRARY_PATH`, then the loader's cache and default directories. A
/// provider library may carry one (the omarchy-onnxruntime TensorRT provider
/// has `RUNPATH /opt/nvidia/tensorrt/lib`) and find `libnvinfer` where
/// this lookup does not; then this is `None`, and TensorRT is not started
/// ([`crate::engine`]): `libnvinfer`'s own `dlopen` of its builder
/// resource does not search the provider's `RUNPATH` either, and fails.
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
            let dir = library.parent().unwrap_or(Path::new("/"));
            let provider = dir.join("libonnxruntime_providers_tensorrt.so");
            // Only the sonames the provider links, in both lookups: another
            // module may have loaded a different major (Maxine's own
            // `libnvinfer.so.10` beside a provider linked to `.so.11`), and
            // its version would key engines the provider builds. When the
            // provider's links cannot be determined this is every name,
            // as before.
            let names = tensorrt_names_linked(&provider);
            let loaded = names.iter().find_map(|name| {
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
            names.into_iter().find_map(|name| {
                // SAFETY: loading TensorRT runs its initialisers, which
                // have no preconditions.
                let lib = unsafe { libloading::Library::new(name) }.ok()?;
                read(lib)
            })
        })
        .clone()
}

/// The names TensorRT `version` (`11.4.0`) asks for its builder resource
/// library by, on a GPU of compute capability `cc`.
///
/// `libnvinfer` opens it with `dlopen` by bare name when a builder is
/// created, which ONNX Runtime's TensorRT provider does for an engine read
/// from the cache too. The name is built at run time (seen with
/// `LD_DEBUG=files`, and from the fragments in the library's strings):
/// TensorRT 11 has one library per GPU architecture,
/// `libnvinfer_builder_resource_sm<major><minor>.so.<version>`, and asks for
/// that one only (with it missing it fails; it does not fall back to the
/// `_ptx` library beside it). TensorRT 10 (10.9) has a single
/// `libnvinfer_builder_resource.so.<version>`; the per-GPU name is
/// accepted for 10 too, for a later 10.x that may have split it: the
/// version in the name already ties either to that one release.
pub fn tensorrt_builder_resource_names(version: &str, cc: (i32, i32)) -> Vec<String> {
    let major: u32 = version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    let per_gpu = format!(
        "libnvinfer_builder_resource_sm{}{}.so.{version}",
        cc.0, cc.1
    );
    if major >= 11 {
        vec![per_gpu]
    } else {
        vec![format!("libnvinfer_builder_resource.so.{version}"), per_gpu]
    }
}

/// The builder resource libraries of TensorRT `version` (`11.4.0`) in
/// `dirs` (the loader's search directories, [`loader_dirs`]): their file
/// names, `libnvinfer_builder_resource*.so.<version>`, sorted and without
/// duplicates. A look at the files, so that whether a library is on the
/// path does not depend on the loader's message, which is translated.
pub fn builder_resources_reachable(version: &str, dirs: &[PathBuf]) -> Vec<String> {
    let suffix = format!(".so.{version}");
    let mut found: Vec<String> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("libnvinfer_builder_resource") && n.ends_with(&suffix))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Opens the library `name` by bare name, as another library's `dlopen`
/// of that name would find it, and keeps it loaded: such a `dlopen` then
/// matches the name already loaded. (Opening the file by its path would
/// not do: a TensorRT builder resource's soname is
/// `do_not_link_against_...`, so a later `dlopen` by the file's name does
/// not match it.) The loader's message when it cannot be opened.
///
/// # Safety
///
/// Loading `name` runs its initialisers, and the library is never
/// unloaded. The caller opens only libraries whose initialisers have no
/// preconditions and which may stay loaded for the life of the process
/// (TensorRT's builder resource libraries, which hold data).
pub(crate) unsafe fn load_kept(name: &str) -> Result<(), String> {
    // SAFETY: the caller's obligation (above).
    match unsafe {
        libloading::os::unix::Library::open(Some(name), libloading::os::unix::RTLD_LAZY)
    } {
        Ok(lib) => {
            std::mem::forget(lib);
            Ok(())
        }
        // `libloading`'s error says only "dlopen failed"; the loader's own
        // message is its source.
        Err(e) => {
            Err(std::error::Error::source(&e).map_or_else(|| e.to_string(), |s| s.to_string()))
        }
    }
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
        // A provider linked to the newer name alone is not given the older
        // one to probe in the already-loaded libraries.
        std::fs::write(&p, b"libnvinfer.so.11\0libc.so.6\0").unwrap();
        assert_eq!(tensorrt_names_linked(&p), ["libnvinfer.so.11"]);
        // A prefix of another name is not that name.
        std::fs::write(&p, b"libnvinfer.so.100\0").unwrap();
        assert_eq!(tensorrt_names_linked(&p), TENSORRT_NAMES);
        assert_eq!(
            tensorrt_names_linked(&dir.path().join("missing.so")),
            TENSORRT_NAMES
        );
    }

    #[test]
    fn the_builder_resource_is_named_by_tensorrt_version_and_gpu() {
        // TensorRT 11: one library per GPU architecture, no fallback.
        assert_eq!(
            tensorrt_builder_resource_names("11.4.0", (12, 0)),
            ["libnvinfer_builder_resource_sm120.so.11.4.0"]
        );
        assert_eq!(
            tensorrt_builder_resource_names("11.4.0", (8, 6)),
            ["libnvinfer_builder_resource_sm86.so.11.4.0"]
        );
        // TensorRT 10: the single library first.
        assert_eq!(
            tensorrt_builder_resource_names("10.9.0", (12, 0)),
            [
                "libnvinfer_builder_resource.so.10.9.0",
                "libnvinfer_builder_resource_sm120.so.10.9.0"
            ]
        );
    }

    #[test]
    fn a_library_not_on_the_path_is_not_loaded() {
        // SAFETY: no such library exists; nothing is loaded.
        let e = unsafe { load_kept("libobc-no-such-library.so.0") }.unwrap_err();
        assert!(e.contains("libobc-no-such-library.so.0"), "{e}");
    }

    #[test]
    fn the_builder_resources_on_the_path_are_found_by_their_files() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for (dir, name) in [
            (&a, "libnvinfer_builder_resource_sm120.so.11.4.0"),
            (&a, "libnvinfer_builder_resource_ptx.so.11.4.0"),
            (&a, "libnvinfer_builder_resource_sm90.so.10.9.0"),
            (&a, "libnvinfer.so.11.4.0"),
            (&b, "libnvinfer_builder_resource_sm120.so.11.4.0"),
            (&b, "libnvinfer_builder_resource_sm86.so.11.4.0"),
        ] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let dirs = [
            a.path().to_path_buf(),
            b.path().join("missing"),
            b.path().to_path_buf(),
        ];
        assert_eq!(
            builder_resources_reachable("11.4.0", &dirs),
            [
                "libnvinfer_builder_resource_ptx.so.11.4.0",
                "libnvinfer_builder_resource_sm120.so.11.4.0",
                "libnvinfer_builder_resource_sm86.so.11.4.0",
            ]
        );
        assert!(builder_resources_reachable("11.5.0", &dirs).is_empty());
    }

    #[test]
    fn a_bare_name_resolves_to_the_file_the_loader_mapped() {
        // libc is loaded in every test process, by its soname.
        let libc = loaded_file("libc.so.6", "malloc").expect("libc is loaded");
        assert!(libc.is_absolute() && libc.is_file(), "{}", libc.display());
        assert!(
            libc.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("libc"),
            "{}",
            libc.display()
        );
        assert_eq!(loaded_file("libobc-no-such-library.so.0", "malloc"), None);
        assert_eq!(loaded_file("libc.so.6", "obc_no_such_symbol"), None);
    }

    #[test]
    fn a_path_is_kept_and_an_unknown_bare_name_stays_a_name() {
        let p = Path::new("/opt/ort/lib/libonnxruntime.so.1");
        assert_eq!(resolved_library(p), p);
        let n = Path::new("libobc-no-such-library.so.0");
        assert_eq!(resolved_library(n), n);
    }

    /// The no-`ORT_DYLIB_PATH` case: the system's ONNX Runtime opened by
    /// its soname, as `open` does, resolves to its directory, where the
    /// engines' check finds the provider libraries.
    #[test]
    fn the_system_runtime_by_soname_resolves_to_its_directory() {
        let name = SYSTEM_NAMES[0];
        // SAFETY: ONNX Runtime's initialisers have no preconditions; it
        // stays loaded for the test process, as it would for the daemon.
        let Ok(lib) = (unsafe { libloading::Library::new(name) }) else {
            eprintln!("no {name} on the library path: not checked");
            return;
        };
        std::mem::forget(lib);
        let found = resolved_library(Path::new(name));
        assert!(
            found.is_absolute() && found.is_file(),
            "{}",
            found.display()
        );
        let dir = found.parent().unwrap();
        for provider in ["tensorrt", "cuda", "openvino"] {
            let p = dir.join(format!("libonnxruntime_providers_{provider}.so"));
            if p.is_file() {
                assert!(
                    !crate::engine::static_reasons(&found, true)
                        .iter()
                        .any(|(_, r)| r
                            .contains(&format!("no libonnxruntime_providers_{provider}.so"))),
                    "{provider}"
                );
            }
        }
    }

    #[test]
    fn loader_dirs_end_with_the_defaults() {
        let dirs = loader_dirs();
        assert!(dirs.contains(&PathBuf::from("/usr/lib")));
    }
}
