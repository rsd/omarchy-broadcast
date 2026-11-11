//! The non-real-time side: the state shared with the audio thread, and the
//! helper thread that connects, watches and retires connections.
//!
//! # Handing a connection to the audio thread
//!
//! The helper publishes a boxed [`Link`] in `Shared::link`. The audio
//! thread brackets every `run()` with [`Shared::enter`] / [`Shared::leave`],
//! which make an epoch counter odd while it may hold the pointer. To retire
//! a link the helper swaps the pointer to null and, if the epoch it then
//! reads is odd, waits until it changes; only then is the link freed. With
//! sequentially consistent swap/increment/loads, a `run()` either loaded
//! the old pointer before the swap (and the helper waits for its `leave`)
//! or loads null. The audio thread never waits and never frees.
//!
//! # When the helper connects
//!
//! Only when audio flows. The audio thread calls [`Shared::wake`] from
//! `run()` when it has no link and is not bypassed; that sets a flag and
//! writes a plugin-local eventfd once (no further syscalls until the
//! helper re-arms it). The helper then connects, retrying with back-off
//! for as long as `run()` keeps being called; when the calls stop it goes
//! back to sleep on the eventfd. An idle mic host therefore never connects,
//! and never socket-activates a worker that exited on idle.
//!
//! # Bypass
//!
//! Every `run()` publishes whether it wants a link ([`Shared::set_wants_link`]:
//! not bypassed, at 48 kHz). While it does not, the helper drops an
//! installed link (the worker sees the hang-up and may exit on idle), makes
//! no connection attempt, and goes back to sleep; `run()` does not wake it
//! while bypassed. Leaving bypass wakes it as usual.
//!
//! # Shutdown
//!
//! `cleanup` sets the shutdown flag and writes a second eventfd that is
//! never drained. Every wait of the helper, the handshake included
//! ([`client::connect_cancellable`]), polls it, so `cleanup` returns at
//! once whatever state the worker is in (not accepting, backlog full,
//! never answering).

use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use obc_audio::client::{self, ConnectError};
use obc_audio::sys;

use crate::engine::Link;

/// Connection settings of one plugin instance. The defaults are what the
/// mic host uses; tests shorten the timings.
#[derive(Debug, Clone)]
pub struct ShimConfig {
    /// The worker socket; `None` uses [`obc_audio::default_socket_path`]
    /// (which honours `OMARCHY_BROADCAST_AUDIO_SOCKET`).
    pub socket_path: Option<PathBuf>,
    /// Blocks per ring.
    pub ring_capacity: u32,
    /// How long the handshake may take.
    pub connect_timeout: Duration,
    /// First retry delay after a failed connect; doubles up to `retry_max`.
    pub retry_min: Duration,
    /// Longest retry delay after failed connects.
    pub retry_max: Duration,
    /// Retry delay after a refusal (another protocol version, busy).
    pub refused_retry: Duration,
    /// Delay before reconnecting after the worker hung up (only if audio
    /// kept flowing during it).
    pub reconnect_delay: Duration,
    /// A connection whose heartbeat does not move for this long while
    /// audio flows is dropped.
    pub hung_timeout: Duration,
}

impl Default for ShimConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            ring_capacity: obc_audio::DEFAULT_RING_CAPACITY,
            connect_timeout: Duration::from_secs(2),
            retry_min: Duration::from_millis(100),
            retry_max: Duration::from_secs(5),
            refused_retry: Duration::from_secs(60),
            reconnect_delay: Duration::from_millis(50),
            hung_timeout: Duration::from_secs(2),
        }
    }
}

/// A snapshot of an instance's state, for diagnostics and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShimStats {
    /// A worker connection is installed.
    pub connected: bool,
    /// Successful handshakes so far.
    pub connects: u64,
    /// Connection attempts so far (including failures and refusals).
    pub attempts: u64,
    /// Refusals (including version mismatches) so far.
    pub refusals: u64,
    /// Blocks missed so far.
    pub misses: u64,
    /// The `latency` port value, in samples.
    pub latency_samples: u32,
    /// The transport delay in use, in samples.
    pub transport_delay: u32,
    /// The output is fully the processed stream.
    pub processing: bool,
    /// `run()` calls so far.
    pub runs: u64,
    /// Input blocks sent to a worker so far.
    pub blocks_sent: u64,
}

/// State shared between the audio thread and the helper.
#[derive(Debug)]
pub struct Shared {
    link: AtomicPtr<Link>,
    epoch: AtomicU64,
    runs: AtomicU64,
    wake_pending: AtomicBool,
    wake_fd: OwnedFd,
    stop_fd: OwnedFd,
    wants_link: AtomicBool,
    shutdown: AtomicBool,
    link_bad: AtomicBool,
    heartbeat: AtomicU64,
    /// Settings, read by the helper before each connect.
    pub config: Mutex<ShimConfig>,
    connected: AtomicBool,
    connects: AtomicU64,
    attempts: AtomicU64,
    refusals: AtomicU64,
    misses: AtomicU64,
    sent: AtomicU64,
    latency: AtomicU32,
    transport: AtomicU32,
    processing: AtomicBool,
}

impl Shared {
    /// New shared state (creates the wake and stop eventfds).
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            link: AtomicPtr::new(ptr::null_mut()),
            epoch: AtomicU64::new(0),
            runs: AtomicU64::new(0),
            wake_pending: AtomicBool::new(false),
            wake_fd: sys::eventfd_new()?,
            stop_fd: sys::eventfd_new()?,
            wants_link: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            link_bad: AtomicBool::new(false),
            heartbeat: AtomicU64::new(0),
            config: Mutex::new(ShimConfig::default()),
            connected: AtomicBool::new(false),
            connects: AtomicU64::new(0),
            attempts: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            latency: AtomicU32::new(crate::engine::NOMINAL_DELAY),
            transport: AtomicU32::new(crate::engine::NOMINAL_DELAY),
            processing: AtomicBool::new(false),
        })
    }

    /// Audio thread, start of `run()`: the link it may use until
    /// [`Shared::leave`] (null if none).
    pub fn enter(&self) -> *mut Link {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.link.load(Ordering::SeqCst)
    }

    /// Audio thread, end of `run()`. `heartbeat` is the link's heartbeat
    /// (mirrored for the helper, which never touches the link itself).
    pub fn leave(&self, heartbeat: Option<u64>, corrupt: bool) {
        if let Some(hb) = heartbeat {
            self.heartbeat.store(hb, Ordering::Relaxed);
        }
        if corrupt {
            self.link_bad.store(true, Ordering::Relaxed);
        }
        self.epoch.fetch_add(1, Ordering::Release);
        self.runs.fetch_add(1, Ordering::Release);
    }

    /// Audio thread: publishes statistics (relaxed stores only).
    pub fn publish(&self, e: &crate::engine::Engine) {
        self.misses.store(e.misses, Ordering::Relaxed);
        self.sent.store(e.sent, Ordering::Relaxed);
        self.latency.store(e.latency_port(), Ordering::Relaxed);
        self.transport.store(e.proc_delay(), Ordering::Relaxed);
        self.processing.store(e.processing(), Ordering::Relaxed);
    }

    /// Audio thread: asks the helper to connect. One eventfd write per
    /// helper sleep, nothing after that.
    pub fn wake(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            sys::eventfd_signal(self.wake_fd.as_fd());
        }
    }

    /// Audio thread, every `run()`: whether this run wants a worker link
    /// (`false` while bypassed). One relaxed store.
    pub fn set_wants_link(&self, wants: bool) {
        self.wants_link.store(wants, Ordering::Relaxed);
    }

    fn wants_link(&self) -> bool {
        self.wants_link.load(Ordering::Relaxed)
    }

    /// Stops the helper (from `cleanup`), interrupting any wait at once.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        sys::eventfd_signal(self.stop_fd.as_fd());
        sys::eventfd_signal(self.wake_fd.as_fd());
    }

    /// The statistics snapshot.
    pub fn stats(&self) -> ShimStats {
        ShimStats {
            connected: self.connected.load(Ordering::Relaxed),
            connects: self.connects.load(Ordering::Relaxed),
            attempts: self.attempts.load(Ordering::Relaxed),
            refusals: self.refusals.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            latency_samples: self.latency.load(Ordering::Relaxed),
            transport_delay: self.transport.load(Ordering::Relaxed),
            processing: self.processing.load(Ordering::Relaxed),
            runs: self.runs.load(Ordering::Relaxed),
            blocks_sent: self.sent.load(Ordering::Relaxed),
        }
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn runs(&self) -> u64 {
        self.runs.load(Ordering::Acquire)
    }

    /// Waits up to `timeout` (forever if `None`) on the wake eventfd;
    /// returns early on a wake or shutdown.
    fn sleep(&self, timeout: Option<Duration>) {
        let mut pfd = [
            sys::pollfd(self.wake_fd.as_fd(), libc::POLLIN),
            sys::pollfd(self.stop_fd.as_fd(), libc::POLLIN),
        ];
        let _ = sys::poll(&mut pfd, timeout);
        sys::eventfd_drain(self.wake_fd.as_fd());
    }

    /// Sleeps for `d` unless shut down; wakes in between are ignored.
    fn sleep_for(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.is_shutdown() {
            let now = Instant::now();
            if now >= end {
                break;
            }
            self.sleep(Some(end - now));
        }
    }

    fn install(&self, link: Box<Link>) {
        self.link_bad.store(false, Ordering::Relaxed);
        self.link.store(Box::into_raw(link), Ordering::SeqCst);
        self.connected.store(true, Ordering::Relaxed);
    }

    /// Takes the link away from the audio thread and frees it once the
    /// audio thread cannot hold it any more.
    pub fn retire(&self) {
        let old = self.link.swap(ptr::null_mut(), Ordering::SeqCst);
        self.connected.store(false, Ordering::Relaxed);
        if old.is_null() {
            return;
        }
        let e = self.epoch.load(Ordering::SeqCst);
        if e % 2 == 1 {
            while self.epoch.load(Ordering::SeqCst) == e {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        // SAFETY: `old` came from Box::into_raw in `install`, was swapped
        // out so no new run() can load it, and the epoch check above
        // guarantees the run() that might have loaded it has left.
        drop(unsafe { Box::from_raw(old) });
    }
}

/// The helper thread's body; returns on shutdown.
pub fn helper_main(shared: Arc<Shared>) {
    let mut next_id = 1u64;
    'idle: loop {
        // Re-arm the wake and sleep until the audio thread asks.
        shared.wake_pending.store(false, Ordering::SeqCst);
        shared.sleep(None);
        if shared.is_shutdown() {
            break;
        }
        let mut backoff: Option<Duration> = None;
        loop {
            if !shared.wants_link() {
                continue 'idle; // bypassed: no link until run() asks again
            }
            let config = shared.config.lock().map(|c| c.clone()).unwrap_or_default();
            let delay = match attempt(&shared, &config, next_id) {
                Attempt::Served => {
                    next_id += 1;
                    backoff = None;
                    config.reconnect_delay
                }
                Attempt::Refused => config.refused_retry,
                Attempt::Failed => {
                    let d = backoff.map_or(config.retry_min, |b| (b * 2).min(config.retry_max));
                    backoff = Some(d);
                    d
                }
            };
            if shared.is_shutdown() {
                break 'idle;
            }
            let mark = shared.runs();
            shared.sleep_for(delay);
            if shared.is_shutdown() {
                break 'idle;
            }
            if shared.runs() == mark {
                continue 'idle; // audio stopped: wait for the next wake
            }
        }
    }
    shared.retire();
}

enum Attempt {
    Served,
    Refused,
    Failed,
}

fn attempt(shared: &Shared, config: &ShimConfig, id: u64) -> Attempt {
    shared.attempts.fetch_add(1, Ordering::Relaxed);
    let Some(path) = config
        .socket_path
        .clone()
        .or_else(obc_audio::default_socket_path)
    else {
        return Attempt::Failed;
    };
    match client::connect_cancellable(
        &path,
        config.ring_capacity,
        config.connect_timeout,
        Some(shared.stop_fd.as_fd()),
    ) {
        Ok((rings, control)) => {
            shared.connects.fetch_add(1, Ordering::Relaxed);
            shared.install(Box::new(Link { id, rings }));
            serve(shared, config, &control.socket);
            shared.retire();
            Attempt::Served
        }
        Err(e) if e.is_refusal() => {
            shared.refusals.fetch_add(1, Ordering::Relaxed);
            Attempt::Refused
        }
        Err(ConnectError::Io(_) | ConnectError::Protocol(_)) => Attempt::Failed,
        Err(_) => Attempt::Failed,
    }
}

/// Watches an installed connection until the worker hangs up, stalls while
/// audio flows, corrupts its ring, the plugin is bypassed, or it shuts down.
fn serve(shared: &Shared, config: &ShimConfig, socket: &sys::SeqPacket) {
    let mut last_hb = shared.heartbeat.load(Ordering::Relaxed);
    let mut last_change = Instant::now();
    let mut runs_at_change = shared.runs();
    loop {
        let mut pfd = [
            sys::pollfd(socket.as_fd(), libc::POLLIN),
            sys::pollfd(shared.wake_fd.as_fd(), libc::POLLIN),
            sys::pollfd(shared.stop_fd.as_fd(), libc::POLLIN),
        ];
        let _ = sys::poll(&mut pfd, Some(Duration::from_millis(100)));
        if shared.is_shutdown() || shared.link_bad.load(Ordering::Relaxed) || !shared.wants_link() {
            return;
        }
        if pfd[1].revents != 0 {
            sys::eventfd_drain(shared.wake_fd.as_fd());
        }
        if pfd[0].revents != 0 {
            // The worker sends nothing after the accept: readable is a
            // hang-up (or a protocol violation, handled the same way).
            return;
        }
        let runs = shared.runs();
        let hb = shared.heartbeat.load(Ordering::Relaxed);
        if hb != last_hb {
            last_hb = hb;
            last_change = Instant::now();
            runs_at_change = runs;
        } else if runs != runs_at_change && last_change.elapsed() > config.hung_timeout {
            return; // the worker is alive on the socket but not working
        }
    }
}
