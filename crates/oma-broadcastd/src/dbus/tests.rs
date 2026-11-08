//! The real `br.com.dias.OmaBroadcast1` object with the controller over the
//! fake backend, on a peer-to-peer D-Bus connection (a Unix socket pair, no
//! bus daemon), driven through the client proxy of `obc-ipc`: every
//! property, every method and each of its errors, `PropertiesChanged` on
//! each state change, and `Stats` while `active` only.

use std::fs;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use obc_ipc::{
    CameraInfo, INTERFACE, INTERFACE_REVISION, IpcError, OBJECT_PATH, OmaBroadcastProxy, Snapshot,
    State,
};
use tempfile::TempDir;
use zbus::proxy::CacheProperties;
use zbus::{Connection, Guid, Variant, connection, fdo};

use super::*;
use crate::backend::fake::{FakeBackend, Hook, World};
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
}

impl Rig {
    async fn new(config: &str, setup: impl FnOnce(&mut World)) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            state: dir.path().join("state.toml"),
        };
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
        let (s0, s1) = UnixStream::pair().unwrap();
        let server = connection::Builder::unix_stream(s0)
            .server(Guid::generate())
            .p2p()
            .serve_at(
                OBJECT_PATH,
                Server::new(published.clone(), commands.clone()),
            )
            .build();
        let client = connection::Builder::unix_stream(s1).p2p().build();
        let (server, client) = tokio::try_join!(server, client).unwrap();
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
        }
    }

    /// A proxy that reads every property from the object (no cache).
    async fn proxy(&self) -> OmaBroadcastProxy<'static> {
        OmaBroadcastProxy::builder(&self.client)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    fn consumers(&self, n: u32) {
        self.commands.send(Command::Consumers(n)).unwrap();
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

async fn wait_state(proxy: &OmaBroadcastProxy<'_>, want: State) {
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
            consumers: 0,
            interface_revision: INTERFACE_REVISION,
        }
    );
    // GetAll returns the same nine properties.
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
            "Cameras",
            "Consumers",
            "Effect",
            "Enabled",
            "Error",
            "InterfaceRevision",
            "OutputMode",
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
        "<signal name=\"Stats\">",
        "<property name=\"Cameras\" type=\"a(sss)\" access=\"read\"",
        "<property name=\"State\" type=\"s\" access=\"read\"",
        "<property name=\"Consumers\" type=\"u\" access=\"read\"",
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
                "br.com.dias.OmaBroadcast1.Error.NoSuchCamera"
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
            "br.com.dias.OmaBroadcast1.Error.Unavailable"
        ),
        other => panic!("expected Unavailable, got {other:?}"),
    }

    // ReloadConfig: invalid -> InvalidConfig and Error; valid clears it.
    fs::write(&rig.paths.config, "[camera]\nmdoe = \"x\"\n").unwrap();
    match within("ReloadConfig", proxy.reload_config()).await {
        Err(e @ IpcError::InvalidConfig(_)) => {
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmaBroadcast1.Error.InvalidConfig"
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
            "br.com.dias.OmaBroadcast1.Error.Failed"
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
    let cached = OmaBroadcastProxy::new(&rig.client).await.unwrap();
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
