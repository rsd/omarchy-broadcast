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
use obc_pipeline::control_node::{CaptureFormat, CtrlValue, MenuItem, MenuItemValue, NodeControl};
use obc_pipeline::controls::{
    CTRL_FLAG_INACTIVE, CTRL_TYPE_BOOLEAN, CTRL_TYPE_BUTTON, CTRL_TYPE_INTEGER, CTRL_TYPE_MENU,
    CtrlInfo, DeviceControl, image_control_id,
};
use obc_pipeline::xu::{FIELD_OF_VIEW, XuRow, XuState};

use super::park::{ParkOps, Parker, StartError, StopFailure, StopSession};
use super::{
    ControlNode, ControlScan, EffectReport, EffectStatus, Failure, MediaBackend,
    PipeWireGeneration, PreviewRequest, SessionParams, WatchGeneration,
};
use crate::camera::Camera;
use crate::log::Level;
use crate::settings::XuReading;

/// A fake camera with any controls ([`World::devices`]): types, menus and
/// flags as the test builds them, dependencies that flip a control's
/// inactive flag when its master is written, a field of view whose writes
/// can change the capture format.
#[derive(Debug, Clone)]
pub struct FakeDevice {
    /// Its controls, in enumeration order, with their values and flags.
    pub controls: Vec<NodeControl>,
    /// `(dependent, master, values)`: writing the master clears the
    /// dependent's inactive flag when the master's new value is one of
    /// `values`, and sets it otherwise.
    pub links: Vec<(u32, u32, Vec<i32>)>,
    /// Its field of view (the Logitech row), if it has one.
    pub fov: Option<FakeFov>,
    /// The capture format `capture_format` reads.
    pub format: CaptureFormat,
    /// Writes to these control ids fail (the device refuses them).
    pub refuse: Vec<u32>,
}

impl Default for FakeDevice {
    fn default() -> Self {
        FakeDevice {
            controls: Vec::new(),
            links: Vec::new(),
            fov: None,
            format: CaptureFormat {
                width: 1920,
                height: 1080,
                pixelformat: u32::from_le_bytes(*b"NV12"),
            },
            refuse: Vec::new(),
        }
    }
}

impl FakeDevice {
    /// The control `id`.
    pub fn control(&self, id: u32) -> &NodeControl {
        self.controls
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no control {id:#010x}"))
    }

    /// The value of control `id`.
    pub fn value(&self, id: u32) -> i32 {
        self.control(id).value.value().expect("a value")
    }

    /// Sets control `id` as the device would by itself (or another
    /// program): its value and the flags of what depends on it.
    pub fn change(&mut self, id: u32, value: i32) {
        if let Some(c) = self.controls.iter_mut().find(|c| c.id == id) {
            c.value = CtrlValue::Value(value);
        }
        for (dependent, master, values) in self.links.clone() {
            if master != id {
                continue;
            }
            if let Some(d) = self.controls.iter_mut().find(|c| c.id == dependent) {
                if values.contains(&value) {
                    d.info.flags &= !CTRL_FLAG_INACTIVE;
                } else {
                    d.info.flags |= CTRL_FLAG_INACTIVE;
                }
            }
        }
    }
}

/// A fake field of view (the [`FIELD_OF_VIEW`] row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeFov {
    /// The current byte.
    pub current: u8,
    /// The default byte (`GET_DEF`).
    pub default: u8,
    /// Writing this byte changes the capture format to this one.
    pub changes_format: Option<(u8, CaptureFormat)>,
}

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
    /// The preview asked for (`set_preview`); it is encoded while a writer
    /// runs ([`World::previewing`]).
    pub preview: Option<PreviewRequest>,
    /// Every `set_preview`, by its rate (`None`: stopped), in order.
    pub preview_sets: Vec<Option<u32>>,
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
    /// The `fixed_framerate` each ingest was started with, in the same
    /// order as `ingests`.
    pub ingest_fixed_framerate: Vec<bool>,
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
    /// `effect_fail` fails the whole chain, as a load of blur or replace
    /// does in the element: a requested noise removal stage that has not
    /// failed itself shows the effect's reason too.
    pub effect_fail_chain: bool,
    /// An effect applied stays `Loading` (the test sets the status) rather
    /// than becoming `Ready` at once.
    pub effect_slow: bool,
    /// What `effect_report` reports while a session runs.
    pub effect_status: EffectStatus,
    /// Noise removal applied (at start or by `set_effect`) fails with this:
    /// the effect is unaffected, as in the element.
    pub denoise_fail: Option<String>,
    /// What `effect_report` reports for the noise removal stage.
    pub denoise_status: EffectStatus,
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
    /// The fake devices' image controls, by camera id, each with its
    /// current value (what `camera_controls` reports, what
    /// `set_camera_control` changes). A camera without an entry exposes
    /// none; a control left out is one it lacks.
    pub controls: std::collections::HashMap<String, Vec<DeviceControl>>,
    /// Every image control write that reached a fake device, in order:
    /// (camera id, control, value).
    pub control_writes: Vec<(String, String, i32)>,
    /// Writes of these controls fail (on any camera); nothing is changed.
    pub fail_control_writes: Vec<String>,
    /// Opening the node of these camera ids for its image controls fails.
    pub fail_control_query: Vec<String>,
    /// The number of image control queries ([`ControlNode::query`]).
    pub control_queries: u32,
    /// The number of nodes opened for their image controls.
    pub control_opens: u32,
    /// The number of single image control reads ([`ControlNode::get`]).
    pub control_reads: u32,
    /// A query skips these controls (on any camera), as unreadable.
    pub fail_control_reads: Vec<String>,
    /// The next queries' [`World::fail_control_reads`], one per query in
    /// turn (a control whose read fails, then works, or the other way
    /// round); once empty, `fail_control_reads` stays as the last one left
    /// it.
    pub next_read_failures: std::collections::VecDeque<Vec<String>>,
    /// The number of single control queries without a value
    /// ([`ControlNode::query`]).
    pub control_info_reads: u32,
    /// A write of this control leaves this value on the device instead (a
    /// driver that rounds or clamps), by control name.
    pub control_write_quirks: std::collections::HashMap<String, i32>,
    /// Fake devices with any controls, by camera id: a camera with an entry
    /// here is this device, and its `controls` entry is not used.
    pub devices: std::collections::HashMap<String, FakeDevice>,
    /// Every field of view write that reached a fake device, in order:
    /// (camera id, option id).
    pub xu_writes: Vec<(String, String)>,
    /// The number of capture format reads.
    pub format_reads: u32,
    /// The control-event watcher that runs: (camera id, control ids).
    pub watch: Option<(String, Vec<u32>)>,
    /// The generation of the last watcher started (0: none yet).
    pub watch_generation: WatchGeneration,
    pub watch_starts: u32,
    /// Running watchers stopped (alone or replaced).
    pub watch_stops: u32,
    /// Starting a watcher fails with this.
    pub fail_watch: Option<String>,
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
            preview: None,
            preview_sets: Vec::new(),
            writer_failure: None,
            holders: 0,
            holder_counts: 0,
            session: None,
            session_starts: 0,
            session_stops: 0,
            ingests: Vec::new(),
            ingest_fixed_framerate: Vec::new(),
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
            effect_fail_chain: false,
            effect_slow: false,
            effect_status: EffectStatus::Off,
            denoise_fail: None,
            denoise_status: EffectStatus::Off,
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
            controls: std::collections::HashMap::new(),
            control_writes: Vec::new(),
            fail_control_writes: Vec::new(),
            fail_control_query: Vec::new(),
            control_queries: 0,
            control_opens: 0,
            control_reads: 0,
            fail_control_reads: Vec::new(),
            next_read_failures: std::collections::VecDeque::new(),
            control_info_reads: 0,
            control_write_quirks: std::collections::HashMap::new(),
            devices: std::collections::HashMap::new(),
            xu_writes: Vec::new(),
            format_reads: 0,
            watch: None,
            watch_generation: 0,
            watch_starts: 0,
            watch_stops: 0,
            fail_watch: None,
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

    /// The rate the preview is encoded at, while one is asked for and a
    /// writer runs.
    pub fn previewing(&self) -> Option<u32> {
        self.writer.as_ref()?;
        self.preview.as_ref().map(|p| p.fps)
    }

    /// The camera the session streams from.
    pub fn streaming(&self) -> Option<&str> {
        self.session.as_ref().and_then(|(_, c)| c.as_deref())
    }

    /// The running session applies `effect`, as the effect element would:
    /// `none` is off at once, an effect loads (or fails); noise removal,
    /// apart, is off unless requested, else ready (or fails, itself or
    /// with the whole chain).
    fn apply_effect(&mut self, effect: &EffectConfig) {
        self.denoise_status = if !effect.denoise {
            EffectStatus::Off
        } else if let Some(r) = &self.denoise_fail {
            EffectStatus::Failed(r.clone())
        } else {
            EffectStatus::Ready
        };
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
        if let EffectStatus::Failed(r) = &self.effect_status
            && self.effect_fail_chain
            && self.denoise_status == EffectStatus::Ready
        {
            self.denoise_status = EffectStatus::Failed(r.clone());
        }
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
        w.denoise_status = EffectStatus::Off;
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

    fn set_preview(&mut self, preview: Option<PreviewRequest>) {
        let mut w = self.world();
        w.preview_sets.push(preview.as_ref().map(|p| p.fps));
        w.preview = preview;
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

    fn switch_camera(&mut self, params: &SessionParams, camera: &Camera) -> Result<(), String> {
        let mut w = self.world();
        w.ingest_fixed_framerate.push(params.fixed_framerate);
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
            denoise: w.denoise_status.clone(),
            load_ms: Some(120.0),
            reused: false,
            gpu_ms_p50: Some(1.2),
        })
    }

    fn open_camera_controls(
        &mut self,
        camera: &Camera,
        write: bool,
    ) -> Result<Box<dyn ControlNode>, String> {
        let mut w = self.world();
        w.control_opens += 1;
        if w.fail_control_query.contains(&camera.id) {
            return Err(format!(
                "opening {} for its controls: Input/output error",
                camera.path.display()
            ));
        }
        Ok(Box::new(FakeControlNode {
            world: Arc::clone(&self.world),
            camera: camera.clone(),
            write,
        }))
    }

    fn watch_controls(&mut self, camera: &Camera, ids: &[u32]) -> Result<WatchGeneration, String> {
        self.unwatch_controls();
        let mut w = self.world();
        if let Some(e) = w.fail_watch.clone() {
            return Err(e);
        }
        w.watch_generation += 1;
        w.watch_starts += 1;
        w.watch = Some((camera.id.clone(), ids.to_vec()));
        Ok(w.watch_generation)
    }

    fn unwatch_controls(&mut self) {
        let mut w = self.world();
        if w.watch.take().is_some() {
            w.watch_stops += 1;
        }
    }
}

/// The synthetic id of a simple control ([`World::controls`]) that is not
/// one of the five image controls.
const SIMPLE_ID_BASE: u32 = 0x0098_0f00;

/// A simple control of [`World::controls`] as the device would report it:
/// an integer of the user class (the image control's id), no flag.
fn simple_control(i: usize, c: &DeviceControl, unreadable: bool) -> NodeControl {
    NodeControl {
        id: image_control_id(&c.name).unwrap_or(SIMPLE_ID_BASE + i as u32),
        name: c.name.clone(),
        info: CtrlInfo {
            name: c.name.clone(),
            minimum: c.min,
            maximum: c.max,
            step: c.step,
            default_value: c.default,
            flags: 0,
            type_: CTRL_TYPE_INTEGER,
        },
        value: if unreadable {
            CtrlValue::Unreadable {
                errno: Some(libc::EPIPE),
                message: "Broken pipe".into(),
            }
        } else {
            CtrlValue::Value(c.value)
        },
        menu: Vec::new(),
    }
}

/// A fake device node opened for its controls.
struct FakeControlNode {
    world: Arc<Mutex<World>>,
    camera: Camera,
    /// Opened for writing: a write through a node that was not fails.
    write: bool,
}

impl FakeControlNode {
    /// The name of control `id` on the camera, if it has it.
    fn name_of(w: &World, camera: &str, id: u32) -> Option<String> {
        if let Some(d) = w.devices.get(camera) {
            return d
                .controls
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone());
        }
        w.controls.get(camera).and_then(|cs| {
            cs.iter()
                .enumerate()
                .find(|(i, c)| simple_control(*i, c, false).id == id)
                .map(|(_, c)| c.name.clone())
        })
    }

    fn fov_of<'a>(&self, w: &'a mut World, row: &'static XuRow) -> Result<&'a mut FakeFov, String> {
        let path = self.camera.path.display().to_string();
        w.devices
            .get_mut(&self.camera.id)
            .and_then(|d| d.fov.as_mut())
            .filter(|_| row.name == FIELD_OF_VIEW.name)
            .ok_or_else(|| format!("{path}: no {}", row.name))
    }
}

impl ControlNode for FakeControlNode {
    fn enumerate(&mut self) -> ControlScan {
        let mut w = lock(&self.world);
        w.control_queries += 1;
        if let Some(next) = w.next_read_failures.pop_front() {
            w.fail_control_reads = next;
        }
        let unreadable = w.fail_control_reads.clone();
        if let Some(d) = w.devices.get(&self.camera.id) {
            let controls = d
                .controls
                .iter()
                .cloned()
                .map(|mut c| {
                    if unreadable.contains(&c.name) {
                        c.value = CtrlValue::Unreadable {
                            errno: Some(libc::EPIPE),
                            message: "Broken pipe".into(),
                        };
                    }
                    c
                })
                .collect();
            let xu = d
                .fov
                .iter()
                .map(|f| XuReading {
                    row: &FIELD_OF_VIEW,
                    state: XuState {
                        info: 0x03,
                        current: f.current,
                        current_option: FIELD_OF_VIEW.option_for_value(f.current),
                        default_raw: Some(f.default),
                        default: FIELD_OF_VIEW
                            .option_for_value(f.default)
                            .unwrap_or(&FIELD_OF_VIEW.options[0]),
                    },
                })
                .collect();
            return ControlScan {
                controls,
                xu,
                problems: Vec::new(),
            };
        }
        let controls = w
            .controls
            .get(&self.camera.id)
            .cloned()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, c)| simple_control(i, c, unreadable.contains(&c.name)))
            .collect();
        ControlScan {
            controls,
            xu: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn set(&mut self, id: u32, _name: &str, value: i32) -> Result<(), String> {
        let mut w = lock(&self.world);
        let path = self.camera.path.display().to_string();
        let Some(name) = Self::name_of(&w, &self.camera.id, id) else {
            return Err(format!("{path}: setting {id:#010x}: Invalid argument"));
        };
        if !self.write {
            return Err(format!(
                "{path}: setting {name}: opened read-only (a controller bug)"
            ));
        }
        if w.fail_control_writes.contains(&name) {
            return Err(format!("{path}: setting {name}: Input/output error"));
        }
        let stored = w.control_write_quirks.get(&name).copied().unwrap_or(value);
        if let Some(d) = w.devices.get_mut(&self.camera.id) {
            let inactive = d.control(id).info.flags & CTRL_FLAG_INACTIVE != 0;
            if d.refuse.contains(&id) || inactive {
                return Err(format!("{path}: setting {name}: Permission denied"));
            }
            d.change(id, stored);
        } else if let Some(c) = w
            .controls
            .get_mut(&self.camera.id)
            .and_then(|cs| cs.iter_mut().find(|c| c.name == name))
        {
            c.value = stored;
        }
        w.control_writes.push((self.camera.id.clone(), name, value));
        Ok(())
    }

    fn get(&mut self, id: u32, _name: &str) -> Result<i32, String> {
        let mut w = lock(&self.world);
        w.control_reads += 1;
        let path = self.camera.path.display();
        if let Some(d) = w.devices.get(&self.camera.id) {
            return d
                .controls
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.value.value())
                .ok_or_else(|| format!("{path}: reading {id:#010x}: Invalid argument"));
        }
        let name = Self::name_of(&w, &self.camera.id, id);
        w.controls
            .get(&self.camera.id)
            .and_then(|cs| cs.iter().find(|c| Some(&c.name) == name.as_ref()))
            .map(|c| c.value)
            .ok_or_else(|| format!("{path}: reading {id:#010x}: Invalid argument"))
    }

    fn query(&mut self, id: u32, _name: &str) -> Result<CtrlInfo, String> {
        let mut w = lock(&self.world);
        w.control_info_reads += 1;
        let path = self.camera.path.display();
        if let Some(d) = w.devices.get(&self.camera.id) {
            return d
                .controls
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.info.clone())
                .ok_or_else(|| format!("{path}: querying {id:#010x}: Invalid argument"));
        }
        w.controls
            .get(&self.camera.id)
            .and_then(|cs| {
                cs.iter()
                    .enumerate()
                    .map(|(i, c)| simple_control(i, c, false))
                    .find(|c| c.id == id)
            })
            .map(|c| c.info)
            .ok_or_else(|| format!("{path}: querying {id:#010x}: Invalid argument"))
    }

    fn capture_format(&mut self) -> Result<CaptureFormat, String> {
        let mut w = lock(&self.world);
        w.format_reads += 1;
        Ok(w.devices
            .get(&self.camera.id)
            .map(|d| d.format)
            .unwrap_or_else(|| FakeDevice::default().format))
    }

    fn xu_set(&mut self, row: &'static XuRow, option: &str) -> Result<(), String> {
        let mut w = lock(&self.world);
        if !self.write {
            return Err("opened read-only (a controller bug)".into());
        }
        let byte = row
            .option(option)
            .ok_or_else(|| format!("{option:?} is not an option of {}", row.name))?
            .value;
        let fov = self.fov_of(&mut w, row)?;
        fov.current = byte;
        let change = fov.changes_format.filter(|(b, _)| *b == byte);
        if let (Some((_, format)), Some(d)) = (change, w.devices.get_mut(&self.camera.id)) {
            d.format = format;
        }
        w.xu_writes
            .push((self.camera.id.clone(), option.to_string()));
        Ok(())
    }

    fn xu_current(&mut self, row: &'static XuRow) -> Result<u8, String> {
        let mut w = lock(&self.world);
        Ok(self.fov_of(&mut w, row)?.current)
    }
}

/// The control ids of [`brio_device`].
pub mod cid {
    pub const SHARPNESS: u32 = 0x0098_091b;
    pub const GAIN: u32 = 0x0098_0913;
    pub const WB_AUTO: u32 = 0x0098_090c;
    pub const WB_TEMP: u32 = 0x0098_091a;
    pub const AUTO_EXPOSURE: u32 = 0x009a_0901;
    pub const EXPOSURE: u32 = 0x009a_0902;
    pub const DYNAMIC_FPS: u32 = 0x009a_0903;
    pub const PAN_RESET: u32 = 0x009a_0906;
}

use cid::{AUTO_EXPOSURE, DYNAMIC_FPS, EXPOSURE, GAIN, PAN_RESET, SHARPNESS, WB_AUTO, WB_TEMP};

/// A control as a device would report it.
pub fn node_control(
    id: u32,
    name: &str,
    type_: u32,
    range: (i32, i32, i32),
    default: i32,
    value: i32,
    flags: u32,
) -> NodeControl {
    NodeControl {
        id,
        name: obc_pipeline::control_node::normalise_control_name(name),
        info: CtrlInfo {
            name: name.into(),
            minimum: range.0,
            maximum: range.1,
            step: range.2,
            default_value: default,
            flags,
            type_,
        },
        value: if type_ == CTRL_TYPE_BUTTON {
            CtrlValue::None
        } else {
            CtrlValue::Value(value)
        },
        menu: Vec::new(),
    }
}

/// A fake MX Brio: its controls with the values a real one reports
/// (exposure, temperature and focus inactive under their automatic modes),
/// the dependencies the driver has, its field of view at 65°.
pub fn brio_device() -> FakeDevice {
    let int = |id, name, r, d, v, f| node_control(id, name, CTRL_TYPE_INTEGER, r, d, v, f);
    let boolean = |id, name, d, v| node_control(id, name, CTRL_TYPE_BOOLEAN, (0, 1, 1), d, v, 0);
    let mut mode = node_control(
        AUTO_EXPOSURE,
        "Auto Exposure",
        CTRL_TYPE_MENU,
        (0, 3, 1),
        3,
        3,
        0,
    );
    mode.menu = vec![
        MenuItem {
            index: 1,
            value: MenuItemValue::Name("Manual Mode".into()),
        },
        MenuItem {
            index: 3,
            value: MenuItemValue::Name("Aperture Priority Mode".into()),
        },
    ];
    FakeDevice {
        controls: vec![
            int(0x0098_0900, "Brightness", (0, 255, 1), 128, 128, 0),
            int(0x0098_0901, "Contrast", (0, 255, 1), 128, 128, 0),
            boolean(WB_AUTO, "White Balance, Automatic", 1, 1),
            int(GAIN, "Gain", (0, 255, 1), 0, 0, 0),
            int(
                WB_TEMP,
                "White Balance Temperature",
                (2000, 7500, 1),
                5000,
                2975,
                CTRL_FLAG_INACTIVE,
            ),
            int(SHARPNESS, "Sharpness", (0, 255, 1), 128, 128, 0),
            mode,
            int(
                EXPOSURE,
                "Exposure Time, Absolute",
                (3, 2047, 1),
                156,
                1250,
                CTRL_FLAG_INACTIVE,
            ),
            boolean(DYNAMIC_FPS, "Exposure, Dynamic Framerate", 0, 1),
            node_control(
                PAN_RESET,
                "Pan, Reset",
                CTRL_TYPE_BUTTON,
                (0, 0, 0),
                0,
                0,
                0,
            ),
        ],
        links: vec![
            (WB_TEMP, WB_AUTO, vec![0]),
            (EXPOSURE, AUTO_EXPOSURE, vec![1]),
        ],
        fov: Some(FakeFov {
            current: 2,
            default: 0,
            changes_format: None,
        }),
        ..FakeDevice::default()
    }
}
