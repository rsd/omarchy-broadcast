//! GStreamer's CUDA library (`libgstcuda-1.0.so.0`), opened at run time.
//!
//! A minimal subset declared by hand from the public GStreamer CUDA API
//! documentation (`gst/cuda/gstcudamemory.h`, `gstcudastream.h`,
//! `gstcudabufferpool.h`, `gstcudacontext.h`): only what the effect element
//! uses on its effect path. Nothing links against the library, so the crate
//! builds and its default tests run without it; the element opens it only
//! once an effect is requested ([`gst_cuda`]), never in passthrough.
//!
//! In a process that streams CUDA frames the library is already loaded (by
//! GStreamer's `nvcodec` plugin), so opening it here only takes another
//! reference to the same library.

use std::ffi::{OsStr, c_void};
use std::sync::OnceLock;

use gst::glib;
use libloading::Library;

/// The name the library is opened by.
pub const LIBRARY: &str = "libgstcuda-1.0.so.0";

/// `GST_MAP_CUDA` (`GST_MAP_FLAG_LAST << 1`): map the device memory of a
/// `GstCudaMemory` instead of a host staging copy.
pub const MAP_CUDA: u32 = gst::ffi::GST_MAP_FLAG_LAST << 1;

/// The caps feature of frames in CUDA memory.
pub const MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// The structure name of the CUDA context in a `GstContext`.
pub const CONTEXT_TYPE: &str = "gst.cuda.context";

/// The public head of `GstCudaMemory`: the `GstMemory` and the context the
/// memory belongs to (the structure goes on with fields this crate never
/// reads; it is only ever accessed through a pointer).
#[repr(C)]
pub struct CudaMemoryHead {
    pub mem: gst::ffi::GstMemory,
    /// `GstCudaContext *`, owned by the memory.
    pub context: *mut c_void,
}

/// The resolved entry points. Obtain the process-wide instance with
/// [`gst_cuda`].
#[allow(non_snake_case)] // fields are named after the C entry points
pub struct GstCuda {
    _lib: Library,
    pub gst_is_cuda_memory: unsafe extern "C" fn(*mut gst::ffi::GstMemory) -> glib::ffi::gboolean,
    pub gst_cuda_memory_get_stream: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub gst_cuda_memory_sync: unsafe extern "C" fn(*mut c_void),
    pub gst_cuda_context_get_handle: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub gst_cuda_stream_new: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub gst_cuda_stream_get_handle: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub gst_cuda_stream_unref: unsafe extern "C" fn(*mut c_void),
    pub gst_cuda_buffer_pool_new: unsafe extern "C" fn(*mut c_void) -> *mut gst::ffi::GstBufferPool,
    pub gst_buffer_pool_config_set_cuda_stream:
        unsafe extern "C" fn(*mut gst::ffi::GstStructure, *mut c_void),
}

static GST_CUDA: OnceLock<Result<GstCuda, String>> = OnceLock::new();

/// The process-wide table: the library opened and its entry points
/// resolved, once. A failure is returned (and remembered) as a reason; it
/// never panics.
pub fn gst_cuda() -> Result<&'static GstCuda, String> {
    GST_CUDA
        .get_or_init(|| GstCuda::open(OsStr::new(LIBRARY)))
        .as_ref()
        .map_err(Clone::clone)
}

/// Resolves `name` from `lib` as a value of type `T` (a function pointer).
///
/// # Safety
/// `T` must be the correct function pointer type for the C symbol `name`.
unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
    let mut cname = name.as_bytes().to_vec();
    cname.push(0);
    // SAFETY: the caller guarantees `T` matches the symbol's C signature; the
    // copied pointer stays valid while `lib` is loaded, and `GstCuda` keeps
    // `lib` alive alongside every pointer resolved from it.
    unsafe { lib.get::<T>(&cname) }
        .map(|s| *s)
        .map_err(|e| format!("{LIBRARY} has no {name}: {e}"))
}

impl GstCuda {
    fn open(path: &OsStr) -> Result<GstCuda, String> {
        // SAFETY: GStreamer's CUDA library has no load-time preconditions; in
        // a streaming process it is already loaded by the nvcodec plugin.
        let lib = unsafe { Library::new(path) }.map_err(|e| {
            format!(
                "{LIBRARY} could not be loaded ({e}); it comes with gst-plugins-bad's CUDA \
                 support, which the effects need"
            )
        })?;
        // SAFETY: every type below is the documented C signature of the
        // symbol it is resolved for (gst/cuda/*.h, GStreamer 1.24+).
        unsafe {
            Ok(GstCuda {
                gst_is_cuda_memory: sym(&lib, "gst_is_cuda_memory")?,
                gst_cuda_memory_get_stream: sym(&lib, "gst_cuda_memory_get_stream")?,
                gst_cuda_memory_sync: sym(&lib, "gst_cuda_memory_sync")?,
                gst_cuda_context_get_handle: sym(&lib, "gst_cuda_context_get_handle")?,
                gst_cuda_stream_new: sym(&lib, "gst_cuda_stream_new")?,
                gst_cuda_stream_get_handle: sym(&lib, "gst_cuda_stream_get_handle")?,
                gst_cuda_stream_unref: sym(&lib, "gst_cuda_stream_unref")?,
                gst_cuda_buffer_pool_new: sym(&lib, "gst_cuda_buffer_pool_new")?,
                gst_buffer_pool_config_set_cuda_stream: sym(
                    &lib,
                    "gst_buffer_pool_config_set_cuda_stream",
                )?,
                _lib: lib,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_cuda_matches_the_header_value() {
        // GST_MAP_READ_CUDA is documented as 131073 (gstcudamemory.h).
        assert_eq!(gst::ffi::GST_MAP_READ | MAP_CUDA, 131_073);
        assert_eq!(gst::ffi::GST_MAP_WRITE | MAP_CUDA, 131_074);
    }

    #[test]
    fn a_missing_library_is_a_reason_not_a_panic() {
        let e = GstCuda::open(OsStr::new("libgstcuda-does-not-exist.so"))
            .err()
            .expect("no such library");
        assert!(e.contains("could not be loaded"), "{e}");
    }
}
