//! The real `br.com.dias.OmarchyBroadcast1` object with the controller over the
//! fake backend, on a peer-to-peer D-Bus connection (a Unix socket pair, no
//! bus daemon), driven through the client proxy of `obc-ipc`: every
//! property, every method and each of its errors, `PropertiesChanged` on
//! each state change, and `Stats` while `active` only. The preview leases,
//! keyed by the caller's unique bus name, need a bus: those tests run the
//! object on a private `dbus-daemon` instead ([`Rig::on_bus`]).

use std::fs;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use obc_ipc::{
    CameraInfo, INTERFACE, INTERFACE_REVISION, IpcError, OBJECT_PATH, OmarchyBroadcastProxy,
    Snapshot, State,
};
use tempfile::TempDir;
use zbus::proxy::CacheProperties;
use zbus::{Connection, Guid, Variant, connection, fdo};

use super::*;
use crate::backend::fake::{FakeBackend, Hook, World};
use crate::backend::{PipeWireEvent, PipeWireState};
use crate::camera::tests::cam;
use crate::controller::{self, Command, Controller, Paths, Published, preliminary_snapshot};
use crate::media::tests::png;
use crate::media::{MediaError, MediaStore};

const TIMEOUT: Duration = Duration::from_secs(5);

/// The daemon's object and controller (over the fake backend) on one end
/// of a socket pair, a client connection on the other.
struct Rig {
    _server: Connection,
    client: Connection,
    commands: Sender<Command>,
    fake: FakeBackend,
    published: Published,
    paths: Paths,
    controller: Option<std::thread::JoinHandle<()>>,
    _dir: TempDir,
    /// The private bus of [`Rig::on_bus`] (last: stopped after the rest).
    bus: Option<PrivateBus>,
}

/// A private `dbus-daemon` on a socket in a short temporary directory,
/// killed when dropped.
struct PrivateBus {
    address: String,
    child: std::process::Child,
    _sockets: TempDir,
}

impl PrivateBus {
    fn start(dir: &std::path::Path) -> PrivateBus {
        use std::io::BufRead;
        const DBUS_DAEMON: &str = "/usr/bin/dbus-daemon";
        assert!(
            std::path::Path::new(DBUS_DAEMON).exists(),
            "this test needs {DBUS_DAEMON} (the reference D-Bus daemon, package `dbus`) to run a \
             private bus"
        );
        // A unix socket path is limited to 108 bytes: a short directory.
        let sockets = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|d| d.is_absolute() && d.as_os_str().len() <= 64)
            .into_iter()
            .chain([std::path::PathBuf::from("/tmp")])
            .find_map(|base| {
                tempfile::Builder::new()
                    .prefix("obc-")
                    .tempdir_in(base)
                    .ok()
            })
            .expect("no writable directory for the test bus socket");
        let config = dir.join("bus.conf");
        fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                sockets.path().display()
            ),
        )
        .unwrap();
        let mut child = std::process::Command::new(DBUS_DAEMON)
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {DBUS_DAEMON}: {e}"));
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line.trim().to_string();
        assert!(address.starts_with("unix:"), "no bus address: {line:?}");
        PrivateBus {
            address,
            child,
            _sockets: sockets,
        }
    }

    async fn connect(&self) -> Connection {
        connection::Builder::address(self.address.as_str())
            .build()
            .await
            .unwrap()
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The media gallery of a rig: its roots under the rig's directory (the
/// videos root a folder of its own, as a session's is).
fn rig_media(dir: &std::path::Path) -> Result<MediaStore, MediaError> {
    Ok(
        MediaStore::new(dir.join("media-bundled"), dir.join("media-user"))
            .with_video_root(dir.join("media-videos")),
    )
}

impl Rig {
    async fn new(config: &str, setup: impl FnOnce(&mut World)) -> Rig {
        Self::build(config, setup, false).await
    }

    /// A rig whose object gets the gallery `media` makes of its directory.
    async fn with_media(
        media: impl FnOnce(&std::path::Path) -> Result<MediaStore, MediaError>,
    ) -> Rig {
        Self::build_with("", |_| {}, false, media).await
    }

    /// The object on a private bus, owning the daemon's name there, with
    /// the subscription to the names leaving the bus; the client is another
    /// connection to that bus.
    async fn on_bus(config: &str, setup: impl FnOnce(&mut World)) -> Rig {
        Self::build(config, setup, true).await
    }

    async fn build(config: &str, setup: impl FnOnce(&mut World), on_bus: bool) -> Rig {
        Self::build_with(config, setup, on_bus, rig_media).await
    }

    async fn build_with(
        config: &str,
        setup: impl FnOnce(&mut World),
        on_bus: bool,
        media: impl FnOnce(&std::path::Path) -> Result<MediaStore, MediaError>,
    ) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            state: dir.path().join("state.toml"),
            cache: Some(dir.path().join("cache")),
            runtime: Some(dir.path().join("run")),
            ledger: None,
        };
        fs::create_dir(dir.path().join("run")).unwrap();
        fs::write(&paths.config, config).unwrap();
        let fake = FakeBackend::default();
        {
            let mut w = fake.world();
            w.cameras = vec![
                cam("usb-a", "Alpha Cam", Some("1111:0001"), "/dev/video0"),
                cam("pci-b", "Beta Cam", None, "/dev/video2"),
            ];
            setup(&mut w);
        }
        let (published, notes) = Published::new(preliminary_snapshot(&paths));
        let (commands, rx) = std::sync::mpsc::channel();
        let object = Server::new(published.clone(), commands.clone(), media(dir.path()));
        let (server, client, bus) = if on_bus {
            let bus = PrivateBus::start(dir.path());
            let server = connection::Builder::address(bus.address.as_str())
                .serve_at(OBJECT_PATH, object)
                .build()
                .await
                .unwrap();
            let owners = subscribe_owners(&server).await.unwrap();
            tokio::spawn(run_owner_watch(owners, commands.clone()));
            server.request_name(obc_ipc::BUS_NAME).await.unwrap();
            let client = bus.connect().await;
            (server, client, Some(bus))
        } else {
            let (s0, s1) = UnixStream::pair().unwrap();
            let server = connection::Builder::unix_stream(s0)
                .server(Guid::generate())
                .p2p()
                .serve_at(OBJECT_PATH, object)
                .build();
            let client = connection::Builder::unix_stream(s1).p2p().build();
            let (server, client) = tokio::try_join!(server, client).unwrap();
            (server, client, None)
        };
        tokio::spawn(run_publisher(server.clone(), notes));
        let (f, p, pb) = (fake.clone(), paths.clone(), published.clone());
        let controller = std::thread::spawn(move || {
            let ctrl = Controller::new(f, p, pb, None, Instant::now());
            controller::run(ctrl, rx);
        });
        Rig {
            _server: server,
            client,
            commands,
            fake,
            published,
            paths,
            controller: Some(controller),
            _dir: dir,
            bus,
        }
    }

    /// A proxy that reads every property from the object (no cache).
    async fn proxy(&self) -> OmarchyBroadcastProxy<'static> {
        OmarchyBroadcastProxy::builder(&self.client)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    fn consumers(&self, n: u32) {
        self.commands.send(Command::Consumers(n)).unwrap();
    }

    /// An event of the PipeWire output that runs (or ran last).
    fn pipewire(&self, event: PipeWireEvent) {
        let generation = self.world().pipewire_generation;
        self.commands
            .send(Command::PipeWire(generation, event))
            .unwrap();
    }

    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.fake.world()
    }

    /// Records every `PropertiesChanged` of the interface as
    /// `Name=value`.
    async fn record_changes(&self) -> Arc<Mutex<Vec<String>>> {
        let props = fdo::PropertiesProxy::builder(&self.client)
            .destination(obc_ipc::BUS_NAME)
            .path(OBJECT_PATH)
            .build()
            .await
            .unwrap();
        let mut stream = props.receive_properties_changed().await.unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let s = Arc::clone(&seen);
        tokio::spawn(async move {
            // Keep the proxy (and its match rule) alive with the stream.
            let _props = props;
            while let Some(signal) = stream.next().await {
                let args = signal.args().unwrap();
                assert_eq!(args.interface_name().as_str(), INTERFACE);
                assert!(args.invalidated_properties().is_empty());
                let mut entries: Vec<String> = args
                    .changed_properties()
                    .iter()
                    .map(|(k, v)| format!("{k}={}", render(v)))
                    .collect();
                entries.sort();
                s.lock().unwrap().extend(entries);
            }
        });
        seen
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(t) = self.controller.take() {
            let _ = t.join();
        }
    }
}

fn render(v: &Variant<'_>) -> String {
    if let Ok(s) = String::try_from(v) {
        return s;
    }
    if let Ok(n) = u32::try_from(v) {
        return n.to_string();
    }
    if let Ok(b) = bool::try_from(v) {
        return b.to_string();
    }
    if let Ok(d) = f64::try_from(v) {
        return d.to_string();
    }
    format!("{v:?}")
}

async fn within<F: std::future::Future>(what: &str, f: F) -> F::Output {
    tokio::time::timeout(TIMEOUT, f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Waits until `done` holds of the recorded changes.
async fn until(what: &str, seen: &Arc<Mutex<Vec<String>>>, done: impl Fn(&[String]) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if done(&seen.lock().unwrap()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; seen {:?}",
            seen.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The first `SettingsChanged` on `signals` past `revision`. The
/// publisher emits a step's signal after that step's method reply, so a
/// stream subscribed after earlier steps may still receive theirs (at or
/// below `revision`, the property read before this step): those are
/// skipped.
async fn settings_changed_after(
    signals: &mut obc_ipc::client::SettingsChangedStream,
    revision: u32,
) -> obc_ipc::client::SettingsChanged {
    within("SettingsChanged", async {
        loop {
            let sig = signals.next().await.unwrap();
            if sig.args().unwrap().revision > revision {
                return sig;
            }
        }
    })
    .await
}

/// The state file once `done` holds of its text (a settings change is
/// saved after a delay).
async fn state_file_until(rig: &Rig, done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = fs::read_to_string(&rig.paths.state).unwrap_or_default();
        if done(&text) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out; the state file is {text:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_state(proxy: &OmarchyBroadcastProxy<'_>, want: State) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let s = proxy.state().await.unwrap();
        if s == want {
            return;
        }
        assert!(Instant::now() < deadline, "state {s}, wanted {want}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_property_reads_through_the_proxy() {
    let rig = Rig::new("[camera]\npreference = [\"beta\"]\n", |_| {}).await;
    let proxy = rig.proxy().await;
    // The controller publishes its first snapshot right after start-up.
    wait_state(&proxy, State::Idle).await;
    let deadline = Instant::now() + TIMEOUT;
    while proxy.cameras().await.unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
    assert_eq!(
        snap,
        Snapshot {
            state: State::Idle,
            error: String::new(),
            warning: String::new(),
            enabled: true,
            cameras: vec![
                CameraInfo {
                    id: "pci-b".into(),
                    name: "Beta Cam".into(),
                    usb: String::new(),
                },
                CameraInfo {
                    id: "usb-a".into(),
                    name: "Alpha Cam".into(),
                    usb: "1111:0001".into(),
                },
            ],
            active_camera: "pci-b".into(),
            output_mode: "NV12 1920x1080@30".into(),
            effect: "none".into(),
            blur_strength: 0.5,
            background_image: String::new(),
            denoise: false,
            consumers: 0,
            loopback_consumers: 0,
            pipewire_consumers: 0,
            preview_consumers: 0,
            pipewire_output: "off".into(),
            camera_controls: Some(obc_ipc::CameraControls {
                camera: "pci-b".into(),
                controls: Vec::new(),
            }),
            settings_revision: Some(1),
            settings_schema_serial: Some(1),
            modules_serial: Some(1),
            backend_profile: Some("quality".into()),
            config_serial: Some(1),
            // No microphone agent here: the mic host is off.
            mic: Some(obc_ipc::MicStatus::default()),
            // Idle: no stage runs.
            stage_values: Vec::new(),
            // Never streamed, no profile row, no USB: its mode's rate only.
            camera_health: Some(obc_ipc::CameraHealth {
                camera: "pci-b".into(),
                nominal_fps: 30.0,
                ..obc_ipc::CameraHealth::none()
            }),
            interface_revision: INTERFACE_REVISION,
        }
    );
    assert_eq!(INTERFACE_REVISION, 20);
    assert_eq!(INTERFACE_REVISION, obc_ipc::CAMERA_HEALTH_REVISION);
    assert_eq!(obc_ipc::CPU_CONVERT_REVISION, 19);
    assert_eq!(obc_ipc::VIDEO_BACKGROUND_REVISION, 18);
    assert_eq!(obc_ipc::PLACEHOLDER_REVISION, 17);
    // GetAll returns the same thirty properties.
    let props = fdo::PropertiesProxy::builder(&rig.client)
        .destination(obc_ipc::BUS_NAME)
        .path(OBJECT_PATH)
        .build()
        .await
        .unwrap();
    let all = props
        .get_all(zbus::names::InterfaceName::from_static_str_unchecked(
            INTERFACE,
        ))
        .await
        .unwrap();
    let mut keys: Vec<&str> = all.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "ActiveCamera",
            "BackendProfile",
            "BackgroundImage",
            "BlurStrength",
            "CameraControls",
            "CameraHealth",
            "Cameras",
            "ConfigSerial",
            "Consumers",
            "Denoise",
            "Effect",
            "Enabled",
            "Error",
            "InterfaceRevision",
            "LoopbackConsumers",
            "MediaSerial",
            "MicConsumers",
            "MicError",
            "MicLatencyMs",
            "MicProvider",
            "MicState",
            "ModulesSerial",
            "OutputMode",
            "PipeWireConsumers",
            "PipeWireOutput",
            "PreviewConsumers",
            "SettingsRevision",
            "SettingsSchemaSerial",
            "StageValues",
            "State",
            "Warning"
        ]
    );
    // Introspection: the methods, the signal, read-only properties that
    // emit changes.
    let xml = fdo::IntrospectableProxy::builder(&rig.client)
        .destination(obc_ipc::BUS_NAME)
        .path(OBJECT_PATH)
        .build()
        .await
        .unwrap()
        .introspect()
        .await
        .unwrap();
    for m in [
        "<method name=\"SetEnabled\">",
        "<method name=\"SelectCamera\">",
        "<method name=\"NextCamera\">",
        "<method name=\"ReloadConfig\">",
        "<method name=\"SetEffect\">",
        "<method name=\"SetBlurStrength\">",
        "<method name=\"SetBackgroundImage\">",
        "<method name=\"SetDenoise\">",
        "<method name=\"AcquirePreview\">",
        "<method name=\"ReleasePreview\">",
        "<method name=\"GetCameraControls\">",
        "<method name=\"SetCameraControl\">",
        "<method name=\"ResetCameraControls\">",
        "<method name=\"GetSettings\">",
        "<method name=\"SetSetting\">",
        "<method name=\"ResetSettings\">",
        "<signal name=\"SettingsChanged\">",
        "<property name=\"SettingsRevision\" type=\"u\" access=\"read\"",
        "<property name=\"SettingsSchemaSerial\" type=\"u\" access=\"read\"",
        "<property name=\"CameraControls\" type=\"(sa(siiiiib))\" access=\"read\"",
        "<signal name=\"PreviewFrame\">",
        "<property name=\"PreviewConsumers\" type=\"u\" access=\"read\"",
        "<property name=\"Denoise\" type=\"b\" access=\"read\"",
        "<property name=\"BlurStrength\" type=\"d\" access=\"read\"",
        "<property name=\"BackgroundImage\" type=\"s\" access=\"read\"",
        "<property name=\"Effect\" type=\"s\" access=\"read\"",
        "<signal name=\"Stats\">",
        "<property name=\"Cameras\" type=\"a(sss)\" access=\"read\"",
        "<property name=\"State\" type=\"s\" access=\"read\"",
        "<property name=\"Consumers\" type=\"u\" access=\"read\"",
        "<property name=\"LoopbackConsumers\" type=\"u\" access=\"read\"",
        "<property name=\"PipeWireConsumers\" type=\"u\" access=\"read\"",
        "<property name=\"PipeWireOutput\" type=\"s\" access=\"read\"",
        "<property name=\"InterfaceRevision\" type=\"u\" access=\"read\"",
        "<method name=\"GetMedia\">",
        "<method name=\"ImportMedia\">",
        "<method name=\"RemoveMedia\">",
        "<method name=\"GetMediaDirs\">",
        "<arg name=\"bundled_dir\" type=\"s\" direction=\"out\"/>",
        "<arg name=\"items\" type=\"aa{sv}\" direction=\"out\"/>",
        "<property name=\"MediaSerial\" type=\"u\" access=\"read\"",
        "<property name=\"StageValues\" type=\"a(ssd)\" access=\"read\"",
        "<property name=\"CameraHealth\" type=\"a{sv}\" access=\"read\"",
        "<method name=\"ApplyRecommendedSettings\">",
    ] {
        assert!(xml.contains(m), "{m} missing from {xml}");
    }
    assert!(!xml.contains("access=\"readwrite\""), "{xml}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_method_and_each_error() {
    let rig = Rig::new("", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;

    // SetEnabled: the property is updated when the reply arrives.
    within("SetEnabled", proxy.set_enabled(false))
        .await
        .unwrap();
    assert_eq!(proxy.state().await.unwrap(), State::Off);
    assert!(!proxy.enabled().await.unwrap());
    within("SetEnabled", proxy.set_enabled(false))
        .await
        .unwrap();
    within("SetEnabled", proxy.set_enabled(true)).await.unwrap();
    assert_eq!(proxy.state().await.unwrap(), State::Idle);

    // SelectCamera: id, name substring, vid:pid, path; NoSuchCamera.
    for (sel, want) in [
        ("pci-b", "pci-b"),
        ("alpha", "usb-a"),
        ("/dev/video2", "pci-b"),
        ("1111:0001", "usb-a"),
    ] {
        within("SelectCamera", proxy.select_camera(sel))
            .await
            .unwrap();
        assert_eq!(proxy.active_camera().await.unwrap(), want, "{sel}");
    }
    match within("SelectCamera", proxy.select_camera("no such camera")).await {
        Err(e @ IpcError::NoSuchCamera(_)) => {
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmarchyBroadcast1.Error.NoSuchCamera"
            );
            assert!(
                e.message().contains("no camera matches \"no such camera\""),
                "{e:?}"
            );
        }
        other => panic!("expected NoSuchCamera, got {other:?}"),
    }

    // NextCamera wraps; with no camera: Unavailable.
    within("NextCamera", proxy.next_camera()).await.unwrap();
    assert_eq!(proxy.active_camera().await.unwrap(), "pci-b");
    within("NextCamera", proxy.next_camera()).await.unwrap();
    assert_eq!(proxy.active_camera().await.unwrap(), "usb-a");
    rig.world().cameras.clear();
    rig.commands.send(Command::Rescan).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while !proxy.cameras().await.unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    match within("NextCamera", proxy.next_camera()).await {
        Err(e @ IpcError::Unavailable(_)) => assert_eq!(
            zbus::DBusError::name(&e).as_str(),
            "br.com.dias.OmarchyBroadcast1.Error.Unavailable"
        ),
        other => panic!("expected Unavailable, got {other:?}"),
    }

    // ReloadConfig: invalid -> InvalidConfig and Error; valid clears it.
    fs::write(&rig.paths.config, "[camera]\nmdoe = \"x\"\n").unwrap();
    match within("ReloadConfig", proxy.reload_config()).await {
        Err(e @ IpcError::InvalidConfig(_)) => {
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmarchyBroadcast1.Error.InvalidConfig"
            );
            assert!(e.message().contains("mdoe"), "{e:?}");
            assert_eq!(proxy.error().await.unwrap(), e.message());
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
    assert_eq!(proxy.state().await.unwrap(), State::Idle, "no error state");
    // An unreadable file (a directory) is InvalidConfig too.
    fs::remove_file(&rig.paths.config).unwrap();
    fs::create_dir(&rig.paths.config).unwrap();
    assert!(matches!(
        within("ReloadConfig", proxy.reload_config()).await,
        Err(IpcError::InvalidConfig(_))
    ));
    fs::remove_dir(&rig.paths.config).unwrap();
    fs::write(&rig.paths.config, "[output]\nmode = \"1280x720@30\"\n").unwrap();
    within("ReloadConfig", proxy.reload_config()).await.unwrap();
    assert_eq!(proxy.error().await.unwrap(), "");
    assert_eq!(proxy.output_mode().await.unwrap(), "NV12 1280x720@30");

    // A failed live switch is Failed.
    let rig = Rig::new("[lifecycle]\nstart_debounce_ms = 0\n", |w| {
        w.fail_ingest = vec!["pci-b".into()]
    })
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    match within("SelectCamera", proxy.select_camera("pci-b")).await {
        Err(e @ IpcError::Failed(_)) => assert_eq!(
            zbus::DBusError::name(&e).as_str(),
            "br.com.dias.OmarchyBroadcast1.Error.Failed"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(proxy.active_camera().await.unwrap(), "usb-a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn properties_changed_follows_every_state_and_stats_flow_only_while_active() {
    let rig = Rig::new(
        "[lifecycle]\nstart_debounce_ms = 100\nidle_grace_s = 0.3\n",
        |w| w.frames = false,
    )
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let cached = OmarchyBroadcastProxy::new(&rig.client).await.unwrap();
    let mut stats = cached.receive_stats().await.unwrap();
    let seen = rig.record_changes().await;

    // No Stats while idle.
    assert!(
        tokio::time::timeout(Duration::from_millis(2500), stats.next())
            .await
            .is_err(),
        "a Stats signal while idle"
    );

    rig.consumers(1);
    until("starting", &seen, |s| {
        s.iter().any(|e| e == "State=starting")
    })
    .await;
    assert_eq!(proxy.state().await.unwrap(), State::Starting);
    rig.world().frames = true;
    until("active", &seen, |s| s.iter().any(|e| e == "State=active")).await;

    // Stats while active, about once per second.
    let t = Instant::now();
    for _ in 0..2 {
        let signal = within("Stats", stats.next()).await.expect("Stats stream");
        let s = signal.args().unwrap().to_stats();
        assert_eq!((s.fps, s.latency_p50_ms, s.gpu_percent), (30.0, 12.0, -1.0));
    }
    // Lower bound only: two signals a period apart take at least about a
    // period (the default suite asserts no upper time bound).
    let two = t.elapsed();
    assert!(two >= Duration::from_millis(900), "{two:?}");

    // SetEnabled(false): Enabled and State announced; back on and active.
    within("SetEnabled", proxy.set_enabled(false))
        .await
        .unwrap();
    until("off", &seen, |s| s.iter().any(|e| e == "State=off")).await;
    within("SetEnabled", proxy.set_enabled(true)).await.unwrap();
    until("active again", &seen, |s| {
        s.iter().filter(|e| *e == "State=active").count() == 2
    })
    .await;

    // The consumer leaves: idle after the grace.
    rig.consumers(0);
    until("idle", &seen, |s| {
        s.last().is_some_and(|e| e == "State=idle")
    })
    .await;
    let states: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e.starts_with("State=") || e.starts_with("Enabled=") || e.starts_with("Consumers=")
        })
        .cloned()
        .collect();
    assert_eq!(
        states,
        [
            "Consumers=1",
            "State=starting",
            "State=active",
            "Enabled=false",
            "State=off",
            "Enabled=true",
            "State=starting",
            "State=active",
            "Consumers=0",
            "State=idle"
        ]
    );
    // The cached proxy followed the changes.
    let deadline = Instant::now() + TIMEOUT;
    while cached.state().await.unwrap() != State::Idle {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Idle again: no more Stats (drain what was already in flight).
    while tokio::time::timeout(Duration::from_millis(50), stats.next())
        .await
        .is_ok()
    {}
    assert!(
        tokio::time::timeout(Duration::from_millis(2500), stats.next())
            .await
            .is_err(),
        "a Stats signal after the session ended"
    );

    // Error and Cameras changes are announced too.
    rig.world().cameras.clear();
    rig.commands.send(Command::Rescan).unwrap();
    until("cameras", &seen, |s| {
        s.iter().any(|e| e.starts_with("Cameras="))
    })
    .await;
    until("active camera", &seen, |s| {
        s.iter().any(|e| e == "ActiveCamera=")
    })
    .await;
    rig.consumers(1);
    until("error", &seen, |s| {
        s.iter().any(|e| e == "Error=no camera is available")
            && s.last().is_some_and(|e| e == "State=error")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_enabled_false_during_a_slow_start_opens_no_camera() {
    let rig = Rig::new("", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    // A slow output build: it lasts until the SetEnabled(false) call has
    // been counted (or, failing that, TIMEOUT).
    let published = rig.published.clone();
    rig.world().during_start = Some(Hook(Box::new(move || {
        let deadline = Instant::now() + TIMEOUT;
        while !published.disable_pending() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    })));
    rig.consumers(1);
    // `starting` is announced before the build begins.
    wait_state(&proxy, State::Starting).await;
    within("SetEnabled", proxy.set_enabled(false))
        .await
        .unwrap();
    assert_eq!(proxy.state().await.unwrap(), State::Off);
    {
        let w = rig.world();
        assert_eq!(w.session_starts, 1, "the build was under way");
        assert!(w.ingests.is_empty(), "a camera was opened: {:?}", w.ingests);
        assert!(w.session.is_none());
    }
    assert!(!rig.published.disable_pending(), "the request was handled");
}

/// The `InvalidArgument` reply of `res`, checked by name; its message.
fn invalid_argument(res: Result<(), IpcError>) -> String {
    match res {
        Err(e @ IpcError::InvalidArgument(_)) => {
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmarchyBroadcast1.Error.InvalidArgument"
            );
            e.message()
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effect_methods_properties_and_their_errors() {
    let rig = Rig::new("[lifecycle]\nstart_debounce_ms = 0\n", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let seen = rig.record_changes().await;
    let image = rig._dir.path().join("background.png");
    fs::write(&image, b"not decoded here").unwrap();
    let image = image.display().to_string();

    // Each method replies after the property changed.
    within("SetBlurStrength", proxy.set_blur_strength(0.75))
        .await
        .unwrap();
    assert_eq!(proxy.blur_strength().await.unwrap(), 0.75);
    within("SetEffect", proxy.set_effect("blur")).await.unwrap();
    assert_eq!(proxy.effect().await.unwrap(), "blur");
    within("SetBackgroundImage", proxy.set_background_image(&image))
        .await
        .unwrap();
    assert_eq!(proxy.background_image().await.unwrap(), image);
    within("SetEffect", proxy.set_effect("replace"))
        .await
        .unwrap();
    assert_eq!(proxy.effect().await.unwrap(), "replace");
    until("the changes", &seen, |s| {
        s.iter().any(|e| e == "Effect=replace")
    })
    .await;
    let effect_changes: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e.starts_with("Effect=")
                || e.starts_with("BlurStrength=")
                || e.starts_with("BackgroundImage=")
        })
        .cloned()
        .collect();
    assert_eq!(
        effect_changes,
        [
            "BlurStrength=0.75".to_string(),
            "Effect=blur".to_string(),
            format!("BackgroundImage={image}"),
            "Effect=replace".to_string(),
        ]
    );
    // Persisted.
    let state = fs::read_to_string(&rig.paths.state).unwrap();
    assert!(state.contains("effect = \"replace\""), "{state}");
    assert!(state.contains("blur_strength = 0.75"), "{state}");

    // The errors: nothing changes.
    let msg = invalid_argument(within("SetEffect", proxy.set_effect("sparkle")).await);
    assert!(msg.contains("unknown effect \"sparkle\""), "{msg}");
    for bad in [-0.1, 1.5, f64::NAN, f64::INFINITY] {
        let msg = invalid_argument(within("SetBlurStrength", proxy.set_blur_strength(bad)).await);
        assert!(msg.contains("outside 0 to 1"), "{bad}: {msg}");
    }
    for (bad, why) in [
        ("relative.png", "not absolute"),
        ("/nowhere/at/all.png", "unreadable"),
        ("/", "not a regular file"),
    ] {
        let msg =
            invalid_argument(within("SetBackgroundImage", proxy.set_background_image(bad)).await);
        assert!(msg.contains(why), "{bad}: {msg}");
    }
    // Clearing the image while replace is selected is refused; after
    // another effect it is accepted, and replace then is refused.
    let msg = invalid_argument(within("SetBackgroundImage", proxy.set_background_image("")).await);
    assert!(msg.contains("select another effect"), "{msg}");
    within("SetEffect", proxy.set_effect("none")).await.unwrap();
    within("SetBackgroundImage", proxy.set_background_image(""))
        .await
        .unwrap();
    assert_eq!(proxy.background_image().await.unwrap(), "");
    let msg = invalid_argument(within("SetEffect", proxy.set_effect("replace")).await);
    assert!(msg.contains("needs a background image"), "{msg}");
    assert_eq!(proxy.effect().await.unwrap(), "none");
    assert_eq!(proxy.blur_strength().await.unwrap(), 0.75);
    assert_eq!(proxy.state().await.unwrap(), State::Idle);

    // While active: an effect that fails to load with on_failure black
    // keeps the session, its frames black, with the reason in Warning;
    // selecting the effect again retries.
    rig.world().effect_fail = Some("the model did not load".into());
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    within("SetEffect", proxy.set_effect("blur")).await.unwrap();
    let black = "effect blur unavailable: the model did not load; the video is black until it \
                 can run";
    assert_eq!(proxy.warning().await.unwrap(), black);
    assert_eq!(proxy.error().await.unwrap(), "");
    assert_eq!(proxy.state().await.unwrap(), State::Active);
    assert!(rig.world().session.is_some(), "the camera stays open");
    rig.world().effect_fail = None;
    within("SetEffect", proxy.set_effect("blur")).await.unwrap();
    assert_eq!(proxy.warning().await.unwrap(), "");
    until("the warning and its recovery", &seen, |s| {
        s.iter().any(|e| *e == format!("Warning={black}"))
            && s.last().is_some_and(|e| e == "Warning=")
            && !s.iter().any(|e| e == "State=error")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_denoise_the_property_its_changes_and_a_failure_in_error() {
    let rig = Rig::new("[lifecycle]\nstart_debounce_ms = 0\n", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    assert_eq!(proxy.interface_revision().await.unwrap(), 20);
    assert!(!proxy.denoise().await.unwrap());
    let seen = rig.record_changes().await;

    // The reply comes after the property changed; it is announced and
    // persisted. The same value again announces nothing.
    within("SetDenoise", proxy.set_denoise(true)).await.unwrap();
    assert!(proxy.denoise().await.unwrap());
    within("SetDenoise", proxy.set_denoise(true)).await.unwrap();
    within("SetDenoise", proxy.set_denoise(false))
        .await
        .unwrap();
    assert!(!proxy.denoise().await.unwrap());
    until("the changes", &seen, |s| {
        s.iter().filter(|e| e.starts_with("Denoise=")).count() == 2
    })
    .await;
    let denoise: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.starts_with("Denoise="))
        .cloned()
        .collect();
    assert_eq!(denoise, ["Denoise=true", "Denoise=false"]);
    let state = fs::read_to_string(&rig.paths.state).unwrap();
    assert!(
        state.contains("\"effect.video.denoise\" = false"),
        "{state}"
    );

    // While active: the stage fails; the session streams on, the state
    // stays active, and Error says why. Switching it off clears it.
    rig.world().denoise_fail = Some("NvVFX_Load failed (-2)".into());
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    within("SetDenoise", proxy.set_denoise(true)).await.unwrap();
    let want = "noise removal unavailable: NvVFX_Load failed (-2); the camera image is shown \
                without it";
    until("the denoise error", &seen, |s| {
        s.iter().any(|e| *e == format!("Error={want}"))
    })
    .await;
    assert_eq!(proxy.state().await.unwrap(), State::Active);
    assert!(rig.world().session.is_some());
    within("SetDenoise", proxy.set_denoise(false))
        .await
        .unwrap();
    assert_eq!(proxy.error().await.unwrap(), "");
    assert_eq!(proxy.state().await.unwrap(), State::Active);
}

/// An effect that fails with `on_failure = "none"` (revision 11): the
/// camera image streams on, the reason in `Warning`, `Error` empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_none_failure_is_in_warning_and_error_stays_empty() {
    let rig = Rig::new(
        "[effect]\non_failure = \"none\"\n[lifecycle]\nstart_debounce_ms = 0\n",
        |w| w.effect_fail = Some("the model did not load".into()),
    )
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let seen = rig.record_changes().await;
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    within("SetEffect", proxy.set_effect("blur")).await.unwrap();
    let want = "effect blur unavailable: the model did not load; the camera image is shown \
                without the effect (effect.on_failure = \"none\")";
    assert_eq!(proxy.warning().await.unwrap(), want);
    assert_eq!(proxy.error().await.unwrap(), "");
    assert_eq!(proxy.state().await.unwrap(), State::Active);
    until("the warning", &seen, |s| {
        s.iter().any(|e| *e == format!("Warning={want}"))
    })
    .await;
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.starts_with("Error=effect")),
        "never in Error"
    );
}

/// `Warning` (revision 11): a selected effect no module provides follows
/// `on_failure` (black by default), announced in `Warning`, never in
/// `Error` or the state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warning_names_a_dropped_effect_and_its_changes_are_announced() {
    let rig = Rig::new("[lifecycle]\nstart_debounce_ms = 0\n", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    assert_eq!(proxy.warning().await.unwrap(), "");
    let seen = rig.record_changes().await;
    within("SetEffect", proxy.set_effect("blur")).await.unwrap();
    within(
        "SetModuleEnabled",
        proxy.set_module_enabled(crate::modules::MAXINE_MODULE, false),
    )
    .await
    .unwrap();
    let want = "effect blur unavailable: NVIDIA Maxine video effects: Disabled; the video is \
                black until it can run";
    assert_eq!(proxy.warning().await.unwrap(), want);
    until("the warning", &seen, |s| {
        s.iter().any(|e| *e == format!("Warning={want}"))
    })
    .await;
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    assert_eq!(proxy.error().await.unwrap(), "");
    assert_eq!(proxy.effect().await.unwrap(), "blur");
    within(
        "SetModuleEnabled",
        proxy.set_module_enabled(crate::modules::MAXINE_MODULE, true),
    )
    .await
    .unwrap();
    assert_eq!(proxy.warning().await.unwrap(), "");
    until("the warning cleared", &seen, |s| {
        s.iter().any(|e| e == "Warning=")
    })
    .await;
    let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
    assert_eq!(snap.warning, "");
}

/// `StageValues` carries what the running stages report, announced by
/// `PropertiesChanged`, and is empty again when the camera goes idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_values_are_a_property_and_their_changes_are_announced() {
    let rig = Rig::new(
        "[lifecycle]\nstart_debounce_ms = 0\nidle_grace_s = 0.0\n",
        |w| {
            w.stage_values = vec![obc_pipeline::effect::StageValue {
                stage: obc_pipeline::effect::StageId::new("auto-frame").unwrap(),
                name: "zoom".into(),
                value: 1.24,
            }];
        },
    )
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    assert!(proxy.stage_values().await.unwrap().is_empty(), "idle: none");
    let seen = rig.record_changes().await;
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    until("the stage values", &seen, |s| {
        s.iter()
            .any(|e| e.starts_with("StageValues=") && e.contains("auto-frame"))
    })
    .await;
    assert_eq!(
        proxy.stage_values().await.unwrap(),
        [obc_ipc::StageValue {
            stage: "auto-frame".into(),
            name: "zoom".into(),
            value: 1.24,
        }]
    );
    let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
    assert_eq!(snap.stage_values.len(), 1);
    rig.consumers(0);
    wait_state(&proxy, State::Idle).await;
    assert!(proxy.stage_values().await.unwrap().is_empty(), "idle again");
    // Set, then cleared: two announcements, the value never repeated.
    until("the stage values cleared", &seen, |s| {
        s.iter().filter(|e| e.starts_with("StageValues=")).count() == 2
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pipewire_properties_and_their_changes() {
    let rig = Rig::new(
        "[lifecycle]\nstart_debounce_ms = 0\nidle_grace_s = 0\n[pipewire]\nenabled = true\n",
        |_| {},
    )
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    assert_eq!(proxy.interface_revision().await.unwrap(), 20);
    // The preliminary snapshot says `off`; the controller's first one has
    // the writer and its PipeWire output, connecting.
    let deadline = Instant::now() + TIMEOUT;
    while proxy.pipewire_output().await.unwrap() != "connecting" {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    {
        let w = rig.world();
        assert_eq!((w.writer_starts, w.pipewire_starts), (1, 1));
        assert_eq!(w.pipewire, Some(1));
    }
    let seen = rig.record_changes().await;

    rig.pipewire(PipeWireEvent::State(PipeWireState::Connecting));
    rig.pipewire(PipeWireEvent::Consumers(0));
    rig.pipewire(PipeWireEvent::State(PipeWireState::Ready));
    rig.pipewire(PipeWireEvent::Consumers(1));
    rig.pipewire(PipeWireEvent::State(PipeWireState::Streaming));
    until("active", &seen, |s| s.iter().any(|e| e == "State=active")).await;
    rig.consumers(1);
    until("both counted", &seen, |s| {
        s.iter().any(|e| e == "Consumers=2")
    })
    .await;
    assert_eq!(
        (
            proxy.consumers().await.unwrap(),
            proxy.loopback_consumers().await.unwrap(),
            proxy.pipewire_consumers().await.unwrap(),
            proxy.pipewire_output().await.unwrap(),
        ),
        (2, 1, 1, "streaming".to_string())
    );
    let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
    assert_eq!(
        (
            snap.consumers,
            snap.loopback_consumers,
            snap.pipewire_consumers,
            snap.pipewire_output.as_str()
        ),
        (2, 1, 1, "streaming")
    );

    // A failure of the PipeWire output: announced, no error state.
    rig.pipewire(PipeWireEvent::State(PipeWireState::Failed(
        "the PipeWire daemon went away".into(),
    )));
    until("failed", &seen, |s| {
        s.iter().any(|e| e == "PipeWireOutput=failed")
    })
    .await;
    assert_eq!(proxy.state().await.unwrap(), State::Active);
    assert_eq!(proxy.error().await.unwrap(), "");
    rig.consumers(0);
    until("idle", &seen, |s| {
        s.last().is_some_and(|e| e == "State=idle")
    })
    .await;
    let pipewire: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e.starts_with("Consumers=")
                || e.starts_with("LoopbackConsumers=")
                || e.starts_with("PipeWire")
                || e.starts_with("State=")
        })
        // The start-up announcement may still have been in flight when the
        // recording began.
        .filter(|e| *e != "PipeWireOutput=connecting")
        .cloned()
        .collect();
    assert_eq!(
        pipewire,
        [
            "PipeWireOutput=ready",
            "Consumers=1",
            "PipeWireConsumers=1",
            "State=starting",
            "State=active",
            "PipeWireOutput=streaming",
            "Consumers=2",
            "LoopbackConsumers=1",
            "Consumers=1",
            "PipeWireConsumers=0",
            "PipeWireOutput=failed",
            "Consumers=0",
            "LoopbackConsumers=0",
            "State=idle",
        ]
    );
    assert!(
        !seen.lock().unwrap().iter().any(|e| e.starts_with("Error=")),
        "{:?}",
        seen.lock().unwrap()
    );
}

/// A proxy on `conn` that reads every property from the object.
async fn proxy_on(conn: &Connection) -> OmarchyBroadcastProxy<'static> {
    OmarchyBroadcastProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_leases_by_bus_name_their_property_and_their_frames() {
    let rig = Rig::on_bus("[lifecycle]\nstart_debounce_ms = 0\n", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    assert_eq!(proxy.interface_revision().await.unwrap(), 20);
    assert_eq!(proxy.preview_consumers().await.unwrap(), 0);
    let seen = rig.record_changes().await;
    let mut frames = proxy.receive_preview_frame().await.unwrap();

    // The lease: the frame file's path, counted as a consumer.
    let path = within("AcquirePreview", proxy.acquire_preview(15))
        .await
        .unwrap();
    let want = rig
        .paths
        .runtime
        .as_ref()
        .unwrap()
        .join("omarchy-broadcast/preview.jpg");
    assert_eq!(std::path::PathBuf::from(&path), want);
    assert_eq!(proxy.preview_consumers().await.unwrap(), 1);
    assert_eq!(proxy.consumers().await.unwrap(), 1);
    assert_eq!(rig.world().previewing(), Some(15));
    until("PreviewConsumers=1", &seen, |s| {
        s.iter().any(|e| e == "PreviewConsumers=1")
    })
    .await;
    wait_state(&proxy, State::Active).await;
    // The same caller again: one lease, the new rate, the same path.
    let again = within("AcquirePreview", proxy.acquire_preview(30))
        .await
        .unwrap();
    assert_eq!(again, path);
    assert_eq!(proxy.preview_consumers().await.unwrap(), 1);
    assert_eq!(rig.world().previewing(), Some(30));

    // A bad rate: InvalidArgument, nothing changes.
    for bad in [0, 10, 16, 60, u32::MAX] {
        let msg = match within("AcquirePreview", proxy.acquire_preview(bad)).await {
            Err(e @ IpcError::InvalidArgument(_)) => {
                assert_eq!(
                    zbus::DBusError::name(&e).as_str(),
                    "br.com.dias.OmarchyBroadcast1.Error.InvalidArgument"
                );
                e.message()
            }
            other => panic!("{bad}: expected InvalidArgument, got {other:?}"),
        };
        assert!(msg.contains("15 or 30"), "{msg}");
    }
    assert_eq!(proxy.preview_consumers().await.unwrap(), 1);

    // Frames reported by the backend's encoder become PreviewFrame.
    rig.published.preview_frame(1);
    rig.published.preview_frame(2);
    for want in [1, 2] {
        let signal = within("PreviewFrame", frames.next())
            .await
            .expect("PreviewFrame stream");
        assert_eq!(signal.args().unwrap().seq, want);
    }

    // A second client takes a lease, then leaves the bus without releasing
    // it: NameOwnerChanged ends it.
    let other = rig.bus.as_ref().unwrap().connect().await;
    let other_proxy = proxy_on(&other).await;
    within("AcquirePreview", other_proxy.acquire_preview(15))
        .await
        .unwrap();
    assert_eq!(proxy.preview_consumers().await.unwrap(), 2);
    assert_eq!(proxy.consumers().await.unwrap(), 2);
    drop(other_proxy);
    other.close().await.unwrap();
    until("the vanished client's lease ended", &seen, |s| {
        s.iter().filter(|e| *e == "PreviewConsumers=1").count() == 2
    })
    .await;
    assert_eq!(proxy.preview_consumers().await.unwrap(), 1);
    assert_eq!(rig.world().previewing(), Some(30));

    // Released; a release without a lease does nothing.
    within("ReleasePreview", proxy.release_preview())
        .await
        .unwrap();
    assert_eq!(proxy.preview_consumers().await.unwrap(), 0);
    assert_eq!(proxy.consumers().await.unwrap(), 0);
    assert_eq!(rig.world().previewing(), None);
    within("ReleasePreview", proxy.release_preview())
        .await
        .unwrap();
    until("PreviewConsumers=0", &seen, |s| {
        s.iter().any(|e| e == "PreviewConsumers=0")
    })
    .await;
    let leases: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.starts_with("PreviewConsumers="))
        .cloned()
        .collect();
    assert_eq!(
        leases,
        [
            "PreviewConsumers=1",
            "PreviewConsumers=2",
            "PreviewConsumers=1",
            "PreviewConsumers=0"
        ]
    );
}

/// The check that a caller is still on the bus once its lease is recorded
/// (a caller gone before would leak it): a name with no owner is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_gone_before_its_lease_was_recorded_is_seen_as_gone() {
    let rig = Rig::on_bus("", |_| {}).await;
    let me = rig.client.unique_name().unwrap().to_string();
    assert!(still_there(&rig._server, &me).await);
    let other = rig.bus.as_ref().unwrap().connect().await;
    let gone = other.unique_name().unwrap().to_string();
    other.close().await.unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while still_there(&rig._server, &gone).await {
        assert!(Instant::now() < deadline, "{gone} still has an owner");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!still_there(&rig._server, ":1.999999").await);
    assert!(!still_there(&rig._server, "not a name").await);
}

/// A caller that sends `AcquirePreview` without waiting for the reply and
/// leaves the bus at once holds no lease, however late the call is
/// served: here the object is held until the caller's departure has
/// reached the controller (no lease to end yet), so only the check made
/// once the lease is recorded can end it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_gone_before_its_lease_was_recorded_leaves_no_lease() {
    let rig = Rig::on_bus("[lifecycle]\nstart_debounce_ms = 0\n", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let seen = rig.record_changes().await;
    // While this guard is held, no call to the object is served.
    let iface = rig
        ._server
        .object_server()
        .interface::<_, Server>(OBJECT_PATH)
        .await
        .unwrap();
    let held = iface.get_mut().await;

    let other = rig.bus.as_ref().unwrap().connect().await;
    let gone = other.unique_name().unwrap().to_string();
    let other_proxy = proxy_on(&other).await;
    other_proxy
        .inner()
        .call_with_flags::<_, _, ()>(
            "AcquirePreview",
            zbus::proxy::MethodFlags::NO_REPLY_EXPECTED,
            &(15u32,),
        )
        .await
        .unwrap();
    drop(other_proxy);
    other.close().await.unwrap();
    // The bus has seen it leave, so the daemon's connection has its
    // NameOwnerChanged; the owner watch hands it to the controller, which
    // has handled it once a later command comes back.
    let deadline = Instant::now() + TIMEOUT;
    while still_there(&rig._server, &gone).await {
        assert!(Instant::now() < deadline, "{gone} still has an owner");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    rig.commands
        .send(Command::ReleasePreview(":0.0".into(), tx))
        .unwrap();
    within("the controller", rx).await.unwrap().unwrap();

    // Served now: the lease is recorded, then ended.
    drop(held);
    until("the lease recorded, then ended", &seen, |s| {
        let leases: Vec<&String> = s
            .iter()
            .filter(|e| e.starts_with("PreviewConsumers="))
            .collect();
        leases == ["PreviewConsumers=1", "PreviewConsumers=0"]
    })
    .await;
    assert_eq!(proxy.preview_consumers().await.unwrap(), 0);
    assert_eq!(rig.world().preview_sets, [Some(15), None]);
    assert_eq!(rig.world().previewing(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_needs_a_sender_and_a_runtime_directory() {
    // Peer to peer: no bus, so no sender to key a lease by.
    let rig = Rig::new("", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    match within("AcquirePreview", proxy.acquire_preview(15)).await {
        Err(e @ IpcError::Failed(_)) => {
            assert_eq!(
                e.message(),
                "the call has no sender: a preview lease needs a bus connection, which names \
                 its holder"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(proxy.preview_consumers().await.unwrap(), 0);
    assert!(rig.world().preview_sets.is_empty());

    // On a bus, with the runtime directory gone: Failed, no lease.
    let rig = Rig::on_bus("", |_| {}).await;
    fs::remove_dir(rig.paths.runtime.as_ref().unwrap()).unwrap();
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    match within("AcquirePreview", proxy.acquire_preview(30)).await {
        Err(e @ IpcError::Failed(_)) => {
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmarchyBroadcast1.Error.Failed"
            );
            assert!(e.message().contains("preview directory"), "{e:?}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(proxy.preview_consumers().await.unwrap(), 0);
}

fn image_control(
    name: &str,
    max: i32,
    default: i32,
    value: i32,
) -> obc_pipeline::controls::DeviceControl {
    obc_pipeline::controls::DeviceControl {
        name: name.into(),
        min: 0,
        max,
        step: 1,
        default,
        value,
    }
}

/// The error name of `res`, which must be an error.
fn error_name<T: std::fmt::Debug>(res: Result<T, IpcError>) -> String {
    match res {
        Err(e @ IpcError::ZBus(_)) => panic!("expected a daemon reply, got {e:?}"),
        Err(e) => zbus::DBusError::name(&e).to_string(),
        Ok(v) => panic!("expected an error, got {v:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn camera_control_methods_their_property_and_their_errors() {
    let rig = Rig::new("[lifecycle]\nstart_debounce_ms = 0\n", |w| {
        w.controls.insert(
            "usb-a".into(),
            vec![
                image_control("saturation", 255, 128, 179),
                image_control("sharpness", 255, 128, 255),
            ],
        );
    })
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let deadline = Instant::now() + TIMEOUT;
    while proxy.active_camera().await.unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = rig.record_changes().await;

    // Read in idle: the device's values, by "" or by id.
    let got = within("GetCameraControls", proxy.get_camera_controls(""))
        .await
        .unwrap();
    assert_eq!(
        got.iter()
            .map(|c| (c.name.as_str(), c.value, c.stored))
            .collect::<Vec<_>>(),
        [("saturation", 179, false), ("sharpness", 255, false)]
    );
    assert_eq!(
        within("GetCameraControls", proxy.get_camera_controls("usb-a"))
            .await
            .unwrap(),
        got
    );
    assert!(
        within("GetCameraControls", proxy.get_camera_controls("pci-b"))
            .await
            .unwrap()
            .is_empty()
    );
    let view = proxy.camera_controls().await.unwrap();
    assert_eq!(
        (view.camera.as_str(), view.controls.clone()),
        ("usb-a", got)
    );

    // Set while idle: stored, not written; the property and its change
    // say so before the reply.
    within(
        "SetCameraControl",
        proxy.set_camera_control("", "sharpness", 128),
    )
    .await
    .unwrap();
    let view = proxy.camera_controls().await.unwrap();
    assert_eq!(
        (view.controls[1].value, view.controls[1].stored),
        (128, true)
    );
    assert!(rig.world().control_writes.is_empty());
    until("CameraControls changed", &seen, |s| {
        s.iter().any(|e| e.starts_with("CameraControls="))
    })
    .await;
    // Saved after the settings write-behind delay.
    let state = state_file_until(&rig, |t| t.contains("values = { sharpness = 128 }")).await;
    assert!(state.contains("values = { sharpness = 128 }"), "{state}");

    // Active: the open applies it; a set writes at once.
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    within(
        "SetCameraControl",
        proxy.set_camera_control("usb-a", "saturation", 128),
    )
    .await
    .unwrap();
    assert_eq!(
        rig.world().control_writes,
        [
            ("usb-a".to_string(), "sharpness".to_string(), 128),
            ("usb-a".to_string(), "saturation".to_string(), 128),
        ]
    );
    // Reset while active: the defaults written, nothing stored.
    within("ResetCameraControls", proxy.reset_camera_controls(""))
        .await
        .unwrap();
    let view = proxy.camera_controls().await.unwrap();
    assert!(view.controls.iter().all(|c| !c.stored && c.value == 128));
    let state = state_file_until(&rig, |t| !t.contains("camera_controls")).await;
    assert!(!state.contains("camera_controls"), "{state}");

    // Each error by its name.
    let invalid = "br.com.dias.OmarchyBroadcast1.Error.InvalidArgument";
    for (name, value) in [
        ("zoom_absolute", 1),
        ("contrast", 1),
        ("sharpness", 256),
        ("sharpness", -1),
    ] {
        assert_eq!(
            error_name(
                within(
                    "SetCameraControl",
                    proxy.set_camera_control("", name, value)
                )
                .await
            ),
            invalid,
            "{name} {value}"
        );
    }
    let no_such = "br.com.dias.OmarchyBroadcast1.Error.NoSuchCamera";
    assert_eq!(
        error_name(within("GetCameraControls", proxy.get_camera_controls("Alpha")).await),
        no_such
    );
    assert_eq!(
        error_name(
            within(
                "SetCameraControl",
                proxy.set_camera_control("nope", "sharpness", 1)
            )
            .await
        ),
        no_such
    );
    assert_eq!(
        error_name(within("ResetCameraControls", proxy.reset_camera_controls("nope")).await),
        no_such
    );
    rig.world().fail_control_writes = vec!["sharpness".into()];
    assert_eq!(
        error_name(
            within(
                "SetCameraControl",
                proxy.set_camera_control("", "sharpness", 1)
            )
            .await
        ),
        "br.com.dias.OmarchyBroadcast1.Error.Failed"
    );
    rig.world().fail_control_query = vec!["pci-b".into()];
    assert_eq!(
        error_name(within("GetCameraControls", proxy.get_camera_controls("pci-b")).await),
        "br.com.dias.OmarchyBroadcast1.Error.Failed"
    );
    rig.world().cameras.clear();
    rig.commands.send(Command::Rescan).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while !proxy.cameras().await.unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let unavailable = "br.com.dias.OmarchyBroadcast1.Error.Unavailable";
    assert_eq!(
        error_name(within("GetCameraControls", proxy.get_camera_controls("")).await),
        unavailable
    );
    assert_eq!(
        error_name(
            within(
                "SetCameraControl",
                proxy.set_camera_control("", "sharpness", 1)
            )
            .await
        ),
        unavailable
    );
    assert_eq!(
        error_name(within("ResetCameraControls", proxy.reset_camera_controls("")).await),
        unavailable
    );
    // No camera: camera "" and no control.
    let view = proxy.camera_controls().await.unwrap();
    assert_eq!(view, obc_ipc::CameraControls::default());
}

/// The `<method>`, `<signal>` or `<property>` element named `name` in
/// introspection data, its arguments included.
fn element<'a>(xml: &'a str, kind: &str, name: &str) -> &'a str {
    let start = xml
        .find(&format!("<{kind} name=\"{name}\""))
        .unwrap_or_else(|| panic!("no {kind} {name} in {xml}"));
    let rest = &xml[start..];
    let end = rest
        .find(&format!("</{kind}>"))
        .map_or_else(|| rest.find("/>").unwrap() + 2, |e| e + kind.len() + 3);
    &rest[..end]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_methods_their_signal_their_properties_and_their_errors() {
    use crate::backend::fake::brio_device;
    use obc_ipc::settings::{ResetScope, camera_options};
    use obc_ipc::{SettingChange, SettingValue};

    const MODE: &str = "camera.exposure.auto_exposure";
    const EXPOSURE: &str = "camera.exposure.exposure_time_absolute";
    const SHARP: &str = "camera.image.sharpness";
    let rig = Rig::new("[lifecycle]\nstart_debounce_ms = 0\n", |w| {
        w.devices.insert("usb-a".into(), brio_device());
    })
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let deadline = Instant::now() + TIMEOUT;
    while proxy.active_camera().await.unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = rig.record_changes().await;

    // The signatures.
    let xml = fdo::IntrospectableProxy::builder(&rig.client)
        .destination(obc_ipc::BUS_NAME)
        .path(OBJECT_PATH)
        .build()
        .await
        .unwrap()
        .introspect()
        .await
        .unwrap();
    let get = element(&xml, "method", "GetSettings");
    for arg in [
        "name=\"options\" type=\"a{sv}\" direction=\"in\"",
        "name=\"revision\" type=\"u\" direction=\"out\"",
        "name=\"serial\" type=\"u\" direction=\"out\"",
        "name=\"camera\" type=\"s\" direction=\"out\"",
        "name=\"groups\" type=\"aa{sv}\" direction=\"out\"",
    ] {
        assert!(get.contains(arg), "{arg} not in {get}");
    }
    assert_eq!(get.matches("direction=\"out\"").count(), 4, "{get}");
    let set = element(&xml, "method", "SetSetting");
    for arg in ["type=\"s\"", "type=\"v\"", "type=\"a{sv}\""] {
        assert!(set.contains(arg), "{arg} not in {set}");
    }
    assert!(!set.contains("direction=\"out\""), "{set}");
    let reset = element(&xml, "method", "ResetSettings");
    assert!(
        reset.contains("type=\"s\"") && reset.contains("type=\"a{sv}\""),
        "{reset}"
    );
    let signal = element(&xml, "signal", "SettingsChanged");
    assert!(signal.contains("type=\"a(sa{sv})\""), "{signal}");
    assert_eq!(signal.matches("type=\"u\"").count(), 2, "{signal}");

    // The tree, the revision and serial properties, the snapshot.
    let tree = within("GetSettings", proxy.settings_tree(""))
        .await
        .unwrap();
    assert_eq!(tree.camera, "usb-a");
    assert_eq!(
        (tree.revision, tree.serial),
        (
            proxy.settings_revision().await.unwrap(),
            proxy.settings_schema_serial().await.unwrap()
        )
    );
    assert!(tree.revision >= 1 && tree.serial >= 1);
    let exp = tree.find(EXPOSURE).unwrap();
    assert!(!exp.enabled);
    assert_eq!(exp.disabled_reason, "Set exposure to manual to change this");
    assert_eq!(
        tree.groups
            .iter()
            .map(|g| g.id.as_str())
            .collect::<Vec<_>>(),
        [
            "image",
            "white_balance",
            "exposure",
            "focus",
            "background",
            "video",
            "lighting"
        ]
    );
    // The global settings (revision 8) after the camera's, scope `global`.
    let globals: Vec<(&str, &str)> = tree
        .groups
        .iter()
        .filter(|g| g.is_global())
        .flat_map(|g| g.settings.iter())
        .map(|s| (s.id.as_str(), s.scope.as_str()))
        .collect();
    assert_eq!(
        globals,
        [
            ("effect.background.mode", "global"),
            ("effect.background.blur_strength", "global"),
            ("effect.video.denoise", "global"),
            ("effect.video.enhance", "global"),
            ("effect.video.hdr", "global"),
            ("effect.lighting.relight", "global"),
            ("effect.lighting.light", "global"),
            ("effect.lighting.direction", "global"),
            ("effect.lighting.brightness", "global")
        ]
    );
    let snap = within("snapshot", Snapshot::fetch(&proxy)).await.unwrap();
    assert_eq!(snap.settings_revision, Some(tree.revision));
    assert_eq!(snap.settings_schema_serial, Some(tree.serial));
    // Another camera by id; unknown option keys are ignored.
    let other = within("GetSettings", proxy.settings_tree("pci-b"))
        .await
        .unwrap();
    // Its controls: none; the global groups are there for every camera.
    assert_eq!((other.camera.as_str(), other.groups.len()), ("pci-b", 3));
    assert!(other.groups.iter().all(|g| g.is_global()));
    let mut options = camera_options("");
    options.insert("future", Variant::from(5u32));
    let reply = within("GetSettings", proxy.get_settings(options))
        .await
        .unwrap();
    assert_eq!(reply.2, "usb-a");

    // Each error by its name.
    let name = |e| format!("br.com.dias.OmarchyBroadcast1.Error.{e}");
    let mut bad_camera = camera_options("");
    bad_camera.insert("camera", Variant::from(5u32));
    assert_eq!(
        error_name(within("GetSettings", proxy.get_settings(bad_camera)).await),
        name("InvalidArgument")
    );
    assert_eq!(
        error_name(within("GetSettings", proxy.settings_tree("nope")).await),
        name("NoSuchCamera")
    );
    let set = |camera: &'static str, id: &'static str, v: SettingValue| {
        let proxy = proxy.clone();
        async move { proxy.set_setting_value(camera, id, &v).await }
    };
    assert_eq!(
        error_name(within("SetSetting", set("nope", SHARP, SettingValue::Int(1))).await),
        name("NoSuchCamera")
    );
    assert_eq!(
        error_name(
            within(
                "SetSetting",
                set("", "camera.image.nope", SettingValue::Int(1))
            )
            .await
        ),
        name("UnknownSetting")
    );
    assert_eq!(
        error_name(within("SetSetting", set("", EXPOSURE, SettingValue::Int(500))).await),
        name("Disabled")
    );
    match within("SetSetting", set("", EXPOSURE, SettingValue::Int(500))).await {
        Err(IpcError::Disabled(m)) => assert_eq!(m, "Set exposure to manual to change this"),
        other => panic!("expected Disabled, got {other:?}"),
    }
    for v in [
        SettingValue::Int(256),
        SettingValue::Bool(true),
        SettingValue::Str("x".into()),
    ] {
        assert_eq!(
            error_name(within("SetSetting", set("", SHARP, v.clone())).await),
            name("InvalidArgument"),
            "{v:?}"
        );
    }
    // A D-Bus type no setting takes.
    assert_eq!(
        error_name(
            within(
                "SetSetting",
                proxy.set_setting(SHARP, Variant::from(1.5f64), camera_options(""))
            )
            .await
        ),
        name("InvalidArgument")
    );
    let reset = |scope: &'static str| {
        let proxy = proxy.clone();
        async move { proxy.reset_settings(scope, camera_options("")).await }
    };
    assert_eq!(
        error_name(within("ResetSettings", reset("all")).await),
        name("InvalidArgument")
    );
    assert_eq!(
        error_name(within("ResetSettings", reset("group:nope")).await),
        name("UnknownSetting")
    );
    assert_eq!(
        error_name(
            within(
                "ResetSettings",
                proxy.reset_settings_scope("nope", &ResetScope::Camera)
            )
            .await
        ),
        name("NoSuchCamera")
    );
    assert!(rig.world().control_writes.is_empty());

    // Streaming: the mode and the exposure time it enables, in one signal.
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    let mut signals = proxy.receive_settings_changed().await.unwrap();
    let before = proxy.settings_revision().await.unwrap();
    let serial = proxy.settings_schema_serial().await.unwrap();
    within(
        "SetSetting",
        set("", MODE, SettingValue::Str("manual".into())),
    )
    .await
    .unwrap();
    let sig = settings_changed_after(&mut signals, before).await;
    let args = sig.args().unwrap();
    assert_eq!((args.revision, args.serial), (before + 1, serial));
    assert_eq!(
        args.to_changes(),
        [
            SettingChange {
                id: MODE.into(),
                value: Some(SettingValue::Str("manual".into())),
                stored: true,
                enabled: true,
                disabled_reason: String::new(),
            },
            SettingChange {
                id: EXPOSURE.into(),
                value: Some(SettingValue::Int(1250)),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
            },
        ]
    );
    // The property is up to date when the signal arrives.
    assert_eq!(proxy.settings_revision().await.unwrap(), before + 1);
    until("SettingsRevision changed", &seen, |s| {
        s.iter()
            .any(|e| *e == format!("SettingsRevision={}", before + 1))
    })
    .await;
    // Any integer type for an int.
    within(
        "SetSetting",
        proxy.set_setting(EXPOSURE, Variant::from(500u16), camera_options("usb-a")),
    )
    .await
    .unwrap();
    assert_eq!(
        rig.world().devices["usb-a"].value(crate::backend::fake::cid::EXPOSURE),
        500
    );
    // A reset of one setting while streaming: its default written.
    within(
        "ResetSettings",
        proxy.reset_settings_scope("", &ResetScope::Setting(EXPOSURE.into())),
    )
    .await
    .unwrap();
    assert_eq!(
        rig.world().devices["usb-a"].value(crate::backend::fake::cid::EXPOSURE),
        156
    );
    let tree = within("GetSettings", proxy.settings_tree(""))
        .await
        .unwrap();
    assert!(!tree.find(EXPOSURE).unwrap().stored);
    // A global setting (revision 8): the existing method's effect and
    // property, one signal, the camera option ignored; a reset of its
    // group.
    drop(signals);
    let mut signals = proxy.receive_settings_changed().await.unwrap();
    let before = proxy.settings_revision().await.unwrap();
    within(
        "SetSetting",
        proxy.set_setting(
            "effect.video.denoise",
            Variant::from(true),
            camera_options("pci-b"),
        ),
    )
    .await
    .unwrap();
    let denoise = |on: bool| SettingChange {
        id: "effect.video.denoise".into(),
        value: Some(SettingValue::Bool(on)),
        stored: on,
        enabled: true,
        disabled_reason: String::new(),
    };
    let sig = settings_changed_after(&mut signals, before).await;
    let args = sig.args().unwrap();
    assert_eq!(args.revision, before + 1);
    assert_eq!(args.to_changes(), [denoise(true)]);
    assert!(proxy.denoise().await.unwrap());
    within(
        "ResetSettings",
        proxy.reset_settings_scope("", &ResetScope::Group("video".into())),
    )
    .await
    .unwrap();
    let sig = settings_changed_after(&mut signals, before + 1).await;
    let args = sig.args().unwrap();
    assert_eq!(args.revision, before + 2);
    assert_eq!(args.to_changes(), [denoise(false)]);
    assert!(!proxy.denoise().await.unwrap());
    // Over the cap: RateLimited. (A signal stream left unread would fill
    // the client connection's queue and stall its replies.)
    drop(signals);
    // Seven settings, each written at most every 50 ms: up to 140 writes
    // a second. Each round sets all seven at once, so a round lasts about
    // one window however slow a single call is, and the writes outpace
    // the cap with margin to spare under load.
    const WB_AUTO: &str = "camera.white_balance.white_balance_automatic";
    const KEEP_FPS: &str = "camera.exposure.keep_framerate";
    let ids = [
        "camera.image.brightness",
        "camera.image.contrast",
        SHARP,
        "camera.exposure.gain",
        EXPOSURE,
        WB_AUTO,
        KEEP_FPS,
    ];
    let value = |id: &str, k: usize| match id {
        WB_AUTO | KEEP_FPS => SettingValue::Bool(k.is_multiple_of(2)),
        EXPOSURE => SettingValue::Int(100 + (k % 2) as i64),
        _ => SettingValue::Int((k % 2) as i64),
    };
    let mut limited = None;
    for k in 0..60usize {
        let calls = ids.iter().map(|id| set("", id, value(id, k)));
        let replies = within("SetSetting", futures_util::future::join_all(calls)).await;
        for reply in replies {
            match reply {
                Ok(()) => {}
                Err(e @ IpcError::RateLimited(_)) => limited = Some(e),
                Err(e) => panic!("round {k}: {e:?}"),
            }
        }
        if limited.is_some() {
            break;
        }
    }
    let e = limited.expect("rate limited within 60 rounds");
    assert_eq!(zbus::DBusError::name(&e).to_string(), name("RateLimited"));
}

/// The members of revision 9 over the bus: the module list, an invalid
/// order refused with its reason, the profile property and its change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_module_members_over_the_bus() {
    let rig = Rig::new("[lifecycle]\nstart_debounce_ms = 0\n", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let list = within("GetModules", proxy.module_list()).await.unwrap();
    assert_eq!(list.serial, proxy.modules_serial().await.unwrap());
    assert_eq!(list.modules[0].id, crate::modules::MAXINE_MODULE);
    assert_eq!(
        within("GetStageOrder", proxy.get_stage_order())
            .await
            .unwrap(),
        ["enhance", "denoise", "background", "relight", "hdr"]
    );
    let e = within(
        "SetStageOrder",
        proxy.set_stage_order(&["relight", "enhance", "denoise", "background", "hdr"]),
    )
    .await
    .unwrap_err();
    match e {
        IpcError::InvalidArgument(m) => {
            assert!(m.contains("relight needs a matte stage before it"), "{m}")
        }
        other => panic!("{other:?}"),
    }
    within("SetStageOrder", proxy.set_stage_order(&[]))
        .await
        .unwrap();
    let seen = rig.record_changes().await;
    within("SetBackendProfile", proxy.set_backend_profile("battery"))
        .await
        .unwrap();
    assert_eq!(proxy.backend_profile().await.unwrap(), "battery");
    until("BackendProfile changed", &seen, |s| {
        s.iter().any(|e| e == "BackendProfile=battery")
    })
    .await;
    let e = within(
        "SetStagePin",
        proxy.set_stage_pin("background", "org.x.none"),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, IpcError::InvalidArgument(_)), "{e:?}");
    within(
        "SetModuleEnabled",
        proxy.set_module_enabled(crate::modules::MAXINE_MODULE, false),
    )
    .await
    .unwrap();
    until("ModulesSerial changed", &seen, |s| {
        s.iter().any(|e| e.starts_with("ModulesSerial="))
    })
    .await;
}

/// The members of revision 10 over the bus: the config schema, a write and
/// a reset (landing in the managed file only, `ConfigSerial` and
/// `ConfigChanged` following), a refusal for a key a later file sets, the
/// file list and the order moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_config_members_over_the_bus() {
    use obc_ipc::ConfigValue as V;
    let config = "[lifecycle]\nstart_debounce_ms = 0\n";
    let rig = Rig::new(config, |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let schema = within("GetConfigSchema", proxy.config_schema())
        .await
        .unwrap();
    assert_eq!(schema.serial, proxy.config_serial().await.unwrap());
    let row = schema.find("lifecycle.start_debounce_ms").unwrap();
    assert_eq!(
        (row.value.clone(), row.source.as_str(), row.editable),
        (V::Int(0), "config.toml", true)
    );
    assert!(
        schema
            .find(&format!(
                "modules.enabled.{}",
                crate::modules::MAXINE_MODULE
            ))
            .is_some()
    );
    let mut changed = proxy.receive_config_changed().await.unwrap();
    let before = proxy.config_serial().await.unwrap();
    within(
        "SetConfig",
        proxy.set_config_value("lifecycle.idle_grace_s", &V::Int(11)),
    )
    .await
    .unwrap();
    let signal = within("ConfigChanged", changed.next()).await.unwrap();
    let serial = signal.args().unwrap().serial;
    assert_ne!(serial, before);
    assert_eq!(proxy.config_serial().await.unwrap(), serial);
    let schema = proxy.config_schema().await.unwrap();
    let row = schema.find("lifecycle.idle_grace_s").unwrap();
    assert_eq!(
        (row.value.clone(), row.source.as_str()),
        (V::Float(11.0), "config.d/50-panel.toml")
    );
    assert_eq!(fs::read_to_string(&rig.paths.config).unwrap(), config);
    // Refusals carry the daemon's reason.
    let e = proxy
        .set_config_value("camera.mode", &V::Str("x".into()))
        .await
        .unwrap_err();
    assert!(matches!(e, IpcError::Disabled(_)), "{e:?}");
    let e = proxy
        .set_config_value("output.mode", &V::Str("x".into()))
        .await
        .unwrap_err();
    assert!(matches!(e, IpcError::InvalidArgument(_)), "{e:?}");
    let e = proxy
        .set_config_value("no.such", &V::Int(1))
        .await
        .unwrap_err();
    assert!(matches!(e, IpcError::UnknownSetting(_)), "{e:?}");
    let dir = rig.paths.config.with_file_name("config.d");
    fs::write(dir.join("90-x.toml"), "[lifecycle]\nidle_grace_s = 2\n").unwrap();
    within("ReloadConfig", proxy.reload_config()).await.unwrap();
    let e = proxy
        .reset_config("lifecycle.idle_grace_s")
        .await
        .unwrap_err();
    match e {
        IpcError::Disabled(m) => assert_eq!(m, "Overridden by config.d/90-x.toml"),
        other => panic!("{other:?}"),
    }
    fs::remove_file(dir.join("90-x.toml")).unwrap();
    within("ReloadConfig", proxy.reload_config()).await.unwrap();
    within("ResetConfig", proxy.reset_config("lifecycle.idle_grace_s"))
        .await
        .unwrap();
    let files = within("GetConfigFiles", proxy.config_files())
        .await
        .unwrap();
    assert_eq!(files.serial, proxy.config_serial().await.unwrap());
    assert_eq!(
        files.dir,
        rig.paths.config.parent().unwrap().display().to_string()
    );
    let listed: Vec<(&str, &str, bool)> = files
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.status.as_str(), f.managed))
        .collect();
    assert_eq!(
        listed,
        [
            ("config.toml", "loaded", false),
            ("config.d/50-panel.toml", "loaded", true)
        ]
    );
    let moves = within("GetStageOrderMoves", proxy.get_stage_order_moves())
        .await
        .unwrap();
    assert_eq!(moves.len(), 5);
    assert_eq!(
        (moves[0].0.as_str(), moves[0].2, moves[0].3),
        ("enhance", false, true)
    );
    assert!(!moves[4].3);
}

// ---------------------------------------------------------------------------
// The media gallery (revision 15)

/// The error of a gallery call, checked to be `InvalidArgument`.
fn refused<T: std::fmt::Debug>(res: Result<T, IpcError>) -> String {
    match res {
        Err(e @ IpcError::InvalidArgument(_)) => e.message(),
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

/// Writes `data` to `path`, creating its directory.
fn put_file(path: &std::path::Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

/// Waits until `MediaSerial` reads `want`, and its change was announced.
async fn media_serial_is(
    proxy: &OmarchyBroadcastProxy<'_>,
    seen: &Arc<Mutex<Vec<String>>>,
    want: u32,
) {
    assert_eq!(proxy.media_serial().await.unwrap(), want);
    let entry = format!("MediaSerial={want}");
    until(&entry, seen, |s| s.contains(&entry)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gallery_lists_imports_and_removes() {
    let rig = Rig::new("", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let seen = rig.record_changes().await;
    let root = rig._dir.path();
    let bundled = root.join("media-bundled/backgrounds/dusk.png");
    put_file(&bundled, &png(3840, 2160, 1));
    let bundled = bundled.display().to_string();

    // The kinds, with their directories; the bundled items first.
    assert_eq!(proxy.media_serial().await.unwrap(), 1);
    let list = within("GetMedia", proxy.media("background")).await.unwrap();
    assert_eq!(list.serial, 1);
    assert_eq!(
        list.bundled_dir,
        root.join("media-bundled/backgrounds").display().to_string()
    );
    assert_eq!(list.user_dir, root.join("media-user").display().to_string());
    assert_eq!(
        list.items,
        [obc_ipc::MediaEntry {
            path: bundled.clone(),
            label: "Dusk".into(),
            bundled: true,
            media_type: "image".into(),
            width: 3840,
            height: 2160,
            duration_ms: 0,
            poster: String::new(),
            cpu_ready: false,
            convert_command: String::new(),
        }]
    );
    // The raw reply carries the documented keys.
    let raw = within("GetMedia", proxy.get_media("background"))
        .await
        .unwrap();
    let mut keys: Vec<&str> = raw.3[0].keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "bundled",
            "duration_ms",
            "height",
            "label",
            "path",
            "poster",
            "type",
            "width"
        ]
    );
    let away = within("GetMedia", proxy.media("away")).await.unwrap();
    assert!(away.items.is_empty());
    assert!(
        away.user_dir.ends_with("media-user/away"),
        "{}",
        away.user_dir
    );
    let msg = refused(within("GetMedia", proxy.media("wallpaper")).await);
    assert!(msg.contains("unknown media kind \"wallpaper\""), "{msg}");

    // GetMediaDirs (revision 19): the user directory GetMedia gives and
    // the videos root for a background; no videos folder for away, the
    // same refusal as GetMedia for another kind.
    let dirs = within("GetMediaDirs", proxy.media_dirs("background"))
        .await
        .unwrap();
    assert_eq!(
        dirs,
        obc_ipc::MediaDirs {
            user_dir: list.user_dir.clone(),
            video_dir: root.join("media-videos").display().to_string(),
        }
    );
    let raw = within("GetMediaDirs", proxy.get_media_dirs("away"))
        .await
        .unwrap();
    let mut keys: Vec<&str> = raw.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["user_dir", "video_dir"]);
    assert_eq!(
        obc_ipc::MediaDirs::from_dict(&raw),
        obc_ipc::MediaDirs {
            user_dir: away.user_dir.clone(),
            video_dir: String::new(),
        }
    );
    let dirs_msg = refused(within("GetMediaDirs", proxy.media_dirs("wallpaper")).await);
    assert_eq!(dirs_msg, msg);

    // An import stores the file and moves the serial.
    let src = root.join("src/Office view.png");
    put_file(&src, &png(1920, 1080, 2));
    let stored = within(
        "ImportMedia",
        proxy.import_media("background", &src.display().to_string()),
    )
    .await
    .unwrap();
    let want = root.join("media-user/Office view.png");
    assert_eq!(stored, want.display().to_string());
    assert_eq!(fs::read(&want).unwrap(), png(1920, 1080, 2));
    media_serial_is(&proxy, &seen, 2).await;
    let list = within("GetMedia", proxy.media("background")).await.unwrap();
    assert_eq!(list.serial, 2);
    let item = list.find(&stored).unwrap();
    assert_eq!(
        (item.label.as_str(), item.bundled, item.width, item.height),
        ("Office view", false, 1920, 1080)
    );

    // The same bytes again (under another name): the stored path, no copy,
    // the serial unmoved.
    let again = root.join("src/copy.png");
    put_file(&again, &png(1920, 1080, 2));
    let second = within(
        "ImportMedia",
        proxy.import_media("background", &again.display().to_string()),
    )
    .await
    .unwrap();
    assert_eq!(second, stored);
    assert_eq!(proxy.media_serial().await.unwrap(), 2);

    // The refusals carry the store's reason; nothing changes.
    let text = root.join("src/notes.txt");
    put_file(&text, b"hello, not an image");
    for (kind, path, why) in [
        (
            "background",
            "relative.png".to_string(),
            "is not an absolute path",
        ),
        ("background", text.display().to_string(), "cannot import"),
        (
            "background",
            "/nowhere/at/all.png".to_string(),
            "cannot import",
        ),
        (
            "background",
            root.join("src/clip.mp4").display().to_string(),
            "cannot import",
        ),
        ("portrait", src.display().to_string(), "unknown media kind"),
    ] {
        let msg = refused(within("ImportMedia", proxy.import_media(kind, &path)).await);
        assert!(msg.contains(why), "{path}: {msg}");
    }
    let msg = refused(within("RemoveMedia", proxy.remove_media(&bundled)).await);
    assert!(
        msg.contains("is not an item of the user's media gallery"),
        "{msg}"
    );
    assert!(std::path::Path::new(&bundled).exists());
    for bad in ["relative.png", "/etc/hostname"] {
        let msg = refused(within("RemoveMedia", proxy.remove_media(bad)).await);
        assert!(msg.contains(bad), "{bad}: {msg}");
    }
    assert_eq!(proxy.media_serial().await.unwrap(), 2);

    // A removal deletes the user's item and moves the serial.
    within("RemoveMedia", proxy.remove_media(&stored))
        .await
        .unwrap();
    assert!(!want.exists());
    media_serial_is(&proxy, &seen, 3).await;
    let list = within("GetMedia", proxy.media("background")).await.unwrap();
    assert_eq!(list.items.len(), 1);
    assert!(list.items[0].bundled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_background_in_force_clears_it_first() {
    let rig = Rig::new("", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let src = rig._dir.path().join("src/room.png");
    put_file(&src, &png(640, 360, 1));
    let import = || async {
        within(
            "ImportMedia",
            proxy.import_media("away", &src.display().to_string()),
        )
        .await
        .unwrap()
    };

    // In force with another effect: cleared, then removed.
    let stored = import().await;
    within("SetBackgroundImage", proxy.set_background_image(&stored))
        .await
        .unwrap();
    assert_eq!(proxy.background_image().await.unwrap(), stored);
    within("RemoveMedia", proxy.remove_media(&stored))
        .await
        .unwrap();
    assert_eq!(proxy.background_image().await.unwrap(), "");
    assert!(!std::path::Path::new(&stored).exists());
    assert_eq!(proxy.media_serial().await.unwrap(), 3);

    // In force under `replace`: the clear is refused, and so is the
    // removal, with its reason; the file and the choice stay.
    let stored = import().await;
    within("SetBackgroundImage", proxy.set_background_image(&stored))
        .await
        .unwrap();
    within("SetEffect", proxy.set_effect("replace"))
        .await
        .unwrap();
    let msg = refused(within("RemoveMedia", proxy.remove_media(&stored)).await);
    assert!(msg.contains("select another effect"), "{msg}");
    assert!(std::path::Path::new(&stored).exists());
    assert_eq!(proxy.background_image().await.unwrap(), stored);
    assert_eq!(proxy.effect().await.unwrap(), "replace");
    assert_eq!(proxy.media_serial().await.unwrap(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_gallery_the_media_methods_fail_with_the_reason() {
    let rig = Rig::with_media(|_| Err(MediaError::NoHome)).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let failed = |res: Result<(), IpcError>| match res {
        Err(IpcError::Failed(m)) => m,
        other => panic!("expected Failed, got {other:?}"),
    };
    let msgs = [
        failed(
            within("GetMedia", proxy.get_media("background"))
                .await
                .map(|_| ()),
        ),
        failed(
            within("ImportMedia", proxy.import_media("background", "/a.png"))
                .await
                .map(|_| ()),
        ),
        failed(within("RemoveMedia", proxy.remove_media("/a.png")).await),
        failed(
            within("GetMediaDirs", proxy.get_media_dirs("background"))
                .await
                .map(|_| ()),
        ),
    ];
    for msg in msgs {
        assert!(
            msg.contains("media gallery is unavailable: HOME is not set"),
            "{msg}"
        );
    }
    // The rest of the object serves on.
    assert_eq!(proxy.media_serial().await.unwrap(), 1);
    assert_eq!(
        proxy.interface_revision().await.unwrap(),
        INTERFACE_REVISION
    );
}

/// The bundled loops of the repository.
fn asset(name: &str) -> std::path::PathBuf {
    std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/media/backgrounds"
    ))
    .join(name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_video_background_is_checked_before_the_controller_takes_it() {
    let rig = Rig::new("", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let dir = rig._dir.path();
    // It starts like an MP4, and the check refuses it: refused here, with
    // the check's reason (the controller never runs the check).
    let bad = dir.join("bad.mp4");
    put_file(&bad, b"\0\0\0\x18ftypisom\0\0\0\0");
    let msg = refused(
        within(
            "SetBackgroundImage",
            proxy.set_background_image(&bad.display().to_string()),
        )
        .await,
    );
    assert!(msg.contains("the video file"), "{msg}");
    assert_eq!(proxy.background_image().await.unwrap(), "");
    // Over the video limit: the video limit is the reason.
    let big = dir.join("big.mp4");
    put_file(&big, b"\0\0\0\x18ftypisom\0\0\0\0");
    fs::File::options()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len(crate::media::probe::MAX_VIDEO_BYTES + 1)
        .unwrap();
    let msg = refused(
        within(
            "SetBackgroundImage",
            proxy.set_background_image(&big.display().to_string()),
        )
        .await,
    );
    assert!(msg.contains("512 MiB limit"), "{msg}");
    // A loop the check accepts: taken, and its check is known to the run.
    let good = dir.join("loop.webm");
    fs::copy(asset("slow-aurora.webm"), &good).unwrap();
    within(
        "SetBackgroundImage",
        proxy.set_background_image(&good.display().to_string()),
    )
    .await
    .unwrap();
    assert_eq!(
        proxy.background_image().await.unwrap(),
        good.display().to_string()
    );
    assert!(crate::media::known_video_info(&good).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_media_lists_the_posters_found_and_announces_each_poster_made() {
    let rig = Rig::with_media(|dir| {
        Ok(
            MediaStore::new(dir.join("media-bundled"), dir.join("media-user"))
                .with_cache(crate::media::MediaCache::new(dir.join("cache"))),
        )
    })
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let seen = rig.record_changes().await;
    let root = rig._dir.path();
    let bundled = root.join("media-bundled/backgrounds");
    for name in ["slow-aurora.webm", "slow-aurora.poster.jpg"] {
        put_file(&bundled.join(name), &fs::read(asset(name)).unwrap());
    }
    let user = root.join("media-user/mine.webm");
    put_file(&user, &fs::read(asset("drifting-bokeh.webm")).unwrap());

    let list = within("GetMedia", proxy.media("background")).await.unwrap();
    let posters: Vec<(String, String)> = list
        .items
        .iter()
        .map(|i| (i.label.clone(), i.poster.clone()))
        .collect();
    assert_eq!(
        posters.len(),
        2,
        "the shipped poster is no item: {posters:?}"
    );
    // The bundled loop's shipped poster, copied into the cache at once.
    let cache = root.join("cache/posters").display().to_string();
    assert!(posters[0].1.starts_with(&cache), "{posters:?}");
    // The user's video had none: the listing made none (it never
    // decodes); the poster maker does, and the serial moves when it
    // lands.
    assert_eq!(posters[1].1, "", "{posters:?}");
    if crate::media::ffmpeg::Ffmpeg::locate().is_err() {
        eprintln!("skipped the rest: ffmpeg is not installed");
        return;
    }
    until("MediaSerial=2", &seen, |s| {
        s.iter().any(|c| c == "MediaSerial=2")
    })
    .await;
    assert_eq!(proxy.media_serial().await.unwrap(), 2);
    let list = within("GetMedia", proxy.media("background")).await.unwrap();
    assert_eq!(list.serial, 2);
    assert!(list.items[1].poster.starts_with(&cache), "{:?}", list.items);
    // Listed again with every poster: nothing more is made.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(proxy.media_serial().await.unwrap(), 2);
    // Both loops play without the GPU: no command (revision 19).
    assert!(
        list.items
            .iter()
            .all(|i| i.cpu_ready && i.convert_command.is_empty()),
        "{:?}",
        list.items
    );

    // An H.264 video does not: it comes with the command that converts it
    // into the user's gallery.
    let clip = root.join("media-user/fast clip.mp4");
    let made = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=64x48:rate=10")
        .args(["-t", "0.3", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&clip)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("skipped the rest: no libx264 encoder");
        return;
    }
    let list = within("GetMedia", proxy.media("background")).await.unwrap();
    let item = list
        .find(&clip.display().to_string())
        .expect("the clip is listed");
    assert!(!item.cpu_ready);
    assert_eq!(
        item.convert_command,
        crate::media::cpu_convert_command(&root.join("media-user"), &clip).unwrap()
    );
    assert!(
        item.convert_command.ends_with(&format!(
            "'{}'",
            root.join("media-user/fast clip-cpu.webm").display()
        )),
        "{}",
        item.convert_command
    );
    let raw = within("GetMedia", proxy.get_media("background"))
        .await
        .unwrap();
    let keyed = |path: &str| {
        raw.3
            .iter()
            .find(|d| {
                d.get("path")
                    .and_then(|v| String::try_from(v.try_clone().unwrap()).ok())
                    .as_deref()
                    == Some(path)
            })
            .map(|d| {
                let mut k: Vec<String> = d.keys().cloned().collect();
                k.sort_unstable();
                k
            })
            .unwrap()
    };
    let keys = keyed(&clip.display().to_string());
    assert!(
        keys.contains(&"cpu_ready".to_string()) && keys.contains(&"convert_command".to_string())
    );
    let keys = keyed(&user.display().to_string());
    assert!(
        keys.contains(&"cpu_ready".to_string()) && !keys.contains(&"convert_command".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_convert_command_gives_the_gallerys_command_for_a_video_and_refuses_the_rest() {
    let rig = Rig::new("", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let root = rig._dir.path();
    let ask = |path: std::path::PathBuf| {
        let proxy = &proxy;
        async move {
            within(
                "GetConvertCommand",
                proxy.get_convert_command(&path.display().to_string()),
            )
            .await
        }
    };
    // A still, a missing file, a relative path, a file the check refuses:
    // refused with the reason.
    let still = root.join("office.png");
    put_file(&still, b"\x89PNG\r\n\x1a\n");
    let msg = refused(ask(still).await);
    assert!(msg.contains("not an MP4, MOV, MKV or WebM file"), "{msg}");
    let msg = refused(ask(root.join("gone.mp4")).await);
    assert!(
        msg.contains("gone.mp4") && msg.contains("unreadable"),
        "{msg}"
    );
    let msg = refused(
        within(
            "GetConvertCommand",
            proxy.get_convert_command("clips/a.mp4"),
        )
        .await,
    );
    assert!(msg.contains("absolute"), "{msg}");
    let bad = root.join("bad.mp4");
    put_file(&bad, b"\0\0\0\x18ftypisom\0\0\0\0");
    let msg = refused(ask(bad).await);
    assert!(msg.contains("the video file"), "{msg}");
    // A video that plays without the GPU already: nothing to convert.
    let ready = root.join("ready.webm");
    fs::copy(asset("slow-aurora.webm"), &ready).unwrap();
    let msg = refused(ask(ready).await);
    assert!(msg.contains("already plays without an NVIDIA GPU"), "{msg}");
    // An H.264 video outside the gallery: the very command `GetMedia`
    // gives, written into the user's videos folder.
    let clip = root.join("elsewhere/it's a clip.mp4");
    fs::create_dir_all(clip.parent().unwrap()).unwrap();
    let made = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=64x48:rate=10")
        .args(["-t", "0.3", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&clip)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("skipped the rest: no ffmpeg with libx264");
        return;
    }
    let gallery = root.join("media-videos");
    let command = ask(clip.clone()).await.unwrap();
    assert_eq!(
        command,
        crate::media::cpu_convert_command(&gallery, &clip).unwrap()
    );
    assert!(
        command.ends_with(&format!(
            "'{}'",
            gallery.join("it'\\''s a clip-cpu.webm").display()
        )),
        "{command}"
    );
    // The same file in the videos folder: GetMedia gives the same
    // command.
    let listed = gallery.join("it's a clip.mp4");
    fs::create_dir_all(&gallery).unwrap();
    fs::copy(&clip, &listed).unwrap();
    let list = within("GetMedia", proxy.media("background")).await.unwrap();
    let item = list
        .find(&listed.display().to_string())
        .expect("the clip is listed");
    assert_eq!(item.convert_command, ask(listed).await.unwrap());
}

#[test]
fn get_media_dirs_gives_no_videos_folder_that_is_the_pictures_one() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let pictures = root.join("Pictures/omarchy-broadcast");
    let shown = |p: &std::path::Path| p.display().to_string();
    // One folder for both (the same path).
    let one = MediaStore::new(root.join("bundled"), &pictures);
    let dirs = media_dirs(&one, MediaKind::Background);
    assert_eq!(dirs.user_dir, shown(&pictures));
    assert_eq!(dirs.video_dir, "");
    // The videos root a link to the pictures root.
    fs::create_dir_all(&pictures).unwrap();
    fs::create_dir_all(root.join("Videos")).unwrap();
    let videos = root.join("Videos/omarchy-broadcast");
    std::os::unix::fs::symlink(&pictures, &videos).unwrap();
    let linked = MediaStore::new(root.join("bundled"), &pictures).with_video_root(&videos);
    assert_eq!(media_dirs(&linked, MediaKind::Background).video_dir, "");
    // A folder of its own is named; away never has one.
    let own = root.join("Movies/omarchy-broadcast");
    let two = MediaStore::new(root.join("bundled"), &pictures).with_video_root(&own);
    assert_eq!(
        media_dirs(&two, MediaKind::Background).video_dir,
        shown(&own)
    );
    assert_eq!(media_dirs(&two, MediaKind::Away).video_dir, "");
}

/// `CameraHealth` (revision 20) through the proxy, announced when the
/// camera changes; `ApplyRecommendedSettings` and its errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn camera_health_and_apply_recommended_settings() {
    let rig = Rig::new("", |w| {
        w.cameras = vec![
            cam("usb-fhd", "FHD Camera", Some("5986:1193"), "/dev/video0"),
            cam("usb-a", "Alpha Cam", Some("1111:0001"), "/dev/video2"),
        ];
    })
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    let health =
        || async { obc_ipc::CameraHealth::from_dict(&proxy.camera_health().await.unwrap()) };
    let deadline = Instant::now() + TIMEOUT;
    while health().await.camera != "usb-fhd" {
        assert!(Instant::now() < deadline, "{:?}", health().await);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let h = health().await;
    assert!(h.has_recommendation);
    assert_eq!(h.delivered_fps, obc_ipc::NOT_MEASURED);
    assert_eq!(h.nominal_fps, 30.0);
    // Its model's row is applied (nothing to write on a camera without
    // controls); the active camera by "" or by its id.
    within("Apply", proxy.apply_recommended_settings(""))
        .await
        .unwrap();
    within("Apply", proxy.apply_recommended_settings("usb-fhd"))
        .await
        .unwrap();
    for (camera, says) in [("nope", "no camera"), ("usb-a", "no recommended settings")] {
        match within("Apply", proxy.apply_recommended_settings(camera)).await {
            Err(e @ IpcError::InvalidArgument(_)) => {
                assert_eq!(
                    zbus::DBusError::name(&e).as_str(),
                    "br.com.dias.OmarchyBroadcast1.Error.InvalidArgument"
                );
                assert!(e.message().contains(says), "{camera}: {e:?}");
            }
            other => panic!("{camera}: expected InvalidArgument, got {other:?}"),
        }
    }
    // Another camera: announced at once.
    let seen = rig.record_changes().await;
    within("SelectCamera", proxy.select_camera("usb-a"))
        .await
        .unwrap();
    until("CameraHealth of usb-a", &seen, |s| {
        s.iter()
            .any(|e| e.starts_with("CameraHealth=") && e.contains("usb-a"))
    })
    .await;
    let h = health().await;
    assert_eq!(h.camera, "usb-a");
    assert!(!h.has_recommendation);
}
