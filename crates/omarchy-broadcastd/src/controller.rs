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
//! `SetBlurStrength`, `SetBackgroundImage`, `SetDenoise`) over the config's
//! `[effect]`. A change applies to the running session without restarting
//! it. When the effect cannot run, `effect.on_failure` decides: `black` is a
//! fault (the session stops, so the camera is closed and the GPU released,
//! and the writer's placeholder keeps the stream black); `none` keeps
//! streaming without the effect, with the reason in `Error`. An effect that
//! is unavailable before the session starts is found out before the camera
//! is opened.
//!
//! Noise removal (`denoise`) is a stage of its own, before the effect or
//! alone with `none`. It hides nothing, so its failure never stops the
//! session, whatever `effect.on_failure` says: the session streams on
//! without the stage, the state does not change, and `Error` says why
//! (after an effect failure's text, which matters more: the background may
//! show). It is not checked before the session starts; a failure arrives
//! through the effect report. The failure is cleared when the stage becomes
//! ready (or is no longer requested), by an effect method, a reload, or the
//! end of the session. A failure of the whole chain (the effect's load or
//! frame) shows in the stage's status too, with the effect's reason: that
//! is the effect's failure, never reported as the stage's. The two statuses
//! are read one after the other, so a stage failure seen while the effect
//! is still loading or ready is taken as the stage's only when the next
//! poll confirms it (the effect's status by then shows a failure of the
//! chain).
//!
//! Consumers are of three kinds: processes holding the loopback (the
//! consumer watcher's count), with `[pipewire] enabled` links to the
//! PipeWire output (its [`PipeWireEvent`]s), and the live preview's leases
//! ([`crate::preview`]). The lifecycle (debounce, grace, a deferred writer
//! rebuild, the GPU parked at idle) follows their total, `Consumers`; any
//! kind keeps the session running. The PipeWire output runs next to the writer
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
//! A preview lease (`AcquirePreview`) is granted in every state, `off` and
//! `error` included: the backend encodes the writer's frames while a lease
//! is held and a writer runs (black placeholders while no camera streams).
//! The leases are keyed by the holder's unique bus name and end with
//! `ReleasePreview`, when the holder leaves the bus
//! ([`Command::PreviewOwnerGone`]) and at shutdown. The frames are not the
//! controller's business: the backend reports them to the D-Bus side
//! directly.
//!
//! Each camera keeps its own settings (`docs/adr/0020-camera-image-controls.md`,
//! extended by the settings channel of revision 7, [`crate::settings`]):
//! every control its node reports that the settings model publishes, and
//! the Extension Unit rows of `obc_pipeline::xu`, stored by camera id and
//! control name in the state file (raw device values). The device is
//! written only while the session has that camera open (its ingest
//! switched to it: `starting` or `active`); otherwise a set is only stored
//! and a reset is left pending (`reset_all` for the whole camera,
//! `reset_names` for some controls). Whenever a camera opens (every
//! successful switch of the ingest to it, and a rename of the session's
//! camera to an id with stored values), a pending reset writes the
//! defaults (dependent settings first), then each stored value is written
//! (masters first, then, the camera read again when the masters may have
//! changed what depends on them, the rest, each looked up by name in that
//! read); a value the device does not take, or whose control could not be
//! read, is logged, kept and never stops the session. Closing the camera
//! restores nothing: what the user set stays on the device. Reading the
//! controls (queries and reads, no stream) is allowed in any state; a
//! control that cannot be read is left out, not the list.
//!
//! The settings tree is published for `ActiveCamera` (`GetSettings`, the
//! `SettingsChanged` signal); `SettingsRevision` is bumped at every
//! published change, `SettingsSchemaSerial` at a structural one (another
//! camera included); both start at 1. One controller step announces one
//! `SettingsChanged`, so a write and the dependency flip it causes arrive
//! together. The camera is read again when `ActiveCamera` changes, on a
//! rescan, at an open, at the camera's first frame, after a reset, and
//! after a control event: while the session has a camera open, a watcher
//! (`obc_pipeline::ctrl_events`, through the backend) reports the changes
//! other software or the camera itself makes, and the read follows
//! [`EVENT_SETTLE`] after the first of a burst; an event that only reports
//! what the last read shows (the daemon's own write, read back) is no
//! reason to read again. Nothing is polled. A set on the camera the
//! session has open is validated against its last read (the published
//! tree) and reads back only the control written and the flags and values
//! of the settings that depend on it; every control is read again after an
//! action, and after a control the device flags `V4L2_CTRL_FLAG_UPDATE`
//! when the daemon cannot name what it changes. A set on a closed camera
//! reads it, to validate against what it has.
//!
//! A set that would write the device is validated at once, then: at most
//! one write per setting per [`COALESCE_WINDOW`] (a set arriving within
//! the window is held, its reply too; a later set of the same setting
//! replaces it, and the replaced call is answered `Ok`; a reset whose
//! scope holds the setting supersedes it the same way, answered `Ok` and
//! never written; the held one is written when the window opens, validated
//! again then, and answered with the real outcome; a held set follows a
//! rename of the session's camera; a camera closed or left meanwhile only
//! stores it; at shutdown every held set is carried out and answered), and
//! at most [`MAX_WRITES_PER_SECOND`] set writes in any second
//! (`RateLimited`, nothing stored). A settings change is saved once no
//! other came for [`SAVE_DELAY`], and at once at shutdown, before a camera
//! switch and with any other state change. While the state file is known
//! not to take it (damaged, ADR 0011, or its last save failed) a settings
//! change is saved, or refused, at once instead, and the set or reset is
//! `Failed` with why (the change is in force), as revision 6 answered; a
//! delayed save that fails is logged and shown in `Error` when nothing
//! else is, until a save succeeds.
//!
//! The revision-6 members are views of the same storage: `CameraControls`
//! and `GetCameraControls` show the five image controls of
//! [`obc_pipeline::controls::IMAGE_CONTROLS`] (adjustable ones only),
//! `SetCameraControl` sets only those, and `ResetCameraControls` resets
//! only those (while closed, their names go to `reset_names`). The
//! `CameraControls` property is refreshed at the same moments as the tree.
//!
//! `[camera] fixed_framerate` pins `exposure_dynamic_framerate` to 0 while
//! a camera streams (`obc_pipeline` ingest's guard), unless the user stored
//! a value for it on that camera: that camera's ingest is then built
//! without the pin and the stored value applies at the open like any
//! other. A change of it while the camera is open and pinned is written and
//! stored, but the guard, which the daemon cannot disarm, puts back the
//! value it found when the camera closes; the stored value applies again
//! from the next open. A reset while pinned (whole, of its group or of it),
//! and a pending one applied at a pinned open, forget the stored value and
//! put back the pin's 0, never the control's default: the pin stays.
//!
//! The streaming camera is followed by its device node, not by its id: an
//! id can change between two scans of the same camera (udev adds the
//! `/dev/v4l` links after the node), and such a change is a rename, not a
//! loss. A node can also be reused by another camera (one unplugged and
//! another plugged in between two scans): that is a loss, and the
//! selection stays on the camera that left ([`camera::same_camera`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use obc_core::config::{CAMERA_CONTROL_NAMES, Config, EffectConfig, RuntimeState};
use obc_core::{EffectMode, EffectOnFailure, OutputContract};
use obc_ipc::settings::ResetScope;
use obc_ipc::{
    CameraControl, CameraControls, CameraInfo, INTERFACE_REVISION, IpcError, SettingChange,
    SettingValue, SettingsTree, Snapshot, State, Stats,
};
use obc_pipeline::control_node::CtrlValue;
use obc_pipeline::controls::{
    CTRL_FLAG_DISABLED, CTRL_FLAG_READ_ONLY, CTRL_FLAG_UPDATE, DeviceControl, IMAGE_CONTROLS,
    check_value, image_control_id,
};
use obc_pipeline::ctrl_events::{
    CTRL_CHANGE_FLAGS, CTRL_CHANGE_RANGE, CTRL_CHANGE_VALUE, CtrlEvent,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::backend::{
    ControlEvent, ControlNode, ControlScan, EffectReport, EffectStatus, Failure, MediaBackend,
    PipeWireEvent, PipeWireGeneration, PipeWireState, PreviewRequest, SessionParams,
    WatchGeneration,
};
use crate::camera::{self, Camera};
use crate::preview::{Leases, PreviewDir, check_fps};
use crate::settings::catalogue::DYNAMIC_FRAMERATE;
use crate::settings::{self, CameraSettings, Entry, Inputs, Refusal, Target};
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
/// At most one device write per setting in this window: a set arriving
/// within it is held until it opens.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(50);
/// The most device writes set calls may make in any [`RATE_WINDOW`];
/// over it a set is `RateLimited`.
pub const MAX_WRITES_PER_SECOND: usize = 100;
/// The window of [`MAX_WRITES_PER_SECOND`].
pub const RATE_WINDOW: Duration = Duration::from_secs(1);
/// A settings change is saved once no other came for this long.
pub const SAVE_DELAY: Duration = Duration::from_millis(300);
/// A control event is read back this long after the first of a burst.
pub const EVENT_SETTLE: Duration = Duration::from_millis(50);

/// The reply channel of a D-Bus method.
pub type Reply = tokio::sync::oneshot::Sender<Result<(), IpcError>>;
/// The reply channel of `AcquirePreview`: the frame file's path.
pub type PathReply = tokio::sync::oneshot::Sender<Result<String, IpcError>>;
/// The reply channel of `GetCameraControls`.
pub type ControlsReply = tokio::sync::oneshot::Sender<Result<Vec<CameraControl>, IpcError>>;
/// The reply channel of `GetSettings`.
pub type SettingsReply = tokio::sync::oneshot::Sender<Result<SettingsTree, IpcError>>;

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
    /// `SetDenoise(b)`.
    SetDenoise(bool, Reply),
    /// `AcquirePreview(u)` from the unique bus name `name`.
    AcquirePreview {
        name: String,
        fps: u32,
        reply: PathReply,
    },
    /// `ReleasePreview()` from that unique bus name.
    ReleasePreview(String, Reply),
    /// That unique bus name left the bus (`NameOwnerChanged` with no new
    /// owner): its preview lease, if any, ends.
    PreviewOwnerGone(String),
    /// `GetCameraControls(s)` (empty: `ActiveCamera`).
    GetCameraControls(String, ControlsReply),
    /// `SetCameraControl(s, s, i)`.
    SetCameraControl {
        camera: String,
        name: String,
        value: i32,
        reply: Reply,
    },
    /// `ResetCameraControls(s)`.
    ResetCameraControls(String, Reply),
    /// `GetSettings(a{sv})`, with its `camera` option (empty:
    /// `ActiveCamera`).
    GetSettings(String, SettingsReply),
    /// `SetSetting(s, v, a{sv})`: the value as decoded (`None`: a D-Bus
    /// type no setting takes).
    SetSetting {
        camera: String,
        id: String,
        value: Option<SettingValue>,
        reply: Reply,
    },
    /// `ResetSettings(s, a{sv})`, the scope as sent.
    ResetSettings {
        camera: String,
        scope: String,
        reply: Reply,
    },
    /// An event of the control-event watcher of that generation (through
    /// the backend's reporter).
    ControlEvent(WatchGeneration, ControlEvent),
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
    Denoise(bool),
    Consumers(u32),
    LoopbackConsumers(u32),
    PipeWireConsumers(u32),
    PreviewConsumers(u32),
    PipeWireOutput(String),
    CameraControls(CameraControls),
    SettingsRevision(u32),
    SettingsSchemaSerial(u32),
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
            Change::Denoise(_) => "Denoise",
            Change::Consumers(_) => "Consumers",
            Change::LoopbackConsumers(_) => "LoopbackConsumers",
            Change::PipeWireConsumers(_) => "PipeWireConsumers",
            Change::PreviewConsumers(_) => "PreviewConsumers",
            Change::PipeWireOutput(_) => "PipeWireOutput",
            Change::CameraControls(_) => "CameraControls",
            Change::SettingsRevision(_) => "SettingsRevision",
            Change::SettingsSchemaSerial(_) => "SettingsSchemaSerial",
        }
    }
}

/// What the controller tells the D-Bus side.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// Properties that changed in one step, in emission order.
    Changed(Vec<Change>),
    Stats(Stats),
    /// A preview frame was written, with its sequence number (sent by the
    /// backend's preview reporter, not by the controller).
    PreviewFrame(u32),
    /// The settings tree changed (`SettingsChanged`): sent after the
    /// snapshot with the new revision and serial, empty when the serial
    /// moved.
    SettingsChanged {
        revision: u32,
        serial: u32,
        changes: Vec<SettingChange>,
    },
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

    fn settings_changed(&self, revision: u32, serial: u32, changes: Vec<SettingChange>) {
        let _ = self.notes.send(Note::SettingsChanged {
            revision,
            serial,
            changes,
        });
    }

    /// Announces a preview frame (`PreviewFrame(seq)`): called from the
    /// preview encoder's thread, through the backend's preview reporter.
    pub fn preview_frame(&self, seq: u32) {
        let _ = self.notes.send(Note::PreviewFrame(seq));
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
    if old.camera_controls != new.camera_controls {
        out.push(Change::CameraControls(
            new.camera_controls.clone().unwrap_or_default(),
        ));
    }
    if old.settings_schema_serial != new.settings_schema_serial {
        out.push(Change::SettingsSchemaSerial(
            new.settings_schema_serial.unwrap_or_default(),
        ));
    }
    if old.settings_revision != new.settings_revision {
        out.push(Change::SettingsRevision(
            new.settings_revision.unwrap_or_default(),
        ));
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
    if old.denoise != new.denoise {
        out.push(Change::Denoise(new.denoise));
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
    if old.preview_consumers != new.preview_consumers {
        out.push(Change::PreviewConsumers(new.preview_consumers));
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

/// The config file, the runtime state file and the runtime directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
    /// `$XDG_RUNTIME_DIR`, where the preview frames go
    /// ([`crate::preview::PreviewDir`]); `None` when it is not set, and
    /// then `AcquirePreview` fails.
    pub runtime: Option<PathBuf>,
}

/// The runtime state from `path`. A file that cannot be read or parsed
/// (including one with an effect value that is not accepted) fails closed:
/// not enabled, no selection, no effect override (the config's effect is
/// the one in force, and `SetEnabled` rewrites the file without the saved
/// effect choice), and the second value is the `Error` text naming the file
/// and saying so (`None`: the file is fine or absent). The third is what
/// was dropped from the camera image controls (never a damage).
pub fn load_runtime_state(path: &Path) -> (RuntimeState, Option<String>, Vec<String>) {
    match RuntimeState::load_noting(path) {
        (state, None, notes) => (state, None, notes),
        (_, Some(warn), _) => {
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
                Vec::new(),
            )
        }
    }
}

/// The snapshot served before the controller has run (only the files are
/// read: no device is touched).
pub fn preliminary_snapshot(paths: &Paths) -> Snapshot {
    let config = Config::load(&paths.config).unwrap_or_default();
    let (runtime, damaged, _) = load_runtime_state(&paths.state);
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
        denoise: effect.denoise,
        consumers: 0,
        loopback_consumers: 0,
        pipewire_consumers: 0,
        preview_consumers: 0,
        pipewire_output: PipeWireState::Off.as_str().into(),
        camera_controls: Some(CameraControls::default()),
        settings_revision: Some(1),
        settings_schema_serial: Some(1),
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

/// The `Error` text of noise removal that cannot run (the session streams
/// on without it).
fn denoise_failure_text(reason: &str) -> String {
    format!("noise removal unavailable: {reason}; the camera image is shown without it")
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
    /// `[pipewire] offer_nv12` as that output was started with it.
    pipewire_offer_nv12: bool,
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
    /// The ingest's switch to `camera` succeeded: the session has it open
    /// (`starting` or `active`), so the image control methods write to it.
    open: bool,
    /// That ingest was built with `fixed_framerate`: it pins
    /// `exposure_dynamic_framerate` and puts its value back at the close.
    pinned: bool,
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
    /// The last save of the state file failed: the `Failed` text it gave
    /// (cleared by the next save that succeeds). A settings change is then
    /// saved at once, so its caller learns whether it survives a restart.
    save_failure: Option<String>,
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
    /// Noise removal cannot run: the session streams without it, and this
    /// is its `Error` text (behind an effect failure's). Cleared when the
    /// stage becomes ready or is no longer requested, by an effect method,
    /// a reload, the next session start or the end of the session.
    denoise_failure: Option<String>,
    /// The noise removal status last polled in this session, as
    /// [`Self::effect_seen`] is for the effect.
    denoise_seen: Option<EffectStatus>,
    /// Noise removal has been polled in this session.
    denoise_polled: bool,
    /// A noise removal failure polled once while the effect was loading or
    /// ready, held for the next poll: it may be the whole chain's failure,
    /// read before the effect's status showed it.
    denoise_held: Option<EffectStatus>,
    /// The effect settings the running session was last given (`None`: no
    /// session). Only a change of the effect, its image or the noise
    /// removal request, or a failure of either, starts a new load to
    /// follow; a strength change does not.
    effect_applied: Option<EffectConfig>,
    /// When the effect's GPU time is logged next.
    effect_log_at: Option<Instant>,
    /// Every consumer: `loopback_consumers + pipewire_consumers` plus the
    /// preview leases, what the lifecycle follows.
    consumers: u32,
    /// The consumer watcher's count of processes holding the loopback.
    loopback_consumers: u32,
    /// The links to the PipeWire output (0 without one).
    pipewire_consumers: u32,
    /// The PipeWire output's state (`Off` without one).
    pipewire_state: PipeWireState,
    /// The preview leases held (`PreviewConsumers`).
    leases: Leases,
    /// What the backend was last asked to preview.
    preview: Option<PreviewRequest>,
    /// The preview directory, as last prepared by `AcquirePreview`.
    preview_dir_in_use: Option<PreviewDir>,
    /// Since when a consumer has been present (debounce).
    present_since: Option<Instant>,
    /// Since when no consumer has been present (grace).
    absent_since: Option<Instant>,
    next_stats: Option<Instant>,
    /// The `CameraControls` property: the controls of the camera it names
    /// (`ActiveCamera` when it was queried), refreshed only at the moments
    /// the module docs list.
    controls_view: CameraControls,
    /// The camera whose controls could not be read, already logged (so a
    /// failing query is logged once, not at every refresh).
    controls_unreadable: Option<String>,
    /// The (camera id, control) pairs a query skipped as unreadable,
    /// already warned about (later skips are logged at debug).
    controls_skipped: HashSet<(String, String)>,
    /// The last read of `ActiveCamera`'s controls (with the camera), what
    /// the settings tree is built from at every publish.
    settings_scan: Option<(Camera, ControlScan)>,
    /// The settings tree last published (`None` before the first).
    settings_published: Option<CameraSettings>,
    /// `SettingsRevision`: from 1, bumped at every published change.
    settings_revision: u32,
    /// `SettingsSchemaSerial`: from 1, bumped at a structural change.
    settings_serial: u32,
    /// The settings model's notes already logged, by (camera id, text).
    settings_noted: HashSet<(String, String)>,
    /// The set requests held by the coalescing window.
    held: Vec<Held>,
    /// When each (camera id, setting id) was last written by a set.
    last_writes: HashMap<(String, String), Instant>,
    /// The device writes of set calls in the last [`RATE_WINDOW`].
    recent_writes: VecDeque<Instant>,
    /// A settings change waits to be saved until then.
    settings_save_at: Option<Instant>,
    /// The running control-event watcher.
    watch: Option<WatchGeneration>,
    /// The camera whose watcher could not start, already logged.
    watch_failed: Option<String>,
    /// A control event asked for a read of the camera at this instant.
    event_refresh_at: Option<Instant>,
    published: Published,
    sink_hook: Option<SinkHook>,
}

/// A set request: `SetSetting` or the revision-6 `SetCameraControl`.
#[derive(Debug, Clone)]
enum SetRequest {
    Setting {
        id: String,
        value: Option<SettingValue>,
    },
    Control {
        name: String,
        value: i32,
    },
}

/// What [`Controller::try_set`] did.
enum SetOutcome {
    /// Answered (written, stored or refused).
    Done(Result<(), IpcError>),
    /// Valid, but inside that setting's coalescing window: held until
    /// `due`.
    Hold {
        camera: String,
        setting: String,
        /// The control's name (what a reset's scope names).
        name: String,
        due: Instant,
    },
}

/// A set request held by the coalescing window, with its reply.
#[derive(Debug)]
struct Held {
    /// The camera's id (resolved when it arrived, followed through a
    /// rename of the session's camera).
    camera: String,
    setting: String,
    /// The control's name.
    name: String,
    req: SetRequest,
    reply: Reply,
    due: Instant,
}

/// A resolved set request ([`Controller::resolve_set`]).
struct Resolved {
    cam: Camera,
    open: bool,
    /// The node, when the request was validated against a read made now
    /// (opened for writing when `open`); `None` when it was validated
    /// against the last read of the open camera, the node not opened yet.
    node: Option<Box<dyn ControlNode>>,
    scan: ControlScan,
    model: CameraSettings,
    index: usize,
    raw: i32,
}

/// The scope of a reset.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResetWhat {
    /// `ResetCameraControls`: the five image controls.
    Image,
    /// Every setting of the camera.
    Camera,
    /// The settings of one group.
    Group(String),
    /// One setting.
    Setting(String),
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
        // Before any output is built: its effect backend takes the root in
        // force when it is made.
        obc_pipeline::effect::set_maxine_root(config.effect.maxine_root.clone());
        if let Some(note) = maxine_root_note(
            None,
            config.effect.maxine_root.as_deref(),
            obc_pipeline::effect::maxine_root_env().as_deref(),
            None,
            false,
        ) {
            warning!("{note}");
        }
        let (runtime, state_damaged, notes) = load_runtime_state(&paths.state);
        for note in notes {
            warning!("{note}");
        }
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
        let effect = runtime.effect_over(&config.effect);
        info!(
            "config {}, state {} (enabled: {}, camera: {}, effect: {}, noise removal: {})",
            paths.config.display(),
            paths.state.display(),
            runtime.enabled,
            runtime.camera.as_deref().unwrap_or("not selected"),
            effect.mode,
            if effect.denoise { "on" } else { "off" }
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
            save_failure: None,
            effect_failure: None,
            effect_seen: None,
            effect_polled: false,
            denoise_failure: None,
            denoise_seen: None,
            denoise_polled: false,
            denoise_held: None,
            effect_applied: None,
            effect_log_at: None,
            consumers: 0,
            loopback_consumers: 0,
            pipewire_consumers: 0,
            pipewire_state: PipeWireState::Off,
            leases: Leases::default(),
            preview: None,
            preview_dir_in_use: None,
            present_since: None,
            absent_since: None,
            next_stats: None,
            controls_view: CameraControls::default(),
            controls_unreadable: None,
            controls_skipped: HashSet::new(),
            settings_scan: None,
            settings_published: None,
            settings_revision: 1,
            settings_serial: 1,
            settings_noted: HashSet::new(),
            held: Vec::new(),
            last_writes: HashMap::new(),
            recent_writes: VecDeque::new(),
            settings_save_at: None,
            watch: None,
            watch_failed: None,
            event_refresh_at: None,
            published,
            sink_hook,
        };
        c.rescan_cameras();
        c.reconcile(now);
        c
    }

    /// Handles one command at `now`. Returns `false` for [`Command::Shutdown`].
    pub fn handle(&mut self, cmd: Command, now: Instant) -> bool {
        // A stream of commands (a slider) must not starve what is due.
        self.settings_due(now);
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
            Command::SetDenoise(on, reply) => {
                let res = self.set_denoise(on, now);
                let _ = reply.send(res);
            }
            Command::AcquirePreview { name, fps, reply } => {
                let res = self.acquire_preview(&name, fps, now);
                let _ = reply.send(res);
            }
            Command::ReleasePreview(name, reply) => {
                self.release_preview(&name, "released", now);
                let _ = reply.send(Ok(()));
            }
            Command::PreviewOwnerGone(name) => {
                self.release_preview(&name, "ended: its holder left the bus", now);
            }
            Command::GetCameraControls(camera, reply) => {
                let res = self.get_camera_controls(&camera);
                let _ = reply.send(res);
            }
            Command::SetCameraControl {
                camera,
                name,
                value,
                reply,
            } => self.set_request(&camera, SetRequest::Control { name, value }, reply, now),
            Command::ResetCameraControls(camera, reply) => {
                let res = self.reset(&camera, ResetWhat::Image, now);
                let _ = reply.send(res);
            }
            Command::GetSettings(camera, reply) => {
                let res = self.get_settings(&camera);
                let _ = reply.send(res);
            }
            Command::SetSetting {
                camera,
                id,
                value,
                reply,
            } => self.set_request(&camera, SetRequest::Setting { id, value }, reply, now),
            Command::ResetSettings {
                camera,
                scope,
                reply,
            } => {
                let res = match ResetScope::parse(&scope) {
                    None => Err(IpcError::InvalidArgument(format!(
                        "{scope:?} is not a reset scope (camera, group:<id> or setting:<id>)"
                    ))),
                    Some(ResetScope::Camera) => self.reset(&camera, ResetWhat::Camera, now),
                    Some(ResetScope::Group(g)) => self.reset(&camera, ResetWhat::Group(g), now),
                    Some(ResetScope::Setting(id)) => {
                        self.reset(&camera, ResetWhat::Setting(id), now)
                    }
                };
                let _ = reply.send(res);
            }
            Command::ControlEvent(generation, event) => {
                self.control_event(generation, event, now);
                self.reconcile(now);
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
        self.settings_due(now);
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
        if let Some(t) = self.settings_deadline() {
            at(t);
        }
        next.saturating_duration_since(now)
    }

    /// Ends the preview leases and stops the session and the writer.
    pub fn shutdown(&mut self) {
        info!("shutting down");
        // Held set requests are carried out (and answered) while the
        // camera is still open, without a lifecycle step each; then what
        // is not saved yet is saved.
        self.run_held(Instant::now(), true);
        self.flush_settings();
        self.leases.clear();
        self.apply_preview();
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
    /// a fault, an effect failure, a noise removal failure, a damaged state
    /// file, in that order), or empty: what clearing an error whose own
    /// cause is gone leaves.
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
        if let Some(e) = &self.denoise_failure {
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

    /// The session params for an ingest on `camera`: `fixed_framerate`
    /// only when its next open pins the frame rate
    /// ([`Self::fixed_framerate_for`]).
    fn session_params_for(&self, camera: &Camera) -> SessionParams {
        SessionParams {
            fixed_framerate: self.fixed_framerate_for(&camera.id),
            ..self.session_params()
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
        if self.controls_view.camera != self.active_camera() {
            self.refresh_controls();
        }
        let tree = self.current_settings();
        // The first tree is revision 1.
        let delta = self
            .settings_published
            .as_ref()
            .and_then(|old| settings::diff(old, &tree));
        if let Some(d) = &delta {
            self.settings_revision = self.settings_revision.wrapping_add(1).max(1);
            if d.structural {
                self.settings_serial = self.settings_serial.wrapping_add(1).max(1);
            }
        }
        if delta.is_some() || self.settings_published.is_none() {
            self.settings_published = Some(tree);
        }
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
            denoise: effect.denoise,
            consumers: self.consumers,
            loopback_consumers: self.loopback_consumers,
            pipewire_consumers: self.pipewire_consumers,
            preview_consumers: self.leases.count(),
            pipewire_output: self.pipewire_state.as_str().into(),
            camera_controls: Some(self.controls_view.clone()),
            settings_revision: Some(self.settings_revision),
            settings_schema_serial: Some(self.settings_serial),
            interface_revision: INTERFACE_REVISION,
        };
        self.published.update(snap);
        if let Some(d) = delta {
            self.published.settings_changed(
                self.settings_revision,
                self.settings_serial,
                d.changes,
            );
        }
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

    /// Starts, stops or rebuilds the PipeWire output next to the running
    /// writer when `[pipewire]` asks for it (`enabled`, or `offer_nv12`
    /// differing from the offer the running node was started with), without
    /// touching the writer or the session. A stop or rebuild waits until the
    /// output has no link left (the loopback's consumers do not matter); it
    /// applies at the step that counts the last link gone. A stopped output
    /// is `off` with no link.
    fn sync_pipewire(&mut self, now: Instant) {
        let Some((running, offer)) = self
            .writer
            .as_ref()
            .map(|w| (w.pipewire.is_some(), w.pipewire_offer_nv12))
        else {
            return;
        };
        let wanted = self.config.pipewire.enabled;
        let offer_wanted = self.config.pipewire.offer_nv12;
        let pipewire = if wanted && !running {
            info!("starting the PipeWire output next to the virtual camera");
            Some(self.backend.start_pipewire(offer_wanted))
        } else if !wanted && running && self.pipewire_consumers == 0 {
            info!("stopping the PipeWire output; the virtual camera keeps running");
            self.backend.stop_pipewire();
            None
        } else if wanted && running && offer != offer_wanted && self.pipewire_consumers == 0 {
            info!(
                "rebuilding the PipeWire output to {} NV12; the virtual camera keeps running",
                if offer_wanted {
                    "offer"
                } else {
                    "stop offering"
                }
            );
            Some(self.backend.start_pipewire(offer_wanted))
        } else {
            return;
        };
        if let Some(w) = self.writer.as_mut() {
            w.pipewire = pipewire;
            w.pipewire_offer_nv12 = offer_wanted;
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
                let offer_nv12 = self.config.pipewire.offer_nv12;
                let pipewire = self
                    .config
                    .pipewire
                    .enabled
                    .then(|| self.backend.start_pipewire(offer_nv12));
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
                    pipewire_offer_nv12: offer_nv12,
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
                // The camera's first frame: its controls are read once
                // more (a camera may change them as it starts streaming).
                self.refresh_controls();
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

    /// Follows the running session's effect and noise removal: logs their
    /// transitions and acts on a failure ([`Self::effect_failed`],
    /// [`Self::poll_denoise`]). The effect comes first: a failure of the
    /// whole chain is the effect's, also shown by the stage. Returns whether
    /// the session was stopped.
    fn poll_effect(&mut self, now: Instant) -> bool {
        let Some(report) = self.backend.effect_report() else {
            return false;
        };
        if self.follow_effect(&report, now) {
            return true;
        }
        self.poll_denoise(report.denoise, &report.status);
        false
    }

    /// The effect's part of [`Self::poll_effect`].
    fn follow_effect(&mut self, report: &EffectReport, now: Instant) -> bool {
        let EffectReport {
            status,
            load_ms,
            reused,
            gpu_ms_p50,
            ..
        } = report;
        let (load_ms, reused, gpu_ms_p50) = (*load_ms, *reused, *gpu_ms_p50);
        let mode = self.effect().mode;
        if *status == EffectStatus::Ready
            && let Some(gpu) = gpu_ms_p50
            && self.effect_log_at.is_none_or(|t| now >= t)
        {
            if self.effect_log_at.is_some() {
                debug!("effect {mode}: {gpu:.2} ms of GPU time per frame (median)");
            }
            self.effect_log_at = Some(now + EFFECT_LOG_PERIOD);
        }
        if self.effect_seen.as_ref() == Some(status) {
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
            EffectStatus::Failed(reason) => return self.effect_failed(reason),
        }
        false
    }

    /// Follows the noise removal stage of the running session, `effect`
    /// being the effect's status of the same report. Its failure never
    /// stops the session: it streams on without the stage (the element
    /// drops it), the state is unchanged, and `Error` says why unless an
    /// effect failure, which matters more, already does.
    ///
    /// A failure with the effect's own failure reason is the whole chain's
    /// (the element shows it on both): the effect's, not recorded as the
    /// stage's. One seen while the effect is loading or ready waits for the
    /// next poll: the effect's status is read first, so a failure of the
    /// chain can show on the stage a poll before it does on the effect.
    /// With the effect off (`none`), or failed for another reason, the
    /// stage's failure is its own at once.
    fn poll_denoise(&mut self, status: EffectStatus, effect: &EffectStatus) {
        if let EffectStatus::Failed(reason) = &status
            && self.denoise_seen.as_ref() != Some(&status)
        {
            if matches!(effect, EffectStatus::Failed(r) if r == reason) {
                debug!("noise removal stopped with the effect: {reason}");
                self.denoise_held = None;
                self.denoise_seen = Some(status);
                self.denoise_polled = true;
                self.clear_denoise_failure();
                return;
            }
            if matches!(effect, EffectStatus::Loading | EffectStatus::Ready)
                && self.denoise_held.as_ref() != Some(&status)
            {
                self.denoise_held = Some(status);
                return;
            }
        }
        self.denoise_held = None;
        if self.denoise_seen.as_ref() == Some(&status) {
            return;
        }
        self.denoise_seen = Some(status.clone());
        let first = !std::mem::replace(&mut self.denoise_polled, true);
        match status {
            EffectStatus::Off => {
                if !first {
                    info!("noise removal off");
                }
                self.clear_denoise_failure();
            }
            EffectStatus::Loading => info!("noise removal: loading"),
            EffectStatus::Ready => {
                info!("noise removal ready");
                self.clear_denoise_failure();
            }
            EffectStatus::Failed(reason) => {
                let msg = denoise_failure_text(&reason);
                warning!("{msg}");
                if self.effect_failure.is_none() {
                    self.error = msg.clone();
                }
                self.denoise_failure = Some(msg);
            }
        }
    }

    /// Ends a noise removal failure: its `Error` gives way to the causes
    /// still standing.
    fn clear_denoise_failure(&mut self) {
        if let Some(old) = self.denoise_failure.take()
            && self.error == old
        {
            self.error = self.standing_error();
        }
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
        let mut last = None;
        for cam in candidates {
            self.leaving_camera();
            let params = self.session_params_for(cam);
            if let Some(s) = self.session.as_mut() {
                s.camera = cam.id.clone();
                s.node = cam.path.clone();
                s.awaiting = Some(now);
                s.open = false;
                s.pinned = params.fixed_framerate;
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
                    self.camera_opened(cam);
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
        self.denoise_failure = None;
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
            open: false,
            pinned: false,
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
    /// `on_failure = "none"`) and a noise removal failure end with it: their
    /// `Error` gives way to the causes still standing.
    fn drop_session(&mut self) {
        self.stop_watch();
        if self.session.take().is_some() {
            self.backend.stop_session();
        }
        self.next_stats = None;
        self.effect_seen = None;
        self.effect_polled = false;
        self.denoise_seen = None;
        self.denoise_polled = false;
        self.denoise_held = None;
        self.effect_applied = None;
        self.effect_log_at = None;
        let effect = self.effect_failure.take();
        let denoise = self.denoise_failure.take();
        if [effect, denoise].iter().flatten().any(|e| *e == self.error) {
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

    /// Sets the loopback and PipeWire counts; the lifecycle follows the
    /// total with the preview leases ([`Self::count_consumers`]).
    fn set_consumers(&mut self, loopback: u32, pipewire: u32, now: Instant) {
        self.loopback_consumers = loopback;
        self.pipewire_consumers = pipewire;
        self.count_consumers(now);
    }

    /// The total of the three kinds of consumer, and the debounce and
    /// grace instants when it leaves or reaches zero.
    fn count_consumers(&mut self, now: Instant) {
        let (loopback, pipewire, preview) = (
            self.loopback_consumers,
            self.pipewire_consumers,
            self.leases.count(),
        );
        let n = loopback.saturating_add(pipewire).saturating_add(preview);
        if n == self.consumers {
            return;
        }
        debug!("consumers: {n} (loopback {loopback}, PipeWire {pipewire}, preview {preview})");
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
        let renamed = self.follow_renames(&old);
        debug!(
            "cameras: {}",
            self.cameras
                .iter()
                .map(Camera::label)
                .collect::<Vec<_>>()
                .join(", ")
        );
        // The session's camera under a new id: what is stored under that
        // id applies now (and the view follows); otherwise ActiveCamera,
        // kept or changed, has its controls read again.
        let session_renamed = renamed.is_some();
        if !renamed.is_some_and(|id| self.apply_after_rename(&id)) {
            self.refresh_controls();
        }
        // Renamed while open: its watcher starts again under the new id.
        if session_renamed
            && let Some(cam) = self.session_camera().cloned()
            && self.is_open(&cam)
        {
            let ids = self
                .settings_scan
                .as_ref()
                .filter(|(c, _)| c.id == cam.id)
                .map(|(c, scan)| (c.clone(), scan.clone()));
            let ids = match ids {
                Some((c, scan)) => self.build_settings(&c, &scan).control_ids(),
                None => Vec::new(),
            };
            self.start_watch(&cam, &ids);
        }
        old
    }

    /// Follows id changes of cameras still present at the same node. A
    /// camera at that node that is not the same device
    /// ([`camera::same_camera`]) took the node over: no rename. Returns the
    /// session camera's new id when it was renamed.
    fn follow_renames(&mut self, old: &[Camera]) -> Option<String> {
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
        let mut session_renamed = None;
        if let Some(new) = session_rename
            && let Some(s) = self.session.as_mut()
        {
            let old = std::mem::replace(&mut s.camera, new.clone());
            info!("camera {old} is now known as {new}");
            // Its held set requests and coalescing windows follow it.
            for h in &mut self.held {
                if h.camera == old {
                    h.camera = new.clone();
                }
            }
            self.last_writes = std::mem::take(&mut self.last_writes)
                .into_iter()
                .map(|((camera, setting), t)| {
                    let camera = if camera == old { new.clone() } else { camera };
                    ((camera, setting), t)
                })
                .collect();
            session_renamed = Some(new);
        }
        if let Some(new) = selection_rename {
            info!(
                "the selected camera {} is now known as {new}",
                self.runtime.camera.as_deref().unwrap_or_default()
            );
            self.runtime.camera = Some(new);
            let _ = self.save_state();
        }
        session_renamed
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

    fn save_state(&mut self) -> Result<(), IpcError> {
        // Whatever the outcome, a pending settings save is done with: it is
        // in this write, or logged as failing.
        self.settings_save_at = None;
        if let Some(d) = &self.state_damaged {
            let msg =
                format!("{d}; the change is in force but is not saved before SetEnabled is called");
            warning!("{msg}");
            return Err(IpcError::Failed(msg));
        }
        match self.runtime.save(&self.paths.state) {
            Ok(()) => {
                // A failure shown in `Error` gives way to what still stands.
                if self.save_failure.take().is_some_and(|m| m == self.error) {
                    self.error = self.standing_error();
                }
                Ok(())
            }
            Err(e) => {
                let msg = format!(
                    "{}: {e}; the change is in force but will not survive a restart",
                    self.paths.state.display()
                );
                error!("{msg}");
                self.save_failure = Some(msg.clone());
                Err(IpcError::Failed(msg))
            }
        }
    }

    /// A settings change was made at `now`: saved after [`SAVE_DELAY`]
    /// (write-behind), unless the state file is known not to take it (it
    /// is damaged and never overwritten before `SetEnabled`, ADR 0011, or
    /// its last save failed). Then it is saved (or refused) at once, and
    /// the call answers `Failed` with why, as revision 6 did.
    fn settings_saved(&mut self, now: Instant) -> Result<(), IpcError> {
        if self.state_damaged.is_some() || self.save_failure.is_some() {
            self.save_state()
        } else {
            self.settings_changed(now);
            Ok(())
        }
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
            self.leaving_camera();
            let params = self.session_params_for(&cam);
            if let Some(s) = self.session.as_mut() {
                s.camera = cam.id.clone();
                s.node = cam.path.clone();
                s.awaiting = Some(now);
                s.open = false;
                s.pinned = params.fixed_framerate;
            }
            info!("switching to {}", cam.label());
            match self.backend.switch_camera(&params, &cam) {
                Ok(()) => self.camera_opened(&cam),
                Err(e) => {
                    let msg = format!("switching to {}: {e}", cam.label());
                    self.camera_failed(msg.clone(), now);
                    res = Err(IpcError::Failed(msg));
                }
            }
        }
        self.reconcile(now);
        res
    }

    /// `AcquirePreview(fps)` from `name`: the rate checked, the directory
    /// made safe, the lease recorded (or renewed at the new rate) and the
    /// backend asked for the preview. Replies with the frame file's path.
    fn acquire_preview(&mut self, name: &str, fps: u32, now: Instant) -> Result<String, IpcError> {
        check_fps(fps).map_err(|e| IpcError::InvalidArgument(e.to_string()))?;
        let dir = PreviewDir::under(self.paths.runtime.as_deref()).map_err(|e| {
            warning!("preview refused: {e}");
            IpcError::Failed(e)
        })?;
        dir.prepare().map_err(|e| {
            warning!("preview refused: {e}");
            IpcError::Failed(e)
        })?;
        let path = dir.frame().into_os_string().into_string().map_err(|p| {
            IpcError::Failed(format!(
                "the preview file {} is not valid UTF-8, which D-Bus cannot carry",
                Path::new(&p).display()
            ))
        })?;
        let renewed = self.leases.holds(name);
        // Checked above: cannot fail.
        let _ = self.leases.acquire(name, fps);
        info!(
            "preview lease {} by {name} at {fps} fps ({} held)",
            if renewed { "renewed" } else { "taken" },
            self.leases.count()
        );
        self.preview_dir_in_use = Some(dir);
        self.apply_preview();
        self.count_consumers(now);
        self.reconcile(now);
        Ok(path)
    }

    /// Ends the preview lease of `name`, if it holds one (`how` is logged).
    fn release_preview(&mut self, name: &str, how: &str, now: Instant) {
        if !self.leases.release(name) {
            return;
        }
        info!(
            "preview lease of {name} {how} ({} held)",
            self.leases.count()
        );
        self.apply_preview();
        self.count_consumers(now);
        self.reconcile(now);
    }

    /// Tells the backend the preview the leases ask for, if it changed: the
    /// highest rate held, into the preview directory; none without a lease.
    fn apply_preview(&mut self) {
        let want = match (self.leases.fps(), &self.preview_dir_in_use) {
            (Some(fps), Some(dir)) => Some(PreviewRequest {
                fps,
                dir: dir.path().to_path_buf(),
            }),
            _ => None,
        };
        if want != self.preview {
            self.backend.set_preview(want.clone());
            self.preview = want;
        }
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

    /// `SetDenoise`: noise removal on or off, before the effect (or alone
    /// with `none`).
    fn set_denoise(&mut self, on: bool, now: Instant) -> Result<(), IpcError> {
        info!("noise removal selected: {}", if on { "on" } else { "off" });
        let changed = self.runtime.denoise.replace(on) != Some(on);
        self.effect_changed(changed, now)
    }

    /// The effect settings changed by a method: saved (unless `changed` is
    /// false: the state file already holds them), applied to the running
    /// session without restarting it, and published before the reply. A
    /// control call clears a fault (an effect that could not run is tried
    /// again) and the `Error` of a session running without its effect or
    /// without noise removal.
    fn effect_changed(&mut self, changed: bool, now: Instant) -> Result<(), IpcError> {
        if let Some(f) = self.fault.take()
            && self.error == f
        {
            self.error = self.standing_error();
        }
        let effect = self.effect_failure.take();
        let denoise = self.denoise_failure.take();
        if [effect, denoise].iter().flatten().any(|e| *e == self.error) {
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
    /// session already has are not given again, unless the effect or noise
    /// removal failed (a retry). A new effect or image, or a retry of the
    /// effect, is followed from its load (its status is reported again), as
    /// is a new noise removal request or its retry; a strength change is
    /// not. A setting the session cannot take is an effect failure
    /// ([`Self::effect_failed`]), and the method's error.
    fn apply_effect(&mut self) -> Result<(), IpcError> {
        let effect = self.effect();
        let failed = matches!(self.effect_seen, Some(EffectStatus::Failed(_)));
        let denoise_failed = matches!(self.denoise_seen, Some(EffectStatus::Failed(_)))
            || self.denoise_held.is_some();
        let prev = self.effect_applied.as_ref();
        if !failed && !denoise_failed && prev == Some(&effect) {
            return Ok(());
        }
        if failed
            || prev.is_none_or(|p| {
                p.mode != effect.mode || p.background_image != effect.background_image
            })
        {
            self.effect_seen = None;
        }
        if denoise_failed || prev.is_none_or(|p| p.denoise != effect.denoise) {
            self.denoise_seen = None;
            self.denoise_held = None;
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

    // ---- camera settings and image controls ----

    /// The camera a control or settings method names: an exact id of a
    /// camera present, or `""` for `ActiveCamera`.
    fn control_camera(&self, camera: &str) -> Result<Camera, IpcError> {
        if camera.is_empty() {
            let id = self.active_camera();
            return self
                .cameras
                .iter()
                .find(|c| c.id == id)
                .cloned()
                .ok_or_else(|| IpcError::Unavailable("no camera is present".into()));
        }
        self.cameras
            .iter()
            .find(|c| c.id == camera)
            .cloned()
            .ok_or_else(|| {
                IpcError::NoSuchCamera(format!(
                    "no camera with the id {camera:?} is present (the ids are in Cameras)"
                ))
            })
    }

    /// Whether the session has `camera` open: it is the session's camera
    /// and the ingest's switch to it succeeded, while `starting` (before
    /// its first frame) or `active`. Only then is its device written by a
    /// method.
    fn is_open(&self, camera: &Camera) -> bool {
        matches!(self.state(), State::Starting | State::Active)
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.open && s.camera == camera.id && s.node == camera.path)
    }

    /// Whether `camera`'s next open pins `exposure_dynamic_framerate`:
    /// `[camera] fixed_framerate`, unless the user stored a value for it
    /// on that camera (the stored choice wins).
    fn fixed_framerate_for(&self, camera: &str) -> bool {
        self.config.camera.fixed_framerate
            && !self
                .runtime
                .camera_controls
                .get(camera)
                .is_some_and(|s| s.values.contains_key(DYNAMIC_FRAMERATE))
    }

    /// The ingest's switch to `camera` succeeded: the session has it open,
    /// its stored settings apply, and its controls are watched for changes
    /// made behind the daemon's back.
    fn camera_opened(&mut self, camera: &Camera) {
        if let Some(s) = self.session.as_mut() {
            s.open = true;
        }
        let ids = self.apply_camera_controls(camera);
        self.start_watch(camera, &ids);
    }

    /// The session is about to leave the camera it has open (a switch, a
    /// reopen, a fail-over): its watcher stops, and a settings change not
    /// saved yet is saved first.
    fn leaving_camera(&mut self) {
        self.stop_watch();
        self.flush_settings();
    }

    /// Starts the control-event watcher on `camera` for the controls
    /// `ids`, replacing the one that runs. A watcher that cannot start is
    /// logged once per camera; the settings then follow the camera at the
    /// other refresh points only.
    fn start_watch(&mut self, camera: &Camera, ids: &[u32]) {
        self.stop_watch();
        if ids.is_empty() {
            return;
        }
        match self.backend.watch_controls(camera, ids) {
            Ok(generation) => {
                debug!(
                    "watching {} controls of {} for changes",
                    ids.len(),
                    camera.label()
                );
                self.watch = Some(generation);
                self.watch_failed = None;
            }
            Err(e) => {
                if self.watch_failed.as_deref() != Some(camera.id.as_str()) {
                    warning!(
                        "{e}; changes other software makes to its settings show at the next \
                         refresh"
                    );
                    self.watch_failed = Some(camera.id.clone());
                }
            }
        }
    }

    /// Stops the control-event watcher, if one runs.
    fn stop_watch(&mut self) {
        if self.watch.take().is_some() {
            self.backend.unwatch_controls();
        }
        self.event_refresh_at = None;
    }

    /// An event of the control-event watcher of `generation`: ignored
    /// unless it is the running one's. A change is read back after
    /// [`EVENT_SETTLE`] (a burst of events is one read), unless the event
    /// only reports what the last read already shows (the daemon's own
    /// write, read back, and the flags it read again after it); a watcher
    /// that ended by itself is dropped.
    fn control_event(&mut self, generation: WatchGeneration, event: ControlEvent, now: Instant) {
        if self.watch != Some(generation) {
            debug!("event of control watcher {generation}, which no longer runs, ignored");
            return;
        }
        match event {
            ControlEvent::Changed(ev) => {
                if self.event_refresh_at.is_some() {
                    return;
                }
                if self.already_read(&ev) {
                    debug!(
                        "control event of {:#010x}: as last read, nothing to read again",
                        ev.id
                    );
                } else {
                    self.event_refresh_at = Some(now + EVENT_SETTLE);
                }
            }
            ControlEvent::Ended(why) => {
                info!("the camera's control events ended: {why}");
                self.stop_watch();
            }
        }
    }

    /// Whether control event `ev` reports only what the last read of the
    /// session's camera shows: each change it names (value, flags, range)
    /// equal to what was read. Any other change, or a control the read
    /// does not have, is news.
    fn already_read(&self, ev: &CtrlEvent) -> bool {
        let Some((cam, scan)) = &self.settings_scan else {
            return false;
        };
        if self.session.as_ref().is_none_or(|s| s.camera != cam.id) {
            return false;
        }
        let Some(c) = scan.controls.iter().find(|c| c.id == ev.id) else {
            return false;
        };
        let known = CTRL_CHANGE_VALUE | CTRL_CHANGE_FLAGS | CTRL_CHANGE_RANGE;
        let info = &c.info;
        ev.changes & !known == 0
            && (ev.changes & CTRL_CHANGE_VALUE == 0 || c.value == CtrlValue::Value(ev.value))
            && (ev.changes & CTRL_CHANGE_FLAGS == 0 || info.flags == ev.flags)
            && (ev.changes & CTRL_CHANGE_RANGE == 0
                || (info.minimum, info.maximum, info.step, info.default_value)
                    == (ev.minimum, ev.maximum, ev.step, ev.default_value))
    }

    /// The image controls of `scan` as revision 6 shows them: the
    /// [`IMAGE_CONTROLS`] the device has, readable and adjustable (not
    /// disabled, read-only or inactive; an integer or a boolean), in that
    /// order.
    fn image_controls(scan: &ControlScan) -> Vec<DeviceControl> {
        IMAGE_CONTROLS
            .iter()
            .filter_map(|(name, cid)| {
                let c = scan.controls.iter().find(|c| c.id == *cid)?;
                let value = c.value.value()?;
                c.info.adjustable().then(|| DeviceControl {
                    name: (*name).to_string(),
                    min: c.info.minimum,
                    max: c.info.maximum,
                    step: c.info.step,
                    default: c.info.default_value,
                    value,
                })
            })
            .collect()
    }

    /// The image controls `device` reports for `camera`, as the
    /// revision-6 methods and property give them: for the camera the
    /// session has open, the device's values; otherwise what it will be
    /// given when it opens (the stored value, else the default after a
    /// pending reset, else the device's).
    fn control_entries(&self, camera: &Camera, device: Vec<DeviceControl>) -> Vec<CameraControl> {
        let open = self.is_open(camera);
        let state = self.runtime.camera_controls.get(&camera.id);
        device
            .into_iter()
            .map(|d| {
                let user = state.and_then(|s| s.values.get(&d.name)).copied();
                let reset = state.is_some_and(|s| s.reset_all || s.reset_names.contains(&d.name));
                let value = match user {
                    _ if open => d.value,
                    Some(v) => v,
                    None if reset => d.default,
                    None => d.value,
                };
                CameraControl {
                    name: d.name,
                    min: d.min,
                    max: d.max,
                    step: d.step,
                    default: d.default,
                    value,
                    stored: user.is_some(),
                }
            })
            .collect()
    }

    /// The settings of `camera` from `scan`, its notes logged (a warning
    /// the first time for that camera and text).
    fn build_settings(&mut self, camera: &Camera, scan: &ControlScan) -> CameraSettings {
        let built = settings::build(&Inputs {
            camera: &camera.id,
            controls: &scan.controls,
            xu: &scan.xu,
            stored: self.runtime.camera_controls.get(&camera.id),
            open: self.is_open(camera),
            fixed_framerate: self.fixed_framerate_for(&camera.id),
        });
        for note in &built.notes {
            if self
                .settings_noted
                .insert((camera.id.clone(), note.clone()))
            {
                warning!("{}: {note}", camera.label());
            }
        }
        built
    }

    /// Opens `camera`'s node (for writing as well when `write`) and reads
    /// every control: the node, kept open for the rest of the operation,
    /// and what it reported. A control that could not be read is logged,
    /// as a warning the first time for that camera.
    fn scan_camera(
        &mut self,
        camera: &Camera,
        write: bool,
    ) -> Result<(Box<dyn ControlNode>, ControlScan), String> {
        let mut node = self.backend.open_camera_controls(camera, write)?;
        let scan = node.enumerate();
        self.note_unreadable(camera, &scan);
        Ok((node, scan))
    }

    /// Logs what `scan` could not read (once per camera and control).
    fn note_unreadable(&mut self, camera: &Camera, scan: &ControlScan) {
        for c in &scan.controls {
            if let CtrlValue::Unreadable { message, .. } = &c.value {
                let what = format!(
                    "{}: {} could not be read ({message}): it is left out of its settings",
                    camera.label(),
                    c.name
                );
                if self
                    .controls_skipped
                    .insert((camera.id.clone(), c.name.clone()))
                {
                    warning!("{what}");
                } else {
                    debug!("{what}");
                }
            }
        }
        for (what, why) in &scan.problems {
            let text = format!("{what} could not be read ({why})");
            if self
                .settings_noted
                .insert((camera.id.clone(), text.clone()))
            {
                warning!("{}: {text}", camera.label());
            }
        }
    }

    /// Shows `scan` (what is on the device now) as the `CameraControls`
    /// property and the settings tree when `camera` is `ActiveCamera`.
    fn show(&mut self, camera: &Camera, scan: ControlScan) {
        if self.active_camera() == camera.id {
            self.controls_view = CameraControls {
                camera: camera.id.clone(),
                controls: self.control_entries(camera, Self::image_controls(&scan)),
            };
            self.settings_scan = Some((camera.clone(), scan));
        }
    }

    /// `camera`'s controls cannot be read (`e`): it shows none, logged once
    /// for that camera.
    fn controls_unreadable(&mut self, camera: &Camera, e: &str) {
        if self.controls_unreadable.as_deref() != Some(camera.id.as_str()) {
            warning!("{e}; no setting is shown for it");
            self.controls_unreadable = Some(camera.id.clone());
        }
        self.show(camera, ControlScan::default());
    }

    /// Reads `ActiveCamera`'s controls again for the `CameraControls`
    /// property and the settings tree. A camera whose controls cannot be
    /// read shows none (logged once for that camera).
    fn refresh_controls(&mut self) {
        let id = self.active_camera();
        let Some(cam) = self.cameras.iter().find(|c| c.id == id).cloned() else {
            self.controls_view = CameraControls {
                camera: id,
                controls: Vec::new(),
            };
            self.settings_scan = None;
            return;
        };
        match self.scan_camera(&cam, false) {
            Ok((_, scan)) => {
                self.controls_unreadable = None;
                self.show(&cam, scan);
            }
            Err(e) => self.controls_unreadable(&cam, &e),
        }
    }

    /// The published settings tree, from the last read of `ActiveCamera`
    /// with what is stored and open now (no device access).
    fn current_settings(&mut self) -> CameraSettings {
        let active = self.active_camera();
        match self.settings_scan.take() {
            Some((cam, scan)) if cam.id == active => {
                let built = self.build_settings(&cam, &scan);
                self.settings_scan = Some((cam, scan));
                built
            }
            other => {
                self.settings_scan = other;
                CameraSettings {
                    camera: active,
                    ..CameraSettings::none()
                }
            }
        }
    }

    /// Writes `raw` to `entry`'s control or Extension Unit row through
    /// `node` and reads it back into `scan` (the driver may round it; a
    /// read that fails keeps the value written). An Extension Unit write
    /// is checked against the capture format, read before and after: a
    /// write that changed it is put back and fails.
    fn write_entry(
        node: &mut dyn ControlNode,
        scan: &mut ControlScan,
        entry: &Entry,
        raw: i32,
    ) -> Result<(), String> {
        match entry.target {
            Target::Ctrl(id) => {
                node.set(id, &entry.name, raw)?;
                if entry.is_action() {
                    return Ok(());
                }
                let now = node.get(id, &entry.name).unwrap_or_else(|e| {
                    debug!("{e}");
                    raw
                });
                if let Some(c) = scan.controls.iter_mut().find(|c| c.id == id) {
                    c.value = CtrlValue::Value(now);
                }
                Ok(())
            }
            Target::Xu(row) => {
                let option = u8::try_from(raw)
                    .ok()
                    .and_then(|b| row.option_for_value(b))
                    .ok_or_else(|| format!("{raw} is not an option of {}", row.name))?;
                let previous = scan
                    .xu
                    .iter()
                    .find(|x| x.row.name == row.name)
                    .and_then(|x| x.state.current_option);
                let before = node.capture_format()?;
                node.xu_set(row, option.id)?;
                let after = node.capture_format();
                if after.as_ref().ok() != Some(&before) {
                    let back = match previous {
                        Some(p) => match node.xu_set(row, p.id) {
                            Ok(()) => format!("it was put back to {}", p.id),
                            Err(e) => format!("putting it back to {} failed: {e}", p.id),
                        },
                        None => "the previous value is not one that can be written back".into(),
                    };
                    let what = match after {
                        Ok(a) => format!("{before} became {a}"),
                        Err(e) => format!("{before}, then unreadable: {e}"),
                    };
                    return Err(format!(
                        "changing the {} changed the camera's format ({what}; {back})",
                        row.label.to_lowercase()
                    ));
                }
                let current = node.xu_current(row).unwrap_or(option.value);
                if let Some(x) = scan.xu.iter_mut().find(|x| x.row.name == row.name) {
                    x.state.current = current;
                    x.state.current_option = row.option_for_value(current);
                }
                Ok(())
            }
        }
    }

    /// Writes the default of each of `indices` of `model` through `node`,
    /// dependents first; one the device flags inactive is not written (its
    /// value has no effect now), nor one it does not let change. While the
    /// session's ingest pins `exposure_dynamic_framerate` (`pinned`:
    /// `[camera] fixed_framerate`, nothing stored), that control is put
    /// back to the pin's 0, not to its default (written only if it is not
    /// 0 already): the pin stays. Returns those that failed, each with its
    /// error (logged), and whether a write may have changed other
    /// controls.
    fn write_defaults(
        node: &mut dyn ControlNode,
        scan: &mut ControlScan,
        model: &CameraSettings,
        indices: &[usize],
        pinned: bool,
    ) -> (Vec<String>, bool) {
        let mut failed = Vec::new();
        let mut others = false;
        for i in model.reset_order(indices) {
            let e = &model.entries[i];
            let Some(default) = e.default else { continue };
            if e.inactive() || e.flags & (CTRL_FLAG_READ_ONLY | CTRL_FLAG_DISABLED) != 0 {
                debug!(
                    "{} not reset: the camera does not let it change now",
                    e.name
                );
                continue;
            }
            let target = if pinned && e.name == DYNAMIC_FRAMERATE {
                if e.device == Some(0) {
                    debug!("{} not reset: the frame rate pin holds it at 0", e.name);
                    continue;
                }
                0
            } else {
                default
            };
            match Self::write_entry(node, scan, e, target) {
                Ok(()) => others |= e.updates_others,
                Err(e2) => {
                    warning!("{e2}");
                    failed.push(format!("{} ({e2})", e.name));
                }
            }
        }
        (failed, others)
    }

    /// `camera` was just opened (its ingest switched to it): a pending
    /// reset writes the defaults (every setting for a whole reset, the
    /// names listed otherwise; dependents first; see
    /// [`Self::write_defaults`] for the frame rate pin), then each stored
    /// value is written (masters first), all through one open of its node.
    /// A stored value the device does not take now (no such control, out of
    /// its range, not an option) is skipped and kept; one whose control is
    /// inactive is not written (it has no effect); one whose control could
    /// not be read is skipped, kept and logged; a write that fails is
    /// logged. None of it fails or stops the session. Each stored value is
    /// looked up by its name in the read it is written from: the masters'
    /// writes may make the camera read again, and a control can be in one
    /// read and not in the other (its read failed once). The views are built
    /// from what is on the device afterwards. Returns the control ids to
    /// watch.
    fn apply_camera_controls(&mut self, camera: &Camera) -> Vec<u32> {
        let (mut node, mut scan) = match self.scan_camera(camera, true) {
            Ok(found) => found,
            Err(e) => {
                warning!("{e}; its stored settings are not applied");
                // Logged: a refresh that fails the same is not logged again.
                self.controls_unreadable = Some(camera.id.clone());
                self.show(camera, ControlScan::default());
                return Vec::new();
            }
        };
        self.controls_unreadable = None;
        let pinned = self.session.as_ref().is_some_and(|s| s.pinned);
        let mut model = self.build_settings(camera, &scan);
        let state = self
            .runtime
            .camera_controls
            .get(&camera.id)
            .cloned()
            .unwrap_or_default();
        if state.reset_all || !state.reset_names.is_empty() {
            let indices: Vec<usize> = (0..model.entries.len())
                .filter(|&i| {
                    let e = &model.entries[i];
                    !e.is_action() && (state.reset_all || state.reset_names.contains(&e.name))
                })
                .collect();
            let (failed, others) =
                Self::write_defaults(&mut *node, &mut scan, &model, &indices, pinned);
            info!(
                "{} of {} reset to the camera's defaults{}",
                if state.reset_all {
                    "settings"
                } else {
                    "some settings"
                },
                camera.label(),
                if failed.is_empty() {
                    String::new()
                } else {
                    format!(", except {}", failed.join(", "))
                }
            );
            if let Some(s) = self.runtime.camera_controls.get_mut(&camera.id) {
                s.reset_all = false;
                s.reset_names.clear();
                if s.is_empty() {
                    self.runtime.camera_controls.remove(&camera.id);
                }
            }
            let _ = self.save_state();
            if others {
                scan = node.enumerate();
                self.note_unreadable(camera, &scan);
            }
            model = self.build_settings(camera, &scan);
        }
        let mut applied = Vec::new();
        // The stored names not written (nor skipped) yet, and those a read
        // of this open could not read.
        let mut left: Vec<String> = state.values.keys().cloned().collect();
        let mut unread: HashSet<String> = unreadable_names(&scan).collect();
        let mut others = false;
        // Phase 0: the masters of this read; phase 1: every name left,
        // looked up in the read made after the masters' writes when they
        // may have changed what depends on them.
        for phase in 0..2 {
            if phase == 1 && others {
                scan = node.enumerate();
                self.note_unreadable(camera, &scan);
                unread.extend(unreadable_names(&scan));
                model = self.build_settings(camera, &scan);
                others = false;
            }
            let indices: Vec<usize> = left
                .iter()
                .filter_map(|name| model.entries.iter().position(|e| e.name == *name))
                .filter(|&i| phase == 1 || !model.entries[i].is_dependent())
                .collect();
            for i in model.apply_order(&indices) {
                let e = &model.entries[i];
                left.retain(|n| *n != e.name);
                let value = state.values[&e.name];
                if let Err(why) = e.check_raw(value) {
                    warning!(
                        "{}: the stored {why}: not applied (kept for the camera)",
                        camera.label()
                    );
                    continue;
                }
                if e.inactive() || e.flags & (CTRL_FLAG_READ_ONLY | CTRL_FLAG_DISABLED) != 0 {
                    debug!(
                        "{}: the stored {} is not applied now: the camera does not let it \
                         change (kept for the camera)",
                        camera.label(),
                        e.name
                    );
                    continue;
                }
                match Self::write_entry(&mut *node, &mut scan, e, value) {
                    Ok(()) => {
                        others |= e.updates_others;
                        applied.push(format!("{} {value}", e.name));
                    }
                    Err(why) => warning!("{why}"),
                }
            }
        }
        if others {
            scan = node.enumerate();
            self.note_unreadable(camera, &scan);
        }
        drop(node);
        for name in &left {
            if unread.contains(name) {
                warning!(
                    "{}: {name} could not be read when it opened: its stored value is not \
                     applied (kept for the camera)",
                    camera.label()
                );
            } else {
                warning!(
                    "{} does not expose {name}: its stored value is not applied (kept for the \
                     camera)",
                    camera.label()
                );
            }
        }
        if !applied.is_empty() {
            info!(
                "settings of {} applied: {}",
                camera.label(),
                applied.join(", ")
            );
        }
        let ids = self.build_settings(camera, &scan).control_ids();
        self.show(camera, scan);
        ids
    }

    /// The session's camera was renamed to `id` by a rescan: what is
    /// stored under that id applies now, if the session has it open.
    /// Returns whether it did (the view is then up to date).
    fn apply_after_rename(&mut self, id: &str) -> bool {
        if !self.runtime.camera_controls.contains_key(id) {
            return false;
        }
        let Some(cam) = self.session_camera().cloned() else {
            return false;
        };
        if cam.id != id || !self.is_open(&cam) {
            return false;
        }
        info!(
            "applying the settings stored for {}, its new id",
            cam.label()
        );
        self.apply_camera_controls(&cam);
        true
    }

    /// `GetCameraControls`: the image controls `camera` exposes, read now.
    fn get_camera_controls(&mut self, camera: &str) -> Result<Vec<CameraControl>, IpcError> {
        let cam = self.control_camera(camera)?;
        let (_, scan) = self.scan_camera(&cam, false).map_err(IpcError::Failed)?;
        Ok(self.control_entries(&cam, Self::image_controls(&scan)))
    }

    /// `GetSettings`: the published tree for `ActiveCamera` (`""` or its
    /// id; with no camera present, camera `""` and no group), the tree read
    /// now for another camera; both with the current revision and serial.
    fn get_settings(&mut self, camera: &str) -> Result<SettingsTree, IpcError> {
        let (rev, serial) = (self.settings_revision, self.settings_serial);
        if camera.is_empty() || camera == self.active_camera() {
            if camera.is_empty() && self.cameras.is_empty() {
                return Ok(CameraSettings::none().tree(rev, serial));
            }
            if let Some(published) = &self.settings_published
                && published.camera == self.active_camera()
                && !published.camera.is_empty()
            {
                return Ok(published.tree(rev, serial));
            }
        }
        let cam = self.control_camera(camera)?;
        let (_, scan) = self.scan_camera(&cam, false).map_err(IpcError::Failed)?;
        Ok(self.build_settings(&cam, &scan).tree(rev, serial))
    }

    /// The device writes made by set calls in the second before `now`.
    fn writes_in_last_second(&mut self, now: Instant) -> usize {
        while self
            .recent_writes
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= RATE_WINDOW)
        {
            self.recent_writes.pop_front();
        }
        self.recent_writes.len()
    }

    /// A settings change: the state file is saved once no other settings
    /// change came for [`SAVE_DELAY`].
    fn settings_changed(&mut self, now: Instant) {
        self.settings_save_at = Some(now + SAVE_DELAY);
    }

    /// Saves a settings change not saved yet, now.
    pub(crate) fn flush_settings(&mut self) {
        if self.settings_save_at.is_some() {
            let _ = self.save_state();
        }
    }

    /// `SetSetting` or `SetCameraControl` (`req`) on `camera`: answered
    /// now, or held for the coalescing window (a held request of the same
    /// setting is replaced, and answered `Ok`).
    fn set_request(&mut self, camera: &str, req: SetRequest, reply: Reply, now: Instant) {
        match self.try_set(camera, &req, now, false, true) {
            SetOutcome::Done(res) => {
                let _ = reply.send(res);
            }
            SetOutcome::Hold {
                camera,
                setting,
                name,
                due,
            } => {
                if let Some(i) = self
                    .held
                    .iter()
                    .position(|h| h.camera == camera && h.setting == setting)
                {
                    let old = self.held.remove(i);
                    debug!("{setting} on {camera}: a held value replaced by a newer one");
                    let _ = old.reply.send(Ok(()));
                }
                self.held.push(Held {
                    camera,
                    setting,
                    name,
                    req,
                    reply,
                    due,
                });
            }
        }
    }

    /// The last read of `camera` (the one the published tree is built
    /// from), unless it is another camera's or its read failed.
    fn last_read(&self, camera: &Camera) -> Option<ControlScan> {
        if self.controls_unreadable.as_deref() == Some(camera.id.as_str()) {
            return None;
        }
        self.settings_scan
            .as_ref()
            .filter(|(c, _)| c.id == camera.id && c.path == camera.path)
            .map(|(_, scan)| scan.clone())
    }

    /// The camera, the setting and the raw value of a set request:
    /// `UnknownSetting`, `Disabled` or `InvalidArgument` (in that order)
    /// for a `SetSetting` the settings refuse; for `SetCameraControl`, the
    /// revision-6 checks (an image control the camera exposes, a value in
    /// its range and on its step). For the camera the session has open,
    /// against its last read (the published tree), without touching the
    /// device; otherwise against a read made now, its node kept open.
    fn resolve_set(&mut self, camera: &str, req: &SetRequest) -> Result<Resolved, IpcError> {
        let cam = self.control_camera(camera)?;
        if let SetRequest::Control { name, .. } = req
            && image_control_id(name).is_none()
        {
            return Err(IpcError::InvalidArgument(format!(
                "{name:?} is not a camera control (expected one of {})",
                IMAGE_CONTROLS.map(|(n, _)| n).join(", ")
            )));
        }
        let open = self.is_open(&cam);
        let (node, scan) = match open.then(|| self.last_read(&cam)).flatten() {
            Some(scan) => (None, scan),
            None => {
                let (node, scan) = self.scan_camera(&cam, open).map_err(IpcError::Failed)?;
                (Some(node), scan)
            }
        };
        let model = self.build_settings(&cam, &scan);
        let (index, raw) = match req {
            SetRequest::Setting { id, value } => {
                model.check(id, value.as_ref()).map_err(|r| match r {
                    Refusal::Unknown(m) => IpcError::UnknownSetting(m),
                    Refusal::Disabled(m) => IpcError::Disabled(m),
                    Refusal::Invalid(m) => IpcError::InvalidArgument(m),
                })?
            }
            SetRequest::Control { name, value } => {
                let not_exposed =
                    || IpcError::InvalidArgument(format!("{} does not expose {name}", cam.label()));
                let image = Self::image_controls(&scan);
                let control = image
                    .iter()
                    .find(|d| d.name == *name)
                    .ok_or_else(not_exposed)?;
                check_value(control, *value)
                    .map_err(|e| IpcError::InvalidArgument(format!("{}: {e}", cam.label())))?;
                let index = model
                    .entries
                    .iter()
                    .position(|e| e.name == *name)
                    .ok_or_else(not_exposed)?;
                (index, *value)
            }
        };
        Ok(Resolved {
            cam,
            open,
            node,
            scan,
            model,
            index,
            raw,
        })
    }

    /// After `entry` of `model` was written through `node`: reads again,
    /// into `scan`, what the write may have changed besides it. The
    /// settings that depend on it (a catalogue rule) are queried and read
    /// one by one. Every control is read again after an action, after a
    /// write the device says may change other controls
    /// (`V4L2_CTRL_FLAG_UPDATE`) when the daemon cannot name them (none
    /// depends on it, or no watcher would report the others), and when one
    /// of those single reads fails.
    fn read_others(
        &mut self,
        cam: &Camera,
        node: &mut dyn ControlNode,
        scan: &mut ControlScan,
        model: &CameraSettings,
        entry: &Entry,
    ) {
        let dependents: Vec<(u32, &str)> = model
            .entries
            .iter()
            .filter(|e| {
                e.setting
                    .enabled_when
                    .iter()
                    .any(|c| c.id == entry.setting.id)
            })
            .filter_map(|e| match e.target {
                Target::Ctrl(id) => Some((id, e.name.as_str())),
                Target::Xu(_) => None,
            })
            .collect();
        let unnamed = entry.is_action()
            || (entry.flags & CTRL_FLAG_UPDATE != 0
                && (dependents.is_empty() || self.watch.is_none()));
        if !unnamed {
            let mut read = true;
            for (id, name) in &dependents {
                let got = node
                    .query(*id, name)
                    .and_then(|info| node.get(*id, name).map(|v| (info, v)));
                match (got, scan.controls.iter_mut().find(|c| c.id == *id)) {
                    (Ok((info, value)), Some(c)) => {
                        c.info = info;
                        c.value = CtrlValue::Value(value);
                    }
                    (Err(e), _) => {
                        debug!("{e}: every control is read again");
                        read = false;
                        break;
                    }
                    (Ok(_), None) => {
                        read = false;
                        break;
                    }
                }
            }
            if read {
                return;
            }
        }
        *scan = node.enumerate();
        self.note_unreadable(cam, scan);
    }

    /// Validates and carries out a set request (see the module docs): the
    /// camera, then the setting (unknown, disabled, invalid), then, while
    /// the session has the camera open, the coalescing window (unless
    /// `due`: the held request's window has passed), the global cap and
    /// the write (read back, with what depends on it); then the value is
    /// stored (not for an action) and the views follow, with a lifecycle
    /// step when `step`.
    fn try_set(
        &mut self,
        camera: &str,
        req: &SetRequest,
        now: Instant,
        due: bool,
        step: bool,
    ) -> SetOutcome {
        let Resolved {
            cam,
            open,
            mut node,
            mut scan,
            model,
            index: i,
            raw,
        } = match self.resolve_set(camera, req) {
            Ok(r) => r,
            Err(e) => return SetOutcome::Done(Err(e)),
        };
        let entry = model.entries[i].clone();
        if entry.is_action() && !open {
            return SetOutcome::Done(Err(IpcError::Unavailable(format!(
                "{}: {} acts only while the camera is in use",
                cam.label(),
                entry.setting.label
            ))));
        }
        if open {
            let key = (cam.id.clone(), entry.setting.id.clone());
            if !due
                && let Some(last) = self.last_writes.get(&key)
                && now < *last + COALESCE_WINDOW
            {
                return SetOutcome::Hold {
                    camera: cam.id,
                    setting: entry.setting.id,
                    name: entry.name,
                    due: *last + COALESCE_WINDOW,
                };
            }
            if self.writes_in_last_second(now) >= MAX_WRITES_PER_SECOND {
                return SetOutcome::Done(Err(IpcError::RateLimited(format!(
                    "more than {MAX_WRITES_PER_SECOND} camera writes in one second: {} was not \
                     changed",
                    entry.setting.id
                ))));
            }
            let mut node = match node.take() {
                Some(node) => node,
                None => match self.backend.open_camera_controls(&cam, true) {
                    Ok(node) => node,
                    Err(e) => {
                        warning!("{e}");
                        return SetOutcome::Done(Err(IpcError::Failed(e)));
                    }
                },
            };
            if let Err(e) = Self::write_entry(&mut *node, &mut scan, &entry, raw) {
                warning!("{e}");
                // The device may differ from what was last read: read
                // again, and shown as it is.
                scan = node.enumerate();
                self.note_unreadable(&cam, &scan);
                self.show(&cam, scan);
                if step {
                    self.reconcile(now);
                }
                return SetOutcome::Done(Err(IpcError::Failed(e)));
            }
            self.last_writes.insert(key, now);
            self.recent_writes.push_back(now);
            if entry.updates_others || entry.is_action() {
                self.read_others(&cam, &mut *node, &mut scan, &model, &entry);
            }
        }
        drop(node);
        // A slider sends many: not worth the journal.
        debug!(
            "{} {} = {raw}{}",
            cam.label(),
            entry.name,
            if open {
                ""
            } else {
                " (stored: applied when it opens)"
            }
        );
        let mut res = Ok(());
        if !entry.is_action() {
            let changed = self
                .runtime
                .camera_controls
                .entry(cam.id.clone())
                .or_default()
                .values
                .insert(entry.name.clone(), raw)
                != Some(raw);
            if changed {
                res = self.settings_saved(now);
            }
        }
        if open
            && entry.name == DYNAMIC_FRAMERATE
            && self.session.as_ref().is_some_and(|s| s.pinned)
        {
            // The ingest's guard (obc-pipeline) holds the value it found and
            // puts it back when the camera closes; it cannot be disarmed
            // from here. The stored value wins from the next open on, which
            // is built without the pin.
            info!(
                "{}: exposure_dynamic_framerate written and stored; this session pinned it \
                 ([camera] fixed_framerate), so the value found at its start is put back when \
                 the camera closes, and the stored choice applies again at the next open",
                cam.label()
            );
        }
        self.show(&cam, scan);
        if step {
            self.reconcile(now);
        }
        SetOutcome::Done(res)
    }

    /// Runs the held set requests whose window has passed (`all`: every
    /// one, at shutdown, without a lifecycle step each) and answers them
    /// with their outcome.
    fn run_held(&mut self, now: Instant, all: bool) {
        loop {
            let Some(i) = self.held.iter().position(|h| all || h.due <= now) else {
                return;
            };
            let h = self.held.remove(i);
            let res = match self.try_set(&h.camera, &h.req, now, true, !all) {
                SetOutcome::Done(res) => res,
                // Cannot happen with `due`; answered rather than lost.
                SetOutcome::Hold { .. } => Ok(()),
            };
            let _ = h.reply.send(res);
        }
    }

    /// Answers the held set requests of `camera` that `in_scope` names
    /// `Ok` without writing them: a reset superseded them, as a newer set
    /// supersedes a held one.
    fn cancel_held(&mut self, camera: &str, in_scope: impl Fn(&Held) -> bool) {
        let (gone, kept): (Vec<Held>, Vec<Held>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|h| h.camera == camera && in_scope(h));
        self.held = kept;
        for h in gone {
            debug!(
                "{} on {camera}: a held value superseded by a reset",
                h.setting
            );
            let _ = h.reply.send(Ok(()));
        }
    }

    /// Runs what the settings have due at `now`: held set requests, a
    /// re-read after a control event, a settings save. A delayed save that
    /// fails is logged, and shown in `Error` when nothing else is.
    fn settings_due(&mut self, now: Instant) {
        self.run_held(now, false);
        if self.event_refresh_at.is_some_and(|t| t <= now) {
            self.event_refresh_at = None;
            self.refresh_controls();
            self.publish();
        }
        if self.settings_save_at.is_some_and(|t| t <= now)
            && let Err(IpcError::Failed(m)) = self.save_state()
            && self.error.is_empty()
            && self.save_failure.as_ref() == Some(&m)
        {
            self.error = m;
        }
    }

    /// The earliest settings deadline, if any.
    fn settings_deadline(&self) -> Option<Instant> {
        self.held
            .iter()
            .map(|h| h.due)
            .chain(self.event_refresh_at)
            .chain(self.settings_save_at)
            .min()
    }

    /// `ResetSettings` (or, for [`ResetWhat::Image`], `ResetCameraControls`):
    /// the stored values in the scope forgotten, and the held set requests
    /// in it answered `Ok` without being written (the reset supersedes
    /// them); while the session has the camera open each default is written
    /// now (dependents first; one the camera flags inactive is not written;
    /// the frame rate pin stays, [`Self::write_defaults`]), else the reset
    /// is left for the next open (`reset_all` for the whole camera, the
    /// names in `reset_names` otherwise). A default the device refuses
    /// fails the call, naming the controls; the values are forgotten all the
    /// same. A change the state file cannot take fails it too
    /// ([`Self::settings_saved`]).
    fn reset(&mut self, camera: &str, what: ResetWhat, now: Instant) -> Result<(), IpcError> {
        let cam = self.control_camera(camera)?;
        let open = self.is_open(&cam);
        let shown = self.active_camera() == cam.id;
        let needs_model = matches!(what, ResetWhat::Group(_) | ResetWhat::Setting(_));
        let mut res = Ok(());
        let mut pending = !open;
        let mut scan = None;
        let mut unread = None;
        // The names in the scope.
        let mut names: Vec<String> = match &what {
            ResetWhat::Image => CAMERA_CONTROL_NAMES.iter().map(|n| n.to_string()).collect(),
            _ => Vec::new(),
        };
        if open || shown || needs_model {
            match self.scan_camera(&cam, open) {
                Ok((mut node, mut found)) => {
                    let model = self.build_settings(&cam, &found);
                    let indices: Vec<usize> = match &what {
                        ResetWhat::Image => {
                            let image = Self::image_controls(&found);
                            (0..model.entries.len())
                                .filter(|&i| image.iter().any(|d| d.name == model.entries[i].name))
                                .collect()
                        }
                        ResetWhat::Camera => (0..model.entries.len()).collect(),
                        ResetWhat::Group(g) => {
                            let members: Vec<usize> = (0..model.entries.len())
                                .filter(|&i| model.entries[i].group == g.as_str())
                                .collect();
                            if members.is_empty() {
                                return Err(IpcError::UnknownSetting(format!(
                                    "the camera {} has no settings group {g:?}",
                                    cam.id
                                )));
                            }
                            members
                        }
                        ResetWhat::Setting(id) => match model.index(id) {
                            Some(i) if model.entries[i].is_action() => {
                                return Err(IpcError::InvalidArgument(format!(
                                    "{id} is an action: it has no value to reset"
                                )));
                            }
                            Some(i) => vec![i],
                            None => {
                                return Err(IpcError::UnknownSetting(format!(
                                    "the camera {} has no setting {id:?}",
                                    cam.id
                                )));
                            }
                        },
                    };
                    let indices: Vec<usize> = indices
                        .into_iter()
                        .filter(|&i| !model.entries[i].is_action())
                        .collect();
                    if !matches!(what, ResetWhat::Image) {
                        names = indices
                            .iter()
                            .map(|&i| model.entries[i].name.clone())
                            .collect();
                    }
                    if open {
                        let pinned = self.session.as_ref().is_some_and(|s| s.pinned);
                        let (failed, others) =
                            Self::write_defaults(&mut *node, &mut found, &model, &indices, pinned);
                        if !failed.is_empty() {
                            res = Err(IpcError::Failed(format!(
                                "{}: not put back to the default: {}",
                                cam.label(),
                                failed.join(", ")
                            )));
                        }
                        if others {
                            found = node.enumerate();
                            self.note_unreadable(&cam, &found);
                        }
                    }
                    scan = Some(found);
                }
                Err(e) => {
                    if needs_model {
                        return Err(IpcError::Failed(e));
                    }
                    if open {
                        warning!("{e}");
                        // Logged: not again for the view.
                        self.controls_unreadable = Some(cam.id.clone());
                        res = Err(IpcError::Failed(format!(
                            "{e}; the defaults are written when the camera next opens"
                        )));
                        pending = true;
                    }
                    unread = Some(e);
                }
            }
        }
        let whole = matches!(what, ResetWhat::Camera);
        self.cancel_held(&cam.id, |h| whole || names.contains(&h.name));
        info!(
            "{} of {} reset{}",
            match &what {
                ResetWhat::Image => "image controls".to_string(),
                ResetWhat::Camera => "settings".to_string(),
                ResetWhat::Group(g) => format!("the {g} settings"),
                ResetWhat::Setting(id) => id.clone(),
            },
            cam.label(),
            if pending {
                ": the camera's defaults apply when it next opens"
            } else {
                " to the camera's defaults"
            }
        );
        let before = self.runtime.camera_controls.get(&cam.id).cloned();
        {
            let state = self
                .runtime
                .camera_controls
                .entry(cam.id.clone())
                .or_default();
            if whole {
                state.values.clear();
                state.reset_names.clear();
                state.reset_all |= pending;
            } else {
                for name in &names {
                    state.values.remove(name);
                    if pending && !state.reset_all {
                        state.reset_names.insert(name.clone());
                    }
                }
            }
            if state.is_empty() {
                self.runtime.camera_controls.remove(&cam.id);
            }
        }
        if before.as_ref() != self.runtime.camera_controls.get(&cam.id) {
            let saved = self.settings_saved(now);
            if res.is_ok() {
                res = saved;
            }
        }
        match (scan, unread) {
            (Some(scan), _) => self.show(&cam, scan),
            (None, Some(e)) => self.controls_unreadable(&cam, &e),
            (None, None) => {}
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
        obc_pipeline::effect::set_maxine_root(self.config.effect.maxine_root.clone());
        if let Some(note) = maxine_root_note(
            Some(old.effect.maxine_root.as_deref()),
            self.config.effect.maxine_root.as_deref(),
            obc_pipeline::effect::maxine_root_env().as_deref(),
            obc_pipeline::effect::maxine_root_conflict(),
            self.session.is_some(),
        ) {
            warning!("{note}");
        }
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
        self.denoise_failure = None;
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
                self.leaving_camera();
                let params = self.session_params_for(&cam);
                if let Some(s) = self.session.as_mut() {
                    s.awaiting = Some(now);
                    s.open = false;
                    s.pinned = params.fixed_framerate;
                }
                match self.backend.switch_camera(&params, &cam) {
                    Ok(()) => self.camera_opened(&cam),
                    Err(e) => self.camera_failed(format!("reopening {}: {e}", cam.label()), now),
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
        } else if self.config.pipewire.enabled
            && self.writer.as_ref().is_some_and(|w| {
                w.pipewire.is_some() && w.pipewire_offer_nv12 != self.config.pipewire.offer_nv12
            })
            && self.pipewire_consumers > 0
        {
            info!("the new PipeWire format offer applies once no application is linked to it");
        }
    }
}

/// The names of the controls `scan` could not read.
fn unreadable_names(scan: &ControlScan) -> impl Iterator<Item = String> + '_ {
    scan.controls
        .iter()
        .filter(|c| matches!(c.value, CtrlValue::Unreadable { .. }))
        .map(|c| c.name.clone())
}

/// What to log, as a warning, about `[effect] maxine_root` (`new`) at start
/// (`old`: `None`) or after a reload that had `Some(old)`. `env` is
/// `MAXINE_VFX_ROOT` when set and non-empty, `conflict` why the root in
/// force now cannot be used (the SDK is already loaded from elsewhere),
/// `running` whether an output (and its effect backend, made with the root
/// in force when it was built) is running.
///
/// At start: that the variable overrides the key, when both are set. After
/// a reload, only when the key changed: that the variable overrides it;
/// else that the SDK is already loaded from elsewhere and only a restart
/// changes it; else, with an output running, that its effects keep the
/// root it was built with.
fn maxine_root_note(
    old: Option<Option<&Path>>,
    new: Option<&Path>,
    env: Option<&Path>,
    conflict: Option<String>,
    running: bool,
) -> Option<String> {
    let changed = match old {
        None => new.is_some(),
        Some(old) => old != new,
    };
    if !changed {
        return None;
    }
    let what = if old.is_none() {
        "effect.maxine_root"
    } else {
        "effect.maxine_root changed"
    };
    if let Some(env) = env {
        let configured = new.map_or_else(|| "unset".to_string(), |p| p.display().to_string());
        return Some(format!(
            "{what} ({configured}), but MAXINE_VFX_ROOT={env} is set in the daemon's \
             environment and takes precedence: the Maxine SDK root is {env}. The \
             maxine-vfx-sdk-local environment file (/usr/share/maxine-vfx/maxine-vfx.env) \
             sets it; to use effect.maxine_root, start the daemon without that variable",
            env = env.display()
        ));
    }
    // At start, nothing else to say: no SDK is loaded yet.
    old?;
    if let Some(why) = conflict {
        return Some(format!("{what}: {why}"));
    }
    running.then(|| {
        format!(
            "{what} while the virtual camera is running: its effects keep the Maxine SDK root \
             it was started with, and the SDK stays loaded from wherever it is first loaded. \
             The new root applies when the virtual camera next starts if no effect has loaded \
             the SDK before; otherwise restart the daemon (systemctl --user restart \
             omarchy-broadcastd)"
        )
    })
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

#[cfg(test)]
mod maxine_root_tests {
    use super::*;

    const ENV: &str = "/opt/nvidia/maxine/videofx";

    fn text(n: Option<String>) -> String {
        n.expect("a warning")
    }

    #[test]
    fn at_start_only_an_overridden_key_is_logged() {
        let key = Some(Path::new("/home/u/VideoFX"));
        assert_eq!(maxine_root_note(None, None, None, None, false), None);
        assert_eq!(maxine_root_note(None, key, None, None, true), None);
        assert_eq!(
            maxine_root_note(None, None, Some(Path::new(ENV)), None, false),
            None
        );
        let m = text(maxine_root_note(
            None,
            key,
            Some(Path::new(ENV)),
            None,
            false,
        ));
        assert!(
            m.starts_with("effect.maxine_root (/home/u/VideoFX), but "),
            "{m}"
        );
        assert!(m.contains(&format!("MAXINE_VFX_ROOT={ENV} is set")), "{m}");
        assert!(m.contains("takes precedence"), "{m}");
        assert!(m.contains("maxine-vfx.env"), "{m}");
    }

    #[test]
    fn a_reload_logs_only_a_changed_key() {
        let (a, b) = (Some(Path::new("/sdk/a")), Some(Path::new("/sdk/b")));
        let why = || Some("already loaded in this process from /sdk/a".to_string());
        // Unchanged: nothing, whatever else holds.
        for env in [None, Some(Path::new(ENV))] {
            assert_eq!(maxine_root_note(Some(a), a, env, why(), true), None);
            assert_eq!(maxine_root_note(Some(None), None, env, why(), true), None);
        }
        // Changed with the variable set: the variable wins, said once per
        // change, set or cleared.
        for (old, new) in [(a, b), (None, b), (a, None)] {
            let m = text(maxine_root_note(
                Some(old),
                new,
                Some(Path::new(ENV)),
                None,
                true,
            ));
            assert!(m.starts_with("effect.maxine_root changed ("), "{m}");
            assert!(m.contains("takes precedence"), "{m}");
        }
        let m = text(maxine_root_note(
            Some(a),
            None,
            Some(Path::new(ENV)),
            None,
            false,
        ));
        assert!(m.contains("(unset)"), "{m}");
        // Changed while the SDK is loaded from elsewhere: the restart,
        // running or not.
        for running in [false, true] {
            let m = text(maxine_root_note(Some(a), b, None, why(), running));
            assert_eq!(
                m,
                "effect.maxine_root changed: already loaded in this process from /sdk/a"
            );
        }
        // Changed with nothing loaded: only a running output keeps the old
        // root.
        assert_eq!(maxine_root_note(Some(a), b, None, None, false), None);
        let m = text(maxine_root_note(Some(a), b, None, None, true));
        assert!(m.contains("while the virtual camera is running"), "{m}");
        assert!(m.contains("restart the daemon"), "{m}");
    }
}
