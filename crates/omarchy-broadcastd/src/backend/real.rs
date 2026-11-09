//! The media backend of the running daemon: the persistent loopback writer
//! and, while active, a CUDA [`Session`] whose output forwards to it.
//!
//! Before the first session nothing here touches CUDA or loads the
//! `nvcodec` plugin: the writer is `appsrc ! v4l2sink` on the CPU, and the
//! camera scan reads sysfs and queries nodes without streaming. NVML (for
//! `gpu_percent`) is initialised only while a session runs and shut down
//! with it.
//!
//! Every output is built with the effect element (`obceffect`), in
//! passthrough for `none`, so the effect can change while it streams. Its
//! models (the effect's and the noise removal's) are loaded only while they
//! are requested and the session runs; stopping the session unloads them
//! before the CUDA context goes
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`, "Lifecycle").
//!
//! **Parking.** Once a session has run, CUDA stays initialised in the
//! process and the GPU cannot power down, even with every context gone.
//! Stopping a session therefore parks the GPU ([`Parker`],
//! `super::park`) once the whole session is gone: the NVML sampler
//! stopped, every pipeline at NULL and dropped (the effect element has
//! joined its worker thread and unloaded its models by then), so nothing
//! in the process uses CUDA. A session that did not stop cleanly (a
//! pipeline left short of NULL, by its stop or by one of its camera
//! switches) may still have CUDA elements alive: the GPU is then not
//! parked, and parking stays off until the daemon restarts (the parker is
//! tainted). A stop that failed otherwise (a camera control not restored,
//! an output clock thread that had panicked) is warned, and parks. The frames the writer and the PipeWire tap
//! may still hold come out of `cudadownload` in system memory, so releasing
//! them later needs no CUDA. Starting a session unparks first, before the
//! output or NVML touch the GPU; if the GPU cannot be restored the start
//! fails (nothing may touch CUDA), and a start that fails after unparking
//! parks again, unless the output it had started did not stop cleanly
//! (tainted, as above). While parked, nothing here touches CUDA: the camera scan,
//! the writer and [`MediaBackend::effect_available`] (the effect backend's
//! probe opens its libraries and makes no CUDA call) need none. The
//! process may exit parked (a shutdown stops the session first, which
//! parks): its exit does not wait on the GPU. Dropping the backend with a
//! session still running does not park it.
//!
//! The optional PipeWire output (`[pipewire] enabled`) is started and
//! stopped on its own next to the running writer
//! ([`MediaBackend::start_pipewire`], [`MediaBackend::stop_pipewire`]),
//! without touching the writer; stopping the writer stops it too. Every
//! output started gets a new generation, and its events reach the
//! [`PipeWireReporter`] given to [`GstBackend::new`] tagged with it. Built
//! with the cargo feature `pipewire`, it is an
//! [`obc_pipeline::pipewire::PipeWireOutput`] fed by the writer's frame
//! tap, with the writer's contract; if it cannot even be started it
//! reports [`PipeWireState::Failed`] and the loopback runs alone. Built
//! without the feature (the default), asked for it, the backend reports
//! [`PipeWireState::Unsupported`] and the loopback runs alone.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gst::prelude::*;
use obc_core::config::EffectConfig;
use obc_core::{EffectMode, EffectOnFailure, ModeRequest, OutputContract, SinkSelector};
use obc_ipc::Stats;
use obc_pipeline::gpu::GpuSampler;
use obc_pipeline::pipeline::names;
use obc_pipeline::stats::{cpu_usage, online_cpus, self_cpu_ticks, ticks_per_sec};
use obc_pipeline::{
    DiscoveryRoute, EffectFallback, EffectKind, EffectSettings, IngestConfig, LiveStats,
    LoopbackWriter, Output, OutputConfig, OutputSink, PipelineError, Session, StopError,
    WriterCounts, WriterHandle, WriterSink, choose_mode, effect_backend_available, list_cameras,
    scan_sysfs, select_sink,
};

use super::park::{Parker, StartError, StopFailure, StopSession};
use super::{
    EffectReport, EffectStatus, Failure, MediaBackend, PipeWireEvent, PipeWireGeneration,
    PipeWireReporter, PipeWireState, SessionParams,
};
use crate::camera::{Camera, stable_id, usb_origin};
use crate::consumers::{NodeId, count_holders};

/// Environment variable that makes the daemon stamp every frame it
/// forwards to the writer with `obc_pipeline::stamp` (the frame's PTS in
/// its first 16 luma bytes), so the end-to-end test can tell camera frames
/// from placeholders whatever the picture. A test hook: it alters the
/// picture; never set it in normal use.
pub const STAMP_ENV: &str = "OBC_TEST_STAMP_FORWARDED";

/// NVML device sampled for `gpu_percent` (the device the CUDA elements use
/// by default).
const GPU_INDEX: u32 = 0;
/// NVML sampling period while a session runs.
const GPU_INTERVAL: Duration = Duration::from_millis(500);
/// The sampler keeps every sample; it is restarted after this many so its
/// memory stays bounded over a long call.
const GPU_SAMPLES_MAX: usize = 1200;

struct Running {
    session: Session,
    live: Arc<LiveStats>,
    /// The current ingest's source id.
    source: Option<u64>,
    gpu: Option<GpuSampler>,
    gpu_seen: usize,
    window_start: Instant,
    cpu_ticks: Option<u64>,
    /// The writer's counters at the start of the stats window.
    counts: WriterCounts,
    /// Why a pipeline a camera switch took down did not reach NULL (the
    /// first one): the session's stop is then unclean, however it goes.
    unclean: Option<String>,
}

impl StopSession for Running {
    /// The NVML sampler stopped, then ingest and output to NULL and
    /// dropped, then the rest: when this returns, nothing of the session is
    /// left. Unclean ([`StopFailure::reached_null`] false) when a pipeline
    /// did not reach NULL, now or in a camera switch ([`session_stopped`]).
    fn stop(self) -> Result<(), StopFailure> {
        let Running {
            session,
            gpu,
            live,
            unclean,
            ..
        } = self;
        if let Some(gpu) = gpu {
            let (_, res) = gpu.stop();
            if let Err(e) = res {
                crate::debug!("NVML shutdown: {e}");
            }
        }
        // Ingest, then output: every pipeline at NULL and dropped.
        let res = session.stop_checked();
        drop(live);
        session_stopped(res, unclean)
    }
}

/// A session stop's outcome for the parker: `res`, the stop of its
/// pipelines, and `unclean`, why one an earlier camera switch took down did
/// not reach NULL. Unclean if either left a pipeline outside NULL; an
/// error with every pipeline in NULL (a camera control not restored, an
/// output clock thread that had panicked and was joined) is not.
fn session_stopped(res: Result<(), StopError>, unclean: Option<String>) -> Result<(), StopFailure> {
    match (res, unclean) {
        (Ok(()), None) => Ok(()),
        (Err(e), None) => Err(StopFailure {
            error: e.to_string(),
            reached_null: e.reached_null,
        }),
        (Ok(()), Some(why)) => Err(StopFailure::unclean(why)),
        (Err(e), Some(why)) => Err(StopFailure::unclean(format!("{e}; earlier, {why}"))),
    }
}

/// Why a camera switch of the session left a pipeline outside NULL
/// ([`obc_pipeline::SwitchTiming::unclean_stop`]), for [`Running::unclean`].
fn switch_left(camera: &str, unclean_stop: Option<String>) -> Option<String> {
    unclean_stop.map(|why| format!("switching to {camera}: {why}"))
}

/// A failed output start's error (`error`), and how stopping the output
/// again went (`cleanup`): unclean only if that left it outside NULL. A
/// cleanup error with the pipeline in NULL is not expected (no clock
/// thread runs after a failed start); it is logged at debug.
fn start_failed(error: String, cleanup: Result<(), StopError>) -> StartError {
    match cleanup {
        Ok(()) => StartError::from(error),
        Err(e) if e.reached_null => {
            crate::debug!("stopping the output pipeline after its failed start: {e}");
            StartError::from(error)
        }
        Err(e) => StartError::unclean(error, format!("output pipeline: {e}")),
    }
}

/// The media backend over GStreamer (see the module docs).
pub struct GstBackend {
    writer: Option<LoopbackWriter>,
    running: Option<Running>,
    stamp: bool,
    /// Where the PipeWire output's events go (`None`: nowhere).
    reporter: Option<PipeWireReporter>,
    /// The generation of the last PipeWire output started (0: none yet).
    generation: PipeWireGeneration,
    /// Parks the GPU between sessions (see the module docs).
    parker: Parker,
    /// The PipeWire output fed by the running writer's tap.
    #[cfg(feature = "pipewire")]
    output: Option<obc_pipeline::pipewire::PipeWireOutput>,
}

impl GstBackend {
    /// Initialises GStreamer (no plugin is loaded by that). The PipeWire
    /// output's events go to `reporter`.
    pub fn new(reporter: Option<PipeWireReporter>) -> Result<Self, String> {
        gst::init().map_err(|e| format!("GStreamer initialisation failed: {e}"))?;
        Ok(GstBackend {
            writer: None,
            running: None,
            stamp: std::env::var_os(STAMP_ENV).is_some_and(|v| !v.is_empty() && v != "0"),
            reporter,
            generation: 0,
            parker: Parker::new(),
            #[cfg(feature = "pipewire")]
            output: None,
        })
    }

    /// The GPU parker: a measurement hook (turn parking off, read the last
    /// park and unpark times). The backend parks and unparks by itself.
    pub fn parker(&mut self) -> &mut Parker {
        &mut self.parker
    }

    fn report(&self, generation: PipeWireGeneration, event: PipeWireEvent) {
        if let Some(r) = &self.reporter {
            r(generation, event);
        }
    }

    /// Starts the PipeWire output of `generation` next to the running
    /// writer, with the writer's contract, and installs its tap on the
    /// writer. Never fails: the outcome is reported (the controller logs
    /// it), every event tagged with `generation`.
    #[cfg(feature = "pipewire")]
    fn start_pipewire_output(&mut self, generation: PipeWireGeneration, offer_nv12: bool) {
        use obc_pipeline::pipewire::{PipeWireOutput, PwEvent, PwOutputConfig};

        let Some(writer) = self.writer.as_ref() else {
            self.report(
                generation,
                PipeWireEvent::State(PipeWireState::Failed(
                    "no loopback writer is running".into(),
                )),
            );
            self.report(generation, PipeWireEvent::Consumers(0));
            return;
        };
        let reporter = self.reporter.clone();
        let report = move |event: PwEvent| {
            if let Some(r) = &reporter {
                r(generation, pipewire_event(event));
            }
        };
        let cfg = PwOutputConfig {
            offer_nv12,
            ..PwOutputConfig::new(*writer.contract())
        };
        match PipeWireOutput::start(cfg, report) {
            Ok(output) => {
                writer.set_tap(Some(output.tap()));
                self.output = Some(output);
            }
            Err(e) => {
                self.report(
                    generation,
                    PipeWireEvent::State(PipeWireState::Failed(format!(
                        "cannot start the PipeWire output: {e}"
                    ))),
                );
                self.report(generation, PipeWireEvent::Consumers(0));
            }
        }
    }

    /// Without the feature `pipewire`: reports
    /// [`PipeWireState::Unsupported`] and no link, and touches nothing.
    #[cfg(not(feature = "pipewire"))]
    fn start_pipewire_output(&mut self, generation: PipeWireGeneration, _offer_nv12: bool) {
        crate::warning!(
            "[pipewire] enabled is set, but this build has no PipeWire support: \
             only the loopback device is offered"
        );
        self.report(generation, PipeWireEvent::State(PipeWireState::Unsupported));
        self.report(generation, PipeWireEvent::Consumers(0));
    }

    fn writer_counts(&self) -> WriterCounts {
        self.writer
            .as_ref()
            .map_or_else(WriterCounts::default, LoopbackWriter::counts)
    }

    fn ingest_config(params: &SessionParams, camera: &Camera) -> IngestConfig {
        let mut cfg = IngestConfig::new(camera.path.clone(), camera.mode);
        cfg.aspect = params.aspect;
        cfg.fixed_framerate = params.fixed_framerate;
        cfg
    }

    fn start_ingest(
        running: &mut Running,
        params: &SessionParams,
        camera: &Camera,
    ) -> Result<(), String> {
        let cfg = Self::ingest_config(params, camera);
        let ingest = running
            .session
            .build_ingest(&cfg)
            .map_err(|e| format!("{}: {e}", camera.label()))?;
        running
            .live
            .attach(&ingest)
            .map_err(|e| format!("{}: {e}", camera.label()))?;
        running.source = None;
        let res = running.session.switch_to(ingest);
        if res.is_err() && running.unclean.is_none() {
            // A pipeline it took down left outside NULL: that session's stop
            // will not park (`Running::unclean`).
            running.unclean = switch_left(
                &camera.label(),
                running.session.last_switch().and_then(|t| t.unclean_stop),
            );
        }
        let timing = res.map_err(|e| format!("{}: {e}", camera.label()))?;
        running.source = Some(timing.source_id);
        Ok(())
    }
}

/// The effect element's settings for the daemon's effect settings.
fn effect_settings(e: &EffectConfig) -> EffectSettings {
    EffectSettings {
        kind: match e.mode {
            EffectMode::None => EffectKind::None,
            EffectMode::Blur => EffectKind::Blur,
            EffectMode::Replace => EffectKind::Replace,
        },
        // Validated to 0..=1 before it gets here.
        blur_strength: e.blur_strength as f32,
        background: e.background_image.clone(),
        fallback: match e.on_failure {
            EffectOnFailure::Black => EffectFallback::Black,
            EffectOnFailure::None => EffectFallback::None,
        },
        denoise: e.denoise,
    }
}

/// The reason of an effect error, without the `effect: ` prefix of
/// [`PipelineError::Effect`].
fn effect_error(e: PipelineError) -> String {
    match e {
        PipelineError::Effect(m) => m,
        other => other.to_string(),
    }
}

/// The frames that reached the loopback between two snapshots of the
/// writer's counters: those `appsrc` accepted (forwarded or placeholder),
/// less those it dropped from its full queue.
fn delivered_between(from: &WriterCounts, to: &WriterCounts) -> u64 {
    let accepted =
        (to.forwarded + to.placeholder).saturating_sub(from.forwarded + from.placeholder);
    let dropped = to.queue_dropped.saturating_sub(from.queue_dropped);
    accepted.saturating_sub(dropped)
}

/// Stamps every buffer reaching the output's forwarding `appsink`
/// ([`STAMP_ENV`]).
fn stamp_forwarded(output: &Output) {
    let Some(pad) = output
        .pipeline()
        .element(names::LOOPBACK)
        .and_then(|e| e.static_pad("sink"))
    else {
        crate::warning!(
            "{STAMP_ENV}: the output has no {} sink pad",
            names::LOOPBACK
        );
        return;
    };
    pad.add_probe(gst::PadProbeType::BUFFER, |_, info| {
        if let Some(buffer) = info.buffer_mut() {
            let pts = buffer.pts().map_or(0, |p| p.nseconds());
            if let Ok(mut map) = buffer.make_mut().map_writable() {
                obc_pipeline::stamp::write_stamp(map.as_mut_slice(), pts);
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// Builds and starts a session's output forwarding to `writer`, and the
/// NVML sampler: everything of [`MediaBackend::start_session`] that touches
/// the GPU. The writer's counters are the caller's to set.
///
/// On error, what was built is gone, and the error says whether it went
/// cleanly ([`StartError::unclean`]: the parker must then not park). An
/// output that failed to build was never started: its elements were only
/// made and linked, in NULL, and are dropped with it. One that failed to
/// start is stopped here, its result seen, rather than by its drop, which
/// ignores it: unclean only if it did not reach NULL ([`start_failed`]).
fn build_session(
    params: &SessionParams,
    writer: WriterHandle,
    stamp: bool,
) -> Result<Running, StartError> {
    let mut ocfg = OutputConfig::new(params.contract, OutputSink::Writer(writer));
    ocfg.gap_threshold = params.input_gap;
    ocfg.hold_timeout = params.hold_timeout;
    // Always with the element: an output built without it cannot take an
    // effect later.
    ocfg.effect = Some(effect_settings(&params.effect));
    let mut output = Output::build(ocfg).map_err(|e| format!("output pipeline: {e}"))?;
    let live = LiveStats::new();
    output.set_observer(Some(live.observer()));
    if stamp {
        stamp_forwarded(&output);
    }
    if let Err(e) = output.start() {
        return Err(start_failed(
            format!("output pipeline: {e}"),
            output.stop_checked(),
        ));
    }
    // Started already: `Session::new` has nothing left to fail on. Should it
    // fail all the same, the output went with it unchecked.
    let session = Session::new(output).map_err(|e| {
        StartError::unclean(
            format!("output pipeline: {e}"),
            "the output pipeline was dropped without its stop being checked",
        )
    })?;
    let epoch = Instant::now();
    let gpu = match GpuSampler::start(
        GPU_INDEX,
        GPU_INTERVAL,
        Arc::new(move || u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)),
    ) {
        Ok(s) => Some(s),
        Err(e) => {
            crate::debug!("GPU statistics unavailable: {e}");
            None
        }
    };
    Ok(Running {
        session,
        live,
        source: None,
        gpu,
        gpu_seen: 0,
        window_start: Instant::now(),
        cpu_ticks: self_cpu_ticks(),
        counts: WriterCounts::default(),
        unclean: None,
    })
}

impl MediaBackend for GstBackend {
    fn scan_cameras(&mut self, request: &ModeRequest) -> Vec<Camera> {
        // The sysfs route: no DeviceMonitor (it would talk to PipeWire),
        // loopback nodes are never opened.
        list_cameras(DiscoveryRoute::Sysfs, Some(request))
            .devices
            .into_iter()
            .filter(|d| d.is_colour_camera())
            .filter_map(|d| {
                let choice = d
                    .capabilities
                    .as_ref()
                    .and_then(|c| choose_mode(c, request))?;
                let (serial, usb_path) = usb_origin(&d.path);
                // The display name is set from the card name once the
                // whole list is known (`camera::display_names`).
                Some(Camera {
                    id: stable_id(&d.path),
                    name: d.card.trim().to_string(),
                    card: d.card.clone(),
                    usb: d.usb,
                    serial,
                    usb_path,
                    path: d.path.clone(),
                    mode: choice.mode,
                })
            })
            .collect()
    }

    fn start_writer(
        &mut self,
        sink: &SinkSelector,
        contract: &OutputContract,
    ) -> Result<PathBuf, String> {
        self.stop_writer();
        let devices = scan_sysfs();
        let device = select_sink(&devices, sink).map_err(|e| e.to_string())?;
        let path = device.path.clone();
        let mut writer = LoopbackWriter::build(*contract, WriterSink::Loopback(path.clone()))
            .map_err(|e| format!("loopback {}: {e}", path.display()))?;
        writer
            .start()
            .map_err(|e| format!("loopback {}: {e}", path.display()))?;
        self.writer = Some(writer);
        Ok(path)
    }

    fn stop_writer(&mut self) {
        self.stop_session();
        self.stop_pipewire();
        if let Some(mut w) = self.writer.take()
            && let Err(e) = w.stop()
        {
            crate::warning!("stopping the loopback writer: {e}");
        }
    }

    fn start_pipewire(&mut self, offer_nv12: bool) -> PipeWireGeneration {
        self.stop_pipewire();
        self.generation += 1;
        let generation = self.generation;
        self.start_pipewire_output(generation, offer_nv12);
        generation
    }

    /// The writer's tap goes first (no frame reaches the output after
    /// that), then the output (its thread is joined and the node
    /// disappears). Events it still sends carry its generation, which the
    /// controller no longer accepts.
    fn stop_pipewire(&mut self) {
        #[cfg(feature = "pipewire")]
        if let Some(output) = self.output.take() {
            if let Some(w) = &self.writer {
                w.set_tap(None);
            }
            output.stop();
        }
    }

    fn writer_failure(&mut self) -> Option<String> {
        let w = self.writer.as_ref()?;
        w.health()
            .err()
            .map(|e| format!("loopback writer failed: {e}"))
    }

    fn loopback_holders(&mut self, node: &Path) -> u32 {
        // The daemon's own descriptors (the writer) are not counted.
        count_holders(Path::new("/proc"), &NodeId::of(node), std::process::id())
    }

    fn start_session(&mut self, params: &SessionParams) -> Result<(), String> {
        let Some(writer) = self.writer.as_ref().map(LoopbackWriter::handle) else {
            self.stop_session();
            return Err("no loopback writer is running".into());
        };
        let stamp = self.stamp;
        // A running session stopped without parking in between; the GPU
        // unparked before the output (CUDA) or NVML touch it, and parked
        // again if the build fails (`Parker::start_session`).
        self.parker
            .start_session(&mut self.running, || build_session(params, writer, stamp))?;
        let counts = self.writer_counts();
        if let Some(r) = self.running.as_mut() {
            r.counts = counts;
        }
        Ok(())
    }

    fn switch_camera(&mut self, params: &SessionParams, camera: &Camera) -> Result<(), String> {
        let running = self
            .running
            .as_mut()
            .ok_or_else(|| "no session is running".to_string())?;
        Self::start_ingest(running, params, camera)
    }

    fn stop_session(&mut self) {
        // All of the session stopped first; parked only if it stopped
        // cleanly (`Parker::stop_session`).
        self.parker.stop_session(&mut self.running);
    }

    fn first_frame(&mut self) -> bool {
        self.running.as_ref().is_some_and(|r| {
            r.source
                .is_some_and(|id| r.session.output().first_fresh(id).is_some())
        })
    }

    fn session_failure(&mut self) -> Option<Failure> {
        let r = self.running.as_ref()?;
        if let Err(e) = r.session.output().health() {
            return Some(Failure::Output(format!("output pipeline failed: {e}")));
        }
        r.session.ingest_failure().map(Failure::Camera)
    }

    fn take_stats(&mut self) -> Stats {
        let counts = self.writer_counts();
        let Some(r) = self.running.as_mut() else {
            return Stats::default();
        };
        let now = Instant::now();
        let secs = now.saturating_duration_since(r.window_start).as_secs_f64();
        r.window_start = now;
        let fps = if secs > 0.0 {
            delivered_between(&r.counts, &counts) as f64 / secs
        } else {
            Stats::NOT_MEASURABLE
        };
        r.counts = counts;
        let window = r.live.take_window();
        let latency = window.latency_p50_ms.unwrap_or(Stats::NOT_MEASURABLE);
        // What the latency depends on (ADR-0010): the camera's own rate and
        // whether the output clock is phase-locked to it. Debug only (`-v`).
        let out = r.session.output().counts();
        crate::debug!(
            "pacing: camera {} fresh fps, output clock {} (offset {:.2} ms)",
            window
                .fresh_fps()
                .map_or_else(|| "n/a".to_string(), |f| format!("{f:.2}")),
            if out.phase_locked {
                "phase locked"
            } else {
                "not phase locked"
            },
            out.phase_offset_ns as f64 / 1e6
        );
        let ticks = self_cpu_ticks();
        let cpu = match (r.cpu_ticks, ticks) {
            (Some(a), Some(b)) if secs > 0.0 => {
                cpu_usage(b.saturating_sub(a), ticks_per_sec(), secs, online_cpus()).one_core_pct
            }
            _ => Stats::NOT_MEASURABLE,
        };
        r.cpu_ticks = ticks;
        let gpu = match &r.gpu {
            Some(s) => {
                let samples = s.samples();
                let new = samples.get(r.gpu_seen..).unwrap_or(&[]);
                r.gpu_seen = samples.len();
                if new.is_empty() {
                    Stats::NOT_MEASURABLE
                } else {
                    new.iter().map(|s| f64::from(s.util_gpu_pct)).sum::<f64>() / new.len() as f64
                }
            }
            None => Stats::NOT_MEASURABLE,
        };
        if r.gpu_seen >= GPU_SAMPLES_MAX
            && let Some(old) = r.gpu.take()
        {
            let _ = old.stop();
            let epoch = Instant::now();
            r.gpu = GpuSampler::start(
                GPU_INDEX,
                GPU_INTERVAL,
                Arc::new(move || u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)),
            )
            .ok();
            r.gpu_seen = 0;
        }
        Stats {
            fps,
            latency_p50_ms: latency,
            cpu_percent: cpu,
            gpu_percent: gpu,
        }
    }

    fn effect_available(&mut self, effect: &EffectConfig) -> Result<(), String> {
        if effect.mode == EffectMode::None {
            return Ok(());
        }
        // Called before a session starts, so while the GPU is parked: the
        // probe only opens the SDK libraries and checks the model directory,
        // with no CUDA call (a CUDA call would block until unparked).
        effect_backend_available()?;
        obc_pipeline::effect::check_settings(&effect_settings(effect)).map_err(effect_error)
    }

    fn set_effect(&mut self, effect: &EffectConfig) -> Result<(), String> {
        let Some(r) = self.running.as_ref() else {
            return Ok(());
        };
        r.session
            .output()
            .set_effect(&effect_settings(effect))
            .map_err(effect_error)
    }

    fn effect_report(&mut self) -> Option<EffectReport> {
        let output = self.running.as_ref()?.session.output();
        let status = |s| match s {
            obc_pipeline::EffectStatus::Off => EffectStatus::Off,
            obc_pipeline::EffectStatus::Loading => EffectStatus::Loading,
            obc_pipeline::EffectStatus::Ready => EffectStatus::Ready,
            obc_pipeline::EffectStatus::Failed(r) => EffectStatus::Failed(r),
        };
        let stats = output.effect_stats().unwrap_or_default();
        Some(EffectReport {
            status: status(output.effect_status()),
            denoise: status(output.denoise_status()),
            load_ms: stats.last_load_ms,
            reused: stats.last_load_reused,
            gpu_ms_p50: stats.gpu_ms_p50,
        })
    }
}

/// The PipeWire output's event as the controller's.
#[cfg(feature = "pipewire")]
fn pipewire_event(event: obc_pipeline::pipewire::PwEvent) -> PipeWireEvent {
    use obc_pipeline::pipewire::{PwEvent, PwState};

    match event {
        PwEvent::Consumers(n) => PipeWireEvent::Consumers(n),
        PwEvent::State(state) => PipeWireEvent::State(match state {
            PwState::Connecting => PipeWireState::Connecting,
            PwState::Ready => PipeWireState::Ready,
            PwState::Streaming => PipeWireState::Streaming,
            PwState::Failed(reason) => PipeWireState::Failed(reason),
        }),
    }
}

impl Drop for GstBackend {
    /// Stops everything without parking a session still running: the
    /// process is exiting, which releases the GPU anyway.
    fn drop(&mut self) {
        self.parker.set_enabled(false);
        self.stop_writer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(forwarded: u64, placeholder: u64, queue_dropped: u64) -> WriterCounts {
        WriterCounts {
            forwarded,
            placeholder,
            queue_dropped,
            ..WriterCounts::default()
        }
    }

    fn stop_err(what: &str, reached_null: bool) -> Result<(), StopError> {
        Err(StopError {
            error: PipelineError::State(what.into()),
            reached_null,
        })
    }

    #[test]
    fn only_a_pipeline_outside_null_makes_a_session_stop_unclean() {
        let not_null = "state change failed: ended in Paused, not Null";
        assert_eq!(session_stopped(Ok(()), None), Ok(()));
        // Every pipeline in NULL: a failure, not an unclean one (parks).
        assert_eq!(
            session_stopped(
                Err(StopError {
                    error: PipelineError::Control {
                        device: "/dev/video0".into(),
                        message: "restoring exposure_dynamic_framerate: No such device".into(),
                    },
                    reached_null: true,
                }),
                None
            ),
            Err(StopFailure::at_null(
                "camera control on /dev/video0: restoring exposure_dynamic_framerate: No such \
                 device"
            ))
        );
        assert_eq!(
            session_stopped(
                stop_err("the output clock thread panicked: boom", true),
                None
            ),
            Err(StopFailure::at_null(
                "state change failed: the output clock thread panicked: boom"
            ))
        );
        assert_eq!(
            session_stopped(stop_err("ended in Paused, not Null", false), None),
            Err(StopFailure::unclean(not_null))
        );
        // An earlier switch left a pipeline outside NULL: unclean, however
        // this stop went.
        let switch = "switching to Cam (a): stopping the previous ingest: ended in Ready";
        assert_eq!(
            session_stopped(Ok(()), Some(switch.into())),
            Err(StopFailure::unclean(switch))
        );
        assert_eq!(
            session_stopped(stop_err("boom", true), Some(switch.into())),
            Err(StopFailure::unclean(format!(
                "state change failed: boom; earlier, {switch}"
            )))
        );
        assert_eq!(
            session_stopped(
                stop_err("ended in Paused, not Null", false),
                Some(switch.into())
            ),
            Err(StopFailure::unclean(format!(
                "{not_null}; earlier, {switch}"
            )))
        );
    }

    #[test]
    fn a_camera_switch_is_unclean_only_when_it_left_a_pipeline_outside_null() {
        assert_eq!(switch_left("Cam (a)", None), None);
        assert_eq!(
            switch_left("Cam (a)", Some("stopping the previous ingest: x".into())).as_deref(),
            Some("switching to Cam (a): stopping the previous ingest: x")
        );
    }

    #[test]
    fn a_failed_output_start_is_unclean_only_when_its_stop_left_it_outside_null() {
        let e = || "output pipeline: state change failed".to_string();
        assert_eq!(start_failed(e(), Ok(())), StartError::from(e()));
        assert_eq!(
            start_failed(e(), stop_err("the output clock thread panicked", true)),
            StartError::from(e())
        );
        assert_eq!(
            start_failed(e(), stop_err("ended in Paused, not Null", false)),
            StartError::unclean(
                e(),
                "output pipeline: state change failed: ended in Paused, not Null"
            )
        );
    }

    #[test]
    fn delivered_frames_exclude_those_dropped_from_the_queue() {
        // 30 accepted in the window (25 forwarded, 5 placeholders), 4 of
        // them dropped by the full queue: 26 reached the loopback.
        assert_eq!(
            delivered_between(&counts(100, 10, 7), &counts(125, 15, 11)),
            26
        );
        // Nothing dropped: every accepted frame.
        assert_eq!(delivered_between(&counts(0, 0, 0), &counts(30, 0, 0)), 30);
        // Drops counted before the window are not subtracted again.
        assert_eq!(
            delivered_between(&counts(50, 0, 20), &counts(80, 0, 20)),
            30
        );
        // The drop counter read a frame the accepted counters had not yet
        // (two reads, not one snapshot): never below zero.
        assert_eq!(delivered_between(&counts(10, 0, 0), &counts(10, 0, 1)), 0);
    }

    #[cfg(feature = "pipewire")]
    #[test]
    fn the_pipewire_outputs_events_map_one_to_one() {
        use obc_pipeline::pipewire::{PwEvent, PwState};

        for (from, to) in [
            (PwEvent::Consumers(0), PipeWireEvent::Consumers(0)),
            (PwEvent::Consumers(3), PipeWireEvent::Consumers(3)),
            (
                PwEvent::State(PwState::Connecting),
                PipeWireEvent::State(PipeWireState::Connecting),
            ),
            (
                PwEvent::State(PwState::Ready),
                PipeWireEvent::State(PipeWireState::Ready),
            ),
            (
                PwEvent::State(PwState::Streaming),
                PipeWireEvent::State(PipeWireState::Streaming),
            ),
            (
                PwEvent::State(PwState::Failed("no daemon".into())),
                PipeWireEvent::State(PipeWireState::Failed("no daemon".into())),
            ),
        ] {
            assert_eq!(pipewire_event(from), to);
        }
    }
}
