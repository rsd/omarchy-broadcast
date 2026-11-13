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
//! decodes the signal. The configuration methods (revision 10) have
//! [`OmarchyBroadcastProxy::config_schema`],
//! [`OmarchyBroadcastProxy::set_config_value`] and
//! [`OmarchyBroadcastProxy::config_files`] (and the `_no_autostart` reads).
//! The media gallery (revision 15) has [`OmarchyBroadcastProxy::media`] and
//! [`OmarchyBroadcastProxy::media_no_autostart`].

use std::collections::HashMap;

use zbus::Variant;
use zbus::proxy::MethodFlags;

use crate::config::{ConfigFiles, ConfigFilesReply, ConfigSchema, ConfigSchemaReply, ConfigValue};
use crate::media::{MediaReply, MediaWire};
use crate::modules::{ModuleList, ModulesReply};
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

    /// The modules (revision 9): `(serial, modules)`, read with
    /// [`OmarchyBroadcastProxy::module_list`].
    fn get_modules(&self) -> Result<ModulesReply, IpcError>;

    fn set_module_enabled(&self, id: &str, enabled: bool) -> Result<(), IpcError>;

    fn get_stage_order(&self) -> Result<Vec<String>, IpcError>;

    fn set_stage_order(&self, order: &[&str]) -> Result<(), IpcError>;

    fn set_backend_profile(&self, profile: &str) -> Result<(), IpcError>;

    fn set_stage_pin(&self, stage: &str, module: &str) -> Result<(), IpcError>;

    /// The stage order in force with the moves the daemon accepts
    /// (revision 10): each stage as `(id, slot, up, down)`, `up` / `down`
    /// whether `SetStageOrder` would accept the order with the stage
    /// swapped with the one before / after it.
    fn get_stage_order_moves(&self) -> Result<Vec<(String, String, bool, bool)>, IpcError>;

    /// The config schema (revision 10): `(serial, groups)`, read with
    /// [`OmarchyBroadcastProxy::config_schema`].
    fn get_config_schema(&self) -> Result<ConfigSchemaReply, IpcError>;

    /// Sets the configuration key `key` in `config.d/50-panel.toml`, checked
    /// and applied (revision 10); send the value with
    /// [`OmarchyBroadcastProxy::set_config_value`]. Refused with
    /// `UnknownSetting`, `Disabled` (read-only, or overridden by a later
    /// file), `InvalidArgument` or `InvalidConfig`.
    fn set_config(&self, key: &str, value: Variant<'_>) -> Result<(), IpcError>;

    /// Removes the key `key` from `config.d/50-panel.toml` (revision 10).
    fn reset_config(&self, key: &str) -> Result<(), IpcError>;

    /// The configuration files in load order (revision 10): `(serial, dir,
    /// files)`, read with [`OmarchyBroadcastProxy::config_files`].
    fn get_config_files(&self) -> Result<ConfigFilesReply, IpcError>;

    /// Turns the microphone on (`true`: the mic host unit
    /// `omarchy-broadcast-mic.service` enabled and started, so it also runs
    /// at the next login) or off (`false`: the previous default microphone
    /// made the default again if the Omarchy Broadcast Mic is the default,
    /// then the unit stopped and disabled). Refused with `Unavailable`,
    /// naming the unit, while an `omarchy-broadcast-mic@<engine>.service`
    /// filter-chain runs, or without a service manager; `Failed` when the
    /// service manager refuses. Revision 13.
    fn set_mic_host(&self, on: bool) -> Result<(), IpcError>;

    /// The media gallery of `kind`, `background` or `away` (revision 15):
    /// `(serial, bundled_dir, user_dir, items)`, read with
    /// [`OmarchyBroadcastProxy::media`]. Fails with `InvalidArgument` for
    /// another kind, `Failed` when the daemon has no gallery (no user data
    /// directory).
    fn get_media(&self, kind: &str) -> Result<MediaWire, IpcError>;

    /// Copies the image at `path` (absolute, read by the daemon) into the
    /// user's gallery of `kind` after checking it, and returns the stored
    /// file's absolute path; a file already in that gallery with the same
    /// bytes is returned instead of being copied again. Refused with
    /// `InvalidArgument` and the reason (an unknown kind, a relative path,
    /// not an accepted image, too large, no decoder, a video); `Failed`
    /// when the gallery cannot be written. Revision 15.
    fn import_media(&self, kind: &str, path: &str) -> Result<String, IpcError>;

    /// Removes the item at `path` from the user's gallery. Removing the
    /// background in force first clears it as an empty
    /// `SetBackgroundImage` does, and is refused with that call's error
    /// when the clear is refused. Refused with `InvalidArgument` for a
    /// bundled item or any path that is not a user item; `Failed` when the
    /// file cannot be removed. Revision 15.
    fn remove_media(&self, path: &str) -> Result<(), IpcError>;

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

    /// About once per second while `MicState` is `active`: blocks over the
    /// processing deadline since the audio worker started, blocks played
    /// raw because the processed one was late, and the voice probability
    /// of the last block (`-1` when unknown). Revision 13.
    #[zbus(signal)]
    fn mic_stats(&self, overruns: u32, misses: u32, voice: f64) -> zbus::Result<()>;

    /// The configuration in force or the config schema changed;
    /// `serial` is the new `ConfigSerial`. Revision 10.
    #[zbus(signal)]
    fn config_changed(&self, serial: u32) -> zbus::Result<()>;

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

    /// Bumped whenever what `GetModules` returns changes. Revision 9.
    #[zbus(property)]
    fn modules_serial(&self) -> zbus::Result<u32>;

    /// Bumped whenever the configuration in force or the config schema
    /// changes (a write, a reload, the modules found). Revision 10.
    #[zbus(property)]
    fn config_serial(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn backend_profile(&self) -> zbus::Result<String>;

    /// What runs other than the user asked, without being an error, empty
    /// if nothing: the selected stages no module provides (the feed
    /// streams without them), and an effect a module provides that cannot
    /// run (the feed black with `on_failure = "black"`, the camera image
    /// with `"none"`). One line of
    /// clauses joined by `; `, for people, not to be parsed. Revision 11.
    #[zbus(property)]
    fn warning(&self) -> zbus::Result<String>;

    /// The microphone: `off`, `passthrough`, `starting`, `active` or
    /// `error` ([`crate::MIC_STATES`]). Revision 13.
    #[zbus(property)]
    fn mic_state(&self) -> zbus::Result<String>;

    /// The applications recording from the microphone; 0 when not known.
    /// Revision 13.
    #[zbus(property)]
    fn mic_consumers(&self) -> zbus::Result<u32>;

    /// The module running each microphone stage, as text; empty when
    /// nothing is processed. Revision 13.
    #[zbus(property)]
    fn mic_provider(&self) -> zbus::Result<String>;

    /// The delay the microphone processing adds, in milliseconds.
    /// Revision 13.
    #[zbus(property)]
    fn mic_latency_ms(&self) -> zbus::Result<f64>;

    /// Why microphone stages that are on cannot run; empty when none.
    /// Revision 13.
    #[zbus(property)]
    fn mic_error(&self) -> zbus::Result<String>;

    /// Bumped whenever what `GetMedia` returns changes through the daemon
    /// (an import that stored a file, a removal); from 1. Revision 15.
    #[zbus(property)]
    fn media_serial(&self) -> zbus::Result<u32>;

    /// The named numeric values the running stages report about the last
    /// frame ([`crate::StageValue`]: auto-frame's `zoom`, for one), `a(ssd)`;
    /// empty when the camera is idle or no stage reports. Its change is
    /// announced when the set changes or a value moves by at least
    /// [`crate::STAGE_VALUE_STEP`], at most every
    /// [`crate::STAGE_VALUES_PERIOD_MS`] milliseconds. Revision 16.
    #[zbus(property)]
    fn stage_values(&self) -> zbus::Result<Vec<crate::StageValue>>;

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

    /// `GetConfigSchema`, read into the model of [`crate::config`].
    pub async fn config_schema(&self) -> Result<ConfigSchema, IpcError> {
        Ok(ConfigSchema::from_wire(&self.get_config_schema().await?))
    }

    /// `GetConfigSchema` with the `NO_AUTO_START` flag: never starts the
    /// daemon.
    pub async fn config_schema_no_autostart(&self) -> Result<ConfigSchema, IpcError> {
        let reply: ConfigSchemaReply = self.call_no_autostart("GetConfigSchema").await?;
        Ok(ConfigSchema::from_wire(&reply))
    }

    /// `SetConfig(key, value)`, the value sent as its
    /// [`ConfigValue::to_variant`].
    pub async fn set_config_value(&self, key: &str, value: &ConfigValue) -> Result<(), IpcError> {
        let value = value.to_variant();
        self.set_config(key, Variant::from(value)).await
    }

    /// `GetConfigFiles`, read into [`ConfigFiles`].
    pub async fn config_files(&self) -> Result<ConfigFiles, IpcError> {
        Ok(ConfigFiles::from_wire(&self.get_config_files().await?))
    }

    /// `GetConfigFiles` with the `NO_AUTO_START` flag: never starts the
    /// daemon.
    pub async fn config_files_no_autostart(&self) -> Result<ConfigFiles, IpcError> {
        let reply: ConfigFilesReply = self.call_no_autostart("GetConfigFiles").await?;
        Ok(ConfigFiles::from_wire(&reply))
    }

    /// The method `member`, without arguments, with the `NO_AUTO_START`
    /// flag.
    async fn call_no_autostart<R>(&self, member: &str) -> Result<R, IpcError>
    where
        R: serde::de::DeserializeOwned + zbus::zvariant::Type,
    {
        let reply: Option<R> = self
            .inner()
            .call_with_flags(member, MethodFlags::NO_AUTO_START, &())
            .await
            .map_err(IpcError::from)?;
        reply.ok_or_else(|| {
            IpcError::ZBus(zbus::Error::Failure(format!("{member} returned no reply")))
        })
    }

    /// `GetModules`, read into the model of [`crate::modules`].
    pub async fn module_list(&self) -> Result<ModuleList, IpcError> {
        Ok(ModuleList::from_wire(&self.get_modules().await?))
    }

    /// `GetModules` with the `NO_AUTO_START` flag: never starts the daemon.
    pub async fn module_list_no_autostart(&self) -> Result<ModuleList, IpcError> {
        let reply: Option<ModulesReply> = self
            .inner()
            .call_with_flags("GetModules", MethodFlags::NO_AUTO_START, &())
            .await
            .map_err(IpcError::from)?;
        let reply = reply.ok_or_else(|| {
            IpcError::ZBus(zbus::Error::Failure("GetModules returned no reply".into()))
        })?;
        Ok(ModuleList::from_wire(&reply))
    }

    /// `GetMedia(kind)`, read into [`MediaReply`].
    pub async fn media(&self, kind: &str) -> Result<MediaReply, IpcError> {
        Ok(MediaReply::from_wire(&self.get_media(kind).await?))
    }

    /// `GetMedia(kind)` with the `NO_AUTO_START` flag: never starts the
    /// daemon.
    pub async fn media_no_autostart(&self, kind: &str) -> Result<MediaReply, IpcError> {
        let reply: Option<MediaWire> = self
            .inner()
            .call_with_flags("GetMedia", MethodFlags::NO_AUTO_START, &(kind,))
            .await
            .map_err(IpcError::from)?;
        let reply = reply.ok_or_else(|| {
            IpcError::ZBus(zbus::Error::Failure("GetMedia returned no reply".into()))
        })?;
        Ok(MediaReply::from_wire(&reply))
    }

    /// `GetStageOrder` with the `NO_AUTO_START` flag: never starts the
    /// daemon.
    pub async fn stage_order_no_autostart(&self) -> Result<Vec<String>, IpcError> {
        let reply: Option<Vec<String>> = self
            .inner()
            .call_with_flags("GetStageOrder", MethodFlags::NO_AUTO_START, &())
            .await
            .map_err(IpcError::from)?;
        reply.ok_or_else(|| {
            IpcError::ZBus(zbus::Error::Failure(
                "GetStageOrder returned no reply".into(),
            ))
        })
    }

    /// `GetStageOrderMoves` with the `NO_AUTO_START` flag: never starts
    /// the daemon. Each stage `(id, slot, up, down)`.
    pub async fn stage_order_moves_no_autostart(
        &self,
    ) -> Result<Vec<(String, String, bool, bool)>, IpcError> {
        self.call_no_autostart("GetStageOrderMoves").await
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
