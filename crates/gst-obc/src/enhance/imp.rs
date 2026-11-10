//! The implementation of `obcenhance`.
//!
//! # Threads and locks
//!
//! Everything runs on the streaming thread: the load with the first frame
//! (a fraction of a second, before the ingest has delivered anything), the
//! frames, the failure and the release in `stop`. `StreamState` holds the
//! CUDA objects and the enhancer under one mutex; `Shared` holds what the
//! application reads (status, counters) and the flag that switches the caps
//! to passthrough after a failure.
//!
//! # Failure
//!
//! The source caps are `scale` times the sink caps while the stage works,
//! and equal to them once it has failed (`transform_caps`, from
//! `Shared::failed`). A failure, in `submit_input_buffer` (the load) or in
//! `transform` (a frame), releases the enhancer, sets the flag, marks the
//! source pad for renegotiation and drops that frame
//! (`BASE_TRANSFORM_FLOW_DROPPED`). The next frame renegotiates: the caps
//! are equal on both sides, so BaseTransform passes frames through, and
//! the downstream scaler takes the capture size as it does without the
//! stage. No frame of the wrong size ever leaves the element.
//!
//! # CUDA context and stream
//!
//! As in `obceffect`: the context is the one of the first input memory, the
//! element creates one `GstCudaStream` in it and runs everything there, the
//! input is synchronised with `gst_cuda_memory_sync` before it is read, the
//! output pool carries the element's stream, and the stream is synchronised
//! before `transform` returns.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::base_transform::{InputBuffer, PrepareOutputBufferSuccess};
use gst_base::subclass::prelude::*;
use gst_video::VideoInfo;
use obc_backend::enhance::{
    ChromaSiting, DeviceYuv420, EnhanceRequest, Enhancer, EnhancerFactory, MAX_SCALE, MIN_SCALE,
    YuvLayout,
};
use obc_backend::{ColourMatrix, ColourSpec, DeviceNv12, GpuContext};
use obc_gpu::{ContextGuard, CuContext, CuStream, Event};

use super::{EnhanceStats, STATUS_MESSAGE};
use crate::cuda::{self, CudaMemoryHead, MAP_CUDA};
use crate::settings::{EffectStatus, percentile};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "obcenhance",
        gst::DebugColorFlags::empty(),
        Some("omarchy-broadcast enhance element"),
    )
});

/// The GPU times kept for the percentiles.
const WINDOW: usize = 300;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Shared {
    factory: Mutex<Option<Arc<dyn EnhancerFactory>>>,
    scale: AtomicU32,
    /// The stage has failed: the caps are the same on both sides.
    failed: AtomicBool,
    status: Mutex<EffectStatus>,
    enhanced: AtomicU64,
    passthrough: AtomicU64,
    dropped: AtomicU64,
    load_ms: Mutex<Option<f64>>,
    gpu_ms: Mutex<VecDeque<f32>>,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            factory: Mutex::new(None),
            scale: AtomicU32::new(MIN_SCALE),
            failed: AtomicBool::new(false),
            status: Mutex::new(EffectStatus::Loading),
            enhanced: AtomicU64::new(0),
            passthrough: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            load_ms: Mutex::new(None),
            gpu_ms: Mutex::new(VecDeque::with_capacity(WINDOW)),
        }
    }
}

/// The element's CUDA objects, created with the first frame and released
/// in `stop` (after the enhancer), or with the element.
struct Gpu {
    /// A reference to the input's `GstCudaContext`.
    context: *mut c_void,
    cu_ctx: CuContext,
    /// The element's own `GstCudaStream`.
    stream: *mut c_void,
    cu_stream: CuStream,
    /// The output pool for the negotiated source caps.
    pool: Option<gst::BufferPool>,
    /// Destroyed in `drop`, with the context current.
    start: ManuallyDrop<Event>,
    end: ManuallyDrop<Event>,
}

// SAFETY: the raw pointers are a GStreamer CUDA context and stream
// reference owned by this struct; both are usable from any thread, and
// every CUDA call on them pushes the context first.
unsafe impl Send for Gpu {}

impl Gpu {
    fn gpu_context(&self) -> GpuContext {
        GpuContext {
            context: self.cu_ctx,
            stream: self.cu_stream,
        }
    }
}

impl Drop for Gpu {
    /// Turns the pool off and destroys the events with the context current,
    /// then releases the stream and, last, the context reference.
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            let _ = pool.set_active(false);
        }
        // SAFETY: the context reference is still ours (released below).
        match unsafe { ContextGuard::push(self.cu_ctx) } {
            Ok(guard) => {
                // SAFETY: created in `ensure_gpu`, dropped only here, once.
                unsafe {
                    ManuallyDrop::drop(&mut self.start);
                    ManuallyDrop::drop(&mut self.end);
                }
                drop(guard);
            }
            Err(e) => gst::error!(
                CAT,
                "cannot push the CUDA context to release the element's events, leaking them: {e}"
            ),
        }
        if let Ok(lib) = cuda::gst_cuda() {
            // SAFETY: our own stream and context references, released once,
            // the context last.
            unsafe {
                (lib.gst_cuda_stream_unref)(self.stream);
                gst::ffi::gst_object_unref(self.context.cast());
            }
        }
    }
}

#[derive(Default)]
struct StreamState {
    in_info: Option<VideoInfo>,
    out_info: Option<VideoInfo>,
    out_caps: Option<gst::Caps>,
    gpu: Option<Gpu>,
    instance: Option<Box<dyn Enhancer>>,
    /// What `instance` has loaded.
    loaded: Option<EnhanceRequest>,
}

impl StreamState {
    /// Unloads and drops the enhancer with its context pushed (or leaks it
    /// if the context cannot be pushed: unloading outside it could act on
    /// another context).
    fn release_instance(&mut self) {
        self.loaded = None;
        let Some(mut instance) = self.instance.take() else {
            return;
        };
        let Some(gpu) = self.gpu.as_ref() else {
            // Never loaded: there is nothing on the GPU to release.
            drop(instance);
            return;
        };
        // SAFETY: the context reference is still ours.
        match unsafe { ContextGuard::push(gpu.cu_ctx) } {
            Ok(_guard) => {
                instance.unload();
                drop(instance);
            }
            Err(e) => {
                gst::error!(
                    CAT,
                    "cannot push the CUDA context to unload the enhancer ({e}); leaking it"
                );
                std::mem::forget(instance);
            }
        }
    }
}

#[derive(Default)]
pub struct ObcEnhance {
    shared: Shared,
    stream: Mutex<StreamState>,
}

#[glib::object_subclass]
impl ObjectSubclass for ObcEnhance {
    const NAME: &'static str = "GstObcEnhance";
    type Type = super::ObcEnhance;
    type ParentType = gst_base::BaseTransform;
}

/// The enhancer's colour description of a GStreamer colorimetry: BT.601 or
/// BT.709 matrix, full or limited range.
fn colour_of(c: &gst_video::VideoColorimetry) -> Result<ColourSpec, String> {
    let full_range = match c.range() {
        gst_video::VideoColorRange::Range0_255 => true,
        gst_video::VideoColorRange::Range16_235 => false,
        other => {
            return Err(format!(
                "the enhance stage cannot process colour range {other:?} (colorimetry {c})"
            ));
        }
    };
    let matrix = match c.matrix() {
        gst_video::VideoColorMatrix::Bt601 => ColourMatrix::Bt601,
        gst_video::VideoColorMatrix::Bt709 => ColourMatrix::Bt709,
        other => {
            return Err(format!(
                "the enhance stage cannot process colour matrix {other:?} (colorimetry {c})"
            ));
        }
    };
    Ok(ColourSpec { matrix, full_range })
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
    gpu: &Gpu,
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
    // SAFETY: `config` is a live pool configuration we own; the call stores
    // a reference to our live stream in it.
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
        // refcount unchanged.
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

    /// The mapped frame size (the buffer's, which a crop region may cut).
    fn size(&self) -> (u32, u32) {
        let i = &self.frame.info;
        (i.width.max(0) as u32, i.height.max(0) as u32)
    }

    fn plane(&self, i: usize) -> (u64, usize) {
        let f = &self.frame;
        (f.data[i] as u64, f.info.stride[i].max(0) as usize)
    }

    fn nv12(&self) -> DeviceNv12 {
        let (y, y_pitch) = self.plane(0);
        let (uv, uv_pitch) = self.plane(1);
        let (width, height) = self.size();
        DeviceNv12 {
            y,
            y_pitch,
            uv,
            uv_pitch,
            width,
            height,
        }
    }

    /// The `width x height` region at `(x, y)` of this 4:2:0 frame.
    fn region(
        &self,
        layout: YuvLayout,
        (x, y): (u32, u32),
        (width, height): (u32, u32),
    ) -> Result<DeviceYuv420, String> {
        let (fw, fh) = self.size();
        if !x.is_multiple_of(2)
            || !y.is_multiple_of(2)
            || x.saturating_add(width) > fw
            || y.saturating_add(height) > fh
        {
            return Err(format!(
                "the crop region {width}x{height} at ({x}, {y}) is not an even region of the \
                 {fw}x{fh} frame"
            ));
        }
        let (x, y) = (x as u64, y as u64);
        let (y0, ys) = self.plane(0);
        let (p1, s1) = self.plane(1);
        let row = |s: usize| (y / 2) * s as u64;
        let luma = y0 + y * ys as u64 + x;
        Ok(match layout {
            YuvLayout::Nv12 => {
                let uv = p1 + row(s1) + x;
                DeviceYuv420 {
                    layout,
                    y: luma,
                    y_pitch: ys,
                    u: uv,
                    u_pitch: s1,
                    v: uv + 1,
                    v_pitch: s1,
                    width,
                    height,
                }
            }
            YuvLayout::I420 => {
                let (p2, s2) = self.plane(2);
                DeviceYuv420 {
                    layout,
                    y: luma,
                    y_pitch: ys,
                    u: p1 + row(s1) + x / 2,
                    u_pitch: s1,
                    v: p2 + row(s2) + x / 2,
                    v_pitch: s2,
                    width,
                    height,
                }
            }
        })
    }
}

impl Drop for CudaFrame {
    fn drop(&mut self) {
        // SAFETY: mapped by `map`, unmapped once.
        unsafe { gst_video::ffi::gst_video_frame_unmap(&mut self.frame) };
    }
}

/// Multiplies (`up`) or divides a width or height caps value by `k`: an
/// integer, a range or a list of them. A division that leaves a remainder
/// has no value. Other values are kept.
fn scale_dim(v: &glib::SendValue, k: i32, up: bool) -> Option<glib::SendValue> {
    let one = |i: i32| -> Option<i32> {
        if up {
            i.checked_mul(k)
        } else {
            (i % k == 0).then_some(i / k)
        }
    };
    if let Ok(i) = v.get::<i32>() {
        return one(i).map(|i| i.to_send_value());
    }
    if let Ok(r) = v.get::<gst::IntRange<i32>>() {
        let (min, max) = if up {
            (r.min().saturating_mul(k), r.max().saturating_mul(k))
        } else {
            ((r.min() + k - 1) / k, r.max() / k)
        };
        let min = min.max(1);
        return (min < max)
            .then(|| gst::IntRange::new(min, max).to_send_value())
            .or_else(|| (min == max).then(|| min.to_send_value()));
    }
    if let Ok(list) = v.get::<gst::List>() {
        let items: Vec<glib::SendValue> = list.iter().filter_map(|i| scale_dim(i, k, up)).collect();
        return (!items.is_empty()).then(|| gst::List::from_values(items).to_send_value());
    }
    Some(v.clone())
}

/// The caps on the other side of the element for `caps` on side
/// `direction`, with the factor `k`.
fn scaled_caps(direction: gst::PadDirection, caps: &gst::Caps, k: u32) -> gst::Caps {
    let k = k as i32;
    let up = direction == gst::PadDirection::Sink;
    let mut out = gst::Caps::new_empty();
    {
        let out = out.get_mut().expect("new caps are writable");
        for (s, features) in caps.iter_with_features() {
            let mut s = s.to_owned();
            let mut ok = true;
            for field in ["width", "height"] {
                let Some(v) = s.value(field).ok().cloned() else {
                    continue;
                };
                match scale_dim(&v, k, up) {
                    Some(v) => s.set_value(field, v),
                    None => ok = false,
                }
            }
            if !ok {
                continue;
            }
            if up {
                s.set("format", "NV12");
            } else {
                s.set("format", gst::List::new(["NV12", "I420"]));
            }
            out.append_structure_full(s, Some(features.to_owned()));
        }
    }
    out
}

/// The enhancer's chroma siting of a GStreamer `chroma-site` (as
/// [`VideoInfo`] reads the caps, which also fills it in for caps without
/// the field): co-sited in both directions is the top-left sample,
/// horizontally only MPEG-2's, neither (`jpeg`, `none`) centred. An
/// unknown siting and vertical-only co-siting, which the enhancer has no
/// flag for, read as MPEG-2. The output is written with the same siting.
fn siting_of(site: gst_video::VideoChromaSite) -> ChromaSiting {
    use gst_video::VideoChromaSite as S;
    if site.is_empty() {
        return ChromaSiting::Cosited;
    }
    let h = site.contains(S::H_COSITED);
    let v = site.contains(S::V_COSITED);
    match (h, v) {
        (true, true) => ChromaSiting::TopLeft,
        (true, false) => ChromaSiting::Cosited,
        (false, false) => ChromaSiting::Interstitial,
        (false, true) => ChromaSiting::Cosited,
    }
}

impl ObcEnhance {
    pub(super) fn set_factory(&self, factory: Option<Arc<dyn EnhancerFactory>>) {
        *lock(&self.shared.factory) = factory;
    }

    pub(super) fn status(&self) -> EffectStatus {
        lock(&self.shared.status).clone()
    }

    pub(super) fn stats(&self) -> EnhanceStats {
        let s = &self.shared;
        let ms: Vec<f32> = lock(&s.gpu_ms).iter().copied().collect();
        EnhanceStats {
            enhanced: s.enhanced.load(Ordering::Relaxed),
            passthrough: s.passthrough.load(Ordering::Relaxed),
            dropped: s.dropped.load(Ordering::Relaxed),
            load_ms: *lock(&s.load_ms),
            gpu_ms_p50: percentile(&ms, 50.0),
            gpu_ms_p95: percentile(&ms, 95.0),
        }
    }

    fn scale(&self) -> u32 {
        self.shared.scale.load(Ordering::Relaxed)
    }

    /// Sets the status and posts it if it changed. Call with no lock held
    /// (a bus sync handler may call back into the element).
    fn set_status(&self, status: EffectStatus) {
        {
            let mut s = lock(&self.shared.status);
            if *s == status {
                return;
            }
            *s = status.clone();
        }
        let msg = gst::message::Element::builder(
            gst::Structure::builder(STATUS_MESSAGE)
                .field("status", status.as_str())
                .field("reason", status.reason().unwrap_or(""))
                .field("scale", self.scale())
                .build(),
        )
        .src(&*self.obj())
        .build();
        let _ = self.obj().post_message(msg);
    }

    /// Fails the stage open (see the module docs): releases the enhancer,
    /// switches the caps to passthrough and asks for a renegotiation. The
    /// caller drops the current frame and then calls `set_status` with the
    /// returned status, once it holds no lock.
    fn fail(&self, st: &mut StreamState, reason: String) -> EffectStatus {
        gst::warning!(
            CAT,
            imp = self,
            "the enhance stage failed, passing the camera frames through: {reason}"
        );
        st.release_instance();
        if let Some(pool) = st.gpu.as_mut().and_then(|g| g.pool.take()) {
            let _ = pool.set_active(false);
        }
        self.shared.failed.store(true, Ordering::SeqCst);
        self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        self.obj().reconfigure_src();
        EffectStatus::Failed(reason)
    }

    /// Creates the CUDA objects from the first input memory and the output
    /// pool for the negotiated caps.
    fn ensure_gpu(&self, st: &mut StreamState, inbuf: &gst::BufferRef) -> Result<(), String> {
        let lib = cuda::gst_cuda()?;
        let context = input_context(lib, inbuf)?;
        if st.gpu.is_none() {
            // SAFETY: `context` is the live GstCudaContext of the input
            // memory; we take our own reference for the lifetime of `Gpu`.
            let cu_ctx = unsafe {
                gst::ffi::gst_object_ref(context.cast());
                (lib.gst_cuda_context_get_handle)(context)
            };
            // SAFETY: our reference, taken above.
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
            st.gpu = Some(Gpu {
                context,
                cu_ctx,
                stream,
                cu_stream,
                pool: None,
                start: ManuallyDrop::new(start),
                end: ManuallyDrop::new(end),
            });
        }
        let gpu = st.gpu.as_mut().expect("created above");
        if gpu.context != context {
            return Err(
                "an input frame is in another CUDA context than the stream's first frame".into(),
            );
        }
        if gpu.pool.is_none() {
            let (Some(caps), Some(info)) = (&st.out_caps, &st.out_info) else {
                return Err("no caps negotiated".into());
            };
            gpu.pool = Some(make_pool(lib, gpu, caps, info)?);
        }
        Ok(())
    }

    /// The request for the negotiated input caps.
    fn request(&self, st: &StreamState) -> Result<EnhanceRequest, String> {
        let info = st.in_info.as_ref().ok_or("no caps negotiated")?;
        let layout = match info.format() {
            gst_video::VideoFormat::Nv12 => YuvLayout::Nv12,
            gst_video::VideoFormat::I420 => YuvLayout::I420,
            other => return Err(format!("the enhance stage cannot read {other:?} frames")),
        };
        Ok(EnhanceRequest {
            width: info.width(),
            height: info.height(),
            scale: self.scale(),
            layout,
            colour: colour_of(&info.colorimetry())?,
            chroma: siting_of(info.chroma_site()),
        })
    }

    /// Makes sure the enhancer is loaded for the negotiated caps; returns
    /// whether it loaded now.
    fn ensure_loaded(&self, st: &mut StreamState, inbuf: &gst::BufferRef) -> Result<bool, String> {
        self.ensure_gpu(st, inbuf)?;
        let req = self.request(st)?;
        if st.loaded == Some(req) {
            return Ok(false);
        }
        st.loaded = None;
        if st.instance.is_none() {
            let factory = lock(&self.shared.factory)
                .clone()
                .ok_or("no enhancer is configured")?;
            st.instance = Some(factory.create().map_err(|e| e.to_string())?);
        }
        let gpu = st.gpu.as_ref().expect("ensured above");
        let ctx = gpu.gpu_context();
        // SAFETY: our context reference is live.
        let _guard = unsafe { ContextGuard::push(gpu.cu_ctx) }
            .map_err(|e| format!("cannot make the CUDA context current: {e}"))?;
        let instance = st.instance.as_mut().expect("created above");
        let report = instance.load(&ctx, &req).map_err(|e| e.to_string())?;
        gst::info!(
            CAT,
            imp = self,
            "{} loaded for {}x{} {} x{} in {:.0} ms",
            instance.name(),
            req.width,
            req.height,
            req.layout.as_str(),
            req.scale,
            report.load_ms
        );
        *lock(&self.shared.load_ms) = Some(report.load_ms);
        st.loaded = Some(req);
        Ok(true)
    }

    /// Enhances `inbuf` into `outbuf` on the element's stream and waits for
    /// it.
    fn render(
        &self,
        st: &mut StreamState,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<(), String> {
        let lib = cuda::gst_cuda()?;
        let req = st.loaded.ok_or("the enhancer is not loaded")?;
        let in_info = st.in_info.clone().ok_or("no caps negotiated")?;
        let out_info = st.out_info.clone().ok_or("no caps negotiated")?;
        let gpu = st.gpu.as_ref().ok_or("no CUDA resources")?;
        let instance = st.instance.as_mut().ok_or("the enhancer is not loaded")?;
        // SAFETY: our context reference is live.
        let _guard = unsafe { ContextGuard::push(gpu.cu_ctx) }.map_err(|e| e.to_string())?;
        let stream = gpu.cu_stream;
        let crop = inbuf.meta::<gst_video::VideoCropMeta>().map(|m| m.rect());
        let mem = inbuf.peek_memory(0);
        // SAFETY: `ensure_gpu` checked that this is a GstCudaMemory of our
        // context; the sync only waits for its producer.
        unsafe { (lib.gst_cuda_memory_sync)(mem.as_mut_ptr().cast()) };
        // SAFETY: read-only CUDA map of the input; it is never written.
        let input = unsafe {
            CudaFrame::map(
                inbuf.as_mut_ptr(),
                &in_info,
                gst::ffi::GST_MAP_READ | MAP_CUDA,
            )
        }?;
        let (x, y, w, h) = crop.unwrap_or((0, 0, input.size().0, input.size().1));
        if (w, h) != (req.width, req.height) {
            return Err(format!(
                "an input frame is {w}x{h}, the caps say {}x{}",
                req.width, req.height
            ));
        }
        let src = input.region(req.layout, (x, y), (w, h))?;
        // SAFETY: the output buffer is ours, writable, from our CUDA pool.
        let out = unsafe {
            CudaFrame::map(
                outbuf.as_mut_ptr(),
                &out_info,
                gst::ffi::GST_MAP_WRITE | MAP_CUDA,
            )
        }?;
        let dst = out.nv12();
        // SAFETY: our own stream.
        let _ = unsafe { gpu.start.record(stream) };
        let result = instance.process(&gpu.gpu_context(), &src, &dst);
        // SAFETY: our own stream.
        let _ = unsafe { gpu.end.record(stream) };
        // Always wait, so that no queued work outlives the maps below, and
        // the input's next write never races this frame's read.
        // SAFETY: our own stream; the context is pushed.
        let synced = unsafe { obc_gpu::stream_synchronize(stream) };
        drop(input);
        drop(out);
        result.map_err(|e| e.to_string())?;
        synced.map_err(|e| e.to_string())?;
        if let Ok(ms) = Event::elapsed_ms(&gpu.start, &gpu.end) {
            let mut w = lock(&self.shared.gpu_ms);
            if w.len() == WINDOW {
                w.pop_front();
            }
            w.push_back(ms);
        }
        Ok(())
    }

    /// Releases the enhancer, then the CUDA objects.
    fn release_gpu(&self) {
        let mut st = lock(&self.stream);
        st.release_instance();
        let gpu = st.gpu.take();
        drop(st);
        if gpu.is_some() {
            drop(gpu);
            gst::info!(
                CAT,
                imp = self,
                "enhancer unloaded and CUDA resources released"
            );
        }
    }
}

impl ObjectImpl for ObcEnhance {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("scale")
                    .nick("Scale")
                    .blurb("The integer upscale factor (set before the element starts)")
                    .minimum(MIN_SCALE)
                    .maximum(MAX_SCALE)
                    .default_value(MIN_SCALE)
                    .build(),
                glib::ParamSpecString::builder("status")
                    .nick("Status")
                    .blurb("loading, ready or failed: <reason>")
                    .read_only()
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        if pspec.name() == "scale" {
            let k = value.get::<u32>().expect("type checked upstream");
            self.shared.scale.store(k, Ordering::Relaxed);
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "scale" => self.scale().to_value(),
            "status" => self.status().to_string().to_value(),
            _ => unreachable!("no other property"),
        }
    }
}

impl GstObjectImpl for ObcEnhance {}

impl ElementImpl for ObcEnhance {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "omarchy-broadcast enhance",
                "Filter/Converter/Video/Scaler/Hardware",
                "Upscales low-resolution NV12 or I420 CUDA frames by an integer factor through \
                 an enhancer; passes them through after a failure",
                "Raul Dias",
            )
        });
        Some(&*META)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("video/x-raw")
                .features(["memory:CUDAMemory"])
                .field("format", gst::List::new(["NV12", "I420"]))
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

impl BaseTransformImpl for ObcEnhance {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.shared.failed.store(false, Ordering::SeqCst);
        self.set_status(EffectStatus::Loading);
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.release_gpu();
        let mut st = lock(&self.stream);
        st.in_info = None;
        st.out_info = None;
        st.out_caps = None;
        Ok(())
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let other = if self.shared.failed.load(Ordering::SeqCst) {
            caps.clone()
        } else {
            scaled_caps(direction, caps, self.scale())
        };
        Some(match filter {
            Some(f) => f.intersect_with_mode(&other, gst::CapsIntersectMode::First),
            None => other,
        })
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let in_info = VideoInfo::from_caps(incaps)
            .map_err(|e| gst::loggable_error!(CAT, "caps {incaps}: {e}"))?;
        let out_info = VideoInfo::from_caps(outcaps)
            .map_err(|e| gst::loggable_error!(CAT, "caps {outcaps}: {e}"))?;
        let mut st = lock(&self.stream);
        if st.out_caps.as_ref() != Some(outcaps)
            && let Some(pool) = st.gpu.as_mut().and_then(|g| g.pool.take())
        {
            let _ = pool.set_active(false);
        }
        st.in_info = Some(in_info);
        st.out_info = Some(out_info);
        st.out_caps = Some(outcaps.clone());
        Ok(())
    }

    fn propose_allocation(
        &self,
        decide_query: Option<&gst::query::Allocation>,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        if decide_query.is_none() {
            // Passthrough: downstream answers.
            return self.parent_propose_allocation(decide_query, query);
        }
        // The element reads the video meta's layout and the crop region,
        // so `videocrop` attaches the region instead of copying.
        query.add_allocation_meta::<gst_video::VideoMeta>(None);
        query.add_allocation_meta::<gst_video::VideoCropMeta>(None);
        Ok(())
    }

    fn decide_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        // The element allocates from its own pool: drop downstream's.
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
        if self.shared.failed.load(Ordering::SeqCst) {
            self.shared.passthrough.fetch_add(1, Ordering::Relaxed);
            return self.parent_submit_input_buffer(is_discont, inbuf);
        }
        let mut st = lock(&self.stream);
        match self.ensure_loaded(&mut st, &inbuf) {
            Ok(_) => {
                drop(st);
                self.parent_submit_input_buffer(is_discont, inbuf)
            }
            Err(reason) => {
                let status = self.fail(&mut st, format!("the enhancer could not load: {reason}"));
                drop(st);
                self.set_status(status);
                Ok(gst_base::BASE_TRANSFORM_FLOW_DROPPED)
            }
        }
    }

    fn prepare_output_buffer(
        &self,
        inbuf: InputBuffer,
    ) -> Result<PrepareOutputBufferSuccess, gst::FlowError> {
        let inbuf = match inbuf {
            InputBuffer::Readable(b) => b,
            InputBuffer::Writable(b) => &*b,
        };
        if self.obj().is_passthrough() {
            return Ok(PrepareOutputBufferSuccess::InputBuffer);
        }
        let st = lock(&self.stream);
        let Some(pool) = st.gpu.as_ref().and_then(|g| g.pool.as_ref()) else {
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["the enhance stage has no CUDA buffer pool"]
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
        // The output is the enhanced region itself.
        if let Some(m) = outref.meta_mut::<gst_video::VideoCropMeta>() {
            let _ = m.remove();
        }
        Ok(PrepareOutputBufferSuccess::Buffer(out))
    }

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut st = lock(&self.stream);
        match self.render(&mut st, inbuf, outbuf) {
            Ok(()) => {
                drop(st);
                self.shared.enhanced.fetch_add(1, Ordering::Relaxed);
                self.set_status(EffectStatus::Ready);
                Ok(gst::FlowSuccess::Ok)
            }
            Err(reason) => {
                let status = self.fail(&mut st, format!("enhancing a frame failed: {reason}"));
                drop(st);
                self.set_status(status);
                Ok(gst_base::BASE_TRANSFORM_FLOW_DROPPED)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(s: &str) -> gst::Caps {
        s.parse().unwrap()
    }

    #[test]
    fn the_source_caps_are_k_times_the_sink_caps_in_nv12() {
        gst::init().unwrap();
        let sink = caps(
            "video/x-raw(memory:CUDAMemory), format=I420, width=640, height=360, \
             framerate=30/1, colorimetry=1:4:0:0",
        );
        let src = scaled_caps(gst::PadDirection::Sink, &sink, 3);
        assert_eq!(
            src,
            caps(
                "video/x-raw(memory:CUDAMemory), format=NV12, width=1920, height=1080, \
                 framerate=30/1, colorimetry=1:4:0:0"
            )
        );
        // Back: either input format, the size divided.
        let back = scaled_caps(gst::PadDirection::Src, &src, 3);
        let s = back.structure(0).unwrap();
        assert_eq!(s.get::<i32>("width").unwrap(), 640);
        assert_eq!(s.get::<i32>("height").unwrap(), 360);
        assert!(back.can_intersect(&sink));
        // A size that does not divide has no input.
        assert!(
            scaled_caps(
                gst::PadDirection::Src,
                &caps("video/x-raw, width=1921, height=1080"),
                2
            )
            .is_empty()
        );
    }

    #[test]
    fn ranges_and_lists_scale_too() {
        gst::init().unwrap();
        let src = scaled_caps(
            gst::PadDirection::Sink,
            &caps("video/x-raw, width=[1, 2147483647], height={ 270, 360 }"),
            4,
        );
        let s = src.structure(0).unwrap();
        let w = s.get::<gst::IntRange<i32>>("width").unwrap();
        assert_eq!((w.min(), w.max()), (4, i32::MAX));
        let h = s.get::<gst::List>("height").unwrap();
        let h: Vec<i32> = h.iter().map(|v| v.get::<i32>().unwrap()).collect();
        assert_eq!(h, vec![1080, 1440]);
        let sink = scaled_caps(
            gst::PadDirection::Src,
            &caps("video/x-raw, width=[1, 2147483647], height=[3, 7]"),
            2,
        );
        let s = sink.structure(0).unwrap();
        let w = s.get::<gst::IntRange<i32>>("width").unwrap();
        assert_eq!((w.min(), w.max()), (1, i32::MAX / 2));
        assert_eq!(
            s.get::<gst::IntRange<i32>>("height")
                .map(|r| (r.min(), r.max())),
            Ok((2, 3))
        );
    }

    /// The siting comes from the caps: `jpegdec`'s `jpeg` is centred, caps
    /// without the field are MPEG-2.
    #[test]
    fn the_chroma_siting_follows_the_caps() {
        gst::init().unwrap();
        let site = |extra: &str| {
            let c = caps(&format!(
                "video/x-raw, format=I420, width=640, height=360, framerate=30/1, \
                 colorimetry=1:4:0:0{extra}"
            ));
            siting_of(VideoInfo::from_caps(&c).unwrap().chroma_site())
        };
        assert_eq!(site(", chroma-site=jpeg"), ChromaSiting::Interstitial);
        assert_eq!(site(", chroma-site=mpeg2"), ChromaSiting::Cosited);
        assert_eq!(site(", chroma-site=cosited"), ChromaSiting::TopLeft);
        // Caps without the field: as GStreamer reads them (centred), so the
        // enhancer and the scaler after it agree.
        assert_eq!(site(""), ChromaSiting::Interstitial);
        assert_eq!(
            siting_of(gst_video::VideoChromaSite::empty()),
            ChromaSiting::Cosited
        );
        // The source caps keep the field, so the output is written with it.
        let src = scaled_caps(
            gst::PadDirection::Sink,
            &caps(
                "video/x-raw, format=I420, width=640, height=360, framerate=30/1, \
                 chroma-site=jpeg",
            ),
            3,
        );
        assert_eq!(
            src.structure(0).unwrap().get::<&str>("chroma-site"),
            Ok("jpeg")
        );
    }

    #[test]
    fn colour_is_bt601_or_bt709_full_or_limited() {
        gst::init().unwrap();
        let c = |s: &str| colour_of(&s.parse().unwrap());
        assert_eq!(c("2:4:5:1").unwrap(), ColourSpec::LIMITED_BT601);
        assert_eq!(c("1:4:0:0").unwrap(), ColourSpec::FULL_BT601);
        assert!(c("bt2020").is_err());
    }
}
