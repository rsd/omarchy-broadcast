//! The real-time playout engine: reblocks the input, sends blocks to the
//! worker, and plays either the processed block or the delay-matched raw
//! input, crossfading at every switch.
//!
//! # Delays
//!
//! * `proc_delay` (`Lp`): output sample `n` plays block-domain sample
//!   `n - Lp` of the processed stream, i.e. block `(n - Lp) / 480`. It is
//!   the largest [`obc_audio::transport_delay`] seen for the worker's
//!   pipeline depth over every quantum since activation: the formula is
//!   not monotonic in the quantum (480 needs 480 samples, 256 needs 960),
//!   so the delay of every quantum is computed and the largest kept, which
//!   is safe for all of them. It only changes while the processed stream
//!   is inaudible (crossfade at 0); a change requested while processing
//!   (a quantum needing more, a new connection) first fades to raw. It is
//!   published in the region's header at every `process` with a link
//!   (one relaxed atomic store), for the worker's `ProcessLatency`.
//! * `raw_delay` (`Dr`): the raw fallback plays input sample `n - Dr`. It
//!   starts at [`NOMINAL_DELAY`]. While the processed stream is fully on,
//!   it follows `Lp + latency_samples` (raw is inaudible then), so a later
//!   switch to raw is between aligned signals. Before a fade-in from raw
//!   to processed, if `Dr` differs from `Lp + latency_samples` (the first
//!   connection of an activation, a new depth or quantum, a new latency),
//!   the raw path is first moved to the new delay by a crossfade between
//!   its two taps (the one unavoidable repeat or skip of a delay change),
//!   and only then does the processed stream fade in, against raw that is
//!   aligned with it. The `latency` port reports `Dr`.
//!
//! # Decisions and crossfades
//!
//! When playout enters block `k`, the engine decides for the whole block:
//! processed if block `k` is in the store as processed, otherwise raw (a
//! miss is counted if a worker is connected and `k` was sent to it). The
//! crossfade then moves one sample at a time ([`CROSSFADE_SAMPLES`], 5 ms,
//! linear) towards the decision. Fading in, both signals exist. Fading
//! out on a miss there is no processed sample to fade from, so the last
//! processed sample is held and faded to zero against the rising raw
//! signal: the output stays continuous (no step), and is bit-exact raw
//! once the fade reaches 0.

use obc_audio::client::ClientRings;
use obc_audio::dsp::{BlockStore, Crossfade, DelayLine, Reblocker, Stored};
use obc_audio::shm::{ClientCounters, SLOT_FLAG_DISCONT, SLOT_FLAG_NEW_STREAM, SLOT_FLAG_RAW};
use obc_audio::{
    BLOCK_SAMPLES, MAX_LATENCY_SAMPLES, MAX_PIPELINE_DEPTH, MAX_QUANTUM, transport_delay,
};

/// The raw delay before any worker has been connected: the transport
/// delay of a 480-sample quantum at depth 1 (10 ms).
pub const NOMINAL_DELAY: u32 = transport_delay(BLOCK_SAMPLES as u32, 1);
/// Crossfade length in samples (5 ms).
pub const CROSSFADE_SAMPLES: u32 = 240;
/// Raw history: room for the largest raw delay plus one 2048-sample piece.
/// The largest transport delay is 4317 samples (quantum 1919 at depth 4:
/// `480 - 1 + 1919 * 2`; checked over every quantum and depth below), plus
/// 4800 of algorithmic delay: 9117 + 2048 = 11165 <= 16384.
pub const RAW_HISTORY: usize = 16_384;

/// The largest transport delay over every quantum and depth.
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
const _: () =
    assert!((MAX_TRANSPORT_DELAY + MAX_LATENCY_SAMPLES + MAX_QUANTUM) as usize <= RAW_HISTORY);
/// Received blocks kept, by sequence number.
pub const STORE_BLOCKS: usize = 32;

const NONE: u64 = u64::MAX;
const BLOCK: u64 = BLOCK_SAMPLES as u64;

/// A connection the audio thread may use during one `run()`.
#[derive(Debug)]
pub struct Link {
    /// Unique per connection, never 0.
    pub id: u64,
    /// The rings.
    pub rings: ClientRings,
}

/// What one `process` call observed.
#[derive(Debug, Default, Clone, Copy)]
pub struct Outcome {
    /// The worker published implausible ring counters: drop the connection.
    pub corrupt: bool,
}

/// The engine's state, owned by the audio thread.
#[derive(Debug)]
pub struct Engine {
    raw: DelayLine,
    reblock: Reblocker,
    store: BlockStore,
    fade: Crossfade,
    scratch: [f32; BLOCK_SAMPLES],
    /// Absolute index of the next input sample; a multiple of 480 at reset.
    n: u64,
    /// Sequence number of the block being accumulated.
    next_seq: u64,
    raw_delay: u32,
    proc_delay: u32,
    proc_target: u32,
    /// The last quantum seen (0: none since activation).
    last_q: u32,
    /// Per depth (index `depth - 1`), the largest transport delay of any
    /// quantum seen since activation.
    max_delay: [u32; MAX_PIPELINE_DEPTH as usize],
    /// While `retargeting`, the raw tap being faded out.
    raw_from: u32,
    raw_fade: Crossfade,
    retargeting: bool,
    latency: u32,
    cur_block: u64,
    cur_use: bool,
    last_p: f32,
    link_id: u64,
    bad_link: u64,
    link_first_seq: u64,
    depth: u32,
    next_flags: u32,
    counters: ClientCounters,
    /// Blocks missed over the instance's life.
    pub misses: u64,
    /// Output blocks that arrived after their playout time.
    pub late: u64,
    /// Input blocks not sent because the input ring was full.
    pub dropped: u64,
    /// Input blocks sent over the instance's life.
    pub sent: u64,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// A fresh engine; allocates its buffers.
    pub fn new() -> Self {
        Self {
            raw: DelayLine::new(RAW_HISTORY),
            reblock: Reblocker::new(),
            store: BlockStore::new(STORE_BLOCKS),
            fade: Crossfade::new(CROSSFADE_SAMPLES),
            scratch: [0.0; BLOCK_SAMPLES],
            n: 0,
            next_seq: 0,
            raw_delay: NOMINAL_DELAY,
            proc_delay: NOMINAL_DELAY,
            proc_target: NOMINAL_DELAY,
            last_q: 0,
            max_delay: [0; MAX_PIPELINE_DEPTH as usize],
            raw_from: NOMINAL_DELAY,
            raw_fade: Crossfade::new(CROSSFADE_SAMPLES),
            retargeting: false,
            latency: 0,
            cur_block: NONE,
            cur_use: false,
            last_p: 0.0,
            link_id: 0,
            bad_link: 0,
            link_first_seq: 0,
            depth: 1,
            next_flags: SLOT_FLAG_NEW_STREAM | SLOT_FLAG_DISCONT,
            counters: ClientCounters::default(),
            misses: 0,
            late: 0,
            dropped: 0,
            sent: 0,
        }
    }

    /// LADSPA `activate`: clears the audio state. Sequence numbers keep
    /// increasing (with a gap) so a connected worker sees a new stream,
    /// never a repeated sequence number. Not real-time.
    pub fn reset(&mut self) {
        self.next_seq += 2;
        self.n = self.next_seq * BLOCK;
        self.raw.reset(self.n);
        self.reblock.reset();
        self.store.clear();
        self.fade.reset();
        self.raw_delay = NOMINAL_DELAY;
        self.proc_delay = self.proc_target;
        self.last_q = 0;
        self.max_delay = [0; MAX_PIPELINE_DEPTH as usize];
        self.raw_from = NOMINAL_DELAY;
        self.raw_fade.reset();
        self.retargeting = false;
        self.cur_block = NONE;
        self.cur_use = false;
        self.last_p = 0.0;
        self.link_first_seq = self.next_seq;
        self.next_flags = SLOT_FLAG_NEW_STREAM | SLOT_FLAG_DISCONT;
    }

    /// Touches every buffer, so the audio thread never takes a page fault
    /// on them. Not real-time; call after [`Engine::new`].
    pub fn prefault(&mut self) {
        self.raw.prefault();
        self.store.prefault();
    }

    /// The delay the plugin currently adds, in samples (the `latency` port).
    pub fn latency_port(&self) -> u32 {
        self.raw_delay
    }

    /// The transport delay in use.
    pub fn proc_delay(&self) -> u32 {
        self.proc_delay
    }

    /// Whether the output is fully the processed stream.
    pub fn processing(&self) -> bool {
        self.cur_use && self.fade.is_on()
    }

    fn on_new_link(&mut self, link: &Link) {
        self.link_id = link.id;
        self.depth = link.rings.pipeline_depth();
        self.link_first_seq = self.next_seq;
        self.next_flags = SLOT_FLAG_NEW_STREAM | SLOT_FLAG_DISCONT;
        self.proc_target = self.max_delay_for(self.depth);
        self.latency = link.rings.latency_samples();
        self.counters = ClientCounters::default();
    }

    /// Processes one piece of at most [`MAX_QUANTUM`] samples of a `run()`
    /// of `quantum` samples. Real-time safe.
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        quantum: u32,
        bypass: bool,
        link: Option<&mut Link>,
    ) -> Outcome {
        debug_assert_eq!(input.len(), output.len());
        debug_assert!(input.len() <= MAX_QUANTUM as usize);
        let mut outcome = Outcome::default();

        let q = quantum.clamp(1, MAX_QUANTUM);
        if q != self.last_q {
            self.last_q = q;
            for (d, max) in (1..).zip(self.max_delay.iter_mut()) {
                *max = (*max).max(transport_delay(q, d));
            }
        }

        let mut link = link.filter(|l| l.id != self.bad_link);
        match link.as_deref() {
            Some(l) if l.id != self.link_id => self.on_new_link(l),
            None => self.link_id = 0,
            Some(_) => {}
        }
        if self.link_id != 0 {
            self.proc_target = self.proc_target.max(self.max_delay_for(self.depth));
        }

        // 1. Take whatever the worker has sent.
        if let Some(l) = link.as_deref_mut() {
            self.latency = l.rings.latency_samples();
            for _ in 0..l.rings.capacity() {
                match l.rings.pop_output(&mut self.scratch) {
                    Ok(None) => break,
                    Ok(Some(meta)) => self.receive(meta.seq, meta.flags, meta.n_samples),
                    Err(_) => {
                        self.bad_link = l.id;
                        outcome.corrupt = true;
                        break;
                    }
                }
            }
        }
        if outcome.corrupt {
            link = None;
            self.link_id = 0;
        }

        // 2. Record the input and send every block it completes.
        let n0 = self.n;
        self.raw.push(input);
        let sending = !bypass && link.is_some();
        let mut pushed = false;
        let mut corrupt_in = false;
        {
            let Self {
                reblock,
                next_seq,
                next_flags,
                counters,
                dropped,
                sent,
                ..
            } = self;
            let mut rings = link.as_deref_mut().map(|l| &mut l.rings);
            reblock.push(input, |block| {
                let seq = *next_seq;
                *next_seq += 1;
                match rings.as_deref_mut() {
                    Some(r) if sending && !corrupt_in => {
                        match r.push_input(seq, *next_flags, block) {
                            Ok(true) => {
                                *next_flags = 0;
                                counters.blocks_sent += 1;
                                *sent += 1;
                                pushed = true;
                            }
                            Ok(false) => {
                                *next_flags |= SLOT_FLAG_DISCONT;
                                counters.blocks_dropped += 1;
                                *dropped += 1;
                            }
                            Err(_) => {
                                *next_flags |= SLOT_FLAG_DISCONT;
                                corrupt_in = true;
                            }
                        }
                    }
                    _ => *next_flags |= SLOT_FLAG_DISCONT,
                }
            });
        }
        if corrupt_in {
            if let Some(l) = link.as_deref() {
                self.bad_link = l.id;
            }
            outcome.corrupt = true;
            link = None;
            self.link_id = 0;
        }
        self.n += input.len() as u64;

        // 3. Play out.
        for (i, out) in output.iter_mut().enumerate() {
            let now = n0 + i as u64;
            if self.proc_target != self.proc_delay && self.fade.is_off() {
                self.proc_delay = self.proc_target;
                self.cur_block = NONE;
            }
            let (block, off) = match now.checked_sub(u64::from(self.proc_delay)) {
                Some(m) => (m / BLOCK, (m % BLOCK) as usize),
                None => (NONE - 1, 0),
            };
            if block != self.cur_block {
                self.cur_block = block;
                self.cur_use = self.decide(block, bypass);
            }
            let p = if self.cur_use {
                match self.store.get(block) {
                    Some((Stored::Processed, s)) => Some(s[off]),
                    _ => None,
                }
            } else {
                None
            };
            // Before fading processed in, align raw with it (see the
            // module docs): only while raw alone is audible.
            let want = self.proc_delay + self.latency;
            if p.is_some() && self.raw_delay != want && self.fade.is_off() && !self.retargeting {
                self.raw_from = self.raw_delay;
                self.raw_delay = want;
                self.raw_fade.reset();
                self.retargeting = true;
            }
            let r = if self.retargeting {
                self.raw_fade.step(true);
                let r = self.raw_fade.mix(
                    self.raw.delayed(now, self.raw_from),
                    self.raw.delayed(now, self.raw_delay),
                );
                if self.raw_fade.is_on() {
                    self.retargeting = false;
                }
                r
            } else {
                self.raw.delayed(now, self.raw_delay)
            };
            self.fade.step(p.is_some() && !self.retargeting);
            *out = match p {
                Some(p) => {
                    self.last_p = p;
                    self.fade.mix(r, p)
                }
                None => self.fade.mix(r, self.last_p),
            };
            if p.is_some() && self.fade.is_on() {
                self.raw_delay = self.proc_delay + self.latency;
            }
        }

        if let Some(l) = link.as_deref() {
            if pushed {
                l.rings.notify();
            }
            self.counters.play_seq = self.cur_block;
            self.counters.blocks_missed = self.misses;
            l.rings.publish(&self.counters);
            // The worker's `ProcessLatency` and status follow it.
            l.rings.publish_transport_delay(self.proc_delay);
        }
        outcome
    }

    fn max_delay_for(&self, depth: u32) -> u32 {
        let i = depth.clamp(1, MAX_PIPELINE_DEPTH) as usize - 1;
        let seen = self.max_delay[i];
        if seen == 0 {
            transport_delay(self.last_q.max(1), depth)
        } else {
            seen
        }
    }

    fn receive(&mut self, seq: u64, flags: u32, n_samples: u32) {
        if seq >= self.next_seq || n_samples != BLOCK_SAMPLES as u32 {
            return; // never sent, or malformed
        }
        let playing = self
            .n
            .checked_sub(u64::from(self.proc_delay))
            .map_or(0, |m| m / BLOCK);
        if seq < playing {
            self.late += 1;
            return;
        }
        let kind = if flags & SLOT_FLAG_RAW != 0 {
            Stored::Raw
        } else {
            Stored::Processed
        };
        self.store.insert(seq, kind, &self.scratch);
    }

    fn decide(&mut self, block: u64, bypass: bool) -> bool {
        if block >= NONE - 1 || bypass || self.proc_target != self.proc_delay {
            return false;
        }
        match self.store.get(block) {
            Some((Stored::Processed, _)) => true,
            Some((Stored::Raw, _)) => false,
            None => {
                if self.link_id != 0 && block >= self.link_first_seq {
                    self.misses += 1;
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_link_is_a_bit_exact_nominal_delay() {
        let mut e = Engine::new();
        e.reset();
        let input: Vec<f32> = (0..10_000)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5)
            .collect();
        let mut out = vec![0.0; input.len()];
        let mut at = 0;
        for q in [32usize, 441, 2048, 480, 1000].iter().cycle() {
            if at >= input.len() {
                break;
            }
            let end = (at + q).min(input.len());
            e.process(
                &input[at..end],
                &mut out[at..end],
                (end - at) as u32,
                false,
                None,
            );
            at = end;
        }
        let d = NOMINAL_DELAY as usize;
        assert_eq!(e.latency_port(), NOMINAL_DELAY);
        assert!(out[..d].iter().all(|&s| s == 0.0));
        for i in d..input.len() {
            assert_eq!(out[i].to_bits(), input[i - d].to_bits(), "sample {i}");
        }
    }

    #[test]
    fn bypass_is_raw() {
        let mut e = Engine::new();
        let input = [0.25f32; 4800];
        let mut out = [0.0f32; 4800];
        e.process(&input[..2048], &mut out[..2048], 2048, true, None);
        assert_eq!(e.latency_port(), NOMINAL_DELAY);
        assert!(!e.processing());
    }
}
