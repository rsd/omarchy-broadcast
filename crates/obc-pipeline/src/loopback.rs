//! The persistent loopback writer: the one pipeline that opens the
//! v4l2loopback device, for the whole life of the daemon
//! (`docs/adr/0011-daemon-lifecycle-and-dbus-api.md`, "A persistent
//! loopback writer").
//!
//! ```text
//! appsrc   <contract caps>, colorimetry=2:4:5:4   is-live format=time, system memory
//!   ! v4l2sink device=<loopback> sync=false        (or fakesink)
//! ```
//!
//! With `exclusive_caps=1`, v4l2loopback lists the device as a capture
//! device only while a writer streams into it, and while a consumer holds
//! it, it keeps the format of that stream and refuses every other one. The
//! writer therefore opens the device once, negotiates its caps once (the
//! caps [`crate::output::Output`] delivers to `v4l2sink`:
//! [`writer_caps_string`]) and never renegotiates, whatever feeds it:
//!
//! * **Placeholder.** A black frame of the contract (NV12 or I420, the
//!   layout `GstVideoInfo` gives the caps; limited range Y=16, U=V=128),
//!   built once on the CPU. A placeholder thread pushes it at the contract
//!   rate whenever no forwarded frame has been pushed for
//!   [`WriterConfig::placeholder_after`] (default two contract periods):
//!   while frames are forwarded it pushes nothing, and it resumes that long
//!   after the last one.
//! * **Forwarded frames.** [`WriterHandle::push`], called by the active
//!   output (an [`OutputSink::Writer`](crate::output::OutputSink::Writer)
//!   output ends in an `appsink` that forwards every buffer). The buffer is
//!   made writable for its metadata only: the memory is shared, never
//!   copied.
//!
//! **Timestamps.** Every buffer pushed, placeholder or forwarded, gets the
//! writer's own timestamp: PTS = the `CLOCK_MONOTONIC` time of the push
//! (read once when the writer is built, then advanced by `Instant`, which
//! is the same clock), strictly increasing (a push in the same nanosecond
//! as the previous one gets one nanosecond more), duration one contract
//! period, offset = the push's index. The producers' own timestamps never
//! reach the sink, so a hand-over between them is invisible downstream.
//! Pushes are serialised by one lock held across the stamp and the push, so
//! PTS order is push order. `v4l2sink` copies the PTS into the V4L2 buffer
//! timestamp, so a reader of the loopback sees `CLOCK_MONOTONIC` times, as a
//! UVC camera stamps them: a writer of a new daemon process (a restart, a
//! crash) continues forward instead of starting again at 0. The output's
//! own timeline (ADR 0010, `D(n)`) is restamped away here and unaffected.
//!
//! **Crash cover.** At each start on a loopback, once the second buffer
//! reaches `v4l2sink` (the first has then set the device's format and
//! allocated its buffers, on the same streaming thread), a helper thread
//! uploads a black timeout image and sets the driver's `timeout`
//! ([`crate::loopback_cover`]), so a consumer keeps receiving (black)
//! frames while no writer streams: after the daemon died, until its restart
//! writes again. The timeout stays above the writer's worst gap between
//! two frames ([`WriterConfig::placeholder_after`] plus a period), so it
//! never fires while the writer runs. The outcome is
//! [`LoopbackWriter::consumer_cover`]; a failure (an older driver without
//! the controls) never stops the writer. Nothing is reset at the stop.
//!
//! **Tap.** [`LoopbackWriter::set_tap`] installs a [`FrameTap`]: every
//! buffer `appsrc` accepted, placeholder and forwarded alike, is handed to
//! it after the restamp (the writer's PTS, duration and offset, exactly as
//! the sink sees them), under the push lock, so tap order is push order.
//! The optional PipeWire output (module `pipewire`, cargo feature
//! `pipewire`) is fed this way. A tap runs on the pushing thread with the push lock
//! held: it must never block (take a reference, wake someone, return).
//!
//! **Queue.** `appsrc` holds at most [`WriterConfig::queue_buffers`]
//! (default 3) and drops the oldest beyond that; it never blocks a producer.
//!
//! **No CUDA.** The writer builds no CUDA element and never loads the
//! `nvcodec` plugin: a daemon with only the writer running holds no CUDA
//! context (ADR-0006). A forwarded buffer (`cudadownload` output) is
//! system memory from a `GstVideoBufferPool` (observed with GStreamer
//! 1.28.8), not CUDA or pinned host memory: `v4l2sink` maps it like any
//! other, and releasing it makes no CUDA call, so the writer and its tap
//! may keep one while the daemon's GPU is parked.
//!
//! **Loopback check.** [`LoopbackWriter::start`] runs the same check as the
//! output ([`MediaPipeline::prepare`]) before going to PLAYING:
//! [`PipelineError::LoopbackFormatHeld`] when a consumer holds the device in
//! another format. A full-range contract is refused at build for a loopback
//! ([`check_sink_range`]).
//!
//! **Errors.** The pipeline's bus is read by a sync handler that records
//! every ERROR (reported by [`LoopbackWriter::health`]) and drops every
//! message, so a pipeline that lives as long as the daemon never queues
//! messages nobody reads.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gst::prelude::*;
use obc_core::OutputContract;
use serde::Serialize;

use crate::frame::Canonical;
use crate::loopback_cover::{ConsumerCover, CoverPlan, CoverReport, CoverSlot};
use crate::output::{
    OutputSink, StopSignal, check_sink_range, contract_caps_string, frames_src, run_recording_panic,
};
use crate::pipeline::{MediaPipeline, PipelineError, add_linked, make, names};

/// Where the writer delivers its frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterSink {
    /// A v4l2loopback device node.
    Loopback(PathBuf),
    /// `fakesink`: frames are discarded (tests; probe its sink pad,
    /// [`names::LOOPBACK`]).
    Fake,
}

/// Everything needed to build the writer.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    pub contract: OutputContract,
    pub sink: WriterSink,
    /// Buffers `appsrc` holds before dropping the oldest.
    pub queue_buffers: u32,
    /// How long without a forwarded frame before the placeholder thread
    /// pushes placeholders again.
    pub placeholder_after: Duration,
}

impl WriterConfig {
    /// The defaults: a queue of [`Self::DEFAULT_QUEUE_BUFFERS`], and
    /// placeholders after [`Self::PLACEHOLDER_AFTER_PERIODS`] contract
    /// periods without a forwarded frame.
    pub fn new(contract: OutputContract, sink: WriterSink) -> Self {
        WriterConfig {
            contract,
            sink,
            queue_buffers: Self::DEFAULT_QUEUE_BUFFERS,
            placeholder_after: Duration::from_nanos(
                Self::PLACEHOLDER_AFTER_PERIODS * contract.frame_duration_ns(),
            ),
        }
    }

    pub const DEFAULT_QUEUE_BUFFERS: u32 = 3;
    pub const PLACEHOLDER_AFTER_PERIODS: u64 = 2;
}

/// A snapshot of the writer's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct WriterCounts {
    /// Placeholder frames `appsrc` accepted.
    pub placeholder: u64,
    /// Forwarded frames `appsrc` accepted.
    pub forwarded: u64,
    /// Pushes refused: by `appsrc` (flushing, stopped), or because the
    /// writer was not running.
    pub push_failed: u64,
    /// Of the frames accepted, those `appsrc` dropped because its queue was
    /// full (they never reached the sink).
    pub queue_dropped: u64,
    /// When the last accepted push happened.
    #[serde(skip)]
    pub last_push: Option<Instant>,
}

/// Receives every buffer the writer's `appsrc` accepted, after the
/// restamp, in push order (see the module docs, "Tap"). Called on the
/// pushing thread (the placeholder thread or the output's streaming
/// thread) with the writer's push lock held: it must never block, and must
/// not call back into the writer.
pub type FrameTap = Arc<dyn Fn(&gst::Buffer) + Send + Sync>;

/// The installed tap, behind the push lock.
#[derive(Default)]
struct TapSlot(Option<FrameTap>);

impl std::fmt::Debug for TapSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Some(tap)"
        } else {
            "None"
        })
    }
}

/// What the push lock protects: the writer's timeline.
#[derive(Debug, Default)]
struct PushState {
    /// Accepting pushes: between start and stop.
    running: bool,
    last_pts_ns: Option<u64>,
    /// Accepted pushes since the start (the next buffer's offset).
    pushes: u64,
    last_push: Option<Instant>,
    /// When the last forwarded frame was pushed (accepted or not).
    last_forward: Option<Instant>,
    /// Sees every accepted buffer; behind the push lock, so a call never
    /// overlaps [`LoopbackWriter::set_tap`].
    tap: TapSlot,
}

/// State shared by the writer, its handles and its placeholder thread.
#[derive(Debug)]
struct Shared {
    appsrc: gst_app::AppSrc,
    contract: OutputContract,
    period_ns: u64,
    /// Origin of the writer's PTS: `epoch_mono_ns` (`CLOCK_MONOTONIC`, read
    /// at the same instant) at `epoch`.
    epoch: Instant,
    epoch_mono_ns: u64,
    state: Mutex<PushState>,
    placeholder: AtomicU64,
    forwarded: AtomicU64,
    push_failed: AtomicU64,
    /// The first ERROR the pipeline posted.
    error: Mutex<Option<String>>,
}

/// What a push carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Producer {
    Placeholder,
    Forwarded,
}

/// Whether the placeholder thread pushes at `now`: only when no forwarded
/// frame was pushed within `after`.
fn placeholder_due(last_forward: Option<Instant>, now: Instant, after: Duration) -> bool {
    placeholder_due_at(last_forward, after).is_none_or(|t| now >= t)
}

/// When placeholders become due again after the last forwarded frame
/// (`None`: none was forwarded, they are due).
fn placeholder_due_at(last_forward: Option<Instant>, after: Duration) -> Option<Instant> {
    last_forward.map(|t| t + after)
}

/// The outcome of a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pushed {
    Accepted,
    /// Refused by `appsrc`, or the writer is not running.
    Refused,
    /// A placeholder not pushed: a frame was forwarded recently. Due at
    /// that instant, unless another frame is forwarded before.
    NotDue(Instant),
}

/// The PTS of the next push at `elapsed_ns`: never before the previous one.
fn next_pts(last: Option<u64>, elapsed_ns: u64) -> u64 {
    last.map_or(elapsed_ns, |p| elapsed_ns.max(p + 1))
}

fn ns_since(epoch: Instant, t: Instant) -> u64 {
    u64::try_from(t.saturating_duration_since(epoch).as_nanos()).unwrap_or(u64::MAX)
}

/// `CLOCK_MONOTONIC` now, in nanoseconds.
pub(crate) fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid `struct timespec` that outlives the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(rc, 0, "clock_gettime(CLOCK_MONOTONIC) failed");
    u64::try_from(ts.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(ts.tv_nsec).unwrap_or(0))
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, PushState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stamps `buffer` on the writer's timeline and pushes it, under the
    /// push lock. A placeholder is pushed only if [`placeholder_due`]
    /// (`after`).
    fn push(&self, mut buffer: gst::Buffer, producer: Producer, after: Duration) -> Pushed {
        let mut st = self.lock();
        let now = Instant::now();
        if producer == Producer::Forwarded {
            st.last_forward = Some(now);
        } else if !placeholder_due(st.last_forward, now, after) {
            return Pushed::NotDue(placeholder_due_at(st.last_forward, after).unwrap_or(now));
        }
        if !st.running {
            // A placeholder racing the stop is not a failure.
            if producer == Producer::Forwarded {
                self.push_failed.fetch_add(1, Ordering::Relaxed);
            }
            return Pushed::Refused;
        }
        let pts = next_pts(
            st.last_pts_ns,
            self.epoch_mono_ns.saturating_add(ns_since(self.epoch, now)),
        );
        {
            // Writable for the metadata only: a shared buffer is copied
            // shallowly (its memory is referenced, not copied).
            let b = buffer.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(pts));
            b.set_dts(gst::ClockTime::NONE);
            b.set_duration(gst::ClockTime::from_nseconds(self.period_ns));
            b.set_offset(st.pushes);
            b.set_offset_end(st.pushes + 1);
            if st.pushes == 0 {
                b.set_flags(gst::BufferFlags::DISCONT);
            } else {
                b.unset_flags(gst::BufferFlags::DISCONT);
            }
        }
        // A reference for the tap (no copy), kept only when one is set.
        let tapped = st.tap.0.as_ref().map(|_| buffer.clone());
        let accepted = self.appsrc.push_buffer(buffer).is_ok();
        if accepted {
            if let (Some(tap), Some(b)) = (&st.tap.0, &tapped) {
                tap(b);
            }
            st.last_pts_ns = Some(pts);
            st.pushes += 1;
            st.last_push = Some(now);
            match producer {
                Producer::Placeholder => &self.placeholder,
                Producer::Forwarded => &self.forwarded,
            }
            .fetch_add(1, Ordering::Relaxed);
            Pushed::Accepted
        } else {
            self.push_failed.fetch_add(1, Ordering::Relaxed);
            Pushed::Refused
        }
    }

    fn counts(&self) -> WriterCounts {
        let last_push = self.lock().last_push;
        WriterCounts {
            placeholder: self.placeholder.load(Ordering::Relaxed),
            forwarded: self.forwarded.load(Ordering::Relaxed),
            push_failed: self.push_failed.load(Ordering::Relaxed),
            queue_dropped: self.appsrc.property::<u64>("dropped"),
            last_push,
        }
    }
}

/// Pushes forwarded frames into a [`LoopbackWriter`]. Cheap to clone, and
/// `Send + Sync`: the active output's streaming thread holds one. Two
/// handles are equal when they feed the same writer.
#[derive(Clone)]
pub struct WriterHandle {
    shared: Arc<Shared>,
    after: Duration,
}

impl std::fmt::Debug for WriterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterHandle")
            .field("contract", &self.shared.contract)
            .finish_non_exhaustive()
    }
}

impl PartialEq for WriterHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl Eq for WriterHandle {}

impl WriterHandle {
    /// Pushes a forwarded frame (system-memory caps of the writer's
    /// contract), restamped on the writer's timeline; its memory is not
    /// copied. While frames arrive, the placeholder thread pushes nothing.
    /// Returns whether `appsrc` accepted it (`false` while the writer is not
    /// running, counted in [`WriterCounts::push_failed`]). Never blocks
    /// beyond the push lock.
    pub fn push(&self, buffer: gst::Buffer) -> bool {
        self.shared.push(buffer, Producer::Forwarded, self.after) == Pushed::Accepted
    }

    /// The contract of the frames the writer carries.
    pub fn contract(&self) -> &OutputContract {
        &self.shared.contract
    }

    pub fn counts(&self) -> WriterCounts {
        self.shared.counts()
    }
}

#[derive(Debug)]
struct PlaceholderThread {
    handle: JoinHandle<()>,
    stop: Arc<StopSignal>,
    failure: Arc<Mutex<Option<String>>>,
}

/// The crash cover's trigger: a buffer probe on `v4l2sink`'s sink pad,
/// removed by itself once it has fired.
#[derive(Debug)]
struct CoverProbe {
    pad: gst::Pad,
    id: Option<gst::PadProbeId>,
    fired: Arc<AtomicBool>,
}

/// The loopback writer. See the module docs.
pub struct LoopbackWriter {
    pipeline: MediaPipeline,
    cfg: WriterConfig,
    shared: Arc<Shared>,
    placeholder: gst::Buffer,
    thread: Option<PlaceholderThread>,
    cover: Arc<CoverSlot>,
    cover_probe: Option<CoverProbe>,
}

impl std::fmt::Debug for LoopbackWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopbackWriter")
            .field("contract", &self.cfg.contract)
            .field("sink", &self.cfg.sink)
            .field("running", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

/// The writer's caps: the contract caps as the output delivers them to its
/// sink ([`contract_caps_string`], labelled
/// [`crate::output::SINK_COLORIMETRY`], system memory) with the canonical
/// frame's square pixels, which the output's frames carry from the ingest
/// on. Fixed caps: `v4l2sink` refuses caps that leave the pixel aspect
/// ratio open.
pub fn writer_caps_string(contract: &OutputContract) -> String {
    format!("{},pixel-aspect-ratio=1/1", contract_caps_string(contract))
}

/// One black frame of `contract` in system memory, laid out as
/// `GstVideoInfo` lays out the writer's caps (NV12 or I420): Y = black of
/// the contract's range (16 for limited), chroma 128, padding included.
pub(crate) fn black_contract_frame(
    contract: &OutputContract,
) -> Result<gst::Buffer, PipelineError> {
    let s = writer_caps_string(contract);
    let caps: gst::Caps = s.parse().map_err(|_| PipelineError::Caps(s.clone()))?;
    let info = gst_video::VideoInfo::from_caps(&caps).map_err(|_| PipelineError::Caps(s))?;
    let (y, chroma) = Canonical::of(contract).black();
    let mut data = vec![chroma; info.size()];
    let luma_start = info.offset()[0];
    let luma_len = usize::try_from(info.stride()[0]).unwrap_or(0) * info.height() as usize;
    data[luma_start..luma_start + luma_len].fill(y);
    Ok(gst::Buffer::from_mut_slice(data))
}

impl LoopbackWriter {
    /// [`Self::build_with`] the default [`WriterConfig`].
    pub fn build(contract: OutputContract, sink: WriterSink) -> Result<Self, PipelineError> {
        Self::build_with(WriterConfig::new(contract, sink))
    }

    /// Builds (but does not start) the writer. A contract the sink cannot
    /// carry (full range for a loopback) is refused here.
    pub fn build_with(cfg: WriterConfig) -> Result<Self, PipelineError> {
        let contract = cfg.contract;
        contract
            .validate()
            .map_err(|e| PipelineError::Caps(e.to_string()))?;
        if let WriterSink::Loopback(device) = &cfg.sink {
            check_sink_range(&contract, &OutputSink::Loopback(device.clone()))?;
        }
        let caps_str = writer_caps_string(&contract);
        let caps: gst::Caps = caps_str
            .parse()
            .map_err(|_| PipelineError::Caps(caps_str.clone()))?;
        let appsrc = frames_src(
            names::WRITER_SRC,
            &caps,
            u64::from(cfg.queue_buffers.max(1)),
        );
        let (sink, loopback_caps) = match &cfg.sink {
            WriterSink::Loopback(device) => {
                let sink = make("v4l2sink", names::LOOPBACK)?;
                sink.set_property("device", device.to_string_lossy().as_ref());
                (sink, Some(caps))
            }
            WriterSink::Fake => (make("fakesink", names::LOOPBACK)?, None),
        };
        sink.set_property("sync", false);
        sink.set_property("enable-last-sample", false);
        let pipeline = gst::Pipeline::with_name("obc-loopback-writer");
        add_linked(&pipeline, &[appsrc.clone().upcast(), sink])?;

        let shared = Arc::new(Shared {
            appsrc,
            contract,
            period_ns: contract.frame_duration_ns(),
            epoch: Instant::now(),
            epoch_mono_ns: monotonic_ns(),
            state: Mutex::default(),
            placeholder: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            push_failed: AtomicU64::new(0),
            error: Mutex::default(),
        });
        let recorder = Arc::downgrade(&shared);
        pipeline
            .bus()
            .expect("a pipeline always has a bus")
            .set_sync_handler(move |_, msg| {
                if let gst::MessageView::Error(e) = msg.view()
                    && let Some(shared) = recorder.upgrade()
                {
                    let mut first = shared.error.lock().unwrap_or_else(|e| e.into_inner());
                    first.get_or_insert_with(|| {
                        format!(
                            "{}: {} ({})",
                            msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                            e.error(),
                            e.debug().map(|d| d.to_string()).unwrap_or_default()
                        )
                    });
                }
                gst::BusSyncReply::Drop
            });
        Ok(LoopbackWriter {
            pipeline: MediaPipeline::from_parts(pipeline, loopback_caps),
            placeholder: black_contract_frame(&contract)?,
            cfg,
            shared,
            thread: None,
            cover: Arc::default(),
            cover_probe: None,
        })
    }

    /// Starts the writer: checks the loopback (before anything streams),
    /// goes to PLAYING and starts the placeholder thread. A second call
    /// does nothing. On error the pipeline is back in NULL. A start after
    /// [`Self::stop`] opens the device again and negotiates again.
    pub fn start(&mut self) -> Result<(), PipelineError> {
        if self.thread.is_some() {
            return Ok(());
        }
        let res = self.start_inner();
        if res.is_err() {
            self.shared.lock().running = false;
            let _ = self.pipeline.pipeline().set_state(gst::State::Null);
            self.end_cover();
        }
        res
    }

    fn start_inner(&mut self) -> Result<(), PipelineError> {
        self.pipeline.prepare()?;
        self.arm_cover();
        self.pipeline
            .pipeline()
            .set_state(gst::State::Playing)
            .map_err(|e| PipelineError::State(e.to_string()))?;
        {
            let mut st = self.shared.lock();
            st.running = true;
            st.pushes = 0;
            st.last_forward = None;
        }
        let stop = Arc::new(StopSignal::default());
        let failure: Arc<Mutex<Option<String>>> = Arc::default();
        let (shared, placeholder, after) = (
            Arc::clone(&self.shared),
            self.placeholder.clone(),
            self.cfg.placeholder_after,
        );
        let (thread_stop, thread_failure) = (Arc::clone(&stop), Arc::clone(&failure));
        let handle = std::thread::Builder::new()
            .name("obc-placeholder".into())
            .spawn(move || {
                run_recording_panic(
                    move || placeholder_loop(&shared, &placeholder, after, &thread_stop),
                    &thread_failure,
                )
            })
            .map_err(|e| PipelineError::State(format!("placeholder thread: {e}")))?;
        self.thread = Some(PlaceholderThread {
            handle,
            stop,
            failure,
        });
        Ok(())
    }

    /// On a loopback, installs the crash cover's trigger (see the module
    /// docs): a probe on `v4l2sink`'s sink pad that, at the second buffer,
    /// hands [`crate::loopback_cover::install`] to a helper thread and
    /// removes itself. Before PLAYING, so it sees the first buffer.
    fn arm_cover(&mut self) {
        let WriterSink::Loopback(device) = &self.cfg.sink else {
            return;
        };
        let generation = self.cover.begin();
        let plan = CoverPlan {
            device: device.clone(),
            format: self.cfg.contract.format,
            black: Canonical::of(&self.cfg.contract).black(),
            writer_worst_gap: self.cfg.placeholder_after
                + Duration::from_nanos(self.shared.period_ns),
        };
        let Some(pad) = self
            .pipeline
            .element(names::LOOPBACK)
            .and_then(|e| e.static_pad("sink"))
        else {
            self.cover.finish(
                generation,
                Err("the writer has no sink pad to watch".into()),
            );
            return;
        };
        let (slot, fired) = (Arc::clone(&self.cover), Arc::new(AtomicBool::new(false)));
        let seen = AtomicU64::new(0);
        let flag = Arc::clone(&fired);
        let plan = Mutex::new(Some(plan));
        let id = pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if seen.fetch_add(1, Ordering::Relaxed) == 0 {
                return gst::PadProbeReturn::Ok;
            }
            flag.store(true, Ordering::Release);
            if let Some(plan) = plan.lock().unwrap_or_else(|e| e.into_inner()).take() {
                slot.spawn_helper(generation, plan);
            }
            gst::PadProbeReturn::Remove
        });
        self.cover_probe = Some(CoverProbe { pad, id, fired });
    }

    /// Ends the crash cover's part of a start, with the pipeline in NULL (no
    /// buffer flows): the probe removed if it has not fired, the helper
    /// thread joined, a cover still pending reported off.
    fn end_cover(&mut self) {
        if let Some(p) = self.cover_probe.take()
            && !p.fired.load(Ordering::Acquire)
            && let Some(id) = p.id
        {
            p.pad.remove_probe(id);
        }
        self.cover.join_helper();
        self.cover.cancel();
    }

    /// The crash cover of the current (or last) start: see
    /// [`crate::loopback_cover`]. [`ConsumerCover::Off`] for a `fakesink`.
    pub fn consumer_cover(&self) -> ConsumerCover {
        self.cover.get()
    }

    /// [`Self::consumer_cover`] once it is no longer pending, or after
    /// `limit` (then still [`ConsumerCover::Pending`]).
    pub fn wait_consumer_cover(&self, limit: Duration) -> ConsumerCover {
        self.cover.wait(limit)
    }

    /// Sets (`Some`) or removes (`None`) the [`CoverReport`] called with
    /// each start's cover outcome once it is known, on the cover's helper
    /// thread. Setting it calls nothing: set it before [`Self::start`].
    pub fn on_consumer_cover(&self, report: Option<CoverReport>) {
        self.cover.set_report(report);
    }

    /// Installs (`Some`) or removes (`None`) the [`FrameTap`] that sees
    /// every accepted buffer. Takes the push lock: once it returns, no call
    /// to the previous tap is in flight and none follows. Kept across
    /// [`Self::stop`] and [`Self::start`].
    pub fn set_tap(&self, tap: Option<FrameTap>) {
        self.shared.lock().tap = TapSlot(tap);
    }

    /// A handle for the active output to forward its frames with.
    pub fn handle(&self) -> WriterHandle {
        WriterHandle {
            shared: Arc::clone(&self.shared),
            after: self.cfg.placeholder_after,
        }
    }

    pub fn counts(&self) -> WriterCounts {
        self.shared.counts()
    }

    pub fn contract(&self) -> &OutputContract {
        &self.cfg.contract
    }

    pub fn config(&self) -> &WriterConfig {
        &self.cfg
    }

    pub fn pipeline(&self) -> &MediaPipeline {
        &self.pipeline
    }

    /// Started, not stopped, and the placeholder thread alive.
    pub fn is_running(&self) -> bool {
        self.thread.is_some() && self.thread_failure().is_none()
    }

    fn thread_failure(&self) -> Option<String> {
        let t = self.thread.as_ref()?;
        if let Some(msg) = t.failure.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Some(format!("the placeholder thread panicked: {msg}"));
        }
        (t.handle.is_finished() && !t.stop.is_set())
            .then(|| "the placeholder thread ended unexpectedly".to_string())
    }

    /// An error once the pipeline has posted an ERROR on its bus (the first
    /// one, kept until the writer is dropped) or the placeholder thread has
    /// ended without being stopped. `Ok` otherwise.
    pub fn health(&self) -> Result<(), PipelineError> {
        let error = self
            .shared
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match error.or_else(|| self.thread_failure()) {
            Some(msg) => Err(PipelineError::State(msg)),
            None => Ok(()),
        }
    }

    /// Stops the placeholder thread (joined before this returns), refuses
    /// further pushes and sets the pipeline to NULL (the device is closed).
    /// Idempotent.
    pub fn stop(&mut self) -> Result<(), PipelineError> {
        let mut res = Ok(());
        // No push can be in flight past this point.
        self.shared.lock().running = false;
        if let Some(t) = self.thread.take() {
            t.stop.set();
            let joined = t.handle.join();
            if let Some(msg) = t.failure.lock().unwrap_or_else(|e| e.into_inner()).take() {
                res = Err(PipelineError::State(format!(
                    "the placeholder thread panicked: {msg}"
                )));
            } else if joined.is_err() {
                res = Err(PipelineError::State(
                    "the placeholder thread panicked".into(),
                ));
            }
        }
        let p = self.pipeline.pipeline();
        let state = p.set_state(gst::State::Null);
        let (_, cur, _) = p.state(gst::ClockTime::from_seconds(5));
        self.end_cover();
        res?;
        state.map_err(|e| PipelineError::State(e.to_string()))?;
        if cur != gst::State::Null {
            return Err(PipelineError::State(format!("ended in {cur:?}, not Null")));
        }
        Ok(())
    }
}

impl Drop for LoopbackWriter {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The placeholder thread: one placeholder per contract period, on its own
/// grid (resynchronised, never bursting, after a stall longer than a
/// period). While frames are forwarded it sleeps until the instant
/// placeholders would be due, and starts a new grid there: they resume
/// [`WriterConfig::placeholder_after`] after the last forwarded frame, not
/// on the next tick of the old grid.
fn placeholder_loop(
    shared: &Shared,
    placeholder: &gst::Buffer,
    after: Duration,
    stop: &StopSignal,
) {
    let period = Duration::from_nanos(shared.period_ns.max(1));
    let mut next = Instant::now();
    loop {
        if stop.wait_until(next) {
            return;
        }
        // A shallow copy: new metadata, the one black frame's memory.
        if let Pushed::NotDue(due) = shared.push(placeholder.copy(), Producer::Placeholder, after) {
            next = due;
            continue;
        }
        next += period;
        let now = Instant::now();
        if next + period < now {
            next = now;
        }
    }
}

/// The writer on the CPU: a `fakesink` in place of `v4l2sink`, the real
/// writer code otherwise. Frame counts and order are asserted; the times
/// compared are lower bounds, except the hand-over bounds of the core
/// invariant (placeholders resume within about three periods), which have
/// a margin of several periods' worth of scheduling delay.
#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use obc_core::{AspectPolicy, ColourRange, OutputFormat};

    use super::*;
    use crate::frame::FrameMemory;
    use crate::ingest::Ingest;
    use crate::output::{Output, OutputConfig};
    use crate::session::Session;
    use crate::test_support::require_elements;

    const ELEMENTS: &[&str] = &[
        "appsrc",
        "fakesink",
        "videotestsrc",
        "capsfilter",
        "videocrop",
        "videoconvertscale",
        "videoconvert",
        "appsink",
        "capssetter",
    ];

    /// One buffer at the writer's sink.
    #[derive(Debug, Clone)]
    struct AtSink {
        pts: u64,
        duration: Option<u64>,
        size: usize,
        /// The address of its first memory's data.
        data: usize,
        /// Every Y byte 16 and every chroma byte 128 (limited black).
        black: bool,
        info: Option<gst_video::VideoInfo>,
    }

    #[derive(Debug, Default)]
    struct SinkLog {
        buffers: Vec<AtSink>,
        caps_events: Vec<gst::Caps>,
    }

    fn is_black(info: &gst_video::VideoInfo, data: &[u8]) -> bool {
        let luma_end = info.offset()[0] + info.stride()[0] as usize * info.height() as usize;
        data.len() == info.size()
            && data[..luma_end].iter().all(|&b| b == 16)
            && data[luma_end..].iter().all(|&b| b == 128)
    }

    /// Records every buffer and CAPS event reaching the writer's sink.
    fn watch_sink(w: &LoopbackWriter) -> Arc<Mutex<SinkLog>> {
        let log: Arc<Mutex<SinkLog>> = Arc::default();
        let pad = w
            .pipeline()
            .element(names::LOOPBACK)
            .and_then(|e| e.static_pad("sink"))
            .unwrap();
        let l = Arc::clone(&log);
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |pad, info| {
                match &info.data {
                    Some(gst::PadProbeData::Event(e)) => {
                        if let gst::EventView::Caps(c) = e.view() {
                            l.lock().unwrap().caps_events.push(c.caps_owned());
                        }
                    }
                    Some(gst::PadProbeData::Buffer(b)) => {
                        let info = pad
                            .current_caps()
                            .and_then(|c| gst_video::VideoInfo::from_caps(&c).ok());
                        let map = b.map_readable().unwrap();
                        let black = info.as_ref().is_some_and(|i| is_black(i, &map));
                        let entry = AtSink {
                            pts: b.pts().map(|p| p.nseconds()).unwrap_or(u64::MAX),
                            duration: b.duration().map(|d| d.nseconds()),
                            size: map.size(),
                            data: map.as_ptr() as usize,
                            black,
                            info,
                        };
                        drop(map);
                        l.lock().unwrap().buffers.push(entry);
                    }
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
        log
    }

    fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + limit;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn small(format: OutputFormat) -> OutputContract {
        OutputContract {
            format,
            .."320x240@30".parse().unwrap()
        }
    }

    #[test]
    fn placeholder_timing_rules() {
        let t0 = Instant::now();
        let after = Duration::from_millis(66);
        assert!(placeholder_due(None, t0, after));
        assert!(!placeholder_due(
            Some(t0),
            t0 + Duration::from_millis(33),
            after
        ));
        assert!(placeholder_due(Some(t0), t0 + after, after));
        assert_eq!(next_pts(None, 5), 5);
        assert_eq!(next_pts(Some(10), 50), 50);
        assert_eq!(next_pts(Some(10), 10), 11);
        assert_eq!(next_pts(Some(10), 3), 11);
        let cfg = WriterConfig::new("1920x1080@30".parse().unwrap(), WriterSink::Fake);
        assert_eq!(cfg.placeholder_after, Duration::from_nanos(2 * 33_333_333));
        assert_eq!(cfg.queue_buffers, 3);
    }

    /// The PTS base is `CLOCK_MONOTONIC`: a second writer (a new daemon
    /// process, in effect) starts after the first one's last PTS, not at 0.
    #[test]
    fn a_new_writer_continues_the_timeline_of_the_previous_one() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut last = 0;
        for round in 0..2 {
            let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
            let log = watch_sink(&w);
            w.start().unwrap();
            wait_until(Duration::from_secs(10), "3 placeholders", || {
                log.lock().unwrap().buffers.len() >= 3
            });
            w.stop().unwrap();
            let log = log.lock().unwrap();
            let first = log.buffers[0].pts;
            assert!(first > last, "round {round}: {first} after {last}");
            last = log.buffers.last().unwrap().pts;
        }
        let a = monotonic_ns();
        std::thread::sleep(Duration::from_millis(2));
        assert!(monotonic_ns() >= a + 2_000_000);
    }

    /// The black frame is a whole frame of the contract format, laid out as
    /// the caps say: Y=16 everywhere, chroma 128 (NV12: one interleaved
    /// plane; I420: two planes), padding rows included.
    #[test]
    fn the_black_frame_fills_the_contract_layout() {
        gst::init().unwrap();
        for (format, contract) in [
            (OutputFormat::Nv12, "1920x1080@30"),
            (OutputFormat::I420, "1920x1080@30"),
            (OutputFormat::Nv12, "642x480@30"),
            (OutputFormat::I420, "642x480@30"),
        ] {
            let c = OutputContract {
                format,
                ..contract.parse().unwrap()
            };
            let caps: gst::Caps = writer_caps_string(&c).parse().unwrap();
            let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
            let b = black_contract_frame(&c).unwrap();
            let map = b.map_readable().unwrap();
            assert_eq!(map.size(), info.size(), "{c}");
            assert!(is_black(&info, &map), "{c}");
            assert_eq!(
                info.n_planes(),
                if format == OutputFormat::Nv12 { 2 } else { 3 }
            );
        }
        // 1920x1080 NV12: 1.5 bytes per pixel, no padding.
        let b = black_contract_frame(&OutputContract::default()).unwrap();
        assert_eq!(b.size(), 1920 * 1080 * 3 / 2);
    }

    /// The writer's pipeline: an `appsrc` with the contract caps exactly as
    /// the output delivers them to `v4l2sink`, bounded and leaky, into the
    /// sink; no CUDA element. A full-range contract is refused for a
    /// loopback, accepted for a `fakesink`.
    #[test]
    fn the_writer_pipeline_has_the_contract_caps_and_no_cuda_element() {
        require_elements(&["appsrc", "fakesink", "v4l2sink"]);
        let contract = OutputContract::default();
        let w =
            LoopbackWriter::build(contract, WriterSink::Loopback("/dev/null-sink".into())).unwrap();
        let p = w.pipeline();
        let src = p.element(names::WRITER_SRC).unwrap();
        let caps = src.property::<gst::Caps>("caps");
        let want: gst::Caps = writer_caps_string(&contract).parse().unwrap();
        assert_eq!(caps, want);
        assert!(src.property::<bool>("is-live"));
        assert_eq!(src.property::<u64>("max-buffers"), 3);
        assert!(!src.property::<bool>("block"));
        assert_eq!(
            src.property_value("leaky-type")
                .get::<gst_app::AppLeakyType>()
                .unwrap(),
            gst_app::AppLeakyType::Downstream
        );
        let sink = p.element(names::LOOPBACK).unwrap();
        assert_eq!(sink.factory().unwrap().name(), "v4l2sink");
        assert!(!sink.property::<bool>("sync"));
        assert!(!sink.property::<bool>("enable-last-sample"));
        for e in p.pipeline().iterate_elements().into_iter().flatten() {
            // appsrc is made by its builder, without a factory.
            if let Some(factory) = e.factory() {
                let plugin = factory.plugin_name().map(|n| n.to_string());
                assert_ne!(plugin.as_deref(), Some("nvcodec"), "{}", e.name());
                assert!(!factory.name().starts_with("cuda"), "{}", e.name());
            }
            assert!(!e.type_().name().starts_with("GstCuda"), "{}", e.name());
        }
        assert!(!w.is_running());
        w.health().unwrap();

        let full = OutputContract {
            range: ColourRange::Full,
            ..contract
        };
        assert!(matches!(
            LoopbackWriter::build(full, WriterSink::Loopback("/dev/null-sink".into())),
            Err(PipelineError::LoopbackRefused { .. })
        ));
        LoopbackWriter::build(full, WriterSink::Fake).unwrap();
    }

    /// (a) Placeholder only: the writer alone sends black frames of the
    /// contract (caps, size, content) at no more than the contract rate:
    /// the k-th has a PTS of at least k periods. One caps negotiation.
    #[test]
    fn alone_the_writer_sends_black_frames_at_the_contract_rate() {
        require_elements(ELEMENTS);
        for format in [OutputFormat::Nv12, OutputFormat::I420] {
            let contract = small(format);
            let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
            let log = watch_sink(&w);
            let mono_start = monotonic_ns();
            w.start().unwrap();
            assert!(w.is_running());
            wait_until(Duration::from_secs(10), "30 placeholders", || {
                log.lock().unwrap().buffers.len() >= 30
            });
            let counts = w.counts();
            // No crash cover on a fakesink.
            assert_eq!(w.consumer_cover(), ConsumerCover::Off);
            w.stop().unwrap();
            let mono_stop = monotonic_ns();
            w.health().unwrap();
            let log = log.lock().unwrap();
            let want: gst::Caps = writer_caps_string(&contract).parse().unwrap();
            assert_eq!(log.caps_events, vec![want], "{format}");
            let period = contract.frame_duration_ns();
            let first = log.buffers[0].data;
            for (k, b) in log.buffers.iter().enumerate() {
                let info = b.info.as_ref().unwrap();
                assert_eq!(
                    (info.width(), info.height(), info.size()),
                    (320, 240, b.size),
                    "{format} #{k}"
                );
                assert!(b.black, "{format} #{k} is not black");
                assert_eq!(b.duration, Some(period));
                assert_eq!(b.data, first, "{format} #{k}: the one black frame");
                // Never faster than the contract rate.
                let since_first = b.pts - log.buffers[0].pts;
                assert!(
                    since_first >= k as u64 * period,
                    "{format} #{k}: pts {}",
                    b.pts
                );
                // CLOCK_MONOTONIC at the push.
                assert!(
                    (mono_start..=mono_stop).contains(&b.pts),
                    "{format} #{k}: pts {} outside CLOCK_MONOTONIC {mono_start}..={mono_stop}",
                    b.pts
                );
            }
            assert!(log.buffers.windows(2).all(|w| w[0].pts < w[1].pts));
            assert!(
                counts.placeholder >= 30 && counts.forwarded == 0,
                "{counts:?}"
            );
            assert_eq!(counts.push_failed, 0);
            assert!(counts.last_push.is_some());
        }
    }

    /// A forwarded buffer reaches the sink with the writer's timestamp and
    /// the same memory (not copied), even while the producer still holds a
    /// reference to it; the producer's buffer keeps its own timestamp. A
    /// push to a writer that is not running is refused and counted.
    #[test]
    fn a_forwarded_buffer_is_restamped_without_a_copy() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        // Placeholders only after 10 s: every buffer here is forwarded.
        let mut cfg = WriterConfig::new(contract, WriterSink::Fake);
        cfg.placeholder_after = Duration::from_secs(10);
        let mut w = LoopbackWriter::build_with(cfg).unwrap();
        let h = w.handle();
        assert_eq!(h, w.handle());
        assert_eq!(h.contract(), &contract);
        let frame = || {
            let mut b = black_contract_frame(&contract).unwrap();
            b.get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_seconds(1000));
            b
        };
        assert!(!h.push(frame()), "a writer not started refuses");
        assert_eq!(h.counts().push_failed, 1);

        let log = watch_sink(&w);
        let mono_start = monotonic_ns();
        w.start().unwrap();
        // The first placeholder goes out at start; the forwarded frames
        // follow it.
        wait_until(Duration::from_secs(10), "the first placeholder", || {
            !log.lock().unwrap().buffers.is_empty()
        });
        let kept: Vec<gst::Buffer> = (0..5).map(|_| frame()).collect();
        let addresses: Vec<usize> = kept
            .iter()
            .map(|b| b.map_readable().unwrap().as_ptr() as usize)
            .collect();
        for b in &kept {
            assert!(h.push(b.clone()));
            std::thread::sleep(Duration::from_millis(5));
        }
        wait_until(Duration::from_secs(10), "the forwarded frames", || {
            let l = log.lock().unwrap();
            l.buffers
                .iter()
                .filter(|b| b.data != l.buffers[0].data)
                .count()
                >= 5
        });
        let counts = w.counts();
        w.stop().unwrap();
        let mono_stop = monotonic_ns();
        assert!(!h.push(frame()), "a stopped writer refuses");
        let log = log.lock().unwrap();
        let placeholder = log.buffers[0].data;
        let forwarded: Vec<&AtSink> = log
            .buffers
            .iter()
            .filter(|b| b.data != placeholder)
            .collect();
        assert_eq!(
            forwarded.iter().map(|b| b.data).collect::<Vec<_>>(),
            addresses,
            "forwarded memory was copied"
        );
        assert!(log.buffers.windows(2).all(|w| w[0].pts < w[1].pts));
        // The writer's time (CLOCK_MONOTONIC at the push), not the
        // producer's 1000 s.
        assert!(
            forwarded
                .iter()
                .all(|b| (mono_start..=mono_stop).contains(&b.pts)),
            "{:?} outside CLOCK_MONOTONIC {mono_start}..={mono_stop}",
            forwarded.iter().map(|b| b.pts).collect::<Vec<_>>()
        );
        for b in &kept {
            assert_eq!(b.pts(), Some(gst::ClockTime::from_seconds(1000)));
        }
        assert_eq!(counts.forwarded, 5, "{counts:?}");
        assert!(counts.placeholder >= 1, "{counts:?}");
        assert_eq!(w.counts().push_failed, 2);
    }

    /// One buffer the tap saw.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Tapped {
        pts: u64,
        duration: Option<u64>,
        offset: u64,
        data: usize,
    }

    /// The tap sees every accepted buffer after the restamp, in push order:
    /// placeholders alone first, then a forwarded buffer sharing the pushed
    /// memory, with the PTS the sink sees, strictly increasing. Removed, it
    /// is not called any more.
    #[test]
    fn the_tap_sees_every_accepted_buffer_as_the_sink_does() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let period = contract.frame_duration_ns();
        // Placeholders pause for 10 s after a forwarded frame: none follows
        // the forwarded ones below.
        let mut cfg = WriterConfig::new(contract, WriterSink::Fake);
        cfg.placeholder_after = Duration::from_secs(10);
        let mut w = LoopbackWriter::build_with(cfg).unwrap();
        let seen: Arc<Mutex<Vec<Tapped>>> = Arc::default();
        let s = Arc::clone(&seen);
        w.set_tap(Some(Arc::new(move |b: &gst::Buffer| {
            let data = b.map_readable().unwrap().as_ptr() as usize;
            s.lock().unwrap().push(Tapped {
                pts: b.pts().map(|p| p.nseconds()).unwrap_or(u64::MAX),
                duration: b.duration().map(|d| d.nseconds()),
                offset: b.offset(),
                data,
            });
        })));
        let h = w.handle();
        let frame = || black_contract_frame(&contract).unwrap();
        assert!(!h.push(frame()), "a writer not started refuses");
        assert!(
            seen.lock().unwrap().is_empty(),
            "a refused push is not tapped"
        );

        let log = watch_sink(&w);
        w.start().unwrap();
        wait_until(Duration::from_secs(10), "placeholders at the tap", || {
            seen.lock().unwrap().len() >= 3
        });
        let placeholder = seen.lock().unwrap()[0].data;
        let pushed = frame();
        let address = pushed.map_readable().unwrap().as_ptr() as usize;
        assert!(h.push(pushed.clone()));
        wait_until(
            Duration::from_secs(10),
            "the forwarded frame at the sink",
            || {
                log.lock()
                    .unwrap()
                    .buffers
                    .iter()
                    .any(|b| b.data == address)
            },
        );

        w.set_tap(None);
        let tapped = seen.lock().unwrap().clone();
        assert!(h.push(frame()));
        let counts = w.counts();
        w.stop().unwrap();
        assert_eq!(
            seen.lock().unwrap().len(),
            tapped.len(),
            "a removed tap was called"
        );

        let (last, placeholders) = tapped.split_last().unwrap();
        assert!(placeholders.len() >= 3);
        assert!(placeholders.iter().all(|t| t.data == placeholder));
        assert_eq!(last.data, address, "the forwarded memory was copied");
        assert!(tapped.windows(2).all(|w| w[0].pts < w[1].pts), "PTS order");
        for (k, t) in tapped.iter().enumerate() {
            assert_eq!(t.offset, k as u64, "#{k}: the push index");
            assert_eq!(t.duration, Some(period), "#{k}");
        }
        // The sink saw what the tap saw (the last push came after the tap
        // was removed), unless the leaky queue dropped some.
        let at_sink: Vec<u64> = log.lock().unwrap().buffers.iter().map(|b| b.pts).collect();
        let tap_pts: Vec<u64> = tapped.iter().map(|t| t.pts).collect();
        if counts.queue_dropped == 0 {
            assert_eq!(&at_sink[..tap_pts.len()], &tap_pts[..]);
        } else {
            let before = at_sink.iter().filter(|&&p| p <= last.pts);
            assert!(before.into_iter().all(|p| tap_pts.contains(p)));
        }
        // The producer's buffer keeps its own (absent) timestamp.
        assert_eq!(pushed.pts(), None);
    }

    /// An output ending in [`OutputSink::Writer`] forwards to the writer:
    /// its sink is an `appsink` (named like any output sink), and an output
    /// whose contract differs from the writer's is refused.
    #[test]
    fn a_writer_output_ends_in_an_appsink_with_the_writer_contract() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let o = Output::build_in(
            OutputConfig::new(contract, OutputSink::Writer(w.handle())),
            FrameMemory::System,
        )
        .unwrap();
        let sink = o.pipeline().element(names::LOOPBACK).unwrap();
        assert!(sink.is::<gst_app::AppSink>());
        assert!(!sink.property::<bool>("sync"));
        assert!(!sink.property::<bool>("enable-last-sample"));
        assert!(format!("{:?}", o.config().sink).starts_with("Writer(WriterHandle"));
        let other = OutputContract {
            format: OutputFormat::I420,
            ..contract
        };
        assert!(matches!(
            Output::build_in(
                OutputConfig::new(other, OutputSink::Writer(w.handle())),
                FrameMemory::System,
            ),
            Err(PipelineError::Caps(_))
        ));
    }

    /// (b), (c), (d) The hand-over between producers, on the CPU: the
    /// writer alone (placeholders), then a system-memory session whose
    /// output forwards to the writer with a live test pattern, then the
    /// session stopped. At the sink: placeholders first, the output's frames,
    /// placeholders again at the end. One caps negotiation for the whole run,
    /// PTS strictly increasing, and a placeholder never follows a forwarded
    /// frame sooner than two periods.
    ///
    /// Only lower bounds on time are asserted (docs/testing.md): a loaded
    /// machine can stall the output's clock thread past two periods, which
    /// lets the writer fill in placeholders between forwarded frames and
    /// lengthens the gaps. The upper bounds (no placeholder while forwarding,
    /// no silence beyond three periods plus a margin) belong to the hardware
    /// test `a_consumer_keeps_one_stream_across_idle_active_idle`; here they
    /// are measured and printed only.
    #[test]
    fn the_writer_hands_over_between_placeholder_and_output_without_renegotiating() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let period = contract.frame_duration_ns();
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let log = watch_sink(&w);
        w.start().unwrap();
        wait_until(Duration::from_secs(10), "placeholders", || {
            log.lock().unwrap().buffers.len() >= 10
        });
        let placeholder = log.lock().unwrap().buffers[0].data;

        let output = Output::build_in(
            OutputConfig::new(contract, OutputSink::Writer(w.handle())),
            FrameMemory::System,
        )
        .unwrap();
        let mut session = Session::new(output).unwrap();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                "video/x-raw,format=NV12,width=160,height=120,framerate=30/1,colorimetry=2:4:5:1"
                    .parse::<gst::Caps>()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let ingest = Ingest::from_head(
            vec![src, caps],
            "NV12",
            (160, 120),
            AspectPolicy::Crop,
            session.output().frame_writer(),
        )
        .unwrap();
        session.switch_to(ingest).unwrap();
        let forwarded = |l: &SinkLog| l.buffers.iter().filter(|b| b.data != placeholder).count();
        wait_until(Duration::from_secs(10), "forwarded frames", || {
            forwarded(&log.lock().unwrap()) >= 30
        });
        // The writer's caps are exactly those the output negotiated at its
        // sink.
        let output_caps = session
            .output()
            .pipeline()
            .element(names::LOOPBACK)
            .and_then(|e| e.static_pad("sink"))
            .and_then(|p| p.current_caps())
            .expect("the output's sink caps");
        let writer_caps: gst::Caps = writer_caps_string(&contract).parse().unwrap();
        assert_eq!(output_caps, writer_caps);
        let fresh = session.output().counts().fresh;
        session.stop().unwrap();
        let after_stop = log.lock().unwrap().buffers.len();
        wait_until(Duration::from_secs(10), "placeholders again", || {
            log.lock().unwrap().buffers.len() >= after_stop + 10
        });
        let counts = w.counts();
        w.stop().unwrap();
        w.health().unwrap();

        let log = log.lock().unwrap();
        assert_eq!(log.caps_events.len(), 1, "{:?}", log.caps_events);
        assert!(fresh > 0, "the pattern never reached the output");
        let b = &log.buffers;
        let first_fwd = b.iter().position(|x| x.data != placeholder).unwrap();
        let last_fwd = b.iter().rposition(|x| x.data != placeholder).unwrap();
        let interleaved = b[first_fwd..=last_fwd]
            .iter()
            .filter(|x| x.data == placeholder)
            .count();
        assert!(first_fwd >= 10 && b.len() - last_fwd > 10);
        assert!(
            b[first_fwd..=last_fwd]
                .iter()
                .filter(|x| x.data != placeholder)
                .count()
                >= 30
        );
        assert!(b.iter().all(|x| x.size == b[0].size));
        assert!(b.windows(2).all(|w| w[0].pts < w[1].pts), "PTS order");
        // Every resumption of placeholders after a forwarded frame, the one
        // after the session stopped included, waits two periods.
        for w in b.windows(2) {
            if w[0].data != placeholder && w[1].data == placeholder {
                let resume = w[1].pts - w[0].pts;
                assert!(
                    resume >= 2 * period,
                    "a placeholder {resume} ns after a forwarded frame, before two periods"
                );
            }
        }
        let resume = b[last_fwd + 1].pts - b[last_fwd].pts;
        let gap = b.windows(2).map(|w| w[1].pts - w[0].pts).max().unwrap();
        eprintln!(
            "hand-over: {} buffers, forwarded {}..={} with {interleaved} placeholders among \
             them, placeholders resumed {:.1} ms after the last forwarded frame, longest PTS \
             gap {:.1} ms; {counts:?}",
            b.len(),
            first_fwd,
            last_fwd,
            resume as f64 / 1e6,
            gap as f64 / 1e6
        );
        // Delivered or dropped by the leaky queue, every forwarded frame
        // was accepted.
        assert!(
            counts.forwarded as usize > last_fwd - first_fwd,
            "{counts:?}"
        );
    }
}
