//! The delivered frame rate of a camera: the rate of the good frames its
//! frame guard passes ([`crate::frame_guard`]), measured from their
//! timestamps.
//!
//! A camera's nominal rate (its capture mode) is what it was asked for; the
//! rate it delivers can be lower: a long exposure in low light (a camera
//! that stretches its frame interval, the VX-800 alternating 33 and 67 ms),
//! a camera whose real rate is not its advertised one (the Generalplus
//! sensor, 51.4 ms per frame: 19.45 fps for a nominal 30), USB bandwidth,
//! or frames dropped as corrupt. The guard's probe records the PTS of each
//! frame it passes; `v4l2src` stamps a buffer from the kernel's buffer
//! timestamp, so the PTS spacing is the camera's own, not the
//! streaming thread's.
//!
//! The rate is (frames - 1) over the span between the first and the last
//! of the frames in a sliding window of about [`RATE_WINDOW`]
//! ([`rate_from_pts`]): a gap in the stream lowers it, as it lowers what
//! the output receives. The first [`RATE_SKIP_FRAMES`] good frames of a
//! stream are not counted (a camera's first frames come irregularly while
//! its exposure settles); a rate is reported only once the window spans at
//! least [`RATE_MIN_SPAN`], and no longer once no good frame has arrived
//! for [`RATE_STALE_AFTER`] (a stalled camera has no current rate). A PTS
//! that goes back restarts the window (a new timeline). Memory is bounded:
//! at most [`RATE_CAPACITY`] timestamps.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Good frames at the start of a stream that are not counted: half a
/// second of a 30 fps camera, as the guard's grace periods
/// ([`crate::frame_guard::EOI_GRACE`]).
pub const RATE_SKIP_FRAMES: u64 = 15;

/// The span of frames the rate is measured over (at most).
pub const RATE_WINDOW: Duration = Duration::from_secs(5);

/// The shortest span of frames a rate is reported for.
pub const RATE_MIN_SPAN: Duration = Duration::from_secs(3);

/// How long after the last good frame (by the monotonic clock) the rate
/// stops being reported. Longer than any frame interval a camera uses in
/// practice (a long exposure is a few hundred milliseconds at most).
pub const RATE_STALE_AFTER: Duration = Duration::from_secs(1);

/// The most timestamps the window holds: [`RATE_WINDOW`] at about 400 fps.
/// A faster stream keeps a shorter window.
pub const RATE_CAPACITY: usize = 2048;

/// The rate of the frames stamped `pts_ns` (nanoseconds, ascending):
/// `frames - 1` over the span from the first to the last, in frames per
/// second. `None` for fewer than two frames, a span shorter than
/// [`RATE_MIN_SPAN`], or timestamps out of order at the ends.
pub fn rate_from_pts(pts_ns: &[u64]) -> Option<f64> {
    let (&first, &last) = (pts_ns.first()?, pts_ns.last()?);
    let span = last.checked_sub(first)?;
    if pts_ns.len() < 2 || span < RATE_MIN_SPAN.as_nanos() as u64 {
        return None;
    }
    Some((pts_ns.len() - 1) as f64 * 1e9 / span as f64)
}

/// The sliding window of good-frame timestamps behind the delivered rate
/// (see the module docs).
#[derive(Debug, Default)]
pub struct DeliveredRate {
    /// Good frames seen, the skipped ones included.
    seen: u64,
    /// The PTS of the counted frames in the window, ascending.
    pts: VecDeque<u64>,
    /// When the last good frame was recorded.
    last_at: Option<Instant>,
}

impl DeliveredRate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a good frame stamped `pts` (nanoseconds; `None` for a
    /// buffer without a PTS, which counts as seen and is not measured),
    /// recorded at `now`.
    pub fn record(&mut self, pts: Option<u64>, now: Instant) {
        self.seen = self.seen.saturating_add(1);
        self.last_at = Some(now);
        if self.seen <= RATE_SKIP_FRAMES {
            return;
        }
        let Some(pts) = pts else {
            return;
        };
        match self.pts.back() {
            Some(&last) if pts == last => return,
            Some(&last) if pts < last => self.pts.clear(),
            _ => {}
        }
        self.pts.push_back(pts);
        let window = RATE_WINDOW.as_nanos() as u64;
        while let Some(&first) = self.pts.front() {
            if pts - first <= window && self.pts.len() <= RATE_CAPACITY {
                break;
            }
            self.pts.pop_front();
        }
    }

    /// The delivered rate at `now`, in frames per second ([`rate_from_pts`]
    /// over the window): `None` until the window spans [`RATE_MIN_SPAN`],
    /// and once no good frame has come for [`RATE_STALE_AFTER`].
    pub fn fps(&mut self, now: Instant) -> Option<f64> {
        let last_at = self.last_at?;
        if now.saturating_duration_since(last_at) > RATE_STALE_AFTER {
            return None;
        }
        rate_from_pts(self.pts.make_contiguous())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// Timestamps from 0 with the intervals `intervals_ns` repeated, `n`
    /// frames.
    fn stamps(intervals_ns: &[u64], n: usize) -> Vec<u64> {
        let mut t = 0;
        (0..n)
            .map(|i| {
                let at = t;
                t += intervals_ns[i % intervals_ns.len()];
                at
            })
            .collect()
    }

    fn close(got: Option<f64>, want: f64) {
        let got = got.unwrap_or_else(|| panic!("no rate, want {want}"));
        assert!((got - want).abs() < 0.01, "{got} != {want}");
    }

    #[test]
    fn a_steady_30_fps_stream() {
        // 1e9 / 30 is not whole: a camera's stamps round each interval.
        let pts: Vec<u64> = (0..150).map(|i| i * 1_000_000_000 / 30).collect();
        close(rate_from_pts(&pts), 30.0);
    }

    #[test]
    fn the_generalplus_rate_of_51_4_ms_frames() {
        let pts = stamps(&[51_400_000], 100);
        close(rate_from_pts(&pts), 1e9 / 51.4e6);
        close(rate_from_pts(&pts), 19.455);
    }

    #[test]
    fn alternating_33_and_67_ms_is_20_fps() {
        // The VX-800 under a long exposure: every other frame late.
        let pts = stamps(&[33 * MS, 67 * MS], 101);
        close(rate_from_pts(&pts), 20.0);
    }

    #[test]
    fn too_short_a_window_has_no_rate() {
        assert_eq!(rate_from_pts(&[]), None);
        assert_eq!(rate_from_pts(&[5]), None);
        // 2.97 s of 30 fps.
        let pts = stamps(&[33 * MS], 91);
        assert_eq!(rate_from_pts(&pts), None);
        let pts = stamps(&[33 * MS], 92);
        assert!(rate_from_pts(&pts).is_some());
        // Out of order at the ends.
        assert_eq!(rate_from_pts(&[4_000 * MS, 0]), None);
    }

    #[test]
    fn a_gap_lowers_the_rate() {
        // 2 s at 30 fps, 1 s with no frame, 2 s at 30 fps: 120 frames over
        // (59 + 30 + 59) intervals of 33.3 ms.
        let period = 1_000_000_000 / 30;
        let mut pts: Vec<u64> = (0..60).map(|i| i * period).collect();
        let resume = pts[59] + 1_000_000_000;
        pts.extend((0..60).map(|i| resume + i * period));
        let span = (pts[119] - pts[0]) as f64 / 1e9;
        close(rate_from_pts(&pts), 119.0 / span);
        let rate = rate_from_pts(&pts).unwrap();
        assert!((23.0..25.0).contains(&rate), "{rate}");
    }

    /// Feeds `n` frames `interval` apart from PTS `from`, the wall clock
    /// moving with them from `t0`.
    fn feed(rate: &mut DeliveredRate, t0: Instant, from: u64, interval: u64, n: u64) -> u64 {
        let mut pts = from;
        for _ in 0..n {
            rate.record(Some(pts), t0 + Duration::from_nanos(pts));
            pts += interval;
        }
        pts
    }

    #[test]
    fn the_window_skips_the_first_frames_and_waits_for_three_seconds() {
        let t0 = Instant::now();
        let mut rate = DeliveredRate::new();
        // The first 15 frames come at 10 fps: not counted.
        let next = feed(&mut rate, t0, 0, 100 * MS, RATE_SKIP_FRAMES);
        assert!(rate.pts.is_empty());
        // 2.9 s at 20 fps: too short.
        let next = feed(&mut rate, t0, next, 50 * MS, 59);
        assert_eq!(rate.fps(t0 + Duration::from_nanos(next - 50 * MS)), None);
        // 3 s: the rate, without the skipped frames.
        let next = feed(&mut rate, t0, next, 50 * MS, 2);
        let now = t0 + Duration::from_nanos(next - 50 * MS);
        close(rate.fps(now), 20.0);
        // Stale once no frame has come for a while.
        assert!(rate.fps(now + RATE_STALE_AFTER).is_some());
        assert_eq!(
            rate.fps(now + RATE_STALE_AFTER + Duration::from_millis(1)),
            None
        );
    }

    #[test]
    fn the_window_slides_and_is_bounded() {
        let t0 = Instant::now();
        let mut rate = DeliveredRate::new();
        // 10 s at 30 fps, then 10 s at 15 fps: only the last 5 s count.
        let period = 1_000_000_000 / 30;
        let next = feed(&mut rate, t0, 0, period, RATE_SKIP_FRAMES + 300);
        let next = feed(&mut rate, t0, next, 2 * period, 150);
        let span = rate.pts.back().unwrap() - rate.pts.front().unwrap();
        assert!(span <= RATE_WINDOW.as_nanos() as u64, "{span}");
        close(rate.fps(t0 + Duration::from_nanos(next)), 15.0);
        // 1000 fps: the window keeps at most RATE_CAPACITY stamps (2 s).
        let next = feed(&mut rate, t0, next, MS, 10_000);
        assert_eq!(rate.pts.len(), RATE_CAPACITY);
        assert_eq!(rate.fps(t0 + Duration::from_nanos(next)), None);
    }

    #[test]
    fn a_timestamp_going_back_restarts_the_window() {
        let t0 = Instant::now();
        let mut rate = DeliveredRate::new();
        let next = feed(&mut rate, t0, 0, 33 * MS, RATE_SKIP_FRAMES + 120);
        assert!(rate.fps(t0 + Duration::from_nanos(next)).is_some());
        // A new timeline from 0: no rate until it spans 3 s again; a
        // repeated stamp and a frame without PTS are not counted.
        rate.record(Some(0), t0 + Duration::from_nanos(next));
        rate.record(Some(0), t0 + Duration::from_nanos(next));
        rate.record(None, t0 + Duration::from_nanos(next));
        assert_eq!(rate.pts.len(), 1);
        assert_eq!(rate.fps(t0 + Duration::from_nanos(next)), None);
    }
}
