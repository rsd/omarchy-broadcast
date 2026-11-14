//! The Output stage: one long-lived pipeline per sink, paced by its own
//! clock (`docs/adr/0009-ingest-process-output.md`).
//!
//! ```text
//! appsrc        <canonical caps>,framerate=<contract>  is-live format=time
//!   [ ! Process stage ]                                 crate::process
//!   [ ! obcmodule ... ]  placed before the effect      crate::module
//!   [ ! obceffect ]                                     crate::effect
//!   [ ! obcmodule ... ]  placed after the effect        crate::module
//!   [ ! cudaconvert ! capsfilter (CUDAMemory) I420 ]    contract format != NV12
//!   ! cudadownload
//!   ! capsfilter  <contract caps>, canonical label      system memory
//!   ! capssetter  colorimetry=<sink label>              relabel only
//!   ! capsfilter  <contract caps>
//!   ! v4l2sink device=<loopback> sync=false             (or fakesink, or an
//!                                                         appsink forwarding to
//!                                                         a loopback writer)
//! ```
//!
//! The caps are fixed when the pipeline is built and never change, whatever
//! feeds the slot. A dedicated clock thread pushes one buffer per tick of the
//! contract frame rate ([`crate::clock::Pacer`]): the newest canonical frame
//! (fresh), the last one again (repeat), or a placeholder: a black frame
//! prepared once at start in CUDA memory, or the placeholder image (below).
//! A pushed frame is a shallow copy
//! of the slot's buffer, restamped (PTS = tick x period): the CUDA memory is
//! shared, never copied. `v4l2sink` runs with `sync=false` because the
//! pacing is the clock thread's.
//!
//! **Queue.** `appsrc` holds at most [`OutputConfig::queue_buffers`]
//! buffers (default 3) and drops the oldest beyond that (`leaky-type`
//! downstream). The clock never emits more ticks back to back than that
//! while catching up ([`PacerConfig::max_burst`] is the same bound), so a
//! catch-up burst is queued whole and a longer stall resynchronises the
//! clock instead. A sink slower than the contract rate therefore never sees
//! a frame older than about the queue length times its own service time:
//! the queue cannot build up a backlog of stale frames. A buffer `appsrc`
//! drops is counted in [`OutputCounts::queue_dropped`], and the highest
//! queue level seen after a push in [`OutputCounts::queue_level_max`].
//!
//! **Phase lock and hold.** The clock emits each tick shortly after a
//! camera frame is expected ([`crate::clock`]), and during a deliberate
//! source switch it holds the last frame ([`Output::hold_for`], armed by
//! [`crate::session::Session`]) instead of sending placeholders after the
//! gap threshold.
//!
//! **Colour label at the sink.** The frames keep the canonical label
//! ([`OutputContract::colorimetry`]) through every CUDA stage, and are
//! relabelled [`SINK_COLORIMETRY`] after download, without touching a
//! pixel. v4l2loopback stores no colour description of its own: it reports
//! every stream back as BT.601 (`2:4:5:4`), whatever the producer set, and
//! while a consumer holds the device it refuses any other label. A
//! producer labelling its frames otherwise could start on a free device but
//! not restart while a consumer holds it.
//!
//! **Colour range at a loopback.** Since the device carries no range, a
//! consumer reads every stream as limited range. [`Output::build`] therefore
//! refuses a full-range contract for a loopback sink
//! ([`check_sink_range`]); other sinks (`fakesink`) accept both ranges.
//!
//! **Restart.** [`Output::stop`] then [`Output::start`] resumes the
//! counters: the clock carries on from the counts and the slot sequence
//! reached, so no counter goes back and frames written while the output
//! was stopped are not counted as superseded.
//!
//! **Effect.** With [`OutputConfig::effect`] set, the Process stage holds
//! the `obceffect` element ([`crate::effect`]) for the output's whole life,
//! and [`Output::set_effect`] changes its effect while it streams, without
//! renegotiating caps; [`Output::effect_status`] and
//! [`Output::effect_stats`] report on it, and every status change is posted
//! on the output pipeline's bus ([`crate::effect::STATUS_MESSAGE`]). The
//! element sits after the clock, so a repeated frame is processed again.
//! It does not combine with [`GpuStage::Roundtrip`] (which stands in for an
//! effect). It takes frames in either memory: on CUDA frames, when the
//! output stops, the element unloads its backend (with the context current)
//! before the pipeline releases the CUDA context; on system-memory frames
//! it makes no CUDA call, and an effect whose backend takes CUDA frames
//! only fails with that reason (the element's fallback decides what is
//! shown).
//!
//! **Module stages.** Each entry of [`OutputConfig::module_stages`] adds
//! one `obcmodule` element ([`crate::module`]) for the output's whole life,
//! before or after the effect element as the entry says (where the effect
//! element would be, without one), and [`Output::set_module_stages`]
//! changes its stages while it streams, without renegotiating caps;
//! [`Output::module_stage_status`] and [`Output::module_stats`] report on
//! it, and its status changes are posted on the output pipeline's bus
//! ([`crate::module::STATUS_MESSAGE`]). Unlike the effect element, these
//! run on system memory too (a chain that takes host frames), and need no
//! empty Process stage. None by default: no element, no cost.
//!
//! **New source.** The clock marks the first fresh frame it emits from each
//! source (the first after a source switch, and the first of the first
//! source) with [`gst_obc::NEW_SOURCE_FLAG`], and clears the flag on every
//! other buffer, so the effect element resets the temporal state of its
//! backend (the denoise stage's, `docs/adr/0018-video-noise-removal.md`)
//! instead of blending two cameras. `DISCONT` cannot carry this: the clock
//! sets it after every skipped tick too.
//!
//! **Repeat.** Likewise the clock marks every repeat tick (the last frame
//! emitted again) with [`gst_obc::REPEAT_FLAG`] and clears it on every
//! other buffer, so a module stage can tell a repeat from a fresh frame
//! (`obcmodule` hands it to its chain as one).
//!
//! **Placeholder image flag.** A placeholder tick that pushes the
//! placeholder image (below) is marked with [`gst_obc::PLACEHOLDER_FLAG`];
//! the black placeholder and every camera frame are not. The effect element
//! and every module stage pass a marked frame downstream untouched (no
//! replace, blur, crop, warm-up black or fallback frame, no backend or
//! chain call, none of their state changed), so the image reaches the sink
//! as it was decoded. The black placeholder still runs through them as
//! before.
//!
//! **CUDA context.** The output pipeline's CUDA elements create the
//! `GstCudaContext` when the pipeline opens; [`Output::cuda_context`] hands
//! it to every ingest pipeline so canonical frames stay in one context.
//!
//! **System memory.** [`Output::build_in`] with [`VideoMemory::System`]
//! carries canonical frames in system memory, with no CUDA element and no
//! CUDA context:
//!
//! ```text
//! appsrc        <canonical caps, system memory>,framerate=<contract>
//!   [ ! obcmodule ... ] [ ! obceffect ] [ ! obcmodule ... ]
//!   [ ! videoconvert ]                                  contract format != NV12
//!   ! capsfilter  <contract caps>, canonical label
//!   ! capssetter  colorimetry=<sink label>
//!   ! capsfilter  <contract caps>
//!   ! <sink>
//! ```
//!
//! The black placeholder (and the placeholder image) is made on the CPU.
//! The Process stage must be empty ([`GpuStage::Roundtrip`] runs on CUDA),
//! and every module stage's chain must take host frames.
//!
//! **Forwarding to a loopback writer.** With [`OutputSink::Writer`] the
//! chain is the same up to and including the relabel and the contract
//! capsfilter, and ends in an `appsink` whose callback hands every buffer
//! to the writer ([`crate::loopback::WriterHandle::push`]) by reference: the
//! writer, not this output, owns the device, restamps the frames and runs
//! the loopback check ([`crate::loopback`]). The writer's contract must be
//! this output's. While streaming (from [`Output::start`] to its stop,
//! drop included) the output is attached to the writer
//! ([`crate::loopback::WriterHandle::attach`], bound the shorter of
//! [`OutputConfig::hold_timeout`] and [`Output::STALL_HOLD`]): a stall of
//! this output's streaming thread (a deliberate switch of the effect, a
//! module loading) is covered by the writer repeating the last frame
//! forwarded, not by its placeholder. A stall past the bound (a wedged
//! output) shows the placeholder. A stopped output is detached, so a
//! camera turned off shows the placeholder, never a frozen frame.
//!
//! **Placeholder image.** The placeholder can show a still image instead
//! of black (the "away image"): [`OutputConfig::placeholder_image`] at
//! build, [`Output::set_placeholder_image`] at any time after, with the
//! semantics of the loopback writer's placeholder
//! ([`crate::loopback::LoopbackWriter::set_placeholder_image`]) and its
//! [`PlaceholderState`] ([`Output::placeholder_state`]). The image is
//! cropped to fill the canonical frame, in the canonical size and colour
//! ([`image_canonical_frame`]), decoded on the CPU on a thread of its own;
//! the placeholder is black until it is ready, and stays black when the
//! image cannot be read ([`PlaceholderState::Failed`]). An output that
//! forwards to a loopback writer ([`OutputSink::Writer`]) pushes nothing
//! at all on a placeholder tick while its image is
//! [`PlaceholderState::Pending`] (from its start, while it decodes or
//! uploads the image), instead of black: the writer's own placeholder, the
//! same away image already decoded, fills the slot (nothing forwarded and
//! no frame kept), so a session start shows the image and never black in
//! between. Such a tick counts in [`OutputCounts::placeholder`], not in
//! [`OutputCounts::emitted`]. Any other sink gets black meanwhile.
//! Switching back to black is immediate. The last image decoded is kept, across black and
//! across stops, until another path is asked for: the same file at the same
//! path ([`FileStamp`]) is shown again without a decode. On a CUDA output
//! the decoded frame is uploaded once per start into a CUDA buffer of the
//! output's context (the one-shot pipeline the black frame is uploaded
//! with); in system memory it is pushed as decoded. The clock thread only
//! takes a reference to the frame in force, under a lock held for an
//! assignment: it never decodes or uploads. The image asked for is the
//! output's: kept across [`Output::stop`] and [`Output::start`]. While the
//! output is stopped nothing is decoded or uploaded, so a stopped output
//! never wakes the GPU; the decode (or, for an image already decoded, the
//! upload) happens at the next start, and the stop drops every CUDA frame
//! with the context. The stop never waits for an upload: one still in
//! flight on a decode thread (a few milliseconds, at most the upload's
//! five-second timeout on a wedged GPU) finishes with its own reference to
//! the context, and its frame is dropped, never shown. A new [`Output`]
//! starts from its
//! [`OutputConfig::placeholder_image`]: an owner that builds one per
//! session sets it there, and calls [`Output::set_placeholder_image`] when
//! the image changes.
//!
//! **Loopback check.** [`Output::start`] compares what the loopback accepts
//! with the contract once, before streaming
//! ([`crate::pipeline::MediaPipeline::prepare`]).
//!
//! **Errors.** An ingest pipeline that fails only stops feeding the slot:
//! the output keeps its rate, repeating and then sending placeholders. The
//! output pipeline's own bus is the owner's to watch
//! ([`Output::pipeline`]), and so is the clock thread: if it ends without
//! being stopped (a panic, e.g. in the [`EmitObserver`]), nothing is
//! emitted any more, [`Output::is_running`] turns false and
//! [`Output::health`] reports why.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gst::prelude::*;
use gst_obc::background::FileStamp;
use obc_core::{ColourRange, OutputContract, OutputFormat};
use serde::Serialize;

use crate::clock::{Latest, Pacer, PacerConfig, PacerCounts, PhaseLockConfig, Step, TickKind};
use crate::effect::{
    EffectBackend, EffectSettings, EffectStats, EffectStatus, effect_element_with_video,
};
use crate::frame::{Canonical, Frame, FrameWriter};
use crate::ingest::CUDA_CONTEXT_TYPE;
use crate::loopback::{PlaceholderState, WriterAttachment, WriterHandle, image_contract_frame};
use crate::module::{ModulePlacement, ModuleStageSpec, ModuleStats, check_specs, module_element};
use crate::pipeline::{
    MediaPipeline, PipelineError, StopError, add_linked, both_stopped, capsfilter, cuda_caps, make,
    names, not_null,
};
use crate::process::{GpuStage, process_elements};
use crate::slot::Slot;
use obc_core::config::VideoMemory;

/// The [`PipelineError::Cuda`] message of a CUDA output whose elements
/// reached PAUSED without opening a CUDA context ([`Output::start`]): CUDA
/// is not usable.
pub const NO_CUDA_CONTEXT: &str = "the output pipeline opened no CUDA context";

/// Where the output pipeline delivers its frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSink {
    /// A v4l2loopback device node.
    Loopback(PathBuf),
    /// `fakesink`: frames are downloaded and discarded (tests, measurements
    /// without a loopback).
    Fake,
    /// An `appsink` forwarding every frame to a persistent loopback writer
    /// of the same contract (see the module docs).
    Writer(WriterHandle),
}

/// Everything needed to build the output pipeline.
#[derive(Debug, Clone)]
pub struct OutputConfig {
    pub contract: OutputContract,
    pub sink: OutputSink,
    pub process: GpuStage,
    /// Input age beyond which placeholders replace repeats.
    pub gap_threshold: Duration,
    /// How far behind its schedule the clock may fall before it
    /// resynchronises.
    pub resync_after: Duration,
    /// Buffers `appsrc` holds before dropping the oldest; also the longest
    /// catch-up burst of the clock.
    pub queue_buffers: u32,
    /// How long a source switch holds the last frame while waiting for the
    /// new source ([`Output::hold_for`]); also, up to
    /// [`Output::STALL_HOLD`], how long a loopback writer this output
    /// forwards to holds its last frame while it sends none
    /// ([`crate::loopback::WriterHandle::attach`]).
    pub hold_timeout: Duration,
    /// The output clock's phase lock ([`crate::clock`]).
    pub phase_lock: PhaseLockConfig,
    /// The effect element of the Process stage ([`crate::effect`]):
    /// `None` (the default) builds no element, and the output cannot take
    /// an effect later; `Some` inserts `obceffect` with these settings
    /// (`EffectSettings::default()`: passthrough until
    /// [`Output::set_effect`], at no cost). The daemon always sets it, with
    /// or without [`Self::effect_backend`], so that an effect asked of a
    /// session no module can run follows its fallback. Does not combine
    /// with [`GpuStage::Roundtrip`].
    pub effect: Option<EffectSettings>,
    /// The backend the effect element creates its instance with: none by
    /// default, and there is no built-in one (the daemon's comes from the
    /// module that provides the `background` stage). Without one the
    /// element passes frames through under `none` and fails any other
    /// request with the no-backend reason, its fallback deciding what it
    /// shows. It can be taken away and set again while the output runs
    /// ([`Output::revoke_effect_backend`], [`Output::set_effect_backend`]).
    /// Unused without [`Self::effect`].
    pub effect_backend: Option<EffectBackend>,
    /// The video background of replace the effect element starts with
    /// ([`gst_obc::ObcEffect::set_background_video`]; the settings'
    /// `background` is then its load-time still): none by default.
    /// [`Output::set_effect_video`] changes it while the output runs.
    /// Unused without [`Self::effect`].
    pub effect_background_video: Option<PathBuf>,
    /// The module stage elements ([`crate::module`]), one per entry, in
    /// order within their placement: none by default. Their names must be
    /// unique; on system-memory frames every factory must take host
    /// frames.
    pub module_stages: Vec<ModuleStageSpec>,
    /// The image the clock's placeholder shows in place of black (see the
    /// module docs, "Placeholder image"): none by default (black). The
    /// output starts with it; [`Output::set_placeholder_image`] changes it
    /// while the output lives.
    pub placeholder_image: Option<PathBuf>,
}

impl OutputConfig {
    pub fn new(contract: OutputContract, sink: OutputSink) -> Self {
        OutputConfig {
            contract,
            sink,
            process: GpuStage::default(),
            gap_threshold: Duration::from_nanos(PacerConfig::DEFAULT_GAP_THRESHOLD_NS),
            resync_after: Duration::from_nanos(PacerConfig::DEFAULT_RESYNC_AFTER_NS),
            queue_buffers: Self::DEFAULT_QUEUE_BUFFERS,
            hold_timeout: Self::DEFAULT_HOLD_TIMEOUT,
            phase_lock: PhaseLockConfig::default(),
            effect: None,
            effect_backend: None,
            effect_background_video: None,
            module_stages: Vec::new(),
            placeholder_image: None,
        }
    }

    /// Default `appsrc` queue bound, in buffers.
    pub const DEFAULT_QUEUE_BUFFERS: u32 = 3;
    /// Default hold during a source switch.
    pub const DEFAULT_HOLD_TIMEOUT: Duration = Duration::from_secs(3);

    /// The output clock's parameters: the catch-up burst is the `appsrc`
    /// queue bound.
    pub fn pacer(&self) -> PacerConfig {
        let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        PacerConfig {
            gap_threshold_ns: ns(self.gap_threshold),
            resync_after_ns: ns(self.resync_after),
            max_burst: u64::from(self.queue_buffers.max(1)),
            lock: self.phase_lock,
            ..PacerConfig::new(self.contract.fps_num, self.contract.fps_den)
        }
    }
}

/// Where an emitted frame came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameOrigin {
    pub source_id: u64,
    /// Its slot sequence number.
    pub seq: u64,
    /// Its timestamp in the ingest pipeline.
    pub source_pts_ns: Option<u64>,
    /// When the ingest wrote it into the slot.
    pub arrived_at: Instant,
}

/// One tick of the output clock, as reported to the observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmitInfo {
    /// Grid index of the tick.
    pub tick: u64,
    /// The output buffer's PTS (`tick x period`).
    pub pts_ns: u64,
    pub kind: TickKind,
    /// The frame emitted; `None` for a placeholder.
    pub origin: Option<FrameOrigin>,
    /// When the buffer was handed to `appsrc`.
    pub emitted_at: Instant,
    /// The instant of the tick's timestamp on the clock's grid: the clock's
    /// start plus `pts_ns`. `emitted_at` minus this is the emission's
    /// deviation from the grid, the phase offset plus the wake-up delay.
    pub grid_at: Instant,
    /// The clock's phase offset when the tick was scheduled.
    pub phase_offset_ns: i64,
    /// The phase lock was acquired and engaged ([`OutputCounts::phase_locked`])
    /// when the tick was emitted: it follows the input from about this tick
    /// on, where earlier ticks ran on the free-running grid.
    pub phase_locked: bool,
    /// `appsrc` accepted the buffer.
    pub pushed: bool,
    /// Buffers queued in `appsrc` right after the push.
    pub queue_level: u64,
}

/// Called by the clock thread after every tick. It runs on the clock thread:
/// it must return quickly and must not call back into the [`Output`].
pub type EmitObserver = Arc<dyn Fn(&EmitInfo) + Send + Sync>;

/// A snapshot of the output's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct OutputCounts {
    /// Frames written into the slot by every source.
    pub input_frames: u64,
    pub fresh: u64,
    pub repeat: u64,
    pub placeholder: u64,
    /// Input frames replaced in the slot before any tick emitted them.
    pub superseded: u64,
    /// Ticks emitted after their successor was already due.
    pub late: u64,
    /// Ticks skipped by a resynchronisation after a long stall.
    pub skipped: u64,
    /// Buffers `appsrc` accepted.
    pub emitted: u64,
    /// Of the buffers accepted, those `appsrc` then dropped because its
    /// queue was full: they never reached the sink. Buffers delivered =
    /// `emitted - queue_dropped`.
    pub queue_dropped: u64,
    /// Buffers `appsrc` refused (pipeline flushing or stopped).
    pub push_failed: u64,
    /// The highest `appsrc` queue level seen right after a push (whole
    /// run; at most [`OutputConfig::queue_buffers`]).
    pub queue_level_max: u64,
    /// Phase lock jumps and wraps ([`PacerCounts`]).
    pub lock_acquisitions: u64,
    pub lock_wraps: u64,
    /// The current phase offset, and whether the lock is engaged.
    pub phase_offset_ns: i64,
    pub phase_locked: bool,
    /// Holds armed by source switches, ended by the new source's first
    /// frame, and ended by the timeout.
    pub holds: u64,
    pub holds_released: u64,
    pub holds_expired: u64,
}

#[derive(Debug, Default)]
struct Counters {
    fresh: AtomicU64,
    repeat: AtomicU64,
    placeholder: AtomicU64,
    superseded: AtomicU64,
    late: AtomicU64,
    skipped: AtomicU64,
    emitted: AtomicU64,
    queue_dropped: AtomicU64,
    push_failed: AtomicU64,
    queue_level_max: AtomicU64,
    lock_acquisitions: AtomicU64,
    lock_wraps: AtomicU64,
    phase_offset_ns: AtomicI64,
    phase_locked: AtomicBool,
    holds: AtomicU64,
    holds_released: AtomicU64,
    holds_expired: AtomicU64,
    /// Source of the last fresh frame, and when (ns since the epoch) its
    /// first fresh frame was emitted.
    fresh_source: AtomicU64,
    fresh_source_since_ns: AtomicU64,
}

/// A stop flag the clock thread sleeps on.
#[derive(Debug, Default)]
pub(crate) struct StopSignal {
    stopped: Mutex<bool>,
    cv: Condvar,
}

impl StopSignal {
    pub(crate) fn set(&self) {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }

    pub(crate) fn is_set(&self) -> bool {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sleeps until `deadline` or until stopped; returns whether stopped.
    pub(crate) fn wait_until(&self, deadline: Instant) -> bool {
        let mut stopped = self.stopped.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *stopped {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            stopped = match self.cv.wait_timeout(stopped, deadline - now) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
    }
}

#[derive(Debug)]
struct ClockThread {
    handle: JoinHandle<()>,
    stop: Arc<StopSignal>,
    /// Set when the clock loop panicked, with the panic message.
    failure: Arc<Mutex<Option<String>>>,
}

impl ClockThread {
    /// Why the thread ended without being stopped, if it did.
    fn failure(&self) -> Option<String> {
        if let Some(msg) = self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Some(format!("the output clock thread panicked: {msg}"));
        }
        (self.handle.is_finished() && !self.stop.is_set())
            .then(|| "the output clock thread ended unexpectedly".to_string())
    }
}

/// The message of a panic payload (`panic!` with a literal or a format).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_string())
}

/// Runs `body`, catching a panic: its message is stored in `failure`.
pub(crate) fn run_recording_panic(body: impl FnOnce(), failure: &Mutex<Option<String>>) {
    if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(body)) {
        *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(panic_message(&*payload));
    }
}

/// The output pipeline and its clock. See the module docs.
pub struct Output {
    pipeline: MediaPipeline,
    appsrc: gst_app::AppSrc,
    cfg: OutputConfig,
    canonical: Canonical,
    slot: Arc<Slot<Frame>>,
    counters: Arc<Counters>,
    /// Time origin of slot arrival times and of the clock's grid.
    epoch: Instant,
    next_source: AtomicU64,
    /// A hold requested by [`Self::hold_for`], taken by the clock thread on
    /// its next tick: the awaited source and the deadline.
    hold_request: Arc<Mutex<Option<(u64, Instant)>>>,
    observer: Option<EmitObserver>,
    cuda: Option<gst::Context>,
    clock: Option<ClockThread>,
    memory: VideoMemory,
    effect: Option<gst_obc::ObcEffect>,
    /// The module stage elements, by spec name, in pipeline order.
    modules: Vec<(String, gst_obc::ObcModule)>,
    /// This output attached to the writer it forwards to
    /// ([`OutputSink::Writer`]): set by a start that succeeded, dropped by
    /// the stop.
    attachment: Option<WriterAttachment>,
    /// The image the clock's placeholder shows, shared with the clock
    /// thread and the image's decode threads.
    away: Arc<PlaceholderImage>,
    /// Starts so far: tells a decode of an earlier start from one of the
    /// current one.
    starts: u64,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("contract", &self.cfg.contract)
            .field("sink", &self.cfg.sink)
            .field("process", &self.cfg.process)
            .field("effect", &self.cfg.effect)
            .field("module_stages", &self.cfg.module_stages)
            .field("running", &self.clock.is_some())
            .finish_non_exhaustive()
    }
}

fn ns_since(epoch: Instant, t: Instant) -> u64 {
    u64::try_from(t.saturating_duration_since(epoch).as_nanos()).unwrap_or(u64::MAX)
}

/// The colour label of the frames the output hands its sink: BT.601
/// (limited range, BT.601 matrix, BT.709 transfer, SMPTE 170M primaries),
/// the label v4l2loopback reports back for every stream. A loopback sink
/// only ever carries limited range ([`check_sink_range`]); see the module
/// docs.
pub const SINK_COLORIMETRY: &str = "2:4:5:4";

/// Refuses a contract whose colour range `sink` cannot carry: v4l2loopback
/// stores no colour range, so a consumer would read full-range frames as
/// limited range (crushed blacks, clipped whites). Every range is accepted
/// by other sinks.
pub fn check_sink_range(contract: &OutputContract, sink: &OutputSink) -> Result<(), PipelineError> {
    match (sink, contract.range) {
        (OutputSink::Loopback(device), ColourRange::Full) => Err(PipelineError::LoopbackRefused {
            device: device.display().to_string(),
            accepted: "limited range only: v4l2loopback carries no colour range, so a consumer \
                       would read a full-range stream as limited"
                .to_string(),
            wanted: contract.to_string(),
        }),
        _ => Ok(()),
    }
}

/// The caps the output delivers to its sink, in system memory, labelled
/// [`SINK_COLORIMETRY`].
pub fn contract_caps_string(contract: &OutputContract) -> String {
    format!(
        "{},colorimetry={SINK_COLORIMETRY}",
        contract_caps_unlabelled(contract)
    )
}

/// The contract caps in system memory without a colour label.
fn contract_caps_unlabelled(contract: &OutputContract) -> String {
    format!(
        "video/x-raw,format={},width={},height={},framerate={}/{}",
        contract.format.gst_name(),
        contract.width,
        contract.height,
        contract.fps_num,
        contract.fps_den
    )
}

/// Which slot sequence number a restarted clock counts as already handled:
/// everything before the frame now in the slot (`latest_seq`, emitted
/// fresh at the restart), or, with an empty slot, every frame written so
/// far (`last_written`).
fn resume_sequence(latest_seq: Option<u64>, last_written: u64) -> u64 {
    latest_seq.map_or(last_written, |s| s.saturating_sub(1))
}

fn parse_caps(s: &str) -> Result<gst::Caps, PipelineError> {
    s.parse().map_err(|_| PipelineError::Caps(s.to_string()))
}

/// The output's `appsrc`: live, timestamped by the clock, never blocking,
/// holding at most `max_buffers` and dropping the oldest beyond that
/// (`GST_APP_LEAKY_TYPE_DOWNSTREAM`).
pub(crate) fn frames_src(name: &str, caps: &gst::Caps, max_buffers: u64) -> gst_app::AppSrc {
    gst_app::AppSrc::builder()
        .name(name)
        .caps(caps)
        .is_live(true)
        .format(gst::Format::Time)
        .do_timestamp(false)
        .stream_type(gst_app::AppStreamType::Stream)
        .block(false)
        .max_buffers(max_buffers)
        .max_bytes(0)
        .max_time(gst::ClockTime::ZERO)
        .leaky_type(gst_app::AppLeakyType::Downstream)
        .build()
}

impl Output {
    /// Builds (but does not start) the output pipeline for canonical frames
    /// in CUDA memory: [`Self::build_in`] with [`VideoMemory::Cuda`]. Its
    /// elements can be inspected and probed before [`Self::start`]. A
    /// contract the sink cannot carry is refused here
    /// ([`check_sink_range`]).
    pub fn build(cfg: OutputConfig) -> Result<Self, PipelineError> {
        Self::build_in(cfg, VideoMemory::Cuda)
    }

    /// [`Self::build`] for canonical frames in `memory`. In
    /// [`VideoMemory::System`] the pipeline has no CUDA element (no element
    /// of the `nvcodec` plugin is made, so it builds where that plugin does
    /// not load) and starts without a CUDA context; what needs CUDA frames
    /// is refused: the roundtrip Process stage ([`PipelineError::Cuda`]) and
    /// a module stage whose chain takes CUDA frames only
    /// ([`PipelineError::Module`]). The effect element takes either memory
    /// (see the module docs).
    pub fn build_in(cfg: OutputConfig, memory: VideoMemory) -> Result<Self, PipelineError> {
        use names::*;
        let contract = cfg.contract;
        contract
            .validate()
            .map_err(|e| PipelineError::Caps(e.to_string()))?;
        check_sink_range(&contract, &cfg.sink)?;
        if let OutputSink::Writer(writer) = &cfg.sink
            && writer.contract() != &contract
        {
            return Err(PipelineError::Caps(format!(
                "the loopback writer carries {}, not this output's {contract}",
                writer.contract()
            )));
        }
        if memory == VideoMemory::System && cfg.process != GpuStage::Passthrough {
            return Err(PipelineError::Cuda(format!(
                "the {} process stage needs frames in CUDA memory",
                cfg.process
            )));
        }
        if cfg.effect.is_some() && cfg.process != GpuStage::Passthrough {
            return Err(PipelineError::Effect(format!(
                "the effect element does not combine with the {} process stage",
                cfg.process
            )));
        }
        check_specs(&cfg.module_stages, memory == VideoMemory::Cuda)?;
        let canonical = Canonical::of(&contract);
        let pipeline = gst::Pipeline::with_name("obc-output");
        let queue = cfg.pacer().max_burst;

        let src_caps = parse_caps(&format!(
            "{},framerate={}/{}",
            canonical.caps_in(memory),
            contract.fps_num,
            contract.fps_den
        ))?;
        let appsrc = frames_src(FRAMES_SRC, &src_caps, queue);

        let mut chain: Vec<gst::Element> = vec![appsrc.clone().upcast()];
        chain.extend(process_elements(cfg.process, canonical.colorimetry)?);
        let mut modules = Vec::new();
        let mut place = |chain: &mut Vec<gst::Element>, at: ModulePlacement| {
            for spec in cfg.module_stages.iter().filter(|s| s.placement == at) {
                let m = module_element(spec)?;
                chain.push(m.clone().upcast());
                modules.push((spec.name.clone(), m));
            }
            Ok::<(), PipelineError>(())
        };
        place(&mut chain, ModulePlacement::BeforeEffect)?;
        let effect = match &cfg.effect {
            Some(settings) => {
                let fx = effect_element_with_video(
                    settings,
                    cfg.effect_backend.as_ref(),
                    cfg.effect_background_video.as_deref(),
                )?;
                chain.push(fx.clone().upcast());
                Some(fx)
            }
            None => None,
        };
        place(&mut chain, ModulePlacement::AfterEffect)?;
        match memory {
            VideoMemory::Cuda => {
                if contract.format != OutputFormat::Nv12 {
                    chain.push(make("cudaconvert", TO_OUT)?);
                    chain.push(capsfilter(
                        OUT_CUDA_CAPS,
                        &format!(
                            "{},colorimetry={}",
                            cuda_caps(contract.format.gst_name()),
                            canonical.colorimetry
                        ),
                    )?);
                }
                chain.push(make("cudadownload", DOWNLOAD)?);
            }
            // Already in system memory: DOWNLOAD_CAPS fixes the format.
            VideoMemory::System if contract.format != OutputFormat::Nv12 => {
                chain.push(make("videoconvert", TO_OUT)?);
            }
            VideoMemory::System => {}
        }
        // System memory with the canonical label, then the sink's label on
        // the same buffers (see the module docs).
        chain.push(capsfilter(
            DOWNLOAD_CAPS,
            &format!(
                "{},colorimetry={}",
                contract_caps_unlabelled(&contract),
                canonical.colorimetry
            ),
        )?);
        let relabel = make("capssetter", RELABEL)?;
        relabel.set_property(
            "caps",
            parse_caps(&format!("video/x-raw,colorimetry={SINK_COLORIMETRY}"))?,
        );
        relabel.set_property("join", true);
        relabel.set_property("replace", false);
        chain.push(relabel);
        chain.push(capsfilter(OUT_CAPS, &contract_caps_string(&contract))?);
        let loopback_caps = match &cfg.sink {
            OutputSink::Loopback(device) => {
                let sink = make("v4l2sink", LOOPBACK)?;
                sink.set_property("device", device.to_string_lossy().as_ref());
                sink.set_property("sync", false);
                sink.set_property("enable-last-sample", false);
                chain.push(sink);
                Some(parse_caps(&contract_caps_string(&contract))?)
            }
            OutputSink::Fake => {
                let sink = make("fakesink", LOOPBACK)?;
                sink.set_property("sync", false);
                sink.set_property("enable-last-sample", false);
                chain.push(sink);
                None
            }
            // The writer checks the loopback; this pipeline has none.
            OutputSink::Writer(writer) => {
                chain.push(forward_sink(LOOPBACK, writer.clone()));
                None
            }
        };
        // Linked pair by pair, so that a failure is told at its origin: a
        // module stage element's or the effect element's is theirs, not the
        // contract's ([`link_failure`]).
        pipeline
            .add_many(&chain)
            .map_err(|e| PipelineError::Link(e.to_string()))?;
        let module_names: Vec<String> = modules.iter().map(|(_, m)| m.name().to_string()).collect();
        let effect_name = effect.as_ref().map(|fx| fx.name().to_string());
        for pair in chain.windows(2) {
            pair[0].link(&pair[1]).map_err(|e| {
                link_failure(
                    &pair[0].name(),
                    &pair[1].name(),
                    &module_names,
                    effect_name.as_deref(),
                    &e.to_string(),
                )
            })?;
        }

        let away = Arc::new(PlaceholderImage::new(canonical, memory));
        away.request(cfg.placeholder_image.as_deref());
        Ok(Output {
            pipeline: MediaPipeline::from_parts(pipeline, loopback_caps),
            appsrc,
            cfg,
            canonical,
            slot: Arc::new(Slot::new()),
            counters: Arc::new(Counters::default()),
            epoch: Instant::now(),
            next_source: AtomicU64::new(1),
            hold_request: Arc::default(),
            observer: None,
            cuda: None,
            clock: None,
            memory,
            effect,
            modules,
            attachment: None,
            away,
            starts: 0,
        })
    }

    fn module(&self, name: &str) -> Option<&gst_obc::ObcModule> {
        self.modules.iter().find(|(n, _)| n == name).map(|(_, m)| m)
    }

    /// Changes the stages of the module stage element `name` while the
    /// output runs (or before it starts): they are checked (each listed
    /// once) and applied at once ([`gst_obc::ObcModule::apply`]); loading
    /// happens on the element's worker, follow it with
    /// [`Self::module_stage_status`] or the bus messages. Applying stages
    /// after a failure retries, even unchanged. Fails for a name the output
    /// was not built with ([`OutputConfig::module_stages`]).
    pub fn set_module_stages(
        &self,
        name: &str,
        stages: &[crate::module::StageRequest],
    ) -> Result<(), PipelineError> {
        let m = self.module(name).ok_or_else(|| {
            PipelineError::Module(format!(
                "this output was built without the module stage {name:?} \
                 (OutputConfig::module_stages)"
            ))
        })?;
        crate::module::check_stages(stages)?;
        m.apply(stages).map_err(PipelineError::Module)
    }

    /// The names of the module stage elements, in pipeline order.
    pub fn module_names(&self) -> Vec<String> {
        self.modules.iter().map(|(n, _)| n.clone()).collect()
    }

    /// The stages of the module stage element `name` as last applied;
    /// `None` without one.
    pub fn module_stages(&self, name: &str) -> Option<Vec<crate::module::StageRequest>> {
        self.module(name).map(|m| m.stages())
    }

    /// The state of stage `id` of the module stage element `name`
    /// ([`EffectStatus::Off`] unless requested, and without that element).
    pub fn module_stage_status(&self, name: &str, id: &str) -> EffectStatus {
        self.module(name)
            .map_or(EffectStatus::Off, |m| m.stage_status(id))
    }

    /// The counters of the module stage element `name`; `None` without one.
    pub fn module_stats(&self, name: &str) -> Option<ModuleStats> {
        self.module(name).map(|m| m.stats())
    }

    /// Changes the effect while the output runs (or before it starts): the
    /// settings are checked ([`crate::effect::check_settings`]) and applied
    /// to the effect element at once. Loading happens on the element's
    /// worker; follow it with [`Self::effect_status`] or the bus messages.
    /// Applying settings after a failure retries, even unchanged. Fails for
    /// an output built without [`OutputConfig::effect`].
    pub fn set_effect(&self, settings: &EffectSettings) -> Result<(), PipelineError> {
        let fx = self.effect.as_ref().ok_or_else(|| {
            PipelineError::Effect(
                "this output was built without the effect element (OutputConfig::effect)".into(),
            )
        })?;
        crate::effect::check_settings(settings)?;
        fx.apply(settings).map_err(PipelineError::Effect)
    }

    /// [`Self::set_effect`] with the video background `video` of replace
    /// ([`gst_obc::ObcEffect::set_background_video`]; the settings'
    /// `background` is its load-time still), as one change: a new video is
    /// set before the settings are applied, and a video taken away after
    /// them, so the element never holds replace with neither a still nor a
    /// video. Changing the video alone reloads nothing.
    pub fn set_effect_video(
        &self,
        settings: &EffectSettings,
        video: Option<&Path>,
    ) -> Result<(), PipelineError> {
        let fx = self.effect.as_ref().ok_or_else(|| {
            PipelineError::Effect(
                "this output was built without the effect element (OutputConfig::effect)".into(),
            )
        })?;
        crate::effect::check_settings_video(settings, video.is_some())?;
        if video.is_some() {
            fx.set_background_video(video)
                .map_err(PipelineError::Effect)?;
        }
        fx.apply(settings).map_err(PipelineError::Effect)?;
        if video.is_none() && fx.background_video().is_some() {
            fx.set_background_video(None)
                .map_err(PipelineError::Effect)?;
        }
        Ok(())
    }

    /// Takes the effect element's backend away at once: the effect and its
    /// stages fail with the no-backend reason, so the next frames follow
    /// the fallback (black, the fallback image, or the input with `none`),
    /// and the element drops its backend instance on its worker
    /// ([`gst_obc::ObcEffect::revoke_backend`]). Never waits for a load.
    /// Does nothing without an effect element or a backend.
    pub fn revoke_effect_backend(&self) {
        if let Some(fx) = &self.effect {
            fx.revoke_backend();
        }
    }

    /// Sets the effect element's backend (`None`: none for the next
    /// instance). After [`Self::revoke_effect_backend`] a backend re-arms
    /// the element: the effect and its stages load again with it, with no
    /// restart. Does nothing without an effect element.
    pub fn set_effect_backend(&self, backend: Option<&EffectBackend>) {
        if let Some(fx) = &self.effect {
            fx.set_backend_factory(backend.map(|b| Arc::clone(b.factory())));
        }
    }

    /// The state of the requested effect; [`EffectStatus::Off`] without an
    /// effect element.
    pub fn effect_status(&self) -> EffectStatus {
        self.effect
            .as_ref()
            .map_or(EffectStatus::Off, |fx| fx.status())
    }

    /// The state of the effect element's stage `id` ([`EffectStatus::Off`]
    /// unless requested, and without an effect element).
    pub fn stage_status(&self, id: &str) -> EffectStatus {
        self.effect
            .as_ref()
            .map_or(EffectStatus::Off, |fx| fx.stage_status(id))
    }

    /// The effect element's counters and timings; `None` without one.
    pub fn effect_stats(&self) -> Option<EffectStats> {
        self.effect.as_ref().map(|fx| fx.stats())
    }

    /// The status values the stages report after the last frame they
    /// processed (a module of ABI 1.3: auto-frame's zoom, for one): those
    /// of the module stage elements, in pipeline order, then the effect
    /// element's. Empty when none reports.
    pub fn stage_values(&self) -> Vec<obc_backend::StageValue> {
        let mut out: Vec<obc_backend::StageValue> = self
            .modules
            .iter()
            .flat_map(|(_, m)| m.stats().stage_values)
            .collect();
        if let Some(fx) = &self.effect {
            out.extend(fx.stats().stage_values);
        }
        out
    }

    /// The current effect settings; `None` without an effect element.
    pub fn effect_settings(&self) -> Option<EffectSettings> {
        self.effect.as_ref().map(|fx| fx.settings())
    }

    /// Switches the clock's placeholder (see the module docs,
    /// "Placeholder image"): black (`None`, at once), or the image at
    /// `image`, cropped to fill the canonical frame, decoded on a thread of
    /// its own while the output runs (black until it is ready; black,
    /// [`PlaceholderState::Failed`], when it cannot be read or uploaded).
    /// The image already asked for is not decoded again unless the file at
    /// its path is another one now ([`FileStamp`]); the last image decoded
    /// is kept across black and across [`Self::stop`] and [`Self::start`],
    /// until another path is asked for, and the same file asked for again
    /// is shown at once. Never blocks for the decode or the upload. Kept
    /// across [`Self::stop`] and [`Self::start`]; while the output is
    /// stopped nothing is decoded or uploaded (the GPU is not woken), the
    /// decode starts with the next start.
    pub fn set_placeholder_image(&self, image: Option<&Path>) {
        self.away.request(image);
    }

    /// What the clock's placeholder shows ([`Self::set_placeholder_image`]):
    /// [`PlaceholderState::Pending`] while the image is decoded or
    /// uploaded, and while the output is stopped with an image asked for
    /// that it has not decoded yet.
    pub fn placeholder_state(&self) -> PlaceholderState {
        self.away.state()
    }

    /// Sets the per-tick observer. Takes effect at [`Self::start`].
    pub fn set_observer(&mut self, observer: Option<EmitObserver>) {
        self.observer = observer;
    }

    /// Starts the output: checks the loopback, takes the CUDA context the
    /// pipeline's elements create, prepares the placeholder frame, goes to
    /// PLAYING and starts the clock. A second call does nothing. On error
    /// the pipeline is back in NULL.
    ///
    /// A state change that failed with an element's error on the bus is
    /// [`PipelineError::ElementState`], naming it
    /// ([`MediaPipeline::state_failure`]).
    pub fn start(&mut self) -> Result<(), PipelineError> {
        if self.clock.is_some() {
            return Ok(());
        }
        let res = self
            .start_inner()
            .map_err(|e| self.pipeline.state_failure(e));
        if res.is_err() {
            let _ = self.pipeline.pipeline().set_state(gst::State::Null);
            self.away.stopped();
            self.cuda = None;
        } else if let OutputSink::Writer(writer) = &self.cfg.sink {
            // Detached by the stop: see the module docs.
            self.attachment = Some(writer.attach(self.cfg.hold_timeout.min(Self::STALL_HOLD)));
        }
        res
    }

    /// The longest a loopback writer this output forwards to holds its
    /// last frame while the output sends none (see the module docs,
    /// "Forwarding to a loopback writer"), unless
    /// [`OutputConfig::hold_timeout`] is shorter. Long enough to cover a
    /// deliberate stall of the streaming thread (a stage or effect switch
    /// can take longer than one frame period), short enough that a real stall (a pipeline
    /// error, a dead clock, a module stuck) still turns to the writer's
    /// placeholder within a second, rather than leaving the last live frame
    /// frozen on the consumer.
    pub const STALL_HOLD: Duration = Duration::from_secs(1);

    fn start_inner(&mut self) -> Result<(), PipelineError> {
        self.pipeline.prepare()?;
        let (cuda, placeholder) = match self.memory {
            VideoMemory::Cuda => {
                let cuda = self.acquire_cuda_context()?;
                let placeholder = make_placeholder(&self.canonical, &cuda)?;
                (Some(cuda), placeholder)
            }
            VideoMemory::System => (None, black_frame(&self.canonical)),
        };
        // Before the clock's first tick: an image ready on the CPU shows
        // from the first placeholder on.
        self.starts += 1;
        self.away.started(self.starts, cuda.as_ref());
        self.cuda = cuda;
        self.pipeline
            .pipeline()
            .set_state(gst::State::Playing)
            .map_err(|e| PipelineError::State(e.to_string()))?;

        // A restart carries on from the counts and sequence reached.
        let baseline = resume_sequence(self.slot.latest().map(|e| e.seq), self.slot.last_seq());
        let prior = self.counts();
        let resumed = PacerCounts {
            fresh: prior.fresh,
            repeat: prior.repeat,
            placeholder: prior.placeholder,
            superseded: prior.superseded,
            late: prior.late,
            skipped: prior.skipped,
            lock_acquisitions: prior.lock_acquisitions,
            lock_wraps: prior.lock_wraps,
            phase_offset_ns: 0,
            phase_locked: false,
            holds: prior.holds,
            holds_released: prior.holds_released,
            holds_expired: prior.holds_expired,
        };
        // A hold requested while stopped does not outlive the stop.
        self.take_hold_request();
        let stop = Arc::new(StopSignal::default());
        let failure: Arc<Mutex<Option<String>>> = Arc::default();
        let ctx = ClockContext {
            pacer: Pacer::resume(self.cfg.pacer(), baseline, resumed),
            appsrc: self.appsrc.clone(),
            slot: Arc::clone(&self.slot),
            counters: Arc::clone(&self.counters),
            hold_request: Arc::clone(&self.hold_request),
            observer: self.observer.clone(),
            placeholder,
            image: Arc::clone(&self.away),
            covered: matches!(self.cfg.sink, OutputSink::Writer(_)),
            epoch: self.epoch,
            stop: Arc::clone(&stop),
        };
        let thread_failure = Arc::clone(&failure);
        let handle = std::thread::Builder::new()
            .name("obc-output-clock".into())
            .spawn(move || run_recording_panic(move || ctx.run(), &thread_failure))
            .map_err(|e| PipelineError::State(format!("output clock thread: {e}")))?;
        self.clock = Some(ClockThread {
            handle,
            stop,
            failure,
        });
        Ok(())
    }

    /// The `GstCudaContext` the output's elements opened, as posted on the
    /// bus when they reached READY.
    fn acquire_cuda_context(&self) -> Result<gst::Context, PipelineError> {
        let p = self.pipeline.pipeline();
        if let Some(ctx) = p.context(CUDA_CONTEXT_TYPE) {
            return Ok(ctx);
        }
        // Elements that open their context later do so on READY -> PAUSED.
        p.set_state(gst::State::Paused)
            .map_err(|e| PipelineError::State(e.to_string()))?;
        p.context(CUDA_CONTEXT_TYPE)
            .ok_or_else(|| PipelineError::Cuda(NO_CUDA_CONTEXT.to_string()))
    }

    /// The CUDA context ingest pipelines must use (after [`Self::start`]).
    pub fn cuda_context(&self) -> Option<&gst::Context> {
        self.cuda.as_ref()
    }

    /// A writer for a new source; each call allocates a new source id.
    pub fn frame_writer(&self) -> FrameWriter {
        let id = self.next_source.fetch_add(1, Ordering::Relaxed);
        FrameWriter::new(Arc::clone(&self.slot), self.epoch, id, self.canonical)
            .in_memory(self.memory)
    }

    /// Where the canonical frames this output takes live
    /// ([`Self::build_in`]).
    pub fn memory(&self) -> VideoMemory {
        self.memory
    }

    /// Holds the last frame while a source switch waits for `source_id`:
    /// until its first fresh frame or [`OutputConfig::hold_timeout`] from
    /// now, the clock repeats the last frame instead of sending placeholders
    /// after the gap threshold ([`crate::clock`], "Hold"). Taken by the
    /// clock on its next tick; a later request replaces it.
    pub fn hold_for(&self, source_id: u64) {
        let deadline = Instant::now() + self.cfg.hold_timeout;
        *self.hold_request.lock().unwrap_or_else(|e| e.into_inner()) = Some((source_id, deadline));
    }

    fn take_hold_request(&self) -> Option<(u64, Instant)> {
        self.hold_request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// When the first fresh frame of `source_id` was emitted, if it was and
    /// no later source has emitted a fresh frame since.
    pub fn first_fresh(&self, source_id: u64) -> Option<Instant> {
        if self.counters.fresh_source.load(Ordering::Acquire) != source_id {
            return None;
        }
        let ns = self.counters.fresh_source_since_ns.load(Ordering::Relaxed);
        Some(self.epoch + Duration::from_nanos(ns))
    }

    pub fn counts(&self) -> OutputCounts {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        OutputCounts {
            input_frames: self.slot.written(),
            fresh: get(&c.fresh),
            repeat: get(&c.repeat),
            placeholder: get(&c.placeholder),
            superseded: get(&c.superseded),
            late: get(&c.late),
            skipped: get(&c.skipped),
            emitted: get(&c.emitted),
            queue_dropped: get(&c.queue_dropped),
            push_failed: get(&c.push_failed),
            queue_level_max: get(&c.queue_level_max),
            lock_acquisitions: get(&c.lock_acquisitions),
            lock_wraps: get(&c.lock_wraps),
            phase_offset_ns: c.phase_offset_ns.load(Ordering::Relaxed),
            phase_locked: c.phase_locked.load(Ordering::Relaxed),
            holds: get(&c.holds),
            holds_released: get(&c.holds_released),
            holds_expired: get(&c.holds_expired),
        }
    }

    pub fn contract(&self) -> &OutputContract {
        &self.cfg.contract
    }

    pub fn config(&self) -> &OutputConfig {
        &self.cfg
    }

    pub fn canonical(&self) -> Canonical {
        self.canonical
    }

    pub fn pipeline(&self) -> &MediaPipeline {
        &self.pipeline
    }

    /// Whether the clock is running: started, not stopped, and its thread
    /// still alive (see [`Self::health`]).
    pub fn is_running(&self) -> bool {
        self.clock.as_ref().is_some_and(|c| c.failure().is_none())
    }

    /// An error once the clock thread has ended without being stopped (it
    /// panicked): the output then emits nothing. `Ok` while it runs, and
    /// when it is not started or was stopped.
    pub fn health(&self) -> Result<(), PipelineError> {
        match self.clock.as_ref().and_then(ClockThread::failure) {
            Some(msg) => Err(PipelineError::State(msg)),
            None => Ok(()),
        }
    }

    /// Stops the clock (joined before this returns), sets the pipeline to
    /// NULL and releases the frames and the CUDA context it holds.
    /// Idempotent.
    pub fn stop(&mut self) -> Result<(), PipelineError> {
        self.stop_checked().map_err(Into::into)
    }

    /// [`Self::stop`], its error saying whether the pipeline reached NULL:
    /// a clock thread that had panicked (or ended) is an error that did,
    /// as the thread is joined all the same.
    pub fn stop_checked(&mut self) -> Result<(), StopError> {
        let mut res = Ok(());
        // Detached before the pipeline goes to NULL: the writer's
        // placeholder follows a stopped output.
        drop(self.attachment.take());
        if let Some(clock) = self.clock.take() {
            // An unexpected end is judged before the stop flag is set.
            let ended = clock.failure();
            clock.stop.set();
            let joined = clock.handle.join();
            let panicked = clock
                .failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .map(|msg| format!("the output clock thread panicked: {msg}"));
            if let Some(msg) = panicked.or(ended) {
                res = Err(PipelineError::State(msg));
            } else if joined.is_err() {
                res = Err(PipelineError::State(
                    "the output clock thread panicked".into(),
                ));
            }
        }
        let p = self.pipeline.pipeline();
        let state = p.set_state(gst::State::Null);
        let (_, cur, _) = p.state(gst::ClockTime::from_seconds(5));
        self.slot.clear();
        // The image's CUDA frames go with the context.
        self.away.stopped();
        self.cuda = None;
        let clock = res.map_err(|error| StopError {
            error,
            reached_null: true,
        });
        let null = not_null(state, cur).map_or(Ok(()), |error| {
            Err(StopError {
                error,
                reached_null: false,
            })
        });
        both_stopped(clock, null)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// One canonical frame (system memory, laid out as [`black_frame`])
/// showing the image at `path`, cropped to fill the frame, in the
/// canonical colour (BT.601, the contract's range): the loopback writer's
/// [`image_contract_frame`] for an NV12 contract of the canonical size.
/// Blocks for the decode.
pub(crate) fn image_canonical_frame(
    canonical: &Canonical,
    path: &Path,
) -> Result<gst::Buffer, String> {
    let contract = OutputContract {
        width: canonical.width,
        height: canonical.height,
        range: canonical.range,
        format: OutputFormat::Nv12,
        ..OutputContract::default()
    };
    image_contract_frame(&contract, path)
}

/// The image the output clock's placeholder shows (see the module docs,
/// "Placeholder image"), shared by the [`Output`], its clock thread and
/// the image's decode threads.
///
/// Locks are taken in the order `gate`, `request`, `shown`; `shown` is
/// held only for an assignment or a reference count, so the clock thread
/// never waits on a decode or an upload. A decode thread's upload holds no
/// lock; the start's (on the starting thread) holds `gate` alone.
#[derive(Debug)]
struct PlaceholderImage {
    canonical: Canonical,
    memory: VideoMemory,
    /// The running start (its number) and its CUDA context (`None` in
    /// system memory); `None` while the output is stopped. Held across the
    /// publication of an uploaded frame: once [`Self::stopped`] has taken
    /// it, no CUDA frame of that start is published any more (an upload
    /// still in flight on a decode thread finishes with its own reference
    /// to the context, and its frame is dropped).
    gate: Mutex<Option<(u64, Option<gst::Context>)>>,
    request: Mutex<ImageRequest>,
    /// The frame a placeholder tick pushes, in the output's memory; `None`:
    /// the black one.
    shown: Mutex<Option<gst::Buffer>>,
}

/// What a placeholder tick shows ([`PlaceholderImage::for_tick`]).
#[derive(Debug)]
enum TickImage {
    Black,
    /// The image asked for is not ready yet.
    Pending,
    Image(gst::Buffer),
}

/// The image asked for of a [`PlaceholderImage`], and what it reached.
#[derive(Debug)]
struct ImageRequest {
    /// The image asked for (`None`: black), with the file at its path then.
    image: Option<(PathBuf, Option<FileStamp>)>,
    /// Counts the requests: a decode's result is kept only if no later one
    /// was made.
    serial: u64,
    state: PlaceholderState,
    /// The running start, as in `gate` (set and cleared under this lock,
    /// after `gate`).
    running: Option<u64>,
    /// The last image decoded, in system memory, with the file it was
    /// decoded from. Kept across black and stops; dropped when another
    /// path is asked for. A failed decode is never kept.
    decoded: Option<((PathBuf, FileStamp), gst::Buffer)>,
    /// That image in CUDA memory of the running start, and the start; on
    /// CUDA outputs only, dropped by the stop.
    uploaded: Option<(u64, (PathBuf, FileStamp), gst::Buffer)>,
}

/// The `(path, stamp)` an image asked for is kept as, once decoded: none
/// for a path that could not be stat'ed.
fn image_key(image: &Option<(PathBuf, Option<FileStamp>)>) -> Option<(PathBuf, FileStamp)> {
    match image {
        Some((p, Some(stamp))) => Some((p.clone(), *stamp)),
        _ => None,
    }
}

impl PlaceholderImage {
    fn new(canonical: Canonical, memory: VideoMemory) -> Self {
        PlaceholderImage {
            canonical,
            memory,
            gate: Mutex::new(None),
            request: Mutex::new(ImageRequest {
                image: None,
                serial: 0,
                state: PlaceholderState::Black,
                running: None,
                decoded: None,
                uploaded: None,
            }),
            shown: Mutex::new(None),
        }
    }

    fn lock_request(&self) -> std::sync::MutexGuard<'_, ImageRequest> {
        self.request.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn show(&self, frame: Option<gst::Buffer>) {
        let old = std::mem::replace(
            &mut *self.shown.lock().unwrap_or_else(|e| e.into_inner()),
            frame,
        );
        // Released outside the lock the clock thread takes.
        drop(old);
    }

    /// The frame a placeholder tick pushes instead of black, if any.
    fn shown(&self) -> Option<gst::Buffer> {
        self.shown.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// What a placeholder tick of the clock shows now: the image, black,
    /// or nothing yet (the image asked for still being decoded or
    /// uploaded). Both locks are only ever held for an assignment, a clone
    /// or a thread spawn: never across a decode or an upload.
    fn for_tick(&self) -> TickImage {
        let r = self.lock_request();
        if r.state == PlaceholderState::Pending {
            return TickImage::Pending;
        }
        match self.shown() {
            Some(b) => TickImage::Image(b),
            None => TickImage::Black,
        }
    }

    fn state(&self) -> PlaceholderState {
        self.lock_request().state.clone()
    }

    /// [`Output::set_placeholder_image`].
    fn request(self: &Arc<Self>, image: Option<&Path>) {
        let want = image.map(|p| (p.to_path_buf(), FileStamp::of(p)));
        let mut r = self.lock_request();
        if r.image == want {
            return;
        }
        r.serial += 1;
        // Another image asked for: the one kept is not shown again.
        if let Some((p, _)) = &want {
            if r.decoded.as_ref().is_some_and(|((dp, _), _)| dp != p) {
                r.decoded = None;
            }
            if r.uploaded.as_ref().is_some_and(|(_, (up, _), _)| up != p) {
                r.uploaded = None;
            }
        }
        r.image = want;
        self.apply(&mut r);
    }

    /// Makes the placeholder follow `r` (its image asked for, what is
    /// decoded and uploaded, whether the output runs): shows what is ready,
    /// and starts the decode or upload that is missing.
    fn apply(self: &Arc<Self>, r: &mut ImageRequest) {
        let Some((path, _)) = &r.image else {
            r.state = PlaceholderState::Black;
            self.show(None);
            return;
        };
        let path = path.clone();
        let key = image_key(&r.image);
        if let (Some(start), Some(k), Some((up_start, up_key, b))) = (r.running, &key, &r.uploaded)
            && start == *up_start
            && k == up_key
        {
            self.show(Some(b.clone()));
            r.state = PlaceholderState::Image;
            return;
        }
        self.show(None);
        let decoded = match (&key, &r.decoded) {
            (Some(k), Some((dk, b))) if k == dk => Some(b.clone()),
            _ => None,
        };
        let Some(start) = r.running else {
            // Decoded (or uploaded) at the next start.
            r.state = if decoded.is_some() {
                PlaceholderState::Image
            } else {
                PlaceholderState::Pending
            };
            return;
        };
        if self.memory == VideoMemory::System
            && let Some(b) = &decoded
        {
            self.show(Some(b.clone()));
            r.state = PlaceholderState::Image;
            return;
        }
        r.state = PlaceholderState::Pending;
        let (this, serial) = (Arc::clone(self), r.serial);
        let spawned = std::thread::Builder::new()
            .name("obc-output-image".into())
            .spawn({
                let path = path.clone();
                move || this.load(start, serial, &path, key, decoded)
            });
        if let Err(e) = spawned {
            r.state = PlaceholderState::Failed(gst_obc::fallback_unreadable(
                &path,
                &format!("no thread to decode it: {e}"),
            ));
        }
    }

    /// The decode thread of request `serial` for start `start`: decodes the
    /// image at `path` on the CPU (unless `decoded` already holds it),
    /// keeps it, then [`Self::deliver`]s it.
    fn load(
        &self,
        start: u64,
        serial: u64,
        path: &Path,
        key: Option<(PathBuf, FileStamp)>,
        decoded: Option<gst::Buffer>,
    ) {
        let frame = match decoded {
            Some(b) => b,
            None => match image_canonical_frame(&self.canonical, path) {
                Ok(b) => {
                    let mut r = self.lock_request();
                    if r.serial != serial {
                        return;
                    }
                    if let Some(k) = key.clone() {
                        r.decoded = Some((k, b.clone()));
                    }
                    b
                }
                Err(why) => {
                    let mut r = self.lock_request();
                    if r.serial == serial {
                        r.state =
                            PlaceholderState::Failed(gst_obc::fallback_unreadable(path, &why));
                    }
                    return;
                }
            },
        };
        self.deliver(start, serial, path, key, frame);
    }

    /// Publishes the decoded `frame` of request `serial` for start `start`,
    /// uploaded first on a CUDA output, unless the output stopped (or
    /// started again) or a later request was made meanwhile. No lock is
    /// held across the upload (it may wait seconds on a wedged GPU), so
    /// [`Output::stop`] never waits for it; it uploads with its own
    /// reference to the start's context, and its frame is dropped unless
    /// that start still runs when it is done.
    fn deliver(
        &self,
        start: u64,
        serial: u64,
        path: &Path,
        key: Option<(PathBuf, FileStamp)>,
        frame: gst::Buffer,
    ) {
        let cuda = {
            let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            match &*gate {
                None => {
                    // Stopped: shown from the next start, without a decode.
                    let mut r = self.lock_request();
                    if r.serial == serial && r.decoded.is_some() {
                        r.state = PlaceholderState::Image;
                    }
                    return;
                }
                Some((running, _)) if *running != start => return,
                Some((_, cuda)) => cuda.clone(),
            }
        };
        let frame = match &cuda {
            Some(ctx) => upload_frame(&self.canonical, ctx, frame).map_err(|e| e.to_string()),
            None => Ok(frame),
        };
        // Dropped after the locks are released, if not published.
        let mut unused = None;
        {
            let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            let mut r = self.lock_request();
            let current = gate.as_ref().is_some_and(|(running, _)| *running == start)
                && r.serial == serial
                && r.running == Some(start);
            match frame {
                Ok(b) if !current => unused = Some(b),
                Err(_) if !current => {}
                Ok(b) => {
                    if cuda.is_some()
                        && let Some(k) = key
                    {
                        r.uploaded = Some((start, k, b.clone()));
                    }
                    self.show(Some(b));
                    r.state = PlaceholderState::Image;
                }
                Err(why) => {
                    r.state = PlaceholderState::Failed(gst_obc::fallback_unreadable(
                        path,
                        &format!("it could not be uploaded: {why}"),
                    ));
                }
            }
        }
        drop(unused);
        drop(cuda);
    }

    /// Start `start` of the output, with its CUDA context (`None` in system
    /// memory), before the clock's first tick: an image already decoded is
    /// shown at once (uploaded here on a CUDA output, with the request lock
    /// released, so a [`Output::set_placeholder_image`] meanwhile does not
    /// wait for it), another one is decoded on its own thread.
    fn started(self: &Arc<Self>, start: u64, cuda: Option<&gst::Context>) {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        *gate = Some((start, cuda.cloned()));
        let mut r = self.lock_request();
        r.running = Some(start);
        r.uploaded = None;
        let wanted = match (cuda, image_key(&r.image), &r.decoded) {
            (Some(ctx), Some(k), Some((dk, b))) if k == *dk => Some((ctx, k, b.clone(), r.serial)),
            _ => None,
        };
        if let Some((ctx, k, b, serial)) = wanted {
            drop(r);
            let uploaded = upload_frame(&self.canonical, ctx, b);
            r = self.lock_request();
            // A request made meanwhile is followed below instead.
            if r.serial == serial {
                match uploaded {
                    Ok(up) => r.uploaded = Some((start, k, up)),
                    Err(e) => {
                        self.show(None);
                        r.state = PlaceholderState::Failed(gst_obc::fallback_unreadable(
                            &k.0,
                            &format!("it could not be uploaded: {e}"),
                        ));
                        return;
                    }
                }
            }
        }
        self.apply(&mut r);
        drop(r);
        drop(gate);
    }

    /// The output stopped (or its start failed): once this returns no
    /// frame of its memory is published or kept but the decoded one (system
    /// memory). Never waits for an upload of a decode thread.
    fn stopped(&self) {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        *gate = None;
        let mut r = self.lock_request();
        r.running = None;
        let uploaded = r.uploaded.take();
        self.show(None);
        if r.state == PlaceholderState::Pending
            && image_key(&r.image)
                .is_some_and(|k| r.decoded.as_ref().is_some_and(|(dk, _)| *dk == k))
        {
            r.state = PlaceholderState::Image;
        }
        drop(r);
        drop(gate);
        drop(uploaded);
    }
}

/// Everything the clock thread owns.
struct ClockContext {
    pacer: Pacer,
    appsrc: gst_app::AppSrc,
    slot: Arc<Slot<Frame>>,
    counters: Arc<Counters>,
    hold_request: Arc<Mutex<Option<(u64, Instant)>>>,
    observer: Option<EmitObserver>,
    /// The black placeholder of this start.
    placeholder: gst::Buffer,
    /// What a placeholder tick shows instead, when an image is ready.
    image: Arc<PlaceholderImage>,
    /// The output forwards to a loopback writer, whose own placeholder
    /// covers a slot this clock leaves empty: a placeholder tick pushes
    /// nothing while the image is pending, instead of black.
    covered: bool,
    epoch: Instant,
    stop: Arc<StopSignal>,
}

impl ClockContext {
    fn run(mut self) {
        // The grid starts now; slot arrival times are moved to it.
        let origin = Instant::now();
        let shift = ns_since(self.epoch, origin);
        let mut discont = true;
        let mut last_fresh_source = 0;
        // appsrc's running count of the buffers it dropped (leaky queue).
        let queue_dropped = |a: &gst_app::AppSrc| a.property::<u64>("dropped");
        let mut dropped_seen = queue_dropped(&self.appsrc);
        while !self.stop.is_set() {
            let now = ns_since(origin, Instant::now());
            let skipped_before = self.pacer.counts().skipped;
            let (index, pts_ns) = match self.pacer.step(now) {
                Step::Wait { until_ns } => {
                    if self
                        .stop
                        .wait_until(origin + Duration::from_nanos(until_ns))
                    {
                        break;
                    }
                    continue;
                }
                Step::Tick { index, pts_ns } => (index, pts_ns),
            };
            discont |= self.pacer.counts().skipped != skipped_before;
            let phase_offset_ns = self.pacer.phase_offset_ns();
            let request = self
                .hold_request
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some((source_id, deadline)) = request {
                self.pacer.hold(source_id, ns_since(origin, deadline));
            }
            let latest = self.slot.latest();
            let kind = self.pacer.decide(
                latest.as_ref().map(|e| Latest {
                    seq: e.seq,
                    arrived_ns: e.arrived_ns.saturating_sub(shift),
                    source_id: e.frame.source_id,
                }),
                now,
            );
            // The first fresh frame of a source (see the module docs).
            let new_source = kind == TickKind::Fresh
                && latest
                    .as_ref()
                    .is_some_and(|e| e.frame.source_id != last_fresh_source);
            let mut image = None;
            let (source, origin_info) = match (kind, &latest) {
                (TickKind::Fresh | TickKind::Repeat, Some(e)) => (
                    &e.frame.buffer,
                    Some(FrameOrigin {
                        source_id: e.frame.source_id,
                        seq: e.seq,
                        source_pts_ns: e.frame.source_pts_ns,
                        arrived_at: self.epoch + Duration::from_nanos(e.arrived_ns),
                    }),
                ),
                _ => {
                    // A reference taken under a short lock: never a decode
                    // or an upload on this thread.
                    match self.image.for_tick() {
                        TickImage::Image(b) => image = Some(b),
                        // Nothing pushed: the writer's placeholder (the
                        // same away image) fills the slot. Counted as a
                        // placeholder, not emitted.
                        TickImage::Pending if self.covered => {
                            drop(latest);
                            // A gap in the pushed stream: the next buffer
                            // pushed is a discontinuity.
                            discont = true;
                            self.counters
                                .placeholder
                                .store(self.pacer.counts().placeholder, Ordering::Relaxed);
                            continue;
                        }
                        TickImage::Pending | TickImage::Black => {}
                    }
                    (image.as_ref().unwrap_or(&self.placeholder), None)
                }
            };
            // Shallow copy: new metadata, same (shared) CUDA memory.
            let mut buffer = source.copy();
            {
                let b = buffer.make_mut();
                b.set_pts(gst::ClockTime::from_nseconds(pts_ns));
                b.set_dts(gst::ClockTime::NONE);
                // Up to the next tick's PTS (exact for N/D rates).
                b.set_duration(gst::ClockTime::from_nseconds(
                    self.pacer.config().duration_ns(index),
                ));
                b.set_offset(index);
                b.set_offset_end(index + 1);
                if discont {
                    b.set_flags(gst::BufferFlags::DISCONT);
                } else {
                    b.unset_flags(gst::BufferFlags::DISCONT);
                }
                if new_source {
                    b.set_flags(gst_obc::NEW_SOURCE_FLAG);
                } else {
                    b.unset_flags(gst_obc::NEW_SOURCE_FLAG);
                }
                if kind == TickKind::Repeat && latest.is_some() {
                    b.set_flags(gst_obc::REPEAT_FLAG);
                } else {
                    b.unset_flags(gst_obc::REPEAT_FLAG);
                }
                if image.is_some() {
                    b.set_flags(gst_obc::PLACEHOLDER_FLAG);
                } else {
                    b.unset_flags(gst_obc::PLACEHOLDER_FLAG);
                }
            }
            drop(latest);
            let emitted_at = Instant::now();
            let pushed = self.appsrc.push_buffer(buffer).is_ok();
            discont = false;

            let queue_level = self.appsrc.property::<u64>("current-level-buffers");
            let c = &self.counters;
            if pushed {
                c.emitted.fetch_add(1, Ordering::Relaxed);
            } else {
                c.push_failed.fetch_add(1, Ordering::Relaxed);
            }
            c.queue_level_max.fetch_max(queue_level, Ordering::Relaxed);
            // A leaky drop happens inside push_buffer, on this thread.
            let dropped_now = queue_dropped(&self.appsrc);
            if dropped_now != dropped_seen {
                c.queue_dropped
                    .fetch_add(dropped_now.saturating_sub(dropped_seen), Ordering::Relaxed);
                dropped_seen = dropped_now;
            }
            if new_source && let Some(o) = &origin_info {
                last_fresh_source = o.source_id;
                c.fresh_source_since_ns
                    .store(ns_since(self.epoch, emitted_at), Ordering::Relaxed);
                c.fresh_source.store(o.source_id, Ordering::Release);
            }
            let n = self.pacer.counts();
            c.fresh.store(n.fresh, Ordering::Relaxed);
            c.repeat.store(n.repeat, Ordering::Relaxed);
            c.placeholder.store(n.placeholder, Ordering::Relaxed);
            c.superseded.store(n.superseded, Ordering::Relaxed);
            c.late.store(n.late, Ordering::Relaxed);
            c.skipped.store(n.skipped, Ordering::Relaxed);
            c.lock_acquisitions
                .store(n.lock_acquisitions, Ordering::Relaxed);
            c.lock_wraps.store(n.lock_wraps, Ordering::Relaxed);
            c.phase_offset_ns
                .store(n.phase_offset_ns, Ordering::Relaxed);
            c.phase_locked.store(n.phase_locked, Ordering::Relaxed);
            c.holds.store(n.holds, Ordering::Relaxed);
            c.holds_released.store(n.holds_released, Ordering::Relaxed);
            c.holds_expired.store(n.holds_expired, Ordering::Relaxed);
            if let Some(observer) = &self.observer {
                observer(&EmitInfo {
                    tick: index,
                    pts_ns,
                    kind,
                    origin: origin_info,
                    emitted_at,
                    grid_at: origin + Duration::from_nanos(pts_ns),
                    phase_offset_ns,
                    phase_locked: n.phase_locked,
                    pushed,
                    queue_level,
                });
            }
        }
    }
}

/// The `appsink` ending an output that forwards to a loopback writer: every
/// buffer is handed to `writer` by reference (the sample is released first,
/// so the writer's metadata change copies nothing it does not have to).
/// Unsynchronised, no last sample kept.
fn forward_sink(name: &str, writer: WriterHandle) -> gst::Element {
    let sink = gst_app::AppSink::builder()
        .name(name)
        .sync(false)
        .max_buffers(1)
        .leaky_type(gst_app::AppLeakyType::Downstream)
        .wait_on_eos(false)
        .enable_last_sample(false)
        .build();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;
                let buffer = sample.buffer_owned();
                drop(sample);
                if let Some(buffer) = buffer {
                    writer.push(buffer);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    sink.upcast()
}

/// NV12 layout as `GstVideoInfo` computes it for even sizes: rows padded to
/// 4 bytes, the interleaved chroma plane after the luma plane.
fn nv12_layout(width: u32, height: u32) -> (usize, usize) {
    let stride = (width as usize + 3) & !3;
    let luma = stride * height as usize;
    (luma, luma + stride * (height as usize / 2))
}

/// One black canonical frame in system memory.
fn black_frame(canonical: &Canonical) -> gst::Buffer {
    let (luma, size) = nv12_layout(canonical.width, canonical.height);
    let (y, uv) = canonical.black();
    let mut data = vec![uv; size];
    data[..luma].fill(y);
    let mut buffer = gst::Buffer::from_mut_slice(data);
    buffer
        .get_mut()
        .expect("a new buffer is writable")
        .set_pts(gst::ClockTime::ZERO);
    buffer
}

/// One black canonical frame in CUDA memory of the context `cuda`
/// ([`upload_frame`] of [`black_frame`]).
fn make_placeholder(
    canonical: &Canonical,
    cuda: &gst::Context,
) -> Result<gst::Buffer, PipelineError> {
    upload_frame(canonical, cuda, black_frame(canonical))
}

/// The canonical `frame` (system memory, laid out as [`black_frame`]) in
/// CUDA memory of the context `cuda`, uploaded through a one-shot pipeline:
/// `appsrc ! cudaupload ! capsfilter (canonical) ! appsink`.
fn upload_frame(
    canonical: &Canonical,
    cuda: &gst::Context,
    frame: gst::Buffer,
) -> Result<gst::Buffer, PipelineError> {
    let pipeline = gst::Pipeline::with_name("obc-placeholder");
    let src_caps = parse_caps(&format!("{},framerate=0/1", canonical.system_caps_string()))?;
    let src = gst_app::AppSrc::builder()
        .name("placeholder_src")
        .caps(&src_caps)
        .format(gst::Format::Time)
        .build();
    let sink = gst_app::AppSink::builder()
        .name("placeholder_sink")
        .sync(false)
        .enable_last_sample(false)
        .build();
    let chain = [
        src.clone().upcast(),
        make("cudaupload", "placeholder_upload")?,
        capsfilter("placeholder_caps", &canonical.caps_string())?,
        sink.clone().upcast(),
    ];
    pipeline.set_context(cuda);
    add_linked(&pipeline, &chain)?;
    let p = MediaPipeline::from_parts(pipeline, None);
    p.play()?;

    let mut frame = frame;
    frame.make_mut().set_pts(gst::ClockTime::ZERO);
    let pushed = src.push_buffer(frame).is_ok() && src.end_of_stream().is_ok();
    let sample = if pushed {
        sink.try_pull_sample(gst::ClockTime::from_seconds(5))
    } else {
        None
    };
    let error = p
        .bus()
        .pop_filtered(&[gst::MessageType::Error])
        .and_then(|m| match m.view() {
            gst::MessageView::Error(e) => Some(e.error().to_string()),
            _ => None,
        });
    p.shutdown()?;
    let buffer = sample.and_then(|s| s.buffer_owned()).ok_or_else(|| {
        PipelineError::Cuda(format!(
            "could not upload the placeholder frame{}",
            error.map(|e| format!(": {e}")).unwrap_or_default()
        ))
    })?;
    Ok(buffer)
}

/// What a failure `e` to link the output's element `from` to `to` is: the
/// module stage element's when either is one of `modules`
/// ([`PipelineError::Module`]), else the effect element's when either is
/// `effect` ([`PipelineError::Effect`]), else the pipeline's
/// ([`PipelineError::Link`], which counts as the output contract's fault).
fn link_failure(
    from: &str,
    to: &str,
    modules: &[String],
    effect: Option<&str>,
    e: &str,
) -> PipelineError {
    let is_module = |n: &str| modules.iter().any(|m| m == n);
    if let Some(m) = [from, to].into_iter().find(|n| is_module(n)) {
        return PipelineError::Module(format!("{m} cannot be linked ({from} to {to}): {e}"));
    }
    if let Some(fx) = effect.filter(|fx| *fx == from || *fx == to) {
        return PipelineError::Effect(format!("{fx} cannot be linked ({from} to {to}): {e}"));
    }
    PipelineError::Link(format!("{from} to {to}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fake_chain::FakeChainLog as FakeLog;

    #[test]
    fn a_link_failure_at_a_module_or_effect_element_is_theirs_not_a_link_error() {
        let modules = vec!["module-a".to_string(), "module-b".to_string()];
        let f = |from: &str, to: &str| link_failure(from, to, &modules, Some("effect"), "refused");
        // Either side a module element: the module's, even next to the
        // effect element.
        for (from, to) in [
            ("frames_src", "module-a"),
            ("module-a", "effect"),
            ("effect", "module-b"),
            ("module-b", "to_out"),
        ] {
            match f(from, to) {
                PipelineError::Module(m) => assert!(m.contains(from) && m.contains(to), "{m}"),
                other => panic!("{from} to {to}: {other:?}"),
            }
        }
        // The effect element without a module: the effect's.
        for (from, to) in [("frames_src", "effect"), ("effect", "download")] {
            assert!(
                matches!(f(from, to), PipelineError::Effect(_)),
                "{from} to {to}"
            );
        }
        // Neither: a link error (the contract's).
        match f("download", "download_caps") {
            PipelineError::Link(m) => assert_eq!(m, "download to download_caps: refused"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            link_failure("a", "effect", &[], None, "x"),
            PipelineError::Link(_)
        ));
    }
    use crate::test_support::require_elements;

    #[test]
    fn nv12_layout_matches_video_info() {
        assert_eq!(nv12_layout(1920, 1080), (1920 * 1080, 1920 * 1080 * 3 / 2));
        // 642 is even but not a multiple of 4: rows are padded to 644.
        assert_eq!(nv12_layout(642, 480), (644 * 480, 644 * 480 + 644 * 240));
    }

    #[test]
    fn contract_caps() {
        let c = OutputContract {
            format: OutputFormat::I420,
            fps_num: 30000,
            fps_den: 1001,
            ..OutputContract::default()
        };
        assert_eq!(
            contract_caps_string(&c),
            "video/x-raw,format=I420,width=1920,height=1080,framerate=30000/1001,\
             colorimetry=2:4:5:4"
        );
        // The sink label does not depend on the range: a loopback refuses
        // a full-range contract, and other sinks ignore the label.
        let full = OutputContract {
            range: ColourRange::Full,
            ..c
        };
        assert_eq!(contract_caps_string(&full), contract_caps_string(&c));
    }

    #[test]
    fn a_loopback_refuses_a_full_range_contract() {
        let full = OutputContract {
            range: ColourRange::Full,
            ..OutputContract::default()
        };
        let loopback = OutputSink::Loopback("/dev/null-sink".into());
        let err = check_sink_range(&full, &loopback).unwrap_err();
        match &err {
            PipelineError::LoopbackRefused {
                device,
                accepted,
                wanted,
            } => {
                assert_eq!(device, "/dev/null-sink");
                assert!(accepted.contains("limited range only"), "{accepted}");
                assert_eq!(wanted, "nv12:1920x1080@30 full");
            }
            other => panic!("expected LoopbackRefused, got {other}"),
        }
        assert!(err.to_string().contains("carries no colour range"), "{err}");
        // Limited range on a loopback, and either range elsewhere, pass.
        check_sink_range(&OutputContract::default(), &loopback).unwrap();
        check_sink_range(&full, &OutputSink::Fake).unwrap();

        // Output::build applies it before building anything (no element is
        // made, so no CUDA element is needed).
        gst::init().unwrap();
        assert!(matches!(
            Output::build(OutputConfig::new(full, loopback)),
            Err(PipelineError::LoopbackRefused { .. })
        ));
    }

    /// A full-range contract builds for a sink that carries the range.
    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_a_fakesink_takes_a_full_range_contract() {
        require_elements(&["cudadownload", "capssetter", "fakesink"]);
        let full = OutputContract {
            range: ColourRange::Full,
            ..OutputContract::default()
        };
        Output::build(OutputConfig::new(full, OutputSink::Fake)).unwrap();
    }

    #[test]
    fn a_panic_in_the_clock_loop_is_recorded() {
        let failure = Mutex::new(None);
        run_recording_panic(|| {}, &failure);
        assert_eq!(*failure.lock().unwrap(), None);
        run_recording_panic(|| panic!("observer failed at tick {}", 7), &failure);
        assert_eq!(
            failure.lock().unwrap().as_deref(),
            Some("observer failed at tick 7")
        );
        let failure = Mutex::new(None);
        run_recording_panic(|| panic!("static message"), &failure);
        assert_eq!(failure.lock().unwrap().as_deref(), Some("static message"));
    }

    /// A system-memory output on a fakesink, started.
    fn cpu_output_started() -> Output {
        require_elements(&["appsrc", "capssetter", "fakesink"]);
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let mut o = Output::build_in(
            OutputConfig::new(contract, OutputSink::Fake),
            VideoMemory::System,
        )
        .unwrap();
        o.start().unwrap();
        o
    }

    const IMAGE_ELEMENTS: &[&str] = &[
        "appsrc",
        "capssetter",
        "fakesink",
        "videotestsrc",
        "pngenc",
        "pngdec",
        "filesink",
        "videoconvert",
        "videocrop",
        "videoscale",
    ];

    /// The luma of each buffer reaching the output's sink: `Some(y)` when
    /// the frame is uniform (every luma byte `y`, every chroma byte 128),
    /// `None` otherwise.
    fn watch_luma(o: &Output) -> Arc<Mutex<Vec<Option<u8>>>> {
        let log: Arc<Mutex<Vec<Option<u8>>>> = Arc::default();
        let pad = o
            .pipeline()
            .element(names::LOOPBACK)
            .and_then(|e| e.static_pad("sink"))
            .unwrap();
        let l = Arc::clone(&log);
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                let info = pad
                    .current_caps()
                    .and_then(|c| gst_video::VideoInfo::from_caps(&c).ok())
                    .unwrap();
                let map = b.map_readable().unwrap();
                let luma_end =
                    info.offset()[0] + info.stride()[0] as usize * info.height() as usize;
                let y = map[0];
                let uniform = map.len() == info.size()
                    && map[..luma_end].iter().all(|&b| b == y)
                    && map[luma_end..].iter().all(|&b| b == 128);
                l.lock().unwrap().push(uniform.then_some(y));
            }
            gst::PadProbeReturn::Ok
        });
        log
    }

    fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + limit;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    const BLACK: Option<u8> = Some(16);
    const WHITE: Option<u8> = Some(235);

    /// Waits for `n` more buffers at the sink after the one in flight, and
    /// returns them.
    fn next_buffers(log: &Mutex<Vec<Option<u8>>>, n: usize) -> Vec<Option<u8>> {
        let from = log.lock().unwrap().len() + 1;
        wait_until(Duration::from_secs(10), "buffers at the sink", || {
            log.lock().unwrap().len() >= from + n
        });
        log.lock().unwrap()[from..from + n].to_vec()
    }

    /// The clock's placeholder in system memory (no frame is ever written,
    /// so every tick is one): black, the image once decoded, black again at
    /// once when cleared, the same file shown again at once (also after a
    /// restart), a file replaced at the same path read again, and an
    /// unreadable one black with its reason. CPU only.
    #[test]
    fn the_placeholder_shows_the_image_in_system_memory() {
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let png = crate::test_support::white_png(dir.path(), 160, 120);
        let mut o = cpu_output_started();
        let log = watch_luma(&o);
        assert_eq!(o.placeholder_state(), PlaceholderState::Black);
        assert_eq!(next_buffers(&log, 3), [BLACK; 3]);

        o.set_placeholder_image(Some(&png));
        wait_until(Duration::from_secs(10), "the image decoded", || {
            o.placeholder_state() == PlaceholderState::Image
        });
        assert_eq!(next_buffers(&log, 5), [WHITE; 5]);
        // Asked for again unchanged: nothing happens.
        o.set_placeholder_image(Some(&png));
        assert_eq!(o.placeholder_state(), PlaceholderState::Image);

        o.set_placeholder_image(None);
        assert_eq!(o.placeholder_state(), PlaceholderState::Black);
        assert_eq!(next_buffers(&log, 3), [BLACK; 3]);
        // The same file again: at once, no decode.
        o.set_placeholder_image(Some(&png));
        assert_eq!(o.placeholder_state(), PlaceholderState::Image);
        assert_eq!(next_buffers(&log, 3), [WHITE; 3]);

        // Kept across a restart, shown from the first placeholder on.
        o.stop().unwrap();
        assert_eq!(o.placeholder_state(), PlaceholderState::Image);
        log.lock().unwrap().clear();
        o.start().unwrap();
        assert_eq!(o.placeholder_state(), PlaceholderState::Image);
        wait_until(Duration::from_secs(10), "buffers after the restart", || {
            log.lock().unwrap().len() >= 3
        });
        assert!(log.lock().unwrap().iter().all(|y| *y == WHITE), "{log:?}");

        // Replaced at the same path by a file that is no image: read
        // afresh, black and Failed.
        o.set_placeholder_image(None);
        let moved = dir.path().join("moved.png");
        std::fs::rename(&png, &moved).unwrap();
        std::fs::write(&png, b"not an image").unwrap();
        o.set_placeholder_image(Some(&png));
        assert_ne!(o.placeholder_state(), PlaceholderState::Image);
        wait_until(Duration::from_secs(10), "the new file read", || {
            matches!(o.placeholder_state(), PlaceholderState::Failed(_))
        });
        let PlaceholderState::Failed(why) = o.placeholder_state() else {
            unreachable!()
        };
        assert!(why.contains("showing black"), "{why}");
        assert_eq!(next_buffers(&log, 3), [BLACK; 3]);

        // A missing file: Failed, black.
        o.set_placeholder_image(Some(&dir.path().join("gone.png")));
        wait_until(Duration::from_secs(10), "the missing file failed", || {
            matches!(o.placeholder_state(), PlaceholderState::Failed(_))
        });
        assert_eq!(next_buffers(&log, 3), [BLACK; 3]);
        o.stop().unwrap();
    }

    /// An image given in the config, or asked for while the output is
    /// stopped, is not decoded before the start (pending), then shown.
    /// CPU only.
    #[test]
    fn an_image_asked_for_while_stopped_is_decoded_at_the_start() {
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let png = crate::test_support::white_png(dir.path(), 64, 64);
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let mut cfg = OutputConfig::new(contract, OutputSink::Fake);
        cfg.placeholder_image = Some(png.clone());
        let mut o = Output::build_in(cfg, VideoMemory::System).unwrap();
        let log = watch_luma(&o);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(o.placeholder_state(), PlaceholderState::Pending);
        assert!(
            o.away.lock_request().decoded.is_none(),
            "decoded while stopped"
        );
        o.start().unwrap();
        wait_until(Duration::from_secs(10), "the image decoded", || {
            o.placeholder_state() == PlaceholderState::Image
        });
        assert_eq!(next_buffers(&log, 3), [WHITE; 3]);
        o.stop().unwrap();
        // Black asked for while stopped: black from the start on.
        o.set_placeholder_image(None);
        log.lock().unwrap().clear();
        o.start().unwrap();
        wait_until(Duration::from_secs(10), "buffers", || {
            log.lock().unwrap().len() >= 3
        });
        assert!(log.lock().unwrap().iter().all(|y| *y == BLACK), "{log:?}");
    }

    /// The placeholder image on a CUDA output: uploaded once per start
    /// into the output's context, downloaded white at the sink; black at
    /// once when cleared, and again at once when asked for again.
    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_the_placeholder_shows_the_image_in_cuda_memory() {
        require_elements(IMAGE_ELEMENTS);
        require_elements(&["cudaupload", "cudadownload"]);
        let dir = tempfile::tempdir().unwrap();
        let png = crate::test_support::white_png(dir.path(), 160, 120);
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let mut o = Output::build(OutputConfig::new(contract, OutputSink::Fake)).unwrap();
        let log = watch_luma(&o);
        o.start().unwrap();
        assert_eq!(next_buffers(&log, 3), [BLACK; 3]);
        o.set_placeholder_image(Some(&png));
        wait_until(Duration::from_secs(10), "the image uploaded", || {
            o.placeholder_state() == PlaceholderState::Image
        });
        assert_eq!(next_buffers(&log, 5), [WHITE; 5]);
        o.set_placeholder_image(None);
        assert_eq!(next_buffers(&log, 3), [BLACK; 3]);
        o.set_placeholder_image(Some(&png));
        assert_eq!(o.placeholder_state(), PlaceholderState::Image);
        assert_eq!(next_buffers(&log, 3), [WHITE; 3]);
        // A restart uploads the image kept, before the first tick.
        o.stop().unwrap();
        assert!(o.away.lock_request().uploaded.is_none(), "CUDA frame kept");
        assert!(o.away.shown().is_none());
        log.lock().unwrap().clear();
        o.start().unwrap();
        assert_eq!(o.placeholder_state(), PlaceholderState::Image);
        wait_until(Duration::from_secs(10), "buffers", || {
            log.lock().unwrap().len() >= 3
        });
        assert!(log.lock().unwrap().iter().all(|y| *y == WHITE), "{log:?}");
        o.stop().unwrap();
    }

    /// An output forwarding to a writer is attached to it from a start that
    /// succeeded to its stop (a second start adds nothing, a restart
    /// attaches again, a drop detaches), bound the shorter of its
    /// `hold_timeout` and [`Output::STALL_HOLD`]; a start that fails leaves
    /// it unattached. The failure is a `filesrc` on a missing file added to
    /// the output's pipeline: it fails the change to PAUSED. CPU only.
    #[test]
    fn a_writer_output_is_attached_from_a_successful_start_to_its_stop() {
        use crate::loopback::{LoopbackWriter, WriterSink};
        require_elements(&["appsrc", "capssetter", "appsink", "fakesink", "filesrc"]);
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let h = w.handle();
        let output = |hold_timeout| {
            let mut cfg = OutputConfig::new(contract, OutputSink::Writer(w.handle()));
            cfg.hold_timeout = hold_timeout;
            Output::build_in(cfg, VideoMemory::System).unwrap()
        };

        let mut o = output(Duration::from_secs(60));
        assert!(h.attached_holds().is_empty());
        o.start().unwrap();
        assert_eq!(h.attached_holds(), [Output::STALL_HOLD]);
        o.start().unwrap();
        assert_eq!(h.attached_holds(), [Output::STALL_HOLD]);
        o.stop().unwrap();
        assert!(h.attached_holds().is_empty());
        o.start().unwrap();
        assert_eq!(h.attached_holds(), [Output::STALL_HOLD], "re-attached");
        o.stop().unwrap();
        assert!(h.attached_holds().is_empty());

        let bad = gst::ElementFactory::make("filesrc")
            .property("location", "/nonexistent/obc-output-start-failure")
            .build()
            .unwrap();
        o.pipeline().pipeline().add(&bad).unwrap();
        assert!(o.start().is_err());
        assert!(!o.is_running());
        assert!(h.attached_holds().is_empty(), "attached by a failed start");
        bad.set_state(gst::State::Null).unwrap();
        o.pipeline().pipeline().remove(&bad).unwrap();
        o.start().unwrap();
        assert_eq!(h.attached_holds(), [Output::STALL_HOLD]);

        // A shorter hold_timeout is the bound; each output has its own
        // attachment, and the drop ends only its own.
        let mut short = output(Duration::from_millis(200));
        short.start().unwrap();
        assert_eq!(
            h.attached_holds(),
            [Output::STALL_HOLD, Duration::from_millis(200)]
        );
        drop(o);
        assert_eq!(h.attached_holds(), [Duration::from_millis(200)]);
        drop(short);
        assert!(h.attached_holds().is_empty());
    }

    /// A clock thread that panicked is an error, but the pipeline is taken
    /// to NULL all the same: `reached_null` is true. CPU only.
    #[test]
    fn a_stop_after_an_observer_panic_reached_null() {
        require_elements(&["appsrc", "capssetter", "fakesink"]);
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let mut o = Output::build_in(
            OutputConfig::new(contract, OutputSink::Fake),
            VideoMemory::System,
        )
        .unwrap();
        o.set_observer(Some(Arc::new(|_: &EmitInfo| panic!("observer failed"))));
        o.start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while o.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!o.is_running(), "the clock thread ended");
        let e = o.stop_checked().unwrap_err();
        assert!(e.reached_null, "{e}");
        assert!(e.to_string().contains("observer failed"), "{e}");
        // Idempotent: nothing left to fail.
        assert!(o.stop_checked().is_ok());
    }

    /// A pipeline that ends its stop outside NULL (the bus handler raises
    /// it back to READY as it reaches NULL) is `reached_null` false. CPU
    /// only.
    #[test]
    fn a_stop_that_ends_outside_null_did_not_reach_null() {
        let mut o = cpu_output_started();
        o.pipeline().pipeline().set_auto_flush_bus(false);
        let pipeline = o.pipeline().pipeline().downgrade();
        o.pipeline().bus().set_sync_handler(move |_, msg| {
            if let gst::MessageView::StateChanged(c) = msg.view()
                && c.current() == gst::State::Null
                && let Some(p) = pipeline.upgrade()
                && msg.src() == Some(p.upcast_ref::<gst::Object>())
            {
                let _ = p.set_state(gst::State::Ready);
            }
            gst::BusSyncReply::Pass
        });
        let e = o.stop_checked().unwrap_err();
        assert!(!e.reached_null, "{e}");
        assert!(e.to_string().contains("not Null"), "{e}");
    }

    #[test]
    fn a_dead_clock_thread_is_reported() {
        // A clock thread that ended without being stopped.
        let failure: Arc<Mutex<Option<String>>> = Arc::default();
        let f = Arc::clone(&failure);
        let handle = std::thread::spawn(move || run_recording_panic(|| panic!("boom"), &f));
        while !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let clock = ClockThread {
            handle,
            stop: Arc::default(),
            failure,
        };
        assert_eq!(
            clock.failure().as_deref(),
            Some("the output clock thread panicked: boom")
        );
        // A thread that returned without a stop request (no panic).
        let handle = std::thread::spawn(|| {});
        while !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let clock = ClockThread {
            handle,
            stop: Arc::default(),
            failure: Arc::default(),
        };
        assert!(clock.failure().unwrap().contains("ended unexpectedly"));
        // A stopped thread is not a failure.
        clock.stop.set();
        assert_eq!(clock.failure(), None);
    }

    #[test]
    fn a_restart_resumes_after_the_frames_already_handled() {
        // The slot still holds frame 41: it is fresh after the restart.
        assert_eq!(resume_sequence(Some(41), 41), 40);
        // An emptied slot after 41 writes: the next write (42) is fresh.
        assert_eq!(resume_sequence(None, 41), 41);
        // A first start.
        assert_eq!(resume_sequence(None, 0), 0);
    }

    /// The queue bound is `queue_buffers` (default 3), and the clock's
    /// catch-up burst is the same bound. (Until v0.2.0 both followed the
    /// 1 s resynchronisation threshold: 31 buffers at 30 fps.)
    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_the_appsrc_queue_is_the_configured_bound_and_the_burst_cap() {
        require_elements(&["cudadownload", "capssetter", "fakesink"]);
        for (rate, queue) in [
            ("30", None),
            ("60", Some(5)),
            ("30000/1001", Some(1)),
            ("240", None),
        ] {
            let contract: OutputContract = format!("1280x720@{rate}").parse().unwrap();
            let mut cfg = OutputConfig::new(contract, OutputSink::Fake);
            if let Some(q) = queue {
                cfg.queue_buffers = q;
            }
            let want = u64::from(queue.unwrap_or(OutputConfig::DEFAULT_QUEUE_BUFFERS));
            let pacer = cfg.pacer();
            assert_eq!(pacer.max_burst, want, "{rate}");
            let o = Output::build(cfg).unwrap();
            let src = o.pipeline().element(names::FRAMES_SRC).unwrap();
            assert_eq!(src.property::<u64>("max-buffers"), want, "{rate}");
            let leaky = src.property_value("leaky-type");
            assert_eq!(
                leaky.get::<gst_app::AppLeakyType>().unwrap(),
                gst_app::AppLeakyType::Downstream,
                "drops the oldest"
            );
        }
        assert_eq!(OutputConfig::DEFAULT_QUEUE_BUFFERS, 3);
    }

    /// A sink slower than the push rate (100 ms per buffer against a push
    /// every 33 ms), behind an `appsrc` built like the output's: the queue
    /// never holds more than its bound, the excess is dropped oldest first
    /// (and counted), and a buffer reaching the sink has at most the bound
    /// of newer buffers queued behind it. System memory only: no GPU
    /// needed. The assertions count buffers, not wall-clock time, so a
    /// loaded machine does not change the outcome.
    #[test]
    fn a_slow_sink_sees_bounded_queue_and_frame_age() {
        require_elements(&["appsrc", "fakesink"]);
        const BOUND: u64 = 3;
        const SERVICE: Duration = Duration::from_millis(100);
        let period = Duration::from_nanos(1_000_000_000 / 30);
        let caps: gst::Caps = "video/x-raw,format=GRAY8,width=16,height=16,framerate=30/1"
            .parse()
            .unwrap();
        let src = frames_src("slow_src", &caps, BOUND);
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("signal-handoffs", true)
            .build()
            .unwrap();
        // Per buffer reaching the sink: its index, how many buffers were
        // queued behind it then, and how long ago it was pushed. A push
        // and its count happen under the `pushed_at` lock, and buffers
        // only leave the queue through this callback (or a drop, at a
        // push), so pushed - dropped - delivered under that lock is the
        // queue's content at that moment, however late the callback runs.
        let pushed_at: Arc<Mutex<Vec<Instant>>> = Arc::default();
        let seen: Arc<Mutex<Vec<(u64, u64, Duration)>>> = Arc::default();
        let (p, a, s) = (Arc::clone(&pushed_at), Arc::clone(&seen), src.clone());
        sink.connect("handoff", false, move |args| {
            let buffer = args[1].get::<gst::Buffer>().unwrap();
            let index = buffer.offset();
            let (queued, age) = {
                let pushed = p.lock().unwrap();
                let dropped = s.property::<u64>("dropped");
                let delivered = a.lock().unwrap().len() as u64 + 1;
                let queued = (pushed.len() as u64).saturating_sub(dropped + delivered);
                (queued, pushed[index as usize].elapsed())
            };
            a.lock().unwrap().push((index, queued, age));
            std::thread::sleep(SERVICE);
            None
        });
        let pipeline = gst::Pipeline::new();
        add_linked(&pipeline, &[src.clone().upcast(), sink]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let pushes = 60u64;
        let mut level_max = 0;
        let start = Instant::now();
        for i in 0..pushes {
            let due = start + period * u32::try_from(i).unwrap();
            if let Some(d) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; 16 * 16]);
            {
                let b = buffer.get_mut().unwrap();
                b.set_pts(gst::ClockTime::from_nseconds(i * period.as_nanos() as u64));
                b.set_offset(i);
            }
            let pushed = {
                let mut pushed_at = pushed_at.lock().unwrap();
                pushed_at.push(Instant::now());
                src.push_buffer(buffer)
            };
            assert!(pushed.is_ok());
            let level = src.property::<u64>("current-level-buffers");
            level_max = level_max.max(level);
        }
        // Let the sink drain what is queued: every buffer pushed is either
        // delivered or dropped (counted).
        let deadline = Instant::now() + Duration::from_secs(20);
        let dropped = loop {
            let dropped = src.property::<u64>("dropped");
            let delivered = seen.lock().unwrap().len() as u64;
            if delivered + dropped >= pushes || Instant::now() > deadline {
                break dropped;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        pipeline.set_state(gst::State::Null).unwrap();

        let seen = seen.lock().unwrap().clone();
        let delivered = seen.len() as u64;
        let queued_max = seen.iter().map(|s| s.1).max().unwrap();
        let oldest = seen.iter().map(|s| s.2).max().unwrap();
        eprintln!(
            "slow sink: {pushes} pushed, {delivered} delivered, {dropped} dropped, queue level max \
             {level_max}, at most {queued_max} buffers queued behind one reaching the sink, oldest \
             buffer at the sink {oldest:?}"
        );
        assert!(level_max <= BOUND, "queue level {level_max}");
        assert!(dropped > 0, "nothing dropped");
        assert_eq!(delivered + dropped, pushes);
        // Delivered in push order.
        assert!(seen.windows(2).all(|w| w[0].0 < w[1].0));
        // Dropped oldest first: the newest buffers, last in the queue when
        // the pushes stopped, all reach the sink (dropping the newest would
        // have lost the last pushes into a full queue).
        let tail: Vec<u64> = seen
            .iter()
            .rev()
            .take(BOUND as usize)
            .map(|s| s.0)
            .collect();
        assert_eq!(
            tail,
            (pushes - BOUND..pushes).rev().collect::<Vec<_>>(),
            "the last {BOUND} pushed were delivered"
        );
        // No backlog: when a buffer reaches the sink, the queue behind it
        // holds at most the bound, however long the streaming thread took
        // from the dequeue to the callback (the pushes meanwhile drop the
        // oldest). An unbounded queue would hold dozens.
        assert!(queued_max <= BOUND, "{queued_max} buffers queued");
    }

    #[test]
    fn stop_signal_wakes_a_sleeper() {
        let s = Arc::new(StopSignal::default());
        assert!(!s.wait_until(Instant::now() + Duration::from_millis(5)));
        let s2 = Arc::clone(&s);
        let t = std::thread::spawn(move || s2.wait_until(Instant::now() + Duration::from_secs(60)));
        std::thread::sleep(Duration::from_millis(20));
        let t0 = Instant::now();
        s.set();
        assert!(t.join().unwrap());
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert!(s.is_set());
    }

    /// The pipeline per contract format and process stage, without starting
    /// it (no GPU needed).
    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_output_elements_per_format_and_stage() {
        require_elements(&[
            "cudaconvert",
            "cudadownload",
            "capssetter",
            "v4l2sink",
            "fakesink",
        ]);
        for (format, stage) in [
            (OutputFormat::Nv12, GpuStage::Passthrough),
            (OutputFormat::I420, GpuStage::Passthrough),
            (OutputFormat::Nv12, GpuStage::Roundtrip),
            (OutputFormat::I420, GpuStage::Roundtrip),
        ] {
            let contract = OutputContract {
                format,
                ..OutputContract::default()
            };
            let mut cfg =
                OutputConfig::new(contract, OutputSink::Loopback("/dev/null-sink".into()));
            cfg.process = stage;
            let o = Output::build(cfg).unwrap();
            let p = o.pipeline();
            let tag = format!("{format} {stage}");
            assert_eq!(
                p.element(names::TO_OUT).is_some(),
                format == OutputFormat::I420,
                "{tag}"
            );
            assert_eq!(
                p.element(names::TO_BGR).is_some(),
                stage == GpuStage::Roundtrip,
                "{tag}"
            );
            let sink = p.element(names::LOOPBACK).unwrap();
            assert_eq!(sink.factory().unwrap().name(), "v4l2sink");
            assert!(!sink.property::<bool>("sync"));
            let src = p.element(names::FRAMES_SRC).unwrap();
            let caps = src.property::<gst::Caps>("caps").to_string();
            assert!(
                caps.contains("memory:CUDAMemory") && caps.contains("framerate=(fraction)30/1"),
                "{caps}"
            );
            assert!(src.property::<bool>("is-live"));
            let out = p
                .element(names::OUT_CAPS)
                .unwrap()
                .property::<gst::Caps>("caps")
                .to_string();
            assert!(
                out.contains(&format!("format=(string){}", format.gst_name()))
                    && out.contains("colorimetry=(string)2:4:5:4"),
                "{out}"
            );
            // Downloaded frames keep the canonical label until the relabel.
            let down = p
                .element(names::DOWNLOAD_CAPS)
                .unwrap()
                .property::<gst::Caps>("caps")
                .to_string();
            assert!(down.contains("colorimetry=(string)2:4:5:1"), "{down}");
            assert!(!down.contains("CUDAMemory"), "{down}");
            let relabel = p.element(names::RELABEL).unwrap();
            assert_eq!(relabel.factory().unwrap().name(), "capssetter");
            assert!(!o.is_running());
            assert!(o.cuda_context().is_none());
        }
        let fake = Output::build(OutputConfig::new(
            OutputContract::default(),
            OutputSink::Fake,
        ))
        .unwrap();
        let sink = fake.pipeline().element(names::LOOPBACK).unwrap();
        assert_eq!(sink.factory().unwrap().name(), "fakesink");
        // Writers get distinct source ids.
        assert_ne!(
            fake.frame_writer().source_id(),
            fake.frame_writer().source_id()
        );
    }

    /// System-memory frames: no download, `videoconvert` in place of
    /// `cudaconvert` for a contract format other than NV12, the effect
    /// element in place, no element of the `nvcodec` plugin, and a Process
    /// stage that needs CUDA refused. CPU only.
    #[test]
    fn a_system_memory_output_has_no_cuda_element() {
        require_elements(&["appsrc", "videoconvert", "capssetter", "fakesink"]);
        for format in [OutputFormat::Nv12, OutputFormat::I420] {
            let contract = OutputContract {
                format,
                ..OutputContract::default()
            };
            let mut cfg = OutputConfig::new(contract, OutputSink::Fake);
            cfg.effect = Some(EffectSettings::default());
            let o = Output::build_in(cfg, VideoMemory::System).unwrap();
            let p = o.pipeline();
            crate::test_support::assert_no_cuda_element(p.pipeline());
            assert!(p.element(names::EFFECT).is_some(), "{format}");
            assert!(p.element(names::DOWNLOAD).is_none(), "{format}");
            assert!(p.element(names::OUT_CUDA_CAPS).is_none(), "{format}");
            assert_eq!(
                p.element(names::TO_OUT)
                    .map(|e| e.factory().unwrap().name().to_string()),
                (format == OutputFormat::I420).then(|| "videoconvert".to_string()),
                "{format}"
            );
            let src = p.element(names::FRAMES_SRC).unwrap();
            let caps = src.property::<gst::Caps>("caps").to_string();
            assert!(!caps.contains("CUDAMemory"), "{caps}");
            assert_eq!(o.frame_writer().memory(), VideoMemory::System);
        }
        let mut cfg = OutputConfig::new(OutputContract::default(), OutputSink::Fake);
        cfg.process = GpuStage::Roundtrip;
        assert!(matches!(
            Output::build_in(cfg, VideoMemory::System),
            Err(PipelineError::Cuda(_))
        ));
    }

    /// The first fresh frame of each source carries the new-source flag
    /// out of `appsrc`'s queue and down to the sink; no other buffer does
    /// (later fresh frames, repeats, placeholders), even when the slot's
    /// buffer carries it. A source that comes back is new again. The
    /// repeat flag is on the repeat ticks, and on no other buffer. CPU
    /// only.
    #[test]
    fn the_first_fresh_frame_of_each_source_is_flagged_down_to_the_sink() {
        require_elements(&["appsrc", "capssetter", "fakesink"]);
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let mut o = Output::build_in(
            OutputConfig::new(contract, OutputSink::Fake),
            VideoMemory::System,
        )
        .unwrap();
        let ticks: Arc<Mutex<Vec<EmitInfo>>> = Arc::default();
        let t = Arc::clone(&ticks);
        o.set_observer(Some(Arc::new(move |e: &EmitInfo| {
            t.lock().unwrap().push(*e);
        })));
        // (tick, flagged, repeat) of every buffer reaching the sink.
        let at_sink: Arc<Mutex<Vec<(u64, bool, bool)>>> = Arc::default();
        let a = Arc::clone(&at_sink);
        o.pipeline()
            .element(names::LOOPBACK)
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                    let flagged = gst_obc::has_new_source_flag(b);
                    let repeat = gst_obc::has_repeat_flag(b);
                    a.lock().unwrap().push((b.offset(), flagged, repeat));
                }
                gst::PadProbeReturn::Ok
            });
        o.start().unwrap();
        let first = o.frame_writer();
        let second = o.frame_writer();
        let canonical = first.canonical();
        let feed = |w: &FrameWriter, n: u64| {
            for i in 0..n {
                let mut b = black_frame(&canonical);
                let r = b.get_mut().unwrap();
                r.set_pts(gst::ClockTime::from_mseconds(i * 33));
                // A flag already on the slot's buffer must not leak out.
                r.set_flags(gst_obc::NEW_SOURCE_FLAG | gst_obc::REPEAT_FLAG);
                w.put(b);
                std::thread::sleep(Duration::from_millis(33));
            }
        };
        feed(&first, 10);
        feed(&second, 10);
        feed(&first, 10);
        // Repeats of the last frame.
        std::thread::sleep(Duration::from_millis(200));
        o.stop().unwrap();

        let ticks = ticks.lock().unwrap().clone();
        let mut last = 0;
        let mut new_source = Vec::new();
        for e in &ticks {
            if e.kind == TickKind::Fresh
                && let Some(origin) = e.origin
                && origin.source_id != last
            {
                last = origin.source_id;
                new_source.push(e.tick);
            }
        }
        assert_eq!(new_source.len(), 3, "first, second, first again: {ticks:?}");
        let at_sink = at_sink.lock().unwrap().clone();
        assert!(at_sink.len() > 30, "{} buffers at the sink", at_sink.len());
        let flagged: Vec<u64> = at_sink.iter().filter(|s| s.1).map(|s| s.0).collect();
        assert_eq!(flagged, new_source, "flagged buffers at the sink");
        assert!(
            ticks.iter().any(|e| e.kind == TickKind::Repeat),
            "the run had repeats"
        );
        let repeats: Vec<u64> = ticks
            .iter()
            .filter(|e| e.kind == TickKind::Repeat)
            .map(|e| e.tick)
            .collect();
        let marked: Vec<u64> = at_sink.iter().filter(|s| s.2).map(|s| s.0).collect();
        assert_eq!(marked, repeats, "repeat-flagged buffers at the sink");
    }

    #[test]
    fn the_default_config_has_no_effect_element() {
        let cfg = OutputConfig::new(OutputContract::default(), OutputSink::Fake);
        assert_eq!(cfg.effect, None);
        assert!(cfg.effect_backend.is_none(), "no built-in backend");
        require_elements(&["appsrc", "videoconvert", "capssetter", "fakesink"]);
        let o = Output::build_in(cfg, VideoMemory::System).unwrap();
        assert!(o.pipeline().element(names::EFFECT).is_none());
        assert_eq!(o.effect_status(), EffectStatus::Off);
        assert_eq!(o.effect_stats(), None);
        assert_eq!(o.effect_settings(), None);
        let e = o
            .set_effect(&EffectSettings::of(crate::EffectKind::Blur))
            .unwrap_err();
        assert!(matches!(e, PipelineError::Effect(_)), "{e}");
        assert!(e.to_string().contains("without the effect element"), "{e}");
    }

    /// The effect element takes system-memory frames too, and does not
    /// combine with the roundtrip stage, refused before any element is
    /// made. CPU only.
    #[test]
    fn an_effect_takes_system_memory_but_no_roundtrip() {
        require_elements(&["appsrc", "capssetter", "fakesink"]);
        let mut cfg = OutputConfig::new(OutputContract::default(), OutputSink::Fake);
        cfg.effect = Some(EffectSettings::default());
        let o = Output::build_in(cfg.clone(), VideoMemory::System).unwrap();
        assert!(o.pipeline().element(names::EFFECT).is_some());
        assert_eq!(o.memory(), VideoMemory::System);
        drop(o);
        cfg.process = GpuStage::Roundtrip;
        assert!(matches!(
            Output::build_in(cfg.clone(), VideoMemory::System),
            Err(PipelineError::Cuda(_))
        ));
        let e = Output::build(cfg.clone()).unwrap_err();
        assert!(
            matches!(&e, PipelineError::Effect(m) if m.contains("roundtrip")),
            "{e}"
        );
        cfg.process = GpuStage::Passthrough;
        cfg.effect = Some(EffectSettings::of(crate::EffectKind::Replace));
        let e = Output::build(cfg).unwrap_err();
        assert!(
            matches!(&e, PipelineError::Effect(m) if m.contains("background")),
            "{e}"
        );
    }

    /// A system-memory output with two module stage elements, one before
    /// the place of the effect and one after it, with the fake host chain
    /// of gst-obc's tests: both are in the pipeline in that order, the
    /// later one's chroma reaches the sink, a live change applies, an
    /// element with its stage off passes frames through, and statuses
    /// reach the getters and the bus. CPU only.
    #[test]
    fn module_stages_run_around_the_effect_on_system_memory() {
        use crate::module::{ModulePlacement, ModuleStageSpec, STATUS_MESSAGE, element_name};
        use crate::test_support::fake_chain::{FakeChainConfig, FakeChainFactory};
        use gst_obc::{StageId, StageRequest};
        use gst_video::prelude::*;
        require_elements(&["appsrc", "capssetter", "fakesink"]);
        let tint = |u: i64| StageRequest::on(StageId::new("tint").unwrap()).with_live("u", u);
        let fake = FakeChainFactory::host(FakeChainConfig::default());
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let mut cfg = OutputConfig::new(contract, OutputSink::Fake);
        // Listed after-first: the placement, not the list, orders them.
        cfg.module_stages = vec![
            ModuleStageSpec {
                name: "post".into(),
                factory: fake.clone(),
                stages: vec![tint(70)],
                placement: ModulePlacement::AfterEffect,
            },
            ModuleStageSpec {
                name: "pre".into(),
                factory: fake.clone(),
                stages: vec![tint(60)],
                placement: ModulePlacement::BeforeEffect,
            },
        ];
        let mut o = Output::build_in(cfg.clone(), VideoMemory::System).unwrap();
        assert_eq!(o.module_names(), ["pre", "post"]);
        let order: Vec<String> = {
            let mut v = Vec::new();
            let mut e = o.pipeline().element(names::FRAMES_SRC);
            while let Some(el) = e {
                v.push(el.name().to_string());
                e = el
                    .static_pad("src")
                    .and_then(|p| p.peer())
                    .and_then(|p| p.parent_element());
            }
            v
        };
        let pre = order
            .iter()
            .position(|n| *n == element_name("pre"))
            .unwrap();
        let post = order
            .iter()
            .position(|n| *n == element_name("post"))
            .unwrap();
        assert_eq!((pre, post), (1, 2), "{order:?}");
        // The chroma of the frame centre at the sink.
        let uv: Arc<Mutex<Vec<u8>>> = Arc::default();
        let u = Arc::clone(&uv);
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, 320, 240)
            .build()
            .unwrap();
        o.pipeline()
            .element(names::LOOPBACK)
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, probe| {
                if let Some(b) = probe.buffer()
                    && let Ok(f) = gst_video::VideoFrameRef::from_buffer_ref_readable(b, &info)
                {
                    let stride = f.plane_stride()[1] as usize;
                    u.lock()
                        .unwrap()
                        .push(f.plane_data(1).unwrap()[60 * stride + 160]);
                }
                gst::PadProbeReturn::Ok
            });
        o.start().unwrap();
        let writer = o.frame_writer();
        let canonical = writer.canonical();
        let feed_until = |done: &dyn Fn(&Output) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(&o) {
                assert!(Instant::now() < deadline, "{:?}", o.module_stats("post"));
                writer.put(black_frame(&canonical));
                std::thread::sleep(Duration::from_millis(33));
            }
        };
        let ready = |name: &'static str| {
            move |o: &Output| o.module_stage_status(name, "tint") == EffectStatus::Ready
        };
        feed_until(&|o| ready("pre")(o) && ready("post")(o));
        let at = || uv.lock().unwrap().len();
        let n = at();
        feed_until(&|_| at() >= n + 5);
        assert_eq!(
            *uv.lock().unwrap().last().unwrap(),
            70,
            "the later stage wins"
        );
        // A live change, then the later element off: the earlier one shows.
        o.set_module_stages("post", &[tint(80)]).unwrap();
        feed_until(&|_| uv.lock().unwrap().last() == Some(&80));
        o.set_module_stages("post", &[StageRequest::new(StageId::new("tint").unwrap())])
            .unwrap();
        feed_until(&|_| uv.lock().unwrap().last() == Some(&60));
        assert_eq!(o.module_stage_status("post", "tint"), EffectStatus::Off);
        assert_eq!(FakeLog::get(&fake.log.loads), 2, "one load per element");
        let stats = o.module_stats("pre").unwrap();
        assert!(
            stats.frames_processed > 5 && stats.module == "fake",
            "{stats:?}"
        );
        assert!(!o.module_stages("post").unwrap()[0].enabled);
        assert_eq!(o.module_stats("nope"), None);
        assert_eq!(o.module_stage_status("nope", "tint"), EffectStatus::Off);
        let e = o.set_module_stages("nope", &[]).unwrap_err();
        assert!(matches!(e, PipelineError::Module(_)), "{e}");
        let e = o.set_module_stages("pre", &[tint(1), tint(2)]).unwrap_err();
        assert!(e.to_string().contains("listed twice"), "{e}");
        // The status messages, from each element.
        let mut seen = Vec::new();
        while let Some(m) = o
            .pipeline()
            .bus()
            .pop_filtered(&[gst::MessageType::Element])
        {
            if let Some(s) = m.structure()
                && s.name() == STATUS_MESSAGE
            {
                let src = m.src().map(|s| s.name().to_string()).unwrap_or_default();
                seen.push((src, s.get::<String>("tint").unwrap()));
            }
        }
        for name in ["pre", "post"] {
            assert!(
                seen.contains(&(element_name(name), "ready".to_string())),
                "{name}: {seen:?}"
            );
        }
        o.stop().unwrap();
        assert_eq!(FakeLog::get(&fake.log.unloads), 2);

        // A chain that takes CUDA frames only is refused on system memory,
        // before anything is built; so is a name used twice.
        let mut cuda_only = cfg.clone();
        cuda_only.module_stages[0].factory = FakeChainFactory::cuda(FakeChainConfig::default());
        let e = Output::build_in(cuda_only, VideoMemory::System).unwrap_err();
        assert!(e.to_string().contains("cuda frames only"), "{e}");
        let mut twice = cfg;
        twice.module_stages[1].name = "post".into();
        let e = Output::build_in(twice, VideoMemory::System).unwrap_err();
        assert!(e.to_string().contains("used twice"), "{e}");
    }

    #[test]
    fn the_default_config_has_no_module_stage_element() {
        require_elements(&["appsrc", "capssetter", "fakesink"]);
        let cfg = OutputConfig::new("320x240@30".parse().unwrap(), OutputSink::Fake);
        assert!(cfg.module_stages.is_empty());
        let o = Output::build_in(cfg, VideoMemory::System).unwrap();
        assert!(o.module_names().is_empty());
        let mut e = o.pipeline().element(names::FRAMES_SRC);
        while let Some(el) = e {
            assert!(
                el.downcast_ref::<gst_obc::ObcModule>().is_none(),
                "no module element: {}",
                el.name()
            );
            e = el
                .static_pad("src")
                .and_then(|p| p.peer())
                .and_then(|p| p.parent_element());
        }
    }

    #[test]
    fn invalid_contract_is_refused() {
        gst::init().unwrap();
        let contract = OutputContract {
            width: 641,
            ..OutputContract::default()
        };
        assert!(matches!(
            Output::build(OutputConfig::new(contract, OutputSink::Fake)),
            Err(PipelineError::Caps(_))
        ));
    }
}
