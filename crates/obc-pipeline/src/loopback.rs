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
//!   after the last one. It can be switched at run time
//!   ([`LoopbackWriter::set_placeholder_image`]) to a still image, cropped
//!   to fill the contract's frame and laid out as the black one
//!   ([`image_contract_frame`]), decoded on a thread of its own: the
//!   placeholder stays black until the image is ready, and when it cannot be
//!   read ([`PlaceholderState::Failed`]). An image asked for before
//!   [`LoopbackWriter::start`] (or just after) is waited for before the
//!   first placeholder, for at most [`WriterConfig::image_wait`] (1 s by
//!   default), so a writer started with an image (the daemon's start, its
//!   restart after a crash, while the crash cover of the previous one still
//!   serves the image) shows it from its first frame. Switching back to
//!   black is immediate. The last image decoded is kept: asked for again while the
//!   file at its path is the same one, it is shown at once, never black
//!   meanwhile. The crash cover has an image of its own (below).
//!   While an output is attached, the placeholder gives way to the hold
//!   (below) for as long as the hold lasts.
//! * **Forwarded frames.** [`WriterHandle::push`], called by the active
//!   output (an [`OutputSink::Writer`](crate::output::OutputSink::Writer)
//!   output ends in an `appsink` that forwards every buffer). The buffer is
//!   made writable for its metadata only: the memory is shared, never
//!   copied.
//! * **Hold.** While an output is attached ([`WriterHandle::attach`],
//!   until its [`WriterAttachment`] is dropped), the writer keeps a reference to
//!   the last forwarded frame `appsrc` accepted (its memory is not copied),
//!   and a slot the placeholder thread would fill within the attach's
//!   `hold` of that frame is filled by it again: a shallow copy, restamped
//!   like any push, counted in [`WriterCounts::held`]. A stall of the
//!   output's streaming thread (a deliberate switch of the effect, under
//!   its engine lock, or a module loading) then shows the last frame, not
//!   black. Past `hold` (a wedged output) the kept frame is dropped and the
//!   placeholder resumes; the last detach drops it at once, so a stopped
//!   output (the camera off) is followed by the placeholder
//!   [`WriterConfig::placeholder_after`] after its last frame, as without
//!   an attach. The writer's start and stop drop it too. This is
//!   privacy-safe: a held frame is one a consumer was already shown, and a
//!   real failure is covered by black, image or held frames the output
//!   itself forwards (the effect element's fallback), which reach the
//!   consumer at once and then are what is held. Without an attach the
//!   writer never holds.
//! * **Restart hold.** A deliberate restart of the session (a module that
//!   now provides a selected stage) stops one output and starts another.
//!   Called before it, [`LoopbackWriter::hold_across_restart`] starts the
//!   restart hold, bounded by its `bound` ([`LoopbackWriter::RESTART_HOLD`]
//!   by default), and opens a window of the same length for the stopping
//!   output's detach. While the hold is in force
//!   ([`WriterHandle::restart_holding`]), the kept frame fills every
//!   placeholder slot, attached or not, whatever the attached bound, and
//!   the clock of an output forwarding to the writer pushes nothing on a
//!   placeholder tick ([`crate::output`], "Forwarding to a loopback
//!   writer"): neither the stopping output's (its camera stops before it
//!   does, and its clock would turn to placeholders after the gap
//!   threshold) nor the new output's, from its start until its first
//!   camera frame. So from the call to the new output's first frame (raw,
//!   or processed by its effect) the consumer sees the last frame, never
//!   the placeholder. The stopping output's frames, forwarded while it is
//!   still attached (within the window) or late, once detached, are shown
//!   and kept in place of the earlier one, and do not end the hold; a
//!   last detach within the window keeps the hold, and consumes the
//!   window. The first frame forwarded by an output attached after that
//!   ends the hold, and the attached rules apply from then on. Past the
//!   bound (a session that never comes back) the kept frame is dropped
//!   and the placeholder resumes, the new output's clock's included.
//!   Without the call a detach drops the kept frame as above, so a camera
//!   turned off or a privacy stop shows the placeholder, never a frozen
//!   frame; a last detach outside the window (the new output stopped
//!   before its first frame: the camera turned off) ends the hold and
//!   drops the kept frame the same way.
//! * **Feeds and standby.** Every [`WriterHandle`] pushes for a feed. The
//!   writer accepts the forwarded frames of one feed, the current one, and
//!   refuses those of every other: not pushed, nothing of the writer's
//!   state touched (the hold, the kept frame, the placeholder's timing, a
//!   restart hold), counted in [`WriterCounts::standby_refused`] and not
//!   in [`WriterCounts::push_failed`]. Until a standby feed is made, there
//!   is only the first one, and every handle pushes as if there were no
//!   feeds at all. [`WriterHandle::standby`] makes a new feed, refused from
//!   the start: an output built on it (a session's next output, prepared
//!   while the current one streams, [`crate::session::Session::prepare_output`])
//!   runs, attaches and forwards, and the consumer sees none of it.
//!   [`WriterHandle::promote`] makes its feed the current one, under the
//!   push lock that serialises pushes, so the switch falls between two
//!   pushes: every frame before it is the old feed's, every frame after it
//!   the new one's, and from then on the old output's frames are refused
//!   the same way. Promotion is not a restart: it neither starts nor ends a
//!   hold. The promoted feed's first frame replaces the kept frame as any
//!   forwarded frame does (and ends a restart hold in force under the
//!   attached rules above, as the new output's first frame would); a
//!   refused frame does neither. A standby output attaches like any other
//!   (attachments are counted), so after a promotion the old output's
//!   detach is not the last one, and the kept frame stays while the
//!   promoted output is attached. [`LoopbackWriter::handle`] hands out a
//!   handle of the current feed.
//! * **Mask.** While [`LoopbackWriter::set_mask`] is on, a forwarded frame
//!   is replaced by the placeholder's image before it is pushed: the
//!   consumer sees the image the placeholder shows, in the forwarded
//!   frame's slot, stamped and counted as a forwarded frame (and in
//!   [`WriterCounts::masked`]), held as it would be. Only an image does
//!   that: while the placeholder is black (none asked for, still decoding,
//!   unreadable) the forwarded frames pass as they are. The output, its
//!   clock and every measure of the stream are untouched: the frames keep
//!   coming, only what the consumer sees of them changes. The daemon masks
//!   a camera whose privacy shutter is closed ([`crate::shutter`]), so the
//!   feed shows the away image in place of its black. Off at the build,
//!   kept across [`LoopbackWriter::stop`] and [`LoopbackWriter::start`].
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
//! uploads a timeout image and sets the driver's `timeout`
//! ([`crate::loopback_cover`]), so a consumer keeps receiving frames while
//! no writer streams: after the daemon died, until its restart writes
//! again. The timeout image is the cover image
//! ([`LoopbackWriter::set_cover_image`], the user's away image), independent
//! of the placeholder: black with none, and while it is first decoded; the
//! image once decoded (its frame is already laid out as the device's
//! frames, and when the device's format says otherwise the cover is black,
//! [`CoverShows::BlackForImage`] saying why); black, with the reason in
//! [`CoverShows::BlackForImage`] and [`LoopbackWriter::cover_image_state`],
//! when it cannot be read. When its frame changes after the start's upload,
//! the cover is installed again, on a helper thread of its own; a
//! placeholder switch or a camera turned on or off installs nothing.
//! [`LoopbackWriter::cover_shows`] tells what the last install put in
//! place. Never the last camera frame: the image is the user's chosen
//! picture, not the camera. The timeout stays above the writer's worst
//! gap between two frames ([`WriterConfig::placeholder_after`] plus a
//! period), so it
//! never fires while the writer runs. The hold does not change that gap: a
//! held frame is pushed in a placeholder's slot, on the placeholder
//! thread's grid. The outcome is
//! [`LoopbackWriter::consumer_cover`]; a failure (an older driver without
//! the controls) never stops the writer. Nothing is reset at the stop.
//!
//! **Tap.** [`LoopbackWriter::set_tap`] installs a [`FrameTap`]: every
//! buffer `appsrc` accepted, placeholder, forwarded and held alike, is handed to
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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gst::prelude::*;
use gst_obc::background::FileStamp;
use obc_core::OutputContract;
use serde::Serialize;

use crate::frame::Canonical;
use crate::loopback_cover::{
    ConsumerCover, CoverPlan, CoverReport, CoverShows, CoverSlot, FrameLayout,
};
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
    /// How long a start waits, before its first placeholder, for the
    /// placeholder's image still being decoded
    /// ([`LoopbackWriter::start`]).
    pub image_wait: Duration,
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
            image_wait: Self::IMAGE_WAIT,
        }
    }

    pub const DEFAULT_QUEUE_BUFFERS: u32 = 3;
    pub const PLACEHOLDER_AFTER_PERIODS: u64 = 2;
    /// The default [`Self::image_wait`]: a still decodes in tens of
    /// milliseconds; past a second the start does not wait any longer.
    pub const IMAGE_WAIT: Duration = Duration::from_secs(1);
}

/// A snapshot of the writer's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct WriterCounts {
    /// Placeholder frames `appsrc` accepted.
    pub placeholder: u64,
    /// Forwarded frames `appsrc` accepted.
    pub forwarded: u64,
    /// Repeats of the last forwarded frame `appsrc` accepted, pushed in a
    /// placeholder's slot while an output is attached (see the module docs,
    /// "Hold").
    pub held: u64,
    /// Pushes refused: by `appsrc` (flushing, stopped), or because the
    /// writer was not running.
    pub push_failed: u64,
    /// Forwarded frames refused because their feed is not the current one
    /// (a standby output's before its promotion, the old output's after
    /// it; see the module docs, "Feeds and standby"). Never pushed, and
    /// not counted in [`Self::push_failed`].
    pub standby_refused: u64,
    /// Of the frames accepted, those `appsrc` dropped because its queue was
    /// full (they never reached the sink).
    pub queue_dropped: u64,
    /// Of the forwarded frames accepted, those replaced by the
    /// placeholder's image while the mask is on
    /// ([`LoopbackWriter::set_mask`]).
    pub masked: u64,
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
    /// The live [`WriterAttachment`]s, by id, in attach order, with their
    /// hold bound: while one is, the last forwarded frame is kept and held,
    /// for the last one's bound.
    attachments: Vec<(u64, Duration)>,
    /// The id of the next [`WriterHandle::attach`].
    next_attachment: u64,
    /// The last forwarded frame `appsrc` accepted while attached or during
    /// a restart hold, by reference (its memory is not copied). Dropped by
    /// the last detach (unless it starts a restart hold), past the hold,
    /// and at the writer's start and stop.
    kept: Option<gst::Buffer>,
    /// The window of [`LoopbackWriter::hold_across_restart`]: until then, a
    /// last detach starts a restart hold instead of dropping `kept`.
    /// Consumed by that detach and by an attach.
    restart_window: Option<Instant>,
    /// The restart hold in force: `kept` is held until then, or until a
    /// frame is forwarded while an output is attached (the new output's
    /// first), or that output detaches.
    restart_hold: Option<Instant>,
    /// Sees every accepted buffer; behind the push lock, so a call never
    /// overlaps [`LoopbackWriter::set_tap`].
    tap: TapSlot,
    /// The feed whose forwarded frames are accepted (see the module docs,
    /// "Feeds and standby"): 0, the first one, until a promotion.
    current_feed: u64,
    /// The id of the next [`WriterHandle::standby`] feed.
    next_feed: u64,
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
    held: AtomicU64,
    push_failed: AtomicU64,
    standby_refused: AtomicU64,
    masked: AtomicU64,
    /// Forwarded frames are replaced by the placeholder's image
    /// ([`LoopbackWriter::set_mask`]).
    mask: AtomicBool,
    /// The first ERROR the pipeline posted.
    error: Mutex<Option<String>>,
    /// What the placeholder thread pushes, and the image asked for.
    frame: Mutex<PlaceholderFrame>,
    /// The crash cover's image asked for
    /// ([`LoopbackWriter::set_cover_image`]); the cover is told each change
    /// under this lock, so in the order of the changes. Never held with
    /// `frame`.
    cover_image: Mutex<StillRequest>,
    /// The crash cover.
    cover: Arc<CoverSlot>,
}

/// The placeholder in force ([`LoopbackWriter::set_placeholder_image`]).
#[derive(Debug)]
struct PlaceholderFrame {
    /// The frame pushed: the black one, or the image once decoded.
    buffer: gst::Buffer,
    /// The image asked for, and what it reached.
    request: StillRequest,
}

/// A still image asked for (the placeholder's or the crash cover's), and
/// what it reached.
#[derive(Debug)]
struct StillRequest {
    /// The image asked for (`None`: black), with the file at its path
    /// then ([`gst_obc::background::FileStamp`]).
    image: Option<(PathBuf, Option<FileStamp>)>,
    /// Counts the requests: a decode's result is kept only if no later one
    /// was made.
    serial: u64,
    state: PlaceholderState,
    /// The last image decoded, with what it was asked for as (a file that
    /// could be stat'ed): shown at once when the same is asked for again.
    /// Kept across black; dropped when another path is asked for. A failed
    /// decode is never kept.
    decoded: Option<DecodedImage>,
}

/// An image decoded into a frame of the contract, with the file it was
/// decoded from.
type DecodedImage = ((PathBuf, FileStamp), gst::Buffer);

/// What a new request of a [`StillRequest`] calls for.
#[derive(Debug)]
enum Asked {
    /// The image already asked for, the file at its path the same.
    Unchanged,
    /// Black.
    Black,
    /// The image, already decoded: shown at once.
    Ready(gst::Buffer),
    /// The image at that path, to decode for request `serial`.
    Decode(u64, PathBuf),
}

impl StillRequest {
    fn new() -> Self {
        StillRequest {
            image: None,
            serial: 0,
            state: PlaceholderState::Black,
            decoded: None,
        }
    }

    /// Asks for `image` (`None`: black). `shared`: the other request's
    /// decoded image, taken over when it is of the same file, so the same
    /// file is decoded once for both.
    fn ask(&mut self, image: Option<&Path>, shared: Option<DecodedImage>) -> Asked {
        let want = image.map(|p| (p.to_path_buf(), FileStamp::of(p)));
        if self.image == want {
            return Asked::Unchanged;
        }
        self.serial += 1;
        // Another image asked for: the one kept is not shown again.
        if let Some((p, _)) = &want
            && self.decoded.as_ref().is_some_and(|((dp, _), _)| dp != p)
        {
            self.decoded = None;
        }
        let key = match &want {
            Some((p, Some(stamp))) => Some((p.clone(), *stamp)),
            _ => None,
        };
        if let (Some(k), Some((sk, b))) = (&key, shared)
            && *k == sk
        {
            self.decoded = Some((sk, b));
        }
        self.image = want;
        match (&key, &self.decoded, image) {
            (Some(k), Some((dk, b)), _) if k == dk => {
                self.state = PlaceholderState::Image;
                Asked::Ready(b.clone())
            }
            (_, _, None) => {
                self.state = PlaceholderState::Black;
                Asked::Black
            }
            (_, _, Some(p)) => {
                self.state = PlaceholderState::Pending;
                Asked::Decode(self.serial, p.to_path_buf())
            }
        }
    }

    /// The decode of request `serial` of the image at `path` ended with
    /// `frame`: the image (kept) or the reason it cannot be shown, as
    /// [`gst_obc::fallback_unreadable`] words it; `None` when a later
    /// request was made meanwhile.
    fn decoded(
        &mut self,
        serial: u64,
        path: &Path,
        frame: Result<gst::Buffer, String>,
    ) -> Option<Result<gst::Buffer, String>> {
        if self.serial != serial {
            return None;
        }
        Some(match frame {
            Ok(b) => {
                if let Some((p, Some(stamp))) = &self.image {
                    self.decoded = Some(((p.clone(), *stamp), b.clone()));
                }
                self.state = PlaceholderState::Image;
                Ok(b)
            }
            Err(why) => {
                let why = gst_obc::fallback_unreadable(path, &why);
                self.state = PlaceholderState::Failed(why.clone());
                Err(why)
            }
        })
    }
}

/// What the writer's placeholder shows (and, for
/// [`LoopbackWriter::cover_image_state`], the crash cover's image).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceholderState {
    /// Black: no image is asked for.
    Black,
    /// Black while the image asked for is decoded.
    Pending,
    /// The image asked for.
    Image,
    /// Black: the image asked for cannot be shown; the reason, as
    /// [`gst_obc::fallback_unreadable`] words it.
    Failed(String),
}

/// What a push carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Producer {
    Placeholder,
    Forwarded,
    /// The last forwarded frame again, in a placeholder's slot.
    Held,
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

/// Whether the kept frame is held at `now`: an output is attached (its
/// bound `hold`) and the last forwarded frame is less than `hold` old.
fn hold_due(hold: Option<Duration>, last_forward: Option<Instant>, now: Instant) -> bool {
    hold.is_some_and(|hold| last_forward.is_some_and(|t| now < t + hold))
}

impl PushState {
    /// The bound of the hold: the last live attachment's, `None` with no
    /// output attached.
    fn hold(&self) -> Option<Duration> {
        self.attachments.last().map(|&(_, hold)| hold)
    }

    /// Whether the kept frame is held at `now`: during a restart hold,
    /// until its bound (attached or not: the last forwarded frame is the
    /// previous output's, so the attached output's own bound does not
    /// apply to it); otherwise while an output is attached, within its
    /// bound of the last forwarded frame ([`hold_due`]).
    fn holding(&self, now: Instant) -> bool {
        match self.restart_hold {
            Some(until) => now < until,
            None => hold_due(self.hold(), self.last_forward, now),
        }
    }

    /// Whether a restart hold is in force at `now` with a frame to hold.
    fn restart_holding(&self, now: Instant) -> bool {
        self.kept.is_some() && self.restart_hold.is_some_and(|until| now < until)
    }

    /// Ends every hold and returns the kept frame, for the caller to drop
    /// after the lock (the writer's start and stop).
    fn end_holds(&mut self) -> Option<gst::Buffer> {
        self.restart_window = None;
        self.restart_hold = None;
        self.kept.take()
    }
}

/// The references a push no longer needs, dropped by the caller once the
/// push lock is released: the last one may return a buffer to its pool.
type Leftover = [Option<gst::Buffer>; 2];

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
    /// (`after`), and is replaced by the kept frame while [`hold_due`].
    /// A forwarded frame of a feed other than the current one is refused
    /// before anything else (see the module docs, "Feeds and standby").
    fn push(&self, buffer: gst::Buffer, producer: Producer, feed: u64, after: Duration) -> Pushed {
        // Before the push lock: the placeholder's lock is never taken
        // under it. A forwarded frame the image replaces stays in the
        // shadowed binding and goes back to its pool when it drops, at the
        // end of this function, outside the push lock.
        let (buffer, masked) = match self.masked_frame(producer) {
            Some(image) => (image, true),
            None => (buffer, false),
        };
        let mut st = self.lock();
        if producer == Producer::Forwarded && feed != st.current_feed {
            drop(st);
            self.standby_refused.fetch_add(1, Ordering::Relaxed);
            // The frame goes back to its pool here, outside the push lock.
            drop(buffer);
            return Pushed::Refused;
        }
        let now = Instant::now();
        // Dropped after the lock is released.
        let mut expired = None;
        let (buffer, producer) = if producer == Producer::Forwarded {
            st.last_forward = Some(now);
            // The new output's first frame ends a restart hold; a frame of
            // the stopping one (attached within the window, or late, with
            // none attached) does not. A hold past its bound is over.
            // Assumed: the stopping output forwards nothing once the next
            // one is attached (the controller stops a session, its output
            // reaching NULL in `Output::stop_checked`, before it starts the
            // next), since a push does not say which output it comes from;
            // a late frame then would end the hold early, the attached
            // rules (and the new output's clock placeholder) applying
            // before its first frame.
            if (!st.attachments.is_empty() && st.restart_window.is_none())
                || st.restart_hold.is_some_and(|until| now >= until)
            {
                st.restart_hold = None;
            }
            (buffer, producer)
        } else if !placeholder_due(st.last_forward, now, after) {
            return Pushed::NotDue(placeholder_due_at(st.last_forward, after).unwrap_or(now));
        } else if st.holding(now)
            && let Some(kept) = &st.kept
        {
            // A shallow copy: new metadata, the forwarded frame's memory.
            (kept.copy(), Producer::Held)
        } else {
            // Past the hold (a wedged output, a restart past its bound):
            // the placeholder, as without an output attached.
            expired = st.kept.take();
            st.restart_hold = None;
            (buffer, producer)
        };
        let (pushed, leftover) = self.push_locked(&mut st, buffer, producer, now);
        drop(st);
        if masked && pushed == Pushed::Accepted {
            self.masked.fetch_add(1, Ordering::Relaxed);
        }
        // A buffer of the output's pool goes back to it here, outside the
        // push lock.
        drop((expired, leftover));
        pushed
    }

    /// [`Self::push`] once the frame is chosen; also returns the references
    /// it no longer needs (the frame a refused push did not hand over, the
    /// tap's reference, the kept frame a forwarded one replaced), for the
    /// caller to drop after the lock.
    fn push_locked(
        &self,
        st: &mut PushState,
        mut buffer: gst::Buffer,
        producer: Producer,
        now: Instant,
    ) -> (Pushed, Leftover) {
        if !st.running {
            // A placeholder racing the stop is not a failure.
            if producer == Producer::Forwarded {
                self.push_failed.fetch_add(1, Ordering::Relaxed);
            }
            return (Pushed::Refused, [Some(buffer), None]);
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
        // A reference (no copy) for the tap, and for the hold while an
        // output is attached or a restart hold lasts (a late frame of the
        // stopping output, shown, is the one held after it); taken only
        // when needed.
        let keep = producer == Producer::Forwarded
            && (!st.attachments.is_empty() || st.restart_hold.is_some());
        let reference = (st.tap.0.is_some() || keep).then(|| buffer.clone());
        let accepted = self.appsrc.push_buffer(buffer).is_ok();
        if accepted {
            if let (Some(tap), Some(b)) = (&st.tap.0, &reference) {
                tap(b);
            }
            let leftover = if keep {
                [std::mem::replace(&mut st.kept, reference), None]
            } else {
                [None, reference]
            };
            st.last_pts_ns = Some(pts);
            st.pushes += 1;
            st.last_push = Some(now);
            match producer {
                Producer::Placeholder => &self.placeholder,
                Producer::Forwarded => &self.forwarded,
                Producer::Held => &self.held,
            }
            .fetch_add(1, Ordering::Relaxed);
            (Pushed::Accepted, leftover)
        } else {
            self.push_failed.fetch_add(1, Ordering::Relaxed);
            (Pushed::Refused, [reference, None])
        }
    }

    /// The placeholder's image to push in place of a forwarded frame while
    /// the mask is on ([`LoopbackWriter::set_mask`]): a shallow copy;
    /// `None` for another producer, with the mask off, and while the
    /// placeholder shows no image.
    fn masked_frame(&self, producer: Producer) -> Option<gst::Buffer> {
        if producer != Producer::Forwarded || !self.mask.load(Ordering::Relaxed) {
            return None;
        }
        let f = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        (f.request.state == PlaceholderState::Image).then(|| f.buffer.copy())
    }

    fn counts(&self) -> WriterCounts {
        let last_push = self.lock().last_push;
        WriterCounts {
            placeholder: self.placeholder.load(Ordering::Relaxed),
            forwarded: self.forwarded.load(Ordering::Relaxed),
            held: self.held.load(Ordering::Relaxed),
            push_failed: self.push_failed.load(Ordering::Relaxed),
            standby_refused: self.standby_refused.load(Ordering::Relaxed),
            queue_dropped: self.appsrc.property::<u64>("dropped"),
            masked: self.masked.load(Ordering::Relaxed),
            last_push,
        }
    }
}

/// Pushes forwarded frames into a [`LoopbackWriter`]. Cheap to clone, and
/// `Send + Sync`: the active output's streaming thread holds one. Each
/// handle pushes for a feed (see the module docs, "Feeds and standby"),
/// which its clones share. Two handles are equal when they feed the same
/// writer, whatever their feeds.
#[derive(Clone)]
pub struct WriterHandle {
    shared: Arc<Shared>,
    after: Duration,
    /// The feed this handle pushes for.
    feed: u64,
}

impl std::fmt::Debug for WriterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterHandle")
            .field("contract", &self.shared.contract)
            .field("feed", &self.feed)
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
    /// running, counted in [`WriterCounts::push_failed`], and while this
    /// handle's feed is not the current one, counted in
    /// [`WriterCounts::standby_refused`]). Never blocks beyond the push
    /// lock.
    pub fn push(&self, buffer: gst::Buffer) -> bool {
        self.shared
            .push(buffer, Producer::Forwarded, self.feed, self.after)
            == Pushed::Accepted
    }

    /// A handle of a new feed, on standby (see the module docs, "Feeds and
    /// standby"): its pushes are refused, counted in
    /// [`WriterCounts::standby_refused`], until [`Self::promote`] makes it
    /// the current feed. For the output a session prepares while its
    /// current one streams ([`crate::session::Session::prepare_output`]).
    /// Never blocks beyond the push lock.
    pub fn standby(&self) -> WriterHandle {
        let mut st = self.shared.lock();
        // Feed 0 is the first one; standby feeds count from 1.
        st.next_feed = st.next_feed.max(1);
        let feed = st.next_feed;
        st.next_feed += 1;
        WriterHandle {
            shared: Arc::clone(&self.shared),
            after: self.after,
            feed,
        }
    }

    /// Makes this handle's feed the current one, at once: under the push
    /// lock, so between two pushes. From then on its frames are accepted
    /// and those of every other feed (the previous output's) are refused,
    /// counted in [`WriterCounts::standby_refused`]. Neither starts nor
    /// ends a hold, nor touches the kept frame: the feed's next frame
    /// replaces it as any forwarded frame does (module docs, "Feeds and
    /// standby"). Promoting the current feed does nothing. Never blocks
    /// beyond the push lock.
    pub fn promote(&self) {
        self.shared.lock().current_feed = self.feed;
    }

    /// Whether this handle's feed is the current one: its pushes reach the
    /// writer's `appsrc` (see [`Self::standby`] and [`Self::promote`]).
    pub fn is_current_feed(&self) -> bool {
        self.shared.lock().current_feed == self.feed
    }

    /// The contract of the frames the writer carries.
    pub fn contract(&self) -> &OutputContract {
        &self.shared.contract
    }

    pub fn counts(&self) -> WriterCounts {
        self.shared.counts()
    }

    /// Attaches an output (see the module docs, "Hold"): until the returned
    /// [`WriterAttachment`] is dropped, a slot the placeholder thread would
    /// fill within `hold` of the last forwarded frame is filled by that frame
    /// again ([`WriterCounts::held`]). Attachments are counted, so two
    /// outputs handing over to each other each hold their own; the bound is
    /// the last live attachment's. An attach consumes the window of
    /// [`LoopbackWriter::hold_across_restart`]; a restart hold in force
    /// lasts past it, until the attached output forwards its first frame
    /// (see the module docs, "Restart hold"). Never blocks beyond the push
    /// lock.
    pub fn attach(&self, hold: Duration) -> WriterAttachment {
        let mut st = self.shared.lock();
        let id = st.next_attachment;
        st.next_attachment += 1;
        st.attachments.push((id, hold));
        st.restart_window = None;
        WriterAttachment {
            shared: Arc::clone(&self.shared),
            id,
        }
    }

    /// Whether a restart hold is in force ([`LoopbackWriter::hold_across_restart`],
    /// module docs "Restart hold"): from that call, the last frame
    /// forwarded is held, through the stopping output's stop, until the
    /// next output forwards its first frame or the bound passes. The
    /// output clock of a forwarding output asks it on each placeholder
    /// tick, and pushes nothing while it is: its placeholder would be
    /// shown (black, or the away image) in place of the held frame, and in
    /// the new output it would end the hold. Never blocks beyond the push
    /// lock.
    pub fn restart_holding(&self) -> bool {
        self.shared.lock().restart_holding(Instant::now())
    }

    /// The bounds of the live attachments, in attach order (tests).
    #[cfg(test)]
    pub(crate) fn attached_holds(&self) -> Vec<Duration> {
        let st = self.shared.lock();
        st.attachments.iter().map(|&(_, hold)| hold).collect()
    }
}

/// An output attached to a [`LoopbackWriter`] ([`WriterHandle::attach`]);
/// dropping it detaches that output, and only that one. With no output
/// attached any more, the kept frame is dropped at once and the placeholder
/// resumes [`WriterConfig::placeholder_after`] after the last forwarded
/// frame, as before any attach; within the window of
/// [`LoopbackWriter::hold_across_restart`], the kept frame is held instead,
/// until the next output's first forwarded frame or the window's end. A
/// last detach while that restart hold is in force (the next output
/// stopped before its first frame) ends it and drops the kept frame.
#[must_use = "the output is detached when the attachment is dropped"]
pub struct WriterAttachment {
    shared: Arc<Shared>,
    id: u64,
}

impl std::fmt::Debug for WriterAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterAttachment")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for WriterAttachment {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        st.attachments.retain(|&(id, _)| id != self.id);
        let released = if st.attachments.is_empty() {
            // Within the window of a deliberate restart, the kept frame
            // stays held until the next output's first forwarded frame or
            // the bound; outside it (no window, or the next output stopped
            // before its first frame) the hold ends and the frame goes.
            match st.restart_window.take() {
                Some(until) if Instant::now() < until && st.kept.is_some() => {
                    st.restart_hold = Some(until);
                    None
                }
                _ => {
                    st.restart_hold = None;
                    st.kept.take()
                }
            }
        } else {
            None
        };
        drop(st);
        // A buffer of the output's pool goes back to it outside the lock.
        drop(released);
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
    /// How the writer's frames are laid out (the crash cover's image
    /// check).
    frame_layout: FrameLayout,
    /// Holds each image decode started from now until opened (tests): a
    /// pending decode observed without racing its thread.
    #[cfg(test)]
    decode_gate: Option<Arc<tests::DecodeGate>>,
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

/// One frame of `contract` showing the image at `path` (PNG or JPEG,
/// checked and decoded as the replace effect's background, cropped to fill
/// the frame, centred), in system memory laid out as
/// [`black_contract_frame`] lays out black (BT.601, the contract's range).
/// Blocks for the decode. The reason names the file, not the path.
pub fn image_contract_frame(contract: &OutputContract, path: &Path) -> Result<gst::Buffer, String> {
    let s = writer_caps_string(contract);
    let caps: gst::Caps = s.parse().map_err(|_| format!("caps {s}"))?;
    let info = gst_video::VideoInfo::from_caps(&caps).map_err(|_| format!("caps {s}"))?;
    let colour = obc_backend::ColourSpec {
        matrix: obc_backend::ColourMatrix::Bt601,
        full_range: contract.range == obc_core::ColourRange::Full,
    };
    let img = gst_obc::background::decode_yuv420(path, info.width(), info.height(), colour)?;
    let mut data = black_contract_frame(contract)
        .map_err(|e| e.to_string())?
        .map_readable()
        .map_err(|e| e.to_string())?
        .to_vec();
    let rows = |plane: usize,
                src: &[u8],
                width: usize,
                height: usize,
                step: usize,
                at: usize,
                data: &mut [u8]| {
        let (offset, stride) = (info.offset()[plane], info.stride()[plane] as usize);
        for r in 0..height {
            for c in 0..width {
                data[offset + r * stride + c * step + at] = src[r * width + c];
            }
        }
    };
    let (w, h) = (img.width as usize, img.height as usize);
    let (cw, ch) = (img.chroma_width(), img.chroma_height());
    rows(0, &img.y, w, h, 1, 0, &mut data);
    match contract.format {
        obc_core::OutputFormat::Nv12 => {
            rows(1, &img.u, cw, ch, 2, 0, &mut data);
            rows(1, &img.v, cw, ch, 2, 1, &mut data);
        }
        obc_core::OutputFormat::I420 => {
            rows(1, &img.u, cw, ch, 1, 0, &mut data);
            rows(2, &img.v, cw, ch, 1, 0, &mut data);
        }
    }
    Ok(gst::Buffer::from_mut_slice(data))
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

/// How `GstVideoInfo` lays out a frame of the writer's caps for
/// `contract`: the layout of [`black_contract_frame`] and
/// [`image_contract_frame`].
pub(crate) fn frame_layout(contract: &OutputContract) -> Result<FrameLayout, PipelineError> {
    let s = writer_caps_string(contract);
    let caps: gst::Caps = s.parse().map_err(|_| PipelineError::Caps(s.clone()))?;
    let info = gst_video::VideoInfo::from_caps(&caps).map_err(|_| PipelineError::Caps(s))?;
    let planes = info.n_planes() as usize;
    let (mut strides, mut offsets) = ([0; 3], [0; 3]);
    for i in 0..planes.min(3) {
        strides[i] = usize::try_from(info.stride()[i]).unwrap_or(0);
        offsets[i] = info.offset()[i];
    }
    Ok(FrameLayout {
        fourcc: crate::loopback_cover::fourcc(contract.format),
        height: info.height(),
        planes,
        strides,
        offsets,
        size: info.size(),
    })
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
            held: AtomicU64::new(0),
            masked: AtomicU64::new(0),
            mask: AtomicBool::new(false),
            push_failed: AtomicU64::new(0),
            standby_refused: AtomicU64::new(0),
            error: Mutex::default(),
            frame: Mutex::new(PlaceholderFrame {
                buffer: black_contract_frame(&contract)?,
                request: StillRequest::new(),
            }),
            cover_image: Mutex::new(StillRequest::new()),
            cover: Arc::default(),
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
            thread: None,
            cover: Arc::clone(&shared.cover),
            cover_probe: None,
            frame_layout: frame_layout(&contract)?,
            #[cfg(test)]
            decode_gate: None,
            shared,
        })
    }

    /// Starts the writer: checks the loopback (before anything streams),
    /// goes to PLAYING and starts the placeholder thread. A second call
    /// does nothing. On error the pipeline is back in NULL. A start after
    /// [`Self::stop`] opens the device again and negotiates again.
    ///
    /// When the placeholder's image is still being decoded
    /// ([`PlaceholderState::Pending`]: asked for before the start, or just
    /// before it), the placeholder thread waits for it before its first
    /// placeholder, for at most [`WriterConfig::image_wait`]: the first
    /// placeholder is the image, not black. Past that bound it starts
    /// black and shows the image once it is ready. Nothing is written
    /// meanwhile (a crash cover of an earlier writer keeps serving its
    /// image); a frame forwarded meanwhile is pushed as always. The start
    /// itself never waits.
    pub fn start(&mut self) -> Result<(), PipelineError> {
        if self.thread.is_some() {
            return Ok(());
        }
        let res = self.start_inner();
        if res.is_err() {
            self.shared.lock().running = false;
            // Disarmed before the device closes (see `stop`).
            self.end_cover();
            let _ = self.pipeline.pipeline().set_state(gst::State::Null);
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
        let kept = {
            let mut st = self.shared.lock();
            st.running = true;
            st.pushes = 0;
            st.last_forward = None;
            st.end_holds()
        };
        // Dropped outside the push lock.
        drop(kept);
        let stop = Arc::new(StopSignal::default());
        let failure: Arc<Mutex<Option<String>>> = Arc::default();
        let (shared, after) = (Arc::clone(&self.shared), self.cfg.placeholder_after);
        let image_until = Instant::now() + self.cfg.image_wait;
        let (thread_stop, thread_failure) = (Arc::clone(&stop), Arc::clone(&failure));
        let handle = std::thread::Builder::new()
            .name("obc-placeholder".into())
            .spawn(move || {
                run_recording_panic(
                    move || placeholder_loop(&shared, after, image_until, &thread_stop),
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
            frame: self.frame_layout,
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
            // Claimed by `end_cover` first: it removes the probe.
            if flag.swap(true, Ordering::AcqRel) {
                return gst::PadProbeReturn::Ok;
            }
            if let Some(plan) = plan.lock().unwrap_or_else(|e| e.into_inner()).take() {
                slot.spawn_helper(generation, plan);
            }
            gst::PadProbeReturn::Remove
        });
        self.cover_probe = Some(CoverProbe { pad, id, fired });
    }

    /// Ends the crash cover's part of a start, before the pipeline goes to
    /// NULL (the device still open, no push accepted any more): the probe
    /// removed unless it fired (whichever of the two claims it first acts),
    /// no install or re-install started from here on, the helper threads
    /// joined (an install in flight completes on the open device), a cover
    /// still pending reported off.
    fn end_cover(&mut self) {
        if let Some(p) = self.cover_probe.take()
            && !p.fired.swap(true, Ordering::AcqRel)
            && let Some(id) = p.id
        {
            p.pad.remove_probe(id);
        }
        self.cover.disarm();
        self.cover.join_helper();
        self.cover.cancel();
    }

    /// What the timeout image of the crash cover shows, as last installed
    /// (see the module docs, "Crash cover"): [`CoverShows::Black`] before
    /// any install.
    pub fn cover_shows(&self) -> CoverShows {
        self.cover.shows()
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

    /// Switches the placeholder: black (`None`, at once), or the image at
    /// `image` ([`image_contract_frame`]), decoded on a thread of its own
    /// (black until it is ready; black, [`PlaceholderState::Failed`], when
    /// it cannot be read). The image already asked for is not decoded
    /// again, unless the file at its path is another one now (replaced at
    /// the same path: [`FileStamp`]). The last image decoded is kept across
    /// black, until another path is asked for: the same file asked for
    /// again is shown at once ([`PlaceholderState::Image`], no decode), and
    /// so is the file the crash cover has decoded
    /// ([`Self::set_cover_image`]). Never blocks for the decode; kept
    /// across [`Self::stop`] and [`Self::start`]. Independent of the crash
    /// cover: it changes nothing of what the cover shows.
    pub fn set_placeholder_image(&self, image: Option<&Path>) {
        let shared = self.lock_cover_image().decoded.clone();
        let mut f = self.shared.frame.lock().unwrap_or_else(|e| e.into_inner());
        let (serial, path) = match f.request.ask(image, shared) {
            Asked::Unchanged => return,
            Asked::Ready(b) => {
                f.buffer = b;
                return;
            }
            Asked::Black => {
                f.buffer = self.placeholder.clone();
                return;
            }
            Asked::Decode(serial, path) => {
                f.buffer = self.placeholder.clone();
                (serial, path)
            }
        };
        drop(f);
        let shared = Arc::clone(&self.shared);
        let decoded = move |frame| {
            let mut f = shared.frame.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(Ok(b)) = f.request.decoded(serial, &path, frame) {
                f.buffer = b;
            }
        };
        self.decode(image, "obc-placeholder-image", decoded);
    }

    /// Decodes the image at `image` into a frame of the contract on a
    /// thread named `name`, and hands the outcome to `done` there (or here,
    /// when no thread can be started).
    fn decode(
        &self,
        image: Option<&Path>,
        name: &str,
        done: impl FnOnce(Result<gst::Buffer, String>) + Send + 'static,
    ) {
        let Some(path) = image.map(Path::to_path_buf) else {
            return;
        };
        let contract = self.cfg.contract;
        let done = Arc::new(Mutex::new(Some(done)));
        let on_thread = Arc::clone(&done);
        #[cfg(test)]
        let gate = self.decode_gate.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                #[cfg(test)]
                if let Some(gate) = gate {
                    gate.wait();
                }
                let frame = image_contract_frame(&contract, &path);
                if let Some(done) = on_thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    done(frame);
                }
            });
        if let Err(e) = spawned
            && let Some(done) = done.lock().unwrap_or_else(|e| e.into_inner()).take()
        {
            done(Err(format!("no thread to decode it: {e}")));
        }
    }

    /// What the placeholder shows ([`Self::set_placeholder_image`]).
    pub fn placeholder_state(&self) -> PlaceholderState {
        self.shared
            .frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request
            .state
            .clone()
    }

    fn lock_cover_image(&self) -> MutexGuard<'_, StillRequest> {
        self.shared
            .cover_image
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Sets the crash cover's image (see the module docs, "Crash cover"):
    /// black (`None`, at once), or the image at `image`
    /// ([`image_contract_frame`]), decoded once on a thread of its own and
    /// never on the caller's. Independent of the placeholder
    /// ([`Self::set_placeholder_image`]): the cover shows its image whether
    /// or not the placeholder shows one, and a placeholder switch or a
    /// camera turned on or off changes nothing of it. The same file as the
    /// one the placeholder has decoded is taken from it, not decoded again;
    /// the image already asked for, the file at its path the same
    /// ([`FileStamp`]), changes nothing, and the last image decoded is kept
    /// across black, as for the placeholder. While another image is
    /// decoded, the cover keeps what it shows; once it is ready the cover
    /// shows it. An image that cannot be read leaves the cover black:
    /// [`Self::cover_image_state`] is [`PlaceholderState::Failed`] with the
    /// reason, and [`Self::cover_shows`] reports
    /// [`CoverShows::BlackForImage`] with it. The cover is installed again
    /// (on a helper thread) only when the frame it shows changes: the image
    /// becomes ready, another one replaces it, or black replaces it. Kept
    /// across [`Self::stop`] and [`Self::start`]; a start installs what it
    /// is then.
    pub fn set_cover_image(&self, image: Option<&Path>) {
        let shared = self
            .shared
            .frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request
            .decoded
            .clone();
        let mut c = self.lock_cover_image();
        let (serial, path) = match c.ask(image, shared) {
            Asked::Unchanged => return,
            Asked::Ready(b) => {
                self.shared.cover.set_image(Some(b), None);
                return;
            }
            Asked::Black => {
                self.shared.cover.set_image(None, None);
                return;
            }
            // The cover keeps what it shows until the decode ends.
            Asked::Decode(serial, path) => (serial, path),
        };
        drop(c);
        let shared = Arc::clone(&self.shared);
        let decoded = move |frame| {
            let mut c = shared.cover_image.lock().unwrap_or_else(|e| e.into_inner());
            match c.decoded(serial, &path, frame) {
                Some(Ok(b)) => shared.cover.set_image(Some(b), None),
                Some(Err(why)) => shared.cover.set_image(None, Some(why)),
                None => {}
            }
        };
        self.decode(image, "obc-cover-image", decoded);
    }

    /// The crash cover's image ([`Self::set_cover_image`]):
    /// [`PlaceholderState::Black`] with none asked for,
    /// [`PlaceholderState::Pending`] while it is decoded,
    /// [`PlaceholderState::Image`] once ready, and
    /// [`PlaceholderState::Failed`] (the cover black) with the reason it
    /// cannot be read.
    pub fn cover_image_state(&self) -> PlaceholderState {
        self.lock_cover_image().state.clone()
    }

    /// The longest [`Self::hold_across_restart`] is meant to hold: long
    /// enough for a session to stop and the next one to forward its first
    /// camera frame (the camera reopened, the effect's modules loaded),
    /// short enough that a restart that never comes back turns to the
    /// placeholder within seconds.
    pub const RESTART_HOLD: Duration = Duration::from_secs(3);

    /// Holds the last forwarded frame across a deliberate restart of the
    /// session (see the module docs, "Restart hold"): from this call, the
    /// writer fills the placeholder's slots with its last frame, as during
    /// an attached stall, and the clocks of the outputs forwarding to it
    /// push no placeholder ([`WriterHandle::restart_holding`]), through
    /// the attached output's stop (when it detaches within `bound` of this
    /// call) until the next output forwards its first frame or `bound`
    /// (from this call) has passed, instead of turning to the placeholder.
    /// A call replaces the window and the hold of an earlier one; the
    /// window is consumed by the first detach that leaves no output
    /// attached, and by an attach. With no frame kept (no output attached,
    /// or past its hold), nothing is held. Never for a stop that must hide
    /// the camera (the camera turned off, a privacy drop): those detach
    /// without this call, and the placeholder follows as before; one that
    /// comes while a hold is in force ends it with
    /// [`Self::end_restart_hold`]. [`Self::RESTART_HOLD`] is the bound
    /// meant for a session restart. Never blocks beyond the push lock.
    pub fn hold_across_restart(&self, bound: Duration) {
        let mut st = self.shared.lock();
        let until = Instant::now() + bound;
        st.restart_window = Some(until);
        if st.kept.is_some() {
            st.restart_hold = Some(until);
        }
    }

    /// Ends a restart hold ([`Self::hold_across_restart`]) at once, and
    /// its window: the kept frame is dropped and the placeholder resumes
    /// [`WriterConfig::placeholder_after`] after the last forwarded frame
    /// (the clock of an output still attached resumes its own placeholder
    /// too, [`WriterHandle::restart_holding`] being false). For a restart
    /// that turns into a stop that must hide the camera before the next
    /// output's first frame (the camera turned off, privacy turned on, the
    /// writer or the new start failed): the frame held is the camera's
    /// last, never shown past that. With no hold in force, only the kept
    /// frame is dropped (a stall of an attached output then shows the
    /// placeholder). Never blocks beyond the push lock.
    pub fn end_restart_hold(&self) {
        let released = {
            let mut st = self.shared.lock();
            st.restart_window = None;
            st.restart_hold = None;
            st.kept.take()
        };
        // A buffer of an output's pool goes back to it outside the lock.
        drop(released);
    }

    /// Turns the mask on or off (see the module docs, "Mask"): while on,
    /// each forwarded frame is replaced by the placeholder's image, when
    /// it shows one ([`PlaceholderState::Image`]); the next forwarded frame
    /// after it is turned off is shown as it is. Changes nothing of the
    /// placeholder, the hold or the crash cover. Never blocks.
    pub fn set_mask(&self, on: bool) {
        self.shared.mask.store(on, Ordering::Relaxed);
    }

    /// Whether the mask is on ([`Self::set_mask`]).
    pub fn masked(&self) -> bool {
        self.shared.mask.load(Ordering::Relaxed)
    }

    /// A handle for the active output to forward its frames with: of the
    /// current feed (see the module docs, "Feeds and standby"; the first
    /// one until a promotion, the last one promoted after).
    pub fn handle(&self) -> WriterHandle {
        WriterHandle {
            shared: Arc::clone(&self.shared),
            after: self.cfg.placeholder_after,
            feed: self.shared.lock().current_feed,
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
        let kept = {
            let mut st = self.shared.lock();
            st.running = false;
            st.end_holds()
        };
        drop(kept);
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
        // The cover is disarmed while the device is still open: a decode
        // finishing now cannot start a re-install on a closed device.
        self.end_cover();
        let p = self.pipeline.pipeline();
        let state = p.set_state(gst::State::Null);
        let (_, cur, _) = p.state(gst::ClockTime::from_seconds(5));
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

/// How often the placeholder thread looks whether the image it waits for
/// at the start is decoded ([`LoopbackWriter::start`]).
const IMAGE_WAIT_POLL: Duration = Duration::from_millis(5);

/// The placeholder thread: one placeholder per contract period, on its own
/// grid (resynchronised, never bursting, after a stall longer than a
/// period). While frames are forwarded it sleeps until the instant
/// placeholders would be due, and starts a new grid there: they resume
/// [`WriterConfig::placeholder_after`] after the last forwarded frame, not
/// on the next tick of the old grid. Before its first placeholder it waits
/// until `image_until` for an image still being decoded.
fn placeholder_loop(shared: &Shared, after: Duration, image_until: Instant, stop: &StopSignal) {
    let period = Duration::from_nanos(shared.period_ns.max(1));
    let mut next = Instant::now();
    let mut waiting = true;
    loop {
        if stop.wait_until(next) {
            return;
        }
        if waiting {
            let pending = shared
                .frame
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .request
                .state
                == PlaceholderState::Pending;
            let now = Instant::now();
            if pending && now < image_until {
                next = (now + IMAGE_WAIT_POLL).min(image_until);
                continue;
            }
            waiting = false;
            next = now;
        }
        // A shallow copy: new metadata, the placeholder frame's memory.
        let placeholder = shared
            .frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffer
            .copy();
        // A placeholder is of no feed: the feed given is not looked at.
        if let Pushed::NotDue(due) = shared.push(placeholder, Producer::Placeholder, 0, after) {
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
    use crate::ingest::Ingest;
    use crate::output::{Output, OutputConfig};
    use crate::session::Session;
    use crate::test_support::{require_elements, white_png};
    use obc_core::config::VideoMemory;

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
        /// Every Y byte 235 and every chroma byte 128 (limited white).
        white: bool,
        info: Option<gst_video::VideoInfo>,
    }

    #[derive(Debug, Default)]
    struct SinkLog {
        buffers: Vec<AtSink>,
        caps_events: Vec<gst::Caps>,
    }

    fn is_black(info: &gst_video::VideoInfo, data: &[u8]) -> bool {
        is_uniform(info, data, 16)
    }

    /// Every byte of the luma plane `y`, every byte after it 128.
    fn is_uniform(info: &gst_video::VideoInfo, data: &[u8], y: u8) -> bool {
        let luma_end = info.offset()[0] + info.stride()[0] as usize * info.height() as usize;
        data.len() == info.size()
            && data[..luma_end].iter().all(|&b| b == y)
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
                        let white = info.as_ref().is_some_and(|i| is_uniform(i, &map, 235));
                        let entry = AtSink {
                            white,
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

    /// Holds the image decodes of a writer ([`LoopbackWriter::gate_decodes`])
    /// until opened: closed, the decode is pending however fast it would
    /// end (a missing file fails at once), so its state is no race.
    #[derive(Debug, Default)]
    pub(super) struct DecodeGate {
        open: Mutex<bool>,
        opened: std::sync::Condvar,
    }

    impl DecodeGate {
        /// Blocks the decode thread until the gate is opened.
        pub(super) fn wait(&self) {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            while !*open {
                open = self.opened.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        }

        fn open(&self) {
            *self.open.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.opened.notify_all();
        }
    }

    /// A closed [`DecodeGate`]; opened by [`Self::open`], or when dropped
    /// (a failing test leaves no decode thread blocked).
    struct GateHandle(Arc<DecodeGate>);

    impl GateHandle {
        fn open(&self) {
            self.0.open();
        }
    }

    impl Drop for GateHandle {
        fn drop(&mut self) {
            self.0.open();
        }
    }

    impl LoopbackWriter {
        /// Holds every image decode started from now until the handle
        /// opens (the one before, if any, stays as it is).
        fn gate_decodes(&mut self) -> GateHandle {
            let gate = Arc::new(DecodeGate::default());
            self.decode_gate = Some(Arc::clone(&gate));
            GateHandle(gate)
        }
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

    const IMAGE_ELEMENTS: &[&str] = &[
        "videotestsrc",
        "pngenc",
        "pngdec",
        "filesink",
        "videoconvert",
        "videocrop",
        "videoscale",
    ];

    /// An image frame is laid out as the black one, in both formats, with
    /// the rows' padding left as it is.
    #[test]
    fn an_image_frame_fills_the_contract_layout() {
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        // A square image into 4:3 and an odd-stride width: cropped to fill.
        let png = white_png(dir.path(), 200, 200);
        for format in [OutputFormat::Nv12, OutputFormat::I420] {
            for mode in ["320x240@30", "642x480@30"] {
                let c = OutputContract {
                    format,
                    ..mode.parse().unwrap()
                };
                let caps: gst::Caps = writer_caps_string(&c).parse().unwrap();
                let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
                let b = image_contract_frame(&c, &png).unwrap();
                let map = b.map_readable().unwrap();
                assert_eq!(map.size(), info.size(), "{c}");
                let (w, h) = (info.width() as usize, info.height() as usize);
                let planes: &[(usize, usize, usize)] = match format {
                    OutputFormat::Nv12 => &[(0, w, h), (1, w, h / 2)],
                    OutputFormat::I420 => &[(0, w, h), (1, w / 2, h / 2), (2, w / 2, h / 2)],
                };
                for &(plane, bytes, rows) in planes {
                    let (o, stride) = (info.offset()[plane], info.stride()[plane] as usize);
                    let want = if plane == 0 { 235 } else { 128 };
                    for r in 0..rows {
                        let row = &map[o + r * stride..o + r * stride + bytes];
                        assert!(row.iter().all(|&b| b == want), "{c} plane {plane} row {r}");
                    }
                }
            }
        }
        let e = image_contract_frame(&small(OutputFormat::Nv12), &dir.path().join("gone.png"))
            .unwrap_err();
        assert!(e.contains("the image file is unreadable"), "{e}");
    }

    /// The placeholder switches to an image (decoded off the caller's
    /// thread: black meanwhile) and back to black at once; an image that
    /// cannot be read leaves it black, with the reason.
    #[test]
    fn the_placeholder_switches_between_black_and_an_image() {
        require_elements(ELEMENTS);
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let png = white_png(dir.path(), 160, 160);
        let missing = dir.path().join("gone.png");
        for format in [OutputFormat::Nv12, OutputFormat::I420] {
            let mut w = LoopbackWriter::build(small(format), WriterSink::Fake).unwrap();
            assert_eq!(w.placeholder_state(), PlaceholderState::Black);
            let log = watch_sink(&w);
            w.start().unwrap();
            let after = |n: usize, what: &str, want: fn(&AtSink) -> bool| {
                wait_until(Duration::from_secs(10), what, || {
                    log.lock().unwrap().buffers.iter().skip(n).any(want)
                });
            };
            let count = || log.lock().unwrap().buffers.len();
            after(0, "a black placeholder", |b| b.black);

            w.set_placeholder_image(Some(&png));
            wait_until(Duration::from_secs(10), "the image decoded", || {
                w.placeholder_state() == PlaceholderState::Image
            });
            after(count(), "the image", |b| b.white);
            // Asked again: nothing changes.
            w.set_placeholder_image(Some(&png));
            assert_eq!(w.placeholder_state(), PlaceholderState::Image);
            let n = count();
            after(n, "the image again", |b| b.white);
            // Replaced at the same path: asked again, it is read again.
            let gone = dir.path().join("moved.png");
            std::fs::rename(&png, &gone).unwrap();
            std::fs::write(&png, b"not an image").unwrap();
            w.set_placeholder_image(Some(&png));
            wait_until(Duration::from_secs(10), "the new file read", || {
                matches!(w.placeholder_state(), PlaceholderState::Failed(_))
            });
            std::fs::rename(&gone, &png).unwrap();
            w.set_placeholder_image(Some(&png));
            wait_until(Duration::from_secs(10), "the image read again", || {
                w.placeholder_state() == PlaceholderState::Image
            });
            after(count(), "the image once more", |b| b.white);

            // Black at once.
            w.set_placeholder_image(None);
            assert_eq!(w.placeholder_state(), PlaceholderState::Black);
            // The push lock orders pushes: one in flight may still be the
            // image, the ones after are black.
            let n = count() + 1;
            after(n, "black again", |b| b.black);
            assert!(log.lock().unwrap().buffers.iter().skip(n).all(|b| b.black));

            // Unreadable: black, and why.
            w.set_placeholder_image(Some(&missing));
            wait_until(Duration::from_secs(10), "the decode failed", || {
                matches!(w.placeholder_state(), PlaceholderState::Failed(_))
            });
            let PlaceholderState::Failed(why) = w.placeholder_state() else {
                unreachable!()
            };
            assert!(
                why.starts_with(&format!(
                    "Away image {} could not be read (the image file is unreadable",
                    missing.display()
                )) && why.ends_with("): showing black"),
                "{why}"
            );
            let n = count();
            after(n, "black with the failure", |b| b.black);
            assert!(log.lock().unwrap().buffers.iter().skip(n).all(|b| b.black));
            w.stop().unwrap();
            let log = log.lock().unwrap();
            assert!(log.caps_events.len() <= 1, "never renegotiated");
            assert!(log.buffers.iter().all(|b| b.black || b.white), "{format:?}");
        }
    }

    /// With the mask on, forwarded frames are replaced by the
    /// placeholder's image, counted as forwarded and as masked; with the
    /// mask off, or the placeholder black, they pass as they are.
    #[test]
    fn the_mask_shows_the_placeholder_image_in_place_of_forwarded_frames() {
        require_elements(ELEMENTS);
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let png = white_png(dir.path(), 160, 160);
        let contract = small(OutputFormat::Nv12);
        // Placeholders only 10 s after a forwarded frame: every buffer at
        // the sink after the first forwarded one is a forwarded one.
        let mut cfg = WriterConfig::new(contract, WriterSink::Fake);
        cfg.placeholder_after = Duration::from_secs(10);
        let mut w = LoopbackWriter::build_with(cfg).unwrap();
        assert!(!w.masked(), "off at the build");
        let h = w.handle();
        let log = watch_sink(&w);
        w.start().unwrap();
        let count = || log.lock().unwrap().buffers.len();
        let push = |n: usize| {
            let from = count();
            for _ in 0..n {
                assert!(h.push(black_contract_frame(&contract).unwrap()));
            }
            wait_until(Duration::from_secs(10), "the forwarded frames", || {
                count() >= from + n
            });
            log.lock().unwrap().buffers[from..from + n].to_vec()
        };
        push(1);
        w.set_placeholder_image(Some(&png));
        wait_until(Duration::from_secs(10), "the image decoded", || {
            w.placeholder_state() == PlaceholderState::Image
        });
        assert!(push(3).iter().all(|b| b.black), "the mask off");
        w.set_mask(true);
        assert!(w.masked());
        assert!(push(3).iter().all(|b| b.white), "the image in their place");
        assert_eq!(w.counts().masked, 3);
        // A black placeholder masks nothing.
        w.set_placeholder_image(None);
        assert!(push(2).iter().all(|b| b.black));
        assert_eq!(w.counts().masked, 3);
        w.set_placeholder_image(Some(&png));
        w.set_mask(false);
        assert!(push(2).iter().all(|b| b.black), "the mask off again");
        let counts = w.counts();
        assert_eq!((counts.forwarded, counts.masked), (11, 3));
        w.stop().unwrap();
    }

    /// The image asked for again after black, the file at its path the
    /// same, is shown at once: not decoded again, never black meanwhile.
    /// Another image asked for in between, or a file replaced at the same
    /// path, it is read afresh.
    #[test]
    fn an_image_asked_for_again_after_black_is_shown_at_once() {
        require_elements(ELEMENTS);
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let png = white_png(dir.path(), 160, 160);
        let mut w = LoopbackWriter::build(small(OutputFormat::Nv12), WriterSink::Fake).unwrap();
        let log = watch_sink(&w);
        w.start().unwrap();
        let count = || log.lock().unwrap().buffers.len();
        let more = |n: usize, what: &str| {
            wait_until(Duration::from_secs(10), what, || count() >= n);
        };
        w.set_placeholder_image(Some(&png));
        wait_until(Duration::from_secs(10), "the image decoded", || {
            w.placeholder_state() == PlaceholderState::Image
        });
        w.set_placeholder_image(None);
        assert_eq!(w.placeholder_state(), PlaceholderState::Black);
        let n = count() + 1;
        more(n + 1, "black pushed");
        // Asked for again: the image at once, no decode pending.
        w.set_placeholder_image(Some(&png));
        assert_eq!(w.placeholder_state(), PlaceholderState::Image);
        // The push lock orders pushes: one in flight may still be black,
        // every one after is the image.
        let n = count() + 1;
        more(n + 5, "the image pushed");
        assert!(
            log.lock().unwrap().buffers.iter().skip(n).all(|b| b.white),
            "never black once asked for again"
        );
        // Another image asked for in between: the one kept is dropped, and
        // asked for again (after black) it is read again.
        let other = dir.path().join("other.png");
        std::fs::write(&other, b"not an image").unwrap();
        w.set_placeholder_image(Some(&other));
        w.set_placeholder_image(None);
        let gate = w.gate_decodes();
        w.set_placeholder_image(Some(&png));
        assert_eq!(w.placeholder_state(), PlaceholderState::Pending);
        gate.open();
        wait_until(Duration::from_secs(10), "the image decoded again", || {
            w.placeholder_state() == PlaceholderState::Image
        });
        // Replaced at the same path: read afresh, not the image kept.
        w.set_placeholder_image(None);
        let gone = dir.path().join("moved.png");
        std::fs::rename(&png, &gone).unwrap();
        std::fs::write(&png, b"not an image").unwrap();
        w.set_placeholder_image(Some(&png));
        assert_ne!(w.placeholder_state(), PlaceholderState::Image);
        wait_until(Duration::from_secs(10), "the new file read", || {
            matches!(w.placeholder_state(), PlaceholderState::Failed(_))
        });
        // A failed decode is not kept: asked for again, read again.
        w.set_placeholder_image(None);
        let gate = w.gate_decodes();
        w.set_placeholder_image(Some(&png));
        assert_eq!(w.placeholder_state(), PlaceholderState::Pending);
        gate.open();
        w.stop().unwrap();
    }

    /// The crash cover has an image of its own: the placeholder's never
    /// reaches it, a cover image is decoded once (the same file the
    /// placeholder decoded is shared, not decoded again, and the other way
    /// round), the cover keeps its frame while another image is decoded,
    /// an unreadable image leaves it black with the reason, and black is
    /// at once.
    #[test]
    fn the_cover_image_is_independent_of_the_placeholder() {
        require_elements(ELEMENTS);
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let png = white_png(dir.path(), 160, 160);
        let w = LoopbackWriter::build(small(OutputFormat::Nv12), WriterSink::Fake).unwrap();
        let cover = || w.cover.image().map(|b| b.as_ptr());
        let placeholder = || w.shared.frame.lock().unwrap().buffer.as_ptr();
        assert_eq!(w.cover_image_state(), PlaceholderState::Black);
        // The placeholder's image is not the cover's.
        w.set_placeholder_image(Some(&png));
        wait_until(Duration::from_secs(10), "the image decoded", || {
            w.placeholder_state() == PlaceholderState::Image
        });
        assert!(cover().is_none(), "the placeholder reached the cover");
        // The same file for the cover: taken from the placeholder at once.
        w.set_cover_image(Some(&png));
        assert_eq!(w.cover_image_state(), PlaceholderState::Image);
        assert_eq!(cover(), Some(placeholder()), "decoded once, shared");
        // Placeholder switches change nothing of the cover.
        w.set_placeholder_image(None);
        assert_eq!(w.placeholder_state(), PlaceholderState::Black);
        assert!(cover().is_some());
        let shown = cover();
        // Another image: the cover keeps its frame while it is decoded;
        // unreadable, it is black with the reason.
        let missing = dir.path().join("gone.png");
        w.set_cover_image(Some(&missing));
        assert_ne!(w.cover_image_state(), PlaceholderState::Image);
        wait_until(Duration::from_secs(10), "the decode failed", || {
            matches!(w.cover_image_state(), PlaceholderState::Failed(_))
        });
        assert!(cover().is_none(), "black for an unreadable image");
        let PlaceholderState::Failed(why) = w.cover_image_state() else {
            unreachable!()
        };
        assert!(why.contains("could not be read"), "{why}");
        assert_eq!(w.cover_shows(), CoverShows::BlackForImage(why));
        assert_eq!(w.placeholder_state(), PlaceholderState::Black);
        // The first image again: the cover dropped it when another path
        // was asked for, the placeholder kept it, so it is shared at once.
        w.set_placeholder_image(Some(&png));
        assert_eq!(w.placeholder_state(), PlaceholderState::Image);
        w.set_cover_image(Some(&png));
        assert_eq!(w.cover_image_state(), PlaceholderState::Image);
        assert_eq!(cover(), shown);
        // Black at once, and the reason gone.
        w.set_cover_image(None);
        assert_eq!(w.cover_image_state(), PlaceholderState::Black);
        assert!(cover().is_none());
        assert_eq!(w.cover_shows(), CoverShows::Black);
        // A cover decoded first is shared with the placeholder.
        let other = dir.path().join("other.png");
        std::fs::copy(&png, &other).unwrap();
        w.set_cover_image(Some(&other));
        wait_until(Duration::from_secs(10), "the cover image decoded", || {
            w.cover_image_state() == PlaceholderState::Image
        });
        w.set_placeholder_image(Some(&other));
        assert_eq!(w.placeholder_state(), PlaceholderState::Image);
        assert_eq!(cover(), Some(placeholder()));
        assert_ne!(cover(), shown, "another file");
    }

    /// A start with the placeholder's image still being decoded waits for
    /// it before its first placeholder: the first frame at the sink is the
    /// image, never black. Past [`WriterConfig::image_wait`] it starts
    /// black and shows the image once ready; an image that cannot be read
    /// ends the wait at once.
    #[test]
    fn a_start_waits_for_its_placeholder_image_before_the_first_placeholder() {
        require_elements(ELEMENTS);
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let png = white_png(dir.path(), 1920, 1080);
        let start = |wait: Duration, image: &Path| {
            let mut cfg = WriterConfig::new(small(OutputFormat::Nv12), WriterSink::Fake);
            cfg.image_wait = wait;
            let mut w = LoopbackWriter::build_with(cfg).unwrap();
            let log = watch_sink(&w);
            // Pending until the test opens the gate, however fast the
            // decode would end.
            let gate = w.gate_decodes();
            w.set_placeholder_image(Some(image));
            assert_eq!(w.placeholder_state(), PlaceholderState::Pending);
            w.start().unwrap();
            (w, log, gate)
        };
        let buffers = |log: &Mutex<SinkLog>, n: usize| {
            wait_until(Duration::from_secs(10), "buffers at the sink", || {
                log.lock().unwrap().buffers.len() >= n
            });
            log.lock().unwrap().buffers.clone()
        };
        // The default bound: the image from the first frame on.
        let (mut w, log, gate) = start(WriterConfig::IMAGE_WAIT, &png);
        gate.open();
        let seen = buffers(&log, 5);
        assert!(seen.iter().all(|b| b.white), "{seen:?}");
        assert_eq!(w.placeholder_state(), PlaceholderState::Image);
        w.stop().unwrap();
        // No wait: black first, the image once decoded.
        let (mut w, log, gate) = start(Duration::ZERO, &png);
        assert!(buffers(&log, 1)[0].black);
        gate.open();
        wait_until(Duration::from_secs(10), "the image", || {
            log.lock().unwrap().buffers.iter().any(|b| b.white)
        });
        w.stop().unwrap();
        // Unreadable: the wait ends with the failure, black.
        let (mut w, log, gate) = start(Duration::from_secs(60), &dir.path().join("gone.png"));
        assert!(log.lock().unwrap().buffers.is_empty(), "no wait");
        gate.open();
        assert!(buffers(&log, 3).iter().all(|b| b.black));
        assert!(matches!(w.placeholder_state(), PlaceholderState::Failed(_)));
        w.stop().unwrap();
    }

    /// An output forwarding to the writer leaves its placeholder slots to
    /// the writer while its own decode of the image is pending (from its
    /// start): the writer's placeholder, already the image, fills them, so
    /// no black frame reaches the sink. Then the output's own image, then
    /// the writer's again after the stop. System memory, CPU only.
    #[test]
    fn a_writer_output_leaves_its_slots_to_the_writer_while_its_image_is_decoded() {
        require_elements(ELEMENTS);
        require_elements(IMAGE_ELEMENTS);
        let dir = tempfile::tempdir().unwrap();
        let png = white_png(dir.path(), 3840, 2160);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        w.set_placeholder_image(Some(&png));
        let log = watch_sink(&w);
        w.start().unwrap();
        wait_until(Duration::from_secs(20), "the writer's image", || {
            w.placeholder_state() == PlaceholderState::Image
        });
        let mut cfg = OutputConfig::new(contract, OutputSink::Writer(w.handle()));
        cfg.placeholder_image = Some(png.clone());
        let mut o = Output::build_in(cfg, VideoMemory::System).unwrap();
        o.start().unwrap();
        wait_until(Duration::from_secs(20), "the output's image", || {
            o.placeholder_state() == PlaceholderState::Image
        });
        wait_until(Duration::from_secs(10), "forwarded frames", || {
            w.counts().forwarded >= 5
        });
        o.stop().unwrap();
        let c = w.counts();
        wait_until(Duration::from_secs(10), "the placeholder again", || {
            w.counts().placeholder >= c.placeholder + 3
        });
        w.stop().unwrap();
        let log = log.lock().unwrap();
        let black = log.buffers.iter().filter(|b| b.black).count();
        assert_eq!(black, 0, "{} buffers, {black} black", log.buffers.len());
        assert!(log.buffers.iter().all(|b| b.white));
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
    /// the k-th has a PTS of at least k periods after the start. One caps
    /// negotiation.
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
                // Never faster than the contract rate. Measured from the
                // start, not from frame #0: the loop schedules on a grid
                // from its thread's start but stamps the push time, so a
                // late push (#0 most often) is followed by a shorter gap.
                // The k-th is planned at least k periods after the thread
                // started (a resync only moves the grid later), which is
                // after `mono_start`, and its PTS is not before its push:
                // exact, no tolerance.
                assert!(
                    b.pts >= mono_start + k as u64 * period,
                    "{format} #{k}: pts {} less than {k} periods after the start {mono_start}",
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

    /// Memory that records, when it is freed, whether the freeing thread
    /// was free of the push lock. The lock is not reentrant: held by the
    /// freeing thread, it stays taken for the whole retry window; held by
    /// another (the placeholder thread pushing meanwhile), it frees soon.
    struct DropProbe {
        data: Vec<u8>,
        shared: std::sync::Weak<Shared>,
        unlocked: Arc<Mutex<Vec<bool>>>,
    }

    impl AsRef<[u8]> for DropProbe {
        fn as_ref(&self) -> &[u8] {
            &self.data
        }
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            let unlocked = self.shared.upgrade().is_none_or(|s| {
                let until = Instant::now() + Duration::from_millis(500);
                loop {
                    if s.state.try_lock().is_ok() {
                        break true;
                    }
                    if Instant::now() >= until {
                        break false;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            self.unlocked.lock().unwrap().push(unlocked);
        }
    }

    /// A refused forwarded frame is freed after the push lock is released:
    /// by a writer not running, and by `appsrc` (here, a pipeline set to
    /// NULL behind a running writer) with a tap and an output attached,
    /// whose references are the last ones.
    #[test]
    fn a_refused_forwarded_frame_is_freed_outside_the_push_lock() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut cfg = WriterConfig::new(contract, WriterSink::Fake);
        cfg.placeholder_after = Duration::from_secs(10);
        let mut w = LoopbackWriter::build_with(cfg).unwrap();
        let h = w.handle();
        let unlocked: Arc<Mutex<Vec<bool>>> = Arc::default();
        let shared = Arc::downgrade(&w.shared);
        let frame = || {
            let black = black_contract_frame(&contract).unwrap();
            let data = black.map_readable().unwrap().to_vec();
            gst::Buffer::from_slice(DropProbe {
                data,
                shared: shared.clone(),
                unlocked: Arc::clone(&unlocked),
            })
        };
        assert!(!h.push(frame()), "a writer not started refuses");
        assert_eq!(*unlocked.lock().unwrap(), [true]);

        w.start().unwrap();
        w.set_tap(Some(Arc::new(|_: &gst::Buffer| {})));
        let attached = h.attach(Duration::from_secs(10));
        w.pipeline().pipeline().set_state(gst::State::Null).unwrap();
        assert!(!h.push(frame()), "appsrc refuses once flushing");
        assert_eq!(*unlocked.lock().unwrap(), [true, true]);
        drop(attached);
        w.stop().unwrap();
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

    /// Unattached, the writer never holds: the placeholder follows the last
    /// forwarded frame. Attached, the last forwarded frame (its very memory,
    /// at the tap) fills the placeholder's slots, until the hold bound has
    /// passed or the output detaches; the placeholder then resumes.
    #[test]
    fn an_attached_output_has_its_last_frame_held_not_the_placeholder() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen: Arc<Mutex<Vec<usize>>> = Arc::default();
        let s = Arc::clone(&seen);
        w.set_tap(Some(Arc::new(move |b: &gst::Buffer| {
            s.lock()
                .unwrap()
                .push(b.map_readable().unwrap().as_ptr() as usize);
        })));
        let h = w.handle();
        let frame = || black_contract_frame(&contract).unwrap();
        let address = |b: &gst::Buffer| b.map_readable().unwrap().as_ptr() as usize;
        // Every frame forwarded stays alive, so no two share an address.
        let mut alive = Vec::new();
        // Forwards three frames, 5 ms apart; the last one's memory.
        let mut forward = || {
            let mut last = 0;
            for _ in 0..3 {
                let b = frame();
                last = address(&b);
                assert!(h.push(b.clone()));
                alive.push(b);
                std::thread::sleep(Duration::from_millis(5));
            }
            last
        };
        w.start().unwrap();
        wait_until(Duration::from_secs(10), "placeholders", || {
            w.counts().placeholder >= 2
        });

        // Unattached: as before any attach.
        let c0 = w.counts();
        forward();
        wait_until(Duration::from_secs(10), "the placeholder again", || {
            w.counts().placeholder >= c0.placeholder + 3
        });
        assert_eq!(w.counts().held, 0, "{:?}", w.counts());

        // Attached: the last frame, while within the hold.
        let attachment = h.attach(Duration::from_secs(2));
        let last = forward();
        let c1 = w.counts();
        wait_until(Duration::from_secs(1), "held frames", || {
            w.counts().held >= c1.held + 3
        });
        let c2 = w.counts();
        assert_eq!(c2.placeholder, c1.placeholder, "{c1:?} -> {c2:?}");
        let tapped = seen.lock().unwrap().clone();
        let first = tapped.iter().position(|&a| a == last).unwrap();
        let held = &tapped[first + 1..];
        assert!(held.len() >= 3, "{held:x?}");
        assert!(
            held.iter().all(|&a| a == last),
            "the held memory is not the last forwarded frame's: {last:#x} {held:x?}"
        );

        // Past the hold: the placeholder resumes, and nothing is held.
        wait_until(
            Duration::from_secs(10),
            "the placeholder past the hold",
            || w.counts().placeholder >= c2.placeholder + 3,
        );
        let c3 = w.counts();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(w.counts().held, c3.held, "held past the hold");

        // Detached: the placeholder at once (after `placeholder_after`),
        // never the kept frame.
        let c4 = w.counts();
        forward();
        drop(attachment);
        wait_until(
            Duration::from_secs(1),
            "the placeholder after a detach",
            || w.counts().placeholder >= c4.placeholder + 3,
        );
        let c5 = w.counts();
        assert_eq!(c5.held, c4.held, "held after a detach: {c4:?} -> {c5:?}");
        w.stop().unwrap();
    }

    /// A buffer the tap saw: when, its memory, its PTS.
    #[derive(Debug, Clone, Copy)]
    struct TapEntry {
        at: Instant,
        data: usize,
        pts: u64,
    }

    /// Installs a tap recording every buffer `appsrc` accepted.
    fn record_tap(w: &mut LoopbackWriter) -> Arc<Mutex<Vec<TapEntry>>> {
        let seen: Arc<Mutex<Vec<TapEntry>>> = Arc::default();
        let s = Arc::clone(&seen);
        w.set_tap(Some(Arc::new(move |b: &gst::Buffer| {
            let entry = TapEntry {
                at: Instant::now(),
                data: b.map_readable().unwrap().as_ptr() as usize,
                pts: b.pts().map(|p| p.nseconds()).unwrap_or(u64::MAX),
            };
            s.lock().unwrap().push(entry);
        })));
        seen
    }

    fn assert_pts_increasing(seen: &[TapEntry]) {
        for pair in seen.windows(2) {
            assert!(
                pair[1].pts > pair[0].pts,
                "PTS not strictly increasing at the tap: {pair:?}"
            );
        }
    }

    /// Detached in the middle of a hold (the camera turned off during a
    /// switch): the placeholder takes over within `placeholder_after` plus
    /// a period (the last forwarded frame is long past `placeholder_after`,
    /// so within a period), and nothing is held after the detach. The
    /// upper bound on time is generous (docs/testing.md); the measured
    /// delay is printed.
    #[test]
    fn a_detach_during_a_hold_resumes_the_placeholder() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let h = w.handle();
        w.start().unwrap();
        wait_until(Duration::from_secs(10), "placeholders", || {
            w.counts().placeholder >= 2
        });
        let attachment = h.attach(Duration::from_secs(60));
        let frame = black_contract_frame(&contract).unwrap();
        let forwarded = frame.map_readable().unwrap().as_ptr() as usize;
        assert!(h.push(frame.clone()));
        wait_until(Duration::from_secs(10), "held frames", || {
            w.counts().held >= 3
        });

        drop(attachment);
        let detached = Instant::now();
        assert!(h.attached_holds().is_empty());
        let at_detach = w.counts();
        wait_until(
            Duration::from_secs(10),
            "the placeholder after the detach",
            || w.counts().placeholder >= at_detach.placeholder + 3,
        );
        std::thread::sleep(Duration::from_millis(100));
        let end = w.counts();
        w.stop().unwrap();
        assert_eq!(
            end.held, at_detach.held,
            "held after the detach: {at_detach:?} -> {end:?}"
        );
        let seen = seen.lock().unwrap().clone();
        let after: Vec<&TapEntry> = seen.iter().filter(|t| t.at >= detached).collect();
        assert!(
            after.iter().all(|t| t.data != forwarded),
            "the kept frame after the detach: {after:?}"
        );
        let first = after.first().expect("a placeholder after the detach");
        let delay = first.at - detached;
        println!("placeholder {delay:?} after the detach");
        let period = Duration::from_nanos(contract.frame_duration_ns());
        assert!(
            delay < w.cfg.placeholder_after + period + Duration::from_secs(1),
            "the placeholder {delay:?} after the detach"
        );
        assert_pts_increasing(&seen);
    }

    /// The hold lasts its bound: from the last forwarded frame, only that
    /// frame (held, PTS strictly increasing) until the bound has passed,
    /// then the placeholder, and nothing held after it. The first
    /// placeholder never comes before the bound (it cannot: the push
    /// checks the bound under its lock); its upper bound is generous
    /// (docs/testing.md), the measured hold is printed.
    #[test]
    fn a_hold_lasts_its_bound_then_the_placeholder_resumes() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let period = Duration::from_nanos(contract.frame_duration_ns());
        let bound = Duration::from_millis(400);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let h = w.handle();
        w.start().unwrap();
        wait_until(Duration::from_secs(10), "placeholders", || {
            w.counts().placeholder >= 2
        });
        let _attachment = h.attach(bound);
        let frame = black_contract_frame(&contract).unwrap();
        let forwarded = frame.map_readable().unwrap().as_ptr() as usize;
        let before = Instant::now();
        assert!(h.push(frame.clone()));
        let pushed = Instant::now();
        let c0 = w.counts();
        wait_until(
            Duration::from_secs(10),
            "the placeholder past the bound",
            || w.counts().placeholder > c0.placeholder,
        );
        std::thread::sleep(Duration::from_millis(200));
        let end = w.counts();
        w.stop().unwrap();

        let seen = seen.lock().unwrap().clone();
        let start = seen.iter().position(|t| t.data == forwarded).unwrap();
        let tail = &seen[start..];
        let resumed = tail.iter().position(|t| t.data != forwarded).unwrap();
        let (held, after) = tail.split_at(resumed);
        let held = &held[1..];
        assert!(!held.is_empty(), "nothing held: {:?}", w.counts());
        assert_eq!(held.len() as u64, end.held - c0.held, "{c0:?} -> {end:?}");
        assert!(
            after.iter().all(|t| t.data != forwarded),
            "held after the placeholder resumed: {after:?}"
        );
        assert_pts_increasing(tail);
        assert_pts_increasing(&seen);

        let first = after[0].at;
        println!(
            "held {} frames, the placeholder {:?} after the frame",
            held.len(),
            first - pushed
        );
        assert!(
            first >= before + bound - period,
            "the placeholder {:?} after the frame, within the bound {bound:?}",
            first - before
        );
        assert!(
            first < pushed + bound + Duration::from_secs(2),
            "the placeholder {:?} after the frame, bound {bound:?}",
            first - pushed
        );
    }

    /// Within the window of `hold_across_restart`, the last detach keeps
    /// the last forwarded frame held (its very memory, no placeholder)
    /// until the window ends; the placeholder then resumes, and nothing is
    /// held after it. The window counts from the call. Bounds on time are
    /// lower bounds, or generous upper ones (docs/testing.md).
    #[test]
    fn a_restart_hold_keeps_the_last_frame_until_its_bound() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let bound = Duration::from_millis(500);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let h = w.handle();
        w.start().unwrap();
        wait_until(Duration::from_secs(10), "placeholders", || {
            w.counts().placeholder >= 2
        });
        let attachment = h.attach(Duration::from_secs(60));
        let frame = black_contract_frame(&contract).unwrap();
        let forwarded = frame.map_readable().unwrap().as_ptr() as usize;
        assert!(h.push(frame.clone()));
        wait_until(Duration::from_secs(10), "held frames", || {
            w.counts().held >= 2
        });
        let called = Instant::now();
        w.hold_across_restart(bound);
        drop(attachment);
        let detached = w.counts();
        wait_until(Duration::from_secs(10), "held after the detach", || {
            w.counts().held >= detached.held + 3
        });
        wait_until(
            Duration::from_secs(10),
            "the placeholder past the bound",
            || w.counts().placeholder >= detached.placeholder + 3,
        );
        let resumed = w.counts();
        std::thread::sleep(Duration::from_millis(100));
        let end = w.counts();
        w.stop().unwrap();
        assert_eq!(end.held, resumed.held, "held past the bound");

        let seen = seen.lock().unwrap().clone();
        let after: Vec<&TapEntry> = seen.iter().filter(|t| t.at >= called).collect();
        let first_placeholder = after
            .iter()
            .position(|t| t.data != forwarded)
            .expect("the placeholder after the bound");
        assert!(first_placeholder >= 3, "{after:?}");
        assert!(
            after[first_placeholder..]
                .iter()
                .all(|t| t.data != forwarded),
            "held after the placeholder resumed"
        );
        let period = Duration::from_nanos(contract.frame_duration_ns());
        let at = after[first_placeholder].at - called;
        println!("the placeholder {at:?} after the call, bound {bound:?}");
        assert!(
            at >= bound - period,
            "the placeholder {at:?} after the call"
        );
        assert!(at < bound + Duration::from_secs(2), "{at:?}");
        assert_pts_increasing(&seen);
    }

    /// Waits for three more placeholders, then checks that nothing more
    /// is held.
    fn held_then_placeholder(w: &LoopbackWriter) {
        let c = w.counts();
        wait_until(Duration::from_secs(10), "the placeholder", || {
            w.counts().placeholder >= c.placeholder + 3
        });
        let c = w.counts();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(w.counts().held, c.held, "held: {c:?} -> {:?}", w.counts());
    }

    /// Waits for three more held frames, with no placeholder meanwhile.
    fn still_held(w: &LoopbackWriter) {
        let c = w.counts();
        wait_until(Duration::from_secs(10), "held frames", || {
            w.counts().held >= c.held + 3
        });
        assert_eq!(
            w.counts().placeholder,
            c.placeholder,
            "{c:?} -> {:?}",
            w.counts()
        );
    }

    /// A restart hold is in force from the call: the stopping output's
    /// frames (still attached, its own bound zero) are kept and do not end
    /// it, and its placeholder slots are held. It lasts past the next
    /// attach (also of bound zero), until the attached output forwards its
    /// first frame: from then on the attached rules apply (with a zero
    /// bound, nothing is held), the kept frame's memory is never pushed
    /// again, and the window is used once. A detach after the window has
    /// passed shows the placeholder at once, as without the call.
    #[test]
    fn a_restart_hold_lasts_past_the_attach_until_the_first_forwarded_frame() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let h = w.handle();
        w.start().unwrap();
        wait_until(Duration::from_secs(10), "placeholders", || {
            w.counts().placeholder >= 2
        });
        let attachment = h.attach(Duration::ZERO);
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        assert!(!h.restart_holding(), "held before the restart");
        w.hold_across_restart(Duration::from_secs(60));
        assert!(h.restart_holding(), "not held from the call");
        still_held(&w);
        // A frame of the stopping output, still attached: kept, the hold
        // goes on.
        let old = black_contract_frame(&contract).unwrap();
        let old_data = old.map_readable().unwrap().as_ptr() as usize;
        let pushed = Instant::now();
        assert!(h.push(old));
        assert!(h.restart_holding(), "ended by the stopping output");
        still_held(&w);
        let tapped = seen.lock().unwrap().clone();
        assert!(
            tapped
                .iter()
                .filter(|t| t.at >= pushed)
                .all(|t| t.data == old_data),
            "not the stopping output's last frame: {tapped:?}"
        );
        drop(attachment);
        assert!(h.restart_holding());
        still_held(&w);
        // The new output attaches: the restart hold goes on.
        let next = h.attach(Duration::ZERO);
        assert!(h.restart_holding(), "ended by the attach");
        still_held(&w);
        // Its first frame ends it.
        let first = black_contract_frame(&contract).unwrap();
        let first_data = first.map_readable().unwrap().as_ptr() as usize;
        let forwarded = Instant::now();
        assert!(h.push(first));
        assert!(!h.restart_holding(), "held past the first frame");
        held_then_placeholder(&w);
        // Before any later buffer could reuse the old frame's memory.
        let tapped = seen.lock().unwrap().clone();
        let after: Vec<&TapEntry> = tapped.iter().filter(|t| t.at >= forwarded).collect();
        assert_eq!(after.first().map(|t| t.data), Some(first_data), "{after:?}");
        assert!(
            after.iter().all(|t| t.data != old_data),
            "the old frame after the new one: {after:?}"
        );
        // The window was used: the detach drops the kept frame.
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        drop(next);
        held_then_placeholder(&w);

        // A window that passed before the detach: the placeholder at once.
        let attachment = h.attach(Duration::from_secs(60));
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        w.hold_across_restart(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(60));
        drop(attachment);
        assert!(!h.restart_holding());
        held_then_placeholder(&w);
        w.stop().unwrap();
        assert_pts_increasing(&seen.lock().unwrap());
    }

    /// A restart hold's bound ends it once the new output is attached too
    /// (it never forwards a frame: a wedged session), whatever that
    /// output's own bound: the placeholder then resumes and nothing more
    /// is held. Bounds on time are lower bounds,
    /// or generous upper ones (docs/testing.md).
    #[test]
    fn the_bound_ends_a_restart_hold_carried_past_the_attach() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let bound = Duration::from_millis(500);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let h = w.handle();
        w.start().unwrap();
        let attachment = h.attach(Duration::from_secs(60));
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        let called = Instant::now();
        w.hold_across_restart(bound);
        drop(attachment);
        let next = h.attach(Duration::from_secs(60));
        still_held(&w);
        let c = w.counts();
        wait_until(
            Duration::from_secs(10),
            "the placeholder past the bound",
            || w.counts().placeholder > c.placeholder,
        );
        let at = called.elapsed();
        assert!(!h.restart_holding(), "held past the bound");
        let period = Duration::from_nanos(contract.frame_duration_ns());
        assert!(
            at >= bound - period,
            "the placeholder {at:?} after the call"
        );
        assert!(at < bound + Duration::from_secs(2), "{at:?}");
        held_then_placeholder(&w);
        drop(next);
        w.stop().unwrap();
    }

    /// A late frame of the stopping output, forwarded after its detach
    /// (no output attached), is shown and held in place of the frame kept
    /// before it, and does not end the restart hold.
    #[test]
    fn a_late_frame_of_the_stopping_output_is_held_and_keeps_the_restart_hold() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let h = w.handle();
        w.start().unwrap();
        let attachment = h.attach(Duration::from_secs(60));
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        w.hold_across_restart(Duration::from_secs(60));
        drop(attachment);
        let late = black_contract_frame(&contract).unwrap();
        let late_data = late.map_readable().unwrap().as_ptr() as usize;
        let pushed = Instant::now();
        assert!(h.push(late));
        assert!(h.restart_holding(), "ended by a late frame");
        still_held(&w);
        let tapped = seen.lock().unwrap().clone();
        let after: Vec<&TapEntry> = tapped.iter().filter(|t| t.at >= pushed).collect();
        assert!(after.len() >= 4, "{after:?}");
        assert!(after.iter().all(|t| t.data == late_data), "{after:?}");
        w.stop().unwrap();
    }

    /// The new output stopped during a restart hold, before its first
    /// frame (the camera turned off): the hold ends and the kept frame is
    /// dropped, so the placeholder follows at once.
    #[test]
    fn a_detach_during_a_restart_hold_ends_it() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let h = w.handle();
        w.start().unwrap();
        let attachment = h.attach(Duration::from_secs(60));
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        w.hold_across_restart(Duration::from_secs(60));
        drop(attachment);
        let next = h.attach(Duration::from_secs(60));
        still_held(&w);
        drop(next);
        assert!(!h.restart_holding(), "held past the new output's stop");
        held_then_placeholder(&w);
        w.stop().unwrap();
    }

    /// `end_restart_hold` during a restart hold (a stop that must hide the
    /// camera before the next output's first frame) ends it at once, with
    /// no output attached or with the new one attached: the kept frame is
    /// dropped (nothing more is held, whatever the attached bound) and the
    /// placeholder resumes. Called with no hold in force, it does nothing
    /// visible.
    #[test]
    fn ending_a_restart_hold_drops_the_kept_frame() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let h = w.handle();
        w.start().unwrap();
        w.end_restart_hold();
        wait_until(Duration::from_secs(10), "placeholders", || {
            w.counts().placeholder >= 2
        });
        // No output attached.
        let attachment = h.attach(Duration::from_secs(60));
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        w.hold_across_restart(Duration::from_secs(60));
        drop(attachment);
        still_held(&w);
        w.end_restart_hold();
        assert!(!h.restart_holding(), "held past the end");
        held_then_placeholder(&w);
        // The new output attached, before its first frame.
        let attachment = h.attach(Duration::from_secs(60));
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        w.hold_across_restart(Duration::from_secs(60));
        drop(attachment);
        let next = h.attach(Duration::from_secs(60));
        still_held(&w);
        w.end_restart_hold();
        assert!(!h.restart_holding(), "held past the end");
        held_then_placeholder(&w);
        // The window went with it: the next output's detach holds nothing.
        drop(next);
        held_then_placeholder(&w);
        w.stop().unwrap();
    }

    /// A second `hold_across_restart` while a restart hold is in force,
    /// the new output attached before its first frame (a second module
    /// toggle): that output is now the stopping one, its frames kept
    /// without ending the hold, which lasts from the second call's bound
    /// past the first's, through its detach, until the next output's
    /// first frame.
    #[test]
    fn a_second_restart_during_a_restart_hold_carries_it_on() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let first_bound = Duration::from_millis(300);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let h = w.handle();
        w.start().unwrap();
        let attachment = h.attach(Duration::ZERO);
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        w.hold_across_restart(first_bound);
        drop(attachment);
        let second = h.attach(Duration::ZERO);
        w.hold_across_restart(Duration::from_secs(60));
        assert!(h.restart_holding());
        // Its frame (the second output, stopping now) does not end it.
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        assert!(h.restart_holding(), "ended by the stopping output");
        std::thread::sleep(first_bound * 2);
        assert!(h.restart_holding(), "ended by the first call's bound");
        still_held(&w);
        drop(second);
        assert!(h.restart_holding(), "ended by the stopping output's detach");
        still_held(&w);
        let third = h.attach(Duration::ZERO);
        assert!(h.restart_holding(), "ended by the attach");
        assert!(h.push(black_contract_frame(&contract).unwrap()));
        assert!(!h.restart_holding(), "held past the first frame");
        held_then_placeholder(&w);
        drop(third);
        held_then_placeholder(&w);
        w.stop().unwrap();
        assert_pts_increasing(&seen.lock().unwrap());
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
            VideoMemory::System,
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
                VideoMemory::System,
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
            VideoMemory::System,
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

    /// Feeds without a standby (see the module docs, "Feeds and standby"):
    /// every handle, and every clone, pushes as before, and nothing is
    /// refused. A standby feed's pushes are refused, counted in
    /// `standby_refused` and not `push_failed`, and change nothing else;
    /// promoted, its pushes are accepted and the old feed's refused, and
    /// the writer hands out handles of the promoted feed. Promoting again
    /// turns back.
    #[test]
    fn a_standby_feed_is_refused_until_promoted_then_the_old_one_is() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let h = w.handle();
        let other = w.handle();
        let frame = || black_contract_frame(&contract).unwrap();
        w.start().unwrap();
        assert!(h.is_current_feed() && other.is_current_feed());
        assert!(h.push(frame()) && other.push(frame()) && h.clone().push(frame()));
        let c0 = w.counts();
        assert_eq!((c0.forwarded, c0.standby_refused), (3, 0), "{c0:?}");

        let standby = h.standby();
        assert_eq!(standby, h, "the same writer");
        assert!(!standby.is_current_feed());
        assert!(!standby.push(frame()), "a standby push was accepted");
        assert!(!standby.clone().push(frame()));
        assert!(h.push(frame()), "the current feed was refused");
        let c1 = w.counts();
        assert_eq!(
            (c1.forwarded, c1.standby_refused, c1.push_failed),
            (c0.forwarded + 1, 2, c0.push_failed),
            "{c1:?}"
        );

        standby.promote();
        assert!(standby.is_current_feed() && !h.is_current_feed());
        assert!(!h.push(frame()) && !other.push(frame()), "the old feed");
        assert!(standby.push(frame()), "the promoted feed was refused");
        assert!(w.handle().is_current_feed(), "a handle of the old feed");
        assert!(w.handle().push(frame()));
        // A second standby feed is refused too, the promoted one goes on.
        let second = standby.standby();
        assert!(!second.push(frame()));
        let c2 = w.counts();
        assert_eq!(
            (c2.forwarded, c2.standby_refused, c2.push_failed),
            (c1.forwarded + 2, c1.standby_refused + 3, c1.push_failed),
            "{c2:?}"
        );
        // Back to the first feed.
        h.promote();
        assert!(h.push(frame()) && !standby.push(frame()));
        w.stop().unwrap();
    }

    /// The promotion is atomic: with an old and a standby feed pushing
    /// distinct frames from two threads, the frames the writer accepted
    /// (at the tap, by memory) are the old feed's up to one push and the
    /// promoted feed's from the next on, never one of the old after one of
    /// the new; neither feed's frame is ever accepted on the wrong side.
    #[test]
    fn a_promotion_falls_between_two_pushes() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let old = w.handle();
        let new = old.standby();
        w.start().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        // Each feed's frames stay alive, so no two share an address.
        let pusher = |h: WriterHandle| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut sent = Vec::new();
                while !stop.load(Ordering::Relaxed) && sent.len() < 400 {
                    let b = black_contract_frame(&contract).unwrap();
                    h.push(b.clone());
                    sent.push(b);
                    std::thread::sleep(Duration::from_millis(1));
                }
                sent
            })
        };
        let (a, b) = (pusher(old.clone()), pusher(new.clone()));
        wait_until(Duration::from_secs(10), "old frames", || {
            w.counts().forwarded >= 20
        });
        new.promote();
        let promoted = w.counts();
        wait_until(Duration::from_secs(10), "new frames", || {
            w.counts().forwarded >= promoted.forwarded + 20
        });
        stop.store(true, Ordering::Relaxed);
        let (old_frames, new_frames) = (a.join().unwrap(), b.join().unwrap());
        w.stop().unwrap();
        let address = |b: &gst::Buffer| b.map_readable().unwrap().as_ptr() as usize;
        let old_set: std::collections::HashSet<usize> = old_frames.iter().map(address).collect();
        let new_set: std::collections::HashSet<usize> = new_frames.iter().map(address).collect();
        let tapped = seen.lock().unwrap().clone();
        let feed: Vec<char> = tapped
            .iter()
            .filter_map(|t| {
                if old_set.contains(&t.data) {
                    Some('o')
                } else if new_set.contains(&t.data) {
                    Some('n')
                } else {
                    None
                }
            })
            .collect();
        let first_new = feed.iter().position(|&c| c == 'n').expect("new frames");
        assert!(first_new >= 20, "new frames before the promotion");
        assert!(
            feed[first_new..].iter().all(|&c| c == 'n'),
            "an old frame after the promotion: {}",
            feed.iter().collect::<String>()
        );
        let end = w.counts();
        assert_eq!(
            end.forwarded as usize,
            feed.len(),
            "accepted frames of neither feed: {end:?}"
        );
        assert!(end.standby_refused > 0 && end.push_failed == 0, "{end:?}");
        assert_pts_increasing(&tapped);
    }

    /// A standby output attaches like any other: once promoted, its first
    /// frame replaces the kept one (the held frames are its memory), the
    /// old output's detach is not the last and keeps it, and nothing of the
    /// standby feed was kept or held before the promotion. The promoted
    /// output's detach, the last, drops the kept frame.
    #[test]
    fn a_promoted_feed_takes_over_the_kept_frame_and_the_old_detach_keeps_it() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let old = w.handle();
        let new = old.standby();
        let data = |b: &gst::Buffer| b.map_readable().unwrap().as_ptr() as usize;
        w.start().unwrap();
        let old_attachment = old.attach(Duration::from_secs(60));
        let old_frame = black_contract_frame(&contract).unwrap();
        let old_data = data(&old_frame);
        assert!(old.push(old_frame));
        let new_attachment = new.attach(Duration::from_secs(60));
        let standby_frame = black_contract_frame(&contract).unwrap();
        let standby_data = data(&standby_frame);
        assert!(!new.push(standby_frame.clone()));
        still_held(&w);
        let tapped = seen.lock().unwrap().clone();
        assert!(tapped.iter().all(|t| t.data != standby_data), "{tapped:?}");
        assert_eq!(tapped.last().map(|t| t.data), Some(old_data));

        new.promote();
        // Promotion alone changes no frame: the old one is still held.
        let promoted = Instant::now();
        still_held(&w);
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|t| t.at >= promoted)
                .all(|t| t.data == old_data)
        );
        let new_frame = black_contract_frame(&contract).unwrap();
        let new_data = data(&new_frame);
        let forwarded = Instant::now();
        assert!(new.push(new_frame));
        drop(old_attachment);
        still_held(&w);
        let after: Vec<usize> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.at >= forwarded)
            .map(|t| t.data)
            .collect();
        assert!(after.len() >= 4, "{after:x?}");
        assert!(after.iter().all(|&a| a == new_data), "{after:x?}");
        drop(new_attachment);
        held_then_placeholder(&w);
        w.stop().unwrap();
        drop(standby_frame);
    }

    /// Promotion is not a restart (see the module docs, "Feeds and
    /// standby"): during a restart hold, a standby feed's frames are
    /// refused and neither end it nor replace the held frame; the
    /// promotion neither ends nor starts one; the promoted feed's first
    /// frame, its output attached, ends it under the attached rules. A
    /// promotion with no restart hold in force starts none.
    #[test]
    fn a_promotion_neither_starts_nor_ends_a_restart_hold() {
        require_elements(ELEMENTS);
        let contract = small(OutputFormat::Nv12);
        let mut w = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
        let seen = record_tap(&mut w);
        let old = w.handle();
        let data = |b: &gst::Buffer| b.map_readable().unwrap().as_ptr() as usize;
        w.start().unwrap();
        let attachment = old.attach(Duration::ZERO);
        let old_frame = black_contract_frame(&contract).unwrap();
        let old_data = data(&old_frame);
        assert!(old.push(old_frame));
        w.hold_across_restart(Duration::from_secs(60));
        drop(attachment);
        assert!(old.restart_holding());

        let new = old.standby();
        let next = new.attach(Duration::ZERO);
        let refused = Instant::now();
        assert!(!new.push(black_contract_frame(&contract).unwrap()));
        assert!(new.restart_holding(), "ended by a refused frame");
        still_held(&w);
        new.promote();
        assert!(new.restart_holding(), "ended by the promotion");
        still_held(&w);
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|t| t.at >= refused)
                .all(|t| t.data == old_data),
            "not the old output's frame"
        );
        assert!(new.push(black_contract_frame(&contract).unwrap()));
        assert!(!new.restart_holding(), "held past the promoted first frame");
        held_then_placeholder(&w);

        // No restart hold: a promotion starts none.
        let third = new.standby();
        third.promote();
        assert!(!third.restart_holding());
        drop(next);
        held_then_placeholder(&w);
        w.stop().unwrap();
    }
}
