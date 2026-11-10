//! Client proxy for `br.com.dias.OmarchyBroadcast1`.
//!
//! [`OmarchyBroadcastProxy::new`] targets the daemon's well-known name and
//! object path on the given connection; use [`OmarchyBroadcastProxy::builder`] to
//! point it elsewhere (a peer-to-peer connection, a test server).
//!
//! The `Stats` signal yields the message wrapper [`Stats`] (this module's,
//! not [`crate::Stats`]); [`StatsArgs::to_stats`] turns its arguments into
//! the value type.
//!
//! The settings methods (revision 7) carry dictionaries; the typed wrappers
//! [`OmarchyBroadcastProxy::settings_tree`],
//! [`OmarchyBroadcastProxy::settings_tree_no_autostart`],
//! [`OmarchyBroadcastProxy::set_setting_value`] and
//! [`OmarchyBroadcastProxy::reset_settings_scope`] take and return the
//! model of [`crate::settings`], and [`SettingsChangedArgs::to_changes`]
//! decodes the signal.

use std::collections::HashMap;

use zbus::Variant;
use zbus::proxy::MethodFlags;

use crate::settings::{
    ResetScope, SettingChange, SettingValue, SettingsReply, SettingsTree, VariantDict,
    camera_options,
};
use crate::{CameraControl, CameraControls, CameraInfo, IpcError, State};

/// The daemon's interface, as seen by a client. Every property is read-only
/// and emits `org.freedesktop.DBus.Properties.PropertiesChanged`.
#[zbus::proxy(
    interface = "br.com.dias.OmarchyBroadcast1",
    default_service = "br.com.dias.OmarchyBroadcast",
    default_path = "/br/com/dias/OmarchyBroadcast"
)]
pub trait OmarchyBroadcast {
    /// Enables (`true`) or disables (`false`, the privacy state `off`) the
    /// camera. Persists across restarts.
    fn set_enabled(&self, enabled: bool) -> Result<(), IpcError>;

    /// Selects a camera by id, card-name substring, `vid:pid` or
    /// `/dev/videoN`. Persists across restarts.
    fn select_camera(&self, selector: &str) -> Result<(), IpcError>;

    /// Selects the next camera in order.
    fn next_camera(&self) -> Result<(), IpcError>;

    /// Re-reads the configuration file.
    fn reload_config(&self) -> Result<(), IpcError>;

    /// Selects the video effect: `none`, `blur` or `replace`. Persists
    /// across restarts. Fails with `InvalidArgument` for an unknown effect,
    /// or `replace` with no background image set. Revision 2.
    fn set_effect(&self, effect: &str) -> Result<(), IpcError>;

    /// Sets the strength of `blur`, from 0.0 to 1.0. Persists across
    /// restarts. Fails with `InvalidArgument` outside that range.
    /// Revision 2.
    fn set_blur_strength(&self, strength: f64) -> Result<(), IpcError>;

    /// Sets the background image of `replace`: an absolute path to a
    /// readable regular file, read by the daemon. Persists across restarts.
    /// Fails with `InvalidArgument` otherwise. Revision 2.
    fn set_background_image(&self, path: &str) -> Result<(), IpcError>;

    /// Switches video noise removal on (`true`) or off. It runs before the
    /// effect, or alone with `none`. Persists across restarts. Revision 4.
    fn set_denoise(&self, denoise: bool) -> Result<(), IpcError>;

    /// Takes (or renews) the caller's preview lease at `fps` frames per
    /// second, 15 or 30 (else `InvalidArgument`), and returns the absolute
    /// path of the frame file, `$XDG_RUNTIME_DIR/omarchy-broadcast/preview.jpg`
    /// of the daemon. The lease belongs to the caller's unique bus name: it
    /// ends with [`Self::release_preview`] or when that name leaves the bus.
    /// Each atomic replace of the file is announced by `PreviewFrame`. A
    /// lease is a consumer. Fails with `Failed` when the daemon has no
    /// usable runtime directory. Revision 5.
    fn acquire_preview(&self, fps: u32) -> Result<String, IpcError>;

    /// Ends the caller's preview lease; without one, does nothing.
    /// Revision 5.
    fn release_preview(&self) -> Result<(), IpcError>;

    /// The image controls `camera` exposes (an id of `Cameras`; empty:
    /// `ActiveCamera`), in [`crate::CAMERA_CONTROL_NAMES`] order, read from
    /// the device in any state. Fails with `Unavailable` for `""` with no
    /// camera, `NoSuchCamera` for an id not present, `Failed` when the
    /// device cannot be read. Revision 6.
    fn get_camera_controls(&self, camera: &str) -> Result<Vec<CameraControl>, IpcError>;

    /// Sets the image control `name` of `camera` (as for
    /// [`Self::get_camera_controls`]) to `value` and stores it for that
    /// camera, applied whenever it opens. Written to the device at once
    /// while the camera streams; otherwise only stored. Persists across
    /// restarts. Fails with `InvalidArgument` for a name that is not one of
    /// [`crate::CAMERA_CONTROL_NAMES`] or that the camera does not expose,
    /// or a value outside the control's range or off its step; `Failed`
    /// when the device or the state file cannot be written. Revision 6.
    fn set_camera_control(&self, camera: &str, name: &str, value: i32) -> Result<(), IpcError>;

    /// Forgets the image controls stored for `camera` and puts every
    /// control back to the camera's default: at once while it streams,
    /// otherwise when it next opens. Persists across restarts. Fails with
    /// `Failed`, naming the controls, when the device refuses a default
    /// (the stored values are forgotten all the same). Revision 6.
    fn reset_camera_controls(&self, camera: &str) -> Result<(), IpcError>;

    /// The settings tree of a camera: `(revision, serial, camera, groups)`
    /// (see [`crate::settings`]). `options`: `camera` s, an id of `Cameras`
    /// (absent or `""`: `ActiveCamera`); unknown keys are ignored. With no
    /// camera present the reply is `(revision, serial, "", [])`, not an
    /// error. Fails with `NoSuchCamera` for an id not present. Revision 7.
    fn get_settings(&self, options: HashMap<&str, Variant<'_>>) -> Result<SettingsReply, IpcError>;

    /// Sets the setting `id` of a camera (`options` as for
    /// [`Self::get_settings`]) to `value`: `b` for a bool, any integer type
    /// for an int, `s` (an option value id) for an enum, `b` `true` to fire
    /// an action. Written to the device while the camera is open, otherwise
    /// stored and applied when it opens. Fails, in this order, with
    /// `UnknownSetting`, `Disabled` (the message is the reason),
    /// `InvalidArgument` (wrong type, out of range, off the step, not an
    /// option) and `RateLimited`; `Failed` when the device refuses it.
    /// Revision 7.
    fn set_setting(
        &self,
        id: &str,
        value: Variant<'_>,
        options: HashMap<&str, Variant<'_>>,
    ) -> Result<(), IpcError>;

    /// Forgets the stored values in `scope` (`camera`, `group:<id>` or
    /// `setting:<id>`, see [`ResetScope`]) and puts those settings back to
    /// their defaults: at once while the camera is open, otherwise when it
    /// next opens. `options` as for [`Self::get_settings`]. Fails with
    /// `InvalidArgument` for any other scope. Revision 7.
    fn reset_settings(
        &self,
        scope: &str,
        options: HashMap<&str, Variant<'_>>,
    ) -> Result<(), IpcError>;

    /// Roughly once per second while `active`; `-1` marks a value that
    /// cannot be measured.
    #[zbus(signal)]
    fn stats(
        &self,
        fps: f64,
        latency_p50_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

    /// Emitted after each atomic replace of the preview frame file while a
    /// preview lease is held; `seq` increases strictly within a daemon run,
    /// from 1. Revision 5.
    #[zbus(signal)]
    fn preview_frame(&self, seq: u32) -> zbus::Result<()>;

    /// The settings tree changed: one signal per operation, every setting
    /// whose value, `stored`, `enabled` or reason changed, as `(id, {value,
    /// stored, enabled, disabled_reason})` ([`SettingsChangedArgs::to_changes`]).
    /// When `serial` moved the list is empty and the client calls
    /// [`Self::get_settings`]. Revision 7.
    #[zbus(signal)]
    fn settings_changed(
        &self,
        revision: u32,
        serial: u32,
        changes: Vec<(String, VariantDict)>,
    ) -> zbus::Result<()>;

    /// Lifecycle state.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<State>;

    /// Text of the latest error, empty if none.
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;

    /// Whether the camera is enabled (false = `off`).
    #[zbus(property)]
    fn enabled(&self) -> zbus::Result<bool>;

    /// Cameras present, in selection order.
    #[zbus(property)]
    fn cameras(&self) -> zbus::Result<Vec<CameraInfo>>;

    /// Id of the camera that streams or would stream; empty if none.
    #[zbus(property)]
    fn active_camera(&self) -> zbus::Result<String>;

    /// Output contract, e.g. `NV12 1920x1080@30`.
    #[zbus(property)]
    fn output_mode(&self) -> zbus::Result<String>;

    /// Effect in use: `none`, `blur` or `replace`.
    #[zbus(property)]
    fn effect(&self) -> zbus::Result<String>;

    /// Strength of `blur`, from 0.0 to 1.0. Revision 2.
    #[zbus(property)]
    fn blur_strength(&self) -> zbus::Result<f64>;

    /// Absolute path of the background image of `replace`; empty if none.
    /// Revision 2.
    #[zbus(property)]
    fn background_image(&self) -> zbus::Result<String>;

    /// Whether video noise removal is requested. Revision 4.
    #[zbus(property)]
    fn denoise(&self) -> zbus::Result<bool>;

    /// Number of consumers: the processes holding the loopback, the links
    /// to the PipeWire output and the preview leases (before revision 3,
    /// the loopback's only; before revision 5, without the leases).
    #[zbus(property)]
    fn consumers(&self) -> zbus::Result<u32>;

    /// Number of processes holding the loopback device open. Revision 3.
    #[zbus(property)]
    fn loopback_consumers(&self) -> zbus::Result<u32>;

    /// Number of links to the PipeWire output; 0 when it is off.
    /// Revision 3.
    #[zbus(property, name = "PipeWireConsumers")]
    fn pipewire_consumers(&self) -> zbus::Result<u32>;

    /// Number of preview leases held. Revision 5.
    #[zbus(property)]
    fn preview_consumers(&self) -> zbus::Result<u32>;

    /// State of the PipeWire output: `off`, `unsupported`, `connecting`,
    /// `ready`, `streaming` or `failed`. Revision 3.
    #[zbus(property, name = "PipeWireOutput")]
    fn pipewire_output(&self) -> zbus::Result<String>;

    /// The image controls of `ActiveCamera` (camera `""` and no control
    /// when there is none), as [`Self::get_camera_controls`] reports them.
    /// Revision 6.
    #[zbus(property)]
    fn camera_controls(&self) -> zbus::Result<CameraControls>;

    /// Bumped on every change of the published settings tree (a value or
    /// the structure); from 1. Revision 7.
    #[zbus(property)]
    fn settings_revision(&self) -> zbus::Result<u32>;

    /// Bumped on a structural change of the settings tree only (another
    /// camera, a setting added or gone, a type, range, step, default,
    /// option or label changed); from 1. Revision 7.
    #[zbus(property)]
    fn settings_schema_serial(&self) -> zbus::Result<u32>;

    /// Revision of the interface the daemon implements.
    #[zbus(property)]
    fn interface_revision(&self) -> zbus::Result<u32>;
}

impl StatsArgs<'_> {
    /// The signal's arguments as a [`crate::Stats`] value.
    pub fn to_stats(&self) -> crate::Stats {
        crate::Stats {
            fps: self.fps,
            latency_p50_ms: self.latency_p50_ms,
            cpu_percent: self.cpu_percent,
            gpu_percent: self.gpu_percent,
        }
    }
}

impl SettingsChangedArgs<'_> {
    /// The signal's changes in the typed model; a malformed change is
    /// dropped alone ([`SettingChange::list_from_wire`]).
    pub fn to_changes(&self) -> Vec<SettingChange> {
        SettingChange::list_from_wire(&self.changes)
    }
}

impl OmarchyBroadcastProxy<'_> {
    /// `GetSettings` for `camera` (an id of `Cameras`; `""`:
    /// `ActiveCamera`), decoded ([`SettingsTree::from_wire`]). Like every
    /// method call through the proxy, it may start the daemon.
    pub async fn settings_tree(&self, camera: &str) -> Result<SettingsTree, IpcError> {
        let reply = self.get_settings(camera_options(camera)).await?;
        Ok(SettingsTree::from_wire(&reply))
    }

    /// [`Self::settings_tree`] with the `NO_AUTO_START` flag: never starts
    /// the daemon. A daemon that is not running is a [`IpcError::ZBus`] that
    /// [`crate::is_not_running_error`] recognises.
    pub async fn settings_tree_no_autostart(&self, camera: &str) -> Result<SettingsTree, IpcError> {
        let reply: Option<SettingsReply> = self
            .inner()
            .call_with_flags(
                "GetSettings",
                MethodFlags::NO_AUTO_START,
                &(camera_options(camera),),
            )
            .await
            .map_err(IpcError::from)?;
        let reply = reply.ok_or_else(|| {
            IpcError::ZBus(zbus::Error::Failure("GetSettings returned no reply".into()))
        })?;
        Ok(SettingsTree::from_wire(&reply))
    }

    /// `SetSetting(id, value)` on `camera` (`""`: `ActiveCamera`), the
    /// value sent as its [`SettingValue::to_variant`].
    pub async fn set_setting_value(
        &self,
        camera: &str,
        id: &str,
        value: &SettingValue,
    ) -> Result<(), IpcError> {
        let value = value.to_variant();
        self.set_setting(id, Variant::from(value), camera_options(camera))
            .await
    }

    /// `ResetSettings(scope)` on `camera` (`""`: `ActiveCamera`).
    pub async fn reset_settings_scope(
        &self,
        camera: &str,
        scope: &ResetScope,
    ) -> Result<(), IpcError> {
        self.reset_settings(&scope.to_wire(), camera_options(camera))
            .await
    }
}
