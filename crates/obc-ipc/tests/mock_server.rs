//! The client proxy against a mock server implementing
//! `br.com.dias.OmarchyBroadcast1`, over a peer-to-peer D-Bus connection on a
//! Unix socket pair: no session bus needed.
//!
//! The mock mirrors the daemon's contract (ADR-0011) closely enough to
//! exercise every property, method, error mapping and signal; it is not the
//! daemon's implementation.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use futures_util::StreamExt;
use obc_ipc::{
    CameraControl, CameraControls, CameraInfo, INTERFACE, INTERFACE_REVISION, IpcError,
    OBJECT_PATH, OmarchyBroadcastProxy, Snapshot, State, Stats,
};
use zbus::message::Flags;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::proxy::CacheProperties;
use zbus::{Connection, Guid, MessageStream, connection, fdo, interface};

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
        }
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
    fn interface_revision(&self) -> u32 {
        INTERFACE_REVISION
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
            interface_revision: INTERFACE_REVISION,
        }
    );
    assert_eq!(INTERFACE_REVISION, 6);
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
