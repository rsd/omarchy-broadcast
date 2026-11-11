//! The worker's internal control interface on the session bus:
//! `br.com.dias.OmarchyBroadcast.AudioWorker1` at
//! `/br/com/dias/OmarchyBroadcast/AudioWorker`, under the name
//! `br.com.dias.OmarchyBroadcast.AudioWorker`. **Internal and unstable**:
//! only omarchy-broadcastd calls it. Documents travel as JSON strings.
//!
//! Every call counts as activity for the exit on idle, and the worker does
//! not exit while one is being answered.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::json;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, fdo, interface};

use crate::engine::{Shared, lock};
use crate::pipewire::PipeWireError;
use crate::snapshot::Snapshot;
use crate::status::State;
use crate::{debug, info, warning};

/// The bus name.
pub const BUS_NAME: &str = "br.com.dias.OmarchyBroadcast.AudioWorker";
/// The object path.
pub const OBJECT_PATH: &str = "/br/com/dias/OmarchyBroadcast/AudioWorker";
/// The interface.
pub const INTERFACE: &str = "br.com.dias.OmarchyBroadcast.AudioWorker1";

/// The object. See the module documentation.
pub struct Worker {
    shared: Arc<Shared>,
}

fn pw_error(e: PipeWireError) -> fdo::Error {
    match e {
        PipeWireError::Unavailable(m) => fdo::Error::NotSupported(m),
        PipeWireError::Invalid(m) => fdo::Error::InvalidArgs(m),
        PipeWireError::Failed(m) => fdo::Error::Failed(m),
    }
}

/// Runs `f` on the blocking pool (it takes the modules' lock, and may
/// run module code; or waits for PipeWire), so the bus is never held up.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> fdo::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| fdo::Error::Failed(format!("the call failed: {e}")))
}

#[interface(name = "br.com.dias.OmarchyBroadcast.AudioWorker1")]
impl Worker {
    /// The modules discovered: id, name, kind, status and reason, stages.
    async fn get_modules(&self) -> fdo::Result<String> {
        let _call = self.shared.call();
        let shared = self.shared.clone();
        blocking(move || lock(&shared.host).modules_json().to_string()).await
    }

    /// The `mic.*` groups of the audio modules, with the values in force.
    async fn get_settings(&self) -> fdo::Result<String> {
        let _call = self.shared.call();
        let shared = self.shared.clone();
        blocking(move || {
            let snapshot = lock(&shared.snapshot).clone();
            let gate = *lock(&shared.gpu_gate);
            lock(&shared.host)
                .settings_json(&snapshot, gate)
                .to_string()
        })
        .await
    }

    /// Applies a full snapshot live; it is never written to disk.
    async fn apply_snapshot(&self, snapshot: String) -> fdo::Result<()> {
        let _call = self.shared.call();
        let s = Snapshot::parse(&snapshot)
            .map_err(|e| fdo::Error::InvalidArgs(format!("the snapshot is not valid: {e}")))?;
        let shared = self.shared.clone();
        blocking(move || shared.apply_snapshot(s)).await?;
        debug!("snapshot applied");
        Ok(())
    }

    /// The user enables audio module `id` again: its crash-ledger entry is
    /// cleared (strikes and disabling) and it is opened if the snapshot
    /// enables it. The daemon calls it when the user enables an audio
    /// module, next to the snapshot that no longer lists it disabled.
    /// Whether the crash ledger had disabled it.
    async fn reset_module(&self, id: String) -> fdo::Result<bool> {
        let _call = self.shared.call();
        let shared = self.shared.clone();
        let was = blocking(move || {
            let snapshot = lock(&shared.snapshot).clone();
            lock(&shared.host).reset_module(&id, &snapshot)
        })
        .await?
        .map_err(fdo::Error::InvalidArgs)?;
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        Ok(was)
    }

    /// The sources a user may pick.
    async fn get_devices(&self) -> fdo::Result<String> {
        let _call = self.shared.call();
        let shared = self.shared.clone();
        let devices = blocking(move || shared.pipewire.devices())
            .await?
            .map_err(pw_error)?;
        let list: Vec<_> = devices
            .iter()
            .map(|d| json!({ "name": d.name, "description": d.description }))
            .collect();
        Ok(json!({ "devices": list }).to_string())
    }

    /// Makes the microphone the default source, its capture side pinned to
    /// `target` (empty: the previous default); returns
    /// `{"previous": name}` (empty when not known).
    async fn make_default(&self, target: String) -> fdo::Result<String> {
        let _call = self.shared.call();
        let shared = self.shared.clone();
        let previous = blocking(move || shared.pipewire.make_default(&target))
            .await?
            .map_err(pw_error)?;
        Ok(json!({ "previous": previous }).to_string())
    }

    /// Makes `previous` the default source again (if the microphone still
    /// is) and unpins the capture side.
    async fn restore_default(&self, previous: String) -> fdo::Result<()> {
        let _call = self.shared.call();
        let shared = self.shared.clone();
        blocking(move || shared.pipewire.restore_default(&previous))
            .await?
            .map_err(pw_error)
    }

    /// The status now.
    async fn get_status(&self) -> String {
        let _call = self.shared.call();
        self.shared.status.borrow().to_json()
    }

    /// The status changed (other than in its counters).
    #[zbus(signal)]
    async fn status_changed(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;

    /// The counters, every second while a chain is active.
    #[zbus(signal)]
    async fn stats(
        emitter: &SignalEmitter<'_>,
        overruns: u32,
        misses: u32,
        voice: f64,
    ) -> zbus::Result<()>;
}

/// Connects to the session bus, serves the object and takes the name.
/// `None` (with a log line) when there is no bus or the name is taken:
/// the audio goes on without the control interface.
pub async fn serve(shared: Arc<Shared>) -> Option<Connection> {
    let built = async {
        zbus::connection::Builder::session()
            .serve_at(
                OBJECT_PATH,
                Worker {
                    shared: shared.clone(),
                },
            )
            .build()
            .await
    }
    .await;
    let conn = match built {
        Ok(c) => c,
        Err(e) => {
            warning!("no session bus ({e}): running without the control interface");
            return None;
        }
    };
    let flags = zbus::fdo::RequestNameFlags::DO_NOT_QUEUE;
    match conn.request_name_with_flags(BUS_NAME, flags).await {
        Ok(zbus::fdo::RequestNameReply::PrimaryOwner)
        | Ok(zbus::fdo::RequestNameReply::AlreadyOwner) => {
            info!("serving {INTERFACE} as {BUS_NAME}");
        }
        Ok(r) => {
            warning!("{BUS_NAME} is owned by another process ({r:?}): running without it");
            return None;
        }
        Err(e) => {
            warning!("cannot own {BUS_NAME} ({e}): running without it");
            return None;
        }
    }
    tokio::spawn(run_signals(conn.clone(), shared));
    Some(conn)
}

/// Releases the bus name (the worker goes idle): calls by name reach
/// nobody from then on, and a new one activates a new worker.
pub async fn release(conn: &Connection) {
    match conn.release_name(BUS_NAME).await {
        Ok(_) => debug!("{BUS_NAME} released"),
        Err(e) => debug!("releasing {BUS_NAME}: {e}"),
    }
}

/// Takes the bus name again (the worker stays after all).
pub async fn reacquire(conn: &Connection) {
    let flags = zbus::fdo::RequestNameFlags::DO_NOT_QUEUE;
    match conn.request_name_with_flags(BUS_NAME, flags).await {
        Ok(zbus::fdo::RequestNameReply::PrimaryOwner)
        | Ok(zbus::fdo::RequestNameReply::AlreadyOwner) => debug!("{BUS_NAME} taken again"),
        Ok(r) => warning!("{BUS_NAME} could not be taken again ({r:?}): running without it"),
        Err(e) => warning!("cannot take {BUS_NAME} again ({e}): running without it"),
    }
}

/// Sends `StatusChanged` on every change and `Stats` every second while
/// active.
async fn run_signals(conn: Connection, shared: Arc<Shared>) {
    let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) else {
        return;
    };
    let mut rx = shared.status.subscribe();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let text = rx.borrow_and_update().to_json();
                if let Err(e) = Worker::status_changed(&emitter, &text).await {
                    debug!("StatusChanged: {e}");
                }
            }
            _ = tick.tick() => {
                let s = shared.status.borrow().clone();
                if s.state == State::Active {
                    let clip = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
                    if let Err(e) =
                        Worker::stats(&emitter, clip(s.overruns), clip(s.misses), s.voice).await
                    {
                        debug!("Stats: {e}");
                    }
                }
            }
        }
    }
}
