//! The subset of the Maxine AR SDK C API this module uses, opened at run
//! time.
//!
//! Written by hand from NVIDIA's public API documentation for the AR SDK
//! and NvCVImage (function signatures, the `NvCVImage` and `NvAR_BBoxes`
//! layouts, the parameter-name strings and the status codes used here). No
//! SDK file is copied or needed at build time.
//!
//! The SDK is loaded once per process, from one root: its libraries stay
//! mapped (`RTLD_GLOBAL`, never closed) and carry the same sonames as any
//! other copy of the SDK, so a second root could not be loaded beside the
//! first. An open of another root is refused, naming the restart it takes.

use std::ffi::{CStr, c_char, c_ulong, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use libloading::Library;

/// `NvCV_Status`: 0 is success, negative values are errors.
pub(crate) type NvCvStatus = i32;
/// `NvAR_FeatureHandle`: an opaque feature instance.
pub(crate) type FeatureHandle = *mut c_void;
/// `CUstream` as the SDK takes it.
pub(crate) type CuStream = *mut c_void;

pub(crate) const NVCV_SUCCESS: NvCvStatus = 0;
/// "The given parameter selector and type is not valid in this feature".
pub(crate) const NVCV_ERR_SELECTOR: NvCvStatus = -5;
/// "There was a problem deserializing the inference runtime engine". What
/// `NvAR_Load` returns on a distribution the SDK does not list, without the
/// os-release override (seen with the AR SDK 1.2).
pub(crate) const NVCV_ERR_TRT_ENGINE: NvCvStatus = -30;
/// "No suitable kernel image is available for the device", what the Video
/// Effects SDK returns in that case.
pub(crate) const NVCV_ERR_CUDA_KERNEL: NvCvStatus = -1209;

// `NvCVImage_PixelFormat` values.
pub(crate) const NVCV_BGR: i32 = 5;
pub(crate) const NVCV_YUV420: i32 = 10;
// `NvCVImage_ComponentType`: unsigned 8-bit.
pub(crate) const NVCV_U8: i32 = 1;
// Layout: interleaved components.
pub(crate) const NVCV_CHUNKY: u32 = 0;
/// `NvCVImage_Alloc` alignment 1: rows exactly `width * pixel bytes` apart.
pub(crate) const NVCV_TIGHT_PITCH: u32 = 1;
// Memory space: CUDA device memory.
pub(crate) const NVCV_GPU: u32 = 1;
// YUV colour-space flags, ORed: matrix, range, chroma siting.
pub(crate) const NVCV_601: u32 = 0x00;
pub(crate) const NVCV_709: u32 = 0x01;
pub(crate) const NVCV_2020: u32 = 0x02;
pub(crate) const NVCV_VIDEO_RANGE: u32 = 0x00;
pub(crate) const NVCV_FULL_RANGE: u32 = 0x04;
pub(crate) const NVCV_CHROMA_COSITED: u32 = 0x00;
pub(crate) const NVCV_CHROMA_INTSTITIAL: u32 = 0x08;
pub(crate) const NVCV_CHROMA_TOPLEFT: u32 = 0x10;

// Feature ids and parameter names (`NvAR_Parameter_*` expand to these).
pub(crate) const FEATURE_GAZE_REDIRECTION: &CStr = c"GazeRedirection";
pub(crate) const FEATURE_FACE_BOX_DETECTION: &CStr = c"FaceBoxDetection";
pub(crate) const P_CUDA_STREAM: &CStr = c"NvAR_Parameter_Config_CUDAStream";
pub(crate) const P_MODEL_DIR: &CStr = c"NvAR_Parameter_Config_ModelDir";
pub(crate) const P_TEMPORAL: &CStr = c"NvAR_Parameter_Config_Temporal";
pub(crate) const P_GAZE_REDIRECT: &CStr = c"NvAR_Parameter_Config_GazeRedirect";
pub(crate) const P_ENABLE_LOOK_AWAY: &CStr = c"NvAR_Parameter_Config_EnableLookAway";
pub(crate) const P_LANDMARKS_SIZE: &CStr = c"NvAR_Parameter_Config_Landmarks_Size";
pub(crate) const P_IN_IMAGE: &CStr = c"NvAR_Parameter_Input_Image";
pub(crate) const P_IN_WIDTH: &CStr = c"NvAR_Parameter_Input_Width";
pub(crate) const P_IN_HEIGHT: &CStr = c"NvAR_Parameter_Input_Height";
pub(crate) const P_OUT_IMAGE: &CStr = c"NvAR_Parameter_Output_Image";
pub(crate) const P_OUT_LANDMARKS: &CStr = c"NvAR_Parameter_Output_Landmarks";
pub(crate) const P_OUT_LANDMARKS_CONFIDENCE: &CStr = c"NvAR_Parameter_Output_LandmarksConfidence";
pub(crate) const P_OUT_HEAD_POSE: &CStr = c"NvAR_Parameter_Output_HeadPose";
pub(crate) const P_OUT_GAZE_VECTOR: &CStr = c"NvAR_Parameter_Output_OutputGazeVector";
pub(crate) const P_OUT_HEAD_TRANSLATION: &CStr = c"NvAR_Parameter_Output_OutputHeadTranslation";
pub(crate) const P_OUT_GAZE_DIRECTION: &CStr = c"NvAR_Parameter_Output_GazeDirection";
pub(crate) const P_OUT_BOUNDING_BOXES: &CStr = c"NvAR_Parameter_Output_BoundingBoxes";
pub(crate) const P_OUT_BOUNDING_BOXES_CONFIDENCE: &CStr =
    c"NvAR_Parameter_Output_BoundingBoxesConfidence";

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

/// `NvAR_Rect`: a box in pixels.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct NvArRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// `NvAR_BBoxes`: an array of boxes the caller owns, its count and size.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct NvArBBoxes {
    pub boxes: *mut NvArRect,
    pub num_boxes: u8,
    pub max_boxes: u8,
}

/// `NvAR_Point2f`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct NvArPoint2f {
    pub x: f32,
    pub y: f32,
}

/// `NvAR_Point3f`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct NvArPoint3f {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// `NvAR_Quaternion`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct NvArQuaternion {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

/// The resolved SDK entry points (`libnvARPose.so` and `libNVCVImage.so`).
#[allow(non_snake_case)] // fields are named after the C entry points
pub(crate) struct Sdk {
    _ar: Library,
    _nvcvimage: Library,
    /// Where the SDK's models are: `<root>/lib/models`.
    pub model_dir: PathBuf,
    pub NvAR_Create: unsafe extern "C" fn(*const c_char, *mut FeatureHandle) -> NvCvStatus,
    pub NvAR_Destroy: unsafe extern "C" fn(FeatureHandle) -> NvCvStatus,
    pub NvAR_Load: unsafe extern "C" fn(FeatureHandle) -> NvCvStatus,
    pub NvAR_Run: unsafe extern "C" fn(FeatureHandle) -> NvCvStatus,
    pub NvAR_SetU32: unsafe extern "C" fn(FeatureHandle, *const c_char, u32) -> NvCvStatus,
    pub NvAR_SetS32: unsafe extern "C" fn(FeatureHandle, *const c_char, i32) -> NvCvStatus,
    pub NvAR_SetString:
        unsafe extern "C" fn(FeatureHandle, *const c_char, *const c_char) -> NvCvStatus,
    pub NvAR_SetCudaStream:
        unsafe extern "C" fn(FeatureHandle, *const c_char, CuStream) -> NvCvStatus,
    pub NvAR_SetObject:
        unsafe extern "C" fn(FeatureHandle, *const c_char, *mut c_void, c_ulong) -> NvCvStatus,
    pub NvAR_SetF32Array:
        unsafe extern "C" fn(FeatureHandle, *const c_char, *mut f32, i32) -> NvCvStatus,
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
    pub NvCVImage_Transfer: unsafe extern "C" fn(
        *const NvCvImage,
        *mut NvCvImage,
        f32,            // scale
        CuStream,       // stream
        *mut NvCvImage, // staging image
    ) -> NvCvStatus,
    NvCV_GetErrorStringFromCode: unsafe extern "C" fn(NvCvStatus) -> *const c_char,
}

/// Destroys the handle a failed `NvAR_Create` may still have set (null:
/// nothing to destroy), so that no feature leaks.
pub(crate) fn destroy_failed(sdk: &Sdk, handle: FeatureHandle) {
    if !handle.is_null() {
        // SAFETY: a handle NvAR_Create set, owned by no one else, destroyed
        // once.
        unsafe { (sdk.NvAR_Destroy)(handle) };
    }
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

    /// `call returned code (description)`, with what to do for the codes
    /// that have a known cause.
    pub(crate) fn explain(&self, call: &str, code: NvCvStatus) -> String {
        let base = format!("{call} returned {code} ({})", self.error_string(code));
        match code {
            NVCV_ERR_TRT_ENGINE | NVCV_ERR_CUDA_KERNEL if call.starts_with("NvAR_Load") => {
                format!(
                    "{base}: on a distribution the SDK does not list this is its /etc/os-release \
                     check; install the maxine-ar-sdk-local package: the daemon applies its \
                     os-release override (LD_PRELOAD of its shim and MAXINE_OS_RELEASE) when it \
                     starts and logs it, unless OBC_NO_SDK_ENV is set"
                )
            }
            _ => base,
        }
    }
}

/// The core libraries, in `<root>/lib`, in load order.
pub(crate) const NVCVIMAGE: &str = "libNVCVImage.so";
pub(crate) const AR_LOCAL: &str = "libnvARPoseLocal.so";
pub(crate) const AR: &str = "libnvARPose.so";
pub(crate) const CORE: [&str; 3] = [NVCVIMAGE, AR_LOCAL, AR];

/// What this process has loaded of the SDK.
struct Loaded {
    /// The root the first library was loaded from (canonical). Set as soon
    /// as one library is loaded, even when the open then fails: what was
    /// loaded stays loaded, so a later open must take the SDK from there.
    root: PathBuf,
    /// The SDK, once an open succeeded.
    sdk: Option<&'static Sdk>,
}

static LOADED: Mutex<Option<Loaded>> = Mutex::new(None);

/// The candidate file names of `name` in a directory: the unversioned name
/// and the major-versioned soname.
fn candidate(dir: &Path, name: &str) -> Option<PathBuf> {
    [dir.join(name), dir.join(format!("{name}.1"))]
        .into_iter()
        .find(|p| p.is_file())
}

/// Resolves `name` from `lib` as a `T` (a function pointer).
///
/// # Safety
/// `T` must be the correct function pointer type for the C symbol `name`.
unsafe fn sym<T: Copy>(lib: &Library, lib_name: &str, name: &str) -> Result<T, String> {
    let mut cname = name.as_bytes().to_vec();
    cname.push(0);
    // SAFETY: the caller guarantees `T`; `Sdk` keeps `lib` loaded for as
    // long as the pointer is used (forever, see `LOADED`).
    unsafe { lib.get::<T>(&cname) }
        .map(|s| *s)
        .map_err(|e| format!("{lib_name} has no {name} ({e}); is this Maxine AR SDK 1.2 or later?"))
}

/// Opens the SDK from `root` (its libraries and those of `features`, by
/// absolute path), or explains why it cannot. Once a library has been
/// loaded from a root, every later open must name the same root: another
/// is refused, naming the restart it takes. Creates no CUDA context.
pub(crate) fn open(root: &Path, features: &[&str]) -> Result<&'static Sdk, String> {
    let mut loaded = LOADED.lock().unwrap_or_else(|p| p.into_inner());
    let canonical = root.canonicalize().map_err(|e| {
        format!(
            "the Maxine AR SDK root {} cannot be read: {e}",
            root.display()
        )
    })?;
    if let Some(l) = loaded.as_ref() {
        if l.root != canonical {
            return Err(format!(
                "the NVIDIA Maxine AR SDK is already loaded in this process from {}, and a \
                 second SDK cannot be loaded beside it: the SDK root is now {}. Restart the \
                 process to use the new root (the daemon: systemctl --user restart \
                 omarchy-broadcastd)",
                l.root.display(),
                root.display()
            ));
        }
        if let Some(sdk) = l.sdk {
            return Ok(sdk);
        }
    }
    let done = crate::preload::preload(&canonical, &CORE, features);
    if !done.loaded.is_empty() && loaded.is_none() {
        *loaded = Some(Loaded {
            root: canonical.clone(),
            sdk: None,
        });
    }
    let with_failures = |e: String| match done.failures() {
        Some(f) => format!("{e}. SDK libraries that could not be loaded first: {f}"),
        None => e,
    };
    let lib_dir = canonical.join("lib");
    let open_one = |name: &str| -> Result<Library, String> {
        let path = candidate(&lib_dir, name).ok_or_else(|| {
            format!(
                "{name} is not in {}: the Maxine AR SDK root {} is not a complete SDK (the \
                 directory that contains lib/, features/ and external/)",
                lib_dir.display(),
                root.display()
            )
        })?;
        // SAFETY: loading an SDK library runs its initialisers, which have
        // no preconditions for the caller.
        unsafe { Library::new(&path) }
            .map_err(|e| format!("{} could not be loaded: {e}", path.display()))
    };
    let nvcvimage = open_one(NVCVIMAGE).map_err(with_failures)?;
    let ar = open_one(AR).map_err(with_failures)?;
    if loaded.is_none() {
        *loaded = Some(Loaded {
            root: canonical.clone(),
            sdk: None,
        });
    }
    let (a, c) = (AR, NVCVIMAGE);
    // SAFETY (all `sym` calls): each field type is the documented C
    // signature of the entry point it is resolved from.
    let sdk = unsafe {
        Sdk {
            model_dir: canonical.join("lib/models"),
            NvAR_Create: sym(&ar, a, "NvAR_Create")?,
            NvAR_Destroy: sym(&ar, a, "NvAR_Destroy")?,
            NvAR_Load: sym(&ar, a, "NvAR_Load")?,
            NvAR_Run: sym(&ar, a, "NvAR_Run")?,
            NvAR_SetU32: sym(&ar, a, "NvAR_SetU32")?,
            NvAR_SetS32: sym(&ar, a, "NvAR_SetS32")?,
            NvAR_SetString: sym(&ar, a, "NvAR_SetString")?,
            NvAR_SetCudaStream: sym(&ar, a, "NvAR_SetCudaStream")?,
            NvAR_SetObject: sym(&ar, a, "NvAR_SetObject")?,
            NvAR_SetF32Array: sym(&ar, a, "NvAR_SetF32Array")?,
            NvCVImage_Alloc: sym(&nvcvimage, c, "NvCVImage_Alloc")?,
            NvCVImage_Dealloc: sym(&nvcvimage, c, "NvCVImage_Dealloc")?,
            NvCVImage_TransferFromYUV: sym(&nvcvimage, c, "NvCVImage_TransferFromYUV")?,
            NvCVImage_TransferToYUV: sym(&nvcvimage, c, "NvCVImage_TransferToYUV")?,
            NvCVImage_Transfer: sym(&nvcvimage, c, "NvCVImage_Transfer")?,
            NvCV_GetErrorStringFromCode: sym(&nvcvimage, c, "NvCV_GetErrorStringFromCode")?,
            _ar: ar,
            _nvcvimage: nvcvimage,
        }
    };
    let sdk: &'static Sdk = Box::leak(Box::new(sdk));
    if let Some(l) = loaded.as_mut() {
        l.sdk = Some(sdk);
    }
    Ok(sdk)
}

/// The root the process's SDK was loaded from, if any. Opens nothing.
#[cfg(test)]
pub(crate) fn loaded_root() -> Option<PathBuf> {
    LOADED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|l| l.root.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_structures_match_the_documented_layouts() {
        // 5 four-byte fields, 8 bytes, 3 pointers and a 64-bit size.
        assert_eq!(std::mem::size_of::<NvCvImage>(), 64);
        assert_eq!(std::mem::offset_of!(NvCvImage, pixel_bytes), 20);
        assert_eq!(std::mem::offset_of!(NvCvImage, colorspace), 25);
        assert_eq!(std::mem::offset_of!(NvCvImage, pixels), 32);
        assert_eq!(std::mem::offset_of!(NvCvImage, delete_proc), 48);
        assert_eq!(std::mem::offset_of!(NvCvImage, buffer_bytes), 56);
        // A pointer and two bytes, padded.
        assert_eq!(std::mem::size_of::<NvArBBoxes>(), 16);
        assert_eq!(std::mem::offset_of!(NvArBBoxes, num_boxes), 8);
        assert_eq!(std::mem::offset_of!(NvArBBoxes, max_boxes), 9);
        assert_eq!(std::mem::size_of::<NvArRect>(), 16);
        assert_eq!(std::mem::size_of::<NvArPoint2f>(), 8);
        assert_eq!(std::mem::size_of::<NvArPoint3f>(), 12);
        assert_eq!(std::mem::size_of::<NvArQuaternion>(), 16);
    }

    #[test]
    fn a_root_without_the_sdk_does_not_open_and_does_not_pin() {
        let dir = tempfile::tempdir().unwrap();
        let e = match open(dir.path(), &[]) {
            Ok(_) => panic!("an empty root opened"),
            Err(e) => e,
        };
        assert!(e.contains("libNVCVImage.so is not in"), "{e}");
        // Nothing was loaded, so nothing is pinned by this open (another
        // test of this binary may have pinned a real SDK).
        assert_ne!(loaded_root(), dir.path().canonicalize().ok());
        let missing = dir.path().join("absent");
        let e = open(&missing, &[]).err().expect("a missing root opened");
        assert!(e.contains("cannot be read"), "{e}");
    }

    #[test]
    fn the_parameter_names_are_the_macro_expansions() {
        for (name, suffix) in [
            (P_MODEL_DIR, "Config_ModelDir"),
            (P_CUDA_STREAM, "Config_CUDAStream"),
            (P_IN_IMAGE, "Input_Image"),
            (P_OUT_IMAGE, "Output_Image"),
            (P_OUT_BOUNDING_BOXES, "Output_BoundingBoxes"),
            (
                P_OUT_BOUNDING_BOXES_CONFIDENCE,
                "Output_BoundingBoxesConfidence",
            ),
        ] {
            assert_eq!(name.to_str().unwrap(), format!("NvAR_Parameter_{suffix}"));
        }
    }
}
