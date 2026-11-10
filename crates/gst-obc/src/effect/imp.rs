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
//!   process a frame, and only when `Control::loaded_kind` says a chain
//!   usable for the request is loaded ([`Control::usable`]), so a cold load
//!   on the worker never blocks it; a switch on the loaded instance blocks
//!   it for the switch itself: milliseconds between two effects, the
//!   denoise stage's load when a switch adds it (see below).
//!
//! # The denoise stage
//!
//! The denoise stage (`docs/adr/0018-video-noise-removal.md`) lives in the
//! same backend instance as the effect: one load request carries both
//! (`Stage::Denoise` in `LoadRequest::stages`), and it unloads with the effect, so it adds no
//! teardown path and nothing of it runs at idle. It hides nothing, so the
//! privacy rules of the effect are unchanged by it:
//!
//! * `none` with denoise passes the input through until the stage is
//!   loaded, then processes every frame. Without denoise `none` makes no
//!   CUDA call, as before.
//! * From `none` (with or without denoise) to blur or replace, the green
//!   screen loads cold: the denoise stage alone does not count as loaded for
//!   an effect, so the frames are black until the effect is ready and the
//!   streaming thread never waits for that load (and under the `Engine` lock
//!   a frame decided for an effect is black if only the stage is loaded).
//! * Every other change (turning denoise on or off under an effect, blur
//!   and replace with denoise, an effect to `none` with denoise) is a switch
//!   on the loaded instance, done under the `Engine` lock as for blur to
//!   replace. The streaming thread waits for it there; when the switch adds
//!   the denoise stage under blur or replace, or goes from an effect to
//!   `none` with denoise, that wait is the stage's load. No frame is output
//!   meanwhile, so the last one shown is the previous chain's processed
//!   frame, never the input; the next is the new chain's.
//! * **A denoise failure drops the stage, never the effect.** The effective
//!   request is "denoise requested and no denoise failure recorded"
//!   (`StageCtl::failure` of `Control::stages`). A load that fails with
//!   `BackendError::Stage` for `Stage::Denoise`, or any load failure while
//!   the stage is the
//!   only one (`none`), records the failure; the worker then loads the
//!   effect alone in the same job (or unloads, for `none`). A frame that
//!   fails with `BackendError::Stage` for the stage (or any frame of the
//!   stage alone)
//!   records it too, whichever request the frame was decided for, as long
//!   as the current one still asks for the stage: the streaming thread
//!   marks the loaded chain as without the stage and starts a new request,
//!   which the worker finds already loaded (or unloads, for `none`). Under
//!   blur or replace the backend has dropped the stage, so the frame runs
//!   once more through the chain without it and is output processed; only
//!   if that fails too does the effect's failure path apply (black or the
//!   input, per `on-failure`). With `none` the frame is the input. The
//!   effect's status never becomes failed for the stage. The failure is
//!   forgotten when the denoise request changes or on a retry (`apply`
//!   with `retry`), never otherwise.
//! * The denoise status (`StageCtl::status`) is separate from the
//!   effect's: off (not requested), loading, ready, or failed with the
//!   reason. A failure of the whole chain (the effect's load or frame) shows
//!   as failed there too, without being recorded as a denoise failure.
//!
//! # The relight stage
//!
//! The relight stage (`Stage::Relight`, after denoise and the mask) follows
//! every rule above, with its own failure, status, request and counters
//! (`Control::stages` holds one `StageCtl` per stage). Its request is "on,
//! with this light": a new light is a new request and, on the loaded chain,
//! a switch that loads no model, so the streaming thread waits only for
//! the switch itself; its direction and gain travel with each frame
//! (`FrameParams`) and change nothing that is loaded. Where the stages
//! differ from one stage alone:
//!
//! * a load failure of one stage drops that stage and the job loads the
//!   rest; any other load failure with `none` drops every requested stage;
//! * a frame failure of one stage under blur or replace drops it and runs
//!   the frame again without it, as often as stages fail; under `none` the
//!   frame is the input (any failure there that names no stage drops every
//!   stage), and a stage left loaded runs from the next frame.
//!
//! # Timings
//!
//! `gpu_ms` is measured with two CUDA events on the element's stream around
//! the backend's calls for the frame (every call when a frame runs again
//! without a stage), not around a temporal reset. `denoise_ms` and
//! `relight_ms` are the backend's `timings` for the frame just processed,
//! read after the
//! frame's stream synchronisation (a backend reports a frame only once its
//! work has completed), under a `try_lock` of `Engine` so the streaming
//! thread never waits for a load for it: while the worker holds the lock,
//! that frame's figures are skipped.
//!
//! # A new source
//!
//! The output clock marks the first fresh frame of each new source with
//! [`NEW_SOURCE_FLAG`](super::NEW_SOURCE_FLAG). The element then asks the
//! backend to forget its temporal state (`InferenceBackend::reset_temporal`)
//! right before the next frame it processes, so the denoise stage does not
//! blend frames of two cameras. A stage whose state cannot be reset in place
//! (Maxine's relighting) is dropped by the backend instead and loaded again
//! fresh by the worker (a new request, the stage loading meanwhile): the
//! frames until it is ready run without it, the effect alone under blur or
//! replace (never the input), the input with the stages alone gone, and no
//! frame is relit by the instance that saw the previous source, so camera
//! A's person never shows in camera B's frames. A flagged frame that is not
//! processed (passed through, or black) keeps the reset pending for the
//! next processed one.
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
    BackendError, BackendFactory, ColourMatrix, ColourSpec, DeviceNv12, EffectKind, FrameParams,
    GpuContext, InferenceBackend, LoadReport, LoadRequest, RelightParams, Stage, StageTimings,
};
use obc_gpu::{ContextGuard, CuContext, CuStream, Event};

use super::{STATUS_MESSAGE, has_new_source_flag};
use crate::background;
use crate::cuda::{self, CudaMemoryHead, MAP_CUDA};
use crate::settings::{
    EffectFallback, EffectSettings, EffectStats, EffectStatus, FrameAction, RelightLight, Target,
    decide, parse_effect, percentile,
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
    /// What the settings need loaded (the requested denoise included), and
    /// the number of the request for it.
    target: Target,
    generation: u64,
    status: EffectStatus,
    /// Each stage's failure and status, in [`Stage::ALL`] order
    /// ([`stage_index`]).
    stages: [StageCtl; Stage::ALL.len()],
    /// The effect of `Engine::loaded` (`EffectKind::None`: the stages
    /// alone), as last published by the worker (cleared before an
    /// unload, or a reload during which the streaming thread must not wait,
    /// starts).
    loaded_kind: Option<EffectKind>,
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
            stages: Default::default(),
            loaded_kind: None,
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

/// What the element knows of one stage.
#[derive(Debug, Clone, Default)]
struct StageCtl {
    /// Why the stage was dropped; while set, the stage is not loaded
    /// although requested (see the module docs).
    failure: Option<String>,
    status: EffectStatus,
}

/// The position of `stage` in [`Stage::ALL`] (and in `Control::stages`).
fn stage_index(stage: Stage) -> usize {
    Stage::ALL
        .iter()
        .position(|&s| s == stage)
        .expect("every stage is in Stage::ALL")
}

impl Control {
    fn stage(&self, stage: Stage) -> &StageCtl {
        &self.stages[stage_index(stage)]
    }

    fn stage_mut(&mut self, stage: Stage) -> &mut StageCtl {
        &mut self.stages[stage_index(stage)]
    }

    /// Whether `stage` is requested and has not failed.
    fn effective(&self, stage: Stage) -> bool {
        self.target.wants(stage) && self.stage(stage).failure.is_none()
    }

    /// Whether any stage is requested and has not failed.
    fn any_effective(&self) -> bool {
        Stage::ALL.iter().any(|&s| self.effective(s))
    }

    /// Whether a requested stage has a failure recorded.
    fn any_stage_failed(&self) -> bool {
        self.stages.iter().any(|s| s.failure.is_some())
    }

    /// What the worker loads for the request: the target without the
    /// stages that failed.
    fn job_target(&self) -> Target {
        Stage::ALL.iter().fold(self.target.clone(), |t, &s| {
            if self.effective(s) { t } else { t.without(s) }
        })
    }

    /// Whether the loaded chain can process frames of the request: any
    /// chain for `none` (with a stage), one with an effect for blur and
    /// replace (the stages alone hide nothing).
    fn usable(&self) -> bool {
        match self.loaded_kind {
            None => false,
            Some(EffectKind::None) => self.target.kind == EffectKind::None,
            Some(_) => true,
        }
    }

    /// The effect status of a new request.
    fn pending_status(&self) -> EffectStatus {
        match self.target.kind {
            EffectKind::None => EffectStatus::Off,
            _ => EffectStatus::Loading,
        }
    }

    /// Sets every stage's status for a new request: off when not
    /// requested, failed while its failure is recorded, else loading.
    fn set_pending_stage_statuses(&mut self) {
        self.set_switch_stage_statuses(None);
    }

    /// [`Self::set_pending_stage_statuses`] for a new request that replaces
    /// `prev` on the loaded chain (`None`: a reload of everything), except
    /// that a ready stage whose request is the same in both stays ready: the
    /// switch keeps it loaded as it is (only the stage that changed loads,
    /// rebinds or goes), and showing it loading again would read as a reload
    /// of its model.
    fn set_switch_stage_statuses(&mut self, prev: Option<&Target>) {
        for s in Stage::ALL {
            let kept = prev.is_some_and(|p| p.stage_request(s) == self.target.stage_request(s))
                && self.stage(s).status == EffectStatus::Ready;
            let status = match (&self.stage(s).failure, self.target.wants(s)) {
                (_, false) => EffectStatus::Off,
                (Some(r), true) => EffectStatus::Failed(r.clone()),
                (None, true) if kept => EffectStatus::Ready,
                (None, true) => EffectStatus::Loading,
            };
            self.stage_mut(s).status = status;
        }
    }

    /// Records a failure of `stage`, unless the request no longer asks for
    /// the stage, one is already recorded, or (with `generation`) request
    /// `generation` is superseded. `reconcile` starts a new request so the
    /// worker brings what is loaded in line with it.
    fn record_stage_failure(
        &mut self,
        stage: Stage,
        generation: Option<u64>,
        reason: String,
        reconcile: bool,
    ) -> bool {
        if generation.is_some_and(|g| g != self.generation)
            || !self.target.wants(stage)
            || self.stage(stage).failure.is_some()
        {
            return false;
        }
        let st = self.stage_mut(stage);
        st.status = EffectStatus::Failed(reason.clone());
        st.failure = Some(reason);
        if reconcile {
            self.generation += 1;
        }
        true
    }

    fn report(&self) -> Report {
        Report {
            kind: self.settings.kind,
            status: self.status.clone(),
            stages: self.stages.iter().map(|s| s.status.clone()).collect(),
        }
    }
}

/// What the status message carries.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    kind: EffectKind,
    status: EffectStatus,
    /// Each stage's status, in [`Stage::ALL`] order.
    stages: Vec<EffectStatus>,
}

impl Report {
    /// `Some(now)` when `now` differs from `self`: a message to post.
    fn changed(&self, now: Report) -> Option<Report> {
        (now != *self).then_some(now)
    }

    fn stage(&self, stage: Stage) -> &EffectStatus {
        &self.stages[stage_index(stage)]
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
    denoised: AtomicU64,
    relit: AtomicU64,
    cuda_active: AtomicBool,
    gpu_ms: Mutex<VecDeque<f32>>,
    denoise_ms: Mutex<VecDeque<f32>>,
    relight_ms: Mutex<VecDeque<f32>>,
}

/// Appends `v` to a window of at most [`EffectStats::WINDOW`] values.
fn push_window(w: &Mutex<VecDeque<f32>>, v: f32) {
    let mut w = lock(w);
    if w.len() == EffectStats::WINDOW {
        w.pop_front();
    }
    w.push_back(v);
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
    kind: EffectKind,
    strength: f32,
    relight: RelightParams,
    fallback: EffectFallback,
    /// A frame of a new source arrived
    /// ([`NEW_SOURCE_FLAG`](super::NEW_SOURCE_FLAG)): reset the backend's
    /// temporal state before the next processed frame.
    reset_pending: bool,
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
fn post_status(element: &super::ObcEffect, r: &Report) {
    let s = gst::Structure::builder(STATUS_MESSAGE)
        .field("effect", r.kind.as_str())
        .field("status", r.status.as_str())
        .field("reason", r.status.reason().unwrap_or(""))
        .field("denoise", r.stage(Stage::Denoise).as_str())
        .field(
            "denoise-reason",
            r.stage(Stage::Denoise).reason().unwrap_or(""),
        )
        .field("relight", r.stage(Stage::Relight).as_str())
        .field(
            "relight-reason",
            r.stage(Stage::Relight).reason().unwrap_or(""),
        )
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

    pub(super) fn stage_status(&self, stage: Stage) -> EffectStatus {
        lock(&self.shared.control).stage(stage).status.clone()
    }

    pub(super) fn stats(&self) -> EffectStats {
        let (effect, status, denoise, relight, last_load_ms, last_load_reused, loads) = {
            let c = lock(&self.shared.control);
            (
                c.settings.kind,
                c.status.clone(),
                c.stage(Stage::Denoise).status.clone(),
                c.stage(Stage::Relight).status.clone(),
                c.last_load_ms,
                c.last_load_reused,
                c.loads,
            )
        };
        let window = |w: &Mutex<VecDeque<f32>>| -> Vec<f32> { lock(w).iter().copied().collect() };
        let gpu_window = window(&self.shared.gpu_ms);
        let denoise_window = window(&self.shared.denoise_ms);
        let relight_window = window(&self.shared.relight_ms);
        let s = &self.shared;
        EffectStats {
            effect,
            status,
            denoise,
            relight,
            frames_processed: s.processed.load(Ordering::Relaxed),
            frames_black: s.black.load(Ordering::Relaxed),
            frames_passthrough: s.passthrough.load(Ordering::Relaxed),
            frames_denoised: s.denoised.load(Ordering::Relaxed),
            frames_relit: s.relit.load(Ordering::Relaxed),
            gpu_ms_p50: percentile(&gpu_window, 50.0),
            gpu_ms_p95: percentile(&gpu_window, 95.0),
            denoise_ms_p50: percentile(&denoise_window, 50.0),
            denoise_ms_p95: percentile(&denoise_window, 95.0),
            relight_ms_p50: percentile(&relight_window, 50.0),
            relight_ms_p95: percentile(&relight_window, 95.0),
            last_load_ms,
            last_load_reused,
            loads,
            cuda_active: s.cuda_active.load(Ordering::Relaxed),
        }
    }

    /// Applies new settings. A change of what has to be loaded (the effect,
    /// the image of replace, a stage request, or the relighting light) is a
    /// new request for the worker; `retry` makes one after a failure (of
    /// the effect or of a stage) even when nothing changed. A stage's
    /// recorded failure is forgotten when its request (for relight: on or
    /// off, or its light) changes or on a retry. The effect's status turns
    /// loading (off for `none`) when the effect changes or had failed; a
    /// change of a stage request alone leaves it as it is. Likewise a ready
    /// stage whose own request is unchanged stays ready: the switch keeps
    /// it loaded, and only the stage that changed shows loading (its model
    /// load, or the rebind of a new light). A change of the
    /// per-frame parameters alone (blur strength, relighting direction and
    /// gain) is never a retry: they load nothing, and a slider moved after
    /// a failure would otherwise reload what failed on every step.
    pub(super) fn apply(&self, new: EffectSettings, retry: bool) {
        let posted = {
            let mut c = lock(&self.shared.control);
            let retry = retry && !c.settings.changes_frame_params_only(&new);
            let before = c.report();
            let target = new.target();
            let effect_failed = matches!(c.status, EffectStatus::Failed(_));
            let stage_failed = c.any_stage_failed();
            let effect_changed =
                (target.kind, &target.background) != (c.target.kind, &c.target.background);
            c.settings = new;
            if target != c.target || (retry && (effect_failed || stage_failed)) {
                for s in Stage::ALL {
                    if retry || target.stage_request(s) != c.target.stage_request(s) {
                        c.stage_mut(s).failure = None;
                    }
                }
                let prev = std::mem::replace(&mut c.target, target);
                c.generation += 1;
                if effect_changed || effect_failed {
                    c.status = c.pending_status();
                }
                c.set_switch_stage_statuses(Some(&prev));
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::debug!(
                CAT,
                imp = self,
                "effect {}: {}, denoise {}, relight {}",
                r.kind,
                r.status,
                r.stage(Stage::Denoise),
                r.stage(Stage::Relight)
            );
            post_status(&self.obj(), &r);
        }
    }

    /// Marks request `generation` as failed (unless superseded), and posts.
    /// A requested stage that has not failed itself shows the failure too:
    /// it runs in the failed chain.
    fn fail(&self, generation: u64, reason: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            if c.generation != generation || matches!(c.status, EffectStatus::Failed(_)) {
                None
            } else {
                let before = c.report();
                for s in Stage::ALL {
                    if c.effective(s) {
                        c.stage_mut(s).status = EffectStatus::Failed(reason.clone());
                    }
                }
                c.status = EffectStatus::Failed(reason);
                before.changed(c.report())
            }
        };
        if let Some(r) = posted {
            gst::warning!(CAT, imp = self, "effect {}: {}", r.kind, r.status);
            post_status(&self.obj(), &r);
        }
    }

    /// Records a failure of `stage` (of request `generation`, unless
    /// superseded; with `None`, of whatever request still asks for the
    /// stage) and starts a new request, so the worker drops the stage (it
    /// finds the rest loaded, or unloads for `none` without another stage);
    /// posts. The effect's status is not touched. A failed effect starts no
    /// new request: it processes nothing until one comes (a retry or a
    /// change), which reconciles.
    fn fail_stage(&self, stage: Stage, generation: Option<u64>, reason: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            let before = c.report();
            let reconcile = !matches!(c.status, EffectStatus::Failed(_));
            if c.record_stage_failure(stage, generation, reason, reconcile) && reconcile {
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::warning!(CAT, imp = self, "{stage}: {}", r.stage(stage));
            post_status(&self.obj(), &r);
        }
    }

    /// Starts a new request so the worker loads `stages` again: the
    /// backend dropped them on a temporal reset (not a failure), so the
    /// chain loaded is without them. Each one still requested and not
    /// failed shows loading until the worker has it ready again; posts.
    fn rebuild_stages(&self, stages: &[Stage]) {
        let posted = {
            let mut c = lock(&self.shared.control);
            let before = c.report();
            let mut wanted = false;
            for &s in stages {
                if c.effective(s) {
                    c.stage_mut(s).status = EffectStatus::Loading;
                    wanted = true;
                }
            }
            if wanted {
                c.generation += 1;
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::debug!(
                CAT,
                imp = self,
                "a new source: loading again {stages:?}, relight {}",
                r.stage(Stage::Relight)
            );
            post_status(&self.obj(), &r);
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
        c.loaded_kind = None;
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

    /// The stage times of the frame just processed and synchronised (see
    /// "Timings" in the module docs), if the backend has them. Never waits
    /// for the `Engine` lock: if the worker holds it (a load, a switch or an
    /// unload under way), this frame's figures are skipped.
    fn stage_times(&self) -> Option<StageTimings> {
        let e = match self.shared.engine.try_lock() {
            Ok(e) => e,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        e.instance.as_ref()?.timings()
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
        // The stages that ran on the processed frame.
        let mut ran = (false, false);
        // The input copied into the output (a failed frame with nothing to
        // hide, or on-failure=none).
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
                let params = st.relight.frame_params(FrameParams {
                    blur_strength: st.strength,
                    ..FrameParams::default()
                });
                let ctx = gpu.gpu_context();
                // The stages alone hide nothing: an effect frame needs a
                // chain with the effect. A frame of `none` needs a chain
                // with something in it (a stage, or the previous effect
                // while switching).
                let alone = st.kind == EffectKind::None;
                // The stages that failed on this frame (the backend has
                // dropped them), whatever the frame then got.
                let mut stage_failures: Vec<(Stage, String)> = Vec::new();
                // The stages the temporal reset dropped, to load again.
                let mut rebuild: Vec<Stage> = Vec::new();
                let mut e = lock(&self.shared.engine);
                let Engine { instance, loaded } = &mut *e;
                let usable = loaded.as_ref().is_some_and(|k| {
                    k.target.kind != EffectKind::None || (alone && k.target.has_stages())
                });
                let result = match (instance.as_mut(), usable) {
                    (Some(instance), true) => 'frame: {
                        // Before the start event: the reset may wait for
                        // the stream, which is not this frame's GPU time.
                        let reset = if std::mem::take(&mut st.reset_pending) {
                            instance.reset_temporal(&ctx)
                        } else {
                            Ok(Default::default())
                        };
                        // A stage whose state the backend cannot forget
                        // (Maxine's relighting) is dropped by the reset
                        // rather than carry the previous source's person
                        // into this one. The chain in force is without it
                        // from this frame, and a new request below has the
                        // worker load it again, fresh: until it is ready
                        // the frames run without it (under blur or replace
                        // the effect alone, never the input), and no frame
                        // is relit by the instance that saw the previous
                        // source.
                        let reset = reset.map(|dropped| {
                            if let Some(k) = loaded.as_mut() {
                                for &s in dropped.as_slice() {
                                    if k.target.wants(s) {
                                        k.target = k.target.without(s);
                                        rebuild.push(s);
                                    }
                                }
                            }
                        });
                        // The stages alone may have gone with it: nothing
                        // left to run (the input, below).
                        if reset.is_ok()
                            && alone
                            && !loaded.as_ref().is_some_and(|k| k.target.has_stages())
                        {
                            break 'frame None;
                        }
                        // SAFETY: our own stream.
                        let _ = unsafe { gpu.start.record(stream) };
                        let mut r =
                            reset.and_then(|()| instance.process(&ctx, &params, &src, &dst));
                        // A stage failure: the backend has dropped the stage
                        // (and any failure of the stages alone drops them
                        // all), so the chain loaded from now on is without
                        // it. An effect is fine without it: the frame runs
                        // again through the chain without it, so it is still
                        // processed (never the input, nor black, for a
                        // healthy effect), as often as stages fail. Without
                        // an effect the frame is the input.
                        while let Err(err) = &r {
                            let failed: Vec<Stage> = match stage_of(err) {
                                Some(s) => vec![s],
                                None if alone => Stage::ALL.to_vec(),
                                None => break,
                            };
                            let Some(k) = loaded.as_mut() else { break };
                            let failed: Vec<Stage> =
                                failed.into_iter().filter(|&s| k.target.wants(s)).collect();
                            if failed.is_empty() {
                                break;
                            }
                            for &s in &failed {
                                k.target = k.target.without(s);
                                stage_failures
                                    .push((s, format!("the {s} stage failed on a frame: {err}")));
                            }
                            if alone {
                                break;
                            }
                            r = instance.process(&ctx, &params, &src, &dst);
                        }
                        if r.is_ok()
                            && let Some(k) = loaded.as_ref()
                        {
                            ran = (k.target.denoise, k.target.relight);
                        }
                        // SAFETY: our own stream.
                        let _ = unsafe { gpu.end.record(stream) };
                        Some(r)
                    }
                    _ => None,
                };
                drop(e);
                if !rebuild.is_empty() {
                    self.rebuild_stages(&rebuild);
                }
                // The effect failed (alone, or again without the stages):
                // recorded first, so the stage failures below do not
                // supersede it.
                if !alone && let Some(Err(err)) = &result {
                    self.fail(st.generation, format!("processing a frame failed: {err}"));
                }
                for (stage, reason) in stage_failures {
                    // Recorded whichever request the frame was decided for:
                    // the stage is gone from the loaded chain, and a request
                    // that still asks for it must learn it now (the first
                    // frame after turning the stage on was decided under the
                    // previous request, and the worker has already handled
                    // the new one).
                    self.fail_stage(stage, None, reason);
                }
                let copy = |src: &DeviceNv12, dst: &DeviceNv12| {
                    // SAFETY: both are the mapped CUDA frames of this call,
                    // in our context, same size; the input is only read.
                    unsafe { obc_gpu::copy_nv12_async(src, dst, stream) }.map_err(|e| e.to_string())
                };
                match result {
                    Some(Ok(())) => processed = true,
                    // The stages alone failed (recorded above): nothing to
                    // hide, the input.
                    Some(Err(_)) if alone => {
                        copy(&src, &dst)?;
                        copied = true;
                    }
                    // The effect failed (recorded above).
                    Some(Err(_)) => match st.fallback {
                        EffectFallback::Black => fill_black(&dst)?,
                        EffectFallback::None => {
                            copy(&src, &dst)?;
                            copied = true;
                        }
                    },
                    // Unloaded since the decision: the input for `none`
                    // (nothing to hide), black for an effect, never the
                    // input.
                    None if alone => {
                        copy(&src, &dst)?;
                        copied = true;
                    }
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
            let (denoised, relit) = ran;
            if denoised {
                self.shared.denoised.fetch_add(1, Ordering::Relaxed);
            }
            if relit {
                self.shared.relit.fetch_add(1, Ordering::Relaxed);
            }
            if (denoised || relit)
                && let Some(t) = self.stage_times()
            {
                if denoised && t.denoise_ms > 0.0 {
                    push_window(&self.shared.denoise_ms, t.denoise_ms);
                }
                if relit && t.relight_ms > 0.0 {
                    push_window(&self.shared.relight_ms, t.relight_ms);
                }
            }
            if let Ok(ms) = Event::elapsed_ms(&gpu.start, &gpu.end) {
                push_window(&self.shared.gpu_ms, ms);
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
    /// What to load: the request without the stages that failed
    /// ([`Control::job_target`]).
    target: Target,
    gpu: GpuContext,
    format: Format,
    factory: Option<Arc<dyn BackendFactory>>,
}

/// Why a load failed; the backend's error, when it came from the backend,
/// tells a failure of one stage alone from one of the chain.
struct Failure {
    reason: String,
    error: Option<BackendError>,
}

impl Failure {
    fn new(reason: impl Into<String>) -> Failure {
        Failure {
            reason: reason.into(),
            error: None,
        }
    }

    fn stage(&self) -> Option<Stage> {
        self.error.as_ref().and_then(stage_of)
    }
}

/// The stage `err` is the failure of alone, the rest of the chain intact.
fn stage_of(err: &BackendError) -> Option<Stage> {
    match err {
        BackendError::Stage { stage, .. } => Some(*stage),
        _ => None,
    }
}

enum Outcome {
    Unloaded,
    Loaded(LoadReport),
    AlreadyLoaded,
    Failed(Failure),
    Superseded,
}

/// What a job did, and the stage failures it ran into (the job then went
/// on without those stages).
struct JobResult {
    outcome: Outcome,
    stage_failures: Vec<(Stage, String)>,
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
                        target: c.job_target(),
                        gpu,
                        format,
                        factory: c.factory.clone(),
                    };
                }
                c = shared.wake.wait(c).unwrap_or_else(|e| e.into_inner());
            }
        };
        handled = Some(job.generation);
        let JobResult {
            outcome,
            stage_failures,
        } = run_job(&shared, &job);
        let (loaded_kind, in_force) = {
            let e = lock(&shared.engine);
            let k = e.loaded.as_ref();
            (k.map(|k| k.target.kind), k.map(|k| k.target.clone()))
        };
        let in_force_list = in_force
            .as_ref()
            .map(|t| t.stage_list().to_string())
            .unwrap_or_default();
        let in_force = |s: Stage| in_force.as_ref().is_some_and(|t| t.wants(s));
        let posted = {
            let mut c = lock(&shared.control);
            let before = c.report();
            c.loaded_kind = loaded_kind;
            if let Outcome::Loaded(r) = &outcome {
                c.loads += 1;
                c.last_load_ms = Some(r.load_ms + r.warmup_ms.unwrap_or(0.0));
                c.last_load_reused = r.reused_model;
            }
            if c.generation == job.generation {
                for (stage, reason) in &stage_failures {
                    c.record_stage_failure(*stage, Some(job.generation), reason.clone(), false);
                }
                let effect = c.target.kind != EffectKind::None;
                match &outcome {
                    Outcome::Unloaded | Outcome::Superseded => {}
                    Outcome::Loaded(_) | Outcome::AlreadyLoaded => {
                        if effect {
                            c.status = EffectStatus::Ready;
                        }
                        for s in Stage::ALL {
                            if c.effective(s) && in_force(s) {
                                c.stage_mut(s).status = EffectStatus::Ready;
                            }
                        }
                    }
                    Outcome::Failed(f) => {
                        if effect {
                            c.status = EffectStatus::Failed(f.reason.clone());
                        }
                        for s in Stage::ALL {
                            if c.effective(s) {
                                c.stage_mut(s).status = EffectStatus::Failed(f.reason.clone());
                            }
                        }
                    }
                }
                for s in Stage::ALL {
                    if !c.target.wants(s) {
                        c.stage_mut(s).status = EffectStatus::Off;
                    }
                }
            }
            before.changed(c.report())
        };
        if let Some(el) = element.upgrade() {
            match &outcome {
                Outcome::Loaded(r) => gst::info!(
                    CAT,
                    obj = el,
                    "loaded {} with stages {} in {:.1} ms (+{:.1} ms warm-up, model reused: {})",
                    job.target.kind,
                    in_force_list,
                    r.load_ms,
                    r.warmup_ms.unwrap_or(0.0),
                    r.reused_model
                ),
                Outcome::Unloaded => gst::info!(CAT, obj = el, "unloaded"),
                _ => {}
            }
            for (stage, reason) in &stage_failures {
                gst::warning!(CAT, obj = el, "the {stage} stage failed to load: {reason}");
            }
            if let Some(r) = posted {
                post_status(&el, &r);
            }
        }
    }
}

fn superseded(shared: &Shared, generation: u64) -> bool {
    let c = lock(&shared.control);
    c.stop || c.generation != generation
}

/// Brings what is loaded in line with `job`. A failure of one stage
/// (`BackendError::Stage` for it), or any failure while the stages are
/// alone (`none`: then of every one of them), is returned as the job's
/// stage failure, and the job goes on without the stage: the rest of the
/// chain, or an unload for `none` without another stage.
fn run_job(shared: &Shared, job: &Job) -> JobResult {
    let mut target = job.target.clone();
    let mut stage_failures = Vec::new();
    loop {
        if target.kind == EffectKind::None && !target.has_stages() {
            unload(shared, job);
            return JobResult {
                outcome: Outcome::Unloaded,
                stage_failures,
            };
        }
        match load(shared, job, &target) {
            Outcome::Failed(f)
                if target.has_stages()
                    && (target.kind == EffectKind::None
                        || f.stage().is_some_and(|s| target.wants(s))) =>
            {
                let failed: Vec<Stage> = match f.stage() {
                    Some(s) if target.wants(s) => vec![s],
                    _ => Stage::ALL
                        .into_iter()
                        .filter(|&s| target.wants(s))
                        .collect(),
                };
                for s in failed {
                    stage_failures.push((s, f.reason.clone()));
                    target = target.without(s);
                }
            }
            outcome => {
                return JobResult {
                    outcome,
                    stage_failures,
                };
            }
        }
    }
}

/// Unloads the backend instance (keeping it for the next load).
fn unload(shared: &Shared, job: &Job) {
    lock(&shared.control).loaded_kind = None;
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
}

/// Loads `target` (an effect, its stages, or the stages alone) for `job`.
fn load(shared: &Shared, job: &Job, target: &Target) -> Outcome {
    let kind = target.kind;
    let colour = match &job.format.colour {
        Ok(c) => *c,
        Err(reason) => return Outcome::Failed(Failure::new(reason.clone())),
    };
    let key = LoadedKey {
        target: target.clone(),
        width: job.format.width,
        height: job.format.height,
        colour,
    };
    let current = lock(&shared.engine).loaded.clone();
    if current.as_ref() == Some(&key) {
        return Outcome::AlreadyLoaded;
    }
    let background = match (kind, &target.background) {
        (EffectKind::Replace, None) => {
            return Outcome::Failed(Failure::new("the replace effect needs a background image"));
        }
        (EffectKind::Replace, Some(path)) => match decode_background(path, &job.format) {
            Ok(b) => Some(Arc::new(b)),
            Err(reason) => return Outcome::Failed(Failure::new(reason)),
        },
        _ => None,
    };
    if superseded(shared, job.generation) {
        return Outcome::Superseded;
    }
    let Some(factory) = job.factory.clone() else {
        return Outcome::Failed(Failure::new(
            "no inference backend is configured for the effects",
        ));
    };
    // A full load for the backend: nothing loaded, or another size or
    // colour (the instance is left unloaded if it fails).
    let full = current
        .as_ref()
        .is_none_or(|k| (k.width, k.height, k.colour) != (key.width, key.height, key.colour));
    // The effect loads cold after the stages alone.
    let cold_effect = kind != EffectKind::None
        && current
            .as_ref()
            .is_some_and(|k| k.target.kind == EffectKind::None);
    if (full || cold_effect) && current.is_some() {
        // The streaming thread must not wait for this load.
        lock(&shared.control).loaded_kind = None;
    }
    // SAFETY: as for the unload above.
    let _guard = match unsafe { ContextGuard::push(job.gpu.context) } {
        Ok(g) => g,
        Err(e) => {
            return Outcome::Failed(Failure::new(format!(
                "cannot make the CUDA context current: {e}"
            )));
        }
    };
    let mut e = lock(&shared.engine);
    if e.instance.is_none() {
        match factory.create() {
            Ok(i) => e.instance = Some(i),
            Err(err) => {
                return Outcome::Failed(Failure {
                    reason: err.to_string(),
                    error: Some(err),
                });
            }
        }
    }
    let req = LoadRequest {
        effect: kind,
        stages: target.stage_list(),
        width: key.width,
        height: key.height,
        colour,
        background,
        relight_light: target.relight_light,
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
            // A switch that fails keeps the previous chain loaded, less the
            // stage of a stage error (a new light the loaded relight stage
            // could not take drops it: the seam's rule), so that the status
            // never says ready for a stage that is not running.
            if full {
                e.loaded = None;
            } else if let (Some(stage), Some(k)) = (stage_of(&err), e.loaded.as_mut()) {
                k.target = k.target.without(stage);
            }
            Outcome::Failed(Failure {
                reason: err.to_string(),
                error: Some(err),
            })
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
                glib::ParamSpecBoolean::builder("denoise")
                    .nick("Denoise")
                    .blurb(
                        "Video noise removal before the effect, or alone with effect none \
                         (dropped, not failed, if it cannot run). Turned on under blur or \
                         replace, frames wait for the stage's load, the last processed frame \
                         staying shown; the input is never shown",
                    )
                    .default_value(false)
                    .flags(rw)
                    .build(),
                glib::ParamSpecBoolean::builder("relight")
                    .nick("Relight")
                    .blurb(
                        "Relighting of the person, after denoise, under the effect or alone \
                         with effect none (dropped, not failed, if it cannot run). Switched \
                         like denoise",
                    )
                    .default_value(false)
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("relight-light")
                    .nick("Relight light")
                    .blurb("The relighting light: warm | neutral | cool")
                    .default_value(Some(RelightLight::default().as_str()))
                    .flags(rw)
                    .build(),
                glib::ParamSpecDouble::builder("relight-pan")
                    .nick("Relight direction")
                    .blurb(
                        "The relighting light's direction around the person, in degrees \
                         (applies on the next frame)",
                    )
                    .minimum(f64::from(RelightParams::MIN_PAN_DEG))
                    .maximum(f64::from(RelightParams::MAX_PAN_DEG))
                    .default_value(0.0)
                    .flags(rw)
                    .build(),
                glib::ParamSpecDouble::builder("relight-gain")
                    .nick("Relight gain")
                    .blurb(
                        "The relit person's brightness, 1 unchanged (applies on the next \
                         frame)",
                    )
                    .minimum(f64::from(RelightParams::MIN_GAIN))
                    .maximum(f64::from(RelightParams::MAX_GAIN))
                    .default_value(f64::from(RelightParams::DEFAULT_GAIN))
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
            "denoise" => {
                s.denoise = value.get::<bool>().unwrap_or(false);
                true
            }
            "relight" => {
                s.relight = value.get::<bool>().unwrap_or(false);
                true
            }
            "relight-light" => match text().parse::<RelightLight>() {
                Ok(l) => {
                    s.relight_params.light = l;
                    true
                }
                Err(e) => {
                    gst::warning!(CAT, imp = self, "ignoring relight-light: {e}");
                    return;
                }
            },
            "relight-pan" => {
                s.relight_params.pan_deg = value.get::<f64>().unwrap_or(0.0) as f32;
                false
            }
            "relight-gain" => {
                s.relight_params.gain = value.get::<f64>().unwrap_or(1.0) as f32;
                false
            }
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
            "denoise" => s.denoise.to_value(),
            "relight" => s.relight.to_value(),
            "relight-light" => s.relight_params.light.as_str().to_value(),
            "relight-pan" => f64::from(s.relight_params.pan_deg).to_value(),
            "relight-gain" => f64::from(s.relight_params.gain).to_value(),
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
                "Background blur or replacement, and video noise removal, on NV12 CUDA \
                 frames through an inference backend; passthrough without either",
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
            let before = c.report();
            // A restart loads again (and retries a failure of the effect; a
            // stage failure stays recorded until the request changes).
            c.generation += 1;
            c.status = c.pending_status();
            c.set_pending_stage_statuses();
            before.changed(c.report())
        };
        if let Some(r) = posted {
            post_status(&self.obj(), &r);
        }
        let mut st = lock(&self.stream);
        st.action = FrameAction::Passthrough;
        st.reset_pending = false;
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
            let before = c.report();
            if c.format.as_ref() != Some(&format) {
                c.format = Some(format);
                // Loading depends on the size and colour: a new request.
                c.generation += 1;
                c.status = c.pending_status();
                c.set_pending_stage_statuses();
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            post_status(&self.obj(), &r);
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
    /// match, creating the CUDA objects when the frame needs them (or the
    /// stages alone need them loaded: they pass frames through until then).
    fn prepare_frame(&self, inbuf: &gst::BufferRef) {
        let (kind, status, fallback, loaded, stages, strength, relight, generation) = {
            let c = lock(&self.shared.control);
            (
                c.settings.kind,
                c.status.clone(),
                c.settings.fallback,
                c.usable(),
                c.any_effective(),
                c.settings.blur_strength,
                c.settings.relight_params,
                c.generation,
            )
        };
        let mut action = decide(kind, &status, fallback, loaded, stages);
        let needs_gpu = action != FrameAction::Passthrough || (kind == EffectKind::None && stages);
        let mut st = lock(&self.stream);
        if needs_gpu && let Err(reason) = self.ensure_gpu(&mut st, inbuf) {
            drop(st);
            if kind == EffectKind::None {
                // Only the stages needed CUDA: drop them, pass through.
                for s in Stage::ALL {
                    self.fail_stage(s, Some(generation), reason.clone());
                }
                action = FrameAction::Passthrough;
            } else {
                self.fail(generation, reason.clone());
                action = decide(
                    kind,
                    &EffectStatus::Failed(reason),
                    fallback,
                    loaded,
                    stages,
                );
            }
            st = lock(&self.stream);
        }
        if has_new_source_flag(inbuf) {
            st.reset_pending = true;
        }
        st.action = action;
        st.generation = generation;
        st.kind = kind;
        st.strength = strength;
        st.relight = relight;
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
