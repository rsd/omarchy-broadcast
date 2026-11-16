//! The microphone's asynchronous side: one task on the daemon's runtime
//! ([`Agent::run`]) that talks to the audio worker and to the service
//! manager, writes the audio settings snapshot, and reports to the
//! controller, which owns every setting and publishes the properties.
//!
//! - The worker (`br.com.dias.OmarchyBroadcast.AudioWorker1`, internal) is
//!   reached through [`Worker`]. Bus activation starts it for what only it
//!   can answer (its modules, settings and devices, the default microphone,
//!   forgiving a module), at the daemon's start and on a user's request;
//!   never just to push a snapshot, read its status or follow up on what
//!   it said: those go out with `NO_AUTO_START` while its name has an
//!   owner, so the daemon never keeps an idle worker alive, nor starts one
//!   that has just exited. Its signals are followed while it runs
//!   ([`WorkerEvent`]); `Stats` becomes `MicStats`.
//! - The mic host's unit is enabled and started, or stopped and disabled,
//!   through [`Units`] (the systemd user manager over D-Bus,
//!   [`SystemdUnits`], which waits for the job's result, up to
//!   [`JOB_WAIT`]); its state is read every [`HOST_POLL`].
//! - Each snapshot the controller sends ([`MicRequest::Snapshot`]) is
//!   written atomically to `audio-settings.json` (the worker reads it at its
//!   start, so it runs right with the daemon down), then pushed with
//!   `ApplySnapshot` if the worker runs. Snapshots queued together are
//!   written once, the last.
//!
//! What changes the controller's state comes back as a [`MicEvent`]
//! (through [`Command::Mic`]), with the method's reply when there is one:
//! the controller answers after it has published the change.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use futures_util::StreamExt;
use obc_ipc::{IpcError, MicStatus};
use serde_json::Value as Json;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::{Connection, DBusError};

use super::{
    Catalogue, HOST_UNIT, LEGACY_UNITS, gain_pending, parse_devices, parse_groups,
    parse_hardware_gain, parse_modules,
};
use crate::controller::{Command, Published, Reply};
use crate::{debug, info, warning};

/// The worker's bus name.
pub const WORKER_BUS_NAME: &str = "br.com.dias.OmarchyBroadcast.AudioWorker";
/// Its object.
pub const WORKER_PATH: &str = "/br/com/dias/OmarchyBroadcast/AudioWorker";
/// Its interface (internal and unstable).
pub const WORKER_INTERFACE: &str = "br.com.dias.OmarchyBroadcast.AudioWorker1";

/// How often the mic host's unit state is read.
pub const HOST_POLL: Duration = Duration::from_secs(5);
/// How many host polls a pin that failed is tried again at (the mic
/// host's capture node, or the device, not in PipeWire yet), before it is
/// only logged: about half a minute.
pub const PIN_RETRIES: u32 = 6;
/// The longest a call to the worker or the service manager may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest [`SystemdUnits`] waits for a start or stop job to finish
/// (within [`CALL_TIMEOUT`]).
pub const JOB_WAIT: Duration = Duration::from_secs(10);

/// What the controller asks of the agent.
#[derive(Debug)]
pub enum MicRequest {
    /// The audio settings snapshot changed: written, and pushed to a
    /// running worker.
    Snapshot(String),
    /// `SetMicHost(on)`; `previous` is the default microphone to restore
    /// first when turning it off, as the controller knew it (the restore
    /// is asked for in any case: the worker acts only while the Omarchy
    /// Broadcast Mic is the default).
    Host {
        on: bool,
        previous: Option<String>,
        reply: Reply,
    },
    /// "Use as default microphone", the capture side pinned to `target`
    /// (empty: the previous default).
    MakeDefault { target: String, reply: Reply },
    /// `mic.device` is `target` (empty: `default`), sent at the start and
    /// at every change: the capture side is pinned to it now while the mic
    /// host runs, whether or not the Omarchy Broadcast Mic is the default,
    /// and again whenever the mic host comes on and after a restore of the
    /// default (the worker's `Pin`, which never changes the default; with
    /// `default`, it unpins only while the Omarchy Broadcast Mic is not the
    /// default). Nothing is recorded or answered; a device that is not
    /// connected is only logged.
    Repin { target: String },
    /// "Use the previous default microphone".
    RestoreDefault { previous: String, reply: Reply },
    /// An audio module was enabled: its crash-ledger entry is cleared.
    ResetModule(String),
}

/// The channel to the agent.
pub type MicLink = UnboundedSender<MicRequest>;

/// What the agent tells the controller.
#[derive(Debug)]
pub enum MicEvent {
    /// The `Mic*` properties.
    Status(MicStatus),
    /// The worker's modules, settings and devices.
    Catalogue(Catalogue),
    /// The Omarchy Broadcast Mic is the default microphone now; `previous`
    /// was (empty when not known), and `in_force` was the real source in
    /// force then (`None` when the worker saw none, as when ours was the
    /// default in force already, or an older worker that does not tell it).
    /// `already`: ours was the configured default before the call, so that
    /// `previous` is the pin in force and a previous default on record is
    /// kept, with its source in force (`false` from an older worker that
    /// does not tell it). Kept, then `reply` answered.
    DefaultMade {
        previous: String,
        in_force: Option<String>,
        already: bool,
        reply: Reply,
    },
    /// The previous default microphone is the default again (`Ok`), or not.
    DefaultRestored {
        result: Result<(), IpcError>,
        reply: Reply,
    },
    /// `SetMicHost` done (`result`); `restored` when the restore on the way
    /// off succeeded (the Omarchy Broadcast Mic is not the default).
    HostDone {
        restored: bool,
        result: Result<(), IpcError>,
        reply: Reply,
    },
}

/// A call to the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    GetStatus,
    GetSettings,
    GetModules,
    GetDevices,
    ApplySnapshot(String),
    MakeDefault(String),
    RestoreDefault(String),
    Pin(String),
    ResetModule(String),
}

impl Call {
    /// Whether the call may start the worker (bus activation) when the
    /// agent asks for it deliberately; the agent also sends such a call
    /// without activation ([`Worker::call`]'s `activate`) when it only
    /// follows a worker that runs.
    pub fn activates(&self) -> bool {
        !matches!(self, Call::GetStatus | Call::ApplySnapshot(_))
    }
}

/// A failed call to the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerError {
    /// No worker (not installed, not running for a call that does not
    /// start it) or no PipeWire for it: `Unavailable`.
    pub unavailable: bool,
    /// The worker does not have the method (an older one, still running).
    pub unknown_method: bool,
    pub message: String,
}

impl WorkerError {
    fn to_ipc(&self, what: &str) -> IpcError {
        let msg = format!("{what}: {}", self.message);
        if self.unavailable {
            IpcError::Unavailable(msg)
        } else {
            IpcError::Failed(msg)
        }
    }
}

/// The audio worker's interface (see the module documentation). A reply is
/// the method's JSON text; `""` for a method without one, `true`/`false`
/// for `ResetModule`.
pub trait Worker: Send + Sync + 'static {
    /// Whether the worker's name has an owner (never starts it).
    fn running(&self) -> impl Future<Output = bool> + Send;
    /// Makes `call`, starting the worker for it when `activate` (only ever
    /// set for a call that [`Call::activates`]); else it reaches a worker
    /// that runs, or none.
    fn call(
        &self,
        call: Call,
        activate: bool,
    ) -> impl Future<Output = Result<String, WorkerError>> + Send;
}

/// What the worker sent.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerEvent {
    /// Its name got an owner (`true`) or lost it.
    Owner(bool),
    /// `StatusChanged`, its JSON text.
    Status(String),
    /// `Stats(overruns, misses, voice)`.
    Stats(u32, u32, f64),
}

/// A failed call to the service manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitError {
    /// No service manager on the bus.
    Unavailable(String),
    /// It refused.
    Failed(String),
}

impl UnitError {
    fn to_ipc(&self) -> IpcError {
        match self {
            UnitError::Unavailable(m) => IpcError::Unavailable(m.clone()),
            UnitError::Failed(m) => IpcError::Failed(m.clone()),
        }
    }
}

/// The service manager, for the mic host's unit.
pub trait Units: Send + Sync + 'static {
    /// Whether `unit` is active (or activating, or reloading).
    fn active(&self, unit: &str) -> impl Future<Output = Result<bool, UnitError>> + Send;
    /// `systemctl --user enable --now unit`: `Ok` once the start job
    /// finished well, an error naming its result when it did not.
    fn enable_now(&self, unit: &str) -> impl Future<Output = Result<(), UnitError>> + Send;
    /// `systemctl --user disable --now unit` (disabled even when the stop
    /// job fails, which is then the error).
    fn disable_now(&self, unit: &str) -> impl Future<Output = Result<(), UnitError>> + Send;
}

/// The agent. See the module documentation.
pub struct Agent<W: Worker, U: Units> {
    worker: W,
    units: U,
    commands: Sender<Command>,
    published: Published,
    /// `audio-settings.json`; `None`: not written (no state directory).
    snapshot_path: Option<PathBuf>,
    host_on: bool,
    /// The worker's last status, while it runs.
    worker_status: Option<Json>,
    sent_status: Option<MicStatus>,
    catalogue: Catalogue,
    sent_catalogue: Option<Catalogue>,
    /// Requests taken from the channel while coalescing snapshots.
    pending: VecDeque<MicRequest>,
    /// The previous default the worker named at the last "Use as default
    /// microphone" here, until a restore: `mic off` restores it even when
    /// the controller had not recorded it yet. Another one while ours was
    /// the configured default already (the worker's `already`) keeps it.
    made_previous: Option<String>,
    /// `mic.device` as the controller last told it ([`MicRequest::Repin`];
    /// empty: `default`), `None` before it did.
    pin_target: Option<String>,
    /// The capture side is to be pinned to `pin_target` ([`Agent::pin`]).
    pin_due: bool,
    /// How often the pin due failed: it is tried again at each host poll,
    /// up to [`PIN_RETRIES`] times (the mic host's capture node, or the
    /// device, may not be in PipeWire yet just after a start).
    pin_failures: u32,
    /// The `capture_serial` of the worker's last status (`None` while no
    /// worker runs, or with a worker that does not tell it): when it
    /// moves, the mic host's capture node is a new one, not pinned yet.
    capture_seen: Option<u64>,
    /// "Use as default microphone" succeeded here, and no restore since:
    /// a worker without `Pin` is asked to move the pin with `MakeDefault`
    /// (idempotent while ours is the default) instead.
    default_made: bool,
    /// How often the mic host's unit state is read ([`HOST_POLL`]).
    host_poll: Duration,
}

impl<W: Worker, U: Units> Agent<W, U> {
    pub fn new(
        worker: W,
        units: U,
        commands: Sender<Command>,
        published: Published,
        snapshot_path: Option<PathBuf>,
    ) -> Self {
        Agent {
            worker,
            units,
            commands,
            published,
            snapshot_path,
            host_on: false,
            worker_status: None,
            sent_status: None,
            catalogue: Catalogue::default(),
            sent_catalogue: None,
            pending: VecDeque::new(),
            made_previous: None,
            pin_target: None,
            pin_due: false,
            pin_failures: 0,
            capture_seen: None,
            default_made: false,
            host_poll: HOST_POLL,
        }
    }

    /// The agent, its host poll every `every` instead of [`HOST_POLL`].
    pub fn with_host_poll(mut self, every: Duration) -> Self {
        self.host_poll = every;
        self
    }

    /// Runs until the controller drops its link: the requests queued at
    /// the start first (the first snapshot), then the host's state, the
    /// worker's catalogue (starting it) and status, then requests, the
    /// worker's events and the host poll as they come.
    pub async fn run(
        mut self,
        mut requests: UnboundedReceiver<MicRequest>,
        events: UnboundedReceiver<WorkerEvent>,
    ) {
        let mut events = Some(events);
        while let Ok(r) = requests.try_recv() {
            self.pending.push_back(r);
        }
        self.drain_pending(&mut requests).await;
        self.refresh_host().await;
        // Whether the host runs is told before the catalogue, which may
        // take a while (the worker starting): the panel does not ask to
        // turn on a microphone that is on meanwhile.
        self.send_status();
        self.pin().await;
        self.refresh_catalogue(true, true).await;
        self.refresh_status().await;
        self.send_status();
        self.send_catalogue();
        let mut tick = tokio::time::interval(self.host_poll);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tokio::select! {
                r = requests.recv() => {
                    let Some(r) = r else { return };
                    self.pending.push_back(r);
                    self.drain_pending(&mut requests).await;
                }
                e = next_event(&mut events) => match e {
                    Some(e) => self.event(e).await,
                    None => events = None,
                },
                _ = tick.tick() => {
                    self.refresh_host().await;
                    self.send_status();
                    self.pin_at_poll().await;
                }
            }
        }
    }

    /// Handles the pending requests, a run of snapshots as its last one.
    async fn drain_pending(&mut self, requests: &mut UnboundedReceiver<MicRequest>) {
        while let Some(r) = self.pending.pop_front() {
            match r {
                MicRequest::Snapshot(mut text) => {
                    // The snapshots queued behind this one replace it.
                    loop {
                        while let Ok(more) = requests.try_recv() {
                            self.pending.push_back(more);
                        }
                        match self.pending.front() {
                            Some(MicRequest::Snapshot(_)) => {
                                if let Some(MicRequest::Snapshot(t)) = self.pending.pop_front() {
                                    text = t;
                                }
                            }
                            _ => break,
                        }
                    }
                    self.snapshot(text).await;
                }
                other => self.request(other).await,
            }
        }
    }

    /// Makes `call`, starting the worker for it when [`Call::activates`].
    async fn call(&self, call: Call) -> Result<String, WorkerError> {
        let activate = call.activates();
        self.call_as(call, activate).await
    }

    /// Makes `call`, never starting the worker for it.
    async fn call_running(&self, call: Call) -> Result<String, WorkerError> {
        self.call_as(call, false).await
    }

    async fn call_as(&self, call: Call, activate: bool) -> Result<String, WorkerError> {
        let name = format!("{call:?}");
        let activate = activate && call.activates();
        match tokio::time::timeout(CALL_TIMEOUT, self.worker.call(call, activate)).await {
            Ok(r) => r,
            Err(_) => Err(WorkerError {
                unavailable: false,
                unknown_method: false,
                message: format!(
                    "the audio worker did not answer {} within {} s",
                    name.split('(').next().unwrap_or(&name),
                    CALL_TIMEOUT.as_secs()
                ),
            }),
        }
    }

    async fn unit<T>(
        &self,
        f: impl Future<Output = Result<T, UnitError>>,
        what: &str,
    ) -> Result<T, UnitError> {
        tokio::time::timeout(CALL_TIMEOUT, f)
            .await
            .unwrap_or_else(|_| {
                Err(UnitError::Failed(format!(
                    "the service manager did not answer ({what}) within {} s",
                    CALL_TIMEOUT.as_secs()
                )))
            })
    }

    /// Writes `text` to the snapshot file and pushes it to a running
    /// worker (then reads its settings again: what is enabled may follow).
    async fn snapshot(&mut self, text: String) {
        if let Some(path) = &self.snapshot_path {
            let path = path.clone();
            let bytes = text.clone().into_bytes();
            let written = tokio::task::spawn_blocking(move || {
                obc_core::config_d::write_atomically(&path, &bytes).map_err(|e| (path, e))
            })
            .await;
            match written {
                Ok(Ok(())) => debug!("audio settings written"),
                Ok(Err((path, e))) => warning!(
                    "the audio settings {} cannot be written ({e}): the audio worker reads the \
                     previous ones at its next start",
                    path.display()
                ),
                Err(e) => warning!("writing the audio settings failed: {e}"),
            }
        }
        if !self.worker.running().await {
            return;
        }
        match self.call_running(Call::ApplySnapshot(text)).await {
            Ok(_) => self.refresh_catalogue(false, false).await,
            Err(e) => debug!("ApplySnapshot: {}", e.message),
        }
    }

    async fn request(&mut self, r: MicRequest) {
        match r {
            MicRequest::Snapshot(text) => self.snapshot(text).await,
            MicRequest::Host {
                on,
                previous,
                reply,
            } => {
                let (restored, result) = if on {
                    (false, self.host_on_now().await)
                } else {
                    self.host_off_now(previous).await
                };
                self.refresh_host().await;
                self.send_status();
                self.send(MicEvent::HostDone {
                    restored,
                    result,
                    reply,
                });
                // A mic host that came on captures the device chosen.
                self.pin().await;
            }
            MicRequest::MakeDefault { target, reply } => {
                match self.call(Call::MakeDefault(target)).await {
                    Ok(text) => {
                        let doc = serde_json::from_str::<Json>(&text).ok();
                        let name = |key: &str| {
                            doc.as_ref()
                                .and_then(|v| v.get(key)?.as_str().map(str::to_string))
                        };
                        let previous = name("previous").unwrap_or_default();
                        let in_force = name("in_force").filter(|n| !n.is_empty());
                        let already = doc
                            .as_ref()
                            .and_then(|v| v.get("already")?.as_bool())
                            .unwrap_or(false);
                        // Ours the configured default already: `previous`
                        // is the pin in force, the one known here stays.
                        if !previous.is_empty() && !(already && self.made_previous.is_some()) {
                            self.made_previous = Some(previous.clone());
                        }
                        self.default_made = true;
                        info!(
                            "the Omarchy Broadcast Mic is the default microphone (previously {})",
                            if previous.is_empty() {
                                "not known"
                            } else {
                                previous.as_str()
                            }
                        );
                        self.send(MicEvent::DefaultMade {
                            previous,
                            in_force,
                            already,
                            reply,
                        });
                    }
                    Err(e) => {
                        let _ =
                            reply.send(Err(e.to_ipc("the default microphone cannot be changed")));
                    }
                }
            }
            MicRequest::Repin { target } => {
                self.pin_target = Some(target);
                self.pin_soon();
                // Queued before the start (the first snapshot's): pinned
                // once the first status is out, which a worker starting
                // for the pin would hold up.
                if self.sent_status.is_none() {
                    return;
                }
                self.refresh_host().await;
                self.pin().await;
            }
            MicRequest::RestoreDefault { previous, reply } => {
                let result = self
                    .call(Call::RestoreDefault(previous.clone()))
                    .await
                    .map(|_| {
                        self.made_previous = None;
                        self.default_made = false;
                        info!("the default microphone is {previous:?} again")
                    })
                    .map_err(|e| e.to_ipc("the previous default microphone cannot be restored"));
                let restored = result.is_ok();
                self.send(MicEvent::DefaultRestored { result, reply });
                // The restore unpinned the capture side: a device chosen
                // is captured again (with `default` it stays unpinned).
                if restored && self.pin_target.as_deref().is_some_and(|t| !t.is_empty()) {
                    self.pin_soon();
                    self.pin().await;
                }
            }
            MicRequest::ResetModule(id) => match self.call(Call::ResetModule(id.clone())).await {
                Ok(was) => {
                    if was == "true" {
                        info!("audio module {id}: enabled again after its crashes");
                    }
                    self.refresh_catalogue(false, false).await;
                }
                Err(e) => debug!("ResetModule {id}: {}", e.message),
            },
        }
    }

    /// `mic on`: refused while an ADR 0014 unit runs; else the mic host
    /// enabled and started.
    async fn host_on_now(&mut self) -> Result<(), IpcError> {
        for legacy in LEGACY_UNITS {
            match self.unit(self.units.active(legacy), legacy).await {
                Ok(true) => {
                    return Err(IpcError::Unavailable(format!(
                        "{legacy} is running and provides the Omarchy Broadcast Mic: stop it \
                         first (systemctl --user disable --now {legacy})"
                    )));
                }
                Ok(false) => {}
                Err(e) => return Err(e.to_ipc()),
            }
        }
        self.unit(self.units.enable_now(HOST_UNIT), HOST_UNIT)
            .await
            .map_err(|e| e.to_ipc())?;
        info!("microphone on: {HOST_UNIT} enabled and started");
        Ok(())
    }

    /// `mic off`: the previous default made the default again, then the
    /// mic host stopped and disabled. The restore is asked for whatever
    /// was recorded when the request was queued (a "Use as default
    /// microphone" may have completed since): the worker acts only while
    /// the Omarchy Broadcast Mic is the default, the previous default
    /// being the one recorded (the controller's, else the one the worker
    /// named here), else WirePlumber's choice. A restore that fails does
    /// not keep the mic on: the applications move to another microphone
    /// as it goes.
    async fn host_off_now(&mut self, previous: Option<String>) -> (bool, Result<(), IpcError>) {
        let mut restored = false;
        let prev = previous
            .filter(|p| !p.is_empty())
            .or_else(|| self.made_previous.clone())
            .unwrap_or_default();
        match self.call(Call::RestoreDefault(prev.clone())).await {
            Ok(_) => {
                restored = true;
                self.made_previous = None;
                self.default_made = false;
                info!(
                    "the default microphone is {} (if it was the Omarchy Broadcast Mic)",
                    if prev.is_empty() {
                        "WirePlumber's choice".to_string()
                    } else {
                        format!("{prev:?}")
                    }
                );
            }
            Err(e) if e.unavailable => debug!(
                "the default microphone cannot be restored ({}): turning the microphone off \
                 all the same",
                e.message
            ),
            Err(e) => warning!(
                "the previous default microphone {prev:?} cannot be restored ({}): turning \
                 the microphone off all the same",
                e.message
            ),
        }
        let result = self
            .unit(self.units.disable_now(HOST_UNIT), HOST_UNIT)
            .await
            .map(|()| info!("microphone off: {HOST_UNIT} stopped and disabled"))
            .map_err(|e| e.to_ipc());
        (restored, result)
    }

    async fn event(&mut self, e: WorkerEvent) {
        match e {
            WorkerEvent::Owner(true) => {
                debug!("the audio worker runs");
                self.refresh_status().await;
                self.refresh_catalogue(false, true).await;
                self.send_status();
                self.pin().await;
            }
            WorkerEvent::Owner(false) => {
                debug!("the audio worker is gone");
                self.worker_status = None;
                self.capture_seen = None;
                self.send_status();
            }
            WorkerEvent::Status(text) => {
                match serde_json::from_str::<Json>(&text) {
                    Ok(v) => {
                        self.worker_status = Some(v);
                        self.follow_capture();
                    }
                    Err(e) => debug!("StatusChanged: not JSON ({e})"),
                }
                self.send_status();
                // What the stages run on decides what is greyed.
                self.refresh_catalogue(false, false).await;
                // A change of the gain alone reaches the controller even
                // when the worker's catalogue cannot be read now.
                self.send_catalogue();
                self.pin().await;
            }
            WorkerEvent::Stats(overruns, misses, voice) => {
                if self
                    .sent_status
                    .as_ref()
                    .is_some_and(|s| s.state == "active")
                {
                    self.published.mic_stats(overruns, misses, voice);
                }
            }
        }
    }

    /// Reads whether the mic host runs; one that comes on is due to have
    /// its capture side pinned ([`Agent::pin`]).
    async fn refresh_host(&mut self) {
        let was = self.host_on;
        self.host_on = match self.unit(self.units.active(HOST_UNIT), HOST_UNIT).await {
            Ok(on) => on,
            Err(e) => {
                debug!("{HOST_UNIT}: {e:?}");
                false
            }
        };
        if self.host_on && !was {
            self.pin_soon();
        }
    }

    /// The capture side is due to be pinned, tried at once.
    fn pin_soon(&mut self) {
        self.pin_due = true;
        self.pin_failures = 0;
    }

    /// A new capture node (the worker's `capture_serial` moved, or was
    /// first seen since the worker started) is due to be pinned: the mic
    /// host or PipeWire restarted, maybe between two host polls.
    fn follow_capture(&mut self) {
        let Some(serial) = self
            .worker_status
            .as_ref()
            .and_then(|s| s.get("capture_serial")?.as_u64())
        else {
            return;
        };
        if self.capture_seen != Some(serial) {
            self.capture_seen = Some(serial);
            self.pin_soon();
        }
    }

    /// [`Agent::pin`] at a host poll: also a pin that failed is tried
    /// again.
    async fn pin_at_poll(&mut self) {
        self.pin_now(true).await;
    }

    /// Pins the capture side to the device chosen, when that is due and
    /// the controller told it, while the mic host runs (the worker's `Pin`,
    /// whether or not the Omarchy Broadcast Mic is the default; it never
    /// changes the default). A pin that fails is tried again at each host
    /// poll, up to [`PIN_RETRIES`] times, then only logged once: a device
    /// that is not connected fails nothing else. A worker without `Pin`
    /// (an older one, still running) is asked as before
    /// ([`Agent::pin_as_before`]).
    async fn pin(&mut self) {
        self.pin_now(false).await;
    }

    async fn pin_now(&mut self, at_poll: bool) {
        if !self.pin_due || (self.pin_failures > 0 && !at_poll) {
            return;
        }
        let Some(target) = self.pin_target.clone().filter(|_| self.host_on) else {
            // Due again when the host comes on.
            self.pin_due = false;
            return;
        };
        let what = pinned_to(&target);
        match self.call(Call::Pin(target.clone())).await {
            Ok(_) => {
                self.pin_due = false;
                if target.is_empty() {
                    debug!("the Omarchy Broadcast Mic captures the default microphone");
                } else {
                    info!("the Omarchy Broadcast Mic captures {what}");
                }
            }
            Err(e) if e.unknown_method => {
                self.pin_due = false;
                self.pin_as_before(target).await;
            }
            Err(e) if self.pin_failures < PIN_RETRIES => {
                self.pin_failures += 1;
                debug!(
                    "the capture side cannot be pinned to {what} yet ({}): trying again",
                    e.message
                );
            }
            Err(e) => {
                self.pin_due = false;
                if e.unavailable {
                    debug!(
                        "the capture side cannot be pinned to {what} ({})",
                        e.message
                    );
                } else {
                    warning!(
                        "the capture side cannot be pinned to {what} ({})",
                        e.message
                    );
                }
            }
        }
    }

    /// A worker older than `Pin`: the capture side is moved with
    /// `MakeDefault`, idempotent (it only moves the pin, and never names
    /// our own node as the previous default) only while ours is the default
    /// as recorded here; else it is left as it is until the worker runs
    /// the new version (it exits when idle).
    async fn pin_as_before(&mut self, target: String) {
        let what = pinned_to(&target);
        if !self.default_made {
            debug!(
                "the audio worker cannot pin the capture side (an older version runs): {what} \
                 is captured once it is restarted"
            );
            return;
        }
        match self.call(Call::MakeDefault(target)).await {
            Ok(_) => info!("the Omarchy Broadcast Mic captures {what} now"),
            Err(e) if e.unavailable => {
                debug!(
                    "the capture side cannot be pinned to {what} ({})",
                    e.message
                )
            }
            Err(e) => warning!(
                "the capture side cannot be pinned to {what} ({})",
                e.message
            ),
        }
    }

    /// Reads the worker's status, while it runs.
    async fn refresh_status(&mut self) {
        if !self.worker.running().await {
            self.worker_status = None;
            return;
        }
        self.worker_status = match self.call_running(Call::GetStatus).await {
            Ok(text) => serde_json::from_str(&text).ok(),
            Err(e) => {
                debug!("GetStatus: {}", e.message);
                None
            }
        };
        self.follow_capture();
    }

    /// Reads the worker's modules and settings (and its devices, with
    /// `devices`), starting it when `activate`; else only while it runs,
    /// every call without activation (a worker that exits meanwhile is not
    /// started again for it).
    async fn refresh_catalogue(&mut self, activate: bool, devices: bool) {
        if !activate && !self.worker.running().await {
            return;
        }
        let modules = match self.call_as(Call::GetModules, activate).await {
            Ok(text) => text,
            Err(e) => {
                debug!("the audio worker's modules cannot be read: {}", e.message);
                return;
            }
        };
        let mut next = self.catalogue.clone();
        next.known = true;
        if let Ok(v) = serde_json::from_str::<Json>(&modules) {
            next.modules = parse_modules(&v);
        }
        match self.call_as(Call::GetSettings, activate).await {
            Ok(text) => {
                if let Ok(v) = serde_json::from_str::<Json>(&text) {
                    next.groups = parse_groups(&v);
                }
            }
            Err(e) => debug!("GetSettings: {}", e.message),
        }
        if devices {
            match self.call_as(Call::GetDevices, activate).await {
                Ok(text) => {
                    if let Ok(v) = serde_json::from_str::<Json>(&text) {
                        next.devices = parse_devices(&v);
                    }
                }
                Err(e) => debug!("GetDevices: {}", e.message),
            }
        }
        self.catalogue = next;
        self.send_catalogue();
    }

    /// Tells the controller the catalogue, once the worker answered and
    /// when it changed, its hardware gain that of the worker's last status
    /// (kept while no worker runs, and while the worker's is pending: the
    /// device keeps its gain meanwhile).
    fn send_catalogue(&mut self) {
        if let Some(status) = &self.worker_status
            && !gain_pending(status)
        {
            self.catalogue.hardware_gain = parse_hardware_gain(status);
        }
        if self.catalogue.known && self.sent_catalogue.as_ref() != Some(&self.catalogue) {
            self.sent_catalogue = Some(self.catalogue.clone());
            self.send(MicEvent::Catalogue(self.catalogue.clone()));
        }
    }

    /// Tells the controller the `Mic*` properties, when they changed.
    fn send_status(&mut self) {
        let s = super::status(self.host_on, self.worker_status.as_ref());
        if self.sent_status.as_ref() != Some(&s) {
            self.sent_status = Some(s.clone());
            self.send(MicEvent::Status(s));
        }
    }

    fn send(&self, e: MicEvent) {
        let _ = self.commands.send(Command::Mic(e));
    }
}

/// How a pin's target is named in the log.
fn pinned_to(target: &str) -> String {
    if target.is_empty() {
        "the default microphone".into()
    } else {
        format!("{target:?}")
    }
}

async fn next_event(events: &mut Option<UnboundedReceiver<WorkerEvent>>) -> Option<WorkerEvent> {
    match events {
        Some(e) => e.recv().await,
        None => std::future::pending().await,
    }
}

// ---- the real worker and service manager ----

/// Whether `e` says the name has no owner or cannot be activated, or that
/// the worker has no PipeWire for the call.
fn unavailable(e: &zbus::Error) -> bool {
    const NAMES: [&str; 3] = [
        "org.freedesktop.DBus.Error.NameHasNoOwner",
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NotSupported",
    ];
    match e {
        zbus::Error::MethodError(name, _, _) => NAMES.contains(&name.as_str()),
        zbus::Error::FDO(f) => NAMES.contains(&f.name().as_str()),
        _ => false,
    }
}

/// Whether `e` says the worker has no such method (an older one).
fn unknown_method(e: &zbus::Error) -> bool {
    const NAME: &str = "org.freedesktop.DBus.Error.UnknownMethod";
    match e {
        zbus::Error::MethodError(name, _, _) => name.as_str() == NAME,
        zbus::Error::FDO(f) => f.name().as_str() == NAME,
        _ => false,
    }
}

/// The message of `e`: the remote error's text when there is one.
fn message(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(m), _) => m.clone(),
        other => other.to_string(),
    }
}

/// The audio worker on the session bus.
#[derive(Clone)]
pub struct BusWorker {
    conn: Connection,
}

impl BusWorker {
    pub fn new(conn: Connection) -> Self {
        BusWorker { conn }
    }

    async fn proxy(&self) -> zbus::Result<zbus::Proxy<'static>> {
        zbus::proxy::Builder::new(&self.conn)
            .destination(WORKER_BUS_NAME)
            .path(WORKER_PATH)
            .interface(WORKER_INTERFACE)
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    async fn invoke(&self, call: Call, activate: bool) -> zbus::Result<String> {
        let proxy = self.proxy().await?;
        let flags = if activate && call.activates() {
            MethodFlags::empty()
        } else {
            MethodFlags::NO_AUTO_START
        };
        let text = |r: Option<String>| r.unwrap_or_default();
        Ok(match call {
            Call::GetStatus => text(proxy.call_with_flags("GetStatus", flags, &()).await?),
            Call::GetSettings => text(proxy.call_with_flags("GetSettings", flags, &()).await?),
            Call::GetModules => text(proxy.call_with_flags("GetModules", flags, &()).await?),
            Call::GetDevices => text(proxy.call_with_flags("GetDevices", flags, &()).await?),
            Call::ApplySnapshot(s) => {
                proxy
                    .call_with_flags::<_, _, ()>("ApplySnapshot", flags, &(s,))
                    .await?;
                String::new()
            }
            Call::MakeDefault(t) => text(proxy.call_with_flags("MakeDefault", flags, &(t,)).await?),
            Call::RestoreDefault(p) => {
                proxy
                    .call_with_flags::<_, _, ()>("RestoreDefault", flags, &(p,))
                    .await?;
                String::new()
            }
            Call::Pin(t) => {
                proxy
                    .call_with_flags::<_, _, ()>("Pin", flags, &(t,))
                    .await?;
                String::new()
            }
            Call::ResetModule(id) => {
                let was: Option<bool> = proxy.call_with_flags("ResetModule", flags, &(id,)).await?;
                was.unwrap_or(false).to_string()
            }
        })
    }
}

impl Worker for BusWorker {
    async fn running(&self) -> bool {
        let Ok(dbus) = zbus::fdo::DBusProxy::builder(&self.conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
        else {
            return false;
        };
        let Ok(name) = zbus::names::BusName::try_from(WORKER_BUS_NAME) else {
            return false;
        };
        dbus.name_has_owner(name).await.unwrap_or(false)
    }

    async fn call(&self, call: Call, activate: bool) -> Result<String, WorkerError> {
        self.invoke(call, activate).await.map_err(|e| WorkerError {
            unavailable: unavailable(&e),
            unknown_method: unknown_method(&e),
            message: message(&e),
        })
    }
}

/// Follows the worker on `conn` (its name's owner, `StatusChanged`,
/// `Stats`) into `tx`, until the bus closes or nobody listens. Subscribing
/// never starts the worker. Once subscribed, the owner is told as it is
/// then (an [`WorkerEvent::Owner`]), so that no change is missed.
pub async fn watch_worker(conn: Connection, tx: UnboundedSender<WorkerEvent>) {
    let res: zbus::Result<()> = async {
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let mut owners = dbus
            .receive_name_owner_changed_with_args(&[(0, WORKER_BUS_NAME)])
            .await?;
        let proxy = BusWorker::new(conn.clone()).proxy().await?;
        let mut status = proxy.receive_signal("StatusChanged").await?;
        let mut stats = proxy.receive_signal("Stats").await?;
        let name = zbus::names::BusName::try_from(WORKER_BUS_NAME)?;
        let owned = dbus.name_has_owner(name).await?;
        if tx.send(WorkerEvent::Owner(owned)).is_err() {
            return Ok(());
        }
        loop {
            let event = tokio::select! {
                o = owners.next() => {
                    let Some(o) = o else { return Ok(()) };
                    let Ok(args) = o.args() else { continue };
                    WorkerEvent::Owner(args.new_owner().is_some())
                }
                s = status.next() => {
                    let Some(m) = s else { return Ok(()) };
                    let Ok(text) = m.body().deserialize::<String>() else { continue };
                    WorkerEvent::Status(text)
                }
                s = stats.next() => {
                    let Some(m) = s else { return Ok(()) };
                    let Ok((o, mi, v)) = m.body().deserialize::<(u32, u32, f64)>() else {
                        continue;
                    };
                    WorkerEvent::Stats(o, mi, v)
                }
            };
            if tx.send(event).is_err() {
                return Ok(());
            }
        }
    }
    .await;
    if let Err(e) = res {
        warning!("the audio worker cannot be followed: {e}");
    }
}

/// The systemd user manager (`org.freedesktop.systemd1`) on the session
/// bus.
#[derive(Clone)]
pub struct SystemdUnits {
    conn: Connection,
}

const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";

impl SystemdUnits {
    pub fn new(conn: Connection) -> Self {
        SystemdUnits { conn }
    }

    async fn manager(&self) -> zbus::Result<zbus::Proxy<'static>> {
        zbus::proxy::Builder::new(&self.conn)
            .destination(SYSTEMD)
            .path(SYSTEMD_PATH)
            .interface(SYSTEMD_MANAGER)
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    /// Runs `method` (`StartUnit` or `StopUnit`) for `unit` and waits, up
    /// to [`JOB_WAIT`], for its job to be removed: `Ok` when its result is
    /// `done`, else an error naming the result. The signals are asked for
    /// (`Subscribe`) and followed before the job is queued, so that a job
    /// that ends at once is not missed. A job still running at the bound
    /// is judged by the unit's state (`StartUnit`: active or activating).
    async fn run_job(
        &self,
        m: &zbus::Proxy<'static>,
        unit: &str,
        method: &str,
        what: &str,
    ) -> Result<(), UnitError> {
        // Already subscribed (an earlier job) is not a failure; another
        // refusal leaves the bound and the unit's state to judge.
        if let Err(e) = m.call::<_, _, ()>("Subscribe", &()).await {
            debug!("{unit}: Subscribe: {}", message(&e));
        }
        let mut removed = m
            .receive_signal("JobRemoved")
            .await
            .map_err(|e| Self::error(unit, what, e))?;
        let job: zbus::OwnedObjectPath = m
            .call(method, &(unit, "replace"))
            .await
            .map_err(|e| Self::error(unit, what, e))?;
        let wait = async {
            while let Some(msg) = removed.next().await {
                let Ok((_, path, _, result)) =
                    msg.body()
                        .deserialize::<(u32, zbus::OwnedObjectPath, String, String)>()
                else {
                    continue;
                };
                if path == job {
                    return Some(result);
                }
            }
            None
        };
        match tokio::time::timeout(JOB_WAIT, wait).await {
            Ok(Some(result)) if result == "done" => Ok(()),
            Ok(Some(result)) => Err(UnitError::Failed(format!(
                "{unit}: {what} failed ({result}); see journalctl --user -u {unit}"
            ))),
            Ok(None) | Err(_) => {
                let active = self.active(unit).await?;
                if active == (method == "StartUnit") {
                    debug!("{unit}: the {method} job did not report; the unit's state agrees");
                    Ok(())
                } else {
                    Err(UnitError::Failed(format!(
                        "{unit}: {what} did not finish within {} s; see journalctl --user -u \
                         {unit}",
                        JOB_WAIT.as_secs()
                    )))
                }
            }
        }
    }

    fn error(unit: &str, what: &str, e: zbus::Error) -> UnitError {
        let msg = format!("{unit}: {what} failed: {}", message(&e));
        if unavailable(&e) {
            UnitError::Unavailable(format!(
                "{unit}: there is no systemd user manager on the session bus ({})",
                message(&e)
            ))
        } else {
            UnitError::Failed(msg)
        }
    }
}

impl Units for SystemdUnits {
    async fn active(&self, unit: &str) -> Result<bool, UnitError> {
        let m = self
            .manager()
            .await
            .map_err(|e| Self::error(unit, "reading", e))?;
        // GetUnit does not load a unit: one not loaded is not active.
        let path: zbus::OwnedObjectPath = match m.call("GetUnit", &(unit,)).await {
            Ok(p) => p,
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
            {
                return Ok(false);
            }
            Err(e) => return Err(Self::error(unit, "reading", e)),
        };
        let props = zbus::fdo::PropertiesProxy::builder(&self.conn)
            .destination(SYSTEMD)
            .path(path)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|e| Self::error(unit, "reading", e))?;
        let iface =
            zbus::names::InterfaceName::from_static_str_unchecked("org.freedesktop.systemd1.Unit");
        let state = props
            .get(iface, "ActiveState")
            .await
            .map_err(|e| Self::error(unit, "reading", e.into()))?;
        let state = String::try_from(state)
            .map_err(|e| UnitError::Failed(format!("{unit}: its state cannot be read: {e}")))?;
        Ok(matches!(
            state.as_str(),
            "active" | "activating" | "reloading"
        ))
    }

    async fn enable_now(&self, unit: &str) -> Result<(), UnitError> {
        let m = self
            .manager()
            .await
            .map_err(|e| Self::error(unit, "enabling", e))?;
        let _: (bool, Vec<(String, String, String)>) = m
            .call("EnableUnitFiles", &(vec![unit], false, false))
            .await
            .map_err(|e| Self::error(unit, "enabling", e))?;
        let _: () = m
            .call("Reload", &())
            .await
            .map_err(|e| Self::error(unit, "reloading the units", e))?;
        self.run_job(&m, unit, "StartUnit", "starting").await
    }

    async fn disable_now(&self, unit: &str) -> Result<(), UnitError> {
        let m = self
            .manager()
            .await
            .map_err(|e| Self::error(unit, "stopping", e))?;
        // Disabled even when the stop fails, so that it does not come back
        // at the next login; the stop's failure is the answer.
        let stopped = self.run_job(&m, unit, "StopUnit", "stopping").await;
        let _: Vec<(String, String, String)> = m
            .call("DisableUnitFiles", &(vec![unit], false))
            .await
            .map_err(|e| Self::error(unit, "disabling", e))?;
        let _: () = m
            .call("Reload", &())
            .await
            .map_err(|e| Self::error(unit, "reloading the units", e))?;
        stopped
    }
}

/// Starts the agent on the current runtime with the real worker and
/// service manager on `conn`, writing the snapshot to `snapshot_path`.
pub fn spawn(
    conn: Connection,
    requests: UnboundedReceiver<MicRequest>,
    commands: Sender<Command>,
    published: Published,
    snapshot_path: Option<PathBuf>,
) {
    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(watch_worker(conn.clone(), tx));
    let agent = Agent::new(
        BusWorker::new(conn.clone()),
        SystemdUnits::new(conn),
        commands,
        published,
        snapshot_path,
    );
    tokio::spawn(agent.run(requests, events));
}
