//! Real-time building blocks of the plugin's playout: the transport delay
//! formula, a delay line addressed by absolute sample index, a reblocker
//! (arbitrary quanta to 480-sample blocks), a linear crossfade and a store
//! of received blocks addressed by sequence number.
//!
//! Every type allocates in its constructor only; every other method is
//! real-time safe (no allocation, no lock, no syscall).

use crate::BLOCK_SAMPLES;

const BLOCK: u32 = BLOCK_SAMPLES as u32;

const fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// The plugin's transport delay `Lp`, in samples, for a graph quantum of
/// `quantum` samples and a worker pipeline depth of `depth` blocks:
///
/// ```text
/// Lp = 480 - gcd(480, Q) + Q * ceil(480 * depth / Q)
/// ```
///
/// It is the smallest delay for which, at every block alignment, the
/// `run()` call that plays the first sample of block `k` comes at least
/// `ceil(480 * depth / Q)` calls (at least `480 * depth` samples of wall
/// time) after the call that completed block `k`, which is when the block
/// was sent. `480 - gcd(480, Q)` is the reblocking term: zero when `Q` is
/// a multiple of 480. Examples, depth 1: Q = 480 gives 480 (10 ms);
/// 256 gives 960; 32 gives 928; 1024 gives 1472. Depth 2, Q = 480: 960.
///
/// `quantum` is clamped to 1..=[`crate::MAX_QUANTUM`] and `depth` to
/// 1..=[`crate::MAX_PIPELINE_DEPTH`].
pub const fn transport_delay(quantum: u32, depth: u32) -> u32 {
    let q = if quantum == 0 {
        1
    } else if quantum > crate::MAX_QUANTUM {
        crate::MAX_QUANTUM
    } else {
        quantum
    };
    let d = if depth == 0 {
        1
    } else if depth > crate::MAX_PIPELINE_DEPTH {
        crate::MAX_PIPELINE_DEPTH
    } else {
        depth
    };
    BLOCK - gcd(BLOCK, q) + q * (BLOCK * d).div_ceil(q)
}

/// Writes every page of `buf` (volatile, so the writes are not optimised
/// away), so a lazily zero-filled allocation is backed by real memory
/// before the audio thread touches it.
fn touch_pages<T: Copy>(buf: &mut [T]) {
    let step = (4096 / size_of::<T>().max(1)).max(1);
    let mut i = 0;
    while i < buf.len() {
        // SAFETY: `i < buf.len()`; a volatile write of the value already there.
        unsafe {
            let p = buf.as_mut_ptr().add(i);
            p.write_volatile(p.read());
        }
        i += step;
    }
    if let Some(last) = buf.last_mut() {
        // SAFETY: a valid element of `buf`, as above.
        unsafe { std::ptr::write_volatile(last, *last) };
    }
}

/// A delay line holding the last `capacity` input samples, addressed by
/// absolute sample index.
#[derive(Debug)]
pub struct DelayLine {
    buf: Box<[f32]>,
    mask: u64,
    written: u64,
}

impl DelayLine {
    /// A zero-filled line of `capacity` samples (rounded up to a power of two).
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two();
        Self {
            buf: vec![0.0; cap].into_boxed_slice(),
            mask: cap as u64 - 1,
            written: 0,
        }
    }

    /// Appends samples; their absolute indices continue from the last push.
    pub fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            self.buf[(self.written & self.mask) as usize] = s;
            self.written += 1;
        }
    }

    /// Skips `n` samples (they read as zeros if still in range).
    pub fn skip_zeros(&mut self, n: u64) {
        for _ in 0..n.min(self.mask + 1) {
            self.buf[(self.written & self.mask) as usize] = 0.0;
            self.written += 1;
        }
        self.written += n.saturating_sub(self.mask + 1);
    }

    /// The sample at absolute index `at`, or 0 if it was never written or
    /// has already left the line.
    pub fn get(&self, at: u64) -> f32 {
        if at >= self.written || self.written - at > self.mask + 1 {
            return 0.0;
        }
        self.buf[(at & self.mask) as usize]
    }

    /// The sample `delay` samples before absolute index `now`.
    pub fn delayed(&self, now: u64, delay: u32) -> f32 {
        match now.checked_sub(u64::from(delay)) {
            Some(at) => self.get(at),
            None => 0.0,
        }
    }

    /// Touches every page of the line (call from a non-real-time thread
    /// after creating it).
    pub fn prefault(&mut self) {
        touch_pages(&mut self.buf);
    }

    /// The number of samples ever pushed (the next absolute index).
    pub fn written(&self) -> u64 {
        self.written
    }

    /// The capacity in samples.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Zeroes the line and restarts it at absolute index `at`.
    pub fn reset(&mut self, at: u64) {
        self.buf.fill(0.0);
        self.written = at;
    }
}

/// Collects arbitrary quanta into 480-sample blocks.
#[derive(Debug)]
pub struct Reblocker {
    block: [f32; BLOCK_SAMPLES],
    fill: usize,
}

impl Default for Reblocker {
    fn default() -> Self {
        Self::new()
    }
}

impl Reblocker {
    /// An empty reblocker.
    pub const fn new() -> Self {
        Self {
            block: [0.0; BLOCK_SAMPLES],
            fill: 0,
        }
    }

    /// Feeds samples, calling `on_block` with every block they complete,
    /// in order. Leftover samples wait for the next call.
    pub fn push(&mut self, mut input: &[f32], mut on_block: impl FnMut(&[f32; BLOCK_SAMPLES])) {
        while !input.is_empty() {
            let take = (BLOCK_SAMPLES - self.fill).min(input.len());
            self.block[self.fill..self.fill + take].copy_from_slice(&input[..take]);
            self.fill += take;
            input = &input[take..];
            if self.fill == BLOCK_SAMPLES {
                on_block(&self.block);
                self.fill = 0;
            }
        }
    }

    /// Samples waiting for the current block.
    pub fn fill(&self) -> usize {
        self.fill
    }

    /// Discards a partial block.
    pub fn reset(&mut self) {
        self.fill = 0;
    }
}

/// A linear crossfade position, counted in whole samples so that the end
/// points are exactly 0 and 1 (bit-exact raw and processed output).
#[derive(Debug, Clone, Copy)]
pub struct Crossfade {
    pos: u32,
    len: u32,
}

impl Crossfade {
    /// A fade of `len` samples (at least 1), starting fully on the "from"
    /// side (gain 0).
    pub const fn new(len: u32) -> Self {
        Self {
            pos: 0,
            len: if len == 0 { 1 } else { len },
        }
    }

    /// Moves one sample towards gain 1 (`up`) or gain 0.
    pub fn step(&mut self, up: bool) {
        if up {
            self.pos = (self.pos + 1).min(self.len);
        } else {
            self.pos = self.pos.saturating_sub(1);
        }
    }

    /// The gain of the "to" side, 0..=1.
    pub fn gain(&self) -> f32 {
        self.pos as f32 / self.len as f32
    }

    /// Exactly at gain 0.
    pub fn is_off(&self) -> bool {
        self.pos == 0
    }

    /// Exactly at gain 1.
    pub fn is_on(&self) -> bool {
        self.pos == self.len
    }

    /// Mixes: `g * to + (1 - g) * from`, exact at the end points.
    pub fn mix(&self, from: f32, to: f32) -> f32 {
        if self.pos == 0 {
            from
        } else if self.pos == self.len {
            to
        } else {
            let g = self.gain();
            g * to + (1.0 - g) * from
        }
    }

    /// Back to gain 0.
    pub fn reset(&mut self) {
        self.pos = 0;
    }
}

/// What a stored block holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// Processed samples.
    Processed,
    /// The worker sent [`crate::shm::SLOT_FLAG_RAW`]: play raw.
    Raw,
}

/// Received blocks addressed by sequence number, `slots` of them.
#[derive(Debug)]
pub struct BlockStore {
    samples: Box<[f32]>,
    tags: Box<[u64]>,
    kinds: Box<[Stored]>,
}

const EMPTY: u64 = u64::MAX;

impl BlockStore {
    /// A store of `slots` blocks (block `s` lives in slot `s % slots`).
    pub fn new(slots: usize) -> Self {
        let slots = slots.max(1);
        Self {
            samples: vec![0.0; slots * BLOCK_SAMPLES].into_boxed_slice(),
            tags: vec![EMPTY; slots].into_boxed_slice(),
            kinds: vec![Stored::Raw; slots].into_boxed_slice(),
        }
    }

    fn index(&self, seq: u64) -> usize {
        (seq % self.tags.len() as u64) as usize
    }

    /// Stores block `seq`, replacing whatever shared its slot.
    pub fn insert(&mut self, seq: u64, kind: Stored, block: &[f32; BLOCK_SAMPLES]) {
        let i = self.index(seq);
        self.tags[i] = seq;
        self.kinds[i] = kind;
        self.samples[i * BLOCK_SAMPLES..(i + 1) * BLOCK_SAMPLES].copy_from_slice(block);
    }

    /// Block `seq`, if it is stored.
    pub fn get(&self, seq: u64) -> Option<(Stored, &[f32])> {
        let i = self.index(seq);
        (seq != EMPTY && self.tags[i] == seq).then(|| {
            (
                self.kinds[i],
                &self.samples[i * BLOCK_SAMPLES..(i + 1) * BLOCK_SAMPLES],
            )
        })
    }

    /// Touches every page of the store (call from a non-real-time thread
    /// after creating it).
    pub fn prefault(&mut self) {
        touch_pages(&mut self.samples);
        touch_pages(&mut self.tags);
        touch_pages(&mut self.kinds);
    }

    /// Forgets every block.
    pub fn clear(&mut self) {
        self.tags.fill(EMPTY);
    }

    /// The number of slots.
    pub fn slots(&self) -> usize {
        self.tags.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute force of the property `transport_delay` promises.
    fn min_delay_by_search(q: u32, d: u32) -> u32 {
        let calls_needed = (480 * d).div_ceil(q) as u64;
        (480..20_000)
            .find(|&l| {
                (0..300u64).all(|j| {
                    let done_call = (480 * j + 479) / q as u64;
                    let play_call = (480 * j + l as u64) / q as u64;
                    play_call >= done_call + calls_needed
                })
            })
            .unwrap()
    }

    #[test]
    fn transport_delay_examples() {
        assert_eq!(transport_delay(480, 1), 480);
        assert_eq!(transport_delay(960, 1), 960);
        assert_eq!(transport_delay(480, 2), 960);
        assert_eq!(transport_delay(256, 1), 960);
        assert_eq!(transport_delay(32, 1), 928);
        assert_eq!(transport_delay(1024, 1), 1472);
        assert_eq!(transport_delay(0, 0), transport_delay(1, 1));
        assert_eq!(
            transport_delay(1 << 20, 99),
            transport_delay(crate::MAX_QUANTUM, crate::MAX_PIPELINE_DEPTH)
        );
    }

    #[test]
    fn transport_delay_is_the_minimal_safe_delay() {
        for q in [
            32, 64, 100, 128, 240, 256, 441, 480, 512, 700, 960, 1024, 2048,
        ] {
            for d in 1..=2 {
                assert_eq!(
                    transport_delay(q, d),
                    min_delay_by_search(q, d),
                    "q={q} d={d}"
                );
            }
        }
    }

    #[test]
    fn delay_line_reads_back_by_index() {
        let mut dl = DelayLine::new(1000);
        assert_eq!(dl.capacity(), 1024);
        let input: Vec<f32> = (0..3000).map(|i| i as f32).collect();
        dl.push(&input[..1500]);
        dl.push(&input[1500..]);
        assert_eq!(dl.get(2999), 2999.0);
        assert_eq!(dl.get(2000), 2000.0);
        assert_eq!(dl.get(1975), 0.0, "left the line");
        assert_eq!(dl.get(3000), 0.0, "not written yet");
        assert_eq!(dl.delayed(3000, 480), 2520.0);
        assert_eq!(dl.delayed(100, 480), 0.0);
        dl.skip_zeros(10);
        assert_eq!(dl.written(), 3010);
        assert_eq!(dl.get(3005), 0.0);
        assert_eq!(dl.get(2999), 2999.0);
    }

    #[test]
    fn reblocker_cuts_any_quanta_into_blocks() {
        let mut rb = Reblocker::new();
        let input: Vec<f32> = (0..480 * 7).map(|i| i as f32).collect();
        let mut blocks = Vec::new();
        let mut at = 0;
        for q in [32, 441, 1024, 7, 960, 2048].iter().cycle() {
            if at >= input.len() {
                break;
            }
            let end = (at + q).min(input.len());
            rb.push(&input[at..end], |b| blocks.push(b.to_vec()));
            at = end;
        }
        assert_eq!(rb.fill(), 0);
        assert_eq!(blocks.len(), 7);
        for (k, b) in blocks.iter().enumerate() {
            assert_eq!(b[0], (k * 480) as f32);
            assert_eq!(b[479], (k * 480 + 479) as f32);
        }
    }

    #[test]
    fn crossfade_is_exact_at_the_ends_and_linear_between() {
        let mut x = Crossfade::new(4);
        assert!(x.is_off());
        assert_eq!(x.mix(1.0, 3.0), 1.0);
        x.step(true);
        x.step(true);
        assert_eq!(x.mix(1.0, 3.0), 2.0);
        x.step(true);
        x.step(true);
        x.step(true);
        assert!(x.is_on());
        assert_eq!(x.mix(1.0, 0.1), 0.1);
        x.step(false);
        assert!((x.gain() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn block_store_matches_sequence_numbers() {
        let mut s = BlockStore::new(4);
        let b = [0.25f32; BLOCK_SAMPLES];
        s.insert(10, Stored::Processed, &b);
        assert!(matches!(s.get(10), Some((Stored::Processed, x)) if x[0] == 0.25));
        assert!(s.get(14).is_none(), "same slot, other sequence");
        assert!(s.get(11).is_none());
        s.insert(14, Stored::Raw, &b);
        assert!(s.get(10).is_none(), "replaced");
        assert!(matches!(s.get(14), Some((Stored::Raw, _))));
        assert!(s.get(u64::MAX).is_none());
        s.clear();
        assert!(s.get(14).is_none());
    }
}
