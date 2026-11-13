//! The media backend of the running daemon: the persistent loopback writer
//! and, while active, a [`Session`] whose output forwards to it, in the
//! memory the controller chose for it ([`SessionParams::memory`]).
//!
//! **System memory.** A session in [`VideoMemory::System`] makes no CUDA
//! call: its output and ingests have no CUDA element (the `nvcodec`
//! plugin is not loaded for it), NVML is not started (`gpu_percent` is not
//! measurable), the GPU is neither unparked nor parked, and the compute
//! capability is not read. What follows of CUDA applies to a session in
//! CUDA memory.
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
//! an output clock thread that had panicked) is warned, and parks. The
//! frames the writer and the PipeWire tap may still hold come out of
//! `cudadownload` in system memory, so releasing them later needs no CUDA.
//! Starting a session unparks first, before the output or NVML touch the
//! GPU; if the GPU cannot be restored the start fails (nothing may touch
//! CUDA), and a start that fails after unparking parks again, unless the
//! output it had started did not stop cleanly (tainted, as above). While
//! parked, nothing here touches CUDA: the camera scan, the writer and
//! [`MediaBackend::effect_available`] (the effect backend's probe opens its
//! libraries and makes no CUDA call) need none. The process may exit parked
//! (a shutdown stops the session first, which parks): its exit does not
//! wait on the GPU. Dropping the backend with a session still running does
//! not park it.
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
//!
//! The live preview ([`MediaBackend::set_preview`]) is an
//! [`obc_pipeline::PreviewEncoder`] fed by the same tap: the writer has one
//! tap slot, so the backend installs one composed tap that hands every
//! frame to the PipeWire output and to the encoder, whichever of them
//! runs, and installs it anew whenever one of them starts or stops (the
//! other keeps its frames). The encoder runs while a preview is asked for
//! and a writer runs; a new writer (a new contract) gets a new encoder.
//! Its frames' sequence numbers come from one counter for the backend's
//! life, so they keep increasing across encoders. It makes no CUDA call.
//!
//! The writer's placeholder ([`MediaBackend::set_placeholder`]): the image
//! asked for is kept here and given to every writer started, which decodes
//! it for its own contract on a thread of its own
//! ([`LoopbackWriter::set_placeholder_image`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use gst::prelude::*;
use obc_core::config::VideoMemory;
use obc_core::{EffectMode, EffectOnFailure, ModeRequest, OutputContract, SinkSelector};
use obc_ipc::Stats;
use obc_module_host::{
    LogLevel, LogSink, ModuleBackendFactory, ModuleChainFactory, ModuleEnhancerFactory,
};
use obc_pipeline::control_node::CaptureFormat;
use obc_pipeline::controls::CtrlInfo;
use obc_pipeline::effect::{Availability, StageId, StageRequest};
use obc_pipeline::gpu::GpuSampler;
use obc_pipeline::ingest::EnhanceBackend;
use obc_pipeline::module::{ModulePlacement, ModuleStageSpec};
use obc_pipeline::output::NO_CUDA_CONTEXT;
use obc_pipeline::pipeline::names;
use obc_pipeline::stats::{cpu_usage, online_cpus, self_cpu_ticks, ticks_per_sec};
use obc_pipeline::xu::{SYSFS_ROOT, XuControl, XuRow, find_xu_controls};
use obc_pipeline::{
    ConsumerCover, CtrlNotice, CtrlWatcher, DiscoveryRoute, EffectFallback, EffectKind,
    EffectSettings, FrameTap, IngestConfig, LiveStats, LoopbackWriter, Output, OutputConfig,
    OutputSink, PipelineError, PlaceholderState, PreviewConfig, PreviewEncoder, Session, StopError,
    WriterCounts, WriterHandle, WriterSink, choose_mode, effect_backend_available_with,
    list_cameras, scan_sysfs, select_sink,
};

use super::park::{Parker, StartError, StopFailure, StopSession};
use super::{
    Backends, ControlEvent, ControlEventReporter, ControlNode, ControlScan, EffectReport,
    EffectRun, EffectStatus, Failure, MediaBackend, Module, ModuleEntry, OpenedModule,
    PipeWireEvent, PipeWireGeneration, PipeWireReporter, PipeWireState, Placement, PreviewReporter,
    PreviewRequest, SessionError, SessionParams, StageRun, WatchGeneration, WriterError,
};
use crate::camera::{Camera, stable_id, usb_origin};
use crate::consumers::{NodeId, count_holders, holder_names};
use crate::settings::XuReading;

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
    /// The ingests' enhancer, chosen when the session started (`None`: no
    /// enhance stage).
    enhance: Option<EnhanceBackend>,
    /// The effect element's backend, chosen when the session started
    /// (`None`: no effect module), handed back to the element after a
    /// revocation ([`MediaBackend::restore_effect_backend`]).
    effect_backend: Option<obc_pipeline::EffectBackend>,
    /// The chains of the output, by element name.
    chains: Vec<String>,
    /// Where the session keeps its frames.
    memory: VideoMemory,
}

impl StopSession for Running {
    fn uses_cuda(&self) -> bool {
        self.memory == VideoMemory::Cuda
    }

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
        // Ingest, then output: every pipeline at NULL and dropped. The
        // mapping to a clean or unclean stop is unit-tested
        // (`session_stopped`); this wiring only on hardware.
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
    /// The PipeWire output's part of the writer's tap.
    pipewire_tap: Option<FrameTap>,
    /// The preview asked for (`None`: none).
    preview: Option<PreviewRequest>,
    /// The preview encoder fed by the running writer's tap.
    encoder: Option<PreviewEncoder>,
    /// Where the preview frames' sequence numbers go.
    preview_reporter: Option<PreviewReporter>,
    /// The last preview sequence number used, shared by every encoder.
    preview_seq: Arc<AtomicU32>,
    /// Where the control events go (`None`: no watcher can start).
    control_reporter: Option<ControlEventReporter>,
    /// The control-event watcher of the open camera.
    watcher: Option<CtrlWatcher>,
    /// The generation of the last watcher started (0: none yet).
    watch_generation: WatchGeneration,
    /// The image the writer's placeholder shows (`None`: black).
    placeholder: Option<PathBuf>,
}

/// The writer's one tap: the PipeWire output's, the preview encoder's, or
/// one calling both, in that order (`None`: neither runs).
fn compose_tap(pipewire: Option<&FrameTap>, preview: Option<&FrameTap>) -> Option<FrameTap> {
    match (pipewire, preview) {
        (None, None) => None,
        (Some(t), None) | (None, Some(t)) => Some(Arc::clone(t)),
        (Some(a), Some(b)) => {
            let (a, b) = (Arc::clone(a), Arc::clone(b));
            Some(Arc::new(move |buffer: &gst::Buffer| {
                a(buffer);
                b(buffer);
            }))
        }
    }
}

impl GstBackend {
    /// Initialises GStreamer (no plugin is loaded by that). The PipeWire
    /// output's events go to `reporter`, the preview frames' sequence
    /// numbers to `preview_reporter`.
    pub fn new(
        reporter: Option<PipeWireReporter>,
        preview_reporter: Option<PreviewReporter>,
    ) -> Result<Self, String> {
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
            pipewire_tap: None,
            preview: None,
            encoder: None,
            preview_reporter,
            preview_seq: Arc::default(),
            control_reporter: None,
            watcher: None,
            watch_generation: 0,
            placeholder: None,
        })
    }

    /// The backend with the control events of its watchers
    /// ([`MediaBackend::watch_controls`]) going to `reporter`.
    pub fn with_control_reporter(mut self, reporter: ControlEventReporter) -> Self {
        self.control_reporter = Some(reporter);
        self
    }

    /// Installs the composed tap of what runs on the writer (see the module
    /// docs). Once it returns, the previous tap is no longer called.
    fn install_tap(&self) {
        if let Some(w) = &self.writer {
            let preview = self.encoder.as_ref().map(PreviewEncoder::tap);
            w.set_tap(compose_tap(self.pipewire_tap.as_ref(), preview.as_ref()));
        }
    }

    /// Starts the preview encoder on the running writer, with its contract,
    /// if a preview is asked for and none runs. A failure is logged: no
    /// frame is written.
    fn start_encoder(&mut self) {
        let (Some(req), Some(writer)) = (self.preview.as_ref(), self.writer.as_ref()) else {
            return;
        };
        if self.encoder.is_some() {
            return;
        }
        let cfg = PreviewConfig {
            seq: Arc::clone(&self.preview_seq),
            ..PreviewConfig::new(*writer.contract(), req.dir.clone(), req.fps)
        };
        let reporter = self.preview_reporter.clone();
        match PreviewEncoder::start(cfg, move |seq| {
            if let Some(r) = &reporter {
                r(seq);
            }
        }) {
            Ok(encoder) => {
                self.encoder = Some(encoder);
                self.install_tap();
            }
            Err(e) => crate::warning!("the live preview cannot start: {e}"),
        }
    }

    /// Stops the preview encoder, if one runs: its tap goes first (no frame
    /// reaches it after that), then its thread; its frame files are
    /// removed. The PipeWire output keeps its tap.
    fn stop_encoder(&mut self) {
        if let Some(encoder) = self.encoder.take() {
            self.install_tap();
            let c = encoder.counts();
            crate::debug!(
                "live preview stopped: {} frames written ({} decimated, {} superseded, {} \
                 failed), {} ms per frame (p50)",
                c.written,
                c.decimated,
                c.superseded,
                c.failed,
                encoder
                    .frame_ms_p50()
                    .map_or_else(|| "n/a".to_string(), |ms| format!("{ms:.2}"))
            );
            encoder.stop();
        }
    }

    /// Builds and starts the writer on `sink` (none may run), then the
    /// preview encoder on its tap if a preview is asked for: a new writer
    /// gets a new encoder. The writer's crash cover is logged once it is in
    /// place, or why not ([`log_consumer_cover`]), from the cover's own
    /// thread: the start does not wait for it.
    fn run_writer(
        &mut self,
        contract: OutputContract,
        sink: WriterSink,
    ) -> Result<(), PipelineError> {
        let mut writer = LoopbackWriter::build(contract, sink)?;
        writer.on_consumer_cover(Some(Arc::new(log_consumer_cover)));
        writer.set_placeholder_image(self.placeholder.as_deref());
        writer.start()?;
        self.writer = Some(writer);
        self.start_encoder();
        Ok(())
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
                self.pipewire_tap = Some(output.tap());
                self.output = Some(output);
                self.install_tap();
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

    fn ingest_config(
        params: &SessionParams,
        camera: &Camera,
        enhance: Option<&EnhanceBackend>,
    ) -> IngestConfig {
        let mut cfg = IngestConfig::new(camera.path.clone(), camera.mode);
        cfg.aspect = params.aspect;
        cfg.fixed_framerate = params.fixed_framerate;
        // Requested only where it applies (the controller's choice), with
        // the enhancer the session started with; never in system memory
        // (the stage takes CUDA frames).
        cfg.enhance =
            params.effect.enhance && enhance.is_some() && params.memory == VideoMemory::Cuda;
        cfg.enhance_backend = enhance.cloned();
        cfg.enhance_params = params.effect.enhance_params.clone();
        cfg
    }

    fn start_ingest(
        running: &mut Running,
        params: &SessionParams,
        camera: &Camera,
    ) -> Result<(), String> {
        let cfg = Self::ingest_config(params, camera, running.enhance.as_ref());
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
        // The mapping (`switch_left`) is unit-tested; this wiring only by
        // the hardware tests.
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

/// A stage's request for the elements.
fn stage_request(s: &StageRun) -> StageRequest {
    StageRequest {
        enabled: s.enabled,
        params: s.params.clone(),
        live: s.live.clone(),
        ..StageRequest::new(s.id.clone())
    }
}

/// The effect element's settings for what the session runs: the
/// background effect with its stage's parameters, then its stages in stage
/// order, each listed whether on or off, with its parameters (the module's
/// keys and units).
fn effect_settings(e: &EffectRun) -> EffectSettings {
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
            EffectOnFailure::Image => EffectFallback::Image,
        },
        fallback_image: e.fallback_image.clone(),
        stages: e.stages.iter().map(stage_request).collect(),
        background_params: e.background_params.clone(),
    }
}

/// The module stage elements of a session: one per chain of `e` whose
/// module `backends` has loaded.
fn module_stages(e: &EffectRun, backends: &Backends) -> Vec<ModuleStageSpec> {
    e.chains
        .iter()
        .filter_map(|c| {
            let (_, choice) = backends.chains.iter().find(|(n, _)| *n == c.name)?;
            let module = choice.module.clone()?;
            let ids: Vec<StageId> = c.stages.iter().map(|s| s.id.clone()).collect();
            let factory = match ModuleChainFactory::new(module, &ids) {
                Ok(f) => f,
                Err(e) => {
                    crate::warning!("module {}: {e}", c.module);
                    return None;
                }
            };
            Some(ModuleStageSpec {
                name: c.name.clone(),
                factory: Arc::new(factory),
                stages: c.stages.iter().map(stage_request).collect(),
                placement: match c.placement {
                    Placement::BeforeEffect => ModulePlacement::BeforeEffect,
                    Placement::AfterEffect => ModulePlacement::AfterEffect,
                },
            })
        })
        .collect()
}

/// The effect backend of `backends`, if its module is loaded.
fn effect_backend(backends: &Backends) -> Option<obc_pipeline::EffectBackend> {
    let module = backends.effect.as_ref()?.module.clone()?;
    Some(obc_pipeline::EffectBackend::new(Arc::new(
        ModuleBackendFactory::new(module),
    )))
}

/// The enhancer of `backends`, if its module is loaded.
fn enhance_backend(backends: &Backends) -> Option<EnhanceBackend> {
    let module = backends.enhance.as_ref()?.module.clone()?;
    Some(EnhanceBackend::new(Arc::new(ModuleEnhancerFactory::new(
        module,
    ))))
}

/// Where a module's log lines go: the daemon's log, with the module id.
fn module_log() -> LogSink {
    Arc::new(|id, level, msg| match level {
        LogLevel::Error => crate::error!("module {id}: {msg}"),
        LogLevel::Warn => crate::warning!("module {id}: {msg}"),
        LogLevel::Info => crate::info!("module {id}: {msg}"),
        LogLevel::Debug | LogLevel::Trace => crate::debug!("module {id}: {msg}"),
    })
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

/// Logs a writer start's crash cover outcome
/// (`obc_pipeline::loopback_cover`): a consumer of the loopback keeps
/// receiving black frames if the daemon dies, or a warning saying why it
/// would not.
fn log_consumer_cover(cover: &ConsumerCover) {
    match cover {
        ConsumerCover::Ready { timeout_ms } => crate::info!(
            "crash cover: black timeout image, timeout {timeout_ms} ms (consumers keep \
             receiving black frames if the daemon stops writing)"
        ),
        ConsumerCover::Failed(why) => crate::warning!(
            "crash cover not in place: {why}; a consumer may lose the camera if the daemon \
             crashes"
        ),
        ConsumerCover::Off | ConsumerCover::Pending => {}
    }
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
///
/// In system memory NVML is not started: the session makes no GPU call.
fn build_session(
    params: &SessionParams,
    writer: WriterHandle,
    stamp: bool,
    fault: &mut StartFault,
) -> Result<Running, StartError> {
    let live = LiveStats::new();
    let observer = live.observer();
    let output = build_output(params, OutputSink::Writer(writer), fault, |output| {
        output.set_observer(Some(observer));
        if stamp {
            stamp_forwarded(output);
        }
    })?;
    // Started already: `Session::new` has nothing left to fail on. Should it
    // fail all the same, the output went with it unchecked.
    let session = Session::new(output).map_err(|e| {
        StartError::unclean(
            format!("output pipeline: {e}"),
            "the output pipeline was dropped without its stop being checked",
        )
    })?;
    let epoch = Instant::now();
    let gpu = match params.memory {
        VideoMemory::System => None,
        VideoMemory::Cuda => match GpuSampler::start(
            GPU_INDEX,
            GPU_INTERVAL,
            Arc::new(move || u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)),
        ) {
            Ok(s) => Some(s),
            Err(e) => {
                crate::debug!("GPU statistics unavailable: {e}");
                None
            }
        },
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
        enhance: enhance_backend(&params.backends),
        effect_backend: effect_backend(&params.backends),
        chains: module_stages(&params.effect, &params.backends)
            .iter()
            .map(|m| m.name.clone())
            .collect(),
        memory: params.memory,
    })
}

/// What a failed output build or start was due to, for
/// [`SessionError`]: the output contract ([`contract_fault`]), or the want
/// of CUDA ([`wants_cuda`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StartFault {
    contract: bool,
    no_cuda: bool,
}

impl StartFault {
    fn of(e: &PipelineError, memory: VideoMemory) -> StartFault {
        StartFault {
            contract: contract_fault(e),
            no_cuda: memory == VideoMemory::Cuda && wants_cuda(e),
        }
    }
}

/// Builds and starts the output of a session of `params` into `sink`, in
/// the session's memory, `prepare` called between the build and the
/// start. `fault` says whether a failure is the output contract's
/// ([`contract_fault`]) or for want of CUDA ([`wants_cuda`]). On error,
/// what was built is gone, as [`build_session`] says.
fn build_output(
    params: &SessionParams,
    sink: OutputSink,
    fault: &mut StartFault,
    prepare: impl FnOnce(&mut Output),
) -> Result<Output, StartError> {
    let mut ocfg = OutputConfig::new(params.contract, sink);
    ocfg.gap_threshold = params.input_gap;
    ocfg.hold_timeout = params.hold_timeout;
    // Always with the element (in passthrough it costs nothing): an output
    // built without it cannot take an effect later. Without a module that
    // provides the effect it has no backend, and a background effect asked
    // of it fails, `on_failure` deciding what it shows (black, the
    // fallback image or the camera image): the room is never shown for
    // want of a module.
    ocfg.effect = Some(effect_settings(&params.effect));
    ocfg.effect_backend = effect_backend(&params.backends);
    ocfg.module_stages = module_stages(&params.effect, &params.backends);
    // A failure for want of CUDA is not parked after (`StartError::no_cuda`).
    let mut output = Output::build_in(ocfg, params.memory).map_err(|e| {
        *fault = StartFault::of(&e, params.memory);
        StartError::from(format!("output pipeline: {e}")).for_want_of_cuda(fault.no_cuda)
    })?;
    prepare(&mut output);
    // The mapping (`start_failed`) is unit-tested; this wiring only by the
    // hardware tests.
    if let Err(e) = output.start() {
        *fault = StartFault::of(&e, params.memory);
        return Err(
            start_failed(format!("output pipeline: {e}"), output.stop_checked())
                .for_want_of_cuda(fault.no_cuda),
        );
    }
    Ok(output)
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
                    modes: d
                        .capabilities
                        .as_ref()
                        .map(|c| c.modes.clone())
                        .unwrap_or_default(),
                })
            })
            .collect()
    }

    fn start_writer(
        &mut self,
        sink: &SinkSelector,
        contract: &OutputContract,
    ) -> Result<PathBuf, WriterError> {
        self.stop_writer();
        let devices = scan_sysfs();
        let device = select_sink(&devices, sink).map_err(|e| e.to_string())?;
        let path = device.path.clone();
        self.run_writer(*contract, WriterSink::Loopback(path.clone()))
            .map_err(|e| WriterError {
                held: matches!(e, PipelineError::LoopbackFormatHeld { .. }),
                revertible: contract_fault(&e),
                held_as: match &e {
                    PipelineError::LoopbackFormatHeld { held_as, .. } => *held_as,
                    _ => None,
                },
                message: format!("loopback {}: {e}", path.display()),
            })?;
        Ok(path)
    }

    fn stop_writer(&mut self) {
        self.stop_session();
        self.stop_pipewire();
        self.stop_encoder();
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

    /// The output's part of the writer's tap goes first (no frame reaches
    /// the output after that; the preview keeps its part), then the output
    /// (its thread is joined and the node disappears). Events it still
    /// sends carry its generation, which the controller no longer accepts.
    fn stop_pipewire(&mut self) {
        if self.pipewire_tap.take().is_some() {
            self.install_tap();
        }
        #[cfg(feature = "pipewire")]
        if let Some(output) = self.output.take() {
            output.stop();
        }
    }

    fn set_preview(&mut self, preview: Option<PreviewRequest>) {
        let Some(req) = preview else {
            self.preview = None;
            self.stop_encoder();
            return;
        };
        let same_dir = self.preview.as_ref().is_some_and(|p| p.dir == req.dir);
        if !same_dir {
            self.stop_encoder();
        }
        if let Some(encoder) = self.encoder.as_mut()
            && encoder.config().fps != req.fps
        {
            encoder.set_fps(req.fps);
        }
        self.preview = Some(req);
        self.start_encoder();
    }

    fn writer_failure(&mut self) -> Option<String> {
        let w = self.writer.as_ref()?;
        w.health()
            .err()
            .map(|e| format!("loopback writer failed: {e}"))
    }

    fn set_placeholder(&mut self, image: Option<&Path>) {
        // The same image given again reaches the writer: it reads it again
        // if the file at its path was replaced.
        if self.placeholder.as_deref() != image {
            match image {
                Some(p) => crate::info!("the writer's placeholder shows {}", p.display()),
                None => crate::info!("the writer's placeholder is black"),
            }
            self.placeholder = image.map(Path::to_path_buf);
        }
        if let Some(w) = &self.writer {
            w.set_placeholder_image(image);
        }
    }

    fn placeholder_pending(&mut self) -> bool {
        self.writer
            .as_ref()
            .is_some_and(|w| w.placeholder_state() == PlaceholderState::Pending)
    }

    fn placeholder_failure(&mut self) -> Option<String> {
        match self.writer.as_ref()?.placeholder_state() {
            PlaceholderState::Failed(why) => Some(why),
            _ => None,
        }
    }

    fn loopback_holders(&mut self, node: &Path) -> u32 {
        // The daemon's own descriptors (the writer, the crash cover's
        // short-lived ones) are not counted.
        count_holders(Path::new("/proc"), &NodeId::of(node), std::process::id())
    }

    fn loopback_holder_names(&mut self, node: &Path) -> Vec<String> {
        holder_names(Path::new("/proc"), &NodeId::of(node), std::process::id())
    }

    fn start_session(&mut self, params: &SessionParams) -> Result<(), SessionError> {
        let Some(writer) = self.writer.as_ref().map(LoopbackWriter::handle) else {
            self.stop_session();
            return Err(SessionError::from(
                "no loopback writer is running".to_string(),
            ));
        };
        let stamp = self.stamp;
        // Whether the output's build or start failed for the contract, or
        // for want of CUDA; neither when the GPU could not be unparked (the
        // build did not run).
        let mut fault = StartFault::default();
        // In CUDA memory a running session stopped without parking in
        // between; the GPU unparked before the output (CUDA) or NVML touch
        // it, and parked again if the build fails (`Parker::start_session`).
        // In system memory nothing is unparked or parked
        // (`Parker::start_session_in`).
        self.parker
            .start_session_in(&mut self.running, params.memory, || {
                build_session(params, writer, stamp, &mut fault)
            })
            .map_err(|message| SessionError {
                message,
                contract: fault.contract,
                no_cuda: fault.no_cuda,
            })?;
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

    fn effect_available(&mut self, effect: &EffectRun, backends: &Backends) -> Result<(), String> {
        if effect.mode == EffectMode::None {
            return Ok(());
        }
        let Some(backend) = effect_backend(backends) else {
            return Err("no effect module is loaded".into());
        };
        // Called before a session starts, so while the GPU is parked: the
        // module's probe reads the filesystem and its deeper check may open
        // the module's own libraries, but neither makes a CUDA call (a CUDA
        // call would block until unparked).
        effect_backend_available_with(backend.factory().as_ref())?;
        obc_pipeline::effect::check_settings_with(
            &effect_settings(effect),
            backend.factory().as_ref(),
        )
        .map_err(effect_error)
    }

    fn set_effect(&mut self, effect: &EffectRun) -> Result<(), String> {
        let Some(r) = self.running.as_ref() else {
            return Ok(());
        };
        let output = r.session.output();
        output
            .set_effect(&effect_settings(effect))
            .map_err(effect_error)?;
        for c in effect.chains.iter().filter(|c| r.chains.contains(&c.name)) {
            let stages: Vec<StageRequest> = c.stages.iter().map(stage_request).collect();
            output
                .set_module_stages(&c.name, &stages)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// `obceffect` drops its backend instance mid-session
    /// (`ObcEffect::revoke_backend`) and is re-armed by a factory.
    fn can_revoke_effect_backend(&self) -> bool {
        true
    }

    fn revoke_effect_backend(&mut self) -> Result<(), String> {
        if let Some(r) = self.running.as_ref() {
            r.session.output().revoke_effect_backend();
        }
        Ok(())
    }

    fn restore_effect_backend(&mut self) -> Result<(), String> {
        if let Some(r) = self.running.as_ref() {
            r.session
                .output()
                .set_effect_backend(r.effect_backend.as_ref());
        }
        Ok(())
    }

    fn effect_report(&mut self) -> Option<EffectReport> {
        let running = self.running.as_ref()?;
        let session = &running.session;
        let output = session.output();
        let status = |s| match s {
            obc_pipeline::EffectStatus::Off => EffectStatus::Off,
            obc_pipeline::EffectStatus::Loading => EffectStatus::Loading,
            obc_pipeline::EffectStatus::Ready => EffectStatus::Ready,
            obc_pipeline::EffectStatus::Failed(r) => EffectStatus::Failed(r),
        };
        let stats = output.effect_stats().unwrap_or_default();
        let mut stages: Vec<(StageId, EffectStatus)> = output
            .effect_settings()
            .map(|s| s.stages)
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                let st = status(output.stage_status(s.id.as_str()));
                (s.id, st)
            })
            .collect();
        for name in &running.chains {
            for s in output.module_stages(name).unwrap_or_default() {
                let st = status(output.module_stage_status(name, s.id.as_str()));
                stages.push((s.id, st));
            }
        }
        Some(EffectReport {
            status: status(output.effect_status()),
            stages,
            // Read from the element, not the bus (the ingest's failure poll
            // would hide it).
            enhance: session
                .ingest()
                .and_then(obc_pipeline::Ingest::enhance_status)
                .map_or(EffectStatus::Off, status),
            load_ms: stats.last_load_ms,
            reused: stats.last_load_reused,
            gpu_ms_p50: stats.gpu_ms_p50,
            fallback_error: stats.fallback_error,
            stage_values: output.stage_values(),
        })
    }

    fn discover_modules(&mut self, dirs: &[PathBuf]) -> Vec<ModuleEntry> {
        let found = obc_module_host::scan(dirs);
        for (id, path) in &found.overridden {
            crate::debug!("module {id}: the manifest {} is overridden", path.display());
        }
        found.entries
    }

    fn open_module(
        &mut self,
        entry: &ModuleEntry,
        config: &serde_json::Value,
    ) -> Result<OpenedModule, String> {
        let manifest = entry.manifest.as_ref().map_err(Clone::clone)?;
        let module = Module::open(manifest, config, module_log())?;
        let stages = module.probe().map_err(|e| e.message)?;
        let settings = module.settings().map_err(|e| e.message)?;
        Ok(OpenedModule {
            module: Some(module),
            stages,
            settings,
        })
    }

    fn probe_module(
        &mut self,
        _id: &str,
        module: Option<&Module>,
    ) -> Result<Vec<(StageId, Availability)>, String> {
        match module {
            Some(m) => m.probe().map_err(|e| e.message),
            None => Err("the module is not loaded".into()),
        }
    }

    fn compute_capability(&mut self) -> Option<(u32, u32)> {
        // NVML runs next to the session already (the GPU statistics): its
        // answer costs no CUDA call.
        self.running
            .as_ref()?
            .gpu
            .as_ref()?
            .info()
            .compute_capability
    }

    fn cuda_unusable(&mut self) -> Option<String> {
        // `find` reads GStreamer's registry (its cache) and loads no
        // plugin: making a CUDA element would load `nvcodec`, whose
        // initialisation calls `cuInit`.
        let registered = obc_gpu::presence::CUDA_ELEMENTS
            .iter()
            .all(|name| gst::ElementFactory::find(name).is_some());
        obc_gpu::presence::probe_process(registered)
            .unusable_reason()
            .map(str::to_string)
    }

    fn open_camera_controls(
        &mut self,
        camera: &Camera,
        write: bool,
    ) -> Result<Box<dyn ControlNode>, String> {
        let what = format!("{} ({})", camera.label(), camera.path.display());
        let node = obc_pipeline::ControlNode::open(&camera.path, write)
            .map_err(|e| format!("opening {what} for its controls: {e}"))?;
        // The Extension Unit rows of its USB device (none for another
        // camera, or one that is not on USB).
        let (xu, xu_problem) = match find_xu_controls(Path::new(SYSFS_ROOT), &camera.path) {
            Ok(found) => (found, None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        Ok(Box::new(RealControlNode {
            node,
            xu,
            xu_problem,
            what,
        }))
    }

    fn watch_controls(&mut self, camera: &Camera, ids: &[u32]) -> Result<WatchGeneration, String> {
        self.unwatch_controls();
        let Some(reporter) = self.control_reporter.clone() else {
            return Err("control events are not reported by this backend".into());
        };
        self.watch_generation += 1;
        let generation = self.watch_generation;
        let watcher = CtrlWatcher::start(&camera.path, ids, move |notice| {
            reporter(
                generation,
                match notice {
                    CtrlNotice::Changed(ev) => ControlEvent::Changed(ev),
                    CtrlNotice::Ended(e) => ControlEvent::Ended(e.to_string()),
                },
            );
        })
        .map_err(|e| {
            format!(
                "watching the controls of {} ({}): {e}",
                camera.label(),
                camera.path.display()
            )
        })?;
        if !watcher.refused().is_empty() {
            crate::debug!(
                "{}: no change events for {} of its controls",
                camera.label(),
                watcher.refused().len()
            );
        }
        self.watcher = Some(watcher);
        Ok(generation)
    }

    fn unwatch_controls(&mut self) {
        // Dropping it wakes the thread and joins it.
        self.watcher = None;
    }
}

/// A camera node opened for its controls, with its Extension Unit rows.
struct RealControlNode {
    node: obc_pipeline::ControlNode,
    xu: Vec<XuControl>,
    /// Why its Extension Unit rows could not be looked up, if so.
    xu_problem: Option<String>,
    /// The camera's label and node, for the errors.
    what: String,
}

impl RealControlNode {
    fn xu_of(&self, row: &'static XuRow) -> Result<&XuControl, String> {
        self.xu
            .iter()
            .find(|x| std::ptr::eq(x.row(), row) || x.row().name == row.name)
            .ok_or_else(|| format!("{} has no {}", self.what, row.name))
    }
}

impl ControlNode for RealControlNode {
    fn enumerate(&mut self) -> ControlScan {
        let found = self.node.enumerate();
        let mut problems = Vec::new();
        if let Some(e) = found.incomplete {
            problems.push(("the list of its controls".to_string(), e.to_string()));
        }
        if let Some(e) = &self.xu_problem {
            problems.push(("its USB descriptors".to_string(), e.clone()));
        }
        let mut xu = Vec::new();
        for x in &self.xu {
            match x.probe(&self.node) {
                Ok(Some(state)) => xu.push(XuReading {
                    row: x.row(),
                    state,
                }),
                Ok(None) => {}
                Err(e) => problems.push((x.row().name.to_string(), e.to_string())),
            }
        }
        ControlScan {
            controls: found.controls,
            xu,
            problems,
        }
    }

    fn set(&mut self, id: u32, name: &str, value: i32) -> Result<(), String> {
        self.node
            .set(id, value)
            .map_err(|e| format!("setting {name} to {value} on {}: {e}", self.what))
    }

    fn get(&mut self, id: u32, name: &str) -> Result<i32, String> {
        self.node
            .get(id)
            .map_err(|e| format!("reading {name} of {}: {e}", self.what))
    }

    fn query(&mut self, id: u32, name: &str) -> Result<CtrlInfo, String> {
        match self.node.query(id) {
            Ok(Some(info)) => Ok(info),
            Ok(None) => Err(format!("{} no longer has {name}", self.what)),
            Err(e) => Err(format!("querying {name} of {}: {e}", self.what)),
        }
    }

    fn capture_format(&mut self) -> Result<CaptureFormat, String> {
        self.node
            .capture_format()
            .map_err(|e| format!("reading the capture format of {}: {e}", self.what))
    }

    fn xu_set(&mut self, row: &'static XuRow, option: &str) -> Result<(), String> {
        let x = *self.xu_of(row)?;
        x.set(&self.node, option)
            .map_err(|e| format!("setting {} to {option} on {}: {e}", row.name, self.what))
    }

    fn xu_current(&mut self, row: &'static XuRow) -> Result<u8, String> {
        let x = *self.xu_of(row)?;
        x.current(&self.node)
            .map_err(|e| format!("reading {} of {}: {e}", row.name, self.what))
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

/// Whether a writer's `e` is the output contract's fault: the loopback
/// refuses it, or its caps or elements cannot be made for it. Not a held
/// loopback ([`WriterError::held`]), a missing element, a device that
/// cannot be opened or another state change failure: those are retried
/// with the same contract.
fn contract_fault(e: &PipelineError) -> bool {
    matches!(
        e,
        PipelineError::LoopbackRefused { .. }
            | PipelineError::Caps(_)
            | PipelineError::Link(_)
            | PipelineError::InvalidProperty { .. }
    )
}

/// Whether an output's `e` is for want of CUDA: an element of the
/// `nvcodec` plugin is missing (the plugin is not installed, or loaded
/// without registering its elements because CUDA did not initialise), one
/// failed its state change with an error of its own on the bus (no device
/// to open a context on, say), or the output opened no CUDA context
/// ([`NO_CUDA_CONTEXT`]). A contract fault, a module's or the effect
/// element's error, another element's state change failure, or another
/// CUDA error is not.
fn wants_cuda(e: &PipelineError) -> bool {
    let cuda_element = |factory: &str| factory.starts_with("cuda") || factory.starts_with("nv");
    match e {
        PipelineError::MissingElement { factory, .. }
        | PipelineError::ElementState { factory, .. } => cuda_element(factory),
        PipelineError::Cuda(m) => m == NO_CUDA_CONTEXT,
        _ => false,
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

    #[test]
    fn only_errors_of_the_contract_revert_it() {
        let refused = PipelineError::LoopbackRefused {
            device: "/dev/video11".into(),
            accepted: "NV12 1920x1080@30".into(),
            wanted: "NV12 1280x720@30".into(),
        };
        assert!(contract_fault(&refused));
        assert!(contract_fault(&PipelineError::Caps("video/x-raw".into())));
        let held = PipelineError::LoopbackFormatHeld {
            device: "/dev/video11".into(),
            accepted: "NV12 1920x1080@30".into(),
            wanted: "NV12 1280x720@30".into(),
            held_as: Some(OutputContract::default()),
        };
        assert!(!contract_fault(&held));
        // A link failure is the contract's only when it is told so at its
        // origin: a module stage element's or the effect element's is
        // theirs (`Output::build`).
        assert!(contract_fault(&PipelineError::Link(
            "to_out to download".into()
        )));
        assert!(!contract_fault(&PipelineError::Module(
            "obc-fg-gen cannot be linked".into()
        )));
        assert!(!contract_fault(&PipelineError::Effect(
            "effect cannot be linked".into()
        )));
        assert!(!contract_fault(&PipelineError::State("busy".into())));
        assert!(!contract_fault(&PipelineError::Cuda("no device".into())));
        // A sink that cannot be selected is not the contract's.
        let e = WriterError::from("no loopback device labelled \"x\"".to_string());
        assert!(!e.held && !e.revertible);
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

    /// The writer's one tap feeds whichever of the PipeWire output and the
    /// preview runs, both in order when both do.
    #[test]
    fn the_composed_tap_feeds_what_runs() {
        use std::sync::Mutex;

        gst::init().unwrap();
        let seen: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let tap = |name: &'static str| -> FrameTap {
            let seen = Arc::clone(&seen);
            Arc::new(move |_: &gst::Buffer| seen.lock().unwrap().push(name))
        };
        let (pipewire, preview) = (tap("pipewire"), tap("preview"));
        let buffer = gst::Buffer::new();
        assert!(compose_tap(None, None).is_none());
        for (a, b, want) in [
            (Some(&pipewire), None, vec!["pipewire"]),
            (None, Some(&preview), vec!["preview"]),
            (Some(&pipewire), Some(&preview), vec!["pipewire", "preview"]),
        ] {
            seen.lock().unwrap().clear();
            compose_tap(a, b).unwrap()(&buffer);
            assert_eq!(*seen.lock().unwrap(), want);
        }
    }

    /// Waits until `done` holds, for at most 10 s.
    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The composition on a running writer (a fake sink: its placeholder
    /// frames, no device, no camera, no GPU): stopping the PipeWire output
    /// leaves the preview its frames and the reverse, and a new writer gets
    /// the preview again. The PipeWire output's part is a counting tap put
    /// where `start_pipewire_output` puts the output's.
    #[test]
    fn each_tap_part_outlives_the_other_and_a_new_writer_is_previewed() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};

        let seqs: Arc<Mutex<Vec<u32>>> = Arc::default();
        let s = Arc::clone(&seqs);
        let reporter: PreviewReporter = Arc::new(move |seq| s.lock().unwrap().push(seq));
        let mut b = GstBackend::new(None, Some(reporter)).unwrap();
        let pipewire_frames = Arc::new(AtomicU64::new(0));
        let pipewire_part = || -> FrameTap {
            let n = Arc::clone(&pipewire_frames);
            Arc::new(move |_: &gst::Buffer| {
                n.fetch_add(1, Ordering::SeqCst);
            })
        };
        let previewed = || seqs.lock().unwrap().len();
        let piped = || pipewire_frames.load(Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let request = || PreviewRequest {
            fps: 30,
            dir: dir.path().to_path_buf(),
        };
        let contract = OutputContract::default();

        b.run_writer(contract, WriterSink::Fake).unwrap();
        b.set_preview(Some(request()));
        b.pipewire_tap = Some(pipewire_part());
        b.install_tap();
        let (p, w) = (previewed(), piped());
        wait_until("both parts fed", || previewed() > p + 2 && piped() > w + 2);

        // The PipeWire output stops: the preview keeps its frames.
        b.stop_pipewire();
        let (p, w) = (previewed(), piped());
        wait_until("the preview fed alone", || previewed() > p + 3);
        assert_eq!(piped(), w, "the stopped output's part is gone");

        // The preview stops: the PipeWire output keeps its frames.
        b.pipewire_tap = Some(pipewire_part());
        b.install_tap();
        b.set_preview(None);
        let (p, w) = (previewed(), piped());
        wait_until("the output fed alone", || piped() > w + 3);
        assert_eq!(previewed(), p, "the stopped preview's part is gone");
        assert!(!dir.path().join(obc_pipeline::preview::FRAME_FILE).exists());

        // A new writer (a rebuild) while a preview is asked for: its tap
        // feeds a new encoder.
        b.set_preview(Some(request()));
        wait_until("the preview again", || previewed() > p + 2);
        b.stop_writer();
        b.run_writer(contract, WriterSink::Fake).unwrap();
        let p = previewed();
        wait_until("the new writer previewed", || previewed() > p + 3);
        let all = seqs.lock().unwrap().clone();
        assert!(all.windows(2).all(|w| w[0] < w[1]), "{all:?}");
        drop(b);
        assert!(!dir.path().join(obc_pipeline::preview::FRAME_FILE).exists());
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

    /// What the session is given with the Maxine module (its settings as it
    /// describes them) and `runtime`'s settings.
    fn maxine_run(runtime: &obc_core::config::RuntimeState) -> EffectRun {
        let modules = crate::modules::testing::maxine(&[]);
        let config = obc_core::config::Config::default();
        let plan = modules.plan();
        EffectRun {
            mode: EffectMode::None,
            blur_strength: 0.5,
            background_image: None,
            on_failure: EffectOnFailure::Black,
            fallback_image: None,
            stages: plan
                .effect_stages
                .iter()
                .map(|s| modules.stage_run(&plan, &config, runtime, s))
                .collect(),
            chains: modules.chain_runs(&plan, &config, runtime),
            enhance: false,
            background_params: modules
                .stage_run(&plan, &config, runtime, &StageId::BACKGROUND)
                .params,
            enhance_params: Default::default(),
        }
    }

    #[test]
    fn the_relighting_settings_map_to_the_elements_degrees_and_gain() {
        use obc_pipeline::effect::Value;
        let mut runtime = obc_core::config::RuntimeState {
            relight: Some(true),
            relight_light: Some(obc_core::RelightLight::Warm),
            relight_direction: Some(-30),
            relight_brightness: Some(150),
            ..Default::default()
        };
        runtime.migrate_legacy_settings();
        let s = effect_settings(&maxine_run(&runtime));
        assert!(s.is_enabled("relight"));
        let r = s.stage("relight").unwrap();
        assert_eq!(r.params.get("light"), Some(&Value::from("warm")));
        assert_eq!(r.live.get("direction"), Some(&Value::Int(-30)));
        assert_eq!(r.live.get("brightness"), Some(&Value::Int(150)));
        // Whole degrees and whole percent: the module takes the pan as the
        // degrees and the gain as the percent over 100, as before.
        let d = effect_settings(&maxine_run(&Default::default()));
        assert!(!d.is_enabled("relight") && !d.is_enabled("denoise"));
        assert_eq!(
            d.stages.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["denoise", "relight", "hdr"],
            "all listed, in the stage order"
        );
        let dr = d.stage("relight").unwrap();
        assert_eq!(dr.params.get("light"), Some(&Value::from("neutral")));
        assert_eq!(
            (dr.live.get("direction"), dr.live.get("brightness")),
            (Some(&Value::Int(0)), Some(&Value::Int(100)))
        );
        for light in obc_core::RelightLight::ALL {
            let mut runtime = obc_core::config::RuntimeState {
                relight_light: Some(light),
                ..Default::default()
            };
            runtime.migrate_legacy_settings();
            let run = maxine_run(&runtime);
            assert_eq!(
                run.stage("relight").unwrap().params.get("light"),
                Some(&Value::from(light.as_str()))
            );
        }
        // The configured bounds are within what the element takes.
        for (direction, brightness) in [(-90, 50), (90, 200)] {
            let mut runtime = obc_core::config::RuntimeState {
                relight: Some(true),
                relight_direction: Some(direction),
                relight_brightness: Some(brightness),
                ..Default::default()
            };
            runtime.migrate_legacy_settings();
            assert!(
                obc_pipeline::effect::check_settings(&effect_settings(&maxine_run(&runtime)))
                    .is_ok()
            );
        }
        // No module: an effect element without a backend, no chain.
        let none = crate::modules::Modules::empty().plan();
        assert!(none.effect.is_none() && none.chains.is_empty());
        assert!(effect_backend(&none.backends()).is_none());
    }

    #[test]
    fn the_background_and_enhance_parameters_reach_the_elements() {
        use obc_pipeline::effect::{Params, Value};
        let params = |k: &str, v: Value| -> Params { [(k.to_string(), v)].into() };
        let run = EffectRun {
            mode: EffectMode::Blur,
            background_params: params("chair", Value::Bool(true)),
            enhance: true,
            enhance_params: params("method", Value::from("fast")),
            ..maxine_run(&Default::default())
        };
        let s = effect_settings(&run);
        assert_eq!(s.kind, obc_pipeline::effect::EffectKind::Blur);
        assert_eq!(s.background_params, run.background_params);
        assert!(s.stage("background").is_none(), "not a listed stage");
        assert!(obc_pipeline::effect::check_settings(&s).is_ok());
        let cfg = obc_core::config::Config::default();
        let session = SessionParams {
            contract: cfg.output_contract(),
            aspect: cfg.output.aspect,
            hold_timeout: cfg.hold_timeout(),
            input_gap: cfg.input_gap(),
            fixed_framerate: false,
            effect: run.clone(),
            backends: Default::default(),
            memory: VideoMemory::Cuda,
        };
        let camera = Camera {
            id: "usb-a".into(),
            name: "A".into(),
            card: "A".into(),
            usb: None,
            serial: None,
            usb_path: None,
            path: "/dev/video9".into(),
            mode: "NV12:640x360@30".parse().unwrap(),
            modes: Vec::new(),
        };
        let backend = EnhanceBackend::new(Arc::new(
            obc_backend_maxine::vsr::MaxineVsrFactory::from_env(),
        ));
        let ingest = GstBackend::ingest_config(&session, &camera, Some(&backend));
        assert!(ingest.enhance);
        assert_eq!(ingest.enhance_params, run.enhance_params);
        // Never in system memory: the stage takes CUDA frames.
        let host = SessionParams {
            memory: VideoMemory::System,
            ..session
        };
        assert!(!GstBackend::ingest_config(&host, &camera, Some(&backend)).enhance);
    }

    #[test]
    fn only_a_missing_cuda_element_or_no_cuda_context_is_for_want_of_cuda() {
        let missing = |factory: &str| PipelineError::MissingElement {
            factory: factory.into(),
            source: gst::glib::bool_error!("no such element factory"),
        };
        assert!(wants_cuda(&missing("cudadownload")));
        assert!(wants_cuda(&missing("cudaupload")));
        assert!(!wants_cuda(&missing("v4l2sink")));
        assert!(wants_cuda(&PipelineError::Cuda(NO_CUDA_CONTEXT.into())));
        // A state change failed with an error from a CUDA element on the
        // bus; not one from another element, nor one with no element.
        let state = |factory: &str| PipelineError::ElementState {
            element: "upload".into(),
            factory: factory.into(),
            message: "Failed to get CUDA context".into(),
        };
        assert!(wants_cuda(&state("cudaupload")));
        assert!(wants_cuda(&state("nvh264enc")));
        assert!(!wants_cuda(&state("v4l2sink")));
        assert!(!wants_cuda(&state("")));
        assert!(!wants_cuda(&PipelineError::State(
            "Element failed to change its state".into()
        )));
        assert!(StartFault::of(&state("cudaconvert"), VideoMemory::Cuda).no_cuda);
        assert!(!StartFault::of(&state("cudaconvert"), VideoMemory::System).no_cuda);
        assert!(!wants_cuda(&PipelineError::Cuda(
            "the roundtrip process stage needs frames in CUDA memory".into()
        )));
        assert!(!wants_cuda(&PipelineError::Caps("video/x-raw".into())));
        assert!(!wants_cuda(&PipelineError::Module("obc-fg-gen".into())));
        // Only in CUDA memory: a session in system memory has none to want.
        let f = StartFault::of(&missing("cudadownload"), VideoMemory::Cuda);
        assert!(f.no_cuda && !f.contract);
        assert!(!StartFault::of(&missing("cudadownload"), VideoMemory::System).no_cuda);
        let f = StartFault::of(&PipelineError::Caps("x".into()), VideoMemory::Cuda);
        assert!(f.contract && !f.no_cuda);
    }
}
