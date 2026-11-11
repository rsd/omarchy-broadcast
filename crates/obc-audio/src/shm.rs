//! The shared region: a versioned header and two single-producer
//! single-consumer rings of 480-sample blocks.
//!
//! The plugin creates and owns the memfd; the worker maps the descriptor it
//! receives in the hello and checks it before trusting any of it. Each side
//! keeps its own ring counters locally and only *publishes* them to the
//! header, so a peer that scribbles over the header cannot make this side
//! index outside its mapping: every slot index is `counter % capacity` of a
//! capacity validated once, and a peer counter that claims more than
//! `capacity` blocks are pending is reported as [`RingCorrupt`].
//!
//! Sample data is copied in and out with plain (non-atomic) copies ordered
//! by the release/acquire publication of the counters. A misbehaving peer
//! can at worst make this side read torn samples, never touch memory
//! outside the mapping.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{BLOCK_SAMPLES, MAX_RING_CAPACITY, MIN_RING_CAPACITY, PROTOCOL_VERSION, sys};

/// `"OBCs"` read as a little-endian `u32`: the first word of the region.
pub const SHM_MAGIC: u32 = u32::from_le_bytes(*b"OBCs");
/// The size of the header, and the offset of the input ring.
pub const HEADER_SIZE: usize = 256;
/// Bytes before the samples in a slot: `seq: u64, flags: u32, n_samples: u32`.
pub const SLOT_HEADER_SIZE: usize = 16;
/// The size of one ring slot: the slot header plus 480 `f32`, rounded up
/// to a 64-byte cache line.
pub const SLOT_SIZE: usize = (SLOT_HEADER_SIZE + BLOCK_SAMPLES * 4).div_ceil(64) * 64;

/// Input slot flag: the first block of a connection (or of a plugin
/// reactivation); the worker resets its modules' temporal state.
pub const SLOT_FLAG_NEW_STREAM: u32 = 1 << 0;
/// Input slot flag: one or more blocks before this one were not sent (the
/// ring was full, or the plugin was bypassed); the sequence numbers jump.
pub const SLOT_FLAG_DISCONT: u32 = 1 << 1;
/// Output slot flag: the worker has no processed audio for this block (a
/// stage failed open, still loading); the plugin plays its own raw block.
/// The samples are ignored.
pub const SLOT_FLAG_RAW: u32 = 1 << 2;

/// Worker flag in the header: the worker is processing (models loaded). Informational.
pub const WORKER_FLAG_PROCESSING: u32 = 1 << 0;

/// Whether `capacity` is an allowed ring capacity.
pub const fn capacity_ok(capacity: u32) -> bool {
    capacity >= MIN_RING_CAPACITY && capacity <= MAX_RING_CAPACITY && capacity.is_power_of_two()
}

/// The size of the region for rings of `capacity` blocks.
pub const fn region_size(capacity: u32) -> usize {
    HEADER_SIZE + 2 * capacity as usize * SLOT_SIZE
}

/// The header at offset 0 of the region. Static fields are written by the
/// plugin before the hello and never change; atomics are grouped in one
/// cache line per writer.
#[repr(C, align(64))]
struct Header {
    // Static, written once by the plugin (offset 0).
    magic: u32,
    version: u32,
    header_size: u32,
    block_samples: u32,
    capacity: u32,
    slot_size: u32,
    input_offset: u32,
    output_offset: u32,
    total_size: u64,
    _reserved: [u64; 3],
    // Written by the plugin (offset 64).
    in_head: AtomicU64,
    out_tail: AtomicU64,
    play_seq: AtomicU64,
    blocks_sent: AtomicU64,
    blocks_dropped: AtomicU64,
    blocks_missed: AtomicU64,
    transport_delay: AtomicU32,
    _client_pad32: u32,
    _client_pad: [u64; 1],
    // Written by the worker (offset 128).
    in_tail: AtomicU64,
    out_head: AtomicU64,
    heartbeat: AtomicU64,
    latency_samples: AtomicU32,
    worker_flags: AtomicU32,
    _worker_pad: [u64; 4],
    _tail: [u64; 8],
}

const _: () = assert!(size_of::<Header>() == HEADER_SIZE);
const _: () = assert!(std::mem::offset_of!(Header, in_head) == 64);
const _: () = assert!(std::mem::offset_of!(Header, transport_delay) == 112);
const _: () = assert!(std::mem::offset_of!(Header, in_tail) == 128);

/// Which ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ring {
    /// Plugin to worker: raw blocks.
    Input,
    /// Worker to plugin: processed blocks, same sequence numbers.
    Output,
}

/// The metadata of a popped block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMeta {
    /// The block's sequence number: its absolute index in the plugin's
    /// input since the plugin instance was created.
    pub seq: u64,
    /// `SLOT_FLAG_*` bits.
    pub flags: u32,
    /// Always 480 from a well-behaved peer; anything else should be dropped.
    pub n_samples: u32,
}

/// The peer published counters that cannot be right (more blocks pending
/// than the ring holds). The connection should be dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingCorrupt;

impl std::fmt::Display for RingCorrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the peer published implausible ring counters")
    }
}

impl std::error::Error for RingCorrupt {}

/// Counters the plugin publishes for the worker's statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientCounters {
    /// The block the plugin is playing out now.
    pub play_seq: u64,
    /// Input blocks sent on this connection.
    pub blocks_sent: u64,
    /// Input blocks not sent because the input ring was full.
    pub blocks_dropped: u64,
    /// Blocks that had no processed output at their playout time.
    pub blocks_missed: u64,
}

/// Why a received region was rejected.
#[derive(Debug)]
pub enum ShmError {
    /// A system call failed.
    Io(io::Error),
    /// The region does not have the declared size, is not sealed, or its
    /// header does not describe the declared geometry.
    Invalid(&'static str),
}

impl From<io::Error> for ShmError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A mapped region.
#[derive(Debug)]
pub struct Region {
    base: NonNull<u8>,
    len: usize,
    capacity: u32,
    fd: OwnedFd,
}

// SAFETY: the mapping is plain shared memory; all shared mutation goes
// through atomics or through the SPSC protocol whose roles the client and
// server endpoints enforce with `&mut self`.
unsafe impl Send for Region {}
// SAFETY: as above; `&Region` methods only touch atomics.
unsafe impl Sync for Region {}

impl Region {
    /// Creates a sealed memfd for rings of `capacity` blocks, maps it and
    /// writes the header (the plugin side).
    pub fn create(capacity: u32) -> io::Result<Self> {
        if !capacity_ok(capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad ring capacity",
            ));
        }
        let len = region_size(capacity);
        let fd = sys::sealed_memfd(c"omarchy-broadcast-mic", len)?;
        let region = Self::map(fd, len, capacity)?;
        let h = region.header();
        // SAFETY: the region is ours alone until the hello is sent; the
        // static fields are written before any peer sees them. The memfd
        // is zero-filled, so every counter starts at 0.
        unsafe {
            (&raw mut (*h).magic).write(SHM_MAGIC);
            (&raw mut (*h).version).write(PROTOCOL_VERSION);
            (&raw mut (*h).header_size).write(HEADER_SIZE as u32);
            (&raw mut (*h).block_samples).write(BLOCK_SAMPLES as u32);
            (&raw mut (*h).capacity).write(capacity);
            (&raw mut (*h).slot_size).write(SLOT_SIZE as u32);
            (&raw mut (*h).input_offset).write(HEADER_SIZE as u32);
            (&raw mut (*h).output_offset)
                .write((HEADER_SIZE + capacity as usize * SLOT_SIZE) as u32);
            (&raw mut (*h).total_size).write(len as u64);
        }
        Ok(region)
    }

    /// Maps a region received from the plugin (the worker side), after
    /// checking that the memfd has exactly the size the hello declared,
    /// that it is sealed against shrinking (so it can never fault under
    /// the worker), and that its header describes the geometry the worker
    /// computes itself from `capacity`.
    pub fn open(fd: OwnedFd, capacity: u32, declared_size: u64) -> Result<Self, ShmError> {
        if !capacity_ok(capacity) {
            return Err(ShmError::Invalid("bad ring capacity"));
        }
        let len = region_size(capacity);
        if declared_size != len as u64 {
            return Err(ShmError::Invalid(
                "declared size does not match the capacity",
            ));
        }
        let seals =
            sys::seals(fd.as_fd()).map_err(|_| ShmError::Invalid("not a sealable memfd"))?;
        if seals & libc::F_SEAL_SHRINK == 0 {
            return Err(ShmError::Invalid("memfd not sealed against shrinking"));
        }
        if sys::file_size(fd.as_fd())? != len as u64 {
            return Err(ShmError::Invalid(
                "memfd size differs from the declared size",
            ));
        }
        let region = Self::map(fd, len, capacity)?;
        let h = region.header();
        // SAFETY: in bounds of the mapping; volatile reads because the
        // peer could still be writing (a malformed header is rejected,
        // never acted upon).
        let ok = unsafe {
            ptr::read_volatile(&raw const (*h).magic) == SHM_MAGIC
                && ptr::read_volatile(&raw const (*h).version) == PROTOCOL_VERSION
                && ptr::read_volatile(&raw const (*h).header_size) == HEADER_SIZE as u32
                && ptr::read_volatile(&raw const (*h).block_samples) == BLOCK_SAMPLES as u32
                && ptr::read_volatile(&raw const (*h).capacity) == capacity
                && ptr::read_volatile(&raw const (*h).slot_size) == SLOT_SIZE as u32
                && ptr::read_volatile(&raw const (*h).input_offset) == HEADER_SIZE as u32
                && ptr::read_volatile(&raw const (*h).output_offset)
                    == (HEADER_SIZE + capacity as usize * SLOT_SIZE) as u32
                && ptr::read_volatile(&raw const (*h).total_size) == len as u64
        };
        if !ok {
            return Err(ShmError::Invalid("header does not match the hello"));
        }
        Ok(region)
    }

    fn map(fd: OwnedFd, len: usize, capacity: u32) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        // SAFETY: a fresh shared mapping of `len` bytes of a file that is
        // (at least, checked by the callers) `len` bytes long and sealed
        // against shrinking. MAP_POPULATE prefaults every page here, on the
        // (non-real-time) thread that maps it, so the plugin's audio thread
        // never takes a page fault on the rings.
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            base: NonNull::new(p.cast()).expect("mmap returned null"),
            len,
            capacity,
            fd,
        })
    }

    /// The memfd, to send with the hello.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Blocks per ring.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    fn header(&self) -> *mut Header {
        self.base.as_ptr().cast()
    }

    fn atomic64(&self, f: impl FnOnce(*mut Header) -> *mut AtomicU64) -> &AtomicU64 {
        // SAFETY: the field lies inside the mapping (the header is at
        // offset 0 and HEADER_SIZE <= len), is 8-aligned (the mapping is
        // page-aligned), and atomics may be shared freely.
        unsafe { &*f(self.header()) }
    }

    fn atomic32(&self, f: impl FnOnce(*mut Header) -> *mut AtomicU32) -> &AtomicU32 {
        // SAFETY: as for `atomic64`.
        unsafe { &*f(self.header()) }
    }

    fn head(&self, ring: Ring) -> &AtomicU64 {
        match ring {
            // SAFETY (closures): field projection of a valid header pointer.
            Ring::Input => self.atomic64(|h| unsafe { &raw mut (*h).in_head }),
            Ring::Output => self.atomic64(|h| unsafe { &raw mut (*h).out_head }),
        }
    }

    fn tail(&self, ring: Ring) -> &AtomicU64 {
        match ring {
            // SAFETY (closures): field projection of a valid header pointer.
            Ring::Input => self.atomic64(|h| unsafe { &raw mut (*h).in_tail }),
            Ring::Output => self.atomic64(|h| unsafe { &raw mut (*h).out_tail }),
        }
    }

    fn slot(&self, ring: Ring, counter: u64) -> *mut u8 {
        let ring_base = match ring {
            Ring::Input => HEADER_SIZE,
            Ring::Output => HEADER_SIZE + self.capacity as usize * SLOT_SIZE,
        };
        let index = (counter % u64::from(self.capacity)) as usize;
        let off = ring_base + index * SLOT_SIZE;
        debug_assert!(off + SLOT_SIZE <= self.len);
        // SAFETY: `index < capacity`, so the slot lies inside the mapping.
        unsafe { self.base.as_ptr().add(off) }
    }

    /// Producer side: writes one block at `*head` and publishes it. Returns
    /// `false` (nothing written) if the ring is full, and also if the
    /// consumer's tail is implausible (use [`Region::try_push`] to tell the
    /// two apart). Real-time safe: no allocation, no syscall.
    ///
    /// The caller must be the only producer of `ring` and must keep `head`
    /// (its own counter, never re-read from the region) across calls.
    pub fn push(
        &self,
        ring: Ring,
        head: &mut u64,
        seq: u64,
        flags: u32,
        samples: &[f32; BLOCK_SAMPLES],
    ) -> bool {
        self.try_push(ring, head, seq, flags, samples)
            .unwrap_or(false)
    }

    /// Like [`Region::push`], but a consumer tail that is ahead of `*head`
    /// (or more than `capacity` behind it) is [`RingCorrupt`]: the
    /// connection should be dropped. `Ok(false)` is a full ring.
    pub fn try_push(
        &self,
        ring: Ring,
        head: &mut u64,
        seq: u64,
        flags: u32,
        samples: &[f32; BLOCK_SAMPLES],
    ) -> Result<bool, RingCorrupt> {
        self.push_raw(ring, head, seq, flags, BLOCK_SAMPLES as u32, samples)
    }

    /// [`Region::try_push`] with an arbitrary `n_samples` in the slot
    /// header. Only for tests that play a misbehaving peer; a real producer
    /// always writes 480.
    #[doc(hidden)]
    pub fn push_raw(
        &self,
        ring: Ring,
        head: &mut u64,
        seq: u64,
        flags: u32,
        n_samples: u32,
        samples: &[f32; BLOCK_SAMPLES],
    ) -> Result<bool, RingCorrupt> {
        let tail = self.tail(ring).load(Ordering::Acquire);
        let pending = head.wrapping_sub(tail);
        if pending > u64::from(self.capacity) {
            // The tail is ahead of our head (the subtraction wrapped) or
            // claims more was consumed than the ring holds.
            return Err(RingCorrupt);
        }
        if pending == u64::from(self.capacity) {
            return Ok(false);
        }
        let slot = self.slot(ring, *head);
        // SAFETY: the slot is inside the mapping and, by the SPSC protocol,
        // not read by the consumer until the head below is published. The
        // slot header is 8-aligned (slots are 64-byte multiples).
        unsafe {
            slot.cast::<u64>().write_volatile(seq);
            slot.add(8).cast::<u32>().write_volatile(flags);
            slot.add(12).cast::<u32>().write_volatile(n_samples);
            ptr::copy_nonoverlapping(
                samples.as_ptr(),
                slot.add(SLOT_HEADER_SIZE).cast::<f32>(),
                BLOCK_SAMPLES,
            );
        }
        *head = head.wrapping_add(1);
        self.head(ring).store(*head, Ordering::Release);
        Ok(true)
    }

    /// Publishes `value` as the head (`head = true`) or tail of `ring`,
    /// bypassing the ring protocol. Only for tests that play a misbehaving
    /// peer publishing implausible counters.
    #[doc(hidden)]
    pub fn publish_counter(&self, ring: Ring, head: bool, value: u64) {
        let c = if head {
            self.head(ring)
        } else {
            self.tail(ring)
        };
        c.store(value, Ordering::Release);
    }

    /// Consumer side: copies the oldest block into `out` and frees its
    /// slot. `Ok(None)` when the ring is empty. Real-time safe.
    ///
    /// The caller must be the only consumer of `ring` and must keep `tail`
    /// across calls.
    pub fn pop(
        &self,
        ring: Ring,
        tail: &mut u64,
        out: &mut [f32; BLOCK_SAMPLES],
    ) -> Result<Option<BlockMeta>, RingCorrupt> {
        let head = self.head(ring).load(Ordering::Acquire);
        let pending = head.wrapping_sub(*tail);
        if pending == 0 {
            return Ok(None);
        }
        if pending > u64::from(self.capacity) {
            return Err(RingCorrupt);
        }
        let slot = self.slot(ring, *tail);
        // SAFETY: the slot is inside the mapping; the acquire load of the
        // head orders these reads after the producer's writes.
        let meta = unsafe {
            let meta = BlockMeta {
                seq: slot.cast::<u64>().read_volatile(),
                flags: slot.add(8).cast::<u32>().read_volatile(),
                n_samples: slot.add(12).cast::<u32>().read_volatile(),
            };
            ptr::copy_nonoverlapping(
                slot.add(SLOT_HEADER_SIZE).cast::<f32>(),
                out.as_mut_ptr(),
                BLOCK_SAMPLES,
            );
            meta
        };
        *tail = tail.wrapping_add(1);
        self.tail(ring).store(*tail, Ordering::Release);
        Ok(Some(meta))
    }

    /// The number of blocks pending in `ring`, as seen from either side.
    pub fn pending(&self, ring: Ring) -> u64 {
        self.head(ring)
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail(ring).load(Ordering::Acquire))
    }

    /// The worker's heartbeat counter.
    pub fn heartbeat(&self) -> u64 {
        // SAFETY (closure): field projection of a valid header pointer.
        self.atomic64(|h| unsafe { &raw mut (*h).heartbeat })
            .load(Ordering::Relaxed)
    }

    /// Worker side: advances the heartbeat.
    pub fn beat(&self) {
        // SAFETY (closure): field projection of a valid header pointer.
        self.atomic64(|h| unsafe { &raw mut (*h).heartbeat })
            .fetch_add(1, Ordering::Relaxed);
    }

    /// The processing's algorithmic delay in samples, as the worker last
    /// published it (unvalidated: the plugin clamps it).
    pub fn latency_samples(&self) -> u32 {
        // SAFETY (closure): field projection of a valid header pointer.
        self.atomic32(|h| unsafe { &raw mut (*h).latency_samples })
            .load(Ordering::Acquire)
    }

    /// Worker side: publishes the algorithmic delay in samples.
    pub fn set_latency_samples(&self, samples: u32) {
        // SAFETY (closure): field projection of a valid header pointer.
        self.atomic32(|h| unsafe { &raw mut (*h).latency_samples })
            .store(samples, Ordering::Release);
    }

    /// The worker's `WORKER_FLAG_*` bits.
    pub fn worker_flags(&self) -> u32 {
        // SAFETY (closure): field projection of a valid header pointer.
        self.atomic32(|h| unsafe { &raw mut (*h).worker_flags })
            .load(Ordering::Relaxed)
    }

    /// Worker side: publishes `WORKER_FLAG_*` bits.
    pub fn set_worker_flags(&self, flags: u32) {
        // SAFETY (closure): field projection of a valid header pointer.
        self.atomic32(|h| unsafe { &raw mut (*h).worker_flags })
            .store(flags, Ordering::Relaxed);
    }

    /// Plugin side: publishes its counters. Real-time safe.
    pub fn publish_client_counters(&self, c: &ClientCounters) {
        // SAFETY (closures): field projections of a valid header pointer.
        self.atomic64(|h| unsafe { &raw mut (*h).play_seq })
            .store(c.play_seq, Ordering::Relaxed);
        self.atomic64(|h| unsafe { &raw mut (*h).blocks_sent })
            .store(c.blocks_sent, Ordering::Relaxed);
        self.atomic64(|h| unsafe { &raw mut (*h).blocks_dropped })
            .store(c.blocks_dropped, Ordering::Relaxed);
        self.atomic64(|h| unsafe { &raw mut (*h).blocks_missed })
            .store(c.blocks_missed, Ordering::Relaxed);
    }

    /// Plugin side: publishes the transport delay it plays processed blocks
    /// out with, in samples (0: not known). Real-time safe.
    pub fn publish_transport_delay(&self, samples: u32) {
        // SAFETY (closure): field projection of a valid header pointer.
        self.atomic32(|h| unsafe { &raw mut (*h).transport_delay })
            .store(samples, Ordering::Relaxed);
    }

    /// Worker side: the transport delay the plugin published (0: none
    /// yet, or a plugin that does not publish it; untrusted).
    pub fn transport_delay(&self) -> u32 {
        // SAFETY (closure): field projection of a valid header pointer.
        self.atomic32(|h| unsafe { &raw mut (*h).transport_delay })
            .load(Ordering::Relaxed)
    }

    /// Worker side: the plugin's published counters (untrusted, for statistics).
    pub fn client_counters(&self) -> ClientCounters {
        // SAFETY (closures): field projections of a valid header pointer.
        ClientCounters {
            play_seq: self
                .atomic64(|h| unsafe { &raw mut (*h).play_seq })
                .load(Ordering::Relaxed),
            blocks_sent: self
                .atomic64(|h| unsafe { &raw mut (*h).blocks_sent })
                .load(Ordering::Relaxed),
            blocks_dropped: self
                .atomic64(|h| unsafe { &raw mut (*h).blocks_dropped })
                .load(Ordering::Relaxed),
            blocks_missed: self
                .atomic64(|h| unsafe { &raw mut (*h).blocks_missed })
                .load(Ordering::Relaxed),
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the mapping created in `map`; no reference
        // into it outlives `self`.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(v: f32) -> [f32; BLOCK_SAMPLES] {
        let mut b = [0.0; BLOCK_SAMPLES];
        for (i, s) in b.iter_mut().enumerate() {
            *s = v + i as f32;
        }
        b
    }

    #[test]
    fn slot_geometry() {
        const { assert!(SLOT_SIZE.is_multiple_of(64)) };
        const { assert!(SLOT_SIZE >= SLOT_HEADER_SIZE + BLOCK_SAMPLES * 4) };
        assert_eq!(region_size(8), 256 + 16 * SLOT_SIZE);
    }

    #[test]
    fn ring_wraps_many_times_in_order() {
        let r = Region::create(4).unwrap();
        let (mut head, mut tail) = (0u64, 0u64);
        let mut out = [0.0; BLOCK_SAMPLES];
        let mut next_pop = 0u64;
        for seq in 0..50u64 {
            assert!(r.push(Ring::Input, &mut head, seq, 0, &block(seq as f32)));
            if seq % 3 == 2 {
                // Drain to empty now and then; the ring wraps 12 times.
                while let Some(m) = r.pop(Ring::Input, &mut tail, &mut out).unwrap() {
                    assert_eq!(m.seq, next_pop);
                    assert_eq!(m.n_samples, BLOCK_SAMPLES as u32);
                    assert_eq!(out, block(next_pop as f32));
                    next_pop += 1;
                }
            }
        }
        while r.pop(Ring::Input, &mut tail, &mut out).unwrap().is_some() {
            next_pop += 1;
        }
        assert_eq!(next_pop, 50);
    }

    #[test]
    fn full_ring_refuses_and_keeps_order() {
        let r = Region::create(4).unwrap();
        let (mut head, mut tail) = (0u64, 0u64);
        for seq in 0..4 {
            assert!(r.push(Ring::Output, &mut head, seq, SLOT_FLAG_RAW, &block(0.0)));
        }
        assert!(!r.push(Ring::Output, &mut head, 99, 0, &block(0.0)), "full");
        assert_eq!(r.pending(Ring::Output), 4);
        let mut out = [0.0; BLOCK_SAMPLES];
        let m = r.pop(Ring::Output, &mut tail, &mut out).unwrap().unwrap();
        assert_eq!((m.seq, m.flags), (0, SLOT_FLAG_RAW));
        assert!(r.push(Ring::Output, &mut head, 4, 0, &block(0.0)));
    }

    #[test]
    fn implausible_peer_counter_is_corrupt() {
        let r = Region::create(4).unwrap();
        // A peer publishing a head 100 blocks ahead of our tail.
        r.head(Ring::Input).store(100, Ordering::Release);
        let mut tail = 0;
        let mut out = [0.0; BLOCK_SAMPLES];
        assert_eq!(r.pop(Ring::Input, &mut tail, &mut out), Err(RingCorrupt));
        // A consumer tail ahead of our head is corrupt, not a full ring.
        r.tail(Ring::Output).store(50, Ordering::Release);
        let mut head = 0;
        assert_eq!(
            r.try_push(Ring::Output, &mut head, 0, 0, &block(0.0)),
            Err(RingCorrupt)
        );
        assert!(!r.push(Ring::Output, &mut head, 0, 0, &block(0.0)));
        assert_eq!(head, 0, "nothing written");
        // One block ahead is already corrupt.
        r.tail(Ring::Input).store(1, Ordering::Release);
        assert_eq!(
            r.try_push(Ring::Input, &mut head, 0, 0, &block(0.0)),
            Err(RingCorrupt)
        );
        // A full ring is Ok(false).
        r.tail(Ring::Input).store(0, Ordering::Release);
        for seq in 0..4 {
            assert_eq!(
                r.try_push(Ring::Input, &mut head, seq, 0, &block(0.0)),
                Ok(true)
            );
        }
        assert_eq!(
            r.try_push(Ring::Input, &mut head, 4, 0, &block(0.0)),
            Ok(false)
        );
    }

    #[test]
    fn open_validates_size_seals_and_header() {
        use std::os::fd::AsFd;
        let r = Region::create(8).unwrap();
        let dup = || r.fd().as_fd().try_clone_to_owned().unwrap();
        let ok = Region::open(dup(), 8, region_size(8) as u64).unwrap();
        // Both mappings see the same counters.
        r.set_latency_samples(123);
        assert_eq!(ok.latency_samples(), 123);
        assert!(matches!(
            Region::open(dup(), 4, region_size(4) as u64),
            Err(ShmError::Invalid(_))
        ));
        assert!(matches!(
            Region::open(dup(), 8, 1234),
            Err(ShmError::Invalid(_))
        ));
        assert!(matches!(
            Region::open(dup(), 3, region_size(3) as u64),
            Err(ShmError::Invalid(_))
        ));
    }

    #[test]
    fn open_rejects_a_short_or_unsealed_memfd() {
        // Sealed but shorter than the hello says.
        let short = sys::sealed_memfd(c"short", region_size(8) - SLOT_SIZE).unwrap();
        assert!(matches!(
            Region::open(short, 8, region_size(8) as u64),
            Err(ShmError::Invalid(_))
        ));
        // The right size but not sealed: it could be shrunk under us.
        // SAFETY: plain syscalls on a fresh descriptor.
        let unsealed = unsafe {
            use std::os::fd::FromRawFd;
            let fd = libc::memfd_create(
                c"unsealed".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            );
            assert!(fd >= 0);
            assert_eq!(libc::ftruncate(fd, region_size(8) as libc::off_t), 0);
            OwnedFd::from_raw_fd(fd)
        };
        assert!(matches!(
            Region::open(unsealed, 8, region_size(8) as u64),
            Err(ShmError::Invalid(_))
        ));
        // A zero-filled memfd of the right size: no header.
        let blank = sys::sealed_memfd(c"blank", region_size(8)).unwrap();
        assert!(matches!(
            Region::open(blank, 8, region_size(8) as u64),
            Err(ShmError::Invalid(_))
        ));
    }
}
