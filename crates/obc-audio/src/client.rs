//! The plugin's side: connect, hand over the rings, then push raw blocks
//! and pop processed ones from the real-time thread.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::proto::{Accept, Hello, MAX_MESSAGE_LEN, Refusal, Reply, ReplyError};
use crate::shm::{BlockMeta, ClientCounters, Region, Ring, RingCorrupt};
use crate::sys::{self, SeqPacket};
use crate::{BLOCK_SAMPLES, MAX_LATENCY_SAMPLES, MAX_PIPELINE_DEPTH};

/// Why [`connect`] did not produce a connection.
#[derive(Debug)]
pub enum ConnectError {
    /// No worker socket, a timeout, or another system error.
    Io(io::Error),
    /// The worker refused (including a version mismatch it detected).
    Refused(Refusal),
    /// The worker speaks another protocol version (its reply said so).
    Version(u32),
    /// The worker answered something that is not a valid reply, or an
    /// accept with values out of range.
    Protocol(&'static str),
    /// The cancel descriptor became readable (the plugin is shutting down).
    Cancelled,
}

impl ConnectError {
    /// A refusal or a version mismatch: the worker is there but will not
    /// take this plugin; retrying soon will not help.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused(_) | Self::Version(_))
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "connect: {e}"),
            Self::Refused(r) => write!(f, "{r}"),
            Self::Version(v) => write!(f, "worker speaks protocol version {v}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
            Self::Cancelled => f.write_str("connect cancelled"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<io::Error> for ConnectError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A connection's control socket and its accept values.
#[derive(Debug)]
pub struct Control {
    /// The connected socket (non-blocking). The worker sends nothing after
    /// the accept; it becomes readable (end of file) when the worker goes
    /// away.
    pub socket: SeqPacket,
    /// What the worker accepted with.
    pub accept: Accept,
}

/// Connects to the worker at `path`: creates the region for rings of
/// `ring_capacity` blocks and the wake socket, sends the hello with both,
/// and waits up to `timeout` for the reply. Not real-time safe. Same as
/// [`connect_cancellable`] without a cancel descriptor.
pub fn connect(
    path: &Path,
    ring_capacity: u32,
    timeout: Duration,
) -> Result<(ClientRings, Control), ConnectError> {
    connect_cancellable(path, ring_capacity, timeout, None)
}

/// [`connect`], abandoned with [`ConnectError::Cancelled`] as soon as
/// `cancel` becomes readable. The whole exchange (connect, hello, reply) is
/// bounded by `timeout` and never blocks in a system call: the socket is
/// non-blocking from the start, and a worker whose listen backlog is full
/// is a `WouldBlock` failure at once. The returned control socket stays
/// non-blocking.
pub fn connect_cancellable(
    path: &Path,
    ring_capacity: u32,
    timeout: Duration,
    cancel: Option<BorrowedFd<'_>>,
) -> Result<(ClientRings, Control), ConnectError> {
    let deadline = Instant::now() + timeout;
    let region = Region::create(ring_capacity)?;
    let (wake, wake_peer) = sys::wake_pair()?;
    let (socket, in_progress) = SeqPacket::connect_nonblocking(path)?;
    if in_progress {
        wait_for(&socket, libc::POLLOUT, deadline, cancel)?;
        if let Some(e) = socket.take_error()? {
            return Err(ConnectError::Io(e));
        }
    }
    let hello = Hello::new(ring_capacity);
    socket.send(&hello.encode(), &[region.fd(), wake_peer.as_fd()])?;
    drop(wake_peer);
    wait_for(&socket, libc::POLLIN, deadline, cancel)?;
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    let (n, _fds) = socket.recv(&mut buf)?;
    if n == 0 {
        return Err(ConnectError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "worker closed the connection during the handshake",
        )));
    }
    let accept = match Reply::decode(&buf[..n]) {
        Ok(Reply::Accept(a)) => a,
        Ok(Reply::Refuse(r)) => return Err(ConnectError::Refused(r)),
        Err(ReplyError::Version(v)) => return Err(ConnectError::Version(v)),
        Err(ReplyError::Malformed(m)) => return Err(ConnectError::Protocol(m)),
    };
    if accept.pipeline_depth == 0 || accept.pipeline_depth > MAX_PIPELINE_DEPTH {
        return Err(ConnectError::Protocol("pipeline depth out of range"));
    }
    if accept.latency_samples > MAX_LATENCY_SAMPLES {
        return Err(ConnectError::Protocol("latency out of range"));
    }
    Ok((
        ClientRings {
            region,
            wake,
            in_head: 0,
            out_tail: 0,
            depth: accept.pipeline_depth,
        },
        Control { socket, accept },
    ))
}

/// Polls `socket` for `events` until `deadline`, giving up early when
/// `cancel` is readable.
fn wait_for(
    socket: &SeqPacket,
    events: libc::c_short,
    deadline: Instant,
    cancel: Option<BorrowedFd<'_>>,
) -> Result<(), ConnectError> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(ConnectError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "the worker did not answer in time",
            )));
        }
        let mut pfd = [
            sys::pollfd(socket.as_fd(), events),
            // A negative descriptor is ignored by poll(2).
            match cancel {
                Some(c) => sys::pollfd(c, libc::POLLIN),
                None => libc::pollfd {
                    fd: -1,
                    events: 0,
                    revents: 0,
                },
            },
        ];
        // Round up so a sub-millisecond remainder does not spin.
        let left = (deadline - now) + Duration::from_micros(999);
        sys::poll(&mut pfd, Some(left))?;
        if pfd[1].revents != 0 {
            return Err(ConnectError::Cancelled);
        }
        if pfd[0].revents != 0 {
            return Ok(());
        }
    }
}

/// The plugin's handle on a connection's rings. Every method is real-time
/// safe. It is `Send` so the helper thread can build it and hand it to the
/// audio thread; only one thread may use it at a time (`&mut self`).
#[derive(Debug)]
pub struct ClientRings {
    region: Region,
    wake: OwnedFd,
    in_head: u64,
    out_tail: u64,
    depth: u32,
}

impl ClientRings {
    /// Sends one raw block. `Ok(false)` if the input ring is full (the
    /// worker is behind); the caller marks the next block it sends with
    /// [`crate::shm::SLOT_FLAG_DISCONT`]. [`RingCorrupt`] if the worker
    /// published an input tail ahead of what was sent: drop the connection.
    pub fn push_input(
        &mut self,
        seq: u64,
        flags: u32,
        block: &[f32; BLOCK_SAMPLES],
    ) -> Result<bool, RingCorrupt> {
        self.region
            .try_push(Ring::Input, &mut self.in_head, seq, flags, block)
    }

    /// Wakes the worker: one `send(2)` with `MSG_DONTWAIT` on the wake
    /// socket, which never blocks whatever the worker does. Call once per
    /// `run()` after pushing.
    pub fn notify(&self) {
        sys::wake_signal(self.wake.as_fd());
    }

    /// Takes the oldest processed block, if any.
    pub fn pop_output(
        &mut self,
        out: &mut [f32; BLOCK_SAMPLES],
    ) -> Result<Option<BlockMeta>, RingCorrupt> {
        self.region.pop(Ring::Output, &mut self.out_tail, out)
    }

    /// The worker's current algorithmic delay, clamped to
    /// [`MAX_LATENCY_SAMPLES`].
    pub fn latency_samples(&self) -> u32 {
        self.region.latency_samples().min(MAX_LATENCY_SAMPLES)
    }

    /// The worker's heartbeat counter.
    pub fn heartbeat(&self) -> u64 {
        self.region.heartbeat()
    }

    /// The pipeline depth the worker accepted with.
    pub fn pipeline_depth(&self) -> u32 {
        self.depth
    }

    /// Blocks per ring.
    pub fn capacity(&self) -> u32 {
        self.region.capacity()
    }

    /// Publishes the plugin's counters to the header.
    pub fn publish(&self, counters: &ClientCounters) {
        self.region.publish_client_counters(counters);
    }

    /// Publishes the transport delay processed blocks are played out with,
    /// in samples (the worker's `ProcessLatency` and status use it).
    pub fn publish_transport_delay(&self, samples: u32) {
        self.region.publish_transport_delay(samples);
    }
}
