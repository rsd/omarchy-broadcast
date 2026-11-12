//! CUDA driver helpers for the effect path (`docs/adr/0012-video-effects-and-maxine-backend.md`).
//!
//! The driver (`libcuda.so.1`) is opened at run time, once per process
//! ([`driver`]); nothing links against it. When it is missing, every helper
//! returns [`GpuError::Unavailable`] instead of panicking.
//!
//! What is here is exactly what an effect element and a backend need around
//! GStreamer's CUDA context:
//!
//! * [`ContextGuard`]: push a context current on this thread, pop on drop;
//! * [`fill_black_nv12`], [`copy_nv12_async`], [`memset_2d_async`],
//!   [`copy_2d_async`]: stream-ordered fills and device-to-device copies;
//! * [`upload_2d`], [`download_2d`]: synchronous host transfers;
//! * [`upload_2d_async`], [`download_2d_async`], [`upload_nv12_async`],
//!   [`download_nv12_async`]: stream-ordered host transfers, asynchronous
//!   with page-locked memory ([`PinnedHostBuffer`]);
//! * [`DeviceBuffer`] / [`DeviceNv12Buffer`]: owned pitched allocations
//!   with host upload and download, for intermediates and tests;
//! * [`Stream`], [`Event`]: owned streams and timing events;
//! * [`mem_get_info`], [`primary_context_active`], [`current_context`]:
//!   lifecycle checks;
//! * [`OwnedContext`]: a context created with `cuCtxCreate`, as GStreamer's
//!   CUDA library creates one, for tests;
//! * [`Lut8`] and [`PqToSdr`]: the custom kernels, shipped as PTX and
//!   compiled by the driver at load time: a byte lookup table over a pitched
//!   8-bit plane, and the tone map of an HDR (PQ, BT.2020) RGB10A2 image to
//!   SDR RGBA8 ([`tonemap`]);
//! * [`park`], [`unpark`], [`parked`]: the driver's checkpoint API on this
//!   process, so an idle daemon holds no GPU device (module `checkpoint`).
//!   While parked, every helper here returns [`GpuError::Parked`] instead
//!   of blocking in the driver.
//!
//! Unless stated otherwise every call needs a context current on the calling
//! thread (the one the memory and streams belong to). Functions that take
//! raw device addresses are `unsafe`: the driver cannot tell a wrong address
//! inside some other live allocation from a right one.

use std::ffi::c_void;
use std::marker::PhantomData;

use obc_backend::{BackendError, ColourSpec, DeviceNv12, HostNv12};

mod checkpoint;
mod driver;
mod kernel;
pub mod tonemap;

pub use checkpoint::{
    NothingToPark, PARK_LOCK_TIMEOUT, ParkError, ParkOutcome, ProcessState, UnparkOutcome, park,
    parked, parked_refusals, process_state, unpark,
};
use driver::{CUDA_ERROR_NOT_READY, CUDA_SUCCESS, MEMORYTYPE_DEVICE, MEMORYTYPE_HOST, Memcpy2d};
pub use driver::{
    CuContext, CuDevice, CuDevicePtr, CuEvent, CuFunction, CuModule, CuStream, DRIVER_LIBRARY,
    Driver, driver,
};
pub use kernel::{LUT8_BLOCK, LUT8_ENTRY, LUT8_PTX, Lut8};
pub use tonemap::PqToSdr;

/// A CUDA failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GpuError {
    /// The driver library could not be loaded or initialised.
    #[error("{0}")]
    Unavailable(String),
    /// A driver call returned an error.
    #[error("{call} failed: {name} ({code}): {description}")]
    Call {
        /// The entry point, e.g. `cuMemAllocPitch_v2`.
        call: &'static str,
        /// The `CUresult`.
        code: i32,
        /// The driver's name for it, e.g. `CUDA_ERROR_OUT_OF_MEMORY`.
        name: String,
        /// The driver's description.
        description: String,
    },
    /// An argument was rejected before reaching the driver.
    #[error("{0}")]
    InvalidArgument(String),
    /// The process is parked ([`park`]): a driver call would block until it
    /// is unparked, so none was made.
    #[error(
        "the CUDA driver is parked (the process's GPU state is checkpointed); no CUDA call may \
         be made until it is unparked"
    )]
    Parked,
}

impl From<GpuError> for BackendError {
    fn from(e: GpuError) -> Self {
        match e {
            GpuError::Unavailable(m) => BackendError::Unavailable(m),
            GpuError::Call { code, .. } => BackendError::Run {
                code,
                message: e.to_string(),
            },
            GpuError::InvalidArgument(m) => BackendError::InvalidInput(m),
            GpuError::Parked => BackendError::Unavailable(e.to_string()),
        }
    }
}

/// The number of CUDA devices. Needs no current context.
pub fn device_count() -> Result<u32, GpuError> {
    let d = driver()?;
    let mut n = 0i32;
    // SAFETY: `n` is a valid out-pointer.
    d.check("cuDeviceGetCount", unsafe { (d.cuDeviceGetCount)(&mut n) })?;
    Ok(n.max(0) as u32)
}

fn device(ordinal: u32) -> Result<CuDevice, GpuError> {
    let d = driver()?;
    let mut dev: CuDevice = 0;
    // SAFETY: `dev` is a valid out-pointer; a bad ordinal is reported.
    d.check("cuDeviceGet", unsafe {
        (d.cuDeviceGet)(&mut dev, ordinal as i32)
    })?;
    Ok(dev)
}

/// The context current on the calling thread (null when none).
pub fn current_context() -> Result<CuContext, GpuError> {
    let d = driver()?;
    let mut c: CuContext = std::ptr::null_mut();
    // SAFETY: `c` is a valid out-pointer; the call only reads thread state.
    d.check("cuCtxGetCurrent", unsafe { (d.cuCtxGetCurrent)(&mut c) })?;
    Ok(c)
}

/// The device ordinal of the context current on the calling thread.
pub fn current_device() -> Result<CuDevice, GpuError> {
    let d = driver()?;
    let mut dev: CuDevice = 0;
    // SAFETY: `dev` is a valid out-pointer.
    d.check("cuCtxGetDevice", unsafe { (d.cuCtxGetDevice)(&mut dev) })?;
    Ok(dev)
}

/// Whether device `ordinal`'s primary context is active (retained by
/// someone in this process, for example by the CUDA runtime). Needs no
/// current context and activates nothing.
pub fn primary_context_active(ordinal: u32) -> Result<bool, GpuError> {
    let d = driver()?;
    let dev = device(ordinal)?;
    let mut flags = 0u32;
    let mut active = 0i32;
    // SAFETY: both out-pointers are valid; the query has no side effect.
    d.check("cuDevicePrimaryCtxGetState", unsafe {
        (d.cuDevicePrimaryCtxGetState)(dev, &mut flags, &mut active)
    })?;
    Ok(active != 0)
}

/// `(free, total)` device memory in bytes, for the current context's device.
pub fn mem_get_info() -> Result<(usize, usize), GpuError> {
    let d = driver()?;
    let (mut free, mut total) = (0usize, 0usize);
    // SAFETY: both out-pointers are valid; a context must be current, which
    // the driver checks and reports.
    d.check("cuMemGetInfo_v2", unsafe {
        (d.cuMemGetInfo_v2)(&mut free, &mut total)
    })?;
    Ok((free, total))
}

/// Blocks until all work on `stream` (null: the legacy default stream) is
/// done.
///
/// # Safety
/// `stream` must be null or a live stream of the current context.
pub unsafe fn stream_synchronize(stream: CuStream) -> Result<(), GpuError> {
    let d = driver()?;
    // SAFETY: the caller guarantees the stream handle.
    d.check("cuStreamSynchronize", unsafe {
        (d.cuStreamSynchronize)(stream)
    })
}

/// A context made current on this thread by [`ContextGuard::push`] and
/// popped again on drop. Not `Send`: it must be dropped on the thread that
/// pushed it.
#[must_use = "the context is popped when the guard is dropped"]
pub struct ContextGuard {
    ctx: CuContext,
    _not_send: PhantomData<*mut ()>,
}

impl ContextGuard {
    /// Pushes `ctx` onto this thread's context stack (`cuCtxPushCurrent_v2`).
    ///
    /// # Safety
    /// `ctx` must be a live context and stay alive until the guard drops.
    pub unsafe fn push(ctx: CuContext) -> Result<ContextGuard, GpuError> {
        let d = driver()?;
        if ctx.is_null() {
            return Err(GpuError::InvalidArgument(
                "cannot push a null CUDA context".into(),
            ));
        }
        // SAFETY: the caller guarantees `ctx` is live.
        d.check("cuCtxPushCurrent_v2", unsafe {
            (d.cuCtxPushCurrent_v2)(ctx)
        })?;
        Ok(ContextGuard {
            ctx,
            _not_send: PhantomData,
        })
    }

    /// The pushed context.
    pub fn context(&self) -> CuContext {
        self.ctx
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            let mut popped: CuContext = std::ptr::null_mut();
            // SAFETY: this guard pushed `self.ctx` on this thread (it is not
            // Send), so the stack is non-empty; `popped` is a valid out-pointer.
            let rc = unsafe { (d.cuCtxPopCurrent_v2)(&mut popped) };
            debug_assert!(
                rc == CUDA_SUCCESS && popped == self.ctx,
                "context stack out of order: popped {popped:?}, expected {:?} (rc {rc})",
                self.ctx
            );
        }
    }
}

/// A context created with `cuCtxCreate_v2`, the way GStreamer's CUDA library
/// creates its own (not the primary context), and destroyed on drop.
///
/// Creation leaves the calling thread's current context unchanged; push it
/// with [`ContextGuard::push`].
pub struct OwnedContext {
    ctx: CuContext,
}

// SAFETY: a CUDA context is not bound to the thread that created it; it may
// be pushed on and destroyed from any thread.
unsafe impl Send for OwnedContext {}

impl OwnedContext {
    /// Creates a context on device `ordinal` (flags 0: automatic scheduling).
    pub fn create(ordinal: u32) -> Result<OwnedContext, GpuError> {
        let d = driver()?;
        let dev = device(ordinal)?;
        let mut ctx: CuContext = std::ptr::null_mut();
        // SAFETY: `ctx` is a valid out-pointer; on success the new context is
        // pushed current on this thread.
        d.check("cuCtxCreate_v2", unsafe {
            (d.cuCtxCreate_v2)(&mut ctx, 0, dev)
        })?;
        let mut popped: CuContext = std::ptr::null_mut();
        // SAFETY: cuCtxCreate pushed `ctx` on this thread; pop it so the
        // thread's context stack is as before.
        let rc = unsafe { (d.cuCtxPopCurrent_v2)(&mut popped) };
        let owned = OwnedContext { ctx };
        d.check("cuCtxPopCurrent_v2", rc)?;
        Ok(owned)
    }

    /// The raw handle, for [`ContextGuard::push`] and [`obc_backend::GpuContext`].
    pub fn raw(&self) -> CuContext {
        self.ctx
    }
}

impl Drop for OwnedContext {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            // SAFETY: `self.ctx` was created by us and is destroyed once.
            unsafe { (d.cuCtxDestroy_v2)(self.ctx) };
        }
    }
}

/// An owned CUDA stream (`cuStreamCreate`), destroyed on drop.
pub struct Stream {
    raw: CuStream,
}

// SAFETY: CUDA streams may be used and destroyed from any thread that has
// their context current.
unsafe impl Send for Stream {}

impl Stream {
    /// Creates a stream in the current context. `non_blocking` streams do not
    /// synchronise with the legacy default stream (`CU_STREAM_NON_BLOCKING`).
    pub fn new(non_blocking: bool) -> Result<Stream, GpuError> {
        let d = driver()?;
        let mut raw: CuStream = std::ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer; flags are 0 or 1.
        d.check("cuStreamCreate", unsafe {
            (d.cuStreamCreate)(&mut raw, u32::from(non_blocking))
        })?;
        Ok(Stream { raw })
    }

    /// The raw handle.
    pub fn raw(&self) -> CuStream {
        self.raw
    }

    /// Blocks until all work on the stream is done.
    pub fn synchronize(&self) -> Result<(), GpuError> {
        // SAFETY: our own live stream.
        unsafe { stream_synchronize(self.raw) }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            // SAFETY: `self.raw` was created by us and is destroyed once; the
            // driver defers the release until queued work completes.
            unsafe { (d.cuStreamDestroy_v2)(self.raw) };
        }
    }
}

/// An owned CUDA event with timing enabled, destroyed on drop.
pub struct Event {
    raw: CuEvent,
}

// SAFETY: CUDA events may be used and destroyed from any thread that has
// their context current.
unsafe impl Send for Event {}

impl Event {
    /// Creates an event in the current context (default flags: timing on).
    pub fn new() -> Result<Event, GpuError> {
        let d = driver()?;
        let mut raw: CuEvent = std::ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer.
        d.check("cuEventCreate", unsafe { (d.cuEventCreate)(&mut raw, 0) })?;
        Ok(Event { raw })
    }

    /// Records the event on `stream`.
    ///
    /// # Safety
    /// `stream` must be null or a live stream of the event's context.
    pub unsafe fn record(&self, stream: CuStream) -> Result<(), GpuError> {
        let d = driver()?;
        // SAFETY: `self.raw` is live; the caller guarantees the stream.
        d.check("cuEventRecord", unsafe {
            (d.cuEventRecord)(self.raw, stream)
        })
    }

    /// `true` once the work before the last record has completed.
    pub fn is_complete(&self) -> Result<bool, GpuError> {
        let d = driver()?;
        // SAFETY: `self.raw` is live.
        let rc = unsafe { (d.cuEventQuery)(self.raw) };
        if rc == CUDA_ERROR_NOT_READY {
            return Ok(false);
        }
        d.check("cuEventQuery", rc).map(|()| true)
    }

    /// Blocks until the event has completed.
    pub fn synchronize(&self) -> Result<(), GpuError> {
        let d = driver()?;
        // SAFETY: `self.raw` is live.
        d.check("cuEventSynchronize", unsafe {
            (d.cuEventSynchronize)(self.raw)
        })
    }

    /// GPU milliseconds from `start` to `end`; both recorded and complete.
    pub fn elapsed_ms(start: &Event, end: &Event) -> Result<f32, GpuError> {
        let d = driver()?;
        let mut ms = 0f32;
        // SAFETY: both events are live; `ms` is a valid out-pointer.
        d.check("cuEventElapsedTime", unsafe {
            (d.cuEventElapsedTime)(&mut ms, start.raw, end.raw)
        })?;
        Ok(ms)
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            // SAFETY: `self.raw` was created by us and is destroyed once.
            unsafe { (d.cuEventDestroy_v2)(self.raw) };
        }
    }
}

/// Fills `height` rows of `width` bytes at `dst` (rows `pitch` apart) with
/// `value`, on `stream` (`cuMemsetD2D8Async`).
///
/// # Safety
/// `dst .. dst + pitch * (height - 1) + width` must lie inside one live
/// device allocation of the current context, not in use by other work that
/// would race with the fill.
pub unsafe fn memset_2d_async(
    dst: CuDevicePtr,
    pitch: usize,
    value: u8,
    width: usize,
    height: usize,
    stream: CuStream,
) -> Result<(), GpuError> {
    let d = driver()?;
    // SAFETY: the caller guarantees the destination rectangle.
    d.check("cuMemsetD2D8Async", unsafe {
        (d.cuMemsetD2D8Async)(dst, pitch, value, width, height, stream)
    })
}

/// Fills an NV12 frame with black for `colour` (Y 16 / UV 128 limited,
/// Y 0 / UV 128 full), on `stream`.
///
/// # Safety
/// `frame` must describe live device memory of the current context, as in
/// [`memset_2d_async`] for both planes.
pub unsafe fn fill_black_nv12(
    frame: &DeviceNv12,
    colour: ColourSpec,
    stream: CuStream,
) -> Result<(), GpuError> {
    frame
        .validate()
        .map_err(|e| GpuError::InvalidArgument(e.to_string()))?;
    let (y, uv) = colour.black_nv12();
    let (w, h) = (frame.width as usize, frame.height as usize);
    // SAFETY: the caller guarantees both planes; the rectangles are those
    // `frame` describes.
    unsafe {
        memset_2d_async(frame.y, frame.y_pitch, y, w, h, stream)?;
        memset_2d_async(frame.uv, frame.uv_pitch, uv, w, h / 2, stream)
    }
}

/// Copies `height` rows of `width_bytes` bytes from device memory to device
/// memory on `stream` (`cuMemcpy2DAsync_v2`).
///
/// # Safety
/// Both rectangles must lie inside live device allocations of the current
/// context; the destination must not overlap the source.
pub unsafe fn copy_2d_async(
    src: CuDevicePtr,
    src_pitch: usize,
    dst: CuDevicePtr,
    dst_pitch: usize,
    width_bytes: usize,
    height: usize,
    stream: CuStream,
) -> Result<(), GpuError> {
    let d = driver()?;
    let p = Memcpy2d {
        src_memory_type: MEMORYTYPE_DEVICE,
        src_device: src,
        src_pitch,
        dst_memory_type: MEMORYTYPE_DEVICE,
        dst_device: dst,
        dst_pitch,
        width_in_bytes: width_bytes,
        height,
        ..Memcpy2d::zeroed()
    };
    // SAFETY: `p` is a valid descriptor that outlives the call (the driver
    // copies it); the caller guarantees both rectangles.
    d.check("cuMemcpy2DAsync_v2", unsafe {
        (d.cuMemcpy2DAsync_v2)(&p, stream)
    })
}

/// Copies both planes of an NV12 frame from `src` to `dst` (same size) on
/// `stream`.
///
/// # Safety
/// As [`copy_2d_async`], for both planes of both frames.
pub unsafe fn copy_nv12_async(
    src: &DeviceNv12,
    dst: &DeviceNv12,
    stream: CuStream,
) -> Result<(), GpuError> {
    src.validate()
        .map_err(|e| GpuError::InvalidArgument(e.to_string()))?;
    dst.validate()
        .map_err(|e| GpuError::InvalidArgument(e.to_string()))?;
    if (src.width, src.height) != (dst.width, dst.height) {
        return Err(GpuError::InvalidArgument(format!(
            "NV12 copy from {}x{} to {}x{}",
            src.width, src.height, dst.width, dst.height
        )));
    }
    let (w, h) = (src.width as usize, src.height as usize);
    // SAFETY: the caller guarantees both frames; the rectangles are theirs.
    unsafe {
        copy_2d_async(src.y, src.y_pitch, dst.y, dst.y_pitch, w, h, stream)?;
        copy_2d_async(src.uv, src.uv_pitch, dst.uv, dst.uv_pitch, w, h / 2, stream)
    }
}

/// Copies `height` rows of `width_bytes` bytes from `host` (rows
/// `host_pitch` apart) to device memory at `dst` (rows `dst_pitch` apart),
/// synchronously (`cuMemcpy2D_v2`, which orders with the legacy default
/// stream: synchronise non-blocking streams that use `dst` first).
///
/// # Safety
/// The destination rectangle must lie inside a live device allocation of
/// the current context. The host side is bounds-checked.
pub unsafe fn upload_2d(
    host: &[u8],
    host_pitch: usize,
    dst: CuDevicePtr,
    dst_pitch: usize,
    width_bytes: usize,
    height: usize,
) -> Result<(), GpuError> {
    check_host_rect(host.len(), host_pitch, width_bytes, height)?;
    let d = driver()?;
    let p = Memcpy2d {
        src_memory_type: MEMORYTYPE_HOST,
        src_host: host.as_ptr().cast::<c_void>(),
        src_pitch: host_pitch,
        dst_memory_type: MEMORYTYPE_DEVICE,
        dst_device: dst,
        dst_pitch,
        width_in_bytes: width_bytes,
        height,
        ..Memcpy2d::zeroed()
    };
    // SAFETY: the host slice holds the source rectangle (checked above) and
    // outlives the synchronous copy; the caller guarantees the destination.
    d.check("cuMemcpy2D_v2", unsafe { (d.cuMemcpy2D_v2)(&p) })
}

/// Copies `height` rows of `width_bytes` bytes from device memory at `src`
/// (rows `src_pitch` apart) into `host` (rows `host_pitch` apart),
/// synchronously; see [`upload_2d`] for ordering.
///
/// # Safety
/// The source rectangle must lie inside a live device allocation of the
/// current context. The host side is bounds-checked.
pub unsafe fn download_2d(
    src: CuDevicePtr,
    src_pitch: usize,
    host: &mut [u8],
    host_pitch: usize,
    width_bytes: usize,
    height: usize,
) -> Result<(), GpuError> {
    check_host_rect(host.len(), host_pitch, width_bytes, height)?;
    let d = driver()?;
    let p = Memcpy2d {
        src_memory_type: MEMORYTYPE_DEVICE,
        src_device: src,
        src_pitch,
        dst_memory_type: MEMORYTYPE_HOST,
        dst_host: host.as_mut_ptr().cast::<c_void>(),
        dst_pitch: host_pitch,
        width_in_bytes: width_bytes,
        height,
        ..Memcpy2d::zeroed()
    };
    // SAFETY: the host slice is large enough for the destination rectangle
    // (checked above) and exclusively borrowed for the synchronous copy; the
    // caller guarantees the source.
    d.check("cuMemcpy2D_v2", unsafe { (d.cuMemcpy2D_v2)(&p) })
}

/// Copies `height` rows of `width_bytes` bytes from device memory at `src`
/// (rows `src_pitch` apart) to host memory at `host` (rows `host_pitch`
/// apart), ordered on `stream` (`cuMemcpy2DAsync_v2`). The copy is
/// asynchronous only for page-locked host memory ([`PinnedHostBuffer`]);
/// either way the host bytes are complete only once `stream` has been
/// synchronised.
///
/// # Safety
/// The source rectangle must lie inside a live device allocation of the
/// current context, and `host .. host + host_pitch * (height - 1) +
/// width_bytes` must be valid for writes and not accessed otherwise until
/// the stream is synchronised.
pub unsafe fn download_2d_async(
    src: CuDevicePtr,
    src_pitch: usize,
    host: *mut u8,
    host_pitch: usize,
    width_bytes: usize,
    height: usize,
    stream: CuStream,
) -> Result<(), GpuError> {
    if host.is_null() || host_pitch < width_bytes {
        return Err(GpuError::InvalidArgument(format!(
            "host rows of {host_pitch} bytes cannot take {width_bytes} bytes (or a null pointer)"
        )));
    }
    let d = driver()?;
    let p = Memcpy2d {
        src_memory_type: MEMORYTYPE_DEVICE,
        src_device: src,
        src_pitch,
        dst_memory_type: MEMORYTYPE_HOST,
        dst_host: host.cast::<c_void>(),
        dst_pitch: host_pitch,
        width_in_bytes: width_bytes,
        height,
        ..Memcpy2d::zeroed()
    };
    // SAFETY: `p` outlives the call (the driver copies it); the caller
    // guarantees both rectangles until the stream is synchronised.
    d.check("cuMemcpy2DAsync_v2", unsafe {
        (d.cuMemcpy2DAsync_v2)(&p, stream)
    })
}

/// Copies `height` rows of `width_bytes` bytes from host memory at `host`
/// (rows `host_pitch` apart) to device memory at `dst` (rows `dst_pitch`
/// apart), ordered on `stream`; see [`download_2d_async`].
///
/// # Safety
/// The destination rectangle must lie inside a live device allocation of
/// the current context, and the host rectangle must be valid for reads and
/// not written until the stream is synchronised.
pub unsafe fn upload_2d_async(
    host: *const u8,
    host_pitch: usize,
    dst: CuDevicePtr,
    dst_pitch: usize,
    width_bytes: usize,
    height: usize,
    stream: CuStream,
) -> Result<(), GpuError> {
    if host.is_null() || host_pitch < width_bytes {
        return Err(GpuError::InvalidArgument(format!(
            "host rows of {host_pitch} bytes cannot give {width_bytes} bytes (or a null pointer)"
        )));
    }
    let d = driver()?;
    let p = Memcpy2d {
        src_memory_type: MEMORYTYPE_HOST,
        src_host: host.cast::<c_void>(),
        src_pitch: host_pitch,
        dst_memory_type: MEMORYTYPE_DEVICE,
        dst_device: dst,
        dst_pitch,
        width_in_bytes: width_bytes,
        height,
        ..Memcpy2d::zeroed()
    };
    // SAFETY: as in `download_2d_async`.
    d.check("cuMemcpy2DAsync_v2", unsafe {
        (d.cuMemcpy2DAsync_v2)(&p, stream)
    })
}

/// Checks that a device and a host NV12 frame have the same size and are
/// self-consistent.
fn check_nv12_pair(device: &DeviceNv12, host: &HostNv12) -> Result<(), GpuError> {
    device
        .validate()
        .map_err(|e| GpuError::InvalidArgument(e.to_string()))?;
    host.validate()
        .map_err(|e| GpuError::InvalidArgument(e.to_string()))?;
    if (device.width, device.height) != (host.width, host.height) {
        return Err(GpuError::InvalidArgument(format!(
            "NV12 transfer between {}x{} and {}x{}",
            device.width, device.height, host.width, host.height
        )));
    }
    Ok(())
}

/// Copies both planes of the NV12 frame `src` (device) into `dst` (host,
/// same size) on `stream`; see [`download_2d_async`].
///
/// # Safety
/// As [`download_2d_async`], for both planes of both frames.
pub unsafe fn download_nv12_async(
    src: &DeviceNv12,
    dst: &HostNv12,
    stream: CuStream,
) -> Result<(), GpuError> {
    check_nv12_pair(src, dst)?;
    let (w, h) = (src.width as usize, src.height as usize);
    // SAFETY: the caller guarantees both frames; the rectangles are theirs.
    unsafe {
        download_2d_async(src.y, src.y_pitch, dst.y, dst.y_pitch, w, h, stream)?;
        download_2d_async(src.uv, src.uv_pitch, dst.uv, dst.uv_pitch, w, h / 2, stream)
    }
}

/// Copies both planes of the NV12 frame `src` (host) into `dst` (device,
/// same size) on `stream`; see [`upload_2d_async`].
///
/// # Safety
/// As [`upload_2d_async`], for both planes of both frames.
pub unsafe fn upload_nv12_async(
    src: &HostNv12,
    dst: &DeviceNv12,
    stream: CuStream,
) -> Result<(), GpuError> {
    check_nv12_pair(dst, src)?;
    let (w, h) = (src.width as usize, src.height as usize);
    // SAFETY: the caller guarantees both frames; the rectangles are theirs.
    unsafe {
        upload_2d_async(src.y, src.y_pitch, dst.y, dst.y_pitch, w, h, stream)?;
        upload_2d_async(src.uv, src.uv_pitch, dst.uv, dst.uv_pitch, w, h / 2, stream)
    }
}

/// Page-locked host memory (`cuMemAllocHost_v2`), which the GPU reads and
/// writes directly, so stream-ordered transfers with it are truly
/// asynchronous. Allocated in, and freed (on drop) with, the context that
/// is current: drop it with that context current.
pub struct PinnedHostBuffer {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: page-locked memory is plain host memory, not bound to a thread;
// the buffer owns it exclusively.
unsafe impl Send for PinnedHostBuffer {}

impl PinnedHostBuffer {
    /// Allocates `len` bytes (non-zero) in the current context. The content
    /// is undefined until written.
    pub fn alloc(len: usize) -> Result<PinnedHostBuffer, GpuError> {
        if len == 0 {
            return Err(GpuError::InvalidArgument(
                "empty page-locked host allocation".into(),
            ));
        }
        let d = driver()?;
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: `p` is a valid out-pointer.
        d.check("cuMemAllocHost_v2", unsafe {
            (d.cuMemAllocHost_v2)(&mut p, len)
        })?;
        Ok(PinnedHostBuffer { ptr: p.cast(), len })
    }

    /// The address of the first byte.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    /// The size in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false: an allocation is never empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes. Read them only while no transfer into the buffer is
    /// pending on a stream.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is our live allocation of `len` bytes.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// The bytes, writable. Write them only while no transfer from or into
    /// the buffer is pending on a stream.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is our live allocation of `len` bytes, exclusively
        // borrowed.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// A `width x height` NV12 frame over the buffer, the Y rows then the
    /// UV rows, `pitch` bytes apart. Fails if the buffer is too small.
    pub fn nv12(&mut self, width: u32, height: u32, pitch: usize) -> Result<HostNv12, GpuError> {
        let rows = height as usize + height as usize / 2;
        if pitch < width as usize || pitch * rows > self.len {
            return Err(GpuError::InvalidArgument(format!(
                "a page-locked buffer of {} bytes cannot hold a {width}x{height} NV12 frame \
                 with rows of {pitch} bytes",
                self.len
            )));
        }
        let f = HostNv12 {
            y: self.ptr,
            y_pitch: pitch,
            // SAFETY: inside the allocation (checked above).
            uv: unsafe { self.ptr.add(pitch * height as usize) },
            uv_pitch: pitch,
            width,
            height,
        };
        f.validate()
            .map_err(|e| GpuError::InvalidArgument(e.to_string()))?;
        Ok(f)
    }
}

impl Drop for PinnedHostBuffer {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            // SAFETY: allocated by us with cuMemAllocHost and freed once.
            unsafe { (d.cuMemFreeHost)(self.ptr.cast()) };
        }
    }
}

fn check_host_rect(
    len: usize,
    host_pitch: usize,
    width_bytes: usize,
    height: usize,
) -> Result<(), GpuError> {
    let fits = host_pitch >= width_bytes
        && (height == 0 || len >= host_pitch * (height - 1) + width_bytes);
    if !fits {
        return Err(GpuError::InvalidArgument(format!(
            "host buffer of {len} bytes, pitch {host_pitch}, cannot hold {height} rows of {width_bytes} bytes"
        )));
    }
    Ok(())
}

/// An owned pitched device allocation (`cuMemAllocPitch_v2`) of `height`
/// rows of at least `width_bytes` bytes, freed on drop (with the allocating
/// context current, or at least alive).
pub struct DeviceBuffer {
    ptr: CuDevicePtr,
    pitch: usize,
    width_bytes: usize,
    height: usize,
}

// SAFETY: a device allocation is not bound to a thread.
unsafe impl Send for DeviceBuffer {}

impl DeviceBuffer {
    /// Allocates in the current context. The pitch is the driver's choice
    /// for 4-byte accesses.
    pub fn alloc_pitch(width_bytes: usize, height: usize) -> Result<DeviceBuffer, GpuError> {
        if width_bytes == 0 || height == 0 {
            return Err(GpuError::InvalidArgument(format!(
                "empty device allocation {width_bytes}x{height}"
            )));
        }
        let d = driver()?;
        let (mut ptr, mut pitch) = (0u64, 0usize);
        // SAFETY: both out-pointers are valid; element size 4 is allowed.
        d.check("cuMemAllocPitch_v2", unsafe {
            (d.cuMemAllocPitch_v2)(&mut ptr, &mut pitch, width_bytes, height, 4)
        })?;
        Ok(DeviceBuffer {
            ptr,
            pitch,
            width_bytes,
            height,
        })
    }

    /// The device address of row 0.
    pub fn ptr(&self) -> CuDevicePtr {
        self.ptr
    }

    /// Bytes between two rows.
    pub fn pitch(&self) -> usize {
        self.pitch
    }

    /// The requested row width in bytes.
    pub fn width_bytes(&self) -> usize {
        self.width_bytes
    }

    /// The number of rows.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Copies `height` rows of `width_bytes` from `host` (rows `host_pitch`
    /// apart) into the buffer, synchronously; see [`upload_2d`].
    pub fn upload(&self, host: &[u8], host_pitch: usize) -> Result<(), GpuError> {
        // SAFETY: the destination is our own allocation of `height` rows of
        // `pitch >= width_bytes` bytes.
        unsafe {
            upload_2d(
                host,
                host_pitch,
                self.ptr,
                self.pitch,
                self.width_bytes,
                self.height,
            )
        }
    }

    /// Copies the buffer into `host` (rows `host_pitch` apart),
    /// synchronously; see [`upload_2d`] for ordering.
    pub fn download(&self, host: &mut [u8], host_pitch: usize) -> Result<(), GpuError> {
        // SAFETY: the source is our own allocation, as in `upload`.
        unsafe {
            download_2d(
                self.ptr,
                self.pitch,
                host,
                host_pitch,
                self.width_bytes,
                self.height,
            )
        }
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            // SAFETY: `self.ptr` was allocated by us and is freed once.
            unsafe { (d.cuMemFree_v2)(self.ptr) };
        }
    }
}

/// An owned NV12 frame in device memory: one pitched allocation, the UV
/// plane right after the Y plane (`height` rows), as GStreamer lays out
/// NV12 CUDA memory.
pub struct DeviceNv12Buffer {
    buf: DeviceBuffer,
    width: u32,
    height: u32,
}

impl DeviceNv12Buffer {
    /// Allocates a `width x height` NV12 frame (both even) in the current
    /// context. The content is undefined until written.
    pub fn alloc(width: u32, height: u32) -> Result<DeviceNv12Buffer, GpuError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(GpuError::InvalidArgument(format!(
                "NV12 size {width}x{height} must be even and non-zero"
            )));
        }
        let rows = height as usize * 3 / 2;
        Ok(DeviceNv12Buffer {
            buf: DeviceBuffer::alloc_pitch(width as usize, rows)?,
            width,
            height,
        })
    }

    /// The frame description (borrowing this buffer's memory).
    pub fn frame(&self) -> DeviceNv12 {
        let pitch = self.buf.pitch();
        DeviceNv12 {
            y: self.buf.ptr(),
            y_pitch: pitch,
            uv: self.buf.ptr() + (pitch * self.height as usize) as u64,
            uv_pitch: pitch,
            width: self.width,
            height: self.height,
        }
    }

    /// Uploads a tightly packed host NV12 frame (`width * height * 3 / 2`
    /// bytes: Y rows then UV rows, `width` bytes each), synchronously.
    pub fn upload_packed(&self, nv12: &[u8]) -> Result<(), GpuError> {
        self.buf.upload(nv12, self.width as usize)
    }

    /// Downloads into a tightly packed host NV12 frame, synchronously.
    pub fn download_packed(&self) -> Result<Vec<u8>, GpuError> {
        let mut out = vec![0u8; self.width as usize * self.height as usize * 3 / 2];
        self.buf.download(&mut out, self.width as usize)?;
        Ok(out)
    }

    /// Fills the frame with black on `stream`.
    ///
    /// # Safety
    /// `stream` must be null or a live stream of the buffer's context.
    pub unsafe fn fill_black(&self, colour: ColourSpec, stream: CuStream) -> Result<(), GpuError> {
        // SAFETY: `frame()` describes this buffer's own live allocation; the
        // caller guarantees the stream.
        unsafe { fill_black_nv12(&self.frame(), colour, stream) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_rectangles_are_bounds_checked() {
        assert!(check_host_rect(24, 12, 12, 2).is_ok());
        assert!(check_host_rect(28, 16, 12, 2).is_ok());
        assert!(check_host_rect(27, 16, 12, 2).is_err());
        assert!(check_host_rect(100, 8, 12, 2).is_err());
        assert!(check_host_rect(0, 0, 0, 0).is_ok());
    }

    #[test]
    fn gpu_errors_map_to_backend_errors() {
        let e = GpuError::Call {
            call: "cuMemAllocPitch_v2",
            code: 2,
            name: "CUDA_ERROR_OUT_OF_MEMORY".into(),
            description: "out of memory".into(),
        };
        match BackendError::from(e) {
            BackendError::Run { code, message } => {
                assert_eq!(code, 2);
                assert!(message.contains("cuMemAllocPitch_v2 failed"), "{message}");
                assert!(message.contains("CUDA_ERROR_OUT_OF_MEMORY"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            BackendError::from(GpuError::Unavailable("x".into())),
            BackendError::Unavailable(_)
        ));
    }
}
