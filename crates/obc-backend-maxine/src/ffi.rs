//! The subset of the Maxine Video Effects C API this backend uses, opened at
//! run time.
//!
//! Written by hand from NVIDIA's public API documentation for the Video
//! Effects SDK and NvCVImage (function signatures, the `NvCVImage` layout,
//! the parameter-name strings and the status codes used here). No SDK file
//! is copied or needed at build time (`docs/adr/0012-video-effects-and-maxine-backend.md`).

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
pub(crate) const NVCV_BGR: i32 = 5;
pub(crate) const NVCV_YUV420: i32 = 10;
// `NvCVImage_ComponentType`: unsigned 8-bit.
pub(crate) const NVCV_U8: i32 = 1;
// Layout: interleaved components.
pub(crate) const NVCV_CHUNKY: u32 = 0;
/// `NvCVImage_Alloc` alignment 1: rows exactly `width * pixel bytes` apart
/// (0 lets the SDK pad the pitch for the GPU).
pub(crate) const NVCV_TIGHT_PITCH: u32 = 1;
// Memory space: CUDA device memory.
pub(crate) const NVCV_GPU: u32 = 1;
// YUV colour-space flags, ORed: matrix, range, chroma siting.
pub(crate) const NVCV_601: u32 = 0x00;
pub(crate) const NVCV_709: u32 = 0x01;
pub(crate) const NVCV_VIDEO_RANGE: u32 = 0x00;
pub(crate) const NVCV_FULL_RANGE: u32 = 0x04;
pub(crate) const NVCV_CHROMA_COSITED: u32 = 0x00;

// Effect selectors and parameter names.
pub(crate) const FX_GREEN_SCREEN: &CStr = c"GreenScreen";
pub(crate) const FX_BACKGROUND_BLUR: &CStr = c"BackgroundBlur";
pub(crate) const FX_DENOISING: &CStr = c"Denoising";
pub(crate) const P_INPUT_IMAGE_0: &CStr = c"SrcImage0";
pub(crate) const P_INPUT_IMAGE_1: &CStr = c"SrcImage1";
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

/// The SDKs opened so far, by (root, names). Never closed: the SDK's
/// libraries stay mapped for the life of the process
/// (`docs/adr/0012-video-effects-and-maxine-backend.md`, Consequences).
#[allow(clippy::type_complexity)]
static OPENED: OnceLock<Mutex<Vec<((Option<PathBuf>, LibNames), &'static Sdk)>>> = OnceLock::new();

/// Opens the SDK (once per root), or explains why it cannot be opened.
pub(crate) fn open(root: Option<&Path>, names: &LibNames) -> Result<&'static Sdk, String> {
    let key = (root.map(Path::to_path_buf), names.clone());
    let opened = OPENED.get_or_init(|| Mutex::new(Vec::new()));
    let mut opened = opened.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((_, sdk)) = opened.iter().find(|(k, _)| *k == key) {
        return Ok(sdk);
    }
    let sdk: &'static Sdk = Box::leak(Box::new(load(root, names)?));
    opened.push((key, sdk));
    Ok(sdk)
}

/// The candidate file names of `name` in a directory: the unversioned name
/// and the major-versioned soname, as the SDK installs both.
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name), dir.join(format!("{name}.1"))]
}

/// Opens one library: from `<root>/lib` when it is there, else by plain
/// name from the library search path.
fn open_one(root: Option<&Path>, name: &str) -> Result<Library, String> {
    if let Some(root) = root {
        let lib_dir = root.join("lib");
        if let Some(path) = candidates(&lib_dir, name).into_iter().find(|p| p.exists()) {
            // SAFETY: loading an SDK library runs its initialisers, which
            // have no preconditions for the caller.
            return unsafe { Library::new(&path) }.map_err(|e| {
                format!(
                    "{} could not be loaded: {e}. The SDK's bundled CUDA, TensorRT and feature \
                     libraries must be on LD_LIBRARY_PATH when the process starts \
                     (docs/setup/maxine.md, section 4)",
                    path.display()
                )
            });
        }
    }
    // SAFETY: as above, for the library found on the search path.
    unsafe { Library::new(name) }.map_err(|e| match root {
        Some(root) => format!(
            "the NVIDIA Maxine Video Effects SDK was not found: no {name} in {} and none on \
             the library search path ({e}). The SDK root {} (MAXINE_VFX_ROOT, or {} when \
             that is unset) must be the SDK directory that contains lib/, features/ and \
             external/: reinstall the maxine-vfx-sdk-local package or set MAXINE_VFX_ROOT \
             to that directory (docs/setup/maxine.md, section 4)",
            root.join("lib").display(),
            root.display(),
            crate::DEFAULT_ROOT
        ),
        None => format!(
            "the NVIDIA Maxine Video Effects SDK was not found: MAXINE_VFX_ROOT is unset or \
             empty, {} is not a directory and {name} is not on the library search path ({e}). \
             Install the maxine-vfx-sdk-local package (built from omarchy-nvidia-sdks) or set \
             MAXINE_VFX_ROOT to the SDK directory (docs/setup/maxine.md, sections 2 and 5)",
            crate::DEFAULT_ROOT
        ),
    })
}

/// Resolves `name` from `lib` as a `T` (a function pointer).
///
/// # Safety
/// `T` must be the correct function pointer type for the C symbol `name`.
unsafe fn sym<T: Copy>(lib: &Library, lib_name: &str, name: &str) -> Result<T, String> {
    let mut cname = name.as_bytes().to_vec();
    cname.push(0);
    // SAFETY: the caller guarantees `T`; `Sdk` keeps `lib` loaded for as
    // long as the pointer is used (forever, see `OPENED`).
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

fn load(root: Option<&Path>, names: &LibNames) -> Result<Sdk, String> {
    // libVideoFX.so needs libNVCVImage.so (by that soname): open it first so
    // the loader reuses it when it is not on the search path.
    let nvcvimage = open_one(root, names.nvcvimage)?;
    let videofx = open_one(root, names.videofx)?;
    let (vf, cv) = (names.videofx, names.nvcvimage);
    // SAFETY (all `sym` calls): each field type is the documented C
    // signature of the entry point it is resolved from.
    let sdk = unsafe {
        Sdk {
            model_dir: None,
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
    let model_dir = object_dir(sdk.NvVFX_Load as *const c_void).map(|d| d.join("models"));
    Ok(Sdk { model_dir, ..sdk })
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
