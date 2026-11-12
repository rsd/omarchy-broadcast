//! The processing thread (not real time): it serves one mic plugin at a
//! time over the shared-memory rings of `obc-audio`, runs each input block
//! through the loaded [`Chain`] and answers it with an output block of the
//! same sequence number, in order.
//!
//! * A second plugin is refused ([`RefuseReason::Busy`]) while one is
//!   connected.
//! * The chain is loaded in a thread of its own the first time a plugin
//!   connects, and again whenever the snapshot changes what a load
//!   depends on (the stages, their modules, their load-time parameters).
//!   Meanwhile the previous chain keeps processing; once the new one has
//!   warmed up, one block is crossfaded from the old to the new and the
//!   old is unloaded. If the chain in use fails while the new one warms
//!   up, the new one takes over at once. A new load that fails keeps the
//!   chain in use (unless it runs a module the new plan no longer has).
//!   Without a chain (loading the first one, processing off, no stage
//!   on), blocks are answered raw ([`SLOT_FLAG_RAW`]), and so are the
//!   warm-up blocks after a load or a new stream. A change of live
//!   parameters only applies them to the next block.
//! * Threads: a chain is loaded on a loader thread, run on this one and
//!   unloaded on a retire thread (with the crash ledger's writes), so a
//!   switch never holds a block up. **A module's `audio_instance_load`,
//!   `audio_instance_process` and `instance_unload` of one instance may
//!   therefore run on three different threads** (never at the same time
//!   for one instance), and instances of one module are loaded, run and
//!   unloaded concurrently.
//! * The crash ledger's marker of a module is set before its chain loads
//!   and cleared when the chain is unloaded cleanly. At exit (a signal, or
//!   idle) a load in flight is waited for ([`EXIT_WAIT`]) and unloaded, so
//!   a clean exit leaves no marker; one still running after that keeps its
//!   marker, as a hang.
//! * The watchdog: a call into a module on this thread (a block, a reset)
//!   that has not returned after [`WATCHDOG_TIMEOUT`] aborts the process.
//!   The module's marker stays (a strike in the crash ledger, as for a
//!   crash), and systemd starts the worker again; meanwhile the plugin
//!   plays raw.
//! * The GPU ([`GpuGate`]): `gpu_policy = "with-camera"` reads the
//!   snapshot's `camera_active` when a mic plugin connects (the session
//!   start) and keeps that answer until it disconnects (a change of the
//!   policy itself applies at once); and GPU stages are planned only while
//!   an application records (the consumers, debounced, are not all level
//!   meters). Unknown consumers (no PipeWire) do not close the gate; a
//!   PipeWire connection that has not synced yet does
//!   ([`GpuGate::Pending`]).
//! * The pipeline depth is negotiated at connect for the deepest chain the
//!   session may need: as if the consumers gate were open (the GPU policy
//!   alone decides), so that a recorder starting does not need a new
//!   depth. If the plan in force needs more than the connection has (the
//!   policy or the snapshot changed), the connection is dropped and the
//!   plugin, which plays raw meanwhile, connects again with the new depth
//!   (the session, and its camera answer, go on).
//! * Latency: the delay the microphone adds while connected (the
//!   transport delay the plugin publishes, else `transport_delay(480,
//!   depth)`, plus the chain's algorithmic delay) is set as the node's
//!   `ProcessLatency` whenever it changes.
//! * Exit on idle: no input block and no D-Bus call for `idle_exit_s`
//!   seconds (0: never), and no D-Bus call in flight: the bus name is
//!   released first, then, if still idle (no call came meanwhile), the
//!   connection is closed and the thread ends; otherwise the name is taken
//!   again and the worker goes on. The shutdown flag ends it too.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use obc_audio::BLOCK_SAMPLES;
use obc_audio::proto::RefuseReason;
use obc_audio::server::{Endpoint, Handshake};
use obc_audio::shm::{SLOT_FLAG_NEW_STREAM, SLOT_FLAG_RAW, WORKER_FLAG_PROCESSING};

use crate::chain::{Chain, Event, crossfade, load_group};
use crate::host::{GpuGate, Host, LoadKey, Plan};
use crate::listener::Socket;
use crate::pipewire::PipeWireControl;
use crate::snapshot::{GpuPolicy, Snapshot};
use crate::status::{StageStatus, State, Status};
use crate::{debug, error, info, warning};

/// How long a plugin has to send its hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest wait of the loop (the heartbeat needs one every 100 ms).
const TICK: Duration = Duration::from_millis(10);

/// How long a call into a module on the processing thread may take before
/// the watchdog aborts the worker: longer than the plugin's own fallback
/// (it plays raw after two seconds without a heartbeat).
pub const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the exit waits for a load in flight, and then for the
/// unloads.
pub const EXIT_WAIT: Duration = Duration::from_secs(5);

/// How long a plugin dropped for a deeper pipeline has to connect again
/// and go on with its session.
const RECONNECT_GRACE: Duration = Duration::from_secs(5);

/// How long an idle exit waits for the bus name to be released.
const RELEASE_WAIT: Duration = Duration::from_secs(2);

/// The text of a refusal while a plugin is connected.
pub const REASON_BUSY: &str = "the audio worker already serves a microphone plugin";

/// Locks `m`, whatever a panicking holder left.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the processing thread shares with the D-Bus side.
pub struct Shared {
    /// The snapshot in force (never persisted by the worker).
    pub snapshot: Mutex<Snapshot>,
    /// Moves at every snapshot applied.
    pub generation: AtomicU64,
    /// The modules.
    pub host: Mutex<Host>,
    /// The status, for `GetStatus` and `StatusChanged`.
    pub status: tokio::sync::watch::Sender<Status>,
    /// The end of the last D-Bus call.
    pub last_call: Mutex<Instant>,
    /// D-Bus calls being answered.
    pub in_flight: AtomicUsize,
    /// Stop now (a signal).
    pub shutdown: AtomicBool,
    /// PipeWire.
    pub pipewire: Box<dyn PipeWireControl>,
    /// The GPU gate the plan in force was made with.
    pub gpu_gate: Mutex<GpuGate>,
    /// The bus side, while the worker owns its name (for the idle exit).
    pub bus: Mutex<Option<tokio::sync::mpsc::UnboundedSender<BusCmd>>>,
}

/// What the processing thread asks of the bus side.
#[derive(Debug)]
pub enum BusCmd {
    /// Release the bus name, then answer.
    Release(Sender<()>),
    /// Take the name again (the worker stays).
    Reacquire,
}

/// A D-Bus call in flight, until dropped.
pub struct CallGuard<'a>(&'a Shared);

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        *lock(&self.0.last_call) = Instant::now();
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    /// The shared state over `host` and `snapshot`.
    pub fn new(host: Host, snapshot: Snapshot, pipewire: Box<dyn PipeWireControl>) -> Shared {
        let (status, _) = tokio::sync::watch::channel(Status::default());
        Shared {
            snapshot: Mutex::new(snapshot),
            generation: AtomicU64::new(0),
            host: Mutex::new(host),
            status,
            last_call: Mutex::new(Instant::now()),
            in_flight: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            pipewire,
            gpu_gate: Mutex::new(GpuGate::default()),
            bus: Mutex::new(None),
        }
    }

    /// Counts a D-Bus call in flight (and as activity) until the guard
    /// drops.
    pub fn call(&self) -> CallGuard<'_> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        *lock(&self.last_call) = Instant::now();
        CallGuard(self)
    }

    /// Applies `snapshot` live: the module choices now, the chain at the
    /// processing thread's next turn.
    pub fn apply_snapshot(&self, snapshot: Snapshot) {
        lock(&self.host).apply(&snapshot);
        *lock(&self.snapshot) = snapshot;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// Why the processing thread ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Idle for `idle_exit_s`.
    Idle,
    /// The shutdown flag.
    Shutdown,
}

/// A load the loader thread finished.
struct Loaded {
    key: LoadKey,
    chain: Chain,
    failures: Vec<(String, String)>,
}

/// The watchdog over the module calls of the processing thread (see the
/// module documentation).
struct Watchdog {
    epoch: Instant,
    /// Milliseconds since `epoch`, plus one, when the call in progress
    /// started; 0 while none is.
    busy_since: AtomicU64,
    stop: AtomicBool,
}

impl Watchdog {
    fn new() -> Watchdog {
        Watchdog {
            epoch: Instant::now(),
            busy_since: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX - 1) + 1
    }

    /// A call into a module starts.
    fn enter(&self) {
        self.busy_since.store(self.now_ms(), Ordering::SeqCst);
    }

    /// It returned.
    fn leave(&self) {
        self.busy_since.store(0, Ordering::SeqCst);
    }

    /// Whether the call in progress, if any, has run over `limit`.
    fn overdue(&self, limit: Duration) -> bool {
        let since = self.busy_since.load(Ordering::SeqCst);
        since != 0
            && self.now_ms().saturating_sub(since)
                > u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)
    }

    /// The watchdog thread's body: aborts the process on an overdue call.
    fn run(&self) {
        while !self.stop.load(Ordering::SeqCst) {
            if self.overdue(WATCHDOG_TIMEOUT) {
                error!(
                    "a module has not returned from processing a block for {} s: aborting \
                     the audio worker (its crash marker stays)",
                    WATCHDOG_TIMEOUT.as_secs()
                );
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Waits for `handle` to finish, at most `timeout`; whether it did.
fn join_within(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = handle.join();
    true
}

/// Unloads `chain` and clears its modules' crash markers.
fn unload(shared: &Shared, chain: Chain) {
    let ids = chain.unload();
    let mut host = lock(&shared.host);
    for id in ids {
        host.ledger_end(&id);
    }
}

/// The processing thread. See the module documentation.
pub struct Engine {
    shared: Arc<Shared>,
    socket: Socket,
    endpoint: Option<Endpoint>,
    /// The pipeline depth of the connection.
    depth: u32,
    active: Option<Chain>,
    pending: Option<Chain>,
    /// The load key of the chain installed last (active or pending).
    installed: Option<LoadKey>,
    loading: bool,
    loads_tx: Sender<Loaded>,
    loads_rx: Receiver<Loaded>,
    /// A plugin connected once: chains load from then on.
    armed: bool,
    generation: u64,
    plan: Plan,
    /// Stages that failed, with why, since the chain was installed.
    failures: BTreeMap<String, String>,
    last_input: Instant,
    published_latency: u32,
    /// The GPU policy and the camera's state at the session's start (or
    /// at the policy's last change in it), while a plugin is connected.
    session: Option<(GpuPolicy, bool)>,
    /// The gate the plan was made with.
    gate: GpuGate,
    /// The `ProcessLatency` asked for last.
    process_latency: Option<u32>,
    /// The loader thread of the load in flight.
    loader: Option<JoinHandle<()>>,
    /// The retire thread's queue and handle.
    retire_tx: Option<Sender<Chain>>,
    retirer: Option<JoinHandle<()>>,
    watchdog: Arc<Watchdog>,
    /// The connection was dropped for a deeper pipeline (when): the next
    /// one, within [`RECONNECT_GRACE`], goes on with the session.
    renegotiating: Option<Instant>,
    /// [`Engine::session_depth`] at the last plan, for the status.
    open_depth: u32,
}

impl Engine {
    /// The processing thread's state over `socket`.
    pub fn new(shared: Arc<Shared>, socket: Socket) -> Engine {
        let (loads_tx, loads_rx) = channel();
        let generation = shared.generation.load(Ordering::SeqCst);
        let (retire_tx, retire_rx) = channel::<Chain>();
        let retirer = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("obc-audio-retire".into())
                .spawn(move || {
                    while let Ok(chain) = retire_rx.recv() {
                        unload(&shared, chain);
                    }
                })
        };
        let (retire_tx, retirer) = match retirer {
            Ok(h) => (Some(retire_tx), Some(h)),
            Err(e) => {
                warning!(
                    "cannot start the retire thread ({e}): unloading on the processing thread"
                );
                (None, None)
            }
        };
        let mut e = Engine {
            shared,
            socket,
            endpoint: None,
            depth: 1,
            active: None,
            pending: None,
            installed: None,
            loading: false,
            loads_tx,
            loads_rx,
            armed: false,
            generation,
            plan: Plan::default(),
            failures: BTreeMap::new(),
            last_input: Instant::now(),
            published_latency: 0,
            session: None,
            gate: GpuGate::default(),
            process_latency: None,
            loader: None,
            retire_tx,
            retirer,
            watchdog: Arc::new(Watchdog::new()),
            renegotiating: None,
            open_depth: 1,
        };
        e.gate = e.gate_now(&e.snapshot());
        e.replan();
        e.publish_status();
        e
    }

    /// The GPU gate of the policy alone (the session's camera answer, not
    /// the consumers).
    fn policy_gate(&self, snapshot: &Snapshot) -> GpuGate {
        let (policy, camera) = self
            .session
            .unwrap_or((snapshot.gpu_policy(), snapshot.camera_active));
        match policy {
            GpuPolicy::Never => GpuGate::Never,
            GpuPolicy::WithCamera if !camera => GpuGate::CameraOff,
            _ => GpuGate::Allowed,
        }
    }

    /// The GPU gate now (see the module documentation).
    fn gate_now(&self, snapshot: &Snapshot) -> GpuGate {
        let gate = self.policy_gate(snapshot);
        if !gate.allows() {
            return gate;
        }
        let pipewire = &self.shared.pipewire;
        match pipewire.consumers() {
            Some(c) if c.real == 0 => GpuGate::NoRecorder,
            Some(_) => GpuGate::Allowed,
            // Connected, the graph not known yet: closed until it is.
            None if pipewire.consumers_pending() => GpuGate::Pending,
            None => GpuGate::Allowed,
        }
    }

    /// The pipeline depth of a session: the deepest the plan may need
    /// while the GPU policy allows (see the module documentation).
    fn session_depth(&self, snapshot: &Snapshot) -> u32 {
        let open = lock(&self.shared.host).plan(snapshot, self.policy_gate(snapshot));
        snapshot
            .pipeline_depth
            .blocks(open.gpu() || self.plan.gpu())
    }

    /// Drops the connection when the plan in force needs a deeper
    /// pipeline than it has: the plugin connects again.
    fn follow_depth(&mut self) {
        if self.endpoint.is_none() {
            // The plugin did not come back in time: the session is over.
            if self
                .renegotiating
                .is_some_and(|t| t.elapsed() > RECONNECT_GRACE)
            {
                self.renegotiating = None;
                self.session = None;
            }
            return;
        }
        let need = self.snapshot().pipeline_depth.blocks(self.plan.gpu());
        if need > self.depth {
            info!(
                "the chain needs a pipeline depth of {need} (the connection has {}): \
                 closing it so that the mic plugin connects again",
                self.depth
            );
            self.endpoint = None;
            self.renegotiating = Some(Instant::now());
        }
    }

    /// The transport delay of the connection: the plugin's, else the
    /// formula's for 480-sample quanta.
    fn transport_delay(&self) -> u32 {
        self.endpoint
            .as_ref()
            .and_then(Endpoint::transport_delay)
            .unwrap_or_else(|| obc_audio::transport_delay(BLOCK_SAMPLES as u32, self.depth))
    }

    /// A new plan for the snapshot and gate in force; when the loads stay
    /// the same, the live parameters apply now.
    fn replan(&mut self) {
        let snapshot = self.snapshot();
        self.plan = lock(&self.shared.host).plan(&snapshot, self.gate);
        *lock(&self.shared.gpu_gate) = self.gate;
        self.open_depth = self.session_depth(&snapshot);
        if self.installed.as_ref() == Some(&self.plan.load_key()) {
            for c in [self.active.as_mut(), self.pending.as_mut()]
                .into_iter()
                .flatten()
            {
                c.set_live(&self.plan.groups);
            }
        }
    }

    /// Follows the GPU gate: a new plan when it moved.
    fn follow_gate(&mut self) {
        let gate = self.gate_now(&self.snapshot());
        if gate != self.gate {
            debug!("GPU gate: {gate:?}");
            self.gate = gate;
            self.replan();
        }
    }

    /// Sets the microphone's `ProcessLatency` to the delay in force while
    /// connected (none while not), when it changed.
    fn follow_process_latency(&mut self) {
        let want = self
            .endpoint
            .as_ref()
            .map(|_| self.transport_delay() + self.published_latency);
        if want != self.process_latency {
            self.process_latency = want;
            self.shared.pipewire.set_latency(want);
        }
    }

    /// Runs until idle or shut down.
    pub fn run(mut self) -> Exit {
        let watchdog = {
            let w = self.watchdog.clone();
            std::thread::Builder::new()
                .name("obc-audio-watchdog".into())
                .spawn(move || w.run())
        };
        if let Err(e) = &watchdog {
            warning!("cannot start the watchdog: {e}");
        }
        let exit = loop {
            if self.shared.shutdown.load(Ordering::SeqCst) {
                break Exit::Shutdown;
            }
            self.follow_snapshot();
            self.follow_gate();
            self.follow_depth();
            self.drive_loads();
            self.wait_and_serve();
            self.follow_process_latency();
            self.publish_status();
            if self.idle() && self.confirm_idle() {
                break Exit::Idle;
            }
        };
        self.watchdog.stop.store(true, Ordering::SeqCst);
        if let Ok(w) = watchdog {
            let _ = w.join();
        }
        if self.endpoint.take().is_some() {
            info!("closing the connection to the mic plugin");
        }
        for c in [self.pending.take(), self.active.take()]
            .into_iter()
            .flatten()
        {
            self.retire(c);
        }
        self.finish_loads();
        drop(self.retire_tx.take());
        if let Some(r) = self.retirer.take()
            && !join_within(r, EXIT_WAIT)
        {
            warning!(
                "the modules did not unload within {} s: exiting anyway",
                EXIT_WAIT.as_secs()
            );
        }
        exit
    }

    /// At exit: waits for the load in flight (at most [`EXIT_WAIT`]) and
    /// unloads whatever loaded, so that no crash marker is left for a
    /// load that ended. A load still running keeps its marker.
    fn finish_loads(&mut self) {
        let mut waiting = self.loading;
        let end = Instant::now() + EXIT_WAIT;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let got = if waiting {
                self.loads_rx.recv_timeout(left)
            } else {
                self.loads_rx
                    .try_recv()
                    .map_err(|_| RecvTimeoutError::Disconnected)
            };
            match got {
                Ok(l) => {
                    waiting = false;
                    self.loading = false;
                    self.retire(l.chain);
                }
                Err(RecvTimeoutError::Timeout) => {
                    warning!(
                        "a chain load did not finish within {} s: exiting anyway (its crash \
                         marker stays)",
                        EXIT_WAIT.as_secs()
                    );
                    return;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if let Some(l) = self.loader.take() {
            let _ = join_within(l, Duration::from_millis(500));
        }
    }

    /// Idle: releases the bus name, then says whether the worker is still
    /// idle (no call came meanwhile); if not, takes the name again.
    fn confirm_idle(&mut self) -> bool {
        let Some(bus) = lock(&self.shared.bus).clone() else {
            return true;
        };
        let (tx, rx) = channel();
        if bus.send(BusCmd::Release(tx)).is_err() {
            return true;
        }
        let _ = rx.recv_timeout(RELEASE_WAIT);
        if self.idle() {
            return true;
        }
        info!("a call came while the worker was going idle: staying");
        let _ = bus.send(BusCmd::Reacquire);
        false
    }

    fn snapshot(&self) -> Snapshot {
        lock(&self.shared.snapshot).clone()
    }

    /// A new snapshot: a new plan; the live parameters apply now.
    fn follow_snapshot(&mut self) {
        let g = self.shared.generation.load(Ordering::SeqCst);
        if g == self.generation {
            return;
        }
        self.generation = g;
        let snapshot = self.snapshot();
        // A new policy applies at once; the camera's state only at the
        // next session.
        if let Some((policy, _)) = self.session
            && policy != snapshot.gpu_policy()
        {
            self.session = Some((snapshot.gpu_policy(), snapshot.camera_active));
        }
        self.gate = self.gate_now(&snapshot);
        self.replan();
    }

    /// Takes finished loads; starts the one the plan needs.
    fn drive_loads(&mut self) {
        while let Ok(l) = self.loads_rx.try_recv() {
            self.loading = false;
            if let Some(h) = self.loader.take() {
                // It sent its last message: it is ending.
                let _ = join_within(h, Duration::from_millis(100));
            }
            if l.key == self.plan.load_key() {
                self.install(l);
            } else {
                debug!("a chain loaded for settings since changed: unloading it");
                self.retire(l.chain);
            }
        }
        let want = self.plan.load_key();
        if !self.armed || self.loading || self.installed.as_ref() == Some(&want) {
            return;
        }
        if want.is_empty() {
            for c in [self.pending.take(), self.active.take()]
                .into_iter()
                .flatten()
            {
                self.retire(c);
            }
            self.failures.clear();
            self.installed = Some(want);
            return;
        }
        let modules: Vec<_> = {
            let host = lock(&self.shared.host);
            self.plan
                .groups
                .iter()
                .map(|g| (g.clone(), host.module(&g.module)))
                .collect()
        };
        self.loading = true;
        let shared = self.shared.clone();
        let tx = self.loads_tx.clone();
        let key = want;
        let spawned = std::thread::Builder::new()
            .name("obc-audio-load".into())
            .spawn(move || {
                let mut loads = Vec::new();
                let mut failures = Vec::new();
                for (plan, module) in modules {
                    let Some(module) = module else {
                        for s in &plan.stages {
                            failures.push((s.to_string(), "The module is not open".to_string()));
                        }
                        continue;
                    };
                    lock(&shared.host).ledger_begin(&plan.module);
                    let load = load_group(&plan, &module);
                    if !load.is_loaded() {
                        lock(&shared.host).ledger_end(&plan.module);
                    }
                    failures.extend(load.failures.iter().cloned());
                    loads.push((plan, load));
                }
                let chain = Chain::new(loads);
                let _ = tx.send(Loaded {
                    key,
                    chain,
                    failures,
                });
            });
        match spawned {
            Ok(h) => self.loader = Some(h),
            Err(e) => {
                warning!("cannot start a chain load: {e}");
                self.loading = false;
            }
        }
    }

    /// Installs a chain loaded for the plan in force: the active one when
    /// there is none, else the pending one, which takes over once warm.
    fn install(&mut self, l: Loaded) {
        self.installed = Some(l.key);
        self.failures.clear();
        for (stage, why) in &l.failures {
            warning!("stage {stage} cannot run: {why}");
            self.failures.insert(stage.clone(), why.clone());
        }
        if let Some(old) = self.pending.take() {
            self.retire(old);
        }
        if !l.chain.is_live() {
            // The chain in use goes on, unless it runs a module the plan
            // no longer has (disabled, or its stages gone to another).
            let planned: BTreeSet<&str> =
                self.plan.groups.iter().map(|g| g.module.as_str()).collect();
            let keep = self
                .active
                .as_ref()
                .is_some_and(|a| a.module_ids().iter().all(|m| planned.contains(m.as_str())));
            if keep {
                warning!("the new chain cannot run: the chain in use goes on");
            } else if let Some(c) = self.active.take() {
                self.retire(c);
            }
            self.retire(l.chain);
            return;
        }
        info!(
            "chain loaded: {} (latency {} samples)",
            describe(&l.chain.running()),
            l.chain.latency()
        );
        if self.active.is_some() {
            self.pending = Some(l.chain);
        } else {
            self.active = Some(l.chain);
        }
    }

    /// Unloads `chain` and clears its modules' crash markers, on the
    /// retire thread (here only when it could not start).
    fn retire(&self, chain: Chain) {
        let chain = match &self.retire_tx {
            Some(tx) => match tx.send(chain) {
                Ok(()) => return,
                Err(e) => e.0,
            },
            None => chain,
        };
        unload(&self.shared, chain);
    }

    /// Waits for a connection, input or a hang-up (at most [`TICK`]), then
    /// serves what came.
    fn wait_and_serve(&mut self) {
        use std::os::fd::AsFd;
        let mut fds = vec![obc_audio::sys::pollfd(
            self.socket.listener.as_fd(),
            libc::POLLIN,
        )];
        if let Some(ep) = &self.endpoint {
            fds.push(obc_audio::sys::pollfd(ep.socket_fd(), libc::POLLIN));
            fds.push(obc_audio::sys::pollfd(ep.input_fd(), libc::POLLIN));
        }
        if let Err(e) = obc_audio::sys::poll(&mut fds, Some(TICK)) {
            if e.kind() != std::io::ErrorKind::Interrupted {
                warning!("poll: {e}");
                std::thread::sleep(TICK);
            }
            return;
        }
        let listener_ready = fds[0].revents != 0;
        let hung_up = fds.get(1).is_some_and(|f| f.revents != 0)
            || fds
                .get(2)
                .is_some_and(|f| f.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0);
        if hung_up {
            info!("the mic plugin disconnected");
            self.disconnect();
        }
        if let Some(ep) = &self.endpoint {
            ep.drain_input_signal();
            ep.beat();
        }
        if listener_ready {
            self.accept();
        }
        self.serve_blocks();
    }

    /// Accepts a connection: the plugin when none is connected, else a
    /// refusal ([`RefuseReason::Busy`]) in a thread of its own.
    fn accept(&mut self) {
        let conn = match self.socket.listener.accept_timeout(Duration::ZERO) {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => {
                warning!("accept: {e}");
                return;
            }
        };
        if self.endpoint.is_some() {
            let _ = std::thread::Builder::new()
                .name("obc-audio-refuse".into())
                .spawn(move || {
                    if let Ok(h) = Handshake::read(conn, HELLO_TIMEOUT) {
                        let _ = h.refuse(RefuseReason::Busy, REASON_BUSY);
                    }
                });
            info!("refused a second mic plugin: {REASON_BUSY}");
            return;
        }
        let handshake = match Handshake::read(conn, HELLO_TIMEOUT) {
            Ok(h) => h,
            Err(e) => {
                warning!("a mic plugin could not connect: {e}");
                return;
            }
        };
        // The session starts: the GPU policy decides with the camera's
        // state now, for the whole session (a reconnection for a deeper
        // pipeline goes on with it).
        let snapshot = self.snapshot();
        if !(self.renegotiating.is_some() && self.session.is_some()) {
            self.session = Some((snapshot.gpu_policy(), snapshot.camera_active));
        }
        self.renegotiating = None;
        self.follow_gate();
        let depth = self.session_depth(&snapshot);
        let latency = self.latency_now();
        match handshake.accept(depth, latency) {
            Ok(ep) => {
                info!("a mic plugin connected (pipeline depth {depth}, latency {latency} samples)");
                ep.beat();
                self.endpoint = Some(ep);
                self.depth = depth;
                self.armed = true;
                self.last_input = Instant::now();
                self.published_latency = latency;
            }
            Err(e) => {
                warning!("a mic plugin could not connect: {e}");
                self.session = None;
            }
        }
    }

    /// The plugin is gone: the session ends (unless it was dropped for a
    /// deeper pipeline).
    fn disconnect(&mut self) {
        self.endpoint = None;
        if self.renegotiating.is_none() {
            self.session = None;
        }
    }

    /// The algorithmic delay of the blocks pushed now.
    fn latency_now(&self) -> u32 {
        self.active
            .as_ref()
            .filter(|c| c.is_live())
            .map_or(0, Chain::latency)
            .min(obc_audio::MAX_LATENCY_SAMPLES)
    }

    /// Answers every input block waiting.
    fn serve_blocks(&mut self) {
        let mut block = [0.0f32; BLOCK_SAMPLES];
        loop {
            let Some(ep) = self.endpoint.as_mut() else {
                return;
            };
            let meta = match ep.pop_input(&mut block) {
                Ok(Some(m)) => m,
                Ok(None) => return,
                Err(_) => {
                    warning!("the mic plugin's ring is corrupt: closing the connection");
                    self.disconnect();
                    return;
                }
            };
            self.last_input = Instant::now();
            self.watchdog.enter();
            let flags = self.process(&mut block, meta.flags, meta.n_samples);
            self.watchdog.leave();
            let latency = self.latency_now();
            let processing = self.active.as_ref().is_some_and(Chain::is_live);
            let Some(ep) = self.endpoint.as_mut() else {
                return;
            };
            if latency != self.published_latency {
                ep.set_latency_samples(latency);
                self.published_latency = latency;
            }
            ep.set_worker_flags(if processing {
                WORKER_FLAG_PROCESSING
            } else {
                0
            });
            ep.push_output(meta.seq, flags, &block);
            ep.beat();
        }
    }

    /// Runs one block in place; returns the output slot's flags.
    fn process(&mut self, block: &mut [f32; BLOCK_SAMPLES], flags: u32, n: u32) -> u32 {
        if n as usize != BLOCK_SAMPLES {
            return SLOT_FLAG_RAW;
        }
        let mut events = Vec::new();
        if flags & SLOT_FLAG_NEW_STREAM != 0 {
            for c in [self.active.as_mut(), self.pending.as_mut()]
                .into_iter()
                .flatten()
            {
                events.extend(c.reset());
            }
        }
        let out_flags = match (self.active.as_mut(), self.pending.as_mut()) {
            (None, _) => SLOT_FLAG_RAW,
            (Some(a), None) => {
                let (processed, ev) = a.process(block, flags);
                events.extend(ev);
                if processed { 0 } else { SLOT_FLAG_RAW }
            }
            (Some(a), Some(p)) => {
                let mut next = *block;
                let (old_ok, ev) = a.process(block, flags);
                events.extend(ev);
                let (new_ok, ev) = p.process(&mut next, flags);
                events.extend(ev);
                if !p.is_live() {
                    // The new chain failed while warming: keep the old.
                    let p = self.pending.take().expect("pending");
                    self.retire(p);
                    if old_ok { 0 } else { SLOT_FLAG_RAW }
                } else if new_ok {
                    if old_ok {
                        crossfade(block, &mut next);
                    }
                    *block = next;
                    let old = self.active.take().expect("active");
                    self.active = self.pending.take();
                    self.retire(old);
                    info!("switched to the new chain");
                    0
                } else if old_ok {
                    0
                } else {
                    SLOT_FLAG_RAW
                }
            }
        };
        for e in events {
            self.note(e);
        }
        if self.active.as_ref().is_some_and(|c| !c.is_live()) {
            let c = self.active.take().expect("active");
            self.retire(c);
            // A new chain warming up takes over at once.
            if let Some(p) = self.pending.take() {
                info!("the chain in use failed: switched to the new chain");
                self.active = Some(p);
            }
        }
        out_flags
    }

    /// Records a failure.
    fn note(&mut self, e: Event) {
        match e {
            Event::StageFailed {
                module,
                stage,
                reason,
            } => {
                warning!("stage {stage} of {module} failed open: {reason}");
                self.failures.insert(stage, reason);
            }
            Event::GroupFailed {
                module,
                stages,
                reason,
            } => {
                warning!(
                    "the stages {} of {module} failed open: {reason}",
                    stages.join(", ")
                );
                for s in stages {
                    self.failures.insert(s, reason.clone());
                }
            }
        }
    }

    /// Whether the worker has been idle long enough to exit.
    fn idle(&self) -> bool {
        let secs = lock(&self.shared.snapshot).idle_exit_s;
        if secs == 0 || self.shared.in_flight.load(Ordering::SeqCst) > 0 {
            return false;
        }
        let last = (*lock(&self.shared.last_call)).max(self.last_input);
        last.elapsed() >= Duration::from_secs(secs)
    }

    /// The status now; `StatusChanged` goes out when it changed other than
    /// in its counters.
    fn publish_status(&mut self) {
        let status = self.status();
        self.shared.status.send_if_modified(|s| {
            let changed = s.without_counters() != status.without_counters();
            *s = status;
            changed
        });
    }

    /// The status. With no plugin connected nothing records, so the state is
    /// `passthrough` (`off` when processing is off), with no provider and no
    /// latency, whatever chain is still loaded or ran last; the stages keep
    /// their own states and reasons. A connection dropped for a deeper
    /// pipeline counts as connected while the plugin connects again (the
    /// app still records): the status does not flip for it.
    fn status(&self) -> Status {
        let connected = self.endpoint.is_some() || self.renegotiating.is_some();
        let running: BTreeMap<String, String> = self
            .active
            .as_ref()
            .map(|c| c.running().into_iter().collect())
            .unwrap_or_default();
        let loading = self.loading || self.pending.is_some();
        let stages: Vec<StageStatus> = self
            .plan
            .stages
            .iter()
            .map(|p| {
                let (state, reason) = if !p.on {
                    ("off", p.reason.clone())
                } else if let Some(why) = self.failures.get(&p.id) {
                    ("failed", why.clone())
                } else if running.contains_key(&p.id) {
                    ("running", String::new())
                } else if self.armed {
                    ("loading", String::new())
                } else {
                    ("loading", "Loads when the microphone is in use".to_string())
                };
                StageStatus {
                    id: p.id.clone(),
                    provider: p.provider.clone(),
                    state: state.into(),
                    reason,
                }
            })
            .collect();
        let processing = self
            .active
            .as_ref()
            .is_some_and(|c| c.is_live() && !c.warming());
        let wanted = !self.plan.groups.is_empty();
        let state = if !self.plan.enabled {
            State::Off
        } else if !connected {
            State::Passthrough
        } else if processing {
            State::Active
        } else if wanted && (loading || self.active.is_some() || !self.armed) {
            State::Starting
        } else if wanted {
            State::Error
        } else {
            State::Passthrough
        };
        let error = if state == State::Error || state == State::Active {
            self.failures
                .iter()
                .next()
                .map(|(s, why)| format!("{s}: {why}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let consumers = self.shared.pipewire.consumers();
        let depth = if self.endpoint.is_some() {
            self.depth
        } else {
            self.open_depth
        };
        let (transport, latency) = if self.endpoint.is_some() {
            (self.transport_delay(), self.latency_now())
        } else if connected {
            (
                obc_audio::transport_delay(BLOCK_SAMPLES as u32, depth),
                self.latency_now(),
            )
        } else {
            (0, 0)
        };
        let samples_per_ms = f64::from(obc_audio::SAMPLE_RATE) / 1000.0;
        Status {
            state,
            consumers: consumers.map_or(0, |c| c.real),
            consumers_known: consumers.is_some(),
            connected,
            provider: if connected { running } else { BTreeMap::new() },
            latency_ms: f64::from(transport + latency) / samples_per_ms,
            latency_samples: latency,
            pipeline_depth: depth,
            error,
            overruns: self.active.as_ref().map_or(0, Chain::overruns),
            misses: self
                .endpoint
                .as_ref()
                .map_or(0, |e| e.client_counters().blocks_missed),
            voice: self
                .active
                .as_ref()
                .and_then(Chain::voice)
                .map_or(-1.0, f64::from),
            stages,
        }
    }
}

/// `stage (module), ...`.
fn describe(running: &[(String, String)]) -> String {
    running
        .iter()
        .map(|(s, m)| format!("{s} ({m})"))
        .collect::<Vec<_>>()
        .join(", ")
}
