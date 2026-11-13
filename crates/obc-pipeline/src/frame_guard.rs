//! The camera frame guard: corrupt camera frames are dropped at the source,
//! before anything downstream sees them.
//!
//! `v4l2src` (gst-plugins-good 1.28) passes on a buffer the driver dequeued
//! with `V4L2_BUF_FLAG_ERROR`, with [`gst::BufferFlags::CORRUPTED`] set. It
//! drops a raw buffer only when it is shorter than the frame, and it never
//! checks the size of an MJPEG buffer: a JPEG the camera truncated (a USB
//! payload lost, `uvcvideo`'s default `nodrop=1` keeping the incomplete
//! frame) reaches `jpegdec`, which shows it torn or shifted. The guard is a
//! buffer probe on the camera source's `src` pad ([`install`]) that drops:
//!
//! * every buffer flagged corrupted, in any format;
//! * in MJPEG mode, a buffer that does not start with the JPEG start of
//!   image marker (SOI, `FF D8`), or has no end of image marker (EOI,
//!   `FF D9`) within its last [`JPEG_TAIL_WINDOW`] bytes ([`judge_frame`]).
//!
//! A dropped frame is simply missing: the output repeats the last good
//! frame, as for any frame the camera did not deliver.
//!
//! **Several probes on one pad.** [`crate::live::LiveStats`] also probes
//! the camera's `src` pad and keeps every frame it sees waiting for its
//! output tick, so it must never see a dropped frame. GStreamer calls a
//! pad's probes in the order they were added, and once one returns
//! `GST_PAD_PROBE_DROP` it calls none of the later ones for that buffer
//! (`probe_hook_marshal`: "probe previously returned DROPPED, not calling
//! again"). The guard is installed when the ingest is built
//! ([`crate::pipeline::camera_head`]), always before
//! [`crate::live::LiveStats::attach`] can reach the built ingest, so it runs
//! first and the statistics never record a dropped frame (tested).
//!
//! **Cameras without EOI.** Some cameras end their JPEGs without the EOI
//! marker (decoders accept that with a warning). For such a camera the EOI
//! check would drop every frame, so the guard gives it up when the first
//! [`EOI_GRACE`] MJPEG frames of a stream all lack it, and says so once.
//! A camera that has ended one frame with EOI keeps the check.
//!
//! **Cameras that flag every frame.** In the same way, a camera (or a USB
//! path) that sets `UVC_STREAM_ERR` on every payload would have every frame
//! flagged, and show nothing but the placeholder. When the first
//! [`FLAGGED_GRACE`] frames the guard drops for the flag come before any
//! frame of the stream has passed, the guard stops dropping flagged frames
//! for that stream, and says so once; they are then judged like any other
//! (an MJPEG one must still be a whole JPEG). A stream that has passed one
//! frame keeps dropping flagged frames: its errors are real and transient.
//! A camera that needs both grace periods shows the placeholder for at
//! most `FLAGGED_GRACE + EOI_GRACE` frames, one second at 30 fps.
//!
//! **Counts and logs.** The drops are counted per cause
//! ([`CorruptFrames`], [`crate::Ingest::corrupt_frames`]). The first drop is
//! logged at once; later ones are summarised at most once every
//! [`LOG_INTERVAL`], never one line per frame. Drops not yet summarised
//! when the ingest stops are summarised then ([`FrameGuard::flush_log`],
//! also when the guard is dropped).
//!
//! **Delivered rate.** The guard also times the frames it passes, from
//! their PTS ([`crate::delivered_rate`], [`FrameGuard::delivered_fps`],
//! [`crate::Ingest::delivered_fps`]): the rate the camera really delivers
//! good frames at, which can be below its nominal rate.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;

use crate::delivered_rate::DeliveredRate;

/// How far from its end a JPEG's EOI marker may sit. A UVC camera may pad a
/// frame after the marker (zero bytes, or a payload's alignment); 1 KiB
/// covers the padding seen in practice many times over and costs nothing
/// to scan. A truncated JPEG still fails the check: within the
/// entropy-coded data every `FF` byte is stuffed (`FF 00`) or a restart
/// marker (`FF D0`-`FF D7`), so `FF D9` near the end of a frame cut short
/// does not occur. Only an embedded thumbnail's EOI, at the start of a
/// frame smaller than the window, could pass it.
pub const JPEG_TAIL_WINDOW: usize = 1024;

/// MJPEG frames without an EOI, from the start of a stream and with none
/// ever having one, after which the guard stops requiring EOI (see the
/// module docs): half a second of a 30 fps camera. Long enough that the
/// incomplete frames a camera may send right after it starts streaming (a
/// few, in practice) do not turn the check off, short enough that a camera
/// that never sends EOI is not black for seconds at every start.
pub const EOI_GRACE: u64 = 15;

/// Frames flagged corrupted, from the start of a stream and before any
/// frame has passed, after which the guard stops dropping flagged frames
/// (see the module docs). The same as [`EOI_GRACE`], for the same reasons:
/// a start-up burst of errors is a few frames, a camera that flags every
/// frame costs half a second of placeholder.
pub const FLAGGED_GRACE: u64 = EOI_GRACE;

/// The shortest interval between two log lines about dropped frames.
pub const LOG_INTERVAL: Duration = Duration::from_secs(10);

/// What the guard does with one camera buffer ([`judge_frame`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameVerdict {
    /// Passed downstream.
    Pass,
    /// Dropped: flagged [`gst::BufferFlags::CORRUPTED`] by the source.
    Flagged,
    /// Dropped: an MJPEG buffer that does not start with SOI (`FF D8`).
    NoSoi,
    /// Dropped: an MJPEG buffer with no EOI (`FF D9`) within its last
    /// [`JPEG_TAIL_WINDOW`] bytes.
    NoEoi,
}

impl FrameVerdict {
    /// Whether the buffer is dropped.
    pub fn drops(self) -> bool {
        self != FrameVerdict::Pass
    }

    fn reason(self) -> &'static str {
        match self {
            FrameVerdict::Pass => "passed",
            FrameVerdict::Flagged => "flagged corrupted by the driver",
            FrameVerdict::NoSoi => "a JPEG without its start marker",
            FrameVerdict::NoEoi => "a JPEG without its end marker (truncated)",
        }
    }
}

/// The verdict on one camera buffer: `corrupted` its CORRUPTED flag,
/// `bytes` its contents (`None` when it could not be mapped), `mjpeg`
/// whether the camera sends MJPEG, `require_eoi` whether a JPEG must end
/// with EOI (see the module docs). A flagged buffer is dropped in any
/// format; a raw buffer is otherwise never inspected; a JPEG must start
/// with SOI and, with `require_eoi`, have an EOI within its last
/// [`JPEG_TAIL_WINDOW`] bytes. A buffer that cannot be mapped is passed:
/// the guard drops only what it has seen to be corrupt.
pub fn judge_frame(
    corrupted: bool,
    bytes: Option<&[u8]>,
    mjpeg: bool,
    require_eoi: bool,
) -> FrameVerdict {
    if corrupted {
        return FrameVerdict::Flagged;
    }
    if !mjpeg {
        return FrameVerdict::Pass;
    }
    let Some(bytes) = bytes else {
        return FrameVerdict::Pass;
    };
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return FrameVerdict::NoSoi;
    }
    if !require_eoi {
        return FrameVerdict::Pass;
    }
    // After the SOI, so the SOI's own bytes never count.
    let body = &bytes[2..];
    let tail = &body[body.len().saturating_sub(JPEG_TAIL_WINDOW)..];
    if tail.windows(2).any(|w| w == [0xff, 0xd9]) {
        FrameVerdict::Pass
    } else {
        FrameVerdict::NoEoi
    }
}

/// Camera frames the guard dropped, per cause.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CorruptFrames {
    /// Flagged corrupted by the source (`V4L2_BUF_FLAG_ERROR`).
    pub flagged: u64,
    /// MJPEG buffers that are not a whole JPEG (no SOI, or no EOI).
    pub bad_jpeg: u64,
}

impl CorruptFrames {
    /// All dropped frames.
    pub fn total(&self) -> u64 {
        self.flagged + self.bad_jpeg
    }
}

/// The log state of a guard: drops since the last line.
#[derive(Debug, Default)]
struct LogState {
    last: Option<Instant>,
    flagged: u64,
    bad_jpeg: u64,
}

/// The state of one camera's guard, shared by its probe and its ingest.
#[derive(Debug)]
pub struct FrameGuard {
    /// The camera, for the log lines.
    label: String,
    mjpeg: bool,
    flagged: AtomicU64,
    bad_jpeg: AtomicU64,
    /// A JPEG of this stream has ended with EOI.
    eoi_seen: AtomicBool,
    /// The EOI check is on (off after [`EOI_GRACE`]).
    require_eoi: AtomicBool,
    /// MJPEG frames without EOI since the start, while none had one.
    no_eoi_run: AtomicU64,
    /// A frame of this stream has passed.
    passed: AtomicBool,
    /// Flagged frames are dropped (off after [`FLAGGED_GRACE`]).
    drop_flagged: AtomicBool,
    /// Flagged frames since the start, while none had passed.
    flagged_run: AtomicU64,
    log: Mutex<LogState>,
    /// The timestamps of the passed frames, for the delivered rate.
    rate: Mutex<DeliveredRate>,
}

impl FrameGuard {
    /// A guard for a camera (`label`, for the logs) sending MJPEG or not.
    pub fn new(label: impl Into<String>, mjpeg: bool) -> Self {
        FrameGuard {
            label: label.into(),
            mjpeg,
            flagged: AtomicU64::new(0),
            bad_jpeg: AtomicU64::new(0),
            eoi_seen: AtomicBool::new(false),
            require_eoi: AtomicBool::new(true),
            no_eoi_run: AtomicU64::new(0),
            passed: AtomicBool::new(false),
            drop_flagged: AtomicBool::new(true),
            flagged_run: AtomicU64::new(0),
            log: Mutex::new(LogState::default()),
            rate: Mutex::new(DeliveredRate::new()),
        }
    }

    /// The frames dropped so far.
    pub fn counts(&self) -> CorruptFrames {
        CorruptFrames {
            flagged: self.flagged.load(Ordering::Relaxed),
            bad_jpeg: self.bad_jpeg.load(Ordering::Relaxed),
        }
    }

    /// Judges one buffer and records the verdict: whether to drop it.
    /// Panic-free (probe context).
    pub fn check(&self, buffer: &gst::BufferRef) -> bool {
        let corrupted = buffer.flags().contains(gst::BufferFlags::CORRUPTED)
            && self.drop_flagged.load(Ordering::Relaxed);
        let require_eoi = self.require_eoi.load(Ordering::Relaxed);
        let verdict = if corrupted || !self.mjpeg {
            judge_frame(corrupted, None, self.mjpeg, require_eoi)
        } else {
            let map = buffer.map_readable().ok();
            judge_frame(false, map.as_deref(), true, require_eoi)
        };
        let now = Instant::now();
        let drop = self.record(verdict, now);
        if !drop {
            self.rate
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(buffer.pts().map(gst::ClockTime::nseconds), now);
        }
        drop
    }

    /// The rate of the good frames this guard has passed, in frames per
    /// second, measured from their PTS over a sliding window
    /// ([`crate::delivered_rate`]); `None` until the window holds enough
    /// frames, and once the frames have stopped.
    pub fn delivered_fps(&self) -> Option<f64> {
        self.rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fps(Instant::now())
    }

    /// Records `verdict` (at `now`, for the logs): whether to drop.
    fn record(&self, verdict: FrameVerdict, now: Instant) -> bool {
        match verdict {
            FrameVerdict::Pass => {
                self.passed.store(true, Ordering::Relaxed);
                if self.mjpeg && self.require_eoi.load(Ordering::Relaxed) {
                    self.eoi_seen.store(true, Ordering::Relaxed);
                }
                return false;
            }
            FrameVerdict::Flagged => {
                self.flagged.fetch_add(1, Ordering::Relaxed);
                if !self.passed.load(Ordering::Relaxed) {
                    let run = self.flagged_run.fetch_add(1, Ordering::Relaxed) + 1;
                    if run >= FLAGGED_GRACE && self.drop_flagged.swap(false, Ordering::Relaxed) {
                        eprintln!(
                            "warning: camera {}: its first {run} frames were all flagged \
                             corrupted by the driver; flagged frames are no longer dropped",
                            self.label
                        );
                    }
                }
            }
            FrameVerdict::NoSoi => {
                self.bad_jpeg.fetch_add(1, Ordering::Relaxed);
            }
            FrameVerdict::NoEoi => {
                self.bad_jpeg.fetch_add(1, Ordering::Relaxed);
                if !self.eoi_seen.load(Ordering::Relaxed) {
                    let run = self.no_eoi_run.fetch_add(1, Ordering::Relaxed) + 1;
                    if run >= EOI_GRACE && self.require_eoi.swap(false, Ordering::Relaxed) {
                        eprintln!(
                            "camera {}: none of its first {run} JPEG frames ends with an \
                             end-of-image marker; its frames are no longer checked for one",
                            self.label
                        );
                    }
                }
            }
        }
        self.log_drop(verdict, now);
        true
    }

    /// Logs the first drop at once, and later ones as a summary at most
    /// once every [`LOG_INTERVAL`].
    fn log_drop(&self, verdict: FrameVerdict, now: Instant) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        match verdict {
            FrameVerdict::Flagged => log.flagged += 1,
            _ => log.bad_jpeg += 1,
        }
        let Some(last) = log.last else {
            eprintln!(
                "camera {}: dropped a corrupt frame ({}); further drops are summarised \
                 at most every {} s",
                self.label,
                verdict.reason(),
                LOG_INTERVAL.as_secs()
            );
            *log = LogState {
                last: Some(now),
                ..LogState::default()
            };
            return;
        };
        if now.saturating_duration_since(last) < LOG_INTERVAL {
            return;
        }
        if let Some(line) = self.take_summary(&mut log, now) {
            eprintln!("{line}");
        }
    }

    /// The summary of the drops `log` holds since its last line (`None`
    /// when there are none), and `log` reset to start a new interval at
    /// `now`.
    fn take_summary(&self, log: &mut LogState, now: Instant) -> Option<String> {
        let last = log.last?;
        let dropped = log.flagged + log.bad_jpeg;
        if dropped == 0 {
            return None;
        }
        let line = format!(
            "camera {}: dropped {dropped} corrupt frames in the last {} s ({} flagged by the \
             driver, {} incomplete JPEGs); {} since the start",
            self.label,
            now.saturating_duration_since(last).as_secs(),
            log.flagged,
            log.bad_jpeg,
            self.counts().total()
        );
        *log = LogState {
            last: Some(now),
            ..LogState::default()
        };
        Some(line)
    }

    /// The summary of the drops not yet logged, if any (taking them: a
    /// second call returns `None`).
    fn pending_summary(&self, now: Instant) -> Option<String> {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        self.take_summary(&mut log, now)
    }

    /// Logs the drops not yet summarised, if any: called when the ingest
    /// stops, and when the guard is dropped, so the last counts are never
    /// lost to the [`LOG_INTERVAL`] rate limit.
    pub fn flush_log(&self) {
        if let Some(line) = self.pending_summary(Instant::now()) {
            eprintln!("{line}");
        }
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        self.flush_log();
    }
}

/// Installs the guard on `src`'s `src` pad (the camera source; see the
/// module docs) for a camera sending MJPEG or not, and returns its state.
/// Install it before any other buffer probe on that pad: the guard must run
/// first. A source without a static `src` pad gets no probe (the counts
/// stay zero).
pub fn install(src: &gst::Element, label: impl Into<String>, mjpeg: bool) -> Arc<FrameGuard> {
    let guard = Arc::new(FrameGuard::new(label, mjpeg));
    if let Some(pad) = src.static_pad("src") {
        let probe = Arc::clone(&guard);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            match info.buffer() {
                Some(buffer) if probe.check(buffer) => gst::PadProbeReturn::Drop,
                _ => gst::PadProbeReturn::Ok,
            }
        });
    }
    guard
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::test_support::require_elements;

    /// A JPEG-shaped buffer: SOI, `body` bytes, EOI, then `padding` zeros.
    fn jpeg(body: usize, padding: usize) -> Vec<u8> {
        let mut b = vec![0xff, 0xd8, 0xff, 0xe0];
        b.extend((0..body).map(|i| (i % 251) as u8 & 0x7f));
        b.extend([0xff, 0xd9]);
        b.extend(std::iter::repeat_n(0u8, padding));
        b
    }

    #[test]
    fn a_flagged_buffer_drops_in_any_format_and_raw_frames_are_never_inspected() {
        let garbage = [0u8; 16];
        for mjpeg in [false, true] {
            assert_eq!(
                judge_frame(true, Some(&jpeg(100, 0)), mjpeg, true),
                FrameVerdict::Flagged
            );
        }
        assert_eq!(
            judge_frame(false, Some(&garbage), false, true),
            FrameVerdict::Pass
        );
        assert_eq!(judge_frame(false, None, false, true), FrameVerdict::Pass);
        // Not mappable: nothing seen to be corrupt.
        assert_eq!(judge_frame(false, None, true, true), FrameVerdict::Pass);
    }

    #[test]
    fn a_jpeg_must_start_with_soi_and_end_with_eoi_within_the_window() {
        let ok = jpeg(5000, 0);
        assert_eq!(
            judge_frame(false, Some(&ok), true, true),
            FrameVerdict::Pass
        );
        // Without SOI.
        assert_eq!(
            judge_frame(false, Some(&ok[2..]), true, true),
            FrameVerdict::NoSoi
        );
        assert_eq!(
            judge_frame(false, Some(&[]), true, true),
            FrameVerdict::NoSoi
        );
        assert_eq!(
            judge_frame(false, Some(&[0xff]), true, true),
            FrameVerdict::NoSoi
        );
        // Truncated: no EOI.
        assert_eq!(
            judge_frame(false, Some(&ok[..3000]), true, true),
            FrameVerdict::NoEoi
        );
        // SOI alone: its bytes are not an EOI either.
        assert_eq!(
            judge_frame(false, Some(&[0xff, 0xd8, 0xd9]), true, true),
            FrameVerdict::NoEoi
        );
        // EOI followed by zero padding, up to the window.
        for padding in [1, 64, JPEG_TAIL_WINDOW - 2] {
            assert_eq!(
                judge_frame(false, Some(&jpeg(5000, padding)), true, true),
                FrameVerdict::Pass,
                "{padding}"
            );
        }
        // Beyond it, the EOI is not looked for.
        assert_eq!(
            judge_frame(false, Some(&jpeg(5000, JPEG_TAIL_WINDOW)), true, true),
            FrameVerdict::NoEoi
        );
        // A small whole frame.
        assert_eq!(
            judge_frame(false, Some(&jpeg(0, 0)), true, true),
            FrameVerdict::Pass
        );
        // Without the EOI check, SOI is enough.
        assert_eq!(
            judge_frame(false, Some(&ok[..3000]), true, false),
            FrameVerdict::Pass
        );
    }

    /// A camera that never ends a JPEG with EOI loses the check after
    /// [`EOI_GRACE`] frames; one that has ended one keeps it.
    #[test]
    fn the_eoi_check_is_given_up_only_for_a_camera_that_never_sends_eoi() {
        let t = Instant::now();
        let never = FrameGuard::new("test", true);
        for _ in 0..EOI_GRACE {
            assert!(never.record(FrameVerdict::NoEoi, t));
        }
        assert!(!never.require_eoi.load(Ordering::Relaxed));
        assert_eq!(never.counts().bad_jpeg, EOI_GRACE);

        let once = FrameGuard::new("test", true);
        assert!(!once.record(FrameVerdict::Pass, t));
        for _ in 0..EOI_GRACE * 2 {
            assert!(once.record(FrameVerdict::NoEoi, t));
        }
        assert!(once.require_eoi.load(Ordering::Relaxed));
    }

    /// A stream whose first [`FLAGGED_GRACE`] frames are all flagged, none
    /// having passed, stops dropping flagged frames (said once); one that
    /// has passed a frame first keeps dropping them.
    #[test]
    fn flagged_frames_pass_only_for_a_stream_that_flags_every_frame() {
        let t = Instant::now();
        for mjpeg in [false, true] {
            let all = FrameGuard::new("test", mjpeg);
            for n in 1..=FLAGGED_GRACE {
                assert!(all.drop_flagged.load(Ordering::Relaxed), "{n}");
                assert!(all.record(FrameVerdict::Flagged, t));
            }
            assert!(!all.drop_flagged.load(Ordering::Relaxed));
            assert_eq!(all.counts().flagged, FLAGGED_GRACE);
            // The switch happens once: later flagged verdicts (none reach
            // `record` through `check` now) do not turn it back on.
            assert!(!all.drop_flagged.swap(false, Ordering::Relaxed));

            let good_first = FrameGuard::new("test", mjpeg);
            assert!(!good_first.record(FrameVerdict::Pass, t));
            for _ in 0..FLAGGED_GRACE * 2 {
                assert!(good_first.record(FrameVerdict::Flagged, t));
            }
            assert!(good_first.drop_flagged.load(Ordering::Relaxed));
            assert_eq!(good_first.flagged_run.load(Ordering::Relaxed), 0);
        }
    }

    /// The drops the rate limit held back are summarised once, by
    /// [`FrameGuard::flush_log`]: none before the first drop, none after
    /// the first (logged at once), the held-back ones then, and none twice.
    #[test]
    fn the_pending_drops_are_summarised_at_stop() {
        let t = Instant::now();
        let g = FrameGuard::new("cam", true);
        assert_eq!(g.pending_summary(t), None);
        assert!(g.record(FrameVerdict::Flagged, t));
        assert_eq!(g.pending_summary(t), None);
        assert!(g.record(FrameVerdict::Flagged, t));
        assert!(g.record(FrameVerdict::NoEoi, t));
        let line = g.pending_summary(t + Duration::from_secs(3)).unwrap();
        assert_eq!(
            line,
            "camera cam: dropped 2 corrupt frames in the last 3 s (1 flagged by the driver, \
             1 incomplete JPEGs); 3 since the start"
        );
        assert_eq!(g.pending_summary(t + Duration::from_secs(4)), None);
    }

    /// On a pipeline: a raw stream flagging every frame shows nothing for
    /// [`FLAGGED_GRACE`] frames, then everything. CPU only.
    #[test]
    fn a_raw_stream_flagging_every_frame_passes_after_the_grace() {
        require_elements(&["appsrc", "fakesink"]);
        let caps = "video/x-raw,format=GRAY8,width=4,height=4,framerate=30/1";
        let total = FLAGGED_GRACE + 5;
        let buffers = (0..total)
            .map(|n| stamped(vec![0u8; 16], n, true))
            .collect();
        let (counts, passed) = push_through_guard(caps, false, buffers);
        assert_eq!(counts.flagged, FLAGGED_GRACE);
        assert_eq!(passed, 5);
    }

    #[test]
    fn drops_are_counted_per_cause() {
        let t = Instant::now();
        let g = FrameGuard::new("test", true);
        for v in [
            FrameVerdict::Flagged,
            FrameVerdict::Pass,
            FrameVerdict::NoSoi,
            FrameVerdict::NoEoi,
            FrameVerdict::Flagged,
        ] {
            assert_eq!(g.record(v, t), v.drops());
        }
        // Later than the log interval: a summary line, counts unchanged.
        assert!(g.record(FrameVerdict::Flagged, t + LOG_INTERVAL));
        assert_eq!(
            g.counts(),
            CorruptFrames {
                flagged: 3,
                bad_jpeg: 2
            }
        );
        assert_eq!(g.counts().total(), 5);
        let log = g.log.lock().unwrap();
        assert_eq!(log.last, Some(t + LOG_INTERVAL));
        assert_eq!((log.flagged, log.bad_jpeg), (0, 0));
    }

    /// Pushes `buffers` through `appsrc (caps) ! fakesink` with the guard
    /// on the source: the guard's counts and the number of buffers that
    /// reached the sink.
    fn push_through_guard(
        caps: &str,
        mjpeg: bool,
        buffers: Vec<gst::Buffer>,
    ) -> (CorruptFrames, u64) {
        let (guard, passed) = run_through_guard(caps, mjpeg, buffers);
        (guard.counts(), passed)
    }

    /// [`push_through_guard`], returning the guard itself.
    fn run_through_guard(
        caps: &str,
        mjpeg: bool,
        buffers: Vec<gst::Buffer>,
    ) -> (Arc<FrameGuard>, u64) {
        require_elements(&["appsrc", "fakesink"]);
        let pipeline = gst::Pipeline::with_name("guard-test");
        let src = gst_app::AppSrc::builder()
            .name(crate::pipeline::names::CAMERA)
            .caps(&caps.parse::<gst::Caps>().unwrap())
            .format(gst::Format::Time)
            .build();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([src.upcast_ref(), &sink]).unwrap();
        src.link(&sink).unwrap();
        let guard = install(src.upcast_ref(), "test", mjpeg);
        let seen = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&seen);
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        pipeline.set_state(gst::State::Playing).unwrap();
        for b in buffers {
            src.push_buffer(b).unwrap();
        }
        src.end_of_stream().unwrap();
        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("EOS or error");
        assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");
        pipeline.set_state(gst::State::Null).unwrap();
        (guard, seen.load(Ordering::Relaxed))
    }

    fn stamped(bytes: Vec<u8>, n: u64, corrupted: bool) -> gst::Buffer {
        let mut b = gst::Buffer::from_slice(bytes);
        let r = b.get_mut().unwrap();
        r.set_pts(gst::ClockTime::from_mseconds(33 * n));
        if corrupted {
            r.set_flags(gst::BufferFlags::CORRUPTED);
        }
        b
    }

    /// On a pipeline: a raw buffer flagged corrupted is dropped and
    /// counted, an unflagged one passes. CPU only.
    #[test]
    fn a_flagged_raw_buffer_is_dropped_and_counted() {
        require_elements(&["appsrc", "fakesink"]);
        let caps = "video/x-raw,format=GRAY8,width=4,height=4,framerate=30/1";
        let buffers = (0..6)
            .map(|n| stamped(vec![0u8; 16], n, n % 3 == 1))
            .collect();
        let (counts, passed) = push_through_guard(caps, false, buffers);
        assert_eq!(
            counts,
            CorruptFrames {
                flagged: 2,
                bad_jpeg: 0
            }
        );
        assert_eq!(passed, 4);
    }

    /// On a pipeline: a JPEG without SOI and one without EOI are dropped;
    /// a whole JPEG, and one whose EOI is followed by zero padding, pass.
    /// CPU only.
    #[test]
    fn incomplete_jpegs_are_dropped_and_padded_ones_pass() {
        require_elements(&["appsrc", "fakesink"]);
        let whole = jpeg(4000, 0);
        let buffers = vec![
            stamped(whole.clone(), 0, false),
            stamped(whole[2..].to_vec(), 1, false),
            stamped(whole[..2500].to_vec(), 2, false),
            stamped(jpeg(4000, 300), 3, false),
            stamped(whole, 4, true),
        ];
        let (counts, passed) = push_through_guard(
            "image/jpeg,width=64,height=64,framerate=30/1",
            true,
            buffers,
        );
        assert_eq!(
            counts,
            CorruptFrames {
                flagged: 1,
                bad_jpeg: 2
            }
        );
        assert_eq!(passed, 2);
    }

    /// On a pipeline: the delivered rate counts only the frames the guard
    /// passes, from their PTS. 4 s of a camera stamping a frame every
    /// 33.3 ms, every third frame flagged corrupted: 20 fps delivered.
    /// CPU only.
    #[test]
    fn the_delivered_rate_counts_only_the_passed_frames() {
        require_elements(&["appsrc", "fakesink"]);
        let caps = "video/x-raw,format=GRAY8,width=4,height=4,framerate=30/1";
        let period = 1_000_000_000 / 30;
        let buffers = (0..(15 + 120 + 1) * 3 / 2)
            .map(|n: u64| {
                let mut b = gst::Buffer::from_slice(vec![0u8; 16]);
                let r = b.get_mut().unwrap();
                r.set_pts(gst::ClockTime::from_nseconds(n * period));
                // Never the first frames: the flag would be forgiven
                // ([`FLAGGED_GRACE`]) before any frame had passed.
                if n % 3 == 2 {
                    r.set_flags(gst::BufferFlags::CORRUPTED);
                }
                b
            })
            .collect();
        let (guard, passed) = run_through_guard(caps, false, buffers);
        assert_eq!(passed + guard.counts().flagged, 204);
        assert_eq!(guard.counts().flagged, 68);
        let fps = guard.delivered_fps().expect("a rate");
        assert!((fps - 20.0).abs() < 0.2, "{fps}");
        // A guard that has passed nothing has no rate.
        assert_eq!(FrameGuard::new("idle", false).delivered_fps(), None);
    }
}
