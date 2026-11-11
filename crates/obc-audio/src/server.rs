//! The worker's side: accept a plugin, check its hello and region, then pop
//! raw blocks and push processed ones with the same sequence numbers.
//!
//! A typical worker loop:
//!
//! ```no_run
//! use std::time::Duration;
//! use obc_audio::server::{Handshake, Listener, Wait};
//!
//! let listener = Listener::bind("/run/user/1000/omarchy-broadcast/audio.sock".as_ref())?;
//! let conn = listener.accept()?;
//! let handshake = Handshake::read(conn, Duration::from_secs(2))?;
//! let mut ep = handshake.accept(1, 0)?;
//! let mut block = [0.0f32; obc_audio::BLOCK_SAMPLES];
//! loop {
//!     match ep.wait(Duration::from_millis(100))? {
//!         Wait::HangUp => break,
//!         Wait::Input | Wait::Timeout => {}
//!     }
//!     ep.beat();
//!     while let Some(meta) = ep.pop_input(&mut block)? {
//!         // process `block` in place ...
//!         ep.push_output(meta.seq, 0, &block);
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use crate::proto::{Hello, HelloError, MAX_MESSAGE_LEN, RefuseReason, Reply};
use crate::shm::{BlockMeta, ClientCounters, Region, Ring, RingCorrupt, ShmError};
use crate::sys::{self, SeqPacket, SeqPacketListener};
use crate::{BLOCK_SAMPLES, MAX_LATENCY_SAMPLES, MAX_PIPELINE_DEPTH, MAX_TRANSPORT_DELAY};

/// The worker's listening socket.
#[derive(Debug)]
pub struct Listener(SeqPacketListener);

impl Listener {
    /// Binds `path` (the file must not exist).
    pub fn bind(path: &Path) -> io::Result<Self> {
        SeqPacketListener::bind(path).map(Self)
    }

    /// Wraps a listening `SOCK_SEQPACKET` socket, e.g. from systemd socket
    /// activation.
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self(SeqPacketListener::from_fd(fd))
    }

    /// Accepts one connection, blocking.
    pub fn accept(&self) -> io::Result<SeqPacket> {
        self.0.accept()
    }

    /// Accepts one connection if one arrives within `timeout`.
    pub fn accept_timeout(&self, timeout: Duration) -> io::Result<Option<SeqPacket>> {
        self.0.accept_timeout(timeout)
    }
}

impl AsFd for Listener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Why a handshake did not produce an endpoint.
#[derive(Debug)]
pub enum HandshakeError {
    /// A system call failed (the client may not have been told).
    Io(io::Error),
    /// The client was sent a refusal with this reason.
    Refused(RefuseReason, String),
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "handshake: {e}"),
            Self::Refused(r, t) => write!(f, "refused client ({r:?}): {t}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A client whose hello has been read and checked, waiting for the
/// worker's decision.
#[derive(Debug)]
pub struct Handshake {
    socket: SeqPacket,
    hello: Hello,
    memfd: OwnedFd,
    wake: OwnedFd,
}

fn refuse_on(socket: &SeqPacket, reason: RefuseReason, text: &str) -> HandshakeError {
    // Best effort: the client may already be gone.
    let _ = socket.send(&Reply::encode_refuse(reason, text), &[]);
    HandshakeError::Refused(reason, text.to_owned())
}

impl Handshake {
    /// Reads the hello from a freshly accepted connection, waiting up to
    /// `timeout`. A hello that is malformed, of another version, of an
    /// unsupported format or without its two descriptors is refused here
    /// (the client is told why) and returned as [`HandshakeError::Refused`].
    pub fn read(socket: SeqPacket, timeout: Duration) -> Result<Self, HandshakeError> {
        socket.set_timeout(Some(timeout))?;
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let (n, mut fds) = match socket.recv(&mut buf) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Err(refuse_on(
                    &socket,
                    RefuseReason::Malformed,
                    "oversized hello",
                ));
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Err(HandshakeError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed before its hello",
            )));
        }
        let hello = match Hello::decode(&buf[..n]) {
            Ok(h) => h,
            Err(HelloError::Version(v)) => {
                let text = format!(
                    "client speaks protocol {v}, worker {}",
                    crate::PROTOCOL_VERSION
                );
                return Err(refuse_on(&socket, RefuseReason::Version, &text));
            }
            Err(HelloError::Malformed(m)) => {
                return Err(refuse_on(&socket, RefuseReason::Malformed, m));
            }
        };
        if let Err(m) = hello.check_format() {
            return Err(refuse_on(&socket, RefuseReason::Format, m));
        }
        if fds.len() != 2 {
            return Err(refuse_on(
                &socket,
                RefuseReason::Malformed,
                "expected a memfd and a wake socket",
            ));
        }
        let wake = fds.pop().expect("two descriptors");
        let memfd = fds.pop().expect("two descriptors");
        if sys::socket_type(wake.as_fd()).ok() != Some(libc::SOCK_SEQPACKET) {
            return Err(refuse_on(
                &socket,
                RefuseReason::Malformed,
                "the second descriptor is not a SOCK_SEQPACKET wake socket",
            ));
        }
        Ok(Self {
            socket,
            hello,
            memfd,
            wake,
        })
    }

    /// The client's hello.
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Maps the client's region and sends the accept. `pipeline_depth`
    /// (1..=[`MAX_PIPELINE_DEPTH`]) is how many blocks of processing time
    /// the worker needs; `latency_samples` (<= [`MAX_LATENCY_SAMPLES`]) the
    /// processing's algorithmic delay, also published in the header. A
    /// region that fails its checks is refused with
    /// [`RefuseReason::SharedMemory`].
    pub fn accept(
        self,
        pipeline_depth: u32,
        latency_samples: u32,
    ) -> Result<Endpoint, HandshakeError> {
        let depth = pipeline_depth.clamp(1, MAX_PIPELINE_DEPTH);
        let latency = latency_samples.min(MAX_LATENCY_SAMPLES);
        let region = match Region::open(self.memfd, self.hello.ring_capacity, self.hello.shm_size) {
            Ok(r) => r,
            Err(ShmError::Invalid(m)) => {
                return Err(refuse_on(&self.socket, RefuseReason::SharedMemory, m));
            }
            Err(ShmError::Io(e)) => {
                return Err(refuse_on(
                    &self.socket,
                    RefuseReason::SharedMemory,
                    &e.to_string(),
                ));
            }
        };
        region.set_latency_samples(latency);
        self.socket
            .send(&Reply::encode_accept(depth, latency), &[])?;
        self.socket.set_timeout(None)?;
        Ok(Endpoint {
            socket: self.socket,
            region,
            wake: self.wake,
            in_tail: 0,
            out_head: 0,
        })
    }

    /// Refuses the client with `reason` and a short text.
    pub fn refuse(self, reason: RefuseReason, text: &str) -> io::Result<()> {
        self.socket.send(&Reply::encode_refuse(reason, text), &[])
    }
}

/// What [`Endpoint::wait`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The plugin signalled input (the pending wake-ups were discarded).
    Input,
    /// The plugin closed the connection or its wake socket (or sent
    /// something unexpected on the control socket, which is treated the
    /// same way).
    HangUp,
    /// Neither, within the timeout.
    Timeout,
}

/// The worker's handle on a connected plugin.
///
/// Obligations of the worker, which the plugin relies on:
/// * every popped input block is answered with exactly one output block of
///   the same `seq` (processed, or flagged [`crate::shm::SLOT_FLAG_RAW`]),
///   in order, within `pipeline_depth` blocks of wall time;
/// * [`Endpoint::beat`] is called at least every 100 ms while connected
///   (the plugin drops a connection whose heartbeat stalls while audio
///   flows);
/// * [`Endpoint::set_latency_samples`] always reflects the algorithmic
///   delay of the blocks being pushed;
/// * after a [`crate::shm::SLOT_FLAG_NEW_STREAM`] input block (and after
///   any module reload), output blocks that still contain the modules'
///   warm-up (the first `latency_samples` output samples, typically zeros)
///   are flagged [`crate::shm::SLOT_FLAG_RAW`], so the plugin crossfades
///   from raw into real processed audio rather than into the warm-up.
#[derive(Debug)]
pub struct Endpoint {
    socket: SeqPacket,
    region: Region,
    wake: OwnedFd,
    in_tail: u64,
    out_head: u64,
}

impl Endpoint {
    /// Takes the oldest raw block into `out`. Sequence numbers increase by
    /// one per block; a jump means the plugin dropped input (the block is
    /// flagged [`crate::shm::SLOT_FLAG_DISCONT`]). A block whose
    /// `n_samples` is not 480 should be answered with
    /// [`crate::shm::SLOT_FLAG_RAW`].
    pub fn pop_input(
        &mut self,
        out: &mut [f32; BLOCK_SAMPLES],
    ) -> Result<Option<BlockMeta>, RingCorrupt> {
        self.region.pop(Ring::Input, &mut self.in_tail, out)
    }

    /// Sends the block for input `seq`. `false` if the output ring is full
    /// (the plugin is not reading: it stopped running); the block is lost
    /// and the plugin plays raw for it. Also `false` if the plugin published
    /// an implausible output tail; the plugin then drops the connection
    /// itself.
    pub fn push_output(&mut self, seq: u64, flags: u32, block: &[f32; BLOCK_SAMPLES]) -> bool {
        self.region
            .push(Ring::Output, &mut self.out_head, seq, flags, block)
    }

    /// Advances the heartbeat.
    pub fn beat(&self) {
        self.region.beat();
    }

    /// Publishes the algorithmic delay, clamped to [`MAX_LATENCY_SAMPLES`].
    pub fn set_latency_samples(&self, samples: u32) {
        self.region
            .set_latency_samples(samples.min(MAX_LATENCY_SAMPLES));
    }

    /// Publishes `WORKER_FLAG_*` bits.
    pub fn set_worker_flags(&self, flags: u32) {
        self.region.set_worker_flags(flags);
    }

    /// The plugin's published counters (for statistics; untrusted).
    pub fn client_counters(&self) -> ClientCounters {
        self.region.client_counters()
    }

    /// The transport delay the plugin plays processed blocks out with, in
    /// samples, as it published it; `None` before it published one (or
    /// from a plugin that does not), or when the value is beyond
    /// [`MAX_TRANSPORT_DELAY`] (untrusted). Fall back to
    /// [`crate::transport_delay`] then.
    pub fn transport_delay(&self) -> Option<u32> {
        Some(self.region.transport_delay()).filter(|d| (1..=MAX_TRANSPORT_DELAY).contains(d))
    }

    /// The client's hello capacity.
    pub fn capacity(&self) -> u32 {
        self.region.capacity()
    }

    /// The "input available" descriptor, for a worker's own poll loop: it
    /// is readable (`POLLIN`) when the plugin signalled input, and reports
    /// `POLLHUP` when the plugin went away. Reset it with
    /// [`Endpoint::drain_input_signal`]. (It is the worker's end of the wake
    /// socket; the worker needs no other knowledge of how wake-ups travel.)
    pub fn input_fd(&self) -> BorrowedFd<'_> {
        self.wake.as_fd()
    }

    /// The control socket, for a worker's own poll loop: readable means
    /// the plugin hung up.
    pub fn socket_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }

    /// Discards the pending input signals. Never blocks.
    pub fn drain_input_signal(&self) {
        sys::wake_drain(self.wake.as_fd());
    }

    /// The region, for diagnostics and for tests that play a misbehaving
    /// worker. A real worker uses the methods above.
    #[doc(hidden)]
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// Waits up to `timeout` for input or a hang-up.
    pub fn wait(&self, timeout: Duration) -> io::Result<Wait> {
        let mut pfd = [
            sys::pollfd(self.socket.as_fd(), libc::POLLIN),
            sys::pollfd(self.wake.as_fd(), libc::POLLIN),
        ];
        sys::poll(&mut pfd, Some(timeout))?;
        if pfd[0].revents != 0
            || pfd[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
        {
            return Ok(Wait::HangUp);
        }
        if pfd[1].revents != 0 {
            self.drain_input_signal();
            return Ok(Wait::Input);
        }
        Ok(Wait::Timeout)
    }
}
