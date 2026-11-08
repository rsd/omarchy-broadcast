//! The lifecycle state machine (`docs/adr/0011-daemon-lifecycle-and-dbus-api.md`,
//! "Lifecycle").
//!
//! One [`Controller`] owns the config, the runtime state and, through its
//! [`MediaBackend`], every pipeline. It reacts to [`Command`]s (D-Bus
//! methods, the watchers, signals) and to the passage of time, both with the
//! current instant passed in, so the tests drive it with explicit times.
//! After every step it publishes the property snapshot ([`Published`]),
//! before a method's reply is sent: a client that reads a property after a
//! reply sees the change.
//!
//! The state follows from what runs:
//!
//! | condition (first that holds)              | `State`    |
//! |-------------------------------------------|------------|
//! | `Enabled` is false                        | `off`      |
//! | a session runs, its camera delivered      | `active`   |
//! | a session runs, no camera frame yet       | `starting` |
//! | no writer, or a lifecycle fault           | `error`    |
//! | otherwise                                 | `idle`     |
//!
//! A session (camera + CUDA output) runs only while it is wanted: enabled,
//! the writer running, no fault, and a consumer present for the start
//! debounce, or (once running) a consumer present or gone for less than the
//! idle grace. A fault (no camera, start failed, camera lost with no
//! replacement, every camera failed, output failure) stops the session; it
//! is cleared when the cause can have changed: the camera set changes, a
//! control method is called, the config is reloaded, or the last consumer
//! leaves.
//!
//! A state file that cannot be read or parsed fails closed: the daemon
//! starts `off`, with `Error` naming the file, and does not rewrite the
//! file until `SetEnabled` is called.
//!
//! The streaming camera is followed by its device node, not by its id: an
//! id can change between two scans of the same camera (udev adds the
//! `/dev/v4l` links after the node), and such a change is a rename, not a
//! loss.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use obc_core::OutputContract;
use obc_core::config::{Config, RuntimeState};
use obc_ipc::{CameraInfo, INTERFACE_REVISION, IpcError, Snapshot, State, Stats};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::backend::{Failure, MediaBackend, SessionParams};
use crate::camera::{self, Camera};
use crate::{debug, error, info, warning};

/// How often a missing or held loopback device is retried.
pub const WRITER_RETRY: Duration = Duration::from_secs(2);
/// A camera that delivers no frame this long after its ingest started has
/// failed.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
/// The `Stats` signal period while `active`.
pub const STATS_PERIOD: Duration = Duration::from_secs(1);
/// How often a starting session is checked for its first frame.
const POLL_STARTING: Duration = Duration::from_millis(50);
/// How often a running session is checked for failures.
const POLL_RUNNING: Duration = Duration::from_millis(250);
/// The longest sleep with nothing scheduled.
const IDLE_WAKE: Duration = Duration::from_secs(60);

/// The reply channel of a D-Bus method.
pub type Reply = tokio::sync::oneshot::Sender<Result<(), IpcError>>;

/// What the controller is asked to do.
#[derive(Debug)]
pub enum Command {
    SetEnabled(bool, Reply),
    SelectCamera(String, Reply),
    NextCamera(Reply),
    /// `ReloadConfig()` (with its reply) or SIGHUP (without).
    ReloadConfig(Option<Reply>),
    /// The consumer watcher's count of processes holding the loopback.
    Consumers(u32),
    /// Video nodes appeared or disappeared: scan the cameras again.
    Rescan,
    /// Stop everything and end the controller loop.
    Shutdown,
}

/// One changed property, with its new value.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    State(State),
    Error(String),
    Enabled(bool),
    Cameras(Vec<CameraInfo>),
    ActiveCamera(String),
    OutputMode(String),
    Consumers(u32),
}

impl Change {
    /// The D-Bus property name.
    pub fn name(&self) -> &'static str {
        match self {
            Change::State(_) => "State",
            Change::Error(_) => "Error",
            Change::Enabled(_) => "Enabled",
            Change::Cameras(_) => "Cameras",
            Change::ActiveCamera(_) => "ActiveCamera",
            Change::OutputMode(_) => "OutputMode",
            Change::Consumers(_) => "Consumers",
        }
    }
}

/// What the controller tells the D-Bus side.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// Properties that changed in one step, in emission order.
    Changed(Vec<Change>),
    Stats(Stats),
}

/// What the controller and the D-Bus side share: the property snapshot the
/// D-Bus object serves, the channel its changes are announced on, and the
/// number of `SetEnabled(false)` calls queued but not handled yet.
#[derive(Debug, Clone)]
pub struct Published {
    snapshot: Arc<Mutex<Snapshot>>,
    notes: UnboundedSender<Note>,
    /// `SetEnabled(false)` calls queued and not handled yet: a session
    /// start in progress (the output build is slow) sees them before it
    /// opens a camera, so the camera is not opened after the user turned
    /// it off.
    disables_pending: Arc<AtomicU32>,
}

impl Published {
    pub fn new(initial: Snapshot) -> (Self, UnboundedReceiver<Note>) {
        let (notes, rx) = unbounded_channel();
        (
            Published {
                snapshot: Arc::new(Mutex::new(initial)),
                notes,
                disables_pending: Arc::new(AtomicU32::new(0)),
            },
            rx,
        )
    }

    /// The current snapshot.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stores `new` and announces what differs from the previous snapshot.
    fn update(&self, new: Snapshot) {
        let changes = {
            let mut cur = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            let changes = diff(&cur, &new);
            *cur = new;
            changes
        };
        if !changes.is_empty() {
            let _ = self.notes.send(Note::Changed(changes));
        }
    }

    fn stats(&self, stats: Stats) {
        let _ = self.notes.send(Note::Stats(stats));
    }

    /// Announces a `SetEnabled(false)` about to be queued (the D-Bus side
    /// calls it before sending the command).
    pub fn request_disable(&self) {
        self.disables_pending.fetch_add(1, Ordering::SeqCst);
    }

    /// Whether a `SetEnabled(false)` is queued and not handled yet.
    pub fn disable_pending(&self) -> bool {
        self.disables_pending.load(Ordering::SeqCst) > 0
    }

    /// A `SetEnabled(false)` is being handled (one queued without the
    /// count, as the tests send them, leaves it at zero).
    fn disable_handled(&self) {
        let mut n = self.disables_pending.load(Ordering::SeqCst);
        while n > 0 {
            match self.disables_pending.compare_exchange_weak(
                n,
                n - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(current) => n = current,
            }
        }
    }
}

/// The properties that differ, `State` last (so a client reacting to a
/// state sees the other properties of that step already updated).
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let mut out = Vec::new();
    if old.enabled != new.enabled {
        out.push(Change::Enabled(new.enabled));
    }
    if old.cameras != new.cameras {
        out.push(Change::Cameras(new.cameras.clone()));
    }
    if old.active_camera != new.active_camera {
        out.push(Change::ActiveCamera(new.active_camera.clone()));
    }
    if old.output_mode != new.output_mode {
        out.push(Change::OutputMode(new.output_mode.clone()));
    }
    if old.consumers != new.consumers {
        out.push(Change::Consumers(new.consumers));
    }
    if old.error != new.error {
        out.push(Change::Error(new.error.clone()));
    }
    if old.state != new.state {
        out.push(Change::State(new.state));
    }
    out
}

/// `OutputMode`: `NV12 1920x1080@30`, a fractional rate as `30000/1001`.
pub fn output_mode_string(c: &OutputContract) -> String {
    let rate = if c.fps_den == 1 {
        c.fps_num.to_string()
    } else {
        format!("{}/{}", c.fps_num, c.fps_den)
    };
    format!("{} {}x{}@{rate}", c.format.gst_name(), c.width, c.height)
}

/// The config file and the runtime state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

/// The runtime state from `path`. A file that cannot be read or parsed
/// fails closed: not enabled, no selection, and the second value is the
/// `Error` text naming the file (`None`: the file is fine or absent).
pub fn load_runtime_state(path: &Path) -> (RuntimeState, Option<String>) {
    match RuntimeState::load(path) {
        (state, None) => (state, None),
        (_, Some(warn)) => {
            let why = warn.trim_end_matches("; using the default state");
            (
                RuntimeState {
                    enabled: false,
                    camera: None,
                },
                Some(format!(
                    "{why}; the virtual camera stays off until SetEnabled is called, which \
                     rewrites the state file"
                )),
            )
        }
    }
}

/// The snapshot served before the controller has run (only the files are
/// read: no device is touched).
pub fn preliminary_snapshot(paths: &Paths) -> Snapshot {
    let config = Config::load(&paths.config).unwrap_or_default();
    let (runtime, damaged) = load_runtime_state(&paths.state);
    Snapshot {
        state: if runtime.enabled {
            State::Idle
        } else {
            State::Off
        },
        error: damaged.unwrap_or_default(),
        enabled: runtime.enabled,
        cameras: Vec::new(),
        active_camera: String::new(),
        output_mode: output_mode_string(&config.output_contract()),
        effect: "none".into(),
        consumers: 0,
        interface_revision: INTERFACE_REVISION,
    }
}

/// Told the loopback node whenever it changes (`None`: no writer), so the
/// consumer watcher watches the right node.
pub type SinkHook = Box<dyn FnMut(Option<&Path>) + Send>;

/// The running writer.
#[derive(Debug, Clone, PartialEq)]
struct WriterOn {
    node: PathBuf,
    contract: OutputContract,
    sink: obc_core::SinkSelector,
}

/// The running session.
#[derive(Debug, Clone, PartialEq)]
struct Streaming {
    /// The id of the camera it streams (or is starting) from.
    camera: String,
    /// That camera's device node: what identifies it across scans.
    node: PathBuf,
    /// The cameras (node, label) that failed in this session since a
    /// camera last delivered, the camera set last changed or the user last
    /// picked one; none of them is tried again before that.
    failed: Vec<(PathBuf, String)>,
    /// A camera frame has reached the output: `active`.
    live: bool,
    /// When the current camera's ingest started, until its first frame.
    awaiting: Option<Instant>,
}

/// The lifecycle state machine (see the module docs).
pub struct Controller<B: MediaBackend> {
    backend: B,
    paths: Paths,
    config: Config,
    runtime: RuntimeState,
    cameras: Vec<Camera>,
    writer: Option<WriterOn>,
    /// Why the writer could not start (the last attempt).
    writer_error: Option<String>,
    writer_retry_at: Option<Instant>,
    /// A deferred writer rebuild found the loopback held although the
    /// watcher counted nobody: when to count again.
    rebuild_recount_at: Option<Instant>,
    session: Option<Streaming>,
    /// A lifecycle fault: `error` until its cause can have changed.
    fault: Option<String>,
    /// The `Error` property.
    error: String,
    /// The state file could not be read or parsed: why (the `Error` text).
    /// The daemon runs `off` and leaves the file alone until `SetEnabled`.
    state_damaged: Option<String>,
    consumers: u32,
    /// Since when a consumer has been present (debounce).
    present_since: Option<Instant>,
    /// Since when no consumer has been present (grace).
    absent_since: Option<Instant>,
    next_stats: Option<Instant>,
    published: Published,
    sink_hook: Option<SinkHook>,
}

impl<B: MediaBackend> Controller<B> {
    /// Loads the config (an invalid file: the defaults, and `Error` set) and
    /// the runtime state, scans the cameras and starts the writer.
    pub fn new(
        backend: B,
        paths: Paths,
        published: Published,
        sink_hook: Option<SinkHook>,
        now: Instant,
    ) -> Self {
        let (config, error) = match Config::load(&paths.config) {
            Ok(c) => (c, String::new()),
            Err(e) => {
                let msg = format!("{e}; running on the default configuration");
                error!("{msg}");
                (Config::default(), msg)
            }
        };
        let (runtime, state_damaged) = load_runtime_state(&paths.state);
        let error = match &state_damaged {
            Some(d) => {
                error!("{d}");
                if error.is_empty() {
                    d.clone()
                } else {
                    format!("{d}; {error}")
                }
            }
            None => error,
        };
        info!(
            "config {}, state {} (enabled: {}, camera: {})",
            paths.config.display(),
            paths.state.display(),
            runtime.enabled,
            runtime.camera.as_deref().unwrap_or("not selected")
        );
        let mut c = Controller {
            backend,
            paths,
            config,
            runtime,
            cameras: Vec::new(),
            writer: None,
            writer_error: None,
            writer_retry_at: None,
            rebuild_recount_at: None,
            session: None,
            fault: None,
            error,
            state_damaged,
            consumers: 0,
            present_since: None,
            absent_since: None,
            next_stats: None,
            published,
            sink_hook,
        };
        c.rescan_cameras();
        c.reconcile(now);
        c
    }

    /// Handles one command at `now`. Returns `false` for [`Command::Shutdown`].
    pub fn handle(&mut self, cmd: Command, now: Instant) -> bool {
        match cmd {
            Command::SetEnabled(on, reply) => {
                if !on {
                    self.published.disable_handled();
                }
                let res = self.set_enabled(on, now);
                let _ = reply.send(res);
            }
            Command::SelectCamera(sel, reply) => {
                let res = self.select_camera(&sel, now);
                let _ = reply.send(res);
            }
            Command::NextCamera(reply) => {
                let res = self.next_camera(now);
                let _ = reply.send(res);
            }
            Command::ReloadConfig(reply) => {
                let res = self.reload(now);
                if let Some(r) = reply {
                    let _ = r.send(res);
                }
            }
            Command::Consumers(n) => {
                self.set_consumers(n, now);
                self.reconcile(now);
            }
            Command::Rescan => self.rescan(now),
            Command::Shutdown => return false,
        }
        true
    }

    /// Runs what is due at `now` (timers, polls).
    pub fn tick(&mut self, now: Instant) {
        self.reconcile(now);
    }

    /// How long until the next [`Self::tick`] is due.
    pub fn next_wakeup(&self, now: Instant) -> Duration {
        let mut next = now + IDLE_WAKE;
        let mut at = |t: Instant| next = next.min(t);
        if self.writer.is_none()
            && let Some(t) = self.writer_retry_at
        {
            at(t);
        }
        if self.rebuild_due()
            && let Some(t) = self.rebuild_recount_at
        {
            at(t);
        }
        match &self.session {
            None => {
                // Only a start that can still happen is due: while off, or
                // in error, a consumer present past the debounce must not
                // make every wait zero.
                if self.start_possible()
                    && self.consumers > 0
                    && let Some(t) = self.present_since
                {
                    let due = t + self.config.start_debounce();
                    if due > now {
                        at(due);
                    }
                }
            }
            Some(s) => {
                if self.consumers == 0
                    && let Some(t) = self.absent_since
                {
                    at(t + self.config.idle_grace());
                }
                at(now
                    + if s.awaiting.is_some() {
                        POLL_STARTING
                    } else {
                        POLL_RUNNING
                    });
            }
        }
        if let Some(t) = self.next_stats {
            at(t);
        }
        next.saturating_duration_since(now)
    }

    /// Stops the session and the writer.
    pub fn shutdown(&mut self) {
        info!("shutting down");
        self.drop_session();
        self.backend.stop_writer();
        self.writer = None;
        self.notify_sink(None, Instant::now());
    }

    /// The published snapshot.
    pub fn snapshot(&self) -> Snapshot {
        self.published.snapshot()
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    // ---- derived values ----

    /// Whether a session could start now if a consumer wanted one.
    fn start_possible(&self) -> bool {
        self.runtime.enabled && self.writer.is_some() && self.fault.is_none()
    }

    /// The `Error` text of the causes that still hold (the writer missing,
    /// a fault, a damaged state file), or empty: what clearing an error
    /// whose own cause is gone leaves.
    fn standing_error(&self) -> String {
        if self.writer.is_none()
            && let Some(e) = &self.writer_error
        {
            return e.clone();
        }
        if let Some(f) = &self.fault {
            return f.clone();
        }
        self.state_damaged.clone().unwrap_or_default()
    }

    /// The camera of the running session, found by its node.
    fn session_camera(&self) -> Option<&Camera> {
        let s = self.session.as_ref()?;
        self.cameras.iter().find(|c| c.path == s.node)
    }

    fn state(&self) -> State {
        if !self.runtime.enabled {
            State::Off
        } else if let Some(s) = &self.session {
            if s.live {
                State::Active
            } else {
                State::Starting
            }
        } else if self.writer.is_none() || self.fault.is_some() {
            State::Error
        } else {
            State::Idle
        }
    }

    /// The camera a new session starts on: the runtime selection while it
    /// is present, else the first in order.
    fn chosen_camera(&self) -> Option<&Camera> {
        self.runtime
            .camera
            .as_deref()
            .and_then(|id| self.cameras.iter().find(|c| c.id == id))
            .or_else(|| self.cameras.first())
    }

    /// `ActiveCamera`: the camera that streams, else the one that would.
    fn active_camera(&self) -> String {
        match &self.session {
            Some(s) => s.camera.clone(),
            None => self
                .chosen_camera()
                .map(|c| c.id.clone())
                .unwrap_or_default(),
        }
    }

    fn output_contract(&self) -> OutputContract {
        self.writer
            .as_ref()
            .map_or_else(|| self.config.output_contract(), |w| w.contract)
    }

    fn session_params(&self) -> SessionParams {
        SessionParams {
            contract: self.output_contract(),
            aspect: self.config.output.aspect,
            hold_timeout: self.config.hold_timeout(),
            input_gap: self.config.input_gap(),
            fixed_framerate: self.config.camera.fixed_framerate,
        }
    }

    /// The config asks for another output contract or loopback than the
    /// writer has.
    fn writer_outdated(&self) -> bool {
        self.writer.as_ref().is_some_and(|w| {
            w.contract != self.config.output_contract() || w.sink != self.config.sink_selector()
        })
    }

    /// A writer rebuild for a deferred config change may happen: nobody
    /// counted, no session.
    fn rebuild_due(&self) -> bool {
        self.writer_outdated() && self.consumers == 0 && self.session.is_none()
    }

    fn publish(&mut self) {
        let snap = Snapshot {
            state: self.state(),
            error: self.error.clone(),
            enabled: self.runtime.enabled,
            cameras: self.cameras.iter().map(Camera::info).collect(),
            active_camera: self.active_camera(),
            output_mode: output_mode_string(&self.output_contract()),
            effect: "none".into(),
            consumers: self.consumers,
            interface_revision: INTERFACE_REVISION,
        };
        self.published.update(snap);
    }

    // ---- the step ----

    fn reconcile(&mut self, now: Instant) {
        self.check_writer(now);
        self.check_session(now);
        let had_session = self.session.is_some();
        self.decide(now);
        if !had_session && self.session.is_some() {
            // A camera that delivers at once is active in this step.
            self.check_session(now);
        } else if had_session && self.session.is_none() {
            // A deferred writer rebuild may apply now.
            self.check_writer(now);
        }
        self.publish();
        self.maybe_stats(now);
    }

    fn check_writer(&mut self, now: Instant) {
        if self.writer.is_some() {
            if let Some(f) = self.backend.writer_failure() {
                error!("{f}");
                self.drop_session();
                self.backend.stop_writer();
                self.writer = None;
                self.notify_sink(None, now);
                self.writer_error = Some(f.clone());
                self.error = f;
                self.writer_retry_at = Some(now + WRITER_RETRY);
                self.publish();
            } else if self.rebuild_due() {
                self.rebuild_writer(now);
            }
        }
        if self.writer.is_none() && self.writer_retry_at.is_none_or(|t| now >= t) {
            self.start_writer(now);
        }
    }

    /// Restarts the writer with the new output contract or loopback, unless
    /// a process holds the device after all: the watcher's count lags a
    /// fresh open (inotify, then a `/proc` scan), and a rebuild under an
    /// application would change the format it negotiated.
    fn rebuild_writer(&mut self, now: Instant) {
        let Some(node) = self.writer.as_ref().map(|w| w.node.clone()) else {
            return;
        };
        let held = self.backend.loopback_holders(&node);
        if held > 0 {
            if self.rebuild_recount_at.is_none() {
                info!(
                    "{} is held by {held} process(es): the new output or loopback settings \
                     wait until it is released",
                    node.display()
                );
            }
            self.rebuild_recount_at = Some(now + WRITER_RETRY);
            return;
        }
        self.rebuild_recount_at = None;
        info!("applying the new output or loopback configuration: restarting the writer");
        self.backend.stop_writer();
        self.writer = None;
        self.notify_sink(None, now);
        self.writer_retry_at = None;
    }

    fn start_writer(&mut self, now: Instant) {
        let sink = self.config.sink_selector();
        let contract = self.config.output_contract();
        match self.backend.start_writer(&sink, &contract) {
            Ok(node) => {
                info!(
                    "virtual camera {} ({}) is ready",
                    node.display(),
                    output_mode_string(&contract)
                );
                if let Some(old) = self.writer_error.take()
                    && self.error == old
                {
                    self.error = self.standing_error();
                }
                self.writer_retry_at = None;
                self.writer = Some(WriterOn {
                    node: node.clone(),
                    contract,
                    sink,
                });
                self.notify_sink(Some(&node), now);
            }
            Err(e) => {
                if self.writer_error.as_deref() != Some(e.as_str()) {
                    error!("{e} (retrying every {} s)", WRITER_RETRY.as_secs());
                }
                self.writer_error = Some(e.clone());
                self.error = e;
                self.writer_retry_at = Some(now + WRITER_RETRY);
            }
        }
    }

    fn notify_sink(&mut self, node: Option<&Path>, now: Instant) {
        self.set_consumers(0, now);
        if let Some(hook) = self.sink_hook.as_mut() {
            hook(node);
        }
    }

    fn check_session(&mut self, now: Instant) {
        if self.session.is_none() {
            return;
        }
        match self.backend.session_failure() {
            Some(Failure::Output(e)) => {
                self.drop_session();
                self.set_fault(e);
                return;
            }
            Some(Failure::Camera(e)) => {
                self.camera_failed(e, now);
                return;
            }
            None => {}
        }
        if self.session_camera().is_none() {
            let id = self.session.as_ref().map(|s| s.camera.clone());
            let msg = format!("camera {} disappeared", id.unwrap_or_default());
            self.camera_failed(msg, now);
            return;
        }
        let Some(s) = &self.session else { return };
        if let Some(since) = s.awaiting {
            if self.backend.first_frame() {
                let first = !s.live;
                if let Some(s) = self.session.as_mut() {
                    s.live = true;
                    s.awaiting = None;
                    // A camera delivered: every camera may be tried again.
                    s.failed.clear();
                }
                if first {
                    info!("streaming from {}", self.session_label());
                    self.error = self.standing_error();
                    // The first window starts at the first frame.
                    let _ = self.backend.take_stats();
                    self.next_stats = Some(now + STATS_PERIOD);
                }
            } else if now.saturating_duration_since(since) >= START_TIMEOUT {
                let msg = format!(
                    "{} delivered no frame within {} s",
                    self.session_label(),
                    START_TIMEOUT.as_secs()
                );
                self.camera_failed(msg, now);
            }
        }
    }

    fn session_label(&self) -> String {
        let Some(s) = &self.session else {
            return String::new();
        };
        self.session_camera()
            .map_or_else(|| s.camera.clone(), Camera::label)
    }

    /// Records the session's current camera as failed (once).
    fn mark_failed(&mut self) {
        let label = self.session_label();
        if let Some(s) = self.session.as_mut()
            && !s.failed.iter().any(|(n, _)| *n == s.node)
        {
            s.failed.push((s.node.clone(), label));
        }
    }

    /// The session's camera failed: the next camera in order that has not
    /// failed yet (since a camera last delivered) takes over, with the
    /// hold; with none, the session stops and the state is `error`.
    fn camera_failed(&mut self, reason: String, now: Instant) {
        if self.session.is_none() {
            return;
        }
        warning!("{reason}");
        self.mark_failed();
        let Some(s) = &self.session else { return };
        let failed_node = s.node.clone();
        let tried: Vec<PathBuf> = s.failed.iter().map(|(n, _)| n.clone()).collect();
        let start = self
            .cameras
            .iter()
            .position(|c| c.path == failed_node)
            .map_or(0, |i| i + 1);
        let n = self.cameras.len();
        let candidates: Vec<Camera> = (0..n)
            .map(|k| self.cameras[(start + k) % n].clone())
            .filter(|c| !tried.contains(&c.path))
            .collect();
        match self.try_cameras(&candidates, now) {
            Ok(()) => {}
            Err(last) => {
                let failed: Vec<String> = self
                    .session
                    .as_ref()
                    .map(|s| s.failed.iter().map(|(_, l)| l.clone()).collect())
                    .unwrap_or_default();
                self.drop_session();
                let mut msg = match last {
                    Some(e) => format!("{reason}; no other camera could take over ({e})"),
                    None => format!("{reason}; no other camera is available"),
                };
                if failed.len() > 1 {
                    msg.push_str(&format!("; every camera failed: {}", failed.join(", ")));
                }
                self.set_fault(msg);
            }
        }
    }

    /// Starts the ingest on the first of `candidates` that starts. `Err`
    /// carries the last error (`None`: no candidate).
    fn try_cameras(&mut self, candidates: &[Camera], now: Instant) -> Result<(), Option<String>> {
        let params = self.session_params();
        let mut last = None;
        for cam in candidates {
            if let Some(s) = self.session.as_mut() {
                s.camera = cam.id.clone();
                s.node = cam.path.clone();
                s.awaiting = Some(now);
            }
            match self.backend.switch_camera(&params, cam) {
                Ok(()) => {
                    info!(
                        "camera {} ({} {}x{}@{})",
                        cam.label(),
                        cam.mode.format,
                        cam.mode.width,
                        cam.mode.height,
                        cam.mode.fps()
                    );
                    return Ok(());
                }
                Err(e) => {
                    warning!("{e}");
                    self.mark_failed();
                    last = Some(e);
                }
            }
        }
        Err(last)
    }

    fn decide(&mut self, now: Instant) {
        if self.consumers == 0 && self.session.is_none() && self.fault.take().is_some() {
            debug!("no consumer: leaving the error state");
        }
        let wanted = self.start_possible()
            && match &self.session {
                Some(_) => {
                    self.consumers > 0
                        || self
                            .absent_since
                            .is_none_or(|t| now < t + self.config.idle_grace())
                }
                None => {
                    self.consumers > 0
                        && self
                            .present_since
                            .is_some_and(|t| now >= t + self.config.start_debounce())
                }
            };
        if wanted && self.session.is_none() {
            self.start_session(now);
        } else if !wanted && self.session.is_some() {
            info!("stopping the camera");
            let t = Instant::now();
            self.drop_session();
            debug!(
                "camera and GPU pipeline stopped in {} ms",
                t.elapsed().as_millis()
            );
        }
    }

    fn start_session(&mut self, now: Instant) {
        let mut candidates: Vec<Camera> = Vec::with_capacity(self.cameras.len());
        if let Some(c) = self.chosen_camera() {
            candidates.push(c.clone());
        }
        for c in &self.cameras {
            if !candidates.iter().any(|x| x.id == c.id) {
                candidates.push(c.clone());
            }
        }
        if self.published.disable_pending() {
            debug!("SetEnabled(false) is queued: not starting the camera");
            return;
        }
        let Some(first) = candidates.first() else {
            self.set_fault("no camera is available".into());
            return;
        };
        info!("an application opened the virtual camera: starting");
        self.session = Some(Streaming {
            camera: first.id.clone(),
            node: first.path.clone(),
            failed: Vec::new(),
            live: false,
            awaiting: Some(now),
        });
        // `starting` is announced before the (slow) start.
        self.publish();
        if let Err(e) = self.backend.start_session(&self.session_params()) {
            self.drop_session();
            self.set_fault(e);
            return;
        }
        // Turned off during the (slow) output build: no camera is opened;
        // the queued SetEnabled(false) is handled next.
        if self.published.disable_pending() {
            info!("disabled while starting: the camera is not opened");
            self.drop_session();
            return;
        }
        if let Err(last) = self.try_cameras(&candidates, now) {
            self.drop_session();
            self.set_fault(last.unwrap_or_else(|| "no camera is available".into()));
        }
    }

    fn drop_session(&mut self) {
        if self.session.take().is_some() {
            self.backend.stop_session();
        }
        self.next_stats = None;
    }

    fn set_fault(&mut self, msg: String) {
        error!("{msg}");
        self.error = msg.clone();
        self.fault = Some(msg);
    }

    fn maybe_stats(&mut self, now: Instant) {
        if self.state() != State::Active {
            self.next_stats = None;
            return;
        }
        let due = *self.next_stats.get_or_insert(now + STATS_PERIOD);
        if now >= due {
            let stats = self.backend.take_stats();
            self.published.stats(stats);
            self.next_stats = Some(now + STATS_PERIOD);
        }
    }

    fn set_consumers(&mut self, n: u32, now: Instant) {
        if n == self.consumers {
            return;
        }
        debug!("consumers: {n}");
        if n > 0 && self.consumers == 0 {
            self.present_since = Some(now);
            self.absent_since = None;
        } else if n == 0 {
            self.present_since = None;
            self.absent_since = Some(now);
        }
        self.consumers = n;
    }

    /// Scans the cameras; returns the previous list. A camera whose id
    /// changed between the two scans (same node) is renamed in the session
    /// and in the runtime selection, not lost.
    fn rescan_cameras(&mut self) -> Vec<Camera> {
        let found = self.backend.scan_cameras(&self.config.camera_mode());
        let old = std::mem::replace(
            &mut self.cameras,
            camera::order(found, &self.config.camera_preference()),
        );
        self.follow_renames(&old);
        debug!(
            "cameras: {}",
            self.cameras
                .iter()
                .map(Camera::label)
                .collect::<Vec<_>>()
                .join(", ")
        );
        old
    }

    /// Follows id changes of cameras still present at the same node.
    fn follow_renames(&mut self, old: &[Camera]) {
        let renamed = |id: &str| -> Option<String> {
            let was = old.iter().find(|c| c.id == id)?;
            let now = self.cameras.iter().find(|c| c.path == was.path)?;
            (now.id != id).then(|| now.id.clone())
        };
        let session_rename = self.session.as_ref().and_then(|s| {
            let now = self.cameras.iter().find(|c| c.path == s.node)?;
            (now.id != s.camera).then(|| now.id.clone())
        });
        let selection_rename = self.runtime.camera.as_deref().and_then(renamed);
        if let Some(new) = session_rename
            && let Some(s) = self.session.as_mut()
        {
            info!(
                "camera {} is now known as {new}",
                std::mem::replace(&mut s.camera, new.clone())
            );
        }
        if let Some(new) = selection_rename {
            info!(
                "the selected camera {} is now known as {new}",
                self.runtime.camera.as_deref().unwrap_or_default()
            );
            self.runtime.camera = Some(new);
            let _ = self.save_state();
        }
    }

    fn rescan(&mut self, now: Instant) {
        let before: Vec<PathBuf> = self.cameras.iter().map(|c| c.path.clone()).collect();
        self.rescan_cameras();
        let after: Vec<PathBuf> = self.cameras.iter().map(|c| c.path.clone()).collect();
        if before != after {
            info!(
                "camera set changed: {}",
                if self.cameras.is_empty() {
                    "none".to_string()
                } else {
                    self.cameras
                        .iter()
                        .map(|c| c.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
            self.fault = None;
            if let Some(s) = self.session.as_mut() {
                s.failed.clear();
            }
        }
        self.reconcile(now);
    }

    fn save_state(&self) -> Result<(), IpcError> {
        if let Some(d) = &self.state_damaged {
            let msg =
                format!("{d}; the change is in force but is not saved before SetEnabled is called");
            warning!("{msg}");
            return Err(IpcError::Failed(msg));
        }
        self.runtime.save(&self.paths.state).map_err(|e| {
            let msg = format!(
                "{}: {e}; the change is in force but will not survive a restart",
                self.paths.state.display()
            );
            error!("{msg}");
            IpcError::Failed(msg)
        })
    }

    // ---- methods ----

    fn set_enabled(&mut self, on: bool, now: Instant) -> Result<(), IpcError> {
        info!("{}", if on { "enabled" } else { "disabled (off)" });
        self.runtime.enabled = on;
        self.fault = None;
        // The camera is released before anything is written to disk.
        if !on {
            self.drop_session();
        }
        // An explicit choice: the state file is rewritten even if it was
        // damaged.
        let damaged = self.state_damaged.take();
        let saved = self.save_state();
        if let Some(d) = damaged
            && self.error == d
        {
            self.error = self.standing_error();
        }
        self.reconcile(now);
        saved
    }

    fn select_camera(&mut self, selector: &str, now: Instant) -> Result<(), IpcError> {
        let cam = camera::find(&self.cameras, selector)
            .map_err(IpcError::NoSuchCamera)?
            .clone();
        self.select(cam, now)
    }

    fn next_camera(&mut self, now: Instant) -> Result<(), IpcError> {
        if self.cameras.is_empty() {
            return Err(IpcError::Unavailable("no camera is present".into()));
        }
        let current = self.active_camera();
        let next = match self.cameras.iter().position(|c| c.id == current) {
            Some(i) => (i + 1) % self.cameras.len(),
            None => 0,
        };
        let cam = self.cameras[next].clone();
        self.select(cam, now)
    }

    fn select(&mut self, cam: Camera, now: Instant) -> Result<(), IpcError> {
        info!("camera selected: {}", cam.label());
        self.runtime.camera = Some(cam.id.clone());
        let mut res = self.save_state();
        self.fault = None;
        if let Some(s) = self.session.as_mut() {
            // The user's pick: every camera may be tried again.
            s.failed.clear();
        }
        if self.session.as_ref().is_some_and(|s| s.node != cam.path) {
            let params = self.session_params();
            if let Some(s) = self.session.as_mut() {
                s.camera = cam.id.clone();
                s.node = cam.path.clone();
                s.awaiting = Some(now);
            }
            info!("switching to {}", cam.label());
            if let Err(e) = self.backend.switch_camera(&params, &cam) {
                let msg = format!("switching to {}: {e}", cam.label());
                self.camera_failed(msg.clone(), now);
                res = Err(IpcError::Failed(msg));
            }
        }
        self.reconcile(now);
        res
    }

    fn reload(&mut self, now: Instant) -> Result<(), IpcError> {
        let res = match Config::load(&self.paths.config) {
            Ok(new) => {
                self.apply_config(new, now);
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                warning!("configuration rejected, the previous one stays in force: {msg}");
                self.error = msg.clone();
                Err(IpcError::InvalidConfig(msg))
            }
        };
        self.reconcile(now);
        res
    }

    fn apply_config(&mut self, new: Config, now: Instant) {
        let old = std::mem::replace(&mut self.config, new);
        info!(
            "configuration {} reloaded{}",
            self.paths.config.display(),
            if old == self.config {
                " (unchanged)"
            } else {
                ""
            }
        );
        self.fault = None;
        // Only the errors whose cause is gone are cleared: a missing
        // writer or a damaged state file still stands.
        self.error = self.standing_error();
        let old_cameras = self.rescan_cameras();
        // The streaming camera's mode changed: reopen it in the new mode.
        if let Some(s) = &self.session {
            let mode = |list: &[Camera]| list.iter().find(|c| c.path == s.node).map(|c| c.mode);
            if let (Some(a), Some(b)) = (mode(&old_cameras), mode(&self.cameras))
                && a != b
                && let Some(cam) = self.session_camera().cloned()
            {
                info!("camera mode changed: reopening {}", cam.label());
                let params = self.session_params();
                if let Some(s) = self.session.as_mut() {
                    s.awaiting = Some(now);
                }
                if let Err(e) = self.backend.switch_camera(&params, &cam) {
                    self.camera_failed(format!("reopening {}: {e}", cam.label()), now);
                }
            }
        }
        if self.writer_outdated() && (self.consumers > 0 || self.session.is_some()) {
            info!(
                "the new output or loopback settings apply once no application holds the virtual camera"
            );
        }
    }
}

/// Runs `ctrl` until [`Command::Shutdown`] or until every sender is gone,
/// then shuts it down.
pub fn run<B: MediaBackend>(mut ctrl: Controller<B>, commands: Receiver<Command>) {
    loop {
        let wait = ctrl.next_wakeup(Instant::now());
        match commands.recv_timeout(wait) {
            Ok(cmd) => {
                if !ctrl.handle(cmd, Instant::now()) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => ctrl.tick(Instant::now()),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    ctrl.shutdown();
}

#[cfg(test)]
mod tests;
