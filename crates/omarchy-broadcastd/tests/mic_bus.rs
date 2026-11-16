//! The microphone's bus clients against fakes on a private D-Bus daemon:
//! the audio worker's internal interface ([`BusWorker`], [`watch_worker`])
//! and the systemd user manager ([`SystemdUnits`]). The private bus has no
//! service activation and no systemd: the user's own worker and service
//! manager are never reached.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::PrivateBus;
use omarchy_broadcastd::mic::HOST_UNIT;
use omarchy_broadcastd::mic::agent::{
    BusWorker, Call, SystemdUnits, UnitError, Units, WORKER_BUS_NAME, WORKER_PATH, Worker,
    WorkerEvent, watch_worker,
};
use zbus::object_server::SignalEmitter;
use zbus::{Connection, OwnedObjectPath, interface};

const TIMEOUT: Duration = Duration::from_secs(5);

async fn connect(bus: &PrivateBus) -> Connection {
    zbus::connection::Builder::address(bus.address.as_str())
        .build()
        .await
        .unwrap()
}

async fn within<F: std::future::Future>(what: &str, f: F) -> F::Output {
    tokio::time::timeout(TIMEOUT, f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// What the fake worker was sent.
#[derive(Default)]
struct Seen {
    snapshots: Vec<String>,
    restored: Vec<String>,
    pinned: Vec<String>,
}

struct FakeWorker(Arc<Mutex<Seen>>);

#[interface(name = "br.com.dias.OmarchyBroadcast.AudioWorker1")]
impl FakeWorker {
    async fn get_modules(&self) -> String {
        r#"{"modules":[{"id":"voice","kind":"audio-filter"}]}"#.into()
    }
    async fn get_settings(&self) -> String {
        r#"{"groups":[]}"#.into()
    }
    async fn get_devices(&self) -> String {
        r#"{"devices":[{"name":"alsa_input.x","description":"X"}]}"#.into()
    }
    async fn get_status(&self) -> String {
        r#"{"state":"passthrough"}"#.into()
    }
    async fn apply_snapshot(&self, snapshot: String) {
        self.0.lock().unwrap().snapshots.push(snapshot);
    }
    async fn make_default(&self, target: String) -> String {
        format!(r#"{{"previous":"was-{target}"}}"#)
    }
    async fn restore_default(&self, previous: String) {
        self.0.lock().unwrap().restored.push(previous);
    }
    async fn pin(&self, target: String) {
        self.0.lock().unwrap().pinned.push(target);
    }
    async fn reset_module(&self, id: String) -> zbus::fdo::Result<bool> {
        if id == "nope" {
            return Err(zbus::fdo::Error::InvalidArgs("No module nope".into()));
        }
        Ok(true)
    }
    #[zbus(signal)]
    async fn status_changed(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn stats(
        emitter: &SignalEmitter<'_>,
        overruns: u32,
        misses: u32,
        voice: f64,
    ) -> zbus::Result<()>;
}

/// A worker older than `Pin`.
struct OldWorker;

#[interface(name = "br.com.dias.OmarchyBroadcast.AudioWorker1")]
impl OldWorker {
    async fn get_status(&self) -> String {
        r#"{"state":"passthrough"}"#.into()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_without_a_method_says_so_apart_from_other_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bus = PrivateBus::start(dir.path());
    let conn = connect(&bus).await;
    let worker = BusWorker::new(conn.clone());
    let e = within("Pin", worker.call(Call::Pin(String::new()), false))
        .await
        .unwrap_err();
    assert!(e.unavailable && !e.unknown_method, "{e:?}");
    let _worker_conn = zbus::connection::Builder::address(bus.address.as_str())
        .serve_at(WORKER_PATH, OldWorker)
        .name(WORKER_BUS_NAME)
        .build()
        .await
        .unwrap();
    let e = within("Pin", worker.call(Call::Pin("alsa_input.x".into()), true))
        .await
        .unwrap_err();
    assert!(e.unknown_method && !e.unavailable, "{e:?}");
    assert_eq!(
        within("GetStatus", worker.call(Call::GetStatus, false))
            .await
            .as_deref(),
        Ok(r#"{"state":"passthrough"}"#)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_is_called_by_name_and_followed_while_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let bus = PrivateBus::start(dir.path());
    let conn = connect(&bus).await;
    let worker = BusWorker::new(conn.clone());

    // No worker: not running, and no call reaches one (nothing to
    // activate on this bus).
    assert!(!worker.running().await);
    let e = within("GetStatus", worker.call(Call::GetStatus, false))
        .await
        .unwrap_err();
    assert!(e.unavailable, "{e:?}");
    let e = within("GetModules", worker.call(Call::GetModules, true))
        .await
        .unwrap_err();
    assert!(e.unavailable, "{e:?}");
    let e = within("GetModules", worker.call(Call::GetModules, false))
        .await
        .unwrap_err();
    assert!(e.unavailable, "{e:?}");

    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(watch_worker(conn.clone(), tx));
    // Subscribed: the owner as it is.
    assert_eq!(
        within("first owner", events.recv()).await,
        Some(WorkerEvent::Owner(false))
    );

    let seen = Arc::new(Mutex::new(Seen::default()));
    let worker_conn = zbus::connection::Builder::address(bus.address.as_str())
        .serve_at(WORKER_PATH, FakeWorker(seen.clone()))
        .name(WORKER_BUS_NAME)
        .build()
        .await
        .unwrap();
    assert_eq!(
        within("owner", events.recv()).await,
        Some(WorkerEvent::Owner(true))
    );
    assert!(worker.running().await);
    for (call, want) in [
        (Call::GetStatus, r#"{"state":"passthrough"}"#),
        (Call::GetSettings, r#"{"groups":[]}"#),
        (
            Call::GetDevices,
            r#"{"devices":[{"name":"alsa_input.x","description":"X"}]}"#,
        ),
        (Call::MakeDefault("t".into()), r#"{"previous":"was-t"}"#),
        (Call::ApplySnapshot("{\"version\":1}".into()), ""),
        (Call::RestoreDefault("prev".into()), ""),
        (Call::Pin("alsa_input.x".into()), ""),
        (Call::ResetModule("voice".into()), "true"),
    ] {
        let what = format!("{call:?}");
        let activate = call.activates();
        assert_eq!(
            within(&what, worker.call(call, activate)).await.as_deref(),
            Ok(want),
            "{what}"
        );
    }
    let e = within(
        "ResetModule",
        worker.call(Call::ResetModule("nope".into()), true),
    )
    .await
    .unwrap_err();
    assert!(
        !e.unavailable && !e.unknown_method && e.message.contains("No module nope"),
        "{e:?}"
    );
    {
        let s = seen.lock().unwrap();
        assert_eq!(s.snapshots, ["{\"version\":1}"]);
        assert_eq!(s.restored, ["prev"]);
        assert_eq!(s.pinned, ["alsa_input.x"]);
    }

    // Its signals.
    let iface = worker_conn
        .object_server()
        .interface::<_, FakeWorker>(WORKER_PATH)
        .await
        .unwrap();
    // Two streams, without an order between them: one at a time.
    FakeWorker::status_changed(iface.signal_emitter(), r#"{"state":"active"}"#)
        .await
        .unwrap();
    assert_eq!(
        within("status", events.recv()).await,
        Some(WorkerEvent::Status(r#"{"state":"active"}"#.into()))
    );
    FakeWorker::stats(iface.signal_emitter(), 3, 4, 0.5)
        .await
        .unwrap();
    assert_eq!(
        within("stats", events.recv()).await,
        Some(WorkerEvent::Stats(3, 4, 0.5))
    );

    // Gone.
    drop(iface);
    drop(worker_conn);
    assert_eq!(
        within("owner gone", events.recv()).await,
        Some(WorkerEvent::Owner(false))
    );
    assert!(!worker.running().await);
}

/// A unit's state, by name.
type UnitStates = Arc<Mutex<HashMap<String, String>>>;

/// The systemd manager's calls, as `Name(args)`.
type Calls = Arc<Mutex<Vec<String>>>;

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.systemd1")]
enum SystemdError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NoSuchUnit(String),
    AlreadySubscribed(String),
}

fn job_path(n: u32) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/job/{n}")).unwrap()
}

fn unit_path(unit: &str) -> OwnedObjectPath {
    let escaped: String = unit
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/unit/{escaped}")).unwrap()
}

struct FakeManager {
    units: UnitStates,
    calls: Calls,
    /// The result of the next start job (`done` when empty).
    start_result: Arc<Mutex<String>>,
    jobs: u32,
}

#[interface(name = "org.freedesktop.systemd1.Manager")]
impl FakeManager {
    async fn subscribe(&self) -> Result<(), SystemdError> {
        let mut calls = self.calls.lock().unwrap();
        // systemd refuses a second Subscribe from the same client.
        if calls.iter().any(|c| c == "Subscribe") {
            return Err(SystemdError::AlreadySubscribed(
                "Client is already subscribed.".into(),
            ));
        }
        calls.push("Subscribe".into());
        Ok(())
    }
    #[zbus(signal)]
    async fn job_removed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        job: OwnedObjectPath,
        unit: &str,
        result: &str,
    ) -> zbus::Result<()>;
    async fn get_unit(&self, name: String) -> Result<OwnedObjectPath, SystemdError> {
        if self.units.lock().unwrap().contains_key(&name) {
            Ok(unit_path(&name))
        } else {
            Err(SystemdError::NoSuchUnit(format!("Unit {name} not loaded.")))
        }
    }
    async fn enable_unit_files(
        &self,
        files: Vec<String>,
        runtime: bool,
        force: bool,
    ) -> (bool, Vec<(String, String, String)>) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("EnableUnitFiles({files:?}, {runtime}, {force})"));
        (true, Vec::new())
    }
    async fn disable_unit_files(
        &self,
        files: Vec<String>,
        runtime: bool,
    ) -> Vec<(String, String, String)> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("DisableUnitFiles({files:?}, {runtime})"));
        Vec::new()
    }
    async fn reload(&self) {
        self.calls.lock().unwrap().push("Reload".into());
    }
    /// The job ends before the reply (the signal comes first), and with
    /// [`FakeManager::start_result`].
    async fn start_unit(
        &mut self,
        name: String,
        mode: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> OwnedObjectPath {
        self.calls
            .lock()
            .unwrap()
            .push(format!("StartUnit({name}, {mode})"));
        let result = std::mem::take(&mut *self.start_result.lock().unwrap());
        let result = if result.is_empty() {
            "done".into()
        } else {
            result
        };
        let state = if result == "done" { "active" } else { "failed" };
        self.units
            .lock()
            .unwrap()
            .insert(name.clone(), state.into());
        self.jobs += 1;
        let job = job_path(self.jobs);
        // Another job's removal first: it is not this one.
        Self::job_removed(&emitter, 0, job_path(999), "other.service", "failed")
            .await
            .unwrap();
        Self::job_removed(&emitter, self.jobs, job.clone(), &name, &result)
            .await
            .unwrap();
        job
    }
    /// The job ends after the reply.
    async fn stop_unit(
        &mut self,
        name: String,
        mode: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> OwnedObjectPath {
        self.calls
            .lock()
            .unwrap()
            .push(format!("StopUnit({name}, {mode})"));
        self.units
            .lock()
            .unwrap()
            .insert(name.clone(), "inactive".into());
        self.jobs += 1;
        let (id, job) = (self.jobs, job_path(self.jobs));
        let emitter = emitter.to_owned();
        let signalled = job.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = FakeManager::job_removed(&emitter, id, signalled, &name, "done").await;
        });
        job
    }
}

struct FakeUnit {
    name: String,
    units: UnitStates,
}

#[interface(name = "org.freedesktop.systemd1.Unit")]
impl FakeUnit {
    #[zbus(property)]
    fn active_state(&self) -> String {
        self.units
            .lock()
            .unwrap()
            .get(&self.name)
            .cloned()
            .unwrap_or_else(|| "inactive".into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mic_host_is_enabled_and_disabled_through_the_service_manager() {
    let dir = tempfile::tempdir().unwrap();
    let bus = PrivateBus::start(dir.path());
    let conn = connect(&bus).await;
    let units = SystemdUnits::new(conn.clone());

    // No service manager: unavailable, the reason naming the unit.
    match within("no manager", units.enable_now(HOST_UNIT)).await {
        Err(UnitError::Unavailable(m)) => assert!(m.contains(HOST_UNIT), "{m}"),
        other => panic!("expected Unavailable, got {other:?}"),
    }

    let states: UnitStates = Arc::default();
    let calls: Calls = Arc::default();
    let start_result: Arc<Mutex<String>> = Arc::default();
    let legacy = "omarchy-broadcast-mic@rnnoise.service";
    let mut b = zbus::connection::Builder::address(bus.address.as_str()).serve_at(
        "/org/freedesktop/systemd1",
        FakeManager {
            units: states.clone(),
            calls: calls.clone(),
            start_result: start_result.clone(),
            jobs: 0,
        },
    );
    for name in [HOST_UNIT, legacy] {
        b = b.serve_at(
            unit_path(name),
            FakeUnit {
                name: name.into(),
                units: states.clone(),
            },
        );
    }
    let _manager = b.name("org.freedesktop.systemd1").build().await.unwrap();

    // Not loaded: not active.
    assert_eq!(within("active", units.active(HOST_UNIT)).await, Ok(false));
    states
        .lock()
        .unwrap()
        .insert(legacy.into(), "activating".into());
    assert_eq!(within("active", units.active(legacy)).await, Ok(true));

    within("enable", units.enable_now(HOST_UNIT)).await.unwrap();
    assert_eq!(within("active", units.active(HOST_UNIT)).await, Ok(true));
    within("disable", units.disable_now(HOST_UNIT))
        .await
        .unwrap();
    assert_eq!(within("active", units.active(HOST_UNIT)).await, Ok(false));
    assert_eq!(
        *calls.lock().unwrap(),
        [
            format!("EnableUnitFiles([\"{HOST_UNIT}\"], false, false)"),
            "Reload".to_string(),
            "Subscribe".to_string(),
            format!("StartUnit({HOST_UNIT}, replace)"),
            format!("StopUnit({HOST_UNIT}, replace)"),
            format!("DisableUnitFiles([\"{HOST_UNIT}\"], false)"),
            "Reload".to_string(),
        ]
    );

    // A start job that fails: `mic on` says so, naming the unit and the
    // result.
    *start_result.lock().unwrap() = "failed".into();
    match within("failing enable", units.enable_now(HOST_UNIT)).await {
        Err(UnitError::Failed(m)) => {
            assert!(m.contains(HOST_UNIT) && m.contains("(failed)"), "{m}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}
