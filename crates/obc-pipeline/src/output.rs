//! The Output stage: one long-lived pipeline per sink, paced by its own
//! clock (`docs/adr/0009-ingest-process-output.md`).
//!
//! ```text
//! appsrc        <canonical caps>,framerate=<contract>  is-live format=time
//!   [ ! Process stage ]                                 crate::process
//!   [ ! cudaconvert ! capsfilter (CUDAMemory) I420 ]    contract format != NV12
//!   ! cudadownload
//!   ! capsfilter  <contract caps>, canonical label      system memory
//!   ! capssetter  colorimetry=<sink label>              relabel only
//!   ! capsfilter  <contract caps>
//!   ! v4l2sink device=<loopback> sync=false             (or fakesink)
//! ```
//!
//! The caps are fixed when the pipeline is built and never change, whatever
//! feeds the slot. A dedicated clock thread pushes one buffer per tick of the
//! contract frame rate ([`crate::clock::Pacer`]): the newest canonical frame
//! (fresh), the last one again (repeat), or a black frame (placeholder)
//! prepared once at start in CUDA memory. A pushed frame is a shallow copy
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
//! **CUDA context.** The output pipeline's CUDA elements create the
//! `GstCudaContext` when the pipeline opens; [`Output::cuda_context`] hands
//! it to every ingest pipeline so canonical frames stay in one context.
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gst::prelude::*;
use obc_core::{ColourRange, OutputContract, OutputFormat};
use serde::Serialize;

use crate::clock::{Latest, Pacer, PacerConfig, PacerCounts, PhaseLockConfig, Step, TickKind};
use crate::frame::{Canonical, Frame, FrameWriter};
use crate::ingest::CUDA_CONTEXT_TYPE;
use crate::pipeline::{
    MediaPipeline, PipelineError, add_linked, capsfilter, cuda_caps, make, names,
};
use crate::process::{GpuStage, process_elements};
use crate::slot::Slot;

/// Where the output pipeline delivers its frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSink {
    /// A v4l2loopback device node.
    Loopback(PathBuf),
    /// `fakesink`: frames are downloaded and discarded (tests, measurements
    /// without a loopback).
    Fake,
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
    /// new source ([`Output::hold_for`]).
    pub hold_timeout: Duration,
    /// The output clock's phase lock ([`crate::clock`]).
    pub phase_lock: PhaseLockConfig,
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
struct StopSignal {
    stopped: Mutex<bool>,
    cv: Condvar,
}

impl StopSignal {
    fn set(&self) {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }

    fn is_set(&self) -> bool {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sleeps until `deadline` or until stopped; returns whether stopped.
    fn wait_until(&self, deadline: Instant) -> bool {
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
fn run_recording_panic(body: impl FnOnce(), failure: &Mutex<Option<String>>) {
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
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("contract", &self.cfg.contract)
            .field("sink", &self.cfg.sink)
            .field("process", &self.cfg.process)
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
    /// Builds (but does not start) the output pipeline. Its elements can be
    /// inspected and probed before [`Self::start`]. A contract the sink
    /// cannot carry is refused here ([`check_sink_range`]).
    pub fn build(cfg: OutputConfig) -> Result<Self, PipelineError> {
        use names::*;
        let contract = cfg.contract;
        contract
            .validate()
            .map_err(|e| PipelineError::Caps(e.to_string()))?;
        check_sink_range(&contract, &cfg.sink)?;
        let canonical = Canonical::of(&contract);
        let pipeline = gst::Pipeline::with_name("obc-output");
        let queue = cfg.pacer().max_burst;

        let src_caps = parse_caps(&format!(
            "{},framerate={}/{}",
            canonical.caps_string(),
            contract.fps_num,
            contract.fps_den
        ))?;
        let appsrc = frames_src(FRAMES_SRC, &src_caps, queue);

        let mut chain: Vec<gst::Element> = vec![appsrc.clone().upcast()];
        chain.extend(process_elements(cfg.process, canonical.colorimetry)?);
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
        };
        add_linked(&pipeline, &chain)?;

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
        })
    }

    /// Sets the per-tick observer. Takes effect at [`Self::start`].
    pub fn set_observer(&mut self, observer: Option<EmitObserver>) {
        self.observer = observer;
    }

    /// Starts the output: checks the loopback, takes the CUDA context the
    /// pipeline's elements create, prepares the placeholder frame, goes to
    /// PLAYING and starts the clock. A second call does nothing. On error
    /// the pipeline is back in NULL.
    pub fn start(&mut self) -> Result<(), PipelineError> {
        if self.clock.is_some() {
            return Ok(());
        }
        let res = self.start_inner();
        if res.is_err() {
            let _ = self.pipeline.pipeline().set_state(gst::State::Null);
            self.cuda = None;
        }
        res
    }

    fn start_inner(&mut self) -> Result<(), PipelineError> {
        self.pipeline.prepare()?;
        let cuda = self.acquire_cuda_context()?;
        let placeholder = make_placeholder(&self.canonical, &cuda)?;
        self.cuda = Some(cuda);
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
        p.context(CUDA_CONTEXT_TYPE).ok_or_else(|| {
            PipelineError::Cuda("the output pipeline opened no CUDA context".to_string())
        })
    }

    /// The CUDA context ingest pipelines must use (after [`Self::start`]).
    pub fn cuda_context(&self) -> Option<&gst::Context> {
        self.cuda.as_ref()
    }

    /// A writer for a new source; each call allocates a new source id.
    pub fn frame_writer(&self) -> FrameWriter {
        let id = self.next_source.fetch_add(1, Ordering::Relaxed);
        FrameWriter::new(Arc::clone(&self.slot), self.epoch, id, self.canonical)
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
        let mut res = Ok(());
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
        self.cuda = None;
        res?;
        state.map_err(|e| PipelineError::State(e.to_string()))?;
        if cur != gst::State::Null {
            return Err(PipelineError::State(format!("ended in {cur:?}, not Null")));
        }
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.stop();
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
    placeholder: gst::Buffer,
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
                _ => (&self.placeholder, None),
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
            if kind == TickKind::Fresh
                && let Some(o) = &origin_info
                && o.source_id != last_fresh_source
            {
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
                    pushed,
                    queue_level,
                });
            }
        }
    }
}

/// NV12 layout as `GstVideoInfo` computes it for even sizes: rows padded to
/// 4 bytes, the interleaved chroma plane after the luma plane.
fn nv12_layout(width: u32, height: u32) -> (usize, usize) {
    let stride = (width as usize + 3) & !3;
    let luma = stride * height as usize;
    (luma, luma + stride * (height as usize / 2))
}

/// One black canonical frame in CUDA memory of the context `cuda`, made by
/// uploading a system-memory frame through a one-shot pipeline:
/// `appsrc ! cudaupload ! capsfilter (canonical) ! appsink`.
fn make_placeholder(
    canonical: &Canonical,
    cuda: &gst::Context,
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

    let (luma, size) = nv12_layout(canonical.width, canonical.height);
    let (y, uv) = canonical.black();
    let mut data = vec![uv; size];
    data[..luma].fill(y);
    let mut buffer = gst::Buffer::from_mut_slice(data);
    buffer
        .get_mut()
        .expect("a new buffer is writable")
        .set_pts(gst::ClockTime::ZERO);
    let pushed = src.push_buffer(buffer).is_ok() && src.end_of_stream().is_ok();
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // Output::build applies it before building anything.
        if !have(&["cudadownload", "capssetter", "v4l2sink", "fakesink"]) {
            return;
        }
        assert!(matches!(
            Output::build(OutputConfig::new(full, loopback)),
            Err(PipelineError::LoopbackRefused { .. })
        ));
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
    fn the_appsrc_queue_is_the_configured_bound_and_the_burst_cap() {
        gst::init().unwrap();
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
        if !have(&["fakesink"]) {
            return;
        }
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

    fn have(factories: &[&str]) -> bool {
        gst::init().unwrap();
        for f in factories {
            if gst::ElementFactory::find(f).is_none() {
                eprintln!("SKIPPED: GStreamer element {f} is not installed");
                return false;
            }
        }
        true
    }

    /// The pipeline per contract format and process stage, without starting
    /// it (no GPU needed).
    #[test]
    fn output_elements_per_format_and_stage() {
        if !have(&[
            "cudaconvert",
            "cudadownload",
            "capssetter",
            "v4l2sink",
            "fakesink",
        ]) {
            return;
        }
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
