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

impl Rig {
    async fn new(config: &str, setup: impl FnOnce(&mut World)) -> Rig {
        Self::build(config, setup, false).await
    }

    /// The object on a private bus, owning the daemon's name there, with
    /// the subscription to the names leaving the bus; the client is another
    /// connection to that bus.
    async fn on_bus(config: &str, setup: impl FnOnce(&mut World)) -> Rig {
        Self::build(config, setup, true).await
    }

    async fn build(config: &str, setup: impl FnOnce(&mut World), on_bus: bool) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            state: dir.path().join("state.toml"),
            runtime: Some(dir.path().join("run")),
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
        let object = Server::new(published.clone(), commands.clone());
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
            interface_revision: INTERFACE_REVISION,
        }
    );
    assert_eq!(INTERFACE_REVISION, 6);
    // GetAll returns the same seventeen properties.
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
            "BackgroundImage",
            "BlurStrength",
            "CameraControls",
            "Cameras",
            "Consumers",
            "Denoise",
            "Effect",
            "Enabled",
            "Error",
            "InterfaceRevision",
            "LoopbackConsumers",
            "OutputMode",
            "PipeWireConsumers",
            "PipeWireOutput",
            "PreviewConsumers",
            "State"
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

    // While active: an effect failure with on_failure black is the error
    // state, with the reason in Error; selecting the effect again retries.
    rig.world().effect_fail = Some("the model did not load".into());
    rig.consumers(1);
    wait_state(&proxy, State::Active).await;
    within("SetEffect", proxy.set_effect("blur")).await.unwrap();
    wait_state(&proxy, State::Error).await;
    assert_eq!(
        proxy.error().await.unwrap(),
        "effect blur unavailable: the model did not load"
    );
    assert!(rig.world().session.is_none(), "the camera is closed");
    rig.world().effect_fail = None;
    within("SetEffect", proxy.set_effect("blur")).await.unwrap();
    wait_state(&proxy, State::Active).await;
    assert_eq!(proxy.error().await.unwrap(), "");
    until("the error state and its recovery", &seen, |s| {
        s.iter()
            .any(|e| e == "Error=effect blur unavailable: the model did not load")
            && s.last().is_some_and(|e| e == "State=active")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_denoise_the_property_its_changes_and_a_failure_in_error() {
    let rig = Rig::new("[lifecycle]\nstart_debounce_ms = 0\n", |_| {}).await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    assert_eq!(proxy.interface_revision().await.unwrap(), 6);
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
    assert!(state.contains("denoise = false"), "{state}");

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pipewire_properties_and_their_changes() {
    let rig = Rig::new(
        "[lifecycle]\nstart_debounce_ms = 0\nidle_grace_s = 0\n[pipewire]\nenabled = true\n",
        |_| {},
    )
    .await;
    let proxy = rig.proxy().await;
    wait_state(&proxy, State::Idle).await;
    assert_eq!(proxy.interface_revision().await.unwrap(), 6);
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
    assert_eq!(proxy.interface_revision().await.unwrap(), 6);
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
    let state = fs::read_to_string(&rig.paths.state).unwrap();
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
    let state = fs::read_to_string(&rig.paths.state).unwrap();
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
