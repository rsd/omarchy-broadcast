//! Live statistics of a running stream, for the daemon's ~1 Hz `Stats`
//! signal (`docs/adr/0011-daemon-lifecycle-and-dbus-api.md`, "D-Bus API
//! v1").
//!
//! The same join as `obc-bench` ([`crate::instrument`]), cut down to what a
//! long-running daemon needs:
//!
//! * a buffer probe on each ingest's camera source (the element named
//!   [`names::CAMERA`], its `src` pad) records when every camera frame left
//!   it, on CLOCK_MONOTONIC (`Instant`), keyed by (source id, camera PTS).
//!   The PTS is unchanged through the ingest pipeline into the slot, so the
//!   output's emit observer finds it again;
//! * the output's emit observer ([`LiveStats::observer`]) counts every tick
//!   the output's `appsrc` accepted and, for a fresh frame, takes its
//!   latency: from the camera frame leaving the source to its tick being
//!   handed to the output's `appsrc` ([`crate::output::EmitInfo::emitted_at`]).
//!
//! **What the counts cover:** the output's ticks, not the frames that reach
//! the loopback device. Between the two, the output's queue and the
//! writer's queue may drop frames, and the loopback writer adds its own
//! placeholders whenever the output falls silent. The rate of frames written
//! to the device comes from the writer's counters
//! ([`crate::loopback::LoopbackWriter::counts`]).
//!
//! **What the latency covers:** the ingest pipeline (queue, CPU adapter,
//! upload, crop and scale), the wait in the latest-frame slot until the
//! next output tick (up to one output period less the phase lock's
//! margin), and the clock thread's wake-up. It does **not** cover the
//! sensor exposure and USB transfer (before the source), nor anything
//! after the hand-over to `appsrc`: the Process stage, the conversion and
//! download, the loopback write and the consumer. Repeats and placeholders
//! carry no new camera frame and have no latency. It is therefore smaller
//! than `obc-bench`'s added latency (which ends at the sink, after the
//! download).
//!
//! **Memory is bounded:** at most [`LiveStats::PENDING_MAX`] camera frames
//! wait for their tick (the oldest are forgotten: frames superseded in the
//! slot are never emitted), and a window keeps at most
//! [`LiveStats::SAMPLES_MAX`] latency samples (the counts go on).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gst::prelude::*;

use crate::clock::TickKind;
use crate::ingest::Ingest;
use crate::output::{EmitInfo, EmitObserver};
use crate::pipeline::{PipelineError, names};
use crate::stats::percentile_sorted;

/// The stream over one window (since the previous [`LiveStats::take_window`]).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveWindow {
    /// How long the window lasted.
    pub duration: Duration,
    /// Output ticks the output's `appsrc` accepted: fresh, repeat and
    /// placeholder. Not the frames written to the loopback device (see the
    /// module docs).
    pub ticks: u64,
    /// Of those, fresh camera frames.
    pub fresh: u64,
    /// The median latency of the window's fresh frames, in milliseconds
    /// (see the module docs for what it covers); `None` without a fresh
    /// frame whose camera time was recorded.
    pub latency_p50_ms: Option<f64>,
}

impl LiveWindow {
    /// Output ticks per second over the window (`None` for an empty one):
    /// the rate the output produced, not the rate written to the loopback
    /// device.
    pub fn output_fps(&self) -> Option<f64> {
        let s = self.duration.as_secs_f64();
        (s > 0.0).then(|| self.ticks as f64 / s)
    }

    /// Fresh camera frames per second over the window.
    pub fn fresh_fps(&self) -> Option<f64> {
        let s = self.duration.as_secs_f64();
        (s > 0.0).then(|| self.fresh as f64 / s)
    }
}

#[derive(Debug)]
struct Inner {
    /// Camera frames not yet emitted: (source id, camera PTS, left the
    /// source at), oldest first.
    pending: VecDeque<(u64, u64, Instant)>,
    window_start: Instant,
    ticks: u64,
    fresh: u64,
    /// Latencies in milliseconds.
    samples: Vec<f64>,
}

/// Live statistics of the stream (see the module docs). Shared: the camera
/// probes and the emit observer hold references.
#[derive(Debug)]
pub struct LiveStats {
    inner: Mutex<Inner>,
}

impl LiveStats {
    /// Camera frames kept waiting for their tick (several seconds of a
    /// 60 fps camera).
    pub const PENDING_MAX: usize = 256;
    /// Latency samples kept per window.
    pub const SAMPLES_MAX: usize = 4096;

    /// A new collector; its first window starts now.
    pub fn new() -> Arc<Self> {
        Arc::new(LiveStats {
            inner: Mutex::new(Inner {
                pending: VecDeque::with_capacity(Self::PENDING_MAX),
                window_start: Instant::now(),
                ticks: 0,
                fresh: 0,
                samples: Vec::new(),
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Installs the probe on `ingest`'s camera source (the element named
    /// [`names::CAMERA`]), recording its frames under its source id. Call it
    /// before the ingest starts ([`crate::session::Session::switch_to`]).
    pub fn attach(self: &Arc<Self>, ingest: &Ingest) -> Result<(), PipelineError> {
        let pad = ingest
            .pipeline()
            .element(names::CAMERA)
            .and_then(|e| e.static_pad("src"))
            .ok_or_else(|| {
                PipelineError::Link(format!(
                    "ingest {} has no {:?} element with a src pad",
                    ingest.source_id(),
                    names::CAMERA
                ))
            })?;
        let source_id = ingest.source_id();
        let stats = Arc::downgrade(self);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let now = Instant::now();
            if let (Some(stats), Some(pts)) = (stats.upgrade(), info.buffer().and_then(|b| b.pts()))
            {
                stats.on_camera(source_id, pts.nseconds(), now);
            }
            gst::PadProbeReturn::Ok
        });
        Ok(())
    }

    fn on_camera(&self, source_id: u64, pts_ns: u64, at: Instant) {
        let mut g = self.lock();
        if g.pending.len() >= Self::PENDING_MAX {
            g.pending.pop_front();
        }
        g.pending.push_back((source_id, pts_ns, at));
    }

    /// The emit observer to install on the output
    /// ([`crate::output::Output::set_observer`], before it starts).
    pub fn observer(self: &Arc<Self>) -> EmitObserver {
        let stats = Arc::clone(self);
        Arc::new(move |e: &EmitInfo| stats.on_tick(e))
    }

    fn on_tick(&self, e: &EmitInfo) {
        if !e.pushed {
            return;
        }
        let mut g = self.lock();
        g.ticks += 1;
        if e.kind != TickKind::Fresh {
            return;
        }
        g.fresh += 1;
        let Some((source_id, Some(pts))) = e.origin.map(|o| (o.source_id, o.source_pts_ns)) else {
            return;
        };
        let Some(i) = g
            .pending
            .iter()
            .rposition(|&(s, p, _)| s == source_id && p == pts)
        else {
            return;
        };
        let (_, _, left) = g.pending[i];
        // Older frames than this one will never be fresh any more.
        g.pending.drain(..=i);
        if g.samples.len() < Self::SAMPLES_MAX {
            let ms = e.emitted_at.saturating_duration_since(left).as_secs_f64() * 1e3;
            g.samples.push(ms);
        }
    }

    /// The window since the previous call (or since [`Self::new`]), and
    /// starts the next one.
    pub fn take_window(&self) -> LiveWindow {
        let now = Instant::now();
        let mut g = self.lock();
        let mut samples = std::mem::take(&mut g.samples);
        samples.sort_by(f64::total_cmp);
        let window = LiveWindow {
            duration: now.saturating_duration_since(g.window_start),
            ticks: std::mem::take(&mut g.ticks),
            fresh: std::mem::take(&mut g.fresh),
            latency_p50_ms: percentile_sorted(&samples, 50.0),
        };
        g.window_start = now;
        window
    }

    /// Camera frames waiting for their tick (for tests and diagnostics).
    pub fn pending(&self) -> usize {
        self.lock().pending.len()
    }
}

/// On the CPU: system-memory output and ingest, a test pattern as the
/// camera. Counts are compared exactly once everything has stopped; the
/// latency only from below.
#[cfg(test)]
mod tests {
    use obc_core::{AspectPolicy, OutputContract};

    use super::*;
    use crate::output::{FrameOrigin, Output, OutputConfig, OutputSink};
    use crate::test_support::require_elements;
    use obc_core::config::VideoMemory;

    fn tick(kind: TickKind, origin: Option<(u64, u64)>, at: Instant) -> EmitInfo {
        EmitInfo {
            tick: 0,
            pts_ns: 0,
            kind,
            origin: origin.map(|(source_id, pts)| FrameOrigin {
                source_id,
                seq: 1,
                source_pts_ns: Some(pts),
                arrived_at: at,
            }),
            emitted_at: at,
            grid_at: at,
            phase_offset_ns: 0,
            phase_locked: false,
            pushed: true,
            queue_level: 0,
        }
    }

    #[test]
    fn a_window_counts_ticks_and_takes_fresh_latencies() {
        let s = LiveStats::new();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        s.on_camera(1, 100, t0);
        s.on_camera(1, 200, t0 + ms(10));
        s.on_camera(1, 300, t0 + ms(20));
        // Frame 200 fresh 30 ms after it left; 100 is now stale.
        s.on_tick(&tick(TickKind::Fresh, Some((1, 200)), t0 + ms(40)));
        assert_eq!(s.pending(), 1);
        s.on_tick(&tick(TickKind::Repeat, Some((1, 200)), t0 + ms(73)));
        s.on_tick(&tick(TickKind::Fresh, Some((1, 300)), t0 + ms(70)));
        s.on_tick(&tick(TickKind::Placeholder, None, t0 + ms(106)));
        // A tick appsrc refused is not counted.
        s.on_tick(&EmitInfo {
            pushed: false,
            ..tick(TickKind::Placeholder, None, t0)
        });
        // Another source's frame with the same PTS is not this one.
        s.on_tick(&tick(TickKind::Fresh, Some((2, 300)), t0 + ms(80)));
        let w = s.take_window();
        assert_eq!((w.ticks, w.fresh), (5, 3));
        let p50 = w.latency_p50_ms.unwrap();
        assert!((p50 - 40.0).abs() < 1e-6, "{p50}");
        assert!(w.output_fps().is_some());
        let next = s.take_window();
        assert_eq!((next.ticks, next.fresh, next.latency_p50_ms), (0, 0, None));
    }

    #[test]
    fn memory_is_bounded() {
        let s = LiveStats::new();
        let t0 = Instant::now();
        for pts in 0..10_000 {
            s.on_camera(1, pts, t0);
        }
        assert_eq!(s.pending(), LiveStats::PENDING_MAX);
        for pts in 0..10_000u64 {
            s.on_camera(1, pts, t0);
            s.on_tick(&tick(TickKind::Fresh, Some((1, pts)), t0));
        }
        let g = s.lock();
        assert_eq!(g.samples.len(), LiveStats::SAMPLES_MAX);
        assert_eq!(g.fresh, 10_000);
    }

    /// A live 30 fps test pattern named as a camera, held back 100 ms by a
    /// queue after the source, through a system-memory output: every tick
    /// is counted, the fresh ones match the output's, and the latency is at
    /// least the queue's hold less one capture period. After the source stops, a window has ticks
    /// but no fresh frame and no latency.
    #[test]
    fn live_stats_follow_a_cpu_stream() {
        require_elements(&[
            "videotestsrc",
            "capsfilter",
            "queue",
            "videocrop",
            "videoconvertscale",
            "appsink",
            "appsrc",
            "capssetter",
            "fakesink",
        ]);
        let contract: OutputContract = "320x240@30".parse().unwrap();
        let stats = LiveStats::new();
        let mut output = Output::build_in(
            OutputConfig::new(contract, OutputSink::Fake),
            VideoMemory::System,
        )
        .unwrap();
        output.set_observer(Some(stats.observer()));
        output.start().unwrap();
        let hold = Duration::from_millis(100);
        let src = gst::ElementFactory::make("videotestsrc")
            .name(names::CAMERA)
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
        let ns = u64::try_from(hold.as_nanos()).unwrap();
        let delay = gst::ElementFactory::make("queue")
            .property("min-threshold-time", ns)
            .property("max-size-time", 2 * ns + 1_000_000_000)
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .build()
            .unwrap();
        let mut ingest = Ingest::from_head(
            vec![src, caps, delay],
            "NV12",
            (160, 120),
            AspectPolicy::Crop,
            output.frame_writer(),
        )
        .unwrap();
        stats.attach(&ingest).unwrap();
        ingest.start_with(None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while output.counts().fresh < 30 {
            assert!(Instant::now() < deadline, "no fresh frames");
            std::thread::sleep(Duration::from_millis(10));
        }
        let first = stats.take_window();
        ingest.stop().unwrap();
        // Ticks without a source: repeats, then placeholders.
        std::thread::sleep(Duration::from_millis(300));
        output.stop().unwrap();
        let second = stats.take_window();
        let counts = output.counts();
        eprintln!("{first:?} {second:?} {counts:?}");
        assert_eq!(first.ticks + second.ticks, counts.emitted);
        assert_eq!(first.fresh + second.fresh, counts.fresh);
        assert!(first.fresh >= 30 && first.ticks >= first.fresh);
        // The queue releases a frame once it holds `hold` of frames: each
        // waits about that long, less up to one capture period.
        let p50 = first.latency_p50_ms.expect("a latency");
        let floor = (hold.as_secs_f64() - 1.0 / 30.0) * 1e3;
        assert!(p50 >= floor, "p50 {p50} ms, below {floor} ms");
        assert!(second.ticks >= 5, "{second:?}");
        assert!(stats.pending() <= LiveStats::PENDING_MAX);
        // A window with no fresh frame has no latency.
        let empty = stats.take_window();
        assert_eq!(
            (empty.ticks, empty.fresh, empty.latency_p50_ms),
            (0, 0, None)
        );
    }

    #[test]
    fn an_ingest_without_a_camera_element_is_refused() {
        require_elements(&["videotestsrc", "videocrop", "videoconvertscale", "appsink"]);
        let output = Output::build_in(
            OutputConfig::new("320x240@30".parse().unwrap(), OutputSink::Fake),
            VideoMemory::System,
        )
        .unwrap();
        let ingest = Ingest::from_head(
            vec![gst::ElementFactory::make("videotestsrc").build().unwrap()],
            "NV12",
            (160, 120),
            AspectPolicy::Crop,
            output.frame_writer(),
        )
        .unwrap();
        assert!(matches!(
            LiveStats::new().attach(&ingest),
            Err(PipelineError::Link(_))
        ));
    }
}
