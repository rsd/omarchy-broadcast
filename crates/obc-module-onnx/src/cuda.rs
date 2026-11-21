//! CUDA frames: the module processes on the CPU, so a CUDA frame is copied
//! to page-locked host memory on the frame's stream, processed there and
//! copied back.
//!
//! The copies go through `obc-gpu`, which opens `libcuda.so.1` the first
//! time it is used: a module that only ever sees host frames (or none)
//! never opens the driver, so it loads and probes on a machine without it.
//! Nothing here creates a CUDA context: the host makes the frames' context
//! current around every call, and the buffers are allocated in it.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::sync::{Mutex, OnceLock};

use obc_backend::{DeviceNv12, HostNv12};
use obc_gpu::{CuStream, PinnedHostBuffer};
use obc_module_sdk::{Error, Frame, log};

/// `CUDA_ERROR_NOT_INITIALIZED`: `cuInit` has not run in this process, so
/// no context can be current or active.
const CUDA_ERROR_NOT_INITIALIZED: i32 = 3;
/// `CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR` and `_MINOR`.
const ATTR_CC_MAJOR: c_int = 75;
const ATTR_CC_MINOR: c_int = 76;
/// The device ONNX Runtime's CUDA and TensorRT providers run on (their
/// default `device_id`, which this module does not change).
pub const ORT_DEVICE: c_int = 0;

/// The driver calls `obc-gpu` does not offer, from the module's own open
/// of `libcuda.so.1`: the current context (for
/// [`crate::engine::CudaContextGuard`]), the device's primary context (its
/// state, and its reset, [`GpuSession`]) and what names the device (for
/// the TensorRT engine cache, [`device_identity`]). The primary context's
/// handle and reset are optional: without them only the reset is skipped.
#[derive(Clone, Copy)]
pub struct Driver {
    init: unsafe extern "C" fn(u32) -> i32,
    ctx_get: unsafe extern "C" fn(*mut *mut c_void) -> i32,
    ctx_set: unsafe extern "C" fn(*mut c_void) -> i32,
    device_get: unsafe extern "C" fn(*mut c_int, c_int) -> i32,
    device_name: unsafe extern "C" fn(*mut c_char, c_int, c_int) -> i32,
    device_attribute: unsafe extern "C" fn(*mut c_int, c_int, c_int) -> i32,
    primary_state: unsafe extern "C" fn(c_int, *mut u32, *mut c_int) -> i32,
    primary: Option<PrimaryFns>,
}

/// The primary-context calls the reset needs, all three or none.
#[derive(Clone, Copy)]
struct PrimaryFns {
    retain: unsafe extern "C" fn(*mut *mut c_void, c_int) -> i32,
    release: unsafe extern "C" fn(c_int) -> i32,
    reset: unsafe extern "C" fn(c_int) -> i32,
}

/// The context current on this thread, as the guard saves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Current {
    /// No context is current (or the driver is not initialised yet, so
    /// none can be).
    None,
    /// This context is.
    Context(*mut c_void),
}

impl Driver {
    /// The context current on this thread; `None` when the driver cannot
    /// say (an error other than "not initialised").
    pub fn current(&self) -> Option<Current> {
        let mut c: *mut c_void = std::ptr::null_mut();
        // SAFETY: a valid out-pointer; the call only reads thread state.
        let rc = unsafe { (self.ctx_get)(&mut c) };
        match rc {
            0 if c.is_null() => Some(Current::None),
            0 => Some(Current::Context(c)),
            CUDA_ERROR_NOT_INITIALIZED => Some(Current::None),
            _ => None,
        }
    }

    /// Makes `saved` the current state of this thread again, if it is not:
    /// the context made current again, or none (`cuCtxSetCurrent(NULL)`).
    ///
    /// # Safety
    /// A saved context is live (it was current on this thread when it was
    /// saved, and its owner keeps it alive across the call).
    pub unsafe fn restore(&self, saved: Current) {
        let now = self.current();
        if now == Some(saved) {
            return;
        }
        let ptr = match saved {
            Current::None => std::ptr::null_mut(),
            Current::Context(c) => c,
        };
        // SAFETY: `ptr` is null or the live context saved on this thread.
        let rc = unsafe { (self.ctx_set)(ptr) };
        // Not initialised: nothing became current since, there is nothing
        // to undo.
        if rc != 0 && rc != CUDA_ERROR_NOT_INITIALIZED {
            log::warn!(
                "ONNX cut-out: the CUDA context could not be made current again (CUresult {rc})"
            );
        }
    }

    /// Device [`ORT_DEVICE`]'s handle, if the driver is initialised and it
    /// exists. Initialises nothing.
    fn ort_device(&self) -> Option<c_int> {
        let mut dev: c_int = 0;
        // SAFETY: a valid out-pointer; fails without side effects when the
        // driver is not initialised.
        (unsafe { (self.device_get)(&mut dev, ORT_DEVICE) } == 0).then_some(dev)
    }

    /// Whether `dev`'s primary context is active. Needs no current context
    /// and activates nothing.
    fn primary_active(&self, dev: c_int) -> Option<bool> {
        let (mut flags, mut active) = (0u32, 0 as c_int);
        // SAFETY: valid out-pointers; a query without side effects.
        let rc = unsafe { (self.primary_state)(dev, &mut flags, &mut active) };
        (rc == 0).then_some(active != 0)
    }

    /// The handle of `dev`'s primary context, which is active: retained
    /// and released again, so its use count is as before. Retaining an
    /// active primary context creates nothing.
    fn primary_handle(&self, p: PrimaryFns, dev: c_int) -> Option<*mut c_void> {
        let mut ctx: *mut c_void = std::ptr::null_mut();
        // SAFETY: a valid out-pointer; the context is active (checked by
        // the caller under the sessions' lock), so this only counts a use.
        if unsafe { (p.retain)(&mut ctx, dev) } != 0 {
            return None;
        }
        // SAFETY: releases the use just retained.
        unsafe { (p.release)(dev) };
        (!ctx.is_null()).then_some(ctx)
    }
}

// SAFETY: plain function pointers into the driver library, kept loaded for
// the life of the process.
unsafe impl Send for Driver {}
unsafe impl Sync for Driver {}

/// The driver's entry points; `None` without the driver. Opening the
/// library initialises nothing (no `cuInit`).
pub fn driver() -> Option<Driver> {
    static FNS: OnceLock<Option<Driver>> = OnceLock::new();
    *FNS.get_or_init(|| {
        // SAFETY: loading the NVIDIA driver library runs its initialisers,
        // which have no preconditions; the library is kept for the process.
        let lib = unsafe { libloading::Library::new("libcuda.so.1") }.ok()?;
        // SAFETY: the declared types are the driver's documented
        // signatures (`CUdevice` is an `int`), under the export names the
        // driver's own header maps the calls to.
        let fns = unsafe {
            Driver {
                init: *lib.get(b"cuInit").ok()?,
                ctx_get: *lib.get(b"cuCtxGetCurrent").ok()?,
                ctx_set: *lib.get(b"cuCtxSetCurrent").ok()?,
                device_get: *lib.get(b"cuDeviceGet").ok()?,
                device_name: *lib.get(b"cuDeviceGetName").ok()?,
                device_attribute: *lib.get(b"cuDeviceGetAttribute").ok()?,
                primary_state: *lib.get(b"cuDevicePrimaryCtxGetState").ok()?,
                primary: (|| {
                    Some(PrimaryFns {
                        retain: *lib.get(b"cuDevicePrimaryCtxRetain").ok()?,
                        release: *lib.get(b"cuDevicePrimaryCtxRelease_v2").ok()?,
                        reset: *lib.get(b"cuDevicePrimaryCtxReset_v2").ok()?,
                    })
                })(),
            }
        };
        std::mem::forget(lib);
        Some(fns)
    })
}

/// The device ONNX Runtime's NVIDIA engines run on: its name and compute
/// capability, for the key of the TensorRT engine cache. Initialises the
/// driver (`cuInit`, which creates no context), so it is asked only when
/// TensorRT is about to be tried, which initialises it anyway. `None`
/// when there is no driver or no such device; only a found device is
/// kept, so a failed `cuInit` is tried again at the next load.
pub fn device_identity() -> Option<(String, (i32, i32))> {
    static ID: OnceLock<(String, (i32, i32))> = OnceLock::new();
    if let Some(id) = ID.get() {
        return Some(id.clone());
    }
    let id = (|| {
        let d = driver()?;
        // SAFETY: no preconditions; creates no context.
        if unsafe { (d.init)(0) } != 0 {
            return None;
        }
        let dev = d.ort_device()?;
        let mut name = [0 as c_char; 256];
        // SAFETY: a buffer of the length given; the driver writes a
        // NUL-terminated name into it.
        if unsafe { (d.device_name)(name.as_mut_ptr(), name.len() as c_int, dev) } != 0 {
            return None;
        }
        // SAFETY: NUL-terminated within the buffer (the driver's contract,
        // and the buffer was zeroed).
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let attr = |a: c_int| {
            let mut v: c_int = 0;
            // SAFETY: a valid out-pointer.
            (unsafe { (d.device_attribute)(&mut v, a, dev) } == 0).then_some(v)
        };
        Some((name, (attr(ATTR_CC_MAJOR)?, attr(ATTR_CC_MINOR)?)))
    })()?;
    Some(ID.get_or_init(|| id).clone())
}

/// The CUDA contexts of the frames of the live chains (and of the chains
/// being loaded), as addresses: the primary context is never reset while
/// one of them is it.
static FRAMES_CONTEXTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// One chain's frames context in [`FRAMES_CONTEXTS`], from before its
/// session is made until after it is gone.
pub struct FramesContext {
    ctx: usize,
}

impl FramesContext {
    /// Records `ctx` (an address; null is not recorded).
    pub fn register(ctx: usize) -> FramesContext {
        if ctx != 0 {
            FRAMES_CONTEXTS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(ctx);
        }
        FramesContext { ctx }
    }
}

impl Drop for FramesContext {
    fn drop(&mut self) {
        if self.ctx == 0 {
            return;
        }
        let mut v = FRAMES_CONTEXTS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = v.iter().position(|&c| c == self.ctx) {
            v.swap_remove(i);
        }
    }
}

thread_local! {
    /// The context current when the host called the module on this
    /// thread, as the outermost [`crate::engine::CudaContextGuard`] saved
    /// it.
    static CALL_CONTEXT: Cell<Option<Current>> = const { Cell::new(None) };
}

/// Marks the span of one module call: the context current at its start
/// is the call's, until the outermost mark is dropped.
pub struct CallContext {
    outermost: bool,
}

impl CallContext {
    /// Records `at_call` as the call's context, unless an outer mark did.
    pub fn enter(at_call: Current) -> CallContext {
        let outermost = CALL_CONTEXT.with(|c| {
            if c.get().is_some() {
                return false;
            }
            c.set(Some(at_call));
            true
        });
        CallContext { outermost }
    }
}

impl Drop for CallContext {
    fn drop(&mut self) {
        if self.outermost {
            CALL_CONTEXT.with(|c| c.set(None));
        }
    }
}

/// Why the primary context `primary` must not be reset, if it must not:
/// it is the context `current` at the call, or the frames context of a
/// `live` chain (a reset would free their memory and streams).
pub fn reset_blocker(
    primary: usize,
    current: Option<usize>,
    live: &[usize],
) -> Option<&'static str> {
    if current == Some(primary) {
        Some("it is the context current at the call")
    } else if live.contains(&primary) {
        Some("a chain's CUDA frames are in it")
    } else {
        None
    }
}

/// The live ONNX Runtime sessions that registered (or tried to register)
/// an NVIDIA provider, in this module.
static GPU_SESSIONS: Mutex<usize> = Mutex::new(0);

/// How many [`GpuSession`]s are alive.
pub fn gpu_sessions() -> usize {
    *GPU_SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// One ONNX Runtime session that registered, or tried to register, an
/// NVIDIA provider: taken before the provider is registered, dropped after
/// the session (or the failed attempt).
///
/// ONNX Runtime's CUDA and TensorRT providers run through the CUDA runtime,
/// whose `cudaSetDevice` activates the device's primary context, and
/// nothing releases it when the sessions go: it would outlive them, a
/// context the daemon did not ask for (which keeps the GPU from being
/// parked). When the last of these is dropped, the primary
/// context of [`ORT_DEVICE`] is reset (`cuDevicePrimaryCtxReset`), which
/// frees what the runtime left in it and makes it inactive. That touches
/// the primary context only: the frames' context (GStreamer's, made with
/// `cuCtxCreate`) and the contexts of other backends (Maxine's
/// run in that same one) are other contexts and are left as they were.
///
/// The count and the reset are under one lock, and a session is counted
/// before it is created, so a reset never runs while another of these
/// sessions is alive or being made. The thread's current context is put
/// back after the reset.
pub struct GpuSession {
    _private: (),
}

impl GpuSession {
    /// Counts one more session.
    pub fn begin() -> GpuSession {
        *GPU_SESSIONS.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        GpuSession { _private: () }
    }
}

impl Drop for GpuSession {
    fn drop(&mut self) {
        let mut n = GPU_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        if *n == 0 {
            // Under the lock: no session can start meanwhile.
            reset_primary_context();
        }
    }
}

/// Resets [`ORT_DEVICE`]'s primary context if it is active, the thread's
/// current context put back afterwards. Without the driver, or before
/// `cuInit` (no CUDA call was made), there is nothing to reset and nothing
/// is initialised.
fn reset_primary_context() {
    let Some(d) = driver() else { return };
    // What is current now may be the primary context itself (a provider's
    // teardown makes it current); the host's is the one current at the call.
    let Some(now) = d.current() else { return };
    let at_call = CALL_CONTEXT.with(Cell::get).unwrap_or(now);
    let Some(dev) = d.ort_device() else { return };
    if d.primary_active(dev) != Some(true) {
        return;
    }
    let Some(p) = d.primary else {
        log::warn!(
            "ONNX cut-out: the CUDA primary context left by ONNX Runtime is not reset: this driver lacks cuDevicePrimaryCtxReset_v2"
        );
        return;
    };
    let Some(primary) = d.primary_handle(p, dev) else {
        log::warn!(
            "ONNX cut-out: the CUDA primary context left by ONNX Runtime is not reset: its handle could not be read"
        );
        return;
    };
    let current = match at_call {
        Current::Context(c) => Some(c as usize),
        Current::None => None,
    };
    let live = FRAMES_CONTEXTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(why) = reset_blocker(primary as usize, current, &live) {
        log::warn!("ONNX cut-out: the CUDA primary context is not reset: {why}");
        return;
    }
    // SAFETY: no session of this module uses the primary context any more
    // (the count is zero under the lock), no chain's frames are in it and
    // it is not current here; the host's contexts are made with
    // `cuCtxCreate`.
    let rc = unsafe { (p.reset)(dev) };
    // What was current before, unless that was the primary context, which
    // is gone: then the call's.
    let back = if now == Current::Context(primary) {
        at_call
    } else {
        now
    };
    // SAFETY: a context current on this thread during the call, other than
    // the primary one; the host keeps it alive.
    unsafe { d.restore(back) };
    if rc == 0 {
        log::debug!("ONNX cut-out: the GPU sessions are gone; the CUDA primary context was reset");
    } else {
        log::warn!(
            "ONNX cut-out: the CUDA primary context left by ONNX Runtime could not be reset (CUresult {rc})"
        );
    }
}

fn gpu_error(what: &str, e: obc_gpu::GpuError) -> Error {
    Error::failed(format!("{what}: {e}"))
}

/// The page-locked host copy of the frames of a CUDA chain.
pub struct Staging {
    buffer: PinnedHostBuffer,
    width: u32,
    height: u32,
}

impl Staging {
    /// A buffer for `width x height` NV12 frames, in the current context.
    pub fn new(width: u32, height: u32) -> Result<Staging, Error> {
        let len = width as usize * (height as usize + height as usize / 2);
        let buffer = PinnedHostBuffer::alloc(len)
            .map_err(|e| gpu_error("the page-locked frame buffer", e))?;
        Ok(Staging {
            buffer,
            width,
            height,
        })
    }

    /// The host frame over the buffer (rows of `width` bytes).
    pub fn host(&mut self) -> Result<HostNv12, Error> {
        self.buffer
            .nv12(self.width, self.height, self.width as usize)
            .map_err(|e| gpu_error("the page-locked frame buffer", e))
    }

    /// Copies `frame` (CUDA NV12) into the buffer and waits for it.
    pub fn download(&mut self, frame: &Frame<'_>) -> Result<HostNv12, Error> {
        let (src, stream) = device_nv12(frame)?;
        let host = self.host()?;
        // SAFETY: the host made the frame's context current around this
        // call; the device frame is valid during it; the host buffer is
        // ours and no transfer into it is pending (the last upload is
        // ordered before this copy on the same stream, and the stream is
        // synchronised before the buffer is read).
        unsafe {
            obc_gpu::download_nv12_async(&src, &host, stream)
                .map_err(|e| gpu_error("copying the frame from the GPU", e))?;
            obc_gpu::stream_synchronize(stream)
                .map_err(|e| gpu_error("waiting for the frame copy", e))?;
        }
        Ok(host)
    }

    /// Copies the buffer into `frame` (CUDA NV12), ordered on its stream.
    /// The copy is not waited for: the next use of the buffer is a
    /// download on the same stream, ordered after it.
    pub fn upload(&mut self, frame: &Frame<'_>) -> Result<(), Error> {
        let (dst, stream) = device_nv12(frame)?;
        let host = self.host()?;
        // SAFETY: as in `download`; the buffer is not written again until
        // the next download has synchronised the stream.
        unsafe {
            obc_gpu::upload_nv12_async(&host, &dst, stream)
                .map_err(|e| gpu_error("copying the frame to the GPU", e))
        }
    }
}

/// The CUDA NV12 frame `frame` describes, and its stream.
fn device_nv12(frame: &Frame<'_>) -> Result<(DeviceNv12, CuStream), Error> {
    let (y, uv) = match (frame.plane(0), frame.plane(1)) {
        (Some(y), Some(uv)) => (y, uv),
        _ => return Err(Error::invalid("an NV12 frame has two planes")),
    };
    let addr = |p: obc_module_sdk::abi::obc_plane| p.ptr_or_fd.wrapping_add(p.offset);
    let f = DeviceNv12 {
        y: addr(y),
        y_pitch: y.pitch as usize,
        uv: addr(uv),
        uv_pitch: uv.pitch as usize,
        width: frame.width(),
        height: frame.height(),
    };
    f.validate().map_err(|e| Error::invalid(e.to_string()))?;
    Ok((f, frame.cuda().stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_primary_context_is_not_reset_when_it_is_current_or_holds_frames() {
        assert_eq!(reset_blocker(7, None, &[]), None);
        assert_eq!(reset_blocker(7, Some(9), &[9, 11]), None);
        assert_eq!(
            reset_blocker(7, Some(7), &[]),
            Some("it is the context current at the call")
        );
        assert_eq!(
            reset_blocker(7, Some(9), &[9, 7]),
            Some("a chain's CUDA frames are in it")
        );
    }

    #[test]
    fn frames_contexts_are_recorded_while_their_token_lives() {
        let count = |c: usize| {
            FRAMES_CONTEXTS
                .lock()
                .unwrap()
                .iter()
                .filter(|&&x| x == c)
                .count()
        };
        // An address no real context has, so other tests do not see it.
        let c = 0x5eed_0001;
        let a = FramesContext::register(c);
        let b = FramesContext::register(c);
        let _null = FramesContext::register(0);
        assert_eq!(count(c), 2);
        drop(a);
        assert_eq!(count(c), 1);
        drop(b);
        assert_eq!(count(c), 0);
        assert_eq!(count(0), 0);
    }
}
