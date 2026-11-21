//! D-Bus interface `br.com.dias.OmarchyBroadcast1` (zbus), shared by the daemon
//! and its clients (the reference for
//! client authors is `docs/dbus-api.md`).
//!
//! This crate holds the names, the value types carried on the bus, the error
//! type the daemon replies with and the client proxy ([`client`]). The
//! server-side `#[interface]` implementation belongs to the daemon.
//!
//! # Reading state without starting the daemon
//!
//! When the daemon's D-Bus activation file is installed, any message
//! addressed to [`BUS_NAME`] without the `NO_AUTO_START` flag starts the
//! daemon if it is not running. The generated proxy sends such messages for
//! reads too: a proxy that caches properties (the default of
//! [`OmarchyBroadcastProxy::new`] and of [`OmarchyBroadcastProxy::builder`]) fills
//! its cache with a `GetAll` without the flag, and the property getters
//! (`state()`, `cameras()`, ...) fall back to a `Get` without it.
//!
//! A client that only looks (a status bar, a `status` command) reads with
//! [`Snapshot::fetch_no_autostart`] instead: one `GetAll` carrying
//! `NO_AUTO_START`, which yields `None` when the daemon is not running. It
//! follows changes with `PropertiesChanged`, applied with
//! [`Snapshot::apply_property`], and the daemon's presence with
//! `org.freedesktop.DBus.NameOwnerChanged` or [`daemon_is_running`]. Signal
//! subscriptions never start the daemon. Method calls through the proxy
//! (build it with `cache_properties(CacheProperties::No)` so building it
//! sends nothing) may start it, which is what a control action wants.
//!
//! zbus sets no time limit on a method call by default: a daemon that holds
//! its name but does not answer keeps the call pending. Bound reads with
//! `zbus::connection::Builder::method_timeout` or with the runtime's own
//! timeout.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use zbus::as_variant::deserialize_for_property;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::{Connection, DBusError, OwnedVariant, Type, Variant};

pub mod client;
pub mod config;
pub mod health;
pub mod media;
pub mod modules;
pub mod settings;

pub use client::OmarchyBroadcastProxy;
pub use config::{
    CONFIG_REVISION, ConfigFileEntry, ConfigFiles, ConfigGroup, ConfigKind, ConfigRow,
    ConfigSchema, ConfigValue,
};
pub use health::{
    CAMERA_HEALTH_PERIOD_MS, CAMERA_HEALTH_REVISION, CameraHealth, DELIVERS_SHORT_RATIO,
    NOT_MEASURED, Recommended, SHOW_MODE_DESK_VIEW, SHOW_MODE_NORMAL, SHUTTER_CLOSED, SHUTTER_OPEN,
    Suggestion,
};
pub use media::{MEDIA_DIRS_REVISION, MEDIA_REVISION, MediaDirs, MediaEntry, MediaReply};
pub use modules::{FORCED_REVISION, MODULES_REVISION, ModuleInfo, ModuleList, StageInfo};
pub use settings::{
    Condition, ConditionOp, IntDisplay, ResetScope, Setting, SettingChange, SettingKind,
    SettingValue, SettingsGroup, SettingsTree, valid_setting_id,
};

/// Well-known name of the daemon on the session bus.
pub const BUS_NAME: &str = "br.com.dias.OmarchyBroadcast";
/// Path of the one object the daemon exports.
pub const OBJECT_PATH: &str = "/br/com/dias/OmarchyBroadcast";
/// Interface name. Breaking changes add `br.com.dias.OmarchyBroadcast2`.
pub const INTERFACE: &str = "br.com.dias.OmarchyBroadcast1";
/// Revision of [`INTERFACE`] this crate describes, exposed by the daemon as
/// the `InterfaceRevision` property. Raised for every backwards-compatible
/// addition (a new property, method or signal). Revision 2 added the
/// effect controls: `BlurStrength`, `BackgroundImage`, `SetEffect`,
/// `SetBlurStrength`, `SetBackgroundImage` and the `InvalidArgument` error.
/// Revision 3 added the optional PipeWire output: `LoopbackConsumers`,
/// `PipeWireConsumers` and `PipeWireOutput`, with `Consumers` the total of
/// both kinds of consumer. Revision 4 added video noise removal: `Denoise`
/// and `SetDenoise`. Revision 5 added the live preview: `AcquirePreview`,
/// `ReleasePreview`, the `PreviewFrame` signal and `PreviewConsumers`, with
/// `Consumers` counting the preview leases too. Revision 6 added the camera
/// image controls: `CameraControls`, `GetCameraControls`,
/// `SetCameraControl` and `ResetCameraControls`. Revision 7 added the
/// camera settings channel ([`settings`]): `SettingsRevision`,
/// `SettingsSchemaSerial`, `GetSettings`, `SetSetting`, `ResetSettings`,
/// the `SettingsChanged` signal and the `UnknownSetting`, `Disabled` and
/// `RateLimited` errors. Revision 8 added global settings to the tree
/// (scope [`settings::SCOPE_GLOBAL`]): the video effects as
/// `effect.background.mode`, `effect.background.blur_strength`,
/// `effect.video.denoise` and `effect.video.enhance`, after the camera's
/// groups and also with no camera; the members are unchanged. Revision 9
/// added the modules ([`modules`]): `GetModules`, `SetModuleEnabled`,
/// `GetStageOrder`, `SetStageOrder`, `SetBackendProfile`, `SetStagePin`,
/// the `ModulesSerial` and `BackendProfile` properties; the global settings
/// are those of the modules installed (none without one). Revision 10
/// added the configuration ([`config`]): `GetConfigSchema`, `SetConfig`,
/// `ResetConfig`, `GetConfigFiles`, the `ConfigChanged` signal and the
/// `ConfigSerial` property; the daemon reads `config.d/` after
/// `config.toml` and writes `config.d/50-panel.toml`. Revision 11 added
/// the `Warning` property: a selected stage that no module provides is
/// dropped, the feed keeps streaming without it, and `Warning` says so;
/// an effect a module provides that cannot load or run no longer stops
/// the feed (with privacy on its frames are black, with it off the
/// camera image shows) and `Warning` says why. Neither is
/// `Error` and the `error` state any more. Revision 12 added the capture
/// and output modes, with no new member: the camera setting
/// `camera.capture.mode` (group `capture`, first: `auto` or a `WxH@FPS`
/// the camera offers, kept per camera), the config row `output.mode` made
/// an editable `enum` whose options the daemon computes (the selected
/// camera's 16:9 modes, and twice a rate when a module provides frame
/// generation), and `Warning` clauses for a capture mode not offered, an
/// output change waiting for the applications that hold the virtual
/// camera, an output contract that could not start and an output rate
/// that frame generation cannot make; `Warning` is at most 512 bytes.
/// Revision 13 added the microphone ([`MicStatus`]): the `MicState`,
/// `MicConsumers`, `MicProvider`, `MicLatencyMs` and `MicError`
/// properties, the `MicStats` signal and `SetMicHost`; the `mic.*`
/// settings (scope `global`, their groups with `surface` `mic`: the
/// daemon's `mic.input` and the audio modules' groups), the `[mic]` config
/// rows, and the audio modules in `GetModules`. Revision 14 enforces the
/// module conflicts and requirements manifests declare, with no new
/// member: a module's `forced` key in `GetModules`
/// ([`modules::FORCED_REVISION`]), the `modules.forced.<id>` config rows,
/// and the `Warning` clauses of a stage turned off by a conflict
/// (`<stage> unavailable: Cannot run together with <stage>`) and of a
/// conflict a forced module lets through (`forced: <a> runs together with
/// <b> despite a declared conflict (modules.forced)`), and of the module a
/// running session keeps for its background effect although it is
/// refused since (`<name> unavailable from the next start: <reason>`).
/// Revision 15 added the media gallery ([`media`]): `GetMedia`,
/// `ImportMedia`, `RemoveMedia` and the `MediaSerial` property.
/// Revision 16 added the `StageValues` property ([`StageValue`]): the named
/// numeric values the running stages report (auto-frame's zoom).
/// Revision 17 added the away image, with no new member
/// ([`PLACEHOLDER_REVISION`]). Revision 18 added video backgrounds, with no
/// new member ([`VIDEO_BACKGROUND_REVISION`]). Revision 19 dropped the
/// playback copies for the CPU and gave the command that converts a video
/// instead ([`CPU_CONVERT_REVISION`]). Revision 20 added the
/// `CameraHealth` property ([`CameraHealth`]: the rate the active camera
/// really delivers, its damaged frames, warnings about its USB placement
/// and damaged frames, effect suggestions and the recommended settings of
/// its model) and `ApplyRecommendedSettings`; the rate in the label of the
/// capture mode in use reads `<delivered> of <nominal> fps` once the camera
/// is measured below [`DELIVERS_SHORT_RATIO`] of its rate
/// ([`CAMERA_HEALTH_REVISION`]). Revision 21 made privacy an option,
/// off by default, with no new member ([`PRIVACY_REVISION`]). Revision 22
/// added the battery saver, with no new member
/// ([`BATTERY_SAVER_REVISION`]). Revision 23 added the battery saver's
/// `auto` mode, now the default, with no new member
/// ([`BATTERY_SAVER_AUTO_REVISION`]). Revision 24 added the MX Brio's
/// extension-unit settings, with no new member
/// ([`CAMERA_XU_SETTINGS_REVISION`]). Revision 25 added the microphone's
/// hardware capture gain, with no new member
/// ([`MIC_HARDWARE_GAIN_REVISION`]). Revision 26 added the optional
/// `description` of a setting and the camera status keys of `CameraHealth`,
/// with no new member ([`CAMERA_STATUS_REVISION`]). Revision 27 added the
/// optional `display_offset` and `display_scale` of an int setting and
/// removed the MX Brio's RightSight settings (`camera.focus.rightsight*`),
/// with no new member ([`DISPLAY_OFFSET_REVISION`]). Revision 28 added the
/// `disabled_kind` of a setting and published the default microphone as
/// the switch `mic.input.use_as_default`, with no new member
/// ([`DISABLED_KIND_REVISION`]).
pub const INTERFACE_REVISION: u32 = 28;
/// The first [`INTERFACE_REVISION`] with the `disabled_kind` (`s`) of a
/// setting dictionary and of a `SettingsChanged` entry
/// ([`settings::disabled`]: why a setting is not enabled, as a token a
/// client lays it out by; `""` while it is enabled), and with the default
/// microphone published as the `bool` switch `mic.input.use_as_default` in
/// place of the actions `mic.input.make_default` and
/// `mic.input.restore_default`, which `SetSetting` still takes. No new
/// member: a client that does not know the key ignores it, and shows a
/// setting that is not enabled greyed in place.
pub const DISABLED_KIND_REVISION: u32 = 28;
/// The first [`INTERFACE_REVISION`] with the optional `display_offset` and
/// `display_scale` (both `x`) of an `int` setting
/// ([`settings::IntDisplay`]), and no new member: a client shows the value
/// as `(value - display_offset) * display_scale` with an explicit sign,
/// then the unit; `min`, `max`, `step`, `default`, `value` and what
/// `SetSetting` takes stay raw. An absent `display_scale` is 1. A key a
/// client does not know is ignored, so an older client shows the raw
/// value.
pub const DISPLAY_OFFSET_REVISION: u32 = 27;
/// The first [`INTERFACE_REVISION`] with the optional `description` of a
/// setting (a sentence for people, a `description` key of the setting
/// dictionary, absent when there is none) and the camera status keys of
/// [`CameraHealth`], and no new member. A key a client does not know is
/// ignored, so an older client shows neither.
pub const CAMERA_STATUS_REVISION: u32 = 26;
/// The first [`INTERFACE_REVISION`] with the microphone's hardware capture
/// gain, and no new member: the config row `mic.hardware_gain_db` (dB,
/// optional) and the mic settings tree's `mic.input.hardware_gain_db` (an
/// `int` in whole dB, unit `dB`; its `min`, `max` and `step` are the
/// device's range as the audio worker reports it, and it is offered only
/// while the captured microphone is a USB device with one capture volume
/// control). The gain is applied only while the row is set, and re-applied
/// whenever the device appears; unset, the device's gain is left as it is.
/// It is the same ALSA control PipeWire shows as the physical source's
/// volume, so setting it pins that volume.
pub const MIC_HARDWARE_GAIN_REVISION: u32 = 25;
/// The first [`INTERFACE_REVISION`] with the MX Brio's extension-unit
/// settings, and no new member: on a Logitech MX Brio (USB 046d:0944) the
/// camera settings tree may carry `camera.focus.show_mode` (a `bool`),
/// `camera.exposure.exposure_compensation` (an `int`, relative: its
/// default is no compensation), `camera.image.vibrance` (an `int`),
/// `camera.white_balance.temperature_compensation` (an `int`, an offset of
/// the automatic white balance; its default is no offset). The ranges of
/// the ints are published in the setting's `min` and `max`, not fixed
/// here; from [`DISPLAY_OFFSET_REVISION`] the two compensations publish a
/// display offset (`display_offset`, and `display_scale` 40 with unit `K`
/// for the temperature compensation). Revisions 24 to 26 also carried
/// `camera.focus.rightsight` (a `bool`), `camera.focus.rightsight_speed`
/// and `camera.focus.rightsight_distance` (enums); revision 27 removed
/// them. Each is offered only when the camera agrees with
/// its row, and greyed with the reason `The camera reports a value this
/// version does not know` while it reports a value outside them.
pub const CAMERA_XU_SETTINGS_REVISION: u32 = 24;
/// The first [`INTERFACE_REVISION`] where `video.battery_saver` accepts
/// `auto`, and no new member: the saver is active on battery or while
/// power-profiles-daemon's active profile is `power-saver` (as `on_battery`
/// when that service is not running). `auto` is the default from this
/// revision on; before, `on_battery` was.
pub const BATTERY_SAVER_AUTO_REVISION: u32 = 23;
/// The first [`INTERFACE_REVISION`] with the battery saver, and no new
/// member: the config row `video.battery_saver` (`off`, `on_battery`,
/// `power_profile` or `always`: when `video.memory = "auto"` runs the next
/// camera session in system memory although CUDA is usable; not enabled
/// under `cuda` or `system`), and the `notice` key of every config row
/// ([`ConfigRow::notice`]; `""` when none), which `video.battery_saver`
/// sets to `Applies when the camera restarts` while the saver decides
/// otherwise than when the running session started, in a way that changes
/// its memory.
pub const BATTERY_SAVER_REVISION: u32 = 22;
/// The first [`INTERFACE_REVISION`] with privacy as an option, and no new
/// member: the config rows `effect.privacy` (a `bool`, off by default:
/// the camera shows as it is when a requested effect cannot run) and
/// `effect.privacy_mode` (`black`, `away` or `freeze`: what shows instead
/// while privacy is on; not enabled while it is off, with a reason that
/// says to turn it on) replace the row `effect.on_failure`. A `SetConfig` of
/// `effect.on_failure` (`black`, `image`, `none`) is still accepted and
/// written as the two keys; a `ResetConfig` of it resets both. The
/// `Warning` clause of the away image privacy cannot show reads
/// `effect.privacy_mode is "away" but effect.fallback_image is not set:
/// showing black`, and privacy off is named `effect.privacy is off`.
pub const PRIVACY_REVISION: u32 = 21;
/// The first [`INTERFACE_REVISION`] with the `StageValues` property.
pub const STAGE_VALUES_REVISION: u32 = 16;
/// The first [`INTERFACE_REVISION`] with the away image, and no new member:
/// the config row `output.placeholder` (`black` or `image`: what the feed
/// shows while there is no camera picture), `effect.fallback_image` made
/// an editable `path` row ("Away image"), and the `Warning` clauses of the
/// away image (`output.placeholder is "image" but effect.fallback_image is
/// not set: showing black`, `Away image <path> could not be read (<why>):
/// showing black`).
pub const PLACEHOLDER_REVISION: u32 = 17;
/// The first [`INTERFACE_REVISION`] with video backgrounds, and no new
/// member: `SetBackgroundImage` (and `[effect] background_image`) accepts
/// a video file (MP4, M4V, MOV, MKV or WebM holding H.264, HEVC, VP9 or
/// AV1, at most 512 MiB), played in a loop behind the person; `ImportMedia`
/// of a background accepts such a video (stored as it is) and an animated
/// GIF, WebP or PNG (stored as a VP9 WebM, with ffmpeg); `GetMedia` lists
/// videos with `type` `video`, their `width`, `height` and `duration_ms`,
/// and their `poster` (a JPEG in the daemon's cache; empty until it is
/// made, `MediaSerial` then moves). `Warning` gains the clauses of a video
/// shown as a still (`<module> shows the video background as a still`,
/// `The video background is shown as a still: <why>`), of a video being
/// prepared for playback on the CPU (`The video background is being
/// prepared for playback on the CPU; a still is shown until it is ready`,
/// or `…; the background is black until it is ready` with no still) and of
/// a video that cannot play (`Video background <path> could not be played
/// (<why>)`). Revision 19 replaced the clauses of the preparation
/// ([`CPU_CONVERT_REVISION`]).
pub const VIDEO_BACKGROUND_REVISION: u32 = 18;
/// The first [`INTERFACE_REVISION`] where the daemon no longer makes
/// playback copies for the CPU, and gave the command that converts a
/// video instead: in system memory a video that is not CPU-ready (VP9
/// profile 0 in WebM or Matroska, at most 1920x1080 and 30 fps) is shown
/// as its poster, and `Warning` says so and points at the command (`The
/// video background is shown as a still: it can't play without an NVIDIA
/// GPU; run omarchy-broadcast media convert-command '<path>' for the
/// conversion command`, the path `<file>` when it would not fit or not
/// show unchanged); `GetConvertCommand(s path) -> s` gives the command for
/// any video, and `GetMedia`'s video items gain `cpu_ready` (b) and, when
/// it is false, `convert_command` (s), the same command
/// ([`media::MediaEntry`]). `GetMediaDirs(s kind) -> a{sv}` names a
/// kind's user directories, the user's videos folder among them
/// ([`media::MediaDirs`], [`media::MEDIA_DIRS_REVISION`]).
pub const CPU_CONVERT_REVISION: u32 = 19;
/// The smallest move of a value that announces a change of
/// `StageValues` (a change of the set always does).
pub const STAGE_VALUE_STEP: f64 = 0.01;
/// The shortest time between two announced changes of `StageValues`, in
/// milliseconds: an easing value does not flood the bus.
pub const STAGE_VALUES_PERIOD_MS: u64 = 250;
/// The first [`INTERFACE_REVISION`] with the microphone: the `Mic*`
/// properties, `MicStats`, `SetMicHost` and the `mic.*` settings.
pub const MIC_REVISION: u32 = 13;
/// The values of `MicState`: the mic host is not running (`off`); raw
/// passthrough, nothing processed (`passthrough`); a chain loading or
/// warming up (`starting`); the microphone processed (`active`); stages on
/// that cannot run (`error`, `MicError` says why). The set may grow: a
/// client shows a value it does not know as it is.
pub const MIC_STATES: [&str; 5] = ["off", "passthrough", "starting", "active", "error"];
/// `MicState` of a daemon whose mic host is not running, and of one that
/// predates [`MIC_REVISION`].
pub const MIC_OFF: &str = "off";
/// The first [`INTERFACE_REVISION`] with `camera.capture.mode` and an
/// editable `output.mode`.
pub const MODES_REVISION: u32 = 12;
/// The first [`INTERFACE_REVISION`] with `LoopbackConsumers`,
/// `PipeWireConsumers` and `PipeWireOutput`.
pub const PIPEWIRE_REVISION: u32 = 3;
/// The first [`INTERFACE_REVISION`] with `Denoise` and `SetDenoise`.
pub const DENOISE_REVISION: u32 = 4;
/// The first [`INTERFACE_REVISION`] with the live preview:
/// `AcquirePreview`, `ReleasePreview`, `PreviewFrame` and
/// `PreviewConsumers`.
pub const PREVIEW_REVISION: u32 = 5;
/// The frame rates `AcquirePreview` accepts.
pub const PREVIEW_FPS: [u32; 2] = [15, 30];
/// The first [`INTERFACE_REVISION`] with the camera image controls:
/// `CameraControls`, `GetCameraControls`, `SetCameraControl` and
/// `ResetCameraControls`.
pub const CONTROLS_REVISION: u32 = 6;
/// The first [`INTERFACE_REVISION`] with the camera settings channel
/// ([`settings`]): `SettingsRevision`, `SettingsSchemaSerial`,
/// `GetSettings`, `SetSetting`, `ResetSettings` and `SettingsChanged`.
pub const SETTINGS_REVISION: u32 = 7;
/// The first [`INTERFACE_REVISION`] with global settings in the tree
/// ([`settings::SCOPE_GLOBAL`]): the video effects.
pub const GLOBAL_SETTINGS_REVISION: u32 = 8;
/// The first [`INTERFACE_REVISION`] with the `Warning` property.
pub const WARNING_REVISION: u32 = 11;
/// The settings schema version this crate describes: the meaning of the
/// setting dictionaries ([`settings`]), separate from
/// [`INTERFACE_REVISION`]. Additions (a key, a setting type, a hint) keep
/// it; a changed meaning raises it, and a client that meets a higher one
/// does not render the settings.
pub const SETTINGS_SCHEMA: u32 = 1;
/// The image controls the daemon may expose, by their v4l2-ctl names, in
/// the order it reports them. A camera exposes those it has; the daemon
/// checks names and values, this list is for clients (labels, layout).
pub const CAMERA_CONTROL_NAMES: [&str; 5] = [
    "brightness",
    "contrast",
    "saturation",
    "sharpness",
    "backlight_compensation",
];
/// `PipeWireOutput` of a daemon whose PipeWire output is not configured, or
/// that has none (before [`PIPEWIRE_REVISION`]).
pub const PIPEWIRE_OFF: &str = "off";
/// Prefix of every D-Bus error name the daemon replies with
/// (`br.com.dias.OmarchyBroadcast1.Error.NoSuchCamera`, ...).
pub const ERROR_PREFIX: &str = "br.com.dias.OmarchyBroadcast1.Error";

/// Lifecycle state of the daemon, carried as a lower-case string (`s`).
///
/// The set is fixed for [`INTERFACE`]: a new state would be a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
#[zbus(signature = "s")]
pub enum State {
    /// Disabled by the user (`Enabled` = false): camera closed, no CUDA
    /// context, black placeholder frames on the loopback.
    Off,
    /// Enabled, no consumer: camera closed, no CUDA context, placeholder.
    Idle,
    /// A consumer is present: the camera and the GPU pipeline are starting.
    Starting,
    /// Camera frames are streaming (includes the grace period after the last
    /// consumer leaves).
    Active,
    /// A lifecycle failure (no loopback, no camera, camera lost); the
    /// `Error` property says which.
    Error,
}

impl State {
    /// Every state, in lifecycle order.
    pub const ALL: [State; 5] = [
        State::Off,
        State::Idle,
        State::Starting,
        State::Active,
        State::Error,
    ];

    /// The wire form: `off`, `idle`, `starting`, `active`, `error`.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Off => "off",
            State::Idle => "idle",
            State::Starting => "starting",
            State::Active => "active",
            State::Error => "error",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not one of the [`State`] names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownState(pub String);

impl fmt::Display for UnknownState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown state {:?} (expected off, idle, starting, active or error)",
            self.0
        )
    }
}

impl std::error::Error for UnknownState {}

impl FromStr for State {
    type Err = UnknownState;

    /// Parses the exact lower-case wire form.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        State::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| UnknownState(s.to_string()))
    }
}

/// One camera the daemon can stream from, as listed by the `Cameras`
/// property: a D-Bus struct `(sss)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct CameraInfo {
    /// Stable identifier, accepted by `SelectCamera` and reported by
    /// `ActiveCamera`.
    pub id: String,
    /// Human-readable name: the V4L2 card name (a repeated `X: X` reduced
    /// to `X`), unique among the cameras present (` (2)`, ` (3)`, ...).
    pub name: String,
    /// USB `vid:pid` in lower-case hex (`1234:abcd`), empty if not a USB
    /// device.
    pub usb: String,
}

/// A named numeric value a running stage reports about the last frame, as
/// listed by the `StageValues` property (revision 16): a D-Bus struct
/// `(ssd)`. Auto-frame reports `("auto-frame", "zoom", z)`, the frame's
/// height over the crop's (1.0 at the full frame).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct StageValue {
    /// The stage id (`auto-frame`).
    pub stage: String,
    /// The value's name, unique within the stage (`zoom`).
    pub name: String,
    /// The value.
    pub value: f64,
}

/// One image control of a camera: a D-Bus struct `(siiiiib)`, an entry of
/// `GetCameraControls` and of the `CameraControls` property (revision 6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct CameraControl {
    /// One of [`CAMERA_CONTROL_NAMES`].
    pub name: String,
    pub min: i32,
    pub max: i32,
    /// The distance between two valid values, counted from `min`.
    pub step: i32,
    /// The camera's own default.
    pub default: i32,
    /// For the camera that streams, the value on the device; otherwise the
    /// value it will be given when it opens: the user's stored value, else
    /// the default after a pending reset, else the value on the device.
    pub value: i32,
    /// The user has a stored value for it, applied whenever the camera
    /// opens.
    pub stored: bool,
}

/// The image controls of one camera: a D-Bus struct `(sa(siiiiib))`, the
/// `CameraControls` property (revision 6). `camera` is the id of
/// `ActiveCamera`, empty (with no control) when there is no camera.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct CameraControls {
    pub camera: String,
    pub controls: Vec<CameraControl>,
}

/// One sample of the `Stats` signal. A value that cannot be measured is
/// [`Stats::NOT_MEASURABLE`] (`-1.0`).
///
/// This struct is a convenience for code that handles the four values
/// together. Its own D-Bus signature is the struct `(dddd)`, but the `Stats`
/// signal carries four separate arguments, signature `dddd`: emit the signal
/// with the four fields as arguments, never with this value as one argument.
/// On the client side, [`client::StatsArgs::to_stats`] builds it from the
/// signal's arguments.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Type)]
pub struct Stats {
    /// Frames delivered to the loopback device per second, as counted by
    /// the daemon's loopback writer: camera frames, repeats and black
    /// placeholder frames alike.
    pub fps: f64,
    /// Median capture-to-output latency, in milliseconds.
    pub latency_p50_ms: f64,
    /// CPU use of the daemon, in percent of one core.
    pub cpu_percent: f64,
    /// GPU utilisation, in percent.
    pub gpu_percent: f64,
}

impl Stats {
    /// The value of a field that cannot be measured.
    pub const NOT_MEASURABLE: f64 = -1.0;

    /// Whether `value` is a measurement rather than [`Self::NOT_MEASURABLE`]
    /// (any negative value counts as not measurable).
    pub fn is_measured(value: f64) -> bool {
        value >= 0.0
    }
}

impl Default for Stats {
    /// Every field not measurable.
    fn default() -> Self {
        Stats {
            fps: Stats::NOT_MEASURABLE,
            latency_p50_ms: Stats::NOT_MEASURABLE,
            cpu_percent: Stats::NOT_MEASURABLE,
            gpu_percent: Stats::NOT_MEASURABLE,
        }
    }
}

/// The errors the daemon's methods reply with, named
/// `br.com.dias.OmarchyBroadcast1.Error.<Variant>`.
///
/// On the client side a reply carrying one of these names converts back to
/// its variant; any other failure is [`IpcError::ZBus`].
#[derive(Debug, DBusError)]
#[zbus(prefix = "br.com.dias.OmarchyBroadcast1.Error")]
pub enum IpcError {
    /// `SelectCamera` (or, from revision 6, a camera control method; from
    /// revision 7, the `camera` option of a settings method) named no
    /// camera that is present.
    NoSuchCamera(String),
    /// The configuration file is invalid; the previous config stays in force.
    InvalidConfig(String),
    /// The request cannot be served right now (e.g. no camera at all).
    Unavailable(String),
    /// An argument is out of its domain: an unknown effect, a blur strength
    /// outside 0 to 1, a background image that is not an absolute path to a
    /// readable regular file, or `replace` with no background image
    /// (revision 2); a preview frame rate other than 15 or 30 (revision 5);
    /// a camera control name that is not one of [`CAMERA_CONTROL_NAMES`] or
    /// that the camera does not expose, or a value outside the control's
    /// range or off its step (revision 6); a `SetSetting` value of the
    /// wrong D-Bus type, outside the setting's range or off its step, an
    /// enum value that is not an option, an action fired with anything but
    /// `true`, a `ResetSettings` scope that is not one (revision 7).
    InvalidArgument(String),
    /// Any other failure.
    Failed(String),
    /// `SetSetting` or `ResetSettings` named a setting id that is not in
    /// the target camera's tree (revision 7).
    UnknownSetting(String),
    /// `SetSetting` named a setting that is not enabled now; the message is
    /// its `disabled_reason` (revision 7).
    Disabled(String),
    /// `SetSetting` came over the daemon's cap on device writes; nothing
    /// was stored (revision 7).
    RateLimited(String),
    /// A transport or protocol error (not a daemon reply).
    #[zbus(error)]
    ZBus(zbus::Error),
}

impl IpcError {
    /// The human-readable message, without the error name.
    pub fn message(&self) -> String {
        match self {
            IpcError::NoSuchCamera(m)
            | IpcError::InvalidConfig(m)
            | IpcError::Unavailable(m)
            | IpcError::InvalidArgument(m)
            | IpcError::Failed(m)
            | IpcError::UnknownSetting(m)
            | IpcError::Disabled(m)
            | IpcError::RateLimited(m) => m.clone(),
            IpcError::ZBus(e) => e.to_string(),
        }
    }
}

/// The microphone's properties (revision 13): `MicState`,
/// `MicConsumers`, `MicProvider`, `MicLatencyMs` and `MicError`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MicStatus {
    /// `MicState`: one of [`MIC_STATES`] (or a later revision's value).
    pub state: String,
    /// `MicConsumers`: the applications recording from the microphone; 0
    /// when not known.
    pub consumers: u32,
    /// `MicProvider`: the module running each stage, as text
    /// (`mic-denoise: <module>, ...`); empty when nothing is processed.
    pub provider: String,
    /// `MicLatencyMs`: the delay processing adds, in milliseconds; 0 when
    /// nothing is processed.
    pub latency_ms: f64,
    /// `MicError`: why stages that are on cannot run; empty when none.
    pub error: String,
}

impl Default for MicStatus {
    fn default() -> Self {
        MicStatus {
            state: MIC_OFF.into(),
            consumers: 0,
            provider: String::new(),
            latency_ms: 0.0,
            error: String::new(),
        }
    }
}

/// Every property of the interface at one moment, for `status`-style output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub state: State,
    pub error: String,
    /// `Warning` (revision 11): what streams on without stages the user
    /// selected (no module provides them), or black while the effect
    /// cannot run; one line; empty when nothing, and when the daemon does
    /// not offer it.
    pub warning: String,
    pub enabled: bool,
    pub cameras: Vec<CameraInfo>,
    pub active_camera: String,
    pub output_mode: String,
    pub effect: String,
    /// `BlurStrength` (revision 2); `0.0` when the daemon does not offer it.
    pub blur_strength: f64,
    /// `BackgroundImage` (revision 2); empty when the daemon does not offer
    /// it.
    pub background_image: String,
    /// `Denoise` (revision 4): video noise removal is requested; `false`
    /// when the daemon does not offer it (tell that case by
    /// `interface_revision` below [`DENOISE_REVISION`]).
    pub denoise: bool,
    /// `Consumers`: every consumer, those holding the loopback, those
    /// linked to the PipeWire output and the preview leases.
    pub consumers: u32,
    /// `LoopbackConsumers` (revision 3): the processes holding the
    /// loopback. From a daemon without it, `Consumers` (which then counts
    /// only those).
    pub loopback_consumers: u32,
    /// `PipeWireConsumers` (revision 3): the links to the PipeWire output;
    /// `0` when the daemon does not offer it.
    pub pipewire_consumers: u32,
    /// `PreviewConsumers` (revision 5): the preview leases held; `0` when
    /// the daemon does not offer it.
    pub preview_consumers: u32,
    /// `PipeWireOutput` (revision 3): `off`, `unsupported`, `connecting`,
    /// `ready`, `streaming` or `failed`; `off` when the daemon does not
    /// offer it.
    pub pipewire_output: String,
    /// `CameraControls` (revision 6): the image controls of `ActiveCamera`;
    /// `None` when the daemon does not offer it.
    pub camera_controls: Option<CameraControls>,
    /// `SettingsRevision` (revision 7): bumped on every change of the
    /// published settings tree; `None` when the daemon does not offer it.
    pub settings_revision: Option<u32>,
    /// `SettingsSchemaSerial` (revision 7): bumped on a structural change
    /// of the settings tree; `None` when the daemon does not offer it.
    pub settings_schema_serial: Option<u32>,
    /// `ModulesSerial` (revision 9): bumped whenever what `GetModules` or
    /// `GetStageOrder` returns changes; `None` when the daemon does not
    /// offer it.
    pub modules_serial: Option<u32>,
    /// `BackendProfile` (revision 9): `quality` or `battery`; `None` when
    /// the daemon does not offer it.
    pub backend_profile: Option<String>,
    /// `ConfigSerial` (revision 10): bumped whenever the configuration in
    /// force or the config schema changes; `None` when the daemon does not
    /// offer it.
    pub config_serial: Option<u32>,
    /// The microphone (revision 13): `None` when the daemon does not offer
    /// it.
    pub mic: Option<MicStatus>,
    /// `StageValues` (revision 16): the values the running stages report;
    /// empty when the camera is idle, when no stage reports, and when the
    /// daemon does not offer it.
    pub stage_values: Vec<StageValue>,
    /// `CameraHealth` (revision 20): what the daemon knows about the active
    /// camera's health; `None` when the daemon does not offer it.
    pub camera_health: Option<CameraHealth>,
    pub interface_revision: u32,
}

impl Snapshot {
    /// A snapshot with nothing read yet: the starting point of
    /// [`Snapshot::from_properties`].
    pub fn empty() -> Snapshot {
        Snapshot {
            state: State::Off,
            error: String::new(),
            warning: String::new(),
            enabled: false,
            cameras: Vec::new(),
            active_camera: String::new(),
            output_mode: String::new(),
            effect: String::new(),
            blur_strength: 0.0,
            background_image: String::new(),
            denoise: false,
            consumers: 0,
            loopback_consumers: 0,
            pipewire_consumers: 0,
            preview_consumers: 0,
            pipewire_output: PIPEWIRE_OFF.into(),
            camera_controls: None,
            settings_revision: None,
            settings_schema_serial: None,
            modules_serial: None,
            backend_profile: None,
            config_serial: None,
            mic: None,
            stage_values: Vec::new(),
            camera_health: None,
            interface_revision: 0,
        }
    }

    /// Sets the field that the D-Bus property `name` maps to, from a
    /// `GetAll` reply or a `PropertiesChanged` signal. Properties this crate
    /// does not know (added by a later revision) are ignored; a value of the
    /// wrong type, or a `State` that is not a [`State`] name, is an error.
    ///
    /// For a daemon known to predate [`PIPEWIRE_REVISION`]
    /// (`interface_revision` read and below it), `Consumers` also sets
    /// `loopback_consumers`: such a daemon counts only the loopback.
    pub fn apply_property(&mut self, name: &str, value: &Variant<'_>) -> zbus::Result<()> {
        match name {
            "State" => {
                let s: String = deserialize_for_property(value)?;
                self.state = s
                    .parse()
                    .map_err(|e: UnknownState| zbus::Error::Failure(e.to_string()))?;
            }
            "Error" => self.error = deserialize_for_property(value)?,
            "Warning" => self.warning = deserialize_for_property(value)?,
            "Enabled" => self.enabled = deserialize_for_property(value)?,
            "Cameras" => self.cameras = deserialize_for_property::<Vec<CameraInfo>>(value)?,
            "ActiveCamera" => self.active_camera = deserialize_for_property(value)?,
            "OutputMode" => self.output_mode = deserialize_for_property(value)?,
            "Effect" => self.effect = deserialize_for_property(value)?,
            "BlurStrength" => self.blur_strength = deserialize_for_property(value)?,
            "BackgroundImage" => self.background_image = deserialize_for_property(value)?,
            "Denoise" => self.denoise = deserialize_for_property(value)?,
            "Consumers" => {
                self.consumers = deserialize_for_property(value)?;
                if (1..PIPEWIRE_REVISION).contains(&self.interface_revision) {
                    self.loopback_consumers = self.consumers;
                }
            }
            "LoopbackConsumers" => self.loopback_consumers = deserialize_for_property(value)?,
            "PipeWireConsumers" => self.pipewire_consumers = deserialize_for_property(value)?,
            "PreviewConsumers" => self.preview_consumers = deserialize_for_property(value)?,
            "PipeWireOutput" => self.pipewire_output = deserialize_for_property(value)?,
            "CameraControls" => self.camera_controls = Some(deserialize_for_property(value)?),
            "SettingsRevision" => self.settings_revision = Some(deserialize_for_property(value)?),
            "SettingsSchemaSerial" => {
                self.settings_schema_serial = Some(deserialize_for_property(value)?)
            }
            "ModulesSerial" => self.modules_serial = Some(deserialize_for_property(value)?),
            "BackendProfile" => self.backend_profile = Some(deserialize_for_property(value)?),
            "ConfigSerial" => self.config_serial = Some(deserialize_for_property(value)?),
            "MicState" => self.mic_mut().state = deserialize_for_property(value)?,
            "MicConsumers" => self.mic_mut().consumers = deserialize_for_property(value)?,
            "MicProvider" => self.mic_mut().provider = deserialize_for_property(value)?,
            "MicLatencyMs" => self.mic_mut().latency_ms = deserialize_for_property(value)?,
            "MicError" => self.mic_mut().error = deserialize_for_property(value)?,
            "StageValues" => {
                self.stage_values = deserialize_for_property::<Vec<StageValue>>(value)?
            }
            "CameraHealth" => {
                let dict: settings::VariantDict = deserialize_for_property(value)?;
                self.camera_health = Some(CameraHealth::from_dict(&dict));
            }
            "InterfaceRevision" => self.interface_revision = deserialize_for_property(value)?,
            _ => {}
        }
        Ok(())
    }

    /// The microphone's properties, made (with their defaults) at the
    /// first one read.
    fn mic_mut(&mut self) -> &mut MicStatus {
        self.mic.get_or_insert_with(MicStatus::default)
    }

    /// A snapshot from the properties returned by `GetAll`. `State` is
    /// required; any other missing property keeps its [`Snapshot::empty`]
    /// value, except `LoopbackConsumers`, which is then `Consumers` (a
    /// daemon without it counts only the loopback).
    pub fn from_properties<'a, I>(props: I) -> zbus::Result<Snapshot>
    where
        I: IntoIterator<Item = (&'a str, &'a Variant<'a>)>,
    {
        let mut snap = Snapshot::empty();
        let mut has_state = false;
        let mut has_loopback = false;
        for (name, value) in props {
            has_state |= name == "State";
            has_loopback |= name == "LoopbackConsumers";
            snap.apply_property(name, value)?;
        }
        if !has_loopback {
            snap.loopback_consumers = snap.consumers;
        }
        if !has_state {
            return Err(zbus::Error::Failure(
                "the daemon's properties have no State".into(),
            ));
        }
        Ok(snap)
    }

    /// Reads every property with one `org.freedesktop.DBus.Properties.GetAll`
    /// carrying the `NO_AUTO_START` flag, so the read never starts the
    /// daemon: the way for clients to read the daemon's state.
    ///
    /// `Ok(None)` means the daemon is not running (the bus reports that
    /// [`BUS_NAME`] has no owner). Any other failure, including a reply that
    /// does not map to a [`Snapshot`], is an error. No time limit is applied
    /// (see the crate documentation).
    pub async fn fetch_no_autostart(conn: &Connection) -> zbus::Result<Option<Snapshot>> {
        let props: zbus::Proxy<'_> = zbus::proxy::Builder::new(conn)
            .destination(BUS_NAME)
            .path(OBJECT_PATH)
            .interface("org.freedesktop.DBus.Properties")
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let reply: Option<HashMap<String, OwnedVariant>> = match props
            .call_with_flags("GetAll", MethodFlags::NO_AUTO_START, &(INTERFACE,))
            .await
        {
            Ok(reply) => reply,
            Err(e) if is_not_running_error(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let reply = reply.ok_or_else(|| zbus::Error::Failure("GetAll returned no reply".into()))?;
        Snapshot::from_properties(reply.iter().map(|(k, v)| (k.as_str(), &**v))).map(Some)
    }

    /// Reads every property through `proxy` (from its cache when the proxy
    /// caches properties, which is the default).
    ///
    /// This read may start the daemon by D-Bus activation: a caching proxy
    /// built on a bus connection has already sent a `GetAll` without
    /// `NO_AUTO_START`, and the getters of an uncached one send `Get`
    /// without it. Clients that only look use
    /// [`Snapshot::fetch_no_autostart`].
    ///
    /// The properties added after revision 1 (`BlurStrength`,
    /// `BackgroundImage`, `PipeWireConsumers`, `PipeWireOutput`, `Denoise`,
    /// `PreviewConsumers`, `CameraControls`, `SettingsRevision`,
    /// `SettingsSchemaSerial`, `ModulesSerial`, `BackendProfile`, `ConfigSerial`,
    /// `Warning`, the `Mic*` properties, `StageValues`, `CameraHealth`) keep their
    /// [`Snapshot::empty`] value when the daemon does not offer
    /// them (see [`is_unknown_property_error`]), and `LoopbackConsumers` is then
    /// `Consumers`, as [`Snapshot::from_properties`] does for a property
    /// missing from `GetAll`; any other failure to read them is an error.
    pub async fn fetch(proxy: &OmarchyBroadcastProxy<'_>) -> zbus::Result<Snapshot> {
        let empty = Snapshot::empty();
        let consumers = proxy.consumers().await?;
        Ok(Snapshot {
            state: proxy.state().await?,
            error: proxy.error().await?,
            warning: optional(proxy.warning().await, empty.warning)?,
            enabled: proxy.enabled().await?,
            cameras: proxy.cameras().await?,
            active_camera: proxy.active_camera().await?,
            output_mode: proxy.output_mode().await?,
            effect: proxy.effect().await?,
            blur_strength: optional(proxy.blur_strength().await, empty.blur_strength)?,
            background_image: optional(proxy.background_image().await, empty.background_image)?,
            denoise: optional(proxy.denoise().await, empty.denoise)?,
            consumers,
            loopback_consumers: optional(proxy.loopback_consumers().await, consumers)?,
            pipewire_consumers: optional(
                proxy.pipewire_consumers().await,
                empty.pipewire_consumers,
            )?,
            preview_consumers: optional(proxy.preview_consumers().await, empty.preview_consumers)?,
            pipewire_output: optional(proxy.pipewire_output().await, empty.pipewire_output)?,
            camera_controls: optional(
                proxy.camera_controls().await.map(Some),
                empty.camera_controls,
            )?,
            settings_revision: optional(
                proxy.settings_revision().await.map(Some),
                empty.settings_revision,
            )?,
            settings_schema_serial: optional(
                proxy.settings_schema_serial().await.map(Some),
                empty.settings_schema_serial,
            )?,
            modules_serial: optional(proxy.modules_serial().await.map(Some), empty.modules_serial)?,
            backend_profile: optional(
                proxy.backend_profile().await.map(Some),
                empty.backend_profile,
            )?,
            config_serial: optional(proxy.config_serial().await.map(Some), empty.config_serial)?,
            mic: fetch_mic(proxy).await?,
            stage_values: optional(proxy.stage_values().await, empty.stage_values)?,
            camera_health: optional(
                proxy
                    .camera_health()
                    .await
                    .map(|d| Some(CameraHealth::from_dict(&d))),
                empty.camera_health,
            )?,
            interface_revision: proxy.interface_revision().await?,
        })
    }
}

/// The microphone's properties through `proxy`; `None` when the daemon
/// does not have them (before [`MIC_REVISION`]).
async fn fetch_mic(proxy: &OmarchyBroadcastProxy<'_>) -> zbus::Result<Option<MicStatus>> {
    let Some(state) = optional(proxy.mic_state().await.map(Some), None)? else {
        return Ok(None);
    };
    Ok(Some(MicStatus {
        state,
        consumers: proxy.mic_consumers().await?,
        provider: proxy.mic_provider().await?,
        latency_ms: proxy.mic_latency_ms().await?,
        error: proxy.mic_error().await?,
    }))
}

/// `value`, or `default` when the read failed because the daemon does not
/// have the property.
fn optional<T>(value: zbus::Result<T>, default: T) -> zbus::Result<T> {
    match value {
        Err(e) if is_unknown_property_error(&e) => Ok(default),
        other => other,
    }
}

/// Whether `e` is the reply to `Properties.Get` for a property the object
/// does not have: `org.freedesktop.DBus.Error.UnknownProperty`, or
/// `org.freedesktop.DBus.Error.InvalidArgs`, which some implementations
/// send instead. A daemon of an older [`INTERFACE_REVISION`] replies so for
/// the properties added since.
pub fn is_unknown_property_error(e: &zbus::Error) -> bool {
    const NAMES: [&str; 2] = [
        "org.freedesktop.DBus.Error.UnknownProperty",
        "org.freedesktop.DBus.Error.InvalidArgs",
    ];
    match e {
        zbus::Error::MethodError(name, _, _) => NAMES.contains(&name.as_str()),
        zbus::Error::FDO(f) => NAMES.contains(&f.name().as_str()),
        _ => false,
    }
}

/// Whether `e` is the bus's reply that [`BUS_NAME`] has no owner (and, for
/// a call that allowed activation, cannot be started): the daemon is not
/// running. `org.freedesktop.DBus.Error.NameHasNoOwner` and
/// `org.freedesktop.DBus.Error.ServiceUnknown` qualify.
pub fn is_not_running_error(e: &zbus::Error) -> bool {
    const NAMES: [&str; 2] = [
        "org.freedesktop.DBus.Error.NameHasNoOwner",
        "org.freedesktop.DBus.Error.ServiceUnknown",
    ];
    match e {
        zbus::Error::MethodError(name, _, _) => NAMES.contains(&name.as_str()),
        zbus::Error::FDO(f) => NAMES.contains(&f.name().as_str()),
        _ => false,
    }
}

/// Whether the daemon is running: asks the bus whether [`BUS_NAME`] has an
/// owner (`org.freedesktop.DBus.NameHasOwner`). Never starts the daemon.
/// Needs a bus connection (not a peer-to-peer one).
pub async fn daemon_is_running(conn: &Connection) -> zbus::Result<bool> {
    let dbus = DBusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let name = BusName::try_from(BUS_NAME)?;
    Ok(dbus.name_has_owner(name).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mic_properties_fill_the_mic_status() {
        let state = Variant::from("idle");
        let mic = Variant::from("active");
        let n = Variant::from(2u32);
        let ms = Variant::from(20.5f64);
        let text = Variant::from("mic-denoise: m");
        let snap = Snapshot::from_properties([
            ("State", &state),
            ("MicState", &mic),
            ("MicConsumers", &n),
            ("MicLatencyMs", &ms),
            ("MicProvider", &text),
        ])
        .unwrap();
        assert_eq!(
            snap.mic,
            Some(MicStatus {
                state: "active".into(),
                consumers: 2,
                provider: "mic-denoise: m".into(),
                latency_ms: 20.5,
                error: String::new(),
            })
        );
        // Before revision 13: none.
        let old = Snapshot::from_properties([("State", &state)]).unwrap();
        assert_eq!(old.mic, None);
        // A value of the wrong type is an error.
        assert!(Snapshot::from_properties([("State", &state), ("MicConsumers", &mic)]).is_err());
        assert_eq!(MicStatus::default().state, MIC_OFF);
        assert!(MIC_STATES.contains(&MIC_OFF));
    }

    #[test]
    fn stage_values_are_read_and_empty_before_revision_16() {
        assert_eq!(<StageValue as Type>::SIGNATURE.to_string(), "(ssd)");
        assert_eq!(<Vec<StageValue> as Type>::SIGNATURE.to_string(), "a(ssd)");
        let state = Variant::from("active");
        let values = zbus::as_variant::to_owned_variant(&vec![StageValue {
            stage: "auto-frame".into(),
            name: "zoom".into(),
            value: 1.24,
        }])
        .unwrap();
        let snap =
            Snapshot::from_properties([("State", &state), ("StageValues", &values)]).unwrap();
        assert_eq!(
            snap.stage_values,
            [StageValue {
                stage: "auto-frame".into(),
                name: "zoom".into(),
                value: 1.24,
            }]
        );
        // From a daemon before revision 16: none.
        let old = Snapshot::from_properties([("State", &state)]).unwrap();
        assert!(old.stage_values.is_empty());
        // A value of the wrong type is an error.
        assert!(Snapshot::from_properties([("State", &state), ("StageValues", &state)]).is_err());
        const { assert!(STAGE_VALUES_REVISION <= INTERFACE_REVISION) };
    }

    #[test]
    fn camera_health_is_read_and_none_before_revision_20() {
        let state = Variant::from("active");
        let health = CameraHealth {
            camera: "cam".into(),
            delivered_fps: 19.5,
            nominal_fps: 30.0,
            corrupt_frames: 3,
            warnings: vec!["A warning.".into()],
            ..CameraHealth::none()
        };
        let dict = zbus::as_variant::to_owned_variant(&health.to_dict()).unwrap();
        let snap = Snapshot::from_properties([("State", &state), ("CameraHealth", &dict)]).unwrap();
        assert_eq!(snap.camera_health, Some(health));
        // From a daemon before revision 20: none.
        let old = Snapshot::from_properties([("State", &state)]).unwrap();
        assert_eq!(old.camera_health, None);
        // A value of the wrong type is an error.
        assert!(Snapshot::from_properties([("State", &state), ("CameraHealth", &state)]).is_err());
    }

    #[test]
    fn names_are_consistent() {
        assert!(INTERFACE.starts_with(BUS_NAME));
        assert!(INTERFACE.ends_with('1'));
        assert_eq!(ERROR_PREFIX, format!("{INTERFACE}.Error"));
        assert_eq!(OBJECT_PATH, format!("/{}", BUS_NAME.replace('.', "/")));
        let e = IpcError::NoSuchCamera("x".into());
        assert_eq!(
            zbus::DBusError::name(&e).as_str(),
            format!("{ERROR_PREFIX}.NoSuchCamera")
        );
        let e = IpcError::InvalidArgument("bad".into());
        assert_eq!(
            zbus::DBusError::name(&e).as_str(),
            format!("{ERROR_PREFIX}.InvalidArgument")
        );
        assert_eq!(e.message(), "bad");
    }

    #[test]
    fn state_round_trips() {
        for st in State::ALL {
            assert_eq!(st.as_str().parse::<State>(), Ok(st));
            assert_eq!(st.to_string(), st.as_str());
            let json = serde_json::to_string(&st).unwrap();
            assert_eq!(json, format!("{:?}", st.as_str()));
            assert_eq!(serde_json::from_str::<State>(&json).unwrap(), st);
        }
        assert!("Active".parse::<State>().is_err());
        assert!("".parse::<State>().is_err());
        assert_eq!(<State as Type>::SIGNATURE.to_string(), "s");
    }

    #[test]
    fn wire_signatures() {
        assert_eq!(<CameraInfo as Type>::SIGNATURE.to_string(), "(sss)");
        assert_eq!(<Vec<CameraInfo> as Type>::SIGNATURE.to_string(), "a(sss)");
        assert_eq!(<Stats as Type>::SIGNATURE.to_string(), "(dddd)");
        assert_eq!(<CameraControl as Type>::SIGNATURE.to_string(), "(siiiiib)");
        assert_eq!(
            <Vec<CameraControl> as Type>::SIGNATURE.to_string(),
            "a(siiiiib)"
        );
        assert_eq!(
            <CameraControls as Type>::SIGNATURE.to_string(),
            "(sa(siiiiib))"
        );
    }

    fn cam(id: &str, name: &str) -> CameraInfo {
        CameraInfo {
            id: id.into(),
            name: name.into(),
            usb: String::new(),
        }
    }

    #[test]
    fn properties_build_a_snapshot() {
        let state = Variant::from("active");
        let enabled = Variant::from(true);
        let consumers = Variant::from(3u32);
        let cameras = zbus::as_variant::to_owned_variant(&vec![cam("a", "A")]).unwrap();
        let future = Variant::from(7u32);
        let props: Vec<(&str, &Variant<'_>)> = vec![
            ("State", &state),
            ("Enabled", &enabled),
            ("Consumers", &consumers),
            ("Cameras", &cameras),
            ("SomethingNew", &future),
        ];
        let snap = Snapshot::from_properties(props).unwrap();
        assert_eq!(snap.state, State::Active);
        // Properties of a later revision missing from GetAll keep the empty
        // values.
        assert_eq!(snap.blur_strength, 0.0);
        assert_eq!(snap.background_image, "");
        assert!(snap.enabled);
        assert_eq!(snap.consumers, 3);
        assert_eq!(snap.cameras, vec![cam("a", "A")]);
        assert_eq!(snap.error, "");

        let bad = Variant::from("Active");
        assert!(Snapshot::from_properties([("State", &bad)]).is_err());
        assert!(Snapshot::from_properties([("Enabled", &enabled)]).is_err());
        let wrong_type = Variant::from(1u32);
        assert!(Snapshot::from_properties([("State", &state), ("Enabled", &wrong_type)]).is_err());

        // A change applies to one field and leaves the others.
        let mut changed = snap.clone();
        let error = Variant::from("camera lost");
        changed.apply_property("Error", &error).unwrap();
        assert_eq!(changed.error, "camera lost");
        assert_eq!(changed.state, snap.state);
        assert!(changed.apply_property("Consumers", &state).is_err());

        // The revision-2 properties.
        let strength = Variant::from(0.75f64);
        let image = Variant::from("/home/u/bg.png");
        changed.apply_property("BlurStrength", &strength).unwrap();
        changed.apply_property("BackgroundImage", &image).unwrap();
        assert_eq!(changed.blur_strength, 0.75);
        assert_eq!(changed.background_image, "/home/u/bg.png");
        assert!(changed.apply_property("BlurStrength", &image).is_err());
        assert!(
            changed
                .apply_property("BackgroundImage", &strength)
                .is_err()
        );
    }

    #[test]
    fn the_revision_3_properties_and_an_older_daemon() {
        let state = Variant::from("active");
        let total = Variant::from(3u32);
        let loopback = Variant::from(1u32);
        let pipewire = Variant::from(2u32);
        let output = Variant::from("streaming");
        let rev3 = Variant::from(3u32);
        // In GetAll order whatever it is.
        let snap = Snapshot::from_properties([
            ("Consumers", &total),
            ("PipeWireOutput", &output),
            ("State", &state),
            ("PipeWireConsumers", &pipewire),
            ("InterfaceRevision", &rev3),
            ("LoopbackConsumers", &loopback),
        ])
        .unwrap();
        assert_eq!(
            (
                snap.consumers,
                snap.loopback_consumers,
                snap.pipewire_consumers,
                snap.pipewire_output.as_str()
            ),
            (3, 1, 2, "streaming")
        );
        // A change of the total leaves the split to its own properties.
        let mut changed = snap.clone();
        changed
            .apply_property("Consumers", &Variant::from(2u32))
            .unwrap();
        changed
            .apply_property("PipeWireConsumers", &Variant::from(1u32))
            .unwrap();
        changed
            .apply_property("PipeWireOutput", &Variant::from("ready"))
            .unwrap();
        assert_eq!(
            (
                changed.consumers,
                changed.loopback_consumers,
                changed.pipewire_consumers,
                changed.pipewire_output.as_str()
            ),
            (2, 1, 1, "ready")
        );
        assert!(changed.apply_property("PipeWireOutput", &total).is_err());
        assert!(
            changed
                .apply_property("LoopbackConsumers", &output)
                .is_err()
        );
        assert!(
            changed
                .apply_property("PipeWireConsumers", &output)
                .is_err()
        );

        // A revision-2 daemon: Consumers counts the loopback only, before
        // and after InterfaceRevision in the reply, and in later changes.
        let rev2 = Variant::from(2u32);
        for props in [
            [("Consumers", &total), ("InterfaceRevision", &rev2)],
            [("InterfaceRevision", &rev2), ("Consumers", &total)],
        ] {
            let snap =
                Snapshot::from_properties([("State", &state)].into_iter().chain(props)).unwrap();
            assert_eq!(snap.loopback_consumers, 3);
            assert_eq!(snap.pipewire_consumers, 0);
            assert_eq!(snap.pipewire_output, PIPEWIRE_OFF);
            let mut changed = snap.clone();
            changed
                .apply_property("Consumers", &Variant::from(1u32))
                .unwrap();
            assert_eq!((changed.consumers, changed.loopback_consumers), (1, 1));
        }
        assert_eq!(Snapshot::empty().pipewire_output, "off");
    }

    #[test]
    fn the_revision_4_property_and_an_older_daemon() {
        let state = Variant::from("idle");
        let on = Variant::from(true);
        let rev4 = Variant::from(DENOISE_REVISION);
        let snap = Snapshot::from_properties([
            ("State", &state),
            ("Denoise", &on),
            ("InterfaceRevision", &rev4),
        ])
        .unwrap();
        assert!(snap.denoise);
        let mut changed = snap.clone();
        changed
            .apply_property("Denoise", &Variant::from(false))
            .unwrap();
        assert!(!changed.denoise);
        assert!(changed.apply_property("Denoise", &state).is_err());
        // A revision-3 daemon has no Denoise: false, told apart by the
        // revision.
        let rev3 = Variant::from(3u32);
        let old =
            Snapshot::from_properties([("State", &state), ("InterfaceRevision", &rev3)]).unwrap();
        assert!(!old.denoise);
        assert!(old.interface_revision < DENOISE_REVISION);
        assert!(!Snapshot::empty().denoise);
    }

    #[test]
    fn the_revision_5_property_and_an_older_daemon() {
        let state = Variant::from("idle");
        let total = Variant::from(3u32);
        let leases = Variant::from(2u32);
        let rev5 = Variant::from(PREVIEW_REVISION);
        let snap = Snapshot::from_properties([
            ("State", &state),
            ("Consumers", &total),
            ("PreviewConsumers", &leases),
            ("LoopbackConsumers", &Variant::from(1u32)),
            ("InterfaceRevision", &rev5),
        ])
        .unwrap();
        assert_eq!(
            (
                snap.consumers,
                snap.loopback_consumers,
                snap.pipewire_consumers,
                snap.preview_consumers
            ),
            (3, 1, 0, 2)
        );
        let mut changed = snap.clone();
        changed
            .apply_property("PreviewConsumers", &Variant::from(0u32))
            .unwrap();
        assert_eq!(changed.preview_consumers, 0);
        assert!(changed.apply_property("PreviewConsumers", &state).is_err());
        // A revision-4 daemon has none: 0, told apart by the revision.
        let rev4 = Variant::from(DENOISE_REVISION);
        let old =
            Snapshot::from_properties([("State", &state), ("InterfaceRevision", &rev4)]).unwrap();
        assert_eq!(old.preview_consumers, 0);
        assert!(old.interface_revision < PREVIEW_REVISION);
        assert_eq!(Snapshot::empty().preview_consumers, 0);
        assert_eq!(PREVIEW_FPS, [15, 30]);
    }

    fn sharpness(value: i32, stored: bool) -> CameraControl {
        CameraControl {
            name: "sharpness".into(),
            min: 0,
            max: 255,
            step: 1,
            default: 128,
            value,
            stored,
        }
    }

    #[test]
    fn the_revision_6_property_and_an_older_daemon() {
        let state = Variant::from("active");
        let rev6 = Variant::from(CONTROLS_REVISION);
        let view = CameraControls {
            camera: "usb-a".into(),
            controls: vec![sharpness(200, true)],
        };
        let controls = zbus::as_variant::to_owned_variant(&view).unwrap();
        let snap = Snapshot::from_properties([
            ("State", &state),
            ("CameraControls", &controls),
            ("InterfaceRevision", &rev6),
        ])
        .unwrap();
        assert_eq!(snap.camera_controls, Some(view.clone()));
        // A change replaces the whole view.
        let mut changed = snap.clone();
        let none = zbus::as_variant::to_owned_variant(&CameraControls::default()).unwrap();
        changed.apply_property("CameraControls", &none).unwrap();
        assert_eq!(changed.camera_controls, Some(CameraControls::default()));
        assert!(changed.apply_property("CameraControls", &state).is_err());
        // A revision-5 daemon has none: None.
        let rev5 = Variant::from(PREVIEW_REVISION);
        let old =
            Snapshot::from_properties([("State", &state), ("InterfaceRevision", &rev5)]).unwrap();
        assert_eq!(old.camera_controls, None);
        assert!(old.interface_revision < CONTROLS_REVISION);
        assert_eq!(Snapshot::empty().camera_controls, None);
        const { assert!(INTERFACE_REVISION >= CONTROLS_REVISION) };
        assert_eq!(
            CAMERA_CONTROL_NAMES,
            [
                "brightness",
                "contrast",
                "saturation",
                "sharpness",
                "backlight_compensation"
            ]
        );
        // As clients serialise it.
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "camera": "usb-a",
                "controls": [{
                    "name": "sharpness", "min": 0, "max": 255, "step": 1,
                    "default": 128, "value": 200, "stored": true
                }]
            })
        );
    }

    #[test]
    fn the_revision_7_properties_errors_and_an_older_daemon() {
        let state = Variant::from("idle");
        let rev7 = Variant::from(SETTINGS_REVISION);
        let rev = Variant::from(12u32);
        let serial = Variant::from(3u32);
        let snap = Snapshot::from_properties([
            ("State", &state),
            ("SettingsRevision", &rev),
            ("SettingsSchemaSerial", &serial),
            ("InterfaceRevision", &rev7),
        ])
        .unwrap();
        assert_eq!(
            (snap.settings_revision, snap.settings_schema_serial),
            (Some(12), Some(3))
        );
        let mut changed = snap.clone();
        changed
            .apply_property("SettingsRevision", &Variant::from(13u32))
            .unwrap();
        assert_eq!(changed.settings_revision, Some(13));
        assert_eq!(changed.settings_schema_serial, Some(3));
        assert!(changed.apply_property("SettingsRevision", &state).is_err());
        assert!(
            changed
                .apply_property("SettingsSchemaSerial", &state)
                .is_err()
        );
        // A revision-6 daemon has neither: None.
        let rev6 = Variant::from(CONTROLS_REVISION);
        let old =
            Snapshot::from_properties([("State", &state), ("InterfaceRevision", &rev6)]).unwrap();
        assert_eq!(
            (old.settings_revision, old.settings_schema_serial),
            (None, None)
        );
        assert!(old.interface_revision < SETTINGS_REVISION);
        assert_eq!(INTERFACE_REVISION, DISABLED_KIND_REVISION);
        const { assert!(DISPLAY_OFFSET_REVISION < DISABLED_KIND_REVISION) };
        const { assert!(CAMERA_STATUS_REVISION < DISPLAY_OFFSET_REVISION) };
        const { assert!(MIC_HARDWARE_GAIN_REVISION < CAMERA_STATUS_REVISION) };
        const { assert!(CAMERA_XU_SETTINGS_REVISION < MIC_HARDWARE_GAIN_REVISION) };
        const { assert!(BATTERY_SAVER_AUTO_REVISION < CAMERA_XU_SETTINGS_REVISION) };
        const { assert!(BATTERY_SAVER_REVISION < BATTERY_SAVER_AUTO_REVISION) };
        const { assert!(PRIVACY_REVISION < BATTERY_SAVER_REVISION) };
        const { assert!(CAMERA_HEALTH_REVISION < PRIVACY_REVISION) };
        const { assert!(CPU_CONVERT_REVISION < CAMERA_HEALTH_REVISION) };
        const { assert!(VIDEO_BACKGROUND_REVISION < CPU_CONVERT_REVISION) };
        const { assert!(PLACEHOLDER_REVISION < VIDEO_BACKGROUND_REVISION) };
        const { assert!(STAGE_VALUES_REVISION < PLACEHOLDER_REVISION) };
        const { assert!(MEDIA_REVISION < STAGE_VALUES_REVISION) };
        const { assert!(FORCED_REVISION < MEDIA_REVISION) };
        const { assert!(MIC_REVISION < FORCED_REVISION) };
        const { assert!(MODES_REVISION < MIC_REVISION) };
        const { assert!(WARNING_REVISION < MODES_REVISION) };
        const { assert!(CONFIG_REVISION < WARNING_REVISION) };
        const { assert!(MODULES_REVISION < CONFIG_REVISION) };
        const { assert!(GLOBAL_SETTINGS_REVISION < MODULES_REVISION) };
        // Revision 9's properties; a daemon before it has neither.
        let serial = Variant::from(4u32);
        let profile = Variant::from("battery");
        let new = Snapshot::from_properties([
            ("State", &state),
            ("ModulesSerial", &serial),
            ("BackendProfile", &profile),
        ])
        .unwrap();
        assert_eq!(new.modules_serial, Some(4));
        assert_eq!(new.backend_profile.as_deref(), Some("battery"));
        assert_eq!((old.modules_serial, old.backend_profile), (None, None));
        // Revision 10's property.
        let config_serial = Variant::from(7u32);
        let rev10 =
            Snapshot::from_properties([("State", &state), ("ConfigSerial", &config_serial)])
                .unwrap();
        assert_eq!((rev10.config_serial, old.config_serial), (Some(7), None));
        // Revision 11's property; a daemon before it has none: empty.
        let warning = Variant::from("effect blur unavailable: x");
        let rev11 = Snapshot::from_properties([("State", &state), ("Warning", &warning)]).unwrap();
        assert_eq!(
            (rev11.warning.as_str(), old.warning.as_str()),
            ("effect blur unavailable: x", "")
        );
        assert!(rev11.clone().apply_property("Warning", &serial).is_err());
        assert_eq!(
            (SETTINGS_REVISION, GLOBAL_SETTINGS_REVISION, SETTINGS_SCHEMA),
            (7, 8, 1)
        );

        for (e, name) in [
            (IpcError::UnknownSetting("u".into()), "UnknownSetting"),
            (IpcError::Disabled("d".into()), "Disabled"),
            (IpcError::RateLimited("r".into()), "RateLimited"),
        ] {
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                format!("{ERROR_PREFIX}.{name}")
            );
            assert_eq!(e.message().len(), 1);
        }
    }

    #[test]
    fn unknown_property_replies_are_recognised() {
        use zbus::fdo;
        let fdo_error = |e: fdo::Error| zbus::Error::FDO(Box::new(e));
        assert!(is_unknown_property_error(&fdo_error(
            fdo::Error::UnknownProperty("BlurStrength".into())
        )));
        assert!(is_unknown_property_error(&fdo_error(
            fdo::Error::InvalidArgs("no such property".into())
        )));
        assert!(!is_unknown_property_error(&fdo_error(
            fdo::Error::AccessDenied("no".into())
        )));
        assert!(!is_unknown_property_error(&fdo_error(
            fdo::Error::NameHasNoOwner("gone".into())
        )));
        assert!(!is_unknown_property_error(&zbus::Error::Failure(
            "x".into()
        )));
        assert_eq!(optional(Ok(1), 0).unwrap(), 1);
        assert_eq!(
            optional(Err(fdo_error(fdo::Error::UnknownProperty("x".into()))), 7).unwrap(),
            7
        );
        assert!(optional(Err(fdo_error(fdo::Error::AccessDenied("x".into()))), 7).is_err());
    }

    #[test]
    fn absent_names_mean_not_running() {
        use zbus::fdo;
        let fdo_error = |e: fdo::Error| zbus::Error::FDO(Box::new(e));
        assert!(is_not_running_error(&fdo_error(
            fdo::Error::NameHasNoOwner("gone".into())
        )));
        assert!(is_not_running_error(&fdo_error(
            fdo::Error::ServiceUnknown("not activatable".into())
        )));
        assert!(!is_not_running_error(&fdo_error(fdo::Error::AccessDenied(
            "no".into()
        ))));
        assert!(!is_not_running_error(&fdo_error(
            fdo::Error::UnknownObject("no object".into())
        )));
        assert!(!is_not_running_error(&zbus::Error::Failure("x".into())));
    }

    #[test]
    fn stats_default_is_not_measurable() {
        let s = Stats::default();
        assert!(!Stats::is_measured(s.fps));
        assert!(Stats::is_measured(0.0));
    }
}
