//! [`Live`]: the [`PipeWireControl`] of the worker, over pipewire-rs.
//!
//! One thread, `obc-audio-pw`, owns the PipeWire main loop, context, core,
//! registry, the `default` metadata and the node proxies it follows. It is
//! started by the first request (lazily: the worker connects only when it
//! runs, and it exits on idle) and stopped by [`PipeWireControl::shutdown`]
//! or the drop: the worker holds no PipeWire connection after exit.
//!
//! * The registry feeds the [`Graph`]: every node, every link, the
//!   `default` metadata's properties. Stream input nodes (the consumers)
//!   are bound to learn whether they are monitors (`stream.monitor`,
//!   `resample.peaks`: properties the registry does not carry), and the
//!   microphone node to set its `ProcessLatency`.
//! * Requests travel over a channel and are answered on the thread, in
//!   order, against the graph as it is (our own metadata writes applied at
//!   once). The consumer count is published after every change and read
//!   without a round trip, debounced ([`CONSUMERS_DEBOUNCE`]).
//! * `ProcessLatency`: the plugin's `latency` port reaches the node only
//!   when its graph starts (spike Q5), so the worker sets the param itself
//!   whenever the delay changes, and again when the node reports another
//!   value while a delay is wanted (a new activation of the filter graph
//!   resets it), at most once per [`LATENCY_RESET_EVERY`].
//! * Until the first round trip of a connection is done (the registry's
//!   first burst of globals is in), the graph is not known: the consumers
//!   are unknown *and pending* ([`PipeWireControl::consumers_pending`]),
//!   and the first request waits for the sync (at most a second).
//! * No PipeWire, or a broken connection: requests fail with
//!   [`PipeWireError::Unavailable`], the consumers are unknown, and the
//!   connection is tried again every [`RETRY_AFTER`].

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ::pipewire as pw;
use pw::context::ContextRc;
use pw::core::{CoreRc, CoreWeak};
use pw::loop_::Timeout;
use pw::main_loop::MainLoopRc;
use pw::metadata::{Metadata, MetadataListener};
use pw::node::{Node, NodeChangeMask, NodeListener};
use pw::registry::RegistryRc;
use pw::spa;
use pw::types::ObjectType;
use spa::pod::serialize::PodSerializer;
use spa::pod::{Object, Pod, Property, Value};
use spa::utils::result::AsyncSeq;

use super::graph::{Graph, MIC_NODE, MetaWrite, NodeInfo, is_monitor};
use super::{
    CONSUMERS_DEBOUNCE, Consumers, Debounce, PipeWireControl, PipeWireError, SourceDevice,
};
use crate::engine::lock;
use crate::{debug, info, warning};

/// How long after a failure the connection is tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(2);

/// How long the first request waits for the first connection.
const READY_WAIT: Duration = Duration::from_secs(1);

/// How long a request waits for its answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// The longest wait of the thread's loop (requests are read after it).
const TICK: Duration = Duration::from_millis(20);

/// How often the `ProcessLatency` is set again at most, when the node
/// keeps reporting another value.
pub const LATENCY_RESET_EVERY: Duration = Duration::from_secs(1);

/// The reason while PipeWire is not connected.
const REASON_NOT_CONNECTED: &str = "PipeWire is not connected";

type Reply<T> = Sender<Result<T, PipeWireError>>;

enum Cmd {
    Devices(Reply<Vec<SourceDevice>>),
    MakeDefault(String, Reply<String>),
    Restore(String, Reply<()>),
    Latency(Option<u32>),
    Quit,
}

/// What the thread publishes.
#[derive(Default)]
struct Published {
    /// The consumers, raw; `None` while not connected and synced.
    consumers: Option<Consumers>,
    /// Connected, not synced yet.
    pending: bool,
    /// The first connection failed or synced: requests may go.
    ready: bool,
}

#[derive(Default)]
struct Common {
    published: Mutex<Published>,
    ready: Condvar,
}

impl Common {
    /// Publishes the consumers; `pending`: connected, not synced yet.
    fn publish(&self, consumers: Option<Consumers>, pending: bool) {
        let mut p = lock(&self.published);
        p.consumers = consumers;
        p.pending = pending && consumers.is_none();
        if !p.ready && !p.pending {
            p.ready = true;
            self.ready.notify_all();
        }
    }
}

struct Thread {
    tx: Sender<Cmd>,
    handle: JoinHandle<()>,
}

/// The worker's PipeWire control. See the module documentation.
pub struct Live {
    /// The remote to connect to (`None`: libpipewire's default).
    remote: Option<String>,
    thread: Mutex<Option<Thread>>,
    /// Shut down: no thread again.
    closed: Mutex<bool>,
    common: Arc<Common>,
    debounce: Mutex<Debounce<Consumers>>,
}

impl Default for Live {
    fn default() -> Self {
        Live::new()
    }
}

impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live").finish_non_exhaustive()
    }
}

impl Live {
    /// A control that connects at its first request.
    pub fn new() -> Live {
        Live::with_remote(None)
    }

    /// A control of the PipeWire remote named `remote`.
    pub fn with_remote(remote: Option<String>) -> Live {
        Live {
            remote,
            thread: Mutex::new(None),
            closed: Mutex::new(false),
            common: Arc::new(Common::default()),
            debounce: Mutex::new(Debounce::new(CONSUMERS_DEBOUNCE)),
        }
    }

    /// The thread's channel, the thread started (and its first connection
    /// tried, within [`READY_WAIT`]) if it was not; `None` after shutdown or
    /// when the thread cannot start.
    fn sender(&self) -> Option<Sender<Cmd>> {
        if *lock(&self.closed) {
            return None;
        }
        let mut thread = lock(&self.thread);
        if let Some(t) = thread.as_ref() {
            return Some(t.tx.clone());
        }
        let (tx, rx) = channel();
        let common = self.common.clone();
        let remote = self.remote.clone();
        let handle = std::thread::Builder::new()
            .name("obc-audio-pw".into())
            .spawn(move || run(rx, common, remote));
        let handle = match handle {
            Ok(h) => h,
            Err(e) => {
                warning!("cannot start the PipeWire thread: {e}");
                *lock(&self.closed) = true;
                return None;
            }
        };
        *thread = Some(Thread {
            tx: tx.clone(),
            handle,
        });
        drop(thread);
        let p = lock(&self.common.published);
        let _ = self
            .common
            .ready
            .wait_timeout_while(p, READY_WAIT, |p| !p.ready);
        Some(tx)
    }

    fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd) -> Result<T, PipeWireError> {
        let tx = self
            .sender()
            .ok_or_else(|| PipeWireError::Unavailable(REASON_NOT_CONNECTED.into()))?;
        let (rtx, rrx) = channel();
        tx.send(make(rtx))
            .map_err(|_| PipeWireError::Unavailable(REASON_NOT_CONNECTED.into()))?;
        match rrx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => Err(PipeWireError::Failed(
                "PipeWire did not answer in time".into(),
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err(PipeWireError::Unavailable(REASON_NOT_CONNECTED.into()))
            }
        }
    }
}

impl PipeWireControl for Live {
    fn devices(&self) -> Result<Vec<SourceDevice>, PipeWireError> {
        self.request(Cmd::Devices)
    }

    fn make_default(&self, target: &str) -> Result<String, PipeWireError> {
        let target = target.to_string();
        self.request(|r| Cmd::MakeDefault(target, r))
    }

    fn restore_default(&self, previous: &str) -> Result<(), PipeWireError> {
        let previous = previous.to_string();
        self.request(|r| Cmd::Restore(previous, r))
    }

    fn consumers(&self) -> Option<Consumers> {
        self.sender()?;
        let raw = lock(&self.common.published).consumers;
        lock(&self.debounce).update(raw, Instant::now())
    }

    fn consumers_pending(&self) -> bool {
        self.sender().is_some() && lock(&self.common.published).pending
    }

    fn set_latency(&self, samples: Option<u32>) {
        if let Some(tx) = self.sender() {
            let _ = tx.send(Cmd::Latency(samples));
        }
    }

    fn shutdown(&self) {
        *lock(&self.closed) = true;
        if let Some(t) = lock(&self.thread).take() {
            let _ = t.tx.send(Cmd::Quit);
            let _ = t.handle.join();
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A bound node and its listener (dropped first).
struct Bound {
    _listener: NodeListener,
    node: Node,
}

/// The bound `default` metadata.
struct BoundMeta {
    id: u32,
    _listener: MetadataListener,
    metadata: Metadata,
}

/// The state of one connection, shared by its callbacks (weakly).
struct Ctx {
    graph: RefCell<Graph>,
    nodes: RefCell<BTreeMap<u32, Bound>>,
    metadata: RefCell<Option<BoundMeta>>,
    registry: RefCell<Option<pw::registry::RegistryWeak>>,
    core: CoreWeak,
    /// The connection broke (why).
    failed: RefCell<Option<String>>,
    /// The sync in flight, and whether another is needed after it (a
    /// bind since it was sent).
    sync: RefCell<(Option<AsyncSeq>, bool)>,
    synced: RefCell<bool>,
    /// The `ProcessLatency` wanted, in samples.
    latency: RefCell<Option<u32>>,
    /// The node reported another `ProcessLatency` than the one wanted.
    latency_stale: RefCell<bool>,
    /// When it was last set.
    latency_set_at: RefCell<Option<Instant>>,
    common: Arc<Common>,
}

impl Ctx {
    fn fail(&self, why: String) {
        self.failed.borrow_mut().get_or_insert(why);
    }

    fn publish(&self) {
        let synced = *self.synced.borrow();
        let consumers = synced.then(|| self.graph.borrow().consumers());
        self.common.publish(consumers, !synced);
    }

    /// Sets the wanted `ProcessLatency` again if the node reported another
    /// one, at most once per [`LATENCY_RESET_EVERY`].
    fn reapply_latency(&self) {
        if !*self.latency_stale.borrow() {
            return;
        }
        let due = self
            .latency_set_at
            .borrow()
            .is_none_or(|t| t.elapsed() >= LATENCY_RESET_EVERY);
        if due {
            *self.latency_stale.borrow_mut() = false;
            self.apply_latency();
        }
    }

    /// Asks the server for a round trip (the first, or after a bind).
    fn resync(&self) {
        let mut sync = self.sync.borrow_mut();
        if sync.0.is_some() {
            sync.1 = true;
            return;
        }
        if let Some(core) = self.core.upgrade() {
            match core.sync(0) {
                Ok(seq) => sync.0 = Some(seq),
                Err(e) => self.fail(format!("PipeWire sync: {e}")),
            }
        }
    }

    fn done(&self, seq: AsyncSeq) {
        let again = {
            let mut sync = self.sync.borrow_mut();
            if sync.0 != Some(seq) {
                return;
            }
            sync.0 = None;
            std::mem::take(&mut sync.1)
        };
        if again {
            self.resync();
        } else if !*self.synced.borrow() {
            *self.synced.borrow_mut() = true;
            self.publish();
        }
    }

    fn mic_node_id(&self) -> Option<u32> {
        self.graph.borrow().node_by_name(MIC_NODE).map(|(id, _)| id)
    }

    /// Sets the wanted `ProcessLatency` on the microphone node, if both
    /// are known.
    fn apply_latency(&self) {
        let Some(samples) = *self.latency.borrow() else {
            return;
        };
        let Some(id) = self.mic_node_id() else {
            return;
        };
        if let Some(b) = self.nodes.borrow().get(&id) {
            let bytes = process_latency_pod(samples);
            if let Some(pod) = Pod::from_bytes(&bytes) {
                b.node
                    .set_param(spa::param::ParamType::ProcessLatency, 0, pod);
                *self.latency_set_at.borrow_mut() = Some(Instant::now());
                debug!("ProcessLatency of {MIC_NODE} set to {samples} samples");
            }
        }
    }
}

/// The `ProcessLatency` param of `samples` at the graph rate.
pub fn process_latency_pod(samples: u32) -> Vec<u8> {
    let value = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamProcessLatency,
        id: spa::sys::SPA_PARAM_ProcessLatency,
        properties: vec![Property::new(
            spa::sys::SPA_PARAM_PROCESS_LATENCY_rate,
            Value::Int(i32::try_from(samples).unwrap_or(i32::MAX)),
        )],
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .map(|(c, _)| c.into_inner())
        .unwrap_or_default()
}

/// The `rate` of a `ProcessLatency` param.
pub fn process_latency_rate(pod: &Pod) -> Option<u32> {
    let (_, value) =
        spa::pod::deserialize::PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };
    obj.properties
        .iter()
        .find(|p| p.key == spa::sys::SPA_PARAM_PROCESS_LATENCY_rate)
        .and_then(|p| match p.value {
            Value::Int(r) => u32::try_from(r).ok(),
            _ => None,
        })
}

/// Runs `body`, a callback's; a panic fails the connection instead of
/// unwinding into libpipewire.
fn guarded(ctx: &Ctx, what: &str, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        ctx.fail(format!("a panic in the PipeWire {what} callback"));
    }
}

/// One connection: its listeners and proxies, in drop order.
struct Conn {
    ctx: Rc<Ctx>,
    _registry_listener: pw::registry::Listener,
    _registry: RegistryRc,
    _core_listener: pw::core::Listener,
    _core: CoreRc,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // The bound proxies before the registry and the core.
        self.ctx.nodes.borrow_mut().clear();
        self.ctx.metadata.borrow_mut().take();
    }
}

fn props_of(
    g: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    key: &str,
) -> Option<String> {
    g.props.and_then(|p| p.get(key)).map(str::to_string)
}

/// A node global: into the graph; a stream input or our microphone bound.
fn on_node(ctx: &Rc<Ctx>, g: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) {
    let name = props_of(g, "node.name").unwrap_or_default();
    let class = props_of(g, "media.class").unwrap_or_default();
    let description = props_of(g, "node.description")
        .or_else(|| props_of(g, "node.nick"))
        .unwrap_or_else(|| name.clone());
    let follow = class.starts_with("Stream/Input") || name == MIC_NODE;
    ctx.graph.borrow_mut().add_node(
        g.id,
        NodeInfo {
            name: name.clone(),
            description,
            media_class: class,
            serial: props_of(g, "object.serial"),
            monitor: None,
        },
    );
    if !follow {
        return;
    }
    let registry = ctx.registry.borrow().as_ref().and_then(|r| r.upgrade());
    let Some(registry) = registry else {
        return;
    };
    let Ok(node) = registry.bind::<Node, _>(g) else {
        return;
    };
    let id = g.id;
    let is_mic = name == MIC_NODE;
    let (w_info, w_param) = (Rc::downgrade(ctx), Rc::downgrade(ctx));
    let listener = node
        .add_listener_local()
        .info(move |info| {
            let Some(ctx) = w_info.upgrade() else {
                return;
            };
            guarded(&ctx, "node info", || {
                // An info event carries the properties only when they
                // changed (the first one has them all).
                if !info.change_mask().contains(NodeChangeMask::PROPS) {
                    return;
                }
                let props = info.props();
                let get = |k: &str| props.and_then(|p| p.get(k));
                let monitor = is_monitor(get("stream.monitor"), get("resample.peaks"));
                ctx.graph.borrow_mut().set_monitor(id, monitor);
                ctx.publish();
            });
        })
        .param(move |_seq, kind, _index, _next, pod| {
            let Some(ctx) = w_param.upgrade() else {
                return;
            };
            guarded(&ctx, "node param", || {
                if kind != spa::param::ParamType::ProcessLatency {
                    return;
                }
                let rate = pod.and_then(process_latency_rate);
                let want = *ctx.latency.borrow();
                if want.is_some() && rate != want {
                    *ctx.latency_stale.borrow_mut() = true;
                    ctx.reapply_latency();
                }
            });
        })
        .register();
    if is_mic {
        node.subscribe_params(&[spa::param::ParamType::ProcessLatency]);
    }
    ctx.nodes.borrow_mut().insert(
        id,
        Bound {
            _listener: listener,
            node,
        },
    );
    ctx.resync();
    if is_mic {
        ctx.apply_latency();
    }
}

/// The `default` metadata: bound, its properties into the graph.
fn on_metadata(ctx: &Rc<Ctx>, g: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) {
    if props_of(g, "metadata.name").as_deref() != Some("default") {
        return;
    }
    let registry = ctx.registry.borrow().as_ref().and_then(|r| r.upgrade());
    let Some(registry) = registry else {
        return;
    };
    let Ok(metadata) = registry.bind::<Metadata, _>(g) else {
        return;
    };
    let w = Rc::downgrade(ctx);
    let listener = metadata
        .add_listener_local()
        .property(move |subject, key, type_, value| {
            if let Some(ctx) = w.upgrade() {
                guarded(&ctx, "metadata", || {
                    ctx.graph
                        .borrow_mut()
                        .meta_event(subject, key, type_, value);
                });
            }
            0
        })
        .register();
    ctx.graph.borrow_mut().clear_meta();
    *ctx.metadata.borrow_mut() = Some(BoundMeta {
        id: g.id,
        _listener: listener,
        metadata,
    });
    ctx.resync();
}

fn connect(
    context: &ContextRc,
    common: &Arc<Common>,
    remote: Option<&str>,
) -> Result<Conn, String> {
    let props = remote.map(|r| pw::properties::properties! { *pw::keys::REMOTE_NAME => r });
    let core = context
        .connect_rc(props)
        .map_err(|e| format!("cannot connect to PipeWire: {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("PipeWire registry: {e}"))?;
    let ctx = Rc::new(Ctx {
        graph: RefCell::default(),
        nodes: RefCell::default(),
        metadata: RefCell::default(),
        registry: RefCell::new(Some(registry.downgrade())),
        core: core.downgrade(),
        failed: RefCell::default(),
        sync: RefCell::default(),
        synced: RefCell::new(false),
        latency: RefCell::default(),
        latency_stale: RefCell::new(false),
        latency_set_at: RefCell::new(None),
        common: common.clone(),
    });
    // Connected: the consumers are pending until the first sync.
    common.publish(None, true);
    let core_listener = {
        let (w_done, w_err) = (Rc::downgrade(&ctx), Rc::downgrade(&ctx));
        core.add_listener_local()
            .done(move |id, seq| {
                if id != pw::core::PW_ID_CORE {
                    return;
                }
                if let Some(ctx) = w_done.upgrade() {
                    guarded(&ctx, "core done", || ctx.done(seq));
                }
            })
            .error(move |id, _seq, res, message| {
                // Only a broken connection ends it: an error about one of
                // our proxies (a node gone before its proxy) does not.
                if id == pw::core::PW_ID_CORE
                    && res == -libc::EPIPE
                    && let Some(ctx) = w_err.upgrade()
                {
                    ctx.fail(format!("the PipeWire connection broke: {message}"));
                } else {
                    debug!("PipeWire error on {id}: {res} {message}");
                }
            })
            .register()
    };
    let registry_listener = {
        let (w_add, w_rm) = (Rc::downgrade(&ctx), Rc::downgrade(&ctx));
        registry
            .add_listener_local()
            .global(move |g| {
                let Some(ctx) = w_add.upgrade() else {
                    return;
                };
                guarded(&ctx, "registry global", || {
                    match g.type_ {
                        ObjectType::Node => on_node(&ctx, g),
                        ObjectType::Link => {
                            let num =
                                |k: &str| props_of(g, k).and_then(|v| v.trim().parse::<u32>().ok());
                            if let (Some(out), Some(input)) =
                                (num("link.output.node"), num("link.input.node"))
                            {
                                ctx.graph.borrow_mut().add_link(g.id, out, input);
                            }
                        }
                        ObjectType::Metadata => on_metadata(&ctx, g),
                        _ => return,
                    }
                    ctx.publish();
                });
            })
            .global_remove(move |id| {
                let Some(ctx) = w_rm.upgrade() else {
                    return;
                };
                guarded(&ctx, "registry global_remove", || {
                    ctx.graph.borrow_mut().remove(id);
                    let gone = ctx.nodes.borrow_mut().remove(&id);
                    drop(gone);
                    let meta_gone = ctx.metadata.borrow().as_ref().is_some_and(|m| m.id == id);
                    if meta_gone {
                        ctx.metadata.borrow_mut().take();
                        ctx.graph.borrow_mut().clear_meta();
                    }
                    ctx.publish();
                });
            })
            .register()
    };
    ctx.resync();
    Ok(Conn {
        ctx,
        _registry_listener: registry_listener,
        _registry: registry,
        _core_listener: core_listener,
        _core: core,
    })
}

/// Executes `writes` on the `default` metadata, and applies them to the
/// graph at once.
fn write(ctx: &Ctx, writes: &[MetaWrite]) -> Result<(), PipeWireError> {
    if writes.is_empty() {
        return Ok(());
    }
    let meta = ctx.metadata.borrow();
    let Some(m) = meta.as_ref() else {
        return Err(PipeWireError::Failed(
            "WirePlumber's default metadata is not available (is WirePlumber running?)".into(),
        ));
    };
    for w in writes {
        match w {
            MetaWrite::Set {
                subject,
                key,
                type_,
                value,
            } => {
                debug!("metadata: {subject} {key} = {value} ({type_})");
                m.metadata
                    .set_property(*subject, key, Some(type_), Some(value));
            }
            MetaWrite::Delete { subject, key } => {
                debug!("metadata: {subject} {key} deleted");
                m.metadata.set_property(*subject, key, None, None);
            }
        }
    }
    drop(meta);
    ctx.graph.borrow_mut().apply(writes);
    Ok(())
}

/// Answers `cmd` on `conn` (`None`: not connected). Returns false on
/// [`Cmd::Quit`].
fn handle(cmd: Cmd, conn: Option<&Conn>, latency: &mut Option<u32>) -> bool {
    let ctx = conn.map(|c| c.ctx.as_ref()).filter(|c| *c.synced.borrow());
    let unavailable = || PipeWireError::Unavailable(REASON_NOT_CONNECTED.into());
    match cmd {
        Cmd::Quit => return false,
        Cmd::Devices(r) => {
            let _ = r.send(
                ctx.map(|c| c.graph.borrow().devices())
                    .ok_or_else(unavailable),
            );
        }
        Cmd::MakeDefault(target, r) => {
            let res = ctx.ok_or_else(unavailable).and_then(|c| {
                let (previous, writes) = c.graph.borrow().make_default(&target)?;
                write(c, &writes)?;
                info!(
                    "{MIC_NODE} is the default source (previous {previous:?}, capture pinned to {:?})",
                    c.graph.borrow().pin().unwrap_or_default()
                );
                Ok(previous)
            });
            let _ = r.send(res);
        }
        Cmd::Restore(previous, r) => {
            let res = ctx.ok_or_else(unavailable).and_then(|c| {
                let writes = c.graph.borrow().restore_default(&previous);
                write(c, &writes)?;
                info!("default source restored ({previous:?}), capture unpinned");
                Ok(())
            });
            let _ = r.send(res);
        }
        Cmd::Latency(samples) => {
            *latency = samples;
            if let Some(c) = conn {
                *c.ctx.latency.borrow_mut() = samples;
                c.ctx.apply_latency();
            }
        }
    }
    true
}

/// The thread body.
fn run(rx: Receiver<Cmd>, common: Arc<Common>, remote: Option<String>) {
    pw::init();
    let built = MainLoopRc::new(None)
        .map_err(|e| e.to_string())
        .and_then(|l| {
            ContextRc::new(&l, None)
                .map(|c| (l, c))
                .map_err(|e| e.to_string())
        });
    let (mainloop, context) = match built {
        Ok(x) => x,
        Err(e) => {
            warning!("PipeWire cannot be used: {e}");
            common.publish(None, false);
            // Answer every request until told to quit.
            while let Ok(cmd) = rx.recv() {
                if !handle(cmd, None, &mut None) {
                    return;
                }
            }
            return;
        }
    };
    let mut conn: Option<Conn> = None;
    let mut retry_at = Instant::now();
    let mut last_error: Option<String> = None;
    // The latency wanted, kept across reconnections.
    let mut latency: Option<u32> = None;
    // Requests that came before the first sync of a connection.
    let mut waiting: VecDeque<Cmd> = VecDeque::new();
    let mut connected_at = Instant::now();
    loop {
        if conn.is_none() && Instant::now() >= retry_at {
            match connect(&context, &common, remote.as_deref()) {
                Ok(c) => {
                    *c.ctx.latency.borrow_mut() = latency;
                    conn = Some(c);
                    connected_at = Instant::now();
                    if last_error.take().is_some() {
                        info!("connected to PipeWire again");
                    }
                }
                Err(e) => {
                    if last_error.as_deref() != Some(e.as_str()) {
                        warning!("{e}: the microphone's PipeWire control is unavailable for now");
                    }
                    last_error = Some(e);
                    common.publish(None, false);
                    retry_at = Instant::now() + RETRY_AFTER;
                }
            }
        }
        mainloop.loop_().iterate(Timeout::Finite(TICK));
        if let Some(c) = &conn {
            c.ctx.reapply_latency();
        }
        let broke = conn.as_ref().and_then(|c| c.ctx.failed.borrow_mut().take());
        if let Some(why) = broke {
            warning!("{why}: connecting again in {}s", RETRY_AFTER.as_secs());
            last_error = Some(why);
            conn = None;
            common.publish(None, false);
            retry_at = Instant::now() + RETRY_AFTER;
        }
        let synced = conn.as_ref().is_some_and(|c| *c.ctx.synced.borrow());
        // A request waits for the first sync, at most READY_WAIT.
        let hold = conn.is_some() && !synced && connected_at.elapsed() < READY_WAIT;
        while let Ok(cmd) = rx.try_recv() {
            waiting.push_back(cmd);
        }
        if hold {
            continue;
        }
        while let Some(cmd) = waiting.pop_front() {
            if !handle(cmd, conn.as_ref(), &mut latency) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connected but not synced: the consumers are pending and requests
    /// not ready yet; a sync or a failure makes them ready.
    #[test]
    fn the_consumers_are_pending_until_the_first_sync() {
        let c = Common::default();
        c.publish(None, true);
        {
            let p = lock(&c.published);
            assert!(p.pending && !p.ready);
        }
        c.publish(Some(Consumers::default()), false);
        {
            let p = lock(&c.published);
            assert!(!p.pending && p.ready);
            assert_eq!(p.consumers, Some(Consumers::default()));
        }
        let c = Common::default();
        c.publish(None, true);
        c.publish(None, false);
        let p = lock(&c.published);
        assert!(!p.pending && p.ready, "a failure ends the wait");
    }

    #[test]
    fn process_latency_pod_round_trips() {
        let bytes = process_latency_pod(1920);
        let pod = Pod::from_bytes(&bytes).unwrap();
        assert_eq!(process_latency_rate(pod), Some(1920));
    }

    /// Without a PipeWire to reach (a remote that does not exist),
    /// requests fail as unavailable, the consumers are unknown, and
    /// shutdown returns at once; after it nothing starts again.
    #[test]
    fn without_pipewire_everything_is_unavailable() {
        let live = Live::with_remote(Some(format!(
            "obc-audiod-test-no-such-remote-{}",
            std::process::id()
        )));
        let started = Instant::now();
        assert!(matches!(live.devices(), Err(PipeWireError::Unavailable(_))));
        assert!(matches!(
            live.make_default(""),
            Err(PipeWireError::Unavailable(_))
        ));
        assert_eq!(live.consumers(), None);
        live.set_latency(Some(960));
        live.shutdown();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(
            live.restore_default(""),
            Err(PipeWireError::Unavailable(_))
        ));
        assert_eq!(live.consumers(), None);
    }
}
