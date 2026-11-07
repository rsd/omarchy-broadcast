//! Per-buffer instrumentation of the ingest and output pipelines.
//!
//! The stream runs in two pipelines joined by a latest-frame slot
//! (`docs/adr/0009-ingest-process-output.md`): one ingest pipeline per
//! source, rebuilt on every switch, and one output pipeline paced by its own
//! clock. The output buffer's PTS is the output tick (`n x period`), not the
//! camera's, so a frame is followed across the slot through the output's
//! emit observer ([`crate::output::EmitObserver`]):
//!
//! * a probe on each ingest's `v4l2src` src pad records when every camera
//!   frame left the camera, keyed by `(source id, camera PTS)` (the PTS is
//!   unchanged through the ingest pipeline, so the slot entry carries it);
//! * the emit observer reports, for every output tick, its output PTS, what
//!   it emitted (fresh, repeat, placeholder) and, for a camera frame, its
//!   source id and camera PTS. A bounded side table maps the output PTS to
//!   that emission and to the camera src-pad time of a fresh frame;
//! * a probe on the output's `v4l2sink` sink pad records every output frame
//!   on arrival (output frame rate) and, after mapping the buffer readable,
//!   its sink-ready time.
//!
//! **Added latency** = sink-ready time - camera src-pad time, for fresh
//! frames only (a repeat or placeholder carries no new camera frame): the
//! delay our stages add over an app reading the camera directly, which
//! receives the same buffer at the src-pad moment. It includes the wait for
//! the next output tick (between zero and one output period).
//!
//! "Sink-ready" matters: `cudadownload` hands `v4l2sink` buffers that are
//! still `GstCudaMemory` (system-memory caps, device-resident data), and CUDA
//! work is asynchronous, so at pad arrival the kernels and the device->host
//! copy may not have run yet; they complete when the buffer is first mapped.
//! The sink probe therefore maps the buffer readable (the same work
//! `v4l2sink`'s own map would trigger next) and takes the timestamp after it.
//! The plain pad-arrival delta is kept as a secondary figure.
//!
//! The emit observer runs on the output clock thread after the buffer was
//! handed to `appsrc`, so the sink probe may see a buffer before its emission
//! is reported. Both sides are joined by output PTS, whichever comes first.
//!
//! **Through the loopback (`--consumer`).** With the stamp enabled, the sink
//! probe writes the output PTS into the first 16 luma bytes (see
//! [`crate::stamp`]); an in-process consumer pipeline reading the loopback
//! decodes it and records its arrival on the same clock: camera src pad ->
//! consumer receipt, for fresh frames.
//!
//! **Switch gaps.** [`Recorder::begin_switch`] opens a switch; the emit
//! observer counts the repeats and placeholders emitted until the first
//! fresh frame of the new source closes it ([`SwitchTracker`]). Only
//! emissions `appsrc` accepted count: a refused buffer never reached the
//! sink, so it neither closes a switch nor adds to its gap. Placeholders
//! emitted in the window while no switch is open are counted apart
//! ([`WindowCounts::placeholders_outside_switches`]): a source that stops
//! delivering shows there.
//!
//! All times are nanoseconds since the recorder's epoch (`Instant`,
//! CLOCK_MONOTONIC), shared by every pipeline.
//!
//! **Memory.** Every in-window sample is kept by default (a benchmark run is
//! finite). An open-ended run sets a retention ([`Recorder::set_retention`]):
//! samples older than it are dropped as new ones arrive, so memory stays
//! bounded; whole-window frame counts ([`WindowCounts`]) are kept regardless.
//! The join tables hold frames in flight only and age out after
//! [`PENDING_MAX_AGE_NS`].

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use gst::prelude::*;
use serde::Serialize;

use crate::clock::TickKind;
use crate::output::{EmitInfo, EmitObserver};
use crate::pipeline::{MediaPipeline, names};
use crate::stamp;

const UNSET: u64 = u64::MAX;
/// `GST_CUDA_MEMORY_TYPE_NAME`.
const CUDA_MEMORY_TYPE: &str = "gst.cuda.memory";
/// Join-table entries older than this are dropped (frames never emitted,
/// never delivered, or never read by the consumer).
pub const PENDING_MAX_AGE_NS: u64 = 2_000_000_000;
/// Join tables are pruned once every this many insertions.
const PRUNE_EVERY: u64 = 64;

/// One output tick, as the observer reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EmitRec {
    kind: TickKind,
    /// Camera src-pad time of the frame, for a fresh frame whose src record
    /// was found.
    src_t: Option<u64>,
    /// When the buffer was handed to `appsrc`.
    t: u64,
}

/// A buffer seen at the sink before its emission was reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SinkArrival {
    t_pad: u64,
    t_ready: u64,
}

/// One completed source switch: from the request to the first fresh frame
/// of the new source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SwitchGap {
    /// The new source's id.
    pub target: u64,
    pub requested_ns: u64,
    /// When the output emitted the new source's first frame.
    pub first_fresh_ns: u64,
    /// Repeats emitted between the request and that frame.
    pub repeats: u64,
    /// Placeholders emitted between the request and that frame.
    pub placeholders: u64,
    /// Fresh frames of the previous source emitted after the request
    /// (before it was stopped).
    pub previous_fresh: u64,
}

impl SwitchGap {
    pub fn gap_ns(&self) -> u64 {
        self.first_fresh_ns.saturating_sub(self.requested_ns)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenSwitch {
    requested_ns: u64,
    /// `None` until the new source has an id.
    target: Option<u64>,
    repeats: u64,
    placeholders: u64,
    previous_fresh: u64,
}

/// Accounts one source switch at a time from the output's emissions. Pure:
/// fed times and tick kinds, so it is tested without a pipeline.
#[derive(Debug, Default, Clone)]
pub struct SwitchTracker {
    open: Option<OpenSwitch>,
}

impl SwitchTracker {
    /// Opens a switch requested at `requested_ns`. Returns `true` when a
    /// previous switch was still open (it is abandoned).
    pub fn begin(&mut self, requested_ns: u64) -> bool {
        self.open
            .replace(OpenSwitch {
                requested_ns,
                target: None,
                repeats: 0,
                placeholders: 0,
                previous_fresh: 0,
            })
            .is_some()
    }

    /// Names the source whose first fresh frame closes the open switch.
    pub fn set_target(&mut self, source_id: u64) {
        if let Some(o) = &mut self.open {
            o.target = Some(source_id);
        }
    }

    /// Drops the open switch (the new source did not start).
    pub fn cancel(&mut self) {
        self.open = None;
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Accounts one emission at `t`; returns the switch it completes.
    pub fn on_emit(&mut self, kind: TickKind, source: Option<u64>, t: u64) -> Option<SwitchGap> {
        let o = self.open.as_mut()?;
        match kind {
            TickKind::Fresh if o.target.is_some() && source == o.target => {
                let gap = SwitchGap {
                    target: source.unwrap_or_default(),
                    requested_ns: o.requested_ns,
                    first_fresh_ns: t,
                    repeats: o.repeats,
                    placeholders: o.placeholders,
                    previous_fresh: o.previous_fresh,
                };
                self.open = None;
                Some(gap)
            }
            TickKind::Fresh => {
                o.previous_fresh += 1;
                None
            }
            TickKind::Repeat => {
                o.repeats += 1;
                None
            }
            TickKind::Placeholder => {
                o.placeholders += 1;
                None
            }
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// `(source id, camera PTS)` -> src-pad time, for frames in flight.
    pending_src: HashMap<(u64, u64), u64>,
    /// Output PTS -> emission, until the sink (and, with a consumer, the
    /// consumer) has seen it.
    emits: HashMap<u64, EmitRec>,
    /// Output PTS -> sink times, for buffers seen before their emission.
    sink_wait: HashMap<u64, SinkArrival>,
    src_times: VecDeque<u64>,
    src_source: VecDeque<u64>,
    src_pts: VecDeque<u64>,
    /// Arrival at the sink pad, every output frame.
    sink_times: VecDeque<u64>,
    /// Sink-ready time of each fresh frame with a latency sample.
    fresh_times: VecDeque<u64>,
    added_latency_ns: VecDeque<u64>,
    added_at_pad_ns: VecDeque<u64>,
    consumer_times: VecDeque<u64>,
    consumer_latency_ns: VecDeque<u64>,
    /// Receipt time of each `consumer_latency_ns` entry (for trimming).
    consumer_latency_t: VecDeque<u64>,
    last_consumer_stamp: Option<u64>,
    switch: SwitchTracker,
    switches_done: VecDeque<SwitchGap>,
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
        let a = trim_series(
            &mut self.src_times,
            &mut [&mut self.src_source, &mut self.src_pts],
            cutoff,
        );
        let b = trim_series(&mut self.sink_times, &mut [], cutoff);
        let c = trim_series(
            &mut self.fresh_times,
            &mut [&mut self.added_latency_ns, &mut self.added_at_pad_ns],
            cutoff,
        );
        let d = trim_series(&mut self.consumer_times, &mut [], cutoff);
        let e = trim_series(
            &mut self.consumer_latency_t,
            &mut [&mut self.consumer_latency_ns],
            cutoff,
        );
        if a || b || c || d || e {
            self.trimmed_before = Some(self.trimmed_before.map_or(cutoff, |p| p.max(cutoff)));
        }
    }

    /// Drops join-table entries older than [`PENDING_MAX_AGE_NS`] at `now`.
    fn prune(&mut self, now: u64) {
        let fresh = |t: u64| now.saturating_sub(t) < PENDING_MAX_AGE_NS;
        self.pending_src.retain(|_, &mut t| fresh(t));
        self.emits.retain(|_, r| fresh(r.t));
        self.sink_wait.retain(|_, a| fresh(a.t_pad));
    }

    /// The snapshot of the series in `[from, to)`, each by its own time.
    fn samples(&self, from: u64, to: u64) -> Samples {
        let src = index_range(&self.src_times, from, to);
        let fresh = index_range(&self.fresh_times, from, to);
        Samples {
            src_times: collect(&self.src_times, src.clone()),
            src_source: collect(&self.src_source, src.clone()),
            src_pts: collect(&self.src_pts, src),
            sink_times: collect(&self.sink_times, index_range(&self.sink_times, from, to)),
            fresh_times: collect(&self.fresh_times, fresh.clone()),
            added_latency_ns: collect(&self.added_latency_ns, fresh.clone()),
            added_at_pad_ns: collect(&self.added_at_pad_ns, fresh),
            consumer_times: collect(
                &self.consumer_times,
                index_range(&self.consumer_times, from, to),
            ),
            consumer_latency_ns: collect(
                &self.consumer_latency_ns,
                index_range(&self.consumer_latency_t, from, to),
            ),
            totals: self.totals,
            retained_from_ns: self.trimmed_before,
            counts: self.counts,
        }
    }
}

/// Frames recorded inside the measurement window, over the whole window:
/// unlike the per-frame series, never dropped by the retention.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WindowCounts {
    /// Frames that left a camera src pad.
    pub src: u64,
    /// Output frames that reached the sink pad.
    pub sink: u64,
    /// Fresh output frames with an added-latency sample.
    pub fresh_with_latency: u64,
    pub consumer: u64,
    /// Placeholders `appsrc` accepted while no source switch was open: the
    /// output had no frame to send outside a switch gap.
    pub placeholders_outside_switches: u64,
}

/// Whole-run counters (warm-up included).
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Totals {
    pub src_buffers: u64,
    pub src_without_pts: u64,
    /// Ticks the output clock reported (pushed or not).
    pub emits: u64,
    /// Ticks `appsrc` refused (never reach the sink).
    pub emits_not_pushed: u64,
    /// Fresh emissions whose camera src-pad record was not found (should
    /// stay 0: the PTS changed in the ingest, or the record aged out).
    pub fresh_without_src: u64,
    pub sink_buffers: u64,
    pub sink_without_pts: u64,
    /// Consumer buffers with no valid stamp (every buffer when the stamp is
    /// not applied).
    pub consumer_buffers: u64,
    pub consumer_unstamped: u64,
    /// Consumer buffers whose stamp had no emission record (aged out).
    pub consumer_unmatched: u64,
    /// Consumer buffers repeating the previous buffer's stamp: v4l2loopback
    /// re-delivered a frame. Not a drop; excluded from consumer fps. Only
    /// detectable with the stamp.
    pub consumer_duplicates: u64,
    pub stamps_written: u64,
    /// Stamps whose `GstBuffer` was not writable, so `make_mut` copied the
    /// buffer *struct* (metadata + memory references; no pixel data).
    pub stamp_buffer_copies: u64,
    /// Stamps whose writable map replaced the buffer's memory, i.e. GStreamer
    /// copied the pixel data (memory shared, e.g. with the slot's frame, or
    /// not mappable writable, or several memories merged). Detected by
    /// comparing memory identity before and after `map_writable`.
    pub stamp_memory_copies: u64,
    pub stamp_time_ns_total: u64,
    /// Switches opened while the previous one was still waiting for its
    /// first fresh frame, and a switch still waiting when the measurement
    /// ended ([`Recorder::abandon_open_switch`]).
    pub switches_abandoned: u64,
}

/// Raw samples inside the measurement window (or the part of it still
/// retained, see [`Samples::retained_from_ns`]).
#[derive(Debug, Clone, Default)]
pub struct Samples {
    /// Camera src-pad times, every source.
    pub src_times: Vec<u64>,
    /// The source of each `src_times` entry.
    pub src_source: Vec<u64>,
    pub src_pts: Vec<u64>,
    /// Arrival at the output sink pad, every output frame.
    pub sink_times: Vec<u64>,
    /// Sink-ready times of fresh frames (after the host map).
    pub fresh_times: Vec<u64>,
    /// Camera src pad -> sink-ready, fresh frames (primary).
    pub added_latency_ns: Vec<u64>,
    /// Camera src pad -> sink pad arrival, fresh frames (before the host
    /// map; secondary).
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

impl Samples {
    /// Camera src-pad times and PTS of one source.
    pub fn source(&self, source_id: u64) -> (Vec<u64>, Vec<u64>) {
        self.src_source
            .iter()
            .zip(self.src_times.iter().zip(&self.src_pts))
            .filter(|(s, _)| **s == source_id)
            .map(|(_, (&t, &p))| (t, p))
            .unzip()
    }
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

/// Shared state written by the pad probes and the emit observer.
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
    /// Insertions into the join tables, for periodic pruning.
    inserts: AtomicU64,
    inner: Mutex<Inner>,
}

impl Recorder {
    /// `consumer`: a loopback consumer will be attached, so emission records
    /// are kept for it to match. `stamp`: the sink probe writes the output
    /// PTS into each frame for the consumer to read (measurement runs only;
    /// it alters 16 luma pixels).
    pub fn new(consumer: bool, stamp: bool) -> Arc<Self> {
        Arc::new(Recorder {
            epoch: Instant::now(),
            window_start: AtomicU64::new(UNSET),
            window_end: AtomicU64::new(UNSET),
            first_sink: AtomicU64::new(UNSET),
            first_consumer: AtomicU64::new(UNSET),
            sink_memory: OnceLock::new(),
            stamp: consumer && stamp,
            consumer,
            retain_ns: AtomicU64::new(UNSET),
            inserts: AtomicU64::new(0),
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
        self.ns_at(Instant::now())
    }

    /// `t` in nanoseconds since the recorder's epoch (0 before it).
    pub fn ns_at(&self, t: Instant) -> u64 {
        u64::try_from(t.saturating_duration_since(self.epoch).as_nanos()).unwrap_or(u64::MAX)
    }

    /// Whether the sink probe stamps frames for the consumer.
    pub fn stamps(&self) -> bool {
        self.stamp
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

    /// Prunes the join tables once every [`PRUNE_EVERY`] insertions.
    fn maybe_prune(&self, g: &mut Inner, now: u64) {
        if self
            .inserts
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(PRUNE_EVERY)
        {
            g.prune(now);
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

    /// Time the first buffer reached the output sink, if any.
    pub fn first_sink_ns(&self) -> Option<u64> {
        let v = self.first_sink.load(Ordering::SeqCst);
        (v != UNSET).then_some(v)
    }

    /// Time the consumer received its first buffer, if any.
    pub fn first_consumer_ns(&self) -> Option<u64> {
        let v = self.first_consumer.load(Ordering::SeqCst);
        (v != UNSET).then_some(v)
    }

    /// Allocator of the first buffer that reached the output sink, e.g.
    /// `GstCudaPoolAllocator (gst.cuda.memory)` or a system-memory allocator.
    pub fn sink_memory(&self) -> Option<String> {
        self.sink_memory.get().cloned()
    }

    pub fn totals(&self) -> Totals {
        self.lock().totals
    }

    /// Opens a source switch requested at `requested_ns` (see
    /// [`SwitchTracker`]).
    pub fn begin_switch(&self, requested_ns: u64) {
        let mut g = self.lock();
        if g.switch.begin(requested_ns) {
            g.totals.switches_abandoned += 1;
        }
    }

    /// Names the new source of the open switch.
    pub fn switch_target(&self, source_id: u64) {
        self.lock().switch.set_target(source_id);
    }

    /// Drops the open switch (the new source did not start).
    pub fn cancel_switch(&self) {
        self.lock().switch.cancel();
    }

    /// Ends the measurement with a switch still open (its first frame not
    /// yet emitted): it is dropped and counted as abandoned
    /// ([`Totals::switches_abandoned`]). Returns whether there was one.
    pub fn abandon_open_switch(&self) -> bool {
        let mut g = self.lock();
        let open = g.switch.is_open();
        if open {
            g.switch.cancel();
            g.totals.switches_abandoned += 1;
        }
        open
    }

    /// The switches completed since the last call.
    pub fn take_switches(&self) -> Vec<SwitchGap> {
        self.lock().switches_done.drain(..).collect()
    }

    fn on_src(&self, source_id: u64, t: u64, pts: Option<u64>) {
        let mut g = self.lock();
        g.totals.src_buffers += 1;
        let Some(pts) = pts else {
            g.totals.src_without_pts += 1;
            return;
        };
        g.pending_src.insert((source_id, pts), t);
        self.maybe_prune(&mut g, t);
        if self.in_window(t) {
            g.src_times.push_back(t);
            g.src_source.push_back(source_id);
            g.src_pts.push_back(pts);
            g.counts.src += 1;
            self.retain(&mut g, t);
        }
    }

    /// One output tick: `pts` is the output PTS, `origin` the emitted
    /// frame's `(source id, camera PTS)` (`None` for a placeholder), `t`
    /// when it was handed to `appsrc`.
    fn on_emit(
        &self,
        pts: u64,
        kind: TickKind,
        origin: Option<(u64, Option<u64>)>,
        t: u64,
        pushed: bool,
    ) {
        let mut g = self.lock();
        g.totals.emits += 1;
        if !pushed {
            // Never reached the sink: no part of a switch gap either.
            g.totals.emits_not_pushed += 1;
            return;
        }
        if let Some(gap) = g.switch.on_emit(kind, origin.map(|o| o.0), t) {
            g.switches_done.push_back(gap);
        }
        if kind == TickKind::Placeholder && !g.switch.is_open() && self.in_window(t) {
            g.counts.placeholders_outside_switches += 1;
        }
        let src_t = match (kind, origin) {
            (TickKind::Fresh, Some((source, Some(cam_pts)))) => {
                g.pending_src.remove(&(source, cam_pts))
            }
            _ => None,
        };
        if kind == TickKind::Fresh && src_t.is_none() {
            g.totals.fresh_without_src += 1;
        }
        let rec = EmitRec { kind, src_t, t };
        match g.sink_wait.remove(&pts) {
            Some(arrival) => {
                self.join(&mut g, rec, arrival);
                if self.consumer {
                    g.emits.insert(pts, rec);
                }
            }
            None => {
                g.emits.insert(pts, rec);
            }
        }
        self.maybe_prune(&mut g, t);
    }

    /// `t_pad`: arrival at the sink pad; `t_ready`: after the host map.
    fn on_sink(&self, t_pad: u64, t_ready: u64, pts: Option<u64>) {
        let _ = self
            .first_sink
            .compare_exchange(UNSET, t_pad, Ordering::SeqCst, Ordering::SeqCst);
        let mut g = self.lock();
        g.totals.sink_buffers += 1;
        if self.in_window(t_pad) {
            g.sink_times.push_back(t_pad);
            g.counts.sink += 1;
            self.retain(&mut g, t_pad);
        }
        let Some(pts) = pts else {
            g.totals.sink_without_pts += 1;
            return;
        };
        let arrival = SinkArrival { t_pad, t_ready };
        // Keep the record for the consumer to match; it ages out otherwise.
        let rec = if self.consumer {
            g.emits.get(&pts).copied()
        } else {
            g.emits.remove(&pts)
        };
        match rec {
            Some(rec) => self.join(&mut g, rec, arrival),
            None => {
                g.sink_wait.insert(pts, arrival);
                self.maybe_prune(&mut g, t_pad);
            }
        }
    }

    /// An emission and its sink arrival: a latency sample for a fresh frame.
    fn join(&self, g: &mut Inner, rec: EmitRec, a: SinkArrival) {
        let (TickKind::Fresh, Some(src_t)) = (rec.kind, rec.src_t) else {
            return;
        };
        if self.in_window(a.t_ready) {
            g.fresh_times.push_back(a.t_ready);
            g.added_latency_ns
                .push_back(a.t_ready.saturating_sub(src_t));
            g.added_at_pad_ns.push_back(a.t_pad.saturating_sub(src_t));
            g.counts.fresh_with_latency += 1;
            self.retain(g, a.t_ready);
        }
    }

    fn on_stamp(&self, elapsed_ns: u64, buffer_copied: bool, memory_copied: bool) {
        let mut g = self.lock();
        g.totals.stamps_written += 1;
        g.totals.stamp_time_ns_total += elapsed_ns;
        g.totals.stamp_buffer_copies += u64::from(buffer_copied);
        g.totals.stamp_memory_copies += u64::from(memory_copied);
    }

    /// `stamp`: the output PTS the consumer decoded, if any.
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
        match g.emits.remove(&pts) {
            Some(rec) => {
                if let (TickKind::Fresh, Some(src_t), true) = (rec.kind, rec.src_t, in_window) {
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
        g.samples(g.trimmed_before.unwrap_or(0), u64::MAX)
    }

    /// The samples recorded in `[from, to)` (each series by its own time:
    /// src pad, sink arrival, sink-ready or consumer receipt). For a periodic
    /// status line: consecutive calls with adjoining ranges never count a
    /// sample twice. Totals and counts are the current whole-run values.
    pub fn samples_between(&self, from: u64, to: u64) -> Samples {
        self.lock().samples(from, to)
    }

    /// Installs the src-pad probe on an ingest pipeline's camera, recording
    /// its frames as source `source_id`.
    pub fn attach_camera(
        self: &Arc<Self>,
        p: &MediaPipeline,
        source_id: u64,
    ) -> Result<(), String> {
        let src_pad = p
            .element(names::CAMERA)
            .and_then(|e| e.static_pad("src"))
            .ok_or("camera src pad missing")?;
        let rec = Arc::clone(self);
        src_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let t = rec.now_ns();
            let pts = info.buffer().and_then(|b| b.pts()).map(|p| p.nseconds());
            rec.on_src(source_id, t, pts);
            gst::PadProbeReturn::Ok
        });
        Ok(())
    }

    /// The observer to install on the output ([`crate::output::Output::set_observer`]).
    pub fn observer(self: &Arc<Self>) -> EmitObserver {
        let rec = Arc::clone(self);
        Arc::new(move |e: &EmitInfo| {
            rec.on_emit(
                e.pts_ns,
                e.kind,
                e.origin.map(|o| (o.source_id, o.source_pts_ns)),
                rec.ns_at(e.emitted_at),
                e.pushed,
            );
        })
    }

    /// Installs the sink-pad probe on the output pipeline.
    pub fn attach_output(self: &Arc<Self>, p: &MediaPipeline) -> Result<(), String> {
        let sink_pad = p
            .element(names::LOOPBACK)
            .and_then(|e| e.static_pad("sink"))
            .ok_or("output sink pad missing")?;
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
        let stamped = self.stamp;
        pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let t = rec.now_ns();
            let stamp = if stamped {
                info.buffer()
                    .and_then(|b| b.map_readable().ok())
                    .and_then(|m| stamp::read_stamp(m.as_slice()))
            } else {
                None
            };
            rec.on_consumer(t, stamp);
            gst::PadProbeReturn::Ok
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TickKind::{Fresh, Placeholder, Repeat};

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

    /// Output PTS 0, 1, 2... at 10 ns per tick in these tests.
    const SRC: u64 = 7;

    #[test]
    fn fresh_latency_joins_camera_time_through_the_side_table() {
        let r = Recorder::new(false, false);
        r.set_window(1_000, 10_000);
        // Before the window: counted, not sampled.
        r.on_src(SRC, 100, Some(500));
        r.on_emit(0, Fresh, Some((SRC, Some(500))), 110, true);
        r.on_sink(115, 120, Some(0));
        // In the window: two camera frames, one thinned out (superseded).
        r.on_src(SRC, 1_000, Some(600));
        r.on_src(SRC, 1_010, Some(616));
        r.on_emit(10, Fresh, Some((SRC, Some(616))), 1_020, true);
        r.on_sink(1_022, 1_030, Some(10));
        // A repeat and a placeholder reach the sink but carry no sample.
        r.on_emit(20, Repeat, Some((SRC, Some(616))), 1_040, true);
        r.on_sink(1_041, 1_042, Some(20));
        r.on_emit(30, Placeholder, None, 1_050, true);
        r.on_sink(1_051, 1_052, Some(30));
        let s = r.samples();
        assert_eq!(s.added_latency_ns, vec![20], "1_030 - 1_010");
        assert_eq!(s.added_at_pad_ns, vec![12]);
        assert_eq!(s.fresh_times, vec![1_030]);
        assert_eq!(s.sink_times, vec![1_022, 1_041, 1_051]);
        assert_eq!(s.src_times, vec![1_000, 1_010]);
        assert_eq!(s.src_pts, vec![600, 616]);
        assert_eq!(
            s.counts,
            WindowCounts {
                src: 2,
                sink: 3,
                fresh_with_latency: 1,
                consumer: 0,
                // No switch open: the placeholder is counted.
                placeholders_outside_switches: 1,
            }
        );
        assert_eq!(s.totals.sink_buffers, 4);
        assert_eq!(s.totals.emits, 4);
        assert_eq!(s.totals.fresh_without_src, 0);
        assert_eq!(r.first_sink_ns(), Some(115));
        // Matched records leave the tables; the thinned frame ages out.
        let g = r.lock();
        assert!(g.emits.is_empty() && g.sink_wait.is_empty());
        assert_eq!(g.pending_src.len(), 1);
    }

    /// The sink probe may run before the observer reports the emission.
    #[test]
    fn sink_before_emit_is_joined_when_the_emission_arrives() {
        let r = Recorder::new(false, false);
        r.set_window(0, 10_000);
        r.on_src(SRC, 100, Some(1));
        r.on_sink(130, 140, Some(33));
        assert!(r.samples().added_latency_ns.is_empty());
        r.on_emit(33, Fresh, Some((SRC, Some(1))), 125, true);
        let s = r.samples();
        assert_eq!(s.added_latency_ns, vec![40]);
        assert_eq!(s.added_at_pad_ns, vec![30]);
        let g = r.lock();
        assert!(g.emits.is_empty() && g.sink_wait.is_empty());
    }

    /// Sources are told apart: the same camera PTS from two sources maps to
    /// each source's own src-pad time.
    #[test]
    fn camera_records_are_keyed_per_source() {
        let r = Recorder::new(false, false);
        r.set_window(0, 10_000);
        r.on_src(1, 100, Some(0));
        r.on_src(2, 200, Some(0));
        r.on_emit(0, Fresh, Some((2, Some(0))), 210, true);
        r.on_sink(211, 215, Some(0));
        r.on_emit(10, Fresh, Some((9, Some(0))), 220, true);
        r.on_sink(221, 225, Some(10));
        let s = r.samples();
        assert_eq!(s.added_latency_ns, vec![15]);
        assert_eq!(s.totals.fresh_without_src, 1);
        assert_eq!(s.source(1), (vec![100], vec![0]));
        assert_eq!(s.source(2), (vec![200], vec![0]));
    }

    #[test]
    fn refused_pushes_are_not_expected_at_the_sink() {
        let r = Recorder::new(false, false);
        r.on_emit(0, Placeholder, None, 10, false);
        assert_eq!(r.totals().emits_not_pushed, 1);
        assert!(r.lock().emits.is_empty());
    }

    #[test]
    fn join_tables_age_out() {
        let r = Recorder::new(true, true);
        r.set_window(0, u64::MAX - 1);
        let step = 10_000_000; // 10 ms
        for i in 0..1_000u64 {
            let t = i * step;
            // Every other camera frame is never emitted; no consumer reads.
            r.on_src(SRC, t, Some(i));
            if i % 2 == 0 {
                r.on_emit(i, Fresh, Some((SRC, Some(i))), t + 1, true);
                r.on_sink(t + 2, t + 3, Some(i));
            }
        }
        let g = r.lock();
        // 2 s of 10 ms frames, plus up to one pruning interval.
        let bound = (PENDING_MAX_AGE_NS / step + PRUNE_EVERY) as usize;
        assert!(g.pending_src.len() <= bound, "{}", g.pending_src.len());
        assert!(g.emits.len() <= bound, "{}", g.emits.len());
    }

    #[test]
    fn samples_between_adjoining_ranges_count_once() {
        let r = Recorder::new(false, false);
        r.set_window(0, u64::MAX - 1);
        r.on_src(SRC, 10, Some(1));
        r.on_emit(0, Fresh, Some((SRC, Some(1))), 12, true);
        r.on_sink(13, 15, Some(0));
        let a = r.samples_between(0, 20);
        assert_eq!(a.added_latency_ns, vec![5]);
        assert_eq!(a.src_times, vec![10]);
        r.on_src(SRC, 20, Some(2));
        r.on_src(SRC, 30, Some(3));
        r.on_emit(1, Fresh, Some((SRC, Some(3))), 31, true);
        r.on_sink(32, 33, Some(1));
        let b = r.samples_between(20, 40);
        assert_eq!(b.src_times, vec![20, 30]);
        assert_eq!(b.sink_times, vec![32]);
        assert_eq!(b.added_latency_ns, vec![3]);
        assert_eq!(b.totals.sink_buffers, 2);
        assert!(r.samples_between(40, 50).sink_times.is_empty());
        assert_eq!(r.samples().sink_times.len(), 2, "the full record is kept");
        assert_eq!(r.samples().retained_from_ns, None);
    }

    #[test]
    fn retention_bounds_the_series_and_keeps_counts() {
        let r = Recorder::new(true, true);
        r.set_window(0, u64::MAX - 1);
        r.set_retention(Some(100));
        for i in 0..1_000u64 {
            let t = i * 10;
            r.on_src(SRC, t, Some(i));
            r.on_emit(i, Fresh, Some((SRC, Some(i))), t, true);
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
        assert_eq!(s.src_times.len(), s.src_source.len());
        assert_eq!(s.fresh_times.len(), s.added_latency_ns.len());
        assert_eq!(s.fresh_times.len(), s.added_at_pad_ns.len());
        assert_eq!(s.consumer_times.len(), s.consumer_latency_ns.len());
        assert_eq!(
            s.counts,
            WindowCounts {
                src: 1_000,
                sink: 1_000,
                fresh_with_latency: 1_000,
                consumer: 1_000,
                placeholders_outside_switches: 0,
            }
        );
        assert_eq!(s.totals.src_buffers, 1_000);
        assert!(s.added_latency_ns.iter().all(|&l| l == 2));
        assert!(s.consumer_latency_ns.iter().all(|&l| l == 3));
        let g = r.lock();
        assert!(g.src_times.len() <= 11 && g.consumer_latency_t.len() <= 11);
    }

    #[test]
    fn no_retention_keeps_everything() {
        let r = Recorder::new(false, false);
        r.set_window(0, u64::MAX - 1);
        for i in 0..500u64 {
            r.on_src(SRC, i * 10, Some(i));
            r.on_emit(i, Fresh, Some((SRC, Some(i))), i * 10 + 1, true);
            r.on_sink(i * 10 + 1, i * 10 + 2, Some(i));
        }
        let s = r.samples();
        assert_eq!(s.retained_from_ns, None);
        assert_eq!((s.src_times.len(), s.sink_times.len()), (500, 500));
        assert_eq!(s.counts.src, 500);
    }

    #[test]
    fn consumer_matches_output_stamp_after_sink() {
        let r = Recorder::new(true, true);
        r.set_window(0, 10_000);
        r.on_src(SRC, 10, Some(7));
        r.on_emit(100, Fresh, Some((SRC, Some(7))), 15, true);
        r.on_sink(18, 20, Some(100));
        r.on_consumer(50, Some(100));
        r.on_consumer(55, Some(100)); // loopback re-delivered the same frame
        r.on_consumer(60, None);
        r.on_consumer(70, Some(100)); // not adjacent, already consumed
        // A repeat reaches the consumer: counted, no latency sample.
        r.on_emit(200, Repeat, Some((SRC, Some(7))), 80, true);
        r.on_sink(81, 82, Some(200));
        r.on_consumer(90, Some(200));
        let s = r.samples();
        assert_eq!(s.added_latency_ns, vec![10]);
        assert_eq!(s.consumer_latency_ns, vec![40]);
        assert_eq!(s.consumer_times.len(), 4);
        assert_eq!(s.totals.consumer_duplicates, 1);
        assert_eq!(s.totals.consumer_unstamped, 1);
        assert_eq!(s.totals.consumer_unmatched, 1);
    }

    #[test]
    fn switch_gap_counts_repeats_and_placeholders_until_the_new_source() {
        let mut s = SwitchTracker::default();
        assert_eq!(s.on_emit(Fresh, Some(1), 0), None, "no switch open");
        assert!(!s.begin(1_000));
        // The old source is still running until the switch stops it.
        assert_eq!(s.on_emit(Fresh, Some(1), 1_010), None);
        s.set_target(2);
        for t in [1_020, 1_030] {
            assert_eq!(s.on_emit(Repeat, Some(1), t), None);
        }
        for t in [1_040, 1_050, 1_060] {
            assert_eq!(s.on_emit(Placeholder, None, t), None);
        }
        let gap = s.on_emit(Fresh, Some(2), 1_070).expect("closed");
        assert_eq!(
            gap,
            SwitchGap {
                target: 2,
                requested_ns: 1_000,
                first_fresh_ns: 1_070,
                repeats: 2,
                placeholders: 3,
                previous_fresh: 1,
            }
        );
        assert_eq!(gap.gap_ns(), 70);
        assert!(!s.is_open());
        assert_eq!(s.on_emit(Fresh, Some(2), 1_080), None);
    }

    #[test]
    fn switch_without_target_never_closes_and_can_be_cancelled() {
        let mut s = SwitchTracker::default();
        s.begin(0);
        // Before the new source has an id, no fresh frame closes the switch.
        assert_eq!(s.on_emit(Fresh, Some(5), 10), None);
        assert!(s.begin(20), "the open switch is abandoned");
        s.cancel();
        assert!(!s.is_open());
        assert_eq!(s.on_emit(Repeat, Some(5), 30), None);
    }

    #[test]
    fn only_an_accepted_push_closes_a_switch() {
        let r = Recorder::new(false, false);
        r.begin_switch(100);
        r.switch_target(3);
        // The new source's first frame, refused by appsrc: not delivered.
        r.on_emit(0, Fresh, Some((3, Some(0))), 110, false);
        r.on_emit(1, Placeholder, None, 120, false);
        assert!(r.take_switches().is_empty());
        r.on_emit(2, Repeat, Some((3, Some(0))), 130, true);
        r.on_emit(3, Fresh, Some((3, Some(1))), 140, true);
        let done = r.take_switches();
        assert_eq!(done.len(), 1);
        // The refused emissions are not part of the gap.
        assert_eq!(
            (
                done[0].first_fresh_ns,
                done[0].repeats,
                done[0].placeholders
            ),
            (140, 1, 0)
        );
        assert_eq!(r.totals().emits_not_pushed, 2);
    }

    #[test]
    fn placeholders_are_counted_outside_switch_gaps_only() {
        let r = Recorder::new(false, false);
        r.set_window(1_000, 10_000);
        // Before the window: not counted.
        r.on_emit(0, Placeholder, None, 500, true);
        // In the window, no switch open: counted (refused ones are not).
        r.on_emit(1, Placeholder, None, 1_000, true);
        r.on_emit(2, Placeholder, None, 1_010, false);
        // During a switch gap: not counted.
        r.begin_switch(1_100);
        r.switch_target(4);
        r.on_emit(3, Placeholder, None, 1_120, true);
        r.on_emit(4, Fresh, Some((4, Some(0))), 1_130, true);
        // After the gap closed: counted again.
        r.on_emit(5, Placeholder, None, 1_140, true);
        // A failed switch is cancelled: what follows is outside a gap.
        r.begin_switch(1_200);
        r.cancel_switch();
        r.on_emit(6, Placeholder, None, 1_210, true);
        // After the window: not counted.
        r.on_emit(7, Placeholder, None, 10_000, true);
        assert_eq!(r.samples().counts.placeholders_outside_switches, 3);
    }

    #[test]
    fn a_switch_open_at_the_end_is_abandoned() {
        let r = Recorder::new(false, false);
        assert!(!r.abandon_open_switch());
        r.begin_switch(100);
        r.switch_target(5);
        assert!(r.abandon_open_switch());
        assert_eq!(r.totals().switches_abandoned, 1);
        // Closed: a late first frame no longer completes it.
        r.on_emit(0, Fresh, Some((5, Some(0))), 200, true);
        assert!(r.take_switches().is_empty());
        assert!(!r.abandon_open_switch());
    }

    #[test]
    fn recorder_reports_completed_switches_once() {
        let r = Recorder::new(false, false);
        r.begin_switch(100);
        r.switch_target(3);
        r.on_emit(0, Placeholder, None, 110, true);
        r.on_emit(1, Fresh, Some((3, Some(0))), 150, true);
        let done = r.take_switches();
        assert_eq!(done.len(), 1);
        assert_eq!((done[0].gap_ns(), done[0].placeholders), (50, 1));
        assert!(r.take_switches().is_empty());
        r.begin_switch(200);
        r.begin_switch(210);
        assert_eq!(r.totals().switches_abandoned, 1);
    }
}
