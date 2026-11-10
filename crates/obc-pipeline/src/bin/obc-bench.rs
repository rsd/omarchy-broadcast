//! `obc-bench`: benchmark and development runner for the stream.
//!
//! Runs the three-stage stream of `docs/adr/0009-ingest-process-output.md`:
//! one ingest pipeline per camera feeding a latest-frame slot, and one output
//! pipeline with a fixed contract (`--output`, `--output-format`,
//! `--output-range`) paced by its own clock, writing to a v4l2loopback
//! device. It measures output fps, input fps per camera, the frames the
//! output clock emitted (fresh, repeat, placeholder) and the input frames it
//! superseded, source-switch gaps, added latency, process CPU, GPU
//! utilisation/memory and the caps on every link. See
//! `docs/benchmarks/io-split.md` for the method and results.
//!
//! Switching: `--switch-every N` moves to the next camera every N seconds,
//! and SIGUSR1 moves to it at once. The cycle is the `--camera` list when it
//! names two or more cameras; otherwise the selected camera, then every other
//! discovered colour camera. Each camera's mode is chosen from `--mode`
//! against its own capabilities. Both act inside the measurement window
//! only: a SIGUSR1 received before it opens (camera start, warm-up) is
//! discarded when it opens, and a timer switch never fires at or after its
//! end. A switch whose new camera has not emitted its first frame when the
//! window closes is reported as abandoned.
//!
//! `--output-range full` is refused for the loopback before anything
//! streams: v4l2loopback carries no colour range, so a consumer would read
//! the stream as limited range.
//!
//! A switch holds the last frame until the next camera's first frame (at
//! most `--hold-timeout` seconds), so a switch shows no black frames unless
//! the next camera takes longer than that to start.
//!
//! The output clock locks the phase of its ticks to the camera's frames
//! (`docs/adr/0010-phase-locked-output-clock.md`); `--no-phase-lock` emits
//! on the fixed grid instead, for comparison. Because emission instants
//! then follow the camera within one period, `fps_sustained` judges the
//! output by its PTS sequence, its frame count and the deviation of its
//! arrivals from the PTS grid (see [`FpsSection`]).
//!
//! Effects (`docs/adr/0012-video-effects-and-maxine-backend.md`):
//! `--effect none|blur|replace` inserts the `obceffect` element in the
//! Process stage, its backend the module that provides the `background`
//! stage, loaded as the daemon loads it (`--modules`: a `modules.d`
//! directory, a manifest or a module library; the daemon's directories
//! without it) (`none` in passthrough, to measure the element's own cost;
//! without `--effect` there is no element), with `--blur-strength` and
//! `--background`. `--denoise` adds the video noise removal stage
//! (`docs/adr/0018-video-noise-removal.md`) before the effect, or alone with
//! `--effect none`. `--relight[=warm|neutral|cool]` adds the relighting
//! stage after it (light `neutral` without a value), with `--relight-pan`
//! (degrees) and `--relight-gain`, under the effect or alone with
//! `--effect none`. A requested effect, and each requested stage, are
//! waited for (ready, or the run fails with the reason) before the warm-up.
//! `--effect-switch-every S` cycles none -> blur -> replace (replace only
//! with `--background`) inside the measurement window, keeping `--denoise`
//! (and `--relight`) throughout; the report's `effect` section has the
//! element's counters, its GPU time per frame, each stage's request, its
//! status and the frames and GPU time of the stage, the load time, the caps events at the sink
//! (one: an effect change never renegotiates) and per switch the time to
//! ready and the black, processed and passthrough frames.
//!
//! `--duration 0` streams until SIGINT/SIGTERM instead, printing a status
//! line every 5 s and one line per switch. `--consumer` is allowed there for
//! the consumer frame rate only: the luma stamp the end-to-end latency needs
//! is applied in measured runs only.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use gst::prelude::*;
use obc_core::{
    AspectPolicy, CameraSelector, ColourRange, DEFAULT_SINK_LABEL, ModeRequest, OutputContract,
    OutputFormat, SinkSelector,
};
use obc_pipeline::capability::choose_mode;
use obc_pipeline::clock::{PacerConfig, PhaseLockConfig};
use obc_pipeline::controls::{CID_EXPOSURE_AUTO_PRIORITY, get_ctrl};
use obc_pipeline::discovery::{CameraList, resolve_node_path};
use obc_pipeline::effect::{StageId, StageRequest, Value};
use obc_pipeline::gpu::{GpuInfo, GpuSample, GpuSampler};
use obc_pipeline::instrument::{Recorder, Samples, SwitchGap, Totals};
use obc_pipeline::output::{check_sink_range, contract_caps_string};
use obc_pipeline::pipeline::names;
use obc_pipeline::stats::{self, FpsReport, Summary};
use obc_pipeline::{
    ControlReport, DeviceInfo, DiscoveryRoute, EffectFallback, EffectKind, EffectSettings,
    EffectStats, EffectStatus, GpuStage, IngestConfig, IngestPath, LinkCaps, MediaPipeline,
    ModeChoice, Output, OutputConfig, OutputCounts, OutputSink, Session, all_cuda_between,
    list_cameras, scan_sysfs, select_camera, select_sink,
};
use serde::Serialize;

const METHOD: &str = "Primary: a pad probe on each ingest pipeline's v4l2src src pad stamps every camera buffer with CLOCK_MONOTONIC, keyed by (source, camera PTS); the PTS is unchanged through the ingest pipeline into the latest-frame slot. The output clock's emit observer reports every tick: its output PTS (tick x period), whether it emitted a fresh frame, a repeat or a placeholder, and the frame's source and camera PTS; a bounded side table maps the output PTS to that emission and to the fresh frame's camera src-pad time. A pad probe on the output's v4l2sink sink pad joins the buffer by output PTS. Added latency = sink-ready time - camera src-pad time, fresh frames only: the delay our stages add over an app reading the camera directly, including the wait for the next output tick (0 to 1 output period). Sink-ready = after the sink probe maps the buffer readable: cudadownload hands v4l2sink GstCudaMemory-backed buffers and CUDA work is asynchronous, so kernels and the device->host copy complete on first map; the probe's map does exactly the work v4l2sink's own map would do next. added_at_sink_pad_ms (before that map) is a secondary figure and under-states the cost. Output fps counts every buffer at the v4l2sink sink pad by arrival time, in 1 s windows. Via consumer (measured runs only): a bench-only probe writes the output PTS into the first 16 luma bytes just before v4l2sink; an in-process consumer pipeline (v4l2src on the loopback ! fakesink) decodes it on arrival on the same clock: camera src pad -> consumer receipt, fresh frames only. Limits: excludes sensor exposure and USB transfer (identical for a direct reader, so it cancels); excludes the consumer app's own render/display (no glass-to-glass measurement); the stamp alters 16 luma pixels in measured runs only. gst_latency_query is what elements declare, not a measurement.";

/// Benchmark and development runner: cameras -> ingest -> latest-frame slot
/// -> output clock -> v4l2loopback.
#[derive(Debug, Parser)]
#[command(name = "obc-bench", version)]
struct Args {
    /// Camera: a card-name substring (case-insensitive), a USB vid:pid
    /// (exactly hhhh:hhhh), or a /dev/videoN path (or /dev/v4l/... link).
    /// Repeatable: two or more give the switching cycle, in order. Omitted:
    /// the first colour camera offering --mode exactly, else the best
    /// degraded match. With one or none, the cycle continues with every
    /// other discovered colour camera.
    #[arg(long, value_name = "NAME|VID:PID|/dev/videoN")]
    camera: Vec<CameraSelector>,
    /// v4l2loopback card label of the output device.
    #[arg(long, default_value = DEFAULT_SINK_LABEL)]
    sink_label: String,
    /// Explicit v4l2loopback node (overrides --sink-label); must be a loopback device.
    #[arg(long)]
    sink_device: Option<PathBuf>,
    /// Requested capture mode, WxH@FPS (any format) or FORMAT:WxH@FPS
    /// (FORMAT: NV12, YUY2/YUYV, MJPEG, ...). Resolved against each camera's
    /// capabilities; a mode a camera does not offer degrades (logged).
    #[arg(long, default_value = "1920x1080@60")]
    mode: ModeRequest,
    /// Output contract size and frame rate, WxH@FPS (FPS may be N/D).
    #[arg(long, default_value = "1920x1080@30")]
    output: OutputContract,
    /// Output pixel format: nv12 | i420.
    #[arg(long, default_value = "nv12")]
    output_format: OutputFormat,
    /// Output colour range: limited | full.
    #[arg(long, default_value = "limited")]
    output_range: ColourRange,
    /// How a source of another aspect ratio is fitted: crop (centre crop to
    /// fill) | letterbox (black borders).
    #[arg(long, default_value = "crop")]
    aspect: AspectPolicy,
    /// Milliseconds without input after which the output sends placeholder
    /// (black) frames instead of repeating the last one.
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(1..=60_000))]
    input_gap_ms: u64,
    /// Move to the next camera every this many seconds (after warm-up).
    #[arg(long, value_parser = positive_secs)]
    switch_every: Option<f64>,
    /// Seconds a switch holds the last frame while waiting for the next
    /// camera's first frame; placeholders follow if it has not arrived.
    #[arg(long, default_value_t = 3.0, value_parser = positive_secs)]
    hold_timeout: f64,
    /// Buffers the output's appsrc queues before dropping the oldest (also
    /// the clock's longest catch-up burst).
    #[arg(long, default_value_t = OutputConfig::DEFAULT_QUEUE_BUFFERS, value_parser = clap::value_parser!(u32).range(1..=64))]
    output_queue_buffers: u32,
    /// Emit every tick at its timestamp instead of locking the ticks'
    /// phase to the camera (the behaviour before the phase lock).
    #[arg(long)]
    no_phase_lock: bool,
    /// Phase lock margin in ms: how long after the latest-arriving frame
    /// of the window a tick fires (tuning; default from the library).
    #[arg(long, value_parser = non_negative_ms)]
    phase_margin_ms: Option<f64>,
    /// Phase lock window: fresh ticks whose waits are kept (tuning).
    #[arg(long, value_parser = clap::value_parser!(u32).range(2..=256))]
    phase_window: Option<u32>,
    /// Phase lock jump threshold in ms: a larger error re-acquires in one
    /// jump instead of slewing (tuning).
    #[arg(long, value_parser = non_negative_ms)]
    phase_jump_ms: Option<f64>,
    /// Process stage: passthrough | roundtrip (NV12 -> BGR -> NV12, in CUDA
    /// memory; the conversion cost of an effect working on BGR).
    #[arg(long, default_value = "passthrough")]
    gpu_stage: GpuStage,
    /// Effect element in the Process stage: none | blur | replace. `none`
    /// inserts the element in passthrough (to measure its own cost);
    /// omitted, there is no element. Does not combine with
    /// --gpu-stage roundtrip.
    #[arg(long)]
    effect: Option<EffectKind>,
    /// Blur strength, 0 to 1.
    #[arg(long, default_value_t = EffectSettings::DEFAULT_BLUR_STRENGTH, value_parser = unit_interval)]
    blur_strength: f32,
    /// Background image of the replace effect (an absolute path).
    #[arg(long)]
    background: Option<PathBuf>,
    /// Output after an effect failure: black | none (the raw input).
    #[arg(long, default_value = "black")]
    effect_on_failure: EffectFallback,
    /// Add the video noise removal stage before the effect (alone with
    /// `--effect none`). Needs --effect.
    #[arg(long)]
    denoise: bool,
    /// Add the relighting stage after the denoise stage (alone with
    /// `--effect none`), with this light: warm | neutral | cool (neutral
    /// without a value). Needs --effect.
    #[arg(long, value_name = "LIGHT", num_args = 0..=1, require_equals = true,
          default_missing_value = "neutral",
          value_parser = clap::builder::PossibleValuesParser::new(["warm", "neutral", "cool"]))]
    relight: Option<String>,
    /// The relighting light's direction, in degrees (-180 to 180).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    relight_pan: f32,
    /// The relit person's brightness (0.5 to 2, 1 unchanged).
    #[arg(long, default_value_t = 1.0)]
    relight_gain: f32,
    /// Cycle the effect none -> blur -> replace (replace only with
    /// --background) every this many seconds inside the measurement
    /// window. Needs --effect (the starting point of the cycle).
    #[arg(long, value_parser = positive_secs)]
    effect_switch_every: Option<f64>,
    /// Measured seconds (after warm-up). 0 streams until SIGINT/SIGTERM with
    /// a status line every 5 s (--consumer then measures consumer fps only).
    #[arg(long, default_value_t = 30.0, value_parser = non_negative_secs)]
    duration: f64,
    /// Seconds after the first camera frame reaches the output before measuring.
    #[arg(long, default_value_t = 3.0, value_parser = non_negative_secs)]
    warmup: f64,
    /// Write the full JSON report here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Also read the loopback with an in-process consumer: consumer fps,
    /// and in measured runs the end-to-end latency via a luma stamp.
    #[arg(long)]
    consumer: bool,
    /// After teardown and dropping all GStreamer/NVML objects, stay alive this
    /// many seconds (for external dGPU-suspend checks).
    #[arg(long, default_value_t = 0.0, value_parser = non_negative_secs)]
    linger_after_teardown: f64,
    /// Dump the output and current ingest graphs (.dot, caps on every link)
    /// here once PLAYING.
    #[arg(long)]
    dot_dir: Option<PathBuf>,
    /// Camera discovery route: auto | monitor | sysfs.
    #[arg(long, default_value = "auto")]
    discovery: DiscoveryRoute,
    /// Ingest queue max-size-buffers (leaky=downstream).
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
    /// Seconds of GPU baseline sampling before the pipelines are built.
    #[arg(long, default_value_t = 3.0, value_parser = non_negative_secs)]
    gpu_baseline: f64,
    /// While a camera streams, set its exposure_dynamic_framerate (UVC
    /// "exposure auto priority") to 0 so it cannot lower its frame rate in
    /// dim light. Each camera's original value is restored when its ingest
    /// stops (on a switch and on every exit path that unwinds: success,
    /// error, panic, SIGINT/SIGTERM), not on SIGKILL/SIGQUIT.
    #[arg(long)]
    camera_fixed_framerate: bool,
    /// Where the effect backend's module is found: a `modules.d` directory,
    /// a module manifest (`.toml`), or a module library (`.so`, with its
    /// manifest found by the library's file name in its own directory or
    /// the default directories). Omitted: the daemon's default directories
    /// (`OBC_MODULE_PATH` included). Used with --effect.
    #[arg(long, value_name = "DIR|MANIFEST|LIBRARY")]
    modules: Option<PathBuf>,
    /// Print debug details (camera candidates).
    #[arg(short, long)]
    verbose: bool,
}

/// The effect backend of `--effect`: the module that provides the
/// `background` stage, found from `--modules` (or the default directories)
/// and opened with an empty configuration (the Maxine module then takes its
/// SDK root from `MAXINE_VFX_ROOT` or its default).
fn effect_backend(modules: Option<&Path>) -> Result<obc_pipeline::EffectBackend> {
    use obc_module_host::discovery::{default_dirs, read_entry, scan};
    use obc_module_host::{ModuleBackendFactory, Registry};
    let entries = match modules {
        None => scan(&default_dirs(&[])).entries,
        Some(p) if p.is_dir() => scan(&[p.to_path_buf()]).entries,
        Some(p) if p.extension().is_some_and(|e| e == "toml") => vec![read_entry(p)],
        Some(lib) => {
            let name = lib
                .file_name()
                .context("the module library has no file name")?;
            let mut dirs = default_dirs(&[]);
            if let Some(parent) = lib.parent() {
                dirs.push(parent.to_path_buf());
            }
            let mut entry = scan(&dirs)
                .entries
                .into_iter()
                .find(|e| {
                    e.manifest
                        .as_ref()
                        .is_ok_and(|m| m.library.file_name() == Some(name))
                })
                .with_context(|| format!("no module manifest names {}", lib.display()))?;
            if let Ok(m) = &mut entry.manifest {
                m.library = std::fs::canonicalize(lib)
                    .with_context(|| format!("cannot read {}", lib.display()))?;
            }
            vec![entry]
        }
    };
    let mut registry = Registry::from_entries(entries);
    registry.open_all(|_| serde_json::json!({}), &obc_module_host::stderr_log());
    let resolution = registry.resolve(StageId::BACKGROUND.as_str());
    let Some(id) = resolution.provider else {
        bail!(
            "no module provides the background stage: {}",
            resolution.reason.unwrap_or_default()
        );
    };
    let module = registry
        .module(&id)
        .cloned()
        .with_context(|| format!("the module {id} is not loaded"))?;
    eprintln!("effect backend: the module {id}");
    Ok(obc_pipeline::EffectBackend::new(Arc::new(
        ModuleBackendFactory::new(module),
    )))
}

impl Args {
    /// `--duration 0`: stream until a stop signal.
    fn streaming(&self) -> bool {
        self.duration == 0.0
    }

    /// The consumer reads the luma stamp: measured runs only.
    fn consumer_stamp(&self) -> bool {
        self.consumer && !self.streaming()
    }

    /// The output contract: `--output` with `--output-format` and
    /// `--output-range`.
    fn contract(&self) -> OutputContract {
        OutputContract {
            format: self.output_format,
            range: self.output_range,
            ..self.output
        }
    }

    /// The effect settings for `kind` from the effect arguments: the
    /// stages `denoise` and `relight` (its light at load time, its direction
    /// in degrees and its brightness in percent, the gain times 100, per
    /// frame), each on as asked.
    fn effect_settings(&self, kind: EffectKind) -> EffectSettings {
        let relight = StageRequest::new(StageId::RELIGHT)
            .with_param("light", self.relight.as_deref().unwrap_or("neutral"))
            .with_live("direction", Value::Float(self.relight_pan.into()))
            .with_live(
                "brightness",
                Value::Float(f64::from(self.relight_gain) * 100.0),
            );
        EffectSettings {
            kind,
            blur_strength: self.blur_strength,
            background: self.background.clone(),
            fallback: self.effect_on_failure,
            stages: vec![
                StageRequest {
                    enabled: self.denoise,
                    ..StageRequest::new(StageId::DENOISE)
                },
                StageRequest {
                    enabled: self.relight.is_some(),
                    ..relight
                },
            ],
        }
    }

    /// The effects `--effect-switch-every` cycles through.
    fn effect_cycle(&self) -> Vec<EffectKind> {
        let mut c = vec![EffectKind::None, EffectKind::Blur];
        if self.background.is_some() {
            c.push(EffectKind::Replace);
        }
        c
    }

    /// Checks the effect arguments before any device is touched.
    fn check_effect(&self) -> Result<()> {
        let Some(kind) = self.effect else {
            if self.effect_switch_every.is_some() {
                bail!("--effect-switch-every needs --effect (the effect the cycle starts with)");
            }
            if self.denoise {
                bail!("--denoise needs --effect (--effect none for the denoise stage alone)");
            }
            if self.relight.is_some() {
                bail!("--relight needs --effect (--effect none for the relighting stage alone)");
            }
            return Ok(());
        };
        if self.gpu_stage != GpuStage::Passthrough {
            bail!(
                "--effect does not combine with --gpu-stage {}",
                self.gpu_stage
            );
        }
        obc_pipeline::effect::check_settings(&self.effect_settings(kind))?;
        // The relighting ranges the Maxine module takes; its own check runs
        // again when the stage loads.
        if self.relight.is_some() {
            if !(-180.0..=180.0).contains(&self.relight_pan) {
                bail!(
                    "--relight-pan {} is outside the relight direction range -180 to 180",
                    self.relight_pan
                );
            }
            if !(0.5..=2.0).contains(&self.relight_gain) {
                bail!(
                    "--relight-gain {} is outside the relight brightness range 0.5 to 2",
                    self.relight_gain
                );
            }
        }
        if let Some(bg) = &self.background {
            gst_obc::background::check_file(bg).map_err(|e| anyhow!(e))?;
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

/// Upper bound for a milliseconds tuning argument (the phase lock's
/// margin and jump threshold work within one output period).
const MAX_TUNING_MS: f64 = 1000.0;

fn non_negative_ms(s: &str) -> Result<f64, String> {
    let v: f64 = s.trim().parse().map_err(|e| format!("{s:?}: {e}"))?;
    if !v.is_finite() || !(0.0..=MAX_TUNING_MS).contains(&v) {
        return Err(format!(
            "{s:?}: expected milliseconds in 0..={MAX_TUNING_MS}"
        ));
    }
    Ok(v)
}

fn unit_interval(s: &str) -> Result<f32, String> {
    let v: f32 = s.trim().parse().map_err(|e| format!("{s:?}: {e}"))?;
    if !v.is_finite() || !(0.0..=1.0).contains(&v) {
        return Err(format!("{s:?}: expected a value in 0..=1"));
    }
    Ok(v)
}

fn positive_secs(s: &str) -> Result<f64, String> {
    parse_secs(s, false)
}

fn non_negative_secs(s: &str) -> Result<f64, String> {
    parse_secs(s, true)
}

/// Set by SIGINT/SIGTERM; the run loop checks it and falls through to the
/// normal teardown, control restore and report.
static STOP: AtomicBool = AtomicBool::new(false);
/// Set by SIGUSR1; the run loop clears it and switches to the next camera.
static NEXT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_sig: libc::c_int) {
    // Async-signal-safe: a single atomic store.
    STOP.store(true, Ordering::SeqCst);
}

extern "C" fn on_next_signal(_sig: libc::c_int) {
    // Async-signal-safe: a single atomic store.
    NEXT.store(true, Ordering::SeqCst);
}

fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

fn next_requested() -> bool {
    NEXT.load(Ordering::SeqCst)
}

/// Installs `handler` for `sig` with `SA_RESTART`.
fn install_handler(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) -> Result<()> {
    // SAFETY: `sa` is zero-initialised (a valid empty mask and flags) before
    // the handler, flags and mask are set; the handlers only store to an
    // atomic, which is async-signal-safe.
    let rc = unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut())
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("installing signal handler");
    }
    Ok(())
}

/// Routes SIGINT and SIGTERM to [`STOP`] and SIGUSR1 to [`NEXT`]. Repeats
/// are idempotent and never fall back to the default action: `timeout(1)`
/// and a terminal both deliver the same signal twice in quick succession (to
/// the child, then to its process group), and dying on the second would
/// skip the control restore. A stuck teardown is ended with SIGQUIT or
/// SIGKILL (no restore then).
fn install_signal_handlers() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        install_handler(sig, on_stop_signal)?;
    }
    install_handler(libc::SIGUSR1, on_next_signal)
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

/// Warnings kept for the report; later ones are only counted, so a long
/// run (`--duration 0`) holds a bounded list.
const WARNINGS_KEPT: usize = 200;

/// The first [`WARNINGS_KEPT`] warnings of a run, and how many more there
/// were.
#[derive(Debug, Default)]
struct Warnings {
    kept: Vec<String>,
    suppressed: u64,
}

impl Warnings {
    fn push(&mut self, msg: String) {
        if self.kept.len() < WARNINGS_KEPT {
            self.kept.push(msg);
        } else {
            self.suppressed += 1;
        }
    }
}

#[derive(Debug, Default)]
struct BusCounters {
    /// QoS drops by `source/element`, whole run: the output pipeline's, and
    /// those of every ingest run that ended, summed per camera. Keyed by
    /// camera rather than by ingest pipeline, so switching adds no keys.
    qos_dropped: BTreeMap<String, u64>,
    /// The running ingest's QoS drops by `source/element`. Running totals
    /// (see [`account_qos_drop`]) restart with each ingest pipeline, so a
    /// run is folded into [`Self::qos_dropped`] when it ends.
    qos_run: BTreeMap<String, u64>,
    warnings: Warnings,
}

impl BusCounters {
    /// Adds the ended ingest run's QoS drops to the whole-run totals.
    fn end_ingest_run(&mut self) {
        for (k, v) in std::mem::take(&mut self.qos_run) {
            *self.qos_dropped.entry(k).or_default() += v;
        }
    }

    /// Whole-run QoS drops, including the running ingest's.
    fn take_qos_dropped(&mut self) -> BTreeMap<String, u64> {
        self.end_ingest_run();
        std::mem::take(&mut self.qos_dropped)
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

/// Drains `p`'s bus, waiting up to `wait` for the first message. QoS drops
/// go into `qos`, keyed `source/element`; `frame_ns` is the nominal frame
/// duration of the pipeline's frames (see [`account_qos_drop`]). An error
/// or EOS is returned as its description.
fn drain_bus(
    p: &MediaPipeline,
    source: &str,
    qos: &mut BTreeMap<String, u64>,
    warnings: &mut Warnings,
    wait: Duration,
    frame_ns: u64,
) -> Result<(), String> {
    let bus = p.bus();
    let name = p.pipeline().name();
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
                return Err(format!(
                    "{name} error from {src}: {} ({})",
                    e.error(),
                    e.debug().map(|d| d.to_string()).unwrap_or_default()
                ));
            }
            MessageView::Eos(_) => return Err(format!("{name}: unexpected EOS")),
            MessageView::Warning(w) => {
                warnings.push(format!("{name}/{src}: {}", w.error()));
            }
            MessageView::Qos(q) => {
                let (_processed, dropped) = q.stats();
                let entry = qos.entry(format!("{source}/{src}")).or_default();
                account_qos_drop(entry, dropped, q.duration(), frame_ns);
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

/// One camera of the switching cycle and the mode chosen for it.
#[derive(Debug, Clone)]
struct CycleCamera {
    device: DeviceInfo,
    choice: ModeChoice,
    /// How it entered the cycle.
    how: String,
    /// Picked automatically (no selector named it).
    auto: bool,
}

impl CycleCamera {
    fn label(&self) -> String {
        format!("{} ({})", self.device.card, self.device.path.display())
    }
}

#[derive(Debug, Serialize)]
struct CameraEntry {
    camera: DeviceInfo,
    /// How it entered the cycle.
    how: String,
    /// The camera mode chosen for --mode.
    mode: String,
    /// The chosen mode matches the request exactly.
    mode_exact: bool,
    /// What differs from the request when degraded.
    mode_differences: Vec<String>,
    /// How frames reach CUDA memory (gpu-direct, cpu-convert, cpu-decode).
    ingest: IngestPath,
}

/// The `exposure_dynamic_framerate` control of one camera over the run.
#[derive(Debug, Clone, Serialize)]
struct CameraControls {
    camera: PathBuf,
    /// Why the control was skipped (absent, disabled, unreadable); None when
    /// the camera exposes it.
    exposure_dynamic_framerate_skipped: Option<String>,
    /// Before the camera's first ingest started.
    exposure_dynamic_framerate_before: Option<i32>,
    /// While its (last) ingest ran.
    exposure_dynamic_framerate_during: Option<i32>,
    /// The bench set it while streaming (`--camera-fixed-framerate`).
    pinned: bool,
    /// Pinning was asked for but the control is read-only.
    read_only: bool,
    /// Read back after teardown.
    exposure_dynamic_framerate_after_exit: Option<i32>,
    /// The value after teardown equals the value before (None: skipped or
    /// unreadable).
    restored: Option<bool>,
}

impl CameraControls {
    fn first(camera: &Path, r: &ControlReport) -> Self {
        CameraControls {
            camera: camera.to_path_buf(),
            exposure_dynamic_framerate_skipped: r.skipped.as_ref().map(|s| s.to_string()),
            exposure_dynamic_framerate_before: r.before,
            exposure_dynamic_framerate_during: r.during,
            pinned: r.pinned,
            read_only: r.read_only,
            exposure_dynamic_framerate_after_exit: None,
            restored: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct ConfigReport {
    /// The switching cycle, in order; the first one streams first.
    cameras: Vec<CameraEntry>,
    sink: DeviceInfo,
    camera_discovery: String,
    camera_discovery_note: Option<String>,
    sink_discovery: &'static str,
    /// No --camera: the first camera was picked automatically.
    camera_auto_picked: bool,
    /// The --mode request, as given.
    requested_mode: String,
    output: OutputContract,
    /// The caps the output delivers to the loopback.
    output_caps: String,
    aspect: AspectPolicy,
    process: GpuStage,
    input_gap_ms: u64,
    hold_timeout_s: f64,
    output_queue_buffers: u32,
    /// The output clock's phase lock settings; `None`: disabled.
    phase_lock: Option<PhaseLockConfig>,
    queue_buffers: u32,
    camera_io_mode: String,
    camera_fixed_framerate: bool,
    switch_every_s: Option<f64>,
    /// --duration 0: streamed until a stop signal.
    streaming: bool,
    duration_s: f64,
    /// Seconds actually measured: `duration_s` unless interrupted.
    measured_s: f64,
    /// The run was stopped early by SIGINT/SIGTERM; figures cover `measured_s`.
    interrupted: bool,
    warmup_s: f64,
    consumer: bool,
    /// The consumer measured end-to-end latency through the luma stamp.
    consumer_stamp: bool,
}

/// Input frame rate of one ingest run (one camera, from its start to the
/// switch away from it), within the measured span.
#[derive(Debug, Serialize)]
struct InputFps {
    source_id: u64,
    camera: String,
    mode: String,
    ingest: IngestPath,
    /// Seconds covered: from its first frame in the span to its end.
    span_s: f64,
    fps: FpsReport,
    /// Frames missing in the camera's own PTS sequence (camera/USB side).
    missing_frames_by_pts: u64,
    /// Camera src-pad inter-arrival gaps > 1.5 frame durations.
    stalls: usize,
    /// Arrival jitter at the src pad: each frame's arrival minus a straight
    /// line fitted through all arrivals by frame number (from the camera
    /// PTS), as percentiles (`[percentile, ms]`). The phase-locked output
    /// clock has to wait for the late side of this spread.
    arrival_jitter_ms: Vec<(u8, f64)>,
    /// The same for the camera's own PTS (driver timestamps): jitter that
    /// is already in the camera's timestamps, not added by the dequeue.
    pts_jitter_ms: Vec<(u8, f64)>,
}

/// Residuals of `values` (ns) against a least-squares line over the frame
/// numbers derived from `pts` (ns) and the nominal frame duration, in ms.
/// Frame numbers advance by each PTS step rounded to whole frames, so a
/// camera whose real rate differs from the nominal one keeps its numbering.
fn line_residuals_ms(values: &[u64], pts: &[u64], frame_ns: u64) -> Vec<f64> {
    if values.len() < 3 || values.len() != pts.len() || frame_ns == 0 {
        return Vec::new();
    }
    let v0 = values[0];
    let mut k = 0f64;
    let xs: Vec<f64> = std::iter::once(0.0)
        .chain(pts.windows(2).map(|w| {
            k += (w[1].saturating_sub(w[0]) as f64 / frame_ns as f64)
                .round()
                .max(1.0);
            k
        }))
        .collect();
    let ys: Vec<f64> = values.iter().map(|&v| v as f64 - v0 as f64).collect();
    let n = xs.len() as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let a = my - b * mx;
    xs.iter()
        .zip(&ys)
        .map(|(x, y)| (y - (a + b * x)) / 1e6)
        .collect()
}

/// How `criteria.fps_sustained` judges the output rate.
const FPS_METHOD: &str = "fps_sustained: the output PTS sequence at the v4l2sink sink pad is exact over the window (every tick index once, in order), the frame count is within max(0.1 %, 1 frame) of the contract rate times the window, and the deviation of each frame's sink-pad arrival from its PTS grid instant (clock start + PTS) spans at most one output period plus a jitter allowance (deviation.limit_ms). A deviation span of at most one period bounds every wall-clock 1 s window to the contract rate +-1 frame; sliding_1s reports the fewest and most frames actually seen in any 1 s window. With --consumer in measured runs the same is required of the consumer, by the output PTS stamped into each frame. The phase-locked clock emits each tick within one period before its timestamp, following the camera, so the grid-window figure (output_grid: windows on one mean arrival phase) is kept for comparison only.";

/// Allowance over one output period for the spread of the sink-pad
/// deviation (wake-up and hand-off jitter of the output thread).
const DEVIATION_ALLOWANCE_NS: u64 = 5_000_000;

#[derive(Debug, Serialize)]
struct FpsSection {
    /// The contract frame rate.
    target: f64,
    method: &'static str,
    /// Every output buffer at the v4l2sink sink pad, by arrival time.
    output: FpsReport,
    consumer: Option<FpsReport>,
    /// The output PTS of the frames at the sink pad, in arrival order.
    output_pts: PtsSequence,
    consumer_pts: Option<PtsSequence>,
    /// Sink-pad arrival minus PTS grid instant, every frame.
    output_deviation: Deviation,
    consumer_deviation: Option<Deviation>,
    /// The fewest and most arrivals in any 1 s wall-clock window.
    output_sliding_1s: Sliding,
    consumer_sliding_1s: Option<Sliding>,
    /// The same arrivals on 1 s windows placed on one mean arrival phase
    /// of the output's frame grid (see [`grid_fps`]): the criterion before
    /// the phase lock, kept for comparison.
    output_grid: GridFps,
    consumer_grid: Option<GridFps>,
    /// Output inter-arrival gaps > 1.5 output periods.
    output_stalls: usize,
    consumer_stalls: Option<usize>,
    /// Per ingest run, at the camera src pad.
    inputs: Vec<InputFps>,
}

/// What the output clock did over the measured window (counter deltas).
#[derive(Debug, Serialize)]
struct FramesSection {
    /// fresh / repeat / placeholder ticks, input frames written into the
    /// slot, input frames superseded before any tick emitted them (expected
    /// when a camera is faster than the contract), late ticks (emitted
    /// after their successor was due), ticks skipped by a resync, and
    /// buffers the output's `appsrc` accepted (`emitted`) and then dropped
    /// from a full queue (`queue_dropped`).
    output: OutputCounts,
    /// Frames that left a camera src pad in the window.
    camera_src_frames: u64,
    /// Camera frames that never reached the slot: `camera_src_frames -
    /// output.input_frames` (frames in flight at the window edges and at
    /// each switch make this a few frames either way).
    ingest_lost_frames: i64,
    /// Ingest queue overrun signals, whole run; with leaky=downstream each
    /// drops one stale buffer.
    queue_overruns: u64,
    /// QoS "dropped" totals posted on the buses, by `source/element`, whole
    /// run: the source is the output pipeline or a camera (all its ingest
    /// runs summed). v4l2src posts one message per gap in the driver's sequence
    /// numbers, without stats: its count is the sum of `round(duration /
    /// frame duration)` (at least 1 per message). Elements that report a
    /// running total (jpegdec, base transforms) contribute their latest total
    /// of each ingest run.
    qos_dropped_by_element: BTreeMap<String, u64>,
    /// Frames `jpegdec` dropped, whole run, when a camera on the CPU decode
    /// path streamed: JPEG data the camera sent that does not decode.
    decoder_dropped_frames: Option<u64>,
}

/// The output clock over the window.
#[derive(Debug, Serialize)]
struct ClockSection {
    /// The phase lock settings; `None`: disabled (`--no-phase-lock`).
    phase_lock: Option<PhaseLockConfig>,
    /// Phase jumps (first lock per camera run, re-acquisitions) and wraps
    /// in the window.
    lock_acquisitions: u64,
    lock_wraps: u64,
    /// At the end of the window.
    phase_offset_ms: f64,
    phase_locked: bool,
    /// How long fresh frames waited in the slot for their tick, and the
    /// same at more percentiles (`[percentile, ms]`).
    slot_wait_ms: Option<Summary>,
    slot_wait_percentiles_ms: Vec<(u8, f64)>,
    /// Fresh frames that waited in the slot longer than one camera frame
    /// interval (the mode's nominal one): the camera's next frame was due
    /// before the tick and missed it, so the tick took the frame before.
    /// This holds for a camera faster than the output (a 60 fps camera
    /// thinned to 30 fps), where a frame the camera itself dropped counts
    /// too when it is the one a tick would have caught (see
    /// `fps.inputs[].missing_frames_by_pts`). At the output's own rate a
    /// missed tick or a dropped frame shows as a repeat instead (a late
    /// frame waits less than a frame interval for the next tick, or is
    /// superseded), so this count stays at zero there. Windows with one
    /// camera mode only; `None` otherwise.
    slot_wait_over_camera_frame: Option<u64>,
    /// The same count split by the phase lock's state at the tick that
    /// emitted the frame, with the fresh frames behind each part. Before
    /// the lock engages the clock free-runs and a camera slower than the
    /// contract (15 fps into 30) has its frames wait out the unlocked grid;
    /// the first seconds of a window opened soon after the start can hold
    /// many of those, which say nothing about the locked clock. The sum of
    /// both parts is `slot_wait_over_camera_frame`. With no frame in the
    /// locked part (`slot_wait_fresh_locked` is 0) the lock did not engage
    /// in the window (an input rate it does not follow, or the lock off):
    /// the whole-window count is then the one to read. Same windows as
    /// that count; `None` otherwise.
    slot_wait_over_camera_frame_locked: Option<u64>,
    slot_wait_over_camera_frame_unlocked: Option<u64>,
    slot_wait_fresh_locked: Option<u64>,
    slot_wait_fresh_unlocked: Option<u64>,
    /// Camera src pad to slot for the same frames: the ingest pipeline's
    /// own time, whose spread (with the camera's) the lock must wait for.
    ingest_ms: Option<Summary>,
    ingest_percentiles_ms: Vec<(u8, f64)>,
    /// The `appsrc` queue bound, and the highest level seen after a push
    /// in the window.
    queue_bound: u32,
    queue_level_max: u64,
    /// Holds armed by switches in the window, ended by the new camera's
    /// first frame, and ended by the timeout.
    holds: u64,
    holds_released: u64,
    holds_expired: u64,
}

#[derive(Debug, Serialize)]
struct SwitchReport {
    /// None: no camera was streaming (the first start, or a switch after a
    /// failed one).
    from: Option<String>,
    to: String,
    trigger: &'static str,
    source_id: Option<u64>,
    /// Seconds after the measurement window opened (negative: before it).
    requested_at_s: f64,
    /// From the request to the new source's first emitted frame.
    gap_ms: Option<f64>,
    /// From the previous camera's last emitted frame to the new camera's
    /// first: how long the output showed no new image.
    held_ms: Option<f64>,
    repeats: Option<u64>,
    /// Black frames emitted during the switch.
    placeholders: Option<u64>,
    /// Fresh frames of the previous camera emitted after the request.
    previous_fresh: Option<u64>,
    error: Option<String>,
    /// The new camera had not emitted its first frame when the window
    /// closed.
    abandoned: bool,
}

#[derive(Debug, Serialize)]
struct SwitchSection {
    /// From the first camera's start to its first emitted frame.
    initial_start_ms: Option<f64>,
    /// Switches (the first start excluded; switches made with no camera
    /// streaming included) in the records below.
    count: usize,
    /// Of those, reached the new camera's first frame.
    completed: usize,
    /// Of those, the new camera did not start.
    failed: usize,
    /// Of those, still waiting for the new camera's first frame when the
    /// window closed (also counted in `abandoned`).
    abandoned_at_end: usize,
    /// Request -> first fresh frame of the new camera, completed switches.
    gap_ms: Option<Summary>,
    /// Previous camera's last frame -> new camera's first, completed
    /// switches.
    held_ms: Option<Summary>,
    /// The most black frames (placeholders) in one completed switch.
    placeholders_max_per_switch: Option<u64>,
    /// Repeats and placeholders emitted during those gaps, summed.
    repeats_during_gaps: u64,
    placeholders_during_gaps: u64,
    /// Switches over the whole run (streaming may drop old records).
    whole_run_switches: u64,
    /// Switches requested while the previous one still waited for its
    /// first frame, and one still waiting when the window closed.
    abandoned: u64,
    records: Vec<SwitchReport>,
}

#[derive(Debug, Serialize)]
struct LatencySection {
    method: &'static str,
    output_period_ms: f64,
    /// One output period plus one camera frame period: the wait for the
    /// next output tick (up to one output period, ADR-0009) plus one camera
    /// frame period for the stages themselves (the slowest camera's among
    /// those that delivered frames in the window). See
    /// [`latency_budget_ms`].
    budget_ms: f64,
    /// Camera src pad -> sink ready, fresh frames.
    added_primary_ms: Option<Summary>,
    added_at_sink_pad_ms: Option<Summary>,
    via_consumer_ms: Option<Summary>,
    gst_latency_query: Option<LatencyQuery>,
    stamp_mean_us: Option<f64>,
    /// Stamps whose GstBuffer struct was copied (`make_mut`; pixels shared).
    stamp_buffer_copies: u64,
    /// Stamps that made GStreamer copy the pixel data (memory replaced on
    /// map; expected when the output buffer shares the slot frame's memory).
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
struct IngestCaps {
    camera: String,
    mode: String,
    ingest: IngestPath,
    /// CUDAMemory on every link from cudaupload to the appsink.
    cuda_memory_upload_to_slot: bool,
    links: Vec<LinkCaps>,
}

#[derive(Debug, Serialize)]
struct CapsSection {
    /// CUDAMemory on every link from the output's appsrc to cudadownload.
    output_cuda_memory: bool,
    /// Allocator of the buffers arriving at v4l2sink (after cudadownload).
    sink_buffer_memory: Option<String>,
    output_links: Vec<LinkCaps>,
    /// One per camera that streamed (its first run).
    ingest: Vec<IngestCaps>,
    consumer_links: Vec<LinkCaps>,
}

#[derive(Debug, Serialize)]
struct TeardownSection {
    /// The current ingest, then the output pipeline, to NULL.
    session_stop: Result<(), String>,
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
    /// The measured run covered the requested duration: not stopped early
    /// by a signal. Always true when streaming (which ends on a signal).
    run_completed: bool,
    /// The output held the contract rate exactly: every complete 1 s
    /// window on the output's frame grid held the frames the grid puts in
    /// it (the rate; for an N/D rate its floor or ceiling), and the whole
    /// window within 0.1 % (at least one frame) of the rate. The same for
    /// the consumer in measured runs with `--consumer` (streaming cannot
    /// tell the loopback's re-delivered frames apart without the stamp).
    fps_sustained: bool,
    /// The output's `appsrc` dropped (full queue) or refused no buffer; in
    /// measured runs with `--consumer`, the consumer received the frames
    /// that reached the sink (within 3 in flight at the window edges) and
    /// every stamp it read matched an emission.
    no_output_drops: bool,
    /// No output tick skipped by a resynchronisation in the window.
    no_skipped_ticks: bool,
    /// Every switch reached the new camera's first frame; one still waiting
    /// when the window closed counts as abandoned, not as incomplete.
    switches_completed: bool,
    /// No placeholder emitted in the window outside a switch gap: the
    /// output always had a camera frame to send.
    no_placeholders_outside_switches: bool,
    /// No ingest pipeline posted an error or an EOS (whole run).
    no_ingest_errors: bool,
    /// Every ingest run in the window delivered camera frames (except one
    /// started by a switch still waiting when the window closed).
    inputs_delivered: bool,
    /// No ingest queue overruns, and camera src-pad frames match the frames
    /// written into the slot within the frames in flight (3 per ingest run).
    no_ingest_drops: bool,
    /// p95 of primary added latency <= the budget (one output period plus
    /// one camera frame period).
    added_latency_p95_within_budget: bool,
    /// p99 of primary added latency <= the budget.
    added_latency_p99_within_budget: bool,
    /// CUDAMemory from cudaupload to the slot in every ingest, and from the
    /// output's appsrc to cudadownload.
    cuda_memory_links: bool,
}

#[derive(Debug, Serialize)]
struct Report {
    tool: &'static str,
    version: &'static str,
    environment: Environment,
    config: ConfigReport,
    fps: FpsSection,
    frames: FramesSection,
    clock: ClockSection,
    switches: SwitchSection,
    latency: LatencySection,
    cpu: CpuSection,
    gpu: Option<GpuSection>,
    caps: CapsSection,
    camera_controls: Vec<CameraControls>,
    totals: Totals,
    /// The first warnings of the run (at most 200).
    warnings: Vec<String>,
    /// Warnings beyond those, counted only.
    warnings_suppressed: u64,
    teardown: TeardownSection,
    criteria: Criteria,
    /// The effect element, with `--effect` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    effect: Option<EffectSection>,
    /// `--duration 0` only: what the per-frame figures cover.
    #[serde(skip_serializing_if = "Option::is_none")]
    retention: Option<RetentionSection>,
}

/// The effect element over the measured window (`--effect`).
#[derive(Debug, Serialize)]
struct EffectSection {
    /// The effect the run started with (`--effect`).
    kind: String,
    blur_strength: f32,
    background: Option<PathBuf>,
    on_failure: String,
    /// The effect and its status when the window closed.
    final_kind: String,
    status: String,
    reason: Option<String>,
    /// The element's last successful load, warm-up included, in ms, and
    /// whether it reused a loaded model (a switch between effects).
    load_ms: Option<f64>,
    load_reused_model: bool,
    /// Successful loads over the whole run.
    loads: u64,
    /// Frames in the window: processed by the backend, black (warm-up or
    /// failure) and passed through untouched (`none`).
    frames_processed: u64,
    frames_black: u64,
    frames_passthrough: u64,
    /// GPU time of one processed frame (CUDA events around the backend
    /// call), median and p95 over the element's last 300 processed frames.
    gpu_ms_p50: Option<f64>,
    gpu_ms_p95: Option<f64>,
    /// `--denoise`: the denoise stage was requested.
    denoise: bool,
    /// Its status when the window closed (`off` unless requested), and the
    /// reason if it failed (the stage was dropped; the effect ran on).
    denoise_status: String,
    denoise_reason: Option<String>,
    /// Processed frames in the window the denoise stage ran on.
    frames_denoised: u64,
    /// GPU time of the denoise stage alone (the backend's stage timings,
    /// `denoise_ms`), median and p95 over the element's last 300 denoised
    /// frames the backend timed.
    denoise_ms_p50: Option<f64>,
    denoise_ms_p95: Option<f64>,
    /// `--relight`: the relighting light requested (`None` without the
    /// stage), its direction and gain.
    relight: Option<String>,
    relight_pan_deg: f32,
    relight_gain: f32,
    /// Its status and failure reason, its frames and its GPU time, as the
    /// denoise figures.
    relight_status: String,
    relight_reason: Option<String>,
    frames_relit: u64,
    relight_ms_p50: Option<f64>,
    relight_ms_p95: Option<f64>,
    /// Caps events at the sink over the whole run: 1 means no effect change
    /// renegotiated.
    sink_caps_events: u64,
    switch_every_s: Option<f64>,
    switches: Vec<EffectSwitchReport>,
    note: &'static str,
}

/// One `--effect-switch-every` change.
#[derive(Debug, Clone, Serialize)]
struct EffectSwitchReport {
    to: String,
    /// When, in seconds after the window opened.
    at_s: f64,
    /// From the change to the status ready (off for `none`), polled every
    /// 20 ms; `None` if it never got there before the next change.
    ready_after_ms: Option<f64>,
    /// The failure reason, if the change failed.
    reason: Option<String>,
    /// The denoise stage's failure reason, if it was dropped at this change
    /// (the effect ran on without it).
    #[serde(skip_serializing_if = "Option::is_none")]
    denoise_reason: Option<String>,
    /// The relighting stage's failure reason, as `denoise_reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    relight_reason: Option<String>,
    /// Frames from this change to the next (or to the end of the window).
    frames_processed: u64,
    frames_black: u64,
    frames_passthrough: u64,
}

const EFFECT_NOTE: &str = "frames_* are deltas of the element's counters over the window; gpu_ms_* come from the element's window of its last 300 processed frames when the window closed, denoise_ms_* and relight_ms_* from its windows of the backend's stage times over its last 300 frames each stage ran on; load_ms is the backend's load plus its warm-up run, measured by the element";

/// The frame counter deltas between two snapshots.
fn effect_frames(a: &EffectStats, b: &EffectStats) -> (u64, u64, u64) {
    (
        b.frames_processed.saturating_sub(a.frames_processed),
        b.frames_black.saturating_sub(a.frames_black),
        b.frames_passthrough.saturating_sub(a.frames_passthrough),
    )
}

/// Counts the caps events reaching `element`'s sink pad.
fn count_caps_events(p: &MediaPipeline, element: &str) -> Result<Arc<AtomicU64>> {
    let n = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&n);
    p.element(element)
        .and_then(|e| e.static_pad("sink"))
        .ok_or_else(|| anyhow!("no sink pad on {element}"))?
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(e)) = &info.data
                && e.type_() == gst::EventType::Caps
            {
                c.fetch_add(1, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    Ok(n)
}

/// Streaming (`--duration 0`) memory bound: which span the per-frame figures
/// describe. Absent from benchmark reports, which keep every sample.
#[derive(Debug, Serialize)]
struct RetentionSection {
    /// `whole_run` (nothing was dropped) or `retained_window`.
    scope: &'static str,
    /// Per-frame samples (and switch records) older than this are dropped
    /// while streaming.
    retain_cap_s: f64,
    /// Start of the span the per-frame figures cover, in seconds after the
    /// measurement window opened (0 for `whole_run`).
    retained_from_s: f64,
    /// Length of that span (ends with the run).
    retained_s: f64,
    /// Frames that left a camera src pad / reached the output sink over the
    /// whole run.
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

/// A path selector may be a stable link (`/dev/v4l/by-id/...`): match the node.
fn resolve_selector(s: &CameraSelector) -> CameraSelector {
    match s {
        CameraSelector::Path(p) => CameraSelector::Path(resolve_node_path(p)),
        other => other.clone(),
    }
}

/// The switching cycle: every `selectors` entry in order when there are two
/// or more; otherwise the selected camera (or the automatic pick) followed
/// by every other colour camera in discovery order. Each camera's mode is
/// chosen for `request` from its own capabilities.
fn camera_cycle(
    devices: &[DeviceInfo],
    selectors: &[CameraSelector],
    request: &ModeRequest,
) -> Result<Vec<CycleCamera>> {
    let entry = |picked: obc_pipeline::CameraChoice<'_>, how: String| CycleCamera {
        device: picked.device.clone(),
        choice: picked.choice,
        how,
        auto: picked.auto,
    };
    if selectors.len() >= 2 {
        return selectors
            .iter()
            .map(|s| {
                let picked = select_camera(devices, Some(s), request)?;
                Ok(entry(picked, format!("selected by --camera {s}")))
            })
            .collect();
    }
    let selector = selectors.first();
    let picked = select_camera(devices, selector, request)?;
    let how = match (selector, picked.choice.exact) {
        (Some(s), _) => format!("selected by --camera {s}"),
        (None, true) => format!("auto-picked: first colour camera offering {request} exactly"),
        (None, false) => {
            format!("auto-picked: no colour camera offers {request} exactly; best degraded match")
        }
    };
    let first = entry(picked, how);
    let mut cycle = vec![first];
    for d in devices.iter().filter(|d| d.is_colour_camera()) {
        if cycle.iter().any(|c| c.device.path == d.path) {
            continue;
        }
        if let Some(choice) = d
            .capabilities
            .as_ref()
            .and_then(|c| choose_mode(c, request))
        {
            cycle.push(CycleCamera {
                device: d.clone(),
                choice,
                how: "discovered (next in the switching cycle)".into(),
                auto: true,
            });
        }
    }
    Ok(cycle)
}

fn run(args: &Args) -> Result<()> {
    install_signal_handlers()?;
    gst::init().context("gst::init")?;
    // Validate every input before any device is touched.
    args.contract().validate()?;
    obc_pipeline::pipeline::check_io_mode(&args.io_mode)?;
    args.check_effect()?;
    let env = environment();

    // ---- device and mode selection (from enumerated capabilities) ----
    let mut cams = list_cameras(args.discovery, Some(&args.mode));
    let selectors: Vec<CameraSelector> = args.camera.iter().map(resolve_selector).collect();
    for s in &selectors {
        if let CameraSelector::Path(p) = s {
            cams.include_node(p);
        }
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
    let cycle = camera_cycle(&cams.devices, &selectors, &args.mode)?;
    let sink_sel = match &args.sink_device {
        Some(p) => SinkSelector::Path(p.clone()),
        None => SinkSelector::Label(args.sink_label.clone()),
    };
    let sysfs = scan_sysfs();
    let sink = select_sink(&sysfs, &sink_sel)?.clone();
    // Before any device is touched: a range the loopback cannot carry.
    check_sink_range(&args.contract(), &OutputSink::Loopback(sink.path.clone()))?;
    println!(
        "cameras (via {:?}{}):",
        cams.used,
        cams.note
            .as_deref()
            .map(|n| format!("; {n}"))
            .unwrap_or_default()
    );
    for (i, c) in cycle.iter().enumerate() {
        println!(
            "  {}. {} [{}]: requested {} -> {}",
            i + 1,
            c.device,
            c.how,
            args.mode,
            c.choice
        );
    }
    println!("sink:   {sink}");
    if let Some(k) = args.effect {
        let s = args.effect_settings(k);
        println!(
            "effect: {k}{}{} (strength {:.2}{}, on failure {}){}",
            if s.is_enabled("denoise") {
                " + denoise"
            } else {
                ""
            },
            if let Some(light) = &args.relight {
                format!(
                    " + relight {light} (pan {:.0} deg, gain {:.2})",
                    args.relight_pan, args.relight_gain
                )
            } else {
                String::new()
            },
            s.blur_strength,
            s.background
                .as_ref()
                .map(|b| format!(", background {}", b.display()))
                .unwrap_or_default(),
            s.fallback,
            args.effect_switch_every
                .map(|p| format!(", cycling {:?} every {p} s", args.effect_cycle()))
                .unwrap_or_default()
        );
    }
    println!(
        "output: {} ({}, process {}, input gap {} ms)",
        args.contract(),
        args.aspect,
        args.gpu_stage,
        args.input_gap_ms
    );
    run_session(args, env, &cams, cycle, sink)
}

/// Streaming: per-frame samples and switch records older than this are
/// dropped, so memory stays bounded however long the run; the exit report
/// describes this last span (or the whole run, if shorter) and says which.
const STREAM_RETAIN_S: f64 = 600.0;

/// One ingest run: a camera from its start to the switch away from it.
#[derive(Debug, Clone)]
struct SourceRun {
    source_id: u64,
    camera: usize,
    /// When its ingest reached PLAYING.
    started_ns: u64,
    /// When its ingest was asked to stop (a switch) or the window closed.
    stopped_ns: Option<u64>,
}

/// One switch request and its outcome.
#[derive(Debug, Clone)]
struct SwitchRecord {
    from: Option<usize>,
    to: usize,
    trigger: &'static str,
    requested_ns: u64,
    source_id: Option<u64>,
    gap: Option<SwitchGap>,
    error: Option<String>,
    /// Still waiting for the new camera's first frame when the window
    /// closed.
    abandoned: bool,
}

/// The trigger of the first camera's start (not a switch).
const START_TRIGGER: &str = "start";

/// Whether a timer switch due at `due_ns` fires at `now_ns`, in a window
/// ending at `end_ns`: only inside the window, never at or after its end
/// (a switch there could not complete in it).
fn timer_switch_due(now_ns: u64, due_ns: u64, end_ns: u64) -> bool {
    now_ns >= due_ns && now_ns < end_ns && due_ns < end_ns
}

/// Whether the run measured what was asked for: a measured run not stopped
/// early (the window is the requested duration); a streaming run always.
fn run_completed(streaming: bool, interrupted: bool, measured_ns: u64, duration_s: f64) -> bool {
    streaming || (!interrupted && measured_ns >= secs_ns(duration_s))
}

/// The ingest runs overlapping `[from, to)`.
fn runs_overlapping(runs: &[SourceRun], from: u64, to: u64) -> impl Iterator<Item = &SourceRun> {
    runs.iter()
        .filter(move |r| r.started_ns < to && r.stopped_ns.is_none_or(|s| s > from))
}

/// Camera frames of `run` that left its src pad in `[from, to)`.
fn run_frames(samples: &Samples, run: &SourceRun, from: u64, to: u64) -> usize {
    samples
        .src_source
        .iter()
        .zip(&samples.src_times)
        .filter(|(s, t)| **s == run.source_id && (from..to).contains(*t))
        .count()
}

/// The frame period of the slowest camera that delivered frames in
/// `[from, to)` (0: none did).
fn streamed_camera_period_ns(
    runs: &[SourceRun],
    cycle: &[CycleCamera],
    samples: &Samples,
    from: u64,
    to: u64,
) -> u64 {
    runs_overlapping(runs, from, to)
        .filter(|r| run_frames(samples, r, from, to) > 0)
        .map(|r| cycle[r.camera].choice.mode.frame_duration_ns())
        .max()
        .unwrap_or(0)
}

/// Whether every ingest run overlapping `[from, to)` delivered at least one
/// camera frame in it, `waiting` (the source of a switch still waiting for
/// its first frame when the window closed) excepted. False when no run
/// counts.
fn inputs_delivered(
    runs: &[SourceRun],
    samples: &Samples,
    from: u64,
    to: u64,
    waiting: Option<u64>,
) -> bool {
    let mut any = false;
    for r in runs_overlapping(runs, from, to).filter(|r| Some(r.source_id) != waiting) {
        if run_frames(samples, r, from, to) == 0 {
            return false;
        }
        any = true;
    }
    any
}

/// Frame counts on 1 s windows placed on the output's frame grid.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct GridFps {
    /// Complete windows.
    windows: usize,
    /// Of those, windows holding another count than the grid puts in them.
    windows_off_grid: usize,
    min_1s: Option<usize>,
    max_1s: Option<usize>,
    /// Frames over the whole span, and the contract rate times its length.
    frames: usize,
    expected_frames: f64,
    /// Every window on the grid, and the span within 0.1 % (at least one
    /// frame) of the rate.
    exact: bool,
}

/// Counts arrival `times` on complete windows of about 1 s within
/// `[from, to)`, for a stream of `fps_num / fps_den`. The boundaries are
/// placed half a frame period away from the arrivals: at the arrivals'
/// mean phase on the period grid (a circular mean, so a phase near zero
/// does not split), plus half a period, and each window ends at the first
/// such point at or after the next whole second. Arrival jitter under half
/// a period then never moves a frame across a boundary, and a window
/// holds exactly the grid points it spans: the rate, or for an N/D rate
/// whose second is not a whole number of periods, its floor or ceiling.
/// A lost or extra frame shows as a window off the grid, and so does a
/// frame late by half a period or more across a window boundary.
fn grid_fps(times: &[u64], from: u64, to: u64, fps_num: u32, fps_den: u32) -> GridFps {
    use std::f64::consts::TAU;
    let mut t: Vec<u64> = times
        .iter()
        .copied()
        .filter(|&x| x >= from && x < to)
        .collect();
    t.sort_unstable();
    let rate = f64::from(fps_num) / f64::from(fps_den.max(1));
    let expected_frames = rate * to.saturating_sub(from) as f64 / 1e9;
    let mut out = GridFps {
        windows: 0,
        windows_off_grid: 0,
        min_1s: None,
        max_1s: None,
        frames: t.len(),
        expected_frames,
        exact: false,
    };
    if t.is_empty() || fps_num == 0 {
        return out;
    }
    let period = 1e9 / rate;
    let (mut sx, mut sy) = (0.0f64, 0.0f64);
    for &x in &t {
        let a = ((x - from) as f64 % period) / period * TAU;
        sx += a.cos();
        sy += a.sin();
    }
    let phase = (sy.atan2(sx) / TAU).rem_euclid(1.0) * period;
    // Boundary points: from + offset + j x period, j >= 0.
    let offset = (phase + period / 2.0) % period;
    let point = |j: u64| from as f64 + offset + j as f64 * period;
    let first_point_at = |at: f64| ((at - from as f64 - offset) / period).ceil().max(0.0) as u64;
    let count_before = |at: f64| t.partition_point(|&x| (x as f64) < at);
    let mut counts = Vec::new();
    for k in 0u64.. {
        let j0 = first_point_at(from as f64 + k as f64 * 1e9);
        let j1 = first_point_at(from as f64 + (k + 1) as f64 * 1e9);
        let (a, b) = (point(j0), point(j1));
        if b > to as f64 {
            break;
        }
        let n = count_before(b) - count_before(a);
        counts.push(n);
        if n as u64 != j1 - j0 {
            out.windows_off_grid += 1;
        }
    }
    out.windows = counts.len();
    out.min_1s = counts.iter().copied().min();
    out.max_1s = counts.iter().copied().max();
    let tolerance = (expected_frames * 0.001).max(1.0);
    out.exact = out.windows > 0
        && out.windows_off_grid == 0
        && (t.len() as f64 - expected_frames).abs() <= tolerance;
    out
}

/// The 1st, 5th, 10th, 25th, 50th, 75th, 90th, 95th and 99th percentiles.
fn percentiles(values: &[f64]) -> Vec<(u8, f64)> {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    [1u8, 5, 10, 25, 50, 75, 90, 95, 99]
        .into_iter()
        .filter_map(|p| stats::percentile_sorted(&v, f64::from(p)).map(|x| (p, x)))
        .collect()
}

/// Whether `frames` is within max(0.1 %, 1 frame) of `expected`.
fn count_within(frames: usize, expected: f64) -> bool {
    (frames as f64 - expected).abs() <= (expected * 0.001).max(1.0)
}

/// The output PTS of the frames at a pad, in arrival order, checked
/// against the grid of `fps_num / fps_den`.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct PtsSequence {
    frames: usize,
    first_index: Option<u64>,
    last_index: Option<u64>,
    /// Ticks between the first and the last that never arrived.
    missing: u64,
    /// Frames whose index is not above the previous one's (repeated or out
    /// of order).
    out_of_order: u64,
    /// Frames without a PTS, or with one off the tick grid.
    off_grid: u64,
    /// Every index from the first to the last once, in order.
    exact: bool,
}

fn pts_sequence(pts: &[u64], fps_num: u32, fps_den: u32) -> PtsSequence {
    let grid = PacerConfig::new(fps_num.max(1), fps_den.max(1));
    let mut out = PtsSequence {
        frames: pts.len(),
        first_index: None,
        last_index: None,
        missing: 0,
        out_of_order: 0,
        off_grid: 0,
        exact: false,
    };
    let mut prev: Option<u64> = None;
    for &p in pts {
        let i = grid.index_at(p);
        if p == u64::MAX || grid.deadline_ns(i) != p {
            out.off_grid += 1;
            continue;
        }
        out.first_index.get_or_insert(i);
        match prev {
            Some(q) if i <= q => out.out_of_order += 1,
            Some(q) => {
                out.missing += i - q - 1;
                prev = Some(i);
            }
            None => prev = Some(i),
        }
    }
    out.last_index = prev;
    out.exact = out.frames > 0 && out.missing == 0 && out.out_of_order == 0 && out.off_grid == 0;
    out
}

/// Arrival minus PTS grid instant over the window.
#[derive(Debug, Clone, Serialize)]
struct Deviation {
    ms: Option<Summary>,
    /// Max minus min.
    spread_ms: Option<f64>,
    /// One output period plus [`DEVIATION_ALLOWANCE_NS`].
    limit_ms: f64,
    within: bool,
}

fn deviation(dev_ns: &[i64], period_ns: u64) -> Deviation {
    let ms: Vec<f64> = dev_ns.iter().map(|&d| d as f64 / 1e6).collect();
    let summary = stats::summarize(&ms);
    let spread_ms = summary.map(|s| s.max - s.min);
    let limit_ms = (period_ns + DEVIATION_ALLOWANCE_NS) as f64 / 1e6;
    Deviation {
        ms: summary,
        spread_ms,
        limit_ms,
        within: spread_ms.is_some_and(|s| s <= limit_ms),
    }
}

/// The fewest and most arrivals in any 1 s window `[t, t + 1 s)` lying in
/// `[from, to)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Sliding {
    min_1s: Option<usize>,
    max_1s: Option<usize>,
}

/// The extremes are at windows starting on an arrival (most) and just
/// after one (fewest), and at the span's ends.
fn sliding_1s(times: &[u64], from: u64, to: u64) -> Sliding {
    const S: u64 = 1_000_000_000;
    let mut t: Vec<u64> = times
        .iter()
        .copied()
        .filter(|&x| x >= from && x < to)
        .collect();
    t.sort_unstable();
    let count = |a: u64| t.partition_point(|&x| x < a + S) - t.partition_point(|&x| x < a);
    let mut starts: Vec<u64> = vec![from];
    for &x in &t {
        starts.push(x);
        starts.push(x + 1);
    }
    let counts: Vec<usize> = starts
        .into_iter()
        .filter(|&a| a >= from && a.saturating_add(S) <= to)
        .map(count)
        .collect();
    Sliding {
        min_1s: counts.iter().copied().min(),
        max_1s: counts.iter().copied().max(),
    }
}

/// The added-latency budget in ms: up to one output period waiting for
/// the next output tick, plus one camera frame period (`camera_period_ns`,
/// the slowest camera's) for the ingest, slot and output stages.
fn latency_budget_ms(output_period_ns: u64, camera_period_ns: u64) -> f64 {
    output_period_ns.saturating_add(camera_period_ns) as f64 / 1e6
}

/// Field-wise `b - a` of two output counter snapshots.
fn counts_delta(a: &OutputCounts, b: &OutputCounts) -> OutputCounts {
    OutputCounts {
        input_frames: b.input_frames.saturating_sub(a.input_frames),
        fresh: b.fresh.saturating_sub(a.fresh),
        repeat: b.repeat.saturating_sub(a.repeat),
        placeholder: b.placeholder.saturating_sub(a.placeholder),
        superseded: b.superseded.saturating_sub(a.superseded),
        late: b.late.saturating_sub(a.late),
        skipped: b.skipped.saturating_sub(a.skipped),
        emitted: b.emitted.saturating_sub(a.emitted),
        queue_dropped: b.queue_dropped.saturating_sub(a.queue_dropped),
        push_failed: b.push_failed.saturating_sub(a.push_failed),
        // Running maxima and states: the later snapshot's.
        queue_level_max: b.queue_level_max,
        lock_acquisitions: b.lock_acquisitions.saturating_sub(a.lock_acquisitions),
        lock_wraps: b.lock_wraps.saturating_sub(a.lock_wraps),
        phase_offset_ns: b.phase_offset_ns,
        phase_locked: b.phase_locked,
        holds: b.holds.saturating_sub(a.holds),
        holds_released: b.holds_released.saturating_sub(a.holds_released),
        holds_expired: b.holds_expired.saturating_sub(a.holds_expired),
    }
}

/// The switch section from the retained records; times relative to
/// `window_start`.
fn summarize_switches(
    records: &[SwitchRecord],
    cycle: &[CycleCamera],
    window_start: u64,
    whole_run_switches: u64,
    abandoned: u64,
) -> SwitchSection {
    let gap_ms = |r: &SwitchRecord| r.gap.map(|g| g.gap_ns() as f64 / 1e6);
    let switches: Vec<&SwitchRecord> = records
        .iter()
        .filter(|r| r.trigger != START_TRIGGER)
        .collect();
    let completed: Vec<&SwitchGap> = switches.iter().filter_map(|r| r.gap.as_ref()).collect();
    SwitchSection {
        initial_start_ms: records
            .iter()
            .find(|r| r.trigger == START_TRIGGER)
            .and_then(gap_ms),
        count: switches.len(),
        completed: completed.len(),
        failed: switches.iter().filter(|r| r.error.is_some()).count(),
        abandoned_at_end: switches.iter().filter(|r| r.abandoned).count(),
        gap_ms: stats::summarize(
            &completed
                .iter()
                .map(|g| g.gap_ns() as f64 / 1e6)
                .collect::<Vec<_>>(),
        ),
        held_ms: stats::summarize(
            &completed
                .iter()
                .filter_map(|g| g.held_ns())
                .map(|n| n as f64 / 1e6)
                .collect::<Vec<_>>(),
        ),
        placeholders_max_per_switch: completed.iter().map(|g| g.placeholders).max(),
        repeats_during_gaps: completed.iter().map(|g| g.repeats).sum(),
        placeholders_during_gaps: completed.iter().map(|g| g.placeholders).sum(),
        whole_run_switches,
        abandoned,
        records: records
            .iter()
            .map(|r| SwitchReport {
                from: r.from.map(|i| cycle[i].label()),
                to: cycle[r.to].label(),
                trigger: r.trigger,
                source_id: r.source_id,
                requested_at_s: (r.requested_ns as f64 - window_start as f64) / 1e9,
                gap_ms: gap_ms(r),
                held_ms: r.gap.and_then(|g| g.held_ns()).map(|n| n as f64 / 1e6),
                repeats: r.gap.map(|g| g.repeats),
                placeholders: r.gap.map(|g| g.placeholders),
                previous_fresh: r.gap.map(|g| g.previous_fresh),
                error: r.error.clone(),
                abandoned: r.abandoned,
            })
            .collect(),
    }
}

/// Input frame rate per ingest run over `[from, to)`.
fn input_reports(
    samples: &Samples,
    runs: &[SourceRun],
    cycle: &[CycleCamera],
    from: u64,
    to: u64,
) -> Vec<InputFps> {
    let mut out = Vec::new();
    for run in runs {
        let cam = &cycle[run.camera];
        let (times, pts) = samples.source(run.source_id);
        let Some(&first) = times.iter().find(|&&t| t >= from) else {
            continue;
        };
        let end = run.stopped_ns.unwrap_or(to).min(to);
        if end <= first {
            continue;
        }
        let mode = cam.choice.mode;
        out.push(InputFps {
            source_id: run.source_id,
            camera: cam.label(),
            mode: mode.to_string(),
            ingest: cam.choice.ingest,
            span_s: (end - first) as f64 / 1e9,
            fps: stats::fps_report(&times, first, end, mode.fps(), 1.0),
            missing_frames_by_pts: stats::missing_frames_from_gaps(&pts, mode.frame_duration_ns()),
            stalls: stats::count_stalls(&times, mode.frame_duration_ns(), 1.5),
            arrival_jitter_ms: percentiles(&line_residuals_ms(
                &times,
                &pts,
                mode.frame_duration_ns(),
            )),
            pts_jitter_ms: percentiles(&line_residuals_ms(&pts, &pts, mode.frame_duration_ns())),
        });
    }
    out
}

/// The nominal frame duration of the camera mode that fed the window, when
/// every input in it ran in the same mode; `None` with no input or with
/// several modes.
fn single_camera_frame_ns(
    inputs: &[InputFps],
    runs: &[SourceRun],
    cycle: &[CycleCamera],
) -> Option<u64> {
    let mut frames = inputs.iter().filter_map(|i| {
        runs.iter()
            .find(|r| r.source_id == i.source_id)
            .map(|r| cycle[r.camera].choice.mode.frame_duration_ns())
    });
    let first = frames.next()?;
    frames.all(|f| f == first).then_some(first)
}

/// How many of `values` exceed `limit`.
fn count_over(values: &[u64], limit: u64) -> u64 {
    values.iter().filter(|v| **v > limit).count() as u64
}

/// Slot waits over a limit, split by the phase lock's state at the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WaitSplit {
    locked_over: u64,
    locked_total: u64,
    unlocked_over: u64,
    unlocked_total: u64,
}

/// Counts the `waits` over `limit` and the waits themselves, apart for the
/// ticks emitted with the phase lock engaged and for the others. `locked`
/// holds the lock state of each wait; a wait without one (the slices differ
/// in length) counts as unlocked.
fn split_over_by_lock(waits: &[u64], locked: &[bool], limit: u64) -> WaitSplit {
    let mut s = WaitSplit {
        locked_over: 0,
        locked_total: 0,
        unlocked_over: 0,
        unlocked_total: 0,
    };
    for (i, &w) in waits.iter().enumerate() {
        let over = u64::from(w > limit);
        if locked.get(i).copied().unwrap_or(false) {
            s.locked_total += 1;
            s.locked_over += over;
        } else {
            s.unlocked_total += 1;
            s.unlocked_over += over;
        }
    }
    s
}

/// The suffix of the summary's missed-tick figure: where the waits over a
/// camera frame fell relative to the lock engaging. Empty when the figure is
/// not available.
fn lock_split_note(split: Option<WaitSplit>) -> String {
    match split {
        None => String::new(),
        Some(w) if w.locked_total == 0 => " (lock not engaged)".into(),
        Some(w) => format!(
            " ({} of {} after the lock engaged, {} of {} before)",
            w.locked_over, w.locked_total, w.unlocked_over, w.unlocked_total
        ),
    }
}

impl ClockSection {
    /// The split of the slot waits by lock state, as reported.
    fn wait_split(&self) -> Option<WaitSplit> {
        Some(WaitSplit {
            locked_over: self.slot_wait_over_camera_frame_locked?,
            locked_total: self.slot_wait_fresh_locked?,
            unlocked_over: self.slot_wait_over_camera_frame_unlocked?,
            unlocked_total: self.slot_wait_fresh_unlocked?,
        })
    }
}

/// One streaming status line over a status window of `window_s` seconds:
/// `w` its samples, `delta` the output counters over it.
fn status_line(
    camera: Option<&CycleCamera>,
    w: &Samples,
    window_s: f64,
    delta: &OutputCounts,
    consumer: bool,
    cpu_one_core_pct: f64,
) -> String {
    let rate = |n: usize| n as f64 / window_s.max(1e-9);
    let p50 = stats::summarize(&ms(&w.added_latency_ns))
        .map_or("n/a".to_string(), |s| format!("{:.2} ms", s.p50));
    let source = camera.map_or("no camera".to_string(), |c| {
        format!(
            "{} {} | {} {}",
            c.device.card,
            c.device.path.display(),
            c.choice.mode,
            c.choice.ingest
        )
    });
    let consumer = if consumer {
        format!(" | consumer {:.1} fps", rate(w.consumer_times.len()))
    } else {
        String::new()
    };
    format!(
        "{source} | in {:.1} fps | out {:.1} fps{consumer} | fresh {} repeat {} placeholder {} superseded {} | added p50 {p50} | cpu {cpu_one_core_pct:.1}%",
        rate(w.src_times.len()),
        rate(w.sink_times.len()),
        delta.fresh,
        delta.repeat,
        delta.placeholder,
        delta.superseded,
    )
}

/// The line printed when a switch reaches the new camera's first frame.
fn switch_line(r: &SwitchRecord, g: &SwitchGap, cycle: &[CycleCamera]) -> String {
    let gap = g.gap_ns() as f64 / 1e6;
    if r.trigger == START_TRIGGER {
        return format!(
            "camera live: {}: first fresh frame {gap:.1} ms after the start",
            cycle[r.to].label()
        );
    }
    let held = g
        .held_ns()
        .map_or("n/a".to_string(), |h| format!("{:.1} ms", h as f64 / 1e6));
    format!(
        "switch ({}): {} -> {}: first fresh frame {gap:.1} ms after the request; last frame held {held}; {} repeats, {} placeholders during the gap",
        r.trigger,
        r.from.map_or("no camera".to_string(), |f| cycle[f].label()),
        cycle[r.to].label(),
        g.repeats,
        g.placeholders
    )
}

/// Outcome of a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    Done,
    Deadline,
    Interrupted,
}

/// The running stream and everything the bench tracks about it.
struct Stream<'a> {
    args: &'a Args,
    cycle: &'a [CycleCamera],
    rec: Arc<Recorder>,
    session: Session,
    /// Index of the camera streaming now (None: none, after a failed switch).
    current: Option<usize>,
    /// Index of the camera tried last; the next switch tries the one after.
    cursor: usize,
    runs: VecDeque<SourceRun>,
    switches: VecDeque<SwitchRecord>,
    whole_run_switches: u64,
    /// The current ingest posted an error; reported once.
    ingest_failed: bool,
    /// Errors (and EOS) posted by ingest pipelines, whole run.
    ingest_errors: u64,
    overruns: Arc<AtomicU64>,
    bus: BusCounters,
    controls: BTreeMap<usize, CameraControls>,
    ingest_links: BTreeMap<usize, Vec<LinkCaps>>,
}

impl<'a> Stream<'a> {
    fn current_frame_ns(&self) -> u64 {
        self.current
            .map_or(0, |i| self.cycle[i].choice.mode.frame_duration_ns())
    }

    /// Drains the buses for up to `step` and collects completed switches. An
    /// output pipeline error, or an output clock that stopped, ends the run;
    /// an ingest error is counted and reported once per ingest (the output
    /// then repeats and sends placeholders).
    fn poll(&mut self, step: Duration) -> Result<()> {
        let half = step / 2;
        let frame_ns = self.current_frame_ns();
        let output_wait = if self.session.ingest().is_some() {
            half
        } else {
            step
        };
        let output = self.session.output().pipeline();
        drain_bus(
            output,
            &output.pipeline().name(),
            &mut self.bus.qos_dropped,
            &mut self.bus.warnings,
            output_wait,
            self.session.output().contract().frame_duration_ns(),
        )
        .map_err(|e| anyhow!(e))?;
        self.session.output().health()?;
        let source = self
            .current
            .map_or("?".to_string(), |i| self.cycle[i].label());
        if let Some(ingest) = self.session.ingest()
            && let Err(e) = drain_bus(
                ingest.pipeline(),
                &source,
                &mut self.bus.qos_run,
                &mut self.bus.warnings,
                half,
                frame_ns,
            )
        {
            self.ingest_errors += 1;
            if !self.ingest_failed {
                self.ingest_failed = true;
                let msg = format!("ingest {source}: {e}");
                eprintln!("warning: {msg}; the output sends repeats, then placeholders");
                self.bus.warnings.push(msg);
            }
        }
        self.collect_switches();
        Ok(())
    }

    /// Polls until `done`, a stop signal or `deadline_ns`.
    fn wait_until(&mut self, deadline_ns: u64, done: fn(&Self) -> bool) -> Result<Wait> {
        loop {
            if done(self) {
                return Ok(Wait::Done);
            }
            if stop_requested() {
                return Ok(Wait::Interrupted);
            }
            let now = self.rec.now_ns();
            if now >= deadline_ns {
                return Ok(Wait::Deadline);
            }
            self.poll(Duration::from_nanos((deadline_ns - now).min(20_000_000)))?;
        }
    }

    /// The first camera's first frame was emitted.
    fn live(&self) -> bool {
        self.switches.front().is_some_and(|r| r.gap.is_some())
    }

    /// Matches completed switch gaps to their records and prints them.
    fn collect_switches(&mut self) {
        for gap in self.rec.take_switches() {
            if let Some(r) = self
                .switches
                .iter_mut()
                .rev()
                .find(|r| r.source_id == Some(gap.target))
            {
                r.gap = Some(gap);
                println!("{}", switch_line(r, &gap, self.cycle));
            }
        }
    }

    /// Drops records older than the retention (streaming).
    fn trim_records(&mut self) {
        if !self.args.streaming() {
            return;
        }
        let cutoff = self.rec.now_ns().saturating_sub(secs_ns(STREAM_RETAIN_S));
        while self.switches.len() > 1
            && self
                .switches
                .front()
                .is_some_and(|r| r.requested_ns < cutoff && r.gap.is_some())
        {
            self.switches.pop_front();
        }
        while self
            .runs
            .front()
            .is_some_and(|r| r.stopped_ns.is_some_and(|s| s < cutoff))
        {
            self.runs.pop_front();
        }
    }

    fn ingest_config(&self, camera: usize) -> IngestConfig {
        let c = &self.cycle[camera];
        let mut cfg = IngestConfig::new(c.device.path.clone(), c.choice.mode);
        cfg.aspect = self.args.aspect;
        cfg.queue_buffers = self.args.queue_buffers;
        cfg.camera_io_mode = self.args.io_mode.clone();
        cfg.fixed_framerate = self.args.camera_fixed_framerate;
        cfg
    }

    /// Keeps the negotiated links of the running ingest (first run of each
    /// camera).
    fn capture_ingest_links(&mut self) {
        if let (Some(i), Some(ingest)) = (self.current, self.session.ingest()) {
            self.ingest_links
                .entry(i)
                .or_insert_with(|| ingest.pipeline().links());
        }
    }

    fn end_current_run(&mut self, t: u64) {
        if let Some(r) = self.runs.back_mut()
            && r.stopped_ns.is_none()
        {
            r.stopped_ns = Some(t);
        }
    }

    /// Switches to camera `to`: builds its ingest, stops the current one
    /// (camera closed, controls restored) and starts the new one. On error
    /// the record keeps it, and the stream has no source if the old one was
    /// already stopped.
    fn switch_to(&mut self, to: usize, trigger: &'static str) -> Result<()> {
        let requested_ns = self.rec.now_ns();
        self.rec.begin_switch(requested_ns);
        self.cursor = to;
        let mut record = SwitchRecord {
            from: self.current,
            to,
            trigger,
            requested_ns,
            source_id: None,
            gap: None,
            error: None,
            abandoned: false,
        };
        let res = self.try_switch(to, &mut record);
        if let Err(e) = &res {
            self.rec.cancel_switch();
            record.error = Some(format!("{e:#}"));
            if self.session.ingest().is_none() {
                self.end_current_run(self.rec.now_ns());
                self.current = None;
            }
        }
        if trigger != START_TRIGGER {
            self.whole_run_switches += 1;
        }
        self.switches.push_back(record);
        self.trim_records();
        res
    }

    fn try_switch(&mut self, to: usize, record: &mut SwitchRecord) -> Result<()> {
        let cfg = self.ingest_config(to);
        let ingest = self.session.build_ingest(&cfg)?;
        let id = ingest.source_id();
        record.source_id = Some(id);
        self.rec.switch_target(id);
        self.rec
            .attach_camera(ingest.pipeline(), id)
            .map_err(|e| anyhow!(e))?;
        if let Some(q) = ingest.pipeline().element(names::QUEUE) {
            let c = Arc::clone(&self.overruns);
            q.connect("overrun", false, move |_| {
                c.fetch_add(1, Ordering::Relaxed);
                None
            });
        }
        self.capture_ingest_links();
        // The current run ends when its ingest is asked to stop.
        let stopping = self.rec.now_ns();
        let res = self.session.switch_to(ingest);
        self.end_current_run(stopping);
        self.bus.end_ingest_run();
        let timing = res?;
        self.ingest_failed = false;
        self.current = Some(to);
        self.runs.push_back(SourceRun {
            source_id: id,
            camera: to,
            started_ns: timing
                .started
                .map_or_else(|| self.rec.now_ns(), |t| self.rec.ns_at(t)),
            stopped_ns: None,
        });
        if let Some(report) = self.session.ingest().map(|i| i.controls().clone()) {
            self.note_controls(to, &report);
        }
        Ok(())
    }

    /// Records a camera's control state when its ingest starts.
    fn note_controls(&mut self, camera: usize, r: &ControlReport) {
        let path = &self.cycle[camera].device.path;
        match self.controls.get_mut(&camera) {
            None => {
                if r.during == Some(1) {
                    println!(
                        "note: {} exposure_dynamic_framerate=1: it may lower its frame rate in dim light (see --camera-fixed-framerate)",
                        self.cycle[camera].label()
                    );
                }
                if r.read_only {
                    self.args.debug(format!(
                        "{} exposure_dynamic_framerate is read-only; not pinned",
                        path.display()
                    ));
                }
                if let Some(skip) = &r.skipped {
                    self.args.debug(format!(
                        "{} exposure_dynamic_framerate skipped: {skip}",
                        path.display()
                    ));
                }
                self.controls.insert(camera, CameraControls::first(path, r));
            }
            Some(c) => {
                // A later run starts from the value the previous one restored.
                if r.before != c.exposure_dynamic_framerate_before {
                    let msg = format!(
                        "{}: exposure_dynamic_framerate was {:?} at a restart, {:?} originally (not restored by the previous run?)",
                        path.display(),
                        r.before,
                        c.exposure_dynamic_framerate_before
                    );
                    eprintln!("warning: {msg}");
                    self.bus.warnings.push(msg);
                }
                c.exposure_dynamic_framerate_during = r.during;
                c.pinned |= r.pinned;
            }
        }
    }

    /// Switches to the next camera of the cycle; a failure is a warning.
    fn switch_next(&mut self, trigger: &'static str) {
        let to = (self.cursor + 1) % self.cycle.len();
        if let Err(e) = self.switch_to(to, trigger) {
            let msg = format!(
                "switch ({trigger}) to {} failed: {e:#}",
                self.cycle[to].label()
            );
            eprintln!("warning: {msg}");
            self.bus.warnings.push(msg);
        }
    }
}

/// What the measurement loop hands to the report.
struct Measured {
    cpu: stats::CpuUsage,
    per_sec: Vec<f64>,
    latency_q: Option<(bool, u64, Option<u64>)>,
    output_links: Vec<LinkCaps>,
    consumer_links: Vec<LinkCaps>,
    interrupted: bool,
    counts_start: OutputCounts,
    counts_end: OutputCounts,
    effect_start: Option<EffectStats>,
    effect_end: Option<EffectStats>,
    effect_switches: Vec<EffectSwitchReport>,
}

fn run_session(
    args: &Args,
    env: Environment,
    cams: &CameraList,
    cycle: Vec<CycleCamera>,
    sink: DeviceInfo,
) -> Result<()> {
    let contract = args.contract();
    let streaming = args.streaming();
    let rec = Recorder::new(args.consumer, args.consumer_stamp());
    if streaming {
        rec.set_retention(Some(secs_ns(STREAM_RETAIN_S)));
    }

    // ---- GPU baseline (before any pipeline exists) ----
    let mut early_warnings = Warnings::default();
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
                early_warnings.push(format!("GPU stats disabled: {e}"));
                None
            }
        }
    };
    let t_build = rec.now_ns();

    // ---- output: build, instrument, start ----
    let mut ocfg = OutputConfig::new(contract, OutputSink::Loopback(sink.path.clone()));
    ocfg.process = args.gpu_stage;
    ocfg.effect = args.effect.map(|k| args.effect_settings(k));
    if ocfg.effect.is_some() {
        ocfg.effect_backend = Some(effect_backend(args.modules.as_deref())?);
    }
    ocfg.gap_threshold = Duration::from_millis(args.input_gap_ms);
    ocfg.hold_timeout = Duration::from_secs_f64(args.hold_timeout);
    ocfg.queue_buffers = args.output_queue_buffers;
    if let Some(m) = args.phase_margin_ms {
        ocfg.phase_lock.margin_ns = (m * 1e6) as u64;
    }
    if let Some(k) = args.phase_window {
        ocfg.phase_lock.window = k as usize;
    }
    if let Some(j) = args.phase_jump_ms {
        ocfg.phase_lock.jump_ns = (j * 1e6) as u64;
    }
    if args.no_phase_lock {
        ocfg.phase_lock = PhaseLockConfig::disabled();
    }
    let phase_lock = ocfg.phase_lock.enabled.then_some(ocfg.phase_lock);
    let mut output = Output::build(ocfg)?;
    output.set_observer(Some(rec.observer()));
    rec.attach_output(output.pipeline())
        .map_err(|e| anyhow!(e))?;
    let sink_caps_events = count_caps_events(output.pipeline(), names::LOOPBACK)?;
    if stop_requested() {
        bail!("interrupted by signal before the measurement window");
    }
    let session = Session::new(output)?;
    let mut s = Stream {
        args,
        cycle: &cycle,
        rec: Arc::clone(&rec),
        session,
        current: None,
        cursor: 0,
        runs: VecDeque::new(),
        switches: VecDeque::new(),
        whole_run_switches: 0,
        ingest_failed: false,
        ingest_errors: 0,
        overruns: Arc::new(AtomicU64::new(0)),
        bus: BusCounters {
            warnings: early_warnings,
            ..BusCounters::default()
        },
        controls: BTreeMap::new(),
        ingest_links: BTreeMap::new(),
    };

    let interrupted_before = || anyhow!("interrupted by signal before the measurement window");
    // `consumer` is declared after `s`, so an early return drops it (NULL,
    // loopback released) before the session.
    let mut consumer: Option<MediaPipeline> = None;
    let outcome = (|| -> Result<Measured> {
        match s.wait_until(rec.now_ns() + secs_ns(15.0), |s| {
            s.rec.first_sink_ns().is_some()
        })? {
            Wait::Done => {}
            Wait::Interrupted => return Err(interrupted_before()),
            Wait::Deadline => bail!("no output frame reached the loopback sink within 15 s"),
        }
        s.switch_to(0, "start")?;
        match s.wait_until(rec.now_ns() + secs_ns(15.0), Stream::live)? {
            Wait::Done => {}
            Wait::Interrupted => return Err(interrupted_before()),
            Wait::Deadline => bail!(
                "no frame of {} reached the output within 15 s",
                cycle[0].label()
            ),
        }
        // A requested effect, and each requested stage, must be ready (or
        // fail) before the warm-up.
        if args
            .effect
            .is_some_and(|k| k != EffectKind::None || args.denoise || args.relight.is_some())
        {
            let settled = |s: &Stream| {
                let o = s.session.output();
                !matches!(o.effect_status(), EffectStatus::Loading)
                    && !matches!(o.stage_status("denoise"), EffectStatus::Loading)
                    && !matches!(o.stage_status("relight"), EffectStatus::Loading)
            };
            match s.wait_until(rec.now_ns() + secs_ns(60.0), settled)? {
                Wait::Done => {}
                Wait::Interrupted => return Err(interrupted_before()),
                Wait::Deadline => bail!("the effect was not ready within 60 s"),
            }
            if let EffectStatus::Failed(reason) = s.session.output().effect_status() {
                bail!("the effect failed: {reason}");
            }
            if let EffectStatus::Failed(reason) = s.session.output().stage_status("denoise") {
                bail!("the denoise stage failed: {reason}");
            }
            if let EffectStatus::Failed(reason) = s.session.output().stage_status("relight") {
                bail!("the relighting stage failed: {reason}");
            }
        }
        let t_first = rec.now_ns();

        if args.consumer {
            let c = MediaPipeline::build_consumer(
                &sink.path,
                (contract.width, contract.height),
                contract.format,
            )?;
            rec.attach_consumer(&c).map_err(|e| anyhow!(e))?;
            c.play()?;
            consumer = Some(c);
            match s.wait_until(rec.now_ns() + secs_ns(10.0), |s| {
                s.rec.first_consumer_ns().is_some()
            })? {
                Wait::Done => {}
                Wait::Interrupted => return Err(interrupted_before()),
                Wait::Deadline => bail!("the loopback consumer received no frame within 10 s"),
            }
        }

        // Dump the graphs before the window opens, so writing them costs
        // neither measured CPU nor measured frames.
        if let Some(dir) = &args.dot_dir {
            std::fs::create_dir_all(dir)?;
            let tag = format!("{}-{}", args.gpu_stage, contract.format);
            std::fs::write(
                dir.join(format!("obc-output-{tag}.dot")),
                s.session.output().pipeline().dot(),
            )?;
            if let Some(i) = s.session.ingest() {
                std::fs::write(
                    dir.join(format!("obc-ingest-{}.dot", i.source_id())),
                    i.pipeline().dot(),
                )?;
            }
            if let Some(c) = &consumer {
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
        let switching = args
            .switch_every
            .map_or(String::new(), |p| format!(", next camera every {p} s"));
        if streaming {
            println!(
                "streaming until SIGINT/SIGTERM after {:.1} s warm-up (output {contract}, {}{switching}); status every 5 s, SIGUSR1 switches to the next camera",
                args.warmup, args.gpu_stage
            );
        } else {
            println!(
                "measuring {:.0} s after {:.1} s warm-up (output {contract}, {}{switching})",
                args.duration, args.warmup, args.gpu_stage
            );
        }
        if s.wait_until(start, |_| false)? == Wait::Interrupted {
            return Err(interrupted_before());
        }
        // Switching acts inside the window only: a SIGUSR1 received during
        // the camera start or the warm-up is discarded here.
        if NEXT.swap(false, Ordering::SeqCst) {
            println!("note: SIGUSR1 received before the measurement window; ignored");
        }
        let counts_start = s.session.output().counts();
        let effect_start = s.session.output().effect_stats();
        let ticks_start = stats::self_cpu_ticks().unwrap_or(0);
        let wall_start = rec.now_ns();

        // Streaming keeps the last STREAM_RETAIN_S 1 s segments only.
        let mut per_sec: VecDeque<f64> = VecDeque::new();
        let (mut last_ticks, mut last_t) = (ticks_start, wall_start);
        let mut next_sec = wall_start + 1_000_000_000;
        let report_every: u64 = if streaming {
            5_000_000_000
        } else {
            10_000_000_000
        };
        let mut next_report = wall_start + report_every;
        let (mut status_ticks, mut status_t) = (ticks_start, wall_start);
        let mut status_counts = counts_start;
        let switch_period = args.switch_every.map(secs_ns);
        let mut next_switch = switch_period.map(|p| wall_start + p);
        let effect_period = args.effect_switch_every.map(secs_ns);
        let mut next_effect = effect_period.map(|p| wall_start + p);
        let effect_cycle = args.effect_cycle();
        let mut effect_at = args
            .effect
            .and_then(|k| effect_cycle.iter().position(|c| *c == k))
            .unwrap_or(0);
        let mut effect_switches: Vec<EffectSwitchReport> = Vec::new();
        // The open switch: its start time and the stats at its start.
        let mut effect_open: Option<(u64, EffectStats)> = None;
        let mut effect_pending = false;
        let mut interrupted = false;
        loop {
            let mut deadline = next_sec.min(next_report).min(end);
            if let Some(t) = next_switch {
                deadline = deadline.min(t);
            }
            if let Some(t) = next_effect {
                deadline = deadline.min(t);
            }
            if effect_pending {
                deadline = deadline.min(rec.now_ns() + 20_000_000);
            }
            if s.wait_until(deadline, |_| next_requested())? == Wait::Interrupted {
                // Close the window here: the report covers what was measured.
                interrupted = true;
                end = rec.now_ns().max(start);
                rec.set_window(start, end);
            }
            if !interrupted && NEXT.swap(false, Ordering::SeqCst) {
                s.switch_next("SIGUSR1");
            }
            if let (Some(t), Some(p)) = (next_switch, switch_period)
                && !interrupted
                && timer_switch_due(rec.now_ns(), t, end)
            {
                s.switch_next("timer");
                let now = rec.now_ns();
                next_switch = Some(if t + p > now { t + p } else { now + p });
            }
            if effect_pending && let Some(r) = effect_switches.last_mut() {
                let status = s.session.output().effect_status();
                let denoise = s.session.output().stage_status("denoise");
                let relight = s.session.output().stage_status("relight");
                let (at, _) = effect_open.as_ref().expect("an open switch");
                let done = match &status {
                    EffectStatus::Ready | EffectStatus::Off => true,
                    EffectStatus::Failed(reason) => {
                        r.reason = Some(reason.clone());
                        true
                    }
                    EffectStatus::Loading => false,
                } && match &denoise {
                    EffectStatus::Loading => false,
                    EffectStatus::Failed(reason) => {
                        r.denoise_reason = Some(reason.clone());
                        true
                    }
                    EffectStatus::Ready | EffectStatus::Off => true,
                } && match &relight {
                    EffectStatus::Loading => false,
                    EffectStatus::Failed(reason) => {
                        r.relight_reason = Some(reason.clone());
                        true
                    }
                    EffectStatus::Ready | EffectStatus::Off => true,
                };
                if done {
                    if r.reason.is_none() {
                        r.ready_after_ms = Some((rec.now_ns() - at) as f64 / 1e6);
                    }
                    effect_pending = false;
                }
            }
            if let (Some(t), Some(p)) = (next_effect, effect_period)
                && !interrupted
                && timer_switch_due(rec.now_ns(), t, end)
            {
                let stats_now = s.session.output().effect_stats().unwrap_or_default();
                if let (Some((_, before)), Some(r)) = (&effect_open, effect_switches.last_mut()) {
                    (r.frames_processed, r.frames_black, r.frames_passthrough) =
                        effect_frames(before, &stats_now);
                }
                effect_at = (effect_at + 1) % effect_cycle.len();
                let to = effect_cycle[effect_at];
                let now = rec.now_ns();
                let mut record = EffectSwitchReport {
                    to: to.to_string(),
                    at_s: (now - wall_start) as f64 / 1e9,
                    ready_after_ms: None,
                    reason: None,
                    denoise_reason: None,
                    relight_reason: None,
                    frames_processed: 0,
                    frames_black: 0,
                    frames_passthrough: 0,
                };
                if let Err(e) = s.session.output().set_effect(&args.effect_settings(to)) {
                    record.reason = Some(e.to_string());
                } else {
                    effect_pending = true;
                }
                println!("  effect -> {to} at t+{:.1}s", record.at_s);
                effect_switches.push(record);
                effect_open = Some((now, stats_now));
                next_effect = Some(if t + p > now { t + p } else { now + p });
            }
            let now = rec.now_ns();
            if now >= next_sec || interrupted || now >= end {
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
                // 1 s segments on a fixed grid from wall_start (no drift).
                while next_sec <= now {
                    next_sec += 1_000_000_000;
                }
            }
            if now >= next_report && !interrupted {
                let elapsed = (now - wall_start) as f64 / 1e9;
                if streaming {
                    let ticks = stats::self_cpu_ticks().unwrap_or(status_ticks);
                    let w = rec.samples_between(status_t, now);
                    let counts = s.session.output().counts();
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
                            s.current.map(|i| &cycle[i]),
                            &w,
                            window_s,
                            &counts_delta(&status_counts, &counts),
                            args.consumer,
                            cpu.one_core_pct
                        )
                    );
                    (status_ticks, status_t, status_counts) = (ticks, now, counts);
                } else {
                    let t = rec.totals();
                    println!(
                        "  t+{elapsed:>4.0}s  output frames {}  consumer frames {}",
                        t.sink_buffers, t.consumer_buffers
                    );
                }
                while next_report <= now {
                    next_report += report_every;
                }
            }
            if interrupted || now >= end {
                break;
            }
        }
        let counts_end = s.session.output().counts();
        let effect_end = s.session.output().effect_stats();
        if let (Some((_, before)), Some(r), Some(after)) =
            (&effect_open, effect_switches.last_mut(), &effect_end)
        {
            (r.frames_processed, r.frames_black, r.frames_passthrough) =
                effect_frames(before, after);
        }
        let ticks_end = stats::self_cpu_ticks().unwrap_or(0);
        let wall_end = rec.now_ns();
        // A switch still waiting for its first frame is abandoned, not
        // incomplete: the window closed under it.
        s.collect_switches();
        if rec.abandon_open_switch()
            && let Some(r) = s.switches.back_mut()
            && r.gap.is_none()
            && r.error.is_none()
        {
            r.abandoned = true;
        }
        let cpu = stats::cpu_usage(
            ticks_end.saturating_sub(ticks_start),
            stats::ticks_per_sec(),
            (wall_end - wall_start) as f64 / 1e9,
            stats::online_cpus(),
        );
        s.capture_ingest_links();
        Ok(Measured {
            cpu,
            per_sec: Vec::from(per_sec),
            latency_q: s.session.output().pipeline().query_latency(),
            output_links: s.session.output().pipeline().links(),
            consumer_links: consumer.as_ref().map(|c| c.links()).unwrap_or_default(),
            interrupted,
            counts_start,
            counts_end,
            effect_start,
            effect_end,
            effect_switches,
        })
    })();

    // ---- teardown: NVML first, then consumer, then ingest and output ----
    let (gpu_samples, nvml_shutdown, gpu_info) = match sampler {
        Some(smp) => {
            let info = smp.info().clone();
            let (samples, res) = smp.stop();
            (samples, Some(res), Some(info))
        }
        None => (Vec::new(), None, None),
    };
    let consumer_null = consumer.map(|c| c.shutdown().map_err(|e| e.to_string()));
    // On error, dropping the stream stops the ingest (restoring its
    // controls) and then the output.
    let m = outcome?;
    s.collect_switches();
    let samples: Samples = rec.samples();
    let (win_start, win_end) = rec.window().unwrap_or((0, 0));
    let Stream {
        session,
        runs,
        switches,
        whole_run_switches,
        ingest_errors,
        overruns,
        mut bus,
        mut controls,
        ingest_links,
        ..
    } = s;
    let mut runs: Vec<SourceRun> = runs.into();
    if let Some(r) = runs.last_mut()
        && r.stopped_ns.is_none()
    {
        r.stopped_ns = Some(win_end);
    }
    let session_stop = session.stop().map_err(|e| e.to_string());
    for (&i, c) in controls.iter_mut() {
        if c.exposure_dynamic_framerate_before.is_some() {
            c.exposure_dynamic_framerate_after_exit =
                get_ctrl(&cycle[i].device.path, CID_EXPOSURE_AUTO_PRIORITY).ok();
            c.restored = c
                .exposure_dynamic_framerate_after_exit
                .map(|a| Some(a) == c.exposure_dynamic_framerate_before);
        }
    }
    let nv_fds = nvidia_fds();
    let cuda_mapped = libcuda_mapped();

    // ---- report ----
    let target = contract.fps();
    let period_ns = contract.frame_duration_ns();
    // Streaming may have dropped old samples: per-frame figures then cover
    // the retained span [stats_start, win_end), stated in the report.
    let stats_start = samples
        .retained_from_ns
        .map_or(win_start, |f| f.max(win_start));
    let output_fps = stats::fps_report(&samples.sink_times, stats_start, win_end, target, 1.0);
    let consumer_fps = args
        .consumer
        .then(|| stats::fps_report(&samples.consumer_times, stats_start, win_end, target, 1.0));
    let inputs = input_reports(&samples, &runs, &cycle, stats_start, win_end);
    let camera_frame_ns = single_camera_frame_ns(&inputs, &runs, &cycle);
    let slot_wait_over_camera_frame =
        camera_frame_ns.map(|frame_ns| count_over(&samples.slot_wait_ns, frame_ns));
    let wait_split = camera_frame_ns.map(|frame_ns| {
        split_over_by_lock(&samples.slot_wait_ns, &samples.slot_wait_locked, frame_ns)
    });
    let delta = counts_delta(&m.counts_start, &m.counts_end);
    let ingest_lost = samples.counts.src as i64 - delta.input_frames as i64;
    let runs_in_window = runs
        .iter()
        .filter(|r| r.stopped_ns.is_none_or(|t| t > win_start) && r.started_ns < win_end)
        .count()
        .max(1);
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
        note: "Streaming keeps per-frame samples and switch records for the last retain_cap_s only. scope=whole_run: the run was shorter, every figure covers it all. scope=retained_window: fps, stalls, inputs, latency, switches and per_second_one_core_pct cover the last retained_s (from retained_from_s after the window opened); config.measured_s, cpu.one_core_pct, frames, criteria.no_ingest_drops, totals and qos_dropped_by_element cover the whole run.",
    });
    let added = stats::summarize(&ms(&samples.added_latency_ns));
    let via_consumer = stats::summarize(&ms(&samples.consumer_latency_ns));
    // Cameras that never delivered a frame in the window do not set it.
    let camera_period_ns = streamed_camera_period_ns(&runs, &cycle, &samples, stats_start, win_end);
    let budget_ms = latency_budget_ms(period_ns, camera_period_ns);
    let output_cuda =
        all_cuda_between(&m.output_links, names::FRAMES_SRC, names::DOWNLOAD) == Some(true);
    let ingest_caps: Vec<IngestCaps> = ingest_links
        .into_iter()
        .map(|(i, links)| IngestCaps {
            camera: cycle[i].label(),
            mode: cycle[i].choice.mode.to_string(),
            ingest: cycle[i].choice.ingest,
            cuda_memory_upload_to_slot: all_cuda_between(&links, names::UPLOAD, names::FRAMES_SINK)
                == Some(true),
            links,
        })
        .collect();
    let switches: Vec<SwitchRecord> = switches.into();
    let switch_section = summarize_switches(
        &switches,
        &cycle,
        win_start,
        whole_run_switches,
        samples.totals.switches_abandoned,
    );
    let queue_overruns = overruns.load(Ordering::Relaxed);

    let grid = |times: &[u64]| {
        grid_fps(
            times,
            stats_start,
            win_end,
            contract.fps_num,
            contract.fps_den,
        )
    };
    let output_grid = grid(&samples.sink_times);
    let consumer_grid = args.consumer.then(|| grid(&samples.consumer_times));
    let output_pts = pts_sequence(&samples.sink_pts, contract.fps_num, contract.fps_den);
    let output_deviation = deviation(&samples.sink_deviation_ns, period_ns);
    let output_sliding = sliding_1s(&samples.sink_times, stats_start, win_end);
    // The consumer is judged where the stamp tells re-delivered frames
    // apart (measured runs).
    let judge_consumer = args.consumer_stamp();
    let consumer_pts = judge_consumer
        .then(|| pts_sequence(&samples.consumer_pts, contract.fps_num, contract.fps_den));
    let consumer_deviation =
        judge_consumer.then(|| deviation(&samples.consumer_deviation_ns, period_ns));
    let consumer_sliding = args
        .consumer
        .then(|| sliding_1s(&samples.consumer_times, stats_start, win_end));
    let expected_frames = output_grid.expected_frames;
    let output_rate_held = output_pts.exact
        && count_within(samples.sink_times.len(), expected_frames)
        && output_deviation.within;
    let consumer_rate_held = !judge_consumer
        || (consumer_pts.as_ref().is_some_and(|p| p.exact)
            && count_within(samples.consumer_times.len(), expected_frames)
            && consumer_deviation.as_ref().is_some_and(|d| d.within));
    let consumer_received_all = !judge_consumer
        || ((samples.counts.consumer as i64 - samples.counts.sink as i64).abs() <= 3
            && samples.totals.consumer_unmatched == 0);
    let waiting_source = switches
        .iter()
        .rev()
        .find(|r| r.abandoned)
        .and_then(|r| r.source_id);
    let criteria = Criteria {
        run_completed: run_completed(
            streaming,
            m.interrupted,
            win_end.saturating_sub(win_start),
            args.duration,
        ),
        fps_sustained: output_rate_held && consumer_rate_held,
        no_output_drops: delta.queue_dropped == 0
            && delta.push_failed == 0
            && consumer_received_all,
        no_skipped_ticks: delta.skipped == 0,
        switches_completed: switch_section.failed == 0
            && switch_section.completed + switch_section.abandoned_at_end == switch_section.count,
        no_placeholders_outside_switches: samples.counts.placeholders_outside_switches == 0,
        no_ingest_errors: ingest_errors == 0,
        inputs_delivered: inputs_delivered(&runs, &samples, stats_start, win_end, waiting_source),
        no_ingest_drops: queue_overruns == 0 && ingest_lost.abs() <= 3 * runs_in_window as i64,
        added_latency_p95_within_budget: added.is_some_and(|s| s.p95 <= budget_ms),
        added_latency_p99_within_budget: added.is_some_and(|s| s.p99 <= budget_ms),
        cuda_memory_links: output_cuda
            && !ingest_caps.is_empty()
            && ingest_caps.iter().all(|c| c.cuda_memory_upload_to_slot),
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

    let qos_dropped = bus.take_qos_dropped();
    let decoder_dropped_frames = ingest_caps
        .iter()
        .any(|c| c.ingest == IngestPath::CpuDecode)
        .then(|| {
            qos_dropped
                .iter()
                .filter(|(k, _)| k.ends_with(&format!("/{}", names::DECODE)))
                .map(|(_, v)| v)
                .sum()
        });

    let effect = effect_section(args, &m, sink_caps_events.load(Ordering::Relaxed));
    let report = Report {
        tool: "obc-bench",
        version: env!("CARGO_PKG_VERSION"),
        environment: env,
        config: ConfigReport {
            cameras: cycle
                .iter()
                .map(|c| CameraEntry {
                    camera: c.device.clone(),
                    how: c.how.clone(),
                    mode: c.choice.mode.to_string(),
                    mode_exact: c.choice.exact,
                    mode_differences: c.choice.differences.clone(),
                    ingest: c.choice.ingest,
                })
                .collect(),
            sink,
            camera_discovery: format!("{:?}", cams.used),
            camera_discovery_note: cams.note.clone(),
            sink_discovery: "sysfs (/sys/class/video4linux/*/name, virtual parent + v4l2loopback attributes)",
            camera_auto_picked: cycle[0].auto,
            requested_mode: args.mode.to_string(),
            output: contract,
            output_caps: contract_caps_string(&contract),
            aspect: args.aspect,
            process: args.gpu_stage,
            input_gap_ms: args.input_gap_ms,
            hold_timeout_s: args.hold_timeout,
            output_queue_buffers: args.output_queue_buffers,
            phase_lock,
            queue_buffers: args.queue_buffers,
            camera_io_mode: args.io_mode.clone(),
            camera_fixed_framerate: args.camera_fixed_framerate,
            switch_every_s: args.switch_every,
            streaming,
            duration_s: args.duration,
            measured_s: win_end.saturating_sub(win_start) as f64 / 1e9,
            interrupted: m.interrupted,
            warmup_s: args.warmup,
            consumer: args.consumer,
            consumer_stamp: args.consumer_stamp(),
        },
        fps: FpsSection {
            target,
            method: FPS_METHOD,
            output_pts,
            consumer_pts,
            output_deviation,
            consumer_deviation,
            output_sliding_1s: output_sliding,
            consumer_sliding_1s: consumer_sliding,
            output_stalls: stats::count_stalls(&samples.sink_times, period_ns, 1.5),
            consumer_stalls: args
                .consumer
                .then(|| stats::count_stalls(&samples.consumer_times, period_ns, 1.5)),
            output: output_fps,
            consumer: consumer_fps,
            output_grid,
            consumer_grid,
            inputs,
        },
        frames: FramesSection {
            output: delta,
            camera_src_frames: samples.counts.src,
            ingest_lost_frames: ingest_lost,
            queue_overruns,
            qos_dropped_by_element: qos_dropped,
            decoder_dropped_frames,
        },
        clock: ClockSection {
            phase_lock,
            lock_acquisitions: delta.lock_acquisitions,
            lock_wraps: delta.lock_wraps,
            phase_offset_ms: delta.phase_offset_ns as f64 / 1e6,
            phase_locked: delta.phase_locked,
            slot_wait_ms: stats::summarize(&ms(&samples.slot_wait_ns)),
            slot_wait_percentiles_ms: percentiles(&ms(&samples.slot_wait_ns)),
            slot_wait_over_camera_frame,
            slot_wait_over_camera_frame_locked: wait_split.map(|w| w.locked_over),
            slot_wait_over_camera_frame_unlocked: wait_split.map(|w| w.unlocked_over),
            slot_wait_fresh_locked: wait_split.map(|w| w.locked_total),
            slot_wait_fresh_unlocked: wait_split.map(|w| w.unlocked_total),
            ingest_ms: stats::summarize(&ms(&samples.ingest_ns)),
            ingest_percentiles_ms: percentiles(&ms(&samples.ingest_ns)),
            queue_bound: args.output_queue_buffers,
            queue_level_max: samples.counts.queue_level_max,
            holds: delta.holds,
            holds_released: delta.holds_released,
            holds_expired: delta.holds_expired,
        },
        switches: switch_section,
        latency: LatencySection {
            method: METHOD,
            output_period_ms: period_ns as f64 / 1e6,
            budget_ms,
            added_primary_ms: added,
            added_at_sink_pad_ms: stats::summarize(&ms(&samples.added_at_pad_ns)),
            via_consumer_ms: via_consumer,
            gst_latency_query: m.latency_q.map(|(live, min, max)| LatencyQuery {
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
            one_core_pct: m.cpu.one_core_pct,
            all_cores_pct: m.cpu.all_cores_pct,
            per_second_one_core_pct: stats::summarize(&m.per_sec),
            online_cpus: stats::online_cpus(),
            ticks_per_sec: stats::ticks_per_sec(),
        },
        gpu,
        caps: CapsSection {
            output_cuda_memory: output_cuda,
            sink_buffer_memory: rec.sink_memory(),
            output_links: m.output_links,
            ingest: ingest_caps,
            consumer_links: m.consumer_links,
        },
        camera_controls: controls.into_values().collect(),
        totals: samples.totals,
        warnings: std::mem::take(&mut bus.warnings.kept),
        warnings_suppressed: bus.warnings.suppressed,
        teardown: TeardownSection {
            session_stop,
            consumer_pipeline_null: consumer_null,
            nvidia_fds_after_teardown: nv_fds,
            libcuda_mapped_after_teardown: cuda_mapped,
            note: "GStreamer destroys its CUDA context when the pipelines reach NULL; remaining /dev/nvidia* fds (if any) are the CUDA driver's process-wide handles from cuInit, released only at process exit",
            dgpu_runtime_status_after_teardown: runtime_status(),
            linger_s: args.linger_after_teardown,
        },
        criteria,
        effect,
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

/// The report's effect section, with `--effect` only.
fn effect_section(args: &Args, m: &Measured, sink_caps_events: u64) -> Option<EffectSection> {
    let kind = args.effect?;
    let end = m.effect_end.clone().unwrap_or_default();
    let start = m.effect_start.clone().unwrap_or_default();
    let (end_denoise, start_denoise) =
        (end.stage(&StageId::DENOISE), start.stage(&StageId::DENOISE));
    let (end_relight, start_relight) =
        (end.stage(&StageId::RELIGHT), start.stage(&StageId::RELIGHT));
    let (frames_processed, frames_black, frames_passthrough) = effect_frames(&start, &end);
    Some(EffectSection {
        kind: kind.to_string(),
        blur_strength: args.blur_strength,
        background: args.background.clone(),
        on_failure: args.effect_on_failure.to_string(),
        final_kind: end.effect.to_string(),
        status: end.status.as_str().to_string(),
        reason: end.status.reason().map(str::to_string),
        load_ms: end.last_load_ms,
        load_reused_model: end.last_load_reused,
        loads: end.loads,
        frames_processed,
        frames_black,
        frames_passthrough,
        gpu_ms_p50: end.gpu_ms_p50,
        gpu_ms_p95: end.gpu_ms_p95,
        denoise: args.denoise,
        denoise_status: end_denoise.status.as_str().to_string(),
        denoise_reason: end_denoise.status.reason().map(str::to_string),
        frames_denoised: end_denoise.frames.saturating_sub(start_denoise.frames),
        denoise_ms_p50: end_denoise.ms_p50,
        denoise_ms_p95: end_denoise.ms_p95,
        relight: args.relight.clone(),
        relight_pan_deg: args.relight_pan,
        relight_gain: args.relight_gain,
        relight_status: end_relight.status.as_str().to_string(),
        relight_reason: end_relight.status.reason().map(str::to_string),
        frames_relit: end_relight.frames.saturating_sub(start_relight.frames),
        relight_ms_p50: end_relight.ms_p50,
        relight_ms_p95: end_relight.ms_p95,
        sink_caps_events,
        switch_every_s: args.effect_switch_every,
        switches: m.effect_switches.clone(),
        note: EFFECT_NOTE,
    })
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
    let c = &r.config;
    println!(
        "== obc-bench output {} ({}, {}) ==",
        c.output, c.aspect, c.process
    );
    if c.streaming {
        println!(
            "streamed {:.1} s (measured after warm-up) until a stop signal",
            c.measured_s
        );
        if let Some(k) = &r.retention {
            println!(
                "per-frame figures cover: {} ({:.1} s from t+{:.1}s; samples kept for the last {:.0} s); whole run: {} camera frames, {} output frames",
                k.scope,
                k.retained_s,
                k.retained_from_s,
                k.retain_cap_s,
                k.whole_run_src_frames,
                k.whole_run_sink_frames
            );
        }
    } else if c.interrupted {
        println!(
            "INTERRUPTED by signal: figures cover {:.1} s of the requested {:.0} s",
            c.measured_s, c.duration_s
        );
    }
    let opt = |v: Option<i32>| v.map_or("unreadable".to_string(), |v| v.to_string());
    for cc in &r.camera_controls {
        let cam = cc.camera.display();
        match (
            &cc.exposure_dynamic_framerate_skipped,
            cc.pinned,
            cc.restored,
        ) {
            (Some(skip), _, _) => {
                println!("{cam} exposure_dynamic_framerate: skipped ({skip})")
            }
            (None, false, _) => println!(
                "{cam} exposure_dynamic_framerate: {} (unchanged)",
                opt(cc.exposure_dynamic_framerate_before)
            ),
            (None, true, Some(true)) => println!(
                "{cam} exposure_dynamic_framerate: {} while streaming, restored to {}",
                opt(cc.exposure_dynamic_framerate_during),
                opt(cc.exposure_dynamic_framerate_before)
            ),
            (None, true, _) => println!(
                "{cam} exposure_dynamic_framerate: RESTORE FAILED (now {}, was {})",
                opt(cc.exposure_dynamic_framerate_after_exit),
                opt(cc.exposure_dynamic_framerate_before)
            ),
        }
    }
    let grid_line = |name: &str, g: &GridFps| {
        println!(
            "  {name:<9} on the frame grid: {} of {} 1 s windows off the grid (min {} max {}); {} frames, {:.1} expected; exact {}",
            g.windows_off_grid,
            g.windows,
            g.min_1s.map_or("-".into(), |v| v.to_string()),
            g.max_1s.map_or("-".into(), |v| v.to_string()),
            g.frames,
            g.expected_frames,
            g.exact
        )
    };
    let timing_line = |name: &str, p: &PtsSequence, d: &Deviation, w: &Sliding| {
        println!(
            "  {name:<9} PTS sequence: {} frames, missing {} out of order {} off grid {} (exact {}); deviation from the grid {} spread {} ms (limit {:.2}, within {}); any 1 s window {}..={}",
            p.frames,
            p.missing,
            p.out_of_order,
            p.off_grid,
            p.exact,
            d.ms.map_or("n/a".into(), |s| format!(
                "min {:.2} p50 {:.2} max {:.2} ms",
                s.min, s.p50, s.max
            )),
            d.spread_ms.map_or("n/a".into(), |v| format!("{v:.2}")),
            d.limit_ms,
            d.within,
            w.min_1s.map_or("-".into(), |v| v.to_string()),
            w.max_1s.map_or("-".into(), |v| v.to_string()),
        )
    };
    println!("fps (contract {:.2}):", f.target);
    fps_line("output", &f.output);
    timing_line(
        "output",
        &f.output_pts,
        &f.output_deviation,
        &f.output_sliding_1s,
    );
    grid_line("output", &f.output_grid);
    if let Some(cf) = &f.consumer {
        fps_line("consumer", cf);
    }
    if let (Some(p), Some(d), Some(w)) = (
        &f.consumer_pts,
        &f.consumer_deviation,
        &f.consumer_sliding_1s,
    ) {
        timing_line("consumer", p, d, w);
    }
    if let Some(cg) = &f.consumer_grid {
        grid_line("consumer", cg);
    }
    for i in &f.inputs {
        println!(
            "  input #{} {} {} {}: {:.2} fps over {:.1} s, 1 s windows min {} max {}; camera-side drops {} (PTS gaps)",
            i.source_id,
            i.camera,
            i.mode,
            i.ingest,
            i.fps.overall,
            i.span_s,
            i.fps.min_1s.map_or("-".into(), |v| format!("{v:.0}")),
            i.fps.max_1s.map_or("-".into(), |v| format!("{v:.0}")),
            i.missing_frames_by_pts
        );
    }
    let o = &r.frames.output;
    println!(
        "frames: fresh {} repeat {} placeholder {} | input {} superseded {} | late {} skipped {} | output queue dropped {} | ingest lost {}, queue overruns {}, qos {:?}",
        o.fresh,
        o.repeat,
        o.placeholder,
        o.input_frames,
        o.superseded,
        o.late,
        o.skipped,
        o.queue_dropped,
        r.frames.ingest_lost_frames,
        r.frames.queue_overruns,
        r.frames.qos_dropped_by_element
    );
    if let Some(n) = r.frames.decoder_dropped_frames {
        println!("  jpegdec dropped {n} undecodable camera frames (whole run)");
    }
    let k = &r.clock;
    println!(
        "clock: phase lock {}; {} acquisitions, {} wraps, phase {:.2} ms at the end (locked {}); slot wait {}; queue level max {} of {}; holds {} ({} released, {} expired)",
        if k.phase_lock.is_some() { "on" } else { "off" },
        k.lock_acquisitions,
        k.lock_wraps,
        k.phase_offset_ms,
        k.phase_locked,
        k.slot_wait_ms.map_or("n/a".into(), |s| format!(
            "p50 {:.2} p95 {:.2} max {:.2} ms, {} over a camera frame{}",
            s.p50,
            s.p95,
            s.max,
            k.slot_wait_over_camera_frame
                .map_or("n/a".into(), |n| n.to_string()),
            lock_split_note(k.wait_split())
        )),
        k.queue_level_max,
        k.queue_bound,
        k.holds,
        k.holds_released,
        k.holds_expired
    );
    let sw = &r.switches;
    println!(
        "switches: {} ({} completed, {} failed, {} abandoned at the end); gap {}; held {}; repeats {} placeholders {} during gaps (at most {} per switch); first start {}",
        sw.count,
        sw.completed,
        sw.failed,
        sw.abandoned_at_end,
        sw.gap_ms.map_or("n/a".into(), |g| format!(
            "min {:.1} p50 {:.1} max {:.1} ms",
            g.min, g.p50, g.max
        )),
        sw.held_ms.map_or("n/a".into(), |g| format!(
            "min {:.1} p50 {:.1} max {:.1} ms",
            g.min, g.p50, g.max
        )),
        sw.repeats_during_gaps,
        sw.placeholders_during_gaps,
        sw.placeholders_max_per_switch
            .map_or("-".into(), |v| v.to_string()),
        sw.initial_start_ms
            .map_or("n/a".into(), |g| format!("{g:.1} ms"))
    );
    println!(
        "latency (output period {:.2} ms, budget {:.2} ms):",
        r.latency.output_period_ms, r.latency.budget_ms
    );
    println!(
        "  added (camera src pad -> sink ready, fresh): {}",
        fmt_sum(&r.latency.added_primary_ms, " ms")
    );
    println!(
        "  (camera src pad -> sink pad arrival): {}",
        fmt_sum(&r.latency.added_at_sink_pad_ms, " ms")
    );
    if c.consumer_stamp {
        println!(
            "  via consumer (camera src pad -> consumer): {}",
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
            "  gst latency query (output): live={} min {:.2} ms max {}",
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
    if let Some(e) = &r.effect {
        let ms = |v: Option<f64>| v.map_or("n/a".into(), |v| format!("{v:.3}"));
        println!(
            "effect: {} (strength {:.2}{}, on failure {}) -> {} {}{}; load {} ms{}, {} loads",
            e.kind,
            e.blur_strength,
            e.background
                .as_ref()
                .map(|b| format!(", background {}", b.display()))
                .unwrap_or_default(),
            e.on_failure,
            e.final_kind,
            e.status,
            e.reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default(),
            e.load_ms.map_or("n/a".into(), |v| format!("{v:.1}")),
            if e.load_reused_model {
                " (model reused)"
            } else {
                ""
            },
            e.loads
        );
        println!(
            "  frames processed {} black {} passthrough {}; GPU per frame p50 {} ms p95 {} ms; caps events at the sink {}",
            e.frames_processed,
            e.frames_black,
            e.frames_passthrough,
            ms(e.gpu_ms_p50),
            ms(e.gpu_ms_p95),
            e.sink_caps_events
        );
        if e.denoise {
            println!(
                "  denoise {}{}; frames denoised {}; stage GPU p50 {} ms p95 {} ms",
                e.denoise_status,
                e.denoise_reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                e.frames_denoised,
                ms(e.denoise_ms_p50),
                ms(e.denoise_ms_p95)
            );
        }
        if let Some(light) = &e.relight {
            println!(
                "  relight {light} (pan {:.0} deg, gain {:.2}) {}{}; frames relit {}; stage GPU p50 {} ms p95 {} ms",
                e.relight_pan_deg,
                e.relight_gain,
                e.relight_status,
                e.relight_reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                e.frames_relit,
                ms(e.relight_ms_p50),
                ms(e.relight_ms_p95)
            );
        }
        for w in &e.switches {
            println!(
                "  switch at t+{:.1}s -> {}: {}{}{}; processed {} black {} passthrough {}",
                w.at_s,
                w.to,
                match (&w.reason, w.ready_after_ms) {
                    (Some(r), _) => format!("FAILED ({r})"),
                    (None, Some(ms)) => format!("ready after {ms:.0} ms"),
                    (None, None) => "not ready before the next change".into(),
                },
                w.denoise_reason
                    .as_deref()
                    .map(|r| format!(", denoise dropped ({r})"))
                    .unwrap_or_default(),
                w.relight_reason
                    .as_deref()
                    .map(|r| format!(", relight dropped ({r})"))
                    .unwrap_or_default(),
                w.frames_processed,
                w.frames_black,
                w.frames_passthrough
            );
        }
    }
    println!(
        "caps: output CUDAMemory appsrc..download: {}; buffers at v4l2sink: {}",
        r.caps.output_cuda_memory,
        r.caps.sink_buffer_memory.as_deref().unwrap_or("?")
    );
    let links = |l: &[LinkCaps]| {
        for l in l {
            println!(
                "    {} -> {}: {}",
                l.from,
                l.to,
                l.caps.as_deref().unwrap_or("-")
            );
        }
    };
    println!("  output:");
    links(&r.caps.output_links);
    for i in &r.caps.ingest {
        println!(
            "  ingest {} {} ({}), CUDAMemory upload..slot: {}",
            i.camera, i.mode, i.ingest, i.cuda_memory_upload_to_slot
        );
        links(&i.links);
    }
    println!(
        "teardown: session {:?}, /dev/nvidia* fds after: {}, libcuda still mapped: {}",
        r.teardown.session_stop,
        r.teardown.nvidia_fds_after_teardown.len(),
        r.teardown.libcuda_mapped_after_teardown
    );
    for w in &r.warnings {
        println!("warning: {w}");
    }
    if r.warnings_suppressed > 0 {
        println!("warning: {} more warnings not kept", r.warnings_suppressed);
    }
    let k = &r.criteria;
    println!(
        "criteria: run_completed={} fps_sustained={} no_output_drops={} no_skipped_ticks={} switches_completed={} no_placeholders_outside_switches={} no_ingest_errors={} inputs_delivered={} no_ingest_drops={} latency_p95<=budget={} latency_p99<=budget={} cuda_links={}",
        k.run_completed,
        k.fps_sustained,
        k.no_output_drops,
        k.no_skipped_ticks,
        k.switches_completed,
        k.no_placeholders_outside_switches,
        k.no_ingest_errors,
        k.inputs_delivered,
        k.no_ingest_drops,
        k.added_latency_p95_within_budget,
        k.added_latency_p99_within_budget,
        k.cuda_memory_links
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_core::UsbId;
    use obc_pipeline::DiscoverySource;
    use obc_pipeline::capability::parse_v4l2_ctl;

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
    fn waits_over_a_camera_frame_are_counted() {
        let frame = 16_666_667;
        assert_eq!(
            count_over(&[1_500_000, frame, frame + 1, 21_000_000], frame),
            2
        );
        assert_eq!(count_over(&[], frame), 0);
    }

    #[test]
    fn milliseconds_are_validated() {
        assert_eq!(non_negative_ms("0"), Ok(0.0));
        assert_eq!(non_negative_ms("1.5"), Ok(1.5));
        for bad in ["inf", "NaN", "-1", "1001", "x"] {
            assert!(non_negative_ms(bad).is_err(), "{bad}");
        }
        let err = non_negative_ms("-1").unwrap_err();
        assert!(err.contains("milliseconds"), "{err}");
    }

    fn parse(extra: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("obc-bench").chain(extra.iter().copied()))
    }

    #[test]
    fn bad_arguments_are_rejected_by_clap() {
        for (flag, v) in [
            ("--gpu-baseline", "inf"),
            ("--duration", "-5"),
            ("--warmup", "NaN"),
            ("--linger-after-teardown", "1e300"),
            ("--switch-every", "0"),
            ("--switch-every", "-1"),
            ("--input-gap-ms", "0"),
            ("--output", "1921x1080@30"),
            ("--output", "1920x1080"),
            ("--output-range", "tv"),
            ("--aspect", "stretch"),
        ] {
            assert!(parse(&[flag, v]).is_err(), "{flag} {v}");
        }
        assert!(parse(&["--duration", "0.5"]).is_ok());
        // --camera-usb is folded into --camera.
        assert!(parse(&["--camera-usb", "046d:0944"]).is_err());
        assert!(parse(&["--camera", ""]).is_err());
        assert!(parse(&["--mode", "H264:1920x1080@30"]).is_err());
    }

    #[test]
    fn defaults() {
        let a = parse(&[]).unwrap();
        assert!(a.camera.is_empty(), "no default camera");
        assert_eq!(a.mode, ModeRequest::default());
        assert_eq!(a.duration, 30.0, "the benchmark default is unchanged");
        assert_eq!(a.contract(), OutputContract::default());
        assert_eq!(a.aspect, AspectPolicy::Crop);
        assert_eq!(a.input_gap_ms, 500);
        assert_eq!(a.switch_every, None);
        assert_eq!(a.gpu_stage, GpuStage::Passthrough);
        assert!(!a.consumer_stamp());
        assert_eq!(a.effect, None, "no effect element by default");
        assert!(a.check_effect().is_ok());
    }

    #[test]
    fn effect_arguments() {
        let a = parse(&["--effect", "blur", "--blur-strength", "0.8"]).unwrap();
        assert_eq!(a.effect, Some(EffectKind::Blur));
        let s = a.effect_settings(EffectKind::Blur);
        assert_eq!((s.blur_strength, s.fallback), (0.8, EffectFallback::Black));
        assert!(a.check_effect().is_ok());
        assert_eq!(a.effect_cycle(), [EffectKind::None, EffectKind::Blur]);
        for (flag, v) in [
            ("--effect", "sepia"),
            ("--blur-strength", "1.5"),
            ("--blur-strength", "-0.1"),
            ("--effect-on-failure", "raw"),
            ("--effect-switch-every", "0"),
        ] {
            assert!(parse(&[flag, v]).is_err(), "{flag} {v}");
        }
        // Replace needs a background; a relative one is refused.
        let r = parse(&["--effect", "replace"]).unwrap();
        assert!(
            r.check_effect()
                .unwrap_err()
                .to_string()
                .contains("background")
        );
        let r = parse(&["--effect", "replace", "--background", "bg.png"]).unwrap();
        assert!(
            r.check_effect()
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
        // --denoise combines with every effect, none included, and needs one.
        let d = parse(&["--effect", "none", "--denoise"]).unwrap();
        assert!(d.check_effect().is_ok());
        let s = d.effect_settings(EffectKind::None);
        assert!(s.is_enabled("denoise") && s.kind == EffectKind::None);
        assert!(
            d.effect_settings(EffectKind::Blur).is_enabled("denoise"),
            "kept by the cycle"
        );
        assert!(!a.effect_settings(EffectKind::Blur).is_enabled("denoise"));
        let bare = parse(&["--denoise"]).unwrap();
        assert!(
            bare.check_effect()
                .unwrap_err()
                .to_string()
                .contains("--effect none")
        );
        // --relight takes an optional light, combines with every effect and
        // denoise, needs one, and its direction and gain are checked.
        let r = parse(&["--effect", "none", "--relight"]).unwrap();
        assert!(r.check_effect().is_ok());
        let s = r.effect_settings(EffectKind::None);
        assert!(s.is_enabled("relight") && !s.is_enabled("denoise"));
        assert_eq!(
            s.stage("relight").unwrap().params.get("light"),
            Some(&Value::from("neutral"))
        );
        let r = parse(&[
            "--effect",
            "blur",
            "--denoise",
            "--relight=warm",
            "--relight-pan",
            "-90",
            "--relight-gain",
            "1.5",
        ])
        .unwrap();
        assert!(r.check_effect().is_ok());
        let s = r.effect_settings(EffectKind::Replace);
        assert!(
            s.is_enabled("relight") && s.is_enabled("denoise"),
            "kept by the cycle"
        );
        assert_eq!(
            s.stage("relight"),
            Some(
                &StageRequest::on(StageId::RELIGHT)
                    .with_param("light", "warm")
                    .with_live("direction", Value::Float(-90.0))
                    .with_live("brightness", Value::Float(150.0))
            )
        );
        assert_eq!(
            s.stages.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["denoise", "relight"],
            "noise removal first"
        );
        assert!(!a.effect_settings(EffectKind::Blur).is_enabled("relight"));
        assert!(parse(&["--effect", "blur", "--relight=pink"]).is_err());
        for (flag, v) in [("--relight-pan", "190"), ("--relight-gain", "3")] {
            let bad = parse(&["--effect", "blur", "--relight", flag, v]).unwrap();
            assert!(
                bad.check_effect()
                    .unwrap_err()
                    .to_string()
                    .contains("relight"),
                "{flag} {v}"
            );
        }
        let bare = parse(&["--relight=cool"]).unwrap();
        assert!(
            bare.check_effect()
                .unwrap_err()
                .to_string()
                .contains("--effect none")
        );
        // A cycle needs a starting effect; roundtrip does not combine.
        let c = parse(&["--effect-switch-every", "5"]).unwrap();
        assert!(
            c.check_effect()
                .unwrap_err()
                .to_string()
                .contains("--effect")
        );
        let rt = parse(&["--effect", "none", "--gpu-stage", "roundtrip"]).unwrap();
        assert!(
            rt.check_effect()
                .unwrap_err()
                .to_string()
                .contains("roundtrip")
        );
        let dir = tempfile::tempdir().unwrap();
        let bg = dir.path().join("bg.png");
        std::fs::write(&bg, b"x").unwrap();
        let ok = parse(&[
            "--effect",
            "none",
            "--background",
            bg.to_str().unwrap(),
            "--effect-switch-every",
            "4",
        ])
        .unwrap();
        assert!(ok.check_effect().is_ok());
        assert_eq!(
            ok.effect_cycle(),
            [EffectKind::None, EffectKind::Blur, EffectKind::Replace]
        );
    }

    #[test]
    fn output_contract_combines_size_rate_format_and_range() {
        let a = parse(&[
            "--output",
            "1280x720@30000/1001",
            "--output-format",
            "i420",
            "--output-range",
            "full",
            "--aspect",
            "letterbox",
            "--input-gap-ms",
            "250",
            "--switch-every",
            "2.5",
        ])
        .unwrap();
        let c = a.contract();
        assert_eq!((c.width, c.height), (1280, 720));
        assert_eq!((c.fps_num, c.fps_den), (30000, 1001));
        assert_eq!(c.format, OutputFormat::I420);
        assert_eq!(c.range, ColourRange::Full);
        assert_eq!(c.colorimetry(), "1:4:5:1");
        assert_eq!(a.aspect, AspectPolicy::Letterbox);
        assert_eq!(a.input_gap_ms, 250);
        assert_eq!(a.switch_every, Some(2.5));
    }

    #[test]
    fn camera_is_repeatable_and_takes_three_forms() {
        let a = parse(&[
            "--camera",
            "/dev/video4",
            "--camera",
            "5986:1193",
            "--camera",
            "MX Brio",
        ])
        .unwrap();
        assert!(matches!(a.camera[0], CameraSelector::Path(_)));
        assert!(matches!(a.camera[1], CameraSelector::Usb(_)));
        assert!(matches!(a.camera[2], CameraSelector::Name(_)));
    }

    #[test]
    fn consumer_is_allowed_while_streaming_without_the_stamp() {
        let a = parse(&["--duration", "0", "--consumer"]).unwrap();
        assert!(a.streaming() && a.consumer && !a.consumer_stamp());
        let bench = parse(&["--consumer"]).unwrap();
        assert!(!bench.streaming() && bench.consumer_stamp());
    }

    const BRIO_CTL: &str = include_str!("../../tests/data/mx-brio-046d-0944.v4l2-ctl.txt");
    const FHD_CTL: &str = include_str!("../../tests/data/fhd-camera-5986-1193-colour.v4l2-ctl.txt");
    const FHD_IR_CTL: &str = include_str!("../../tests/data/fhd-camera-5986-1193-ir.v4l2-ctl.txt");

    fn dev(path: &str, card: &str, usb: (u16, u16), fixture: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            path: PathBuf::from(path),
            card: card.to_string(),
            driver: Some(
                if fixture.is_some() {
                    "uvcvideo"
                } else {
                    "v4l2 loopback"
                }
                .into(),
            ),
            usb: Some(UsbId {
                vendor: usb.0,
                product: usb.1,
            }),
            caps: Some(0x0420_0001),
            capture: true,
            loopback: fixture.is_none(),
            source: DiscoverySource::Synthetic,
            capabilities: fixture.map(parse_v4l2_ctl),
        }
    }

    /// A laptop camera (colour and IR nodes), an external MX Brio and a
    /// loopback being fed (so it shows capture caps).
    fn devices() -> Vec<DeviceInfo> {
        vec![
            dev(
                "/dev/vidtest0",
                "FHD Camera",
                (0x5986, 0x1193),
                Some(FHD_CTL),
            ),
            dev(
                "/dev/vidtest2",
                "FHD Camera IR",
                (0x5986, 0x1193),
                Some(FHD_IR_CTL),
            ),
            dev("/dev/vidtest4", "MX Brio", (0x046d, 0x0944), Some(BRIO_CTL)),
            dev("/dev/vidtest11", "Omarchy Broadcast", (0, 0), None),
        ]
    }

    fn summary(cycle: &[CycleCamera]) -> Vec<(String, String)> {
        cycle
            .iter()
            .map(|c| (c.device.card.clone(), c.choice.mode.to_string()))
            .collect()
    }

    #[test]
    fn cycle_follows_repeated_cameras_with_a_mode_each() {
        let devices = devices();
        let req: ModeRequest = "1920x1080@60".parse().unwrap();
        let sel = |s: &str| s.parse::<CameraSelector>().unwrap();
        let cycle = camera_cycle(&devices, &[sel("046d:0944"), sel("FHD Camera")], &req);
        // "FHD Camera" matches the colour node only (the IR node is not colour).
        let cycle = cycle.unwrap();
        assert_eq!(
            summary(&cycle),
            vec![
                ("MX Brio".into(), "NV12:1920x1080@60".into()),
                ("FHD Camera".into(), "MJPEG:1920x1080@30".into()),
            ]
        );
        assert_eq!(cycle[0].choice.ingest, IngestPath::GpuDirect);
        assert_eq!(cycle[1].choice.ingest, IngestPath::CpuDecode);
        assert!(!cycle[0].auto && !cycle[1].auto);
    }

    #[test]
    fn single_or_no_camera_cycles_through_every_colour_camera() {
        let devices = devices();
        let req: ModeRequest = "1920x1080@60".parse().unwrap();
        // No selector: the exact match comes first, then the rest in order;
        // neither the IR node nor the loopback is a candidate.
        let auto = camera_cycle(&devices, &[], &req).unwrap();
        assert_eq!(
            summary(&auto),
            vec![
                ("MX Brio".into(), "NV12:1920x1080@60".into()),
                ("FHD Camera".into(), "MJPEG:1920x1080@30".into()),
            ]
        );
        assert!(auto[0].auto && auto[0].how.contains("exactly"));
        let one = camera_cycle(&devices, &["5986:1193".parse().unwrap()], &req).unwrap();
        assert_eq!(one[0].device.card, "FHD Camera");
        assert_eq!(one[1].device.card, "MX Brio");
        assert!(!one[0].auto && one[1].auto);
        // A selector matching nothing is an error, not a fallback.
        assert!(camera_cycle(&devices, &["1234:5678".parse().unwrap()], &req).is_err());
    }

    #[test]
    fn latency_budget_is_one_output_period_plus_one_camera_period() {
        // 30 fps contract fed at 60 fps: 33.3 + 16.7 ms.
        let b = latency_budget_ms(33_333_333, 16_666_666);
        assert!((b - 50.0).abs() < 0.001, "{b}");
        // 30 fps contract fed at 30 fps: two 33.3 ms periods.
        let b = latency_budget_ms(33_333_333, 33_333_333);
        assert!((b - 66.667).abs() < 0.001, "{b}");
        // No camera: the output period alone.
        let b = latency_budget_ms(33_333_333, 0);
        assert!((b - 33.333).abs() < 0.001, "{b}");
    }

    #[test]
    fn warnings_are_capped_and_counted() {
        let mut w = Warnings::default();
        for i in 0..WARNINGS_KEPT + 25 {
            w.push(format!("w{i}"));
        }
        assert_eq!(w.kept.len(), WARNINGS_KEPT);
        assert_eq!(w.kept[0], "w0");
        assert_eq!(w.suppressed, 25);
    }

    #[test]
    fn qos_drops_are_keyed_by_camera_across_ingest_runs() {
        gst::init().unwrap();
        let frame = 33_333_333u64;
        let mut c = BusCounters::default();
        let running = |n| gst::GenericFormattedValue::new(gst::Format::Buffers, n);
        // Fifty switches between two cameras: each run's decoder reports a
        // running total from 0, and its camera one lost frame.
        for run in 0..50 {
            let cam = if run % 2 == 0 {
                "A (/dev/video0)"
            } else {
                "B (/dev/video4)"
            };
            for total in [1, 2] {
                account_qos_drop(
                    c.qos_run.entry(format!("{cam}/decode")).or_default(),
                    running(total),
                    None,
                    frame,
                );
            }
            account_qos_drop(
                c.qos_run.entry(format!("{cam}/camera")).or_default(),
                gst::GenericFormattedValue::new(gst::Format::Undefined, -1),
                Some(gst::ClockTime::from_nseconds(frame)),
                frame,
            );
            c.end_ingest_run();
        }
        c.qos_run.insert("A (/dev/video0)/decode".into(), 3);
        let all = c.take_qos_dropped();
        assert_eq!(all.len(), 4, "{all:?}");
        // 25 runs per camera, 2 decoder drops each (the latest total).
        assert_eq!(all["A (/dev/video0)/decode"], 25 * 2 + 3);
        assert_eq!(all["B (/dev/video4)/decode"], 25 * 2);
        assert_eq!(all["A (/dev/video0)/camera"], 25);
        assert!(c.qos_run.is_empty() && c.qos_dropped.is_empty());
    }

    /// The miss count over a window that opens before the half-rate lock
    /// engages: the free-running frames before it are counted apart.
    #[test]
    fn slot_waits_over_a_frame_split_at_the_lock() {
        let frame = 66_666_667u64;
        let ms = 1_000_000u64;
        // 15 fps into 30: 17 frames before the lock, 8 after it. Before the
        // lock 15 of the waits are over a camera frame; after it one is.
        let mut waits = vec![70 * ms; 15];
        waits.extend([20 * ms, 30 * ms]);
        waits.extend([
            25 * ms,
            31 * ms,
            69 * ms,
            28 * ms,
            33 * ms,
            20 * ms,
            29 * ms,
            32 * ms,
        ]);
        let mut locked = vec![false; 17];
        locked.extend(vec![true; 8]);
        let s = split_over_by_lock(&waits, &locked, frame);
        assert_eq!(
            s,
            WaitSplit {
                locked_over: 1,
                locked_total: 8,
                unlocked_over: 15,
                unlocked_total: 17,
            }
        );
        assert_eq!(
            lock_split_note(Some(s)),
            " (1 of 8 after the lock engaged, 15 of 17 before)"
        );
        // The whole-window count is the sum, as before.
        assert_eq!(count_over(&waits, frame), s.locked_over + s.unlocked_over);
        // Exactly the limit is not over it, in either part.
        let edge = split_over_by_lock(&[frame, frame + 1], &[true, false], frame);
        assert_eq!((edge.locked_over, edge.unlocked_over), (0, 1));
    }

    /// A lock that never engages (20 fps into 30): every wait is in the
    /// unlocked part, which then equals the whole-window count, and the
    /// summary says so.
    #[test]
    fn slot_waits_without_a_lock_are_all_unlocked() {
        let frame = 50_000_000u64;
        let waits = [10_000_000, 60_000_000, 51_000_000, 40_000_000];
        let s = split_over_by_lock(&waits, &[false; 4], frame);
        assert_eq!((s.locked_over, s.locked_total), (0, 0));
        assert_eq!((s.unlocked_over, s.unlocked_total), (2, 4));
        assert_eq!(s.unlocked_over, count_over(&waits, frame));
        // No lock state recorded at all counts as unlocked too.
        assert_eq!(split_over_by_lock(&waits, &[], frame), s);
        assert_eq!(split_over_by_lock(&[], &[], frame).unlocked_total, 0);
        assert_eq!(lock_split_note(Some(s)), " (lock not engaged)");
        assert_eq!(lock_split_note(None), "");
    }

    #[test]
    fn output_counter_deltas_per_window() {
        let a = OutputCounts {
            input_frames: 100,
            fresh: 50,
            repeat: 2,
            placeholder: 0,
            superseded: 48,
            late: 1,
            skipped: 0,
            emitted: 52,
            queue_dropped: 1,
            push_failed: 0,
            queue_level_max: 2,
            lock_acquisitions: 1,
            lock_wraps: 0,
            phase_offset_ns: -5_000_000,
            phase_locked: true,
            holds: 1,
            holds_released: 1,
            holds_expired: 0,
        };
        let b = OutputCounts {
            input_frames: 400,
            fresh: 200,
            repeat: 5,
            placeholder: 10,
            superseded: 198,
            late: 1,
            skipped: 3,
            emitted: 215,
            queue_dropped: 4,
            push_failed: 1,
            queue_level_max: 3,
            lock_acquisitions: 3,
            lock_wraps: 2,
            phase_offset_ns: -20_000_000,
            phase_locked: false,
            holds: 4,
            holds_released: 3,
            holds_expired: 1,
        };
        let d = counts_delta(&a, &b);
        assert_eq!(
            d,
            OutputCounts {
                input_frames: 300,
                fresh: 150,
                repeat: 3,
                placeholder: 10,
                superseded: 150,
                late: 0,
                skipped: 3,
                emitted: 163,
                queue_dropped: 3,
                push_failed: 1,
                // The running maximum and the states are the later ones.
                queue_level_max: 3,
                lock_acquisitions: 2,
                lock_wraps: 2,
                phase_offset_ns: -20_000_000,
                phase_locked: false,
                holds: 3,
                holds_released: 2,
                holds_expired: 1,
            }
        );
        assert_eq!(d.fresh + d.repeat + d.placeholder, d.emitted);
        // Never negative, even for snapshots in the wrong order.
        assert_eq!(
            counts_delta(&b, &a),
            OutputCounts {
                queue_level_max: 2,
                phase_offset_ns: -5_000_000,
                phase_locked: true,
                ..OutputCounts::default()
            }
        );
    }

    /// Output fps per 1 s window counts arrivals on a fixed grid; a tick
    /// late into the next window shows up as 29 + 31, not as a drop overall.
    #[test]
    fn output_fps_windows() {
        let period = 33_333_333u64;
        let start = 5_000_000_000u64;
        let mut times: Vec<u64> = (0..90).map(|i| start + i * period + 1_000).collect();
        // Tick 29 (last of window 0) arrives 2 ms into window 1.
        times[29] = start + 1_000_000_000 + 2_000_000;
        let r = stats::fps_report(&times, start, start + 3_000_000_000, 30.0, 1.0);
        assert_eq!(r.windows, 3);
        assert_eq!((r.min_1s, r.max_1s), (Some(29.0), Some(31.0)));
        assert_eq!(r.windows_below_target, 0, "within the 1-frame tolerance");
        assert!((r.overall - 30.0).abs() < 1e-9);
    }

    /// A camera with the MX Brio's capabilities (NV12 1080p60 chosen), or
    /// with `fixture`'s.
    fn cam_with(card: &str, path: &str, fixture: &str) -> CycleCamera {
        let d = dev(path, card, (1, 1), Some(fixture));
        let choice =
            choose_mode(d.capabilities.as_ref().unwrap(), &ModeRequest::default()).unwrap();
        CycleCamera {
            device: d,
            choice,
            how: String::new(),
            auto: false,
        }
    }

    fn cam(card: &str, path: &str) -> CycleCamera {
        cam_with(card, path, BRIO_CTL)
    }

    fn record(
        from: Option<usize>,
        to: usize,
        id: u64,
        requested_ns: u64,
        gap: Option<(u64, u64, u64)>,
    ) -> SwitchRecord {
        SwitchRecord {
            from,
            to,
            trigger: "timer",
            requested_ns,
            source_id: Some(id),
            gap: gap.map(|(gap_ns, repeats, placeholders)| SwitchGap {
                target: id,
                requested_ns,
                first_fresh_ns: requested_ns + gap_ns,
                repeats,
                placeholders,
                previous_fresh: 0,
                held_from_ns: Some(requested_ns.saturating_sub(10_000_000)),
            }),
            error: None,
            abandoned: false,
        }
    }

    #[test]
    fn switch_gaps_exclude_the_first_start_and_count_failures() {
        let cycle = [cam("A", "/dev/a"), cam("B", "/dev/b")];
        let ms = 1_000_000u64;
        let mut start = record(None, 0, 1, 1_000 * ms, Some((900 * ms, 0, 27)));
        start.trigger = START_TRIGGER;
        let mut failed = record(Some(1), 0, 4, 40_000 * ms, None);
        failed.error = Some("busy".into());
        // After the failed switch no camera streams: the next switch starts
        // from none. It is a switch like any other, not the first start.
        let from_none = record(None, 1, 5, 45_000 * ms, Some((600 * ms, 0, 18)));
        // Requested, first frame not yet out when the window closed.
        let mut waiting = record(Some(1), 0, 6, 50_000 * ms, None);
        waiting.abandoned = true;
        let records = [
            start,
            record(Some(0), 1, 2, 10_000 * ms, Some((400 * ms, 12, 0))),
            record(Some(1), 0, 3, 20_000 * ms, Some((800 * ms, 15, 9))),
            failed,
            from_none,
            waiting,
        ];
        let s = summarize_switches(&records, &cycle, 5_000 * ms, 5, 1);
        assert_eq!(s.initial_start_ms, Some(900.0));
        assert_eq!(
            (s.count, s.completed, s.failed, s.abandoned_at_end),
            (5, 3, 1, 1)
        );
        let g = s.gap_ms.unwrap();
        assert_eq!((g.min, g.max, g.count), (400.0, 800.0, 3));
        assert_eq!(
            (s.repeats_during_gaps, s.placeholders_during_gaps),
            (27, 27)
        );
        assert_eq!(s.records.len(), 6);
        assert_eq!(s.records[0].requested_at_s, -4.0);
        assert_eq!(s.records[1].from.as_deref(), Some("A (/dev/a)"));
        assert_eq!(s.records[1].to, "B (/dev/b)");
        assert_eq!(s.records[3].error.as_deref(), Some("busy"));
        assert_eq!(s.records[4].from, None);
        assert!(s.records[5].abandoned && !s.records[4].abandoned);
        assert_eq!((s.whole_run_switches, s.abandoned), (5, 1));
        // The first start alone: no switch.
        let s = summarize_switches(&records[..1], &cycle, 0, 0, 0);
        assert_eq!((s.count, s.initial_start_ms), (0, Some(900.0)));
        // Without a start record (trimmed while streaming), a switch from
        // no camera is not taken for the first start.
        let s = summarize_switches(&records[4..5], &cycle, 0, 1, 0);
        assert_eq!((s.count, s.completed, s.initial_start_ms), (1, 1, None));
    }

    #[test]
    fn a_timer_switch_never_fires_at_or_after_the_window_end() {
        let end = 130_000_000_000u64;
        // Due at the end (duration a multiple of the period), woken just
        // after it: no switch.
        assert!(!timer_switch_due(end + 200_000, end, end));
        assert!(!timer_switch_due(end, end - 1_000, end));
        // Due before the end and woken before it: fires.
        assert!(timer_switch_due(end - 1, end - 1_000, end));
        assert!(timer_switch_due(end - 1_000, end - 1_000, end));
        // Not due yet.
        assert!(!timer_switch_due(end - 2_000, end - 1_000, end));
        // Streaming (open window): any due switch fires.
        assert!(timer_switch_due(5_000, 4_000, u64::MAX - 1));
    }

    #[test]
    fn a_run_is_complete_only_when_it_measured_the_requested_duration() {
        let s = 1_000_000_000u64;
        assert!(run_completed(false, false, 585 * s, 585.0));
        // Stopped by a signal after 15 s of 585 s.
        assert!(!run_completed(false, true, 15 * s + 340_000_000, 585.0));
        // Shorter than requested without the flag.
        assert!(!run_completed(false, false, 15 * s, 585.0));
        // Fractional durations.
        assert!(run_completed(false, false, secs_ns(2.5), 2.5));
        // Streaming ends on a signal by design.
        assert!(run_completed(true, true, 12 * s, 0.0));
    }

    /// Arrivals of a stream at `num/den`, `latency` after each grid point
    /// from `t0`, with a per-frame jitter.
    fn arrivals(
        t0: u64,
        frames: u64,
        num: u32,
        den: u32,
        latency: u64,
        jitter: impl Fn(u64) -> i64,
    ) -> Vec<u64> {
        let cfg = obc_pipeline::clock::PacerConfig::new(num, den);
        (0..frames)
            .map(|i| (t0 + cfg.deadline_ns(i) + latency).saturating_add_signed(jitter(i)))
            .collect()
    }

    #[test]
    fn line_residuals_measure_jitter_around_the_cadence() {
        let f = 16_666_667u64;
        let pts: Vec<u64> = (0..100).map(|i| 1_000 + i * f).collect();
        // Arrivals on a slightly faster clock, every 10th 2 ms late.
        let times: Vec<u64> = (0..100u64)
            .map(|i| 5_000_000 + i * (f - 1_000) + if i % 10 == 3 { 2_000_000 } else { 0 })
            .collect();
        let r = line_residuals_ms(&times, &pts, f);
        let late: Vec<f64> = (0..100).filter(|i| i % 10 == 3).map(|i| r[i]).collect();
        assert!(late.iter().all(|v| (v - 1.8).abs() < 0.05), "{late:?}");
        assert!(
            r.iter()
                .enumerate()
                .filter(|(i, _)| i % 10 != 3)
                .all(|(_, v)| (v + 0.2).abs() < 0.05)
        );
        // A dropped frame keeps the numbering from the PTS.
        let mut p2 = pts.clone();
        let mut t2 = times.clone();
        p2.remove(50);
        t2.remove(50);
        assert_eq!(line_residuals_ms(&t2, &p2, f).len(), 99);
        assert!(
            line_residuals_ms(&pts, &pts, f)
                .iter()
                .all(|v| v.abs() < 1e-3)
        );
        // A camera 0.1 % slower than nominal over 2000 frames (33 ms of
        // drift): still numbered frame by frame, no residual.
        let slow: Vec<u64> = (0..2000u64).map(|i| i * (f + f / 1000)).collect();
        assert!(
            line_residuals_ms(&slow, &slow, f)
                .iter()
                .all(|v| v.abs() < 1e-3)
        );
    }

    #[test]
    fn pts_sequence_requires_every_tick_once_in_order() {
        let d = |i: u64| PacerConfig::new(30, 1).deadline_ns(i);
        let all: Vec<u64> = (100..400).map(d).collect();
        let s = pts_sequence(&all, 30, 1);
        assert!(s.exact, "{s:?}");
        assert_eq!(
            (s.first_index, s.last_index, s.frames),
            (Some(100), Some(399), 300)
        );
        // A missing tick, a repeated one, one without PTS, one off the grid.
        let mut v = all.clone();
        v.remove(10);
        let s = pts_sequence(&v, 30, 1);
        assert_eq!((s.missing, s.exact), (1, false));
        let mut v = all.clone();
        v.insert(20, d(119));
        assert_eq!(pts_sequence(&v, 30, 1).out_of_order, 1);
        let mut v = all.clone();
        v[5] = u64::MAX;
        v[6] += 1;
        let s = pts_sequence(&v, 30, 1);
        assert_eq!((s.off_grid, s.missing, s.exact), (2, 2, false));
        // N/D rates: the exact grid.
        let ntsc: Vec<u64> = (0..100)
            .map(|i| PacerConfig::new(30000, 1001).deadline_ns(i))
            .collect();
        assert!(pts_sequence(&ntsc, 30000, 1001).exact);
        assert!(!pts_sequence(&[], 30, 1).exact);
    }

    #[test]
    fn deviation_spread_is_judged_against_one_period_plus_the_allowance() {
        let period = 33_333_333;
        // A phase that moved across the whole band: within.
        let d = deviation(&[-33_000_000, -1_000_000, 0, 2_000_000], period);
        assert!(d.within, "{d:?}");
        assert!((d.spread_ms.unwrap() - 35.0).abs() < 1e-9);
        assert!((d.limit_ms - 38.333_333).abs() < 1e-3);
        // Beyond: a frame 10 ms late on top of a full band.
        assert!(!deviation(&[-33_000_000, 0, 10_000_000], period).within);
        assert!(!deviation(&[], period).within);
    }

    #[test]
    fn sliding_windows_find_the_extremes_anywhere() {
        let s = 1_000_000_000u64;
        let p = 33_333_333u64;
        // Exactly 30 fps (on the exact grid): 30 in every window, wherever
        // it starts.
        let grid = PacerConfig::new(30, 1);
        let times: Vec<u64> = (0..300).map(|i| s + grid.deadline_ns(i)).collect();
        let w = sliding_1s(&times, s, 11 * s);
        assert_eq!((w.min_1s, w.max_1s), (Some(30), Some(30)));
        // Two frames moved 30 ms earlier and later around one point: a window
        // can catch one more or one less, never two.
        let mut v = times.clone();
        v[100] -= 30_000_000;
        v[131] += 30_000_000;
        let w = sliding_1s(&v, s, 11 * s);
        assert_eq!((w.min_1s, w.max_1s), (Some(29), Some(31)));
        // A doubled interval (one tick late by a period, the next early):
        // still 29..=31.
        let mut v = times.clone();
        v[150] += p - 1;
        let w = sliding_1s(&v, s, 11 * s);
        assert!(w.min_1s >= Some(29) && w.max_1s <= Some(31), "{w:?}");
        assert!(count_within(300, 300.0) && count_within(301, 300.0));
        assert!(!count_within(302, 300.0));
    }

    #[test]
    fn grid_windows_hold_exactly_the_contract_rate() {
        let s = 1_000_000_000u64;
        let from = 7 * s + 123_456_789;
        let to = from + 30 * s;
        // 30 fps for 31 s, arriving 3 ms after the grid with +-5 ms jitter.
        let times = arrivals(from - s / 2, 31 * 30, 30, 1, 3_000_000, |i| {
            [5_000_000, -5_000_000, 1_000_000][i as usize % 3]
        });
        let g = grid_fps(&times, from, to, 30, 1);
        assert_eq!((g.windows, g.windows_off_grid), (29, 0), "{g:?}");
        assert_eq!((g.min_1s, g.max_1s), (Some(30), Some(30)));
        assert!(g.exact, "{g:?}");
        assert!((g.frames as f64 - 900.0).abs() <= 1.0, "{g:?}");

        // Arrivals right on the period boundary (phase near 0 and near a
        // full period): the circular mean keeps them together.
        let times = arrivals(from, 30 * 30, 30, 1, 0, |i| {
            if i % 2 == 0 { 300_000 } else { -300_000 }
        });
        assert!(grid_fps(&times, from, to, 30, 1).exact);

        // One frame lost: one window off the grid.
        let mut lost = arrivals(from - s / 2, 31 * 30, 30, 1, 3_000_000, |_| 0);
        lost.remove(400);
        let g = grid_fps(&lost, from, to, 30, 1);
        assert_eq!(g.windows_off_grid, 1, "{g:?}");
        assert!(!g.exact);

        // The last frame of a window late by more than half a period:
        // 29 + 31. Arrivals sit at phase 3 ms on the grid from `from`, so
        // the boundaries sit at 19.7 ms; the first window ends at
        // from + 1019.7 ms, after frame 45 (from + 1003 ms).
        let mut late = arrivals(from - s / 2, 31 * 30, 30, 1, 3_000_000, |_| 0);
        late[45] += 20_000_000;
        let g = grid_fps(&late, from, to, 30, 1);
        assert_eq!((g.min_1s, g.max_1s), (Some(29), Some(31)), "{g:?}");
        assert!(!g.exact);

        // 99 % of the rate (one frame in 100 missing) fails.
        let ninety_nine: Vec<u64> = arrivals(from - s / 2, 31 * 30, 30, 1, 3_000_000, |_| 0)
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % 100 != 50)
            .map(|(_, t)| t)
            .collect();
        assert!(!grid_fps(&ninety_nine, from, to, 30, 1).exact);

        // No frame at all.
        let g = grid_fps(&[], from, to, 30, 1);
        assert_eq!((g.windows, g.exact), (0, false));
    }

    #[test]
    fn grid_windows_allow_floor_and_ceiling_for_fractional_rates() {
        let s = 1_000_000_000u64;
        let from = 3 * s;
        let to = from + 60 * s;
        let times = arrivals(from - s, 62 * 30, 30000, 1001, 2_000_000, |i| {
            [4_000_000, -4_000_000, 0, 2_500_000][i as usize % 4]
        });
        let g = grid_fps(&times, from, to, 30000, 1001);
        assert!(g.exact, "{g:?}");
        assert_eq!((g.min_1s, g.max_1s), (Some(29), Some(30)), "{g:?}");
        assert!(g.windows >= 58, "{g:?}");
        // A lost frame is still caught.
        let mut lost = times.clone();
        lost.remove(900);
        assert!(!grid_fps(&lost, from, to, 30000, 1001).exact);
    }

    #[test]
    fn budget_and_delivery_count_only_cameras_that_streamed() {
        // A: NV12 1080p60 (16.7 ms); B: MJPEG 1080p30 (33.3 ms).
        let cycle = [cam("A", "/dev/a"), cam_with("B", "/dev/b", FHD_CTL)];
        let s = 1_000_000_000u64;
        let (from, to) = (10 * s, 20 * s);
        let mut samples = Samples::default();
        for i in 0..600u64 {
            samples.src_times.push(from + i * s / 60);
            samples.src_source.push(1);
            samples.src_pts.push(i);
        }
        let a = SourceRun {
            source_id: 1,
            camera: 0,
            started_ns: 5 * s,
            stopped_ns: None,
        };
        // B is in the cycle but never streamed: the budget uses A alone.
        assert_eq!(
            streamed_camera_period_ns(std::slice::from_ref(&a), &cycle, &samples, from, to),
            cycle[0].choice.mode.frame_duration_ns()
        );
        assert!(inputs_delivered(
            std::slice::from_ref(&a),
            &samples,
            from,
            to,
            None
        ));
        // B started inside the window and delivered nothing: a dead input.
        let mut a_until = a.clone();
        a_until.stopped_ns = Some(15 * s);
        let b = SourceRun {
            source_id: 2,
            camera: 1,
            started_ns: 15 * s,
            stopped_ns: None,
        };
        let runs = [a_until, b];
        assert_eq!(
            streamed_camera_period_ns(&runs, &cycle, &samples, from, to),
            cycle[0].choice.mode.frame_duration_ns()
        );
        assert!(!inputs_delivered(&runs, &samples, from, to, None));
        // ... unless its switch was still waiting when the window closed.
        assert!(inputs_delivered(&runs, &samples, from, to, Some(2)));
        // Once B delivers, its slower period sets the budget.
        samples.src_times.push(16 * s);
        samples.src_source.push(2);
        samples.src_pts.push(0);
        assert_eq!(
            streamed_camera_period_ns(&runs, &cycle, &samples, from, to),
            cycle[1].choice.mode.frame_duration_ns()
        );
        assert!(inputs_delivered(&runs, &samples, from, to, None));
        // A run that ended before the window does not count.
        let old = SourceRun {
            source_id: 9,
            camera: 1,
            started_ns: 0,
            stopped_ns: Some(from),
        };
        assert!(inputs_delivered(
            &[old.clone(), runs[0].clone()],
            &samples,
            from,
            to,
            None
        ));
        // No run in the window at all: nothing was delivered.
        assert!(!inputs_delivered(&[old], &samples, from, to, None));
    }

    #[test]
    fn input_fps_per_run_covers_its_own_span() {
        // A: NV12 1080p60; B: MJPEG 1080p30.
        let cycle = [cam("A", "/dev/a"), cam_with("B", "/dev/b", FHD_CTL)];
        let s = 1_000_000_000u64;
        // Source 1 at 60 fps for 2 s, then source 2 at 30 fps for 2 s.
        let mut samples = Samples::default();
        for i in 0..120u64 {
            samples.src_times.push(i * s / 60);
            samples.src_source.push(1);
            samples.src_pts.push(i * s / 60);
        }
        for i in 0..60u64 {
            samples.src_times.push(2 * s + 300_000_000 + i * s / 30);
            samples.src_source.push(2);
            samples.src_pts.push(i * s / 30);
        }
        let runs = [
            SourceRun {
                source_id: 1,
                camera: 0,
                started_ns: 0,
                stopped_ns: Some(2 * s),
            },
            SourceRun {
                source_id: 2,
                camera: 1,
                started_ns: 2 * s,
                stopped_ns: None,
            },
        ];
        let r = input_reports(&samples, &runs, &cycle, 0, 4 * s + 300_000_000);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].fps.frames, 120);
        assert!((r[0].fps.overall - 60.0).abs() < 1e-9);
        assert_eq!(r[0].fps.windows, 2);
        // Run 2 is measured from its first frame, not from the switch.
        assert!((r[1].span_s - 2.0).abs() < 1e-9);
        assert_eq!(r[1].fps.frames, 60);
        assert_eq!(r[1].missing_frames_by_pts, 0);
    }

    #[test]
    fn status_line_reports_window_rates_and_counts() {
        let c = cam("MX Brio", "/dev/video4");
        let w = Samples {
            src_times: vec![0; 300],
            sink_times: vec![0; 150],
            consumer_times: vec![0; 150],
            added_latency_ns: vec![10_000_000, 20_000_000, 30_000_000],
            ..Samples::default()
        };
        let delta = OutputCounts {
            fresh: 140,
            repeat: 6,
            placeholder: 4,
            superseded: 160,
            ..OutputCounts::default()
        };
        let line = status_line(Some(&c), &w, 5.0, &delta, true, 12.34);
        assert!(
            line.starts_with("MX Brio /dev/video4 | NV12:1920x1080@60 gpu-direct"),
            "{line}"
        );
        for part in [
            "in 60.0 fps",
            "out 30.0 fps",
            "consumer 30.0 fps",
            "fresh 140 repeat 6 placeholder 4 superseded 160",
            "added p50 20.00 ms",
            "cpu 12.3%",
        ] {
            assert!(line.contains(part), "{part}: {line}");
        }
        assert!(status_line(None, &w, 5.0, &delta, false, 0.0).starts_with("no camera |"));
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
}
