//! `obc-bench`: passthrough benchmark.
//!
//! Captures from the selected camera, moves frames through CUDA memory and
//! writes them to a v4l2loopback device, measuring sink fps, added latency,
//! process CPU, GPU utilisation/memory and the caps on every link. See
//! `docs/benchmarks/passthrough.md` for the method and results.
//!
//! `--duration 0` streams until SIGINT/SIGTERM instead, printing a status
//! line every 5 s (no luma stamp, no loopback consumer).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use gst::prelude::*;
use obc_core::{CameraSelector, DEFAULT_SINK_LABEL, ModeRequest, OutputFormat, SinkSelector};
use obc_pipeline::capability::choose_mode;
use obc_pipeline::controls::{
    CID_EXPOSURE_AUTO_PRIORITY, ControlGuard, CtrlSkip, get_ctrl, probe_ctrl,
};
use obc_pipeline::discovery::resolve_node_path;
use obc_pipeline::gpu::{GpuInfo, GpuSample, GpuSampler};
use obc_pipeline::instrument::{Recorder, Samples, Totals};
use obc_pipeline::pipeline::names;
use obc_pipeline::stats::{self, FpsReport, Summary};
use obc_pipeline::{
    DeviceInfo, DiscoveryRoute, GpuStage, IngestPath, LinkCaps, MediaPipeline, ModeChoice,
    PipelineConfig, all_cuda_between, list_cameras, scan_sysfs, select_camera, select_sink,
};
use serde::Serialize;

const METHOD: &str = "Primary: pad probes on the v4l2src src pad and the v4l2sink sink pad stamp each buffer with CLOCK_MONOTONIC, keyed by buffer PTS (unchanged through queue/cudaupload/cudaconvert/cudadownload); added latency = sink-ready time - src-pad time, i.e. the delay our stages add over an app reading the camera directly (which receives the same buffer at the src-pad moment). Sink-ready = after the sink probe maps the buffer readable: cudadownload hands v4l2sink GstCudaMemory-backed buffers and CUDA work is asynchronous, so kernels and the device->host copy complete on first map; the probe's map does exactly the work v4l2sink's own map would do next. added_at_sink_pad_ms (before that map) is reported as a secondary figure and under-states the cost. Via consumer: a bench-only probe writes the PTS into the first 16 luma bytes just before v4l2sink; an in-process consumer pipeline (v4l2src on the loopback ! fakesink) decodes it on arrival on the same clock: camera src pad -> consumer receipt. Limits: excludes sensor exposure and USB transfer (identical for a direct reader, so it cancels); excludes the consumer app's own render/display (no glass-to-glass measurement); the stamp alters 16 luma pixels in bench mode only. gst_latency_query is what elements declare, not a measurement.";

/// Passthrough benchmark: camera -> CUDA memory -> v4l2loopback.
#[derive(Debug, Parser)]
#[command(name = "obc-bench", version)]
struct Args {
    /// Camera: a card-name substring (case-insensitive), a USB vid:pid
    /// (exactly hhhh:hhhh), or a /dev/videoN path (or /dev/v4l/... link).
    /// Omitted: the first colour camera offering --mode exactly, else the
    /// best degraded match.
    #[arg(long, value_name = "NAME|VID:PID|/dev/videoN")]
    camera: Option<CameraSelector>,
    /// v4l2loopback card label of the output device.
    #[arg(long, default_value = DEFAULT_SINK_LABEL)]
    sink_label: String,
    /// Explicit v4l2loopback node (overrides --sink-label); must be a loopback device.
    #[arg(long)]
    sink_device: Option<PathBuf>,
    /// Requested capture mode, WxH@FPS (any format) or FORMAT:WxH@FPS
    /// (FORMAT: NV12, YUY2/YUYV, MJPEG, ...). Resolved against the camera's
    /// capabilities; a mode it does not offer degrades (logged).
    #[arg(long, default_value = "1920x1080@60")]
    mode: ModeRequest,
    /// GPU stage: passthrough | roundtrip (NV12 -> BGR -> out, in CUDA memory).
    #[arg(long, default_value = "passthrough")]
    gpu_stage: GpuStage,
    /// Loopback pixel format: nv12 | i420.
    #[arg(long, default_value = "nv12")]
    output_format: OutputFormat,
    /// Measured seconds (after warm-up). 0 streams until SIGINT/SIGTERM with
    /// a status line every 5 s (no --consumer, no luma stamp).
    #[arg(long, default_value_t = 30.0, value_parser = non_negative_secs)]
    duration: f64,
    /// Seconds after the first frame reaches the sink before measuring.
    #[arg(long, default_value_t = 3.0, value_parser = non_negative_secs)]
    warmup: f64,
    /// Write the full JSON report here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Also read the loopback with an in-process consumer (consumer fps and
    /// end-to-end latency via a luma stamp). Not with --duration 0.
    #[arg(long)]
    consumer: bool,
    /// After teardown and dropping all GStreamer/NVML objects, stay alive this
    /// many seconds (for external dGPU-suspend checks).
    #[arg(long, default_value_t = 0.0, value_parser = non_negative_secs)]
    linger_after_teardown: f64,
    /// Dump the pipeline graph (.dot, caps on every link) here once PLAYING.
    #[arg(long)]
    dot_dir: Option<PathBuf>,
    /// Camera discovery route: auto | monitor | sysfs.
    #[arg(long, default_value = "auto")]
    discovery: DiscoveryRoute,
    /// queue max-size-buffers (leaky=downstream).
    #[arg(long, default_value_t = 2)]
    queue_buffers: u32,
    /// v4l2src io-mode (mmap, dmabuf, auto, ...); checked against v4l2src's
    /// own enum before any device is touched.
    #[arg(long, default_value = "mmap")]
    io_mode: String,
    /// Skip NVML sampling.
    #[arg(long)]
    no_gpu_stats: bool,
    /// NVML device index.
    #[arg(long, default_value_t = 0)]
    gpu_index: u32,
    /// NVML sampling interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    gpu_interval_ms: u64,
    /// Seconds of GPU baseline sampling before the pipeline is built.
    #[arg(long, default_value_t = 3.0, value_parser = non_negative_secs)]
    gpu_baseline: f64,
    /// Temporarily set the camera's exposure_dynamic_framerate (UVC "exposure
    /// auto priority") to 0 so it cannot lower its frame rate in dim light;
    /// the original value is restored on every exit path that unwinds
    /// (success, error, panic, SIGINT/SIGTERM), not on SIGKILL/SIGQUIT.
    #[arg(long)]
    camera_fixed_framerate: bool,
    /// Print debug details (camera candidates, skipped camera controls).
    #[arg(short, long)]
    verbose: bool,
}

impl Args {
    /// `--duration 0`: stream until a stop signal.
    fn streaming(&self) -> bool {
        self.duration == 0.0
    }

    /// Cross-argument checks, before any device is touched.
    fn validate(&self) -> Result<()> {
        if self.streaming() && self.consumer {
            bail!(
                "--consumer cannot be combined with --duration 0: the consumer measurement needs the luma stamp, which is never applied while streaming"
            );
        }
        Ok(())
    }

    fn debug(&self, msg: impl AsRef<str>) {
        if self.verbose {
            eprintln!("debug: {}", msg.as_ref());
        }
    }
}

/// Upper bound for any seconds argument: keeps nanosecond arithmetic and
/// `Duration` conversion far from overflow.
const MAX_SECS: f64 = 7.0 * 86_400.0;

fn parse_secs(s: &str, allow_zero: bool) -> Result<f64, String> {
    let v: f64 = s.trim().parse().map_err(|e| format!("{s:?}: {e}"))?;
    let low_ok = if allow_zero { v >= 0.0 } else { v > 0.0 };
    if !v.is_finite() || !low_ok || v > MAX_SECS {
        return Err(format!(
            "{s:?}: expected seconds in {}..={MAX_SECS}",
            if allow_zero { "0" } else { "(0" }
        ));
    }
    Ok(v)
}

#[cfg(test)]
fn positive_secs(s: &str) -> Result<f64, String> {
    parse_secs(s, false)
}

fn non_negative_secs(s: &str) -> Result<f64, String> {
    parse_secs(s, true)
}

/// Set by SIGINT/SIGTERM; the run loop checks it and falls through to the
/// normal teardown, control restore and report.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_sig: libc::c_int) {
    // Async-signal-safe: a single atomic store.
    STOP.store(true, Ordering::SeqCst);
}

fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Routes SIGINT and SIGTERM to [`STOP`]. Repeats are idempotent and never
/// fall back to the default action: `timeout(1)` and a terminal both deliver
/// the same signal twice in quick succession (to the child, then to its
/// process group), and dying on the second would skip the control restore.
/// A stuck teardown is ended with SIGQUIT or SIGKILL (no restore then).
fn install_stop_handlers() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: `sa` is zero-initialised (a valid empty mask and flags)
        // before the handler, flags and mask are set; the handler only stores
        // to an atomic, which is async-signal-safe.
        let rc = unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_stop_signal as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("installing signal handler");
        }
    }
    Ok(())
}

/// Sleeps `secs`, returning early once a stop signal arrived.
fn sleep_unless_stopped(secs: f64) {
    let until = std::time::Instant::now() + Duration::from_secs_f64(secs.clamp(0.0, MAX_SECS));
    while !stop_requested() {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

#[derive(Debug, Default)]
struct BusCounters {
    qos_dropped: BTreeMap<String, u64>,
    warnings: Vec<String>,
    /// Nominal frame duration of the camera mode, to turn a QoS message's
    /// lost-time `duration` into frames (see [`account_qos_drop`]).
    frame_ns: u64,
}

impl BusCounters {
    fn new(frame_ns: u64) -> Self {
        BusCounters {
            frame_ns,
            ..BusCounters::default()
        }
    }
}

/// Adds one QoS message's drops to `entry`, an element's whole-run total.
///
/// * Elements that fill the stats (`jpegdec` and other video decoders, base
///   transforms) report `dropped` as a running total since READY in
///   `buffers` or `default` format: the latest (largest) value is kept, not
///   summed.
/// * `v4l2src` never sets the stats (they stay `undefined`): it posts one
///   message per gap in the driver's sequence numbers, with
///   `duration = lost frames x frame duration`. Those messages add
///   `round(duration / frame_ns)` frames, at least 1 (also when the duration
///   is unknown).
fn account_qos_drop(
    entry: &mut u64,
    dropped: gst::GenericFormattedValue,
    duration: Option<gst::ClockTime>,
    frame_ns: u64,
) {
    match dropped {
        gst::GenericFormattedValue::Buffers(Some(b)) => *entry = (*entry).max(*b),
        gst::GenericFormattedValue::Default(Some(d)) => *entry = (*entry).max(*d),
        _ => {
            let frames = match duration {
                Some(d) if frame_ns > 0 => {
                    (d.nseconds() as f64 / frame_ns as f64).round().max(1.0) as u64
                }
                _ => 1,
            };
            *entry = entry.saturating_add(frames);
        }
    }
}

/// Drains `bus`, waiting up to `wait` for the first message.
fn drain_bus(p: &MediaPipeline, c: &mut BusCounters, wait: Duration) -> Result<()> {
    let bus = p.bus();
    let mut timeout = gst::ClockTime::from_nseconds(wait.as_nanos() as u64);
    while let Some(msg) = bus.timed_pop(timeout) {
        timeout = gst::ClockTime::ZERO;
        use gst::MessageView;
        let src = msg
            .src()
            .map(|s| s.name().to_string())
            .unwrap_or_else(|| "?".into());
        match msg.view() {
            MessageView::Error(e) => {
                bail!(
                    "{} error from {src}: {} ({})",
                    p.pipeline().name(),
                    e.error(),
                    e.debug().map(|d| d.to_string()).unwrap_or_default()
                );
            }
            MessageView::Eos(_) => bail!("{}: unexpected EOS", p.pipeline().name()),
            MessageView::Warning(w) => {
                c.warnings.push(format!("{src}: {}", w.error()));
            }
            MessageView::Qos(q) => {
                let (_processed, dropped) = q.stats();
                let entry = c.qos_dropped.entry(src).or_default();
                account_qos_drop(entry, dropped, q.duration(), c.frame_ns);
            }
            MessageView::Latency(_) => {
                let _ = p.pipeline().recalculate_latency();
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct LatencyQuery {
    live: bool,
    min_ms: f64,
    max_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Environment {
    unix_time_s: u64,
    date_utc: String,
    gstreamer: String,
    kernel: String,
    cpu_model: String,
    online_cpus: usize,
}

#[derive(Debug, Serialize)]
struct CameraControls {
    /// Why exposure_dynamic_framerate was skipped (absent, disabled,
    /// unreadable); None when the camera exposes it.
    exposure_dynamic_framerate_skipped: Option<String>,
    /// exposure_dynamic_framerate before the run (None: skipped).
    exposure_dynamic_framerate_before: Option<i32>,
    /// Value in effect during the run.
    exposure_dynamic_framerate_during: Option<i32>,
    /// `Some` when the bench changed it: whether the original value was put
    /// back (None: unchanged, nothing to restore).
    changed_and_restored: Option<Result<(), String>>,
}

#[derive(Debug, Serialize)]
struct ConfigReport {
    camera: DeviceInfo,
    sink: DeviceInfo,
    camera_discovery: String,
    camera_discovery_note: Option<String>,
    sink_discovery: &'static str,
    camera_controls: CameraControls,
    /// No --camera: the camera was picked automatically.
    camera_auto_picked: bool,
    pipeline: PipelineConfig,
    /// The --mode request, as given.
    requested_mode: String,
    /// The camera mode chosen for it.
    mode: String,
    /// The chosen mode matches the request exactly.
    mode_exact: bool,
    /// What differs from the request when degraded.
    mode_differences: Vec<String>,
    /// How frames reach CUDA memory (gpu-direct, cpu-convert, cpu-decode).
    ingest: IngestPath,
    /// --duration 0: streamed until a stop signal.
    streaming: bool,
    duration_s: f64,
    /// Seconds actually measured: `duration_s` unless interrupted.
    measured_s: f64,
    /// The run was stopped early by SIGINT/SIGTERM; figures cover `measured_s`.
    interrupted: bool,
    warmup_s: f64,
    consumer: bool,
}

#[derive(Debug, Serialize)]
struct FpsSection {
    target: f64,
    sink: FpsReport,
    consumer: Option<FpsReport>,
    camera_src: FpsReport,
    /// Sink inter-arrival gaps > 1.5 frame durations (late or missing frames).
    sink_stalls: usize,
    /// The same at the camera src pad: stalls that originate before our stages.
    camera_src_stalls: usize,
    consumer_stalls: Option<usize>,
    /// Frames missing in the camera's own PTS sequence (camera/USB side).
    camera_missing_frames_by_pts: u64,
    /// Frames that left v4l2src in the window minus frames that reached the sink.
    pipeline_lost_frames: i64,
    /// queue overrun signals; with leaky=downstream each drops one stale buffer.
    queue_overruns: u64,
    /// QoS "dropped" totals posted on the bus, by element, whole run. v4l2src
    /// posts one message per gap in the driver's sequence numbers, without
    /// stats: its count is the sum of `round(duration / frame duration)`
    /// (at least 1 per message). Elements that report a running total
    /// (jpegdec, base transforms) contribute their latest total.
    qos_dropped_by_element: BTreeMap<String, u64>,
    /// Frames `jpegdec` dropped, whole run (CPU decode path only): JPEG data
    /// the camera sent that does not decode. Camera-side, but they count in
    /// `pipeline_lost_frames` because they leave v4l2src.
    decoder_dropped_frames: Option<u64>,
}

#[derive(Debug, Serialize)]
struct LatencySection {
    method: &'static str,
    budget_ms: f64,
    added_primary_ms: Option<Summary>,
    added_at_sink_pad_ms: Option<Summary>,
    via_consumer_ms: Option<Summary>,
    gst_latency_query: Option<LatencyQuery>,
    stamp_mean_us: Option<f64>,
    /// Stamps whose GstBuffer struct was copied (`make_mut`; pixels shared).
    stamp_buffer_copies: u64,
    /// Stamps that made GStreamer copy the pixel data (memory replaced on map).
    stamp_memory_copies: u64,
}

#[derive(Debug, Serialize)]
struct CpuSection {
    /// Whole window, % of one core (includes the consumer pipeline and NVML sampler threads).
    one_core_pct: f64,
    all_cores_pct: f64,
    /// Over 1 s segments of the window; a final segment shorter than 0.5 s
    /// is left out (its tick quantisation would dominate), not the whole figure.
    per_second_one_core_pct: Option<Summary>,
    online_cpus: usize,
    ticks_per_sec: u64,
}

#[derive(Debug, Serialize)]
struct GpuStat {
    samples: usize,
    util_gpu_pct: Option<Summary>,
    util_mem_pct: Option<Summary>,
    mem_used_mib: Option<Summary>,
    proc_mem_mib: Option<Summary>,
    proc_sm_pct: Option<Summary>,
}

#[derive(Debug, Serialize)]
struct GpuSection {
    source: &'static str,
    info: GpuInfo,
    baseline: GpuStat,
    running: GpuStat,
    /// Other processes seen on the GPU (baseline + run); device-wide util and
    /// memory include their load.
    other_gpu_pids: Vec<u32>,
    nvml_shutdown: Result<(), String>,
}

#[derive(Debug, Serialize)]
struct CapsSection {
    cuda_memory_between_upload_and_download: bool,
    /// Allocator of the buffers arriving at v4l2sink (after cudadownload).
    sink_buffer_memory: Option<String>,
    links: Vec<LinkCaps>,
    consumer_links: Vec<LinkCaps>,
}

#[derive(Debug, Serialize)]
struct TeardownSection {
    main_pipeline_null: Result<(), String>,
    consumer_pipeline_null: Option<Result<(), String>>,
    /// /dev/nvidia* fds held by this process right after teardown.
    nvidia_fds_after_teardown: Vec<String>,
    libcuda_mapped_after_teardown: bool,
    /// GStreamer's own CUDA context is destroyed at NULL (gst_cuda_context_finalize);
    /// the fds above are the CUDA driver's process-wide handles opened by cuInit,
    /// which the driver API never closes before process exit.
    note: &'static str,
    /// dGPU runtime PM status read from sysfs (a read never wakes the GPU).
    dgpu_runtime_status_after_teardown: Option<String>,
    linger_s: f64,
}

#[derive(Debug, Serialize)]
struct Criteria {
    /// Sink (and consumer, if run) overall >= 99% of target and no complete 1 s
    /// window more than 1 frame below target.
    fps_sustained: bool,
    /// Every frame the camera delivered reached the sink (and the consumer, if
    /// run): no queue overruns, src/sink counts within the frames in flight at
    /// the window edges, and no consumer frame without a src record
    /// (re-delivered duplicates are counted separately, not as drops).
    /// Separates our drops from camera-side drops.
    no_pipeline_drops: bool,
    /// p95 of primary added latency <= one frame.
    added_latency_p95_within_one_frame: bool,
    /// p99 of primary added latency <= one frame.
    added_latency_p99_within_one_frame: bool,
    cuda_memory_links: bool,
}

#[derive(Debug, Serialize)]
struct Report {
    tool: &'static str,
    version: &'static str,
    environment: Environment,
    config: ConfigReport,
    fps: FpsSection,
    latency: LatencySection,
    cpu: CpuSection,
    gpu: Option<GpuSection>,
    caps: CapsSection,
    totals: Totals,
    warnings: Vec<String>,
    teardown: TeardownSection,
    criteria: Criteria,
    /// `--duration 0` only: what the per-frame figures cover.
    #[serde(skip_serializing_if = "Option::is_none")]
    retention: Option<RetentionSection>,
}

/// Streaming (`--duration 0`) memory bound: which span the per-frame figures
/// describe. Absent from benchmark reports, which keep every sample.
#[derive(Debug, Serialize)]
struct RetentionSection {
    /// `whole_run` (nothing was dropped) or `retained_window`.
    scope: &'static str,
    /// Per-frame samples older than this are dropped while streaming.
    retain_cap_s: f64,
    /// Start of the span the per-frame figures cover, in seconds after the
    /// measurement window opened (0 for `whole_run`).
    retained_from_s: f64,
    /// Length of that span (ends with the run).
    retained_s: f64,
    /// Frames that left v4l2src / reached the sink over the whole run.
    whole_run_src_frames: u64,
    whole_run_sink_frames: u64,
    note: &'static str,
}

fn ms(ns: &[u64]) -> Vec<f64> {
    ns.iter().map(|&n| n as f64 / 1e6).collect()
}

fn gpu_stat(samples: &[&GpuSample]) -> GpuStat {
    let f = |g: &dyn Fn(&GpuSample) -> Option<f64>| -> Option<Summary> {
        let v: Vec<f64> = samples.iter().filter_map(|s| g(s)).collect();
        stats::summarize(&v)
    };
    GpuStat {
        samples: samples.len(),
        util_gpu_pct: f(&|s| Some(f64::from(s.util_gpu_pct))),
        util_mem_pct: f(&|s| Some(f64::from(s.util_mem_pct))),
        mem_used_mib: f(&|s| Some(s.mem_used_bytes as f64 / 1048576.0)),
        proc_mem_mib: f(&|s| s.proc_mem_bytes.map(|b| b as f64 / 1048576.0)),
        proc_sm_pct: f(&|s| s.proc_sm_pct.map(f64::from)),
    }
}

fn nvidia_fds() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
        for e in rd.flatten() {
            if let Ok(target) = std::fs::read_link(e.path()) {
                let t = target.to_string_lossy().into_owned();
                if t.starts_with("/dev/nvidia") {
                    out.push(format!("fd {} -> {t}", e.file_name().to_string_lossy()));
                }
            }
        }
    }
    out.sort();
    out
}

/// The NVIDIA PCI function bound to the `nvidia` driver, e.g. `0000:01:00.0`.
fn nvidia_pci_device() -> Option<PathBuf> {
    let rd = std::fs::read_dir("/sys/bus/pci/drivers/nvidia").ok()?;
    let mut devs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.len() == 12 && n.as_bytes()[4] == b':')
        })
        .collect();
    devs.sort();
    devs.into_iter().next()
}

fn runtime_status() -> Option<String> {
    let dev = nvidia_pci_device()?;
    std::fs::read_to_string(dev.join("power/runtime_status"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn libcuda_mapped() -> bool {
    std::fs::read_to_string("/proc/self/maps").is_ok_and(|m| m.contains("libcuda.so"))
}

/// Days-since-epoch to civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn environment() -> Environment {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let cpu_model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_default();
    Environment {
        unix_time_s: secs,
        date_utc: format!(
            "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        ),
        gstreamer: gst::version_string().to_string(),
        kernel: read("/proc/sys/kernel/osrelease"),
        cpu_model,
        online_cpus: stats::online_cpus(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    Done,
    Deadline,
    Interrupted,
}

fn wait_until(
    rec: &Recorder,
    pipes: &[&MediaPipeline],
    bus: &mut BusCounters,
    deadline_ns: u64,
    done: &dyn Fn() -> bool,
) -> Result<Wait> {
    loop {
        if done() {
            return Ok(Wait::Done);
        }
        if stop_requested() {
            return Ok(Wait::Interrupted);
        }
        let now = rec.now_ns();
        if now >= deadline_ns {
            return Ok(Wait::Deadline);
        }
        let step = Duration::from_nanos((deadline_ns - now).min(50_000_000));
        let per = step / pipes.len().max(1) as u32;
        for p in pipes {
            drain_bus(p, bus, per)?;
        }
    }
}

fn secs_ns(s: f64) -> u64 {
    (s.max(0.0) * 1e9) as u64
}

fn main() {
    let args = Args::parse();
    let res = run(&args);
    if let Err(e) = &res {
        eprintln!("obc-bench: {e:#}");
    }
    // All guards and pipelines are dropped by now (run returned).
    // A stop signal is the normal end of a streaming run.
    if stop_requested() && !(args.streaming() && res.is_ok()) {
        std::process::exit(130);
    }
    if res.is_err() {
        std::process::exit(1);
    }
}

/// The selected camera and how it was chosen.
struct Selected {
    camera: DeviceInfo,
    choice: ModeChoice,
    auto: bool,
}

fn select(args: &Args, cams: &mut obc_pipeline::discovery::CameraList) -> Result<Selected> {
    // A path may be a stable link (/dev/v4l/by-id/...): match the node.
    let selector = args.camera.clone().map(|s| match s {
        CameraSelector::Path(p) => CameraSelector::Path(resolve_node_path(&p)),
        other => other,
    });
    if let Some(CameraSelector::Path(p)) = &selector {
        cams.include_node(p);
    }
    if args.verbose {
        for d in &cams.devices {
            let best = d
                .capabilities
                .as_ref()
                .filter(|_| d.is_colour_camera())
                .and_then(|c| choose_mode(c, &args.mode));
            args.debug(format!(
                "candidate {d}: {}",
                best.map_or_else(|| "not a colour camera".to_string(), |c| c.to_string())
            ));
        }
    }
    let picked = select_camera(&cams.devices, selector.as_ref(), &args.mode)?;
    Ok(Selected {
        camera: picked.device.clone(),
        choice: picked.choice,
        auto: picked.auto,
    })
}

fn run(args: &Args) -> Result<()> {
    install_stop_handlers()?;
    gst::init().context("gst::init")?;
    // Validate every input before any device is touched.
    args.validate()?;
    obc_pipeline::pipeline::check_io_mode(&args.io_mode)?;
    let env = environment();

    // ---- device and mode selection (from enumerated capabilities) ----
    let mut cams = list_cameras(args.discovery, Some(&args.mode));
    let sel = select(args, &mut cams)?;
    let sink_sel = match &args.sink_device {
        Some(p) => SinkSelector::Path(p.clone()),
        None => SinkSelector::Label(args.sink_label.clone()),
    };
    let sysfs = scan_sysfs();
    let sink = select_sink(&sysfs, &sink_sel)?.clone();
    let how = match (&args.camera, sel.choice.exact) {
        (Some(s), _) => format!("selected by --camera {s}"),
        (None, true) => format!(
            "auto-picked: first colour camera offering {} exactly",
            args.mode
        ),
        (None, false) => format!(
            "auto-picked: no colour camera offers {} exactly; best degraded match",
            args.mode
        ),
    };
    println!(
        "camera: {} ({how}; via {:?}{})",
        sel.camera,
        cams.used,
        cams.note
            .as_deref()
            .map(|n| format!("; {n}"))
            .unwrap_or_default()
    );
    println!("mode:   requested {} -> {}", args.mode, sel.choice);
    println!("sink:   {sink}");

    // ---- camera frame-rate control (optional; read always, pin only if asked) ----
    // Probed with VIDIOC_QUERYCTRL first: a camera without the control (or
    // with it disabled) is skipped with a debug line only. The guard restores
    // the original value when dropped: on early returns and panics, and on
    // SIGINT/SIGTERM via the normal teardown below.
    let edf = probe_ctrl(&sel.camera.path, CID_EXPOSURE_AUTO_PRIORITY);
    let edf_skipped: Option<CtrlSkip> = edf.as_ref().err().cloned();
    if let Some(skip) = &edf_skipped {
        args.debug(format!(
            "camera control exposure_dynamic_framerate skipped: {skip}"
        ));
    }
    let edf_before = edf.as_ref().ok().map(|(_, v)| *v);
    let mut edf_guard = match &edf {
        Ok((info, v)) if args.camera_fixed_framerate && *v != 0 => {
            if info.read_only() {
                args.debug("exposure_dynamic_framerate is read-only; not pinned");
                None
            } else {
                Some(
                    ControlGuard::set(&sel.camera.path, CID_EXPOSURE_AUTO_PRIORITY, *v, 0)
                        .context("setting exposure_dynamic_framerate=0")?,
                )
            }
        }
        _ => None,
    };
    let edf_during =
        edf_before.and_then(|_| get_ctrl(&sel.camera.path, CID_EXPOSURE_AUTO_PRIORITY).ok());
    if edf_during == Some(1) {
        println!(
            "note: camera exposure_dynamic_framerate=1: it may lower its frame rate in dim light (see --camera-fixed-framerate)"
        );
    }

    run_measured(
        args,
        env,
        &cams,
        sel,
        sink,
        CameraControls {
            exposure_dynamic_framerate_skipped: edf_skipped.map(|s| s.to_string()),
            exposure_dynamic_framerate_before: edf_before,
            exposure_dynamic_framerate_during: edf_during,
            changed_and_restored: None,
        },
        &mut edf_guard,
    )
}

/// Streaming: per-frame samples older than this are dropped, so memory stays
/// bounded however long the run; the exit report describes this last span
/// (or the whole run, if shorter) and says which.
const STREAM_RETAIN_S: f64 = 600.0;

/// Streaming status: a frame that has not reached the sink this long after
/// leaving the camera counts as a pipeline drop (added latency is a few ms).
/// Each status line reports drops for frames that left the camera between
/// the previous line's cut-off and `now - DROP_GRACE_NS`.
const DROP_GRACE_NS: u64 = 1_000_000_000;

/// Camera-side drops in a status window: the PTS gaps of its frames, plus
/// the gap from the last frame before the window (`prev_pts`) to its first,
/// so a gap spanning a status boundary is counted once, in the later window.
fn window_camera_drops(prev_pts: Option<u64>, pts: &[u64], frame_ns: u64) -> u64 {
    match (prev_pts, pts.first()) {
        (Some(p), Some(_)) => {
            let mut all = Vec::with_capacity(pts.len() + 1);
            all.push(p);
            all.extend_from_slice(pts);
            stats::missing_frames_from_gaps(&all, frame_ns)
        }
        _ => stats::missing_frames_from_gaps(pts, frame_ns),
    }
}

/// Pipeline drops among `sent` frames of which `delivered` reached the sink
/// ([`Recorder::delivery_between`]). More delivered than sent is a
/// bookkeeping bug, not a measurement: it is logged (and asserted in debug
/// builds) and the negative value is shown as is, never clamped to 0.
fn window_pipeline_drops(sent: u64, delivered: u64) -> i64 {
    let drops = sent as i64 - delivered as i64;
    if drops < 0 {
        eprintln!(
            "warning: status window counts {delivered} delivered frames of {sent} sent (bookkeeping bug)"
        );
    }
    debug_assert!(drops >= 0, "delivered {delivered} > sent {sent}");
    drops
}

/// One streaming status line. `w`: the samples of the last status window;
/// `pipeline_drops` and `camera_drops` from [`window_pipeline_drops`] and
/// [`window_camera_drops`].
fn status_line(
    sel: &Selected,
    w: &Samples,
    window_s: f64,
    cpu_one_core_pct: f64,
    pipeline_drops: i64,
    camera_drops: u64,
) -> String {
    let sink_fps = w.sink_times.len() as f64 / window_s.max(1e-9);
    let p50 = stats::summarize(&ms(&w.added_latency_ns))
        .map_or("n/a".to_string(), |s| format!("{:.2} ms", s.p50));
    format!(
        "{} {} | {} {} | sink {:.1} fps | added p50 {} | drops {} (camera-side {}) | cpu {:.1}%",
        sel.camera.card,
        sel.camera.path.display(),
        sel.choice.mode,
        sel.choice.ingest,
        sink_fps,
        p50,
        pipeline_drops,
        camera_drops,
        cpu_one_core_pct
    )
}

#[allow(clippy::too_many_arguments)]
fn run_measured(
    args: &Args,
    env: Environment,
    cams: &obc_pipeline::discovery::CameraList,
    sel: Selected,
    sink: DeviceInfo,
    mut controls: CameraControls,
    edf_guard: &mut Option<ControlGuard>,
) -> Result<()> {
    let mode = sel.choice.mode;
    let streaming = args.streaming();
    let rec = Recorder::new(args.consumer);
    if streaming {
        rec.set_retention(Some(secs_ns(STREAM_RETAIN_S)));
    }
    let mut bus = BusCounters::new(mode.frame_duration_ns());

    // ---- GPU baseline (before the pipeline exists) ----
    let sampler = if args.no_gpu_stats {
        None
    } else {
        let r = Arc::clone(&rec);
        let now: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(move || r.now_ns());
        match GpuSampler::start(
            args.gpu_index,
            Duration::from_millis(args.gpu_interval_ms.max(100)),
            now,
        ) {
            Ok(s) => {
                sleep_unless_stopped(args.gpu_baseline);
                Some(s)
            }
            Err(e) => {
                bus.warnings.push(format!("GPU stats disabled: {e}"));
                None
            }
        }
    };
    let t_build = rec.now_ns();

    // ---- build + start ----
    let mut cfg = PipelineConfig::new(sel.camera.path.clone(), sink.path.clone(), mode);
    cfg.gpu_stage = args.gpu_stage;
    cfg.output = args.output_format;
    cfg.queue_buffers = args.queue_buffers;
    cfg.camera_io_mode = args.io_mode.clone();
    let main_p = MediaPipeline::build(&cfg)?;
    rec.attach_main(&main_p).map_err(|e| anyhow!(e))?;
    let overruns = Arc::new(AtomicU64::new(0));
    {
        let q = main_p.element(names::QUEUE).context("queue element")?;
        let c = Arc::clone(&overruns);
        q.connect("overrun", false, move |_| {
            c.fetch_add(1, Ordering::Relaxed);
            None
        });
    }

    let interrupted_before = || anyhow!("interrupted by signal before the measurement window");
    let outcome = (|| -> Result<_> {
        if stop_requested() {
            return Err(interrupted_before());
        }
        main_p.play()?;
        let r = Arc::clone(&rec);
        match wait_until(
            &rec,
            &[&main_p],
            &mut bus,
            rec.now_ns() + secs_ns(15.0),
            &|| r.first_sink_ns().is_some(),
        )? {
            Wait::Done => {}
            Wait::Interrupted => return Err(interrupted_before()),
            Wait::Deadline => bail!("no frame reached the loopback sink within 15 s"),
        }
        let t_first = rec.first_sink_ns().unwrap_or_default();

        let consumer_p = if args.consumer {
            let c = MediaPipeline::build_consumer(&sink.path, &mode, args.output_format)?;
            rec.attach_consumer(&c).map_err(|e| anyhow!(e))?;
            c.play()?;
            let r = Arc::clone(&rec);
            // On an early return `c` is dropped, which sets it to NULL.
            match wait_until(
                &rec,
                &[&main_p, &c],
                &mut bus,
                rec.now_ns() + secs_ns(10.0),
                &|| r.first_consumer_ns().is_some(),
            )? {
                Wait::Done => {}
                Wait::Interrupted => return Err(interrupted_before()),
                Wait::Deadline => bail!("the loopback consumer received no frame within 10 s"),
            }
            Some(c)
        } else {
            None
        };
        let pipes: Vec<&MediaPipeline> = std::iter::once(&main_p)
            .chain(consumer_p.as_ref())
            .collect();

        // Dump the graph before the window opens, so writing it costs
        // neither measured CPU nor measured frames.
        if let Some(dir) = &args.dot_dir {
            std::fs::create_dir_all(dir)?;
            let tag = format!("{}-{}", args.gpu_stage, args.output_format);
            std::fs::write(dir.join(format!("obc-main-{tag}.dot")), main_p.dot())?;
            if let Some(c) = &consumer_p {
                std::fs::write(dir.join(format!("obc-consumer-{tag}.dot")), c.dot())?;
            }
        }

        let start = t_first.max(rec.now_ns()) + secs_ns(args.warmup);
        // Streaming: an open window, closed by the stop signal.
        let mut end = if streaming {
            u64::MAX - 1
        } else {
            start + secs_ns(args.duration)
        };
        rec.set_window(start, end);
        if streaming {
            println!(
                "streaming until SIGINT/SIGTERM after {:.1} s warm-up ({mode} {}, {}, output {}); status every 5 s",
                args.warmup, sel.choice.ingest, args.gpu_stage, args.output_format
            );
        } else {
            println!(
                "measuring {:.0} s after {:.1} s warm-up ({mode} {}, {}, output {})",
                args.duration, args.warmup, sel.choice.ingest, args.gpu_stage, args.output_format
            );
        }
        if wait_until(&rec, &pipes, &mut bus, start, &|| false)? == Wait::Interrupted {
            return Err(interrupted_before());
        }
        let ticks_start = stats::self_cpu_ticks().unwrap_or(0);
        let wall_start = rec.now_ns();

        // Streaming keeps the last STREAM_RETAIN_S 1 s segments only.
        let mut per_sec: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
        let (mut last_ticks, mut last_t) = (ticks_start, wall_start);
        let report_every: u64 = if streaming {
            5_000_000_000
        } else {
            10_000_000_000
        };
        let mut next_report = wall_start + report_every;
        let (mut status_ticks, mut status_t) = (ticks_start, wall_start);
        // Pipeline drops are reported for frames that left the camera up to
        // DROP_GRACE_NS ago; this is where the previous line stopped.
        let mut drops_t = wall_start;
        let mut interrupted = false;
        for k in 1u64.. {
            // 1 s segments on a fixed grid from wall_start (no drift).
            let next = (wall_start + k * 1_000_000_000).min(end);
            if wait_until(&rec, &pipes, &mut bus, next, &|| false)? == Wait::Interrupted {
                // Close the window here: the report covers what was measured.
                interrupted = true;
                end = rec.now_ns().max(start);
                rec.set_window(start, end);
            }
            let now = rec.now_ns();
            let ticks = stats::self_cpu_ticks().unwrap_or(last_ticks);
            let seg_s = (now - last_t) as f64 / 1e9;
            if seg_s >= 0.5 {
                if streaming && per_sec.len() >= STREAM_RETAIN_S as usize {
                    per_sec.pop_front();
                }
                per_sec.push_back(
                    stats::cpu_usage(
                        ticks.saturating_sub(last_ticks),
                        stats::ticks_per_sec(),
                        seg_s,
                        stats::online_cpus(),
                    )
                    .one_core_pct,
                );
            }
            (last_ticks, last_t) = (ticks, now);
            if now >= next_report && !interrupted {
                let elapsed = (now - wall_start) as f64 / 1e9;
                if streaming {
                    let w = rec.samples_between(status_t, now);
                    let camera_drops = window_camera_drops(
                        rec.last_src_pts_before(status_t),
                        &w.src_pts,
                        mode.frame_duration_ns(),
                    );
                    let drops_to = now.saturating_sub(DROP_GRACE_NS).max(drops_t);
                    let (sent, delivered) = rec.delivery_between(drops_t, drops_to);
                    let pipeline_drops = window_pipeline_drops(sent, delivered);
                    drops_t = drops_to;
                    let window_s = (now - status_t) as f64 / 1e9;
                    let cpu = stats::cpu_usage(
                        ticks.saturating_sub(status_ticks),
                        stats::ticks_per_sec(),
                        window_s,
                        stats::online_cpus(),
                    );
                    println!(
                        "[t+{elapsed:>5.0}s] {}",
                        status_line(
                            &sel,
                            &w,
                            window_s,
                            cpu.one_core_pct,
                            pipeline_drops,
                            camera_drops
                        )
                    );
                    (status_ticks, status_t) = (ticks, now);
                } else {
                    let t = rec.totals();
                    println!(
                        "  t+{elapsed:>4.0}s  sink frames {}  consumer frames {}",
                        t.sink_buffers, t.consumer_buffers
                    );
                }
                next_report += report_every;
            }
            if interrupted || now >= end {
                break;
            }
        }
        let ticks_end = stats::self_cpu_ticks().unwrap_or(0);
        let wall_end = rec.now_ns();
        let cpu = stats::cpu_usage(
            ticks_end.saturating_sub(ticks_start),
            stats::ticks_per_sec(),
            (wall_end - wall_start) as f64 / 1e9,
            stats::online_cpus(),
        );
        let latency_q = main_p.query_latency();
        let links = main_p.links();
        let consumer_links = consumer_p.as_ref().map(|c| c.links()).unwrap_or_default();
        Ok((
            consumer_p,
            cpu,
            Vec::from(per_sec),
            latency_q,
            links,
            consumer_links,
            interrupted,
        ))
    })();

    // ---- teardown: NVML first, then consumer, then main pipeline ----
    let (gpu_samples, nvml_shutdown, gpu_info) = match sampler {
        Some(s) => {
            let info = s.info().clone();
            let (samples, res) = s.stop();
            (samples, Some(res), Some(info))
        }
        None => (Vec::new(), None, None),
    };
    let (consumer_p, cpu, per_sec, latency_q, links, consumer_links, interrupted) = match outcome {
        Ok(v) => v,
        Err(e) => {
            // The consumer (if any) was dropped inside the closure (-> NULL);
            // the caller's control guard restores on return.
            let _ = main_p.shutdown();
            return Err(e);
        }
    };
    let consumer_null = consumer_p.map(|c| c.shutdown().map_err(|e| e.to_string()));
    let samples: Samples = rec.samples();
    let (win_start, win_end) = rec.window().unwrap_or((0, 0));
    let main_null = main_p.shutdown().map_err(|e| e.to_string());
    controls.changed_and_restored = edf_guard
        .take()
        .map(|g| g.restore().map_err(|e| e.to_string()));
    let nv_fds = nvidia_fds();
    let cuda_mapped = libcuda_mapped();

    // ---- report ----
    let target = mode.fps();
    let frame_ns = mode.frame_duration_ns();
    // Streaming may have dropped old samples: per-frame figures then cover
    // the retained span [stats_start, win_end), stated in the report.
    let stats_start = samples
        .retained_from_ns
        .map_or(win_start, |f| f.max(win_start));
    let sink_fps = stats::fps_report(&samples.sink_times, stats_start, win_end, target, 1.0);
    let consumer_fps = args
        .consumer
        .then(|| stats::fps_report(&samples.consumer_times, stats_start, win_end, target, 1.0));
    let camera_fps = stats::fps_report(&samples.src_times, stats_start, win_end, target, 1.0);
    // Whole-window counts: identical to the sample counts unless trimmed.
    let lost_frames = samples.counts.src as i64 - samples.counts.sink as i64;
    let retention = streaming.then(|| RetentionSection {
        scope: if samples.retained_from_ns.is_some() {
            "retained_window"
        } else {
            "whole_run"
        },
        retain_cap_s: STREAM_RETAIN_S,
        retained_from_s: stats_start.saturating_sub(win_start) as f64 / 1e9,
        retained_s: win_end.saturating_sub(stats_start) as f64 / 1e9,
        whole_run_src_frames: samples.counts.src,
        whole_run_sink_frames: samples.counts.sink,
        note: "Streaming keeps per-frame samples for the last retain_cap_s only. scope=whole_run: the run was shorter, every figure covers it all. scope=retained_window: fps, stalls, camera_missing_frames_by_pts, latency and per_second_one_core_pct cover the last retained_s (from retained_from_s after the window opened); config.measured_s, cpu.one_core_pct, fps.pipeline_lost_frames, criteria.no_pipeline_drops, totals and qos_dropped_by_element cover the whole run.",
    });
    let added = stats::summarize(&ms(&samples.added_latency_ns));
    let via_consumer = stats::summarize(&ms(&samples.consumer_latency_ns));
    let budget_ms = frame_ns as f64 / 1e6;
    let cuda_ok = all_cuda_between(&links, names::UPLOAD, names::DOWNLOAD) == Some(true);

    let fps_ok =
        |r: &FpsReport| r.overall >= target * 0.99 && r.windows_below_target == 0 && r.windows > 0;
    let criteria = Criteria {
        fps_sustained: fps_ok(&sink_fps) && consumer_fps.as_ref().is_none_or(fps_ok),
        no_pipeline_drops: overruns.load(Ordering::Relaxed) == 0
            && lost_frames.abs() <= 3
            && consumer_fps
                .as_ref()
                .is_none_or(|c| (samples.counts.src as i64 - c.frames as i64).abs() <= 3)
            && samples.totals.consumer_unmatched == 0,
        added_latency_p95_within_one_frame: added.is_some_and(|s| s.p95 <= budget_ms),
        added_latency_p99_within_one_frame: added.is_some_and(|s| s.p99 <= budget_ms),
        cuda_memory_links: cuda_ok,
    };

    let gpu = gpu_info.map(|info| {
        let base: Vec<&GpuSample> = gpu_samples.iter().filter(|s| s.t_ns < t_build).collect();
        let mut other: Vec<u32> = gpu_samples
            .iter()
            .filter(|s| s.t_ns < t_build || (s.t_ns >= win_start && s.t_ns < win_end))
            .flat_map(|s| s.other_pids.iter().copied())
            .collect();
        other.sort_unstable();
        other.dedup();
        let run: Vec<&GpuSample> = gpu_samples
            .iter()
            .filter(|s| s.t_ns >= win_start && s.t_ns < win_end)
            .collect();
        GpuSection {
            source: "NVML (nvmlDeviceGetUtilizationRates / GetMemoryInfo / GetComputeRunningProcesses / GetProcessUtilization), the data behind nvidia-smi dmon",
            info,
            baseline: gpu_stat(&base),
            running: gpu_stat(&run),
            other_gpu_pids: other,
            nvml_shutdown: nvml_shutdown.unwrap_or(Ok(())),
        }
    });

    let report = Report {
        tool: "obc-bench",
        version: env!("CARGO_PKG_VERSION"),
        environment: env,
        config: ConfigReport {
            camera: sel.camera,
            sink,
            camera_discovery: format!("{:?}", cams.used),
            camera_discovery_note: cams.note.clone(),
            camera_controls: controls,
            camera_auto_picked: sel.auto,
            sink_discovery: "sysfs (/sys/class/video4linux/*/name, virtual parent + v4l2loopback attributes)",
            pipeline: cfg,
            requested_mode: args.mode.to_string(),
            mode: mode.to_string(),
            mode_exact: sel.choice.exact,
            mode_differences: sel.choice.differences,
            ingest: sel.choice.ingest,
            streaming,
            duration_s: args.duration,
            measured_s: win_end.saturating_sub(win_start) as f64 / 1e9,
            interrupted,
            warmup_s: args.warmup,
            consumer: args.consumer,
        },
        fps: FpsSection {
            target,
            sink_stalls: stats::count_stalls(&samples.sink_times, frame_ns, 1.5),
            camera_src_stalls: stats::count_stalls(&samples.src_times, frame_ns, 1.5),
            consumer_stalls: args
                .consumer
                .then(|| stats::count_stalls(&samples.consumer_times, frame_ns, 1.5)),
            camera_missing_frames_by_pts: stats::missing_frames_from_gaps(
                &samples.src_pts,
                frame_ns,
            ),
            pipeline_lost_frames: lost_frames,
            queue_overruns: overruns.load(Ordering::Relaxed),
            decoder_dropped_frames: (sel.choice.ingest == IngestPath::CpuDecode)
                .then(|| bus.qos_dropped.get(names::DECODE).copied().unwrap_or(0)),
            qos_dropped_by_element: std::mem::take(&mut bus.qos_dropped),
            sink: sink_fps,
            consumer: consumer_fps,
            camera_src: camera_fps,
        },
        latency: LatencySection {
            method: METHOD,
            budget_ms,
            added_primary_ms: added,
            added_at_sink_pad_ms: stats::summarize(&ms(&samples.added_at_pad_ns)),
            via_consumer_ms: via_consumer,
            gst_latency_query: latency_q.map(|(live, min, max)| LatencyQuery {
                live,
                min_ms: min as f64 / 1e6,
                max_ms: max.map(|m| m as f64 / 1e6),
            }),
            stamp_mean_us: (samples.totals.stamps_written > 0).then(|| {
                samples.totals.stamp_time_ns_total as f64
                    / samples.totals.stamps_written as f64
                    / 1e3
            }),
            stamp_buffer_copies: samples.totals.stamp_buffer_copies,
            stamp_memory_copies: samples.totals.stamp_memory_copies,
        },
        cpu: CpuSection {
            one_core_pct: cpu.one_core_pct,
            all_cores_pct: cpu.all_cores_pct,
            per_second_one_core_pct: stats::summarize(&per_sec),
            online_cpus: stats::online_cpus(),
            ticks_per_sec: stats::ticks_per_sec(),
        },
        gpu,
        caps: CapsSection {
            cuda_memory_between_upload_and_download: cuda_ok,
            sink_buffer_memory: rec.sink_memory(),
            links,
            consumer_links,
        },
        totals: samples.totals,
        warnings: std::mem::take(&mut bus.warnings),
        teardown: TeardownSection {
            main_pipeline_null: main_null,
            consumer_pipeline_null: consumer_null,
            nvidia_fds_after_teardown: nv_fds,
            libcuda_mapped_after_teardown: cuda_mapped,
            note: "GStreamer destroys its CUDA context when the pipeline reaches NULL; remaining /dev/nvidia* fds (if any) are the CUDA driver's process-wide handles from cuInit, released only at process exit",
            dgpu_runtime_status_after_teardown: runtime_status(),
            linger_s: args.linger_after_teardown,
        },
        criteria,
        retention,
    };

    if let Some(path) = &args.json {
        write_json(path, &report)?;
    }
    print_summary(&report);

    if args.linger_after_teardown > 0.0 && !stop_requested() {
        println!(
            "lingering {:.0} s after teardown (pid {}): no pipeline, no NVML; /dev/nvidia* fds held: {}",
            args.linger_after_teardown,
            std::process::id(),
            if report.teardown.nvidia_fds_after_teardown.is_empty() {
                "none".to_string()
            } else {
                report.teardown.nvidia_fds_after_teardown.join(", ")
            }
        );
        sleep_unless_stopped(args.linger_after_teardown);
        let fds = nvidia_fds();
        println!(
            "linger done; dGPU runtime_status {}; /dev/nvidia* fds held now: {}",
            runtime_status().unwrap_or_else(|| "unknown".into()),
            if fds.is_empty() {
                "none".into()
            } else {
                fds.join(", ")
            }
        );
    }
    Ok(())
}

fn write_json(path: &Path, report: &Report) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut s = serde_json::to_string_pretty(report)?;
    s.push('\n');
    std::fs::write(path, s).with_context(|| format!("writing {}", path.display()))
}

fn fmt_sum(s: &Option<Summary>, unit: &str) -> String {
    match s {
        Some(s) => format!(
            "mean {:.2}{unit}  p50 {:.2}  p95 {:.2}  p99 {:.2}  max {:.2}  (n={})",
            s.mean, s.p50, s.p95, s.p99, s.max, s.count
        ),
        None => "n/a".into(),
    }
}

fn print_summary(r: &Report) {
    let f = &r.fps;
    let fps_line = |name: &str, x: &FpsReport| {
        println!(
            "  {name:<9} {:.2} fps overall, 1 s windows min {} max {} ({} windows, {} below target)",
            x.overall,
            x.min_1s.map_or("-".into(), |v| format!("{v:.0}")),
            x.max_1s.map_or("-".into(), |v| format!("{v:.0}")),
            x.windows,
            x.windows_below_target
        )
    };
    println!(
        "== obc-bench {} ({}) {} -> {} ==",
        r.config.mode, r.config.ingest, r.config.pipeline.gpu_stage, r.config.pipeline.output
    );
    if r.config.streaming {
        println!(
            "streamed {:.1} s (measured after warm-up) until a stop signal",
            r.config.measured_s
        );
        if let Some(k) = &r.retention {
            println!(
                "per-frame figures cover: {} ({:.1} s from t+{:.1}s; samples kept for the last {:.0} s); whole run: {} frames from the camera, {} at the sink",
                k.scope,
                k.retained_s,
                k.retained_from_s,
                k.retain_cap_s,
                k.whole_run_src_frames,
                k.whole_run_sink_frames
            );
        }
    } else if r.config.interrupted {
        println!(
            "INTERRUPTED by signal: figures cover {:.1} s of the requested {:.0} s",
            r.config.measured_s, r.config.duration_s
        );
    }
    let cc = &r.config.camera_controls;
    let opt = |v: Option<i32>| v.map_or("unreadable".to_string(), |v| v.to_string());
    match &cc.changed_and_restored {
        None if cc.exposure_dynamic_framerate_skipped.is_some() => println!(
            "camera exposure_dynamic_framerate: skipped ({})",
            cc.exposure_dynamic_framerate_skipped
                .as_deref()
                .unwrap_or("")
        ),
        None => println!(
            "camera exposure_dynamic_framerate: {} (unchanged)",
            opt(cc.exposure_dynamic_framerate_before)
        ),
        Some(Ok(())) => println!(
            "camera exposure_dynamic_framerate: {} during the run, restored to {}",
            opt(cc.exposure_dynamic_framerate_during),
            opt(cc.exposure_dynamic_framerate_before)
        ),
        Some(Err(e)) => println!(
            "camera exposure_dynamic_framerate: RESTORE FAILED (still {}, was {}): {e}",
            opt(cc.exposure_dynamic_framerate_during),
            opt(cc.exposure_dynamic_framerate_before)
        ),
    }
    println!("fps (target {:.0}):", f.target);
    fps_line("camera", &f.camera_src);
    fps_line("sink", &f.sink);
    if let Some(c) = &f.consumer {
        fps_line("consumer", c);
    }
    println!(
        "  drops: camera-side {} (PTS gaps), pipeline {}, queue overruns {}, stalls camera/sink {}/{}, qos {:?}",
        f.camera_missing_frames_by_pts,
        f.pipeline_lost_frames,
        f.queue_overruns,
        f.camera_src_stalls,
        f.sink_stalls,
        f.qos_dropped_by_element
    );
    if let Some(n) = f.decoder_dropped_frames {
        println!("  jpegdec dropped {n} undecodable camera frames (whole run)");
    }
    println!("latency (budget {:.2} ms):", r.latency.budget_ms);
    println!(
        "  added (src pad -> sink ready): {}",
        fmt_sum(&r.latency.added_primary_ms, " ms")
    );
    println!(
        "  (src pad -> sink pad arrival): {}",
        fmt_sum(&r.latency.added_at_sink_pad_ms, " ms")
    );
    if r.config.consumer {
        println!(
            "  via consumer (src pad -> consumer): {}",
            fmt_sum(&r.latency.via_consumer_ms, " ms")
        );
        println!(
            "  stamp: mean {:.1} us, buffer copies {}, pixel-data copies {}",
            r.latency.stamp_mean_us.unwrap_or(0.0),
            r.latency.stamp_buffer_copies,
            r.latency.stamp_memory_copies
        );
    }
    if let Some(q) = &r.latency.gst_latency_query {
        println!(
            "  gst latency query: live={} min {:.2} ms max {}",
            q.live,
            q.min_ms,
            q.max_ms.map_or("none".into(), |m| format!("{m:.2} ms"))
        );
    }
    println!(
        "cpu: {:.1}% of one core, {:.2}% of all {} cores",
        r.cpu.one_core_pct, r.cpu.all_cores_pct, r.cpu.online_cpus
    );
    if let Some(g) = &r.gpu {
        let m = |s: &Option<Summary>| s.map_or("n/a".into(), |s| format!("{:.1}", s.mean));
        let p = |s: &Option<Summary>| s.map_or("n/a".into(), |s| format!("{:.1}", s.p95));
        println!(
            "gpu {} ({}): util baseline {}% -> running mean {}% p95 {}%; mem used baseline {} MiB -> running {} MiB; our process {} MiB",
            g.info.name,
            g.info.pci_bus_id,
            m(&g.baseline.util_gpu_pct),
            m(&g.running.util_gpu_pct),
            p(&g.running.util_gpu_pct),
            m(&g.baseline.mem_used_mib),
            m(&g.running.mem_used_mib),
            m(&g.running.proc_mem_mib),
        );
    }
    println!(
        "caps: CUDAMemory on every link upload..download: {}; buffers at v4l2sink: {}",
        r.caps.cuda_memory_between_upload_and_download,
        r.caps.sink_buffer_memory.as_deref().unwrap_or("?")
    );
    for l in &r.caps.links {
        println!(
            "  {} -> {}: {}",
            l.from,
            l.to,
            l.caps.as_deref().unwrap_or("-")
        );
    }
    println!(
        "teardown: main {:?}, /dev/nvidia* fds after: {}, libcuda still mapped: {}",
        r.teardown.main_pipeline_null,
        r.teardown.nvidia_fds_after_teardown.len(),
        r.teardown.libcuda_mapped_after_teardown
    );
    for w in &r.warnings {
        println!("warning: {w}");
    }
    let c = &r.criteria;
    println!(
        "criteria: fps_sustained={} no_pipeline_drops={} latency_p95<=1frame={} latency_p99<=1frame={} cuda_links={}",
        c.fps_sustained,
        c.no_pipeline_drops,
        c.added_latency_p95_within_one_frame,
        c.added_latency_p99_within_one_frame,
        c.cuda_memory_links
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_are_validated() {
        assert_eq!(positive_secs("20"), Ok(20.0));
        assert_eq!(non_negative_secs("0"), Ok(0.0));
        for bad in ["inf", "-inf", "NaN", "-1", "1e30", "x"] {
            assert!(non_negative_secs(bad).is_err(), "{bad}");
            assert!(positive_secs(bad).is_err(), "{bad}");
        }
        assert!(positive_secs("0").is_err());
    }

    #[test]
    fn bad_arguments_are_rejected_by_clap() {
        for (flag, v) in [
            ("--gpu-baseline", "inf"),
            ("--duration", "-5"),
            ("--warmup", "NaN"),
            ("--linger-after-teardown", "1e300"),
        ] {
            assert!(
                Args::try_parse_from(["obc-bench", flag, v]).is_err(),
                "{flag} {v}"
            );
        }
        assert!(Args::try_parse_from(["obc-bench", "--duration", "0.5"]).is_ok());
        // --camera-usb is folded into --camera.
        assert!(Args::try_parse_from(["obc-bench", "--camera-usb", "046d:0944"]).is_err());
        assert!(Args::try_parse_from(["obc-bench", "--camera", ""]).is_err());
        assert!(Args::try_parse_from(["obc-bench", "--mode", "H264:1920x1080@30"]).is_err());
    }

    #[test]
    fn camera_takes_three_forms() {
        let cam = |v: &str| {
            Args::try_parse_from(["obc-bench", "--camera", v])
                .unwrap()
                .camera
                .unwrap()
        };
        assert!(matches!(cam("/dev/video4"), CameraSelector::Path(_)));
        assert!(matches!(cam("5986:1193"), CameraSelector::Usb(_)));
        assert!(matches!(cam("MX Brio"), CameraSelector::Name(_)));
        let none = Args::try_parse_from(["obc-bench"]).unwrap();
        assert_eq!(none.camera, None, "no default camera");
        assert_eq!(none.mode, ModeRequest::default());
        assert_eq!(none.duration, 30.0, "the benchmark default is unchanged");
    }

    #[test]
    fn mode_takes_both_forms() {
        let m = |v: &str| {
            Args::try_parse_from(["obc-bench", "--mode", v])
                .unwrap()
                .mode
        };
        assert_eq!(m("1920x1080@30").format, None);
        assert_eq!(
            m("YUY2:1920x1080@60").format,
            Some(obc_core::PixelFormat::Yuy2)
        );
    }

    #[test]
    fn qos_without_stats_counts_lost_frames_from_duration() {
        gst::init().unwrap();
        let frame = 33_333_333u64; // 30 fps
        let undefined = gst::GenericFormattedValue::new(gst::Format::Undefined, -1);
        let mut n = 0;
        // v4l2src: duration = lost frames x frame duration.
        account_qos_drop(
            &mut n,
            undefined,
            Some(gst::ClockTime::from_nseconds(3 * frame)),
            frame,
        );
        assert_eq!(n, 3);
        // Rounded to the nearest frame (timestamps jitter around the grid).
        account_qos_drop(
            &mut n,
            undefined,
            Some(gst::ClockTime::from_nseconds(2 * frame - 2_000_000)),
            frame,
        );
        assert_eq!(n, 5);
        // At least one frame per message, also without a duration.
        account_qos_drop(
            &mut n,
            undefined,
            Some(gst::ClockTime::from_nseconds(1)),
            frame,
        );
        account_qos_drop(&mut n, undefined, None, frame);
        assert_eq!(n, 7);

        // The same through a real message as v4l2src posts it.
        let msg = gst::message::Qos::builder(true)
            .duration(gst::ClockTime::from_nseconds(4 * frame))
            .build();
        let gst::MessageView::Qos(q) = msg.view() else {
            unreachable!()
        };
        let (_, dropped) = q.stats();
        assert!(matches!(dropped, gst::GenericFormattedValue::Undefined(_)));
        let mut m = 0;
        account_qos_drop(&mut m, dropped, q.duration(), frame);
        assert_eq!(m, 4);
    }

    #[test]
    fn qos_running_totals_keep_the_largest() {
        gst::init().unwrap();
        let frame = 16_666_667u64;
        let mut n = 0;
        for (fmt, total) in [
            (gst::Format::Buffers, 2),
            (gst::Format::Buffers, 5),
            (gst::Format::Buffers, 4), // out of order: never goes back
        ] {
            let v = gst::GenericFormattedValue::new(fmt, total);
            // The duration is ignored for running totals.
            account_qos_drop(&mut n, v, Some(gst::ClockTime::from_seconds(1)), frame);
        }
        assert_eq!(n, 5);
        let mut d = 0;
        account_qos_drop(
            &mut d,
            gst::GenericFormattedValue::new(gst::Format::Default, 9),
            None,
            frame,
        );
        assert_eq!(d, 9);

        // A decoder's message with stats set, as jpegdec posts it.
        let msg = gst::message::Qos::builder(true)
            .stats(
                gst::format::Buffers::from_u64(100),
                gst::format::Buffers::from_u64(7),
            )
            .build();
        let gst::MessageView::Qos(q) = msg.view() else {
            unreachable!()
        };
        let mut m = 3;
        account_qos_drop(&mut m, q.stats().1, q.duration(), frame);
        assert_eq!(m, 7);
    }

    #[test]
    fn camera_gap_across_a_status_boundary_counts_once() {
        let f = 10u64;
        // Window 1 ends with PTS 30; window 2 starts at 60: 2 frames missing
        // (40, 50), visible only with the previous window's last PTS.
        let w1 = [0, 10, 20, 30];
        let w2 = [60, 70];
        assert_eq!(window_camera_drops(None, &w1, f), 0);
        assert_eq!(window_camera_drops(Some(30), &w2, f), 2);
        assert_eq!(window_camera_drops(None, &w2, f), 0, "the old, blind count");
        // Same total as over the unsplit sequence.
        assert_eq!(
            stats::missing_frames_from_gaps(&[0, 10, 20, 30, 60, 70], f),
            2
        );
        // An empty window carries nothing; the gap shows up when frames resume.
        assert_eq!(window_camera_drops(Some(30), &[], f), 0);
    }

    #[test]
    fn pipeline_drops_are_signed() {
        assert_eq!(window_pipeline_drops(300, 300), 0);
        assert_eq!(window_pipeline_drops(300, 297), 3);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "delivered 5 > sent 3")]
    fn more_delivered_than_sent_is_asserted() {
        window_pipeline_drops(3, 5);
    }

    #[test]
    fn streaming_rejects_the_consumer() {
        let a = Args::try_parse_from(["obc-bench", "--duration", "0", "--consumer"]).unwrap();
        assert!(a.streaming());
        let e = a.validate().unwrap_err().to_string();
        assert!(
            e.contains("--consumer") && e.contains("--duration 0"),
            "{e}"
        );
        let ok = Args::try_parse_from(["obc-bench", "--duration", "0"]).unwrap();
        assert!(ok.validate().is_ok());
        let bench = Args::try_parse_from(["obc-bench", "--consumer"]).unwrap();
        assert!(!bench.streaming() && bench.validate().is_ok());
    }
}
