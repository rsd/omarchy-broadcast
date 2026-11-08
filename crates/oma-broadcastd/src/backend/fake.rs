//! A recording media backend for the state-machine and D-Bus tests: no
//! device, no pipeline. The test shares the [`World`] with the backend and
//! changes it (cameras present, failures) between steps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use obc_core::{ModeRequest, OutputContract, SinkSelector};
use obc_ipc::Stats;

use super::{Failure, MediaBackend, SessionParams};
use crate::camera::Camera;

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
}

impl Default for World {
    fn default() -> Self {
        World {
            cameras: Vec::new(),
            sink: Ok(PathBuf::from("/dev/video11")),
            writer: None,
            writer_starts: 0,
            writer_failure: None,
            holders: 0,
            holder_counts: 0,
            session: None,
            session_starts: 0,
            session_stops: 0,
            ingests: Vec::new(),
            fail_start: None,
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
        }
    }
}

impl World {
    /// The camera the session streams from.
    pub fn streaming(&self) -> Option<&str> {
        self.session.as_ref().and_then(|(_, c)| c.as_deref())
    }
}

/// Something that happens while the fake does something slow.
pub struct Hook(pub Box<dyn FnMut() + Send>);

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hook")
    }
}

/// The backend half; clone the [`Arc`] to keep the test's half.
#[derive(Debug, Clone, Default)]
pub struct FakeBackend(pub Arc<Mutex<World>>);

impl FakeBackend {
    pub fn world(&self) -> MutexGuard<'_, World> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn start_ingest(w: &mut World, camera: &Camera) -> Result<(), String> {
        let Some((_, current)) = w.session.as_mut() else {
            return Err("no session".into());
        };
        *current = None;
        w.ingests.push(camera.id.clone());
        if w.fail_ingest.contains(&camera.id) {
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
        let node = w.sink.clone()?;
        w.writer = Some((node.clone(), *contract));
        w.writer_starts += 1;
        Ok(node)
    }

    fn stop_writer(&mut self) {
        self.stop_session();
        self.world().writer = None;
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
        let mut w = self.world();
        assert!(w.writer.is_some(), "a session needs the writer");
        assert_eq!(
            Some(params.contract),
            w.writer.as_ref().map(|(_, c)| *c),
            "the session contract is the writer's"
        );
        if let Some(e) = w.fail_start.clone() {
            return Err(e);
        }
        w.session = Some((params.clone(), None));
        w.session_starts += 1;
        Ok(())
    }

    fn switch_camera(&mut self, _params: &SessionParams, camera: &Camera) -> Result<(), String> {
        let mut w = self.world();
        Self::start_ingest(&mut w, camera)
    }

    fn stop_session(&mut self) {
        let mut w = self.world();
        if w.session.take().is_some() {
            w.session_stops += 1;
        }
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
}
