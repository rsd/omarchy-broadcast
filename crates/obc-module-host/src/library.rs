//! Opening a module library and checking it before anything of it runs.
//!
//! [`Library::open`] loads the shared library with
//! `RTLD_NOW | RTLD_LOCAL | RTLD_NODELETE` (every symbol resolved at once,
//! none shared with other libraries, and the code never unmapped), then:
//!
//! 1. calls `obc_module_abi_version` and refuses a library built for another
//!    major version of the ABI, or a newer minor version than this host's,
//!    with a reason that names both versions; the entry point is then never
//!    called;
//! 2. calls `obc_module_entry` with this host's [`obc_host`], whose log
//!    callback routes the module's lines to a [`LogSink`];
//! 3. checks the vtable it returns: at least as large as the vtable of ABI
//!    1.0 (an entry past its `struct_size` is taken as null), of the same
//!    major version, with every required entry set. The audio entries of
//!    ABI 1.2 ([`Library::has_audio`]) are required of an `audio-filter`
//!    module only, which [`crate::Module::open_for`] checks.
//!
//! The [`Library`] is shared ([`Arc`]) by every module object and instance
//! made from it, and closed when the last one goes. One path is opened
//! once at a time: a second open of a library that is still open shares
//! the first one, so the module's process-wide state (its log host, for
//! one) has a single owner.
//!
//! Closing never unloads the code (`RTLD_NODELETE`). A module may keep
//! process-wide state that must not be lost while the process runs (the
//! Maxine module guards the SDK it loaded, which cannot be unloaded and
//! loaded again from another place), and threads or callbacks it left
//! behind must never jump into unmapped code. A library opened again finds
//! its state as it was, and its entry point is called again with a new
//! [`obc_host`]. For the same reason the `obc_host` given to the entry
//! point, and the log target behind it, are never freed: a module may have
//! kept the pointer, and a line it logs after its library was closed still
//! lands in the log (a few bytes per open).

use std::ffi::{CStr, c_char, c_void};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use obc_module_abi::{
    OBC_FRAME_FLAGS_1_0, OBC_FRAME_FLAGS_ALL, OBC_LOG_DEBUG, OBC_LOG_ERROR, OBC_LOG_INFO,
    OBC_LOG_TRACE, OBC_LOG_WARN, OBC_MODULE_ABI_MAJOR, OBC_MODULE_ABI_MINOR,
    OBC_MODULE_ABI_VERSION, obc_host, obc_log_level, obc_module_abi_major, obc_module_abi_minor,
    obc_module_abi_version_fn, obc_module_entry_fn, obc_module_vtable,
};

/// The level of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LogLevel {
    /// An error the user should see.
    Error,
    /// Something unexpected that does not stop the module.
    Warn,
    /// A normal event.
    Info,
    /// Detail for debugging.
    Debug,
    /// Very verbose detail.
    Trace,
}

impl LogLevel {
    /// The level of an ABI log level; an unknown one is [`LogLevel::Info`].
    pub fn from_raw(level: obc_log_level) -> LogLevel {
        match level {
            OBC_LOG_ERROR => LogLevel::Error,
            OBC_LOG_WARN => LogLevel::Warn,
            OBC_LOG_DEBUG => LogLevel::Debug,
            OBC_LOG_TRACE => LogLevel::Trace,
            OBC_LOG_INFO => LogLevel::Info,
            _ => LogLevel::Info,
        }
    }

    /// The lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the log lines of modules go: called with the module id, the level
/// and the line, from any thread. The lines of the module itself and those
/// the host writes about it (a setting dropped, a stage ignored) both go
/// there.
pub type LogSink = Arc<dyn Fn(&str, LogLevel, &str) + Send + Sync>;

/// A [`LogSink`] that writes `<module id>: <level>: <line>` to standard
/// error.
pub fn stderr_log() -> LogSink {
    Arc::new(|id, level, msg| eprintln!("{id}: {level}: {msg}"))
}

/// What the log callback of a library reads through `obc_host::user`.
struct LogTarget {
    id: String,
    sink: LogSink,
}

/// The host's log callback.
unsafe extern "C" fn host_log(user: *mut c_void, level: obc_log_level, msg: *const c_char) {
    // Nothing may unwind into the module.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if user.is_null() || msg.is_null() {
            return;
        }
        // SAFETY: `user` is the leaked `LogTarget` of the library's open,
        // alive for the rest of the process, never written.
        let target = unsafe { &*user.cast::<LogTarget>() };
        // SAFETY: the ABI's contract: a NUL-terminated string, valid during
        // the call.
        let msg = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
        (target.sink)(&target.id, LogLevel::from_raw(level), &msg);
    }));
}

/// An ABI version as `major.minor`.
pub fn version_text(version: u32) -> String {
    format!(
        "{}.{}",
        obc_module_abi_major(version),
        obc_module_abi_minor(version)
    )
}

/// Whether a library built for ABI `version` can be used by this host, and
/// why not: another major version, or a newer minor version.
pub fn check_abi_version(version: u32) -> Result<(), String> {
    let (major, minor) = (
        u32::from(obc_module_abi_major(version)),
        u32::from(obc_module_abi_minor(version)),
    );
    let host = version_text(OBC_MODULE_ABI_VERSION);
    if major != OBC_MODULE_ABI_MAJOR {
        return Err(format!(
            "The library is built for module ABI {}; this daemon supports module ABI {host}",
            version_text(version)
        ));
    }
    if minor > OBC_MODULE_ABI_MINOR {
        return Err(format!(
            "The library is built for module ABI {}, newer than this daemon's module ABI {host}",
            version_text(version)
        ));
    }
    Ok(())
}

/// The `OBC_FRAME_FLAG_*` bits a module built for ABI `version` (one this
/// host accepts, [`check_abi_version`]) knows: the host clears the others
/// on every frame it hands the module, so that a module of an older minor
/// version never sees a flag added after it. ABI 1.0 knows
/// `OBC_FRAME_FLAG_NEW_SOURCE`; 1.1 adds `OBC_FRAME_FLAG_REPEAT`.
pub fn frame_flags_of(version: u32) -> u32 {
    if obc_module_abi_minor(version) == 0 {
        OBC_FRAME_FLAGS_1_0
    } else {
        OBC_FRAME_FLAGS_ALL
    }
}

/// The size of the vtable of ABI 1.0, the smallest a host of this major
/// version accepts: what a module built for 1.0 sets in `struct_size`. It is
/// fixed: later minors append entries and must not move it.
pub const VTABLE_SIZE_1_0: usize = 136;

// `check` is the last entry of ABI 1.0, so the 1.0 vtable ends right after it.
const _: () = assert!(
    std::mem::offset_of!(obc_module_vtable, check)
        + size_of::<
            Option<
                unsafe extern "C" fn(
                    *mut obc_module_abi::obc_module,
                    *const *const c_char,
                    usize,
                ) -> obc_module_abi::obc_status,
            >,
        >()
        == VTABLE_SIZE_1_0
);

/// The size of the vtable of ABI 1.2, which appends the audio entries
/// (`audio_instance_load`, `audio_instance_process`) to the 1.0 vtable
/// (1.1 added no entry).
pub const VTABLE_SIZE_1_2: usize = VTABLE_SIZE_1_0 + 2 * size_of::<usize>();

// `audio_instance_process` is the last entry of ABI 1.2.
const _: () = assert!(
    std::mem::offset_of!(obc_module_vtable, audio_instance_load) == VTABLE_SIZE_1_0
        && std::mem::offset_of!(obc_module_vtable, audio_instance_process) + size_of::<usize>()
            == VTABLE_SIZE_1_2
);

/// The size of the vtable of ABI 1.3, which appends
/// `instance_status_values` to the 1.2 vtable.
pub const VTABLE_SIZE_1_3: usize = VTABLE_SIZE_1_2 + size_of::<usize>();

// `instance_status_values` is the last entry of ABI 1.3.
const _: () = assert!(
    std::mem::offset_of!(obc_module_vtable, instance_status_values) == VTABLE_SIZE_1_2
        && std::mem::offset_of!(obc_module_vtable, instance_status_values) + size_of::<usize>()
            == VTABLE_SIZE_1_3
);

/// The first minor version of ABI 1 with audio.
pub const AUDIO_MINOR: u16 = 2;

/// A loaded, checked module library.
pub struct Library {
    _lib: libloading::Library,
    vtable: obc_module_vtable,
    path: PathBuf,
    abi_version: u32,
}

// SAFETY: after `open`, nothing of a `Library` is written. The vtable holds
// function pointers that the ABI allows to call from any thread (the
// serialisation it asks for is done by the module and instance handles),
// and the host structure's pointers are only read by the module.
unsafe impl Send for Library {}
// SAFETY: as above; shared reads only.
unsafe impl Sync for Library {}

impl fmt::Debug for Library {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Library")
            .field("path", &self.path)
            .field("abi_version", &version_text(self.abi_version))
            .finish_non_exhaustive()
    }
}

/// How module libraries are opened: see the module documentation.
pub const OPEN_FLAGS: i32 =
    libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL | RTLD_NODELETE;

/// `RTLD_NODELETE`, which `libloading` does not export: 0x1000 in glibc and
/// musl on Linux.
const RTLD_NODELETE: i32 = 0x1000;

/// The libraries loaded now, by path, to share a second open.
static OPEN: Mutex<Vec<(PathBuf, Weak<Library>)>> = Mutex::new(Vec::new());

/// The vtable at `raw`, whose `struct_size` is `size`: the entries that fit
/// in `size`, the others null (an older minor version's vtable lacks the
/// entries appended since).
///
/// # Safety
///
/// `raw` points to at least `size` readable bytes.
unsafe fn read_vtable(raw: *const obc_module_vtable, size: usize) -> obc_module_vtable {
    let mut vt = std::mem::MaybeUninit::<obc_module_vtable>::zeroed();
    let n = size.min(size_of::<obc_module_vtable>());
    // SAFETY: `n` bytes are readable at `raw` (the caller's contract) and
    // writable in `vt`; the two do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(raw.cast::<u8>(), vt.as_mut_ptr().cast::<u8>(), n) };
    // SAFETY: every field is an integer or a nullable function pointer,
    // for which all zero bytes are a valid value (0 and `None`).
    unsafe { vt.assume_init() }
}

/// The entries of the vtable a module must set, by name.
fn missing_required(vt: &obc_module_vtable) -> Vec<&'static str> {
    let required = [
        ("init", vt.init.is_some()),
        ("probe", vt.probe.is_some()),
        ("describe_settings", vt.describe_settings.is_some()),
        ("instance_create", vt.instance_create.is_some()),
        ("instance_destroy", vt.instance_destroy.is_some()),
        ("instance_load", vt.instance_load.is_some()),
        ("instance_process", vt.instance_process.is_some()),
        (
            "instance_reset_temporal",
            vt.instance_reset_temporal.is_some(),
        ),
        ("instance_unload", vt.instance_unload.is_some()),
        ("last_error", vt.last_error.is_some()),
        ("instance_last_error", vt.instance_last_error.is_some()),
        ("destroy", vt.destroy.is_some()),
    ];
    required
        .into_iter()
        .filter(|(_, set)| !set)
        .map(|(name, _)| name)
        .collect()
}

impl Library {
    /// Opens and checks the library at `path` for module `id`, whose log
    /// lines go to `log` (see the module documentation). The error is the
    /// reason shown for the module. A library of this path that is still
    /// loaded is shared, with the log target of its first open.
    pub fn open(path: &Path, id: &str, log: LogSink) -> Result<Arc<Library>, String> {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        open.retain(|(_, weak)| weak.strong_count() > 0);
        if let Some(lib) = open
            .iter()
            .find(|(p, _)| *p == key)
            .and_then(|(_, weak)| weak.upgrade())
        {
            return Ok(lib);
        }
        let lib = Arc::new(Library::open_new(path, id, log)?);
        open.push((key, Arc::downgrade(&lib)));
        Ok(lib)
    }

    fn open_new(path: &Path, id: &str, log: LogSink) -> Result<Library, String> {
        if !path.is_file() {
            return Err(format!("The library {} is not installed", path.display()));
        }
        use libloading::os::unix::Library as UnixLibrary;
        // SAFETY: loading a library runs its initialisers; that is what
        // installing a module consents to. No symbol is used before the
        // checks below.
        let lib: libloading::Library = unsafe { UnixLibrary::open(Some(path), OPEN_FLAGS) }
            .map_err(|e| format!("The library {} could not be loaded: {e}", path.display()))?
            .into();

        let symbol = |name: &str| format!("{name}\0");
        // SAFETY: the symbol has the ABI's type, `obc_module_abi_version_fn`;
        // a library that exports it under another type breaks the ABI.
        let version_fn: obc_module_abi_version_fn = unsafe {
            lib.get::<obc_module_abi_version_fn>(
                symbol(obc_module_abi::OBC_MODULE_ABI_VERSION_SYMBOL).as_bytes(),
            )
        }
        .map(|f| *f)
        .map_err(|_| {
            format!(
                "The library {} is not an omarchy-broadcast module (it does not export {})",
                path.display(),
                obc_module_abi::OBC_MODULE_ABI_VERSION_SYMBOL
            )
        })?;
        // SAFETY: side-effect free by the ABI.
        let abi_version = unsafe { version_fn() };
        check_abi_version(abi_version)?;
        // SAFETY: as for the version function, `obc_module_entry_fn`.
        let entry: obc_module_entry_fn = unsafe {
            lib.get::<obc_module_entry_fn>(
                symbol(obc_module_abi::OBC_MODULE_ENTRY_SYMBOL).as_bytes(),
            )
        }
        .map(|f| *f)
        .map_err(|_| {
            format!(
                "The library {} does not export {}",
                path.display(),
                obc_module_abi::OBC_MODULE_ENTRY_SYMBOL
            )
        })?;
        // Leaked, never freed: see the module documentation.
        let log: &'static LogTarget = Box::leak(Box::new(LogTarget {
            id: id.to_string(),
            sink: log,
        }));
        let mut host = obc_host::empty();
        host.user = (log as *const LogTarget).cast_mut().cast::<c_void>();
        host.log = Some(host_log);
        let host: &'static obc_host = Box::leak(Box::new(host));
        // SAFETY: `host` and its log target live as long as the process,
        // longer than the library's code (`RTLD_NODELETE`).
        let raw = unsafe { entry(host) };
        if raw.is_null() {
            return Err(format!(
                "The module refused this daemon (module ABI {})",
                version_text(OBC_MODULE_ABI_VERSION)
            ));
        }
        // SAFETY: non-null; the first field is read before the size is
        // known.
        let size = unsafe { (*raw).struct_size } as usize;
        if size < VTABLE_SIZE_1_0 {
            return Err(format!(
                "The module's vtable is {size} bytes, smaller than the {VTABLE_SIZE_1_0} of \
                 module ABI 1.0"
            ));
        }
        // SAFETY: `size` bytes, at least the 1.0 vtable's, valid while the
        // library is loaded; copied.
        let vtable = unsafe { read_vtable(raw, size) };
        if u32::from(obc_module_abi_major(vtable.abi_version)) != OBC_MODULE_ABI_MAJOR {
            return Err(format!(
                "The module's vtable is of module ABI {}; this daemon supports module ABI {}",
                version_text(vtable.abi_version),
                version_text(OBC_MODULE_ABI_VERSION)
            ));
        }
        let missing = missing_required(&vtable);
        if !missing.is_empty() {
            return Err(format!(
                "The module's vtable lacks required entries: {}",
                missing.join(", ")
            ));
        }
        Ok(Library {
            _lib: lib,
            vtable,
            path: path.to_path_buf(),
            abi_version,
        })
    }

    /// The vtable; every required entry is set.
    pub(crate) fn vtable(&self) -> &obc_module_vtable {
        &self.vtable
    }

    /// The library's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The packed ABI version the library is built for.
    pub fn abi_version(&self) -> u32 {
        self.abi_version
    }

    /// Whether the vtable has both audio entries of ABI 1.2: what an
    /// `audio-filter` module needs. A library of ABI 1.0 or 1.1 never has
    /// them (its vtable ends before them).
    pub fn has_audio(&self) -> bool {
        self.vtable.audio_instance_load.is_some() && self.vtable.audio_instance_process.is_some()
    }

    /// Why this library cannot be an `audio-filter` module, or `None` when
    /// it can: an ABI version before 1.2, or a vtable without the audio
    /// entries.
    pub fn audio_refusal(&self) -> Option<String> {
        if obc_module_abi_minor(self.abi_version) < AUDIO_MINOR {
            return Some(format!(
                "The library is built for module ABI {}; an audio-filter module needs module ABI \
                 1.{AUDIO_MINOR} or later",
                version_text(self.abi_version)
            ));
        }
        if self.has_audio() {
            return None;
        }
        let missing: Vec<&str> = [
            (
                "audio_instance_load",
                self.vtable.audio_instance_load.is_some(),
            ),
            (
                "audio_instance_process",
                self.vtable.audio_instance_process.is_some(),
            ),
        ]
        .into_iter()
        .filter(|(_, set)| !set)
        .map(|(name, _)| name)
        .collect();
        Some(format!(
            "The module's vtable lacks the entries an audio-filter module needs: {}",
            missing.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_module_abi::obc_module_abi_pack;

    #[test]
    fn abi_versions_are_checked_major_then_minor() {
        assert_eq!(check_abi_version(OBC_MODULE_ABI_VERSION), Ok(()));
        let e = check_abi_version(obc_module_abi_pack(2, 0)).unwrap_err();
        assert!(
            e.contains("module ABI 2.0") && e.contains("module ABI 1.3"),
            "{e}"
        );
        let e = check_abi_version(obc_module_abi_pack(0, 9)).unwrap_err();
        assert!(e.contains("0.9"), "{e}");
        let e = check_abi_version(obc_module_abi_pack(1, 7)).unwrap_err();
        assert!(e.contains("1.7, newer than") && e.contains("1.3"), "{e}");
        // A library built for ABI 1.0, 1.1, 1.2 or 1.3 loads.
        assert_eq!(check_abi_version(obc_module_abi_pack(1, 0)), Ok(()));
        assert_eq!(check_abi_version(obc_module_abi_pack(1, 1)), Ok(()));
        assert_eq!(check_abi_version(obc_module_abi_pack(1, 2)), Ok(()));
        assert_eq!(check_abi_version(obc_module_abi_pack(1, 3)), Ok(()));
    }

    #[test]
    fn a_module_of_abi_1_0_is_handed_only_the_flags_it_knows() {
        use obc_module_abi::{OBC_FRAME_FLAG_NEW_SOURCE, OBC_FRAME_FLAG_REPEAT};
        assert_eq!(
            frame_flags_of(obc_module_abi_pack(1, 0)),
            OBC_FRAME_FLAG_NEW_SOURCE
        );
        for minor in [1, 2, 3] {
            assert_eq!(
                frame_flags_of(obc_module_abi_pack(1, minor)),
                OBC_FRAME_FLAG_NEW_SOURCE | OBC_FRAME_FLAG_REPEAT
            );
        }
    }

    #[test]
    fn libraries_are_opened_never_to_be_unloaded() {
        use libloading::os::unix::{RTLD_LOCAL, RTLD_NOW};
        assert_eq!(OPEN_FLAGS, RTLD_NOW | RTLD_LOCAL | 0x1000);
    }

    /// A vtable of a smaller (older) size: the entries past it are null.
    #[test]
    fn a_vtable_is_read_up_to_its_size() {
        unsafe extern "C" fn destroy(_: *mut obc_module_abi::obc_module) {}
        // SAFETY: all zero bytes are a valid vtable.
        let mut vt: obc_module_vtable = unsafe { std::mem::zeroed() };
        vt.struct_size = VTABLE_SIZE_1_0 as u32;
        vt.destroy = Some(destroy);
        // SAFETY: the whole vtable is readable.
        let read = unsafe { read_vtable(&vt, VTABLE_SIZE_1_0) };
        assert!(read.destroy.is_some());
        // As if `destroy` were past the end.
        let short = std::mem::offset_of!(obc_module_vtable, destroy);
        // SAFETY: `short` bytes are readable.
        let read = unsafe { read_vtable(&vt, short) };
        assert!(read.destroy.is_none() && read.check.is_none());
        // A 1.0 vtable never has the audio entries of 1.2, whatever lies
        // past its end.
        unsafe extern "C" fn load(
            _: *mut obc_module_abi::obc_instance,
            _: *const obc_module_abi::obc_audio_request,
            _: *mut obc_module_abi::obc_audio_load_report,
        ) -> obc_module_abi::obc_status {
            0
        }
        vt.audio_instance_load = Some(load);
        // SAFETY: the whole vtable is readable.
        let read = unsafe { read_vtable(&vt, VTABLE_SIZE_1_0) };
        assert!(read.audio_instance_load.is_none());
        // SAFETY: as above.
        let read = unsafe { read_vtable(&vt, VTABLE_SIZE_1_2) };
        assert!(read.audio_instance_load.is_some());
        // A 1.2 vtable never has the status values of 1.3.
        unsafe extern "C" fn values(
            _: *mut obc_module_abi::obc_instance,
            _: *mut *const obc_module_abi::obc_status_value,
            _: *mut usize,
        ) -> obc_module_abi::obc_status {
            0
        }
        vt.instance_status_values = Some(values);
        // SAFETY: as above.
        let read = unsafe { read_vtable(&vt, VTABLE_SIZE_1_2) };
        assert!(read.instance_status_values.is_none());
        // SAFETY: as above.
        let read = unsafe { read_vtable(&vt, VTABLE_SIZE_1_3) };
        assert!(read.instance_status_values.is_some());
        assert_eq!(VTABLE_SIZE_1_3, size_of::<obc_module_vtable>());
    }

    #[test]
    fn a_missing_library_is_not_installed() {
        let e =
            Library::open(Path::new("/nonexistent/libx.so"), "org.x.y", stderr_log()).unwrap_err();
        assert_eq!(e, "The library /nonexistent/libx.so is not installed");
    }

    #[test]
    fn a_file_that_is_not_a_library_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libfake.so");
        std::fs::write(&path, "not an ELF file").unwrap();
        let e = Library::open(&path, "org.x.y", stderr_log()).unwrap_err();
        assert!(e.contains("could not be loaded"), "{e}");
    }

    #[test]
    fn a_library_without_the_symbols_is_not_a_module() {
        // The C library is a shared library that exports neither symbol.
        let libc = [
            "/usr/lib/libc.so.6",
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/usr/lib64/libc.so.6",
        ]
        .into_iter()
        .map(Path::new)
        .find(|p| p.is_file());
        let Some(libc) = libc else { return };
        let e = Library::open(libc, "org.x.y", stderr_log()).unwrap_err();
        assert!(e.contains("not an omarchy-broadcast module"), "{e}");
    }

    #[test]
    fn log_levels_map_from_the_abi() {
        assert_eq!(LogLevel::from_raw(OBC_LOG_ERROR), LogLevel::Error);
        assert_eq!(LogLevel::from_raw(OBC_LOG_WARN), LogLevel::Warn);
        assert_eq!(LogLevel::from_raw(OBC_LOG_INFO), LogLevel::Info);
        assert_eq!(LogLevel::from_raw(OBC_LOG_DEBUG), LogLevel::Debug);
        assert_eq!(LogLevel::from_raw(OBC_LOG_TRACE), LogLevel::Trace);
        assert_eq!(LogLevel::from_raw(99), LogLevel::Info);
        assert_eq!(LogLevel::Warn.to_string(), "warn");
    }
}
