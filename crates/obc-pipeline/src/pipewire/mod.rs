//! An optional native PipeWire camera node (cargo feature `pipewire`, off
//! by default): a `Video/Source` stream, "Omarchy Broadcast" by default,
//! carrying exactly the frames the persistent loopback writer sends
//! ([`crate::loopback`]), with the writer's pacing and timestamps.
//!
//! ```text
//! LoopbackWriter ──push──> appsrc ! v4l2sink              (always)
//!        └──FrameTap──> latest-frame slot ──wake──> obc-pipewire thread
//!                                                    dequeue, convert, queue
//!                                                    ──> PipeWire consumers
//! ```
//!
//! **Why native.** `pipewiresink` was rejected: with one fixed format, a
//! consumer asking for another kills the producer (research, spike result
//! 2). The native stream (pipewire-rs) owns its EnumFormat list and its
//! buffers, and a consumer's failed negotiation fails only that consumer's
//! link.
//!
//! **Node.** One thread, `obc-pipewire`, owns the PipeWire main loop,
//! context, core, registry and an output stream with `media.class =
//! Video/Source` and `media.role = Camera` (the camera portal lists a node
//! only with both), `media.category = Capture`, `node.name` and
//! `node.description` from [`PwOutputConfig`]. The stream is a driver
//! (`DRIVER`, no `AUTOCONNECT`: a provider links to nobody), maps its
//! buffers itself (`MAP_BUFFERS`), and stays registered and linkable while
//! no consumer is linked and while no frame arrives.
//!
//! **Formats.** Three EnumFormat entries, all at the contract's exact size
//! and frame rate, in the order YUY2, NV12, I420 ([`format::offered_formats`];
//! YUY2 first because OBS 32.x overreads NV12 buffers).
//! Nothing is scaled. When a consumer has chosen, the stream asks for buffers of one data block
//! holding a whole frame in `GstVideoInfo`'s default layout
//! ([`format::Layout`], what `pipewiresink` sends and `pipewiresrc` and
//! browsers accept), in memfd or plain memory, with a `spa_meta_header`.
//! No colour range or matrix is announced: v4l2loopback carries none
//! either, and consumers read the frames as limited-range BT.601 (the
//! writer's label).
//!
//! **Frames.** The [`FrameTap`] given to
//! [`LoopbackWriter::set_tap`](crate::loopback::LoopbackWriter::set_tap)
//! keeps only the latest frame (a reference, no copy) and wakes the loop
//! once per pending frame; while the stream is not streaming (no consumer
//! linked) it drops the frame at once, so nothing crosses threads while
//! nobody watches. On the loop, the stream dequeues a buffer (none free:
//! the frame is dropped and counted), converts the frame into it and
//! queues it with the writer's PTS (nanoseconds) and push index
//! (`spa_meta_header` `pts` and `seq`). There is no timer of our own and
//! no restamp.
//!
//! **Conversion on the CPU** ([`convert`]), inside the one copy into the
//! PipeWire buffer that is needed anyway. Not on the GPU before the
//! download, because the format is the consumer's choice at run time
//! (and can differ between consumers over the node's life), the idle
//! placeholders never touch the GPU (ADR-0011: Idle holds no CUDA
//! context), and a GPU conversion would need a second `cudadownload` next
//! to the loopback's.
//!
//! **Consumers** ([`links`]): the Link globals of the registry whose output
//! node is ours and whose state is not `Error`, reported as
//! [`PwEvent::Consumers`] on every change. Each such link is bound and its
//! state followed: a consumer whose negotiation failed but that keeps its
//! stream (and so its link) alive is not counted, so it holds no camera.
//! The stream's own state gives [`PwState::Ready`] (registered, no
//! consumer streaming) and [`PwState::Streaming`].
//!
//! **Failure.** No PipeWire daemon is not an error of
//! [`PipeWireOutput::start`]: a failed connection, a core error, a
//! disconnection or a stream really in error reports [`PwState::Failed`]
//! and a count of 0, tears the connection down and retries after
//! [`RETRY_AFTER`] (reporting `Connecting`, then `Ready`). Right after
//! start the output always reports `State(Connecting)` and `Consumers(0)`,
//! then every change. A panic inside a PipeWire callback is such a failure
//! (it never unwinds into libpipewire); a panic elsewhere in the thread
//! tears the node down, reports `State(Failed)` and `Consumers(0)`, and
//! ends the thread (no retry).
//!
//! **A refused consumer is not a failure.** When a consumer asks for a
//! format or size not offered, PipeWire posts the link's error ("no more
//! input formats") on our node's proxy too, and libpipewire announces it
//! as a state change to `Error` while leaving the stream in its state
//! (it is the application's call whether the error is permanent). Taking
//! that announcement as fatal is what kills a `pipewiresink` producer
//! (research, spike result 2). Here the stream's actual state decides: the
//! announcement is only counted ([`PwCounts::link_errors`]), the node keeps
//! its id and serves the next consumer.
//!
//! The library logs nothing: everything is reported through the events,
//! [`PipeWireOutput::state`] and [`PipeWireOutput::counts`].

pub mod convert;
pub mod format;
pub mod links;
mod stream;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use obc_core::OutputContract;
use serde::Serialize;

use crate::loopback::FrameTap;
use crate::pipeline::PipelineError;

/// How long after a failure the connection is tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(2);

/// Everything needed to start the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PwOutputConfig {
    /// The frames the writer carries (format, size, frame rate).
    pub contract: OutputContract,
    /// `node.name`: stable, what consumers target.
    pub node_name: String,
    /// `node.description` and `media.name`: what pickers show.
    pub description: String,
}

impl PwOutputConfig {
    pub const DEFAULT_NODE_NAME: &str = "oma-broadcast";
    pub const DEFAULT_DESCRIPTION: &str = "Omarchy Broadcast";

    /// The daemon's defaults for `contract`.
    pub fn new(contract: OutputContract) -> Self {
        PwOutputConfig {
            contract,
            node_name: Self::DEFAULT_NODE_NAME.into(),
            description: Self::DEFAULT_DESCRIPTION.into(),
        }
    }

    fn validate(&self) -> Result<(), PipelineError> {
        self.contract
            .validate()
            .map_err(|e| PipelineError::Caps(e.to_string()))?;
        for (what, v) in [
            ("node name", &self.node_name),
            ("description", &self.description),
        ] {
            if v.trim().is_empty() || v.contains('\0') {
                return Err(PipelineError::State(format!(
                    "PipeWire output: invalid {what} {v:?}"
                )));
            }
        }
        Ok(())
    }
}

/// The node's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PwState {
    /// Connecting to PipeWire, or the stream not registered yet.
    Connecting,
    /// The node is registered and linkable; no consumer is streaming.
    Ready,
    /// A consumer is linked and frames are sent.
    Streaming,
    /// The connection failed or broke (the reason); retried after
    /// [`RETRY_AFTER`].
    Failed(String),
}

/// What the output reports, in order, from its thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PwEvent {
    /// Links from our node (consumers).
    Consumers(u32),
    State(PwState),
}

/// A snapshot of the frame counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PwCounts {
    /// Frames queued to PipeWire.
    pub delivered: u64,
    /// Frames dropped because no buffer was free.
    pub no_buffer: u64,
    /// Frames replaced in the slot by a newer one before the loop took them.
    pub superseded: u64,
    /// Frames the tap dropped because the stream was not streaming.
    pub idle_dropped: u64,
    /// Frames that could not be mapped or converted, or whose buffer was
    /// not mapped writable with room for a frame (dropped; that buffer is
    /// queued empty).
    pub failed: u64,
    /// Errors announced on our node that left the stream as it was (a
    /// consumer's failed negotiation fails only that consumer's link).
    pub link_errors: u64,
}

/// Messages to the loop thread.
enum Msg {
    /// A frame is waiting in the slot.
    Frame,
    Quit,
}

/// What the tap, the loop thread and the owner share.
struct Shared {
    /// The stream is streaming (set by its state callback).
    streaming: AtomicBool,
    /// A [`Msg::Frame`] is on its way.
    wake_pending: AtomicBool,
    latest: Mutex<Option<gst::Buffer>>,
    sender: pipewire::channel::Sender<Msg>,
    state: Mutex<PwState>,
    consumers: AtomicU32,
    /// [`NO_NODE`] while not registered.
    node_id: AtomicU32,
    delivered: AtomicU64,
    no_buffer: AtomicU64,
    superseded: AtomicU64,
    idle_dropped: AtomicU64,
    failed: AtomicU64,
    link_errors: AtomicU64,
}

const NO_NODE: u32 = u32::MAX;

impl Shared {
    /// The tap: keep the latest frame and wake the loop, or drop it at once
    /// while not streaming. Never blocks beyond the slot's lock.
    fn offer(&self, buffer: &gst::Buffer) {
        if !self.streaming.load(Ordering::Acquire) {
            self.idle_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let old = self
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(buffer.clone());
        if old.is_some() {
            self.superseded.fetch_add(1, Ordering::Relaxed);
        }
        if !self.wake_pending.swap(true, Ordering::AcqRel) && self.sender.send(Msg::Frame).is_err()
        {
            self.wake_pending.store(false, Ordering::Release);
        }
    }

    fn take_latest(&self) -> Option<gst::Buffer> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Streaming or not; leaving it drops the waiting frame.
    fn set_streaming(&self, on: bool) {
        self.streaming.store(on, Ordering::Release);
        if !on {
            self.take_latest();
        }
    }

    fn set_state(&self, state: PwState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }

    fn state(&self) -> PwState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The running node. See the module docs.
pub struct PipeWireOutput {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    cfg: PwOutputConfig,
}

impl std::fmt::Debug for PipeWireOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireOutput")
            .field("node_name", &self.cfg.node_name)
            .field("state", &self.state())
            .field("consumers", &self.consumers())
            .finish_non_exhaustive()
    }
}

impl PipeWireOutput {
    /// Starts the `obc-pipewire` thread, which connects to PipeWire and
    /// registers the node, retrying while PipeWire is unavailable.
    /// `report` is called on that thread: `State(Connecting)` and
    /// `Consumers(0)` first, then every change. Fails only for an invalid
    /// config, or a thread or PipeWire main loop that cannot be created.
    pub fn start(
        cfg: PwOutputConfig,
        report: impl Fn(PwEvent) + Send + 'static,
    ) -> Result<Self, PipelineError> {
        cfg.validate()?;
        gst::init().map_err(|e| PipelineError::State(e.to_string()))?;
        let caps_str = crate::loopback::writer_caps_string(&cfg.contract);
        let caps: gst::Caps = caps_str
            .parse()
            .map_err(|_| PipelineError::Caps(caps_str.clone()))?;
        let writer_info =
            gst_video::VideoInfo::from_caps(&caps).map_err(|_| PipelineError::Caps(caps_str))?;
        let (sender, receiver) = pipewire::channel::channel();
        let shared = Arc::new(Shared {
            streaming: AtomicBool::new(false),
            wake_pending: AtomicBool::new(false),
            latest: Mutex::default(),
            sender,
            state: Mutex::new(PwState::Connecting),
            consumers: AtomicU32::new(0),
            node_id: AtomicU32::new(NO_NODE),
            delivered: AtomicU64::new(0),
            no_buffer: AtomicU64::new(0),
            superseded: AtomicU64::new(0),
            idle_dropped: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            link_errors: AtomicU64::new(0),
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let thread_cfg = cfg.clone();
        let handle = std::thread::Builder::new()
            .name("obc-pipewire".into())
            .spawn(move || {
                stream::run(
                    thread_cfg,
                    writer_info,
                    thread_shared,
                    receiver,
                    Box::new(report),
                    ready_tx,
                )
            })
            .map_err(|e| PipelineError::State(format!("PipeWire thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(PipeWireOutput {
                shared,
                thread: Some(handle),
                cfg,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(PipelineError::State(format!("PipeWire: {e}")))
            }
            Err(_) => {
                let _ = handle.join();
                Err(PipelineError::State(
                    "the PipeWire thread ended before it started".into(),
                ))
            }
        }
    }

    /// The tap to install on the loopback writer
    /// ([`LoopbackWriter::set_tap`](crate::loopback::LoopbackWriter::set_tap)).
    /// It holds the output's shared state, not the thread: once the output
    /// is stopped it only drops frames.
    pub fn tap(&self) -> FrameTap {
        let shared = Arc::clone(&self.shared);
        Arc::new(move |b: &gst::Buffer| shared.offer(b))
    }

    pub fn state(&self) -> PwState {
        self.shared.state()
    }

    pub fn consumers(&self) -> u32 {
        self.shared.consumers.load(Ordering::Acquire)
    }

    /// Our node's id while registered.
    pub fn node_id(&self) -> Option<u32> {
        let id = self.shared.node_id.load(Ordering::Acquire);
        (id != NO_NODE).then_some(id)
    }

    pub fn counts(&self) -> PwCounts {
        let s = &self.shared;
        PwCounts {
            delivered: s.delivered.load(Ordering::Relaxed),
            no_buffer: s.no_buffer.load(Ordering::Relaxed),
            superseded: s.superseded.load(Ordering::Relaxed),
            idle_dropped: s.idle_dropped.load(Ordering::Relaxed),
            failed: s.failed.load(Ordering::Relaxed),
            link_errors: s.link_errors.load(Ordering::Relaxed),
        }
    }

    pub fn config(&self) -> &PwOutputConfig {
        &self.cfg
    }

    /// Quits the loop and joins the thread: the node disappears. Also done
    /// on drop.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.thread.take() {
            let _ = self.shared.sender.send(Msg::Quit);
            let _ = handle.join();
            self.shared.set_streaming(false);
            self.shared.node_id.store(NO_NODE, Ordering::Release);
            self.shared.consumers.store(0, Ordering::Release);
        }
    }
}

impl Drop for PipeWireOutput {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    /// Without a PipeWire daemon (a remote that does not exist) start
    /// succeeds and reports Connecting and a count of 0, then Failed with
    /// the reason, and tries again after [`RETRY_AFTER`]; the tap drops
    /// frames while nothing streams; stop returns at once, mid-wait.
    #[test]
    fn without_pipewire_the_output_reports_failed_and_retries() {
        *stream::TEST_REMOTE.lock().unwrap() =
            Some(format!("obc-test-no-such-remote-{}", std::process::id()));
        let events: Arc<Mutex<Vec<(Instant, PwEvent)>>> = Arc::default();
        let e = Arc::clone(&events);
        let t0 = Instant::now();
        let out =
            PipeWireOutput::start(PwOutputConfig::new(OutputContract::default()), move |ev| {
                e.lock().unwrap().push((Instant::now(), ev))
            })
            .unwrap();
        let failures = |ev: &[(Instant, PwEvent)]| {
            ev.iter()
                .filter(|(_, e)| matches!(e, PwEvent::State(PwState::Failed(_))))
                .map(|(t, _)| *t)
                .collect::<Vec<_>>()
        };
        let deadline = t0 + RETRY_AFTER * 3;
        while failures(&events.lock().unwrap()).len() < 2 {
            assert!(Instant::now() < deadline, "{:?}", events.lock().unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
        let tap = out.tap();
        tap(&gst::Buffer::new());
        assert_eq!(out.counts().idle_dropped, 1);
        assert!(
            matches!(out.state(), PwState::Failed(_)),
            "{:?}",
            out.state()
        );
        assert_eq!((out.consumers(), out.node_id()), (0, None));
        let stopping = Instant::now();
        out.stop();
        assert!(stopping.elapsed() < Duration::from_secs(1), "stop waited");
        *stream::TEST_REMOTE.lock().unwrap() = None;

        let ev = events.lock().unwrap();
        let kinds: Vec<&PwEvent> = ev.iter().map(|(_, e)| e).collect();
        assert_eq!(
            kinds[..2],
            [&PwEvent::State(PwState::Connecting), &PwEvent::Consumers(0)]
        );
        let PwEvent::State(PwState::Failed(reason)) = kinds[2] else {
            panic!("{kinds:?}")
        };
        assert!(reason.contains("connect"), "{reason}");
        // Failed, then Connecting again after the retry delay, then Failed.
        assert_eq!(kinds[3], &PwEvent::State(PwState::Connecting));
        assert!(matches!(kinds[4], PwEvent::State(PwState::Failed(_))));
        let f = failures(&ev);
        assert!(
            f[1] - f[0] >= RETRY_AFTER,
            "retried after {:?}",
            f[1] - f[0]
        );
        // The count was 0 throughout: reported once.
        assert_eq!(
            kinds
                .iter()
                .filter(|e| matches!(e, PwEvent::Consumers(_)))
                .count(),
            1
        );
    }

    /// A panic of the thread's body is reported: `Failed` with the panic,
    /// then a count of 0; the output still stops.
    #[test]
    fn a_panic_of_the_thread_reports_failed_and_no_consumers() {
        let events: Arc<Mutex<Vec<PwEvent>>> = Arc::default();
        let e = Arc::clone(&events);
        let cfg = PwOutputConfig {
            node_name: stream::TEST_PANIC_NODE.into(),
            ..PwOutputConfig::new(OutputContract::default())
        };
        let out = PipeWireOutput::start(cfg, move |ev| e.lock().unwrap().push(ev)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while events.lock().unwrap().len() < 4 {
            assert!(Instant::now() < deadline, "{:?}", events.lock().unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
        let failed = PwState::Failed("the PipeWire thread panicked: a test panic".into());
        assert_eq!(
            *events.lock().unwrap(),
            [
                PwEvent::State(PwState::Connecting),
                PwEvent::Consumers(0),
                PwEvent::State(failed.clone()),
                PwEvent::Consumers(0),
            ]
        );
        assert_eq!(
            (out.state(), out.consumers(), out.node_id()),
            (failed, 0, None)
        );
        out.stop();
    }

    #[test]
    fn config_defaults_and_validation() {
        let cfg = PwOutputConfig::new(OutputContract::default());
        assert_eq!(cfg.node_name, "oma-broadcast");
        assert_eq!(cfg.description, "Omarchy Broadcast");
        cfg.validate().unwrap();
        let bad = PwOutputConfig {
            node_name: " ".into(),
            ..cfg.clone()
        };
        assert!(bad.validate().is_err());
        let odd = PwOutputConfig {
            contract: OutputContract {
                width: 641,
                ..OutputContract::default()
            },
            ..cfg
        };
        assert!(matches!(odd.validate(), Err(PipelineError::Caps(_))));
    }
}
