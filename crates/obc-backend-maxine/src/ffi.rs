//! The subset of the Maxine Video Effects C API this backend uses, opened at
//! run time.
//!
//! Written by hand from NVIDIA's public API documentation for the Video
//! Effects SDK and NvCVImage (function signatures, the `NvCVImage` layout,
//! the parameter-name strings and the status codes used here). No SDK file
//! is copied or needed at build time.

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use libloading::Library;

/// `NvCV_Status`: 0 is success, negative values are errors.
pub(crate) type NvCvStatus = i32;
/// `NvVFX_Handle`: an opaque effect instance.
pub(crate) type VfxHandle = *mut c_void;
/// `NvVFX_StateObjectHandle`: an opaque temporal-state object.
pub(crate) type StateHandle = *mut c_void;
/// `CUstream` as the SDK takes it.
pub(crate) type CuStream = *mut c_void;

pub(crate) const NVCV_SUCCESS: NvCvStatus = 0;
/// An otherwise unspecified error.
pub(crate) const NVCV_ERR_GENERAL: NvCvStatus = -1;
/// The requested feature is not implemented: the SDK did not find the
/// feature library (`libnvVFXGreenScreen.so`, ...) by name.
pub(crate) const NVCV_ERR_UNIMPLEMENTED: NvCvStatus = -2;
/// A file (the models) could not be found.
pub(crate) const NVCV_ERR_FILE: NvCvStatus = -13;
/// A parameter value is invalid for the effect.
pub(crate) const NVCV_ERR_PARAMETER: NvCvStatus = -7;
/// The image resolution is not supported.
pub(crate) const NVCV_ERR_RESOLUTION: NvCvStatus = -16;
/// "No kernel image is available for the device". From `NvVFX_Load` on an
/// unlisted distribution this is the SDK's `/etc/os-release` check, not a
/// CUDA problem (`docs/setup/maxine.md`, section 3).
pub(crate) const NVCV_ERR_CUDA_KERNEL: NvCvStatus = -1209;

// `NvCVImage_PixelFormat` values.
pub(crate) const NVCV_A: i32 = 2;
pub(crate) const NVCV_RGB: i32 = 4;
pub(crate) const NVCV_BGR: i32 = 5;
pub(crate) const NVCV_YUV420: i32 = 10;
// Packed R10G10B10A2 (with `NVCV_P32`).
pub(crate) const NVCV_RGB10A2: i32 = 13;
// `NvCVImage_ComponentType`: unsigned 8-bit.
pub(crate) const NVCV_U8: i32 = 1;
// `NvCVImage_ComponentType`: 32-bit float.
pub(crate) const NVCV_F32: i32 = 7;
// `NvCVImage_ComponentType`: one packed 32-bit word per pixel.
pub(crate) const NVCV_P32: i32 = 11;
// Layout: interleaved components.
pub(crate) const NVCV_CHUNKY: u32 = 0;
/// `NvCVImage_Alloc` alignment 1: rows exactly `width * pixel bytes` apart
/// (0 lets the SDK pad the pitch for the GPU).
pub(crate) const NVCV_TIGHT_PITCH: u32 = 1;
// Memory space: pageable host memory.
pub(crate) const NVCV_CPU: u32 = 0;
// Memory space: CUDA device memory.
pub(crate) const NVCV_GPU: u32 = 1;
// YUV colour-space flags, ORed: matrix, range, chroma siting.
pub(crate) const NVCV_601: u32 = 0x00;
pub(crate) const NVCV_709: u32 = 0x01;
pub(crate) const NVCV_VIDEO_RANGE: u32 = 0x00;
pub(crate) const NVCV_FULL_RANGE: u32 = 0x04;
pub(crate) const NVCV_CHROMA_COSITED: u32 = 0x00;
pub(crate) const NVCV_CHROMA_INTSTITIAL: u32 = 0x08;
pub(crate) const NVCV_CHROMA_TOPLEFT: u32 = 0x10;

// Effect selectors and parameter names.
pub(crate) const FX_GREEN_SCREEN: &CStr = c"GreenScreen";
pub(crate) const FX_BACKGROUND_BLUR: &CStr = c"BackgroundBlur";
pub(crate) const FX_DENOISING: &CStr = c"Denoising";
pub(crate) const FX_RELIGHTING: &CStr = c"Relighting";
pub(crate) const P_INPUT_IMAGE_0: &CStr = c"SrcImage0";
pub(crate) const P_INPUT_IMAGE_1: &CStr = c"SrcImage1";
/// The relighting effect's lighting environment (its
/// `NVVFX_INPUT_HDR_RELIGHTING_IMAGE`).
pub(crate) const P_INPUT_IMAGE_2: &CStr = c"SrcImage2";
pub(crate) const P_OUTPUT_IMAGE_0: &CStr = c"DstImage0";
pub(crate) const P_MODEL_DIRECTORY: &CStr = c"ModelDir";
pub(crate) const P_CUDA_STREAM: &CStr = c"CudaStream";
pub(crate) const P_CUDA_GRAPH: &CStr = c"CudaGraph";
pub(crate) const P_MAX_INPUT_WIDTH: &CStr = c"MaxInputWidth";
pub(crate) const P_MAX_INPUT_HEIGHT: &CStr = c"MaxInputHeight";
pub(crate) const P_MAX_NUMBER_STREAMS: &CStr = c"MaxNumberStreams";
pub(crate) const P_STRENGTH: &CStr = c"Strength";
pub(crate) const P_MODE: &CStr = c"Mode";
pub(crate) const P_STATE: &CStr = c"State";
pub(crate) const P_STATE_SIZE: &CStr = c"StateSize";
/// The relighting effect's rotation of its environment, in radians.
pub(crate) const P_ANGLE_PAN: &CStr = c"AnglePan";
/// The relighting effect's brightness of the relit foreground.
pub(crate) const P_FG_GAIN: &CStr = c"FgGain";

/// The `NvCVImage` image descriptor: width, height, signed pitch, pixel
/// format, component type, six byte-sized descriptors (pixel bytes,
/// component bytes, component count, layout, memory space, colour space),
/// two reserved bytes, the pixel pointer, the pointer to free, the free
/// procedure and the buffer size.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct NvCvImage {
    pub width: u32,
    pub height: u32,
    pub pitch: i32,
    pub pixel_format: i32,
    pub component_type: i32,
    pub pixel_bytes: u8,
    pub component_bytes: u8,
    pub num_components: u8,
    pub planar: u8,
    pub gpu_mem: u8,
    pub colorspace: u8,
    pub reserved: [u8; 2],
    pub pixels: *mut c_void,
    pub delete_ptr: *mut c_void,
    pub delete_proc: Option<unsafe extern "C" fn(*mut c_void)>,
    pub buffer_bytes: u64,
}

impl NvCvImage {
    /// An empty descriptor (no buffer), as the C API expects before
    /// `NvCVImage_Alloc` and for staging images.
    pub(crate) fn empty() -> Self {
        NvCvImage {
            width: 0,
            height: 0,
            pitch: 0,
            pixel_format: 0,
            component_type: 0,
            pixel_bytes: 0,
            component_bytes: 0,
            num_components: 0,
            planar: 0,
            gpu_mem: 0,
            colorspace: 0,
            reserved: [0; 2],
            pixels: std::ptr::null_mut(),
            delete_ptr: std::ptr::null_mut(),
            delete_proc: None,
            buffer_bytes: 0,
        }
    }
}

/// The resolved SDK entry points (`libVideoFX.so` and `libNVCVImage.so`).
#[allow(non_snake_case)] // fields are named after the C entry points
pub(crate) struct Sdk {
    _videofx: Library,
    _nvcvimage: Library,
    /// Where the SDK's models are: `<directory of libVideoFX>/models`.
    pub model_dir: Option<PathBuf>,
    /// The SDK root the libraries were loaded from by absolute path
    /// (canonical; `None`: from the library search path, even when a root
    /// was asked for, because that root had no SDK libraries).
    pub root: Option<PathBuf>,
    /// The directory `libVideoFX.so` was loaded from (canonical), as the
    /// loader reports it.
    pub lib_dir: Option<PathBuf>,
    /// The SDK libraries [`crate::preload`] could not load first, as one
    /// line (`None`: all loaded, or no root).
    pub preload_failures: Option<String>,
    pub NvVFX_CreateEffect: unsafe extern "C" fn(*const c_char, *mut VfxHandle) -> NvCvStatus,
    pub NvVFX_DestroyEffect: unsafe extern "C" fn(VfxHandle),
    pub NvVFX_SetU32: unsafe extern "C" fn(VfxHandle, *const c_char, u32) -> NvCvStatus,
    pub NvVFX_SetF32: unsafe extern "C" fn(VfxHandle, *const c_char, f32) -> NvCvStatus,
    pub NvVFX_GetU32: unsafe extern "C" fn(VfxHandle, *const c_char, *mut u32) -> NvCvStatus,
    pub NvVFX_SetObject: unsafe extern "C" fn(VfxHandle, *const c_char, *mut c_void) -> NvCvStatus,
    pub NvVFX_SetString:
        unsafe extern "C" fn(VfxHandle, *const c_char, *const c_char) -> NvCvStatus,
    pub NvVFX_SetImage:
        unsafe extern "C" fn(VfxHandle, *const c_char, *mut NvCvImage) -> NvCvStatus,
    pub NvVFX_SetCudaStream: unsafe extern "C" fn(VfxHandle, *const c_char, CuStream) -> NvCvStatus,
    pub NvVFX_SetStateObjectHandleArray:
        unsafe extern "C" fn(VfxHandle, *const c_char, *mut StateHandle) -> NvCvStatus,
    pub NvVFX_AllocateState: unsafe extern "C" fn(VfxHandle, *mut StateHandle) -> NvCvStatus,
    pub NvVFX_DeallocateState: unsafe extern "C" fn(VfxHandle, StateHandle) -> NvCvStatus,
    pub NvVFX_ResetState: unsafe extern "C" fn(VfxHandle, StateHandle) -> NvCvStatus,
    pub NvVFX_Load: unsafe extern "C" fn(VfxHandle) -> NvCvStatus,
    pub NvVFX_Run: unsafe extern "C" fn(VfxHandle, i32) -> NvCvStatus,
    pub NvCVImage_Alloc:
        unsafe extern "C" fn(*mut NvCvImage, u32, u32, i32, i32, u32, u32, u32) -> NvCvStatus,
    pub NvCVImage_Dealloc: unsafe extern "C" fn(*mut NvCvImage),
    #[allow(clippy::type_complexity)]
    pub NvCVImage_TransferFromYUV: unsafe extern "C" fn(
        *const c_void, // y
        i32,           // y pixel bytes
        i32,           // y pitch
        *const c_void, // u
        *const c_void, // v
        i32,           // u/v pixel bytes
        i32,           // u/v pitch
        i32,           // YUV pixel format
        i32,           // YUV component type
        u32,           // YUV colour space flags
        u32,           // YUV memory space
        *mut NvCvImage,
        *const c_void, // destination rectangle (null: whole image)
        f32,           // scale
        CuStream,
        *mut NvCvImage, // staging image
    ) -> NvCvStatus,
    #[allow(clippy::type_complexity)]
    pub NvCVImage_TransferToYUV: unsafe extern "C" fn(
        *const NvCvImage,
        *const c_void, // source rectangle (null: whole image)
        *const c_void, // y
        i32,           // y pixel bytes
        i32,           // y pitch
        *const c_void, // u
        *const c_void, // v
        i32,           // u/v pixel bytes
        i32,           // u/v pitch
        i32,           // YUV pixel format
        i32,           // YUV component type
        u32,           // YUV colour space flags
        u32,           // YUV memory space
        f32,           // scale
        CuStream,
        *mut NvCvImage, // staging image
    ) -> NvCvStatus,
    pub NvCVImage_Composite: unsafe extern "C" fn(
        *const NvCvImage, // foreground
        *const NvCvImage, // background
        *const NvCvImage, // matte
        *mut NvCvImage,   // destination
        CuStream,
    ) -> NvCvStatus,
    NvCV_GetErrorStringFromCode: unsafe extern "C" fn(NvCvStatus) -> *const c_char,
}

impl Sdk {
    /// The SDK's description of `code`.
    pub(crate) fn error_string(&self, code: NvCvStatus) -> String {
        // SAFETY: the function accepts any code and returns a static
        // NUL-terminated string (or null, handled below).
        let p = unsafe { (self.NvCV_GetErrorStringFromCode)(code) };
        if p.is_null() {
            return "unknown error".into();
        }
        // SAFETY: non-null, static and NUL-terminated.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// Which library files to look for. Fixed in production; the unit tests use
/// names that cannot exist, so the search-path fallback fails on any machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibNames {
    pub videofx: &'static str,
    pub nvcvimage: &'static str,
}

pub(crate) const SDK_LIBS: LibNames = LibNames {
    videofx: "libVideoFX.so",
    nvcvimage: "libNVCVImage.so",
};

/// Where this process's SDK libraries came from, or where an open would
/// take them from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// By absolute path from this SDK root (canonical: a symbolic link is
    /// recorded as the directory it pointed at).
    Root(PathBuf),
    /// By name from the library search path; the directory the loader found
    /// the first of them in (canonical), when it reports it.
    SearchPath(Option<PathBuf>),
}

impl Source {
    /// Where an open of `root` takes the SDK from: the root, canonical, when
    /// it has any of the libraries [`crate::preload::roots`] starts from;
    /// else (no root, or one without SDK libraries) the search path.
    fn of(root: Option<&Path>, names: &LibNames) -> Source {
        root.and_then(|r| r.canonicalize().ok())
            .filter(|c| !crate::preload::roots(c, names.nvcvimage, names.videofx).is_empty())
            .map_or(Source::SearchPath(None), Source::Root)
    }

    /// Whether an open that takes the SDK from `wanted` gets the libraries
    /// already loaded from `self` rather than a second SDK. The search path
    /// is the same for the life of the process (the loader reads it when the
    /// process starts). A root matches the search path only when the search
    /// path served the SDK from that root's `lib`; otherwise what one of them
    /// would load is not known to be the other's, so they do not match.
    fn serves(&self, wanted: &Source) -> bool {
        match (self, wanted) {
            (Source::Root(a), Source::Root(b)) => a == b,
            (Source::SearchPath(_), Source::SearchPath(_)) => true,
            (Source::SearchPath(Some(dir)), Source::Root(b)) => {
                b.join("lib").canonicalize().is_ok_and(|l| l == *dir)
            }
            _ => false,
        }
    }

    fn describe(&self) -> String {
        match self {
            Source::Root(r) => r.display().to_string(),
            Source::SearchPath(Some(d)) => format!("the library search path ({})", d.display()),
            Source::SearchPath(None) => "the library search path".into(),
        }
    }
}

fn describe_root(root: Option<&Path>) -> String {
    match root {
        Some(r) => r.display().to_string(),
        None => "the library search path".into(),
    }
}

/// An SDK opened successfully.
struct Opened {
    source: Source,
    names: LibNames,
    sdk: &'static Sdk,
}

/// What this process has loaded of the SDK.
#[derive(Default)]
struct Registry {
    /// Where the first SDK library loaded in this process came from. Set as
    /// soon as one library is loaded, even when the open then fails: what
    /// was loaded stays loaded (`RTLD_GLOBAL`, never closed) and has the
    /// same sonames as any other SDK's, so every later open must take the
    /// SDK from the same place ([`Source::serves`]). A failed open that
    /// loaded nothing leaves it unset.
    pin: Option<Source>,
    /// The SDKs opened, reused by later opens from the same place with the
    /// same names. Never closed: the SDK's libraries stay mapped for the
    /// life of the process.
    opened: Vec<Opened>,
}

/// The process's SDK.
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    pin: None,
    opened: Vec::new(),
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|p| p.into_inner())
}

impl Registry {
    /// The SDK root the process's SDK was loaded from: the pinned root, or
    /// the directory above the `lib` the search path served the SDK from.
    /// `None` when nothing is loaded yet, or the search path served it from
    /// a directory that is not a root's `lib` (or did not say which).
    fn loaded_root(&self) -> Option<PathBuf> {
        match self.pin.as_ref()? {
            Source::Root(r) => Some(r.clone()),
            Source::SearchPath(Some(dir)) if dir.file_name().is_some_and(|n| n == "lib") => {
                dir.parent().map(Path::to_path_buf)
            }
            Source::SearchPath(_) => None,
        }
    }

    /// Why an open of `root`, which takes the SDK from `wanted`, is refused:
    /// the SDK is already loaded from elsewhere.
    fn conflict(&self, root: Option<&Path>, wanted: &Source) -> Option<String> {
        let pin = self.pin.as_ref()?;
        if pin.serves(wanted) {
            return None;
        }
        Some(format!(
            "the NVIDIA Maxine Video Effects SDK is already loaded in this process from {}, \
             and a second SDK cannot be loaded beside it: the SDK root is now {}. Restart the \
             process to use the new root (the daemon: systemctl --user restart \
             omarchy-broadcastd)",
            pin.describe(),
            describe_root(root)
        ))
    }

    /// See [`open`].
    fn open(&mut self, root: Option<&Path>, names: &LibNames) -> Result<&'static Sdk, String> {
        let wanted = Source::of(root, names);
        if let Some(why) = self.conflict(root, &wanted) {
            return Err(why);
        }
        if let Some(i) = serving(
            self.opened.iter().map(|o| (&o.source, &o.names)),
            names,
            &wanted,
        ) {
            return Ok(self.opened[i].sdk);
        }
        let sdk = load(root, &wanted, names, &mut self.pin)?;
        let sdk: &'static Sdk = Box::leak(Box::new(sdk));
        self.opened.push(Opened {
            source: opened_source(wanted, self.pin.as_ref()),
            names: names.clone(),
            sdk,
        });
        Ok(sdk)
    }
}

/// The source to record for an SDK just loaded for `wanted`. An open from the
/// search path wants `SearchPath(None)`, but the loader reported the
/// directory it loaded from, and that is in `pin`: record it, so that a later
/// open of that directory's root finds this SDK ([`serving`]).
fn opened_source(wanted: Source, pin: Option<&Source>) -> Source {
    match (&wanted, pin) {
        (Source::SearchPath(None), Some(p @ Source::SearchPath(Some(_)))) => p.clone(),
        _ => wanted,
    }
}

/// The index of the SDK, among those opened (as their sources and names),
/// that an open of `names` from `wanted` gets: one with the same names whose
/// source [serves](Source::serves) `wanted`. Not only an equal source: an SDK
/// loaded from the search path at a root's `lib` is that root's, and opening
/// the root must not load it a second time.
fn serving<'a>(
    mut opened: impl Iterator<Item = (&'a Source, &'a LibNames)>,
    names: &LibNames,
    wanted: &Source,
) -> Option<usize> {
    opened.position(|(source, n)| n == names && source.serves(wanted))
}

/// What [`open`] would refuse `root` with because the SDK is already loaded
/// from elsewhere; `None` when nothing is loaded yet or the SDK `root`
/// leads to is the one loaded. Opens nothing.
pub(crate) fn root_conflict(root: Option<&Path>) -> Option<String> {
    registry().conflict(root, &Source::of(root, &SDK_LIBS))
}

/// The SDK root this process loaded its SDK from ([`Registry::loaded_root`]);
/// `None` when nothing is loaded yet or the place is not a root. Opens
/// nothing.
pub(crate) fn loaded_root() -> Option<PathBuf> {
    registry().loaded_root()
}

/// Opens the SDK, or explains why it cannot be opened. Once a library of
/// the SDK has been loaded in this process, from a root or from the search
/// path, the process is pinned to that place, even if that open failed: an
/// open that would take the SDK from there returns the same SDK (or tries
/// again), any other is refused (fail-closed, naming the restart it takes).
pub(crate) fn open(root: Option<&Path>, names: &LibNames) -> Result<&'static Sdk, String> {
    registry().open(root, names)
}

/// The candidate file names of `name` in a directory: the unversioned name
/// and the major-versioned soname, as the SDK installs both.
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name), dir.join(format!("{name}.1"))]
}

/// Opens one library, from `wanted`: from the root's `lib` by absolute path
/// (never from the search path then, which would mix two SDKs), else by
/// plain name from the library search path, `symbol` telling the loader's
/// directory for it. `note` gets where it came from once it is loaded.
/// `root` is what was asked for, named in the errors.
fn open_one(
    root: Option<&Path>,
    wanted: &Source,
    name: &str,
    symbol: &CStr,
    note: &mut dyn FnMut(Source),
) -> Result<Library, String> {
    if let Source::Root(canonical) = wanted {
        let shown = root.unwrap_or(canonical);
        let lib_dir = canonical.join("lib");
        let Some(path) = candidates(&lib_dir, name).into_iter().find(|p| p.is_file()) else {
            return Err(format!(
                "{name} is not in {}: the SDK root {} has other SDK libraries but not this one, \
                 and the library search path is not tried for it, so that two SDKs are not \
                 mixed. The SDK root must be a complete SDK, the directory that contains lib/, \
                 features/ and external/ (docs/setup/maxine.md, section 4)",
                lib_dir.display(),
                shown.display()
            ));
        };
        // SAFETY: loading an SDK library runs its initialisers, which have
        // no preconditions for the caller.
        let lib = unsafe { Library::new(&path) }.map_err(|e| {
            format!(
                "{} could not be loaded: {e}. The SDK root {} must be a complete SDK: the \
                 libraries it needs are loaded from its lib/, features/*/lib, \
                 external/cuda/lib and external/tensorrt/lib directories \
                 (docs/setup/maxine.md, section 4)",
                path.display(),
                shown.display()
            )
        })?;
        note(wanted.clone());
        return Ok(lib);
    }
    // SAFETY: as above, for the library found on the search path.
    let lib = unsafe { Library::new(name) }.map_err(|e| match root {
        Some(root) => format!(
            "the NVIDIA Maxine Video Effects SDK was not found: no {name} in {} and none on \
             the library search path ({e}). The SDK root {} (MAXINE_VFX_ROOT when set, else \
             [effect] maxine_root in the configuration, else {}) must be the SDK directory \
             that contains lib/, features/ and external/: reinstall the maxine-vfx-sdk-local \
             package, or set [effect] maxine_root or MAXINE_VFX_ROOT to that directory \
             (docs/setup/maxine.md, section 4)",
            root.join("lib").display(),
            root.display(),
            crate::DEFAULT_ROOT
        ),
        None => format!(
            "the NVIDIA Maxine Video Effects SDK was not found: MAXINE_VFX_ROOT is unset or \
             empty, no [effect] maxine_root is configured, {} is not a directory and {name} is \
             not on the library search path ({e}). Install the maxine-vfx-sdk-local package \
             (built from omarchy-nvidia-sdks), or set [effect] maxine_root in the \
             configuration (or MAXINE_VFX_ROOT) to the SDK directory (docs/setup/maxine.md, \
             sections 2 and 5)",
            crate::DEFAULT_ROOT
        ),
    })?;
    // SAFETY: only the symbol's address is read, never called through.
    let dir = unsafe { lib.get::<*const c_void>(symbol.to_bytes_with_nul()) }
        .ok()
        .and_then(|s| object_dir(*s));
    note(Source::SearchPath(dir));
    Ok(lib)
}

/// Resolves `name` from `lib` as a `T` (a function pointer).
///
/// # Safety
/// `T` must be the correct function pointer type for the C symbol `name`.
unsafe fn sym<T: Copy>(lib: &Library, lib_name: &str, name: &str) -> Result<T, String> {
    let mut cname = name.as_bytes().to_vec();
    cname.push(0);
    // SAFETY: the caller guarantees `T`; `Sdk` keeps `lib` loaded for as
    // long as the pointer is used (forever, see `Registry`).
    unsafe { lib.get::<T>(&cname) }.map(|s| *s).map_err(|e| {
        format!("{lib_name} has no {name} ({e}); is this Maxine VFX SDK 1.3 or later?")
    })
}

/// The directory of the shared object that contains `addr`.
fn object_dir(addr: *const c_void) -> Option<PathBuf> {
    let mut info = libc::Dl_info {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    // SAFETY: `info` is a valid out-structure; `addr` is any address (dladdr
    // only looks it up).
    let rc = unsafe { libc::dladdr(addr, &mut info) };
    if rc == 0 || info.dli_fname.is_null() {
        return None;
    }
    // SAFETY: dladdr stores a NUL-terminated path owned by the loader.
    let file = unsafe { CStr::from_ptr(info.dli_fname) };
    let path = PathBuf::from(file.to_string_lossy().into_owned());
    let path = path.canonicalize().unwrap_or(path);
    path.parent().map(Path::to_path_buf)
}

/// Opens the SDK from `wanted` ([`Source::of`] of `root`, which is what
/// was asked for and is named in the errors). From a root, everything the
/// SDK needs is loaded from it first, by absolute path (`crate::preload`);
/// from the search path, the loader finds what it can. The first library
/// loaded sets `pin` when it is unset ([`Registry::pin`]), whether or not
/// the open then succeeds.
fn load(
    root: Option<&Path>,
    wanted: &Source,
    names: &LibNames,
    pin: &mut Option<Source>,
) -> Result<Sdk, String> {
    let mut note = |s: Source| {
        if pin.is_none() {
            *pin = Some(s);
        }
    };
    let preload_failures = match wanted {
        Source::Root(canonical) => {
            let done = crate::preload::preload(canonical, names.nvcvimage, names.videofx);
            if !done.loaded.is_empty() {
                note(wanted.clone());
            }
            done.failures()
        }
        Source::SearchPath(_) => None,
    };
    let with_failures = |e: String| match &preload_failures {
        Some(f) => format!("{e}. SDK libraries that could not be loaded first: {f}"),
        None => e,
    };
    // libVideoFX.so needs libNVCVImage.so (by that soname): open it first so
    // the loader reuses it when it is not on the search path.
    let nvcvimage = open_one(root, wanted, names.nvcvimage, c"NvCVImage_Alloc", &mut note)
        .map_err(with_failures)?;
    let videofx =
        open_one(root, wanted, names.videofx, c"NvVFX_Load", &mut note).map_err(with_failures)?;
    let (vf, cv) = (names.videofx, names.nvcvimage);
    // SAFETY (all `sym` calls): each field type is the documented C
    // signature of the entry point it is resolved from.
    let sdk = unsafe {
        Sdk {
            model_dir: None,
            root: match wanted {
                Source::Root(canonical) => Some(canonical.clone()),
                Source::SearchPath(_) => None,
            },
            lib_dir: None,
            preload_failures: preload_failures.clone(),
            NvVFX_CreateEffect: sym(&videofx, vf, "NvVFX_CreateEffect")?,
            NvVFX_DestroyEffect: sym(&videofx, vf, "NvVFX_DestroyEffect")?,
            NvVFX_SetU32: sym(&videofx, vf, "NvVFX_SetU32")?,
            NvVFX_SetF32: sym(&videofx, vf, "NvVFX_SetF32")?,
            NvVFX_GetU32: sym(&videofx, vf, "NvVFX_GetU32")?,
            NvVFX_SetObject: sym(&videofx, vf, "NvVFX_SetObject")?,
            NvVFX_SetString: sym(&videofx, vf, "NvVFX_SetString")?,
            NvVFX_SetImage: sym(&videofx, vf, "NvVFX_SetImage")?,
            NvVFX_SetCudaStream: sym(&videofx, vf, "NvVFX_SetCudaStream")?,
            NvVFX_SetStateObjectHandleArray: sym(&videofx, vf, "NvVFX_SetStateObjectHandleArray")?,
            NvVFX_AllocateState: sym(&videofx, vf, "NvVFX_AllocateState")?,
            NvVFX_DeallocateState: sym(&videofx, vf, "NvVFX_DeallocateState")?,
            NvVFX_ResetState: sym(&videofx, vf, "NvVFX_ResetState")?,
            NvVFX_Load: sym(&videofx, vf, "NvVFX_Load")?,
            NvVFX_Run: sym(&videofx, vf, "NvVFX_Run")?,
            NvCVImage_Alloc: sym(&nvcvimage, cv, "NvCVImage_Alloc")?,
            NvCVImage_Dealloc: sym(&nvcvimage, cv, "NvCVImage_Dealloc")?,
            NvCVImage_TransferFromYUV: sym(&nvcvimage, cv, "NvCVImage_TransferFromYUV")?,
            NvCVImage_TransferToYUV: sym(&nvcvimage, cv, "NvCVImage_TransferToYUV")?,
            NvCVImage_Composite: sym(&nvcvimage, cv, "NvCVImage_Composite")?,
            NvCV_GetErrorStringFromCode: sym(&nvcvimage, cv, "NvCV_GetErrorStringFromCode")?,
            _videofx: videofx,
            _nvcvimage: nvcvimage,
        }
    };
    let lib_dir = object_dir(sdk.NvVFX_Load as *const c_void);
    let model_dir = lib_dir.as_ref().map(|d| d.join("models"));
    Ok(Sdk {
        model_dir,
        lib_dir,
        ..sdk
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvcvimage_matches_the_documented_layout() {
        // 5 four-byte fields, 8 bytes, 3 pointers and a 64-bit size.
        assert_eq!(std::mem::size_of::<NvCvImage>(), 64);
        assert_eq!(std::mem::offset_of!(NvCvImage, pixel_bytes), 20);
        assert_eq!(std::mem::offset_of!(NvCvImage, colorspace), 25);
        assert_eq!(std::mem::offset_of!(NvCvImage, pixels), 32);
        assert_eq!(std::mem::offset_of!(NvCvImage, delete_proc), 48);
        assert_eq!(std::mem::offset_of!(NvCvImage, buffer_bytes), 56);
    }

    /// Names that cannot exist, so nothing is found on the search path.
    const NO_SUCH: LibNames = LibNames {
        videofx: "libobc-no-such-VideoFX-pin.so",
        nvcvimage: "libobc-no-such-NVCVImage-pin.so",
    };

    /// The error of an open that must fail.
    fn failed(r: Result<&'static Sdk, String>) -> String {
        match r {
            Ok(_) => panic!("an impossible SDK opened"),
            Err(e) => e,
        }
    }

    /// A small loadable system library (zlib) to stand in for an SDK one.
    fn small_library() -> PathBuf {
        [
            "/usr/lib/libz.so.1",
            "/usr/lib64/libz.so.1",
            "/lib/x86_64-linux-gnu/libz.so.1",
            "/usr/lib/x86_64-linux-gnu/libz.so.1",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .expect("these tests copy zlib (libz.so.1) into a fake SDK root")
    }

    /// A root that has one loadable library, as a feature library, and none
    /// of the libraries of [`NO_SUCH`].
    fn root_with_a_library(dir: &Path) -> PathBuf {
        let feat = dir.join("features/fx/lib");
        std::fs::create_dir_all(&feat).unwrap();
        std::fs::copy(small_library(), feat.join("libnvVFXFx.so")).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn where_an_open_takes_the_sdk_from() {
        let dir = tempfile::tempdir().unwrap();
        // No root, a missing one, or one without SDK libraries: the search
        // path.
        assert_eq!(Source::of(None, &NO_SUCH), Source::SearchPath(None));
        let nowhere = dir.path().join("no-sdk");
        assert_eq!(
            Source::of(Some(&nowhere), &NO_SUCH),
            Source::SearchPath(None)
        );
        assert_eq!(
            Source::of(Some(dir.path()), &NO_SUCH),
            Source::SearchPath(None)
        );
        // A root with SDK libraries: that root, canonical, however written.
        let a = root_with_a_library(&dir.path().join("a"));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        for written in [
            dir.path().join("a"),
            dir.path().join("a/."),
            dir.path().join("a/../a"),
            link,
        ] {
            assert_eq!(
                Source::of(Some(&written), &NO_SUCH),
                Source::Root(a.clone()),
                "{written:?}"
            );
        }
    }

    #[test]
    fn a_source_serves_only_an_open_from_the_same_place() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("lib")).unwrap();
        let root = dir.path().canonicalize().unwrap();
        let lib = root.join("lib");
        let other = PathBuf::from("/elsewhere/sdk");
        let (r, o) = (Source::Root(root.clone()), Source::Root(other.clone()));
        let search = Source::SearchPath(None);
        // A root serves itself only, never the search path.
        assert!(r.serves(&r));
        assert!(!r.serves(&o));
        assert!(!r.serves(&search));
        // The search path serves the search path, and the root whose lib
        // it served the SDK from.
        for s in [
            Source::SearchPath(None),
            Source::SearchPath(Some(lib.clone())),
        ] {
            assert!(s.serves(&search), "{s:?}");
            assert!(!s.serves(&o), "{s:?}");
        }
        assert!(Source::SearchPath(Some(lib)).serves(&r));
        assert!(!Source::SearchPath(None).serves(&r));
        assert!(!Source::SearchPath(Some(root.clone())).serves(&r));
    }

    #[test]
    fn failed_opens_that_load_nothing_do_not_pin() {
        let mut reg = Registry::default();
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        // A root with a library file that does not load, as one of the
        // SDK's: nothing loaded either.
        let lib = two.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(lib.join("libobc-no-such-NVCVImage-pin.so.1"), b"no library").unwrap();
        for root in [Some(one.path()), Some(two.path()), None, Some(one.path())] {
            let Err(e) = reg.open(root, &NO_SUCH) else {
                panic!("an impossible library opened");
            };
            assert!(
                e.contains("was not found") || e.contains("could not be loaded"),
                "{e}"
            );
            assert!(!e.contains("already loaded"), "{e}");
            assert_eq!(reg.pin, None, "{root:?}");
        }
        assert!(reg.opened.is_empty());
    }

    #[test]
    fn a_failed_open_that_loaded_a_library_pins_its_root() {
        let dir = tempfile::tempdir().unwrap();
        let a = root_with_a_library(&dir.path().join("a"));
        let b = root_with_a_library(&dir.path().join("b"));
        let mut reg = Registry::default();
        // The feature library loads, the SDK's own are missing: the open
        // fails without trying the search path, and the root is pinned.
        let e = failed(reg.open(Some(&a), &NO_SUCH));
        assert!(e.contains("is not in"), "{e}");
        assert!(e.contains("not tried"), "{e}");
        assert_eq!(reg.pin, Some(Source::Root(a.clone())));
        assert!(reg.opened.is_empty());
        // The same root may try again; anything else is refused.
        let e = failed(reg.open(Some(&a), &NO_SUCH));
        assert!(!e.contains("already loaded"), "{e}");
        let nowhere = dir.path().join("no-sdk");
        for other in [Some(b.as_path()), Some(nowhere.as_path()), None] {
            let e = failed(reg.open(other, &NO_SUCH));
            assert!(e.contains("already loaded in this process from"), "{e}");
            assert!(e.contains(&a.display().to_string()), "{e}");
            assert!(e.contains("Restart the process"), "{e}");
            assert_eq!(
                reg.conflict(other, &Source::of(other, &NO_SUCH)).as_deref(),
                Some(e.as_str())
            );
        }
        let e = failed(reg.open(None, &NO_SUCH));
        assert!(
            e.contains("the SDK root is now the library search path"),
            "{e}"
        );
        assert_eq!(reg.pin, Some(Source::Root(a)));
    }

    /// `Sdk` cannot be built without the real libraries, so the lookup that
    /// decides between reusing an SDK and loading again is tested on its own.
    #[test]
    fn a_root_open_reuses_the_sdk_the_search_path_loaded_from_its_lib() {
        let b = PathBuf::from("/sdk/b");
        let from_path = Source::SearchPath(Some("/sdk/b/lib".into()));
        let loaded = [(&from_path, &NO_SUCH)];
        // The root whose lib the search path served: the same SDK.
        assert_eq!(
            serving(loaded.iter().copied(), &NO_SUCH, &Source::Root(b)),
            None,
            "a lib that does not exist on disk is not canonical: not served"
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("lib")).unwrap();
        let from_path = Source::SearchPath(Some(root.join("lib")));
        let loaded = [(&from_path, &NO_SUCH)];
        let want = Source::Root(root.clone());
        assert_eq!(serving(loaded.iter().copied(), &NO_SUCH, &want), Some(0));
        // Other names, or another root: loaded again (or refused).
        assert_eq!(serving(loaded.iter().copied(), &SDK_LIBS, &want), None);
        let other = Source::Root(root.join("other"));
        assert_eq!(serving(loaded.iter().copied(), &NO_SUCH, &other), None);
        // An equal source is still found, and the search path again.
        let same = [(&want, &NO_SUCH)];
        assert_eq!(serving(same.iter().copied(), &NO_SUCH, &want), Some(0));
        let any = Source::SearchPath(None);
        assert_eq!(serving(loaded.iter().copied(), &NO_SUCH, &any), Some(0));
        // What `Registry::open` records for a search-path open is the
        // directory the loader reported (the pin), not the bare wish.
        let recorded = opened_source(Source::SearchPath(None), Some(&from_path));
        assert_eq!(recorded, from_path);
        let loaded = [(&recorded, &NO_SUCH)];
        assert_eq!(serving(loaded.iter().copied(), &NO_SUCH, &want), Some(0));
        assert_eq!(
            opened_source(Source::SearchPath(None), None),
            Source::SearchPath(None)
        );
        assert_eq!(opened_source(want.clone(), Some(&from_path)), want);
        // The registry sees the pin and the lookup agree.
        let reg = Registry {
            pin: Some(from_path),
            opened: Vec::new(),
        };
        assert_eq!(reg.conflict(Some(&root), &want), None);
    }

    #[test]
    fn a_retargeted_link_is_compared_by_what_was_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let a = root_with_a_library(&dir.path().join("a"));
        let b = root_with_a_library(&dir.path().join("b"));
        let link = dir.path().join("current");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let mut reg = Registry::default();
        let _ = reg.open(Some(&link), &NO_SUCH);
        assert_eq!(reg.pin, Some(Source::Root(a.clone())));
        // The same link, now to another SDK: refused, naming what is loaded.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        let e = failed(reg.open(Some(&link), &NO_SUCH));
        assert!(e.contains(&format!("from {}", a.display())), "{e}");
        assert!(e.contains(&format!("now {}", link.display())), "{e}");
        // Back to the loaded one: not refused.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let e = failed(reg.open(Some(&link), &NO_SUCH));
        assert!(!e.contains("already loaded"), "{e}");
    }

    #[test]
    fn a_search_path_pin_is_described_with_its_directory() {
        let reg = Registry {
            pin: Some(Source::SearchPath(Some("/sdk/lib".into()))),
            opened: Vec::new(),
        };
        let want = Source::Root("/other".into());
        let e = reg.conflict(Some(Path::new("/other")), &want).unwrap();
        assert!(e.contains("from the library search path (/sdk/lib)"), "{e}");
        assert!(e.contains("the SDK root is now /other"), "{e}");
        assert_eq!(reg.conflict(None, &Source::SearchPath(None)), None);
        // Nothing loaded: nothing conflicts.
        assert_eq!(Registry::default().conflict(None, &want), None);
    }

    #[test]
    fn the_loaded_root_is_the_pin() {
        assert_eq!(Registry::default().loaded_root(), None, "nothing loaded");
        let pinned = |pin: Source| Registry {
            pin: Some(pin),
            opened: Vec::new(),
        };
        assert_eq!(
            pinned(Source::Root("/sdk/a".into())).loaded_root(),
            Some(PathBuf::from("/sdk/a"))
        );
        assert_eq!(
            pinned(Source::SearchPath(Some("/sdk/b/lib".into()))).loaded_root(),
            Some(PathBuf::from("/sdk/b"))
        );
        assert_eq!(
            pinned(Source::SearchPath(Some("/usr/lib64/maxine".into()))).loaded_root(),
            None,
            "not a root's lib"
        );
        assert_eq!(pinned(Source::SearchPath(None)).loaded_root(), None);
    }

    #[test]
    fn candidates_include_the_versioned_soname() {
        let c = candidates(Path::new("/sdk/lib"), "libVideoFX.so");
        assert_eq!(
            c,
            vec![
                PathBuf::from("/sdk/lib/libVideoFX.so"),
                PathBuf::from("/sdk/lib/libVideoFX.so.1")
            ]
        );
    }
}
