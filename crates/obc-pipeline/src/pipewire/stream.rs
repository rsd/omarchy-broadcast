//! The `obc-pipewire` thread: the PipeWire main loop, the connection
//! (context, core, registry, stream) and its reconnection.
//!
//! The thread iterates the main loop itself. PipeWire callbacks only
//! record what happened in [`Ctl`] (a frame waiting, a failure, the retry
//! timer, a quit); the control loop in [`run_inner`] acts on it between
//! iterations, outside every callback: a failed connection is torn down
//! there (no object is destroyed from inside its own callback), and the
//! stream is triggered there, so the channel's lock (held while its
//! messages are dispatched) is never held across a frame's conversion and
//! the tap never waits for one.
//!
//! **Panics.** pipewire-rs calls our callbacks from `extern "C"`
//! trampolines that do not catch an unwind: a panic there aborts the
//! process. Every callback body runs in [`guarded`], which catches it and
//! records it as the connection's failure (torn down and retried like any
//! other). A panic elsewhere in the thread is caught in [`run`]: the
//! connection is torn down while unwinding, `State(Failed)` and
//! `Consumers(0)` are reported, and the thread ends.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use gst_video::prelude::*;
use pipewire as pw;
use pw::context::ContextRc;
use pw::core::CoreRc;
use pw::link::{Link, LinkListener};
use pw::loop_::Timeout;
use pw::main_loop::MainLoopRc;
use pw::permissions::PermissionFlags;
use pw::proxy::{ProxyListener, ProxyT};
use pw::registry::{GlobalObject, RegistryRc, RegistryWeak};
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use pw::types::ObjectType;

use super::convert::{Source, convert};
use super::format::{self, Layout, as_pod};
use super::links::{LinkTable, link_nodes};
use super::{Msg, NO_NODE, PwEvent, PwOutputConfig, PwState, RETRY_AFTER, Shared};

/// Reports state and count changes (and mirrors them into [`Shared`]).
struct Reporter {
    report: Box<dyn Fn(PwEvent) + Send>,
    shared: Arc<Shared>,
    state: RefCell<PwState>,
    consumers: Cell<u32>,
}

impl Reporter {
    /// The first reports: `Connecting` and a count of 0, unconditionally.
    fn initial(&self) {
        self.shared.set_state(PwState::Connecting);
        self.shared.consumers.store(0, Ordering::Release);
        (self.report)(PwEvent::State(PwState::Connecting));
        (self.report)(PwEvent::Consumers(0));
    }

    fn state(&self, state: PwState) {
        if *self.state.borrow() == state {
            return;
        }
        *self.state.borrow_mut() = state.clone();
        self.shared.set_state(state.clone());
        (self.report)(PwEvent::State(state));
    }

    fn consumers(&self, n: u32) {
        if self.consumers.get() == n {
            return;
        }
        self.consumers.set(n);
        self.shared.consumers.store(n, Ordering::Release);
        (self.report)(PwEvent::Consumers(n));
    }

    /// The thread's body panicked (caught in [`run`]); the thread ends.
    /// Reported unconditionally, whatever was reported before (the panic
    /// may have come mid-report): `Failed` with the panic message, then a
    /// count of 0, so the owner releases what the consumers held. A panic
    /// of `report` itself is caught: the thread must not unwind again.
    fn panicked(&self, msg: &str) {
        let state = PwState::Failed(format!("the PipeWire thread panicked: {msg}"));
        self.shared.set_streaming(false);
        self.shared.node_id.store(NO_NODE, Ordering::Release);
        self.shared.set_state(state.clone());
        self.shared.consumers.store(0, Ordering::Release);
        self.state.replace(state.clone());
        self.consumers.set(0);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            (self.report)(PwEvent::State(state));
            (self.report)(PwEvent::Consumers(0));
        }));
    }
}

/// A panic's message.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Runs the body of the PipeWire callback `what`. A panic must not unwind
/// into libpipewire (pipewire-rs's trampolines do not catch it: the
/// process would abort): it is caught and recorded as the connection's
/// failure, which the control loop tears down and retries.
fn guarded(ctl: &Ctl, what: &str, body: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(body)) {
        ctl.fail(format!(
            "a panic in the PipeWire {what} callback: {}",
            panic_message(payload.as_ref())
        ));
    }
}

/// A bound Link from our node and the listeners following its state and
/// its removal. Field order is drop order: the listeners before their
/// proxy.
///
/// Destroyed only once the server has removed the proxy (`removed`, its
/// `remove_id`), not on the registry's `global_remove`, which comes first:
/// destroying it then sends a `destroy` for a resource the server has just
/// dropped, which it answers with a core error ("unknown resource"), on
/// the core's id. And never from inside its own callback: a removed watch
/// is dropped by the control loop ([`Env::drop_removed_links`]).
struct LinkWatch {
    _listener: LinkListener,
    _proxy_listener: ProxyListener,
    _link: Link,
    /// The server removed the proxy.
    removed: Rc<Cell<bool>>,
}

/// One connection. Field order is drop order: listeners before the
/// objects they listen to, the stream before the core. The link proxies
/// ([`Env::watches`]) are dropped before it ([`teardown`]).
struct Session {
    _stream_listener: StreamListener<StreamData>,
    _registry_listener: pw::registry::Listener,
    _core_listener: pw::core::Listener,
    stream: StreamRc,
    _registry: RegistryRc,
    _core: CoreRc,
}

/// What the thread's control loop acts on.
#[derive(Default)]
struct Ctl {
    quit: Cell<bool>,
    /// A frame is waiting in the slot.
    frame: Cell<bool>,
    /// The first failure since the last connect.
    failure: RefCell<Option<String>>,
    reconnect: Cell<bool>,
    session: RefCell<Option<Session>>,
}

impl Ctl {
    fn fail(&self, reason: String) {
        self.failure.borrow_mut().get_or_insert(reason);
    }
}

/// The stream listener's data: the layout chosen by the consumer.
struct StreamData {
    layout: Option<Layout>,
}

/// Everything the stream callbacks need.
struct Env {
    /// Itself, for the link listeners (which [`Env::watches`] holds).
    me: Weak<Env>,
    cfg: PwOutputConfig,
    writer_info: gst_video::VideoInfo,
    shared: Arc<Shared>,
    reporter: Rc<Reporter>,
    links: RefCell<LinkTable>,
    /// The connection's registry, once connected.
    registry: RefCell<Option<RegistryWeak>>,
    /// The links from our node being followed, by link id.
    watches: RefCell<HashMap<u32, LinkWatch>>,
    /// Watches of links gone from the registry, until the server has
    /// removed their proxy.
    retired: RefCell<Vec<LinkWatch>>,
    ctl: Rc<Ctl>,
}

impl Env {
    /// The table changed: follow the links from our node and report the
    /// count.
    fn links_changed(&self) {
        self.sync_watches();
        self.reporter.consumers(self.links.borrow().consumers());
    }

    fn set_node(&self, id: Option<u32>) {
        self.shared
            .node_id
            .store(id.unwrap_or(NO_NODE), Ordering::Release);
        self.links.borrow_mut().set_node(id);
        self.links_changed();
    }

    /// Binds every link from our node not followed yet and retires the
    /// watches of links no longer ours (gone from the registry, or the
    /// node changed).
    fn sync_watches(&self) {
        let ours = self.links.borrow().ours();
        {
            let mut watches = self.watches.borrow_mut();
            let gone: Vec<u32> = watches
                .keys()
                .copied()
                .filter(|id| !ours.contains(id))
                .collect();
            let mut retired = self.retired.borrow_mut();
            retired.extend(gone.iter().filter_map(|id| watches.remove(id)));
        }
        let registry = self
            .registry
            .borrow()
            .as_ref()
            .and_then(RegistryWeak::upgrade);
        let Some(registry) = registry else {
            return;
        };
        for id in ours {
            if self.watches.borrow().contains_key(&id) {
                continue;
            }
            // A link that cannot be bound (out of memory) stays counted
            // while its state is unknown; binding is tried again on the
            // next change.
            if let Some(watch) = self.watch_link(&registry, id) {
                self.watches.borrow_mut().insert(id, watch);
            }
        }
    }

    /// Binds the Link global `id` and follows its state: in `Error`, the
    /// link is not counted.
    fn watch_link(&self, registry: &RegistryRc, id: u32) -> Option<LinkWatch> {
        // `bind` reads only the id and the type.
        let global = GlobalObject {
            id,
            permissions: PermissionFlags::empty(),
            type_: ObjectType::Link,
            version: 0,
            props: None::<&spa::utils::dict::DictRef>,
        };
        let link: Link = registry.bind(&global).ok()?;
        let removed = Rc::new(Cell::new(false));
        let proxy_listener = {
            let removed = Rc::clone(&removed);
            link.upcast_ref()
                .add_listener_local()
                // A flag set: nothing that can panic.
                .removed(move || removed.set(true))
                .register()
        };
        let env = self.me.clone();
        let listener = link
            .add_listener_local()
            .info(move |info| {
                let Some(env) = env.upgrade() else {
                    return;
                };
                guarded(&env.ctl, "link info", || {
                    // Read raw: `LinkInfoRef::state` panics on an error
                    // message that is not UTF-8.
                    let failed = info.as_raw().state == pw::sys::pw_link_state_PW_LINK_STATE_ERROR;
                    env.links.borrow_mut().set_failed(id, failed);
                    env.links_changed();
                });
            })
            .register();
        Some(LinkWatch {
            _listener: listener,
            _proxy_listener: proxy_listener,
            _link: link,
            removed,
        })
    }

    /// Destroys the retired watches whose proxy the server has removed.
    /// Called by the control loop, outside every callback.
    fn drop_removed_links(&self) {
        let done: Vec<LinkWatch> = {
            let mut retired = self.retired.borrow_mut();
            let (done, keep) = std::mem::take(&mut *retired)
                .into_iter()
                .partition(|w| w.removed.get());
            *retired = keep;
            done
        };
        drop(done);
    }
}

/// Tears the connection down when the thread's body unwinds, so that the
/// node does not outlive the loop (the callbacks hold the [`Env`] the
/// session belongs to: it would leak, registered).
struct TeardownOnUnwind<'a>(&'a Env);

impl Drop for TeardownOnUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            teardown(self.0);
        }
    }
}

/// The thread body. `ready` gets the outcome of creating the main loop and
/// context (the only failures [`super::PipeWireOutput::start`] returns).
pub(super) fn run(
    cfg: PwOutputConfig,
    writer_info: gst_video::VideoInfo,
    shared: Arc<Shared>,
    receiver: pw::channel::Receiver<Msg>,
    report: Box<dyn Fn(PwEvent) + Send>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    // Outside the body: still there to report a panic.
    let reporter = Rc::new(Reporter {
        report,
        shared: Arc::clone(&shared),
        state: RefCell::new(PwState::Connecting),
        consumers: Cell::new(0),
    });
    let body_reporter = Rc::clone(&reporter);
    let res = catch_unwind(AssertUnwindSafe(move || {
        run_inner(cfg, writer_info, shared, receiver, body_reporter, ready)
    }));
    if let Err(payload) = res {
        reporter.panicked(&panic_message(payload.as_ref()));
    }
}

/// The node name that makes the thread panic once connecting (unit tests).
#[cfg(test)]
pub(super) const TEST_PANIC_NODE: &str = "obc-test-panic";

fn run_inner(
    cfg: PwOutputConfig,
    writer_info: gst_video::VideoInfo,
    shared: Arc<Shared>,
    receiver: pw::channel::Receiver<Msg>,
    reporter: Rc<Reporter>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    pw::init();
    let (mainloop, context) = match MainLoopRc::new(None)
        .map_err(|e| format!("main loop: {e}"))
        .and_then(|ml| {
            let ctx = ContextRc::new(&ml, None).map_err(|e| format!("context: {e}"))?;
            Ok((ml, ctx))
        }) {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let ctl = Rc::new(Ctl::default());
    let env = Rc::new_cyclic(|me| Env {
        me: me.clone(),
        cfg,
        writer_info,
        shared: Arc::clone(&shared),
        reporter: Rc::clone(&reporter),
        links: RefCell::default(),
        registry: RefCell::default(),
        watches: RefCell::default(),
        retired: RefCell::default(),
        ctl: Rc::clone(&ctl),
    });
    let _unwind = TeardownOnUnwind(&env);

    let lp = mainloop.loop_();
    let timer = {
        let ctl = Rc::clone(&ctl);
        lp.add_timer(move |_| guarded(&ctl, "timer", || ctl.reconnect.set(true)))
    };
    let _receiver = {
        let (ctl, shared) = (Rc::clone(&ctl), Arc::clone(&shared));
        receiver.attach(lp, move |msg| {
            guarded(&ctl, "channel", || match msg {
                Msg::Frame => {
                    shared.wake_pending.store(false, Ordering::Release);
                    ctl.frame.set(true);
                }
                Msg::Quit => ctl.quit.set(true),
            })
        })
    };
    let _ = ready.send(Ok(()));
    reporter.initial();
    ctl.reconnect.set(true);

    loop {
        if ctl.quit.get() {
            break;
        }
        if let Some(reason) = ctl.failure.borrow_mut().take() {
            // Torn down here, outside every callback.
            teardown(&env);
            reporter.state(PwState::Failed(reason));
            ctl.reconnect.set(false);
            let _ = timer.update_timer(Some(RETRY_AFTER), None);
        } else if ctl.reconnect.replace(false) {
            reporter.state(PwState::Connecting);
            #[cfg(test)]
            if env.cfg.node_name == TEST_PANIC_NODE {
                panic!("a test panic");
            }
            match connect(&context, &env) {
                Ok(session) => *ctl.session.borrow_mut() = Some(session),
                Err(e) => {
                    ctl.failure.borrow_mut().get_or_insert(e);
                }
            }
            continue;
        }
        if ctl.frame.replace(false) {
            trigger(&env);
        }
        env.drop_removed_links();
        lp.iterate(Timeout::Infinite);
    }
    teardown(&env);
    drop(timer);
}

/// Has the stream send the waiting frame (process() runs inside the
/// trigger), or drops it while not streaming.
fn trigger(env: &Env) {
    // Not borrowed across the trigger: the callbacks it runs must not find
    // the session borrowed.
    let stream = env.ctl.session.borrow().as_ref().map(|s| s.stream.clone());
    match stream {
        Some(stream) if env.shared.streaming.load(Ordering::Acquire) => {
            let _ = stream.trigger_process();
        }
        _ => {
            env.shared.take_latest();
        }
    }
}

/// Drops the connection (the node disappears) and resets what depended on
/// it: not streaming, no node, no links, a count of 0.
fn teardown(env: &Env) {
    // The link proxies first, while their core is there.
    let watches = std::mem::take(&mut *env.watches.borrow_mut());
    drop(watches);
    let retired = std::mem::take(&mut *env.retired.borrow_mut());
    drop(retired);
    env.registry.borrow_mut().take();
    // The stream's listener goes first (field order), so destroying the
    // stream reports nothing back.
    let session = env.ctl.session.borrow_mut().take();
    drop(session);
    env.shared.set_streaming(false);
    env.links.borrow_mut().clear();
    env.set_node(None);
}

/// The PipeWire remote the unit tests connect to instead of the user's
/// daemon (one that does not exist: no daemon is contacted).
#[cfg(test)]
pub(super) static TEST_REMOTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn core_properties() -> Option<pw::properties::PropertiesBox> {
    #[cfg(test)]
    if let Some(remote) = TEST_REMOTE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return Some(pw::properties::properties! { *pw::keys::REMOTE_NAME => remote });
    }
    None
}

fn connect(context: &ContextRc, env: &Rc<Env>) -> Result<Session, String> {
    let core = context
        .connect_rc(core_properties())
        .map_err(|e| format!("cannot connect to PipeWire: {e}"))?;
    let core_listener = {
        let env = Rc::clone(env);
        core.add_listener_local()
            .error(move |id, _seq, res, message| {
                guarded(&env.ctl, "core error", || {
                    // Errors on other objects (our stream's node among
                    // them) reach the stream through its own state; only
                    // the core's own error (id 0) means the connection is
                    // gone.
                    if id == pw::core::PW_ID_CORE {
                        env.ctl
                            .fail(format!("PipeWire core error {res}: {message}"));
                    }
                })
            })
            .register()
    };
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("PipeWire registry: {e}"))?;
    let registry_listener = {
        let (added, removed) = (Rc::clone(env), Rc::clone(env));
        registry
            .add_listener_local()
            .global(move |g| {
                guarded(&added.ctl, "registry global", || {
                    if g.type_ != ObjectType::Link {
                        return;
                    }
                    let props = g.props.as_ref();
                    let get = |k: &str| props.and_then(|p| p.get(k));
                    if let Some((out, input)) =
                        link_nodes(get("link.output.node"), get("link.input.node"))
                    {
                        added.links.borrow_mut().add(g.id, out, input);
                        added.links_changed();
                    }
                })
            })
            .global_remove(move |id| {
                guarded(&removed.ctl, "registry global_remove", || {
                    // Its watch, if any, is retired.
                    removed.links.borrow_mut().remove(id);
                    removed.links_changed();
                })
            })
            .register()
    };

    let cfg = &env.cfg;
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Camera",
        *pw::keys::MEDIA_CLASS => "Video/Source",
        *pw::keys::NODE_NAME => cfg.node_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => cfg.description.as_str(),
        *pw::keys::MEDIA_NAME => cfg.description.as_str(),
    };
    let stream = StreamRc::new(core.clone(), &cfg.description, props)
        .map_err(|e| format!("PipeWire stream: {e}"))?;
    let stream_listener = {
        let (on_state, on_param, on_process) = (Rc::clone(env), Rc::clone(env), Rc::clone(env));
        stream
            .add_local_listener_with_user_data(StreamData { layout: None })
            .state_changed(move |stream, _, _old, new| {
                guarded(&on_state.ctl, "stream state", || {
                    on_state_changed(&on_state, stream, new)
                })
            })
            .param_changed(move |stream, data, id, param| {
                guarded(&on_param.ctl, "stream param", || {
                    on_param_changed(&on_param, stream, data, id, param)
                })
            })
            .process(move |stream, data| {
                guarded(&on_process.ctl, "stream process", || {
                    on_process_frame(&on_process, stream, data)
                })
            })
            .register()
            .map_err(|e| format!("PipeWire stream listener: {e}"))?
    };
    let pods = format::enum_format_pods(&cfg.contract);
    let mut params: Vec<&spa::pod::Pod> = pods.iter().map(|b| as_pod(b)).collect();
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("PipeWire stream connect: {e}"))?;
    // Only now: a registry that outlived a failed connect would outlive its
    // core. No callback runs before the loop iterates again.
    *env.registry.borrow_mut() = Some(registry.downgrade());
    Ok(Session {
        _stream_listener: stream_listener,
        _registry_listener: registry_listener,
        _core_listener: core_listener,
        stream,
        _registry: registry,
        _core: core,
    })
}

fn on_state_changed(env: &Env, stream: &pw::stream::Stream, new: StreamState) {
    match new {
        StreamState::Paused => {
            env.shared.set_streaming(false);
            env.set_node(Some(stream.node_id()).filter(|&id| id != NO_NODE));
            env.reporter.state(PwState::Ready);
        }
        StreamState::Streaming => {
            env.set_node(Some(stream.node_id()).filter(|&id| id != NO_NODE));
            env.shared.set_streaming(true);
            env.reporter.state(PwState::Streaming);
        }
        StreamState::Error(e) => {
            // An error on our node's proxy, e.g. a consumer's failed
            // negotiation ("no more input formats"), is only announced:
            // libpipewire leaves the stream in its state and the stream
            // keeps serving. Only a stream really in error is fatal.
            if matches!(stream.state(), StreamState::Error(_)) {
                env.shared.set_streaming(false);
                env.ctl.fail(format!("PipeWire stream error: {e}"));
            } else {
                env.shared.link_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        StreamState::Unconnected => {
            env.shared.set_streaming(false);
            env.ctl.fail("disconnected from PipeWire".into());
        }
        StreamState::Connecting => {}
    }
}

fn on_param_changed(
    env: &Env,
    stream: &pw::stream::Stream,
    data: &mut StreamData,
    id: u32,
    param: Option<&spa::pod::Pod>,
) {
    if id != spa::param::ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else {
        // The format was cleared (the consumer went away).
        data.layout = None;
        return;
    };
    match format::negotiate(param, &env.cfg.contract) {
        Ok(layout) => {
            data.layout = Some(layout);
            let buffers = format::buffers_param(&layout);
            let meta = format::meta_header_param();
            let mut params = [as_pod(&buffers), as_pod(&meta)];
            let _ = stream.update_params(&mut params);
        }
        // Only what was offered can be chosen; anything else gets no
        // buffers and no frames, and fails only that consumer's link.
        Err(_) => data.layout = None,
    }
}

fn on_process_frame(env: &Env, stream: &pw::stream::Stream, data: &mut StreamData) {
    let Some(frame) = env.shared.take_latest() else {
        return;
    };
    let Some(layout) = data.layout else {
        env.shared.failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // SAFETY: a buffer dequeued from this stream, on the stream's loop
    // thread, returned or queued below before this function returns.
    let raw = unsafe { stream.dequeue_raw_buffer() };
    if raw.is_null() {
        env.shared.no_buffer.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: `raw` is a valid dequeued buffer (above).
    let filled = unsafe { fill(raw, &frame, &layout, &env.writer_info) };
    match filled {
        Ok(Filled::Frame) => {
            // SAFETY: as above; the buffer is handed back to the stream.
            unsafe { stream.queue_raw_buffer(raw) };
            env.shared.delivered.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Filled::Unwritable) => {
            // SAFETY: as above; queued with an empty chunk, nothing written.
            unsafe { stream.queue_raw_buffer(raw) };
            env.shared.failed.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            // SAFETY: as above; an unused buffer goes back to the pool.
            unsafe { pw::sys::pw_stream_return_buffer(stream.as_raw_ptr(), raw) };
            env.shared.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What [`fill`] made of a dequeued buffer.
#[derive(Debug, PartialEq, Eq)]
enum Filled {
    /// The frame is in it: queue it.
    Frame,
    /// Its data is not mapped writable with room for a frame: nothing was
    /// written, its chunk says empty; queue it (the frame is dropped).
    Unwritable,
}

/// The data block is mapped, writable and holds at least `need` bytes.
fn writable(data: &spa::sys::spa_data, need: usize) -> bool {
    data.flags & spa::sys::SPA_DATA_FLAG_WRITABLE != 0
        && !data.data.is_null()
        && usize::try_from(data.maxsize).is_ok_and(|max| max >= need)
}

/// The chunk of a buffer about to be queued: `size` bytes from offset 0,
/// no flags (a recycled buffer can carry a stale one, e.g. CORRUPTED).
fn set_chunk(chunk: &mut spa::sys::spa_chunk, size: usize, stride: usize) {
    chunk.offset = 0;
    chunk.size = u32::try_from(size).unwrap_or(u32::MAX);
    chunk.stride = i32::try_from(stride).unwrap_or(i32::MAX);
    chunk.flags = 0;
}

/// Converts `frame` (the writer's caps, `info`) into the first data block
/// of `raw` and sets its chunk and header; a data block not mapped
/// writable with room for the layout gets an empty chunk instead.
///
/// # Safety
/// `raw` must be a buffer dequeued from a stream and not yet queued.
unsafe fn fill(
    raw: *mut pw::sys::pw_buffer,
    frame: &gst::Buffer,
    layout: &Layout,
    info: &gst_video::VideoInfo,
) -> Result<Filled, String> {
    // SAFETY: the caller's contract; PipeWire fills these fields.
    let spa_buf = unsafe { (*raw).buffer };
    if spa_buf.is_null() || unsafe { (*spa_buf).n_datas } < 1 {
        return Err("a buffer without data".into());
    }
    // SAFETY: `datas` holds `n_datas` (>= 1) `spa_data`, ours until
    // queued.
    let data = unsafe { &mut *(*spa_buf).datas };
    if data.chunk.is_null() {
        return Err("a buffer without a chunk".into());
    }
    // SAFETY: non-null, in the buffer's metadata, ours until queued.
    let chunk = unsafe { &mut *data.chunk };
    if !writable(data, layout.size) {
        set_chunk(chunk, 0, 0);
        return Ok(Filled::Unwritable);
    }

    let vframe = gst_video::VideoFrameRef::from_buffer_ref_readable(frame.as_ref(), info)
        .map_err(|e| format!("cannot map the frame: {e}"))?;
    let format = match info.format() {
        gst_video::VideoFormat::Nv12 => obc_core::OutputFormat::Nv12,
        gst_video::VideoFormat::I420 => obc_core::OutputFormat::I420,
        other => return Err(format!("writer format {other:?}")),
    };
    let n = vframe.n_planes() as usize;
    let empty: &[u8] = &[];
    let mut planes = [empty; 3];
    let mut strides = [0usize; 3];
    for p in 0..n.min(3) {
        planes[p] = vframe
            .plane_data(p as u32)
            .map_err(|e| format!("plane {p}: {e}"))?;
        strides[p] = usize::try_from(vframe.plane_stride()[p]).map_err(|_| "negative stride")?;
    }
    let src = Source {
        format,
        width: vframe.width() as usize,
        height: vframe.height() as usize,
        planes,
        strides,
    };
    // SAFETY: mapped, writable and `maxsize` bytes long (checked above),
    // ours until queued; nothing else refers to it meanwhile.
    let dst =
        unsafe { std::slice::from_raw_parts_mut(data.data.cast::<u8>(), data.maxsize as usize) };
    convert(&src, dst, layout).map_err(|e| e.to_string())?;
    set_chunk(chunk, layout.size, layout.strides[0]);

    // SAFETY: `spa_buf` is valid (above); the header, when the consumer
    // accepted the Meta param, is a `spa_meta_header` of at least its size.
    let header = unsafe {
        spa::sys::spa_buffer_find_meta_data(
            spa_buf,
            spa::sys::SPA_META_Header,
            std::mem::size_of::<spa::sys::spa_meta_header>(),
        )
    }
    .cast::<spa::sys::spa_meta_header>();
    if !header.is_null() {
        let discont = frame.flags().contains(gst::BufferFlags::DISCONT);
        // SAFETY: non-null, sized as checked above, ours until queued.
        unsafe {
            *header = spa::sys::spa_meta_header {
                flags: if discont {
                    spa::sys::SPA_META_HEADER_FLAG_DISCONT
                } else {
                    0
                },
                offset: 0,
                pts: frame
                    .pts()
                    .map_or(-1, |p| i64::try_from(p.nseconds()).unwrap_or(i64::MAX)),
                dts_offset: 0,
                seq: frame.offset(),
            };
        }
    }
    Ok(Filled::Frame)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn shared() -> Arc<Shared> {
        let (sender, _receiver) = pw::channel::channel();
        Arc::new(Shared {
            streaming: Default::default(),
            wake_pending: Default::default(),
            latest: Mutex::default(),
            sender,
            state: Mutex::new(PwState::Connecting),
            consumers: Default::default(),
            node_id: NO_NODE.into(),
            delivered: Default::default(),
            no_buffer: Default::default(),
            superseded: Default::default(),
            idle_dropped: Default::default(),
            failed: Default::default(),
            link_errors: Default::default(),
        })
    }

    #[test]
    fn a_panicking_callback_body_becomes_the_failure() {
        let ctl = Ctl::default();
        let mut ran = false;
        guarded(&ctl, "test", || ran = true);
        assert!(ran);
        assert_eq!(*ctl.failure.borrow(), None);
        guarded(&ctl, "stream process", || panic!("boom {}", 1));
        let failure = ctl.failure.borrow().clone().unwrap();
        assert!(failure.contains("stream process") && failure.contains("boom 1"));
        // The first failure is kept.
        guarded(&ctl, "other", || panic!("second"));
        assert_eq!(ctl.failure.borrow().as_deref(), Some(failure.as_str()));
        // A &str payload too.
        let ctl = Ctl::default();
        guarded(&ctl, "core error", || std::panic::panic_any("static"));
        assert!(ctl.failure.borrow().as_deref().unwrap().contains("static"));
    }

    /// After a panic of the body the owner hears `Failed` and a count of
    /// 0, even while consumers were counted, and the shared state says so.
    #[test]
    fn a_panic_reports_failed_then_no_consumers() {
        let events: Arc<Mutex<Vec<PwEvent>>> = Arc::default();
        let e = Arc::clone(&events);
        let shared = shared();
        let reporter = Reporter {
            report: Box::new(move |ev| e.lock().unwrap().push(ev)),
            shared: Arc::clone(&shared),
            state: RefCell::new(PwState::Connecting),
            consumers: Cell::new(0),
        };
        reporter.state(PwState::Streaming);
        reporter.consumers(1);
        shared.set_streaming(true);
        shared.node_id.store(42, Ordering::Release);
        reporter.panicked("boom");
        let failed = PwState::Failed("the PipeWire thread panicked: boom".into());
        assert_eq!(
            events.lock().unwrap()[2..],
            [PwEvent::State(failed.clone()), PwEvent::Consumers(0)]
        );
        assert_eq!(shared.state(), failed);
        assert_eq!(shared.consumers.load(Ordering::Acquire), 0);
        assert_eq!(shared.node_id.load(Ordering::Acquire), NO_NODE);
        assert!(!shared.streaming.load(Ordering::Acquire));

        // A report that panics itself does not unwind out of it.
        let shared = super::tests::shared();
        let reporter = Reporter {
            report: Box::new(|_| panic!("report")),
            shared: Arc::clone(&shared),
            state: RefCell::new(PwState::Connecting),
            consumers: Cell::new(3),
        };
        reporter.panicked("boom");
        assert!(matches!(shared.state(), PwState::Failed(_)));
    }

    #[test]
    fn only_a_mapped_writable_block_with_room_is_written() {
        let mut byte = 0u8;
        // SAFETY: a plain C struct; all-zero is a valid value.
        let mut d: spa::sys::spa_data = unsafe { std::mem::zeroed() };
        d.flags = spa::sys::SPA_DATA_FLAG_READWRITE;
        d.data = (&raw mut byte).cast();
        d.maxsize = 100;
        assert!(writable(&d, 100));
        assert!(!writable(&d, 101));
        d.flags = spa::sys::SPA_DATA_FLAG_READABLE;
        assert!(!writable(&d, 1));
        d.flags = spa::sys::SPA_DATA_FLAG_WRITABLE;
        assert!(writable(&d, 1));
        d.data = std::ptr::null_mut();
        assert!(!writable(&d, 0));
    }

    #[test]
    fn a_queued_chunk_carries_no_stale_flag() {
        let mut c = spa::sys::spa_chunk {
            offset: 7,
            size: 9,
            stride: 3,
            flags: spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32,
        };
        set_chunk(&mut c, 3_110_400, 1920);
        assert_eq!(
            (c.offset, c.size, c.stride, c.flags),
            (0, 3_110_400, 1920, 0)
        );
        c.flags = spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32;
        set_chunk(&mut c, 0, 0);
        assert_eq!((c.offset, c.size, c.stride, c.flags), (0, 0, 0, 0));
    }
}
