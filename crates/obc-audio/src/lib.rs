//! Shared-memory audio transport between the Omarchy Broadcast Mic plugin
//! (a LADSPA plugin inside the mic host's filter-chain, the *client*) and
//! the audio worker `omarchy-broadcast-audiod` (the *server*).
//!
//! **The protocol is internal and unstable**: both ends ship together and
//! only this crate's current version ([`PROTOCOL_VERSION`] = 2) is spoken
//! (version 1, never released, sent an eventfd instead of the wake socket
//! below). What is fixed forever is the first
//! eight bytes of every message (magic, version), so that any two versions
//! can at least tell each other apart and refuse cleanly.
//!
//! # Process model
//!
//! The plugin connects to a `SOCK_SEQPACKET` Unix socket at
//! `$XDG_RUNTIME_DIR/omarchy-broadcast/audio.sock` ([`default_socket_path`];
//! the environment variable [`SOCKET_ENV`] overrides it). The worker may be
//! socket-activated by that connect. After the handshake the socket carries
//! nothing more: audio flows through a shared memory region the plugin
//! created, and the socket only signals the end of the connection (either
//! side closes it; the other sees end of file).
//!
//! # Handshake
//!
//! All integers little-endian.
//!
//! **Hello** (plugin to worker), 32 bytes, with two descriptors attached
//! as `SCM_RIGHTS`, in order: the memfd of the region, and the worker's end
//! of the *wake socket*, a connected `SOCK_SEQPACKET` pair the plugin
//! created ([`sys::wake_pair`]). The plugin sends one 1-byte message on its
//! end, once per audio callback that sent blocks, to say "input available";
//! the worker polls its end and discards what is pending. The worker
//! refuses (reason 2) a second descriptor that is not a `SOCK_SEQPACKET`
//! socket.
//!
//! Why a socket pair and not an eventfd: the two ends of a pair are
//! separate open files, and the plugin's send passes `MSG_DONTWAIT |
//! MSG_NOSIGNAL` on every call. Whatever the worker does with its end
//! (clears `O_NONBLOCK`, never reads so the queue fills, closes it), the
//! plugin's audio thread can only get `EAGAIN` or `EPIPE`, which it
//! ignores; it never blocks and never gets `SIGPIPE`. A shared eventfd is
//! one open file: a worker clearing its `O_NONBLOCK` and saturating the
//! counter would make the plugin's write block.
//!
//! | offset | type | field |
//! |---|---|---|
//! | 0 | u32 | magic `"OBCh"` ([`proto::HELLO_MAGIC`]) |
//! | 4 | u32 | protocol version ([`PROTOCOL_VERSION`] = 2) |
//! | 8 | u32 | sample rate, 48000 |
//! | 12 | u32 | channels, 1 |
//! | 16 | u32 | block samples, 480 |
//! | 20 | u32 | ring capacity in blocks, a power of two in 2..=64 (the plugin uses 8) |
//! | 24 | u64 | region size in bytes, [`shm::region_size`] of the capacity |
//!
//! **Reply** (worker to plugin): a 24-byte header, then for a refusal its
//! UTF-8 text.
//!
//! | offset | type | field |
//! |---|---|---|
//! | 0 | u32 | magic `"OBCr"` ([`proto::REPLY_MAGIC`]) |
//! | 4 | u32 | the worker's protocol version |
//! | 8 | u32 | kind: 1 accept, 2 refuse |
//! | 12 | u32 | accept: pipeline depth in blocks, 1..=4; refuse: reason code ([`proto::RefuseReason`]) |
//! | 16 | u32 | accept: algorithmic latency in samples, 0..=4800; refuse: 0 |
//! | 20 | u32 | refuse: text length (at most 232); accept: 0 |
//!
//! A worker refuses a hello of another version with reason 1 and its own
//! version; a plugin treats a reply of another version as a refusal. After
//! a refusal the plugin stays in passthrough and does not try again for a
//! long back-off (60 s by default), and only while audio flows.
//!
//! # Shared region
//!
//! Created by the plugin with `memfd_create`, sized
//! `256 + 2 * capacity * 1984` bytes and sealed with `F_SEAL_SHRINK |
//! F_SEAL_GROW | F_SEAL_SEAL` before it is sent, so neither process can
//! ever fault on it. The worker refuses (reason 4) a memfd whose size or
//! seals differ, or whose header does not match the geometry it computes
//! itself from the hello ([`shm::Region::open`]). Both sides map it with
//! `MAP_POPULATE`, on a non-real-time thread, so the plugin's audio thread
//! never takes a page fault on it.
//!
//! Header (256 bytes; static fields written once by the plugin):
//!
//! | offset | type | field | written by |
//! |---|---|---|---|
//! | 0 | u32 | magic `"OBCs"` | plugin, once |
//! | 4 | u32 | version (2) | plugin, once |
//! | 8 | u32 | header size (256) | plugin, once |
//! | 12 | u32 | block samples (480) | plugin, once |
//! | 16 | u32 | capacity | plugin, once |
//! | 20 | u32 | slot size (1984) | plugin, once |
//! | 24 | u32 | input ring offset (256) | plugin, once |
//! | 28 | u32 | output ring offset (256 + capacity * 1984) | plugin, once |
//! | 32 | u64 | total size | plugin, once |
//! | 64 | atomic u64 | input head | plugin |
//! | 72 | atomic u64 | output tail | plugin |
//! | 80 | atomic u64 | play sequence (block being played out) | plugin |
//! | 88 | atomic u64 | blocks sent | plugin |
//! | 96 | atomic u64 | blocks dropped (input ring full) | plugin |
//! | 104 | atomic u64 | blocks missed (no output in time) | plugin |
//! | 112 | atomic u32 | transport delay in samples (0: not published) | plugin |
//! | 128 | atomic u64 | input tail | worker |
//! | 136 | atomic u64 | output head | worker |
//! | 144 | atomic u64 | heartbeat (advanced at least every 100 ms) | worker |
//! | 152 | atomic u32 | latency in samples (algorithmic delay, <= 4800) | worker |
//! | 156 | atomic u32 | worker flags (bit 0: processing) | worker |
//!
//! Two rings follow: input (plugin to worker) at offset 256 and output
//! (worker to plugin) after it, `capacity` slots each. A slot is 1984 bytes:
//! `seq: u64`, `flags: u32`, `n_samples: u32` (480), then 480 mono `f32`
//! samples. Heads and tails are free-running block counters; slot index is
//! `counter % capacity`; the producer publishes its head with release
//! ordering after writing the slot, the consumer its tail after reading it.
//! A peer counter that is implausible (a head more than `capacity` ahead of
//! the consumer's tail, or a tail ahead of the producer's head) is
//! reported as [`shm::RingCorrupt`]; the plugin then plays raw, drops the
//! connection and reconnects later. No locks, and no syscall on the
//! plugin's audio thread other than the wake send.
//!
//! `seq` is the block's absolute index in the plugin's input since the
//! plugin instance was created (it does not restart on reconnect). Input
//! flags: [`shm::SLOT_FLAG_NEW_STREAM`] on the first block of a connection
//! or after a reactivation, [`shm::SLOT_FLAG_DISCONT`] after a gap. The
//! worker answers every input block with one output block of the **same
//! `seq`**, in order; [`shm::SLOT_FLAG_RAW`] on an output block means "no
//! processed audio for this one, play raw".
//!
//! # Playout and latency (plugin side)
//!
//! The plugin plays processed block `k` a constant *transport delay* `Lp`
//! after its input, `Lp = `[`transport_delay`]`(Q, depth)` where `Q` is the
//! graph quantum and `depth` the accepted pipeline depth:
//! `Lp = 480 - gcd(480, Q) + Q * ceil(480 * depth / Q)` (480 samples for
//! Q = 480 at depth 1; 960 at depth 2). The worker therefore has
//! `depth` blocks of wall time (at least `480 * depth` samples, rounded up
//! to whole quanta) from the audio callback that sent block `k` to the one
//! that plays it. The total delay the plugin adds when processing is
//! `Lp + latency_samples`; its raw fallback is delayed by the same amount,
//! so a processed/raw switch is a crossfade (5 ms, linear) between aligned
//! signals. A block that is not there at its playout time is played raw.
//!
//! The plugin publishes the transport delay it plays out with (`Lp`, the
//! largest over the quanta it has seen, see the plugin) at offset 112 of
//! the header, on every audio callback of a connection; the worker uses it
//! for the node's `ProcessLatency` and its status, and falls back to
//! `transport_delay(480, depth)` while it reads 0. The field was reserved
//! (zero) space before, so it needs no new protocol version: a plugin that
//! does not write it reads as 0, and a worker that does not read it
//! ignores it.
//!
//! # Modules
//!
//! * [`sys`]: the syscall wrappers (sockets with descriptors, the wake socket, eventfd, poll, memfd).
//! * [`proto`]: hello and reply encoding.
//! * [`shm`]: the region, its header and the rings.
//! * [`client`]: the plugin's connect (non-blocking, cancellable) and real-time ring handle.
//! * [`server`]: the worker's listener, handshake and endpoint.
//! * [`dsp`]: delay line, reblocker, crossfade, block store, the delay formula.

pub mod client;
pub mod dsp;
pub mod proto;
pub mod server;
pub mod shm;
pub mod sys;

use std::path::PathBuf;

pub use dsp::transport_delay;

/// The protocol version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 2;
/// The only sample rate of this version.
pub const SAMPLE_RATE: u32 = 48_000;
/// Mono.
pub const CHANNELS: u32 = 1;
/// Samples per block: 10 ms at 48 kHz.
pub const BLOCK_SAMPLES: usize = 480;
/// The smallest ring capacity, in blocks.
pub const MIN_RING_CAPACITY: u32 = 2;
/// The largest ring capacity, in blocks.
pub const MAX_RING_CAPACITY: u32 = 64;
/// The ring capacity the plugin asks for: 80 ms of audio each way.
pub const DEFAULT_RING_CAPACITY: u32 = 8;
/// The largest pipeline depth a worker may ask for, in blocks.
pub const MAX_PIPELINE_DEPTH: u32 = 4;
/// The largest algorithmic delay a worker may report: 100 ms.
pub const MAX_LATENCY_SAMPLES: u32 = 4_800;
/// The largest quantum the plugin processes through the worker; larger
/// audio callbacks are handled in pieces of this size and always play raw
/// (the processed blocks cannot be back within the same callback).
pub const MAX_QUANTUM: u32 = 2_048;
/// The largest transport delay over every quantum (1..=[`MAX_QUANTUM`])
/// and pipeline depth (1..=[`MAX_PIPELINE_DEPTH`]): 4317 samples.
pub const MAX_TRANSPORT_DELAY: u32 = {
    let mut worst = 0;
    let mut q = 1;
    while q <= MAX_QUANTUM {
        let mut d = 1;
        while d <= MAX_PIPELINE_DEPTH {
            let t = transport_delay(q, d);
            if t > worst {
                worst = t;
            }
            d += 1;
        }
        q += 1;
    }
    worst
};
const _: () = assert!(MAX_TRANSPORT_DELAY == 4317);

/// Environment variable that overrides the socket path (for tests and
/// private instances).
pub const SOCKET_ENV: &str = "OMARCHY_BROADCAST_AUDIO_SOCKET";

/// The worker's socket: `$OMARCHY_BROADCAST_AUDIO_SOCKET` if set, else
/// `$XDG_RUNTIME_DIR/omarchy-broadcast/audio.sock`; `None` without either.
pub fn default_socket_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(SOCKET_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|p| !p.is_empty())?;
    Some(
        PathBuf::from(dir)
            .join("omarchy-broadcast")
            .join("audio.sock"),
    )
}
