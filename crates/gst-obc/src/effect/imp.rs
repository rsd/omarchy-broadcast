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
//!   it for the switch itself: milliseconds between two effects, a stage's
//!   load when a switch adds it (see below).
//!
//! # The stages
//!
//! The stackable stages (`docs/adr/0018-video-noise-removal.md`,
//! `docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md`)
//! live in the same backend instance as the effect: one load request carries
//! the effect and every requested stage (`LoadRequest::stages`, in the
//! settings' order, with their load-time parameters), and they unload with
//! the effect, so they add no teardown path and nothing of them runs at
//! idle. The element knows no stage by name: every rule below applies to
//! each [`StageId`] the settings list. A stage hides nothing, so the privacy
//! rules of the effect are unchanged by it:
//!
//! * `none` with stages passes the input through until they are loaded,
//!   then processes every frame. Without a stage `none` makes no CUDA call.
//! * From `none` (with or without stages) to blur or replace, the matte
//!   loads cold: the stages alone do not count as loaded for an effect, so
//!   the frames are black until the effect is ready and the streaming thread
//!   never waits for that load (and under the `Engine` lock a frame decided
//!   for an effect is black if only the stages are loaded).
//! * Every other change (turning a stage on or off under an effect, a new
//!   load-time parameter of a stage, blur and replace with stages, an effect
//!   to `none` with stages) is a switch on the loaded instance, done under
//!   the `Engine` lock as for blur to replace. The streaming thread waits for
//!   it there; when the switch adds a stage under blur or replace, or goes
//!   from an effect to `none` with stages, that wait is the stage's load (a
//!   new load-time parameter costs what the backend says: a relighting
//!   stage's new light loads no model). No frame is output meanwhile, so the
//!   last one shown is the previous chain's processed frame, never the
//!   input; the next is the new chain's. A stage's live parameters travel
//!   with each frame (`FrameParams`) and change nothing that is loaded.
//! * **A stage failure drops the stage, never the effect.** The effective
//!   request of a stage is "requested and no failure recorded"
//!   (`StageCtl::failure` of `Control::stages`). A load that fails with
//!   `BackendError::Stage` for a stage drops that stage, and any other load
//!   failure while the stages are alone (`none`) drops every requested
//!   stage; the worker then loads the rest in the same job (or unloads, for
//!   `none` without another stage). A frame that fails with
//!   `BackendError::Stage` for a stage (or any frame failure of the stages
//!   alone, which drops them all) records it too, whichever request the
//!   frame was decided for, as long as the current one still asks for the
//!   stage: the streaming thread marks the loaded chain as without the stage
//!   and starts a new request, which the worker finds already loaded (or
//!   unloads, for `none`). Under blur or replace the backend has dropped the
//!   stage, so the frame runs again through the chain without it, as often
//!   as stages fail, and is output processed; only if a failure names no
//!   stage does the effect's failure path apply (black or the input, per
//!   `on-failure`). With `none` the frame is the input, and a stage left
//!   loaded runs from the next frame. The effect's status never becomes
//!   failed for a stage. The failure is forgotten when the stage's request
//!   (on or off, or its load-time parameters) changes or on a retry
//!   (`apply` with `retry`), never otherwise.
//! * Each stage's status (`StageCtl::status`) is separate from the
//!   effect's: off (not requested), loading, ready, or failed with the
//!   reason. A failure of the whole chain (the effect's load or frame) shows
//!   as failed there too, without being recorded as a failure of the stage.
//!
//! # Timings
//!
//! `gpu_ms` is measured with two CUDA events on the element's stream around
//! the backend's calls for the frame (every call when a frame runs again
//! without a stage), not around a temporal reset. Each stage's time is the
//! backend's `timings` step of that stage for the frame just processed,
//! read after the frame's stream synchronisation (a backend reports a frame
//! only once its work has completed), under a `try_lock` of `Engine` so the
//! streaming thread never waits for a load for it: while the worker holds
//! the lock, that frame's figures are skipped.
//!
//! # A new source
//!
//! The output clock marks the first fresh frame of each new source with
//! [`NEW_SOURCE_FLAG`](super::NEW_SOURCE_FLAG). The element then asks the
//! backend to forget its temporal state (`InferenceBackend::reset_temporal`)
//! right before the next frame it processes, so a temporal stage (noise
//! removal) does not blend frames of two cameras. A stage whose state cannot
//! be reset in place (a relighting stage, for one backend) is dropped by the
//! backend instead and loaded again fresh by the worker (a new request, the
//! stage loading meanwhile): the frames until it is ready run without it,
//! the effect alone under blur or replace (never the input), the input with
//! the stages alone gone, and no frame is processed by that stage's instance
//! that saw the previous source, so camera A's person never shows in camera
//! B's frames. A flagged frame that is not
//! processed (passed through, or black) keeps the reset pending for the
//! next processed one.
//!
//! # A repeat
//!
//! The output clock marks a tick that emits the last camera frame again
//! with [`REPEAT_FLAG`](super::REPEAT_FLAG). [`decide`] runs on it as on
//! any frame. When the frame is to be processed and the element holds the
//! output of the last processed frame, made under the same request
//! (`Control::generation`), epoch (`Control::epoch`: no apply, load, switch,
//! unload or failure since), per-frame parameters and caps, with no
//! temporal reset pending, that output is sent again and no effect runs:
//! a frame generation module downstream answers a repeat with its held
//! frame, so the work would be thrown away. Otherwise the repeat is
//! processed as any frame; black stays black, and a repeat is never passed
//! through as it came while an effect is asked.
//!
//! The cache is written only from a processed frame (`generate_output`
//! keeps a reference to the buffer pushed downstream), and dropped by any
//! frame that goes out black or as the input, the new-source flag, new
//! caps, a flush and a stop. The re-sent buffer is a new buffer on the same
//! memory, with the repeat's timestamps and flags. Downstream never writes
//! the cached memory: it is shared with the cache's buffer, so a map for
//! writing (`obcmodule` processes in place) fails to lock it and GStreamer
//! writes into a copy of it (`gst_memory_make_mapped`). That copy is the
//! price of the cache, on every processed frame a module downstream writes
//! into. A re-sent frame counts in no frame counter of the statistics.
//!
//! # The fallback image
//!
//! With `on-failure=image` and a `fallback-image`, the image is wanted for
//! the path, the file there (`background::FileStamp`: an image replaced
//! at the same path is another key, so settings applied again read it
//! again) and the negotiated size and colour (`FallbackKey`). Each new
//! key (from `apply` or `set_caps`) starts a decode on a thread of its own
//! (`obceffect-image`, [`background::decode_yuv420`]), numbered
//! (`FallbackCtl::serial`) so that a result arriving after the key changed
//! again is dropped; any other fallback, or no path, wants none. The
//! decoded NV12 is kept in `Control`, read by the streaming thread with
//! the rest of the frame's decision, uploaded once into a device frame of
//! the element's context (`GpuResources::fallback`, dropped with the other
//! CUDA objects) and copied, on the element's stream, into every frame
//! decided black and every failed frame `on-failure` hides. Until it is
//! ready, or when it cannot be read or uploaded, those frames are black
//! and the reason is reported (`FallbackCtl::error`: the statistics and the
//! status message).
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
use gst_base::subclass::base_transform::{
    GenerateOutputSuccess, InputBuffer, PrepareOutputBufferSuccess,
};
use gst_base::subclass::prelude::*;
use gst_video::VideoInfo;
use obc_backend::{
    BackendError, BackendFactory, ColourSpec, DeviceNv12, EffectKind, FrameParams, GpuContext,
    InferenceBackend, LoadReport, LoadRequest, StageId, StageTimings,
};
use obc_gpu::{ContextGuard, CuContext, CuStream, Event};

use super::{STATUS_MESSAGE, has_new_source_flag, has_repeat_flag};
use crate::background;
use crate::cuda::{self, MAP_CUDA};
use crate::settings::{
    EffectFallback, EffectSettings, EffectStats, EffectStatus, FrameAction, StageStats, Target,
    decide, fallback_unreadable, parse_effect, percentile,
};
use crate::video::{CudaFrame, colour_of, input_context};

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

/// What the backend instance has loaded.
#[derive(Debug, Clone, PartialEq)]
struct LoadedKey {
    target: Target,
    width: u32,
    height: u32,
    colour: ColourSpec,
}

/// What the fallback image is decoded for: its path and the file there,
/// and the negotiated size and colour.
#[derive(Debug, Clone, PartialEq)]
struct FallbackKey {
    path: PathBuf,
    /// The file at `path` when the key was made: an image replaced at the
    /// same path is another key, decoded again.
    file: Option<background::FileStamp>,
    width: u32,
    height: u32,
    colour: ColourSpec,
}

/// The decoded fallback image, packed NV12 of its key's size.
#[derive(Debug)]
struct FallbackFrame {
    /// The [`FallbackCtl::serial`] of its decode.
    serial: u64,
    width: u32,
    height: u32,
    nv12: Vec<u8>,
}

/// The fallback image (see "The fallback image" in the module docs).
#[derive(Debug, Default)]
struct FallbackCtl {
    /// What is decoded, or being decoded; `None`: no image is wanted.
    key: Option<FallbackKey>,
    /// Counts the decodes started: the number of the one for `key`.
    serial: u64,
    /// The image of `key`, once decoded.
    frame: Option<Arc<FallbackFrame>>,
    /// Why the image of `key` cannot be shown ([`fallback_unreadable`]).
    error: Option<String>,
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
    /// Counts every change that a processed frame depends on and
    /// `generation` does not count: any `apply` (the per-frame parameters
    /// included), a load, switch or unload published by the worker, a
    /// failure, a stage dropped or rebuilt, new caps, a stop. The cached
    /// output of a repeat (see "A repeat" in the module docs) is valid only
    /// under the epoch it was written in.
    epoch: u64,
    status: EffectStatus,
    /// The failure and status of every stage the settings have listed, in
    /// the order they were first listed (a stage stays here once listed, so
    /// that its status can be reported off).
    stages: Vec<(StageId, StageCtl)>,
    /// The parameters of each frame for the settings
    /// ([`EffectSettings::frame_params`]), shared with the streaming thread.
    frame: Arc<FrameParams>,
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
    fallback: FallbackCtl,
}

impl Default for Control {
    fn default() -> Self {
        Control {
            settings: EffectSettings::default(),
            target: Target::default(),
            generation: 0,
            epoch: 0,
            status: EffectStatus::Off,
            stages: Vec::new(),
            frame: Arc::new(FrameParams::default()),
            loaded_kind: None,
            gpu: None,
            format: None,
            factory: None,
            stop: false,
            last_load_ms: None,
            last_load_reused: false,
            loads: 0,
            fallback: FallbackCtl::default(),
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

/// The status of a stage that is not listed.
static OFF: EffectStatus = EffectStatus::Off;

impl Control {
    /// What the element knows of `stage`, if it has been listed.
    fn stage(&self, stage: &str) -> Option<&StageCtl> {
        self.stages.iter().find(|(s, _)| s == stage).map(|(_, c)| c)
    }

    /// The status of `stage` (off if it has never been listed).
    fn stage_status(&self, stage: &str) -> &EffectStatus {
        self.stage(stage).map_or(&OFF, |c| &c.status)
    }

    /// What the element knows of `stage`, added if it has not been listed.
    fn stage_mut(&mut self, stage: &StageId) -> &mut StageCtl {
        let i = match self.stages.iter().position(|(s, _)| s == stage) {
            Some(i) => i,
            None => {
                self.stages.push((stage.clone(), StageCtl::default()));
                self.stages.len() - 1
            }
        };
        &mut self.stages[i].1
    }

    /// Every stage the settings have listed, in order.
    fn stage_ids(&self) -> Vec<StageId> {
        self.stages.iter().map(|(s, _)| s.clone()).collect()
    }

    /// Whether `stage` is requested and has not failed.
    fn effective(&self, stage: &str) -> bool {
        self.target.wants(stage) && self.stage(stage).is_none_or(|c| c.failure.is_none())
    }

    /// Whether any stage is requested and has not failed.
    fn any_effective(&self) -> bool {
        self.target
            .stages
            .iter()
            .any(|(s, _)| self.effective(s.as_str()))
    }

    /// Whether a requested stage has a failure recorded.
    fn any_stage_failed(&self) -> bool {
        self.stages.iter().any(|(_, c)| c.failure.is_some())
    }

    /// What the worker loads for the request: the target without the
    /// stages that failed.
    fn job_target(&self) -> Target {
        self.target
            .stages
            .iter()
            .fold(self.target.clone(), |t, (s, _)| {
                if self.effective(s.as_str()) {
                    t
                } else {
                    t.without(s.as_str())
                }
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
        let target = &self.target;
        for (s, ctl) in &mut self.stages {
            let s = s.as_str();
            let kept = prev.is_some_and(|p| p.stage_request(s) == target.stage_request(s))
                && ctl.status == EffectStatus::Ready;
            ctl.status = match (&ctl.failure, target.wants(s)) {
                (_, false) => EffectStatus::Off,
                (Some(r), true) => EffectStatus::Failed(r.clone()),
                (None, true) if kept => EffectStatus::Ready,
                (None, true) => EffectStatus::Loading,
            };
        }
    }

    /// Records a failure of `stage`, unless the request no longer asks for
    /// the stage, one is already recorded, or (with `generation`) request
    /// `generation` is superseded. `reconcile` starts a new request so the
    /// worker brings what is loaded in line with it.
    fn record_stage_failure(
        &mut self,
        stage: &StageId,
        generation: Option<u64>,
        reason: String,
        reconcile: bool,
    ) -> bool {
        if generation.is_some_and(|g| g != self.generation)
            || !self.target.wants(stage.as_str())
            || self
                .stage(stage.as_str())
                .is_some_and(|c| c.failure.is_some())
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

    /// The path of the fallback image the settings and the caps want, if
    /// any.
    fn fallback_path(&self) -> Option<&PathBuf> {
        let path = self.settings.fallback_image.as_ref()?;
        self.format.as_ref()?;
        (self.settings.fallback == EffectFallback::Image).then_some(path)
    }

    /// The fallback image the settings and the caps want, if any, the file
    /// at its path ([`Self::fallback_path`]) being `file`.
    fn fallback_wanted(&self, file: Option<background::FileStamp>) -> Option<FallbackKey> {
        let path = self.fallback_path()?;
        let format = self.format.as_ref()?;
        Some(FallbackKey {
            path: path.clone(),
            file,
            width: format.width,
            height: format.height,
            colour: *format.colour.as_ref().unwrap_or(&format.black),
        })
    }

    fn report(&self) -> Report {
        Report {
            fallback_error: self.fallback.error.clone(),
            kind: self.settings.kind,
            status: self.status.clone(),
            stages: self
                .stages
                .iter()
                .map(|(s, c)| (s.clone(), c.status.clone()))
                .collect(),
        }
    }
}

/// What the status message carries.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    kind: EffectKind,
    status: EffectStatus,
    /// The status of every stage the settings have listed, in order.
    stages: Vec<(StageId, EffectStatus)>,
    /// Why the fallback image cannot be shown ([`FallbackCtl::error`]).
    fallback_error: Option<String>,
}

impl Report {
    /// `Some(now)` when `now` differs from `self`: a message to post. A
    /// stage newly listed, and off, changes nothing.
    fn changed(&self, now: Report) -> Option<Report> {
        let same = (self.kind, &self.status, &self.fallback_error)
            == (now.kind, &now.status, &now.fallback_error)
            && self
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

    /// `denoise loading, relight off` (the stages, for logs).
    fn stages_text(&self) -> String {
        self.stages
            .iter()
            .map(|(s, st)| format!("{s} {st}"))
            .collect::<Vec<_>>()
            .join(", ")
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
    /// The counters of every stage that has run, in the order it first ran.
    stage_figures: Mutex<Vec<(StageId, StageFigures)>>,
}

/// The counters of one stage.
#[derive(Default)]
struct StageFigures {
    /// Processed frames the stage ran on.
    frames: u64,
    /// The stage's GPU times over its last frames.
    ms: VecDeque<f32>,
}

/// Appends `v` to a window of at most [`EffectStats::WINDOW`] values.
fn push_to(w: &mut VecDeque<f32>, v: f32) {
    if w.len() == EffectStats::WINDOW {
        w.pop_front();
    }
    w.push_back(v);
}

/// Appends `v` to a window of at most [`EffectStats::WINDOW`] values.
fn push_window(w: &Mutex<VecDeque<f32>>, v: f32) {
    push_to(&mut lock(w), v);
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
    /// The fallback image uploaded, with the serial of its decode; freed
    /// in `drop`, with the context current.
    fallback: Option<(u64, obc_gpu::DeviceNv12Buffer)>,
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
                drop(self.fallback.take());
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
                // Freed with the context, like the events.
                if let Some((_, buffer)) = self.fallback.take() {
                    std::mem::forget(buffer);
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
    /// The parameters of each frame: the blur strength and the stages'
    /// live parameters.
    frame: Arc<FrameParams>,
    fallback: EffectFallback,
    /// The fallback image when the action was decided (with
    /// `on-failure=image`, once decoded).
    fallback_frame: Option<Arc<FallbackFrame>>,
    /// A frame of a new source arrived
    /// ([`NEW_SOURCE_FLAG`](super::NEW_SOURCE_FLAG)): reset the backend's
    /// temporal state before the next processed frame.
    reset_pending: bool,
    /// The epoch ([`Control::epoch`]) the action was decided under.
    epoch: u64,
    /// This frame is a repeat that gets the cached output (see "A repeat"
    /// in the module docs): no effect runs on it.
    resend: bool,
    /// `render` processed this frame: its output becomes the cache.
    processed: bool,
    /// The last processed output, while it is valid.
    cache: Option<Cached>,
    gpu: Option<GpuResources>,
}

/// The last processed output buffer, kept to answer a repeat, and what it
/// was made under.
struct Cached {
    /// A reference to the very buffer pushed downstream. Holding it keeps
    /// its memory shared, so a downstream map for writing copies the
    /// memory first and never writes this one.
    buffer: gst::Buffer,
    generation: u64,
    epoch: u64,
    /// The per-frame parameters the frame was processed with.
    frame: Arc<FrameParams>,
    caps: gst::Caps,
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
    let mut b = gst::Structure::builder(STATUS_MESSAGE)
        .field("effect", r.kind.as_str())
        .field("status", r.status.as_str())
        .field("reason", r.status.reason().unwrap_or(""))
        .field(
            "fallback-image-error",
            r.fallback_error.as_deref().unwrap_or(""),
        );
    for (stage, status) in &r.stages {
        b = b.field(stage.as_str(), status.as_str()).field(
            format!("{stage}-reason").as_str(),
            status.reason().unwrap_or(""),
        );
    }
    let s = b.build();
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

    pub(super) fn stage_status(&self, stage: &str) -> EffectStatus {
        lock(&self.shared.control).stage_status(stage).clone()
    }

    pub(super) fn stats(&self) -> EffectStats {
        let (effect, status, stage_statuses, last_load_ms, last_load_reused, loads, fallback_error) = {
            let c = lock(&self.shared.control);
            (
                c.settings.kind,
                c.status.clone(),
                c.stages
                    .iter()
                    .map(|(s, ctl)| (s.clone(), ctl.status.clone()))
                    .collect::<Vec<_>>(),
                c.last_load_ms,
                c.last_load_reused,
                c.loads,
                c.fallback.error.clone(),
            )
        };
        let gpu_window: Vec<f32> = lock(&self.shared.gpu_ms).iter().copied().collect();
        let stages = {
            let figures = lock(&self.shared.stage_figures);
            stage_statuses
                .into_iter()
                .map(|(id, status)| {
                    let f = figures.iter().find(|(s, _)| *s == id).map(|(_, f)| f);
                    let window: Vec<f32> = f
                        .map(|f| f.ms.iter().copied().collect())
                        .unwrap_or_default();
                    StageStats {
                        status,
                        frames: f.map_or(0, |f| f.frames),
                        ms_p50: percentile(&window, 50.0),
                        ms_p95: percentile(&window, 95.0),
                        id,
                    }
                })
                .collect()
        };
        let s = &self.shared;
        EffectStats {
            effect,
            status,
            stages,
            frames_processed: s.processed.load(Ordering::Relaxed),
            frames_black: s.black.load(Ordering::Relaxed),
            frames_passthrough: s.passthrough.load(Ordering::Relaxed),
            gpu_ms_p50: percentile(&gpu_window, 50.0),
            gpu_ms_p95: percentile(&gpu_window, 95.0),
            last_load_ms,
            last_load_reused,
            loads,
            cuda_active: s.cuda_active.load(Ordering::Relaxed),
            fallback_error,
        }
    }

    /// Applies new settings. A change of what has to be loaded (the effect,
    /// the image of replace, a stage request: on or off, or its load-time
    /// parameters) is a new request for the worker; `retry` makes one after
    /// a failure (of the effect or of a stage) even when nothing changed. A
    /// stage's recorded failure is forgotten when its request changes or on
    /// a retry. The effect's status turns
    /// loading (off for `none`) when the effect changes or had failed; a
    /// change of a stage request alone leaves it as it is. Likewise a ready
    /// stage whose own request is unchanged stays ready: the switch keeps
    /// it loaded, and only the stage that changed shows loading (its model
    /// load, or a new load-time parameter). A change of the per-frame
    /// parameters alone (blur strength, the stages' live parameters) is
    /// never a retry: they load nothing, and a slider moved after
    /// a failure would otherwise reload what failed on every step.
    pub(super) fn apply(&self, new: EffectSettings, retry: bool) {
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
            let retry = retry && !c.settings.changes_frame_params_only(&new);
            let before = c.report();
            let target = new.target();
            let effect_failed = matches!(c.status, EffectStatus::Failed(_));
            let stage_failed = c.any_stage_failed();
            let effect_changed =
                (target.kind, &target.background) != (c.target.kind, &c.target.background);
            for st in &new.stages {
                c.stage_mut(&st.id);
            }
            c.frame = Arc::new(new.frame_params());
            c.settings = new;
            if target != c.target || (retry && (effect_failed || stage_failed)) {
                let old = c.target.clone();
                for (s, ctl) in &mut c.stages {
                    if retry || target.stage_request(s.as_str()) != old.stage_request(s.as_str()) {
                        ctl.failure = None;
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
                "effect {}: {}; stages: {}",
                r.kind,
                r.status,
                r.stages_text()
            );
            post_status(&self.obj(), &r);
        }
        self.refresh_fallback();
    }

    /// Starts decoding the fallback image the settings and caps now want
    /// (see "The fallback image" in the module docs), forgetting the
    /// previous one, when that is another; posts when its report changes.
    fn refresh_fallback(&self) {
        // The file at the path is stat'ed with the control lock released:
        // `stat` opens nothing, but it can hang on a stalled network or
        // FUSE mount, and the streaming thread takes the lock every frame.
        let path = lock(&self.shared.control).fallback_path().cloned();
        let file = path.as_deref().and_then(background::FileStamp::of);
        let (start, posted) = {
            let mut c = lock(&self.shared.control);
            // Another path is wanted now (new settings or caps): the
            // refresh that follows them stats it.
            if c.fallback_path() != path.as_ref() {
                return;
            }
            let want = c.fallback_wanted(file);
            if want == c.fallback.key {
                return;
            }
            let before = c.report();
            c.epoch += 1;
            c.fallback.serial += 1;
            c.fallback.key = want.clone();
            c.fallback.frame = None;
            c.fallback.error = None;
            (
                want.map(|k| (k, c.fallback.serial)),
                before.changed(c.report()),
            )
        };
        if let Some(r) = posted {
            post_status(&self.obj(), &r);
        }
        let Some((key, serial)) = start else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let element = self.obj().downgrade();
        let k = key.clone();
        let spawned = std::thread::Builder::new()
            .name("obceffect-image".into())
            .spawn(move || {
                let res = background::decode_yuv420(&k.path, k.width, k.height, k.colour);
                let frame = res.map(|img| FallbackFrame {
                    serial,
                    width: img.width,
                    height: img.height,
                    nv12: img.nv12_packed(),
                });
                fallback_decoded(&shared, &element, serial, &k.path, frame);
            });
        if let Err(e) = spawned {
            let why = format!("no thread to decode it: {e}");
            fallback_decoded(
                &self.shared,
                &self.obj().downgrade(),
                serial,
                &key.path,
                Err(why),
            );
        }
    }

    /// The fallback image of decode `serial` could not be uploaded (`why`):
    /// black in its place from now on, reported (unless superseded).
    fn fallback_upload_failed(&self, serial: u64, why: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            if c.fallback.serial != serial || c.fallback.frame.is_none() {
                return;
            }
            let before = c.report();
            c.epoch += 1;
            c.fallback.frame = None;
            c.fallback.error = c
                .fallback
                .key
                .as_ref()
                .map(|k| fallback_unreadable(&k.path, &format!("cannot upload it: {why}")));
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::warning!(
                CAT,
                imp = self,
                "{}",
                r.fallback_error.as_deref().unwrap_or("")
            );
            post_status(&self.obj(), &r);
        }
    }

    /// Fills `dst` with what hides the input: the fallback image `image`
    /// when there is one of `dst`'s size (uploaded into `gpu` once per
    /// decode), else black of `black`; on `stream`.
    fn fill_hidden(
        &self,
        gpu: &mut GpuResources,
        image: Option<&Arc<FallbackFrame>>,
        dst: &DeviceNv12,
        black: ColourSpec,
        stream: CuStream,
    ) -> Result<(), String> {
        if let Some(img) = image.filter(|i| (i.width, i.height) == (dst.width, dst.height)) {
            if gpu.fallback.as_ref().is_none_or(|(s, _)| *s != img.serial) {
                // The old frame first (the context is current).
                gpu.fallback = None;
                let up = obc_gpu::DeviceNv12Buffer::alloc(img.width, img.height)
                    .and_then(|b| b.upload_packed(&img.nv12).map(|()| b));
                match up {
                    Ok(b) => gpu.fallback = Some((img.serial, b)),
                    Err(e) => self.fallback_upload_failed(img.serial, e.to_string()),
                }
            }
            if let Some((_, b)) = gpu.fallback.as_ref().filter(|(s, _)| *s == img.serial) {
                // SAFETY: both frames are live allocations of our context,
                // of the same size; the image is only read.
                return unsafe { obc_gpu::copy_nv12_async(&b.frame(), dst, stream) }
                    .map_err(|e| e.to_string());
            }
        }
        // SAFETY: `dst` is our mapped output frame; `stream` is ours.
        unsafe { obc_gpu::fill_black_nv12(dst, black, stream) }.map_err(|e| e.to_string())
    }

    /// Marks request `generation` as failed (unless superseded), and posts.
    /// A requested stage that has not failed itself shows the failure too:
    /// it runs in the failed chain.
    fn fail(&self, generation: u64, reason: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
            if c.generation != generation || matches!(c.status, EffectStatus::Failed(_)) {
                None
            } else {
                let before = c.report();
                for s in c.stage_ids() {
                    if c.effective(s.as_str()) {
                        c.stage_mut(&s).status = EffectStatus::Failed(reason.clone());
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
    fn fail_stage(&self, stage: &StageId, generation: Option<u64>, reason: String) {
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
            let before = c.report();
            let reconcile = !matches!(c.status, EffectStatus::Failed(_));
            if c.record_stage_failure(stage, generation, reason, reconcile) && reconcile {
                self.shared.wake.notify_all();
            }
            before.changed(c.report())
        };
        if let Some(r) = posted {
            gst::warning!(CAT, imp = self, "{stage}: {}", r.stage(stage.as_str()));
            post_status(&self.obj(), &r);
        }
    }

    /// Starts a new request so the worker loads `stages` again: the
    /// backend dropped them on a temporal reset (not a failure), so the
    /// chain loaded is without them. Each one still requested and not
    /// failed shows loading until the worker has it ready again; posts.
    fn rebuild_stages(&self, stages: &[StageId]) {
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
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
        if let Some(r) = posted {
            gst::debug!(
                CAT,
                imp = self,
                "a new source: loading again {stages:?}; stages: {}",
                r.stages_text()
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
                fallback: None,
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
        let (gpu, cached) = {
            let mut st = lock(&self.stream);
            (st.gpu.take(), st.cache.take())
        };
        // The cached output is memory of our pool and context: gone first.
        drop(cached);
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
        c.epoch += 1;
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
        // Whatever this frame becomes, the previous output is no longer the
        // last one.
        st.processed = false;
        st.cache = None;
        let lib = cuda::gst_cuda()?;
        let info = st.info.clone().ok_or("no caps negotiated")?;
        let format = st.format.clone().ok_or("no caps negotiated")?;
        // What hides the input: the fallback image with `image`, else black.
        let image = match st.fallback {
            EffectFallback::Image => st.fallback_frame.clone(),
            _ => None,
        };
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
        let mut processed = false;
        // The stages that ran on the processed frame.
        let mut ran: Vec<StageId> = Vec::new();
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
            FrameAction::Black => {
                self.fill_hidden(gpu, image.as_ref(), &dst, format.black, stream)?
            }
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
                let params = Arc::clone(&st.frame);
                let ctx = gpu.gpu_context();
                // The stages alone hide nothing: an effect frame needs a
                // chain with the effect. A frame of `none` needs a chain
                // with something in it (a stage, or the previous effect
                // while switching).
                let alone = st.kind == EffectKind::None;
                // The stages that failed on this frame (the backend has
                // dropped them), whatever the frame then got.
                let mut stage_failures: Vec<(StageId, String)> = Vec::new();
                // The stages the temporal reset dropped, to load again.
                let mut rebuild: Vec<StageId> = Vec::new();
                let mut e = lock(&self.shared.engine);
                let Engine { instance, loaded } = &mut *e;
                // A backend that lost its chain on its own (a module
                // instance unloaded after a panic) runs nothing: as if
                // unloaded since the decision.
                let usable = loaded.as_ref().is_some_and(|k| {
                    k.target.kind != EffectKind::None || (alone && k.target.has_stages())
                }) && instance.as_ref().is_some_and(|i| i.is_loaded());
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
                                for s in dropped.as_slice() {
                                    if k.target.wants(s.as_str()) {
                                        k.target = k.target.without(s.as_str());
                                        rebuild.push(s.clone());
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
                            let Some(k) = loaded.as_mut() else { break };
                            let failed: Vec<StageId> = match stage_of(err) {
                                Some(s) if k.target.wants(s.as_str()) => vec![s],
                                Some(_) => Vec::new(),
                                None if alone => {
                                    k.target.stages.iter().map(|(s, _)| s.clone()).collect()
                                }
                                None => break,
                            };
                            if failed.is_empty() {
                                break;
                            }
                            for s in failed {
                                k.target = k.target.without(s.as_str());
                                let reason = format!("the {s} stage failed on a frame: {err}");
                                stage_failures.push((s, reason));
                            }
                            if alone {
                                break;
                            }
                            r = instance.process(&ctx, &params, &src, &dst);
                        }
                        if r.is_ok()
                            && let Some(k) = loaded.as_ref()
                        {
                            ran = k.target.stages.iter().map(|(s, _)| s.clone()).collect();
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
                for (stage, reason) in &stage_failures {
                    // Recorded whichever request the frame was decided for:
                    // the stage is gone from the loaded chain, and a request
                    // that still asks for it must learn it now (the first
                    // frame after turning the stage on was decided under the
                    // previous request, and the worker has already handled
                    // the new one).
                    self.fail_stage(stage, None, reason.clone());
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
                        EffectFallback::Black | EffectFallback::Image => {
                            self.fill_hidden(gpu, image.as_ref(), &dst, format.black, stream)?
                        }
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
                    None => self.fill_hidden(gpu, image.as_ref(), &dst, format.black, stream)?,
                }
            }
        }
        sync_on_exit.disarm();
        // SAFETY: our own stream. Waiting here also ends this frame's read
        // of the input (see the module docs for why).
        unsafe { obc_gpu::stream_synchronize(stream) }.map_err(|e| e.to_string())?;
        drop(input_frame);
        drop(out);
        // Only a processed frame is cached (`generate_output`); black or
        // the input leaves no cache behind.
        st.processed = processed;
        if processed {
            self.shared.processed.fetch_add(1, Ordering::Relaxed);
            if !ran.is_empty() {
                let times = self.stage_times();
                let mut figures = lock(&self.shared.stage_figures);
                for id in ran {
                    let i = match figures.iter().position(|(s, _)| *s == id) {
                        Some(i) => i,
                        None => {
                            figures.push((id.clone(), StageFigures::default()));
                            figures.len() - 1
                        }
                    };
                    let f = &mut figures[i].1;
                    f.frames += 1;
                    if let Some(ms) = times.as_ref().and_then(|t| t.step_ms(id.as_str()))
                        && ms > 0.0
                    {
                        push_to(&mut f.ms, ms);
                    }
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

    fn stage(&self) -> Option<StageId> {
        self.error.as_ref().and_then(stage_of)
    }
}

/// The stage `err` is the failure of alone, the rest of the chain intact.
fn stage_of(err: &BackendError) -> Option<StageId> {
    match err {
        BackendError::Stage { stage, .. } => Some(stage.clone()),
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
    stage_failures: Vec<(StageId, String)>,
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
        let in_force = |s: &str| in_force.as_ref().is_some_and(|t| t.wants(s));
        let posted = {
            let mut c = lock(&shared.control);
            let before = c.report();
            c.loaded_kind = loaded_kind;
            c.epoch += 1;
            if let Outcome::Loaded(r) = &outcome {
                c.loads += 1;
                c.last_load_ms = Some(r.load_ms + r.warmup_ms.unwrap_or(0.0));
                c.last_load_reused = r.reused_model;
            }
            if c.generation == job.generation {
                for (stage, reason) in &stage_failures {
                    c.record_stage_failure(stage, Some(job.generation), reason.clone(), false);
                }
                let effect = c.target.kind != EffectKind::None;
                match &outcome {
                    Outcome::Unloaded | Outcome::Superseded => {}
                    Outcome::Loaded(_) | Outcome::AlreadyLoaded => {
                        if effect {
                            c.status = EffectStatus::Ready;
                        }
                        for s in c.stage_ids() {
                            if c.effective(s.as_str()) && in_force(s.as_str()) {
                                c.stage_mut(&s).status = EffectStatus::Ready;
                            }
                        }
                    }
                    Outcome::Failed(f) => {
                        if effect {
                            c.status = EffectStatus::Failed(f.reason.clone());
                        }
                        for s in c.stage_ids() {
                            if c.effective(s.as_str()) {
                                c.stage_mut(&s).status = EffectStatus::Failed(f.reason.clone());
                            }
                        }
                    }
                }
                let target = c.target.clone();
                for (s, ctl) in &mut c.stages {
                    if !target.wants(s.as_str()) {
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

/// The decode `serial` of the fallback image at `path` ended with `frame`:
/// kept (or its reason) unless a later decode started; posts when the
/// report changes.
fn fallback_decoded(
    shared: &Shared,
    element: &glib::WeakRef<super::ObcEffect>,
    serial: u64,
    path: &std::path::Path,
    frame: Result<FallbackFrame, String>,
) {
    let posted = {
        let mut c = lock(&shared.control);
        if c.fallback.serial != serial {
            return;
        }
        let before = c.report();
        c.epoch += 1;
        match frame {
            Ok(f) => c.fallback.frame = Some(Arc::new(f)),
            Err(why) => c.fallback.error = Some(fallback_unreadable(path, &why)),
        }
        before.changed(c.report())
    };
    if let Some(el) = element.upgrade() {
        match &posted {
            Some(r) if r.fallback_error.is_some() => {
                gst::warning!(
                    CAT,
                    obj = el,
                    "{}",
                    r.fallback_error.as_deref().unwrap_or("")
                )
            }
            _ => gst::debug!(CAT, obj = el, "fallback image {} decoded", path.display()),
        }
        if let Some(r) = posted {
            post_status(&el, &r);
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
                        || f.stage().is_some_and(|s| target.wants(s.as_str()))) =>
            {
                let failed: Vec<StageId> = match f.stage() {
                    Some(s) if target.wants(s.as_str()) => vec![s],
                    _ => target.stages.iter().map(|(s, _)| s.clone()).collect(),
                };
                for s in failed {
                    target = target.without(s.as_str());
                    stage_failures.push((s, f.reason.clone()));
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
    {
        let mut c = lock(&shared.control);
        c.loaded_kind = None;
        c.epoch += 1;
    }
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
    let current = {
        let mut e = lock(&shared.engine);
        // A backend that lost its chain on its own (a module instance
        // unloaded after a panic) has nothing loaded, whatever was last.
        if e.loaded.is_some() && !e.instance.as_ref().is_some_and(|i| i.is_loaded()) {
            e.loaded = None;
        }
        e.loaded.clone()
    };
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
        let mut c = lock(&shared.control);
        c.loaded_kind = None;
        c.epoch += 1;
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
        params: target.stage_params(),
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
                k.target = k.target.without(stage.as_str());
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
                    .blurb(
                        "Output after a failure: black | image (the fallback image) | none \
                         (the unprocessed input)",
                    )
                    .default_value(Some("black"))
                    .flags(rw)
                    .build(),
                glib::ParamSpecString::builder("fallback-image")
                    .nick("Fallback image")
                    .blurb(
                        "Absolute path of the image shown in place of black with \
                         on-failure=image (empty: none, black)",
                    )
                    .default_value(Some(""))
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
            "fallback-image" => {
                let t = text();
                let p = (!t.is_empty()).then(|| PathBuf::from(t));
                if let Some(p) = &p
                    && !p.is_absolute()
                {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "ignoring fallback-image {}: not an absolute path",
                        p.display()
                    );
                    return;
                }
                s.fallback_image = p;
                false
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
            "fallback-image" => s
                .fallback_image
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
                .to_value(),
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
                "Background blur or replacement, and stackable stages such as video noise \
                 removal, on NV12 CUDA frames through an inference backend; passthrough \
                 without either",
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
            c.epoch += 1;
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
        st.resend = false;
        st.cache = None;
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
            if st.caps.as_ref() != Some(incaps) {
                st.cache = None;
            }
            st.info = Some(info);
            st.caps = Some(incaps.clone());
            st.format = Some(format.clone());
        }
        let posted = {
            let mut c = lock(&self.shared.control);
            c.epoch += 1;
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
        // The fallback image is decoded for the size and colour.
        self.refresh_fallback();
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
        let mut st = lock(&self.stream);
        if st.action == FrameAction::Passthrough {
            return Ok(PrepareOutputBufferSuccess::InputBuffer);
        }
        if st.resend {
            match st.cache.as_ref() {
                Some(cached) => {
                    // A new buffer on the cached memory (shared, not
                    // copied), with the repeat's timestamps and flags.
                    let mut out = cached.buffer.copy();
                    drop(st);
                    let outref = out.get_mut().ok_or(gst::FlowError::Error)?;
                    self.parent_copy_metadata(inbuf, outref)
                        .map_err(|_| gst::FlowError::Error)?;
                    return Ok(PrepareOutputBufferSuccess::Buffer(out));
                }
                // Not reachable (the cache goes only between frames); the
                // repeat is processed then.
                None => st.resend = false,
            }
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
        if st.resend {
            // The cached output, already in `outbuf`.
            return Ok(gst::FlowSuccess::Ok);
        }
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

    fn generate_output(&self) -> Result<GenerateOutputSuccess, gst::FlowError> {
        let out = self.parent_generate_output()?;
        let mut st = lock(&self.stream);
        if std::mem::take(&mut st.processed)
            && let GenerateOutputSuccess::Buffer(buffer) = &out
            && let Some(caps) = st.caps.clone()
        {
            st.cache = Some(Cached {
                buffer: buffer.clone(),
                generation: st.generation,
                epoch: st.epoch,
                frame: Arc::clone(&st.frame),
                caps,
            });
        }
        st.resend = false;
        Ok(out)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if event.type_() == gst::EventType::FlushStop {
            let mut st = lock(&self.stream);
            st.cache = None;
            st.resend = false;
        }
        self.parent_sink_event(event)
    }
}

impl ObcEffect {
    /// Decides what the frame gets and sets BaseTransform's passthrough to
    /// match, creating the CUDA objects when the frame needs them (or the
    /// stages alone need them loaded: they pass frames through until then).
    fn prepare_frame(&self, inbuf: &gst::BufferRef) {
        let (kind, status, fallback, loaded, stages, frame, generation, epoch, fallback_frame) = {
            let c = lock(&self.shared.control);
            (
                c.settings.kind,
                c.status.clone(),
                c.settings.fallback,
                c.usable(),
                c.any_effective(),
                Arc::clone(&c.frame),
                c.generation,
                c.epoch,
                c.fallback.frame.clone(),
            )
        };
        let mut action = decide(kind, &status, fallback, loaded, stages);
        let needs_gpu = action != FrameAction::Passthrough || (kind == EffectKind::None && stages);
        let mut st = lock(&self.stream);
        if needs_gpu && let Err(reason) = self.ensure_gpu(&mut st, inbuf) {
            drop(st);
            if kind == EffectKind::None {
                // Only the stages needed CUDA: drop them, pass through.
                let requested: Vec<StageId> = {
                    let c = lock(&self.shared.control);
                    c.target.stages.iter().map(|(s, _)| s.clone()).collect()
                };
                for s in &requested {
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
            st.cache = None;
        }
        // A frame decided black or the input ends the cache; a repeat to
        // process gets the cached output when it is still valid.
        if action != FrameAction::Process {
            st.cache = None;
        }
        let valid = st.cache.as_ref().is_some_and(|c| {
            c.generation == generation
                && c.epoch == epoch
                && Arc::ptr_eq(&c.frame, &frame)
                && st.caps.as_ref() == Some(&c.caps)
        });
        if !valid {
            st.cache = None;
        }
        st.resend = action == FrameAction::Process
            && has_repeat_flag(inbuf)
            && !st.reset_pending
            && st.cache.is_some();
        st.action = action;
        st.generation = generation;
        st.epoch = epoch;
        st.kind = kind;
        st.frame = frame;
        st.fallback = fallback;
        st.fallback_frame = fallback_frame;
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
    use super::{ColourSpec, Control, EffectFallback, Format, OnExit, background};
    use std::cell::Cell;
    use std::path::Path;

    /// The fallback key `c` wants for the file now at `path`, as
    /// `refresh_fallback` builds it.
    fn key_now(c: &Control, path: &Path) -> Option<super::FallbackKey> {
        c.fallback_wanted(background::FileStamp::of(path))
    }

    #[test]
    fn a_fallback_image_replaced_at_its_path_is_another_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("away.png");
        std::fs::write(&path, b"one").unwrap();
        let mut c = Control::default();
        c.settings.fallback = EffectFallback::Image;
        c.settings.fallback_image = Some(path.clone());
        assert_eq!(key_now(&c, &path), None, "no caps yet");
        c.format = Some(Format {
            width: 64,
            height: 48,
            colour: Ok(ColourSpec::LIMITED_BT601),
            black: ColourSpec::LIMITED_BT601,
        });
        assert_eq!(c.fallback_path(), Some(&path));
        let first = key_now(&c, &path).unwrap();
        assert_eq!(key_now(&c, &path), Some(first.clone()), "unchanged file");
        // Another file renamed onto the path, as an editor saves.
        let next = dir.path().join("next.png");
        std::fs::write(&next, b"two").unwrap();
        std::fs::rename(&next, &path).unwrap();
        let second = key_now(&c, &path).unwrap();
        assert_ne!(second, first, "replaced file");
        assert_eq!(second.path, first.path);
        assert_eq!(key_now(&c, &path), Some(second), "unchanged again");
        // Black wanted: no image, no key.
        c.settings.fallback = EffectFallback::Black;
        assert_eq!(c.fallback_path(), None);
        assert_eq!(key_now(&c, &path), None);
    }

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
