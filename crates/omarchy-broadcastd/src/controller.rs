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
//! The state follows from what runs (`Warning` never changes it):
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
//! it. When an effect a module provides cannot load or run (found out
//! before the camera opens, at its load, or on a frame), the session never
//! stops for it and the state is never `error`; `effect.on_failure`
//! decides what the feed shows: `black` keeps the camera open with the
//! effect element's frames black (the room hidden, the consumers keep the
//! device), the state `starting`/`active` as usual; `none` streams the
//! camera image without the effect. Either way the reason is in `Warning`
//! (bounded; logged whole), never in `Error`, cleared once the effect is
//! ready or is no longer to run, and the effect is tried again by an
//! effect method, a reload, a module change that gives it again, or the
//! next session. A selected stage that a module provides but the running
//! session cannot run, and that is not restarted for (the second pass of a
//! session start), is named in `Warning` too. Settings the output cannot be
//! built with or that the running session refuses (`replace` without an
//! image that can be read) are different: the session would show what it
//! ran before, so with `black` they are a fault (the session stops, the
//! camera is closed and the GPU released, and the writer's placeholder
//! keeps the stream black), found out before the camera is opened when the
//! session starts. An output that cannot be built after the effect's
//! check refused the effect is that fault, in the effect's words.
//!
//! A selected stage that no module provides now (none installed or
//! enabled, its module disabled, refused by a module conflict, disabled by
//! the crash ledger or unable to load, the stage not installed for this
//! machine) is never a failure: the state is `starting`/`active` as usual,
//! `Error` is untouched and `Warning` names it with its module's reason
//! ([`Standing`], [`Controller::dropped_stages`]). Noise removal,
//! relighting, enhancement (where it applies to the camera) and a module's
//! own stages (once a module has described its switch in this process) are
//! dropped (the session is given them off), whatever `on_failure` says,
//! and the feed keeps streaming with the stages that still have a module.
//! The background effect follows `on_failure` instead, as a provided
//! effect that cannot load does ([`Controller::fails_closed`]): with
//! `black` or `image` it is asked of the effect element all the same,
//! which fails it and shows black or the fallback image (an output always
//! has the element, without a backend when no module provides the
//! effect), so the room is never shown for want of a module; with `none`
//! it is dropped (the camera image). A session that loses the module it
//! started the effect with ([`Controller::background_lost`]) follows
//! `on_failure` too, with the camera kept open: under `black` or `image`
//! the effect element drops that module's backend instance and fails the
//! effect at once ([`Controller::follow_effect_backend`]), and takes it
//! back once the module can run it again, the effect given again with no
//! restart; under `none` the effect is dropped (the camera image).
//! The choice is kept (`Effect`, the settings, the state file). A session keeps the modules whose code
//! it started with, but follows what they provide
//! ([`Controller::follow_modules`]): after a module is enabled or disabled,
//! a reload or a probe, a stage whose module can no longer run it is
//! turned off in the running session, and one it can run again is turned
//! back on (the background effect too, followed from its load; enhancement
//! by a reopen of the camera), the camera kept open otherwise; when a
//! selected stage is provided by a module the session cannot run it with
//! (it started without it, or its own module can no longer run it), the
//! session is stopped and the lifecycle starts a new one with it, once,
//! since an output built without a module cannot take it later
//! ([`Controller::restart_for`]); at a session start that learns it (the
//! modules probed again once the GPU is up), in the same step. A pin,
//! profile or order change alone applies at the next start: a stage whose
//! module in the session can still run it keeps running there.
//!
//! Noise removal (`denoise`) is a stage of its own, before the effect or
//! alone with `none`. It hides nothing, so its failure never stops the
//! session, whatever `effect.on_failure` says: the session streams on
//! without the stage, the state does not change, and `Error` says why
//! (an effect failure is in `Warning`, never over it). It is not checked before the session starts; a failure arrives
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
//! The video effects are in the tree too, as global settings (revision 8,
//! [`crate::settings::effects`]): their groups follow the camera's and are
//! published with no camera as well. They are views of the effect settings:
//! a `SetSetting` on one is validated against the tree (unknown, disabled,
//! invalid) and then is exactly `SetEffect`, `SetBlurStrength` or
//! `SetDenoise` (saved at once, applied live, a fault cleared), never held
//! by the coalescing window nor counted in the write cap; a reset of a
//! global group or setting forgets the state file's overrides, so the
//! configuration's values apply again. Every effect method, a reload and a
//! change of what is installed announce them like any other setting. Which
//! features are installed is read from the filesystem (no CUDA call) at
//! start, on a reload and when a session has started; the GPU's compute
//! capability, which picks the models of this GPU, is learnt once from the
//! first session (the GPU is up then) and kept, never asked at idle; it is
//! cached across restarts ([`crate::gpu_cache`]), so the probes before the
//! first session already pick this GPU's models. The root probed is the
//! one the process loaded the SDK from once it has, whatever the
//! configuration names since. A change of what is installed during a
//! session turns noise removal on or off in what the session was given,
//! and touches nothing else. Noise
//! removal known not to be installed is shown greyed and off, and is not
//! requested from the session at all, so it raises no error; the choice is
//! kept and applies once it is installed. When no SDK root resolves,
//! nothing is known and it is requested as before.
//!
//! Relighting of the person (`relight`, the global settings of the
//! `lighting` group: `effect.lighting.relight`, `.light`, `.direction` and
//! `.brightness`) is a stage of the output after the mask, as noise removal
//! is one before it: it follows noise removal's rules. Its settings are
//! saved at once and applied live to the running session (the switch and
//! the light through the effect element, the direction and the brightness
//! as parameters it reads at each frame, so a stream of values costs no
//! reload), and announced in one `SettingsChanged`; with no session they
//! are only stored. It needs the relighting and the green-screen features:
//! known not to be installed, it is greyed and off, never requested and
//! never an error. Its failure never stops the session (it streams on
//! without the stage) and only shows in `Error`, after a noise removal
//! failure's text and before an enhancement failure's; it is cleared as
//! noise removal's is (the stage ready or no longer requested, an effect
//! setting written or reset, a reload, the end of the session).
//!
//! Enhancement of a low-resolution camera (`enhance`, the global setting
//! `effect.video.enhance`) is a stage of the ingest, not of the output: an
//! upscale of the camera image before it is fitted to the output. It is
//! requested only for a camera whose capture mode is small enough for it to
//! pay and uploaded in a format it reads
//! ([`obc_pipeline::ingest::enhance_plan`]), and while it is not
//! known to be uninstalled; otherwise the ingest is built without it, and
//! the setting is greyed. The ingest is built with it or without it, so a
//! change of what is requested while a camera is open (the setting, a
//! reload) reopens the camera through the camera-switch path, the output
//! holding the last frame; with the camera closed it applies at the next
//! open. It hides nothing: its failure never stops the session (the stage
//! passes the camera frames through) and only shows in `Error`, after a
//! noise removal failure's text. The failure is cleared by a write or a
//! reset of the setting and by a reload (both retry the stage, reopening
//! the camera), a camera switch or reopen, and the end of the session.
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
use obc_core::config_d::{self, ConfigSet, ManagedEdit, OverridePolicy, WriteError};
use obc_core::config_schema::{self, ModuleFact, RegistryFacts, StageFact};
use obc_core::modules::{BackendProfile, StoredValue, legacy};
use obc_core::{EffectMode, EffectOnFailure, ModeRequest, OutputContract};
use obc_ipc::settings::ResetScope;
use obc_ipc::{
    CameraControl, CameraControls, CameraInfo, INTERFACE_REVISION, IpcError, SettingChange,
    SettingValue, SettingsTree, Snapshot, State, Stats,
};
use obc_module_host::registry::FRAME_GENERATION;
use obc_pipeline::capability::{
    Capabilities, IngestPath, choose_mode, mode_value, output_mode_choices,
};
use obc_pipeline::control_node::CtrlValue;
use obc_pipeline::controls::{
    CTRL_FLAG_DISABLED, CTRL_FLAG_READ_ONLY, CTRL_FLAG_UPDATE, DeviceControl, IMAGE_CONTROLS,
    check_value, image_control_id,
};
use obc_pipeline::ctrl_events::{
    CTRL_CHANGE_FLAGS, CTRL_CHANGE_RANGE, CTRL_CHANGE_VALUE, CtrlEvent,
};
use obc_pipeline::effect::{Params, StageId};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::backend::{
    ControlEvent, ControlNode, ControlScan, EffectReport, EffectRun, EffectStatus, Failure,
    MediaBackend, PipeWireEvent, PipeWireGeneration, PipeWireState, PreviewRequest, SessionParams,
    StageRun, WatchGeneration,
};
use crate::camera::{self, Camera};
use crate::gpu_cache;
use crate::mic::{self, agent::MicEvent, agent::MicLink, agent::MicRequest};
use crate::modules::{self, Modules, Plan};
use crate::preview::{Leases, PreviewDir, check_fps};
use crate::settings::catalogue::{self, DYNAMIC_FRAMERATE};
use crate::settings::effects::{self, Binding, GlobalEntry, Write};
use crate::settings::{self, CameraSettings, Entry, Inputs, Refusal, Target};
use crate::{debug, error, info, warning};

/// The longest `Warning`, in bytes (an ellipsis ends a longer one).
pub const WARNING_MAX: usize = 512;
/// The most holders a deferral warning names before "and N more".
const HOLDERS_NAMED: usize = 3;
/// The longest holder name shown, in bytes.
const HOLDER_NAME_MAX: usize = 32;
/// The holders of a deferral warning when none could be named.
pub const HOLDERS_UNKNOWN: &str = "the apps using it";

/// How often a missing or held loopback device is retried.
pub const WRITER_RETRY: Duration = Duration::from_secs(2);
/// The longest wait between two tries of a contract that found the
/// loopback held although nobody was counted.
pub const HELD_RETRY_MAX: Duration = Duration::from_secs(32);
/// A camera that delivers no frame this long after its ingest started has
/// failed.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
/// The `Stats` signal period while `active`.
pub const STATS_PERIOD: Duration = Duration::from_secs(1);
/// How often a starting session is checked for its first frame.
const POLL_STARTING: Duration = Duration::from_millis(50);
/// How often a running session is checked for failures.
const POLL_RUNNING: Duration = Duration::from_millis(250);
/// The longest the writer's placeholder is waited for while it decodes
/// the fallback image: past it the image is said not to be readable, and
/// the steps no longer come often for it (a decode that hung or died).
const PLACEHOLDER_DECODE_LIMIT: Duration = Duration::from_secs(10);
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
/// The reply channel of `GetModules`.
pub type ModulesReply = tokio::sync::oneshot::Sender<Result<obc_ipc::ModuleList, IpcError>>;
/// The reply channel of `GetStageOrder`.
pub type OrderReply = tokio::sync::oneshot::Sender<Result<Vec<String>, IpcError>>;
/// The reply channel of `GetStageOrderMoves`.
pub type MovesReply = tokio::sync::oneshot::Sender<Result<Vec<StageMove>, IpcError>>;
/// The reply channel of `GetConfigSchema`.
pub type ConfigSchemaReply = tokio::sync::oneshot::Sender<Result<obc_ipc::ConfigSchema, IpcError>>;
/// The reply channel of `GetConfigFiles`.
pub type ConfigFilesReply = tokio::sync::oneshot::Sender<Result<obc_ipc::ConfigFiles, IpcError>>;

/// One stage of `GetStageOrderMoves`: the stage, its slot, and whether the
/// daemon would accept the order with it swapped with the stage before it
/// (`up`) or after it (`down`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageMove {
    pub id: String,
    pub slot: String,
    pub up: bool,
    pub down: bool,
}

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
    /// `GetModules()`.
    GetModules(ModulesReply),
    /// `SetModuleEnabled(s, b)`.
    SetModuleEnabled(String, bool, Reply),
    /// `GetStageOrder()`.
    GetStageOrder(OrderReply),
    /// `SetStageOrder(as)` (empty: the default order).
    SetStageOrder(Vec<String>, Reply),
    /// `SetBackendProfile(s)`.
    SetBackendProfile(String, Reply),
    /// `SetStagePin(s, s)` (an empty module clears the pin).
    SetStagePin(String, String, Reply),
    /// `GetStageOrderMoves()`.
    GetStageOrderMoves(MovesReply),
    /// `GetConfigSchema()`.
    GetConfigSchema(ConfigSchemaReply),
    /// `SetConfig(s, v)`: the value as decoded (`None`: a D-Bus type no
    /// key takes).
    SetConfig(String, Option<obc_ipc::ConfigValue>, Reply),
    /// `ResetConfig(s)`.
    ResetConfig(String, Reply),
    /// `GetConfigFiles()`.
    GetConfigFiles(ConfigFilesReply),
    /// `ReloadConfig()` (with its reply) or SIGHUP (without).
    ReloadConfig(Option<Reply>),
    /// The consumer watcher's count of processes holding the loopback.
    Consumers(u32),
    /// An event of the PipeWire output of that generation (through the
    /// backend's reporter).
    PipeWire(PipeWireGeneration, PipeWireEvent),
    /// Video nodes appeared or disappeared: scan the cameras again.
    Rescan,
    /// `SetMicHost(b)` (revision 13).
    SetMicHost(bool, Reply),
    /// What the microphone's agent found or did ([`crate::mic::agent`]).
    Mic(MicEvent),
    /// Stop everything and end the controller loop.
    Shutdown,
}

/// One changed property, with its new value.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    State(State),
    Error(String),
    Warning(String),
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
    ModulesSerial(u32),
    BackendProfile(String),
    ConfigSerial(u32),
    MicState(String),
    MicConsumers(u32),
    MicProvider(String),
    MicLatencyMs(f64),
    MicError(String),
}

impl Change {
    /// The D-Bus property name.
    pub fn name(&self) -> &'static str {
        match self {
            Change::State(_) => "State",
            Change::Error(_) => "Error",
            Change::Warning(_) => "Warning",
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
            Change::ModulesSerial(_) => "ModulesSerial",
            Change::BackendProfile(_) => "BackendProfile",
            Change::ConfigSerial(_) => "ConfigSerial",
            Change::MicState(_) => "MicState",
            Change::MicConsumers(_) => "MicConsumers",
            Change::MicProvider(_) => "MicProvider",
            Change::MicLatencyMs(_) => "MicLatencyMs",
            Change::MicError(_) => "MicError",
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
    /// The configuration in force or the config schema changed
    /// (`ConfigChanged`): sent after the snapshot with the new
    /// `ConfigSerial`.
    ConfigChanged(u32),
    /// `MicStats(overruns, misses, voice)`, relayed from the audio worker
    /// (not by the controller).
    MicStats(u32, u32, f64),
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

    fn config_changed(&self, serial: u32) {
        let _ = self.notes.send(Note::ConfigChanged(serial));
    }

    /// Announces a preview frame (`PreviewFrame(seq)`): called from the
    /// preview encoder's thread, through the backend's preview reporter.
    pub fn preview_frame(&self, seq: u32) {
        let _ = self.notes.send(Note::PreviewFrame(seq));
    }

    /// Announces the audio worker's counters (`MicStats`): called by the
    /// microphone's agent.
    pub fn mic_stats(&self, overruns: u32, misses: u32, voice: f64) {
        let _ = self.notes.send(Note::MicStats(overruns, misses, voice));
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
    if old.modules_serial != new.modules_serial {
        out.push(Change::ModulesSerial(
            new.modules_serial.unwrap_or_default(),
        ));
    }
    if old.backend_profile != new.backend_profile {
        out.push(Change::BackendProfile(
            new.backend_profile.clone().unwrap_or_default(),
        ));
    }
    if old.config_serial != new.config_serial {
        out.push(Change::ConfigSerial(new.config_serial.unwrap_or_default()));
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
    if old.warning != new.warning {
        out.push(Change::Warning(new.warning.clone()));
    }
    if old.mic != new.mic {
        let (o, n) = (
            old.mic.clone().unwrap_or_default(),
            new.mic.clone().unwrap_or_default(),
        );
        if o.consumers != n.consumers {
            out.push(Change::MicConsumers(n.consumers));
        }
        if o.provider != n.provider {
            out.push(Change::MicProvider(n.provider.clone()));
        }
        if o.latency_ms.to_bits() != n.latency_ms.to_bits() {
            out.push(Change::MicLatencyMs(n.latency_ms));
        }
        if o.error != n.error {
            out.push(Change::MicError(n.error.clone()));
        }
        if o.state != n.state {
            out.push(Change::MicState(n.state));
        }
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

/// The config file, the runtime state file, the runtime directory and the
/// cache directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
    /// `$XDG_CACHE_HOME/omarchy-broadcast`, where what was learnt about the
    /// GPU is kept ([`crate::gpu_cache`]); `None` when it cannot be found,
    /// and then nothing is kept.
    pub cache: Option<PathBuf>,
    /// `$XDG_RUNTIME_DIR`, where the preview frames go
    /// ([`crate::preview::PreviewDir`]); `None` when it is not set, and
    /// then `AcquirePreview` fails.
    pub runtime: Option<PathBuf>,
    /// The modules' crash ledger (`modules-crash.json` in the state file's
    /// directory, [`crate::modules::LEDGER_FILE`]); `None`: none is kept.
    pub ledger: Option<PathBuf>,
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
    let config = ConfigSet::load(&paths.config)
        .map(|s| s.config)
        .unwrap_or_default();
    let (mut runtime, damaged, _) = load_runtime_state(&paths.state);
    // `[settings]` over the typed keys of revision 8, as the controller reads them.
    runtime.migrate_legacy_settings();
    let effect = runtime.effect_over(&config.effect);
    Snapshot {
        state: if runtime.enabled {
            State::Idle
        } else {
            State::Off
        },
        error: damaged.unwrap_or_default(),
        // Which modules provide what is not known before they are opened.
        warning: String::new(),
        enabled: runtime.enabled,
        cameras: Vec::new(),
        active_camera: String::new(),
        output_mode: output_mode_string(&config.output_contract()),
        effect: effect.mode.as_str().into(),
        blur_strength: effect.blur_strength,
        background_image: image_string(&effect),
        denoise: denoise_choice(&config, &runtime),
        consumers: 0,
        loopback_consumers: 0,
        pipewire_consumers: 0,
        preview_consumers: 0,
        pipewire_output: PipeWireState::Off.as_str().into(),
        camera_controls: Some(CameraControls::default()),
        settings_revision: Some(1),
        settings_schema_serial: Some(1),
        modules_serial: Some(1),
        // The state file's choice until it moves into the configuration
        // (the controller's first snapshot follows the configuration).
        backend_profile: Some(
            runtime
                .modules
                .profile(&config.modules)
                .as_str()
                .to_string(),
        ),
        config_serial: Some(1),
        // What the mic host runs is not known before the agent asked.
        mic: Some(obc_ipc::MicStatus::default()),
        interface_revision: INTERFACE_REVISION,
    }
}

/// `Denoise`: noise removal chosen (the state file's choice over the
/// config's), whether or not a module provides it.
fn denoise_choice(config: &Config, runtime: &RuntimeState) -> bool {
    match runtime.settings.get(legacy::DENOISE) {
        Some(StoredValue::Bool(b)) => *b,
        _ => runtime.denoise.unwrap_or(config.effect.denoise),
    }
}

/// The switch of `stage` from the core's own keys, for noise removal,
/// relighting and enhancement (the state file's value, else the config's
/// `[effect]` key); off for any other stage.
fn core_switch(config: &Config, runtime: &RuntimeState, stage: &StageId) -> bool {
    let key = if *stage == StageId::DENOISE {
        return denoise_choice(config, runtime);
    } else if *stage == StageId::RELIGHT {
        legacy::RELIGHT
    } else if *stage == StageId::ENHANCE {
        legacy::ENHANCE
    } else {
        return false;
    };
    match runtime.settings.get(key) {
        Some(StoredValue::Bool(b)) => *b,
        _ => config.effect.legacy_settings().get(key) == Some(&StoredValue::Bool(true)),
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

/// The background effect and its fallback, of the settings in force or of
/// what a session was given.
trait Fallback {
    fn mode(&self) -> EffectMode;
    fn on_failure(&self) -> EffectOnFailure;
}

impl Fallback for EffectConfig {
    fn mode(&self) -> EffectMode {
        self.mode
    }
    fn on_failure(&self) -> EffectOnFailure {
        self.on_failure
    }
}

impl Fallback for EffectRun {
    fn mode(&self) -> EffectMode {
        self.mode
    }
    fn on_failure(&self) -> EffectOnFailure {
        self.on_failure
    }
}

/// The text of an effect that cannot run where it is an error: a fault
/// (`Error`) or a method's answer.
fn effect_failure_text(effect: &impl Fallback, reason: &str) -> String {
    match effect.on_failure() {
        EffectOnFailure::Black | EffectOnFailure::Image => {
            format!("effect {} unavailable: {reason}", effect.mode())
        }
        EffectOnFailure::None => format!(
            "effect {} unavailable: {reason}; the camera image is shown without the effect \
             (effect.on_failure = \"none\")",
            effect.mode()
        ),
    }
}

/// The `Warning` clause of the background effect, provided by a module
/// but unable to load or run (`reason`): the session streams on, with
/// `effect.on_failure = "black"` the effect element's frames black until
/// the effect runs (with `"image"` the fallback image), with `"none"` the
/// camera image. The reason is made
/// safe to show and bounded ([`STAGE_REASON_MAX`], as a stage's in
/// `Error`: a module's hint at its end survives).
fn effect_warning_text(effect: &impl Fallback, reason: &str) -> String {
    let reason = settings::sanitise(reason, STAGE_REASON_MAX);
    match effect.on_failure() {
        EffectOnFailure::Black => format!(
            "effect {} unavailable: {reason}; the video is black until it can run",
            effect.mode()
        ),
        EffectOnFailure::Image => format!(
            "effect {} unavailable: {reason}; the fallback image is shown until it can run",
            effect.mode()
        ),
        EffectOnFailure::None => format!(
            "effect {} unavailable: {reason}; the camera image is shown without the effect \
             (effect.on_failure = \"none\")",
            effect.mode()
        ),
    }
}

/// The `Warning` clauses of the selected stages that no module provides
/// (label, module name, reason; [`Controller::dropped_stages`]): those
/// that share a module and a reason in one clause, the labels first (so a
/// client that clips the text cuts a reason, never the list), in the
/// order of their first stage.
fn dropped_text(dropped: &[(String, Option<String>, String)]) -> Vec<String> {
    let mut groups: Vec<(&Option<String>, &str, Vec<&str>)> = Vec::new();
    for (label, module, reason) in dropped {
        match groups
            .iter_mut()
            .find(|(m, r, _)| *m == module && *r == reason.as_str())
        {
            Some(g) => g.2.push(label),
            None => groups.push((module, reason, vec![label])),
        }
    }
    groups
        .into_iter()
        .map(|(module, reason, labels)| {
            let labels = labels.join(", ");
            match module {
                Some(name) => format!("{labels} unavailable: {name}: {reason}"),
                None => format!("{labels} unavailable: {reason}"),
            }
        })
        .collect()
}

/// The modules whose code a session of `plan` runs, and where: the effect
/// module, the enhancer and each chain's module by chain.
fn plan_ids(plan: &Plan) -> (Option<&str>, Option<&str>, Vec<(&str, &str)>) {
    (
        plan.effect.as_ref().map(|m| m.id.as_str()),
        plan.enhance.as_ref().map(|m| m.id.as_str()),
        plan.chains
            .iter()
            .map(|c| (c.name.as_str(), c.module.id.as_str()))
            .collect(),
    )
}

/// The longest failure reason `Error` carries for a stage, and `Warning`
/// for the background effect, in bytes.
const STAGE_REASON_MAX: usize = 400;

/// The longest stage label `Warning` carries, in bytes (a module's
/// switch label is bounded so already).
const LABEL_MAX: usize = obc_module_host::settings::LABEL_MAX;

/// The `Warning` reason of a selected stage that a module provides but the
/// running session does not run (started without that module): only a new
/// session runs it.
const REASON_NEXT_START: &str = "it runs when the camera next starts";

/// The `Error` text of stage `id`, labelled `label`, that cannot run (the
/// session streams on without it): the reason made safe to show and
/// bounded ([`settings::sanitise`]), worded to be true under every effect
/// (with blur or replace the video is the effect without the stage, not
/// the camera image). Noise removal keeps the words of revision 8, whose
/// reason is the backend's own.
fn stage_failure_text(id: &StageId, label: &str, reason: &str) -> String {
    if *id == StageId::DENOISE {
        return format!("{label} unavailable: {reason}; the camera image is shown without it");
    }
    format!(
        "{label} unavailable: {}; the video continues without it",
        settings::sanitise(reason, STAGE_REASON_MAX)
    )
}

/// The `Error` text of enhancement that cannot run (the session streams on
/// without it): the reason made safe to show and bounded
/// ([`settings::sanitise`]), worded as [`stage_failure_text`].
fn enhance_failure_text(reason: &str) -> String {
    format!(
        "enhance unavailable: {}; the video continues without it",
        settings::sanitise(reason, STAGE_REASON_MAX)
    )
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

/// A new contract that found the loopback held in another format although
/// nobody was counted ([`Controller::held_retry`]): it and the loopback it
/// was tried on, when it is tried again, and the wait that led there.
#[derive(Debug, Clone, PartialEq)]
struct HeldRetry {
    contract: OutputContract,
    sink: obc_core::SinkSelector,
    at: Instant,
    delay: Duration,
}

/// A capture mode chosen for a camera that could not open
/// ([`Controller::mode_failed`]).
#[derive(Debug, Clone, PartialEq)]
struct ModeFailed {
    /// What was asked of the camera.
    asked: ModeRequest,
    /// Why it could not open in the mode chosen for it.
    why: String,
    /// The camera's modes then: the record goes when they change.
    modes: Vec<obc_core::VideoMode>,
    /// The camera delivered a frame in its automatic mode since: the
    /// chosen mode is to blame. Until then the record only steers the
    /// retry: a camera that cannot open in its automatic mode either is
    /// not failing for its mode, and the record goes
    /// ([`Controller::forget_unconfirmed_mode`]); `Warning` shows only a
    /// confirmed record.
    confirmed: bool,
}

/// Why privacy holds the feed ([`Controller::privacy_hold`]).
#[derive(Debug, Clone, PartialEq)]
enum PrivacyHold {
    /// A start or a change the effect refused set this fault.
    Fault(String),
    /// The guard dropped a session that streamed without the effect.
    Guard,
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
    /// When the current camera's ingest started, until its first frame (a
    /// reopen of the same camera before then keeps it: the start timeout
    /// runs from the first open).
    awaiting: Option<Instant>,
    /// The ingest's switch to `camera` succeeded: the session has it open
    /// (`starting` or `active`), so the image control methods write to it.
    open: bool,
    /// That ingest was built with `fixed_framerate`: it pins
    /// `exposure_dynamic_framerate` and puts its value back at the close.
    pinned: bool,
    /// That ingest was built with the enhance stage.
    enhance: bool,
    /// With these load-time parameters of the stage.
    enhance_params: Params,
    /// The output contract the session output started with: the last
    /// good one once a camera frame has reached it
    /// ([`Controller::output_proven`]).
    contract: OutputContract,
}

/// The lifecycle state machine (see the module docs).
pub struct Controller<B: MediaBackend> {
    backend: B,
    paths: Paths,
    /// The configuration in force: [`Self::config_set`]'s.
    config: Config,
    /// The files it was loaded from (`config.toml`, then `config.d/`), with
    /// where each key comes from; the built-in defaults when the set did
    /// not load at start.
    config_set: ConfigSet,
    /// `ConfigSerial`: from 1, bumped at every change of the configuration
    /// in force or of its files (a write, a reload, also a refused one, the
    /// move of the state file's module choices) and of the module rows of
    /// the config schema (the modules found).
    config_serial: u32,
    /// The `ConfigSerial` last announced with `ConfigChanged`.
    config_announced: u32,
    /// The module registry's facts the config schema was last made from,
    /// and the `ModulesSerial` they were read at.
    config_facts: (u32, RegistryFacts),
    runtime: RuntimeState,
    cameras: Vec<Camera>,
    writer: Option<WriterOn>,
    /// Why the writer could not start (the last attempt).
    writer_error: Option<String>,
    writer_retry_at: Option<Instant>,
    /// A deferred writer rebuild found the loopback held although the
    /// watcher counted nobody: when to count again.
    rebuild_recount_at: Option<Instant>,
    /// A new contract found the loopback held in another format although
    /// nobody was counted: the last good writer streams instead, the
    /// deferral shows in `Warning`, and that contract is tried again at
    /// the time given, after the delay given (doubled at each try, up to
    /// [`HELD_RETRY_MAX`]: each try stops the writer). It exists only for
    /// the contract and the loopback wanted now
    /// ([`Self::forget_stale_held_retry`]). `None`: no such fallback runs.
    held_retry: Option<HeldRetry>,
    /// What [`Self::holder_names`] were read for: the loopback count and
    /// the output contract waiting (`None`: no output change waits).
    holders_read: Option<(u32, OutputContract)>,
    /// The process names holding the loopback while an output change waits
    /// for them, for `Warning`.
    holder_names: Vec<String>,
    /// The contract of the last writer that started in this run.
    writer_good: Option<OutputContract>,
    /// The contract of the last session output that started in this run.
    session_good: Option<OutputContract>,
    /// A configured output contract that could not start in this run
    /// (its writer or its session output, not a loopback a consumer
    /// holds): it, why, and the contract streamed instead. It stands while
    /// the configuration asks for that contract ([`Self::wanted_contract`]);
    /// the configuration files are not rewritten.
    failed_contract: Option<(OutputContract, String, OutputContract)>,
    session: Option<Streaming>,
    /// The cameras, by id, whose chosen capture mode could not open before
    /// a first frame in this run: they capture in the automatic mode (the
    /// config's `camera.mode`, [`Self::mode_request_for`]) and `Warning`
    /// says so, until the mode is chosen again, the choice is forgotten,
    /// or the camera goes or reports other modes. The choice stays in the
    /// state file.
    mode_failed: HashMap<String, ModeFailed>,
    /// A lifecycle fault: `error` until its cause can have changed.
    fault: Option<String>,
    /// Privacy holds the feed: no session streams because the effect, under
    /// an `on_failure` that hides the camera, refused it (the fault it set)
    /// or the guard dropped it ([`Self::streams_without_black_effect`]).
    /// Held until a session streams (is live): with no session, while that
    /// fault stands; a start tried again under the hold keeps it until its
    /// first frame, or until it is refused or fails. Disabled or without a
    /// consumer, it ends. With `on_failure = "image"` the writer's
    /// placeholder shows the fallback image meanwhile
    /// ([`Self::placeholder_image`]).
    privacy_hold: Option<PrivacyHold>,
    /// The image the writer's placeholder was last given (`None`: black).
    placeholder_set: Option<PathBuf>,
    /// Why the writer's placeholder cannot show that image (a `Warning`
    /// clause).
    placeholder_failure: Option<String>,
    /// The writer is still decoding that image: whether it can show it is
    /// not known yet, so the steps come often until it is
    /// ([`Self::next_wakeup`]).
    placeholder_pending: bool,
    /// When a step first saw that decode pending ([`Self::watch_placeholder`]).
    placeholder_pending_since: Option<Instant>,
    /// That decode has been pending for [`PLACEHOLDER_DECODE_LIMIT`]: the
    /// image is said not to be readable, and the steps no longer come often.
    placeholder_overdue: bool,
    /// Why the running session's effect element cannot show the fallback
    /// image, while its effect has failed (a `Warning` clause).
    fallback_failure: Option<String>,
    /// The `Error` property.
    error: String,
    /// The state file could not be read or parsed: why (the `Error` text).
    /// The daemon runs `off` and leaves the file alone until `SetEnabled`.
    state_damaged: Option<String>,
    /// The last save of the state file failed: the `Failed` text it gave
    /// (cleared by the next save that succeeds). A settings change is then
    /// saved at once, so its caller learns whether it survives a restart.
    save_failure: Option<String>,
    /// The background effect, provided by a module, cannot load or run:
    /// the session streams on (with `effect.on_failure = "black"` the
    /// effect element's frames black, the room hidden; with `"none"` the
    /// camera image), and this is the first clause of `Warning`
    /// ([`effect_warning_text`]). Cleared when the effect becomes ready,
    /// by a reload, the next session start or the end of the session,
    /// when no background effect is to run (none selected, or dropped for
    /// want of a module: [`Self::follow_background`]), and by an effect
    /// method under `none` (under `black` it stands until the retry
    /// reports: the frames stay black meanwhile).
    effect_failure: Option<String>,
    /// The `Warning` text last published (what [`Self::publish_with`]
    /// logs a change of).
    warning_shown: String,
    /// The effect status last polled in this session (`None`: not polled
    /// since the session started or the effect was last applied, so the
    /// next status is reported even if it is the same failure again).
    effect_seen: Option<EffectStatus>,
    /// The effect has been polled in this session.
    effect_polled: bool,
    /// Each stage of the session apart from the effect (noise removal,
    /// relighting, a module's stage), in stage order: whether it cannot run
    /// (the session streams without it, and its `Error` text, behind an
    /// effect failure's and those of the stages before it), its last
    /// polled status and a failure held for the next poll (see
    /// [`Self::poll_stage`]).
    stages: Vec<(StageId, StageTrack)>,
    /// Enhancement cannot run: the session streams without it, and this is
    /// its `Error` text (behind the stages' failures). Cleared by a
    /// write or reset of the setting, a reload, a camera switch or reopen,
    /// and the end of the session.
    enhance_failure: Option<String>,
    /// The enhancement status last polled for the current ingest (`None`:
    /// not polled since it was built).
    enhance_seen: Option<EffectStatus>,
    /// The effect settings the running session was last given (`None`: no
    /// session). Only a change of the effect, its image or the noise
    /// removal request, or a failure of either, starts a new load to
    /// follow; a strength change does not.
    effect_applied: Option<EffectRun>,
    /// When the effect's GPU time is logged next.
    effect_log_at: Option<Instant>,
    /// The modules were discovered and opened ([`Self::load_modules`]):
    /// until then nothing is known of what they provide, and `Warning`
    /// says nothing.
    modules_loaded: bool,
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
    /// The modules: what is installed and enabled, and what each offers
    /// ([`crate::modules`]).
    modules: Modules,
    /// The running session's plan: which module runs each stage, fixed
    /// when it started (`None`: no session).
    plan: Option<Plan>,
    /// The module of [`Self::plan`] could run the background effect when
    /// the session started ([`Self::background_lost`]).
    background_planned: bool,
    /// The running session was last given a background effect that no
    /// module of its plan could run ([`Self::fails_closed`]), as of the
    /// last publication: once one can, it is given again
    /// ([`Self::follow_background`]).
    background_closed: bool,
    /// The running session's effect element was made to drop its backend
    /// ([`Self::follow_effect_backend`]): the module the session started
    /// the background effect with can no longer run it.
    background_revoked: bool,
    /// The running session's output could not start with its effects and
    /// module stages and streams without them (its plan is empty): the
    /// `Warning` clause saying so. Ends with the session.
    passthrough: Option<String>,
    /// The modules whose code the running session has run (marked in the
    /// crash ledger until it stops): those of what it was given, added to
    /// as the effect or the ingest changes ([`Self::mark_running`]).
    marked: Vec<String>,
    /// The GPU's compute capability: the one an earlier run learnt
    /// ([`crate::gpu_cache`]) until a session of this process reads it
    /// while it has the GPU up (never at idle: the GPU may be parked); then
    /// kept for the process, and in the cache when it differs.
    compute_capability: Option<(u32, u32)>,
    /// A session of this process has read [`Self::compute_capability`].
    compute_capability_learnt: bool,
    published: Published,
    sink_hook: Option<SinkHook>,
    /// The microphone's agent ([`crate::mic::agent`]); `None` without one
    /// (the tests that do not test the microphone).
    mic: Option<MicLink>,
    /// The `Mic*` properties, as the agent last told them.
    mic_status: obc_ipc::MicStatus,
    /// The worker's modules, settings and devices, as the agent last told
    /// them.
    mic_catalogue: mic::Catalogue,
    /// The audio settings snapshot last sent to the agent.
    mic_sent: Option<String>,
    /// `mic.device` as of the last snapshot sent (`None` before the first):
    /// a change while the Omarchy Broadcast Mic is the default re-pins the
    /// capture side.
    mic_device_sent: Option<String>,
    /// Bumped when the worker's modules change: `ModulesSerial` is the
    /// registry's serial plus this.
    mic_serial: u32,
}

/// Where a stage the user may have selected stands for a plan (the
/// running session's, or the one a session would start with): see
/// [`Controller::standing`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Standing {
    /// Not selected: the background effect `none`, a stage switched off,
    /// enhancement for a camera it does not apply to, or a stage whose
    /// switch is not known.
    Off,
    /// The plan's module for it can run it now (whatever a pin or the
    /// profile would choose at the next start).
    Runs,
    /// No module provides it (none installed or enabled, its module
    /// disabled, refused, crashed or unavailable, the stage not
    /// installed): the stage is dropped and the feed streams on without
    /// it; the background effect follows `on_failure`
    /// ([`Controller::fails_closed`]). The module that would run it (its
    /// name) and why it cannot, for `Warning`.
    Missing {
        module: Option<String>,
        reason: String,
    },
    /// A module provides it, but not one the running session can run it
    /// with (a session started without one, or its own module disabled
    /// since, or unable to run it any more): only a new session runs it.
    Elsewhere,
}

/// What the controller follows of one stage of the session apart from the
/// effect ([`Controller::poll_stage`]).
#[derive(Debug, Clone, Default)]
struct StageTrack {
    /// The stage cannot run: the `Error` text. Cleared when the stage
    /// becomes ready or is no longer requested, by an effect method, a
    /// reload, the next session start or the end of the session.
    failure: Option<String>,
    /// The status last polled, as [`Controller::effect_seen`] is for the
    /// effect.
    seen: Option<EffectStatus>,
    /// Polled in this session.
    polled: bool,
    /// A failure polled once while the effect was loading or ready, held
    /// for the next poll: it may be the whole chain's failure, read before
    /// the effect's status showed it.
    held: Option<EffectStatus>,
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
    /// the runtime state, loads the modules, scans the cameras and starts
    /// the writer.
    pub fn new(
        backend: B,
        paths: Paths,
        published: Published,
        sink_hook: Option<SinkHook>,
        now: Instant,
    ) -> Self {
        Self::new_with(backend, paths, published, sink_hook, now, true)
    }

    /// [`Self::new`] without the modules: none is discovered, opened or
    /// counted in the crash ledger until [`Self::load_modules`]. What the
    /// daemon starts with, so that an instance that does not get the bus
    /// name leaves the ledger alone.
    pub fn new_without_modules(
        backend: B,
        paths: Paths,
        published: Published,
        sink_hook: Option<SinkHook>,
        now: Instant,
    ) -> Self {
        Self::new_with(backend, paths, published, sink_hook, now, false)
    }

    /// Connects the microphone's agent: the audio settings snapshot is sent
    /// to it now and at every change ([`crate::mic`]).
    pub fn set_mic(&mut self, link: MicLink) {
        self.mic = Some(link);
        self.mic_sent = None;
        self.mic_device_sent = None;
        self.push_mic_snapshot();
    }

    /// Discovers, opens and probes the modules (counting the crash ledger
    /// for this start) and publishes what they offer. For a controller made
    /// with [`Self::new_without_modules`], once, before any command.
    pub fn load_modules(&mut self, now: Instant) {
        let mut loaded = Modules::load(
            &mut self.backend,
            &self.config,
            self.paths.ledger.as_deref(),
            self.compute_capability,
        );
        // The empty list was published before: the loaded one is a change.
        loaded.follow(&self.modules);
        self.modules = loaded;
        self.modules_loaded = true;
        self.reconcile(now);
    }

    fn new_with(
        backend: B,
        paths: Paths,
        published: Published,
        sink_hook: Option<SinkHook>,
        now: Instant,
        load_modules: bool,
    ) -> Self {
        let (config_set, error) = match ConfigSet::load(&paths.config) {
            Ok(set) => (set, String::new()),
            Err(e) => {
                let msg = format!("{e}; running on the default configuration");
                error!("{msg}");
                (ConfigSet::defaults(&paths.config), msg)
            }
        };
        if let Some(note) = maxine_root_note(
            None,
            config_set.config.effect.maxine_root.as_deref(),
            maxine_root_env().as_deref(),
            None,
            false,
        ) {
            warning!("{note}");
        }
        let (mut runtime, state_damaged, notes) = load_runtime_state(&paths.state);
        // The typed effect keys of an older state file are module settings
        // now, saved by their setting ids from the next save on.
        if runtime.migrate_legacy_settings() {
            debug!("the state file's effect settings are kept by setting id from now on");
        }
        // The module choices of an older state file move into the
        // configuration (a damaged state file has none to move). A set that
        // did not load has nothing to write them into: they stay, to move
        // at a reload that succeeds.
        let config_set = if state_damaged.is_none() && error.is_empty() {
            migrate_choices(&paths, config_set, &mut runtime).0
        } else {
            config_set
        };
        let config = in_force(&config_set.config, &runtime);
        // Learnt by an earlier run: the models of this GPU are known from
        // the first probe, with no GPU call at idle.
        let compute_capability = paths.cache.as_deref().and_then(gpu_cache::load);
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
            if denoise_choice(&config, &runtime) {
                "on"
            } else {
                "off"
            }
        );
        let mut backend = backend;
        let modules = if load_modules {
            Modules::load(
                &mut backend,
                &config,
                paths.ledger.as_deref(),
                compute_capability,
            )
        } else {
            Modules::empty()
        };
        let config_facts = (modules.serial(), registry_facts(&modules));
        let mut c = Controller {
            backend,
            paths,
            config,
            config_set,
            config_serial: 1,
            config_announced: 1,
            config_facts,
            runtime,
            cameras: Vec::new(),
            writer: None,
            writer_error: None,
            writer_retry_at: None,
            rebuild_recount_at: None,
            held_retry: None,
            holders_read: None,
            holder_names: Vec::new(),
            writer_good: None,
            session_good: None,
            failed_contract: None,
            session: None,
            mode_failed: HashMap::new(),
            fault: None,
            privacy_hold: None,
            placeholder_set: None,
            placeholder_failure: None,
            placeholder_pending: false,
            placeholder_pending_since: None,
            placeholder_overdue: false,
            fallback_failure: None,
            error,
            state_damaged,
            save_failure: None,
            effect_failure: None,
            warning_shown: String::new(),
            effect_seen: None,
            effect_polled: false,
            stages: Vec::new(),
            enhance_failure: None,
            enhance_seen: None,
            effect_applied: None,
            effect_log_at: None,
            modules_loaded: load_modules,
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
            modules,
            plan: None,
            background_planned: false,
            background_closed: false,
            background_revoked: false,
            passthrough: None,
            marked: Vec::new(),
            compute_capability,
            compute_capability_learnt: false,
            published,
            sink_hook,
            mic: None,
            mic_status: obc_ipc::MicStatus::default(),
            mic_catalogue: mic::Catalogue::default(),
            mic_sent: None,
            mic_device_sent: None,
            mic_serial: 0,
        };
        c.rescan_cameras();
        // The output modes the cameras found offer are known from the
        // first schema.
        c.config_facts.1 = c.schema_facts();
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
                    Some(ResetScope::Camera) => {
                        let forgot = self.forget_camera_mode(&camera, now);
                        let res = self.reset(&camera, ResetWhat::Camera, now);
                        if forgot {
                            self.follow_camera_modes(now);
                            self.reconcile(now);
                        }
                        res
                    }
                    Some(ResetScope::Group(g)) if g == catalogue::CAPTURE => self.set_camera_mode(
                        &camera,
                        Some(&SettingValue::Str(settings::MODE_AUTO.into())),
                        now,
                    ),
                    Some(ResetScope::Setting(id)) if id == settings::MODE => self.set_camera_mode(
                        &camera,
                        Some(&SettingValue::Str(settings::MODE_AUTO.into())),
                        now,
                    ),
                    Some(ResetScope::Group(g)) if self.mic_group_of(&g).is_some() => {
                        let g = self.mic_group_of(&g).unwrap_or(g);
                        self.reset_mic(&ResetWhat::Group(g), now)
                    }
                    Some(ResetScope::Setting(id)) if mic::is_mic(&id) => {
                        self.reset_mic(&ResetWhat::Setting(id), now)
                    }
                    Some(ResetScope::Group(g)) => {
                        let what = ResetWhat::Group(g);
                        match self.reset_global(&what, now) {
                            Some(res) => res,
                            None => self.reset(&camera, what, now),
                        }
                    }
                    Some(ResetScope::Setting(id)) => {
                        let what = ResetWhat::Setting(id);
                        match self.reset_global(&what, now) {
                            Some(res) => res,
                            None => self.reset(&camera, what, now),
                        }
                    }
                };
                let _ = reply.send(res);
            }
            Command::ControlEvent(generation, event) => {
                self.control_event(generation, event, now);
                self.reconcile(now);
            }
            Command::GetModules(reply) => {
                let _ = reply.send(Ok(self.module_list()));
            }
            Command::SetModuleEnabled(id, on, reply) => {
                let res = self.set_module_enabled(&id, on, now);
                let _ = reply.send(res);
            }
            Command::GetStageOrder(reply) => {
                let _ = reply.send(Ok(self.order_strings()));
            }
            Command::SetStageOrder(order, reply) => {
                let res = self.set_stage_order(order, now);
                let _ = reply.send(res);
            }
            Command::SetBackendProfile(profile, reply) => {
                let res = self.set_backend_profile(&profile, now);
                let _ = reply.send(res);
            }
            Command::SetStagePin(stage, module, reply) => {
                let res = self.set_stage_pin(&stage, &module, now);
                let _ = reply.send(res);
            }
            Command::GetStageOrderMoves(reply) => {
                let _ = reply.send(Ok(self.stage_moves()));
            }
            Command::GetConfigSchema(reply) => {
                let _ = reply.send(Ok(self.config_schema()));
            }
            Command::SetConfig(key, value, reply) => {
                let res = self.set_config(&key, value, now);
                let _ = reply.send(res);
            }
            Command::ResetConfig(key, reply) => {
                let res = self.reset_config(&key, now);
                let _ = reply.send(res);
            }
            Command::GetConfigFiles(reply) => {
                let _ = reply.send(Ok(self.config_files()));
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
            Command::SetMicHost(on, reply) => self.set_mic_host(on, reply),
            Command::Mic(event) => self.mic_event(event),
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
            && let Some(t) = self.rebuild_recount_at.or_else(|| self.held_retry_due())
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
        // The placeholder image's decode ends off this thread: its outcome
        // (a `Warning` when it cannot be read) is looked for soon, until
        // it is overdue.
        if self.placeholder_pending && !self.placeholder_overdue {
            at(now + POLL_RUNNING);
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
    /// a fault, the stages' failures in stage order (a noise removal
    /// failure, then a relighting failure), an enhancement failure, a
    /// damaged state file, in that order), or empty: what clearing an
    /// error whose own cause is gone leaves. A failure of the background
    /// effect is never one: it is in `Warning` ([`Self::effect_failure`]).
    fn standing_error(&self) -> String {
        if self.writer.is_none()
            && let Some(e) = &self.writer_error
        {
            return e.clone();
        }
        if let Some(f) = &self.fault {
            return f.clone();
        }
        // In stage order: noise removal's before relighting's.
        let run = match &self.effect_applied {
            Some(e) => e.clone(),
            None => self.effect_run(),
        };
        let order: Vec<StageId> = run
            .stages
            .iter()
            .chain(run.chains.iter().flat_map(|c| c.stages.iter()))
            .map(|s| s.id.clone())
            .collect();
        let rank = |id: &StageId| order.iter().position(|o| o == id).unwrap_or(usize::MAX);
        if let Some((_, t)) = self
            .stages
            .iter()
            .filter(|(_, t)| t.failure.is_some())
            .min_by_key(|(id, _)| rank(id))
        {
            return t.failure.clone().unwrap_or_default();
        }
        if let Some(e) = &self.enhance_failure {
            return e.clone();
        }
        self.state_damaged.clone().unwrap_or_default()
    }

    /// Shows the highest cause still standing ([`Self::standing_error`])
    /// in `Error` when there is one: a cause just raised shows unless a
    /// higher one holds, and never overwrites it.
    fn show_standing_error(&mut self) {
        let standing = self.standing_error();
        if !standing.is_empty() {
            self.error = standing;
        }
    }

    /// The effect settings in force: the state file's over the config's.
    pub fn effect(&self) -> EffectConfig {
        self.runtime.effect_over(&self.config.effect)
    }

    /// The session's plan: the running session's, else the one a session
    /// would start with now.
    fn current_plan(&self) -> Plan {
        match &self.plan {
            Some(p) => p.clone(),
            None => self.fresh_plan(),
        }
    }

    /// The plan a session started now would run: the modules' plan, with
    /// frame generation after every other stage when a module provides it
    /// and the output rate could be generated: twice the rate of a mode a
    /// known camera offers ([`Self::frame_generation_possible`]), not only
    /// of the mode the session starts in. The stage is on only while the
    /// output rate is twice the camera's ([`Self::frame_generation_follows`]),
    /// so a capture mode change or a fail-over mid-session turns it on or
    /// off without a restart; off, its element passes the frames through
    /// and loads nothing.
    fn fresh_plan(&self) -> Plan {
        let mut plan = self.modules.plan();
        if self.frame_generation_possible() {
            self.modules.add_frame_generation(&mut plan);
        }
        plan
    }

    /// Whether the output contract's rate is twice `camera`'s capture
    /// rate (within 0.2%): frame generation makes the frames between.
    fn frame_generation_wanted(&self, camera: &Camera) -> bool {
        doubles(self.output_contract().fps(), camera.mode.fps())
    }

    /// Whether the output contract's rate is twice the rate of a mode of a
    /// known camera (its mode now, or one it offers).
    fn frame_generation_possible(&self) -> bool {
        let out = self.output_contract().fps();
        self.cameras
            .iter()
            .any(|c| doubles(out, c.mode.fps()) || c.modes.iter().any(|m| doubles(out, m.fps())))
    }

    /// The frame generation stage id.
    fn frame_generation_id() -> StageId {
        StageId::new(FRAME_GENERATION).expect("a valid stage id")
    }

    /// `Warning`'s clause for an output rate that frame generation should
    /// make but cannot (no module provides it, it was dropped or failed):
    /// the frames are repeated.
    fn frame_generation_warning(&self) -> Option<String> {
        if !self.modules_loaded {
            return None;
        }
        let camera = self.settings_camera()?;
        if !self.frame_generation_wanted(camera) {
            return None;
        }
        let fg = Self::frame_generation_id();
        let plan = self.current_plan();
        let failed = self
            .stages
            .iter()
            .any(|(s, t)| *s == fg && t.failure.is_some());
        if self.runs_in(&plan, &fg) && !self.stage_failed(&fg) && !failed {
            return None;
        }
        Some(format!(
            "{:.0} fps output repeats frames: no frame generation available",
            self.output_contract().fps().round()
        ))
    }

    /// What the session is given: the background effect in force, the
    /// stages of the plan with the settings in force (a stage known not to
    /// run is never requested, so it raises no error; the choice stays in
    /// force and applies once it can run). Enhancement is off here: it is
    /// the ingest's, given per camera ([`Self::session_params_for`]).
    fn effect_run(&self) -> EffectRun {
        self.effect_run_with(&self.current_plan())
    }

    fn effect_run_with(&self, plan: &Plan) -> EffectRun {
        let effect = self.effect();
        // A background effect no module of the plan can run follows
        // `on_failure` ([`Self::fails_closed`]): asked of the effect element
        // all the same under `black` and `image` (it fails, and shows black
        // or the fallback image), dropped under `none` (the camera image).
        let mode = if self.runs_in(plan, &StageId::BACKGROUND) || self.fails_closed(plan) {
            effect.mode
        } else {
            EffectMode::None
        };
        EffectRun {
            mode,
            blur_strength: effect.blur_strength,
            background_image: effect.background_image,
            on_failure: effect.on_failure,
            fallback_image: effect.fallback_image,
            stages: plan
                .effect_stages
                .iter()
                .map(|s| self.modules.stage_run(plan, &self.config, &self.runtime, s))
                .collect(),
            chains: self.frame_generation_follows(self.modules.chain_runs(
                plan,
                &self.config,
                &self.runtime,
            )),
            enhance: false,
            background_params: self
                .modules
                .stage_run(plan, &self.config, &self.runtime, &StageId::BACKGROUND)
                .params,
            enhance_params: Params::new(),
        }
    }

    /// `chains` with frame generation off unless the output rate is twice
    /// the camera's now (the camera the settings describe): a plan keeps
    /// the stage for the whole session, while the capture mode may change.
    fn frame_generation_follows(
        &self,
        mut chains: Vec<crate::backend::ChainRun>,
    ) -> Vec<crate::backend::ChainRun> {
        let wanted = self
            .settings_camera()
            .is_some_and(|c| self.frame_generation_wanted(c));
        if !wanted {
            for s in chains.iter_mut().flat_map(|c| c.stages.iter_mut()) {
                if s.id.as_str() == FRAME_GENERATION {
                    s.enabled = false;
                }
            }
        }
        chains
    }

    /// A selected background effect that no module of `plan` can run is
    /// asked of the effect element all the same: under an `on_failure`
    /// that hides the camera (`black`, `image`) the element fails it and
    /// shows what `on_failure` says, as for a provided effect that cannot
    /// load, so the room is never shown for want of a module (a module
    /// conflict, disabled, a failed open or probe, the crash ledger, none
    /// installed, a pin to nothing). Also for a session that lost the
    /// module it started with ([`Self::background_lost`]): its element
    /// drops that module's instance ([`Self::follow_effect_backend`]); but
    /// with a media backend that cannot have it do so, the effect is
    /// dropped (the element would go on with that module's instance).
    fn fails_closed(&self, plan: &Plan) -> bool {
        self.effect().on_failure.hides()
            && (!self.background_lost(plan) || self.backend.can_revoke_effect_backend())
    }

    /// `plan` is the running session's, whose module could run the
    /// background effect when the session started and can no longer.
    fn background_lost(&self, plan: &Plan) -> bool {
        let provider = plan.provider(&StageId::BACKGROUND);
        self.background_planned
            && provider.is_some()
            && self
                .plan
                .as_ref()
                .is_some_and(|p| p.provider(&StageId::BACKGROUND) == provider)
            && !self.runs_in(plan, &StageId::BACKGROUND)
    }

    /// `Warning`'s clause of a selected background effect that no module
    /// of `plan` can run ([`Standing::Missing`]; or only a new session's
    /// can, [`Standing::Elsewhere`], while the effect is asked closed of
    /// this one, [`Self::fails_closed`]), in the words of a provided
    /// effect that cannot load ([`effect_warning_text`]): the module that
    /// would provide it and why it cannot, and what `on_failure` shows
    /// meanwhile. `None` otherwise, and for a session that lost its module
    /// and drops the effect ([`Self::fails_closed`]: the stage is dropped,
    /// [`dropped_text`]).
    fn background_absent(&self, plan: &Plan) -> Option<String> {
        if !self.modules_loaded {
            return None;
        }
        let why = match self.standing(plan, &StageId::BACKGROUND) {
            Standing::Missing { module, reason } => match module {
                Some(name) => format!("{name}: {reason}"),
                None => reason,
            },
            Standing::Elsewhere if self.fails_closed(plan) => REASON_NEXT_START.to_string(),
            _ => return None,
        };
        let effect = self.effect();
        if effect.on_failure.hides()
            && self.background_lost(plan)
            && !self.backend.can_revoke_effect_backend()
        {
            return None;
        }
        Some(effect_warning_text(&effect, &why))
    }

    /// Whether the module of `plan` for `stage` can run it now.
    fn runs_in(&self, plan: &Plan, stage: &StageId) -> bool {
        plan.provider(stage)
            .is_some_and(|m| self.modules.able(m, stage.as_str()))
    }

    /// Whether the user selected `stage`: for `background` an effect
    /// other than `none`; for another stage its switch is on. The switch
    /// is the one of the plan's module for it (a stage it gives no switch
    /// is on), else the one a module described ([`Modules::switch_of`]),
    /// else, for noise removal, relighting and enhancement, the core's own
    /// keys; a stage whose switch is not known is not selected.
    /// Enhancement counts only where it applies to the camera (the
    /// session's, or the one a session would start on).
    fn selected(&self, plan: &Plan, stage: &StageId) -> bool {
        if *stage == StageId::BACKGROUND {
            return self.effect().mode != EffectMode::None;
        }
        let id = stage.as_str();
        let on = if let Some(d) = plan.settings_of(stage) {
            d.enable_of(id)
                .and_then(|s| modules::value_of(&self.config, &self.runtime, s))
                .is_none_or(|v| v == StoredValue::Bool(true))
        } else if let Some(s) = self.modules.switch_of(id) {
            modules::value_of(&self.config, &self.runtime, s) == Some(StoredValue::Bool(true))
        } else {
            core_switch(&self.config, &self.runtime, stage)
        };
        if on && *stage == StageId::ENHANCE {
            return self
                .settings_camera()
                .is_some_and(|c| self.enhance_applies(c) == effects::Applies::Yes);
        }
        on
    }

    /// Where `stage` stands for `plan`: not selected, run by it, missing
    /// (no module provides it now: which would and why; or a stage
    /// conflict turns it off: which stage wins) or provided by a
    /// module `plan` does not run (only a new session can). A running
    /// session keeps its own module while that module can run the stage,
    /// even if another module would be chosen now (a pin or the profile
    /// apply at the next start).
    fn standing(&self, plan: &Plan, stage: &StageId) -> Standing {
        if !self.selected(plan, stage) {
            return Standing::Off;
        }
        // Turned off by a stage conflict: dropped, and the winner named.
        if let Some(winner) = self
            .modules
            .losers(plan, &self.config, &self.runtime)
            .get(stage)
        {
            return Standing::Missing {
                module: None,
                reason: self
                    .modules
                    .conflict_reason(winner, &self.config, &self.runtime),
            };
        }
        if self.runs_in(plan, stage) {
            return Standing::Runs;
        }
        if self.modules.resolve(stage.as_str()).provider.is_some() {
            return Standing::Elsewhere;
        }
        let (module, reason) = self.modules.absent_reason(stage.as_str());
        Standing::Missing { module, reason }
    }

    /// The selected stages that no module provides for `plan`, or that a
    /// module provides which `plan` does not run ([`Standing::Elsewhere`],
    /// [`REASON_NEXT_START`]), which the feed streams without: (label,
    /// module name, reason), the background
    /// effect first (`effect <mode>`), then the other stages in the stage
    /// order, enhancement last. Nothing before the modules are loaded.
    fn dropped_stages(&self, plan: &Plan) -> Vec<(String, Option<String>, String)> {
        if !self.modules_loaded {
            return Vec::new();
        }
        let mut stages = vec![StageId::BACKGROUND];
        for s in self
            .modules
            .known_stages()
            .into_iter()
            .chain([StageId::DENOISE, StageId::RELIGHT])
        {
            // Frame generation has a clause of its own
            // ([`Self::frame_generation_warning`]).
            if s.as_str() == FRAME_GENERATION {
                continue;
            }
            if s != StageId::BACKGROUND && s != StageId::ENHANCE && !stages.contains(&s) {
                stages.push(s);
            }
        }
        stages.push(StageId::ENHANCE);
        // A background effect with a clause of its own follows `on_failure`
        // ([`Self::background_absent`]).
        if self.background_absent(plan).is_some() {
            stages.retain(|s| *s != StageId::BACKGROUND);
        }
        stages
            .iter()
            .filter_map(|s| match self.standing(plan, s) {
                Standing::Missing { module, reason } => Some((self.label_of(s), module, reason)),
                // Run only by a new session: the second pass of a session
                // start never restarts ([`Self::start_session`]).
                Standing::Elsewhere => {
                    Some((self.label_of(s), None, REASON_NEXT_START.to_string()))
                }
                _ => None,
            })
            .collect()
    }

    /// The label of `stage` in `Warning`: `effect <mode>` for the
    /// background effect, else its switch's label, lower-cased and made
    /// safe to show (a module's text), else the core's name for noise
    /// removal and relighting, else its id.
    fn label_of(&self, stage: &StageId) -> String {
        if *stage == StageId::BACKGROUND {
            return format!("effect {}", self.effect().mode);
        }
        if self.modules.switch_of(stage.as_str()).is_some() {
            return settings::sanitise(&self.modules.stage_label(stage.as_str()), LABEL_MAX);
        }
        if *stage == StageId::DENOISE {
            "noise removal".into()
        } else if *stage == StageId::RELIGHT {
            "relighting".into()
        } else {
            stage.to_string()
        }
    }

    /// `Warning`: a provided effect that cannot run
    /// ([`Self::effect_failure`]), then the selected stages no module
    /// provides, which the feed streams without, grouped by module and
    /// reason ([`dropped_text`]), joined by `; `, made one line safe to
    /// show whatever a module calls itself; empty when nothing.
    ///
    /// Then the conflicts a forced module lets through
    /// ([`Self::forced_warnings`]), frame generation that cannot make the
    /// output rate
    /// ([`Self::frame_generation_warning`]), a capture mode the camera does
    /// not offer ([`Self::mode_warning`]) and the output contract's
    /// clauses ([`Self::output_warnings`]). At most [`WARNING_MAX`] bytes
    /// ([`cap_warning`]).
    fn warning(&self) -> String {
        // Without its effects, the session's clause says why, not each
        // stage.
        let plan = match (&self.passthrough, &self.plan) {
            (Some(_), _) => None,
            (None, Some(p)) => Some(p.clone()),
            (None, None) => Some(self.fresh_plan()),
        };
        let dropped = plan
            .as_ref()
            .map(|p| self.dropped_stages(p))
            .unwrap_or_default();
        // A background effect no module can run says so in the effect's
        // clause: the element's own failure (it has no backend, or one
        // that cannot run it) is a consequence.
        let effect = plan
            .as_ref()
            .and_then(|p| self.background_absent(p))
            .or_else(|| self.effect_failure.clone());
        let text = effect
            .into_iter()
            .chain(self.passthrough.clone())
            .chain(self.fallback_warnings())
            .chain(dropped_text(&dropped))
            .chain(self.held_warning())
            .chain(self.forced_warnings())
            .chain(self.frame_generation_warning())
            .chain(self.mode_warning())
            .chain(self.output_warnings())
            .collect::<Vec<_>>()
            .join("; ");
        cap_warning(&text)
    }

    /// `Warning`'s clause of the module the running session keeps for its
    /// background effect although it is refused since
    /// ([`Modules::held_refusal`]): it stops at the next start.
    fn held_warning(&self) -> Option<String> {
        let (name, why) = self.modules.held_refusal()?;
        Some(format!(
            "{} unavailable from the next start: {why}",
            settings::sanitise(&name, modules::NAME_MAX)
        ))
    }

    /// `Warning`'s clauses of the conflicts a forced module
    /// (`[modules.forced]`) lets through, while one does: each module
    /// conflict between two modules that are both available (opened, not
    /// refused otherwise; by module id), then each stage conflict between
    /// two stages of the session that are both on (in the stage order;
    /// none while the session runs without its effects).
    /// Nothing when the force overrides no conflict.
    fn forced_warnings(&self) -> Vec<String> {
        if !self.modules_loaded {
            return Vec::new();
        }
        let clause = |a: &str, b: &str| {
            format!(
                "forced: {a} runs together with {b} despite a declared conflict (modules.forced)"
            )
        };
        let name = |id: &str| settings::sanitise(&self.modules.name_of(id), modules::NAME_MAX);
        let mut out: Vec<String> = self
            .modules
            .forced_module_conflicts()
            .iter()
            .map(|(a, b)| clause(&name(a), &name(b)))
            .collect();
        if self.passthrough.is_none() {
            let plan = self.current_plan();
            let label = |s: &StageId| self.modules.conflict_label(s, &self.config, &self.runtime);
            out.extend(
                self.modules
                    .forced_stage_conflicts(&plan, &self.config, &self.runtime)
                    .iter()
                    .map(|(a, b)| clause(&label(a), &label(b))),
            );
        }
        out
    }

    /// `Warning`'s clause for a capture mode the user chose for the
    /// camera the settings describe that it does not offer: the mode asked
    /// for, and the nearest one it captures instead
    /// ([`obc_pipeline::choose_mode`]).
    fn mode_warning(&self) -> Option<String> {
        let camera = self.settings_camera()?;
        if let Some(f) = self.mode_failed.get(&camera.id) {
            if !f.confirmed {
                return None;
            }
            let m = camera.mode;
            return Some(format!(
                "{} could not open {} ({}): it captures {} instead",
                camera.name,
                settings::mode_choice_value(&f.asked),
                f.why,
                mode_value(m.width, m.height, m.fps_num, m.fps_den)
            ));
        }
        let asked = self.runtime.camera_modes.get(&camera.id)?;
        if camera.modes.is_empty() || asked.is_exact(&camera.mode) {
            return None;
        }
        let m = camera.mode;
        Some(format!(
            "{} does not offer {}: it captures {} instead",
            camera.name,
            settings::mode_choice_value(asked),
            mode_value(m.width, m.height, m.fps_num, m.fps_den)
        ))
    }

    /// `Warning`'s clauses of the output contract: a configured contract
    /// that could not start ([`Self::failed_contract`]), and one that waits
    /// for the applications holding the virtual camera to release it.
    fn output_warnings(&self) -> Vec<String> {
        let sr = |c: &OutputContract| mode_value(c.width, c.height, c.fps_num, c.fps_den);
        let mut out = Vec::new();
        if let Some((failed, why, instead)) = &self.failed_contract
            && *failed == self.config.output_contract()
        {
            out.push(format!(
                "Output {} could not start ({why}): streaming {} instead",
                sr(failed),
                sr(instead)
            ));
        }
        let waits = self
            .writer
            .as_ref()
            .is_some_and(|w| w.contract != self.wanted_contract());
        if waits && (self.consumers > 0 || self.held_retry_due().is_some()) {
            out.push(format!(
                "Output {} applies when {} release the virtual camera",
                sr(&self.wanted_contract()),
                holders_text(&self.holder_names)
            ));
        }
        out
    }

    /// The running session follows what the modules provide now (a module
    /// enabled or disabled, a reload, a probe): the stages turned on or off
    /// ([`Self::follow_stages`]), the background effect dropped or given
    /// again ([`Self::follow_background`]) and, with `enhance`, the ingest
    /// reopened with or without enhancement ([`Self::follow_enhance`]),
    /// the camera kept open otherwise. When a new session would run a
    /// selected stage that this one cannot ([`Self::restart_for`]), the
    /// session is stopped instead, and the lifecycle starts a new one with
    /// it (the camera reopened): an output built without a module cannot
    /// take it later; without `restart` (the second pass of a session
    /// start, [`Self::start_session`]) it never is. Whether the session was
    /// stopped.
    fn follow_modules(&mut self, enhance: bool, now: Instant, restart: bool) -> bool {
        if self.session.is_none() {
            return false;
        }
        if restart && self.restart_for_modules() {
            return true;
        }
        self.follow_stages();
        if enhance {
            let _ = self.follow_enhance(false, now);
        }
        false
    }

    /// Stops the running session when a new one would run a selected stage
    /// that it cannot ([`Self::restart_for`]); the lifecycle starts the new
    /// one. Whether it was stopped.
    fn restart_for_modules(&mut self) -> bool {
        let Some(what) = self.restart_for() else {
            return false;
        };
        info!("a module provides {what} now: restarting the camera to run it");
        self.drop_session();
        true
    }

    /// What a session started now would run, of what the user selected,
    /// that the running one has no module able to run: the label of the
    /// first such stage (the background effect, another stage,
    /// enhancement), `None` when there is none or no session. A stage
    /// whose module in the running session can still run it is never one
    /// (pinned or ranked to another module since, it changes module at the
    /// next start), and a new session of the same modules as the running
    /// one runs nothing more (so a session is never restarted again for
    /// what it just started with).
    fn restart_for(&self) -> Option<String> {
        self.session.as_ref()?;
        let plan = self.plan.as_ref()?;
        let next = self.fresh_plan();
        if plan_ids(plan) == plan_ids(&next) {
            return None;
        }
        if self.standing(plan, &StageId::BACKGROUND) == Standing::Elsewhere {
            return Some("the background effect".into());
        }
        let runs_here = |id: &StageId| self.runs_in(plan, id);
        let then = self.effect_run_with(&next);
        if let Some(s) = then
            .stages
            .iter()
            .chain(then.chains.iter().flat_map(|c| c.stages.iter()))
            .find(|s| s.enabled && !runs_here(&s.id))
        {
            return Some(self.modules.stage_label(s.id.as_str()));
        }
        let enhance = StageId::ENHANCE;
        if next.enhance.is_some()
            && self
                .modules
                .stage_on(&next, &self.config, &self.runtime, &enhance)
            && !runs_here(&enhance)
            && self
                .session_camera()
                .is_some_and(|c| self.enhance_applies(c) == effects::Applies::Yes)
        {
            return Some(self.modules.stage_label(enhance.as_str()));
        }
        None
    }

    /// The background effect of the running session (`effect`, what it
    /// was last given, `run` what it would be given now) follows whether a
    /// module it runs can run it: dropped (given off; `Warning` says why)
    /// when none can and it does not fail closed ([`Self::fails_closed`]),
    /// given again (followed from its load) when one can, also when it was
    /// asked while none could (the element failed it).
    /// An effect the session streams without because it failed
    /// (`on_failure = "none"`) is not given again here: only a control
    /// call or a reload retries it. With no background effect to run (none
    /// selected, or dropped), the failure of one is not what the feed
    /// shows: cleared, so a module enabled again gives it again. Whether
    /// `effect` changed.
    fn follow_background(&mut self, effect: &mut EffectRun, run: &EffectRun) -> bool {
        if run.mode == EffectMode::None {
            // Before the comparison: an effect rejected at the start under
            // `none` was given off already.
            self.effect_failure = None;
        }
        if run.mode == effect.mode {
            // Asked while no module of the plan could run it (the element
            // failed it, showing what `on_failure` says): a module can now,
            // so the element tries it again.
            if run.mode != EffectMode::None
                && self.background_closed
                && self.runs_in(&self.current_plan(), &StageId::BACKGROUND)
            {
                info!(
                    "a module can run the background effect now: effect {}",
                    run.mode
                );
                self.background_closed = false;
                self.effect_seen = None;
                return true;
            }
            return false;
        }
        if run.mode == EffectMode::None {
            // `Warning` says why ([`Self::publish_with`] logs it).
            info!("no module of the session can run the background effect: effect off");
            effect.mode = EffectMode::None;
        } else {
            if self.effect_failure.is_some() && run.on_failure == EffectOnFailure::None {
                return false;
            }
            info!(
                "a module provides the background effect again: effect {}",
                run.mode
            );
            effect.mode = run.mode;
            effect.blur_strength = run.blur_strength;
            effect.background_image = run.background_image.clone();
            self.effect_seen = None;
        }
        true
    }

    /// Whether enhancement can run: `Some(false)` when no module can run
    /// it now.
    fn enhance_installed(&self) -> Option<bool> {
        Some(
            self.modules
                .resolve(StageId::ENHANCE.as_str())
                .provider
                .is_some(),
        )
    }

    /// Whether enhancement applies to `camera`'s capture mode (small
    /// enough for it to pay, in a format it reads once uploaded), with the
    /// output's contract and aspect policy, and why not.
    fn enhance_applies(&self, camera: &Camera) -> effects::Applies {
        use obc_pipeline::ingest::{EnhanceRefusal, enhance_plan};
        let out = self.output_contract();
        let format = camera.mode.format;
        match enhance_plan(
            self.config.output.aspect,
            IngestPath::of(format).upload_format(format),
            (camera.mode.width, camera.mode.height),
            (out.width, out.height),
        ) {
            Ok(_) => effects::Applies::Yes,
            Err(EnhanceRefusal::Resolution) => effects::Applies::FullResolution,
            Err(EnhanceRefusal::Format) => effects::Applies::Format,
        }
    }

    /// Whether an ingest on `camera` is built with the enhance stage: a
    /// module provides it, it is on and can run, and it applies to the
    /// camera.
    fn enhance_for(&self, camera: &Camera) -> bool {
        let plan = self.current_plan();
        plan.enhance.is_some()
            && self
                .modules
                .stage_on(&plan, &self.config, &self.runtime, &StageId::ENHANCE)
            && self.enhance_installed() != Some(false)
            && self.enhance_applies(camera) == effects::Applies::Yes
    }

    /// The camera the settings tree describes: the session's, else the one
    /// a session would start on.
    fn settings_camera(&self) -> Option<&Camera> {
        match &self.session {
            Some(_) => self.session_camera(),
            None => self.chosen_camera(),
        }
    }

    /// Probes the modules again (the filesystem only: no CUDA call, safe at
    /// idle), with the compute capability known; with a session, a stage
    /// that can now run, or no longer can, is turned on or off in it
    /// ([`Self::follow_stages`]). While a session runs, the compute
    /// capability is read first if this process has not read it yet (the
    /// GPU is up then); a value other than the cached one is cached, and
    /// the modules are opened again with it.
    ///
    /// Whether the session was stopped to be started again with another
    /// module ([`Self::follow_modules`]; never without `restart`).
    fn refresh_features(&mut self, now: Instant, restart: bool) -> bool {
        if self.session.is_some() && !self.compute_capability_learnt {
            self.learn_compute_capability();
        }
        self.modules
            .refresh(&mut self.backend, &self.config, self.compute_capability);
        self.follow_modules(true, now, restart)
    }

    /// Reads the compute capability from the running session's GPU side,
    /// once per process, and caches it when it differs from what was known.
    fn learn_compute_capability(&mut self) {
        let Some(cc) = self.backend.compute_capability() else {
            return;
        };
        self.compute_capability_learnt = true;
        let (major, minor) = cc;
        debug!("GPU compute capability {major}.{minor}");
        if self.compute_capability == Some(cc) {
            return;
        }
        self.compute_capability = Some(cc);
        if let Some(dir) = &self.paths.cache
            && let Err(e) = gpu_cache::save(dir, cc)
        {
            debug!(
                "cannot keep the GPU compute capability in {}: {e}",
                gpu_cache::path(dir).display()
            );
        }
    }

    /// What can run changed while a session runs: the stages that follow
    /// it are turned on or off in what the session was given, and the
    /// background effect dropped or given again
    /// ([`Self::follow_background`]), in one change of the session; nothing
    /// else is. In particular an effect the session started without
    /// (rejected by [`Self::effect_check`] with `on_failure = "none"`) is
    /// not requested again: only a control call or a reload retries it.
    fn follow_stages(&mut self) {
        self.follow_effect_backend();
        let run = self.effect_run();
        let Some(mut effect) = self.effect_applied.clone() else {
            return;
        };
        let mut changed = Vec::new();
        for s in effect
            .stages
            .iter_mut()
            .chain(effect.chains.iter_mut().flat_map(|c| c.stages.iter_mut()))
        {
            if let Some(now) = run.stage(s.id.as_str())
                && now.enabled != s.enabled
            {
                s.enabled = now.enabled;
                changed.push(s.id.clone());
            }
        }
        let background = self.follow_background(&mut effect, &run);
        if changed.is_empty() && !background {
            return;
        }
        for id in &changed {
            let t = self.track(id);
            t.seen = None;
            t.held = None;
        }
        self.effect_applied = Some(effect.clone());
        self.mark_running(&effect);
        if let Err(reason) = self.give_effect(&effect) {
            let _ = self.effect_refused(effect, &reason);
        }
    }

    /// Frame generation in the running session follows the camera it
    /// streams now (a capture mode change, a switch, a fail-over): on while
    /// the output rate is twice the camera's, off otherwise
    /// ([`Self::frame_generation_follows`]). Only that stage changes; a
    /// session planned without it ([`Self::fresh_plan`]) is left as it is.
    fn follow_frame_generation(&mut self) {
        let Some(mut effect) = self.effect_applied.clone() else {
            return;
        };
        let run = self.effect_run();
        let Some(on) = run.stage(FRAME_GENERATION).map(|s| s.enabled) else {
            return;
        };
        let mut changed = false;
        for s in effect
            .chains
            .iter_mut()
            .flat_map(|c| c.stages.iter_mut())
            .filter(|s| s.id.as_str() == FRAME_GENERATION && s.enabled != on)
        {
            s.enabled = on;
            changed = true;
        }
        if !changed {
            return;
        }
        info!(
            "frame generation {} for the camera's rate",
            if on { "on" } else { "off" }
        );
        let t = self.track(&Self::frame_generation_id());
        t.seen = None;
        t.held = None;
        self.effect_applied = Some(effect.clone());
        self.mark_running(&effect);
        if let Err(reason) = self.give_effect(&effect) {
            let _ = self.effect_refused(effect, &reason);
        }
    }

    /// The tracking of stage `id`, made when first needed.
    fn track(&mut self, id: &StageId) -> &mut StageTrack {
        let i = match self.stages.iter().position(|(s, _)| s == id) {
            Some(i) => i,
            None => {
                self.stages.push((id.clone(), StageTrack::default()));
                self.stages.len() - 1
            }
        };
        &mut self.stages[i].1
    }

    /// Whether stage `id` failed (or holds a failure for the next poll).
    fn stage_failed(&self, id: &StageId) -> bool {
        self.stages.iter().any(|(s, t)| {
            s == id && (matches!(t.seen, Some(EffectStatus::Failed(_))) || t.held.is_some())
        })
    }

    /// Ends every stage failure; whether one of them was the `Error` shown.
    fn take_stage_failures(&mut self) -> bool {
        let mut shown = false;
        for (_, t) in &mut self.stages {
            if let Some(f) = t.failure.take() {
                shown |= f == self.error;
            }
        }
        shown
    }

    /// The global settings (the video effects) as they are now.
    fn global_settings(&self) -> Vec<GlobalEntry> {
        effects::build(&effects::Inputs {
            config: &self.config,
            runtime: &self.runtime,
            modules: &self.modules,
            enhance_applies: self.settings_camera().map(|c| self.enhance_applies(c)),
        })
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
            .map_or_else(|| self.wanted_contract(), |w| w.contract)
    }

    /// The output contract the writer is to have: the configured one,
    /// unless it could not start in this run, then the one streamed
    /// instead ([`Self::failed_contract`]).
    fn wanted_contract(&self) -> OutputContract {
        let c = self.config.output_contract();
        match &self.failed_contract {
            Some((failed, _, instead)) if *failed == c => *instead,
            _ => c,
        }
    }

    fn session_params(&self) -> SessionParams {
        SessionParams {
            contract: self.output_contract(),
            aspect: self.config.output.aspect,
            hold_timeout: self.config.hold_timeout(),
            input_gap: self.config.input_gap(),
            fixed_framerate: self.config.camera.fixed_framerate,
            effect: self.effect_run(),
            backends: self.current_plan().backends(),
        }
    }

    /// The session params for an ingest on `camera`: `fixed_framerate`
    /// only when its next open pins the frame rate
    /// ([`Self::fixed_framerate_for`]), the enhance stage only when it is
    /// requested for that camera ([`Self::enhance_for`]), with its
    /// parameters ([`Self::enhance_params_for`]).
    fn session_params_for(&self, camera: &Camera) -> SessionParams {
        let mut params = SessionParams {
            fixed_framerate: self.fixed_framerate_for(&camera.id),
            ..self.session_params()
        };
        params.effect.enhance = self.enhance_for(camera);
        params.effect.enhance_params = self.enhance_params_for(camera);
        params
    }

    /// The load-time parameters of the enhance stage of an ingest on
    /// `camera`, from its module's settings in force: none when the ingest
    /// has no enhance stage ([`Self::enhance_for`]).
    fn enhance_params_for(&self, camera: &Camera) -> Params {
        if !self.enhance_for(camera) {
            return Params::new();
        }
        let plan = self.current_plan();
        self.modules
            .stage_run(&plan, &self.config, &self.runtime, &StageId::ENHANCE)
            .params
    }

    /// The config asks for another output contract or loopback device than
    /// the writer has. The PipeWire setting is not the writer's
    /// ([`Self::sync_pipewire`]).
    fn writer_outdated(&self) -> bool {
        self.writer.as_ref().is_some_and(|w| {
            w.contract != self.wanted_contract() || w.sink != self.config.sink_selector()
        })
    }

    /// A writer rebuild for a deferred config change may happen: nobody
    /// counted, no session.
    fn rebuild_due(&self) -> bool {
        self.writer_outdated() && self.consumers == 0 && self.session.is_none()
    }

    fn publish(&mut self) {
        self.publish_with(true);
    }

    /// Publishes the properties, and the settings tree when `settings`.
    /// Without it the tree is left for a later [`Self::publish`] of the same
    /// operation, so that an operation sends one `SettingsChanged` at most
    /// (`docs/adr/0023-settings-schema.md`); the properties then carry the
    /// revision published so far.
    fn publish_with(&mut self, settings: bool) {
        // The module the session keeps for its background effect, and the
        // stages a stage conflict turns off, as `GetModules` lists them
        // (the serial moves when they change).
        self.hold_background();
        self.follow_effect_backend();
        let plan = self.current_plan();
        self.modules
            .follow_switches(&plan, &self.config, &self.runtime);
        self.background_closed = self.session.is_some()
            && self
                .effect_applied
                .as_ref()
                .is_some_and(|e| e.mode != EffectMode::None)
            && !self.runs_in(&plan, &StageId::BACKGROUND);
        self.sync_placeholder();
        if self.controls_view.camera != self.active_camera() {
            self.refresh_controls();
        }
        self.read_holders();
        let delta = if settings {
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
            delta
        } else {
            None
        };
        // The modules found make the module rows of the config schema, the
        // camera the settings describe the options of `output.mode`.
        let mut facts = if self.config_facts.0 != self.modules.serial() {
            registry_facts(&self.modules)
        } else {
            self.config_facts.1.clone()
        };
        facts.output_modes = self.output_mode_options();
        facts.on_failure_options =
            config_schema::on_failure_options(&self.config_set.config.effect);
        if facts != self.config_facts.1 {
            self.bump_config();
        }
        self.config_facts = (self.modules.serial(), facts);
        let effect = self.effect();
        // Computed once per publish, and logged where it changes.
        let warning = self.warning();
        if warning != self.warning_shown {
            if warning.is_empty() {
                info!("no warning any more");
            } else {
                warning!("{warning}");
            }
            self.warning_shown.clone_from(&warning);
        }
        let snap = Snapshot {
            state: self.state(),
            error: self.error.clone(),
            warning,
            enabled: self.runtime.enabled,
            cameras: self.cameras.iter().map(Camera::info).collect(),
            active_camera: self.active_camera(),
            output_mode: output_mode_string(&self.output_contract()),
            effect: effect.mode.as_str().into(),
            blur_strength: effect.blur_strength,
            background_image: image_string(&effect),
            denoise: denoise_choice(&self.config, &self.runtime),
            consumers: self.consumers,
            loopback_consumers: self.loopback_consumers,
            pipewire_consumers: self.pipewire_consumers,
            preview_consumers: self.leases.count(),
            pipewire_output: self.pipewire_state.as_str().into(),
            camera_controls: Some(self.controls_view.clone()),
            settings_revision: Some(self.settings_revision),
            settings_schema_serial: Some(self.settings_serial),
            modules_serial: Some(self.modules_serial()),
            backend_profile: Some(
                self.config
                    .modules
                    .profile_or_default()
                    .as_str()
                    .to_string(),
            ),
            config_serial: Some(self.config_serial),
            mic: Some(self.mic_status.clone()),
            interface_revision: INTERFACE_REVISION,
        };
        self.published.update(snap);
        self.push_mic_snapshot();
        if self.config_announced != self.config_serial {
            self.config_announced = self.config_serial;
            self.published.config_changed(self.config_serial);
        }
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
        self.watch_placeholder(now);
        self.check_writer(now);
        self.check_session(now);
        if self.streams_without_black_effect() {
            // However it came to this (a control call, a reload, a module
            // change), the room is never shown: the lifecycle starts a
            // session with the effect, or none.
            warning!(
                "an effect under on_failure = \"{}\" is not in the session: stopping it",
                self.effect().on_failure
            );
            self.drop_session();
            self.privacy_hold = Some(PrivacyHold::Guard);
        }
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

    /// The running session streams without its effects
    /// ([`Self::passthrough`]) while a session started now would be refused
    /// without them ([`Self::refuses_passthrough`]): it shows what the user
    /// asked never to be shown.
    fn streams_without_black_effect(&self) -> bool {
        self.passthrough.is_some() && self.refuses_passthrough()
    }

    /// A session started now would be given a background effect, under an
    /// `on_failure` that hides the camera, and so is never started without
    /// its effects ([`Self::start_passthrough`]): the effect is selected,
    /// whether or not a module of a fresh plan can run it
    /// ([`Self::fails_closed`]). The one test the guard and the start both
    /// use.
    fn refuses_passthrough(&self) -> bool {
        Self::hides_effect(&self.effect_run_with(&self.fresh_plan()))
    }

    fn check_writer(&mut self, now: Instant) {
        self.forget_stale_held_retry();
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
        if self.held_retry_due().is_some_and(|t| now < t) {
            return;
        }
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
        self.start_writer_with(self.wanted_contract(), now);
    }

    /// Starts the writer with `contract`. A contract that cannot start
    /// falls back to another ([`Self::fallback_contracts`]: the last one
    /// that started in this run, the format a held loopback holds, the last
    /// good one of an earlier run, the built-in default), the first that
    /// starts: as a failed contract ([`Self::failed_contract`]) when the
    /// contract is at fault, as a deferral ([`Self::held_retry`]) when the
    /// loopback is held in another format. Without one that starts, the
    /// writer is retried as it is.
    fn start_writer_with(&mut self, contract: OutputContract, now: Instant) {
        let sink = self.config.sink_selector();
        let e = match self.try_writer(contract, &sink, now) {
            Ok(()) => return,
            Err(e) => e,
        };
        // A new contract that cannot start: another streams instead, so
        // the virtual camera keeps a feed.
        let mut last = e.clone();
        if e.held || e.revertible {
            let held_as = e.held_as.filter(|_| e.held);
            let fallbacks = self.fallback_contracts(&contract, held_as);
            if e.held {
                // Not the contract's fault: tried again once the device is
                // released. Known limit: each try stops the writer, so the
                // PipeWire output's node is recreated at every try (2, 4,
                // 8, 16, then every 32 s while the hold lasts); only a
                // holder the watcher cannot count (another user's process)
                // keeps it going.
                let delay = match &self.held_retry {
                    Some(h) if h.contract == contract && h.sink == sink => {
                        (h.delay * 2).min(HELD_RETRY_MAX)
                    }
                    _ => WRITER_RETRY,
                };
                self.held_retry = Some(HeldRetry {
                    contract,
                    sink: sink.clone(),
                    at: now + delay,
                    delay,
                });
            }
            for good in &fallbacks {
                match self.try_writer(*good, &sink, now) {
                    Ok(()) => {
                        if e.held {
                            warning!(
                                "the output {} waits for the virtual camera to be released \
                                 ({e}): streaming {} meanwhile",
                                output_mode_string(&contract),
                                output_mode_string(good)
                            );
                        } else {
                            warning!(
                                "the output {} could not start ({e}): streaming {} instead",
                                output_mode_string(&contract),
                                output_mode_string(good)
                            );
                            self.failed_contract = Some((contract, e.message, *good));
                        }
                        return;
                    }
                    Err(f) => last = f,
                }
            }
            // None started: the first fallback is the one retried.
            if !e.held
                && let Some(first) = fallbacks.first()
            {
                self.failed_contract = Some((contract, e.message.clone(), *first));
            }
        }
        let e = last.message;
        if self.writer_error.as_deref() != Some(e.as_str()) {
            error!("{e} (retrying every {} s)", WRITER_RETRY.as_secs());
        }
        self.writer_error = Some(e.clone());
        self.error = e;
        self.writer_retry_at = Some(now + WRITER_RETRY);
    }

    /// Starts the writer on `sink` with `contract`, and the PipeWire output
    /// next to it if one is configured.
    fn try_writer(
        &mut self,
        contract: OutputContract,
        sink: &obc_core::SinkSelector,
        now: Instant,
    ) -> Result<(), crate::backend::WriterError> {
        let node = self.backend.start_writer(sink, &contract)?;
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
            sink: sink.clone(),
            pipewire,
            pipewire_offer_nv12: offer_nv12,
        });
        self.writer_good = Some(contract);
        if contract == self.wanted_contract() {
            self.held_retry = None;
        }
        self.notify_sink(Some(&node), now);
        Ok(())
    }

    /// What streams instead of `contract` when it cannot, in the order
    /// tried: the format a held loopback holds (`held_as`), the last
    /// contract a writer started with in this run, the last one a writer and
    /// a session output both started with, in this run or an earlier one
    /// (the state file's `output_good`), and the built-in default; each
    /// once, and never `contract` itself.
    fn fallback_contracts(
        &self,
        contract: &OutputContract,
        held_as: Option<OutputContract>,
    ) -> Vec<OutputContract> {
        let mut out: Vec<OutputContract> = Vec::new();
        // A held loopback takes only the format it holds: tried first,
        // rather than a start that must fail.
        for c in [
            held_as,
            self.writer_good,
            self.runtime.output_good,
            Some(OutputContract::default()),
        ]
        .into_iter()
        .flatten()
        {
            if c != *contract && !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    /// When the contract wanted, found the loopback held, is tried again
    /// ([`Self::held_retry`]); `None` when no fallback runs for it.
    fn held_retry_due(&self) -> Option<Instant> {
        self.held_retry
            .as_ref()
            .filter(|h| {
                h.contract == self.wanted_contract() && h.sink == self.config.sink_selector()
            })
            .map(|h| h.at)
    }

    /// Drops the held-retry record ([`Self::held_retry`]) once its contract
    /// or its loopback is no longer the one wanted (a configuration change,
    /// a contract that failed in the meantime): a change of either is tried
    /// at once, and a return to it starts again from [`WRITER_RETRY`].
    fn forget_stale_held_retry(&mut self) {
        if self.held_retry.is_some() && self.held_retry_due().is_none() {
            self.held_retry = None;
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
                let contract = s.contract;
                if let Some(s) = self.session.as_mut() {
                    s.live = true;
                    s.awaiting = None;
                    // A camera delivered: every camera may be tried again.
                    s.failed.clear();
                }
                // A frame in the automatic mode of a camera whose chosen
                // mode could not open: the mode is to blame.
                if let Some(id) = self.session.as_ref().map(|s| s.camera.clone())
                    && let Some(f) = self.mode_failed.get_mut(&id)
                {
                    f.confirmed = true;
                }
                if first {
                    // A frame streamed: the contract is the last good one.
                    self.output_proven(contract, now);
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
        // What the element shows in the effect's place matters while the
        // effect has failed.
        self.fallback_failure = match &report.status {
            EffectStatus::Failed(_) => report.fallback_error.clone(),
            _ => None,
        };
        if self.follow_effect(&report, now) {
            return true;
        }
        for (id, status) in report.stages {
            self.poll_stage(&id, status, &report.status);
        }
        self.poll_enhance(report.enhance);
        false
    }

    /// Follows the enhance stage of the current ingest. Its failure never
    /// stops the session: the stage passes the camera frames through, the
    /// state is unchanged, and `Error` says why unless a higher cause holds
    /// ([`Self::standing_error`]).
    fn poll_enhance(&mut self, status: EffectStatus) {
        if self.enhance_seen.as_ref() == Some(&status) {
            return;
        }
        let first = self.enhance_seen.is_none();
        self.enhance_seen = Some(status.clone());
        match status {
            EffectStatus::Off => {
                if !first {
                    info!("enhance off");
                }
            }
            EffectStatus::Loading => info!("enhance: loading"),
            EffectStatus::Ready => info!("enhance ready"),
            EffectStatus::Failed(reason) => {
                let msg = enhance_failure_text(&reason);
                warning!("{msg}");
                self.enhance_failure = Some(msg);
                self.show_standing_error();
            }
        }
    }

    /// Ends an enhancement failure and forgets the stage's status (a new
    /// ingest reports its own): its `Error` gives way to the causes still
    /// standing. Whether there was one.
    fn clear_enhance_failure(&mut self) -> bool {
        self.enhance_seen = None;
        let Some(old) = self.enhance_failure.take() else {
            return false;
        };
        if self.error == old {
            self.error = self.standing_error();
        }
        true
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
                self.effect_failure = None;
            }
            EffectStatus::Failed(reason) => return self.effect_failed(reason),
        }
        false
    }

    /// Follows stage `id` of the running session (noise removal,
    /// relighting, a module's stage), `effect` being the effect's status
    /// of the same report. Its failure never stops the session: it streams
    /// on without the stage (the element drops it), the state is unchanged,
    /// and `Error` says why unless a higher cause holds
    /// ([`Self::standing_error`]): the writer missing, a fault, an earlier
    /// stage's failure.
    ///
    /// A failure with the effect's own failure reason is the whole chain's
    /// (the element shows it on both): the effect's, not recorded as the
    /// stage's. One seen while the effect is loading or ready waits for the
    /// next poll: the effect's status is read first, so a failure of the
    /// chain can show on the stage a poll before it does on the effect.
    /// With the effect off (`none`), or failed for another reason, the
    /// stage's failure is its own at once.
    fn poll_stage(&mut self, id: &StageId, status: EffectStatus, effect: &EffectStatus) {
        let label = self.modules.stage_label(id.as_str());
        let t = self.track(id);
        if let EffectStatus::Failed(reason) = &status
            && t.seen.as_ref() != Some(&status)
        {
            if matches!(effect, EffectStatus::Failed(r) if r == reason) {
                debug!("{label} stopped with the effect: {reason}");
                t.held = None;
                t.seen = Some(status);
                t.polled = true;
                self.clear_stage_failure(id);
                return;
            }
            if matches!(effect, EffectStatus::Loading | EffectStatus::Ready)
                && t.held.as_ref() != Some(&status)
            {
                t.held = Some(status);
                return;
            }
        }
        t.held = None;
        if t.seen.as_ref() == Some(&status) {
            return;
        }
        t.seen = Some(status.clone());
        let first = !std::mem::replace(&mut t.polled, true);
        match status {
            EffectStatus::Off => {
                if !first {
                    info!("{label} off");
                }
                self.clear_stage_failure(id);
            }
            EffectStatus::Loading => info!("{label}: loading"),
            EffectStatus::Ready => {
                info!("{label} ready");
                self.clear_stage_failure(id);
            }
            EffectStatus::Failed(reason) => {
                let msg = stage_failure_text(id, &label, &reason);
                warning!("{msg}");
                self.track(id).failure = Some(msg);
                self.show_standing_error();
            }
        }
    }

    /// Ends stage `id`'s failure: its `Error` gives way to the causes still
    /// standing.
    fn clear_stage_failure(&mut self, id: &StageId) {
        if let Some(old) = self.track(id).failure.take()
            && self.error == old
        {
            self.error = self.standing_error();
        }
    }

    /// The running session's effect, which a module provides, cannot load
    /// or run, for `reason`. The session never stops for it: with
    /// `on_failure = "black"` the effect element's frames are black (the
    /// room hidden, the consumers keep the device), the state stays
    /// `starting`/`active`; with `"none"` it keeps streaming without the
    /// effect. `Warning` says why, either way ([`Self::effect_failure`]):
    /// cleared once the effect is ready, and tried again by an effect
    /// method, a reload, a module change that gives the effect again or
    /// the next session. The whole reason is logged (`Warning` bounds it).
    /// Returns whether the session was stopped: never.
    fn effect_failed(&mut self, reason: &str) -> bool {
        let effect = self.effect();
        warning!("effect {} failed: {reason}", effect.mode);
        self.effect_failure = Some(effect_warning_text(&effect, reason));
        false
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
        let Some(s) = &self.session else {
            return;
        };
        // Before its first frame in a chosen mode: that mode is blamed and
        // the camera tried once more in its automatic mode.
        if s.awaiting.is_some()
            && let Some(id) = self.session_camera().map(|c| c.id.clone())
            && let Some(cam) = self.blame_mode(&id, &reason)
        {
            if let Some(s) = self.session.as_mut() {
                // A new deadline for the new mode.
                s.awaiting = None;
            }
            if self.reopen_camera(&cam, now).is_ok() {
                self.follow_stages();
            }
            return;
        }
        if let Some(id) = self.session_camera().map(|c| c.id.clone()) {
            self.forget_unconfirmed_mode(&id);
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
            let mut res = self.open_candidate(cam, now);
            // In a chosen mode that cannot open: once more in its
            // automatic mode.
            if let Err(e) = &res
                && let Some(auto) = self.blame_mode(&cam.id, e)
            {
                res = self.open_candidate(&auto, now);
            }
            match res {
                Ok(()) => return Ok(()),
                Err(e) => {
                    self.forget_unconfirmed_mode(&cam.id);
                    warning!("{e}");
                    self.mark_failed();
                    last = Some(e);
                }
            }
        }
        Err(last)
    }

    /// Starts the ingest on `cam` ([`Self::try_cameras`]).
    fn open_candidate(&mut self, cam: &Camera, now: Instant) -> Result<(), String> {
        {
            self.leaving_camera();
            let params = self.session_params_for(cam);
            if let Some(s) = self.session.as_mut() {
                s.camera = cam.id.clone();
                s.node = cam.path.clone();
                s.awaiting = Some(now);
                s.open = false;
                s.pinned = params.fixed_framerate;
                s.enhance = params.effect.enhance;
                s.enhance_params = params.effect.enhance_params.clone();
            }
            self.mark_running(&params.effect);
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
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
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

    /// Starts a session. When the modules, probed again once the GPU is up
    /// (its compute capability learnt), make another module the one to run
    /// a selected stage, the session just built is stopped and started
    /// again with it in the same step, once: the second start plans with
    /// the modules as refreshed and never restarts, and nothing is
    /// published in between (`starting` is not followed by `idle`).
    fn start_session(&mut self, now: Instant) {
        if self.start_session_once(now, true) {
            self.start_session_once(now, false);
        }
    }

    /// One start of [`Self::start_session`]; with `restart`, the session
    /// may be stopped to be started again ([`Self::refresh_features`]):
    /// whether it was.
    fn start_session_once(&mut self, now: Instant, restart: bool) -> bool {
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
            return false;
        }
        let Some(first) = candidates.first() else {
            self.set_fault("no camera is available".into());
            return false;
        };
        // Which module runs each stage is decided now, for the whole
        // session (frame generation when the first camera's rate is half
        // the output's).
        self.plan = Some(self.fresh_plan());
        self.background_planned = self
            .plan
            .as_ref()
            .is_some_and(|p| self.runs_in(p, &StageId::BACKGROUND));
        self.background_revoked = false;
        self.hold_background();
        // A selected stage no module provides is dropped (the effect run
        // has it off): the session streams on without it, whatever
        // `on_failure` says, and `Warning` says why. But the background
        // effect: under `black` or `image` it is asked of the effect element
        // all the same, which fails it and shows what `on_failure` says
        // ([`Self::fails_closed`]).
        let mut params = self.session_params();
        self.effect_failure = None;
        self.stages.clear();
        self.enhance_failure = None;
        self.enhance_seen = None;
        // The effect is checked before the camera is opened. Settings the
        // output cannot be built with (`replace` without a readable
        // image) are a fault with `on_failure = "black"`: the camera stays
        // closed. A provided effect that cannot run is not: with `black`
        // the session starts with it all the same, the effect element's
        // frames black until it runs, and `Warning` says why. With
        // `none` the session starts without it, `Warning` saying why.
        let mut refused: Option<String> = None;
        if params.effect.mode != EffectMode::None {
            let settings = Self::effect_settings_check(&params.effect);
            let res = settings
                .clone()
                .and_then(|()| self.effect_check(&params.effect));
            if let Err(reason) = res {
                match params.effect.on_failure {
                    EffectOnFailure::Black | EffectOnFailure::Image if settings.is_err() => {
                        self.plan = None;
                        self.hold_background();
                        self.set_privacy_fault(effect_failure_text(&params.effect, &reason));
                        return false;
                    }
                    EffectOnFailure::Black | EffectOnFailure::Image => {
                        warning!("effect {} cannot run: {reason}", params.effect.mode);
                        self.effect_failure = Some(effect_warning_text(&params.effect, &reason));
                        refused = Some(reason);
                    }
                    EffectOnFailure::None => {
                        warning!("effect {} cannot run: {reason}", params.effect.mode);
                        self.effect_failure = Some(effect_warning_text(&params.effect, &reason));
                        params.effect.mode = EffectMode::None;
                    }
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
            enhance: false,
            enhance_params: Params::new(),
            contract: params.contract,
        });
        // `starting` is announced before the (slow) start; the settings it
        // changes (what is installed for this GPU, learnt once it is up)
        // are announced once, at the end of the step.
        self.publish_with(false);
        // From here the modules' code runs: marked in the crash ledger
        // until the session stops.
        self.mark_running(&params.effect);
        if let Err(e) = self.backend.start_session(&params)
            && self.start_passthrough(&params, &e.message).is_none()
        {
            self.drop_session();
            if e.contract {
                self.session_contract_failed(&params.contract, &e.message);
            }
            let e = e.message;
            // The effect's check refused it before: that is the cause the
            // user can act on, the build's text a consequence.
            let msg = match refused {
                Some(reason) => {
                    warning!("the output could not be built: {e}");
                    effect_failure_text(&params.effect, &reason)
                }
                None => e,
            };
            // Without its effect the output was not tried
            // ([`Self::start_passthrough`]): privacy holds the feed.
            if Self::hides_effect(&params.effect) {
                self.set_privacy_fault(msg);
            } else {
                self.set_fault(msg);
            }
            return false;
        }
        let again = restart;
        let restart = restart && self.passthrough.is_none();
        // The GPU is up: the compute capability can be learnt, and what is
        // installed for this GPU read again (applied to the session if it
        // changed what runs, or the session stopped to be started again
        // with another module).
        if self.refresh_features(now, restart) {
            return true;
        }
        if self.session.is_none() {
            return false;
        }
        // Started without its effects while the effect could not run, it
        // now can (learnt for this GPU): as the guard would, the session is
        // stopped before the camera opens, and started again with the
        // effect in this step (once) or the next.
        if self.streams_without_black_effect() {
            warning!(
                "the effect can run now under on_failure = \"{}\": not streaming without it",
                self.effect().on_failure
            );
            self.drop_session();
            self.privacy_hold = Some(PrivacyHold::Guard);
            return again;
        }
        // Turned off during the (slow) output build: no camera is opened;
        // the queued SetEnabled(false) is handled next.
        if self.published.disable_pending() {
            info!("disabled while starting: the camera is not opened");
            self.drop_session();
            return false;
        }
        if let Err(last) = self.try_cameras(&candidates, now) {
            self.drop_session();
            self.set_fault(last.unwrap_or_else(|| "no camera is available".into()));
        }
        false
    }

    /// Before the session runs `effect` (started, given a camera or
    /// changed): the crash ledger's markers of the modules whose code it
    /// runs and that are not marked yet are set. A marker stays until the
    /// session stops: a module that ran may keep its code loaded.
    fn mark_running(&mut self, effect: &EffectRun) {
        let Some(plan) = &self.plan else {
            return;
        };
        let new: Vec<String> = plan
            .running_ids(effect)
            .into_iter()
            .filter(|id| !self.marked.contains(id))
            .collect();
        self.modules.session_started(&new);
        self.marked.extend(new);
    }

    /// Whether the output can be built with `effect`'s settings: `replace`
    /// has an image, and that image can be read.
    fn effect_settings_check(effect: &EffectRun) -> Result<(), String> {
        if effect.mode != EffectMode::Replace {
            return Ok(());
        }
        match &effect.background_image {
            None => Err("the replace effect needs a background image".into()),
            Some(p) => check_background_file(p),
        }
    }

    /// Whether `effect` can run: the backend says it is available (its
    /// module's probe, and the settings as the module checks them). It is
    /// only asked of an effect a module provides: one no module provides
    /// is dropped before ([`Self::effect_run_with`]), never a failure.
    fn effect_check(&mut self, effect: &EffectRun) -> Result<(), String> {
        let plan = self.current_plan();
        let Some(module) = &plan.effect else {
            return Ok(());
        };
        // The backend's check runs the module's probe (which may open its
        // own libraries): under the crash ledger's marker.
        self.modules.probe_started(&module.id);
        let res = self.backend.effect_available(effect, &plan.backends());
        self.modules.probe_ended(&module.id);
        res
    }

    /// The modules keep the module the session's plan runs the background
    /// effect from open while the plan is in force, refused or not
    /// ([`Modules::hold`]): a refusal never turns a running effect into the
    /// raw picture, it applies at the next start. When the hold ends, the
    /// module rules are followed (the side of a conflict kept is opened).
    fn hold_background(&mut self) {
        let held = self
            .plan
            .as_ref()
            .and_then(|p| p.provider(&StageId::BACKGROUND))
            .map(str::to_string);
        self.modules
            .hold(&mut self.backend, &self.config, held.as_deref());
    }

    /// The running session's effect element keeps its backend while the
    /// module the session started the background effect with can run it.
    /// Under an `on_failure` that hides the camera (`black`, `image`), a
    /// session that lost that module ([`Self::background_lost`]: disabled,
    /// a failed probe, a failed reopen on a reload) has the element drop
    /// its backend instance ([`MediaBackend::revoke_effect_backend`]): it
    /// fails the effect at once and shows black or the fallback image, the
    /// effect still asked of it ([`Self::fails_closed`]) and the camera
    /// kept open. Once the module can run it again (or the effect is no
    /// longer to be hidden), the element takes its backend back
    /// ([`MediaBackend::restore_effect_backend`]) and the effect is given
    /// again ([`Self::follow_background`]), with no restart. Under `none`
    /// the effect is dropped instead (the camera image), and a media
    /// backend that cannot revoke drops it too.
    ///
    /// Only while a background effect is selected: with the effect off
    /// (mode `none`) the element runs the stages alone, which the module
    /// may still run (a probe that lost the background stage only), and a
    /// revocation would fail them. An effect selected again is followed
    /// here before it is given ([`Self::give_effect`]), so the element
    /// never runs it on the lost module's instance. The element's backend
    /// taken back, the effect's failure for want of it
    /// ([`Self::effect_failure`]) is over: the effect reads as loading
    /// until it is ready, as at a start.
    fn follow_effect_backend(&mut self) {
        let revoke = self.session.is_some()
            && self.effect().mode != EffectMode::None
            && self.effect().on_failure.hides()
            && self.backend.can_revoke_effect_backend()
            && self.plan.as_ref().is_some_and(|p| self.background_lost(p));
        if revoke == self.background_revoked {
            return;
        }
        let done = if revoke {
            info!(
                "the session's effect module cannot run the background effect: its backend dropped"
            );
            self.backend.revoke_effect_backend()
        } else {
            info!(
                "the session's effect module can run the background effect again: its backend taken back"
            );
            self.backend.restore_effect_backend()
        };
        match done {
            Ok(()) => {
                self.background_revoked = revoke;
                if !revoke {
                    // The element loads the effect again: the next poll
                    // reads it loading (or ready), not failed.
                    self.effect_failure = None;
                    self.effect_seen = None;
                }
            }
            Err(e) => warning!("the effect element's backend: {e}"),
        }
    }

    /// Gives `effect` to the running session, its effect element's backend
    /// followed first ([`Self::follow_effect_backend`]).
    fn give_effect(&mut self, effect: &EffectRun) -> Result<(), String> {
        self.follow_effect_backend();
        self.backend.set_effect(effect)
    }

    /// Stops the session. An effect failure of that session (in
    /// `Warning`), a noise removal failure, a relighting failure and an
    /// enhancement failure end with it: the stages' `Error` gives way to
    /// the causes still standing.
    fn drop_session(&mut self) {
        self.stop_watch();
        if self.session.take().is_some() {
            self.backend.stop_session();
        }
        // The modules' code stopped (cleanly: the daemon is still here).
        self.plan = None;
        self.background_revoked = false;
        self.hold_background();
        self.passthrough = None;
        let marked = std::mem::take(&mut self.marked);
        self.modules.session_ended(&marked);
        self.next_stats = None;
        self.effect_seen = None;
        self.effect_polled = false;
        self.effect_applied = None;
        self.effect_log_at = None;
        self.enhance_seen = None;
        let stages = self.take_stage_failures();
        self.stages.clear();
        self.effect_failure = None;
        self.fallback_failure = None;
        let enhance = self.enhance_failure.take();
        if stages || enhance.is_some_and(|e| e == self.error) {
            self.error = self.standing_error();
        }
    }

    /// The session output could not be built with `contract` (`e`): when
    /// that is the configured contract, another one is streamed instead
    /// from the next writer rebuild (once no application holds the virtual
    /// camera, which keeps the format it opened): the last one that
    /// started a session in this run, else the last good one of an earlier
    /// run (`output_good`), else the built-in default.
    fn session_contract_failed(&mut self, contract: &OutputContract, e: &str) {
        if let Some(good) = self.session_fallback(contract)
            && *contract == self.config.output_contract()
        {
            warning!(
                "the output {} could not be built ({e}): {} is streamed instead once the \
                 virtual camera is released",
                output_mode_string(contract),
                output_mode_string(&good)
            );
            self.failed_contract = Some((*contract, e.to_string(), good));
        }
    }

    /// A session output that could not start with `params` (`why`) is
    /// started once more at the same contract without the effect and the
    /// module stages (an empty plan: the frames pass through), so that the
    /// application that opened the virtual camera gets the camera rather
    /// than an error. Not when a background effect is asked with
    /// `on_failure = "black"` (the user's choice is black over an
    /// unprocessed picture), nor when there is nothing to drop. The params
    /// it started with, `None` if it was not tried or failed too.
    fn start_passthrough(&mut self, params: &SessionParams, why: &str) -> Option<SessionParams> {
        let black = Self::hides_effect(&params.effect);
        let bare_plan = Plan::default();
        if black || plan_ids(&bare_plan) == plan_ids(&self.current_plan()) {
            return None;
        }
        let plan = self.plan.replace(bare_plan);
        let bare = SessionParams {
            contract: params.contract,
            ..self.session_params()
        };
        if let Err(e) = self.backend.start_session(&bare) {
            warning!("the output could not start without its effects either: {e}");
            self.plan = plan;
            return None;
        }
        let sr = |c: &OutputContract| mode_value(c.width, c.height, c.fps_num, c.fps_den);
        warning!(
            "the output {} could not start with its effects ({why}): streaming without them",
            output_mode_string(&params.contract)
        );
        self.passthrough = Some(format!(
            "Output {} could not start with its effects ({why}): streaming without them",
            sr(&params.contract)
        ));
        self.effect_applied = Some(bare.effect.clone());
        Some(bare)
    }

    /// What a session output that cannot be built with `contract` falls
    /// back to ([`Self::session_contract_failed`]), never `contract`.
    fn session_fallback(&self, contract: &OutputContract) -> Option<OutputContract> {
        [
            self.session_good,
            self.runtime.output_good,
            Some(OutputContract::default()),
        ]
        .into_iter()
        .flatten()
        .find(|g| g != contract)
    }

    /// A writer and a session output started with `contract`, and a camera
    /// frame reached the output: it is the last good one, kept in the state file (write-behind) for the next
    /// run, unless the file is damaged (left alone until `SetEnabled`).
    fn output_proven(&mut self, contract: OutputContract, now: Instant) {
        self.session_good = Some(contract);
        if self.runtime.output_good != Some(contract) && self.state_damaged.is_none() {
            self.runtime.output_good = Some(contract);
            self.settings_changed(now);
        }
    }

    /// A lifecycle fault that is not privacy's: it ends any privacy hold
    /// (the placeholder is black, as for any other fault).
    fn set_fault(&mut self, msg: String) {
        self.privacy_hold = None;
        self.fault_is(msg);
    }

    fn fault_is(&mut self, msg: String) {
        error!("{msg}");
        self.error = msg.clone();
        self.fault = Some(msg);
    }

    /// [`Self::set_fault`] for a session refused for privacy: the effect
    /// cannot run and `on_failure` hides the camera. While the fault
    /// stands, privacy holds the feed ([`Self::privacy_hold`]).
    fn set_privacy_fault(&mut self, msg: String) {
        self.privacy_hold = Some(PrivacyHold::Fault(msg.clone()));
        self.fault_is(msg);
    }

    /// A background effect is asked of the session with an `on_failure`
    /// that hides the camera: the session never streams without it.
    fn hides_effect(effect: &EffectRun) -> bool {
        effect.mode != EffectMode::None && effect.on_failure.hides()
    }

    /// The image the writer's placeholder shows now: the fallback image
    /// while privacy holds the feed ([`Self::privacy_hold`]: enabled, a
    /// consumer, and either no session with the hold's fault still
    /// standing, or a session started again under the hold that has not
    /// streamed yet) and a background effect is selected with
    /// `on_failure = "image"` and an image; `None` (black) in every other
    /// case: off, idle, a camera switch's gap of a session that streamed or
    /// another fault. A hold that no longer applies is forgotten: once the
    /// session's first frame reached the output, its frames take over and
    /// the hold ends. A start tried again under the hold so keeps the image
    /// (a still, never the camera) until then, not black meanwhile.
    fn placeholder_image(&mut self) -> Option<PathBuf> {
        let live = self.session.as_ref().is_some_and(|s| s.live);
        if live || !self.runtime.enabled || self.consumers == 0 {
            self.privacy_hold = None;
        }
        // The fault a start tried again cleared: the hold stands until
        // that session streams, or is refused (a fault of its own).
        if self.session.is_none()
            && let Some(PrivacyHold::Fault(f)) = &self.privacy_hold
            && self.fault.as_ref() != Some(f)
        {
            self.privacy_hold = None;
        }
        let effect = self.effect();
        if self.privacy_hold.is_none()
            || effect.mode == EffectMode::None
            || effect.on_failure != EffectOnFailure::Image
        {
            return None;
        }
        effect.fallback_image
    }

    /// Gives the writer's placeholder the image it shows now
    /// ([`Self::placeholder_image`]) when that changed, and reads whether
    /// it can.
    fn sync_placeholder(&mut self) {
        let want = self.placeholder_image();
        if want != self.placeholder_set {
            match &want {
                Some(p) => info!(
                    "privacy holds the feed: the placeholder shows the fallback image {}",
                    p.display()
                ),
                None if self.placeholder_set.is_some() => info!("the placeholder is black again"),
                None => {}
            }
            self.backend.set_placeholder(want.as_deref());
            self.placeholder_set = want;
            self.restart_placeholder_wait();
        }
        self.placeholder_pending =
            self.placeholder_set.is_some() && self.backend.placeholder_pending();
        if !self.placeholder_pending {
            self.restart_placeholder_wait();
        }
        self.placeholder_failure = match &self.placeholder_set {
            Some(p) if self.placeholder_pending && self.placeholder_overdue => {
                Some(obc_pipeline::effect::fallback_unreadable(
                    p,
                    &format!(
                        "not decoded within {} s",
                        PLACEHOLDER_DECODE_LIMIT.as_secs()
                    ),
                ))
            }
            Some(_) => self.backend.placeholder_failure(),
            None => None,
        };
    }

    /// Times the writer's placeholder decode a step finds still pending
    /// ([`Self::placeholder_pending`], as last read): overdue once it has
    /// been for [`PLACEHOLDER_DECODE_LIMIT`].
    fn watch_placeholder(&mut self, now: Instant) {
        if !self.placeholder_pending {
            return;
        }
        let since = *self.placeholder_pending_since.get_or_insert(now);
        self.placeholder_overdue = now.saturating_duration_since(since) >= PLACEHOLDER_DECODE_LIMIT;
    }

    /// The placeholder's decode is waited for afresh: the image was given
    /// to it again, or its outcome is known.
    fn restart_placeholder_wait(&mut self) {
        self.placeholder_pending_since = None;
        self.placeholder_overdue = false;
    }

    /// `Warning`'s clauses of the fallback image: `on_failure = "image"`
    /// without one (black is shown in its place, whenever the camera is
    /// hidden), and an image the writer's placeholder or the running
    /// session's effect element cannot read.
    fn fallback_warnings(&self) -> Vec<String> {
        let effect = self.effect();
        let mut out = Vec::new();
        if effect.on_failure == EffectOnFailure::Image && effect.fallback_image.is_none() {
            out.push(
                "effect.on_failure is \"image\" but effect.fallback_image is not set: showing \
                 black"
                    .to_string(),
            );
        }
        out.extend(self.placeholder_failure.clone());
        if self.session.is_some() {
            out.extend(self.fallback_failure.clone());
        }
        out
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
        self.apply_camera_modes();
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
                s.enhance = params.effect.enhance;
                s.enhance_params = params.effect.enhance_params.clone();
            }
            info!("switching to {}", cam.label());
            self.mark_running(&params.effect);
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
        self.frame_params_changed(changed, now)
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
    /// with `none`): the switch of the `denoise` stage (the setting
    /// `effect.video.denoise` of revision 8). Kept whether or not a module
    /// can run it now, as revision 8 kept it.
    fn set_denoise(&mut self, on: bool, now: Instant) -> Result<(), IpcError> {
        let entries = self.global_settings();
        let (id, label) = entries
            .iter()
            .find(|e| e.binding == Binding::Enable(StageId::DENOISE))
            .map_or_else(
                || (legacy::DENOISE.to_string(), "Noise removal".to_string()),
                |e| (e.setting.id.clone(), e.setting.label.clone()),
            );
        self.set_module_setting(
            &id,
            &label,
            &Binding::Enable(StageId::DENOISE),
            StoredValue::Bool(on),
            now,
        )
    }

    /// A write of a module setting `id` (labelled `label`), checked: kept
    /// in the state file by its id, then applied as its binding says. A
    /// stage's switch is applied to the running session at once (the
    /// enhance stage's by reopening the camera, [`Self::follow_enhance`]);
    /// a `live` parameter is given to each frame (no reload, never a
    /// retry); a `reload` parameter reloads the stage's chain; a `restart`
    /// parameter, or a setting bound to no stage, takes effect at the next
    /// session.
    fn set_module_setting(
        &mut self,
        id: &str,
        label: &str,
        binding: &Binding,
        value: StoredValue,
        now: Instant,
    ) -> Result<(), IpcError> {
        let changed = self.runtime.settings.get(id) != Some(&value);
        let what = label.to_lowercase();
        match binding {
            Binding::Enable(stage) => {
                let on = if value == StoredValue::Bool(true) {
                    "on"
                } else {
                    "off"
                };
                // The enhance stage's own words.
                let what = if *stage == StageId::ENHANCE {
                    "enhance"
                } else {
                    &what
                };
                info!("{what} selected: {on}");
            }
            // A slider sends many: not worth the journal.
            Binding::Param(_, obc_module_host::settings::Apply::Live) => {
                debug!("{what}: {value}");
            }
            _ => info!("{what}: {value}"),
        }
        self.runtime.settings.insert(id.to_string(), value);
        match binding {
            Binding::Enable(stage) => {
                self.effect_changed_with(changed, *stage == StageId::ENHANCE, now)
            }
            Binding::Param(_, obc_module_host::settings::Apply::Live) => {
                self.frame_params_changed(changed, now)
            }
            // The enhance stage's are the ingest's: its camera reopens with
            // them ([`Self::follow_enhance`]), as for its switch.
            Binding::Param(stage, obc_module_host::settings::Apply::Reload) => {
                self.effect_changed_with(changed, *stage == StageId::ENHANCE, now)
            }
            _ => {
                let res = if changed { self.save_state() } else { Ok(()) };
                self.reconcile(now);
                res
            }
        }
    }

    /// The effect settings changed by a method: saved (unless `changed` is
    /// false: the state file already holds them), applied to the running
    /// session without restarting it, and published before the reply. A
    /// control call clears a fault (an effect that could not run is tried
    /// again), the `Warning` of a session running without its effect
    /// (under `black` once the retry reports: [`Self::effect_failure`])
    /// and the `Error` of one without noise removal or relighting.
    fn effect_changed(&mut self, changed: bool, now: Instant) -> Result<(), IpcError> {
        self.effect_changed_with(changed, false, now)
    }

    /// A parameter of each frame changed by a method (the blur strength,
    /// the relighting direction or brightness): saved (unless `changed` is
    /// false) and given to the running session as it is, never as a retry.
    /// A slider sends many, and a retry reloads what failed (a model load
    /// per tick), so a failure, its `Error` and a fault stay as they are:
    /// only a toggle, a new light or image, a reset or a reload retries.
    /// A fault is the exception: with the session stopped in error there is
    /// nothing to give the value to, and any control call clears a fault
    /// ([`Self::effect_changed`]), once (the session it starts is not in
    /// error).
    fn frame_params_changed(&mut self, changed: bool, now: Instant) -> Result<(), IpcError> {
        if self.fault.is_some() {
            return self.effect_changed(changed, now);
        }
        let mut res = if changed { self.save_state() } else { Ok(()) };
        if self.session.is_some()
            && let Err(e) = self.apply_frame_params()
        {
            res = Err(e);
        }
        self.reconcile(now);
        res
    }

    /// Gives the running session the per-frame parameters in force on top
    /// of what it was last given ([`Self::apply_effect`] when it was given
    /// nothing yet), so that nothing it loads changes and nothing failed is
    /// retried: the effect element takes a change of these alone as one
    /// without a retry.
    fn apply_frame_params(&mut self) -> Result<(), IpcError> {
        let Some(prev) = self.effect_applied.clone() else {
            return self.apply_effect();
        };
        let run = self.effect_run();
        let mut effect = prev.clone();
        effect.blur_strength = run.blur_strength;
        for s in effect
            .stages
            .iter_mut()
            .chain(effect.chains.iter_mut().flat_map(|c| c.stages.iter_mut()))
        {
            if let Some(now) = run.stage(s.id.as_str()) {
                s.live = now.live.clone();
            }
        }
        if effect == prev {
            return Ok(());
        }
        self.effect_applied = Some(effect.clone());
        self.mark_running(&effect);
        let Err(reason) = self.give_effect(&effect) else {
            return Ok(());
        };
        Err(self.effect_refused(effect, &reason))
    }

    /// [`Self::effect_changed`], and the open camera reopened when what its
    /// ingest should have of enhancement changed ([`Self::follow_enhance`]);
    /// with `enhance`, the setting itself was written: its failure is
    /// cleared and, still requested, tried again.
    fn effect_changed_with(
        &mut self,
        changed: bool,
        enhance: bool,
        now: Instant,
    ) -> Result<(), IpcError> {
        if let Some(f) = self.fault.take()
            && self.error == f
        {
            self.error = self.standing_error();
        }
        let stages = self.take_stage_failures();
        // Tried again below: the next report says whether it still fails.
        // Under `black` the frames stay black meanwhile, so the text
        // stands until that report (ready clears it, a failure replaces
        // it); with no effect selected, or under `none` (the retry loads
        // it, the frames black meanwhile), it is no longer true.
        let effect = self.effect();
        if effect.mode == EffectMode::None || effect.on_failure == EffectOnFailure::None {
            self.effect_failure = None;
        }
        if stages {
            self.error = self.standing_error();
        }
        let retry = enhance && self.clear_enhance_failure();
        let mut res = if changed { self.save_state() } else { Ok(()) };
        // A stage selected now that a module provides but the running
        // session cannot run (started without that module): a new session
        // runs it ([`Self::restart_for`]).
        self.restart_for_modules();
        if self.session.is_some()
            && let Err(e) = self.apply_effect()
        {
            res = Err(e);
        }
        if let Err(e) = self.follow_enhance(retry, now)
            && res.is_ok()
        {
            res = Err(e);
        }
        self.reconcile(now);
        res
    }

    /// Applies the effect in force to the running session. Settings the
    /// session already has are not given again, unless the effect, noise
    /// removal or relighting failed (a retry). A new effect or image, or a
    /// retry of the effect, is followed from its load (its status is
    /// reported again), as is a new noise removal or relighting request (a
    /// new light included) or its retry; a strength, direction or
    /// brightness change is not (those go through
    /// [`Self::apply_frame_params`], never a retry). A setting the session
    /// cannot take is an effect failure
    /// ([`Self::effect_failed`]), and the method's error.
    fn apply_effect(&mut self) -> Result<(), IpcError> {
        self.apply_effect_with(false)
    }

    /// [`Self::apply_effect`]; with `again`, the effect is given to the
    /// session even when unchanged (its element reads a replaced fallback
    /// image again).
    fn apply_effect_with(&mut self, again: bool) -> Result<(), IpcError> {
        let effect = self.effect_run();
        let failed = matches!(self.effect_seen, Some(EffectStatus::Failed(_)));
        let prev = self.effect_applied.clone();
        let all = |e: &EffectRun| -> Vec<StageRun> {
            e.stages
                .iter()
                .chain(e.chains.iter().flat_map(|c| c.stages.iter()))
                .cloned()
                .collect()
        };
        let stages = all(&effect);
        let any_failed = stages.iter().any(|s| self.stage_failed(&s.id));
        if !again && !failed && !any_failed && prev.as_ref() == Some(&effect) {
            return Ok(());
        }
        if failed
            || prev.as_ref().is_none_or(|p| {
                p.mode != effect.mode || p.background_image != effect.background_image
            })
        {
            self.effect_seen = None;
        }
        // A stage turned on or off, given new load-time parameters (a new
        // light), or failed, is followed from its load.
        for s in &stages {
            let before = prev.as_ref().and_then(|p| p.stage(s.id.as_str()));
            let renewed = before.is_none_or(|b| b.enabled != s.enabled || b.params != s.params);
            if self.stage_failed(&s.id) || renewed {
                let t = self.track(&s.id);
                t.seen = None;
                t.held = None;
            }
        }
        self.effect_applied = Some(effect.clone());
        self.mark_running(&effect);
        let Err(reason) = self.give_effect(&effect) else {
            return Ok(());
        };
        Err(self.effect_refused(effect, &reason))
    }

    /// The session refused `effect` for `reason` (settings its output
    /// cannot take: an image that can no longer be read). The session
    /// still runs what it had before, which may show the room, so with
    /// `on_failure = "black"` it stops and the state is `error` (the
    /// writer's placeholder keeps the stream black); with `"none"` it
    /// streams on without the effect and `Warning` says why. The error the
    /// method answers.
    fn effect_refused(&mut self, effect: EffectRun, reason: &str) -> IpcError {
        let msg = effect_failure_text(&effect, reason);
        match effect.on_failure {
            EffectOnFailure::Black | EffectOnFailure::Image => {
                info!("stopping the camera: the session refused the effect");
                self.drop_session();
                self.set_privacy_fault(msg.clone());
            }
            EffectOnFailure::None => {
                warning!("{msg}");
                self.effect_failure = Some(effect_warning_text(&effect, reason));
                // Streaming on without the effect, as `none` says.
                let off = EffectRun {
                    mode: EffectMode::None,
                    ..effect
                };
                if let Err(e) = self.backend.set_effect(&off) {
                    warning!("turning the effect off: {e}");
                }
                self.effect_applied = Some(off);
            }
        }
        IpcError::Failed(msg)
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
        self.follow_frame_generation();
        let ids = self.apply_camera_controls(camera);
        self.start_watch(camera, &ids);
    }

    /// The session is about to leave the camera it has open (a switch, a
    /// reopen, a fail-over): its watcher stops, a settings change not
    /// saved yet is saved first, and an enhancement failure of that ingest
    /// ends with it.
    fn leaving_camera(&mut self) {
        self.stop_watch();
        self.flush_settings();
        self.clear_enhance_failure();
    }

    /// The capture mode setting of camera `id` (`None`: no such camera, or
    /// its modes are not known).
    fn mode_setting_of(&self, id: &str) -> Option<obc_ipc::Setting> {
        let cam = self.cameras.iter().find(|c| c.id == id)?;
        settings::mode_setting(&cam.modes, self.runtime.camera_modes.get(id))
    }

    /// The mode asked of camera `id`: the one the user chose for it, else
    /// (or while that one could not open, [`Self::mode_failed`]) the
    /// config's `camera.mode`.
    fn mode_request_for(&self, id: &str) -> ModeRequest {
        self.runtime
            .camera_modes
            .get(id)
            .filter(|_| !self.mode_failed.contains_key(id))
            .copied()
            .unwrap_or_else(|| self.config.camera_mode())
    }

    /// Camera `id` failed (`why`) before its first frame in the mode
    /// chosen for it: unless that already happened in this run, or its
    /// automatic mode is the same one, the failure is put down to the mode
    /// ([`Self::mode_failed`]) and the camera, in its automatic mode, is
    /// returned to be tried once more.
    fn blame_mode(&mut self, id: &str, why: &str) -> Option<Camera> {
        if self.mode_failed.contains_key(id) {
            return None;
        }
        let asked = *self.runtime.camera_modes.get(id)?;
        let cam = self.cameras.iter().find(|c| c.id == id)?;
        let (before, modes) = (cam.mode, cam.modes.clone());
        self.mode_failed.insert(
            id.to_string(),
            ModeFailed {
                asked,
                why: why.to_string(),
                modes,
                confirmed: false,
            },
        );
        self.apply_camera_modes();
        let cam = self.cameras.iter().find(|c| c.id == id)?.clone();
        if cam.mode == before {
            self.mode_failed.remove(id);
            self.apply_camera_modes();
            return None;
        }
        warning!(
            "{} could not open {} ({why}): trying its automatic mode {}",
            cam.label(),
            settings::mode_choice_value(&asked),
            cam.mode
        );
        Some(cam)
    }

    /// Camera `id` failed in its automatic mode before a first frame there:
    /// its chosen mode is not to blame. An unconfirmed record
    /// ([`ModeFailed::confirmed`]) goes, the chosen mode applies again
    /// (tried at the camera's next open); a confirmed one stays.
    fn forget_unconfirmed_mode(&mut self, id: &str) {
        if self.mode_failed.get(id).is_some_and(|f| !f.confirmed) {
            self.mode_failed.remove(id);
            self.apply_camera_modes();
        }
    }

    /// Each camera whose modes are known takes the mode chosen for what is
    /// asked of it ([`Self::mode_request_for`], [`choose_mode`]: the size,
    /// then the rate, then NV12 over YUY2 over MJPEG; a mode it does not
    /// offer gives the nearest). The others keep the scan's.
    fn apply_camera_modes(&mut self) {
        // A camera gone, or reporting other modes, is no longer blamed.
        let cameras = &self.cameras;
        self.mode_failed
            .retain(|id, f| cameras.iter().any(|c| c.id == *id && c.modes == f.modes));
        for i in 0..self.cameras.len() {
            if self.cameras[i].modes.is_empty() {
                continue;
            }
            let req = self.mode_request_for(&self.cameras[i].id);
            let caps = Capabilities {
                modes: self.cameras[i].modes.clone(),
                other_formats: Vec::new(),
            };
            if let Some(choice) = choose_mode(&caps, &req) {
                self.cameras[i].mode = choice.mode;
            }
        }
    }

    /// The cameras take their modes again ([`Self::apply_camera_modes`]):
    /// the session's open camera, when its mode changed, is reopened in the
    /// new one (through the switch path: the output holds the last frame,
    /// and the loopback is not touched), and its stages follow (frame
    /// generation runs only while the output rate is twice the camera's).
    fn follow_camera_modes(&mut self, now: Instant) {
        let before = self.session_camera().map(|c| c.mode);
        self.apply_camera_modes();
        let Some(cam) = self.session_camera().cloned() else {
            return;
        };
        if before.is_some_and(|m| m != cam.mode) && self.session.as_ref().is_some_and(|s| s.open) {
            info!("camera mode {}: reopening {}", cam.mode, cam.label());
            if self.reopen_camera(&cam, now).is_ok() {
                self.follow_stages();
            }
        }
    }

    /// `SetSetting` of the capture mode ([`settings::MODE`]) on `camera`:
    /// checked against its options, stored for that camera (`auto`: the
    /// choice forgotten, the config's `camera.mode` applies), saved and
    /// applied at once ([`Self::follow_camera_modes`]).
    fn set_camera_mode(
        &mut self,
        camera: &str,
        value: Option<&SettingValue>,
        now: Instant,
    ) -> Result<(), IpcError> {
        let cam = self.control_camera(camera)?;
        let setting = settings::mode_setting(&cam.modes, self.runtime.camera_modes.get(&cam.id));
        let want = settings::check_mode(setting.as_ref(), &cam.id, value).map_err(refusal_error)?;
        let old = match want {
            Some(m) => self.runtime.camera_modes.insert(cam.id.clone(), m),
            None => self.runtime.camera_modes.remove(&cam.id),
        };
        // A mode that could not open is tried again when chosen again.
        let failed = self.mode_failed.remove(&cam.id).is_some();
        if old == want && !failed {
            return Ok(());
        }
        info!(
            "{}: capture mode {}",
            cam.label(),
            want.as_ref()
                .map_or_else(|| "automatic".to_string(), settings::mode_choice_value)
        );
        let res = self.settings_saved(now);
        self.follow_camera_modes(now);
        self.reconcile(now);
        res
    }

    /// Forgets the capture mode chosen for `camera` (a reset of the whole
    /// camera), the save scheduled; whether there was one.
    fn forget_camera_mode(&mut self, camera: &str, now: Instant) -> bool {
        let Ok(cam) = self.control_camera(camera) else {
            return false;
        };
        self.mode_failed.remove(&cam.id);
        let forgot = self.runtime.camera_modes.remove(&cam.id).is_some();
        if forgot {
            self.settings_changed(now);
        }
        forgot
    }

    /// Reads who holds the loopback while an output change waits for them
    /// (again when their count or the contract waiting changes), for
    /// `Warning`; forgets them when nothing waits.
    fn read_holders(&mut self) {
        let waiting = self.loopback_consumers > 0
            && self
                .writer
                .as_ref()
                .is_some_and(|w| w.contract != self.wanted_contract());
        if !waiting {
            self.holders_read = None;
            self.holder_names.clear();
            return;
        }
        let key = (self.loopback_consumers, self.wanted_contract());
        if self.holders_read == Some(key) {
            return;
        }
        if let Some(node) = self.writer.as_ref().map(|w| w.node.clone()) {
            self.holder_names = self.backend.loopback_holder_names(&node);
        }
        self.holders_read = Some(key);
    }

    /// Reopens the session's `camera` (a new ingest on the same camera,
    /// through the switch path: the output holds the last frame), with the
    /// params it takes now. A failure is the camera's
    /// ([`Self::camera_failed`]), and the error returned.
    fn reopen_camera(&mut self, cam: &Camera, now: Instant) -> Result<(), IpcError> {
        self.leaving_camera();
        let params = self.session_params_for(cam);
        if let Some(s) = self.session.as_mut() {
            // A camera that has not delivered yet keeps its deadline: reopens
            // (enhancement toggled before the first frame) never postpone
            // the start timeout.
            s.awaiting = Some(s.awaiting.unwrap_or(now));
            s.open = false;
            s.pinned = params.fixed_framerate;
            s.enhance = params.effect.enhance;
            s.enhance_params = params.effect.enhance_params.clone();
        }
        self.mark_running(&params.effect);
        match self.backend.switch_camera(&params, cam) {
            Ok(()) => {
                self.camera_opened(cam);
                Ok(())
            }
            Err(e) => {
                let msg = format!("reopening {}: {e}", cam.label());
                self.camera_failed(msg.clone(), now);
                Err(IpcError::Failed(msg))
            }
        }
    }

    /// Reopens the session's open camera when whether its ingest should
    /// have the enhance stage ([`Self::enhance_for`]), or the stage's
    /// parameters ([`Self::enhance_params_for`]), differ from what it was
    /// built with, or, with `retry`, when it should have it (a failed
    /// stage tried again). Nothing without a session or an open camera:
    /// it applies at the next open.
    fn follow_enhance(&mut self, retry: bool, now: Instant) -> Result<(), IpcError> {
        let Some(s) = &self.session else {
            return Ok(());
        };
        if !s.open {
            return Ok(());
        }
        let has = (s.enhance, s.enhance_params.clone());
        let Some(cam) = self.session_camera().cloned() else {
            return Ok(());
        };
        let want = (self.enhance_for(&cam), self.enhance_params_for(&cam));
        if want == has && !(retry && want.0) {
            return Ok(());
        }
        let what = match (want.0, has.0) {
            (true, true) => "settings changed",
            (true, false) => "on",
            (false, _) => "off",
        };
        info!("enhance {what}: reopening {}", cam.label());
        self.reopen_camera(&cam, now)
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
        let mut built = built;
        built.global = self.global_settings();
        built.mic = self.mic_groups();
        built.mode = self.mode_setting_of(&camera.id);
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
                    mode: self.mode_setting_of(&active),
                    camera: active,
                    global: self.global_settings(),
                    mic: self.mic_groups(),
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
                let none = CameraSettings {
                    global: self.global_settings(),
                    mic: self.mic_groups(),
                    ..CameraSettings::none()
                };
                return Ok(none.tree(rev, serial));
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
        if let SetRequest::Setting { id, value } = &req
            && mic::is_mic(id)
        {
            self.set_mic_setting(id, value.as_ref(), reply, now);
            return;
        }
        if let SetRequest::Setting { id, value } = &req
            && effects::is_global(id)
        {
            let res = self.set_global(id, value.as_ref(), now);
            let _ = reply.send(res);
            return;
        }
        if let SetRequest::Setting { id, value } = &req
            && id == settings::MODE
        {
            let res = self.set_camera_mode(camera, value.as_ref(), now);
            let _ = reply.send(res);
            return;
        }
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

    /// `SetSetting` on a global setting (the camera option is ignored):
    /// validated against the tree as it is now (unknown, disabled, invalid),
    /// then exactly the existing setter (`SetEffect` or `SetBlurStrength`,
    /// or the module setting's, [`Self::set_module_setting`]), with its
    /// checks, errors, saving and publishing. Not a device write: neither held by the coalescing
    /// window nor counted in the cap; the blur strength and the relighting
    /// direction and brightness are parameters the effect reads at each
    /// frame, so a stream of values costs no reload.
    fn set_global(
        &mut self,
        id: &str,
        value: Option<&SettingValue>,
        now: Instant,
    ) -> Result<(), IpcError> {
        let entries = self.global_settings();
        match effects::check(&entries, id, value).map_err(refusal_error)? {
            Write::Mode(mode) => self.set_effect(mode.as_str(), now),
            Write::BlurStrength(v) => self.set_blur_strength(v, now),
            Write::Setting {
                id,
                label,
                binding,
                value,
            } => self.set_module_setting(&id, &label, &binding, value, now),
        }
    }

    /// `ResetSettings` of a global group or setting (the camera option is
    /// ignored): the state file's overrides in scope are forgotten, so the
    /// configuration's values apply again, at once and to the running
    /// session, as an effect method does (saved, a fault or effect failure
    /// cleared). `None` when the scope is not global.
    fn reset_global(&mut self, what: &ResetWhat, now: Instant) -> Option<Result<(), IpcError>> {
        let entries = self.global_settings();
        let ids: Vec<String> = match what {
            ResetWhat::Group(g) if entries.iter().any(|e| e.group == *g) => entries
                .iter()
                .filter(|e| e.group == *g)
                .map(|e| e.setting.id.clone())
                .collect(),
            ResetWhat::Setting(id) if effects::is_global(id) => {
                if !entries.iter().any(|e| e.setting.id == *id)
                    && !self.runtime.settings.contains_key(id)
                {
                    return Some(Err(IpcError::UnknownSetting(format!(
                        "there is no setting {id:?}"
                    ))));
                }
                vec![id.clone()]
            }
            _ => return None,
        };
        let mut changed = false;
        for id in &ids {
            changed |= effects::forget(&mut self.runtime, id);
        }
        info!("{} reset to the configuration's value", ids.join(", "));
        let enhance = entries
            .iter()
            .any(|e| ids.contains(&e.setting.id) && e.binding == Binding::Enable(StageId::ENHANCE));
        Some(self.effect_changed_with(changed, enhance, now))
    }

    // ---- modules ----

    /// The stage order in force, by id.
    #[cfg(test)]
    pub(crate) fn order_ids(&self) -> Vec<String> {
        self.modules.order().iter().map(|s| s.to_string()).collect()
    }

    /// `GetModules`.
    fn module_list(&self) -> obc_ipc::ModuleList {
        obc_ipc::ModuleList {
            serial: self.modules_serial(),
            modules: mic::merge_modules(self.modules.info(), &self.mic_catalogue.modules),
        }
    }

    /// `ModulesSerial`: the registry's, moved on by the worker's modules.
    fn modules_serial(&self) -> u32 {
        self.modules.serial().wrapping_add(self.mic_serial)
    }

    /// The stages in force, each with its slot and whether the order with
    /// it swapped with its neighbour before (`up`) or after (`down`) is one
    /// `SetStageOrder` accepts (the same check). No move is when a file
    /// after the managed one sets the order (`SetStageOrder` refuses).
    fn stage_moves(&self) -> Vec<StageMove> {
        let order: Vec<String> = self.order_strings();
        let movable = self.config_set.overridden_by("modules.order").is_empty();
        let info = self.modules.info();
        let slot_of = |stage: &str| {
            let stages = || {
                info.iter()
                    .flat_map(|m| m.stages.iter())
                    .filter(|s| s.id == stage)
            };
            stages()
                .find(|s| s.selected)
                .or_else(|| stages().next())
                .map(|s| s.slot.clone())
                .unwrap_or_default()
        };
        let swapped_ok = |i: usize| {
            let mut o = order.clone();
            o.swap(i, i + 1);
            movable && self.modules.validate_order(&o).is_ok()
        };
        (0..order.len())
            .map(|i| StageMove {
                id: order[i].clone(),
                slot: slot_of(&order[i]),
                up: i > 0 && swapped_ok(i - 1),
                down: i + 1 < order.len() && swapped_ok(i),
            })
            .collect()
    }

    /// The stage order in force, by id.
    fn order_strings(&self) -> Vec<String> {
        self.modules.order().iter().map(|s| s.to_string()).collect()
    }

    /// The facts the config schema is made from: the modules', the
    /// options of `output.mode` ([`Self::output_mode_options`]) and those of
    /// `effect.on_failure` (the image only with one configured).
    fn schema_facts(&self) -> RegistryFacts {
        let mut facts = registry_facts(&self.modules);
        facts.output_modes = self.output_mode_options();
        facts.on_failure_options =
            config_schema::on_failure_options(&self.config_set.config.effect);
        facts
    }

    /// The options of `output.mode`: what the camera the settings describe
    /// can feed (no camera, or one whose modes are not known: two common
    /// modes), twice its rates when a module provides frame generation,
    /// and the configured value ([`output_mode_choices`]).
    fn output_mode_options(&self) -> Vec<(String, String)> {
        let c = self.config_set.config.output.mode;
        let current = if c.fps_den == 1 {
            format!("{}x{}@{}", c.width, c.height, c.fps_num)
        } else {
            format!("{}x{}@{}/{}", c.width, c.height, c.fps_num, c.fps_den)
        };
        let caps = self
            .settings_camera()
            .filter(|c| !c.modes.is_empty())
            .map(|c| Capabilities {
                modes: c.modes.clone(),
                other_formats: Vec::new(),
            });
        let generated = self.modules.resolve(FRAME_GENERATION).provider.is_some();
        output_mode_choices(caps.as_ref(), &current, generated)
    }

    /// `GetConfigSchema`.
    fn config_schema(&self) -> obc_ipc::ConfigSchema {
        let facts = self.schema_facts();
        obc_ipc::ConfigSchema {
            serial: self.config_serial,
            groups: config_schema::config_schema(&self.config_set, &facts)
                .into_iter()
                .map(ipc_group)
                .collect(),
        }
    }

    /// `GetConfigFiles`: the files as they are now (a file added or broken
    /// since the last load shows, before a reload takes it).
    fn config_files(&self) -> obc_ipc::ConfigFiles {
        let (dir, files) = config_d::config_files(&self.paths.config);
        obc_ipc::ConfigFiles {
            serial: self.config_serial,
            dir: dir.display().to_string(),
            files: files
                .into_iter()
                .map(|f| obc_ipc::ConfigFileEntry {
                    path: f.path,
                    status: f.status.as_str().to_string(),
                    managed: f.managed,
                    reason: f.reason,
                })
                .collect(),
        }
    }

    /// `SetConfig`: checked, written to the managed file, applied.
    fn set_config(
        &mut self,
        key: &str,
        value: Option<obc_ipc::ConfigValue>,
        now: Instant,
    ) -> Result<(), IpcError> {
        let value = value.map(core_value).ok_or_else(|| {
            IpcError::InvalidArgument(format!(
                "{key}: the value is of a D-Bus type no configuration key takes"
            ))
        })?;
        self.settle_pending_move(now);
        let facts = self.schema_facts();
        let res = config_d::set_config(&self.paths.config, &facts, key, &value);
        // Enabling a module is the same as `SetModuleEnabled`: one the
        // crash ledger disabled is forgiven.
        let forgive = match (key.strip_prefix(config_schema::ENABLED_PREFIX), &value) {
            (Some(id), config_schema::ConfigValue::Bool(true)) => Some(id.to_string()),
            _ => None,
        };
        self.config_written(&format!("{key} set"), res, forgive.as_deref(), now)
    }

    /// `ResetConfig`: the key removed from the managed file, applied.
    fn reset_config(&mut self, key: &str, now: Instant) -> Result<(), IpcError> {
        self.settle_pending_move(now);
        let facts = self.schema_facts();
        let res = config_d::reset_config(&self.paths.config, &facts, key);
        self.config_written(&format!("{key} reset"), res, None, now)
    }

    /// Writes `edits` to the managed file for a module method (refused for
    /// a key a later file sets) and applies the result.
    fn write_choice(
        &mut self,
        what: &str,
        edits: &[ManagedEdit],
        forgive: Option<&str>,
        now: Instant,
    ) -> Result<(), IpcError> {
        self.settle_pending_move(now);
        let res = config_d::persist_managed(&self.paths.config, edits, OverridePolicy::Refuse);
        self.config_written(what, res, forgive, now)
    }

    /// The edit that clears `key` of the managed file: its removal, or
    /// (when an earlier file sets it, so that the removal would bring that
    /// value back) its empty value `empty`, which `still_set` tells.
    fn clear_edit(
        &self,
        key: &str,
        empty: toml::Value,
        still_set: impl Fn(&Config) -> bool,
    ) -> ManagedEdit {
        let removed = config_d::apply_edits(
            &self.config_set.managed,
            &[ManagedEdit::Remove(key.to_string())],
        );
        match self.config_set.with_managed(&removed) {
            Ok(c) if still_set(&c) => ManagedEdit::Set(key.to_string(), empty),
            _ => ManagedEdit::Remove(key.to_string()),
        }
    }

    /// A write of the managed file was made (or refused, `res`): the new set
    /// is applied. A change of the module choices alone (enabled modules,
    /// profile, pins, order) goes to the module registry, from the next
    /// session, as the module methods always did (a module enabled is
    /// opened now; `forgive` names one to re-enable even when the
    /// configuration already had it on, which forgives a crash); any other
    /// change is applied as a reload would, restarting only what it needs.
    fn config_written(
        &mut self,
        what: &str,
        res: Result<ConfigSet, WriteError>,
        forgive: Option<&str>,
        now: Instant,
    ) -> Result<(), IpcError> {
        let set = match res {
            Ok(set) => set,
            Err(e) => {
                let err = write_error(e);
                info!("configuration write refused: {}", err.message());
                return Err(err);
            }
        };
        info!("configuration: {what}");
        // An audio module enabled: the worker's crash ledger forgives it,
        // after the snapshot that enables it.
        let audio_reset = forgive.filter(|id| {
            mic::merge_modules(self.modules.info(), &self.mic_catalogue.modules)
                .iter()
                .any(|m| m.id == *id && m.kind == "audio-filter")
        });
        let config = in_force(&set.config, &self.runtime);
        // The registry's refusal for the module the request named (as
        // `SetModuleEnabled` answered before; a module that fails to open
        // is no refusal: its status says why).
        let mut refused = None;
        if choices_only(&self.config, &config) {
            let old = std::mem::replace(&mut self.config, config);
            self.adopt_set(set);
            let mut changed = old.modules != self.config.modules;
            // The modules forced first: what they lift decides what
            // enabling a module opens.
            if old.modules.forced != self.config.modules.forced {
                changed |= self.modules.apply_forced(&mut self.backend, &self.config);
            }
            for m in self.modules.info() {
                let on = self.config.modules.is_enabled(&m.id);
                let named = forgive == Some(m.id.as_str());
                if on != old.modules.is_enabled(&m.id) || named {
                    let res = self
                        .modules
                        .set_enabled(&mut self.backend, &self.config, &m.id, on);
                    match res {
                        Ok(c) => changed |= c,
                        Err(e) if named => refused = Some(e),
                        Err(e) => debug!("{e}"),
                    }
                }
            }
            // The pins and the profile decide which side of a module
            // conflict gives the cut-out.
            self.modules
                .apply_choices_settled(&mut self.backend, &self.config);
            if changed {
                self.follow_modules(true, now, true);
            }
        } else {
            self.apply_set(set, now);
            if let Some(id) = forgive {
                if let Err(e) = self
                    .modules
                    .set_enabled(&mut self.backend, &self.config, id, true)
                {
                    refused = Some(e);
                }
                self.follow_modules(true, now, true);
            }
        }
        self.reconcile(now);
        if let (Some(id), Some(link)) = (audio_reset, &self.mic) {
            let _ = link.send(MicRequest::ResetModule(id.to_string()));
        }
        match refused {
            Some(e) => Err(IpcError::Failed(e)),
            None => Ok(()),
        }
    }

    /// Moves the state file's module choices into the managed file when a
    /// start or reload could not (they are in force from the state until
    /// then), before a write of the managed file: the write then goes over
    /// them. Nothing when the set does not load or the move fails again
    /// (the write that follows meets the same cause and says it).
    fn settle_pending_move(&mut self, now: Instant) {
        if self.runtime.modules.is_empty() || self.state_damaged.is_some() {
            return;
        }
        let Ok(set) = ConfigSet::load(&self.paths.config) else {
            return;
        };
        let (set, _) = migrate_choices(&self.paths, set, &mut self.runtime);
        if self.runtime.modules.is_empty() {
            let _ =
                self.config_written("the state file's module choices moved", Ok(set), None, now);
        }
    }

    /// `SetModuleEnabled`: `modules.enabled.<id>` in the managed file. An
    /// enabled module is opened now (its settings appear); a running
    /// session keeps the modules it started with.
    fn set_module_enabled(&mut self, id: &str, on: bool, now: Instant) -> Result<(), IpcError> {
        if !self.modules.installed(id) {
            return Err(IpcError::InvalidArgument(format!(
                "No module {id} is installed"
            )));
        }
        let key = format!("{}{id}", config_schema::ENABLED_PREFIX);
        let what = format!("module {id} {}", if on { "enabled" } else { "disabled" });
        self.write_choice(
            &what,
            &[ManagedEdit::Set(key, toml::Value::Boolean(on))],
            on.then_some(id),
            now,
        )
    }

    /// `SetStageOrder`: checked against the stages the enabled modules
    /// offer (the reason when it is not valid), kept as `modules.order`;
    /// an empty list is the default order.
    fn set_stage_order(&mut self, order: Vec<String>, now: Instant) -> Result<(), IpcError> {
        const KEY: &str = "modules.order";
        let (edit, what) = if order.is_empty() {
            let edit = self.clear_edit(KEY, toml::Value::Array(Vec::new()), |c| {
                !c.modules.order.is_empty()
            });
            (edit, "stage order: the default".to_string())
        } else {
            self.modules
                .validate_order(&order)
                .map_err(IpcError::InvalidArgument)?;
            let what = format!("stage order: {}", order.join(", "));
            let value = toml::Value::Array(order.into_iter().map(toml::Value::String).collect());
            (ManagedEdit::Set(KEY.into(), value), what)
        };
        self.write_choice(&what, &[edit], None, now)
    }

    /// `SetBackendProfile`: `quality` or `battery`, kept as
    /// `modules.profile`.
    fn set_backend_profile(&mut self, profile: &str, now: Instant) -> Result<(), IpcError> {
        let p: BackendProfile = profile.parse().map_err(IpcError::InvalidArgument)?;
        self.write_choice(
            &format!("backend profile: {p}"),
            &[ManagedEdit::Set(
                "modules.profile".into(),
                toml::Value::String(p.as_str().into()),
            )],
            None,
            now,
        )
    }

    /// `SetStagePin`: module `module` provides `stage` while it can (`""`:
    /// no pin; the profile decides), kept as `modules.pin.<stage>`.
    fn set_stage_pin(&mut self, stage: &str, module: &str, now: Instant) -> Result<(), IpcError> {
        if !obc_core::modules::valid_stage_id(stage) {
            return Err(IpcError::InvalidArgument(
                obc_core::modules::not_a_stage_id(stage),
            ));
        }
        if !module.is_empty() && !self.modules.installed(module) {
            return Err(IpcError::InvalidArgument(format!(
                "No module {module} is installed"
            )));
        }
        let key = format!("{}{stage}", config_schema::PIN_PREFIX);
        let (edit, what) = if module.is_empty() {
            let edit = self.clear_edit(&key, toml::Value::String(String::new()), |c| {
                c.modules.pins().contains_key(stage)
            });
            (edit, format!("stage {stage}: not pinned"))
        } else {
            (
                ManagedEdit::Set(key, toml::Value::String(module.into())),
                format!("stage {stage}: pinned to {module}"),
            )
        };
        self.write_choice(&what, &[edit], None, now)
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
                model.check(id, value.as_ref()).map_err(refusal_error)?
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
        let res = match ConfigSet::load(&self.paths.config) {
            Ok(set) => {
                // Choices a start or an earlier reload could not move
                // move now.
                let set = if self.state_damaged.is_none() {
                    migrate_choices(&self.paths, set, &mut self.runtime).0
                } else {
                    set
                };
                self.apply_set(set, now);
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                warning!("configuration rejected, the previous one stays in force: {msg}");
                self.error = msg.clone();
                // What `GetConfigFiles` shows changed (the file in error).
                self.bump_config();
                Err(IpcError::InvalidConfig(msg))
            }
        };
        self.reconcile(now);
        res
    }

    /// Bumps `ConfigSerial` (announced with the next publish).
    fn bump_config(&mut self) {
        self.config_serial = self.config_serial.wrapping_add(1).max(1);
    }

    /// Takes `set` as the set in force, its configuration not applied yet:
    /// `ConfigSerial` moves when the configuration, where its keys come
    /// from or its files changed.
    fn adopt_set(&mut self, set: ConfigSet) {
        let changed = set.config != self.config_set.config
            || set.provenance != self.config_set.provenance
            || set.files != self.config_set.files;
        self.config_set = set;
        if changed {
            self.bump_config();
        }
    }

    /// Takes `set` as the set in force and applies it as a reload does.
    fn apply_set(&mut self, set: ConfigSet, now: Instant) {
        let config = in_force(&set.config, &self.runtime);
        self.adopt_set(set);
        self.apply_config(config, now);
    }

    fn apply_config(&mut self, new: Config, now: Instant) {
        let old = std::mem::replace(&mut self.config, new);
        // A contract that could not start is tried again once the
        // configuration asks for another one.
        if self
            .failed_contract
            .as_ref()
            .is_some_and(|(f, ..)| *f != self.config.output_contract())
        {
            self.failed_contract = None;
        }
        if let Some(note) = maxine_root_note(
            Some(old.effect.maxine_root.as_deref()),
            self.config.effect.maxine_root.as_deref(),
            maxine_root_env().as_deref(),
            None,
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
        for (_, t) in &mut self.stages {
            t.failure = None;
        }
        // A failed enhance stage is tried again (below), as a failed effect
        // is.
        let mut enhance_retry = self.enhance_failure.take().is_some();
        self.enhance_seen = None;
        // Only the errors whose cause is gone are cleared: a missing
        // writer or a damaged state file still stands.
        self.error = self.standing_error();
        // The modules (and their configuration, the SDK root among it) may
        // have changed, or a feature been installed: found and opened again.
        // A running session keeps the modules it started with, and follows
        // what they provide now ([`Self::follow_modules`]).
        self.modules
            .reload(&mut self.backend, &self.config, self.compute_capability);
        // Enhancement is followed below, once the cameras are read again.
        self.follow_modules(false, now, true);
        // The effect in force may have changed (or failed before): applied
        // again, which retries a failed effect. A fallback image is given
        // again even when nothing changed, and so is the placeholder's: the
        // file at its path may have been replaced, and is read again if so.
        let image = self.effect().on_failure == EffectOnFailure::Image
            && self.effect().fallback_image.is_some();
        if self.session.is_some() {
            let _ = self.apply_effect_with(image);
        }
        if let Some(p) = &self.placeholder_set {
            self.backend.set_placeholder(Some(p));
            self.restart_placeholder_wait();
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
                let _ = self.reopen_camera(&cam, now);
                // A new ingest: the stage already tried again.
                enhance_retry = false;
            }
        }
        // Enhancement requested otherwise (the setting's default, the
        // aspect, what is installed), or failed: the camera is reopened with
        // it or without it.
        let _ = self.follow_enhance(enhance_retry, now);
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

// ---- the microphone (crate::mic) ----

impl<B: MediaBackend> Controller<B> {
    /// The `mic.*` groups of the tree now.
    fn mic_groups(&self) -> Vec<obc_ipc::SettingsGroup> {
        mic::groups(&mic::Inputs {
            config: &self.config,
            config_set: &self.config_set,
            runtime: &self.runtime,
            catalogue: &self.mic_catalogue,
            host_on: self.mic_status.state != obc_ipc::MIC_OFF,
        })
    }

    /// The microphone group a `group:` reset scope names: `mic.<id>`
    /// always names the microphone's group `<id>`; a bare `<id>` names it
    /// only when no camera or global group (the video effects, which
    /// modules extend) has that id, so that a camera's or video module's
    /// group never resets the microphone's by mistake.
    fn mic_group_of(&self, scope: &str) -> Option<String> {
        let groups = self.mic_groups();
        let is_mic = |id: &str| groups.iter().any(|g| g.id == id);
        if let Some(id) = scope.strip_prefix(obc_ipc::settings::MIC_ID_PREFIX) {
            return is_mic(id).then(|| id.to_string());
        }
        let taken = catalogue::GROUPS.iter().any(|g| g.id == scope)
            || self.global_settings().iter().any(|e| e.group == scope);
        (!taken && is_mic(scope)).then(|| scope.to_string())
    }

    /// The camera session runs (for `gpu_policy = "with-camera"`).
    fn camera_active(&self) -> bool {
        matches!(self.state(), State::Starting | State::Active)
    }

    /// Sends the audio settings snapshot to the agent when it changed.
    fn push_mic_snapshot(&mut self) {
        let Some(link) = &self.mic else {
            return;
        };
        let text = mic::snapshot(&self.config, &self.runtime, self.camera_active());
        if self.mic_sent.as_deref() != Some(text.as_str()) {
            if link.send(MicRequest::Snapshot(text.clone())).is_err() {
                debug!("the microphone's agent is gone");
            }
            self.mic_sent = Some(text);
        }
        // The device chosen changed while ours is the default: the capture
        // side follows at once (the record of the previous default stays).
        let device = self.config.mic.device.clone();
        let changed = self
            .mic_device_sent
            .as_deref()
            .is_some_and(|sent| sent != device);
        if changed && self.runtime.mic_previous_default.is_some() {
            let target = match device.as_str() {
                obc_core::config::MIC_DEVICE_DEFAULT => String::new(),
                d => d.to_string(),
            };
            if link.send(MicRequest::Repin { target }).is_err() {
                debug!("the microphone's agent is gone");
            }
        }
        self.mic_device_sent = Some(device);
    }

    /// `SetSetting` of a `mic.*` id: checked against the tree as it is
    /// now; a config key written as `SetConfig` writes it, a module's value
    /// kept in the state file (and the snapshot sent), an action handed to
    /// the agent, which answers.
    fn set_mic_setting(
        &mut self,
        id: &str,
        value: Option<&SettingValue>,
        reply: Reply,
        now: Instant,
    ) {
        let groups = self.mic_groups();
        let write = match mic::check_input(
            &groups,
            &self.mic_catalogue.devices,
            &self.config.mic.device,
            id,
            value,
        ) {
            Ok(w) => w,
            Err(r) => {
                let _ = reply.send(Err(refusal_error(r)));
                return;
            }
        };
        let res = match write {
            mic::Write::Config(key, value) => {
                self.settle_pending_move(now);
                let facts = self.schema_facts();
                let res = config_d::set_config(&self.paths.config, &facts, key, &value);
                self.config_written(&format!("{key} set"), res, None, now)
            }
            mic::Write::Value(id, v) => {
                info!("{id} set");
                self.runtime.settings.insert(id, v);
                let res = self.settings_saved(now);
                self.publish();
                res
            }
            mic::Write::MakeDefault => {
                let target = match self.config.mic.device.as_str() {
                    obc_core::config::MIC_DEVICE_DEFAULT => String::new(),
                    d => d.to_string(),
                };
                return self.ask_mic(reply, |reply| MicRequest::MakeDefault { target, reply });
            }
            mic::Write::RestoreDefault => {
                let previous = self
                    .runtime
                    .mic_previous_default
                    .clone()
                    .unwrap_or_default();
                return self.ask_mic(reply, |reply| MicRequest::RestoreDefault {
                    previous,
                    reply,
                });
            }
        };
        let _ = reply.send(res);
    }

    /// Hands a request with its reply to the agent; without one, the reply
    /// is `Unavailable`.
    fn ask_mic(&self, reply: Reply, make: impl FnOnce(Reply) -> MicRequest) {
        match &self.mic {
            Some(link) => {
                if let Err(e) = link.send(make(reply)) {
                    let reply = match e.0 {
                        MicRequest::Host { reply, .. }
                        | MicRequest::MakeDefault { reply, .. }
                        | MicRequest::RestoreDefault { reply, .. } => reply,
                        _ => return,
                    };
                    let _ = reply.send(Err(IpcError::Unavailable(
                        "the microphone cannot be controlled: the daemon is stopping".into(),
                    )));
                }
            }
            None => {
                let _ = reply.send(Err(IpcError::Unavailable(
                    "this daemon does not control the microphone".into(),
                )));
            }
        }
    }

    /// `SetMicHost(on)`: handed to the agent, with the default microphone
    /// recorded now to restore on the way off (the agent restores in any
    /// case, with the one it knows when this is empty: a "Use as default
    /// microphone" queued before may complete first).
    fn set_mic_host(&mut self, on: bool, reply: Reply) {
        let previous = (!on)
            .then(|| self.runtime.mic_previous_default.clone())
            .flatten();
        self.ask_mic(reply, |reply| MicRequest::Host {
            on,
            previous,
            reply,
        });
    }

    /// Records the default microphone made by the agent (`None`: the
    /// Omarchy Broadcast Mic is no longer the default) and saves it; a
    /// save that fails is logged and answered.
    fn remember_default(&mut self, previous: Option<String>) -> Result<(), IpcError> {
        if self.runtime.mic_previous_default == previous {
            return Ok(());
        }
        self.runtime.mic_previous_default = previous;
        self.save_state()
    }

    /// What the agent found or did.
    fn mic_event(&mut self, event: MicEvent) {
        match event {
            MicEvent::Status(status) => {
                self.mic_status = status;
                self.publish();
            }
            MicEvent::Catalogue(catalogue) => {
                if catalogue.modules != self.mic_catalogue.modules {
                    self.mic_serial = self.mic_serial.wrapping_add(1);
                }
                self.mic_catalogue = catalogue;
                self.publish();
            }
            MicEvent::DefaultMade { previous, reply } => {
                let res = self.remember_default(Some(previous));
                self.publish();
                let _ = reply.send(res);
            }
            MicEvent::DefaultRestored { result, reply } => {
                let res = result.and_then(|()| self.remember_default(None));
                self.publish();
                let _ = reply.send(res);
            }
            MicEvent::HostDone {
                restored,
                result,
                reply,
            } => {
                let saved = if restored {
                    self.remember_default(None)
                } else {
                    Ok(())
                };
                self.publish();
                let _ = reply.send(result.and(saved));
            }
        }
    }

    /// `ResetSettings` of a microphone group or setting: a module's values
    /// forgotten (their defaults apply), the daemon's own settings removed
    /// from the managed file (an earlier file's value, or the default,
    /// applies).
    fn reset_mic(&mut self, what: &ResetWhat, now: Instant) -> Result<(), IpcError> {
        let groups = self.mic_groups();
        let (keys, ids): (Vec<&'static str>, Vec<String>) = match what {
            ResetWhat::Group(g) if g == mic::INPUT => (mic::INPUT_KEYS.to_vec(), Vec::new()),
            ResetWhat::Group(g) => match mic::module_group_ids(&groups, g) {
                Some(ids) => (Vec::new(), ids),
                None => {
                    return Err(IpcError::UnknownSetting(format!("there is no group {g:?}")));
                }
            },
            ResetWhat::Setting(id) => match mic::config_key_of(id) {
                Some(key) => (vec![key], Vec::new()),
                None if mic::find(&groups, id).is_some()
                    || self.runtime.settings.contains_key(id) =>
                {
                    (Vec::new(), vec![id.clone()])
                }
                None => {
                    return Err(IpcError::UnknownSetting(format!(
                        "there is no setting {id:?}"
                    )));
                }
            },
            _ => return Err(IpcError::UnknownSetting("not a microphone setting".into())),
        };
        let mut res = Ok(());
        if !ids.is_empty() {
            let mut changed = false;
            for id in &ids {
                changed |= self.runtime.settings.remove(id).is_some();
            }
            info!("{} reset", ids.join(", "));
            if changed {
                res = self.settings_saved(now);
            }
            self.publish();
        }
        for key in keys {
            self.settle_pending_move(now);
            let facts = self.schema_facts();
            let r = config_d::reset_config(&self.paths.config, &facts, key);
            res = res.and(self.config_written(&format!("{key} reset"), r, None, now));
        }
        res
    }
}

/// The configuration in force for the loaded `config`: the state file's
/// module choices laid over it while they wait to move into the managed
/// file ([`migrate_choices`] could not write it, or the set did not load),
/// as they were in force before the move; `config` itself otherwise.
fn in_force(config: &Config, runtime: &RuntimeState) -> Config {
    let mut c = config.clone();
    if !runtime.modules.is_empty() {
        c.modules = runtime.modules.over(&config.modules);
    }
    c
}

/// Moves the module choices of `runtime` (the state file's enabled
/// modules, stage order, profile and pins) into the managed file of `set`
/// ([`config_d::migrate_modules_state`]): the managed file is written first
/// (for the keys it does not set already), then, only once that worked,
/// the state file is saved without them. A crash between the two loses
/// nothing (the configuration holds them; the state's copy is dropped at
/// the next load). A write that fails leaves the state as it is, to be
/// tried at the next load. Returns the set in force afterwards and whether
/// the managed file was written.
fn migrate_choices(paths: &Paths, set: ConfigSet, runtime: &mut RuntimeState) -> (ConfigSet, bool) {
    let m = config_d::migrate_modules_state(runtime, &set.managed);
    if !m.is_needed() {
        return (set, false);
    }
    for note in &m.notes {
        warning!("the state file's module choices: {note}");
    }
    let written = if m.edits.is_empty() {
        Ok(None)
    } else {
        config_d::persist_managed(&paths.config, &m.edits, OverridePolicy::Ignore).map(Some)
    };
    let managed = config_d::managed_path(&paths.config);
    match written {
        Ok(new) => {
            runtime.modules = m.state.modules;
            match runtime.save(&paths.state) {
                Ok(()) => info!(
                    "the module choices of {} moved to {}",
                    paths.state.display(),
                    managed.display()
                ),
                Err(e) => warning!(
                    "the module choices are in {} now, but {} cannot be saved without them \
                     ({e}): its copy is dropped at its next save",
                    managed.display(),
                    paths.state.display()
                ),
            }
            match new {
                Some(new) => (new, true),
                None => (set, false),
            }
        }
        Err(e) => {
            error!(
                "the module choices of {} cannot be moved to {}: {e}; they stay in the state \
                 file, in force from it, and are moved at the next start, reload or write",
                paths.state.display(),
                managed.display()
            );
            (set, false)
        }
    }
}

/// What the module registry found, for the module rows of the config
/// schema: the installed modules (by name) and, for each stage they offer,
/// the modules offering it.
fn registry_facts(modules: &Modules) -> RegistryFacts {
    let info = modules.info();
    let mut stages: Vec<StageFact> = Vec::new();
    for m in &info {
        for st in &m.stages {
            match stages.iter_mut().find(|f| f.id == st.id) {
                Some(f) if !f.modules.contains(&m.id) => f.modules.push(m.id.clone()),
                Some(_) => {}
                None => stages.push(StageFact {
                    id: st.id.clone(),
                    modules: vec![m.id.clone()],
                }),
            }
        }
    }
    RegistryFacts {
        modules: info
            .iter()
            .map(|m| ModuleFact {
                id: m.id.clone(),
                name: if m.name.is_empty() {
                    m.id.clone()
                } else {
                    m.name.clone()
                },
                // `[mic] force_conflicts` lifts an audio module's.
                forceable: m.kind == obc_module_host::ModuleKind::VideoStage.as_str(),
            })
            .collect(),
        stages,
        output_modes: Vec::new(),
        on_failure_options: Vec::new(),
    }
}

/// Whether `new` differs from `old` in the module choices alone (enabled,
/// disabled and forced modules, profile, pins, order), or not at all.
fn choices_only(old: &Config, new: &Config) -> bool {
    let mut n = new.clone();
    n.modules.enabled = old.modules.enabled.clone();
    n.modules.forced = old.modules.forced.clone();
    n.modules.disabled = old.modules.disabled.clone();
    n.modules.profile = old.modules.profile;
    n.modules.pin = old.modules.pin.clone();
    n.modules.order = old.modules.order.clone();
    // The microphone's keys go to the audio worker, not to the camera.
    n.mic = old.mic.clone();
    n == *old
}

/// A value of the bus as the config schema's.
fn core_value(v: obc_ipc::ConfigValue) -> config_schema::ConfigValue {
    use config_schema::ConfigValue as C;
    match v {
        obc_ipc::ConfigValue::Bool(b) => C::Bool(b),
        obc_ipc::ConfigValue::Int(i) => C::Int(i),
        obc_ipc::ConfigValue::Float(f) => C::Float(f),
        obc_ipc::ConfigValue::Str(s) => C::Str(s),
        obc_ipc::ConfigValue::List(l) => C::List(l),
    }
}

/// A value of the config schema as the bus's.
fn ipc_value(v: config_schema::ConfigValue) -> obc_ipc::ConfigValue {
    use config_schema::ConfigValue as C;
    match v {
        C::Bool(b) => obc_ipc::ConfigValue::Bool(b),
        C::Int(i) => obc_ipc::ConfigValue::Int(i),
        C::Float(f) => obc_ipc::ConfigValue::Float(f),
        C::Str(s) => obc_ipc::ConfigValue::Str(s),
        C::List(l) => obc_ipc::ConfigValue::List(l),
    }
}

/// A group of the config schema as `GetConfigSchema` sends it.
fn ipc_group(g: config_schema::ConfigGroup) -> obc_ipc::ConfigGroup {
    use config_schema::RowKind as K;
    obc_ipc::ConfigGroup {
        id: g.id,
        label: g.label,
        rows: g
            .rows
            .into_iter()
            .map(|r| obc_ipc::ConfigRow {
                id: r.id,
                label: r.label,
                description: r.description,
                kind: match r.kind {
                    K::Bool => obc_ipc::ConfigKind::Bool,
                    K::Int {
                        min,
                        max,
                        step,
                        unit,
                    } => obc_ipc::ConfigKind::Int {
                        min,
                        max,
                        step,
                        unit,
                    },
                    K::Float {
                        min,
                        max,
                        step,
                        unit,
                    } => obc_ipc::ConfigKind::Float {
                        min,
                        max,
                        step,
                        unit,
                    },
                    K::Enum { options } => obc_ipc::ConfigKind::Enum { options },
                    K::String => obc_ipc::ConfigKind::String,
                    K::Path => obc_ipc::ConfigKind::Path,
                    K::List => obc_ipc::ConfigKind::List,
                },
                value: ipc_value(r.value),
                default: r.default.map(ipc_value),
                editable: r.editable,
                source: r.source,
                overridden_by: r.overridden_by,
                apply: r.apply.as_str().to_string(),
                enabled: r.enabled,
                disabled_reason: r.disabled_reason,
            })
            .collect(),
    }
}

/// The error a refused write of the managed file answers.
fn write_error(e: WriteError) -> IpcError {
    let msg = e.to_string();
    match e {
        WriteError::UnknownKey(_) => IpcError::UnknownSetting(msg),
        WriteError::NotEditable(_) | WriteError::Overridden { .. } => IpcError::Disabled(msg),
        WriteError::Invalid(_) => IpcError::InvalidArgument(msg),
        WriteError::Config(_) => IpcError::InvalidConfig(msg),
        WriteError::ManagedUnreadable { .. } | WriteError::Io { .. } => IpcError::Failed(msg),
    }
}

/// The error a refused `SetSetting` answers.
fn refusal_error(r: Refusal) -> IpcError {
    match r {
        Refusal::Unknown(m) => IpcError::UnknownSetting(m),
        Refusal::Disabled(m) => IpcError::Disabled(m),
        Refusal::Invalid(m) => IpcError::InvalidArgument(m),
    }
}

/// `text` made one line safe to show ([`settings::sanitise`]) and at most
/// [`WARNING_MAX`] bytes: a longer one is cut on a character boundary and
/// ends with an ellipsis, counted in the bound.
fn cap_warning(text: &str) -> String {
    let mut s = settings::sanitise(text, usize::MAX);
    if s.len() <= WARNING_MAX {
        return s;
    }
    const ELLIPSIS: &str = "\u{2026}";
    let mut end = WARNING_MAX - ELLIPSIS.len();
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    let trimmed = s.trim_end().len();
    s.truncate(trimmed);
    s.push_str(ELLIPSIS);
    s
}

/// Whether `out` fps is twice `camera` fps (within 0.2%).
fn doubles(out: f64, camera: f64) -> bool {
    let twice = 2.0 * camera;
    twice > 0.0 && ((out - twice) / twice).abs() <= 0.002
}

/// The holders of a deferral warning: their names made safe to show and
/// without duplicates, at most [`HOLDERS_NAMED`] then "and N more";
/// [`HOLDERS_UNKNOWN`] when none could be named.
fn holders_text(names: &[String]) -> String {
    let mut unique: Vec<String> = Vec::new();
    for n in names {
        let n = settings::sanitise(n, HOLDER_NAME_MAX);
        if !n.is_empty() && !unique.contains(&n) {
            unique.push(n);
        }
    }
    match unique.len() {
        0 => HOLDERS_UNKNOWN.to_string(),
        1 => unique.remove(0),
        n if n <= HOLDERS_NAMED => {
            let last = unique.pop().unwrap_or_default();
            format!("{} and {last}", unique.join(", "))
        }
        n => format!(
            "{} and {} more",
            unique[..HOLDERS_NAMED].join(", "),
            n - HOLDERS_NAMED
        ),
    }
}

/// The names of the controls `scan` could not read.
fn unreadable_names(scan: &ControlScan) -> impl Iterator<Item = String> + '_ {
    scan.controls
        .iter()
        .filter(|c| matches!(c.value, CtrlValue::Unreadable { .. }))
        .map(|c| c.name.clone())
}

/// `MAXINE_VFX_ROOT`, when set and non-empty: it takes precedence over
/// `[effect] maxine_root` (the Maxine module reads it).
fn maxine_root_env() -> Option<PathBuf> {
    std::env::var_os("MAXINE_VFX_ROOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
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
