//! A closed privacy shutter, seen in the camera's frames.
//!
//! Some cameras close their privacy shutter without saying so: the
//! Logitech MX Brio's mechanical shutter sends no HID report and changes no
//! UVC control. What it does change is the picture: closed, its raw frames
//! are perfectly flat at the black level (luma 16 at every pixel, measured
//! in NV12 and YUYV). A camera with a [`crate::quirks::StatusQuirk`] row
//! saying so ([`crate::quirks::flat_black_shutter`]) gets a [`watch`] on
//! its frames, and [`crate::Ingest::shutter`] says whether its shutter is
//! open or closed.
//!
//! **The rule** ([`ShutterDetector`], [`FlatBlack`]). A sampled frame is
//! *flat black* when the luma of a sparse grid of [`GRID_COLUMNS`] x
//! [`GRID_ROWS`] points across it is flat at a black level:
//!
//! * a raw format ([`FlatBlack::Raw`]): a spread (maximum minus minimum)
//!   of [`RAW_SPREAD`] (0: every point equal) and a mean within
//!   [`BLACK_MIN`]..=[`BLACK_MAX`] (the limited-range black level, 16);
//! * decoded MJPEG ([`FlatBlack::DecodedMjpeg`]): either that limited
//!   black with a spread of at most [`DECODED_SPREAD`] (the decoder's
//!   rounding), or the full-range black, a spread of 0 and a mean of at
//!   most [`FULL_BLACK_MAX`]. The decoder (`jpegdec`) puts out full-range
//!   frames, so a closed shutter may well decode to luma 0 rather than
//!   16: the black level of the MX Brio's MJPEG stream with its shutter
//!   closed has not been measured, and both are accepted.
//!
//! The shutter is closed once every frame sampled has been flat black for
//! [`CLOSED_AFTER`], and open again at the first sampled frame that is
//! not. A dark room is never taken for it: its frames are dark, but a
//! sensor's noise spreads their luma by a few levels, and a frame flat at
//! another level (a grey, or a black of 0 in a raw format) is not the
//! black level.
//!
//! **Cost.** The watch is a buffer probe that samples at most one frame
//! every [`SAMPLE_EVERY`] of stream time (the frames' PTS), reading
//! [`GRID_COLUMNS`] x [`GRID_ROWS`] luma values from the CPU-mapped frame:
//! never a full scan, never a GPU readback. It changes nothing in the
//! stream. On a raw camera it sits on the camera source's `src` pad, after
//! the frame guard ([`crate::frame_guard`]), so a dropped corrupt frame is
//! never sampled; on an MJPEG camera on the CPU decoder's `src` pad
//! (`jpegdec`, whose I420 luma it reads). A format without luma (RGB) has
//! no watch.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gst_video::prelude::*;
use obc_core::PixelFormat;

use crate::colour_range::luma_layout;

/// Columns of the sampling grid.
pub const GRID_COLUMNS: usize = 16;
/// Rows of the sampling grid.
pub const GRID_ROWS: usize = 9;
/// The shortest stream time between two sampled frames (at most four a
/// second).
pub const SAMPLE_EVERY: Duration = Duration::from_millis(250);
/// How long every sampled frame must be flat black before the shutter is
/// closed.
pub const CLOSED_AFTER: Duration = Duration::from_secs(1);
/// The lowest mean luma of a flat black frame.
pub const BLACK_MIN: f64 = 15.0;
/// The highest mean luma of a flat black frame.
pub const BLACK_MAX: f64 = 17.0;
/// The highest mean luma of a flat full-range black frame (decoded MJPEG
/// only, [`FlatBlack::DecodedMjpeg`]).
pub const FULL_BLACK_MAX: f64 = 1.0;
/// The largest luma spread of a flat black raw frame: none.
pub const RAW_SPREAD: u8 = 0;
/// The largest luma spread of a flat limited-range black decoded MJPEG
/// frame: one level of the decoder's rounding. A full-range black (0) of
/// decoded MJPEG has no spread at all.
pub const DECODED_SPREAD: u8 = 1;

/// Which frames count as flat black (see the module docs, "The rule").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlatBlack {
    /// A raw format: the limited-range black, every point equal.
    Raw,
    /// Decoded MJPEG: the limited-range black within the decoder's
    /// rounding, or the full-range black with every point equal.
    DecodedMjpeg,
}

/// What the frames say of the shutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutterState {
    Open,
    Closed,
}

/// Whether `samples` (luma values of one frame) are flat black under
/// `rule` ([`FlatBlack`]). No sample is not.
pub fn is_flat_black(samples: &[u8], rule: FlatBlack) -> bool {
    let (Some(&min), Some(&max)) = (samples.iter().min(), samples.iter().max()) else {
        return false;
    };
    let sum: u64 = samples.iter().map(|&v| u64::from(v)).sum();
    let mean = sum as f64 / samples.len() as f64;
    let spread = max - min;
    let limited = (BLACK_MIN..=BLACK_MAX).contains(&mean);
    match rule {
        FlatBlack::Raw => spread == RAW_SPREAD && limited,
        FlatBlack::DecodedMjpeg => {
            (spread <= DECODED_SPREAD && limited) || (spread == 0 && mean <= FULL_BLACK_MAX)
        }
    }
}

/// The luma of the [`GRID_COLUMNS`] x [`GRID_ROWS`] grid points of a frame
/// into `out` (cleared first): `plane` its luma plane (`stride` bytes a
/// row, `width` x `height` pixels), laid out as
/// [`crate::colour_range::luma_layout`] says (`pixel` bytes a sample from
/// `offset`). Each point is the centre of its cell. Points past the end of
/// `plane` are skipped.
pub fn sample_grid(
    plane: &[u8],
    stride: usize,
    (width, height): (usize, usize),
    (pixel, offset): (usize, usize),
    out: &mut Vec<u8>,
) {
    out.clear();
    if width == 0 || height == 0 {
        return;
    }
    for row in 0..GRID_ROWS {
        let y = (2 * row + 1) * height / (2 * GRID_ROWS);
        for col in 0..GRID_COLUMNS {
            let x = (2 * col + 1) * width / (2 * GRID_COLUMNS);
            if let Some(&v) = plane.get(y * stride + x * pixel + offset) {
                out.push(v);
            }
        }
    }
}

/// The rule of the module docs, on sampled frames: pure, timed by the
/// caller.
#[derive(Debug, Clone)]
pub struct ShutterDetector {
    rule: FlatBlack,
    closed_after: Duration,
    /// When the run of flat black frames began (`None`: the last frame
    /// sampled was not one).
    flat_since: Option<Duration>,
    state: ShutterState,
}

impl ShutterDetector {
    /// A detector taking the frames `rule` says ([`FlatBlack`]) for flat
    /// black; open until frames say otherwise.
    pub fn new(rule: FlatBlack) -> Self {
        Self::with_closed_after(rule, CLOSED_AFTER)
    }

    /// [`Self::new`] closing after `closed_after` of flat black frames.
    pub fn with_closed_after(rule: FlatBlack, closed_after: Duration) -> Self {
        ShutterDetector {
            rule,
            closed_after,
            flat_since: None,
            state: ShutterState::Open,
        }
    }

    /// One sampled frame, its luma `samples`, at stream time `at`: the
    /// state from then on.
    pub fn observe(&mut self, at: Duration, samples: &[u8]) -> ShutterState {
        if is_flat_black(samples, self.rule) {
            // A timestamp going back (a new timeline) starts the run again.
            let since = match self.flat_since {
                Some(t) if t <= at => t,
                _ => at,
            };
            self.flat_since = Some(since);
            if at - since >= self.closed_after {
                self.state = ShutterState::Closed;
            }
        } else {
            self.flat_since = None;
            self.state = ShutterState::Open;
        }
        self.state
    }

    /// The state after the last frame observed.
    pub fn state(&self) -> ShutterState {
        self.state
    }
}

/// No frame sampled yet.
const UNKNOWN: u8 = 0;
const OPEN: u8 = 1;
const CLOSED: u8 = 2;

#[derive(Debug)]
struct Sampler {
    detector: ShutterDetector,
    every: Duration,
    /// The stream time of the last frame sampled.
    last: Option<Duration>,
    /// Reused for each frame's samples.
    samples: Vec<u8>,
    /// The time base of frames without a PTS.
    started: Instant,
}

/// A [`watch`] on a camera's frames; [`Self::state`] is read by anyone,
/// any time (an atomic).
#[derive(Debug)]
pub struct ShutterWatch {
    sampler: Mutex<Sampler>,
    /// [`UNKNOWN`], [`OPEN`] or [`CLOSED`].
    state: AtomicU8,
    /// The luma layout of the frames ([`luma_layout`]).
    layout: (usize, usize),
}

impl ShutterWatch {
    fn new(layout: (usize, usize), detector: ShutterDetector, every: Duration) -> Self {
        ShutterWatch {
            sampler: Mutex::new(Sampler {
                detector,
                every,
                last: None,
                samples: Vec::with_capacity(GRID_COLUMNS * GRID_ROWS),
                started: Instant::now(),
            }),
            state: AtomicU8::new(UNKNOWN),
            layout,
        }
    }

    /// What the frames say of the shutter; `None` before a frame was
    /// sampled.
    pub fn state(&self) -> Option<ShutterState> {
        match self.state.load(Ordering::Relaxed) {
            OPEN => Some(ShutterState::Open),
            CLOSED => Some(ShutterState::Closed),
            _ => None,
        }
    }

    /// The stream time of a frame stamped `pts`: its PTS, or for a frame
    /// without one the monotonic time since the watch was made.
    fn stream_time(&self, pts: Option<Duration>) -> Duration {
        pts.unwrap_or_else(|| {
            self.sampler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .started
                .elapsed()
        })
    }

    /// One frame at stream time `at` ([`Self::stream_time`]), its luma
    /// `plane` (`stride`, `size` as [`sample_grid`] takes them): sampled
    /// unless one was less than the sampling interval before.
    fn on_frame(&self, at: Duration, plane: &[u8], stride: usize, size: (usize, usize)) {
        let mut s = self.sampler.lock().unwrap_or_else(|e| e.into_inner());
        if s.last.is_some_and(|t| t <= at && at - t < s.every) {
            return;
        }
        s.last = Some(at);
        let Sampler {
            detector, samples, ..
        } = &mut *s;
        sample_grid(plane, stride, size, self.layout, samples);
        let state = detector.observe(at, samples);
        self.state.store(
            match state {
                ShutterState::Open => OPEN,
                ShutterState::Closed => CLOSED,
            },
            Ordering::Relaxed,
        );
    }

    /// Whether a frame at stream time `at` ([`Self::stream_time`]) is due
    /// for a sample: the probe's cheap check, before the caps are read or
    /// the frame mapped (a frame without a PTS as much as any).
    fn due(&self, at: Duration) -> bool {
        let s = self.sampler.lock().unwrap_or_else(|e| e.into_inner());
        s.last.is_none_or(|t| at < t || at - t >= s.every)
    }

    /// One buffer leaving `pad`, its caps the pad's current ones.
    fn on_buffer(&self, pad: &gst::Pad, buffer: &gst::BufferRef) {
        let at = self.stream_time(buffer.pts().map(|t| Duration::from_nanos(t.nseconds())));
        if !self.due(at) {
            return;
        }
        let Some(info) = pad
            .current_caps()
            .and_then(|c| gst_video::VideoInfo::from_caps(&c).ok())
        else {
            return;
        };
        let Ok(frame) = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info) else {
            return;
        };
        let (Ok(plane), Some(&stride)) = (frame.plane_data(0), frame.plane_stride().first()) else {
            return;
        };
        let size = (info.width() as usize, info.height() as usize);
        self.on_frame(at, plane, usize::try_from(stride).unwrap_or(0), size);
    }
}

/// Watches the frames of `format` leaving `element`'s `src` pad for a
/// closed shutter (see the module docs): `format` is the format of those
/// frames (I420 after `jpegdec`), `rule` [`FlatBlack::Raw`] for a raw
/// camera, [`FlatBlack::DecodedMjpeg`] for decoded MJPEG. `None` (no
/// probe) for a format without luma or an element without a static `src`
/// pad. Panic-free (probe context).
pub fn watch(
    element: &gst::Element,
    format: PixelFormat,
    rule: FlatBlack,
) -> Option<Arc<ShutterWatch>> {
    watch_with(element, format, ShutterDetector::new(rule), SAMPLE_EVERY)
}

/// [`watch`] with its detector and sampling interval given (tests).
fn watch_with(
    element: &gst::Element,
    format: PixelFormat,
    detector: ShutterDetector,
    every: Duration,
) -> Option<Arc<ShutterWatch>> {
    let layout = luma_layout(format)?;
    let pad = element.static_pad("src")?;
    let watch = Arc::new(ShutterWatch::new(layout, detector, every));
    let probe = Arc::clone(&watch);
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if let Some(buffer) = info.buffer() {
            probe.on_buffer(pad, buffer);
        }
        gst::PadProbeReturn::Ok
    });
    Some(watch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::require_elements;

    const W: usize = 64;
    const H: usize = 36;

    /// A `W` x `H` luma plane (one byte a pixel) with `luma(x, y)`.
    fn plane(luma: impl Fn(usize, usize) -> u8) -> Vec<u8> {
        (0..W * H).map(|i| luma(i % W, i / W)).collect()
    }

    /// The grid samples of a luma plane.
    fn grid(p: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        sample_grid(p, W, (W, H), (1, 0), &mut out);
        out
    }

    /// A small deterministic noise in -`amp`..=`amp` for pixel `i`.
    fn noise(i: usize, amp: i32) -> i32 {
        let h = (i as u32).wrapping_mul(2_654_435_761) >> 16;
        (h % (2 * amp as u32 + 1)) as i32 - amp
    }

    /// The states of a raw detector fed `frames` (luma planes), one every
    /// 250 ms from 0.
    fn run(frames: impl IntoIterator<Item = Vec<u8>>) -> Vec<(Duration, ShutterState)> {
        let mut d = ShutterDetector::new(FlatBlack::Raw);
        frames
            .into_iter()
            .enumerate()
            .map(|(n, p)| {
                let at = Duration::from_millis(250 * n as u64);
                (at, d.observe(at, &grid(&p)))
            })
            .collect()
    }

    #[test]
    fn the_grid_is_sparse_and_centred() {
        let p = plane(|x, y| (x + y) as u8);
        let g = grid(&p);
        assert_eq!(g.len(), GRID_COLUMNS * GRID_ROWS);
        // Cell 0's centre: (2, 2) on a 64 x 36 frame.
        assert_eq!(g[0], 4);
        // YUY2: every other byte is luma.
        let packed: Vec<u8> = (0..W * H * 2)
            .map(|i| if i % 2 == 0 { 16 } else { 128 })
            .collect();
        let mut out = Vec::new();
        sample_grid(&packed, W * 2, (W, H), (2, 0), &mut out);
        assert!(out.iter().all(|&v| v == 16));
        // A plane cut short: the points past its end are skipped.
        sample_grid(&p[..W * 4], W, (W, H), (1, 0), &mut out);
        assert!(out.len() < GRID_COLUMNS * GRID_ROWS);
        sample_grid(&p, W, (0, 0), (1, 0), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn flat_black_closes_after_a_second_and_not_before() {
        let states = run((0..8).map(|_| plane(|_, _| 16)));
        for (at, s) in &states {
            let want = if *at >= CLOSED_AFTER {
                ShutterState::Closed
            } else {
                ShutterState::Open
            };
            assert_eq!(*s, want, "at {at:?}");
        }
        // Closed at exactly one second (the fifth frame), not at 750 ms.
        assert_eq!(states[3].1, ShutterState::Open);
        assert_eq!(states[4].1, ShutterState::Closed);
    }

    #[test]
    fn a_dark_noisy_room_never_closes() {
        // Mean about 18, a noise of +-2 to 3 levels.
        for amp in [2, 3] {
            let frames =
                (0..40).map(|n| plane(|x, y| (18 + noise(n * W * H + y * W + x, amp)) as u8));
            assert!(
                run(frames).iter().all(|(_, s)| *s == ShutterState::Open),
                "noise {amp}"
            );
        }
        // Darker still, the mean at the black level: the noise keeps it open.
        let frames = (0..40).map(|n| plane(|x, y| (16 + noise(n * 7 + y * W + x, 1)) as u8));
        assert!(run(frames).iter().all(|(_, s)| *s == ShutterState::Open));
    }

    #[test]
    fn a_normal_picture_never_closes() {
        let frames = (0..40).map(|n| plane(|x, y| (40 + (x * 3 + y * 2 + n) % 180) as u8));
        assert!(run(frames).iter().all(|(_, s)| *s == ShutterState::Open));
    }

    #[test]
    fn one_point_off_by_one_is_not_flat_in_a_raw_format() {
        let mut g = vec![16u8; GRID_COLUMNS * GRID_ROWS];
        g[77] = 17;
        assert!(!is_flat_black(&g, FlatBlack::Raw));
        // Decoded MJPEG allows the decoder's rounding.
        assert!(is_flat_black(&g, FlatBlack::DecodedMjpeg));
        g[78] = 15;
        assert!(!is_flat_black(&g, FlatBlack::DecodedMjpeg));
        // On frames: one pixel off at a grid point keeps it open.
        let frames = (0..8).map(|_| plane(|x, y| if (x, y) == (2, 2) { 17 } else { 16 }));
        assert!(run(frames).iter().all(|(_, s)| *s == ShutterState::Open));
    }

    #[test]
    fn a_picture_coming_back_opens_at_once() {
        let mut d = ShutterDetector::new(FlatBlack::Raw);
        let black = grid(&plane(|_, _| 16));
        let picture = grid(&plane(|x, _| (20 + x) as u8));
        for n in 0..6 {
            d.observe(Duration::from_millis(250 * n), &black);
        }
        assert_eq!(d.state(), ShutterState::Closed);
        assert_eq!(
            d.observe(Duration::from_millis(1500), &picture),
            ShutterState::Open
        );
        // A new run of black takes another second.
        assert_eq!(
            d.observe(Duration::from_millis(1750), &black),
            ShutterState::Open
        );
        assert_eq!(
            d.observe(Duration::from_millis(2500), &black),
            ShutterState::Open
        );
        assert_eq!(
            d.observe(Duration::from_millis(2750), &black),
            ShutterState::Closed
        );
    }

    #[test]
    fn flat_frames_away_from_the_black_level_never_close() {
        for level in [0u8, 14, 18, 128, 235] {
            let frames = (0..12).map(|_| plane(|_, _| level));
            assert!(
                run(frames).iter().all(|(_, s)| *s == ShutterState::Open),
                "flat {level}"
            );
        }
        // 15 and 17 are within the black level's tolerance.
        assert!(is_flat_black(&[15; 4], FlatBlack::Raw));
        assert!(is_flat_black(&[17; 4], FlatBlack::Raw));
        assert!(!is_flat_black(&[], FlatBlack::Raw));
    }

    #[test]
    fn a_timeline_going_back_starts_the_run_again() {
        let mut d = ShutterDetector::new(FlatBlack::Raw);
        let black = [16u8; 8];
        d.observe(Duration::from_secs(10), &black);
        assert_eq!(
            d.observe(Duration::from_secs(1), &black),
            ShutterState::Open
        );
        assert_eq!(
            d.observe(Duration::from_secs(2), &black),
            ShutterState::Closed
        );
    }

    #[test]
    fn the_watch_samples_at_most_every_interval() {
        let w = ShutterWatch::new((1, 0), ShutterDetector::new(FlatBlack::Raw), SAMPLE_EVERY);
        assert_eq!(w.state(), None, "nothing sampled yet");
        let black = plane(|_, _| 16);
        let picture = plane(|x, _| (20 + x) as u8);
        // 30 fps of black for 1.1 s: closed.
        for n in 0..34u64 {
            w.on_frame(Duration::from_millis(n * 33), &black, W, (W, H));
        }
        assert_eq!(w.state(), Some(ShutterState::Closed));
        // A picture 100 ms after the last sample is not sampled; one 250 ms
        // after it is.
        let last = w.sampler.lock().unwrap().last.unwrap();
        w.on_frame(last + Duration::from_millis(100), &picture, W, (W, H));
        assert_eq!(w.state(), Some(ShutterState::Closed));
        w.on_frame(last + SAMPLE_EVERY, &picture, W, (W, H));
        assert_eq!(w.state(), Some(ShutterState::Open));
    }

    /// Frames without a PTS are timed on the monotonic clock: one is not
    /// due (its caps not read, its frame not mapped) until the sampling
    /// interval has passed since the last sample.
    #[test]
    fn frames_without_a_timestamp_are_gated_on_the_monotonic_clock() {
        let w = ShutterWatch::new(
            (1, 0),
            ShutterDetector::new(FlatBlack::Raw),
            Duration::from_millis(200),
        );
        let black = plane(|_, _| 16);
        assert!(w.due(w.stream_time(None)), "the first frame is sampled");
        w.on_frame(w.stream_time(None), &black, W, (W, H));
        assert_eq!(w.state(), Some(ShutterState::Open));
        assert!(!w.due(w.stream_time(None)), "not due right after a sample");
        std::thread::sleep(Duration::from_millis(220));
        assert!(w.due(w.stream_time(None)));
        // A stamped frame keeps its own time.
        assert_eq!(
            w.stream_time(Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
    }

    /// The states of a decoded-MJPEG detector fed `frames`, one every
    /// 250 ms from 0.
    fn run_mjpeg(frames: impl IntoIterator<Item = Vec<u8>>) -> Vec<ShutterState> {
        let mut d = ShutterDetector::new(FlatBlack::DecodedMjpeg);
        frames
            .into_iter()
            .enumerate()
            .map(|(n, p)| d.observe(Duration::from_millis(250 * n as u64), &grid(&p)))
            .collect()
    }

    /// Decoded MJPEG takes the full-range black (flat 0 or 1) as well as
    /// the limited one: the decoder puts out full range, and the closed
    /// black level of the MJPEG stream is not measured.
    #[test]
    fn decoded_mjpeg_closes_on_a_flat_full_range_black() {
        for level in [0u8, 1] {
            let states = run_mjpeg((0..8).map(|_| plane(|_, _| level)));
            assert_eq!(states[3], ShutterState::Open, "flat {level}");
            assert_eq!(states[4], ShutterState::Closed, "flat {level}");
        }
        // The limited black, within the rounding, as before.
        let states = run_mjpeg((0..8).map(|_| plane(|x, _| if x == 2 { 17 } else { 16 })));
        assert_eq!(states[7], ShutterState::Closed);
        // A raw format never takes a black of 0.
        assert!(!is_flat_black(&[0; 8], FlatBlack::Raw));
        assert!(is_flat_black(&[0; 8], FlatBlack::DecodedMjpeg));
    }

    #[test]
    fn decoded_mjpeg_never_closes_on_a_dark_noisy_room_near_zero() {
        // Luma 0..=3: a spread of 2 or more on every frame.
        let frames = (0..40).map(|n| {
            plane(|x, y| {
                let v = (n * W * H + y * W + x) % 4;
                v as u8
            })
        });
        assert!(run_mjpeg(frames).iter().all(|s| *s == ShutterState::Open));
        let frames =
            (0..40).map(|n| plane(|x, y| (2 + noise(n * W * H + y * W + x, 1)).max(0) as u8));
        assert!(run_mjpeg(frames).iter().all(|s| *s == ShutterState::Open));
    }

    #[test]
    fn decoded_mjpeg_full_range_black_allows_no_spread() {
        let mut g = vec![1u8; GRID_COLUMNS * GRID_ROWS];
        g[40] = 2;
        assert!(!is_flat_black(&g, FlatBlack::DecodedMjpeg));
        let frames = (0..8).map(|_| plane(|x, y| if (x, y) == (2, 2) { 2 } else { 1 }));
        assert!(run_mjpeg(frames).iter().all(|s| *s == ShutterState::Open));
    }

    #[test]
    fn decoded_mjpeg_never_closes_between_the_two_blacks() {
        // A spread of 1 around a mean of about 8: neither black.
        let frames = (0..40).map(|_| plane(|x, y| 8 + ((x + y) % 2) as u8));
        assert!(run_mjpeg(frames).iter().all(|s| *s == ShutterState::Open));
        for level in [2u8, 8, 14, 18] {
            assert!(
                !is_flat_black(&[level; 8], FlatBlack::DecodedMjpeg),
                "flat {level}"
            );
        }
    }

    /// Pushes `buffers` (stamped every 33 ms) through `appsrc (caps) !
    /// fakesink` with a watch of `format` on the source: the watch.
    fn push_through_watch(caps: &str, format: PixelFormat, frames: Vec<Vec<u8>>) -> ShutterState {
        require_elements(&["appsrc", "fakesink"]);
        let pipeline = gst::Pipeline::with_name("shutter-test");
        let src = gst_app::AppSrc::builder()
            .caps(&caps.parse::<gst::Caps>().unwrap())
            .format(gst::Format::Time)
            .build();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([src.upcast_ref(), &sink]).unwrap();
        src.link(&sink).unwrap();
        let watch = super::watch(src.upcast_ref(), format, FlatBlack::Raw).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        for (n, bytes) in frames.into_iter().enumerate() {
            let mut b = gst::Buffer::from_slice(bytes);
            b.get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(33 * n as u64));
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
        watch.state().expect("sampled")
    }

    /// On a pipeline, CPU only: 1.3 s of flat black NV12 closes the
    /// shutter, and YUY2 with a picture keeps it open.
    #[test]
    fn a_probe_reads_the_luma_of_mapped_frames() {
        let nv12 = |y: u8| {
            let mut f = vec![y; W * H];
            f.extend(std::iter::repeat_n(128u8, W * H / 2));
            f
        };
        let caps = format!("video/x-raw,format=NV12,width={W},height={H},framerate=30/1");
        let state = push_through_watch(
            &caps,
            PixelFormat::Nv12,
            (0..40).map(|_| nv12(16)).collect(),
        );
        assert_eq!(state, ShutterState::Closed);
        let yuy2: Vec<u8> = (0..W * H * 2)
            .map(|i| if i % 2 == 0 { (16 + i % 50) as u8 } else { 128 })
            .collect();
        let caps = format!("video/x-raw,format=YUY2,width={W},height={H},framerate=30/1");
        let state = push_through_watch(&caps, PixelFormat::Yuy2, vec![yuy2; 40]);
        assert_eq!(state, ShutterState::Open);
        // No luma, no watch.
        let src = gst::ElementFactory::make("fakesrc").build().unwrap();
        assert!(super::watch(&src, PixelFormat::Rgb, FlatBlack::Raw).is_none());
    }
}
