//! Output pacing: when the output clock emits a frame, and which frame.
//!
//! [`Pacer`] is pure: it is fed the current time and the slot's newest
//! entry ([`Latest`]: sequence number, arrival time, source), and answers
//! with what to do. The output thread ([`crate::output`]) supplies the real
//! clock; the tests supply scripted times.
//!
//! **Timestamps.** Tick `n` carries the timestamp `D(n) = n x period`, an
//! absolute grid computed from the exact frame-rate fraction, so neither
//! rounding nor wake-up delays accumulate. Every index is emitted exactly
//! once, in order (unless a resynchronisation skips some, see below).
//!
//! **Emission instant.** Tick `n` is emitted at `D(n) + phase`, where the
//! phase offset is a signed offset bounded to `[-period, 0]`: a tick is
//! never emitted after its timestamp, nor more than one period before it.
//! With the phase fixed, the output rate is the contract rate exactly; the
//! bound means that over any span the emitted count differs from
//! `rate x span` by at most one frame.
//!
//! **Phase lock.** A camera frame waits in the slot until the next tick, so
//! with a fixed phase the added latency depends on where the camera's
//! frames happen to fall between ticks (anywhere from zero to one period).
//! The pacer therefore moves the phase so that each tick fires a small
//! margin ([`PhaseLockConfig::margin_ns`]) after the expected arrival of a
//! camera frame:
//!
//! * every fresh tick measures the frame's **wait** (tick time minus the
//!   frame's arrival). The last [`PhaseLockConfig::window`] waits are kept,
//!   corrected for every phase change made since they were measured, so
//!   they predict the waits at the current phase;
//! * the smallest predicted wait (the latest-arriving frame, which makes
//!   the estimate robust to arrival jitter) is steered to the margin.
//!   **Acquisition** (the first lock on an input run) moves the phase in
//!   one jump, and so does an error above [`PhaseLockConfig::jump_ns`] (the
//!   arrivals moved: a new camera, or a frame that missed its tick);
//!   otherwise the phase **tracks** camera clock drift by at most
//!   [`PhaseLockConfig::slew_ns`] per fresh tick;
//! * the phase stays in its band: a phase that has to leave it by more
//!   than [`PhaseLockConfig::wrap_hysteresis_ns`] **wraps** by a multiple of
//!   the input frame interval, chosen to land nearest the middle of the
//!   band (one period for a camera at the contract rate, half a period for
//!   one at twice the rate). A wrap makes one emission interval shorter or
//!   longer than a period (at most two periods) and changes no count: a
//!   camera slower than the contract shows one repeated frame there, a
//!   faster one one superseded frame. A 60 fps camera at a 30 fps contract
//!   simply moves to its other frame train, half a period away, without a
//!   repeat;
//! * the lock is engaged only while it can follow the input: the input
//!   runs at about the output rate, a multiple of it or half of it, within
//!   the drift the slew can follow.
//!   Two conditions, both over the frames of the same run (same source, no
//!   placeholder, hold or arrival gap over 2.5 periods in between; over
//!   2.5 input frame intervals for an input slower than the coarse limit
//!   below, at whatever rate):
//!   - the frame interval over the kept frames is at most
//!     [`PhaseLockConfig::max_input_interval_permille`] of a period, a
//!     coarse check that reacts within a window to a camera lowering its
//!     rate in dim light or a 15 fps mode. Over it, the input is
//!     **sub-rate**, and is followed only at half the rate (see below);
//!   - the input's frame train drifts against the ticks by at most
//!     [`PhaseLockConfig::slew_ns`] per tick when it moves earlier (an
//!     input faster than `k` x the output rate), and at most half of it
//!     when it moves later (a slower input, whose frames a tracking lag
//!     under jitter pushes past their ticks). The ticks catch every `k`-th
//!     frame (`k x interval` nearest a period: 1 at 30 fps, 2 at 60 fps),
//!     and that train moves against them by `period - k x interval` per
//!     tick. The interval is a least-squares fit of arrival time on
//!     sequence number over the run's last 64 fresh frames, so arrival
//!     jitter and a frame off its cadence barely move it. Early in a run,
//!     with few frames, the estimate is uncertain: a drift within three
//!     standard errors of the limit counts as followable, so the lock
//!     acquires after a few frames and lets go once the drift is known to
//!     be beyond the limit (a 29.85 or 30.25 fps camera against 30 fps, a
//!     58 fps one against 60). A lock that tried to follow such a camera
//!     would fall behind, miss ticks and jump again and again. Once it
//!     let go with a full window, the lock engages again only within half
//!     the limit, so an estimate near the limit cannot switch it on and
//!     off.
//!   - a **sub-rate** input is followed only at a frame interval of two
//!     periods (15 fps against 30 fps, 30 fps against 60 fps), once a full
//!     window of 64 arrivals measures it, and only while its cadence is
//!     steady (the median distance of an arrival from the fitted line at
//!     most an eighth of a period). Every frame then arrives at the same
//!     phase of the ticks: the lock steers the tick after it to the margin
//!     like any other, and the repeat goes out on the tick in between. The
//!     drift is `2 x period - interval` per frame, against the same limits.
//!     An arrival gap of two frame intervals (a frame the camera dropped;
//!     a longer gap ends the run) counts as two frames in the fit. The
//!     early-edge wrap hysteresis is at least the 4 ms step on which
//!     cameras deliver their frames, so one step at an edge cannot wrap
//!     the phase there and back. At other rates (20 or 24 fps against
//!     30 fps) the frames fall on several phases of the ticks, and the lock
//!     is not engaged.
//!
//!   Not engaged, the phase is held: the output pads a slow input with
//!   repeats, and thins a fast one, exactly as a free-running clock at
//!   that phase would. A run is acquired once: when the input comes back
//!   within reach, the lock tracks again from the held phase (and jumps
//!   only for an error above the jump threshold), so jitter that briefly
//!   stretches the measured interval cannot cause a series of jumps.
//!
//! **Schedule.** A tick is emitted when its emission instant has passed. A
//! wake-up later than the next tick's instant is a **late** tick: it is
//! still emitted, immediately, and the ticks after it follow at once until
//! the schedule is caught up. When the clock is behind by
//! [`PacerConfig::resync_after_ns`] or more (a long stall: suspend, a
//! stopped debugger), or when more than [`PacerConfig::max_burst`] ticks are
//! due at once, the pacer jumps to the tick due now and counts the ticks it
//! **skipped**. The output's queue holds `max_burst` buffers, so a burst is
//! never thinned by the queue, and a sink that stalls longer than the
//! queue's worth of frames gets a resynchronised stream instead of a backlog
//! of stale frames. A phase change never makes a tick late: the phase
//! stays within one period, so the next instant is never before the
//! current one, though a move towards the early edge (a jump or a wrap)
//! can make it due at once: two emissions back to back, in order and
//! neither after its timestamp.
//!
//! **Content.** Per tick: a frame newer than the last one emitted is
//! **fresh** (frames that arrived and were replaced in between are counted as
//! **superseded**); otherwise the last frame is emitted again (**repeat**)
//! while it is younger than the gap threshold; after that, and before any
//! frame arrived, the output's black frame (**placeholder**).
//!
//! **Hold.** A deliberate source switch arms a hold ([`Pacer::hold`]) naming
//! the new source and a deadline. While it holds, the last frame is repeated
//! whatever its age (unless the output was already sending placeholders);
//! fresh frames of other sources (the previous camera's last frames) are
//! still emitted but do not end it. The first fresh frame of the awaited
//! source ends it, and so does the deadline, after which the gap threshold
//! applies again.

use std::collections::VecDeque;

/// Phase lock parameters (see the module docs). The defaults and the
/// reasons for them are in `docs/adr/0010-phase-locked-output-clock.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PhaseLockConfig {
    /// Lock the emission phase to the input. Off: the phase stays 0 and
    /// ticks are emitted at their timestamps.
    pub enabled: bool,
    /// Target time from the latest-arriving frame of the window to its tick.
    pub margin_ns: u64,
    /// Fresh ticks whose waits are kept (the minimum is taken over them).
    pub window: usize,
    /// Fresh ticks of a run needed before the first lock.
    pub acquire_after: usize,
    /// Largest phase change per fresh tick while tracking.
    pub slew_ns: u64,
    /// An error larger than this (waits longer than the margin by more)
    /// re-acquires in one jump instead of slewing.
    pub jump_ns: u64,
    /// How far past the early edge of its band (`-period`) the phase must
    /// want to go before it wraps; closer than that it stays at the edge
    /// (the waits grow by up to this much). Past the late edge (0) it wraps
    /// at once, since waits shorter than the margin risk missed ticks. One
    /// edge with a hysteresis is enough to keep arrival jitter from
    /// wrapping back and forth. A sub-rate input uses at least 4 ms (see
    /// the module docs).
    pub wrap_hysteresis_ns: u64,
    /// The lock is engaged while the input frame interval (over the kept
    /// frames) is at most this many thousandths of the output period, and
    /// the input's drift against the ticks is within what the slew can
    /// follow; over it, only an input at half the output rate is followed
    /// (see the module docs).
    pub max_input_interval_permille: u32,
}

impl PhaseLockConfig {
    pub const DEFAULT_MARGIN_NS: u64 = 1_500_000;
    pub const DEFAULT_WINDOW: usize = 8;
    pub const DEFAULT_ACQUIRE_AFTER: usize = 3;
    pub const DEFAULT_SLEW_NS: u64 = 200_000;
    pub const DEFAULT_JUMP_NS: u64 = 8_000_000;
    pub const DEFAULT_WRAP_HYSTERESIS_NS: u64 = 1_000_000;
    pub const DEFAULT_MAX_INPUT_INTERVAL_PERMILLE: u32 = 1050;

    /// The lock switched off: ticks at their timestamps.
    pub fn disabled() -> Self {
        PhaseLockConfig {
            enabled: false,
            ..Self::default()
        }
    }
}

impl Default for PhaseLockConfig {
    fn default() -> Self {
        PhaseLockConfig {
            enabled: true,
            margin_ns: Self::DEFAULT_MARGIN_NS,
            window: Self::DEFAULT_WINDOW,
            acquire_after: Self::DEFAULT_ACQUIRE_AFTER,
            slew_ns: Self::DEFAULT_SLEW_NS,
            jump_ns: Self::DEFAULT_JUMP_NS,
            wrap_hysteresis_ns: Self::DEFAULT_WRAP_HYSTERESIS_NS,
            max_input_interval_permille: Self::DEFAULT_MAX_INPUT_INTERVAL_PERMILLE,
        }
    }
}

/// Pacing parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacerConfig {
    /// Output frame rate numerator (frames per `fps_den` seconds).
    pub fps_num: u32,
    pub fps_den: u32,
    /// Age of the newest input frame beyond which the placeholder replaces
    /// repeats.
    pub gap_threshold_ns: u64,
    /// How far behind its schedule the clock may fall before it
    /// resynchronises instead of catching up.
    pub resync_after_ns: u64,
    /// Most ticks emitted back to back while catching up; with more due at
    /// once the clock resynchronises. The output's queue holds this many
    /// buffers.
    pub max_burst: u64,
    pub lock: PhaseLockConfig,
}

impl PacerConfig {
    /// Default gap threshold before the placeholder (500 ms).
    pub const DEFAULT_GAP_THRESHOLD_NS: u64 = 500_000_000;
    /// Default resynchronisation threshold (1 s).
    pub const DEFAULT_RESYNC_AFTER_NS: u64 = 1_000_000_000;
    /// Default catch-up burst, and output queue, in buffers.
    pub const DEFAULT_MAX_BURST: u64 = 3;

    /// Defaults for a frame rate of `fps_num / fps_den`.
    pub fn new(fps_num: u32, fps_den: u32) -> Self {
        PacerConfig {
            fps_num,
            fps_den,
            gap_threshold_ns: Self::DEFAULT_GAP_THRESHOLD_NS,
            resync_after_ns: Self::DEFAULT_RESYNC_AFTER_NS,
            max_burst: Self::DEFAULT_MAX_BURST,
            lock: PhaseLockConfig::default(),
        }
    }

    /// Deadline (and timestamp) of tick `index`: `index x 1e9 x den / num`
    /// nanoseconds, exact to the nanosecond (rounded down).
    pub fn deadline_ns(&self, index: u64) -> u64 {
        let ns =
            u128::from(index) * 1_000_000_000 * u128::from(self.fps_den) / u128::from(self.fps_num);
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    /// Duration of tick `index`: from its deadline to the next one. With a
    /// rate whose period is not a whole number of nanoseconds (`30000/1001`)
    /// it alternates between the two neighbouring integers, so that
    /// `deadline_ns(index) + duration_ns(index) == deadline_ns(index + 1)`:
    /// timestamps and durations tile the grid without gaps or overlaps.
    pub fn duration_ns(&self, index: u64) -> u64 {
        self.deadline_ns(index.saturating_add(1))
            .saturating_sub(self.deadline_ns(index))
    }

    /// The shortest tick duration (the period rounded down): the width of
    /// the phase band.
    pub fn period_ns(&self) -> u64 {
        self.deadline_ns(1).max(1)
    }

    /// Index of the last tick due at or before `t_ns`: the largest `i` with
    /// `deadline_ns(i) <= t_ns` (consistent with its rounding).
    pub fn index_at(&self, t_ns: u64) -> u64 {
        let i = ((u128::from(t_ns) + 1) * u128::from(self.fps_num) - 1)
            / (1_000_000_000 * u128::from(self.fps_den));
        u64::try_from(i).unwrap_or(u64::MAX)
    }
}

/// What a tick emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TickKind {
    /// A frame not emitted before.
    Fresh,
    /// The last emitted frame, again.
    Repeat,
    /// The black placeholder frame.
    Placeholder,
}

/// What the clock thread should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing is due before `until_ns`.
    Wait { until_ns: u64 },
    /// Emit tick `index`, timestamped `pts_ns` (its deadline).
    Tick { index: u64, pts_ns: u64 },
}

/// The slot's newest entry, as the pacer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Latest {
    pub seq: u64,
    /// When it was written into the slot, on the pacer's time base.
    pub arrived_ns: u64,
    /// The source that wrote it.
    pub source_id: u64,
}

/// Running totals of the pacer's decisions, and the phase lock's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct PacerCounts {
    pub fresh: u64,
    pub repeat: u64,
    pub placeholder: u64,
    /// Input frames replaced in the slot before any tick emitted them.
    pub superseded: u64,
    /// Ticks emitted after the next tick's emission instant had already
    /// passed.
    pub late: u64,
    /// Ticks never emitted because the schedule was resynchronised.
    pub skipped: u64,
    /// Phase jumps: the first lock on an input run, and re-acquisitions
    /// after the arrivals moved by more than the jump threshold.
    pub lock_acquisitions: u64,
    /// Phase wraps at an edge of the band.
    pub lock_wraps: u64,
    /// The current phase offset (`[-period, 0]`): ticks are emitted this
    /// long before (negative) their timestamps.
    pub phase_offset_ns: i64,
    /// The lock is acquired and engaged (the input runs at about the
    /// output rate, a multiple of it or half of it, within the drift the
    /// slew can follow).
    pub phase_locked: bool,
    /// Holds armed ([`Pacer::hold`]).
    pub holds: u64,
    /// Holds ended by the awaited source's first fresh frame.
    pub holds_released: u64,
    /// Holds ended by their deadline.
    pub holds_expired: u64,
}

impl PacerCounts {
    /// Ticks emitted (fresh + repeat + placeholder).
    pub fn ticks(&self) -> u64 {
        self.fresh + self.repeat + self.placeholder
    }
}

/// One fresh tick's wait, kept by the phase lock.
#[derive(Debug, Clone, Copy)]
struct Sample {
    seq: u64,
    arrived_ns: u64,
    /// The wait minus the phase corrections made up to it: the wait at
    /// the current phase is `q + corrections`.
    q: i64,
}

/// The phase lock's state.
#[derive(Debug, Clone, Default)]
struct PhaseLock {
    /// The phase offset, in `[-period, 0]`.
    phase: i64,
    /// Every correction applied, without the wraps.
    corrections: i64,
    /// Acquired on the current run.
    locked: bool,
    /// The lock can follow the input (see the module docs).
    engaged: bool,
    /// The source of the current run.
    source: Option<u64>,
    samples: VecDeque<Sample>,
    /// `(index, seq, arrived_ns)` of the run's last [`RATE_WINDOW`] fresh
    /// frames: the input's frame interval, for the drift gate. The index
    /// counts the input's frames: the sequence number, except that a
    /// sub-rate input's arrival gap of two frame intervals (a frame the
    /// camera dropped; a longer gap ends the run) counts as two frames.
    arrivals: VecDeque<(u64, u64, u64)>,
    /// The drift gate let go of the run with a full window of arrivals:
    /// it lets the lock engage again only within half its limit.
    let_go: bool,
}

impl PhaseLock {
    /// A new input run: the kept waits describe another input.
    fn break_run(&mut self) {
        self.locked = false;
        self.engaged = false;
        self.source = None;
        self.samples.clear();
        self.arrivals.clear();
        self.let_go = false;
    }

    /// Input frame interval over the kept frames of the run.
    fn input_interval(&self) -> Option<u64> {
        let (a, b) = (self.samples.front()?, self.samples.back()?);
        let frames = b.seq.checked_sub(a.seq).filter(|n| *n > 0)?;
        Some(b.arrived_ns.saturating_sub(a.arrived_ns) / frames)
    }

    /// Whether the slew can follow the input's frame train: the train the
    /// ticks catch (every `k`-th frame, `k x interval` nearest a period)
    /// moves against them by `period - k x interval` per tick, and that
    /// drift must be within a limit per tick: the slew for a train that
    /// moves earlier (an input faster than `k` x the contract), half the
    /// slew for one that moves later (a slower input), whose frames a
    /// tracking lag pushes past their ticks. The interval is the slope of a
    /// least-squares fit of arrival time on sequence number over the kept
    /// arrivals, so arrival jitter and a frame off its cadence barely move
    /// it. A drift beyond the limit by less than [`DRIFT_STANDARD_ERRORS`]
    /// of its standard error (from the fit's residuals) counts as
    /// followable: early in a run the estimate is uncertain and the lock
    /// acquires and tracks as soon as it has a few frames; it lets go once
    /// the drift is known to be out of reach. Let go with a full window
    /// ([`RATE_WINDOW`] arrivals), the gate takes the lock back only within
    /// half the limit, so an estimate near the limit cannot switch it on
    /// and off with every frame.
    ///
    /// A **sub-rate** input (`sub_rate`: the coarse interval is over its
    /// limit) is followed only at an interval of [`SUB_RATE_RATIO`] periods
    /// (15 fps against 30 fps), only once a full window of arrivals
    /// measures it, and only while its cadence is steady (the median
    /// distance of an arrival from the fitted line at most a
    /// [`SUB_RATE_JITTER_DIVISOR`]th of a period). Each frame then has a
    /// tick of its own, and the frame train moves against the ticks by
    /// `2 x period - interval` per frame (per fresh tick), against the same
    /// limits.
    fn drift_followable(&mut self, period: u64, slew: u64, sub_rate: bool) -> bool {
        let n = self.arrivals.len();
        if sub_rate && n < RATE_WINDOW {
            return false;
        }
        let Some(&(index0, _, t0)) = self.arrivals.front() else {
            return true;
        };
        if n < 3 {
            return true;
        }
        let pts: Vec<(f64, f64)> = self
            .arrivals
            .iter()
            .map(|&(index, _, t)| ((index - index0) as f64, t as f64 - t0 as f64))
            .collect();
        let nf = n as f64;
        let mx = pts.iter().map(|p| p.0).sum::<f64>() / nf;
        let my = pts.iter().map(|p| p.1).sum::<f64>() / nf;
        let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        if sxx <= 0.0 {
            return !sub_rate;
        }
        let interval = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / sxx;
        if interval <= 0.0 {
            return false;
        }
        let residual = |p: &(f64, f64)| p.1 - my - interval * (p.0 - mx);
        let residuals: f64 = pts.iter().map(|p| residual(p).powi(2)).sum();
        let standard_error = (residuals / (nf - 2.0) / sxx).sqrt();
        let period = period as f64;
        let (k, drift) = if sub_rate {
            let mut abs: Vec<f64> = pts.iter().map(|p| residual(p).abs()).collect();
            abs.sort_unstable_by(f64::total_cmp);
            if abs[n / 2] > period / SUB_RATE_JITTER_DIVISOR {
                return false;
            }
            (1.0, SUB_RATE_RATIO * period - interval)
        } else {
            let k = (period / interval).round().max(1.0);
            (k, period - k * interval)
        };
        let cap = if drift < 0.0 {
            slew as f64 / 2.0
        } else {
            slew as f64
        };
        let limit = if self.let_go { cap / 2.0 } else { cap };
        let followable = drift.abs() <= limit + DRIFT_STANDARD_ERRORS * k * standard_error;
        if followable {
            self.let_go = false;
        } else if n >= RATE_WINDOW {
            self.let_go = true;
        }
        followable
    }
}

/// Fresh frames of a run whose arrivals estimate the input's frame
/// interval for the drift gate (about 2 s at 30 fps).
const RATE_WINDOW: usize = 64;

/// How many standard errors the drift estimate may exceed the slew by and
/// still count as followable (see [`PhaseLock::drift_followable`]).
const DRIFT_STANDARD_ERRORS: f64 = 3.0;

/// The input frame interval, in output periods, at which a sub-rate input
/// is followed (see [`PhaseLock::drift_followable`]): 15 fps against
/// 30 fps, 30 fps against 60 fps. Every frame then arrives at the same
/// phase of the ticks and leaves on the next one; the repeat goes out on
/// the tick in between. At other ratios the frames fall on several phases
/// of the ticks (two, half a period apart, at 20 fps against 30 fps), and
/// arrivals that step by a few milliseconds at once, as on cameras that
/// deliver on a coarse host-side grid, make the frames of the phase the
/// lock is steered to miss their ticks; those inputs are not followed.
const SUB_RATE_RATIO: f64 = 2.0;

/// A sub-rate input is followed only while the median distance of its
/// arrivals from their fitted line is at most this fraction of a period
/// (4.2 ms at 30 fps): a steady cadence with jitter, not an irregular one.
const SUB_RATE_JITTER_DIVISOR: f64 = 8.0;

/// The smallest early-edge wrap hysteresis
/// ([`PhaseLockConfig::wrap_hysteresis_ns`]) for a sub-rate input, at
/// least the 4 ms grid on which the reference cameras deliver their
/// frames. With [`PhaseLockConfig::DEFAULT_WRAP_HYSTERESIS_NS`] a phase
/// wrapped past the early edge lands within a millisecond of the late
/// edge, where the next arrival a grid step later wraps it back: two
/// wraps, and two fresh or repeat ticks in a row, for no drift. In
/// simulation (15 fps cameras at 14.98 to 15.03 fps on a 4 ms arrival
/// grid) the wraps fell to the band crossings of the drift from 3 ms on;
/// it is under [`PhaseLockConfig::DEFAULT_JUMP_NS`], so the waits it lets
/// grow at the early edge never trigger a jump.
const SUB_RATE_WRAP_HYSTERESIS_NS: u64 = 4_000_000;

/// An arrival gap longer than this many half periods (2.5 periods; half
/// input frame intervals for a sub-rate input) ends the input run: the
/// source stalled, and its frames may come back at another phase.
const STALL_HALF_PERIODS: u128 = 5;

/// The phase wrap: the multiple of the input interval nearest half a
/// period (at least one interval, at most one period); one period when the
/// input interval is unknown or longer than a period.
fn wrap_ns(period: u64, interval: Option<u64>) -> u64 {
    match interval {
        Some(i) if i > 0 && i <= period => {
            let k = ((period + i) / (2 * i)).max(1);
            (k * i).min(period)
        }
        _ => period,
    }
}

/// A hold armed by a source switch.
#[derive(Debug, Clone, Copy)]
struct Hold {
    source_id: u64,
    until_ns: u64,
}

/// The output clock's decisions. See the module docs.
#[derive(Debug, Clone)]
pub struct Pacer {
    cfg: PacerConfig,
    /// Index of the next tick to emit.
    next: u64,
    /// Sequence number of the last emitted frame (0: none yet).
    last_seq: u64,
    /// What the last tick emitted.
    last_kind: Option<TickKind>,
    counts: PacerCounts,
    lock: PhaseLock,
    hold: Option<Hold>,
}

impl Pacer {
    pub fn new(cfg: PacerConfig) -> Self {
        Self::resume(cfg, 0, PacerCounts::default())
    }

    pub fn config(&self) -> &PacerConfig {
        &self.cfg
    }

    pub fn counts(&self) -> PacerCounts {
        PacerCounts {
            phase_offset_ns: self.lock.phase,
            phase_locked: self.lock.locked && self.lock.engaged,
            ..self.counts
        }
    }

    /// Index of the next tick to emit.
    pub fn next_index(&self) -> u64 {
        self.next
    }

    /// The current phase offset (see [`PacerCounts::phase_offset_ns`]).
    pub fn phase_offset_ns(&self) -> i64 {
        self.lock.phase
    }

    /// When tick `index` is emitted at the current phase: its deadline plus
    /// the (non-positive) phase offset, not before 0.
    pub fn scheduled_ns(&self, index: u64) -> u64 {
        self.cfg
            .deadline_ns(index)
            .saturating_sub(self.lock.phase.unsigned_abs())
    }

    /// Whether a hold is armed.
    pub fn holding(&self) -> bool {
        self.hold.is_some()
    }

    /// A pacer continuing an earlier run of the same output: the frame of
    /// sequence `last_seq` and every earlier one count as already handled
    /// (emitted or superseded), and the counts carry on from `counts`. The
    /// grid starts again at tick 0, at phase 0, unlocked.
    pub fn resume(cfg: PacerConfig, last_seq: u64, counts: PacerCounts) -> Self {
        Pacer {
            cfg,
            next: 0,
            last_seq,
            last_kind: None,
            counts: PacerCounts {
                phase_offset_ns: 0,
                phase_locked: false,
                ..counts
            },
            lock: PhaseLock::default(),
            hold: None,
        }
    }

    /// Advances the schedule to `now_ns`: the next tick to emit (and the
    /// schedule moves past it), or how long to wait for it. Resynchronises
    /// after a long stall or when more than [`PacerConfig::max_burst`]
    /// ticks are due (see the module docs).
    pub fn step(&mut self, now_ns: u64) -> Step {
        let due = self.scheduled_ns(self.next);
        if now_ns < due {
            return Step::Wait { until_ns: due };
        }
        let lead = self.lock.phase.unsigned_abs();
        let current = self
            .cfg
            .index_at(now_ns.saturating_add(lead))
            .max(self.next);
        let due_now = current - self.next + 1;
        if now_ns - due >= self.cfg.resync_after_ns || due_now > self.cfg.max_burst.max(1) {
            self.counts.skipped += current - self.next;
            self.next = current;
        } else if due_now > 1 {
            self.counts.late += 1;
        }
        let index = self.next;
        self.next += 1;
        Step::Tick {
            index,
            pts_ns: self.cfg.deadline_ns(index),
        }
    }

    /// Arms a hold until `until_ns`, awaiting the first fresh frame of
    /// `source_id` (see the module docs). Replaces a hold already armed.
    /// The input run ends: the next source is locked anew.
    pub fn hold(&mut self, source_id: u64, until_ns: u64) {
        self.hold = Some(Hold {
            source_id,
            until_ns,
        });
        self.counts.holds += 1;
        self.lock.break_run();
    }

    /// Decides the content of the tick being emitted at `now_ns`, given the
    /// slot's newest entry (`None`: the slot is empty), records it, and
    /// updates the phase lock.
    pub fn decide(&mut self, latest: Option<Latest>, now_ns: u64) -> TickKind {
        if self.hold.is_some_and(|h| now_ns >= h.until_ns) {
            self.hold = None;
            self.counts.holds_expired += 1;
        }
        let held = self.hold.is_some()
            && matches!(self.last_kind, Some(TickKind::Fresh | TickKind::Repeat));
        let kind = match latest {
            Some(l) if l.seq > self.last_seq => {
                self.counts.superseded += l.seq - self.last_seq - 1;
                self.last_seq = l.seq;
                TickKind::Fresh
            }
            Some(l)
                if l.seq == self.last_seq
                    && (held
                        || now_ns.saturating_sub(l.arrived_ns) < self.cfg.gap_threshold_ns) =>
            {
                TickKind::Repeat
            }
            _ => TickKind::Placeholder,
        };
        match kind {
            TickKind::Fresh => {
                self.counts.fresh += 1;
                if let Some(l) = latest {
                    if self.hold.is_some_and(|h| h.source_id == l.source_id) {
                        self.hold = None;
                        self.counts.holds_released += 1;
                    }
                    // The previous source's last frames during a hold do
                    // not describe the next source.
                    if self.hold.is_none() {
                        self.track(l, now_ns);
                    }
                }
            }
            TickKind::Repeat => self.counts.repeat += 1,
            TickKind::Placeholder => {
                self.counts.placeholder += 1;
                self.lock.break_run();
            }
        }
        self.last_kind = Some(kind);
        kind
    }

    /// The phase lock's update for a fresh frame `l` emitted at `now_ns`.
    fn track(&mut self, l: Latest, now_ns: u64) {
        let lc = self.cfg.lock;
        if !lc.enabled {
            return;
        }
        let period = i64::try_from(self.cfg.period_ns()).unwrap_or(i64::MAX);
        let st = &mut self.lock;
        let is_sub_rate = |interval: Option<u64>| {
            interval.filter(|i| {
                u128::from(*i) * 1000
                    > u128::from(self.cfg.period_ns()) * u128::from(lc.max_input_interval_permille)
            })
        };
        // A new source, or the same one after a stall (an arrival gap over
        // `STALL_HALF_PERIODS` half periods, or half input intervals for a
        // sub-rate input): a new run, acquired anew.
        let slow_interval = is_sub_rate(st.input_interval());
        let stall_unit = slow_interval.unwrap_or(self.cfg.period_ns());
        let stalled = st.samples.back().is_some_and(|s| {
            u128::from(l.arrived_ns.saturating_sub(s.arrived_ns)) * 2
                > u128::from(stall_unit) * STALL_HALF_PERIODS
        });
        if st.source != Some(l.source_id) || stalled {
            st.break_run();
            st.source = Some(l.source_id);
        }
        let wait = i64::try_from(now_ns.saturating_sub(l.arrived_ns)).unwrap_or(i64::MAX);
        st.samples.push_back(Sample {
            seq: l.seq,
            arrived_ns: l.arrived_ns,
            q: wait - st.corrections,
        });
        while st.samples.len() > lc.window.max(2) {
            st.samples.pop_front();
        }
        let index = st.arrivals.back().map_or(0, |&(index, seq, t)| {
            let frames = l.seq.saturating_sub(seq).max(1);
            let elapsed =
                slow_interval.map_or(0, |i| (l.arrived_ns.saturating_sub(t) + i / 2) / i.max(1));
            index + frames.max(elapsed)
        });
        st.arrivals.push_back((index, l.seq, l.arrived_ns));
        while st.arrivals.len() > RATE_WINDOW {
            st.arrivals.pop_front();
        }
        let interval = st.input_interval();
        let sub_rate = is_sub_rate(interval).is_some();
        let engaged = st.samples.len() >= lc.acquire_after.clamp(2, lc.window.max(2))
            && interval.is_some()
            && st.drift_followable(self.cfg.period_ns(), lc.slew_ns, sub_rate);
        st.engaged = engaged;
        if !engaged {
            // Held phase: the output pads a slow input as without the lock.
            // The lock stays acquired: when the input is back at the rate
            // (or the window no longer spans a late frame), tracking resumes
            // from here, and a large error jumps as usual.
            return;
        }
        let ns = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        let min_wait = st.samples.iter().map(|s| s.q).min().unwrap_or(0) + st.corrections;
        let error = min_wait - ns(lc.margin_ns);
        let desired = st.phase - error;
        // A sub-rate input's frames arrive on the camera's coarser steps:
        // a larger hysteresis keeps one step from wrapping the phase there
        // and back.
        let hysteresis = if sub_rate {
            ns(lc.wrap_hysteresis_ns.max(SUB_RATE_WRAP_HYSTERESIS_NS))
        } else {
            ns(lc.wrap_hysteresis_ns)
        };
        let jump = !st.locked || error > ns(lc.jump_ns);
        let target = if jump {
            st.locked = true;
            self.counts.lock_acquisitions += 1;
            desired
        } else {
            let slewed = st.phase - error.clamp(-ns(lc.slew_ns), ns(lc.slew_ns));
            // Past the late edge the waits fall under the margin and frames
            // would start to miss their ticks: wrap at once. Past the early
            // edge the waits only grow: wrap once the excess passes the
            // hysteresis, so jitter cannot wrap back and forth.
            if desired > 0 || desired < -period - hysteresis {
                // Wrap (below), absorbing the error.
                desired
            } else {
                slewed.clamp(-period, 0)
            }
        };
        st.corrections += target - st.phase;
        st.phase = target;
        // Into the band. Part of a jump, this is no wrap of its own.
        let w = ns(wrap_ns(self.cfg.period_ns(), interval));
        let mut wrapped = false;
        while st.phase > 0 {
            st.phase -= w;
            wrapped = true;
        }
        while st.phase < -period {
            st.phase += w;
            wrapped = true;
        }
        if wrapped && !jump {
            self.counts.lock_wraps += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn cfg30() -> PacerConfig {
        PacerConfig::new(30, 1)
    }

    /// The slot's entry `seq`, written at `arrived_ns` by source 1.
    fn at(seq: u64, arrived_ns: u64) -> Option<Latest> {
        Some(Latest {
            seq,
            arrived_ns,
            source_id: 1,
        })
    }

    /// A scripted run: an input producing a frame every `input_period_ns`
    /// from `input_start_ns` until `input_stop_ns` (exclusive), and a clock
    /// thread that wakes exactly at each emission instant plus
    /// `wake_delay(index)`. Returns the emitted `(index, pts, kind)` of the
    /// ticks timestamped before `end_ns`, and the pacer. (Ticks are emitted
    /// up to one period before their timestamps once the phase lock moves
    /// them, so the run ends by timestamp, not by emission time.)
    struct Script {
        cfg: PacerConfig,
        input_start_ns: u64,
        input_period_ns: u64,
        input_stop_ns: u64,
        end_ns: u64,
    }

    impl Script {
        fn run(&self, wake_delay: impl Fn(u64) -> u64) -> (Vec<(u64, u64, TickKind)>, Pacer) {
            let mut pacer = Pacer::new(self.cfg);
            let mut out = Vec::new();
            let mut now = 0;
            // Latest input frame at time t: (seq, arrived).
            let latest_at = |t: u64| -> Option<Latest> {
                if self.input_period_ns == 0 || t < self.input_start_ns {
                    return None;
                }
                let last_t = t.min(self.input_stop_ns.saturating_sub(1));
                if last_t < self.input_start_ns {
                    return None;
                }
                let k = (last_t - self.input_start_ns) / self.input_period_ns;
                at(k + 1, self.input_start_ns + k * self.input_period_ns)
            };
            while now < self.end_ns {
                match pacer.step(now) {
                    Step::Wait { until_ns } => {
                        now = until_ns + wake_delay(pacer.next_index());
                    }
                    Step::Tick { pts_ns, .. } if pts_ns >= self.end_ns => break,
                    Step::Tick { index, pts_ns } => {
                        let kind = pacer.decide(latest_at(now), now);
                        out.push((index, pts_ns, kind));
                    }
                }
            }
            (out, pacer)
        }
    }

    fn kinds(out: &[(u64, u64, TickKind)]) -> Vec<TickKind> {
        out.iter().map(|o| o.2).collect()
    }

    #[test]
    fn deadlines_are_exact_for_fractional_rates() {
        let c = cfg30();
        assert_eq!(c.deadline_ns(0), 0);
        assert_eq!(c.deadline_ns(1), 33_333_333);
        assert_eq!(c.deadline_ns(3), 100_000_000);
        assert_eq!(c.deadline_ns(30), 1_000_000_000);
        assert_eq!(c.index_at(99_999_999), 2);
        assert_eq!(c.index_at(100_000_000), 3);
        let ntsc = PacerConfig::new(30000, 1001);
        assert_eq!(ntsc.deadline_ns(30000), 1001 * 1_000_000_000);
        assert_eq!(ntsc.deadline_ns(1), 33_366_666);
        // A day of ticks still lands on the exact grid.
        let day = 24 * 3600 * 30;
        assert_eq!(c.deadline_ns(day), 24 * 3600 * 1_000_000_000);
        assert_eq!(c.index_at(c.deadline_ns(day)), day);
    }

    #[test]
    fn durations_tile_the_grid_for_fractional_rates() {
        let ntsc = PacerConfig::new(30000, 1001);
        // 33_366_666.67 ns per tick: the durations alternate between the
        // neighbouring integers instead of always rounding down.
        let d: Vec<u64> = (0..3).map(|i| ntsc.duration_ns(i)).collect();
        assert_eq!(d, [33_366_666, 33_366_667, 33_366_667]);
        let mut sum = 0;
        for i in 0..30_000 {
            let dur = ntsc.duration_ns(i);
            assert!((33_366_666..=33_366_667).contains(&dur), "{i}: {dur}");
            assert_eq!(ntsc.deadline_ns(i) + dur, ntsc.deadline_ns(i + 1), "{i}");
            sum += dur;
        }
        // 30000 ticks last exactly 1001 s.
        assert_eq!(sum, 1001 * 1_000_000_000);
        // A whole-nanosecond period stays constant.
        let c = PacerConfig::new(25, 1);
        assert!((0..100).all(|i| c.duration_ns(i) == 40_000_000));
    }

    #[test]
    fn exact_rate_input_is_all_fresh() {
        // Input at 30 fps, arriving 5 ms after each output deadline.
        let s = Script {
            cfg: cfg30(),
            input_start_ns: 5 * MS,
            input_period_ns: cfg30().deadline_ns(1),
            input_stop_ns: u64::MAX,
            end_ns: 2_000 * MS,
        };
        let (out, p) = s.run(|_| 0);
        // Tick 0 is due at 0, before any input.
        assert_eq!(out[0].2, TickKind::Placeholder);
        assert_eq!(out.len(), 60);
        // From then on each tick sees one new frame: due at n*period, the
        // frame of (n-1)*period+5ms is the newest.
        assert!(kinds(&out[1..]).iter().all(|k| *k == TickKind::Fresh));
        let c = p.counts();
        assert_eq!((c.superseded, c.late, c.skipped), (0, 0, 0));
        // PTS is the grid.
        for (i, (idx, pts, _)) in out.iter().enumerate() {
            assert_eq!(*idx, i as u64);
            assert_eq!(*pts, cfg30().deadline_ns(i as u64));
        }
    }

    #[test]
    fn faster_input_is_thinned_and_counted_as_superseded() {
        // 60 fps input into 30 fps output: every other frame is replaced.
        let s = Script {
            cfg: cfg30(),
            input_start_ns: MS,
            input_period_ns: 1_000_000_000 / 60,
            input_stop_ns: u64::MAX,
            end_ns: 1_000 * MS,
        };
        let (out, p) = s.run(|_| 0);
        let c = p.counts();
        assert_eq!(out.len(), 30);
        assert_eq!(c.fresh, 29);
        assert_eq!(c.placeholder, 1);
        assert_eq!(c.repeat, 0);
        // 60 frames arrived before the last tick, about half never emitted.
        assert!((27..=30).contains(&c.superseded), "{c:?}");
    }

    #[test]
    fn slower_input_is_padded_with_repeats() {
        // 15 fps input into 30 fps output: fresh, repeat, fresh, repeat...
        let s = Script {
            cfg: cfg30(),
            input_start_ns: MS,
            input_period_ns: 1_000_000_000 / 15,
            input_stop_ns: u64::MAX,
            end_ns: 2_000 * MS,
        };
        let (out, p) = s.run(|_| 0);
        let c = p.counts();
        assert_eq!(out.len(), 60);
        assert_eq!(c.placeholder, 1);
        assert_eq!(c.fresh, 30);
        assert_eq!(c.repeat, 29);
        assert_eq!(c.superseded, 0);
        let k = kinds(&out[1..7]);
        assert_eq!(
            k,
            [
                TickKind::Fresh,
                TickKind::Repeat,
                TickKind::Fresh,
                TickKind::Repeat,
                TickKind::Fresh,
                TickKind::Repeat
            ]
        );
    }

    #[test]
    fn stalled_input_repeats_until_the_gap_then_placeholder() {
        // 30 fps input that stops at 1 s; gap threshold 500 ms.
        let s = Script {
            cfg: cfg30(),
            input_start_ns: MS,
            input_period_ns: cfg30().deadline_ns(1),
            input_stop_ns: 1_000 * MS,
            end_ns: 3_000 * MS,
        };
        let (out, p) = s.run(|_| 0);
        let last_arrival = MS + 29 * cfg30().deadline_ns(1);
        for &(_, pts, kind) in &out[1..] {
            let want = if pts <= last_arrival + cfg30().deadline_ns(1) {
                TickKind::Fresh
            } else if pts - last_arrival < 500 * MS {
                TickKind::Repeat
            } else {
                TickKind::Placeholder
            };
            assert_eq!(kind, want, "pts {pts}");
        }
        let c = p.counts();
        assert_eq!(c.fresh, 30);
        // 500 ms of repeats at 30 fps, give or take the tick phase.
        assert!((14..=15).contains(&c.repeat), "{c:?}");
        assert_eq!(c.ticks(), 90);
    }

    #[test]
    fn resumed_input_is_fresh_immediately() {
        let mut p = Pacer::new(cfg30());
        let t = |i: u64| cfg30().deadline_ns(i);
        assert!(matches!(p.step(t(0)), Step::Tick { index: 0, .. }));
        assert_eq!(p.decide(at(1, 0), t(0)), TickKind::Fresh);
        // No input for 40 ticks: repeats, then placeholders.
        let mut kinds = Vec::new();
        for i in 1..=40 {
            assert!(matches!(p.step(t(i)), Step::Tick { .. }));
            kinds.push(p.decide(at(1, 0), t(i)));
        }
        assert_eq!(kinds[..14], [TickKind::Repeat; 14]);
        assert_eq!(kinds[14..], [TickKind::Placeholder; 26]);
        // A new source's first frame (sequence continues) is fresh at once.
        assert!(matches!(p.step(t(41)), Step::Tick { .. }));
        assert_eq!(p.decide(at(2, t(41) - MS), t(41)), TickKind::Fresh);
        assert!(matches!(p.step(t(42)), Step::Tick { .. }));
        assert_eq!(p.decide(at(2, t(41) - MS), t(42)), TickKind::Repeat);
    }

    #[test]
    fn empty_slot_emits_placeholders() {
        let s = Script {
            cfg: cfg30(),
            input_start_ns: 0,
            input_period_ns: 0,
            input_stop_ns: 0,
            end_ns: 1_000 * MS,
        };
        let (out, p) = s.run(|_| 0);
        assert_eq!(out.len(), 30);
        assert!(kinds(&out).iter().all(|k| *k == TickKind::Placeholder));
        assert_eq!(p.counts().placeholder, 30);
    }

    #[test]
    fn short_late_wakeups_catch_up_without_losing_ticks() {
        // Every 10th wake-up is 50 ms late (over one period): that tick and
        // the next one are emitted back to back.
        let s = Script {
            cfg: cfg30(),
            input_start_ns: 0,
            input_period_ns: 0,
            input_stop_ns: 0,
            end_ns: 3_000 * MS,
        };
        let (out, p) = s.run(|i| if i % 10 == 5 { 50 * MS } else { 200_000 });
        let c = p.counts();
        assert_eq!(c.skipped, 0);
        assert!(c.late >= 9, "{c:?}");
        // Every grid index is emitted exactly once, in order.
        for (i, (idx, pts, _)) in out.iter().enumerate() {
            assert_eq!(*idx, i as u64);
            assert_eq!(*pts, cfg30().deadline_ns(i as u64));
        }
        assert!(out.len() >= 89, "{}", out.len());
    }

    #[test]
    fn small_jitter_is_not_late() {
        let mut p = Pacer::new(cfg30());
        for i in 0..30 {
            // 5 ms after each deadline: within one period.
            let now = cfg30().deadline_ns(i) + 5 * MS;
            assert!(matches!(p.step(now), Step::Tick { .. }));
            p.decide(None, now);
            assert!(matches!(p.step(now), Step::Wait { .. }));
        }
        assert_eq!(p.counts().late, 0);
    }

    #[test]
    fn long_stall_resynchronises_and_counts_skipped_ticks() {
        let mut p = Pacer::new(cfg30());
        let t = |i: u64| cfg30().deadline_ns(i);
        for i in 0..10 {
            assert_eq!(
                p.step(t(i)),
                Step::Tick {
                    index: i,
                    pts_ns: t(i)
                }
            );
            p.decide(None, t(i));
        }
        // The thread stalls for 2.5 s: next due is tick 10 at 333 ms; now
        // is just past tick 85.
        let now = t(85) + MS;
        assert_eq!(cfg30().index_at(now), 85);
        assert_eq!(
            p.step(now),
            Step::Tick {
                index: 85,
                pts_ns: t(85)
            }
        );
        let c = p.counts();
        assert_eq!(c.skipped, 75);
        assert_eq!(c.late, 0, "a resync is not a late tick");
        // Back on the grid: the next tick waits for its deadline.
        p.decide(None, now);
        assert_eq!(p.step(now), Step::Wait { until_ns: t(86) });
    }

    /// A wake-up up to the burst cap late catches up back to back. (Until
    /// v0.2.0 the cap followed the 1 s resynchronisation threshold, so a
    /// 900 ms stall burst 27 ticks; the cap is now the output queue bound,
    /// 3 by default, and such a stall resynchronises, see below.)
    #[test]
    fn a_stall_within_the_burst_cap_catches_up() {
        let mut p = Pacer::new(cfg30());
        let t = |i: u64| cfg30().deadline_ns(i);
        assert!(matches!(p.step(0), Step::Tick { index: 0, .. }));
        // Woken just after tick 3: ticks 1..=3, the cap, are due now.
        let now = t(3) + MS;
        let mut emitted = Vec::new();
        while let Step::Tick { index, .. } = p.step(now) {
            emitted.push(index);
        }
        assert_eq!(emitted, [1, 2, 3]);
        assert_eq!(p.counts().skipped, 0);
        // All but the last were emitted after their successor was due.
        assert_eq!(p.counts().late, 2);
    }

    /// The burst cap is the output queue bound (3), whatever the rate: a
    /// stall leaving more ticks due resynchronises well before the 1 s
    /// threshold, so the queue never receives a backlog.
    #[test]
    fn the_default_burst_cap_is_the_queue_bound() {
        for (num, den) in [(30, 1), (60, 1), (30000, 1001), (240, 1)] {
            let cfg = PacerConfig::new(num, den);
            assert_eq!(cfg.max_burst, PacerConfig::DEFAULT_MAX_BURST);
            let mut p = Pacer::new(cfg);
            let next = 5;
            for i in 0..next {
                p.step(cfg.deadline_ns(i));
            }
            // Woken with exactly the cap due: caught up, nothing skipped.
            let now = cfg.deadline_ns(next + cfg.max_burst - 1);
            let mut burst = 0;
            while let Step::Tick { .. } = p.step(now) {
                burst += 1;
            }
            assert_eq!(
                (burst, p.counts().skipped),
                (cfg.max_burst, 0),
                "{num}/{den}"
            );
            // Woken with one more due than the cap: resynchronised.
            let next = p.next_index();
            let now = cfg.deadline_ns(next + cfg.max_burst);
            let mut burst = 0;
            while let Step::Tick { .. } = p.step(now) {
                burst += 1;
            }
            assert_eq!(burst, 1, "{num}/{den}");
            assert_eq!(p.counts().skipped, cfg.max_burst, "{num}/{den}");
        }
        assert_eq!(PacerConfig::DEFAULT_MAX_BURST, 3);
    }

    #[test]
    fn a_burst_larger_than_the_cap_resynchronises() {
        let mut cfg = cfg30();
        cfg.max_burst = 4;
        let t = |i: u64| cfg.deadline_ns(i);
        let mut p = Pacer::new(cfg);
        assert!(matches!(p.step(0), Step::Tick { index: 0, .. }));
        // Woken with ticks 1..=4 due: four, the cap, caught up back to back.
        let mut emitted = Vec::new();
        while let Step::Tick { index, .. } = p.step(t(4)) {
            emitted.push(index);
        }
        assert_eq!(emitted, [1, 2, 3, 4]);
        assert_eq!((p.counts().late, p.counts().skipped), (3, 0));
        // Woken with ticks 5..=9 due (five, well under the 1 s threshold):
        // the clock jumps to tick 9 and counts four skipped.
        emitted.clear();
        while let Step::Tick { index, .. } = p.step(t(9) + MS) {
            emitted.push(index);
        }
        assert_eq!(emitted, [9]);
        assert_eq!((p.counts().late, p.counts().skipped), (3, 4));
        // No burst ever exceeded the cap.
        assert_eq!(p.step(t(9) + MS), Step::Wait { until_ns: t(10) });
    }

    #[test]
    fn a_resumed_pacer_carries_on_its_counts_and_sequence() {
        let mut p = Pacer::new(cfg30());
        p.step(0);
        assert_eq!(p.decide(at(3, 0), 0), TickKind::Fresh);
        let before = p.counts();
        assert_eq!(before.superseded, 2);
        // Restarted after frames up to 40 were written: frame 41 is fresh
        // and nothing written before the restart counts as superseded.
        let mut r = Pacer::resume(cfg30(), 40, before);
        assert!(matches!(r.step(0), Step::Tick { index: 0, .. }));
        assert_eq!(r.decide(at(41, 0), 0), TickKind::Fresh);
        let after = r.counts();
        assert_eq!(after.superseded, before.superseded);
        assert_eq!(after.fresh, before.fresh + 1);
        // A frame still in the slot at the restart (40) is a repeat.
        let mut r = Pacer::resume(cfg30(), 40, before);
        r.step(0);
        assert_eq!(r.decide(at(40, 0), 0), TickKind::Repeat);
    }

    #[test]
    fn superseded_counts_from_the_first_frame() {
        let mut p = Pacer::new(cfg30());
        p.step(0);
        // Five frames arrived before the first tick: four never emitted.
        assert_eq!(p.decide(at(5, 0), 0), TickKind::Fresh);
        assert_eq!(p.counts().superseded, 4);
        p.step(cfg30().deadline_ns(1));
        assert_eq!(p.decide(at(6, 0), MS), TickKind::Fresh);
        assert_eq!(p.counts().superseded, 4);
    }

    // ---- Phase lock and hold: simulations on scripted time ----

    /// A small deterministic generator (xorshift64*) for arrival jitter.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// Uniform in `[-j, j]`.
        fn jitter(&mut self, j: u64) -> i64 {
            if j == 0 {
                return 0;
            }
            (self.next() % (2 * j + 1)) as i64 - j as i64
        }
    }

    /// Arrivals of one source: a frame every `1/fps` s from `start_ns` for
    /// `duration_ns`, each moved by up to `jitter_ns` either way.
    fn train(
        source: u64,
        start_ns: u64,
        fps: f64,
        duration_ns: u64,
        jitter_ns: u64,
        seed: u64,
    ) -> Vec<(u64, u64)> {
        train_tailed(source, start_ns, fps, duration_ns, jitter_ns, (0, 0), seed)
    }

    /// As [`train`], and `tail.0` frames in a thousand arrive later by up
    /// to `tail.1` more (a skewed delay, like a CPU decode that sometimes
    /// takes longer).
    fn train_tailed(
        source: u64,
        start_ns: u64,
        fps: f64,
        duration_ns: u64,
        jitter_ns: u64,
        tail: (u64, u64),
        seed: u64,
    ) -> Vec<(u64, u64)> {
        let mut rng = Rng(seed | 1);
        let period = 1e9 / fps;
        let mut out: Vec<(u64, u64)> = (0u64..)
            .map(|k| start_ns as f64 + k as f64 * period)
            .take_while(|t| *t < (start_ns + duration_ns) as f64)
            .map(|t| {
                let mut t = (t as i64 + rng.jitter(jitter_ns)).max(0) as u64;
                if tail.0 > 0 && rng.next() % 1000 < tail.0 {
                    t += rng.next() % (tail.1 + 1);
                }
                (t, source)
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// One emitted tick of a simulation.
    #[derive(Debug, Clone, Copy)]
    struct Emit {
        index: u64,
        pts: u64,
        /// When it was emitted.
        at: u64,
        kind: TickKind,
        /// Fresh: the frame's wait in the slot (`at` - its arrival).
        wait: Option<u64>,
        /// The source of the frame emitted (not for a placeholder).
        source: Option<u64>,
        /// Lock acquisitions so far, after this tick.
        acquisitions: u64,
        /// The lock is acquired and engaged after this tick.
        locked: bool,
    }

    /// A simulated output clock fed by `arrivals` (`(time, source)`), with
    /// holds armed at given times (`(at, source, until)`), until the ticks
    /// timestamped before `end_ns` are emitted. The clock wakes exactly at
    /// each emission instant.
    struct Sim {
        cfg: PacerConfig,
        arrivals: Vec<(u64, u64)>,
        holds: Vec<(u64, u64, u64)>,
        end_ns: u64,
    }

    impl Sim {
        fn new(arrivals: Vec<(u64, u64)>, end_ns: u64) -> Self {
            let mut arrivals = arrivals;
            arrivals.sort_unstable();
            Sim {
                cfg: cfg30(),
                arrivals,
                holds: Vec::new(),
                end_ns,
            }
        }

        fn latest_at(&self, t: u64) -> Option<Latest> {
            let n = self.arrivals.partition_point(|(a, _)| *a <= t);
            (n > 0).then(|| {
                let (arrived_ns, source_id) = self.arrivals[n - 1];
                Latest {
                    seq: n as u64,
                    arrived_ns,
                    source_id,
                }
            })
        }

        fn run(&self) -> (Vec<Emit>, Pacer) {
            let mut pacer = Pacer::new(self.cfg);
            let mut holds = self.holds.clone();
            holds.sort_unstable();
            let mut holds = holds.into_iter().peekable();
            let mut out = Vec::new();
            let mut now = 0;
            loop {
                match pacer.step(now) {
                    Step::Wait { until_ns } => now = until_ns,
                    Step::Tick { pts_ns, .. } if pts_ns >= self.end_ns => break,
                    Step::Tick { index, pts_ns } => {
                        while let Some(&(_, source, until)) = holds.peek().filter(|h| h.0 <= now) {
                            pacer.hold(source, until);
                            holds.next();
                        }
                        let latest = self.latest_at(now);
                        let kind = pacer.decide(latest, now);
                        out.push(Emit {
                            index,
                            pts: pts_ns,
                            at: now,
                            kind,
                            wait: latest
                                .filter(|_| kind == TickKind::Fresh)
                                .map(|l| now - l.arrived_ns),
                            source: latest
                                .filter(|_| kind != TickKind::Placeholder)
                                .map(|l| l.source_id),
                            acquisitions: pacer.counts().lock_acquisitions,
                            locked: pacer.counts().phase_locked,
                        });
                    }
                }
            }
            (out, pacer)
        }
    }

    const M: u64 = PhaseLockConfig::DEFAULT_MARGIN_NS;

    /// Every index once, in order, timestamped on the grid; every emission
    /// within `[-period, 0]` of its timestamp.
    fn assert_grid(out: &[Emit], cfg: &PacerConfig) {
        for (i, e) in out.iter().enumerate() {
            assert_eq!(e.index, i as u64);
            assert_eq!(e.pts, cfg.deadline_ns(e.index));
            assert!(
                e.at <= e.pts && e.pts - e.at <= cfg.period_ns(),
                "tick {} emitted at {} for pts {}",
                e.index,
                e.at,
                e.pts
            );
        }
    }

    /// Emissions in every 1 s wall-clock window `[t, t + 1 s)` within
    /// `[from, to)`: the fewest and the most, over windows starting at
    /// every emission and just after every emission (where the extremes
    /// are).
    fn sliding_1s(at: &[u64], from: u64, to: u64) -> (usize, usize) {
        let s = 1_000_000_000;
        let count =
            |a: u64, b: u64| at.partition_point(|t| *t < b) - at.partition_point(|t| *t < a);
        let (mut lo, mut hi) = (usize::MAX, 0);
        for &t in at.iter().filter(|t| **t >= from && **t + s <= to) {
            hi = hi.max(count(t, t + s));
            lo = lo.min(count(t + 1, t + 1 + s));
        }
        (lo, hi)
    }

    fn p50(mut v: Vec<u64>) -> u64 {
        v.sort_unstable();
        v[v.len() / 2]
    }

    #[test]
    fn phase_lock_60_to_30_settles_at_the_margin_for_every_input_phase() {
        let period = cfg30().period_ns();
        let input = 1_000_000_000 / 60;
        // Input phases against the output grid, near both ends of the
        // grid period and of the input period included.
        for phase in [
            0,
            1,
            MS / 2,
            M,
            M + 1,
            5 * MS,
            input - M,
            input - 1,
            input,
            input + 1,
            20 * MS,
            period - M,
            period - 1,
            period,
        ] {
            let sim = Sim::new(train(1, phase, 60.0, 5_000 * MS, 0, 1), 5_000 * MS);
            let (out, p) = sim.run();
            assert_grid(&out, &sim.cfg);
            let c = p.counts();
            assert_eq!(c.lock_acquisitions, 1, "phase {phase}: {c:?}");
            assert!(c.phase_locked, "phase {phase}");
            assert_eq!(
                (c.skipped, c.late, c.lock_wraps),
                (0, 0, 0),
                "phase {phase}"
            );
            let locked_at = out.iter().position(|e| e.acquisitions > 0).unwrap();
            assert!(locked_at < 6, "phase {phase}: acquired at tick {locked_at}");
            let after = &out[locked_at + 1..];
            assert!(
                after.iter().all(|e| e.kind == TickKind::Fresh),
                "phase {phase}"
            );
            for e in after {
                let w = e.wait.unwrap();
                assert!(
                    w <= M + MS / 2,
                    "phase {phase}: tick {} waited {w}",
                    e.index
                );
            }
        }
    }

    /// Ten simulated minutes per camera, with clock drift and +-1 ms of
    /// arrival jitter: the output count, every wall-clock second, the
    /// timestamps and the waits.
    #[test]
    fn phase_lock_follows_drift_and_jitter_for_ten_minutes() {
        let t_s = 600u64;
        let end = t_s * 1_000_000_000;
        for (fps, seed) in [
            (59.99, 3),
            (59.993, 5),
            (60.01, 7),
            (29.97, 11),
            (30.03, 13),
            (30.0, 17),
        ] {
            let sim = Sim::new(
                train(1, 7 * MS, fps, end + 100 * MS, MS, seed),
                end + 100 * MS,
            );
            let (out, p) = sim.run();
            let c = p.counts();
            assert_grid(&out, &sim.cfg);
            assert_eq!(
                (c.skipped, c.late, c.placeholder),
                (0, 0, 1),
                "{fps}: {c:?}"
            );
            let at: Vec<u64> = out.iter().map(|e| e.at).collect();
            let emitted = at.iter().filter(|t| **t < end).count() as i64;
            assert!(
                (emitted - 30 * t_s as i64).abs() <= 1,
                "{fps}: {emitted} emitted"
            );
            let (lo, hi) = sliding_1s(&at, 0, end);
            assert!(lo >= 29 && hi <= 31, "{fps}: 1 s windows {lo}..={hi}");
            // Waits once locked (after the first second).
            let waits: Vec<u64> = out
                .iter()
                .filter(|e| e.at > 1_000 * MS)
                .filter_map(|e| e.wait)
                .collect();
            let median = p50(waits.clone());
            eprintln!(
                "{fps} fps: emitted {emitted}, 1 s windows {lo}..={hi}, wait p50 {:.2} ms max \
                 {:.2} ms, fresh {} repeat {} superseded {}, acquisitions {} wraps {}",
                median as f64 / 1e6,
                *waits.iter().max().unwrap() as f64 / 1e6,
                c.fresh,
                c.repeat,
                c.superseded,
                c.lock_acquisitions,
                c.lock_wraps
            );
            assert!(median <= M + MS, "{fps}: p50 wait {median}");
            // The camera's surplus or shortfall shows as superseded frames
            // or repeats, about what the rate difference makes.
            let expected =
                ((fps - if fps > 45.0 { 60.0 } else { 30.0 }).abs() * t_s as f64).ceil() as u64;
            if fps < 30.0 {
                assert!(c.repeat <= expected + 3, "{fps}: {c:?}");
            }
            if fps > 45.0 && fps < 60.0 {
                assert!(
                    c.repeat <= 3,
                    "{fps}: a 60 fps camera needs no repeat: {c:?}"
                );
            }
            assert!(c.lock_acquisitions <= 3, "{fps}: {c:?}");
        }
    }

    /// Frames that arrive far off their cadence now and then (as measured
    /// on a CPU-decode camera: about one in fifty dequeued 19 ms early)
    /// stretch the measured input interval past the engagement limit for a
    /// moment. The lock holds its phase meanwhile and resumes tracking: one
    /// acquisition, no series of jumps, no missed frames.
    #[test]
    fn phase_lock_rides_out_frames_off_their_cadence() {
        let end = 60_000 * MS;
        let mut arrivals = train(1, 9 * MS, 29.99, end, MS, 41);
        for (i, a) in arrivals.iter_mut().enumerate() {
            if i % 50 == 25 {
                a.0 -= 19 * MS;
            }
        }
        let sim = Sim::new(arrivals, end);
        let (out, p) = sim.run();
        let c = p.counts();
        assert_grid(&out, &sim.cfg);
        assert_eq!(c.lock_acquisitions, 1, "{c:?}");
        // A 29.99 fps camera falls one frame short every 100 s: no repeat
        // beyond that in a minute.
        assert!(c.repeat <= 1, "{c:?}");
        let waits: Vec<u64> = out
            .iter()
            .filter(|e| e.at > 1_000 * MS)
            .filter_map(|e| e.wait)
            .collect();
        assert!(p50(waits) <= M + MS, "{c:?}");
    }

    /// Slower inputs other than a steady 15 fps (against 30 fps): 20 and
    /// 24 fps, 14.97 fps (its frames drift later by 0.13 ms a frame, more
    /// than half the slew), an irregular ~20 fps (40 to 60 ms between
    /// frames) and an irregular ~15 fps (55 to 78 ms). The lock never
    /// engages, and the output is exactly the free-running clock's.
    #[test]
    fn phase_lock_is_not_engaged_for_slower_input_off_its_ratio() {
        let irregular = |seed: u64, from: u64, span: u64| {
            let mut rng = Rng(seed);
            let mut out = Vec::new();
            let mut t = 3 * MS;
            while t < 20_000 * MS {
                out.push((t, 1));
                t += from + rng.next() % span;
            }
            out
        };
        for (name, arrivals) in [
            ("20 fps", train(1, 3 * MS, 20.0, 20_000 * MS, MS, 21)),
            ("19.98 fps", train(1, 3 * MS, 19.98, 20_000 * MS, MS, 23)),
            ("24 fps", train(1, 3 * MS, 24.0, 20_000 * MS, MS, 25)),
            ("14.97 fps", train(1, 3 * MS, 14.97, 20_000 * MS, MS, 27)),
            ("~20 fps", irregular(99, 40 * MS, 20 * MS)),
            ("~15 fps", irregular(97, 55 * MS, 23 * MS)),
        ] {
            let locked = Sim::new(arrivals.clone(), 20_000 * MS);
            let mut free = Sim::new(arrivals, 20_000 * MS);
            free.cfg.lock = PhaseLockConfig::disabled();
            let (a, pa) = locked.run();
            let (b, _) = free.run();
            assert_eq!(pa.counts().lock_acquisitions, 0, "{name}");
            assert_eq!(pa.counts().phase_offset_ns, 0, "{name}");
            // Exactly the ticks, instants and content of the free-running
            // clock: repeats pad the input as before.
            let key = |e: &Emit| (e.index, e.at, e.kind);
            assert_eq!(
                a.iter().map(key).collect::<Vec<_>>(),
                b.iter().map(key).collect::<Vec<_>>(),
                "{name}"
            );
            assert!(pa.counts().repeat > 100, "{name}: {:?}", pa.counts());
        }
    }

    /// Median wait of the fresh frames per wall-clock second from `from_ns`
    /// to the last whole second before `end_ns`.
    fn median_wait_per_second(out: &[Emit], from_ns: u64, end_ns: u64) -> Vec<f64> {
        let s = 1_000_000_000;
        (from_ns / s..end_ns / s - 1)
            .filter_map(|sec| {
                let w: Vec<u64> = out
                    .iter()
                    .filter(|e| e.at / s == sec)
                    .filter_map(|e| e.wait)
                    .collect();
                (!w.is_empty()).then(|| p50(w) as f64)
            })
            .collect()
    }

    fn spread(v: &[f64]) -> f64 {
        v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
    }

    /// A camera at half the contract rate (15 fps against 30 fps, the rate
    /// a camera falls to in dim light; and 30 fps against 60 fps), with
    /// clock drift either way, uniform arrival jitter up to 2 ms, arrivals
    /// on a 4 ms grid (as measured on two UVC cameras), and a dropped
    /// frame every 400. Once a full window of arrivals measures the
    /// cadence (about 4 s), the lock engages for good: every fresh frame
    /// leaves on the next tick about the margin after its arrival, the
    /// repeat on the tick in between, and the median wait per second stays
    /// within a few milliseconds instead of sweeping the whole period as
    /// with the free-running clock. The phase wraps only as often as the
    /// drift carries it across its band, not back and forth at an edge
    /// where the arrivals step by the grid. The output contract is unchanged:
    /// timestamps on the grid, every index once, 30 +- 1 frames in every
    /// wall-clock second, nothing skipped, late or superseded.
    #[test]
    fn phase_lock_follows_an_input_at_half_the_rate() {
        let t_s = 120u64;
        let end = t_s * 1_000_000_000;
        // (camera fps, output fps, arrival jitter, arrival grid step, a
        // dropped frame every so many, first arrival, seed). The starts and
        // seeds of the last three put arrivals a grid step apart at a band
        // edge: with the 1 ms hysteresis of the contract-rate lock they
        // wrapped 145, 7 and 15 times in two minutes, for no, 2 and 2.4
        // crossings of the band.
        for (fps, out_fps, jitter, grid, drop_every, start, seed) in [
            (15.0, 30, MS, 0, 0, 7 * MS, 303),
            (14.99, 30, MS / 2, 0, 0, 7 * MS, 305),
            (14.99, 30, 2 * MS, 0, 0, 7 * MS, 307),
            (14.985, 30, MS, 0, 0, 7 * MS, 309),
            (15.02, 30, MS, 0, 0, 7 * MS, 311),
            (15.02, 30, 2 * MS, 0, 0, 7 * MS, 313),
            (14.99, 30, MS / 2, 4 * MS, 0, 7 * MS, 315),
            (15.02, 30, MS / 2, 4 * MS, 0, 7 * MS, 317),
            (14.99, 30, MS, 0, 400, 7 * MS, 319),
            (29.99, 60, MS / 2, 0, 0, 7 * MS, 321),
            (15.0, 30, MS / 2, 4 * MS, 0, 32 * MS, 55_434),
            (15.01, 30, MS / 2, 4 * MS, 0, 10 * MS, 47_515),
            (14.99, 30, MS, 4 * MS, 0, 28 * MS, 55_434),
        ] {
            let mut arrivals = train(1, start, fps, end + 100 * MS, jitter, seed);
            if let Some(step) = std::num::NonZeroU64::new(grid) {
                for a in &mut arrivals {
                    a.0 = a.0 / step * step.get();
                }
            }
            if drop_every > 0 {
                arrivals = arrivals
                    .into_iter()
                    .enumerate()
                    .filter(|(i, _)| i % drop_every != drop_every / 2)
                    .map(|(_, a)| a)
                    .collect();
            }
            let mut sim = Sim::new(arrivals.clone(), end);
            sim.cfg = PacerConfig::new(out_fps, 1);
            let (out, p) = sim.run();
            let c = p.counts();
            let tag = format!("{fps} fps into {out_fps}, jitter {jitter}, grid {grid}: {c:?}");
            let period = sim.cfg.period_ns();
            assert_grid(&out, &sim.cfg);
            assert_eq!((c.skipped, c.late, c.placeholder), (0, 0, 1), "{tag}");
            assert_eq!(c.superseded, 0, "{tag}");
            let at: Vec<u64> = out.iter().map(|e| e.at).collect();
            let emitted = at.iter().filter(|t| **t < end).count() as i64;
            assert!(
                (emitted - i64::from(out_fps) * t_s as i64).abs() <= 1,
                "{tag}: {emitted}"
            );
            // The run records the ticks timestamped before `end`, emitted
            // up to a period earlier: the windows end a period before it.
            let (lo, hi) = sliding_1s(&at, 0, end - period);
            let rate = out_fps as usize;
            assert!(lo + 1 >= rate && hi <= rate + 1, "{tag}: {lo}..={hi}");
            // Engaged within about a window of arrivals, for good: a
            // dropped frame (an arrival gap of two frame intervals) counts
            // as two frames and neither ends the run nor upsets the fit.
            let first = out.iter().position(|e| e.locked).expect(&tag);
            assert!(
                out[first].at < 5_000 * MS,
                "{tag}: locked at {}",
                out[first].at
            );
            assert!(out[first..].iter().all(|e| e.locked), "{tag}");
            assert_eq!(c.lock_acquisitions, 1, "{tag}");
            let drops = arrivals.len().checked_div(drop_every).map_or(0, |n| n + 1);
            // Every fresh frame leaves on its next tick, about the margin
            // after its arrival. With 2 ms of jitter (or 1 ms on the 4 ms
            // grid) and drift a few miss it and leave a period later, at the
            // rate of the lock at the contract rate with the same drift per
            // fresh frame (in 20 seeds: 0.37 % at 14.99 fps, 0.21 % for a
            // 29.97 fps camera against 30 fps); hardly any with less.
            let settled = out[first].at + 1_000 * MS;
            let waits: Vec<u64> = out
                .iter()
                .filter(|e| e.at > settled)
                .filter_map(|e| e.wait)
                .collect();
            let missed = waits.iter().filter(|w| **w > 2 * period / 3).count();
            let allowed = if jitter + grid / 2 >= 2 * MS {
                waits.len() / 200
            } else {
                2
            };
            assert!(
                missed <= allowed,
                "{tag}: {missed} frames missed their tick"
            );
            let median = p50(waits.clone());
            // The margin, plus the arrival jitter the lock waits for (the
            // grid adds up to half its step either way).
            assert!(
                median <= M + jitter + grid / 2 + MS,
                "{tag}: p50 wait {median}"
            );
            // The phase wraps only where the drift carries it across its
            // band: a 4 ms step of the arrival grid at an edge does not wrap
            // it there and back.
            let fresh_locked = out[first..].iter().filter(|e| e.wait.is_some()).count();
            let crossings =
                (2.0 * period as f64 - 1e9 / fps).abs() * fresh_locked as f64 / period as f64;
            assert!(
                c.lock_wraps as f64 <= crossings + 1.0,
                "{tag}: {} wraps for {crossings:.1} crossings of the band",
                c.lock_wraps
            );
            // Fresh and repeat alternate (two exceptions at each wrap, each
            // missed tick and each dropped frame).
            let after: Vec<TickKind> = out
                .iter()
                .filter(|e| e.at > settled)
                .map(|e| e.kind)
                .collect();
            let doubled = after.windows(2).filter(|w| w[0] == w[1]).count();
            assert!(
                doubled as u64 <= 2 * (c.lock_wraps + (drops + missed) as u64) + 2,
                "{tag}: {doubled} fresh or repeat ticks in a row"
            );
            // No sawtooth: the median wait per second stays within a few
            // milliseconds, and the wrap hysteresis (a faster camera's waits
            // grow by up to it at the early edge before the phase wraps).
            let per_s = median_wait_per_second(&out, settled, end);
            let mut free = Sim::new(arrivals, end);
            free.cfg = sim.cfg;
            free.cfg.lock = PhaseLockConfig::disabled();
            let (fo, _) = free.run();
            let free_per_s = median_wait_per_second(&fo, settled, end);
            let free_waits: Vec<u64> = fo
                .iter()
                .filter(|e| e.at > settled)
                .filter_map(|e| e.wait)
                .collect();
            let free_missed = free_waits.iter().filter(|w| **w > 2 * period / 3).count();
            if fps != 15.0 {
                assert!(
                    missed * 4 < free_missed,
                    "{tag}: free running {free_missed}"
                );
            }
            eprintln!(
                "{fps} fps into {out_fps}, jitter {:.1} ms, grid {} ms: locked after {:.2} s, \
                 acquisitions {} wraps {}, wait p50 {:.2} ms, per-second p50 {:.2}..{:.2} ms, \
                 missed {missed}; free running p50 {:.2} ms, per-second p50 {:.2}..{:.2} ms, \
                 {free_missed} waits over two thirds of a period",
                jitter as f64 / 1e6,
                grid / MS,
                out[first].at as f64 / 1e9,
                c.lock_acquisitions,
                c.lock_wraps,
                median as f64 / 1e6,
                per_s.iter().cloned().fold(f64::MAX, f64::min) / 1e6,
                per_s.iter().cloned().fold(f64::MIN, f64::max) / 1e6,
                p50(free_waits) as f64 / 1e6,
                free_per_s.iter().cloned().fold(f64::MAX, f64::min) / 1e6,
                free_per_s.iter().cloned().fold(f64::MIN, f64::max) / 1e6,
            );
            assert!(
                spread(&per_s) <= (4 * MS + SUB_RATE_WRAP_HYSTERESIS_NS) as f64,
                "{tag}: per-second p50 wait {per_s:?}"
            );
            if fps != 15.0 {
                // A drifting camera sweeps the free-running clock's wait
                // over most of the period in two minutes.
                assert!(spread(&free_per_s) > period as f64 / 2.0, "{tag}");
            }
        }
    }

    /// A switch between two 15 fps cameras at different phases, through a
    /// hold: the second camera is a new run, measured anew and acquired in
    /// one jump once its cadence is known; its frames then wait about the
    /// margin.
    #[test]
    fn phase_lock_reacquires_a_half_rate_input_after_a_switch() {
        let period = cfg30().period_ns();
        let mut arrivals = train(1, 11 * MS, 15.0, 10_000 * MS, MS / 2, 5);
        let start_2 = 10_000 * MS + 600 * MS + 19 * MS;
        arrivals.extend(train(2, start_2, 14.99, 20_000 * MS, MS / 2, 7));
        let mut sim = Sim::new(arrivals, 30_000 * MS);
        sim.holds.push((10_000 * MS, 2, 13_000 * MS));
        let (out, p) = sim.run();
        let c = p.counts();
        assert_grid(&out, &sim.cfg);
        assert_eq!((c.placeholder, c.skipped, c.late), (1, 0, 0), "{c:?}");
        assert_eq!((c.holds_released, c.lock_acquisitions), (1, 2), "{c:?}");
        let first_2 = out.iter().position(|e| e.source == Some(2)).unwrap();
        let relocked = out[first_2..].iter().position(|e| e.locked).unwrap() + first_2;
        assert!(
            out[relocked].at < start_2 + 5_000 * MS,
            "re-locked at {}",
            out[relocked].at
        );
        assert!(out[relocked..].iter().all(|e| e.locked));
        let waits: Vec<u64> = out[relocked + 1..].iter().filter_map(|e| e.wait).collect();
        assert!(waits.len() > 200);
        assert!(p50(waits.clone()) <= M + MS, "{:?}", p50(waits.clone()));
        assert!(waits.iter().all(|w| *w < period / 3), "{waits:?}");
    }

    #[test]
    fn phase_lock_reacquires_after_a_phase_jump() {
        // Source 1 at 30 fps, then source 2 at another phase 100 ms after
        // its last frame (repeats bridge the gap; no placeholder).
        for (phase_2, same_source) in [
            (5 * MS, false),
            (21 * MS, false),
            (29 * MS, true),
            (9 * MS, true),
        ] {
            let s2 = if same_source { 1 } else { 2 };
            let mut arrivals = train(1, 11 * MS, 30.0, 3_000 * MS, 0, 1);
            let last = arrivals.last().unwrap().0;
            let start_2 = (last + 100 * MS) / cfg30().period_ns() * cfg30().period_ns() + phase_2;
            arrivals.extend(train(s2, start_2, 30.0, 3_000 * MS, 0, 1));
            let sim = Sim::new(arrivals, 6_000 * MS);
            let (out, p) = sim.run();
            assert_grid(&out, &sim.cfg);
            assert_eq!(p.counts().placeholder, 1);
            let first_2 = out.iter().position(|e| e.at > start_2).unwrap();
            let settled = out[first_2..]
                .iter()
                .filter(|e| e.kind == TickKind::Fresh)
                .position(|e| e.wait.unwrap() <= M + MS / 2)
                .unwrap();
            let window = PhaseLockConfig::DEFAULT_WINDOW;
            // A new source re-acquires as soon as it is engaged; the same
            // source once its old waits left the window.
            let limit = if same_source { window + 2 } else { 4 };
            assert!(
                settled <= limit,
                "phase {phase_2} same source {same_source}: settled after {settled} fresh ticks"
            );
            let fresh_after: Vec<&Emit> = out[first_2..]
                .iter()
                .filter(|e| e.kind == TickKind::Fresh)
                .skip(settled)
                .collect();
            assert!(fresh_after.len() > 60);
            assert!(fresh_after.iter().all(|e| e.wait.unwrap() <= M + MS / 2));
            assert!(p.counts().lock_acquisitions >= 2);
        }
    }

    /// A camera whose frame train drifts against the ticks faster than the
    /// slew can follow (29.5 to 29.75 fps and 30.25 to 31 fps against
    /// 30 fps, 58 fps against its 60 fps multiple): the lock lets go within
    /// a second, holds its phase, and from then on emits exactly what the
    /// free-running clock emits for a camera at that phase. The free
    /// running clock's superseded frames, repeats and median wait depend on
    /// where the camera's frames fall against the grid, so the comparison
    /// is with the free-running clock at the same camera phase (at most one
    /// frame more, from the ticks before the drift was measured) and with
    /// the range the free-running clock covers over camera phases. No
    /// series of jumps or wraps.
    #[test]
    fn phase_lock_lets_go_of_drift_beyond_its_slew() {
        let t_s = 120u64;
        let end = t_s * 1_000_000_000;
        let wait_p50 = |out: &[Emit]| {
            p50(out
                .iter()
                .filter(|e| e.at > 1_000 * MS)
                .filter_map(|e| e.wait)
                .collect())
        };
        // The free-running clock fed `arrivals` moved later by `shift`
        // (a camera phase `shift` later against the grid).
        let free_run = |arrivals: &[(u64, u64)], shift: u64| {
            let mut sim = Sim::new(arrivals.iter().map(|&(t, s)| (t + shift, s)).collect(), end);
            sim.cfg.lock = PhaseLockConfig::disabled();
            let (out, p) = sim.run();
            let c = p.counts();
            (out.clone(), c.superseded, c.repeat, wait_p50(&out))
        };
        let mut seed = 51;
        for fps in [29.5, 29.6, 29.75, 30.25, 31.0, 58.0] {
            for jitter in [MS / 2, MS] {
                seed += 2;
                let arrivals = train(1, 7 * MS, fps, end + 100 * MS, jitter, seed);
                let sim = Sim::new(arrivals.clone(), end);
                let (out, p) = sim.run();
                assert_grid(&out, &sim.cfg);
                let on = p.counts();
                let w_on = wait_p50(&out);
                let tag = format!("{fps} fps, jitter {jitter}: {on:?}");
                // Let go within a second, for good; at most the one jump of
                // the acquisition, no wrap.
                let last_locked = out.iter().rposition(|e| e.locked);
                assert!(last_locked.is_none_or(|i| i < 30), "{tag}: {last_locked:?}");
                assert!(on.lock_acquisitions <= 1, "{tag}");
                assert_eq!(on.lock_wraps, 0, "{tag}");
                // The free-running clock at the held phase: the same ticks
                // once the lock let go.
                let held = on.phase_offset_ns.unsigned_abs();
                let (same, sup, rep, w) = free_run(&arrivals, held);
                let from = last_locked.map_or(0, |i| i + 3);
                assert!(
                    out[from..]
                        .iter()
                        .zip(&same[from..])
                        .all(|(a, b)| (a.kind, a.wait) == (b.kind, b.wait)),
                    "{tag}"
                );
                assert!(on.superseded <= sup + 1, "{tag}: free running {sup}");
                assert!(on.repeat <= rep + 1, "{tag}: free running {rep}");
                assert!(w_on <= w, "{tag}: p50 wait {w_on}, free running {w}");
                // Within what the free-running clock does over camera
                // phases (eight, an eighth of a period apart).
                let phases: Vec<_> = (0..8)
                    .map(|j| free_run(&arrivals, j * sim.cfg.period_ns() / 8))
                    .collect();
                let worst_sup = phases.iter().map(|f| f.1).max().unwrap();
                let worst_rep = phases.iter().map(|f| f.2).max().unwrap();
                let worst_w = phases.iter().map(|f| f.3).max().unwrap();
                eprintln!(
                    "{fps} fps, jitter {:.1} ms: superseded {} repeat {} p50 wait {:.2} ms; \
                     free running at the held phase {sup} {rep} {:.2} ms, worst phase {worst_sup} \
                     {worst_rep} {:.2} ms",
                    jitter as f64 / 1e6,
                    on.superseded,
                    on.repeat,
                    w_on as f64 / 1e6,
                    w as f64 / 1e6,
                    worst_w as f64 / 1e6
                );
                assert!(on.superseded <= worst_sup, "{tag}");
                assert!(on.repeat <= worst_rep, "{tag}");
                assert!(w_on <= worst_w + MS / 10, "{tag}");
            }
        }
    }

    /// A camera slower than the contract by a little more than half the
    /// slew per tick (29.82 to 29.9 fps against 30 fps), with 1 to 2 ms of
    /// arrival jitter: the slow side's limit. Its frames would miss their
    /// ticks under a lock that tracked it, so the lock lets go once the fit
    /// over a full window of arrivals shows the drift (within a few
    /// seconds), for good, and from then on emits exactly what the
    /// free-running clock emits for a camera at the held phase. Before the
    /// drift is known the lock tracks the camera, and with 2 ms of jitter
    /// a few of its frames miss their ticks there: the counts are within
    /// eight frames of the free-running clock at the held phase, and the
    /// median wait within a tenth of a millisecond. One acquisition, no
    /// wrap.
    #[test]
    fn phase_lock_lets_go_of_a_slow_drift_near_its_limit() {
        let t_s = 120u64;
        let end = t_s * 1_000_000_000;
        let wait_p50 = |out: &[Emit]| {
            p50(out
                .iter()
                .filter(|e| e.at > 1_000 * MS)
                .filter_map(|e| e.wait)
                .collect())
        };
        let free_run = |arrivals: &[(u64, u64)], shift: u64| {
            let mut sim = Sim::new(arrivals.iter().map(|&(t, s)| (t + shift, s)).collect(), end);
            sim.cfg.lock = PhaseLockConfig::disabled();
            let (out, p) = sim.run();
            let c = p.counts();
            (out.clone(), c.superseded, c.repeat, wait_p50(&out))
        };
        let mut seed = 71;
        for fps in [29.82, 29.85, 29.9] {
            for jitter in [MS, 2 * MS] {
                seed += 2;
                let arrivals = train(1, 7 * MS, fps, end + 100 * MS, jitter, seed);
                let sim = Sim::new(arrivals.clone(), end);
                let (out, p) = sim.run();
                assert_grid(&out, &sim.cfg);
                let on = p.counts();
                let w_on = wait_p50(&out);
                let tag = format!("{fps} fps, jitter {jitter}: {on:?}");
                // Let go within five seconds, for good.
                let last_locked = out.iter().rposition(|e| e.locked);
                assert!(
                    last_locked.is_none_or(|i| i < 150),
                    "{tag}: {last_locked:?}"
                );
                assert!(on.lock_acquisitions <= 1, "{tag}");
                assert_eq!(on.lock_wraps, 0, "{tag}");
                let held = on.phase_offset_ns.unsigned_abs();
                let (same, sup, rep, w) = free_run(&arrivals, held);
                let (_, sup_0, rep_0, w_0) = free_run(&arrivals, 0);
                eprintln!(
                    "{fps} fps, jitter {:.1} ms: superseded {} repeat {} p50 wait {:.2} ms, let go \
                     after tick {last_locked:?}; free running at the held phase {sup} {rep} \
                     {:.2} ms, at phase 0 {sup_0} {rep_0} {:.2} ms",
                    jitter as f64 / 1e6,
                    on.superseded,
                    on.repeat,
                    w_on as f64 / 1e6,
                    w as f64 / 1e6,
                    w_0 as f64 / 1e6
                );
                let from = last_locked.map_or(0, |i| i + 3);
                assert!(
                    out[from..]
                        .iter()
                        .zip(&same[from..])
                        .all(|(a, b)| (a.kind, a.wait) == (b.kind, b.wait)),
                    "{tag}"
                );
                assert!(on.superseded <= sup + 8, "{tag}: free running {sup}");
                assert!(on.repeat <= rep + 8, "{tag}: free running {rep}");
                assert!(w_on <= w + MS / 10, "{tag}: p50 {w_on}, free running {w}");
            }
        }
    }

    #[test]
    fn a_hold_repeats_past_the_gap_until_the_awaited_source() {
        // Source 1 stops at 1 s; the switch holds for source 2 (3 s), which
        // delivers from 2.2 s: 1.2 s without input, over the 500 ms gap.
        let mut arrivals = train(1, 4 * MS, 30.0, 1_000 * MS, 0, 1);
        arrivals.extend(train(2, 2_200 * MS, 30.0, 1_000 * MS, 0, 1));
        let mut sim = Sim::new(arrivals, 3_500 * MS);
        sim.holds.push((1_000 * MS, 2, 4_000 * MS));
        let (out, p) = sim.run();
        let c = p.counts();
        assert_eq!(c.placeholder, 1, "only the tick before any input: {c:?}");
        assert_eq!((c.holds, c.holds_released, c.holds_expired), (1, 1, 0));
        let first_2 = out.iter().position(|e| e.source == Some(2)).unwrap();
        assert_eq!(out[first_2].kind, TickKind::Fresh);
        assert!(out[first_2].at >= 2_200 * MS);
        let last_1 = out
            .iter()
            .rposition(|e| e.kind == TickKind::Fresh && e.source == Some(1));
        let gap = &out[last_1.unwrap() + 1..first_2];
        assert!(gap.len() > 30, "{} ticks held", gap.len());
        assert!(
            gap.iter()
                .all(|e| e.kind == TickKind::Repeat && e.source == Some(1))
        );
        // Without the hold the same input goes black after 500 ms.
        let mut free = Sim::new(sim.arrivals.clone(), 3_500 * MS);
        free.holds.clear();
        let (_, q) = free.run();
        assert!(q.counts().placeholder > 15, "{:?}", q.counts());
    }

    #[test]
    fn a_hold_times_out_to_placeholders() {
        // The awaited source never delivers (a failed switch).
        let arrivals = train(1, 4 * MS, 30.0, 1_000 * MS, 0, 1);
        let mut sim = Sim::new(arrivals, 6_000 * MS);
        let until = 1_000 * MS + 3_000 * MS;
        sim.holds.push((1_000 * MS, 2, until));
        let (out, p) = sim.run();
        let c = p.counts();
        assert_eq!((c.holds, c.holds_released, c.holds_expired), (1, 0, 1));
        let last_fresh = out.iter().rposition(|e| e.kind == TickKind::Fresh).unwrap();
        let after = &out[last_fresh + 1..];
        let held = after
            .iter()
            .take_while(|e| e.kind == TickKind::Repeat)
            .count();
        let black = &after[held..];
        // Repeats until the deadline, then black: the last frame is older
        // than the gap threshold by then.
        assert!(after[held - 1].at < until);
        assert!(black[0].at >= until && black[0].at < until + cfg30().period_ns());
        assert!(black.iter().all(|e| e.kind == TickKind::Placeholder));
        assert!(black.len() > 50);
    }

    #[test]
    fn a_straggler_of_the_old_source_does_not_end_the_hold() {
        // The hold is armed at 1 s; source 1 still delivers one frame at
        // 1.05 s; source 2 arrives at 2 s.
        let mut arrivals = train(1, 4 * MS, 30.0, 1_000 * MS, 0, 1);
        arrivals.push((1_050 * MS, 1));
        arrivals.extend(train(2, 2_000 * MS, 30.0, 500 * MS, 0, 1));
        let mut sim = Sim::new(arrivals, 3_000 * MS);
        sim.holds.push((1_000 * MS, 2, 4_000 * MS));
        let (out, p) = sim.run();
        let straggler = out
            .iter()
            .position(|e| e.kind == TickKind::Fresh && e.at > 1_050 * MS)
            .unwrap();
        assert_eq!(out[straggler].source, Some(1));
        assert!(p.counts().holds_released == 1);
        // From the straggler to source 2, 950 ms: repeats throughout.
        let first_2 = out.iter().position(|e| e.source == Some(2)).unwrap();
        assert!(first_2 - straggler > 20);
        assert!(
            out[straggler + 1..first_2]
                .iter()
                .all(|e| e.kind == TickKind::Repeat)
        );
        assert_eq!(p.counts().placeholder, 1);
    }

    #[test]
    fn an_unexpected_loss_still_goes_black_at_the_gap() {
        // A switch completes (hold released), then the new source stops
        // without a switch: placeholders after 500 ms, as without holds.
        let mut arrivals = train(1, 4 * MS, 30.0, 1_000 * MS, 0, 1);
        arrivals.extend(train(2, 1_300 * MS, 30.0, 1_000 * MS, 0, 1));
        let mut sim = Sim::new(arrivals, 4_000 * MS);
        sim.holds.push((1_000 * MS, 2, 4_000 * MS));
        let (out, p) = sim.run();
        assert_eq!(p.counts().holds_released, 1);
        let last = out.iter().rposition(|e| e.kind == TickKind::Fresh).unwrap();
        let after = &out[last + 1..];
        let repeats = after
            .iter()
            .take_while(|e| e.kind == TickKind::Repeat)
            .count();
        assert!((14..=16).contains(&repeats), "{repeats} repeats");
        assert!(
            after[repeats..]
                .iter()
                .all(|e| e.kind == TickKind::Placeholder)
        );
    }

    #[test]
    fn a_hold_does_not_bring_back_a_frame_after_placeholders() {
        // The source was already lost (placeholders) when the switch came.
        let arrivals = train(1, 4 * MS, 30.0, 500 * MS, 0, 1);
        let mut sim = Sim::new(arrivals, 3_000 * MS);
        sim.holds.push((2_000 * MS, 2, 5_000 * MS));
        let (out, _) = sim.run();
        let from = out.iter().position(|e| e.at >= 2_000 * MS).unwrap();
        assert!(out[from..].iter().all(|e| e.kind == TickKind::Placeholder));
    }

    /// The evidence behind the phase lock defaults: p50 / p99 slot wait,
    /// frames that missed their tick, jumps and wraps, and the emission
    /// interval noise (RMS deviation from the period, wraps excluded) for
    /// each setting, over cameras with drift and arrival jitter.
    /// `cargo test -p obc-pipeline --lib phase_lock_tuning_sweep -- --ignored --nocapture`
    #[test]
    #[ignore = "a parameter sweep that prints a table; slow in debug builds"]
    fn phase_lock_tuning_sweep() {
        let t_s = 120u64;
        let end = t_s * 1_000_000_000;
        let cameras = [59.993, 60.01, 29.97, 30.03, 30.0];
        let base = PhaseLockConfig::default();
        let mut settings = vec![("default".to_string(), base)];
        for k in [4, 6, 12, 16] {
            settings.push((format!("window {k}"), PhaseLockConfig { window: k, ..base }));
        }
        for m in [500_000, 1_000_000, 2_000_000, 3_000_000] {
            settings.push((
                format!("margin {m}"),
                PhaseLockConfig {
                    margin_ns: m,
                    ..base
                },
            ));
        }
        for sl in [50_000, 100_000, 400_000] {
            settings.push((
                format!("slew {sl}"),
                PhaseLockConfig {
                    slew_ns: sl,
                    ..base
                },
            ));
        }
        for h in [0, 500_000, 2_000_000] {
            settings.push((
                format!("hysteresis {h}"),
                PhaseLockConfig {
                    wrap_hysteresis_ns: h,
                    ..base
                },
            ));
        }
        for j in [1_000_000, 2_000_000, 3_000_000, 4_000_000] {
            settings.push((format!("jump {j}"), PhaseLockConfig { jump_ns: j, ..base }));
        }
        settings.push((
            "window 32".to_string(),
            PhaseLockConfig { window: 32, ..base },
        ));
        println!(
            "jitter_ms setting | p50 p99 max wait (ms, worst camera) | missed | jumps | wraps | interval rms (us, worst)"
        );
        // Uniform jitter, then a skewed one: +-1 ms, and one frame in ten
        // later by up to 5 ms more (as measured on a CPU decode path).
        for (jitter, tail) in [
            (250_000u64, (0, 0)),
            (MS, (0, 0)),
            (2 * MS, (0, 0)),
            (MS, (100, 5 * MS)),
        ] {
            for (name, lock) in &settings {
                let (mut p50w, mut p99w, mut maxw, mut rms_w) = (0u64, 0u64, 0u64, 0f64);
                let (mut missed, mut jumps, mut wraps) = (0u64, 0u64, 0u64);
                for (i, fps) in cameras.iter().enumerate() {
                    let mut sim = Sim::new(
                        train_tailed(1, 7 * MS, *fps, end, jitter, tail, 31 + i as u64),
                        end,
                    );
                    sim.cfg.lock = *lock;
                    let (out, p) = sim.run();
                    let c = p.counts();
                    let mut waits: Vec<u64> = out
                        .iter()
                        .filter(|e| e.at > 1_000 * MS)
                        .filter_map(|e| e.wait)
                        .collect();
                    waits.sort_unstable();
                    p50w = p50w.max(waits[waits.len() / 2]);
                    p99w = p99w.max(waits[waits.len() * 99 / 100]);
                    maxw = maxw.max(*waits.last().unwrap());
                    // A wait over a third of a period: the frame missed the
                    // tick it was meant for.
                    missed += waits
                        .iter()
                        .filter(|w| **w > sim.cfg.period_ns() / 3)
                        .count() as u64;
                    jumps += c.lock_acquisitions;
                    wraps += c.lock_wraps;
                    let devs: Vec<f64> = out
                        .windows(2)
                        .map(|w| (w[1].at - w[0].at) as f64 - sim.cfg.period_ns() as f64)
                        .filter(|d| d.abs() < 3e6)
                        .collect();
                    let rms = (devs.iter().map(|d| d * d).sum::<f64>() / devs.len() as f64).sqrt();
                    rms_w = rms_w.max(rms);
                }
                println!(
                    "{:>4.2}{} {name:<16} | {:.2} {:.2} {:.2} | {missed} | {jumps} | {wraps} | {:.0}",
                    jitter as f64 / 1e6,
                    if tail.0 > 0 { "+tail" } else { "" },
                    p50w as f64 / 1e6,
                    p99w as f64 / 1e6,
                    maxw as f64 / 1e6,
                    rms_w / 1e3
                );
            }
        }
    }
}
