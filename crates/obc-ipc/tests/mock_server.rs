//! The client proxy against a mock server implementing
//! `br.com.dias.OmaBroadcast1`, over a peer-to-peer D-Bus connection on a
//! Unix socket pair: no session bus needed.
//!
//! The mock mirrors the daemon's contract (ADR-0011) closely enough to
//! exercise every property, method, error mapping and signal; it is not the
//! daemon's implementation.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use futures_util::StreamExt;
use obc_ipc::{
    CameraInfo, INTERFACE, INTERFACE_REVISION, IpcError, OBJECT_PATH, OmaBroadcastProxy, Snapshot,
    State, Stats,
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
    consumers: u32,
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
            consumers: 0,
        }
    }
}

#[interface(name = "br.com.dias.OmaBroadcast1")]
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

    #[zbus(signal)]
    async fn stats(
        emitter: &SignalEmitter<'_>,
        fps: f64,
        latency_p50_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

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
        "none".into()
    }

    #[zbus(property)]
    fn consumers(&self) -> u32 {
        self.consumers
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        INTERFACE_REVISION
    }
}

/// A server connection serving [`Mock`] and a client connection, joined by a
/// socket pair.
async fn pair() -> (Connection, Connection) {
    let (s0, s1) = UnixStream::pair().unwrap();
    let server = connection::Builder::unix_stream(s0)
        .server(Guid::generate())
        .p2p()
        .serve_at(OBJECT_PATH, Mock::new())
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
    let proxy = OmaBroadcastProxy::new(&client).await.unwrap();

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
            consumers: 0,
            interface_revision: INTERFACE_REVISION,
        }
    );
    assert_eq!(proxy.inner().interface().as_str(), INTERFACE);
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
        Snapshot::fetch(&OmaBroadcastProxy::new(&client).await.unwrap()),
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
    let proxy = OmaBroadcastProxy::builder(&client)
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
    let server = connection::Builder::unix_stream(s0)
        .server(Guid::generate())
        .p2p()
        .build();
    let client = connection::Builder::unix_stream(s1).p2p().build();
    let (server, client) = tokio::try_join!(server, client).unwrap();
    // Starts the object server, which answers for the paths it does not serve.
    let _ = server.object_server();
    let got = within("snapshot", Snapshot::fetch_no_autostart(&client)).await;
    assert!(got.is_err(), "{got:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_calls_every_method_and_sees_changes() {
    let (_server, client) = pair().await;
    // A caching proxy learns of changes from PropertiesChanged, which may
    // arrive after the method reply; an uncached one reads the server's
    // value at the time of the call.
    let proxy = OmaBroadcastProxy::new(&client).await.unwrap();
    let uncached = OmaBroadcastProxy::builder(&client)
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_map_back_by_name() {
    let (_server, client) = pair().await;
    let proxy = OmaBroadcastProxy::new(&client).await.unwrap();

    match within("SelectCamera", proxy.select_camera("nope")).await {
        Err(IpcError::NoSuchCamera(msg)) => assert!(msg.contains("nope"), "{msg}"),
        other => panic!("expected NoSuchCamera, got {other:?}"),
    }
    match within("ReloadConfig", proxy.reload_config()).await {
        Err(e @ IpcError::InvalidConfig(_)) => {
            assert!(e.message().contains("unknown field"), "{e}");
            assert_eq!(
                zbus::DBusError::name(&e).as_str(),
                "br.com.dias.OmaBroadcast1.Error.InvalidConfig"
            );
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
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
    let proxy = OmaBroadcastProxy::new(&client).await.unwrap();
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
