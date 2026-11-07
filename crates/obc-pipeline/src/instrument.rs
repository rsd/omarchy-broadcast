//! Per-buffer instrumentation: pad probes that time every frame.
//!
//! Latency method:
//!
//! * **Primary, in-process.** A probe on the `v4l2src` src pad and one on the
//!   `v4l2sink` sink pad record a monotonic timestamp per buffer, keyed by the
//!   buffer PTS (unchanged by queue/cudaupload/cudaconvert/cudadownload).
//!   Added latency = sink-ready time - src-pad time: the delay our stages add
//!   over an app reading the camera directly, because a direct reader
//!   receives the same buffer at the src-pad moment.
//!
//!   "Sink-ready" matters: `cudadownload` hands `v4l2sink` buffers that are
//!   still `GstCudaMemory` (system-memory caps, device-resident data), and
//!   CUDA work is asynchronous, so at pad arrival the conversion kernels and
//!   the device->host copy may not have run yet; they complete when the
//!   buffer is first mapped. The sink probe therefore maps the buffer
//!   readable (the same sync + download `v4l2sink`'s own map would trigger
//!   next, so no extra work) and takes the timestamp after it. The plain
//!   pad-arrival delta is kept as a secondary figure.
//! * **Through the loopback (`--consumer`).** Just before `v4l2sink`, a
//!   bench-only probe writes the PTS into the first 16 luma bytes (see
//!   [`crate::stamp`]); an in-process consumer pipeline reading the loopback
//!   decodes it and records its arrival on the same clock: camera src pad ->
//!   consumer receipt.
//!
//! All times are nanoseconds since the recorder's epoch (`Instant`,
//! CLOCK_MONOTONIC), shared by both pipelines.
//!
//! **Memory.** Every in-window sample is kept by default (a benchmark run is
//! finite). An open-ended run sets a retention ([`Recorder::set_retention`]):
//! samples older than it are dropped as new ones arrive, so memory stays
//! bounded; whole-window frame counts ([`WindowCounts`]) are kept regardless.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use gst::prelude::*;

use crate::pipeline::{MediaPipeline, names};
use crate::stamp;

const UNSET: u64 = u64::MAX;
/// `GST_CUDA_MEMORY_TYPE_NAME`.
const CUDA_MEMORY_TYPE: &str = "gst.cuda.memory";
/// Pending src timestamps older than this are dropped (frames lost on the way).
const PENDING_MAX_AGE_NS: u64 = 2_000_000_000;

#[derive(Debug, Default)]
struct Inner {
    /// PTS -> src-pad time, for frames in flight.
    pending: HashMap<u64, u64>,
    src_times: VecDeque<u64>,
    src_pts: VecDeque<u64>,
    sink_times: VecDeque<u64>,
    added_latency_ns: VecDeque<u64>,
    added_at_pad_ns: VecDeque<u64>,
    consumer_times: VecDeque<u64>,
    consumer_latency_ns: VecDeque<u64>,
    /// Receipt time of each `consumer_latency_ns` entry (for trimming).
    consumer_latency_t: VecDeque<u64>,
    last_consumer_stamp: Option<u64>,
    /// In-window frame counts, never trimmed.
    counts: WindowCounts,
    /// The latest retention cutoff that dropped a sample.
    trimmed_before: Option<u64>,
    totals: Totals,
}

/// Pops the front of `times` (and the same number from each paired series)
/// while it is older than `cutoff`; `true` if anything was dropped.
fn trim_series(times: &mut VecDeque<u64>, paired: &mut [&mut VecDeque<u64>], cutoff: u64) -> bool {
    let mut cut = false;
    while times.front().is_some_and(|&t| t < cutoff) {
        times.pop_front();
        for p in paired.iter_mut() {
            p.pop_front();
        }
        cut = true;
    }
    cut
}

/// `[from, to)` index range of a sorted time series.
fn index_range(times: &VecDeque<u64>, from: u64, to: u64) -> std::ops::Range<usize> {
    let a = times.partition_point(|&t| t < from);
    let b = times.partition_point(|&t| t < to).max(a);
    a..b
}

fn collect(v: &VecDeque<u64>, r: std::ops::Range<usize>) -> Vec<u64> {
    v.range(r).copied().collect()
}

impl Inner {
    /// Drops every sample older than `cutoff` (retention).
    fn trim(&mut self, cutoff: u64) {
        let a = trim_series(&mut self.src_times, &mut [&mut self.src_pts], cutoff);
        let b = trim_series(
            &mut self.sink_times,
            &mut [&mut self.added_latency_ns, &mut self.added_at_pad_ns],
            cutoff,
        );
        let c = trim_series(&mut self.consumer_times, &mut [], cutoff);
        let d = trim_series(
            &mut self.consumer_latency_t,
            &mut [&mut self.consumer_latency_ns],
            cutoff,
        );
        if a || b || c || d {
            self.trimmed_before = Some(self.trimmed_before.map_or(cutoff, |p| p.max(cutoff)));
        }
    }
}

/// Frames recorded inside the measurement window, over the whole window:
/// unlike the per-frame series, never dropped by the retention.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WindowCounts {
    pub src: u64,
    pub sink: u64,
    pub consumer: u64,
}

/// Whole-run counters (warm-up included).
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Totals {
    pub src_buffers: u64,
    pub sink_buffers: u64,
    pub consumer_buffers: u64,
    pub src_without_pts: u64,
    /// Sink buffers whose PTS had no src record (should stay 0).
    pub sink_unmatched: u64,
    /// Consumer buffers with no valid stamp.
    pub consumer_unstamped: u64,
    /// Consumer buffers whose stamp had no src record (aged out).
    pub consumer_unmatched: u64,
    /// Consumer buffers repeating the previous frame's stamp: v4l2loopback
    /// re-delivered a frame. Not a drop; excluded from consumer fps.
    pub consumer_duplicates: u64,
    pub stamps_written: u64,
    /// Stamps whose `GstBuffer` was not writable, so `make_mut` copied the
    /// buffer *struct* (metadata + memory references; no pixel data).
    pub stamp_buffer_copies: u64,
    /// Stamps whose writable map replaced the buffer's memory, i.e. GStreamer
    /// copied the pixel data (memory shared or not mappable writable, or
    /// several memories merged). Detected by comparing memory identity
    /// before and after `map_writable`.
    pub stamp_memory_copies: u64,
    pub stamp_time_ns_total: u64,
}

/// Raw samples inside the measurement window (or the part of it still
/// retained, see [`Samples::retained_from_ns`]).
#[derive(Debug, Clone, Default)]
pub struct Samples {
    pub src_times: Vec<u64>,
    pub src_pts: Vec<u64>,
    /// Sink-ready times (after the host map).
    pub sink_times: Vec<u64>,
    /// src pad -> sink-ready (primary).
    pub added_latency_ns: Vec<u64>,
    /// src pad -> sink pad arrival (before the host map; secondary).
    pub added_at_pad_ns: Vec<u64>,
    pub consumer_times: Vec<u64>,
    pub consumer_latency_ns: Vec<u64>,
    pub totals: Totals,
    /// `Some(t)` when the retention dropped older samples: every series above
    /// covers `[t, window end)` only. `None`: the whole window is here.
    pub retained_from_ns: Option<u64>,
    /// In-window frame counts over the whole window (never trimmed).
    pub counts: WindowCounts,
}

/// Identity of a buffer's memory: `(n_memory, first GstMemory pointer)`.
/// A writable map that copies or merges memory replaces it, changing this.
fn memory_identity(b: &gst::BufferRef) -> (usize, usize) {
    let n = b.n_memory();
    let first = if n > 0 {
        b.peek_memory(0).as_ptr() as usize
    } else {
        0
    };
    (n, first)
}

/// Shared state written by the pad probes.
#[derive(Debug)]
pub struct Recorder {
    epoch: Instant,
    window_start: AtomicU64,
    window_end: AtomicU64,
    first_sink: AtomicU64,
    first_consumer: AtomicU64,
    sink_memory: OnceLock<String>,
    stamp: bool,
    consumer: bool,
    /// Samples older than this many ns are dropped (`UNSET`: keep all).
    retain_ns: AtomicU64,
    inner: Mutex<Inner>,
}

impl Recorder {
    /// `consumer`: a loopback consumer will be attached, so the sink stamps
    /// frames and keeps src records for the consumer to match.
    pub fn new(consumer: bool) -> Arc<Self> {
        Arc::new(Recorder {
            epoch: Instant::now(),
            window_start: AtomicU64::new(UNSET),
            window_end: AtomicU64::new(UNSET),
            first_sink: AtomicU64::new(UNSET),
            first_consumer: AtomicU64::new(UNSET),
            sink_memory: OnceLock::new(),
            stamp: consumer,
            consumer,
            retain_ns: AtomicU64::new(UNSET),
            inner: Mutex::new(Inner::default()),
        })
    }

    /// The probe-shared state. A poisoned lock is recovered rather than
    /// unwrapped: probes run in GStreamer's C callbacks, where a panic aborts
    /// the process (and the samples are plain counters, valid either way).
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// Opens the measurement window `[start, end)`; samples outside it are
    /// not kept (counters still run).
    pub fn set_window(&self, start_ns: u64, end_ns: u64) {
        self.window_end.store(end_ns, Ordering::SeqCst);
        self.window_start.store(start_ns, Ordering::SeqCst);
    }

    /// Keeps only the samples of the last `retain_ns` (relative to the newest
    /// sample), for open-ended runs; `None` (the default) keeps every sample.
    /// Whole-window counts and [`Totals`] are unaffected.
    pub fn set_retention(&self, retain_ns: Option<u64>) {
        self.retain_ns
            .store(retain_ns.unwrap_or(UNSET), Ordering::SeqCst);
    }

    /// Applies the retention after a sample at time `t` was added.
    fn retain(&self, g: &mut Inner, t: u64) {
        let r = self.retain_ns.load(Ordering::Relaxed);
        if r != UNSET && t > r {
            g.trim(t - r);
        }
    }

    pub fn window(&self) -> Option<(u64, u64)> {
        let s = self.window_start.load(Ordering::SeqCst);
        let e = self.window_end.load(Ordering::SeqCst);
        (s != UNSET).then_some((s, e))
    }

    fn in_window(&self, t: u64) -> bool {
        let s = self.window_start.load(Ordering::Relaxed);
        s != UNSET && t >= s && t < self.window_end.load(Ordering::Relaxed)
    }

    /// Time the first buffer reached the loopback sink, if any.
    pub fn first_sink_ns(&self) -> Option<u64> {
        let v = self.first_sink.load(Ordering::SeqCst);
        (v != UNSET).then_some(v)
    }

    /// Time the consumer received its first buffer, if any.
    pub fn first_consumer_ns(&self) -> Option<u64> {
        let v = self.first_consumer.load(Ordering::SeqCst);
        (v != UNSET).then_some(v)
    }

    /// Allocator of the first buffer that reached the loopback sink, e.g.
    /// `GstCudaPoolAllocator (gst.cuda.memory)` or a system-memory allocator.
    pub fn sink_memory(&self) -> Option<String> {
        self.sink_memory.get().cloned()
    }

    pub fn totals(&self) -> Totals {
        self.lock().totals
    }

    fn on_src(&self, t: u64, pts: Option<u64>) {
        let mut g = self.lock();
        g.totals.src_buffers += 1;
        let Some(pts) = pts else {
            g.totals.src_without_pts += 1;
            return;
        };
        g.pending.insert(pts, t);
        if g.totals.src_buffers.is_multiple_of(64) {
            g.pending
                .retain(|_, &mut ts| t.saturating_sub(ts) < PENDING_MAX_AGE_NS);
        }
        if self.in_window(t) {
            g.src_times.push_back(t);
            g.src_pts.push_back(pts);
            g.counts.src += 1;
            self.retain(&mut g, t);
        }
    }

    /// `t_pad`: arrival at the sink pad; `t`: after the host map (ready).
    fn on_sink(&self, t_pad: u64, t: u64, pts: Option<u64>) {
        let _ = self
            .first_sink
            .compare_exchange(UNSET, t, Ordering::SeqCst, Ordering::SeqCst);
        let mut g = self.lock();
        g.totals.sink_buffers += 1;
        let src_t = match pts {
            // Keep the record for the consumer to match; it ages out otherwise.
            Some(p) if self.consumer => g.pending.get(&p).copied(),
            Some(p) => g.pending.remove(&p),
            None => None,
        };
        let Some(src_t) = src_t else {
            g.totals.sink_unmatched += 1;
            return;
        };
        if self.in_window(t) {
            g.sink_times.push_back(t);
            g.added_latency_ns.push_back(t.saturating_sub(src_t));
            g.added_at_pad_ns.push_back(t_pad.saturating_sub(src_t));
            g.counts.sink += 1;
            self.retain(&mut g, t);
        }
    }

    fn on_stamp(&self, elapsed_ns: u64, buffer_copied: bool, memory_copied: bool) {
        let mut g = self.lock();
        g.totals.stamps_written += 1;
        g.totals.stamp_time_ns_total += elapsed_ns;
        g.totals.stamp_buffer_copies += u64::from(buffer_copied);
        g.totals.stamp_memory_copies += u64::from(memory_copied);
    }

    fn on_consumer(&self, t: u64, stamp: Option<u64>) {
        let _ = self
            .first_consumer
            .compare_exchange(UNSET, t, Ordering::SeqCst, Ordering::SeqCst);
        let mut g = self.lock();
        g.totals.consumer_buffers += 1;
        if stamp.is_some() && stamp == g.last_consumer_stamp {
            g.totals.consumer_duplicates += 1;
            return;
        }
        g.last_consumer_stamp = stamp;
        let in_window = self.in_window(t);
        if in_window {
            g.consumer_times.push_back(t);
            g.counts.consumer += 1;
            self.retain(&mut g, t);
        }
        let Some(pts) = stamp else {
            g.totals.consumer_unstamped += 1;
            return;
        };
        match g.pending.remove(&pts) {
            Some(src_t) => {
                if in_window {
                    g.consumer_latency_ns.push_back(t.saturating_sub(src_t));
                    g.consumer_latency_t.push_back(t);
                }
            }
            None => g.totals.consumer_unmatched += 1,
        }
    }

    /// Snapshot of everything recorded so far. When the retention dropped
    /// samples, every series is cut at the same instant
    /// ([`Samples::retained_from_ns`]), so they describe one common span.
    pub fn samples(&self) -> Samples {
        let g = self.lock();
        let from = g.trimmed_before.unwrap_or(0);
        let all = |v: &VecDeque<u64>| index_range(v, from, u64::MAX);
        let src = all(&g.src_times);
        let sink = all(&g.sink_times);
        let consumer_latency = all(&g.consumer_latency_t);
        Samples {
            src_times: collect(&g.src_times, src.clone()),
            src_pts: collect(&g.src_pts, src),
            sink_times: collect(&g.sink_times, sink.clone()),
            added_latency_ns: collect(&g.added_latency_ns, sink.clone()),
            added_at_pad_ns: collect(&g.added_at_pad_ns, sink),
            consumer_times: collect(&g.consumer_times, all(&g.consumer_times)),
            consumer_latency_ns: collect(&g.consumer_latency_ns, consumer_latency),
            totals: g.totals,
            retained_from_ns: g.trimmed_before,
            counts: g.counts,
        }
    }

    /// The samples recorded in `[from, to)` (each series by its own time:
    /// src-pad, sink-ready or consumer receipt). For a periodic status line:
    /// consecutive calls with adjoining ranges never count a sample twice.
    /// Totals and counts are the current whole-run values.
    pub fn samples_between(&self, from: u64, to: u64) -> Samples {
        let g = self.lock();
        let src = index_range(&g.src_times, from, to);
        let sink = index_range(&g.sink_times, from, to);
        let consumer_latency = index_range(&g.consumer_latency_t, from, to);
        Samples {
            src_times: collect(&g.src_times, src.clone()),
            src_pts: collect(&g.src_pts, src),
            sink_times: collect(&g.sink_times, sink.clone()),
            added_latency_ns: collect(&g.added_latency_ns, sink.clone()),
            added_at_pad_ns: collect(&g.added_at_pad_ns, sink),
            consumer_times: collect(&g.consumer_times, index_range(&g.consumer_times, from, to)),
            consumer_latency_ns: collect(&g.consumer_latency_ns, consumer_latency),
            totals: g.totals,
            retained_from_ns: g.trimmed_before,
            counts: g.counts,
        }
    }

    /// PTS of the last in-window frame that left the camera before `t`, so a
    /// PTS gap spanning a status boundary is counted in the later window.
    pub fn last_src_pts_before(&self, t: u64) -> Option<u64> {
        let g = self.lock();
        let i = g.src_times.partition_point(|&x| x < t);
        i.checked_sub(1).and_then(|i| g.src_pts.get(i).copied())
    }

    /// `(sent, delivered)`: frames that left the camera src pad in
    /// `[from, to)`, and how many of those have reached the sink so far
    /// (matched by PTS: a sink sample's src time is its time minus its added
    /// latency). Attributes each frame to the moment it left the camera,
    /// whichever window it reached the sink in.
    pub fn delivery_between(&self, from: u64, to: u64) -> (u64, u64) {
        let g = self.lock();
        let sent = index_range(&g.src_times, from, to).len() as u64;
        let first = g.sink_times.partition_point(|&t| t < from);
        let delivered = g
            .sink_times
            .range(first..)
            .zip(g.added_latency_ns.range(first..))
            .filter(|&(&t, &lat)| (from..to).contains(&(t - lat)))
            .count() as u64;
        (sent, delivered)
    }

    /// Installs the src-pad and sink-pad probes on the main pipeline.
    pub fn attach_main(self: &Arc<Self>, p: &MediaPipeline) -> Result<(), String> {
        let src_pad = p
            .element(names::CAMERA)
            .and_then(|e| e.static_pad("src"))
            .ok_or("camera src pad missing")?;
        let rec = Arc::clone(self);
        src_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let t = rec.now_ns();
            let pts = info.buffer().and_then(|b| b.pts()).map(|p| p.nseconds());
            rec.on_src(t, pts);
            gst::PadProbeReturn::Ok
        });

        let sink_pad = p
            .element(names::LOOPBACK)
            .and_then(|e| e.static_pad("sink"))
            .ok_or("loopback sink pad missing")?;
        let rec = Arc::clone(self);
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let t_pad = rec.now_ns();
            let Some(buffer) = info.buffer_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            let pts = buffer.pts().map(|p| p.nseconds());
            // Wait for pending GPU work + device->host copy (see module docs).
            drop(buffer.map_readable());
            let t = rec.now_ns();
            rec.on_sink(t_pad, t, pts);
            if rec.sink_memory.get().is_none() {
                let desc = (0..buffer.n_memory())
                    .map(|i| {
                        let m = buffer.peek_memory(i);
                        let alloc = m
                            .allocator()
                            .map(|a| a.type_().name().to_string())
                            .unwrap_or_else(|| "?".into());
                        if m.is_type(CUDA_MEMORY_TYPE) {
                            format!("{alloc} ({CUDA_MEMORY_TYPE})")
                        } else {
                            alloc
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = rec.sink_memory.set(desc);
            }
            if rec.stamp
                && let Some(pts) = pts
            {
                let t0 = Instant::now();
                let buffer_copied = !buffer.is_writable();
                let b = buffer.make_mut();
                let before = memory_identity(b);
                if let Ok(mut map) = b.map_writable() {
                    stamp::write_stamp(map.as_mut_slice(), pts);
                }
                let memory_copied = memory_identity(b) != before;
                rec.on_stamp(t0.elapsed().as_nanos() as u64, buffer_copied, memory_copied);
            }
            gst::PadProbeReturn::Ok
        });
        Ok(())
    }

    /// Installs the receipt probe on the consumer pipeline's sink.
    pub fn attach_consumer(self: &Arc<Self>, p: &MediaPipeline) -> Result<(), String> {
        let pad = p
            .element(names::CONSUMER_SINK)
            .and_then(|e| e.static_pad("sink"))
            .ok_or("consumer sink pad missing")?;
        let rec = Arc::clone(self);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let t = rec.now_ns();
            let stamp = info
                .buffer()
                .and_then(|b| b.map_readable().ok())
                .and_then(|m| stamp::read_stamp(m.as_slice()));
            rec.on_consumer(t, stamp);
            gst::PadProbeReturn::Ok
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_identity_tracks_copies() {
        gst::init().unwrap();
        let mut buf = gst::Buffer::with_size(64).unwrap();
        let b = buf.get_mut().unwrap();
        let before = memory_identity(b);
        drop(b.map_writable().unwrap());
        assert_eq!(
            memory_identity(b),
            before,
            "exclusive memory: mapped in place"
        );

        // A second buffer sharing the memory makes it non-exclusive: the
        // writable map has to copy it.
        let shared = buf.copy();
        let b = buf.get_mut().unwrap();
        let before = memory_identity(b);
        drop(b.map_writable().unwrap());
        assert_ne!(memory_identity(b), before, "shared memory: copied on map");
        drop(shared);

        // Two memories are merged into one on a writable map.
        let mut two = gst::Buffer::new();
        {
            let t = two.get_mut().unwrap();
            t.append_memory(gst::Memory::with_size(8));
            t.append_memory(gst::Memory::with_size(8));
        }
        let t = two.get_mut().unwrap();
        let before = memory_identity(t);
        drop(t.map_writable().unwrap());
        assert_ne!(memory_identity(t), before);
    }

    #[test]
    fn matches_by_pts_and_respects_window() {
        let r = Recorder::new(false);
        r.on_src(100, Some(1));
        r.on_sink(140, 150, Some(1)); // before window: counted, not sampled
        r.set_window(1_000, 2_000);
        r.on_src(1_000, Some(2));
        r.on_src(1_010, Some(3));
        r.on_sink(1_004, 1_005, Some(2));
        r.on_sink(1_020, 1_030, Some(3));
        r.on_sink(1_040, 1_040, Some(99)); // unknown PTS
        let s = r.samples();
        assert_eq!(s.added_latency_ns, vec![5, 20]);
        assert_eq!(s.added_at_pad_ns, vec![4, 10]);
        assert_eq!(s.src_pts, vec![2, 3]);
        assert_eq!(s.totals.sink_buffers, 4);
        assert_eq!(s.totals.sink_unmatched, 1);
        assert_eq!(r.first_sink_ns(), Some(150));
    }

    #[test]
    fn samples_between_adjoining_ranges_count_once() {
        let r = Recorder::new(false);
        r.set_window(0, u64::MAX - 1);
        r.on_src(10, Some(1));
        r.on_sink(12, 15, Some(1));
        let a = r.samples_between(0, 20);
        assert_eq!(a.added_latency_ns, vec![5]);
        assert_eq!(a.src_times, vec![10]);
        r.on_src(20, Some(2));
        r.on_src(30, Some(3));
        r.on_sink(31, 33, Some(3));
        let b = r.samples_between(20, 40);
        assert_eq!(b.src_times, vec![20, 30]);
        assert_eq!(b.sink_times, vec![33]);
        assert_eq!(b.added_latency_ns, vec![3]);
        assert_eq!(b.totals.sink_buffers, 2);
        assert!(r.samples_between(40, 50).sink_times.is_empty());
        assert_eq!(r.samples().sink_times.len(), 2, "the full record is kept");
        assert_eq!(r.samples().retained_from_ns, None);
        assert_eq!(r.last_src_pts_before(20), Some(1));
        assert_eq!(r.last_src_pts_before(31), Some(3));
        assert_eq!(r.last_src_pts_before(10), None);
    }

    /// A frame that leaves the camera in one status window and reaches the
    /// sink in the next counts as delivered for the window it left in; a
    /// frame that never arrives is a drop of the window it left in.
    #[test]
    fn delivery_is_attributed_to_the_window_a_frame_left_in() {
        let r = Recorder::new(false);
        r.set_window(0, u64::MAX - 1);
        r.on_src(90, Some(1));
        r.on_src(95, Some(2)); // never reaches the sink
        r.on_src(99, Some(3));
        r.on_sink(100, 104, Some(3)); // crosses the boundary at 100
        r.on_sink(101, 105, Some(1));
        r.on_src(150, Some(4));
        r.on_sink(151, 152, Some(4));
        assert_eq!(r.delivery_between(0, 100), (3, 2));
        assert_eq!(r.delivery_between(100, 200), (1, 1));
        // The naive per-window difference would be 3-0 and then 1-3.
        assert_eq!(r.samples_between(0, 100).sink_times.len(), 0);
        assert_eq!(r.samples_between(100, 200).sink_times.len(), 3);
    }

    #[test]
    fn retention_bounds_the_series_and_keeps_counts() {
        let r = Recorder::new(true);
        r.set_window(0, u64::MAX - 1);
        r.set_retention(Some(100));
        for i in 0..1_000u64 {
            let t = i * 10;
            r.on_src(t, Some(i));
            r.on_sink(t + 1, t + 2, Some(i));
            r.on_consumer(t + 3, Some(i));
        }
        let s = r.samples();
        // Newest sample at 9_993: everything before 9_893 is gone, and all
        // series are cut at the same instant.
        let from = s.retained_from_ns.expect("trimmed");
        assert_eq!(from, 9_893);
        assert!(s.src_times.iter().all(|&t| t >= from));
        assert!(s.src_times.len() <= 11 && s.sink_times.len() <= 11);
        assert_eq!(s.src_times.len(), s.src_pts.len());
        assert_eq!(s.sink_times.len(), s.added_latency_ns.len());
        assert_eq!(s.sink_times.len(), s.added_at_pad_ns.len());
        assert_eq!(s.consumer_times.len(), s.consumer_latency_ns.len());
        assert_eq!(
            s.counts,
            WindowCounts {
                src: 1_000,
                sink: 1_000,
                consumer: 1_000
            }
        );
        assert_eq!(s.totals.src_buffers, 1_000);
        assert!(s.added_latency_ns.iter().all(|&l| l == 2));
        // Recent ranges stay answerable.
        assert_eq!(r.delivery_between(9_900, 9_990), (9, 9));
        let g = r.lock();
        assert!(g.src_times.len() <= 11 && g.consumer_latency_t.len() <= 11);
    }

    #[test]
    fn no_retention_keeps_everything() {
        let r = Recorder::new(false);
        r.set_window(0, u64::MAX - 1);
        for i in 0..500u64 {
            r.on_src(i * 10, Some(i));
            r.on_sink(i * 10 + 1, i * 10 + 2, Some(i));
        }
        let s = r.samples();
        assert_eq!(s.retained_from_ns, None);
        assert_eq!((s.src_times.len(), s.sink_times.len()), (500, 500));
        assert_eq!(s.counts.src, 500);
    }

    #[test]
    fn consumer_matches_stamp_after_sink() {
        let r = Recorder::new(true);
        r.set_window(0, 10_000);
        r.on_src(10, Some(7));
        r.on_sink(20, 20, Some(7));
        r.on_consumer(50, Some(7));
        r.on_consumer(55, Some(7)); // loopback re-delivered the same frame
        r.on_consumer(60, None);
        r.on_consumer(70, Some(7)); // not adjacent, already consumed
        let s = r.samples();
        assert_eq!(s.added_latency_ns, vec![10]);
        assert_eq!(s.consumer_latency_ns, vec![40]);
        assert_eq!(s.consumer_times.len(), 3);
        assert_eq!(s.totals.consumer_duplicates, 1);
        assert_eq!(s.totals.consumer_unstamped, 1);
        assert_eq!(s.totals.consumer_unmatched, 1);
    }
}
