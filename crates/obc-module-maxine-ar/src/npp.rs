//! The scaling of the auto-frame stage: NPP's `nppiResizeSqrPixel` on the
//! chain's BGR images, from the CUDA libraries the AR SDK bundles.
//!
//! Written by hand from NVIDIA's public NPP and CUDA runtime documentation
//! (the signatures, the `NppStreamContext`, `NppiSize` and `NppiRect`
//! layouts and the constants used here). No SDK file is copied or needed at
//! build time.
//!
//! `libnppig.so.12` (NPP's geometry functions) and `libcudart.so.12` are in
//! the `DT_NEEDED` closure of `libnvARPose.so`, so the SDK's preload has
//! loaded them already ([`crate::preload`]); [`open`] opens them again by
//! absolute path in the SDK's directories, which finds the same objects.
//! Nothing links against them.
//!
//! The `_Ctx` variant takes the stream context by value: the work goes to
//! the host's stream, and NPP's global stream (which the SDK's own NPP
//! calls may use) is never changed. The context is filled from the CUDA
//! runtime's attribute queries, which read the device of the context the
//! host made current; none creates or makes current a context.

use std::ffi::{c_int, c_uint, c_void};
use std::path::Path;
use std::sync::Mutex;

use libloading::Library;

use crate::ffi::CuStream;

/// `NppStatus`: 0 is success, negative values are errors and positive ones
/// warnings.
pub(crate) type NppStatus = c_int;
/// `cudaError_t`: 0 is success.
type CudaError = c_int;

/// `NPPI_INTER_CUBIC`.
pub(crate) const NPPI_INTER_CUBIC: c_int = 4;

// `cudaDeviceAttr` values.
const ATTR_MAX_THREADS_PER_BLOCK: c_int = 1;
const ATTR_MAX_SHARED_MEMORY_PER_BLOCK: c_int = 8;
const ATTR_MULTIPROCESSOR_COUNT: c_int = 16;
const ATTR_MAX_THREADS_PER_MULTIPROCESSOR: c_int = 39;
const ATTR_COMPUTE_CAPABILITY_MAJOR: c_int = 75;
const ATTR_COMPUTE_CAPABILITY_MINOR: c_int = 76;

/// The libraries, as the SDK bundles them.
pub(crate) const NPPIG: &str = "libnppig.so.12";
pub(crate) const CUDART: &str = "libcudart.so.12";

/// `NppiSize`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NppiSize {
    pub width: c_int,
    pub height: c_int,
}

/// `NppiRect`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NppiRect {
    pub x: c_int,
    pub y: c_int,
    pub width: c_int,
    pub height: c_int,
}

/// `NppStreamContext`: the stream and what NPP would otherwise ask the
/// runtime about the device.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct NppStreamContext {
    pub stream: CuStream,
    pub device: c_int,
    pub multiprocessor_count: c_int,
    pub max_threads_per_multiprocessor: c_int,
    pub max_threads_per_block: c_int,
    pub shared_mem_per_block: usize,
    pub compute_capability_major: c_int,
    pub compute_capability_minor: c_int,
    pub stream_flags: c_uint,
    pub reserved0: c_int,
}

/// The resolved entry points.
#[allow(non_snake_case)] // fields are named after the C entry points
pub(crate) struct Npp {
    _nppig: Library,
    _cudart: Library,
    #[allow(clippy::type_complexity)]
    nppiResizeSqrPixel_8u_C3R_Ctx: unsafe extern "C" fn(
        *const u8, // source
        NppiSize,  // source size
        c_int,     // source step
        NppiRect,  // source region
        *mut u8,   // destination
        c_int,     // destination step
        NppiRect,  // destination region
        f64,       // x factor
        f64,       // y factor
        f64,       // x shift
        f64,       // y shift
        c_int,     // interpolation
        NppStreamContext,
    ) -> NppStatus,
    cudaGetDevice: unsafe extern "C" fn(*mut c_int) -> CudaError,
    cudaDeviceGetAttribute: unsafe extern "C" fn(*mut c_int, c_int, c_int) -> CudaError,
    cudaStreamGetFlags: unsafe extern "C" fn(CuStream, *mut c_uint) -> CudaError,
}

// SAFETY: the entry points are plain functions of libraries that stay
// loaded; nothing in `Npp` is tied to a thread.
unsafe impl Send for Npp {}
// SAFETY: as above; the struct is read-only after it is made.
unsafe impl Sync for Npp {}

static OPENED: Mutex<Option<&'static Npp>> = Mutex::new(None);

/// Resolves `name` from `lib` as a `T`.
///
/// # Safety
/// `T` must be the correct function pointer type for the C symbol `name`.
unsafe fn sym<T: Copy>(lib: &Library, lib_name: &str, name: &str) -> Result<T, String> {
    let mut cname = name.as_bytes().to_vec();
    cname.push(0);
    // SAFETY: the caller guarantees `T`; `Npp` keeps `lib` loaded forever.
    unsafe { lib.get::<T>(&cname) }
        .map(|s| *s)
        .map_err(|e| format!("{lib_name} has no {name} ({e})"))
}

/// Opens NPP and the CUDA runtime of the SDK at `root` (after the SDK's
/// own open, which loads them), once per process. Creates no CUDA context.
pub(crate) fn open(root: &Path) -> Result<&'static Npp, String> {
    let mut opened = OPENED.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(n) = *opened {
        return Ok(n);
    }
    let dirs = crate::preload::search_dirs(root);
    let load = |name: &str| -> Result<Library, String> {
        let path = crate::preload::resolve(name, &dirs).ok_or_else(|| {
            format!(
                "{name} is not in the Maxine AR SDK at {} (its bundled CUDA libraries)",
                root.display()
            )
        })?;
        // SAFETY: the library is the SDK's bundled one, loaded already by
        // the SDK's preload; opening it again runs no initialiser.
        unsafe { Library::new(&path) }
            .map_err(|e| format!("{} could not be loaded: {e}", path.display()))
    };
    let nppig = load(NPPIG)?;
    let cudart = load(CUDART)?;
    // SAFETY (all `sym` calls): each field type is the documented C
    // signature of the entry point it is resolved from.
    let npp = unsafe {
        Npp {
            nppiResizeSqrPixel_8u_C3R_Ctx: sym(&nppig, NPPIG, "nppiResizeSqrPixel_8u_C3R_Ctx")?,
            cudaGetDevice: sym(&cudart, CUDART, "cudaGetDevice")?,
            cudaDeviceGetAttribute: sym(&cudart, CUDART, "cudaDeviceGetAttribute")?,
            cudaStreamGetFlags: sym(&cudart, CUDART, "cudaStreamGetFlags")?,
            _nppig: nppig,
            _cudart: cudart,
        }
    };
    let npp: &'static Npp = Box::leak(Box::new(npp));
    *opened = Some(npp);
    Ok(npp)
}

/// A BGR image of 8-bit chunky pixels in device memory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BgrView {
    pub pixels: *mut c_void,
    pub pitch: i32,
    pub width: u32,
    pub height: u32,
}

/// The factors and shifts that map the crop `(x, y, w, h)` of a
/// `width` x `height` image onto the whole of an image of that size:
/// `destination = source * factor + shift`.
pub(crate) fn crop_mapping(x: f64, y: f64, w: f64, h: f64, width: u32, height: u32) -> [f64; 4] {
    let fx = f64::from(width) / w;
    let fy = f64::from(height) / h;
    [fx, fy, -x * fx, -y * fy]
}

impl Npp {
    /// The stream context of `stream`, for the device of the current
    /// context.
    pub(crate) fn stream_context(&self, stream: CuStream) -> Result<NppStreamContext, String> {
        let check = |what: &str, rc: CudaError| {
            if rc == 0 {
                Ok(())
            } else {
                Err(format!("{what} returned CUDA error {rc}"))
            }
        };
        let mut device: c_int = 0;
        // SAFETY: an out-pointer; the call reads the current context.
        check("cudaGetDevice", unsafe {
            (self.cudaGetDevice)(&mut device)
        })?;
        let attr = |a: c_int| -> Result<c_int, String> {
            let mut v: c_int = 0;
            // SAFETY: an out-pointer, a documented attribute and the device
            // of the current context.
            check("cudaDeviceGetAttribute", unsafe {
                (self.cudaDeviceGetAttribute)(&mut v, a, device)
            })?;
            Ok(v)
        };
        let mut stream_flags: c_uint = 0;
        if !stream.is_null() {
            // SAFETY: the host's stream, of the current context; an
            // out-pointer.
            check("cudaStreamGetFlags", unsafe {
                (self.cudaStreamGetFlags)(stream, &mut stream_flags)
            })?;
        }
        Ok(NppStreamContext {
            stream,
            device,
            multiprocessor_count: attr(ATTR_MULTIPROCESSOR_COUNT)?,
            max_threads_per_multiprocessor: attr(ATTR_MAX_THREADS_PER_MULTIPROCESSOR)?,
            max_threads_per_block: attr(ATTR_MAX_THREADS_PER_BLOCK)?,
            shared_mem_per_block: attr(ATTR_MAX_SHARED_MEMORY_PER_BLOCK)?.max(0) as usize,
            compute_capability_major: attr(ATTR_COMPUTE_CAPABILITY_MAJOR)?,
            compute_capability_minor: attr(ATTR_COMPUTE_CAPABILITY_MINOR)?,
            stream_flags,
            reserved0: 0,
        })
    }

    /// Scales the crop `(x, y, w, h)` of `src` (in pixels, inside it) to the
    /// whole of `dst`, of the same size, on the context's stream.
    ///
    /// # Safety
    /// `src` and `dst` must describe distinct live device images of the
    /// current context, of the size they say, until the work on the
    /// context's stream has run.
    pub(crate) unsafe fn crop_to(
        &self,
        ctx: &NppStreamContext,
        src: &BgrView,
        dst: &BgrView,
        crop: (f64, f64, f64, f64),
    ) -> Result<(), String> {
        let (w, h) = (src.width as c_int, src.height as c_int);
        let whole = NppiRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let [fx, fy, sx, sy] = crop_mapping(crop.0, crop.1, crop.2, crop.3, src.width, src.height);
        // SAFETY: the caller's contract for the images; the regions are
        // the images themselves.
        let rc = unsafe {
            (self.nppiResizeSqrPixel_8u_C3R_Ctx)(
                src.pixels as *const u8,
                NppiSize {
                    width: w,
                    height: h,
                },
                src.pitch,
                whole,
                dst.pixels as *mut u8,
                dst.pitch,
                NppiRect {
                    x: 0,
                    y: 0,
                    width: dst.width as c_int,
                    height: dst.height as c_int,
                },
                fx,
                fy,
                sx,
                sy,
                NPPI_INTER_CUBIC,
                *ctx,
            )
        };
        if rc < 0 {
            return Err(format!(
                "nppiResizeSqrPixel_8u_C3R_Ctx returned NPP error {rc}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_structures_match_the_documented_layouts() {
        assert_eq!(std::mem::size_of::<NppiSize>(), 8);
        assert_eq!(std::mem::size_of::<NppiRect>(), 16);
        // A pointer, four ints, a size_t, three ints and a reserved one.
        assert_eq!(std::mem::size_of::<NppStreamContext>(), 48);
        assert_eq!(
            std::mem::offset_of!(NppStreamContext, shared_mem_per_block),
            24
        );
        assert_eq!(
            std::mem::offset_of!(NppStreamContext, compute_capability_major),
            32
        );
        assert_eq!(std::mem::offset_of!(NppStreamContext, stream_flags), 40);
    }

    #[test]
    fn a_crop_maps_onto_the_whole_image() {
        // The crop of two thirds at (320, 180) of 1920x1080.
        let [fx, fy, sx, sy] = crop_mapping(320.0, 180.0, 1280.0, 720.0, 1920, 1080);
        assert_eq!((fx, fy), (1.5, 1.5));
        // Its corners land on the image's.
        assert_eq!(320.0 * fx + sx, 0.0);
        assert_eq!(180.0 * fy + sy, 0.0);
        assert_eq!((320.0 + 1280.0) * fx + sx, 1920.0);
        assert_eq!((180.0 + 720.0) * fy + sy, 1080.0);
        // The whole image maps onto itself.
        assert_eq!(
            crop_mapping(0.0, 0.0, 1920.0, 1080.0, 1920, 1080),
            [1.0, 1.0, 0.0, 0.0]
        );
    }

    #[test]
    fn a_root_without_the_libraries_does_not_open() {
        let dir = tempfile::tempdir().unwrap();
        // Another test of this binary may have opened a real SDK already.
        if OPENED.lock().unwrap().is_some() {
            return;
        }
        let e = open(dir.path()).err().expect("an empty root opened NPP");
        assert!(e.contains("libnppig.so.12 is not in"), "{e}");
    }
}
