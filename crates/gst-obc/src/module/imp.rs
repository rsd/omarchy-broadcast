//! The implementation of `obcmodule`.
//!
//! # Threads and locks
//!
//! * The **streaming thread** decides per frame whether it is processed
//!   (`prepare_frame`, in `submit_input_buffer`), switching BaseTransform's
//!   passthrough to match, then processes it in place (`transform_ip`).
//! * The **worker thread** (one per started element) creates, loads,
//!   switches and unloads the chain. It works from the latest request only:
//!   requests are numbered (`Control::generation`) and a superseded one is
//!   dropped.
//! * `Control` (requests, statuses, the CUDA handles once known) and
//!   `Engine` (the chain and what it has loaded) have one mutex each and no
//!   code path holds both. The streaming thread never waits for a load:
//!   while the worker holds `Engine` to load, switch or unload
//!   (`Shared::busy`) the frame passes through (the stages hide nothing),
//!   and it waits a few milliseconds at most for the worker's brief reads
//!   of what is loaded.
//!
//! # Failures
//!
//! A stage's failure is recorded in `StageCtl::failure`; while it is set
//! the stage is not loaded although requested ("effective" means requested
//! and not failed). A load that fails with `BackendError::Stage` for a
//! stage drops that stage and the worker loads the rest in the same job;
//! any other load failure records a failure for every effective stage, and
//! the chain is unloaded by the next job. On a frame, a stage failure
//! drops the stage from what is loaded (the chain has dropped it) and the
//! frame runs again without it; any other failure marks nothing loaded,
//! fails every effective stage and starts a new request, which unloads.
//!
//! The chain writes the frame in place, so a call that fails may leave it
//! partly processed. Before the first call on a frame the element keeps a
//! copy of it as it came in (`RunFrame::save`), and puts the copy back
//! after every failing call (`RunFrame::restore`): the run again without a
//! stage starts from the input, not from what the earlier stages already
//! wrote, and a frame no call processed leaves the element as it came in
//! (fail open, unprocessed). The copy is made on every frame that has a
//! stage to run, since a failure cannot be foreseen: a device-to-device
//! copy on the element's stream for CUDA frames (into a buffer kept for
//! the stream), a `memcpy` for system memory (likewise), and nothing for
//! the host copy of a CUDA frame, which is restored by copying the frame
//! to the host again (only after a failure). A frame without a stage to
//! run is never copied.
//!
//! # A new source
//!
//! A buffer with the new-source flag sets `StreamState::reset_pending`; the
//! next processed frame calls `reset_temporal` first. The stages the chain
//! drops are taken out of what is loaded and a new request has the worker
//! load them again.
//!
//! # A repeat
//!
//! A buffer with the repeat flag (the output clock emitted the last camera
//! frame again) is run through the chain with
//! `StageChain::process_repeat`, so a module of ABI 1.1 sees it as a
//! repeat (`OBC_FRAME_FLAG_REPEAT`); the rest is as for any frame (the
//! copy, the restore, a stage dropped and the frame run again).
//!
//! # CUDA context and stream
//!
//! As in `obceffect`: the context is the one of the first input memory (the
//! stream's shared `GstCudaContext`), the element creates one
//! `GstCudaStream` in it and runs everything there (a CUDA chain is loaded
//! with it, the host copy is made on it), the input is synchronised with
//! `gst_cuda_memory_sync` once mapped, and the element's stream is
//! synchronised before `transform_ip` returns.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::thread::JoinHandle;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::VideoInfo;
use gst_video::prelude::*;
use obc_backend::DeviceNv12;
use obc_backend::{
    BackendError, ChainFrame, ChainRequest, ColourSpec, FrameKind, GpuContext, HostNv12,
    LoadReport, Params, StageChain, StageChainFactory, StageId, StageList, StageValue,
};
use obc_gpu::{ContextGuard, CuContext, CuStream, DeviceNv12Buffer, PinnedHostBuffer};

use super::{ModuleStats, STATUS_MESSAGE, chain_kind};
use crate::cuda::{self, MAP_CUDA};
use crate::effect::{has_new_source_flag, has_repeat_flag};
use crate::settings::{EffectStatus, StageRequest, StageStats};
use crate::video::{CudaFrame, colour_of, input_context};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "obcmodule",
        gst::DebugColorFlags::empty(),
        Some("omarchy-broadcast module stage element"),
    )
});

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The requested stages that are on, in order, with their load-time
/// parameters.
type Target = Vec<(StageId, Params)>;

fn target_of(stages: &[StageRequest]) -> Target {
    stages
        .iter()
        .filter(|s| s.enabled)
        .map(|s| (s.id.clone(), s.params.clone()))
        .collect()
}

fn request_of<'a>(target: &'a Target, stage: &str) -> Option<&'a Params> {
    target.iter().find(|(s, _)| s == stage).map(|(_, p)| p)
}

fn wants(target: &Target, stage: &str) -> bool {
    request_of(target, stage).is_some()
}

/// Whether `new` differs from `old` in the live parameters alone.
fn changes_live_only(old: &[StageRequest], new: &[StageRequest]) -> bool {
    let fixed = |v: &[StageRequest]| {
        v.iter()
            .map(|s| StageRequest {
                live: Params::new(),
                ..s.clone()
            })
            .collect::<Vec<_>>()
    };
    old != new && fixed(old) == fixed(new)
}

/// The negotiated frames.
#[derive(Debug, Clone, PartialEq)]
struct Format {
    width: u32,
    height: u32,
    /// The colour the chain is loaded with, or why the caps' colorimetry
    /// cannot be described.
    colour: Result<ColourSpec, String>,
    /// The frames are in CUDA memory.
    cuda: bool,
}

/// What the chain has loaded.
#[derive(Debug, Clone, PartialEq)]
struct LoadedKey {
    stages: Target,
    width: u32,
    height: u32,
    colour: ColourSpec,
    kind: FrameKind,
}

impl LoadedKey {
    fn without(&mut self, stage: &str) {
        self.stages.retain(|(s, _)| s != stage);
    }
}

#[derive(Default)]
struct Engine {
    chain: Option<Box<dyn StageChain>>,
    loaded: Option<LoadedKey>,
    /// The frame kind the chain was created for.
    kind: Option<FrameKind>,
    /// The CUDA context of a chain on CUDA frames, current around every
    /// call on it.
    context: Option<CuContext>,
}

// SAFETY: `context` is an opaque CUDA driver handle usable from any thread
// (made current per call); the chain is `Send`.
unsafe impl Send for Engine {}

impl Engine {
    /// Unloads and drops the chain, with its context current; a chain whose
    /// context cannot be made current is leaked rather than unloaded in
    /// none (it may hold device objects of that context).
    fn release_chain(&mut self) {
        self.loaded = None;
        let Some(mut chain) = self.chain.take() else {
            return;
        };
        let context = self.context.take();
        self.kind = None;
        let guard = match context {
            // SAFETY: the element keeps the context alive (its reference is
            // released only after the chain).
            Some(ctx) => match unsafe { ContextGuard::push(ctx) } {
                Ok(g) => Some(g),
                Err(e) => {
                    gst::error!(
                        CAT,
                        "cannot push the CUDA context to unload the chain ({e}); leaking it \
                         instead of unloading it outside its context"
                    );
                    std::mem::forget(chain);
                    return;
                }
            },
            None => None,
        };
        chain.unload();
        drop(chain);
        drop(guard);
    }
}

/// What the element knows of one stage.
#[derive(Debug, Clone, Default)]
struct StageCtl {
    /// Why the stage was dropped; while set, the stage is not loaded
    /// although requested.
    failure: Option<String>,
    status: EffectStatus,
}

static OFF: EffectStatus = EffectStatus::Off;

#[derive(Default)]
struct Control {
    stages: Vec<StageRequest>,
    /// What the requests need loaded, and the number of the request.
    target: Target,
    generation: u64,
    /// Every stage the requests have listed, in the order first listed.
    ctl: Vec<(StageId, StageCtl)>,
    /// The live parameters of the stages that are on.
    live: Arc<BTreeMap<StageId, Params>>,
    format: Option<Format>,
    /// The CUDA context and the element's stream, once the streaming
    /// thread has created them.
    gpu: Option<GpuContext>,
    factory: Option<Arc<dyn StageChainFactory>>,
    stop: bool,
    /// A chain with at least one stage is loaded (published by whoever
    /// last changed `Engine::loaded`).
    ready: bool,
    kind: Option<FrameKind>,
    loads: u64,
    last_load_ms: Option<f64>,
    last_load_reused: bool,
    last_error: Option<String>,
}

impl Control {
    fn stage(&self, stage: &str) -> Option<&StageCtl> {
        self.ctl.iter().find(|(s, _)| s == stage).map(|(_, c)| c)
    }

    fn stage_mut(&mut self, stage: &StageId) -> &mut StageCtl {
        let i = match self.ctl.iter().position(|(s, _)| s == stage) {
            Some(i) => i,
            None => {
                self.ctl.push((stage.clone(), StageCtl::default()));
                self.ctl.len() - 1
            }
        };
        &mut self.ctl[i].1
    }

    fn stage_ids(&self) -> Vec<StageId> {
        self.ctl.iter().map(|(s, _)| s.clone()).collect()
    }

    /// Requested and not failed.
    fn effective(&self, stage: &str) -> bool {
        wants(&self.target, stage) && self.stage(stage).is_none_or(|c| c.failure.is_none())
    }

    fn any_effective(&self) -> bool {
        self.target.iter().any(|(s, _)| self.effective(s.as_str()))
    }

    /// What the worker loads: the target without the failed stages.
    fn job_target(&self) -> Target {
        self.target
            .iter()
            .filter(|(s, _)| self.effective(s.as_str()))
            .cloned()
            .collect()
    }

    fn module(&self) -> String {
        self.factory
            .as_ref()
            .map(|f| f.name().to_owned())
            .unwrap_or_default()
    }

    /// Every stage's status for a new request that replaces `prev` (`None`:
    /// a reload of everything): off when not requested, failed while its
    /// failure is recorded, else loading, except that a ready stage whose
    /// request is the same in both stays ready.
    fn set_switch_statuses(&mut self, prev: Option<&Target>) {
        let target = &self.target;
        for (s, ctl) in &mut self.ctl {
            let s = s.as_str();
            let kept = prev.is_some_and(|p| request_of(p, s) == request_of(target, s))
                && ctl.status == EffectStatus::Ready;
            ctl.status = match (&ctl.failure, wants(target, s)) {
                (_, false) => EffectStatus::Off,
                (Some(r), true) => EffectStatus::Failed(r.clone()),
                (None, true) if kept => EffectStatus::Ready,
                (None, true) => EffectStatus::Loading,
            };
        }
    }

    /// Records a failure of `stage` unless the request no longer asks for
    /// it or one is recorded; `reconcile` starts a new request.
    fn record_failure(&mut self, stage: &StageId, reason: String, reconcile: bool) -> bool {
        if !wants(&self.target, stage.as_str())
            || self
                .stage(stage.as_str())
                .is_some_and(|c| c.failure.is_some())
        {
            return false;
        }
        let st = self.stage_mut(stage);
        st.status = EffectStatus::Failed(reason.clone());
        st.failure = Some(reason.clone());
        self.last_error = Some(reason);
        if reconcile {
            self.generation += 1;
        }
        true
    }

    /// The next job for the worker, if one can run: the format is known,
    /// and a chain on CUDA frames has its context.
    fn job(&self) -> Option<Job> {
        let format = self.format.clone()?;
        let target = self.job_target();
        let kind = match &self.factory {
            None => Err("no module is configured for these stages".to_owned()),
            Some(f) => chain_kind(f.as_ref(), format.cuda),
        };
        if !target.is_empty() && kind == Ok(FrameKind::Cuda) && self.gpu.is_none() {
            return None;
        }
        Some(Job {
            generation: self.generation,
            target,
            kind,
            format,
            gpu: self.gpu,
            factory: self.factory.clone(),
        })
    }

    fn report(&self) -> Report {
        Report {
            module: self.module(),
            stages: self
                .ctl
                .iter()
                .map(|(s, c)| (s.clone(), c.status.clone()))
                .collect(),
        }
    }
}

/// What the status message carries.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    module: String,
    stages: Vec<(StageId, EffectStatus)>,
}

impl Report {
    /// `Some(now)` when a stage's status differs (a stage newly listed, and
    /// off, changes nothing).
    fn changed(&self, now: Report) -> Option<Report> {
        let same = self
            .stages
            .iter()
            .chain(&now.stages)
            .all(|(s, _)| self.stage(s.as_str()) == now.stage(s.as_str()));
        (!same).then_some(now)
    }

    fn stage(&self, stage: &str) -> &EffectStatus {
        self.stages
            .iter()
            .find(|(s, _)| s == stage)
            .map_or(&OFF, |(_, st)| st)
    }

    fn text(&self) -> String {
        self.stages
            .iter()
            .map(|(s, st)| format!("{s} {st}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn post_status(element: &super::ObcModule, r: &Report) {
    let mut b = gst::Structure::builder(STATUS_MESSAGE).field("module", r.module.as_str());
    for (stage, status) in &r.stages {
        b = b.field(stage.as_str(), status.as_str()).field(
            format!("{stage}-reason").as_str(),
            status.reason().unwrap_or(""),
        );
    }
    let _ = element.post_message(
        gst::message::Element::builder(b.build())
            .src(element)
            .build(),
    );
}

#[derive(Default)]
struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    engine: Mutex<Engine>,
    /// The worker holds `engine` for a load, a switch or an unload (not
    /// just to read what is loaded): a frame then passes through rather
    /// than wait.
    busy: AtomicBool,
    processed: AtomicU64,
    converted: AtomicU64,
    passthrough: AtomicU64,
    cuda_active: AtomicBool,
    /// Frames each stage ran on, in the order the stages first ran.
    stage_frames: Mutex<Vec<(StageId, u64)>>,
    /// The status values the chain reported after the last frame it
    /// processed, of the stages that ran on it.
    stage_values: Mutex<Vec<StageValue>>,
}

/// The element's CUDA objects, created on the streaming thread from the
/// first CUDA frame that has a stage to run, and released when dropped (in
/// `stop`, after the chain).
struct Gpu {
    /// A reference to the input's `GstCudaContext`.
    context: *mut c_void,
    cu_ctx: CuContext,
    /// The element's own `GstCudaStream`.
    stream: *mut c_void,
    cu_stream: CuStream,
    /// The page-locked host copy of a frame, for a chain on host frames.
    staging: Option<PinnedHostBuffer>,
    /// The copy of a CUDA frame as it came in, for a chain on CUDA frames
    /// (see "Failures" above).
    scratch: Option<DeviceNv12Buffer>,
}

// SAFETY: as obceffect's resources: a GStreamer CUDA context and stream
// reference owned by this struct, usable from any thread; every CUDA call
// on them pushes the context first.
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
    fn drop(&mut self) {
        let (staging, scratch) = (self.staging.take(), self.scratch.take());
        if staging.is_some() || scratch.is_some() {
            // SAFETY: the context reference is still ours (released below).
            match unsafe { ContextGuard::push(self.cu_ctx) } {
                Ok(g) => {
                    drop(staging);
                    drop(scratch);
                    drop(g);
                }
                Err(e) => {
                    gst::error!(
                        CAT,
                        "cannot push the CUDA context to free the frame copies, leaking them: {e}"
                    );
                    std::mem::forget(staging);
                    std::mem::forget(scratch);
                }
            }
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
    info: Option<VideoInfo>,
    format: Option<Format>,
    /// This frame goes to the chain.
    process: bool,
    live: Arc<BTreeMap<StageId, Params>>,
    reset_pending: bool,
    /// This frame carries [`REPEAT_FLAG`](crate::REPEAT_FLAG).
    repeat: bool,
    gpu: Option<Gpu>,
    /// The copy of a system-memory frame as it came in (see "Failures"
    /// above), kept for the next frame.
    scratch: Vec<u8>,
}

#[derive(Default)]
pub struct ObcModule {
    shared: Arc<Shared>,
    stream: Mutex<StreamState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[glib::object_subclass]
impl ObjectSubclass for ObcModule {
    const NAME: &'static str = "GstObcModule";
    type Type = super::ObcModule;
    type ParentType = gst_base::BaseTransform;
}

impl ObcModule {
    pub(super) fn set_factory(&self, factory: Option<Arc<dyn StageChainFactory>>) {
        lock(&self.shared.control).factory = factory;
    }

    pub(super) fn factory(&self) -> Option<Arc<dyn StageChainFactory>> {
        lock(&self.shared.control).factory.clone()
    }

    pub(super) fn stages(&self) -> Vec<StageRequest> {
        lock(&self.shared.control).stages.clone()
    }

    pub(super) fn stage_status(&self, stage: &str) -> EffectStatus {
        lock(&self.shared.control)
            .stage(stage)
            .map_or(EffectStatus::Off, |c| c.status.clone())
    }

    pub(super) fn stats(&self) -> ModuleStats {
        let (module, kind, statuses, loads, last_load_ms, last_load_reused, last_error) = {
            let c = lock(&self.shared.control);
            (
                c.module(),
                c.kind,
                c.ctl
                    .iter()
                    .map(|(s, ctl)| (s.clone(), ctl.status.clone()))
                    .collect::<Vec<_>>(),
                c.loads,
                c.last_load_ms,
                c.last_load_reused,
                c.last_error.clone(),
            )
        };
        // Only the values of the stages that are ready: a stage turned off
        // or failed since its last frame reports nothing.
        let stage_values = lock(&self.shared.stage_values)
            .iter()
            .filter(|v| {
                statuses
                    .iter()
                    .any(|(s, st)| *s == v.stage && *st == EffectStatus::Ready)
            })
            .cloned()
            .collect();
        let frames = lock(&self.shared.stage_frames);
        let stages = statuses
            .into_iter()
            .map(|(id, status)| StageStats {
                status,
                frames: frames.iter().find(|(s, _)| *s == id).map_or(0, |(_, n)| *n),
                ms_p50: None,
                ms_p95: None,
                id,
            })
            .collect();
        let s = &self.shared;
        ModuleStats {
            module,
            kind,
            stages,
            frames_processed: s.processed.load(Ordering::Relaxed),
            frames_converted: s.converted.load(Ordering::Relaxed),
            frames_passthrough: s.passthrough.load(Ordering::Relaxed),
            loads,
            last_load_ms,
            last_load_reused,
            last_error,
            cuda_active: s.cuda_active.load(Ordering::Relaxed),
            stage_values,
        }
    }

    fn post(&self, posted: Option<Report>) {
        if let Some(r) = posted {
            gst::debug!(CAT, imp = self, "stages: {}", r.text());
            post_status(&self.obj(), &r);
        }
    }

    /// Applies new requests: a change of what has to be loaded (a stage on
    /// or off, its load-time parameters, the order) is a new request for
    /// the worker; `retry` makes one after a failure even when nothing
    /// changed, unless only live parameters changed. A stage's failure is
    /// forgotten when its request changes, or on a retry.
    pub(super) fn apply(&self, new: Vec<StageRequest>, retry: bool) {
        let posted = {
            let mut c = lock(&self.shared.control);
            let retry = retry && !changes_live_only(&c.stages, &new);
            let before = c.report();
            let target = target_of(&new);
            for st in &new {
                c.stage_mut(&st.id);
            }
            c.live = Arc::new(
                new.iter()
                    .filter(|s| s.enabled && !s.live.is_empty())
                    .map(|s| (s.id.clone(), s.live.clone()))
                    .collect(),
            );
            c.stages = new;
            let any_failed = c.ctl.iter().any(|(_, ctl)| ctl.failure.is_some());
            if target != c.target || (retry && any_failed) {
                let old = c.target.clone();
                for (s, ctl) in &mut c.ctl {
                    if retry || request_of(&target, s.as_str()) != request_of(&old, s.as_str()) {
                        ctl.failure = None;
                    }
                }
                c.target = target;
                c.generation += 1;
                c.set_switch_statuses(Some(&old));
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        self.post(posted);
    }

    /// Records `failures` (each starts a new request, so the worker brings
    /// the chain in line), and posts.
    fn fail_stages(&self, failures: &[(StageId, String)]) {
        if failures.is_empty() {
            return;
        }
        let posted = {
            let mut c = lock(&self.shared.control);
            let before = c.report();
            let mut any = false;
            for (stage, reason) in failures {
                any |= c.record_failure(stage, reason.clone(), true);
            }
            if any {
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        for (stage, reason) in failures {
            gst::warning!(CAT, imp = self, "{stage}: {reason}");
        }
        self.post(posted);
    }

    /// The chain failed as a whole on the streaming thread: nothing is
    /// loaded any more, and every effective stage fails with `reason`.
    fn fail_all(&self, reason: String) {
        let stages: Vec<(StageId, String)> = {
            let mut c = lock(&self.shared.control);
            c.ready = false;
            c.last_error = Some(reason.clone());
            c.job_target()
                .into_iter()
                .map(|(s, _)| (s, reason.clone()))
                .collect()
        };
        self.fail_stages(&stages);
    }

    /// Has the worker load `stages` again: the chain dropped them on a
    /// temporal reset. Each one still effective shows loading.
    fn rebuild(&self, stages: &[StageId]) {
        if stages.is_empty() {
            return;
        }
        let posted = {
            let mut c = lock(&self.shared.control);
            let before = c.report();
            let mut wanted = false;
            for s in stages {
                if c.effective(s.as_str()) {
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
        self.post(posted);
    }

    /// Creates the CUDA objects from the first input memory: a reference to
    /// its context and a stream in it.
    fn ensure_gpu(&self, st: &mut StreamState, inbuf: &gst::BufferRef) -> Result<(), String> {
        let lib = cuda::gst_cuda()?;
        let context = input_context(lib, inbuf)?;
        if let Some(gpu) = &st.gpu {
            if gpu.context != context {
                return Err(
                    "an input frame is in another CUDA context than the stream's first frame"
                        .into(),
                );
            }
            return Ok(());
        }
        // SAFETY: `context` is the live GstCudaContext of the input memory;
        // we take our own reference for the lifetime of `Gpu`.
        let cu_ctx = unsafe {
            gst::ffi::gst_object_ref(context.cast());
            (lib.gst_cuda_context_get_handle)(context)
        };
        // SAFETY: as above; the stream is created in that context.
        let stream = unsafe { (lib.gst_cuda_stream_new)(context) };
        if stream.is_null() || cu_ctx.is_null() {
            // SAFETY: our own context reference, taken above.
            unsafe {
                if !stream.is_null() {
                    (lib.gst_cuda_stream_unref)(stream);
                }
                gst::ffi::gst_object_unref(context.cast());
            }
            return Err("could not create a CUDA stream in the stream's context".into());
        }
        // SAFETY: `stream` is the live GstCudaStream just created.
        let cu_stream = unsafe { (lib.gst_cuda_stream_get_handle)(stream) };
        st.gpu = Some(Gpu {
            context,
            cu_ctx,
            stream,
            cu_stream,
            staging: None,
            scratch: None,
        });
        self.shared.cuda_active.store(true, Ordering::Relaxed);
        let gpu = st.gpu.as_ref().map(Gpu::gpu_context);
        lock(&self.shared.control).gpu = gpu;
        self.shared.wake.notify_all();
        gst::info!(
            CAT,
            imp = self,
            "CUDA resources created on the input's context"
        );
        Ok(())
    }

    /// Unloads the chain, then releases the CUDA objects.
    fn release(&self) {
        lock(&self.shared.engine).release_chain();
        let gpu = lock(&self.stream).gpu.take();
        drop(gpu);
        self.shared.cuda_active.store(false, Ordering::Relaxed);
        let mut c = lock(&self.shared.control);
        c.gpu = None;
        c.ready = false;
        c.kind = None;
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

    /// Decides whether the frame goes to the chain and sets BaseTransform's
    /// passthrough to match, creating the CUDA objects when a stage has to
    /// run on CUDA frames.
    fn prepare_frame(&self, inbuf: &gst::BufferRef) {
        let (any, ready, live) = {
            let c = lock(&self.shared.control);
            (c.any_effective(), c.ready, Arc::clone(&c.live))
        };
        let mut st = lock(&self.stream);
        let cuda = st.format.as_ref().is_some_and(|f| f.cuda);
        let mut process = any && ready;
        if any
            && cuda
            && let Err(reason) = self.ensure_gpu(&mut st, inbuf)
        {
            drop(st);
            self.fail_all(reason);
            process = false;
            st = lock(&self.stream);
        }
        if has_new_source_flag(inbuf) {
            st.reset_pending = true;
        }
        st.repeat = has_repeat_flag(inbuf);
        st.process = process;
        st.live = live;
        drop(st);
        if !process {
            self.shared.passthrough.fetch_add(1, Ordering::Relaxed);
            if !any {
                // No stage is on: nothing reports.
                lock(&self.shared.stage_values).clear();
            }
        }
        let obj = self.obj();
        if obj.is_passthrough() == process {
            obj.set_passthrough(!process);
        }
    }

    /// Processes `buf` in place with the loaded chain, if the worker does
    /// not hold it. Returns whether the chain ran on it and through the
    /// host copy.
    fn render(&self, st: &mut StreamState, buf: &mut gst::BufferRef) -> Result<Rendered, String> {
        let info = st.info.clone().ok_or("no caps negotiated")?;
        let cuda = st.format.as_ref().is_some_and(|f| f.cuda);
        let Some(mut e) = self.engine_for_frame() else {
            return Ok(Rendered::default());
        };
        let Engine { chain, loaded, .. } = &mut *e;
        let (Some(chain), Some(key)) = (chain.as_mut(), loaded.as_mut()) else {
            return Ok(Rendered::default());
        };
        if (key.width, key.height) != (info.width(), info.height()) {
            return Ok(Rendered::default());
        }
        let live = Arc::clone(&st.live);
        let reset = std::mem::take(&mut st.reset_pending);
        let repeat = st.repeat;
        let mut run = Run::default();
        let mut converted = false;
        match (key.kind, cuda) {
            (FrameKind::Cuda, true) => {
                let gpu = st.gpu.as_mut().ok_or("no CUDA resources")?;
                let ctx = gpu.gpu_context();
                // SAFETY: our context reference is live.
                let _guard =
                    unsafe { ContextGuard::push(ctx.context) }.map_err(|e| e.to_string())?;
                let frame = map_cuda(buf, &info)?;
                let stream = ctx.stream;
                let _sync = SyncOnDrop(stream);
                let mut target = CudaInPlace {
                    gpu: ctx,
                    frame: frame.nv12(),
                    scratch: &mut gpu.scratch,
                };
                run = run_chain(chain.as_mut(), key, reset, repeat, &live, &mut target);
                // SAFETY: our own stream; the frame stays mapped until it
                // is done.
                unsafe { obc_gpu::stream_synchronize(stream) }.map_err(|e| e.to_string())?;
                drop(frame);
            }
            (FrameKind::Host, false) => {
                let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(buf, &info)
                    .map_err(|_| "could not map a frame for writing")?;
                let y_pitch = frame.plane_stride()[0].max(0) as usize;
                let uv_pitch = frame.plane_stride()[1].max(0) as usize;
                let y_plane = frame.plane_data_mut(0).map_err(|e| e.to_string())?;
                let (y, y_len) = (y_plane.as_mut_ptr(), y_plane.len());
                let uv_plane = frame.plane_data_mut(1).map_err(|e| e.to_string())?;
                let (uv, uv_len) = (uv_plane.as_mut_ptr(), uv_plane.len());
                let mut target = HostInPlace {
                    host: HostNv12 {
                        y,
                        y_pitch,
                        uv,
                        uv_pitch,
                        width: info.width(),
                        height: info.height(),
                    },
                    y_len,
                    uv_len,
                    scratch: &mut st.scratch,
                };
                run = run_chain(chain.as_mut(), key, reset, repeat, &live, &mut target);
                drop(frame);
            }
            (FrameKind::Host, true) => {
                let gpu = st.gpu.as_mut().ok_or("no CUDA resources")?;
                let ctx = gpu.gpu_context();
                // SAFETY: our context reference is live.
                let _guard =
                    unsafe { ContextGuard::push(ctx.context) }.map_err(|e| e.to_string())?;
                let frame = map_cuda(buf, &info)?;
                let dev = frame.nv12();
                let stream = ctx.stream;
                let _sync = SyncOnDrop(stream);
                let (w, h) = (info.width(), info.height());
                let pitch = (w as usize).next_multiple_of(64);
                let need = pitch * (h as usize + h as usize / 2);
                if gpu.staging.as_ref().is_none_or(|s| s.len() < need) {
                    gpu.staging = None;
                    gpu.staging = Some(
                        PinnedHostBuffer::alloc(need)
                            .map_err(|e| format!("could not allocate the host copy: {e}"))?,
                    );
                }
                let staging = gpu.staging.as_mut().expect("allocated above");
                let host = staging.nv12(w, h, pitch).map_err(|e| e.to_string())?;
                let mut target = HostCopy { dev, host, stream };
                run = run_chain(chain.as_mut(), key, reset, repeat, &live, &mut target);
                let host = target.host;
                if run.ran.is_some() {
                    // SAFETY: as for the download.
                    unsafe {
                        obc_gpu::upload_nv12_async(&host, &dev, stream)
                            .and_then(|()| obc_gpu::stream_synchronize(stream))
                    }
                    .map_err(|e| format!("could not copy a frame back from the host: {e}"))?;
                    converted = true;
                }
                drop(frame);
            }
            // Refused when the job was made: never loaded.
            (FrameKind::Cuda, false) => {}
        }
        // The values after a frame the chain ran on, of the stages that ran
        // (the engine is held for the frame already: no wait).
        let values = run.ran.as_ref().map(|ran| {
            chain
                .status_values()
                .into_iter()
                .filter(|v| ran.contains(&v.stage))
                .collect()
        });
        let unloaded = key.stages.is_empty() || run.chain_error.is_some();
        if unloaded {
            *loaded = None;
        }
        drop(e);
        Ok(Rendered {
            unloaded,
            values,
            ran: run.ran,
            converted,
            failures: run.failures,
            rebuild: run.rebuild,
            chain_error: run.chain_error,
        })
    }
}

impl ObcModule {
    /// The engine for one frame: never while the worker loads, switches or
    /// unloads (`Shared::busy`), and waiting at most a few milliseconds
    /// for the worker's brief reads of what is loaded.
    fn engine_for_frame(&self) -> Option<MutexGuard<'_, Engine>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
        loop {
            if self.shared.busy.load(Ordering::Acquire) {
                return None;
            }
            match self.shared.engine.try_lock() {
                Ok(e) => return Some(e),
                Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
            }
        }
    }
}

/// Marks the worker busy with the engine (`Shared::busy`) while it lives.
struct Busy<'a>(&'a AtomicBool);

impl<'a> Busy<'a> {
    fn new(flag: &'a AtomicBool) -> Busy<'a> {
        flag.store(true, Ordering::Release);
        Busy(flag)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Synchronises a CUDA stream when dropped: every exit of a CUDA path waits
/// for the work queued on the frame before it is unmapped (the guard is
/// declared after the frame, so it drops first).
struct SyncOnDrop(CuStream);

impl Drop for SyncOnDrop {
    fn drop(&mut self) {
        // SAFETY: the element's own stream; its context is still pushed.
        let _ = unsafe { obc_gpu::stream_synchronize(self.0) };
    }
}

/// Maps `buf` (CUDA memory, writable) for reading and writing on the GPU,
/// and waits for its producer.
fn map_cuda(buf: &mut gst::BufferRef, info: &VideoInfo) -> Result<CudaFrame, String> {
    let lib = cuda::gst_cuda()?;
    // SAFETY: the buffer is writable (BaseTransform made it so for the
    // in-place transform) and outlives the frame; a memory shared with
    // another buffer is copied by the map before it is written.
    let frame = unsafe {
        CudaFrame::map(
            buf.as_mut_ptr(),
            info,
            gst::ffi::GST_MAP_READ | gst::ffi::GST_MAP_WRITE | MAP_CUDA,
        )
    }?;
    for i in 0..buf.n_memory() {
        let mem = buf.peek_memory(i).as_mut_ptr();
        // SAFETY: a live memory of the buffer; only CUDA memories are synced.
        unsafe {
            if (lib.gst_is_cuda_memory)(mem) != glib::ffi::GFALSE {
                (lib.gst_cuda_memory_sync)(mem.cast());
            }
        }
    }
    Ok(frame)
}

/// What one frame through the chain gave.
#[derive(Default)]
struct Run {
    /// The stages that processed the frame, if it was processed.
    ran: Option<Vec<StageId>>,
    /// Stages that failed alone (dropped from what is loaded).
    failures: Vec<(StageId, String)>,
    /// Stages the temporal reset dropped, to load again.
    rebuild: Vec<StageId>,
    /// The chain failed as a whole.
    chain_error: Option<String>,
}

#[derive(Default)]
struct Rendered {
    /// Nothing is loaded any more.
    unloaded: bool,
    ran: Option<Vec<StageId>>,
    /// The status values after the frame, when the chain ran on it.
    values: Option<Vec<StageValue>>,
    converted: bool,
    failures: Vec<(StageId, String)>,
    rebuild: Vec<StageId>,
    chain_error: Option<String>,
}

/// The stage `err` is the failure of alone.
fn stage_of(err: &BackendError) -> Option<StageId> {
    match err {
        BackendError::Stage { stage, .. } => Some(stage.clone()),
        _ => None,
    }
}

/// The frame one run of the chain works on in place, with the copy of it
/// as it came in (see "Failures" above).
trait RunFrame {
    /// Keeps a copy of the frame as it is.
    fn save(&mut self) -> Result<(), String>;
    /// Puts the frame back as `save` found it.
    fn restore(&mut self) -> Result<(), String>;
    /// The frame, as the chain takes it.
    fn frame(&mut self) -> ChainFrame<'_>;
}

/// A CUDA frame, run in place; the copy is a device buffer of the stream.
struct CudaInPlace<'a> {
    gpu: GpuContext,
    frame: DeviceNv12,
    scratch: &'a mut Option<DeviceNv12Buffer>,
}

impl CudaInPlace<'_> {
    fn scratch(&self) -> Result<DeviceNv12, String> {
        self.scratch
            .as_ref()
            .map(DeviceNv12Buffer::frame)
            .ok_or_else(|| "no copy of the frame was kept".to_owned())
    }
}

impl RunFrame for CudaInPlace<'_> {
    fn save(&mut self) -> Result<(), String> {
        let (w, h) = (self.frame.width, self.frame.height);
        if self
            .scratch
            .as_ref()
            .is_none_or(|b| (b.frame().width, b.frame().height) != (w, h))
        {
            *self.scratch = None;
            *self.scratch = Some(DeviceNv12Buffer::alloc(w, h).map_err(|e| e.to_string())?);
        }
        let copy = self.scratch()?;
        // SAFETY: the mapped frame and our buffer of its size, both of the
        // current context; ordered before the chain's work on the stream.
        unsafe { obc_gpu::copy_nv12_async(&self.frame, &copy, self.gpu.stream) }
            .map_err(|e| e.to_string())
    }

    fn restore(&mut self) -> Result<(), String> {
        let copy = self.scratch()?;
        // SAFETY: as in `save`; ordered after the failed call's work.
        unsafe { obc_gpu::copy_nv12_async(&copy, &self.frame, self.gpu.stream) }
            .map_err(|e| e.to_string())
    }

    fn frame(&mut self) -> ChainFrame<'_> {
        ChainFrame::Cuda {
            gpu: &self.gpu,
            frame: &self.frame,
        }
    }
}

/// A system-memory frame, run in place; the copy is a `Vec` of the stream.
struct HostInPlace<'a> {
    /// The mapped frame, whose planes are `y_len` and `uv_len` bytes.
    host: HostNv12,
    y_len: usize,
    uv_len: usize,
    scratch: &'a mut Vec<u8>,
}

impl RunFrame for HostInPlace<'_> {
    fn save(&mut self) -> Result<(), String> {
        self.scratch.clear();
        // SAFETY: both planes are mapped for reading and writing, `y_len`
        // and `uv_len` bytes long, for as long as `self` lives, and nothing
        // else accesses them meanwhile.
        unsafe {
            self.scratch
                .extend_from_slice(std::slice::from_raw_parts(self.host.y, self.y_len));
            self.scratch
                .extend_from_slice(std::slice::from_raw_parts(self.host.uv, self.uv_len));
        }
        Ok(())
    }

    fn restore(&mut self) -> Result<(), String> {
        if self.scratch.len() != self.y_len + self.uv_len {
            return Err("no copy of the frame was kept".into());
        }
        let (y, uv) = self.scratch.split_at(self.y_len);
        // SAFETY: as in `save`; the copy is of the same planes, so of their
        // lengths, and does not overlap them.
        unsafe {
            std::ptr::copy_nonoverlapping(y.as_ptr(), self.host.y, self.y_len);
            std::ptr::copy_nonoverlapping(uv.as_ptr(), self.host.uv, self.uv_len);
        }
        Ok(())
    }

    fn frame(&mut self) -> ChainFrame<'_> {
        ChainFrame::Host(&mut self.host)
    }
}

/// A CUDA frame run through a host copy: the device frame itself is the
/// copy of the input (it is written only once the chain has run), so both
/// `save` and `restore` copy it to the host.
struct HostCopy {
    dev: DeviceNv12,
    host: HostNv12,
    stream: CuStream,
}

impl RunFrame for HostCopy {
    fn save(&mut self) -> Result<(), String> {
        // SAFETY: `dev` is the mapped frame of the current context and
        // `host` our page-locked buffer of that size; both outlive the
        // synchronisation.
        unsafe {
            obc_gpu::download_nv12_async(&self.dev, &self.host, self.stream)
                .and_then(|()| obc_gpu::stream_synchronize(self.stream))
        }
        .map_err(|e| format!("could not copy a frame to the host: {e}"))
    }

    fn restore(&mut self) -> Result<(), String> {
        self.save()
    }

    fn frame(&mut self) -> ChainFrame<'_> {
        ChainFrame::Host(&mut self.host)
    }
}

/// Runs one frame through `chain` (loaded as `key`): the pending temporal
/// reset first, then the chain on `frame` with the live parameters of the
/// loaded stages, again without a stage each time one fails alone. The
/// frame is kept before the first call and put back after every failing
/// one: a frame the chain did not process is as it came in. A `repeat`
/// (the buffer carried [`REPEAT_FLAG`](crate::REPEAT_FLAG)) goes to the
/// chain as one ([`StageChain::process_repeat`]).
fn run_chain(
    chain: &mut dyn StageChain,
    key: &mut LoadedKey,
    reset: bool,
    repeat: bool,
    live: &BTreeMap<StageId, Params>,
    frame: &mut impl RunFrame,
) -> Run {
    let mut run = Run::default();
    if reset {
        match chain.reset_temporal() {
            Ok(dropped) => {
                for s in dropped.as_slice() {
                    if wants(&key.stages, s.as_str()) {
                        key.without(s.as_str());
                        run.rebuild.push(s.clone());
                    }
                }
            }
            Err(err) => {
                run.chain_error = Some(format!("forgetting the previous source failed: {err}"));
                return run;
            }
        }
    }
    if key.stages.is_empty() {
        return run;
    }
    if let Err(e) = frame.save() {
        run.chain_error = Some(format!("could not keep a copy of a frame: {e}"));
        return run;
    }
    while !key.stages.is_empty() {
        let frame_live: BTreeMap<StageId, Params> = live
            .iter()
            .filter(|(s, _)| wants(&key.stages, s.as_str()))
            .map(|(s, p)| (s.clone(), p.clone()))
            .collect();
        let result = if repeat {
            chain.process_repeat(frame.frame(), &frame_live)
        } else {
            chain.process(frame.frame(), &frame_live)
        };
        match result {
            Ok(()) => {
                run.ran = Some(key.stages.iter().map(|(s, _)| s.clone()).collect());
                break;
            }
            Err(err) => {
                if let Err(e) = frame.restore() {
                    run.chain_error = Some(format!(
                        "processing a frame failed ({err}), and so did restoring it: {e}"
                    ));
                    break;
                }
                match stage_of(&err) {
                    Some(s) if wants(&key.stages, s.as_str()) => {
                        key.without(s.as_str());
                        run.failures
                            .push((s.clone(), format!("the {s} stage failed on a frame: {err}")));
                    }
                    _ => {
                        run.chain_error = Some(format!("processing a frame failed: {err}"));
                        break;
                    }
                }
            }
        }
    }
    run
}

/// One unit of work for the worker.
struct Job {
    generation: u64,
    /// What to load: the request without the failed stages.
    target: Target,
    kind: Result<FrameKind, String>,
    format: Format,
    gpu: Option<GpuContext>,
    factory: Option<Arc<dyn StageChainFactory>>,
}

/// Why a load failed.
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

    fn of(err: BackendError) -> Failure {
        Failure {
            reason: err.to_string(),
            error: Some(err),
        }
    }

    fn stage(&self) -> Option<StageId> {
        self.error.as_ref().and_then(stage_of)
    }
}

enum Outcome {
    Unloaded,
    Loaded(LoadReport),
    AlreadyLoaded,
    Failed(Failure),
    Superseded,
}

fn worker_loop(shared: Arc<Shared>, element: glib::WeakRef<super::ObcModule>) {
    let mut handled: Option<u64> = None;
    loop {
        let job = {
            let mut c = lock(&shared.control);
            loop {
                if c.stop {
                    return;
                }
                if handled != Some(c.generation)
                    && let Some(job) = c.job()
                {
                    break job;
                }
                c = shared.wake.wait(c).unwrap_or_else(|e| e.into_inner());
            }
        };
        handled = Some(job.generation);
        let (outcome, stage_failures) = run_job(&shared, &job);
        let in_force = lock(&shared.engine).loaded.clone();
        let posted = {
            let mut c = lock(&shared.control);
            let before = c.report();
            c.ready = in_force.as_ref().is_some_and(|k| !k.stages.is_empty());
            c.kind = in_force.as_ref().map(|k| k.kind);
            if let Outcome::Loaded(r) = &outcome {
                c.loads += 1;
                c.last_load_ms = Some(r.load_ms + r.warmup_ms.unwrap_or(0.0));
                c.last_load_reused = r.reused_model;
            }
            if c.generation == job.generation {
                for (stage, reason) in &stage_failures {
                    c.record_failure(stage, reason.clone(), false);
                }
                match &outcome {
                    Outcome::Unloaded | Outcome::Superseded => {}
                    Outcome::Loaded(_) | Outcome::AlreadyLoaded => {
                        for s in c.stage_ids() {
                            let loaded = in_force
                                .as_ref()
                                .is_some_and(|k| wants(&k.stages, s.as_str()));
                            if c.effective(s.as_str()) && loaded {
                                c.stage_mut(&s).status = EffectStatus::Ready;
                            }
                        }
                    }
                    Outcome::Failed(f) => {
                        for s in c.stage_ids() {
                            if c.effective(s.as_str()) {
                                // Recorded: the next request unloads.
                                c.record_failure(&s, f.reason.clone(), true);
                            }
                        }
                        c.last_error = Some(f.reason.clone());
                    }
                }
                let target = c.target.clone();
                for (s, ctl) in &mut c.ctl {
                    if !wants(&target, s.as_str()) {
                        ctl.status = EffectStatus::Off;
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
                    "loaded {} ({} frames) in {:.1} ms (+{:.1} ms warm-up, reused: {})",
                    in_force
                        .as_ref()
                        .map(|k| k
                            .stages
                            .iter()
                            .map(|(s, _)| s.as_str())
                            .collect::<Vec<_>>()
                            .join(","))
                        .unwrap_or_default(),
                    in_force.as_ref().map_or("no", |k| k.kind.as_str()),
                    r.load_ms,
                    r.warmup_ms.unwrap_or(0.0),
                    r.reused_model
                ),
                Outcome::Unloaded => gst::info!(CAT, obj = el, "unloaded"),
                Outcome::Failed(f) => gst::warning!(CAT, obj = el, "load failed: {}", f.reason),
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

/// Brings what is loaded in line with `job`; a stage that fails alone is
/// returned and the job goes on without it.
fn run_job(shared: &Shared, job: &Job) -> (Outcome, Vec<(StageId, String)>) {
    let mut target = job.target.clone();
    let mut failures = Vec::new();
    loop {
        if target.is_empty() {
            let _busy = Busy::new(&shared.busy);
            lock(&shared.engine).release_chain();
            return (Outcome::Unloaded, failures);
        }
        match load(shared, job, &target) {
            Outcome::Failed(f) if f.stage().is_some_and(|s| wants(&target, s.as_str())) => {
                let s = f.stage().expect("matched above");
                target.retain(|(t, _)| *t != s);
                failures.push((s, f.reason));
            }
            outcome => return (outcome, failures),
        }
    }
}

/// Loads `target` for `job` on the element's chain, creating it if needed.
fn load(shared: &Shared, job: &Job, target: &Target) -> Outcome {
    let kind = match &job.kind {
        Ok(k) => *k,
        Err(reason) => return Outcome::Failed(Failure::new(reason.clone())),
    };
    let colour = match &job.format.colour {
        Ok(c) => *c,
        Err(reason) => return Outcome::Failed(Failure::new(reason.clone())),
    };
    let key = LoadedKey {
        stages: target.clone(),
        width: job.format.width,
        height: job.format.height,
        colour,
        kind,
    };
    let stages = match StageList::new(target.iter().map(|(s, _)| s.clone())) {
        Ok(s) => s,
        Err(e) => return Outcome::Failed(Failure::of(e)),
    };
    if lock(&shared.engine).loaded.as_ref() == Some(&key) {
        return Outcome::AlreadyLoaded;
    }
    if superseded(shared, job.generation) {
        return Outcome::Superseded;
    }
    let Some(factory) = job.factory.clone() else {
        return Outcome::Failed(Failure::new("no module is configured for these stages"));
    };
    let gpu = match kind {
        FrameKind::Cuda => match job.gpu {
            Some(g) => Some(g),
            None => return Outcome::Failed(Failure::new("no CUDA context for the chain")),
        },
        FrameKind::Host => None,
    };
    let _busy = Busy::new(&shared.busy);
    let mut e = lock(&shared.engine);
    // A chain made for other frames goes first.
    if e.chain.is_some() && (e.kind != Some(kind) || e.context != gpu.map(|g| g.context)) {
        e.release_chain();
    }
    let _guard = match gpu {
        // SAFETY: the element keeps the context alive until it has joined
        // this thread.
        Some(g) => match unsafe { ContextGuard::push(g.context) } {
            Ok(guard) => Some(guard),
            Err(err) => {
                return Outcome::Failed(Failure::new(format!(
                    "cannot make the CUDA context current: {err}"
                )));
            }
        },
        None => None,
    };
    if e.chain.is_none() {
        match factory.create() {
            Ok(c) => {
                e.chain = Some(c);
                e.kind = Some(kind);
                e.context = gpu.map(|g| g.context);
            }
            Err(err) => return Outcome::Failed(Failure::of(err)),
        }
    }
    let req = ChainRequest {
        stages,
        params: target
            .iter()
            .filter(|(_, p)| !p.is_empty())
            .cloned()
            .collect(),
        width: key.width,
        height: key.height,
        colour,
        kind,
        gpu,
    };
    if let Err(err) = req.validate() {
        return Outcome::Failed(Failure::of(err));
    }
    let full = e.loaded.as_ref().is_none_or(|k| {
        (k.width, k.height, k.colour, k.kind) != (key.width, key.height, key.colour, key.kind)
    });
    let result = e.chain.as_mut().expect("created above").load(&req);
    match result {
        Ok(report) => {
            e.loaded = Some(key);
            Outcome::Loaded(report)
        }
        Err(err) => {
            if full {
                e.loaded = None;
            } else if let (Some(stage), Some(k)) = (stage_of(&err), e.loaded.as_mut()) {
                k.without(stage.as_str());
                if k.stages.is_empty() {
                    e.loaded = None;
                }
            }
            Outcome::Failed(Failure::of(err))
        }
    }
}

impl ObjectImpl for ObcModule {}

impl GstObjectImpl for ObcModule {}

impl ElementImpl for ObcModule {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "omarchy-broadcast module stage",
                "Filter/Effect/Video",
                "A chain of stages of a loadable module, in place on NV12 frames in CUDA or \
                 system memory; passthrough without a stage",
                "Raul Dias",
            )
        });
        Some(&*META)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let mut caps = gst::Caps::builder("video/x-raw")
                .features(["memory:CUDAMemory"])
                .field("format", "NV12")
                .build();
            caps.merge(
                gst::Caps::builder("video/x-raw")
                    .field("format", "NV12")
                    .build(),
            );
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

impl BaseTransformImpl for ObcModule {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        lock(&self.shared.control).stop = false;
        let shared = Arc::clone(&self.shared);
        let element = self.obj().downgrade();
        let handle = std::thread::Builder::new()
            .name("obcmodule-load".into())
            .spawn(move || worker_loop(shared, element))
            .map_err(|e| {
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["cannot start the module stage worker thread: {e}"]
                )
            })?;
        *lock(&self.worker) = Some(handle);
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.stop_worker();
        self.release();
        lock(&self.shared.stage_values).clear();
        let posted = {
            let mut c = lock(&self.shared.control);
            let before = c.report();
            // A restart loads again and retries every failure, once its
            // caps are known: not at the geometry of the last run.
            for (_, ctl) in &mut c.ctl {
                ctl.failure = None;
            }
            c.format = None;
            c.generation += 1;
            c.set_switch_statuses(None);
            before.changed(c.report())
        };
        self.post(posted);
        let mut st = lock(&self.stream);
        st.process = false;
        st.reset_pending = false;
        st.info = None;
        st.format = None;
        st.scratch = Vec::new();
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
        let cuda = incaps
            .features(0)
            .is_some_and(|f| f.contains(crate::cuda::MEMORY_FEATURE));
        let format = Format {
            width: info.width(),
            height: info.height(),
            colour: colour_of(&info.colorimetry()),
            cuda,
        };
        {
            let mut st = lock(&self.stream);
            st.info = Some(info);
            st.format = Some(format.clone());
        }
        let posted = {
            let mut c = lock(&self.shared.control);
            let before = c.report();
            if c.format.as_ref() != Some(&format) {
                c.format = Some(format);
                c.generation += 1;
                c.set_switch_statuses(None);
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        self.post(posted);
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

    fn transform_ip(&self, buf: &mut gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut st = lock(&self.stream);
        if !st.process {
            return Ok(gst::FlowSuccess::Ok);
        }
        let result = self.render(&mut st, buf);
        drop(st);
        let r = match result {
            Ok(r) => r,
            Err(reason) => Rendered {
                chain_error: Some(reason),
                ..Rendered::default()
            },
        };
        let s = &self.shared;
        match &r.ran {
            Some(ran) => {
                s.processed.fetch_add(1, Ordering::Relaxed);
                if r.converted {
                    s.converted.fetch_add(1, Ordering::Relaxed);
                }
                let mut frames = lock(&s.stage_frames);
                for id in ran {
                    match frames.iter_mut().find(|(s, _)| s == id) {
                        Some((_, n)) => *n += 1,
                        None => frames.push((id.clone(), 1)),
                    }
                }
                drop(frames);
                if let Some(values) = r.values {
                    *lock(&s.stage_values) = values;
                }
            }
            None => {
                s.passthrough.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.rebuild(&r.rebuild);
        self.fail_stages(&r.failures);
        if r.unloaded {
            lock(&s.control).ready = false;
        }
        if let Some(reason) = r.chain_error {
            self.fail_all(reason);
        }
        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: &str, on: bool) -> StageRequest {
        StageRequest {
            enabled: on,
            ..StageRequest::new(StageId::new(id).unwrap())
        }
    }

    #[test]
    fn live_only_changes_are_told_apart() {
        let a = vec![req("tint", true).with_live("u", 1i64)];
        let b = vec![req("tint", true).with_live("u", 2i64)];
        assert!(changes_live_only(&a, &b));
        assert!(!changes_live_only(&a, &a));
        let c = vec![req("tint", true).with_param("y", 1i64)];
        assert!(!changes_live_only(&a, &c));
        let d = vec![req("tint", false).with_live("u", 1i64)];
        assert!(!changes_live_only(&a, &d));
    }

    #[test]
    fn the_target_is_the_stages_that_are_on_in_order() {
        let t = target_of(&[
            req("lift", true).with_param("y", 3i64),
            req("tint", false),
            req("hello", true),
        ]);
        let ids: Vec<&str> = t.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(ids, ["lift", "hello"]);
        assert!(wants(&t, "lift") && !wants(&t, "tint"));
        assert_eq!(
            request_of(&t, "lift").and_then(|p| p.get("y")),
            Some(&3i64.into())
        );
    }

    #[test]
    fn a_status_report_changes_only_with_a_status() {
        let id = |s: &str| StageId::new(s).unwrap();
        let a = Report {
            module: "m".into(),
            stages: vec![(id("tint"), EffectStatus::Loading)],
        };
        let off = Report {
            module: "m".into(),
            stages: vec![
                (id("tint"), EffectStatus::Loading),
                (id("lift"), EffectStatus::Off),
            ],
        };
        assert!(a.changed(off.clone()).is_none(), "a new stage, off");
        let ready = Report {
            module: "m".into(),
            stages: vec![(id("tint"), EffectStatus::Ready)],
        };
        assert_eq!(a.changed(ready.clone()), Some(ready));
    }
}
