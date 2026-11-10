//! The client proxy against a mock server implementing
//! `br.com.dias.OmarchyBroadcast1`, over a peer-to-peer D-Bus connection on a
//! Unix socket pair: no session bus needed.
//!
//! The mock mirrors the daemon's contract (ADR-0011) closely enough to
//! exercise every property, method, error mapping and signal; it is not the
//! daemon's implementation.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use futures_util::StreamExt;
use obc_ipc::settings::{VariantDict, WireChange, camera_option};
use obc_ipc::{
    CameraControl, CameraControls, CameraInfo, Condition, ConditionOp, INTERFACE,
    INTERFACE_REVISION, IpcError, OBJECT_PATH, OmarchyBroadcastProxy, ResetScope,
    SETTINGS_REVISION, Setting, SettingChange, SettingKind, SettingValue, SettingsGroup,
    SettingsTree, Snapshot, State, Stats,
};
use zbus::message::Flags;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::proxy::CacheProperties;
use zbus::{Connection, Guid, MessageStream, OwnedVariant, connection, fdo, interface};

const TIMEOUT: Duration = Duration::from_secs(5);

struct Mock {
    enabled: bool,
    state: State,
    error: String,
    cameras: Vec<CameraInfo>,
    active: usize,
    /// Holding the loopback.
    consumers: u32,
    pipewire_consumers: u32,
    effect: String,
    blur_strength: f64,
    background_image: String,
    denoise: bool,
    /// Preview leases (the mock does not key them by caller).
    previews: u32,
    /// The image controls of the first camera (the mock has none for the
    /// other).
    controls: Vec<CameraControl>,
    /// The settings of the first camera (none for the other), with
    /// `SettingsRevision` and `SettingsSchemaSerial`.
    settings: Vec<SettingsGroup>,
    settings_revision: u32,
    settings_serial: u32,
}

/// The setting the mock always answers `RateLimited` for.
const FLOOD: &str = "camera.image.flood";
const AUTO_WB: &str = "camera.white_balance.white_balance_automatic";
const TEMPERATURE: &str = "camera.white_balance.white_balance_temperature";
const WB_REASON: &str = "Turn off automatic white balance to change this";

fn setting(id: &str, label: &str, kind: SettingKind) -> Setting {
    Setting {
        id: id.into(),
        label: label.into(),
        kind,
        scope: "camera".into(),
        stored: false,
        enabled: true,
        disabled_reason: String::new(),
        enabled_when: Vec::new(),
        hint: None,
    }
}

fn mock_settings() -> Vec<SettingsGroup> {
    let mut temperature = setting(
        TEMPERATURE,
        "Temperature",
        SettingKind::Int {
            min: 2800,
            max: 7500,
            step: 1,
            default: 4000,
            value: 4000,
            unit: Some("K".into()),
        },
    );
    temperature.enabled = false;
    temperature.disabled_reason = WB_REASON.into();
    temperature.enabled_when = vec![Condition {
        id: AUTO_WB.into(),
        op: ConditionOp::Eq,
        value: SettingValue::Bool(false),
    }];
    temperature.hint = Some("slider".into());
    vec![
        SettingsGroup {
            id: "image".into(),
            label: "Image".into(),
            settings: vec![
                setting(
                    "camera.image.sharpness",
                    "Sharpness",
                    SettingKind::Int {
                        min: 0,
                        max: 255,
                        step: 1,
                        default: 128,
                        value: 128,
                        unit: None,
                    },
                ),
                setting(
                    FLOOD,
                    "Flood",
                    SettingKind::Bool {
                        default: false,
                        value: false,
                    },
                ),
            ],
        },
        SettingsGroup {
            id: "white_balance".into(),
            label: "White balance".into(),
            settings: vec![
                setting(
                    AUTO_WB,
                    "Automatic white balance",
                    SettingKind::Bool {
                        default: true,
                        value: true,
                    },
                ),
                temperature,
            ],
        },
        SettingsGroup {
            id: "exposure".into(),
            label: "Exposure".into(),
            settings: vec![setting(
                "camera.exposure.auto_exposure",
                "Exposure mode",
                SettingKind::Enum {
                    options: vec![
                        ("manual".into(), "Manual".into()),
                        ("aperture_priority".into(), "Auto".into()),
                    ],
                    default: "aperture_priority".into(),
                    value: "aperture_priority".into(),
                },
            )],
        },
        SettingsGroup {
            id: "focus".into(),
            label: "Focus & zoom".into(),
            settings: vec![setting(
                "camera.focus.pan_reset",
                "Recentre pan",
                SettingKind::Action,
            )],
        },
    ]
}

fn change_of(s: &Setting) -> SettingChange {
    SettingChange {
        id: s.id.clone(),
        value: s.kind.value(),
        stored: s.stored,
        enabled: s.enabled,
        disabled_reason: s.disabled_reason.clone(),
    }
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

impl Mock {
    fn new() -> Self {
        Mock {
            enabled: true,
            state: State::Idle,
            error: String::new(),
            cameras: vec![
                CameraInfo {
                    id: "usb-1234_abcd".into(),
                    name: "First Camera".into(),
                    usb: "1234:abcd".into(),
                },
                CameraInfo {
                    id: "platform-video2".into(),
                    name: "Second Camera".into(),
                    usb: String::new(),
                },
            ],
            active: 0,
            consumers: 1,
            pipewire_consumers: 2,
            effect: "none".into(),
            blur_strength: 0.5,
            background_image: String::new(),
            denoise: false,
            previews: 0,
            controls: vec![sharpness(128, false)],
            settings: mock_settings(),
            settings_revision: 1,
            settings_serial: 1,
        }
    }

    fn settings_tree(&self, at: usize) -> SettingsTree {
        SettingsTree {
            revision: self.settings_revision,
            serial: self.settings_serial,
            camera: self.cameras[at].id.clone(),
            groups: if at == 0 {
                self.settings.clone()
            } else {
                Vec::new()
            },
        }
    }

    fn setting_mut(&mut self, id: &str) -> Option<&mut Setting> {
        self.settings
            .iter_mut()
            .flat_map(|g| g.settings.iter_mut())
            .find(|s| s.id == id)
    }

    /// The white balance dependency, as the daemon computes it: the
    /// temperature is enabled while automatic white balance is off.
    fn apply_dependencies(&mut self) {
        let auto = matches!(
            self.setting_mut(AUTO_WB).map(|s| &s.kind),
            Some(SettingKind::Bool { value: true, .. })
        );
        if let Some(t) = self.setting_mut(TEMPERATURE) {
            t.enabled = !auto;
            t.disabled_reason = if auto {
                WB_REASON.into()
            } else {
                String::new()
            };
        }
    }

    /// Bumps the revision and announces the changes of `ids`.
    async fn publish(&mut self, ids: &[&str], emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        self.settings_revision += 1;
        let changes: Vec<WireChange> = ids
            .iter()
            .filter_map(|id| self.setting_mut(id).map(|s| change_of(s).to_wire()))
            .collect();
        Mock::settings_changed(
            emitter,
            self.settings_revision,
            self.settings_serial,
            changes,
        )
        .await?;
        self.settings_revision_changed(emitter).await
    }

    /// The camera a control method names (`""`: the active one), as the
    /// daemon resolves it.
    fn controls_of(&self, camera: &str) -> Result<usize, IpcError> {
        if camera.is_empty() {
            return Ok(self.active);
        }
        self.cameras
            .iter()
            .position(|c| c.id == camera)
            .ok_or_else(|| IpcError::NoSuchCamera(format!("no camera {camera:?} is present")))
    }

    fn view(&self) -> CameraControls {
        CameraControls {
            camera: self.cameras[self.active].id.clone(),
            controls: if self.active == 0 {
                self.controls.clone()
            } else {
                Vec::new()
            },
        }
    }
}

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
impl Mock {
    async fn set_enabled(
        &mut self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.enabled = enabled;
        self.state = if enabled { State::Idle } else { State::Off };
        self.enabled_changed(&emitter).await?;
        self.state_changed(&emitter).await?;
        Ok(())
    }

    async fn select_camera(
        &mut self,
        selector: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        let want = selector.to_lowercase();
        let found = self.cameras.iter().position(|c| {
            c.id == selector || c.usb == selector || c.name.to_lowercase().contains(&want)
        });
        match found {
            Some(i) => {
                self.active = i;
                self.active_camera_changed(&emitter).await?;
                Ok(())
            }
            None => Err(IpcError::NoSuchCamera(format!(
                "no camera matches {selector:?}"
            ))),
        }
    }

    async fn next_camera(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.active = (self.active + 1) % self.cameras.len();
        self.active_camera_changed(&emitter).await?;
        Ok(())
    }

    async fn reload_config(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.error = "config.toml: line 3: unknown field `mdoe`".into();
        self.error_changed(&emitter).await?;
        Err(IpcError::InvalidConfig(self.error.clone()))
    }

    async fn set_effect(
        &mut self,
        effect: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        match effect {
            "replace" if self.background_image.is_empty() => Err(IpcError::InvalidArgument(
                "replace needs a background image".into(),
            )),
            "none" | "blur" | "replace" => {
                self.effect = effect.into();
                self.effect_changed(&emitter).await?;
                Ok(())
            }
            other => Err(IpcError::InvalidArgument(format!(
                "unknown effect {other:?}"
            ))),
        }
    }

    async fn set_blur_strength(
        &mut self,
        strength: f64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        if !(0.0..=1.0).contains(&strength) {
            return Err(IpcError::InvalidArgument(format!(
                "blur strength {strength} is outside 0 to 1"
            )));
        }
        self.blur_strength = strength;
        self.blur_strength_changed(&emitter).await?;
        Ok(())
    }

    async fn set_background_image(
        &mut self,
        path: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        if !path.starts_with('/') {
            return Err(IpcError::InvalidArgument(format!(
                "{path:?} is not an absolute path"
            )));
        }
        self.background_image = path.into();
        self.background_image_changed(&emitter).await?;
        Ok(())
    }

    async fn set_denoise(
        &mut self,
        denoise: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.denoise = denoise;
        self.denoise_changed(&emitter).await?;
        Ok(())
    }

    async fn acquire_preview(
        &mut self,
        fps: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<String, IpcError> {
        if !obc_ipc::PREVIEW_FPS.contains(&fps) {
            return Err(IpcError::InvalidArgument(format!(
                "preview frame rate {fps} is not 15 or 30"
            )));
        }
        self.previews = 1;
        self.preview_consumers_changed(&emitter).await?;
        Ok("/run/user/1000/omarchy-broadcast/preview.jpg".into())
    }

    async fn release_preview(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.previews = 0;
        self.preview_consumers_changed(&emitter).await?;
        Ok(())
    }

    async fn get_camera_controls(&self, camera: String) -> Result<Vec<CameraControl>, IpcError> {
        Ok(if self.controls_of(&camera)? == 0 {
            self.controls.clone()
        } else {
            Vec::new()
        })
    }

    async fn set_camera_control(
        &mut self,
        camera: String,
        name: String,
        value: i32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        let at = self.controls_of(&camera)?;
        let Some(c) = self.controls.iter_mut().find(|c| c.name == name && at == 0) else {
            return Err(IpcError::InvalidArgument(format!(
                "the camera does not expose {name:?}"
            )));
        };
        if !(c.min..=c.max).contains(&value) {
            return Err(IpcError::InvalidArgument(format!(
                "{name} {value} is outside its range {}..={}",
                c.min, c.max
            )));
        }
        c.value = value;
        c.stored = true;
        self.camera_controls_changed(&emitter).await?;
        Ok(())
    }

    async fn reset_camera_controls(
        &mut self,
        camera: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.controls_of(&camera)?;
        for c in &mut self.controls {
            c.value = c.default;
            c.stored = false;
        }
        self.camera_controls_changed(&emitter).await?;
        Ok(())
    }

    // The daemon's four named out args, so introspection matches it: the
    // tuple is spelled out (the macro does not see through the
    // `SettingsReply` alias, and would advertise one struct).
    #[zbus(out_args("revision", "serial", "camera", "groups"))]
    async fn get_settings(
        &self,
        options: VariantDict,
    ) -> Result<(u32, u32, String, Vec<VariantDict>), IpcError> {
        let at = self.controls_of(&camera_option(&options)?)?;
        Ok(self.settings_tree(at).to_wire())
    }

    async fn set_setting(
        &mut self,
        id: String,
        value: OwnedVariant,
        options: VariantDict,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        let at = self.controls_of(&camera_option(&options)?)?;
        let unknown = || IpcError::UnknownSetting(format!("no setting {id:?}"));
        if at != 0 {
            return Err(unknown());
        }
        let value = SettingValue::from_variant(&value);
        let Some(s) = self.setting_mut(&id) else {
            return Err(unknown());
        };
        if !s.enabled {
            return Err(IpcError::Disabled(s.disabled_reason.clone()));
        }
        let valid = match (&s.kind, &value) {
            (SettingKind::Int { min, max, .. }, Some(SettingValue::Int(v))) => {
                (*min..=*max).contains(v)
            }
            (SettingKind::Bool { .. }, Some(SettingValue::Bool(_))) => true,
            (SettingKind::Enum { options, .. }, Some(SettingValue::Str(v))) => {
                options.iter().any(|(o, _)| o == v)
            }
            (SettingKind::Action, Some(SettingValue::Bool(true))) => true,
            _ => false,
        };
        if !valid {
            return Err(IpcError::InvalidArgument(format!(
                "{id}: not a value it takes"
            )));
        }
        // Checked last, and a refused write stores nothing.
        if id == FLOOD {
            return Err(IpcError::RateLimited("too many writes".into()));
        }
        match (&mut s.kind, value) {
            (SettingKind::Int { value, .. }, Some(SettingValue::Int(v))) => *value = v,
            (SettingKind::Bool { value, .. }, Some(SettingValue::Bool(v))) => *value = v,
            (SettingKind::Enum { value, .. }, Some(SettingValue::Str(v))) => *value = v,
            _ => {}
        }
        if !matches!(s.kind, SettingKind::Action) {
            s.stored = true;
        }
        self.apply_dependencies();
        // A dependency flip travels in the same signal as its cause.
        let mut ids = vec![id.as_str()];
        if id == AUTO_WB {
            ids.push(TEMPERATURE);
        }
        self.publish(&ids, &emitter).await?;
        Ok(())
    }

    async fn reset_settings(
        &mut self,
        scope: String,
        options: VariantDict,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        self.controls_of(&camera_option(&options)?)?;
        let Some(scope) = ResetScope::parse(&scope) else {
            return Err(IpcError::InvalidArgument(format!("no scope {scope:?}")));
        };
        let mut ids = Vec::new();
        for g in &mut self.settings {
            for s in &mut g.settings {
                let hit = match &scope {
                    ResetScope::Camera => true,
                    ResetScope::Group(id) => g.id == *id,
                    ResetScope::Setting(id) => s.id == *id,
                };
                if hit {
                    match &mut s.kind {
                        SettingKind::Int { default, value, .. } => *value = *default,
                        SettingKind::Bool { default, value } => *value = *default,
                        SettingKind::Enum { default, value, .. } => *value = default.clone(),
                        SettingKind::Action | SettingKind::Unknown(_) => {}
                    }
                    s.stored = false;
                    ids.push(s.id.clone());
                }
            }
        }
        self.apply_dependencies();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.publish(&ids, &emitter).await?;
        Ok(())
    }

    #[zbus(signal)]
    async fn settings_changed(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        serial: u32,
        changes: Vec<WireChange>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn stats(
        emitter: &SignalEmitter<'_>,
        fps: f64,
        latency_p50_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn preview_frame(emitter: &SignalEmitter<'_>, seq: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> State {
        self.state
    }

    #[zbus(property)]
    fn error(&self) -> String {
        self.error.clone()
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.enabled
    }

    #[zbus(property)]
    fn cameras(&self) -> Vec<CameraInfo> {
        self.cameras.clone()
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        self.cameras[self.active].id.clone()
    }

    #[zbus(property)]
    fn output_mode(&self) -> String {
        "NV12 1920x1080@30".into()
    }

    #[zbus(property)]
    fn effect(&self) -> String {
        self.effect.clone()
    }

    #[zbus(property)]
    fn blur_strength(&self) -> f64 {
        self.blur_strength
    }

    #[zbus(property)]
    fn background_image(&self) -> String {
        self.background_image.clone()
    }

    #[zbus(property)]
    fn denoise(&self) -> bool {
        self.denoise
    }

    #[zbus(property)]
    fn consumers(&self) -> u32 {
        self.consumers + self.pipewire_consumers + self.previews
    }

    #[zbus(property)]
    fn preview_consumers(&self) -> u32 {
        self.previews
    }

    #[zbus(property)]
    fn loopback_consumers(&self) -> u32 {
        self.consumers
    }

    #[zbus(property, name = "PipeWireConsumers")]
    fn pipewire_consumers(&self) -> u32 {
        self.pipewire_consumers
    }

    #[zbus(property, name = "PipeWireOutput")]
    fn pipewire_output(&self) -> String {
        "ready".into()
    }

    #[zbus(property)]
    fn camera_controls(&self) -> CameraControls {
        self.view()
    }

    #[zbus(property)]
    fn settings_revision(&self) -> u32 {
        self.settings_revision
    }

    #[zbus(property)]
    fn settings_schema_serial(&self) -> u32 {
        self.settings_serial
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        INTERFACE_REVISION
    }
}

/// A daemon of revision 6: the camera controls, no settings channel.
struct Rev6;

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
impl Rev6 {
    async fn get_camera_controls(&self, _camera: String) -> Result<Vec<CameraControl>, IpcError> {
        Ok(vec![sharpness(128, false)])
    }

    #[zbus(property)]
    fn state(&self) -> State {
        State::Idle
    }

    #[zbus(property)]
    fn error(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn cameras(&self) -> Vec<CameraInfo> {
        Vec::new()
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn output_mode(&self) -> String {
        "NV12 1920x1080@30".into()
    }

    #[zbus(property)]
    fn effect(&self) -> String {
        "none".into()
    }

    #[zbus(property)]
    fn consumers(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn camera_controls(&self) -> CameraControls {
        CameraControls::default()
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        6
    }
}

/// A daemon of revision 1: the properties before the effect controls, and
/// no effect methods.
struct Rev1;

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
impl Rev1 {
    #[zbus(property)]
    fn state(&self) -> State {
        State::Idle
    }

    #[zbus(property)]
    fn error(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn cameras(&self) -> Vec<CameraInfo> {
        Vec::new()
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn output_mode(&self) -> String {
        "NV12 1920x1080@30".into()
    }

    #[zbus(property)]
    fn effect(&self) -> String {
        "none".into()
    }

    #[zbus(property)]
    fn consumers(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        1
    }
}

/// A server connection serving [`Mock`] and a client connection, joined by a
/// socket pair.
async fn pair() -> (Connection, Connection) {
    pair_serving(Mock::new()).await
}

/// A server connection serving `iface` and a client connection, joined by a
/// socket pair.
async fn pair_serving(iface: impl zbus::object_server::Interface) -> (Connection, Connection) {
    let (s0, s1) = UnixStream::pair().unwrap();
    let server = connection::Builder::unix_stream(s0)
        .server(Guid::generate())
        .p2p()
        .serve_at(OBJECT_PATH, iface)
        .build();
    let client = connection::Builder::unix_stream(s1).p2p().build();
    tokio::try_join!(server, client).unwrap()
}

async fn within<F: std::future::Future>(what: &str, f: F) -> F::Output {
    tokio::time::timeout(TIMEOUT, f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_reads_every_property() {
    let (_server, client) = pair().await;
    let proxy = OmarchyBroadcastProxy::new(&client).await.unwrap();

    let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
    assert_eq!(
        snap,
        Snapshot {
            state: State::Idle,
            error: String::new(),
            enabled: true,
            cameras: Mock::new().cameras,
            active_camera: "usb-1234_abcd".into(),
            output_mode: "NV12 1920x1080@30".into(),
            effect: "none".into(),
            blur_strength: 0.5,
            background_image: String::new(),
            denoise: false,
            consumers: 3,
            loopback_consumers: 1,
            pipewire_consumers: 2,
            preview_consumers: 0,
            pipewire_output: "ready".into(),
            camera_controls: Some(CameraControls {
                camera: "usb-1234_abcd".into(),
                controls: vec![sharpness(128, false)],
            }),
            settings_revision: Some(1),
            settings_schema_serial: Some(1),
            interface_revision: INTERFACE_REVISION,
        }
    );
    assert_eq!(INTERFACE_REVISION, 7);
    assert_eq!(proxy.inner().interface().as_str(), INTERFACE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revision_1_daemon_reads_with_defaults_for_the_newer_properties() {
    let (_server, client) = pair_serving(Rev1).await;
    for proxy in [
        OmarchyBroadcastProxy::new(&client).await.unwrap(),
        OmarchyBroadcastProxy::builder(&client)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap(),
    ] {
        let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
        assert_eq!(snap.interface_revision, 1);
        assert_eq!(snap.state, State::Idle);
        assert_eq!(snap.effect, "none");
        assert_eq!(snap.blur_strength, Snapshot::empty().blur_strength);
        assert_eq!(snap.background_image, "");
        assert!(!snap.denoise);
        // Before revision 3, Consumers counts the loopback only.
        assert_eq!(snap.consumers, 2);
        assert_eq!(snap.loopback_consumers, 2);
        assert_eq!(snap.pipewire_consumers, 0);
        assert_eq!(snap.pipewire_output, "off");
        assert_eq!(snap.preview_consumers, 0);
        assert_eq!(snap.camera_controls, None);
        assert_eq!(snap.settings_revision, None);
        assert_eq!(snap.settings_schema_serial, None);
    }
    let no_autostart = within("snapshot", Snapshot::fetch_no_autostart(&client))
        .await
        .unwrap()
        .unwrap();
    let proxy = OmarchyBroadcastProxy::new(&client).await.unwrap();
    assert_eq!(
        no_autostart,
        within("snapshot", Snapshot::fetch(&proxy)).await.unwrap()
    );
    // The effect methods are absent: a D-Bus error, not a daemon reply.
    match within("SetEffect", proxy.set_effect("blur")).await {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    match within("SetDenoise", proxy.set_denoise(true)).await {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    match within("AcquirePreview", proxy.acquire_preview(15)).await {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    match within("GetCameraControls", proxy.get_camera_controls("")).await {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    match within(
        "SetCameraControl",
        proxy.set_camera_control("", "sharpness", 1),
    )
    .await
    {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    match within("ResetCameraControls", proxy.reset_camera_controls("")).await {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
}

/// Waits for the next `Properties.GetAll` that `server` receives and returns
/// whether it carried `NO_AUTO_START`.
async fn next_get_all(incoming: &mut MessageStream) -> bool {
    loop {
        let msg = within("GetAll at the server", incoming.next())
            .await
            .expect("server message stream ended")
            .unwrap();
        let header = msg.header();
        if header.message_type() == zbus::message::Type::MethodCall
            && header.member().is_some_and(|m| m.as_str() == "GetAll")
        {
            return header.primary().flags().contains(Flags::NO_AUTO_START);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_no_autostart_read_sees_every_property_and_carries_the_flag() {
    let (server, client) = pair().await;
    let mut incoming = MessageStream::from(&server);

    let snap = within("snapshot", Snapshot::fetch_no_autostart(&client))
        .await
        .unwrap()
        .expect("the mock serves the interface");
    assert!(
        next_get_all(&mut incoming).await,
        "the read must not allow D-Bus activation"
    );
    let via_proxy = within(
        "proxy snapshot",
        Snapshot::fetch(&OmarchyBroadcastProxy::new(&client).await.unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(snap, via_proxy);
    assert_eq!(snap.state, State::Idle);
    assert_eq!(snap.cameras, Mock::new().cameras);
    assert_eq!(snap.interface_revision, INTERFACE_REVISION);

    // The contrast the crate documentation warns about: the default
    // (caching) proxy fills its cache with a GetAll that allows activation.
    assert!(
        !next_get_all(&mut incoming).await,
        "zbus's cache fill is expected to allow activation"
    );

    // A change, applied from PropertiesChanged-style values, matches a fresh
    // read.
    let proxy = OmarchyBroadcastProxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    within("SetEnabled", proxy.set_enabled(false))
        .await
        .unwrap();
    let mut applied = snap.clone();
    applied
        .apply_property("Enabled", &zbus::Variant::from(false))
        .unwrap();
    applied
        .apply_property("State", &zbus::Variant::from("off"))
        .unwrap();
    let fresh = within("snapshot", Snapshot::fetch_no_autostart(&client))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(applied, fresh);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_no_autostart_read_reports_a_missing_object_as_an_error() {
    // A peer that serves nothing at the object path: an error, not "not
    // running" (only the bus can say a name has no owner).
    let (s0, s1) = UnixStream::pair().unwrap();
    // The object server answers for the paths it does not serve. It serves
    // something elsewhere so that `build()` returns only once its dispatch
    // task is subscribed: started lazily (`object_server()`), a request that
    // arrived first was never answered and the read timed out.
    let server = connection::Builder::unix_stream(s0)
        .server(Guid::generate())
        .p2p()
        .serve_at("/unrelated", Rev1)
        .build();
    let client = connection::Builder::unix_stream(s1).p2p().build();
    let (_server, client) = tokio::try_join!(server, client).unwrap();
    let got = within("snapshot", Snapshot::fetch_no_autostart(&client)).await;
    assert!(got.is_err(), "{got:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_calls_every_method_and_sees_changes() {
    let (_server, client) = pair().await;
    // A caching proxy learns of changes from PropertiesChanged, which may
    // arrive after the method reply; an uncached one reads the server's
    // value at the time of the call.
    let proxy = OmarchyBroadcastProxy::new(&client).await.unwrap();
    let uncached = OmarchyBroadcastProxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    let props = fdo::PropertiesProxy::builder(&client)
        .destination(obc_ipc::BUS_NAME)
        .path(OBJECT_PATH)
        .build()
        .await
        .unwrap();
    let mut changed = props.receive_properties_changed().await.unwrap();
    let mut enabled = proxy.receive_enabled_changed().await;

    // SetEnabled(false): Enabled and State change, announced by
    // PropertiesChanged and reflected in the proxy's cache.
    within("SetEnabled", proxy.set_enabled(false))
        .await
        .unwrap();
    assert!(!uncached.enabled().await.unwrap());
    assert_eq!(uncached.state().await.unwrap(), State::Off);
    let mut seen = Vec::new();
    while !(seen.iter().any(|k| k == "Enabled") && seen.iter().any(|k| k == "State")) {
        let signal = within("PropertiesChanged", changed.next())
            .await
            .expect("PropertiesChanged stream ended");
        let args = signal.args().unwrap();
        assert_eq!(args.interface_name().as_str(), INTERFACE);
        seen.extend(args.changed_properties().keys().map(|k| k.to_string()));
    }
    loop {
        let change = within("Enabled change", enabled.next())
            .await
            .expect("Enabled stream ended");
        if !change.get().await.unwrap() {
            break;
        }
    }
    assert!(!proxy.enabled().await.unwrap());
    within("SetEnabled", proxy.set_enabled(true)).await.unwrap();
    assert_eq!(uncached.state().await.unwrap(), State::Idle);

    // SelectCamera by name substring and by vid:pid; NextCamera wraps.
    within("SelectCamera", proxy.select_camera("second"))
        .await
        .unwrap();
    assert_eq!(uncached.active_camera().await.unwrap(), "platform-video2");
    within("NextCamera", proxy.next_camera()).await.unwrap();
    assert_eq!(uncached.active_camera().await.unwrap(), "usb-1234_abcd");
    within("NextCamera", proxy.next_camera()).await.unwrap();
    within("SelectCamera", proxy.select_camera("1234:abcd"))
        .await
        .unwrap();
    assert_eq!(uncached.active_camera().await.unwrap(), "usb-1234_abcd");

    // The effect controls: options first, then the effect.
    within("SetBlurStrength", proxy.set_blur_strength(0.8))
        .await
        .unwrap();
    assert_eq!(uncached.blur_strength().await.unwrap(), 0.8);
    within("SetEffect", proxy.set_effect("blur")).await.unwrap();
    assert_eq!(uncached.effect().await.unwrap(), "blur");
    within(
        "SetBackgroundImage",
        proxy.set_background_image("/tmp/bg.png"),
    )
    .await
    .unwrap();
    assert_eq!(uncached.background_image().await.unwrap(), "/tmp/bg.png");
    within("SetEffect", proxy.set_effect("replace"))
        .await
        .unwrap();
    let snap = within("snapshot", Snapshot::fetch(&uncached))
        .await
        .unwrap();
    assert_eq!(
        (
            snap.effect.as_str(),
            snap.blur_strength,
            snap.background_image.as_str()
        ),
        ("replace", 0.8, "/tmp/bg.png")
    );
    within("SetDenoise", proxy.set_denoise(true)).await.unwrap();
    assert!(uncached.denoise().await.unwrap());

    // The changes arrive as PropertiesChanged and apply to a snapshot.
    let mut seen = Vec::new();
    let mut applied = Snapshot::empty();
    while !["BlurStrength", "BackgroundImage", "Effect", "Denoise"]
        .iter()
        .all(|k| seen.iter().any(|s| s == k))
    {
        let signal = within("PropertiesChanged", changed.next())
            .await
            .expect("PropertiesChanged stream ended");
        let args = signal.args().unwrap();
        for (k, v) in args.changed_properties() {
            applied.apply_property(k, v).unwrap();
            seen.push(k.to_string());
        }
    }
    assert_eq!(applied.blur_strength, 0.8);
    assert_eq!(applied.background_image, "/tmp/bg.png");
    assert!(applied.denoise);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_map_back_by_name() {
    let (_server, client) = pair().await;
    let proxy = OmarchyBroadcastProxy::new(&client).await.unwrap();

    match within("SelectCamera", proxy.select_camera("nope")).await {
        Err(IpcError::NoSuchCamera(msg)) => assert!(msg.contains("nope"), "{msg}"),
        other => panic!("expected NoSuchCamera, got {other:?}"),
    }
    match within("ReloadConfig", proxy.reload_config()).await {
        Err(e @ IpcError::InvalidConfig(_)) => {
            assert!(e.message().contains("unknown field"), "{e}");
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmarchyBroadcast1.Error.InvalidConfig"
            );
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
    for (what, reply) in [
        (
            "SetEffect",
            within("SetEffect", proxy.set_effect("sparkle")).await,
        ),
        (
            "SetEffect replace",
            within("SetEffect", proxy.set_effect("replace")).await,
        ),
        (
            "SetBlurStrength",
            within("SetBlurStrength", proxy.set_blur_strength(1.5)).await,
        ),
        (
            "SetBackgroundImage",
            within("SetBackgroundImage", proxy.set_background_image("bg.png")).await,
        ),
    ] {
        match reply {
            Err(e @ IpcError::InvalidArgument(_)) => {
                assert!(!e.message().is_empty(), "{what}");
                assert_eq!(
                    zbus::DBusError::name(&e).as_str(),
                    "br.com.dias.OmarchyBroadcast1.Error.InvalidArgument",
                    "{what}"
                );
            }
            other => panic!("{what}: expected InvalidArgument, got {other:?}"),
        }
    }
    // A failed reload reports its reason through Error as well.
    let mut error = proxy.receive_error_changed().await;
    loop {
        let change = within("Error change", error.next())
            .await
            .expect("Error stream ended");
        if !change.get().await.unwrap().is_empty() {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_signal_reaches_the_client() {
    let (server, client) = pair().await;
    let proxy = OmarchyBroadcastProxy::new(&client).await.unwrap();
    let mut stats = proxy.receive_stats().await.unwrap();

    let iface: InterfaceRef<Mock> = server.object_server().interface(OBJECT_PATH).await.unwrap();
    Mock::stats(
        iface.signal_emitter(),
        30.0,
        12.5,
        3.0,
        Stats::NOT_MEASURABLE,
    )
    .await
    .unwrap();

    let signal = within("Stats", stats.next())
        .await
        .expect("Stats stream ended");
    let got = signal.args().unwrap().to_stats();
    assert_eq!(
        got,
        Stats {
            fps: 30.0,
            latency_p50_ms: 12.5,
            cpu_percent: 3.0,
            gpu_percent: -1.0,
        }
    );
    assert!(!Stats::is_measured(got.gpu_percent));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_preview_methods_property_and_signal() {
    let (server, client) = pair().await;
    let proxy = OmarchyBroadcastProxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    let mut frames = proxy.receive_preview_frame().await.unwrap();

    let path = within("AcquirePreview", proxy.acquire_preview(15))
        .await
        .unwrap();
    assert_eq!(path, "/run/user/1000/omarchy-broadcast/preview.jpg");
    assert_eq!(proxy.preview_consumers().await.unwrap(), 1);
    assert_eq!(proxy.consumers().await.unwrap(), 4);
    let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
    assert_eq!(snap.preview_consumers, 1);
    for bad in [0, 10, 16, 60, u32::MAX] {
        match within("AcquirePreview", proxy.acquire_preview(bad)).await {
            Err(e @ IpcError::InvalidArgument(_)) => assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmarchyBroadcast1.Error.InvalidArgument"
            ),
            other => panic!("{bad}: expected InvalidArgument, got {other:?}"),
        }
    }

    let iface: InterfaceRef<Mock> = server.object_server().interface(OBJECT_PATH).await.unwrap();
    for seq in [1, 2] {
        Mock::preview_frame(iface.signal_emitter(), seq)
            .await
            .unwrap();
    }
    for want in [1, 2] {
        let signal = within("PreviewFrame", frames.next())
            .await
            .expect("PreviewFrame stream ended");
        assert_eq!(signal.args().unwrap().seq, want);
    }

    within("ReleasePreview", proxy.release_preview())
        .await
        .unwrap();
    assert_eq!(proxy.preview_consumers().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_camera_control_methods_and_property() {
    let (_server, client) = pair().await;
    let proxy = OmarchyBroadcastProxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    let cached = OmarchyBroadcastProxy::new(&client).await.unwrap();
    let mut changes = cached.receive_camera_controls_changed().await;

    let got = within("GetCameraControls", proxy.get_camera_controls(""))
        .await
        .unwrap();
    assert_eq!(got, [sharpness(128, false)]);
    let by_id = within(
        "GetCameraControls",
        proxy.get_camera_controls("usb-1234_abcd"),
    )
    .await
    .unwrap();
    assert_eq!(by_id, got);
    assert!(
        within(
            "GetCameraControls",
            proxy.get_camera_controls("platform-video2")
        )
        .await
        .unwrap()
        .is_empty()
    );

    within(
        "SetCameraControl",
        proxy.set_camera_control("", "sharpness", 200),
    )
    .await
    .unwrap();
    let view = proxy.camera_controls().await.unwrap();
    assert_eq!(
        view,
        CameraControls {
            camera: "usb-1234_abcd".into(),
            controls: vec![sharpness(200, true)],
        }
    );
    // The change is announced, and a cached proxy follows it.
    let changed = within("CameraControls changed", changes.next())
        .await
        .expect("change stream");
    assert_eq!(changed.get().await.unwrap(), view);
    let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
    assert_eq!(snap.camera_controls, Some(view));

    // The error replies map back to their variants.
    match within(
        "SetCameraControl",
        proxy.set_camera_control("", "sharpness", 256),
    )
    .await
    {
        Err(e @ IpcError::InvalidArgument(_)) => {
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmarchyBroadcast1.Error.InvalidArgument"
            );
            assert!(e.message().contains("0..=255"), "{e:?}");
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    match within("ResetCameraControls", proxy.reset_camera_controls("nope")).await {
        Err(e @ IpcError::NoSuchCamera(_)) => assert_eq!(
            zbus::DBusError::name(&e).as_str(),
            "br.com.dias.OmarchyBroadcast1.Error.NoSuchCamera"
        ),
        other => panic!("expected NoSuchCamera, got {other:?}"),
    }

    within("ResetCameraControls", proxy.reset_camera_controls(""))
        .await
        .unwrap();
    assert_eq!(
        proxy.camera_controls().await.unwrap().controls,
        [sharpness(128, false)]
    );
}

/// Waits for the next method call `member` that `server` receives and
/// returns whether it carried `NO_AUTO_START`.
async fn next_call(incoming: &mut MessageStream, member: &str) -> bool {
    loop {
        let msg = within("a call at the server", incoming.next())
            .await
            .expect("server message stream ended")
            .unwrap();
        let header = msg.header();
        if header.message_type() == zbus::message::Type::MethodCall
            && header.member().is_some_and(|m| m.as_str() == member)
        {
            return header.primary().flags().contains(Flags::NO_AUTO_START);
        }
    }
}

fn expect_error(what: &str, got: Result<(), IpcError>, name: &str) -> String {
    match got {
        Err(IpcError::ZBus(e)) => panic!("{what}: expected {name}, got a transport error {e}"),
        Err(e) => {
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                format!("br.com.dias.OmarchyBroadcast1.Error.{name}"),
                "{what}: {e:?}"
            );
            e.message()
        }
        Ok(()) => panic!("{what}: expected {name}, got Ok"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_settings_properties_and_methods_with_the_typed_model() {
    let (server, client) = pair().await;
    let mut incoming = MessageStream::from(&server);
    let proxy = OmarchyBroadcastProxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    assert_eq!(proxy.settings_revision().await.unwrap(), 1);
    assert_eq!(proxy.settings_schema_serial().await.unwrap(), 1);
    let snap = within("snapshot", Snapshot::fetch_no_autostart(&client))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (snap.settings_revision, snap.settings_schema_serial),
        (Some(1), Some(1))
    );
    assert!(snap.interface_revision >= SETTINGS_REVISION);

    // The tree, decoded, is the mock's own model; by id too.
    let tree = within("GetSettings", proxy.settings_tree(""))
        .await
        .unwrap();
    assert!(
        !next_call(&mut incoming, "GetSettings").await,
        "a proxy call allows activation"
    );
    assert_eq!(tree, Mock::new().settings_tree(0));
    assert_eq!(tree.camera, "usb-1234_abcd");
    let by_id = within("GetSettings", proxy.settings_tree("usb-1234_abcd"))
        .await
        .unwrap();
    assert_eq!(by_id, tree);
    let other = within("GetSettings", proxy.settings_tree("platform-video2"))
        .await
        .unwrap();
    assert_eq!(
        (other.camera.as_str(), other.groups.len()),
        ("platform-video2", 0)
    );
    for _ in 0..2 {
        assert!(!next_call(&mut incoming, "GetSettings").await);
    }
    // The read that never starts the daemon carries the flag.
    let quiet = within(
        "GetSettings no-autostart",
        proxy.settings_tree_no_autostart(""),
    )
    .await
    .unwrap();
    assert_eq!(quiet, tree);
    assert!(
        next_call(&mut incoming, "GetSettings").await,
        "the read must not allow D-Bus activation"
    );

    // SetSetting through the typed wrapper, then ResetSettings.
    within(
        "SetSetting",
        proxy.set_setting_value("", "camera.image.sharpness", &SettingValue::Int(200)),
    )
    .await
    .unwrap();
    within(
        "SetSetting enum",
        proxy.set_setting_value(
            "usb-1234_abcd",
            "camera.exposure.auto_exposure",
            &SettingValue::Str("manual".into()),
        ),
    )
    .await
    .unwrap();
    within(
        "SetSetting action",
        proxy.set_setting_value("", "camera.focus.pan_reset", &SettingValue::Bool(true)),
    )
    .await
    .unwrap();
    // An int of another D-Bus type is accepted (the mock reads any).
    within(
        "SetSetting u32",
        proxy.set_setting(
            "camera.image.sharpness",
            zbus::Variant::from(201u32),
            HashMap::new(),
        ),
    )
    .await
    .unwrap();
    let tree = within("GetSettings", proxy.settings_tree(""))
        .await
        .unwrap();
    let sharp = tree.find("camera.image.sharpness").unwrap();
    assert_eq!(sharp.kind.value(), Some(SettingValue::Int(201)));
    assert!(sharp.stored);
    assert_eq!(
        tree.find("camera.exposure.auto_exposure")
            .unwrap()
            .kind
            .value(),
        Some(SettingValue::Str("manual".into()))
    );
    assert_eq!(tree.revision, 5);
    assert_eq!(proxy.settings_revision().await.unwrap(), 5);

    within(
        "ResetSettings",
        proxy.reset_settings_scope("", &ResetScope::Group("image".into())),
    )
    .await
    .unwrap();
    let tree = within("GetSettings", proxy.settings_tree(""))
        .await
        .unwrap();
    let sharp = tree.find("camera.image.sharpness").unwrap();
    assert_eq!(
        (sharp.kind.value(), sharp.stored),
        (Some(SettingValue::Int(128)), false)
    );
    // The other groups are left alone.
    assert!(tree.find("camera.exposure.auto_exposure").unwrap().stored);
    within(
        "ResetSettings camera",
        proxy.reset_settings_scope("usb-1234_abcd", &ResetScope::Camera),
    )
    .await
    .unwrap();
    let tree = within("GetSettings", proxy.settings_tree(""))
        .await
        .unwrap();
    assert_eq!(tree.groups, Mock::new().settings);
}

/// The `<method name="NAME">` element of introspection data.
fn method_element<'a>(xml: &'a str, name: &str) -> &'a str {
    let start = xml
        .find(&format!("<method name=\"{name}\">"))
        .unwrap_or_else(|| panic!("no method {name} in {xml}"));
    let len = xml[start..].find("</method>").expect("a closed element");
    &xml[start..start + len]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_settings_introspects_with_the_daemons_four_out_args() {
    let (_server, client) = pair().await;
    let xml = within(
        "Introspect",
        fdo::IntrospectableProxy::builder(&client)
            .destination(obc_ipc::BUS_NAME)
            .path(OBJECT_PATH)
            .build(),
    )
    .await
    .unwrap()
    .introspect()
    .await
    .unwrap();
    let get = method_element(&xml, "GetSettings");
    let outs: Vec<&str> = get
        .split("<arg ")
        .filter(|a| a.contains("direction=\"out\""))
        .collect();
    assert_eq!(outs.len(), 4, "{get}");
    for (arg, (name, ty)) in outs.iter().zip([
        ("revision", "u"),
        ("serial", "u"),
        ("camera", "s"),
        ("groups", "aa{sv}"),
    ]) {
        assert!(
            arg.contains(&format!("name=\"{name}\"")) && arg.contains(&format!("type=\"{ty}\"")),
            "{name} {ty} not in {arg}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_settings_errors_map_back_by_name() {
    let (_server, client) = pair().await;
    let proxy = OmarchyBroadcastProxy::new(&client).await.unwrap();
    let set = |id: &'static str, v: SettingValue| {
        let proxy = proxy.clone();
        async move { proxy.set_setting_value("", id, &v).await }
    };

    let m = expect_error(
        "unknown id",
        within("SetSetting", set("camera.image.nope", SettingValue::Int(1))).await,
        "UnknownSetting",
    );
    assert!(m.contains("camera.image.nope"), "{m}");
    let m = expect_error(
        "disabled",
        within("SetSetting", set(TEMPERATURE, SettingValue::Int(5000))).await,
        "Disabled",
    );
    assert_eq!(m, WB_REASON);
    expect_error(
        "out of range",
        within(
            "SetSetting",
            set("camera.image.sharpness", SettingValue::Int(256)),
        )
        .await,
        "InvalidArgument",
    );
    expect_error(
        "wrong type",
        within(
            "SetSetting",
            set("camera.image.sharpness", SettingValue::Bool(true)),
        )
        .await,
        "InvalidArgument",
    );
    expect_error(
        "rate limited",
        within("SetSetting", set(FLOOD, SettingValue::Bool(true))).await,
        "RateLimited",
    );
    expect_error(
        "no such camera",
        within("GetSettings", async {
            proxy.settings_tree("nope").await.map(|_| ())
        })
        .await,
        "NoSuchCamera",
    );
    expect_error(
        "bad scope",
        within(
            "ResetSettings",
            proxy.reset_settings("global", HashMap::new()),
        )
        .await,
        "InvalidArgument",
    );
    let mut wrong = HashMap::new();
    wrong.insert("camera", zbus::Variant::from(3u32));
    expect_error(
        "camera option not a string",
        within("GetSettings", async {
            proxy.get_settings(wrong).await.map(|_| ())
        })
        .await,
        "InvalidArgument",
    );
    // Nothing was stored by a refused call.
    let tree = within("GetSettings", proxy.settings_tree(""))
        .await
        .unwrap();
    assert_eq!(tree.groups, Mock::new().settings);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_changed_reaches_the_client_in_one_signal_per_operation() {
    let (_server, client) = pair().await;
    let proxy = OmarchyBroadcastProxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    let mut signals = proxy.receive_settings_changed().await.unwrap();
    let cached = OmarchyBroadcastProxy::new(&client).await.unwrap();
    let mut revisions = cached.receive_settings_revision_changed().await;

    // Turning automatic white balance off enables the temperature: both in
    // one signal.
    within(
        "SetSetting",
        proxy.set_setting_value("", AUTO_WB, &SettingValue::Bool(false)),
    )
    .await
    .unwrap();
    let signal = within("SettingsChanged", signals.next())
        .await
        .expect("SettingsChanged stream ended");
    let args = signal.args().unwrap();
    assert_eq!((args.revision, args.serial), (2, 1));
    assert_eq!(
        args.to_changes(),
        vec![
            SettingChange {
                id: AUTO_WB.into(),
                value: Some(SettingValue::Bool(false)),
                stored: true,
                enabled: true,
                disabled_reason: String::new(),
            },
            SettingChange {
                id: TEMPERATURE.into(),
                value: Some(SettingValue::Int(4000)),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
            },
        ]
    );
    // An action's change has no value.
    within(
        "SetSetting action",
        proxy.set_setting_value("", "camera.focus.pan_reset", &SettingValue::Bool(true)),
    )
    .await
    .unwrap();
    let signal = within("SettingsChanged", signals.next())
        .await
        .expect("SettingsChanged stream ended");
    let args = signal.args().unwrap();
    assert_eq!(args.revision, 3);
    assert_eq!(args.changes[0].1.len(), 3);
    assert_eq!(args.to_changes()[0].value, None);
    // SettingsRevision follows by PropertiesChanged.
    loop {
        let change = within("SettingsRevision change", revisions.next())
            .await
            .expect("SettingsRevision stream ended");
        if change.get().await.unwrap() == 3 {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revision_6_daemon_has_no_settings() {
    let (_server, client) = pair_serving(Rev6).await;
    for proxy in [
        OmarchyBroadcastProxy::new(&client).await.unwrap(),
        OmarchyBroadcastProxy::builder(&client)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap(),
    ] {
        let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
        assert_eq!(snap.interface_revision, 6);
        assert_eq!(snap.camera_controls, Some(CameraControls::default()));
        assert_eq!(snap.settings_revision, None);
        assert_eq!(snap.settings_schema_serial, None);
    }
    let quiet = within("snapshot", Snapshot::fetch_no_autostart(&client))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (quiet.settings_revision, quiet.settings_schema_serial),
        (None, None)
    );
    let proxy = OmarchyBroadcastProxy::new(&client).await.unwrap();
    // The settings methods are absent: a D-Bus error, not a daemon reply.
    match within("GetSettings", proxy.settings_tree("")).await {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    match within("GetSettings", proxy.settings_tree_no_autostart("")).await {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    match within(
        "SetSetting",
        proxy.set_setting_value("", "camera.image.sharpness", &SettingValue::Int(1)),
    )
    .await
    {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    match within(
        "ResetSettings",
        proxy.reset_settings_scope("", &ResetScope::Camera),
    )
    .await
    {
        Err(IpcError::ZBus(_)) => {}
        other => panic!("expected a transport-level error, got {other:?}"),
    }
    assert!(
        within("GetCameraControls", proxy.get_camera_controls(""))
            .await
            .is_ok()
    );
}
