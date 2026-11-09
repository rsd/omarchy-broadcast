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
//! file until `SetEnabled` is called. That includes a state file with an
//! effect value this daemon does not accept: it never streams the raw
//! image in place of an effect it could not read.
//!
//! The effect (`docs/adr/0012-video-effects-and-maxine-backend.md`): the
//! effective settings are the state file's (set by `SetEffect`,
//! `SetBlurStrength`, `SetBackgroundImage`) over the config's `[effect]`.
//! A change applies to the running session without restarting it. When the
//! effect cannot run, `effect.on_failure` decides: `black` is a fault (the
//! session stops, so the camera is closed and the GPU released, and the
//! writer's placeholder keeps the stream black); `none` keeps streaming
//! without the effect, with the reason in `Error`. An effect that is
//! unavailable before the session starts is found out before the camera
//! is opened.
//!
//! Consumers are of two kinds: processes holding the loopback (the
//! consumer watcher's count) and, with `[pipewire] enabled`, links to the
//! PipeWire output (its [`PipeWireEvent`]s). The lifecycle (debounce, grace,
//! a deferred writer rebuild) follows their total, `Consumers`; either kind
//! keeps the session running. The PipeWire output runs next to the writer
//! but has its own lifecycle, so `[pipewire] enabled` never disturbs the
//! loopback device: a reload that enables it starts the output at once; one
//! that disables it stops the output once it has no link (at once if it
//! has none; loopback consumers do not matter); neither touches the writer
//! or the session. A writer rebuild (a new output mode or loopback device,
//! deferred until no consumer of either kind is left and no session runs)
//! rebuilds the PipeWire output too, with the new contract. Its failure is
//! not a lifecycle fault: the loopback keeps working, so it leaves `State`
//! and `Error` alone (it shows in `PipeWireOutput` and the journal). Every
//! PipeWire output the backend starts has its own generation, carried by
//! each of its events: only the current output's events are applied, so
//! those an output stopped or replaced left in the command queue change
//! nothing, in whatever order they arrive.
//!
//! The streaming camera is followed by its device node, not by its id: an
//! id can change between two scans of the same camera (udev adds the
//! `/dev/v4l` links after the node), and such a change is a rename, not a
//! loss. A node can also be reused by another camera (one unplugged and
//! another plugged in between two scans): that is a loss, and the
//! selection stays on the camera that left ([`camera::same_camera`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use obc_core::config::{Config, EffectConfig, RuntimeState};
use obc_core::{EffectMode, EffectOnFailure, OutputContract};
use obc_ipc::{CameraInfo, INTERFACE_REVISION, IpcError, Snapshot, State, Stats};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::backend::{
    EffectReport, EffectStatus, Failure, MediaBackend, PipeWireEvent, PipeWireGeneration,
    PipeWireState, SessionParams,
};
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
/// How often the effect's GPU time is logged (debug) while it runs.
const EFFECT_LOG_PERIOD: Duration = Duration::from_secs(60);
/// The largest background image `SetBackgroundImage` accepts (the effect
/// element's limit).
pub const MAX_BACKGROUND_BYTES: u64 = 32 * 1024 * 1024;

/// The reply channel of a D-Bus method.
pub type Reply = tokio::sync::oneshot::Sender<Result<(), IpcError>>;

/// What the controller is asked to do.
#[derive(Debug)]
pub enum Command {
    SetEnabled(bool, Reply),
    SelectCamera(String, Reply),
    NextCamera(Reply),
    /// `SetEffect(s)`.
    SetEffect(String, Reply),
    /// `SetBlurStrength(d)`.
    SetBlurStrength(f64, Reply),
    /// `SetBackgroundImage(s)` (empty: clear it).
    SetBackgroundImage(String, Reply),
    /// `ReloadConfig()` (with its reply) or SIGHUP (without).
    ReloadConfig(Option<Reply>),
    /// The consumer watcher's count of processes holding the loopback.
    Consumers(u32),
    /// An event of the PipeWire output of that generation (through the
    /// backend's reporter).
    PipeWire(PipeWireGeneration, PipeWireEvent),
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
    Effect(String),
    BlurStrength(f64),
    BackgroundImage(String),
    Consumers(u32),
    LoopbackConsumers(u32),
    PipeWireConsumers(u32),
    PipeWireOutput(String),
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
            Change::Effect(_) => "Effect",
            Change::BlurStrength(_) => "BlurStrength",
            Change::BackgroundImage(_) => "BackgroundImage",
            Change::Consumers(_) => "Consumers",
            Change::LoopbackConsumers(_) => "LoopbackConsumers",
            Change::PipeWireConsumers(_) => "PipeWireConsumers",
            Change::PipeWireOutput(_) => "PipeWireOutput",
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
    if old.effect != new.effect {
        out.push(Change::Effect(new.effect.clone()));
    }
    if old.blur_strength.to_bits() != new.blur_strength.to_bits() {
        out.push(Change::BlurStrength(new.blur_strength));
    }
    if old.background_image != new.background_image {
        out.push(Change::BackgroundImage(new.background_image.clone()));
    }
    if old.consumers != new.consumers {
        out.push(Change::Consumers(new.consumers));
    }
    if old.loopback_consumers != new.loopback_consumers {
        out.push(Change::LoopbackConsumers(new.loopback_consumers));
    }
    if old.pipewire_consumers != new.pipewire_consumers {
        out.push(Change::PipeWireConsumers(new.pipewire_consumers));
    }
    if old.pipewire_output != new.pipewire_output {
        out.push(Change::PipeWireOutput(new.pipewire_output.clone()));
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
/// (including one with an effect value that is not accepted) fails closed:
/// not enabled, no selection, no effect override (the config's effect is
/// the one in force, and `SetEnabled` rewrites the file without the saved
/// effect choice), and the second value is the `Error` text naming the file
/// and saying so (`None`: the file is fine or absent).
pub fn load_runtime_state(path: &Path) -> (RuntimeState, Option<String>) {
    match RuntimeState::load(path) {
        (state, None) => (state, None),
        (_, Some(warn)) => {
            let why = warn.trim_end_matches("; using the default state");
            (
                RuntimeState {
                    enabled: false,
                    ..RuntimeState::default()
                },
                Some(format!(
                    "{why}; the virtual camera stays off until SetEnabled is called, which \
                     rewrites the state file; any effect choice saved there is dropped in favour \
                     of the configuration's effect"
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
    let effect = runtime.effect_over(&config.effect);
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
        effect: effect.mode.as_str().into(),
        blur_strength: effect.blur_strength,
        background_image: image_string(&effect),
        consumers: 0,
        loopback_consumers: 0,
        pipewire_consumers: 0,
        pipewire_output: PipeWireState::Off.as_str().into(),
        interface_revision: INTERFACE_REVISION,
    }
}

/// `BackgroundImage`: the path, or empty.
fn image_string(effect: &EffectConfig) -> String {
    effect
        .background_image
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Checks a background image for `SetBackgroundImage`: an absolute path to
/// a readable regular file of at most [`MAX_BACKGROUND_BYTES`]. Whether it
/// decodes is found out when the effect loads it.
///
/// The file is opened without blocking (`O_NONBLOCK`: a FIFO, or a device,
/// put in its place cannot hang the controller's thread; `O_NOCTTY`; std
/// adds `O_CLOEXEC`) and then examined through that handle, so what is
/// checked is what was opened, not what the path names a moment later.
pub fn check_background_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    if !path.is_absolute() {
        return Err(format!(
            "the background image path {} is not absolute",
            path.display()
        ));
    }
    let unreadable =
        |e: std::io::Error| format!("the background image {} is unreadable: {e}", path.display());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() {
        return Err(format!(
            "the background image {} is not a regular file",
            path.display()
        ));
    }
    if meta.len() > MAX_BACKGROUND_BYTES {
        return Err(format!(
            "the background image {} is {} bytes, more than the {} MiB limit",
            path.display(),
            meta.len(),
            MAX_BACKGROUND_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

/// The `Error` text of an effect that cannot run.
fn effect_failure_text(effect: &EffectConfig, reason: &str) -> String {
    match effect.on_failure {
        EffectOnFailure::Black => format!("effect {} unavailable: {reason}", effect.mode),
        EffectOnFailure::None => format!(
            "effect {} unavailable: {reason}; the camera image is shown without the effect \
             (effect.on_failure = \"none\")",
            effect.mode
        ),
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
    /// The generation of the PipeWire output running next to it (`None`:
    /// none runs).
    pipewire: Option<PipeWireGeneration>,
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
    /// The effect cannot run and `effect.on_failure` is `none`: the
    /// session streams without it, and this is the `Error` text. Cleared
    /// when the effect becomes ready, by an effect method, a reload, the
    /// next session start or the end of the session.
    effect_failure: Option<String>,
    /// The effect status last polled in this session (`None`: not polled
    /// since the session started or the effect was last applied, so the
    /// next status is reported even if it is the same failure again).
    effect_seen: Option<EffectStatus>,
    /// The effect has been polled in this session.
    effect_polled: bool,
    /// The effect settings the running session was last given (`None`: no
    /// session). Only a change of the effect or its image, or a failure,
    /// starts a new load to follow; a strength change does not.
    effect_applied: Option<EffectConfig>,
    /// When the effect's GPU time is logged next.
    effect_log_at: Option<Instant>,
    /// Every consumer: `loopback_consumers + pipewire_consumers`, what the
    /// lifecycle follows.
    consumers: u32,
    /// The consumer watcher's count of processes holding the loopback.
    loopback_consumers: u32,
    /// The links to the PipeWire output (0 without one).
    pipewire_consumers: u32,
    /// The PipeWire output's state (`Off` without one).
    pipewire_state: PipeWireState,
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
            "config {}, state {} (enabled: {}, camera: {}, effect: {})",
            paths.config.display(),
            paths.state.display(),
            runtime.enabled,
            runtime.camera.as_deref().unwrap_or("not selected"),
            runtime.effect_over(&config.effect).mode
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
            effect_failure: None,
            effect_seen: None,
            effect_polled: false,
            effect_applied: None,
            effect_log_at: None,
            consumers: 0,
            loopback_consumers: 0,
            pipewire_consumers: 0,
            pipewire_state: PipeWireState::Off,
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
            Command::SetEffect(name, reply) => {
                let res = self.set_effect(&name, now);
                let _ = reply.send(res);
            }
            Command::SetBlurStrength(v, reply) => {
                let res = self.set_blur_strength(v, now);
                let _ = reply.send(res);
            }
            Command::SetBackgroundImage(path, reply) => {
                let res = self.set_background_image(&path, now);
                let _ = reply.send(res);
            }
            Command::ReloadConfig(reply) => {
                let res = self.reload(now);
                if let Some(r) = reply {
                    let _ = r.send(res);
                }
            }
            Command::Consumers(n) => {
                self.set_consumers(n, self.pipewire_consumers, now);
                self.reconcile(now);
            }
            Command::PipeWire(generation, event) => {
                self.pipewire_event(generation, event, now);
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
        if let Some(e) = &self.effect_failure {
            return e.clone();
        }
        self.state_damaged.clone().unwrap_or_default()
    }

    /// The effect settings in force: the state file's over the config's.
    pub fn effect(&self) -> EffectConfig {
        self.runtime.effect_over(&self.config.effect)
    }

    /// The camera of the running session, found by its node (and its id,
    /// which [`Self::follow_renames`] keeps current).
    fn session_camera(&self) -> Option<&Camera> {
        let s = self.session.as_ref()?;
        self.cameras
            .iter()
            .find(|c| c.path == s.node && c.id == s.camera)
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
            effect: self.effect(),
        }
    }

    /// The config asks for another output contract or loopback device than
    /// the writer has. The PipeWire setting is not the writer's
    /// ([`Self::sync_pipewire`]).
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
        let effect = self.effect();
        let snap = Snapshot {
            state: self.state(),
            error: self.error.clone(),
            enabled: self.runtime.enabled,
            cameras: self.cameras.iter().map(Camera::info).collect(),
            active_camera: self.active_camera(),
            output_mode: output_mode_string(&self.output_contract()),
            effect: effect.mode.as_str().into(),
            blur_strength: effect.blur_strength,
            background_image: image_string(&effect),
            consumers: self.consumers,
            loopback_consumers: self.loopback_consumers,
            pipewire_consumers: self.pipewire_consumers,
            pipewire_output: self.pipewire_state.as_str().into(),
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
        self.sync_pipewire(now);
    }

    /// Starts or stops the PipeWire output next to the running writer when
    /// `[pipewire] enabled` asks for it, without touching the writer or
    /// the session. A stop waits until the output has no link left (the
    /// loopback's consumers do not matter); it applies at the step that
    /// counts the last link gone. A stopped output is `off` with no link.
    fn sync_pipewire(&mut self, now: Instant) {
        let Some(running) = self.writer.as_ref().map(|w| w.pipewire.is_some()) else {
            return;
        };
        let wanted = self.config.pipewire.enabled;
        let pipewire = if wanted && !running {
            info!("starting the PipeWire output next to the virtual camera");
            Some(self.backend.start_pipewire())
        } else if !wanted && running && self.pipewire_consumers == 0 {
            info!("stopping the PipeWire output; the virtual camera keeps running");
            self.backend.stop_pipewire();
            None
        } else {
            return;
        };
        if let Some(w) = self.writer.as_mut() {
            w.pipewire = pipewire;
        }
        self.pipewire_state = if pipewire.is_some() {
            PipeWireState::Connecting
        } else {
            PipeWireState::Off
        };
        self.set_consumers(self.loopback_consumers, 0, now);
    }

    /// Restarts the writer (and the PipeWire output, if one is configured)
    /// with the new output contract or loopback device, unless a process
    /// holds the device after all: the watcher's count lags a fresh open
    /// (inotify, then a `/proc` scan), and a rebuild under an application
    /// would change the format it negotiated.
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
                let pipewire = self
                    .config
                    .pipewire
                    .enabled
                    .then(|| self.backend.start_pipewire());
                info!(
                    "virtual camera {} ({}) is ready{}",
                    node.display(),
                    output_mode_string(&contract),
                    if pipewire.is_some() {
                        ", with the PipeWire output"
                    } else {
                        ""
                    }
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
                    pipewire,
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

    /// The writer started or stopped: nobody is counted on the new one yet
    /// (the watcher counts the loopback's holders again, a new PipeWire
    /// output reports its links), and the PipeWire output is `connecting`
    /// if the new writer has one, else `off`.
    fn notify_sink(&mut self, node: Option<&Path>, now: Instant) {
        self.set_consumers(0, 0, now);
        self.pipewire_state = if self.writer.as_ref().is_some_and(|w| w.pipewire.is_some()) {
            PipeWireState::Connecting
        } else {
            PipeWireState::Off
        };
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
            warning!("{msg}");
            // Not marked failed: it is no candidate any more, and its node
            // may already belong to another camera.
            self.fail_over(msg, now);
            return;
        }
        if self.poll_effect(now) {
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

    /// Follows the running session's effect: logs its transitions and acts
    /// on a failure ([`Self::effect_failed`]). Returns whether the session
    /// was stopped.
    fn poll_effect(&mut self, now: Instant) -> bool {
        let Some(EffectReport {
            status,
            load_ms,
            reused,
            gpu_ms_p50,
        }) = self.backend.effect_report()
        else {
            return false;
        };
        let mode = self.effect().mode;
        if status == EffectStatus::Ready
            && let Some(gpu) = gpu_ms_p50
            && self.effect_log_at.is_none_or(|t| now >= t)
        {
            if self.effect_log_at.is_some() {
                debug!("effect {mode}: {gpu:.2} ms of GPU time per frame (median)");
            }
            self.effect_log_at = Some(now + EFFECT_LOG_PERIOD);
        }
        if self.effect_seen.as_ref() == Some(&status) {
            return false;
        }
        self.effect_seen = Some(status.clone());
        let first = !std::mem::replace(&mut self.effect_polled, true);
        match status {
            EffectStatus::Off => {
                if !first {
                    info!("effect off: the camera image passes through");
                }
            }
            EffectStatus::Loading => info!("effect {mode}: loading"),
            EffectStatus::Ready => {
                let took = match (load_ms, reused) {
                    (Some(ms), true) => format!(" (models reused, {ms:.0} ms)"),
                    (Some(ms), false) => format!(" (loaded in {ms:.0} ms)"),
                    (None, _) => String::new(),
                };
                info!("effect {mode} ready{took}");
                if let Some(old) = self.effect_failure.take()
                    && self.error == old
                {
                    self.error = self.standing_error();
                }
            }
            EffectStatus::Failed(reason) => return self.effect_failed(&reason),
        }
        false
    }

    /// The running session's effect cannot run, for `reason`. With
    /// `on_failure = "black"` the session stops and the state is `error`;
    /// with `"none"` it keeps streaming without the effect and `Error` says
    /// why. Returns whether the session was stopped.
    fn effect_failed(&mut self, reason: &str) -> bool {
        let effect = self.effect();
        let msg = effect_failure_text(&effect, reason);
        match effect.on_failure {
            EffectOnFailure::Black => {
                info!("stopping the camera: the effect cannot run");
                self.drop_session();
                self.set_fault(msg);
                true
            }
            EffectOnFailure::None => {
                warning!("{msg}");
                self.error = msg.clone();
                self.effect_failure = Some(msg);
                false
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
        self.fail_over(reason, now);
    }

    /// The session's camera stopped (for `reason`): the next camera in
    /// order that has not failed takes over; with none, the session stops
    /// and the state is `error`.
    fn fail_over(&mut self, reason: String, now: Instant) {
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
        // The effect is checked before the camera is opened: with
        // `on_failure = "black"`, one that cannot run is a fault and the
        // camera stays closed.
        let mut params = self.session_params();
        self.effect_failure = None;
        if params.effect.mode != EffectMode::None
            && let Err(reason) = self.effect_check(&params.effect)
        {
            let msg = effect_failure_text(&params.effect, &reason);
            match params.effect.on_failure {
                EffectOnFailure::Black => {
                    self.set_fault(msg);
                    return;
                }
                EffectOnFailure::None => {
                    warning!("{msg}");
                    self.error = msg.clone();
                    self.effect_failure = Some(msg);
                    params.effect.mode = EffectMode::None;
                }
            }
        }
        let first = first.clone();
        self.effect_applied = Some(params.effect.clone());
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
        if let Err(e) = self.backend.start_session(&params) {
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

    /// Whether `effect` can run: `replace` has an image, and the backend
    /// says the effect is available.
    fn effect_check(&mut self, effect: &EffectConfig) -> Result<(), String> {
        if effect.mode == EffectMode::Replace && effect.background_image.is_none() {
            return Err("the replace effect needs a background image".into());
        }
        self.backend.effect_available(effect)
    }

    /// Stops the session. An effect failure of that session (with
    /// `on_failure = "none"`) ends with it: its `Error` gives way to the
    /// causes still standing.
    fn drop_session(&mut self) {
        if self.session.take().is_some() {
            self.backend.stop_session();
        }
        self.next_stats = None;
        self.effect_seen = None;
        self.effect_polled = false;
        self.effect_applied = None;
        self.effect_log_at = None;
        if let Some(e) = self.effect_failure.take()
            && self.error == e
        {
            self.error = self.standing_error();
        }
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

    /// Sets the two counts; the lifecycle follows their total.
    fn set_consumers(&mut self, loopback: u32, pipewire: u32, now: Instant) {
        self.loopback_consumers = loopback;
        self.pipewire_consumers = pipewire;
        let n = loopback.saturating_add(pipewire);
        if n == self.consumers {
            return;
        }
        debug!("consumers: {n} (loopback {loopback}, PipeWire {pipewire})");
        if n > 0 && self.consumers == 0 {
            self.present_since = Some(now);
            self.absent_since = None;
        } else if n == 0 {
            self.present_since = None;
            self.absent_since = Some(now);
        }
        self.consumers = n;
    }

    /// An event of the PipeWire output of `generation`. Ignored unless that
    /// output is the one running next to the writer (an event of an output
    /// already stopped or replaced, whenever it arrives). A failure, or a
    /// build without PipeWire support, is logged and shown in
    /// `PipeWireOutput` only: the loopback is unaffected, so it is no fault
    /// and `Error` stays as it is. Such an output has no links.
    fn pipewire_event(
        &mut self,
        generation: PipeWireGeneration,
        event: PipeWireEvent,
        now: Instant,
    ) {
        if self.writer.as_ref().and_then(|w| w.pipewire) != Some(generation) {
            debug!(
                "event of PipeWire output {generation}, which no longer runs, ignored: {event:?}"
            );
            return;
        }
        match event {
            PipeWireEvent::Consumers(n) => self.set_consumers(self.loopback_consumers, n, now),
            PipeWireEvent::State(state) => {
                if state == self.pipewire_state {
                    return;
                }
                match &state {
                    PipeWireState::Failed(reason) => warning!(
                        "PipeWire output failed: {reason}; the loopback device keeps working"
                    ),
                    PipeWireState::Unsupported => warning!(
                        "PipeWire output unavailable: this build has no PipeWire support; the \
                         loopback device keeps working"
                    ),
                    PipeWireState::Ready => info!("PipeWire output ready"),
                    other => debug!("PipeWire output: {}", other.as_str()),
                }
                if matches!(
                    state,
                    PipeWireState::Failed(_) | PipeWireState::Unsupported | PipeWireState::Off
                ) {
                    self.set_consumers(self.loopback_consumers, 0, now);
                }
                self.pipewire_state = state;
            }
        }
    }

    /// Scans the cameras; returns the previous list. A camera whose id
    /// changed between the two scans (same node) is renamed in the session
    /// and in the runtime selection, not lost.
    fn rescan_cameras(&mut self) -> Vec<Camera> {
        let mut found = self.backend.scan_cameras(&self.config.camera_mode());
        camera::display_names(&mut found);
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

    /// Follows id changes of cameras still present at the same node. A
    /// camera at that node that is not the same device
    /// ([`camera::same_camera`]) took the node over: no rename.
    fn follow_renames(&mut self, old: &[Camera]) {
        let renamed = |id: &str, node: Option<&Path>| -> Option<String> {
            if self.cameras.iter().any(|c| c.id == id) {
                return None;
            }
            let was = old
                .iter()
                .find(|c| c.id == id && node.is_none_or(|n| c.path == n))?;
            let now = self.cameras.iter().find(|c| c.path == was.path)?;
            camera::same_camera(was, now).then(|| now.id.clone())
        };
        let session_rename = self
            .session
            .as_ref()
            .and_then(|s| renamed(&s.camera, Some(&s.node)));
        let selection_rename = self
            .runtime
            .camera
            .as_deref()
            .and_then(|id| renamed(id, None));
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
        let old = self.rescan_cameras();
        let nodes = |c: &[Camera]| c.iter().map(|c| c.path.clone()).collect::<Vec<PathBuf>>();
        // A node another camera took over changes the set as well.
        let reused = old.iter().any(|was| {
            self.cameras
                .iter()
                .any(|now| now.path == was.path && !camera::same_camera(was, now))
        });
        if nodes(&old) != nodes(&self.cameras) || reused {
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
        let fault = self.fault.take();
        // The camera is released before anything is written to disk (which
        // also ends an effect failure of the session).
        if !on {
            self.drop_session();
        }
        // An explicit choice: the state file is rewritten even if it was
        // damaged.
        let damaged = self.state_damaged.take();
        let saved = self.save_state();
        // The errors whose cause is gone give way to those still standing.
        if [fault, damaged].iter().flatten().any(|e| *e == self.error) {
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

    fn set_effect(&mut self, name: &str, now: Instant) -> Result<(), IpcError> {
        let mode: EffectMode = name
            .parse()
            .map_err(|e: obc_core::CoreError| IpcError::InvalidArgument(e.to_string()))?;
        if mode == EffectMode::Replace && self.effect().background_image.is_none() {
            return Err(IpcError::InvalidArgument(
                "the replace effect needs a background image: set one with SetBackgroundImage \
                 first"
                    .into(),
            ));
        }
        info!("effect selected: {mode}");
        let changed = self.runtime.effect.replace(mode) != Some(mode);
        self.effect_changed(changed, now)
    }

    fn set_blur_strength(&mut self, strength: f64, now: Instant) -> Result<(), IpcError> {
        if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
            return Err(IpcError::InvalidArgument(format!(
                "blur strength {strength} is outside 0 to 1"
            )));
        }
        // A slider sends many: not worth the journal.
        debug!("blur strength: {strength}");
        let changed = self.runtime.blur_strength.replace(strength) != Some(strength);
        self.effect_changed(changed, now)
    }

    /// `SetBackgroundImage`: a path sets the run-time image; `""` removes
    /// it, so the config's image (if any) applies again. Removing it is
    /// refused while `replace` is in force and the config has no image to
    /// fall back to.
    fn set_background_image(&mut self, path: &str, now: Instant) -> Result<(), IpcError> {
        let new = if path.is_empty() {
            if self.effect().mode == EffectMode::Replace
                && self.config.effect.background_image.is_none()
            {
                return Err(IpcError::InvalidArgument(
                    "the replace effect is selected and needs its background image: select \
                     another effect before clearing it"
                        .into(),
                ));
            }
            match &self.config.effect.background_image {
                Some(p) => info!(
                    "background image cleared: the configuration's ({}) applies",
                    p.display()
                ),
                None => info!("background image cleared"),
            }
            None
        } else {
            let p = PathBuf::from(path);
            check_background_file(&p).map_err(IpcError::InvalidArgument)?;
            info!("background image: {}", p.display());
            Some(p)
        };
        let changed = self.runtime.background_image != new;
        self.runtime.background_image = new;
        self.effect_changed(changed, now)
    }

    /// The effect settings changed by a method: saved (unless `changed` is
    /// false: the state file already holds them), applied to the running
    /// session without restarting it, and published before the reply. A
    /// control call clears a fault (an effect that could not run is tried
    /// again) and the `Error` of an effect running without its effect.
    fn effect_changed(&mut self, changed: bool, now: Instant) -> Result<(), IpcError> {
        if let Some(f) = self.fault.take()
            && self.error == f
        {
            self.error = self.standing_error();
        }
        if let Some(e) = self.effect_failure.take()
            && self.error == e
        {
            self.error = self.standing_error();
        }
        let mut res = if changed { self.save_state() } else { Ok(()) };
        if self.session.is_some()
            && let Err(e) = self.apply_effect()
        {
            res = Err(e);
        }
        self.reconcile(now);
        res
    }

    /// Applies the effect in force to the running session. Settings the
    /// session already has are not given again, unless the effect failed
    /// (a retry). A new effect or image, or a retry, is followed from its
    /// load (its status is reported again); a strength change is not. A
    /// setting the session cannot take is an effect failure
    /// ([`Self::effect_failed`]), and the method's error.
    fn apply_effect(&mut self) -> Result<(), IpcError> {
        let effect = self.effect();
        let failed = matches!(self.effect_seen, Some(EffectStatus::Failed(_)));
        match &self.effect_applied {
            Some(prev) if !failed && *prev == effect => return Ok(()),
            Some(prev)
                if !failed
                    && prev.mode == effect.mode
                    && prev.background_image == effect.background_image => {}
            _ => self.effect_seen = None,
        }
        self.effect_applied = Some(effect.clone());
        let Err(reason) = self.backend.set_effect(&effect) else {
            return Ok(());
        };
        let msg = effect_failure_text(&effect, &reason);
        if !self.effect_failed(&reason) {
            // Streaming on without the effect, as `none` says.
            let off = EffectConfig {
                mode: EffectMode::None,
                ..effect
            };
            if let Err(e) = self.backend.set_effect(&off) {
                warning!("turning the effect off: {e}");
            }
            self.effect_applied = Some(off);
        }
        Err(IpcError::Failed(msg))
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
        self.effect_failure = None;
        // Only the errors whose cause is gone are cleared: a missing
        // writer or a damaged state file still stands.
        self.error = self.standing_error();
        // The effect in force may have changed (or failed before): applied
        // again, which retries a failed effect.
        if self.session.is_some() {
            let _ = self.apply_effect();
        }
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
                "the new output or loopback settings apply once no application uses the \
                 virtual camera"
            );
        }
        if !self.config.pipewire.enabled
            && self.writer.as_ref().is_some_and(|w| w.pipewire.is_some())
            && self.pipewire_consumers > 0
        {
            info!("the PipeWire output stops once no application is linked to it");
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
