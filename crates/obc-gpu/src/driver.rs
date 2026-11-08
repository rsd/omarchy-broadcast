//! The CUDA driver API, opened at run time.
//!
//! A minimal subset of `libcuda.so.1`, declared by hand from NVIDIA's public
//! CUDA Driver API documentation: only the entry points the effect path
//! uses, under their versioned export names (`cuCtxCreate_v2`, ...). Nothing
//! links against the driver, so the workspace builds and runs its default
//! tests on a machine without it.

use std::ffi::{CStr, OsStr, c_char, c_void};
use std::sync::OnceLock;

use libloading::Library;

use crate::GpuError;

/// `CUresult`: 0 is success.
pub(crate) type CuResult = i32;
/// `CUcontext`, an opaque driver handle.
pub type CuContext = *mut c_void;
/// `CUstream`, an opaque driver handle; null is the legacy default stream.
pub type CuStream = *mut c_void;
/// `CUevent`, an opaque driver handle.
pub type CuEvent = *mut c_void;
/// `CUdevice`, a device ordinal handle.
pub type CuDevice = i32;
/// `CUdeviceptr`, a device address.
pub type CuDevicePtr = u64;
/// `CUmodule`, an opaque driver handle to loaded code.
pub type CuModule = *mut c_void;
/// `CUfunction`, an opaque driver handle to a kernel of a [`CuModule`].
pub type CuFunction = *mut c_void;

pub(crate) const CUDA_SUCCESS: CuResult = 0;
/// `CUDA_ERROR_NOT_READY`: returned by `cuEventQuery` while work is pending.
pub(crate) const CUDA_ERROR_NOT_READY: CuResult = 600;

/// `CUmemorytype` values for [`Memcpy2d`].
pub(crate) const MEMORYTYPE_HOST: u32 = 1;
pub(crate) const MEMORYTYPE_DEVICE: u32 = 2;

/// The argument of `cuMemcpy2D_v2` / `cuMemcpy2DAsync_v2` (`CUDA_MEMCPY2D`),
/// laid out as the driver documents it: for source then destination, the X
/// offset in bytes, the Y offset in rows, the memory type, the host pointer,
/// the device pointer, the array handle and the pitch; then the width in
/// bytes and the height in rows.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Memcpy2d {
    pub src_x_in_bytes: usize,
    pub src_y: usize,
    pub src_memory_type: u32,
    pub src_host: *const c_void,
    pub src_device: CuDevicePtr,
    pub src_array: *mut c_void,
    pub src_pitch: usize,
    pub dst_x_in_bytes: usize,
    pub dst_y: usize,
    pub dst_memory_type: u32,
    pub dst_host: *mut c_void,
    pub dst_device: CuDevicePtr,
    pub dst_array: *mut c_void,
    pub dst_pitch: usize,
    pub width_in_bytes: usize,
    pub height: usize,
}

impl Memcpy2d {
    /// A copy with every field zeroed (no offsets, null pointers).
    pub(crate) fn zeroed() -> Self {
        Memcpy2d {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: 0,
            src_host: std::ptr::null(),
            src_device: 0,
            src_array: std::ptr::null_mut(),
            src_pitch: 0,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: 0,
            dst_host: std::ptr::null_mut(),
            dst_device: 0,
            dst_array: std::ptr::null_mut(),
            dst_pitch: 0,
            width_in_bytes: 0,
            height: 0,
        }
    }
}

/// The resolved driver entry points. Obtain the process-wide instance with
/// [`driver`]; the library stays loaded for the life of the process.
#[allow(non_snake_case)] // fields are named after the C entry points
pub struct Driver {
    _lib: Library,
    pub(crate) cuInit: unsafe extern "C" fn(u32) -> CuResult,
    pub(crate) cuDeviceGet: unsafe extern "C" fn(*mut CuDevice, i32) -> CuResult,
    pub(crate) cuDeviceGetCount: unsafe extern "C" fn(*mut i32) -> CuResult,
    pub(crate) cuDevicePrimaryCtxGetState:
        unsafe extern "C" fn(CuDevice, *mut u32, *mut i32) -> CuResult,
    pub(crate) cuCtxCreate_v2: unsafe extern "C" fn(*mut CuContext, u32, CuDevice) -> CuResult,
    pub(crate) cuCtxDestroy_v2: unsafe extern "C" fn(CuContext) -> CuResult,
    pub(crate) cuCtxPushCurrent_v2: unsafe extern "C" fn(CuContext) -> CuResult,
    pub(crate) cuCtxPopCurrent_v2: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    pub(crate) cuCtxGetCurrent: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    pub(crate) cuCtxGetDevice: unsafe extern "C" fn(*mut CuDevice) -> CuResult,
    pub(crate) cuMemGetInfo_v2: unsafe extern "C" fn(*mut usize, *mut usize) -> CuResult,
    pub(crate) cuMemAllocPitch_v2:
        unsafe extern "C" fn(*mut CuDevicePtr, *mut usize, usize, usize, u32) -> CuResult,
    pub(crate) cuMemFree_v2: unsafe extern "C" fn(CuDevicePtr) -> CuResult,
    pub(crate) cuMemsetD2D8Async:
        unsafe extern "C" fn(CuDevicePtr, usize, u8, usize, usize, CuStream) -> CuResult,
    pub(crate) cuMemcpy2D_v2: unsafe extern "C" fn(*const Memcpy2d) -> CuResult,
    pub(crate) cuMemcpy2DAsync_v2: unsafe extern "C" fn(*const Memcpy2d, CuStream) -> CuResult,
    pub(crate) cuStreamCreate: unsafe extern "C" fn(*mut CuStream, u32) -> CuResult,
    pub(crate) cuStreamDestroy_v2: unsafe extern "C" fn(CuStream) -> CuResult,
    pub(crate) cuStreamSynchronize: unsafe extern "C" fn(CuStream) -> CuResult,
    pub(crate) cuEventCreate: unsafe extern "C" fn(*mut CuEvent, u32) -> CuResult,
    pub(crate) cuEventDestroy_v2: unsafe extern "C" fn(CuEvent) -> CuResult,
    pub(crate) cuEventRecord: unsafe extern "C" fn(CuEvent, CuStream) -> CuResult,
    pub(crate) cuEventQuery: unsafe extern "C" fn(CuEvent) -> CuResult,
    pub(crate) cuEventSynchronize: unsafe extern "C" fn(CuEvent) -> CuResult,
    pub(crate) cuEventElapsedTime: unsafe extern "C" fn(*mut f32, CuEvent, CuEvent) -> CuResult,
    pub(crate) cuModuleLoadData: unsafe extern "C" fn(*mut CuModule, *const c_void) -> CuResult,
    pub(crate) cuModuleGetFunction:
        unsafe extern "C" fn(*mut CuFunction, CuModule, *const c_char) -> CuResult,
    /// Grid x/y/z, block x/y/z, dynamic shared memory bytes, stream,
    /// kernel parameters, extra options.
    #[allow(clippy::type_complexity)]
    pub(crate) cuLaunchKernel: unsafe extern "C" fn(
        CuFunction,
        u32,
        u32,
        u32,
        u32,
        u32,
        u32,
        u32,
        CuStream,
        *mut *mut c_void,
        *mut *mut c_void,
    ) -> CuResult,
    pub(crate) cuModuleUnload: unsafe extern "C" fn(CuModule) -> CuResult,
    cuGetErrorName: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
    cuGetErrorString: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
}

/// The name the driver library is opened by.
pub const DRIVER_LIBRARY: &str = "libcuda.so.1";

static DRIVER: OnceLock<Result<Driver, GpuError>> = OnceLock::new();

/// The process-wide driver table: `libcuda.so.1` opened, its entry points
/// resolved and `cuInit(0)` called, once. A failure is returned (and
/// remembered) as [`GpuError::Unavailable`]; it never panics.
///
/// `cuInit` creates no context.
pub fn driver() -> Result<&'static Driver, GpuError> {
    DRIVER
        .get_or_init(|| Driver::open(OsStr::new(DRIVER_LIBRARY)))
        .as_ref()
        .map_err(Clone::clone)
}

/// Resolves `name` from `lib` as a value of type `T` (a function pointer).
///
/// # Safety
/// `T` must be the correct function pointer type for the C symbol `name`.
unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> Result<T, GpuError> {
    let mut cname = name.as_bytes().to_vec();
    cname.push(0);
    // SAFETY: the caller guarantees `T` matches the symbol's C signature; the
    // copied pointer stays valid while `lib` is loaded, and `Driver` keeps
    // `lib` alive alongside every pointer resolved from it.
    unsafe { lib.get::<T>(&cname) }
        .map(|s| *s)
        .map_err(|e| GpuError::Unavailable(format!("{DRIVER_LIBRARY} has no {name}: {e}")))
}

impl Driver {
    /// Opens the driver library at `path` and resolves every entry point.
    pub(crate) fn open(path: &OsStr) -> Result<Driver, GpuError> {
        // SAFETY: loading the NVIDIA driver library runs its initialisers,
        // which have no preconditions; any other library at `path` is the
        // caller's choice (tests pass a path that does not exist).
        let lib = unsafe { Library::new(path) }.map_err(|e| {
            GpuError::Unavailable(format!(
                "the NVIDIA CUDA driver could not be loaded ({}: {e}); is the NVIDIA driver installed?",
                path.to_string_lossy()
            ))
        })?;
        // SAFETY (all `sym` calls below): each field type is the documented C
        // signature of the entry point it is resolved from.
        let d = unsafe {
            Driver {
                cuInit: sym(&lib, "cuInit")?,
                cuDeviceGet: sym(&lib, "cuDeviceGet")?,
                cuDeviceGetCount: sym(&lib, "cuDeviceGetCount")?,
                cuDevicePrimaryCtxGetState: sym(&lib, "cuDevicePrimaryCtxGetState")?,
                cuCtxCreate_v2: sym(&lib, "cuCtxCreate_v2")?,
                cuCtxDestroy_v2: sym(&lib, "cuCtxDestroy_v2")?,
                cuCtxPushCurrent_v2: sym(&lib, "cuCtxPushCurrent_v2")?,
                cuCtxPopCurrent_v2: sym(&lib, "cuCtxPopCurrent_v2")?,
                cuCtxGetCurrent: sym(&lib, "cuCtxGetCurrent")?,
                cuCtxGetDevice: sym(&lib, "cuCtxGetDevice")?,
                cuMemGetInfo_v2: sym(&lib, "cuMemGetInfo_v2")?,
                cuMemAllocPitch_v2: sym(&lib, "cuMemAllocPitch_v2")?,
                cuMemFree_v2: sym(&lib, "cuMemFree_v2")?,
                cuMemsetD2D8Async: sym(&lib, "cuMemsetD2D8Async")?,
                cuMemcpy2D_v2: sym(&lib, "cuMemcpy2D_v2")?,
                cuMemcpy2DAsync_v2: sym(&lib, "cuMemcpy2DAsync_v2")?,
                cuStreamCreate: sym(&lib, "cuStreamCreate")?,
                cuStreamDestroy_v2: sym(&lib, "cuStreamDestroy_v2")?,
                cuStreamSynchronize: sym(&lib, "cuStreamSynchronize")?,
                cuEventCreate: sym(&lib, "cuEventCreate")?,
                cuEventDestroy_v2: sym(&lib, "cuEventDestroy_v2")?,
                cuEventRecord: sym(&lib, "cuEventRecord")?,
                cuEventQuery: sym(&lib, "cuEventQuery")?,
                cuEventSynchronize: sym(&lib, "cuEventSynchronize")?,
                cuEventElapsedTime: sym(&lib, "cuEventElapsedTime")?,
                cuModuleLoadData: sym(&lib, "cuModuleLoadData")?,
                cuModuleGetFunction: sym(&lib, "cuModuleGetFunction")?,
                cuLaunchKernel: sym(&lib, "cuLaunchKernel")?,
                cuModuleUnload: sym(&lib, "cuModuleUnload")?,
                cuGetErrorName: sym(&lib, "cuGetErrorName")?,
                cuGetErrorString: sym(&lib, "cuGetErrorString")?,
                _lib: lib,
            }
        };
        // SAFETY: cuInit takes a flags word that must be 0; it is the first
        // driver call and may be repeated.
        let rc = unsafe { (d.cuInit)(0) };
        if rc != CUDA_SUCCESS {
            return Err(GpuError::Unavailable(format!(
                "cuInit failed: {} ({rc}): {}; is an NVIDIA GPU present and the driver loaded?",
                d.error_name(rc),
                d.error_string(rc)
            )));
        }
        Ok(d)
    }

    /// The driver's symbolic name for `code` (`CUDA_ERROR_INVALID_VALUE`).
    pub fn error_name(&self, code: i32) -> String {
        let mut p: *const c_char = std::ptr::null();
        // SAFETY: `p` is a valid out-pointer; on success the driver stores a
        // pointer to a static NUL-terminated string.
        let rc = unsafe { (self.cuGetErrorName)(code, &mut p) };
        if rc != CUDA_SUCCESS || p.is_null() {
            return format!("CUresult {code}");
        }
        // SAFETY: the driver returned a static, NUL-terminated string.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    /// The driver's description of `code`.
    pub fn error_string(&self, code: i32) -> String {
        let mut p: *const c_char = std::ptr::null();
        // SAFETY: as in `error_name`.
        let rc = unsafe { (self.cuGetErrorString)(code, &mut p) };
        if rc != CUDA_SUCCESS || p.is_null() {
            return "unknown error".into();
        }
        // SAFETY: the driver returned a static, NUL-terminated string.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    /// Maps a `CUresult` from `call` to `Ok` or [`GpuError::Call`].
    pub(crate) fn check(&self, call: &'static str, rc: CuResult) -> Result<(), GpuError> {
        if rc == CUDA_SUCCESS {
            Ok(())
        } else {
            Err(GpuError::Call {
                call,
                code: rc,
                name: self.error_name(rc),
                description: self.error_string(rc),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_driver_library_is_an_error_value() {
        let e = Driver::open(OsStr::new("libobc-no-such-cuda.so.1"))
            .err()
            .expect("opening a missing library must fail");
        match &e {
            GpuError::Unavailable(m) => {
                assert!(m.contains("libobc-no-such-cuda.so.1"), "{m}");
                assert!(m.contains("NVIDIA driver"), "{m}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_library_without_the_entry_points_is_an_error_value() {
        // libc is always present and exports none of the CUDA symbols.
        let e = Driver::open(OsStr::new("libc.so.6"))
            .err()
            .expect("libc is not a CUDA driver");
        assert!(
            matches!(&e, GpuError::Unavailable(m) if m.contains("has no cuInit")),
            "{e:?}"
        );
    }

    #[test]
    fn memcpy2d_matches_the_documented_layout() {
        // 16 fields: 14 pointer-sized, 2 memory types padded to 8 bytes.
        assert_eq!(std::mem::size_of::<Memcpy2d>(), 128);
        assert_eq!(std::mem::offset_of!(Memcpy2d, src_host), 24);
        assert_eq!(std::mem::offset_of!(Memcpy2d, dst_x_in_bytes), 56);
        assert_eq!(std::mem::offset_of!(Memcpy2d, dst_host), 80);
        assert_eq!(std::mem::offset_of!(Memcpy2d, width_in_bytes), 112);
    }
}
