//! The implementation of `obceffect`.
//!
//! # Threads and locks
//!
//! * The **streaming thread** decides per frame what it gets
//!   ([`decide`]) in `submit_input_buffer`, switching BaseTransform's
//!   passthrough to match, then allocates and fills the output.
//! * The **worker thread** (one per started element) loads, switches and
//!   unloads the backend, and decodes the background image. It works from
//!   the latest request only: requests are numbered (`Control::generation`)
//!   and a superseded one is dropped.
//! * `Control` (settings, status, the CUDA handles once known) and `Engine`
//!   (the backend instance and what it has loaded) have one mutex each and
//!   no code path holds both. The streaming thread locks `Engine` only to
//!   process a frame, and only when `Control::loaded` says something is
//!   loaded, so a cold load on the worker never blocks it; a switch between
//!   two effects blocks it for the switch itself (milliseconds).
//!
//! # CUDA context and stream
//!
//! The context is the one the first input memory belongs to (the stream's
//! shared `GstCudaContext`); every later input must be in it. The element
//! creates one `GstCudaStream` in it and runs everything there: the
//! backend's `load` and `process`, the black fills and the copies. It does
//! not adopt the input's stream: the backend binds the stream at load time,
//! and the input's stream changes with every ingest pipeline (each camera
//! switch) and differs for the output's placeholder frame, so sharing it
//! would force a full reload on each switch. Instead:
//!
//! * the input is synchronised with `gst_cuda_memory_sync` before it is read
//!   (a no-op unless its producer left work pending on another stream);
//! * the output pool carries the element's stream, so the output memory is
//!   marked as written on it (downstream CUDA elements order against it);
//! * the element's stream is synchronised before `transform` returns: the
//!   input buffer may go back to its ingest pool as soon as it is released,
//!   and its next write must not race this frame's read. Downstream
//!   (`cudadownload`, on the same thread) would wait for that work anyway.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::thread::JoinHandle;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::base_transform::{InputBuffer, PrepareOutputBufferSuccess};
use gst_base::subclass::prelude::*;
use gst_video::VideoInfo;
use obc_backend::{
    BackendFactory, ColourMatrix, ColourSpec, DeviceNv12, EffectKind, FrameParams, GpuContext,
    InferenceBackend, LoadReport, LoadRequest,
};
use obc_gpu::{ContextGuard, CuContext, CuStream, Event};

use super::STATUS_MESSAGE;
use crate::background;
use crate::cuda::{self, CudaMemoryHead, MAP_CUDA};
use crate::settings::{
    EffectFallback, EffectSettings, EffectStats, EffectStatus, FrameAction, Target, decide,
    parse_effect, percentile,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "obceffect",
        gst::DebugColorFlags::empty(),
        Some("omarchy-broadcast effect element"),
    )
});

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The negotiated frame format, as the backend sees it.
#[derive(Debug, Clone, PartialEq)]
struct Format {
    width: u32,
    height: u32,
    /// The colour the backend converts with, or why it cannot represent
    /// the caps' colorimetry (only an effect fails on that; `none` works).
    colour: Result<ColourSpec, String>,
    /// Black for this range (the matrix does not matter for black).
    black: ColourSpec,
}

impl Format {
    fn of(info: &VideoInfo) -> Format {
        let c = info.colorimetry();
        let full = c.range() == gst_video::VideoColorRange::Range0_255;
        let black = if full {
            ColourSpec::FULL_BT601
        } else {
            ColourSpec::LIMITED_BT601
        };
        Format {
            width: info.width(),
            height: info.height(),
            colour: colour_of(&c),
            black,
        }
    }
}

/// The backend's colour description of a GStreamer colorimetry: BT.601 or
/// BT.709 matrix, full or limited range. Anything else is refused.
fn colour_of(c: &gst_video::VideoColorimetry) -> Result<ColourSpec, String> {
    let full_range = match c.range() {
        gst_video::VideoColorRange::Range0_255 => true,
        gst_video::VideoColorRange::Range16_235 => false,
        other => {
            return Err(format!(
                "the effects cannot process colour range {other:?} (colorimetry {c}); they \
                 need full or limited range"
            ));
        }
    };
    let matrix = match c.matrix() {
        gst_video::VideoColorMatrix::Bt601 => ColourMatrix::Bt601,
        gst_video::VideoColorMatrix::Bt709 => ColourMatrix::Bt709,
        other => {
            return Err(format!(
                "the effects cannot process colour matrix {other:?} (colorimetry {c}); they \
                 need BT.601 or BT.709"
            ));
        }
    };
    Ok(ColourSpec { matrix, full_range })
}

/// What the backend instance has loaded.
#[derive(Debug, Clone, PartialEq)]
struct LoadedKey {
    target: Target,
    width: u32,
    height: u32,
    colour: ColourSpec,
}

#[derive(Default)]
struct Engine {
    instance: Option<Box<dyn InferenceBackend>>,
    loaded: Option<LoadedKey>,
}

struct Control {
    settings: EffectSettings,
    /// What the settings need loaded, and the number of the request for it.
    target: Target,
    generation: u64,
    status: EffectStatus,
    /// `Engine::loaded.is_some()`, as last published by the worker (set to
    /// false before an unload or a full reload starts).
    loaded: bool,
    /// The CUDA context and the element's stream, once the streaming thread
    /// has created them.
    gpu: Option<GpuContext>,
    format: Option<Format>,
    factory: Option<Arc<dyn BackendFactory>>,
    stop: bool,
    last_load_ms: Option<f64>,
    last_load_reused: bool,
    loads: u64,
}

impl Default for Control {
    fn default() -> Self {
        Control {
            settings: EffectSettings::default(),
            target: Target::default(),
            generation: 0,
            status: EffectStatus::Off,
            loaded: false,
            gpu: None,
            format: None,
            factory: None,
            stop: false,
            last_load_ms: None,
            last_load_reused: false,
            loads: 0,
        }
    }
}

#[derive(Default)]
struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    engine: Mutex<Engine>,
    processed: AtomicU64,
    black: AtomicU64,
    passthrough: AtomicU64,
    cuda_active: AtomicBool,
    gpu_ms: Mutex<VecDeque<f32>>,
}

/// The element's CUDA objects, created on the streaming thread when an
/// effect first needs them and released when dropped: in `stop`, after the
/// backend is unloaded, or with the element if it is finalised without
/// stopping.
struct GpuResources {
    /// A reference to the input's `GstCudaContext`.
    context: *mut c_void,
    cu_ctx: CuContext,
    /// The element's own `GstCudaStream`.
    stream: *mut c_void,
    cu_stream: CuStream,
    /// The output pool for the negotiated caps (rebuilt when they change).
    pool: Option<gst::BufferPool>,
    /// Destroyed in `drop`, with the context current.
    start: ManuallyDrop<Event>,
    end: ManuallyDrop<Event>,
}

// SAFETY: the raw pointers are a GStreamer CUDA context and stream
// reference owned by this struct; both are GObject/boxed objects usable from
// any thread, and every CUDA call on them pushes the context first.
unsafe impl Send for GpuResources {}

impl GpuResources {
    fn gpu_context(&self) -> GpuContext {
        GpuContext {
            context: self.cu_ctx,
            stream: self.cu_stream,
        }
    }
}

impl Drop for GpuResources {
    /// Turns the pool off and destroys the events with the context current,
    /// then releases the stream and, last, the context reference (the
    /// context is destroyed with its last reference, ADR-0006). Runs once:
    /// `stop` releases by dropping, and a finalised element drops whatever
    /// `stop` did not take.
    fn drop(&mut self) {
        // SAFETY: the context reference is still ours (released below).
        match unsafe { ContextGuard::push(self.cu_ctx) } {
            Ok(guard) => {
                if let Some(pool) = self.pool.take() {
                    let _ = pool.set_active(false);
                }
                // SAFETY: created in `ensure_gpu`, dropped only here, once.
                unsafe {
                    ManuallyDrop::drop(&mut self.start);
                    ManuallyDrop::drop(&mut self.end);
                }
                drop(guard);
            }
            Err(e) => {
                // Destroying the events in no (or another) context could act
                // on the wrong one: leak them, they die with the context.
                gst::error!(
                    CAT,
                    "cannot push the CUDA context to release the element's events, leaking \
                     them: {e}"
                );
                if let Some(pool) = self.pool.take() {
                    let _ = pool.set_active(false);
                }
            }
        }
        if let Ok(lib) = cuda::gst_cuda() {
            // SAFETY: our own stream and context references, released once,
            // the context last. `gst_cuda_stream_unref` pushes the context
            // itself.
            unsafe {
                (lib.gst_cuda_stream_unref)(self.stream);
                gst::ffi::gst_object_unref(self.context.cast());
            }
        }
    }
}

/// Runs a closure when dropped unless disarmed: the cleanup of every early
/// return of a function.
struct OnExit<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnExit<F> {
    fn new(f: F) -> Self {
        OnExit(Some(f))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for OnExit<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Leaks a backend instance that cannot be unloaded because its CUDA
/// context could not be pushed: unloading (or dropping, which unloads)
/// without it would make the SDK's CUDA runtime free its objects in no
/// context or in another one, and could create the primary context
/// (ADR-0006). Its memory is reclaimed when the context is destroyed.
fn leak_backend(instance: Box<dyn InferenceBackend>, why: &str) {
    gst::error!(
        CAT,
        "cannot push the CUDA context to unload the backend ({why}); leaking it instead of \
         unloading it outside its context"
    );
    std::mem::forget(instance);
}

#[derive(Default)]
struct StreamState {
    info: Option<VideoInfo>,
    caps: Option<gst::Caps>,
    format: Option<Format>,
    action: FrameAction,
    /// The request the action was decided for, and its per-frame settings.
    generation: u64,
    strength: f32,
    fallback: EffectFallback,
    gpu: Option<GpuResources>,
}

#[derive(Default)]
pub struct ObcEffect {
    shared: Arc<Shared>,
    stream: Mutex<StreamState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[glib::object_subclass]
impl ObjectSubclass for ObcEffect {
    const NAME: &'static str = "GstObcEffect";
    type Type = super::ObcEffect;
    type ParentType = gst_base::BaseTransform;
}

/// Posts the status message (never with a lock held: a bus sync handler
/// may call back into the element).
fn post_status(element: &super::ObcEffect, kind: EffectKind, status: &EffectStatus) {
    let s = gst::Structure::builder(STATUS_MESSAGE)
        .field("effect", kind.as_str())
        .field("status", status.as_str())
        .field("reason", status.reason().unwrap_or(""))
        .build();
    let _ = element.post_message(gst::message::Element::builder(s).src(element).build());
}

impl ObcEffect {
    pub(super) fn set_backend_factory(&self, factory: Option<Arc<dyn BackendFactory>>) {
        lock(&self.shared.control).factory = factory;
    }

    pub(super) fn backend_factory(&self) -> Option<Arc<dyn BackendFactory>> {
        lock(&self.shared.control).factory.clone()
    }

    pub(super) fn settings(&self) -> EffectSettings {
        lock(&self.shared.control).settings.clone()
    }

    pub(super) fn status(&self) -> EffectStatus {
        lock(&self.shared.control).status.clone()
    }

    pub(super) fn stats(&self) -> EffectStats {
        let (effect, status, last_load_ms, last_load_reused, loads) = {
            let c = lock(&self.shared.control);
            (
                c.settings.kind,
                c.status.clone(),
                c.last_load_ms,
                c.last_load_reused,
                c.loads,
            )
        };
        let window: Vec<f32> = lock(&self.shared.gpu_ms).iter().copied().collect();
        let s = &self.shared;
        EffectStats {
            effect,
            status,
            frames_processed: s.processed.load(Ordering::Relaxed),
            frames_black: s.black.load(Ordering::Relaxed),
            frames_passthrough: s.passthrough.load(Ordering::Relaxed),
            gpu_ms_p50: percentile(&window, 50.0),
            gpu_ms_p95: percentile(&window, 95.0),
            last_load_ms,
            last_load_reused,
            loads,
            cuda_active: s.cuda_active.load(Ordering::Relaxed),
        }
    }

    /// Applies new settings. A change of what has to be loaded (the effect,
    /// or the image of replace) is a new request for the worker; `retry`
    /// makes one after a failure even when nothing changed.
    pub(super) fn apply(&self, new: EffectSettings, retry: bool) {
        let posted = {
            let mut c = lock(&self.shared.control);
            let target = new.target();
            let failed = matches!(c.status, EffectStatus::Failed(_));
            c.settings = new;
            if target != c.target || (retry && failed) {
                c.target = target;
                c.generation += 1;
                c.status = match c.target.kind {
                    EffectKind::None => EffectStatus::Off,
                    _ => EffectStatus::Loading,
                };
                self.shared.wake.notify_all();
                Some((c.settings.kind, c.status.clone()))
            } else {
                None
            }
        };
        if let Some((kind, status)) = posted {
            gst::debug!(CAT, imp = self, "effect {kind}: {status}");
            post_status(&self.obj(), kind, &status);
        }
    }

    /// Marks request `generation` as failed (unless superseded), and posts.
    fn fail(&self, generation: u64, reason: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            if c.generation != generation || matches!(c.status, EffectStatus::Failed(_)) {
                None
            } else {
                c.status = EffectStatus::Failed(reason);
                Some((c.settings.kind, c.status.clone()))
            }
        };
        if let Some((kind, status)) = posted {
            gst::warning!(CAT, imp = self, "effect {kind}: {status}");
            post_status(&self.obj(), kind, &status);
        }
    }

    /// Creates the CUDA objects from the first input memory: a reference to
    /// its context, a stream, two timing events; and the output pool for
    /// the negotiated caps if there is none.
    fn ensure_gpu(&self, st: &mut StreamState, inbuf: &gst::BufferRef) -> Result<(), String> {
        let lib = cuda::gst_cuda()?;
        let context = input_context(lib, inbuf)?;
        if st.gpu.is_none() {
            // SAFETY: `context` is the live GstCudaContext of the input
            // memory; we take our own reference for the lifetime of
            // `GpuResources`.
            let cu_ctx = unsafe {
                gst::ffi::gst_object_ref(context.cast());
                (lib.gst_cuda_context_get_handle)(context)
            };
            let release_ctx = || unsafe { gst::ffi::gst_object_unref(context.cast()) };
            // SAFETY: as above; the stream is created in that context.
            let stream = unsafe { (lib.gst_cuda_stream_new)(context) };
            if stream.is_null() || cu_ctx.is_null() {
                release_ctx();
                return Err("could not create a CUDA stream in the stream's context".into());
            }
            // SAFETY: `stream` is the live GstCudaStream just created.
            let cu_stream = unsafe { (lib.gst_cuda_stream_get_handle)(stream) };
            // SAFETY: `cu_ctx` is the live context we hold a reference to.
            let events = unsafe { ContextGuard::push(cu_ctx) }
                .and_then(|_g| Ok((Event::new()?, Event::new()?)));
            let (start, end) = match events {
                Ok(e) => e,
                Err(e) => {
                    // SAFETY: our own stream and context references.
                    unsafe { (lib.gst_cuda_stream_unref)(stream) };
                    release_ctx();
                    return Err(format!("CUDA timing events: {e}"));
                }
            };
            st.gpu = Some(GpuResources {
                context,
                cu_ctx,
                stream,
                cu_stream,
                pool: None,
                start: ManuallyDrop::new(start),
                end: ManuallyDrop::new(end),
            });
            self.shared.cuda_active.store(true, Ordering::Relaxed);
            let gpu = st.gpu.as_ref().map(GpuResources::gpu_context);
            let mut c = lock(&self.shared.control);
            c.gpu = gpu;
            self.shared.wake.notify_all();
            gst::info!(
                CAT,
                imp = self,
                "CUDA resources created on the input's context"
            );
        }
        let gpu = st.gpu.as_mut().expect("created above");
        if gpu.context != context {
            return Err(
                "an input frame is in another CUDA context than the stream's first frame".into(),
            );
        }
        if gpu.pool.is_none() {
            let (Some(caps), Some(info)) = (&st.caps, &st.info) else {
                return Err("no caps negotiated".into());
            };
            gpu.pool = Some(make_pool(lib, gpu, caps, info)?);
        }
        Ok(())
    }

    /// Releases the backend, then the CUDA objects (see the module docs).
    fn release_gpu(&self) {
        let gpu = lock(&self.stream).gpu.take();
        let Some(gpu) = gpu else {
            return;
        };
        {
            // SAFETY: the context reference is still ours.
            let guard = unsafe { ContextGuard::push(gpu.cu_ctx) };
            let mut e = lock(&self.shared.engine);
            if let Some(mut instance) = e.instance.take() {
                match &guard {
                    Ok(_) => {
                        instance.unload();
                        drop(instance);
                    }
                    Err(err) => leak_backend(instance, &err.to_string()),
                }
            }
            e.loaded = None;
            drop(e);
            drop(guard);
        }
        // The pool, the events, the stream and the context reference.
        drop(gpu);
        self.shared.cuda_active.store(false, Ordering::Relaxed);
        let mut c = lock(&self.shared.control);
        c.gpu = None;
        c.loaded = false;
        gst::info!(
            CAT,
            imp = self,
            "backend unloaded and CUDA resources released"
        );
    }

    fn stop_worker(&self) {
        {
            let mut c = lock(&self.shared.control);
            c.stop = true;
            self.shared.wake.notify_all();
        }
        if let Some(h) = lock(&self.worker).take() {
            let _ = h.join();
        }
    }

    /// Fills `out` for `action` on the element's stream and waits for it.
    fn render(
        &self,
        st: &mut StreamState,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<(), String> {
        let lib = cuda::gst_cuda()?;
        let info = st.info.clone().ok_or("no caps negotiated")?;
        let format = st.format.clone().ok_or("no caps negotiated")?;
        let gpu = st.gpu.as_mut().ok_or("no CUDA resources")?;
        // SAFETY: our context reference is live.
        let _guard = unsafe { ContextGuard::push(gpu.cu_ctx) }.map_err(|e| e.to_string())?;
        // SAFETY: the output buffer is ours, writable, from our CUDA pool.
        let out = unsafe {
            CudaFrame::map(
                outbuf.as_mut_ptr(),
                &info,
                gst::ffi::GST_MAP_WRITE | MAP_CUDA,
            )
        }?;
        let dst = out.nv12();
        let stream = gpu.cu_stream;
        let fill_black = |dst: &DeviceNv12| {
            // SAFETY: `dst` is our mapped output frame; `stream` is ours.
            unsafe { obc_gpu::fill_black_nv12(dst, format.black, stream) }
                .map_err(|e| e.to_string())
        };
        let mut processed = false;
        // The input copied into the output (a failed frame, on-failure=none).
        let mut copied = false;
        // The input stays mapped until the stream has finished reading it.
        let mut input_frame: Option<CudaFrame> = None;
        // Every exit from here waits for the stream before the frames are
        // unmapped (this guard drops before them), so an early return never
        // releases the input, or hands out the output, while work queued on
        // them may still run. The normal path disarms it and waits itself.
        let mut sync_on_exit = OnExit::new(move || {
            // SAFETY: our own stream; the context is still pushed.
            let _ = unsafe { obc_gpu::stream_synchronize(stream) };
        });
        match st.action {
            FrameAction::Passthrough => {
                return Err("internal: passthrough frame in transform".into());
            }
            FrameAction::Black => fill_black(&dst)?,
            FrameAction::Process => {
                let mem = inbuf.peek_memory(0);
                // SAFETY: `ensure_gpu` checked that this is a GstCudaMemory
                // of our context; the sync only waits for its producer.
                unsafe { (lib.gst_cuda_memory_sync)(mem.as_mut_ptr().cast()) };
                // SAFETY: read-only CUDA map of the input; it is never
                // written.
                let input = unsafe {
                    CudaFrame::map(inbuf.as_mut_ptr(), &info, gst::ffi::GST_MAP_READ | MAP_CUDA)
                }?;
                let src = input.nv12();
                // Owned by the outer scope, so it outlives `sync_on_exit`.
                input_frame = Some(input);
                let params = FrameParams {
                    blur_strength: st.strength,
                };
                let ctx = gpu.gpu_context();
                let mut e = lock(&self.shared.engine);
                let Engine { instance, loaded } = &mut *e;
                let result = match (instance.as_mut(), loaded.is_some()) {
                    (Some(instance), true) => {
                        // SAFETY: our own stream.
                        let _ = unsafe { gpu.start.record(stream) };
                        let r = instance.process(&ctx, &params, &src, &dst);
                        // SAFETY: our own stream.
                        let _ = unsafe { gpu.end.record(stream) };
                        Some(r)
                    }
                    _ => None,
                };
                drop(e);
                match result {
                    Some(Ok(())) => processed = true,
                    Some(Err(err)) => {
                        let reason = format!("processing a frame failed: {err}");
                        self.fail(st.generation, reason);
                        match st.fallback {
                            EffectFallback::Black => fill_black(&dst)?,
                            // SAFETY: both frames are mapped CUDA frames of
                            // our context, same size; the input is read.
                            EffectFallback::None => {
                                unsafe { obc_gpu::copy_nv12_async(&src, &dst, stream) }
                                    .map_err(|e| e.to_string())?;
                                copied = true;
                            }
                        }
                    }
                    // Unloaded since the decision: black, never the input.
                    None => fill_black(&dst)?,
                }
            }
        }
        sync_on_exit.disarm();
        // SAFETY: our own stream. Waiting here also ends this frame's read
        // of the input (see the module docs for why).
        unsafe { obc_gpu::stream_synchronize(stream) }.map_err(|e| e.to_string())?;
        drop(input_frame);
        drop(out);
        if processed {
            self.shared.processed.fetch_add(1, Ordering::Relaxed);
            if let Ok(ms) = Event::elapsed_ms(&gpu.start, &gpu.end) {
                let mut w = lock(&self.shared.gpu_ms);
                if w.len() == EffectStats::WINDOW {
                    w.pop_front();
                }
                w.push_back(ms);
            }
        } else if copied {
            self.shared.passthrough.fetch_add(1, Ordering::Relaxed);
        } else {
            self.shared.black.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// The `GstCudaContext` of the input's (single) memory, or why there is
/// none.
fn input_context(lib: &cuda::GstCuda, inbuf: &gst::BufferRef) -> Result<*mut c_void, String> {
    if inbuf.n_memory() != 1 {
        return Err(format!(
            "an input frame has {} memories, not one CUDA memory",
            inbuf.n_memory()
        ));
    }
    let mem = inbuf.peek_memory(0).as_mut_ptr();
    // SAFETY: `mem` is a live GstMemory of the buffer.
    if unsafe { (lib.gst_is_cuda_memory)(mem) } == glib::ffi::GFALSE {
        return Err("an input frame is not in CUDA memory".into());
    }
    // SAFETY: `gst_is_cuda_memory` said it is a GstCudaMemory, whose public
    // head is `CudaMemoryHead`.
    let ctx = unsafe { (*mem.cast::<CudaMemoryHead>()).context };
    if ctx.is_null() {
        return Err("an input CUDA memory has no context".into());
    }
    Ok(ctx)
}

/// A CUDA buffer pool on our context for `caps`, carrying our stream.
fn make_pool(
    lib: &cuda::GstCuda,
    gpu: &GpuResources,
    caps: &gst::Caps,
    info: &VideoInfo,
) -> Result<gst::BufferPool, String> {
    use gst::glib::translate::from_glib_full;
    // SAFETY: our live context; the call returns a new (full) reference.
    let raw = unsafe { (lib.gst_cuda_buffer_pool_new)(gpu.context) };
    if raw.is_null() {
        return Err("could not create a CUDA buffer pool".into());
    }
    // SAFETY: a new GstBufferPool reference, transferred to us.
    let pool: gst::BufferPool = unsafe { from_glib_full(raw) };
    let mut config = pool.config();
    config.set_params(Some(caps), info.size() as u32, 2, 0);
    config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
    // SAFETY: `config` is a live pool configuration structure we own;
    // the call stores a reference to our live stream in it.
    unsafe { (lib.gst_buffer_pool_config_set_cuda_stream)(config.as_mut_ptr(), gpu.stream) };
    pool.set_config(config)
        .map_err(|e| format!("CUDA buffer pool configuration: {e}"))?;
    pool.set_active(true)
        .map_err(|e| format!("CUDA buffer pool activation: {e}"))?;
    Ok(pool)
}

/// A video frame mapped with `GST_MAP_CUDA`: plane pointers are device
/// addresses, laid out as the buffer's video meta says. Unmapped on drop.
struct CudaFrame {
    frame: gst_video::ffi::GstVideoFrame,
}

impl CudaFrame {
    /// # Safety
    /// `buffer` must be a live buffer of CUDA memory described by `info`,
    /// writable if `flags` has `GST_MAP_WRITE`, and outlive the frame.
    unsafe fn map(
        buffer: *mut gst::ffi::GstBuffer,
        info: &VideoInfo,
        flags: u32,
    ) -> Result<CudaFrame, String> {
        use gst::glib::translate::ToGlibPtr;
        // SAFETY: a zeroed GstVideoFrame is the documented initial state.
        let mut frame: gst_video::ffi::GstVideoFrame = unsafe { std::mem::zeroed() };
        // SAFETY: the caller guarantees `buffer`; NO_REF keeps the buffer's
        // refcount (and so its writability) unchanged.
        let ok = unsafe {
            gst_video::ffi::gst_video_frame_map(
                &mut frame,
                info.to_glib_none().0 as *mut _,
                buffer,
                flags | gst_video::ffi::GST_VIDEO_FRAME_MAP_FLAG_NO_REF,
            )
        };
        if ok == glib::ffi::GFALSE {
            return Err("could not map a frame's CUDA memory".into());
        }
        Ok(CudaFrame { frame })
    }

    fn nv12(&self) -> DeviceNv12 {
        let f = &self.frame;
        DeviceNv12 {
            y: f.data[0] as u64,
            y_pitch: f.info.stride[0].max(0) as usize,
            uv: f.data[1] as u64,
            uv_pitch: f.info.stride[1].max(0) as usize,
            width: f.info.width.max(0) as u32,
            height: f.info.height.max(0) as u32,
        }
    }
}

impl Drop for CudaFrame {
    fn drop(&mut self) {
        // SAFETY: mapped by `map`, unmapped once.
        unsafe { gst_video::ffi::gst_video_frame_unmap(&mut self.frame) };
    }
}

/// One unit of work for the worker: the request and what it needs.
struct Job {
    generation: u64,
    target: Target,
    gpu: GpuContext,
    format: Format,
    factory: Option<Arc<dyn BackendFactory>>,
}

enum Outcome {
    Unloaded,
    Loaded(LoadReport),
    AlreadyLoaded,
    Failed(String),
    Superseded,
}

fn worker_loop(shared: Arc<Shared>, element: glib::WeakRef<super::ObcEffect>) {
    let mut handled: Option<u64> = None;
    loop {
        let job = {
            let mut c = lock(&shared.control);
            loop {
                if c.stop {
                    return;
                }
                if handled != Some(c.generation)
                    && let (Some(gpu), Some(format)) = (c.gpu, c.format.clone())
                {
                    break Job {
                        generation: c.generation,
                        target: c.target.clone(),
                        gpu,
                        format,
                        factory: c.factory.clone(),
                    };
                }
                c = shared.wake.wait(c).unwrap_or_else(|e| e.into_inner());
            }
        };
        handled = Some(job.generation);
        let outcome = run_job(&shared, &job);
        let loaded = lock(&shared.engine).loaded.is_some();
        let posted = {
            let mut c = lock(&shared.control);
            c.loaded = loaded;
            if let Outcome::Loaded(r) = &outcome {
                c.loads += 1;
                c.last_load_ms = Some(r.load_ms + r.warmup_ms.unwrap_or(0.0));
                c.last_load_reused = r.reused_model;
            }
            let current = c.generation == job.generation;
            let status = match &outcome {
                _ if !current => None,
                Outcome::Unloaded | Outcome::Superseded => None,
                Outcome::Loaded(_) | Outcome::AlreadyLoaded => Some(EffectStatus::Ready),
                Outcome::Failed(r) => Some(EffectStatus::Failed(r.clone())),
            };
            match status {
                Some(s) if s != c.status => {
                    c.status = s;
                    Some((c.settings.kind, c.status.clone()))
                }
                _ => None,
            }
        };
        if let Some(el) = element.upgrade() {
            match &outcome {
                Outcome::Loaded(r) => gst::info!(
                    CAT,
                    obj = el,
                    "loaded {} in {:.1} ms (+{:.1} ms warm-up, model reused: {})",
                    job.target.kind,
                    r.load_ms,
                    r.warmup_ms.unwrap_or(0.0),
                    r.reused_model
                ),
                Outcome::Unloaded => gst::info!(CAT, obj = el, "unloaded"),
                _ => {}
            }
            if let Some((kind, status)) = posted {
                post_status(&el, kind, &status);
            }
        }
    }
}

fn superseded(shared: &Shared, generation: u64) -> bool {
    let c = lock(&shared.control);
    c.stop || c.generation != generation
}

fn run_job(shared: &Shared, job: &Job) -> Outcome {
    let kind = job.target.kind;
    if kind == EffectKind::None {
        lock(&shared.control).loaded = false;
        // SAFETY: the element keeps the context alive until it has joined
        // this thread.
        let guard = unsafe { ContextGuard::push(job.gpu.context) };
        let mut e = lock(&shared.engine);
        match &guard {
            Ok(_) => {
                if let Some(instance) = e.instance.as_mut() {
                    instance.unload();
                }
            }
            Err(err) => {
                if let Some(instance) = e.instance.take() {
                    leak_backend(instance, &err.to_string());
                }
            }
        }
        e.loaded = None;
        drop(e);
        drop(guard);
        return Outcome::Unloaded;
    }
    let colour = match &job.format.colour {
        Ok(c) => *c,
        Err(reason) => return Outcome::Failed(reason.clone()),
    };
    let key = LoadedKey {
        target: job.target.clone(),
        width: job.format.width,
        height: job.format.height,
        colour,
    };
    let current = lock(&shared.engine).loaded.clone();
    if current.as_ref() == Some(&key) {
        return Outcome::AlreadyLoaded;
    }
    let background = match (kind, &job.target.background) {
        (EffectKind::Replace, None) => {
            return Outcome::Failed("the replace effect needs a background image".into());
        }
        (EffectKind::Replace, Some(path)) => match decode_background(path, &job.format) {
            Ok(b) => Some(Arc::new(b)),
            Err(reason) => return Outcome::Failed(reason),
        },
        _ => None,
    };
    if superseded(shared, job.generation) {
        return Outcome::Superseded;
    }
    let Some(factory) = job.factory.clone() else {
        return Outcome::Failed("no inference backend is configured for the effects".into());
    };
    let full = current
        .as_ref()
        .is_none_or(|k| (k.width, k.height, k.colour) != (key.width, key.height, key.colour));
    if full && current.is_some() {
        lock(&shared.control).loaded = false;
    }
    // SAFETY: as for the unload above.
    let _guard = match unsafe { ContextGuard::push(job.gpu.context) } {
        Ok(g) => g,
        Err(e) => return Outcome::Failed(format!("cannot make the CUDA context current: {e}")),
    };
    let mut e = lock(&shared.engine);
    if e.instance.is_none() {
        match factory.create() {
            Ok(i) => e.instance = Some(i),
            Err(err) => return Outcome::Failed(err.to_string()),
        }
    }
    let req = LoadRequest {
        effect: kind,
        width: key.width,
        height: key.height,
        colour,
        background,
    };
    let result = e
        .instance
        .as_mut()
        .expect("created above")
        .load(&job.gpu, &req);
    match result {
        Ok(report) => {
            e.loaded = Some(key);
            Outcome::Loaded(report)
        }
        Err(err) => {
            if full {
                e.loaded = None;
            }
            Outcome::Failed(err.to_string())
        }
    }
}

fn decode_background(
    path: &std::path::Path,
    format: &Format,
) -> Result<obc_backend::HostBgr, String> {
    background::decode(path, format.width, format.height)
}

impl ObjectImpl for ObcEffect {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let rw = glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING;
            vec![
                glib::ParamSpecString::builder("effect")
                    .nick("Effect")
                    .blurb("none | blur | replace")
                    .default_value(Some("none"))
                    .flags(rw)
                    .build(),
                glib::ParamSpecDouble::builder("blur-strength")
                    .nick("Blur strength")
                    .blurb("Background blur strength, 0 to 1 (applies on the next frame)")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(f64::from(EffectSettings::DEFAULT_BLUR_STRENGTH))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("background")
                    .nick("Background image")
                    .blurb("Absolute path of the replace effect's image (empty: none)")
                    .default_value(Some(""))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("on-failure")
                    .nick("On failure")
                    .blurb("Output after a failure: black | none (the unprocessed input)")
                    .default_value(Some("black"))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("status")
                    .nick("Status")
                    .blurb("off | loading | ready | failed: <reason>")
                    .read_only()
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut s = self.settings();
        let text = || {
            value
                .get::<Option<String>>()
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        let retry = match pspec.name() {
            "effect" => match parse_effect(&text()) {
                Ok(k) => {
                    s.kind = k;
                    true
                }
                Err(e) => {
                    gst::warning!(CAT, imp = self, "ignoring effect: {e}");
                    return;
                }
            },
            "blur-strength" => {
                s.blur_strength = value.get::<f64>().unwrap_or(0.5) as f32;
                false
            }
            "background" => {
                let t = text();
                let p = (!t.is_empty()).then(|| PathBuf::from(t));
                if let Some(p) = &p
                    && !p.is_absolute()
                {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "ignoring background {}: not an absolute path",
                        p.display()
                    );
                    return;
                }
                s.background = p;
                true
            }
            "on-failure" => match text().parse::<EffectFallback>() {
                Ok(f) => {
                    s.fallback = f;
                    false
                }
                Err(e) => {
                    gst::warning!(CAT, imp = self, "ignoring on-failure: {e}");
                    return;
                }
            },
            other => unreachable!("no property {other}"),
        };
        self.apply(s, retry);
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let s = self.settings();
        match pspec.name() {
            "effect" => s.kind.as_str().to_value(),
            "blur-strength" => f64::from(s.blur_strength).to_value(),
            "background" => s
                .background
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
                .to_value(),
            "on-failure" => s.fallback.as_str().to_value(),
            "status" => self.status().to_string().to_value(),
            other => unreachable!("no property {other}"),
        }
    }
}

impl GstObjectImpl for ObcEffect {}

impl ElementImpl for ObcEffect {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "omarchy-broadcast effect",
                "Filter/Effect/Video/Hardware",
                "Background blur or replacement on NV12 CUDA frames through an inference \
                 backend; passthrough without an effect",
                "Raul Dias",
            )
        });
        Some(&*META)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("video/x-raw")
                .features(["memory:CUDAMemory"])
                .field("format", "NV12")
                .build();
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("sink template"),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("src template"),
            ]
        });
        TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for ObcEffect {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        {
            let mut c = lock(&self.shared.control);
            c.stop = false;
        }
        let shared = Arc::clone(&self.shared);
        let element = self.obj().downgrade();
        let handle = std::thread::Builder::new()
            .name("obceffect-load".into())
            .spawn(move || worker_loop(shared, element))
            .map_err(|e| {
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["cannot start the effect worker thread: {e}"]
                )
            })?;
        *lock(&self.worker) = Some(handle);
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.stop_worker();
        self.release_gpu();
        let posted = {
            let mut c = lock(&self.shared.control);
            let status = match c.settings.kind {
                EffectKind::None => EffectStatus::Off,
                _ => EffectStatus::Loading,
            };
            // A restart loads again (and retries a failure).
            c.generation += 1;
            (c.status != status).then(|| {
                c.status = status;
                (c.settings.kind, c.status.clone())
            })
        };
        if let Some((kind, status)) = posted {
            post_status(&self.obj(), kind, &status);
        }
        let mut st = lock(&self.stream);
        st.action = FrameAction::Passthrough;
        Ok(())
    }

    fn transform_caps(
        &self,
        _direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        // The same caps on both sides: the element never converts.
        Some(match filter {
            Some(f) => f.intersect_with_mode(caps, gst::CapsIntersectMode::First),
            None => caps.clone(),
        })
    }

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = VideoInfo::from_caps(incaps)
            .map_err(|e| gst::loggable_error!(CAT, "caps {incaps}: {e}"))?;
        let format = Format::of(&info);
        {
            let mut st = lock(&self.stream);
            if st.caps.as_ref() != Some(incaps)
                && let Some(gpu) = st.gpu.as_mut()
                && let Some(pool) = gpu.pool.take()
            {
                let _ = pool.set_active(false);
            }
            st.info = Some(info);
            st.caps = Some(incaps.clone());
            st.format = Some(format.clone());
        }
        let posted = {
            let mut c = lock(&self.shared.control);
            let mut posted = None;
            if c.format.as_ref() != Some(&format) {
                c.format = Some(format);
                // Loading depends on the size and colour: a new request.
                c.generation += 1;
                if c.target.kind != EffectKind::None && c.status != EffectStatus::Loading {
                    c.status = EffectStatus::Loading;
                    posted = Some((c.settings.kind, c.status.clone()));
                }
                self.shared.wake.notify_all();
            }
            posted
        };
        if let Some((kind, status)) = posted {
            post_status(&self.obj(), kind, &status);
        }
        Ok(())
    }

    fn decide_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        // The element allocates from its own pool, and only once an effect
        // runs: drop downstream's pool so BaseTransform activates nothing
        // (no CUDA allocation in passthrough).
        while query.allocation_pools().next().is_some() {
            query.remove_nth_allocation_pool(0);
        }
        Ok(())
    }

    fn submit_input_buffer(
        &self,
        is_discont: bool,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.prepare_frame(&inbuf);
        self.parent_submit_input_buffer(is_discont, inbuf)
    }

    fn prepare_output_buffer(
        &self,
        inbuf: InputBuffer,
    ) -> Result<PrepareOutputBufferSuccess, gst::FlowError> {
        let inbuf = match inbuf {
            InputBuffer::Readable(b) => b,
            InputBuffer::Writable(b) => &*b,
        };
        let st = lock(&self.stream);
        if st.action == FrameAction::Passthrough {
            return Ok(PrepareOutputBufferSuccess::InputBuffer);
        }
        let Some(pool) = st.gpu.as_ref().and_then(|g| g.pool.as_ref()) else {
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["the effect has no CUDA buffer pool to write black or processed frames"]
            );
            return Err(gst::FlowError::Error);
        };
        let mut out = pool.acquire_buffer(None).inspect_err(|e| {
            gst::error!(
                CAT,
                imp = self,
                "no output buffer from the CUDA pool: {e:?}"
            );
        })?;
        drop(st);
        let outref = out.get_mut().ok_or(gst::FlowError::Error)?;
        self.parent_copy_metadata(inbuf, outref)
            .map_err(|_| gst::FlowError::Error)?;
        Ok(PrepareOutputBufferSuccess::Buffer(out))
    }

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut st = lock(&self.stream);
        if let Err(reason) = self.render(&mut st, inbuf, outbuf) {
            drop(st);
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["the effect could not write a frame: {reason}"]
            );
            return Err(gst::FlowError::Error);
        }
        Ok(gst::FlowSuccess::Ok)
    }
}

impl ObcEffect {
    /// Decides what the frame gets and sets BaseTransform's passthrough to
    /// match, creating the CUDA objects when the frame needs them.
    fn prepare_frame(&self, inbuf: &gst::BufferRef) {
        let (kind, status, fallback, loaded, strength, generation) = {
            let c = lock(&self.shared.control);
            (
                c.settings.kind,
                c.status.clone(),
                c.settings.fallback,
                c.loaded,
                c.settings.blur_strength,
                c.generation,
            )
        };
        let mut action = decide(kind, &status, fallback, loaded);
        let mut st = lock(&self.stream);
        if action != FrameAction::Passthrough
            && let Err(reason) = self.ensure_gpu(&mut st, inbuf)
        {
            drop(st);
            self.fail(generation, reason.clone());
            st = lock(&self.stream);
            action = decide(kind, &EffectStatus::Failed(reason), fallback, loaded);
        }
        st.action = action;
        st.generation = generation;
        st.strength = strength;
        st.fallback = fallback;
        drop(st);
        let passthrough = action == FrameAction::Passthrough;
        if passthrough {
            self.shared.passthrough.fetch_add(1, Ordering::Relaxed);
        }
        let obj = self.obj();
        if obj.is_passthrough() != passthrough {
            obj.set_passthrough(passthrough);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OnExit;
    use std::cell::Cell;

    /// The shape of `render`: an early `?` return after work was queued.
    fn queue_then(fail_early: bool, synced: &Cell<u32>) -> Result<(), String> {
        let mut guard = OnExit::new(|| synced.set(synced.get() + 1));
        if fail_early {
            Err("a fill failed".to_string())?;
        }
        guard.disarm();
        Ok(())
    }

    #[test]
    fn the_exit_guard_runs_on_an_early_return_only() {
        let synced = Cell::new(0);
        assert!(queue_then(true, &synced).is_err());
        assert_eq!(synced.get(), 1, "an early return synchronises");
        queue_then(false, &synced).unwrap();
        assert_eq!(synced.get(), 1, "a disarmed guard does nothing");
    }
}
