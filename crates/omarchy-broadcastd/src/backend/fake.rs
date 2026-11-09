//! A recording media backend for the state-machine and D-Bus tests: no
//! device, no pipeline. The test shares the [`World`] with the backend and
//! changes it (cameras present, failures) between steps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use obc_core::config::EffectConfig;
use obc_core::{EffectMode, ModeRequest, OutputContract, SinkSelector};
use obc_gpu::{GpuError, NothingToPark, ParkError, ParkOutcome, UnparkOutcome};
use obc_ipc::Stats;

use super::park::{ParkOps, Parker, StartError, StopFailure, StopSession};
use super::{EffectReport, EffectStatus, Failure, MediaBackend, PipeWireGeneration, SessionParams};
use crate::camera::Camera;
use crate::log::Level;

/// The fake media world.
#[derive(Debug)]
pub struct World {
    /// What a scan returns.
    pub cameras: Vec<Camera>,
    /// The result of finding the sink: the node, or why it cannot be used.
    pub sink: Result<PathBuf, String>,
    /// The running writer: node and contract.
    pub writer: Option<(PathBuf, OutputContract)>,
    pub writer_starts: u32,
    /// The generation of the PipeWire output running next to the writer.
    pub pipewire: Option<PipeWireGeneration>,
    /// The generation of the last PipeWire output started (0: none yet).
    pub pipewire_generation: PipeWireGeneration,
    pub pipewire_starts: u32,
    /// Running PipeWire outputs stopped (alone or with the writer).
    pub pipewire_stops: u32,
    /// `offer_nv12` of the last `start_pipewire`.
    pub pipewire_offer_nv12: Option<bool>,
    /// Reported (once) by `writer_failure` while a writer runs.
    pub writer_failure: Option<String>,
    /// What `loopback_holders` counts.
    pub holders: u32,
    pub holder_counts: u32,
    /// The running session: its params and current camera id (`None`
    /// after a failed switch).
    pub session: Option<(SessionParams, Option<String>)>,
    pub session_starts: u32,
    pub session_stops: u32,
    /// Every camera an ingest was started on, in order.
    pub ingests: Vec<String>,
    /// `start_session` fails with this.
    pub fail_start: Option<String>,
    /// With `fail_start`: cleaning up the partly built output did not stop
    /// cleanly, with this (`backend::park::StartError::unclean`).
    pub fail_start_cleanup: Option<String>,
    /// The next session stop is unclean, with this (taken by that stop):
    /// a pipeline left short of NULL.
    pub fail_stop: Option<String>,
    /// The next session stop fails with this, every pipeline in NULL all
    /// the same (taken by that stop): a camera control not restored, say.
    pub fail_stop_at_null: Option<String>,
    /// With `fail_ingest`: the failed switch also left a pipeline outside
    /// NULL, with this (`obc_pipeline::SwitchTiming::unclean_stop`).
    pub unclean_switch: Option<String>,
    /// The running session's first unclean switch: its stop is then
    /// unclean, however it goes (the real backend's `Running::unclean`).
    pub session_unclean: Option<String>,
    /// Run by `start_session`, as if it happened during the output build.
    pub during_start: Option<Hook>,
    /// Ingests on these camera ids fail to start.
    pub fail_ingest: Vec<String>,
    /// Whether a started ingest delivers frames.
    pub frames: bool,
    /// Reported (once) by `session_failure` while a session runs.
    pub failure: Option<Failure>,
    pub stats: Stats,
    pub stats_taken: u32,
    pub scans: u32,
    /// Scans give every camera the requested size and rate (as a camera
    /// offering every mode would).
    pub follow_request: bool,
    /// `effect_available` fails with this for any effect but `none`.
    pub effect_unavailable: Option<String>,
    pub effect_probes: u32,
    /// An effect applied (at start or by `set_effect`) fails with this.
    pub effect_fail: Option<String>,
    /// An effect applied stays `Loading` (the test sets the status) rather
    /// than becoming `Ready` at once.
    pub effect_slow: bool,
    /// What `effect_report` reports while a session runs.
    pub effect_status: EffectStatus,
    /// The effect the running session applies.
    pub effect: Option<EffectConfig>,
    /// Every `set_effect` on a running session, in order.
    pub effect_sets: Vec<EffectConfig>,
    /// `set_effect` fails with this (once).
    pub fail_set_effect: Option<String>,
    /// The fake GPU driver under the backend's real [`Parker`]
    /// (`backend::park`): CUDA is initialised once a session is built
    /// (as GStreamer's `cuInit` does); before that there is nothing to
    /// park.
    pub cuda_initialised: bool,
    /// The GPU is parked: a stopped session parks it, a start unparks it
    /// before building anything.
    pub parked: bool,
    pub parks: u32,
    pub unparks: u32,
    /// Parking fails with this (the GPU stays awake), counted in
    /// `park_failures`.
    pub fail_park: Option<String>,
    pub park_failures: u32,
    /// Unparking fails with this: the start fails, the GPU stays parked.
    pub fail_unpark: Option<String>,
    /// What touched the GPU, in order: `park`, `unpark` and `build` (an
    /// output built).
    pub gpu_log: Vec<&'static str>,
    /// What the parker logged.
    pub parker_said: Vec<(Level, String)>,
}

impl Default for World {
    fn default() -> Self {
        World {
            cameras: Vec::new(),
            sink: Ok(PathBuf::from("/dev/video11")),
            writer: None,
            writer_starts: 0,
            pipewire: None,
            pipewire_generation: 0,
            pipewire_starts: 0,
            pipewire_stops: 0,
            pipewire_offer_nv12: None,
            writer_failure: None,
            holders: 0,
            holder_counts: 0,
            session: None,
            session_starts: 0,
            session_stops: 0,
            ingests: Vec::new(),
            fail_start: None,
            fail_start_cleanup: None,
            fail_stop: None,
            fail_stop_at_null: None,
            unclean_switch: None,
            session_unclean: None,
            during_start: None,
            fail_ingest: Vec::new(),
            frames: true,
            failure: None,
            stats: Stats {
                fps: 30.0,
                latency_p50_ms: 12.0,
                cpu_percent: 5.0,
                gpu_percent: Stats::NOT_MEASURABLE,
            },
            stats_taken: 0,
            scans: 0,
            follow_request: false,
            effect_unavailable: None,
            effect_probes: 0,
            effect_fail: None,
            effect_slow: false,
            effect_status: EffectStatus::Off,
            effect: None,
            effect_sets: Vec::new(),
            fail_set_effect: None,
            cuda_initialised: false,
            parked: false,
            parks: 0,
            unparks: 0,
            fail_park: None,
            park_failures: 0,
            fail_unpark: None,
            gpu_log: Vec::new(),
            parker_said: Vec::new(),
        }
    }
}

fn lock(world: &Mutex<World>) -> MutexGuard<'_, World> {
    world.lock().unwrap_or_else(|e| e.into_inner())
}

/// The fake GPU driver (the world's GPU fields), as `obc_gpu::park` and
/// `obc_gpu::unpark` would answer. Never called with the world locked.
#[derive(Debug, Clone)]
pub struct WorldGpu(Arc<Mutex<World>>);

impl ParkOps for WorldGpu {
    fn park(&mut self) -> Result<ParkOutcome, ParkError> {
        let mut w = lock(&self.0);
        if !w.cuda_initialised {
            return Ok(ParkOutcome::NothingToPark(NothingToPark::NotInitialised));
        }
        if w.parked {
            return Ok(ParkOutcome::NothingToPark(NothingToPark::AlreadyParked));
        }
        if let Some(e) = w.fail_park.clone() {
            w.park_failures += 1;
            return Err(ParkError::Awake(GpuError::Unavailable(e)));
        }
        w.parked = true;
        w.parks += 1;
        w.gpu_log.push("park");
        Ok(ParkOutcome::Parked {
            lock: Duration::from_millis(1),
            checkpoint: Duration::from_millis(15),
        })
    }

    fn unpark(&mut self) -> Result<UnparkOutcome, GpuError> {
        let mut w = lock(&self.0);
        if !w.parked {
            return Ok(UnparkOutcome::NotParked);
        }
        if let Some(e) = w.fail_unpark.clone() {
            return Err(GpuError::Unavailable(e));
        }
        w.parked = false;
        w.unparks += 1;
        w.gpu_log.push("unpark");
        Ok(UnparkOutcome::Unparked {
            restore: Duration::from_millis(17),
            unlock: Duration::ZERO,
        })
    }

    fn refusals(&self) -> u64 {
        0
    }
}

impl World {
    /// Stops the running PipeWire output, if any.
    fn stop_pipewire(&mut self) {
        if self.pipewire.take().is_some() {
            self.pipewire_stops += 1;
        }
    }

    /// The camera the session streams from.
    pub fn streaming(&self) -> Option<&str> {
        self.session.as_ref().and_then(|(_, c)| c.as_deref())
    }

    /// The running session applies `effect`, as the effect element would:
    /// `none` is off at once, an effect loads (or fails).
    fn apply_effect(&mut self, effect: &EffectConfig) {
        self.effect = Some(effect.clone());
        self.effect_status = if effect.mode == EffectMode::None {
            EffectStatus::Off
        } else if let Some(r) = &self.effect_fail {
            EffectStatus::Failed(r.clone())
        } else if self.effect_slow {
            EffectStatus::Loading
        } else {
            EffectStatus::Ready
        };
    }
}

/// The running session (the world's `session`), as the parker's
/// lifecycle sees it: its stop is the world's, unclean with `fail_stop` or
/// after an unclean switch (`session_unclean`), failed at NULL with
/// `fail_stop_at_null`.
struct FakeSession(Arc<Mutex<World>>);

impl StopSession for FakeSession {
    fn stop(self) -> Result<(), StopFailure> {
        let mut w = lock(&self.0);
        if w.session.take().is_some() {
            w.session_stops += 1;
        }
        // The models go with the session.
        w.effect = None;
        w.effect_status = EffectStatus::Off;
        let at_null = w.fail_stop_at_null.take();
        match (w.fail_stop.take(), w.session_unclean.take()) {
            (Some(e), _) | (None, Some(e)) => Err(StopFailure::unclean(e)),
            (None, None) => at_null.map_or(Ok(()), |e| Err(StopFailure::at_null(e))),
        }
    }
}

/// Something that happens while the fake does something slow.
pub struct Hook(pub Box<dyn FnMut() + Send>);

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hook")
    }
}

/// The backend half; clone it to keep the test's half (the clones share
/// the world and the parker).
#[derive(Debug, Clone)]
pub struct FakeBackend {
    world: Arc<Mutex<World>>,
    /// The daemon's park state machine, over the world's fake GPU. Locked
    /// before the world, never while the world is locked.
    parker: Arc<Mutex<Parker<WorldGpu>>>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        let world: Arc<Mutex<World>> = Arc::default();
        let said = Arc::clone(&world);
        let parker = Parker::with(
            WorldGpu(Arc::clone(&world)),
            Box::new(move |level, msg| lock(&said).parker_said.push((level, msg.to_string()))),
        );
        FakeBackend {
            world,
            parker: Arc::new(Mutex::new(parker)),
        }
    }
}

impl FakeBackend {
    pub fn world(&self) -> MutexGuard<'_, World> {
        lock(&self.world)
    }

    fn parker(&self) -> MutexGuard<'_, Parker<WorldGpu>> {
        self.parker.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The running session, for the parker's lifecycle.
    fn running(&self) -> Option<FakeSession> {
        let running = self.world().session.is_some();
        running.then(|| FakeSession(Arc::clone(&self.world)))
    }

    /// Whether the parker is tainted (a session did not stop cleanly).
    pub fn parking_tainted(&self) -> bool {
        self.parker().tainted()
    }

    fn start_ingest(w: &mut World, camera: &Camera) -> Result<(), String> {
        let Some((_, current)) = w.session.as_mut() else {
            return Err("no session".into());
        };
        *current = None;
        w.ingests.push(camera.id.clone());
        if w.fail_ingest.contains(&camera.id) {
            if let Some(why) = w.unclean_switch.clone()
                && w.session_unclean.is_none()
            {
                w.session_unclean = Some(format!("switching to {}: {why}", camera.label()));
            }
            return Err(format!("{}: cannot open", camera.label()));
        }
        if let Some((_, current)) = w.session.as_mut() {
            *current = Some(camera.id.clone());
        }
        Ok(())
    }
}

impl MediaBackend for FakeBackend {
    fn scan_cameras(&mut self, request: &ModeRequest) -> Vec<Camera> {
        let mut w = self.world();
        w.scans += 1;
        let mut cams = w.cameras.clone();
        if w.follow_request {
            for c in &mut cams {
                c.mode.width = request.width;
                c.mode.height = request.height;
                c.mode.fps_num = request.fps_num;
                c.mode.fps_den = request.fps_den;
            }
        }
        cams
    }

    fn start_writer(
        &mut self,
        _sink: &SinkSelector,
        contract: &OutputContract,
    ) -> Result<PathBuf, String> {
        let mut w = self.world();
        w.session = None;
        w.writer = None;
        w.stop_pipewire();
        let node = w.sink.clone()?;
        w.writer = Some((node.clone(), *contract));
        w.writer_starts += 1;
        Ok(node)
    }

    fn stop_writer(&mut self) {
        self.stop_session();
        let mut w = self.world();
        w.writer = None;
        w.stop_pipewire();
    }

    fn start_pipewire(&mut self, offer_nv12: bool) -> PipeWireGeneration {
        let mut w = self.world();
        w.pipewire_offer_nv12 = Some(offer_nv12);
        assert!(w.writer.is_some(), "the PipeWire output needs the writer");
        w.stop_pipewire();
        w.pipewire_generation += 1;
        w.pipewire_starts += 1;
        let generation = w.pipewire_generation;
        w.pipewire = Some(generation);
        generation
    }

    fn stop_pipewire(&mut self) {
        self.world().stop_pipewire();
    }

    fn writer_failure(&mut self) -> Option<String> {
        let mut w = self.world();
        if w.writer.is_some() {
            w.writer_failure.take()
        } else {
            None
        }
    }

    fn loopback_holders(&mut self, _node: &Path) -> u32 {
        let mut w = self.world();
        w.holder_counts += 1;
        w.holders
    }

    fn start_session(&mut self, params: &SessionParams) -> Result<(), String> {
        // Without the world's lock: the test may look at it meanwhile.
        let hook = self.world().during_start.take();
        if let Some(mut hook) = hook {
            (hook.0)();
            self.world().during_start = Some(hook);
        }
        {
            let w = self.world();
            assert!(w.writer.is_some(), "a session needs the writer");
            assert_eq!(
                Some(params.contract),
                w.writer.as_ref().map(|(_, c)| *c),
                "the session contract is the writer's"
            );
        }
        // The real parker's lifecycle: a running session stopped, the GPU
        // unparked before anything touches it, parked again if the start
        // fails cleanly.
        let world = Arc::clone(&self.world);
        let mut running = self.running();
        self.parker().start_session(&mut running, || {
            let mut w = lock(&world);
            if let Some(e) = w.fail_start.clone() {
                return Err(match w.fail_start_cleanup.clone() {
                    Some(why) => StartError::unclean(e, why),
                    None => StartError::from(e),
                });
            }
            w.cuda_initialised = true;
            w.session_unclean = None;
            w.gpu_log.push("build");
            w.session = Some((params.clone(), None));
            w.session_starts += 1;
            w.apply_effect(&params.effect);
            Ok(FakeSession(Arc::clone(&world)))
        })
    }

    fn switch_camera(&mut self, _params: &SessionParams, camera: &Camera) -> Result<(), String> {
        let mut w = self.world();
        Self::start_ingest(&mut w, camera)
    }

    fn stop_session(&mut self) {
        // All of it stopped, then the real parker parks if it stopped
        // cleanly.
        let mut running = self.running();
        self.parker().stop_session(&mut running);
    }

    fn first_frame(&mut self) -> bool {
        let w = self.world();
        w.frames && w.streaming().is_some()
    }

    fn session_failure(&mut self) -> Option<Failure> {
        let mut w = self.world();
        if w.session.is_some() {
            w.failure.take()
        } else {
            None
        }
    }

    fn take_stats(&mut self) -> Stats {
        let mut w = self.world();
        w.stats_taken += 1;
        w.stats
    }

    fn effect_available(&mut self, effect: &EffectConfig) -> Result<(), String> {
        let mut w = self.world();
        w.effect_probes += 1;
        match &w.effect_unavailable {
            Some(r) if effect.mode != EffectMode::None => Err(r.clone()),
            _ => Ok(()),
        }
    }

    fn set_effect(&mut self, effect: &EffectConfig) -> Result<(), String> {
        let mut w = self.world();
        if w.session.is_none() {
            return Ok(());
        }
        w.effect_sets.push(effect.clone());
        if let Some(e) = w.fail_set_effect.take() {
            return Err(e);
        }
        w.apply_effect(effect);
        Ok(())
    }

    fn effect_report(&mut self) -> Option<EffectReport> {
        let w = self.world();
        w.session.as_ref()?;
        Some(EffectReport {
            status: w.effect_status.clone(),
            load_ms: Some(120.0),
            reused: false,
            gpu_ms_p50: Some(1.2),
        })
    }
}
