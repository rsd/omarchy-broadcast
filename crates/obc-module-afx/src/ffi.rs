//! The NVIDIA Maxine Audio Effects SDK, opened at run time.
//!
//! The entry points are declared by hand from the SDK's documented C API
//! (nothing of the SDK is copied or linked): the SDK's core library
//! (`<root>/nvafx/lib/libnv_audiofx.so.3`) is opened by absolute path when
//! a chain is checked or loaded, once per process and root, and never
//! closed (its teardown at process exit is the SDK's, and an effect still
//! alive then can crash it: every [`Effect`] is destroyed when dropped).
//!
//! # Finding the feature libraries
//!
//! `NvAFX_CreateEffect` opens the effect's feature library itself, by its
//! **unversioned** file name (`libnv_audiofx_<selector>.so`), so loading
//! that library by absolute path beforehand does not help (its soname is
//! the versioned name). The dynamic linker looks for it from the core
//! library: in the core library's run path, whose first entry is `./` (the
//! working directory), then in `LD_LIBRARY_PATH`, which the linker reads
//! once, when the process starts.
//!
//! The module never changes the working directory (it is the process's,
//! shared by every thread). It requires the library search path instead:
//! [`Sdk::create`] calls the SDK only when the first `LD_LIBRARY_PATH`
//! directory that holds the file leads to the feature's `lib` directory
//! of this root ([`search_path`]); otherwise the stage fails with a reason
//! that names [`ENV`] and the worker's systemd drop-in [`DROP_IN`], which
//! sets the variable from the SDK package's environment file. So the
//! library always comes from the root in use.

use std::ffi::{CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};

/// The core library, relative to the SDK root.
pub const CORE_DIR: &str = "nvafx/lib";
/// The core library's file name (its soname).
pub const CORE: &str = "libnv_audiofx.so.3";
/// The core library's unversioned file name, tried after [`CORE`].
pub const CORE_UNVERSIONED: &str = "libnv_audiofx.so";

/// The library search path variable the feature libraries are found on.
pub const ENV: &str = "LD_LIBRARY_PATH";
/// The audio worker's systemd drop-in that sets [`ENV`] for the SDK.
pub const DROP_IN: &str = "omarchy-broadcast-audiod.service.d/maxine-afx.conf";

/// The SDK's status codes the module names in its messages.
pub mod status {
    /// Success.
    pub const SUCCESS: i32 = 0;
    /// The effect is not available (its feature library was not found).
    pub const EFFECT_NOT_AVAILABLE: i32 = 6;
}

/// A short description of SDK status `code`.
pub fn describe(code: i32) -> &'static str {
    match code {
        0 => "success",
        1 => "failed",
        2 => "invalid handle",
        3 => "invalid parameter",
        4 => "parameter cannot change now",
        5 => "insufficient data",
        6 => "effect not available",
        7 => "output buffer too small",
        8 => "model load failed",
        9 => "model not loaded",
        10 => "incompatible model",
        11 => "GPU not supported",
        12 => "no supported GPU found",
        13 => "wrong GPU",
        14 => "CUDA error",
        15 => "invalid operation",
        16 => "unsupported runtime",
        _ => "unknown status",
    }
}

/// An SDK call that failed: what was called and the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfxError {
    /// What the call was (`Load`, `SetU32(effect_version)`).
    pub call: String,
    /// The SDK status.
    pub code: i32,
}

impl std::fmt::Display for AfxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} returned {} ({})",
            self.call,
            self.code,
            describe(self.code)
        )
    }
}

type Handle = *mut c_void;
type CreateFn = unsafe extern "C" fn(*const c_char, *mut Handle) -> c_int;
type DestroyFn = unsafe extern "C" fn(Handle) -> c_int;
type SetU32Fn = unsafe extern "C" fn(Handle, *const c_char, u32) -> c_int;
type SetFloatFn = unsafe extern "C" fn(Handle, *const c_char, f32) -> c_int;
type SetStringFn = unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int;
type GetU32Fn = unsafe extern "C" fn(Handle, *const c_char, *mut u32) -> c_int;
type GetU32ListFn = unsafe extern "C" fn(Handle, *const c_char, *mut u32, *mut u32) -> c_int;
type LoadFn = unsafe extern "C" fn(Handle) -> c_int;
type RunFn = unsafe extern "C" fn(Handle, *const *const f32, *mut *mut f32, u32, u32) -> c_int;
type ResetFn = unsafe extern "C" fn(Handle, *mut c_char, c_int) -> c_int;

/// The SDK's entry points, from one root.
pub struct Sdk {
    root: PathBuf,
    create: CreateFn,
    destroy: DestroyFn,
    set_u32: SetU32Fn,
    set_float: SetFloatFn,
    set_string: SetStringFn,
    get_u32: GetU32Fn,
    get_u32_list: GetU32ListFn,
    load: LoadFn,
    run: RunFn,
    reset: ResetFn,
    _lib: Library,
}

impl std::fmt::Debug for Sdk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sdk").field("root", &self.root).finish()
    }
}

/// The SDKs opened in this process, by root: never closed.
static OPENED: Mutex<Vec<&'static Sdk>> = Mutex::new(Vec::new());

/// The core library of the SDK at `root`, if it is there.
pub fn core_library(root: &Path) -> Option<PathBuf> {
    let dir = root.join(CORE_DIR);
    [CORE, CORE_UNVERSIONED]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

/// Opens the SDK at `root` (once per process and root) and resolves its
/// entry points. Creates no CUDA context: nothing of the SDK runs but the
/// dynamic linker's loading of its libraries.
pub fn open(root: &Path) -> Result<&'static Sdk, String> {
    let mut opened = OPENED.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(sdk) = opened.iter().find(|s| s.root == root) {
        return Ok(sdk);
    }
    let path = core_library(root).ok_or_else(|| {
        format!(
            "the Maxine Audio Effects SDK's core library is not in {}",
            root.join(CORE_DIR).display()
        )
    })?;
    // SAFETY: loading the SDK runs its initialisers, which is what using
    // it means. Global, so that the feature libraries it opens later
    // resolve against it.
    let lib = unsafe { Library::open(Some(&path), RTLD_NOW | RTLD_GLOBAL) }
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    macro_rules! sym {
        ($name:literal, $ty:ty) => {{
            // SAFETY: the SDK's documented entry point, of this type.
            let s: libloading::os::unix::Symbol<$ty> = unsafe { lib.get($name) }
                .map_err(|e| format!("{} has no {}: {e}", path.display(), stringify!($name)))?;
            *s
        }};
    }
    let sdk = Sdk {
        root: root.to_path_buf(),
        create: sym!(b"NvAFX_CreateEffect", CreateFn),
        destroy: sym!(b"NvAFX_DestroyEffect", DestroyFn),
        set_u32: sym!(b"NvAFX_SetU32", SetU32Fn),
        set_float: sym!(b"NvAFX_SetFloat", SetFloatFn),
        set_string: sym!(b"NvAFX_SetString", SetStringFn),
        get_u32: sym!(b"NvAFX_GetU32", GetU32Fn),
        get_u32_list: sym!(b"NvAFX_GetU32List", GetU32ListFn),
        load: sym!(b"NvAFX_Load", LoadFn),
        run: sym!(b"NvAFX_Run", RunFn),
        reset: sym!(b"NvAFX_Reset", ResetFn),
        _lib: lib,
    };
    let sdk: &'static Sdk = Box::leak(Box::new(sdk));
    opened.push(sdk);
    Ok(sdk)
}

/// The unversioned file name of `selector`'s feature library, which the
/// SDK opens by name.
pub fn feature_library(selector: &str) -> String {
    format!("libnv_audiofx_{selector}.so")
}

/// Whether the library search path `ld_library_path` leads to the
/// feature library `name` of `dir`: the first of its directories that
/// holds `name` must lead (links resolved) to a file of `dir`. `Err`: why
/// not, naming [`ENV`] and [`DROP_IN`] (or that `name` is not in `dir`).
pub fn search_path(dir: &Path, name: &str, ld_library_path: Option<&str>) -> Result<(), String> {
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    let want = canon(dir);
    let first = ld_library_path
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(Path::new)
        .find(|d| d.join(name).is_file());
    let leads = first.is_some_and(|first| {
        canon(&first.join(name))
            .and_then(|f| f.parent().map(Path::to_path_buf))
            .is_some_and(|f| Some(f) == want)
    });
    if leads {
        return Ok(());
    }
    if !dir.join(name).is_file() {
        return Err(format!(
            "{name} is not in {} (the SDK opens it by that name)",
            dir.display()
        ));
    }
    let why = match first {
        Some(f) => format!("its first directory with {name} is {}", f.display()),
        None => format!("it does not name {}", dir.display()),
    };
    Err(format!(
        "the library search path does not lead to {name} of this SDK root ({ENV}: {why}); \
         start the audio worker with the drop-in {DROP_IN}, which sets {ENV}"
    ))
}

impl Sdk {
    /// The root this SDK was opened from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates the effect `selector`, whose feature library is in `dir`
    /// (found on the library search path: see the module documentation).
    pub fn create(&'static self, selector: &str, dir: &Path) -> Result<Effect, String> {
        let name = feature_library(selector);
        search_path(dir, &name, std::env::var(ENV).ok().as_deref())?;
        let sel = CString::new(selector).map_err(|e| e.to_string())?;
        let mut h: Handle = std::ptr::null_mut();
        // SAFETY: a valid selector; `h` writable.
        let code = unsafe { (self.create)(sel.as_ptr(), &mut h) };
        if code != status::SUCCESS || h.is_null() {
            if !h.is_null() {
                // A handle returned with an error status is the SDK's still.
                // SAFETY: created by the SDK, destroyed once, never used.
                unsafe { (self.destroy)(h) };
            }
            let hint = if code == status::EFFECT_NOT_AVAILABLE {
                format!(" ({name} was not found)")
            } else {
                String::new()
            };
            return Err(format!(
                "{}{hint}",
                AfxError {
                    call: format!("CreateEffect({selector})"),
                    code,
                }
            ));
        }
        Ok(Effect {
            sdk: self,
            h,
            selector: selector.to_string(),
        })
    }
}

/// An SDK effect, destroyed when dropped.
pub struct Effect {
    sdk: &'static Sdk,
    h: Handle,
    selector: String,
}

// SAFETY: the handle is used through `&mut self` only (and dropped once);
// the SDK's calls on one effect may come from any thread, one at a time.
unsafe impl Send for Effect {}

impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effect")
            .field("selector", &self.selector)
            .finish()
    }
}

impl Drop for Effect {
    fn drop(&mut self) {
        // SAFETY: created by the SDK, destroyed once.
        let code = unsafe { (self.sdk.destroy)(self.h) };
        if code != status::SUCCESS {
            obc_module_sdk::log::warn!(
                "{}",
                AfxError {
                    call: format!("DestroyEffect({})", self.selector),
                    code,
                }
            );
        }
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("parameter names and paths have no NUL byte")
}

impl Effect {
    /// The selector it was created with.
    pub fn selector(&self) -> &str {
        &self.selector
    }

    fn check(&self, call: impl FnOnce() -> String, code: c_int) -> Result<(), AfxError> {
        if code == status::SUCCESS {
            Ok(())
        } else {
            Err(AfxError { call: call(), code })
        }
    }

    /// `NvAFX_SetU32`.
    pub fn set_u32(&mut self, name: &str, value: u32) -> Result<(), AfxError> {
        let n = cstr(name);
        // SAFETY: a live handle and a valid name.
        let code = unsafe { (self.sdk.set_u32)(self.h, n.as_ptr(), value) };
        self.check(|| format!("SetU32({name})"), code)
    }

    /// `NvAFX_SetFloat`.
    pub fn set_f32(&mut self, name: &str, value: f32) -> Result<(), AfxError> {
        let n = cstr(name);
        // SAFETY: a live handle and a valid name.
        let code = unsafe { (self.sdk.set_float)(self.h, n.as_ptr(), value) };
        self.check(|| format!("SetFloat({name})"), code)
    }

    /// `NvAFX_SetString`.
    pub fn set_str(&mut self, name: &str, value: &str) -> Result<(), AfxError> {
        let (n, v) = (cstr(name), cstr(value));
        // SAFETY: a live handle and valid strings, copied by the SDK.
        let code = unsafe { (self.sdk.set_string)(self.h, n.as_ptr(), v.as_ptr()) };
        self.check(|| format!("SetString({name})"), code)
    }

    /// `NvAFX_GetU32`.
    pub fn get_u32(&mut self, name: &str) -> Result<u32, AfxError> {
        let n = cstr(name);
        let mut v = 0u32;
        // SAFETY: a live handle, a valid name, `v` writable.
        let code = unsafe { (self.sdk.get_u32)(self.h, n.as_ptr(), &mut v) };
        self.check(|| format!("GetU32({name})"), code).map(|()| v)
    }

    /// `NvAFX_GetU32List`, of up to 32 values.
    pub fn get_u32_list(&mut self, name: &str) -> Result<Vec<u32>, AfxError> {
        let n = cstr(name);
        let mut list = [0u32; 32];
        let mut size = list.len() as u32;
        // SAFETY: a live handle, a valid name, `size` entries writable.
        let code =
            unsafe { (self.sdk.get_u32_list)(self.h, n.as_ptr(), list.as_mut_ptr(), &mut size) };
        self.check(|| format!("GetU32List({name})"), code)?;
        Ok(list[..(size as usize).min(list.len())].to_vec())
    }

    /// `NvAFX_Load`: where the GPU work starts.
    pub fn load(&mut self) -> Result<(), AfxError> {
        // SAFETY: a live handle.
        let code = unsafe { (self.sdk.load)(self.h) };
        self.check(|| "Load".to_string(), code)
    }

    /// `NvAFX_Run` on one mono block (`input` and `output` of one length).
    pub fn run(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), AfxError> {
        assert_eq!(input.len(), output.len(), "one block size in and out");
        let ins = [input.as_ptr()];
        let mut outs = [output.as_mut_ptr()];
        // SAFETY: a live handle; one channel of `len` samples each way,
        // which the SDK reads and writes during the call only.
        let code = unsafe {
            (self.sdk.run)(
                self.h,
                ins.as_ptr(),
                outs.as_mut_ptr(),
                input.len() as u32,
                1,
            )
        };
        self.check(|| "Run".to_string(), code)
    }

    /// `NvAFX_Reset` of the effect's one stream.
    pub fn reset(&mut self) -> Result<(), AfxError> {
        let mut bitmap = [1 as c_char];
        // SAFETY: a live handle and a one-entry bitmap.
        let code = unsafe { (self.sdk.reset)(self.h, bitmap.as_mut_ptr(), 1) };
        self.check(|| "Reset".to_string(), code)
    }
}

/// The compute capability of CUDA device 0, from the CUDA driver
/// (`cuInit` and `cuDeviceGetAttribute`; no context is created). Only
/// called from a load, where GPU work is allowed.
pub fn compute_capability() -> Result<(u32, u32), String> {
    type InitFn = unsafe extern "C" fn(u32) -> c_int;
    type DeviceGetFn = unsafe extern "C" fn(*mut c_int, c_int) -> c_int;
    type AttrFn = unsafe extern "C" fn(*mut c_int, c_int, c_int) -> c_int;
    const MAJOR: c_int = 75;
    const MINOR: c_int = 76;
    // SAFETY: the CUDA driver's initialisers.
    let lib = unsafe { Library::open(Some("libcuda.so.1"), RTLD_NOW) }
        .map_err(|e| format!("cannot open the CUDA driver: {e}"))?;
    // SAFETY: the driver's documented entry points, of these types.
    let (init, get, attr) = unsafe {
        (
            *lib.get::<InitFn>(b"cuInit").map_err(|e| e.to_string())?,
            *lib.get::<DeviceGetFn>(b"cuDeviceGet")
                .map_err(|e| e.to_string())?,
            *lib.get::<AttrFn>(b"cuDeviceGetAttribute")
                .map_err(|e| e.to_string())?,
        )
    };
    let (mut dev, mut major, mut minor) = (0, 0, 0);
    // SAFETY: plain driver calls with writable outputs.
    let codes = unsafe {
        [
            init(0),
            get(&mut dev, 0),
            attr(&mut major, MAJOR, dev),
            attr(&mut minor, MINOR, dev),
        ]
    };
    if let Some(c) = codes.iter().find(|c| **c != 0) {
        return Err(format!("the CUDA driver returned {c}"));
    }
    // The driver stays loaded: the SDK uses it too.
    std::mem::forget(lib);
    Ok((major as u32, minor as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_described() {
        assert_eq!(describe(0), "success");
        assert_eq!(describe(6), "effect not available");
        assert_eq!(describe(99), "unknown status");
        let e = AfxError {
            call: "Load".into(),
            code: 8,
        };
        assert_eq!(e.to_string(), "Load returned 8 (model load failed)");
    }

    #[test]
    fn the_feature_library_must_be_on_the_search_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let name = feature_library("denoiser");
        std::fs::write(a.path().join(&name), b"").unwrap();
        let a_dir = a.path().to_str().unwrap();
        let b_dir = b.path().to_str().unwrap();
        // On the search path.
        assert_eq!(
            search_path(a.path(), &name, Some(&format!("/nonexistent:{a_dir}"))),
            Ok(())
        );
        // Not on it, or another directory's copy first: refused, naming
        // the variable and the drop-in.
        for ld in [None, Some(b_dir.to_string())] {
            let e = search_path(a.path(), &name, ld.as_deref()).unwrap_err();
            assert!(e.contains(ENV) && e.contains(DROP_IN), "{e}");
        }
        std::fs::write(b.path().join(&name), b"").unwrap();
        let e = search_path(a.path(), &name, Some(&format!("{b_dir}:{a_dir}"))).unwrap_err();
        assert!(e.contains(b_dir), "{e}");
        // A link elsewhere on the search path to this directory's file.
        let c = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(a.path().join(&name), c.path().join(&name)).unwrap();
        let c_dir = c.path().to_str().unwrap();
        assert_eq!(search_path(a.path(), &name, Some(c_dir)), Ok(()));
        // Not in its directory: not findable.
        let e = search_path(b.path().join("x").as_path(), &name, Some(a_dir)).unwrap_err();
        assert!(e.contains("is not in"), "{e}");
    }
}
