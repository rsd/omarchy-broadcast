//! Output pacing: when the output clock emits a frame, and which frame.
//!
//! [`Pacer`] is pure: it is fed the current time and the slot's newest
//! sequence number and arrival time, and answers with what to do. The output
//! thread ([`crate::output`]) supplies the real clock; the tests supply
//! scripted times.
//!
//! **Schedule.** Tick `n` is due at `n x period` after the epoch, an absolute
//! grid computed from the exact frame-rate fraction, so neither rounding nor
//! wake-up delays accumulate. A tick is emitted when its deadline has passed.
//! A wake-up later than the next tick's deadline is a **late** tick: it is
//! still emitted, immediately, and the ticks after it follow at once until
//! the grid is caught up, so every second of output still carries the
//! contract's frame count. When the clock is behind by
//! [`PacerConfig::resync_after_ns`] or more (a long stall: suspend, a stopped
//! debugger), or when more than [`PacerConfig::max_burst`] ticks are due at
//! once, catching up would burst more frames than the output can queue;
//! instead the pacer jumps to the tick due now and counts the ticks it
//! **skipped**. The output sizes its queue to `max_burst`, so a catch-up
//! burst is never thinned by the queue.
//!
//! **Content.** Per tick: a frame newer than the last one emitted is
//! **fresh** (frames that arrived and were replaced in between are counted as
//! **superseded**); otherwise the last frame is emitted again (**repeat**)
//! while it is younger than the gap threshold; after that, and before any
//! frame arrived, the output's black frame (**placeholder**).

/// Pacing parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacerConfig {
    /// Output frame rate numerator (frames per `fps_den` seconds).
    pub fps_num: u32,
    pub fps_den: u32,
    /// Age of the newest input frame beyond which the placeholder replaces
    /// repeats.
    pub gap_threshold_ns: u64,
    /// How far behind its grid the clock may fall before it resynchronises
    /// instead of catching up.
    pub resync_after_ns: u64,
    /// Most ticks emitted back to back while catching up; with more due at
    /// once the clock resynchronises. [`Self::new`] sets it to the ticks a
    /// stall just under `resync_after_ns` leaves due
    /// ([`Self::burst_within`]).
    pub max_burst: u64,
}

impl PacerConfig {
    /// Default gap threshold before the placeholder (500 ms).
    pub const DEFAULT_GAP_THRESHOLD_NS: u64 = 500_000_000;
    /// Default resynchronisation threshold (1 s).
    pub const DEFAULT_RESYNC_AFTER_NS: u64 = 1_000_000_000;

    /// Defaults for a frame rate of `fps_num / fps_den`.
    pub fn new(fps_num: u32, fps_den: u32) -> Self {
        Self::with(
            fps_num,
            fps_den,
            Self::DEFAULT_GAP_THRESHOLD_NS,
            Self::DEFAULT_RESYNC_AFTER_NS,
        )
    }

    /// A frame rate, gap threshold and resynchronisation threshold; the
    /// burst cap follows from the last ([`Self::burst_within`]).
    pub fn with(fps_num: u32, fps_den: u32, gap_threshold_ns: u64, resync_after_ns: u64) -> Self {
        let mut cfg = PacerConfig {
            fps_num,
            fps_den,
            gap_threshold_ns,
            resync_after_ns,
            max_burst: 1,
        };
        cfg.max_burst = cfg.burst_within(resync_after_ns);
        cfg
    }

    /// The most ticks that can be due at once while the clock is behind its
    /// grid by less than `behind_ns`: those whose deadlines fall in a
    /// window of that length, plus the one at its start. At least 1.
    pub fn burst_within(&self, behind_ns: u64) -> u64 {
        self.index_at(behind_ns.saturating_sub(1))
            .saturating_add(2)
            .max(1)
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

/// Running totals of the pacer's decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct PacerCounts {
    pub fresh: u64,
    pub repeat: u64,
    pub placeholder: u64,
    /// Input frames replaced in the slot before any tick emitted them.
    pub superseded: u64,
    /// Ticks emitted after the next tick's deadline had already passed.
    pub late: u64,
    /// Ticks never emitted because the grid was resynchronised.
    pub skipped: u64,
}

impl PacerCounts {
    /// Ticks emitted (fresh + repeat + placeholder).
    pub fn ticks(&self) -> u64 {
        self.fresh + self.repeat + self.placeholder
    }
}

/// The output clock's decisions. See the module docs.
#[derive(Debug, Clone)]
pub struct Pacer {
    cfg: PacerConfig,
    /// Index of the next tick to emit.
    next: u64,
    /// Sequence number of the last emitted frame (0: none yet).
    last_seq: u64,
    counts: PacerCounts,
}

impl Pacer {
    pub fn new(cfg: PacerConfig) -> Self {
        Pacer {
            cfg,
            next: 0,
            last_seq: 0,
            counts: PacerCounts::default(),
        }
    }

    pub fn config(&self) -> &PacerConfig {
        &self.cfg
    }

    pub fn counts(&self) -> PacerCounts {
        self.counts
    }

    /// Index of the next tick to emit.
    pub fn next_index(&self) -> u64 {
        self.next
    }

    /// A pacer continuing an earlier run of the same output: the frame of
    /// sequence `last_seq` and every earlier one count as already handled
    /// (emitted or superseded), and the counts carry on from `counts`. The
    /// grid starts again at tick 0.
    pub fn resume(cfg: PacerConfig, last_seq: u64, counts: PacerCounts) -> Self {
        Pacer {
            cfg,
            next: 0,
            last_seq,
            counts,
        }
    }

    /// Advances the schedule to `now_ns`: the next tick to emit (and the
    /// schedule moves past it), or how long to wait for it. Resynchronises
    /// the grid after a long stall or when more than
    /// [`PacerConfig::max_burst`] ticks are due (see the module docs).
    pub fn step(&mut self, now_ns: u64) -> Step {
        let due = self.cfg.deadline_ns(self.next);
        if now_ns < due {
            return Step::Wait { until_ns: due };
        }
        let current = self.cfg.index_at(now_ns).max(self.next);
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

    /// Decides the content of the tick being emitted at `now_ns`, given the
    /// slot's newest entry as `(seq, arrived_ns)` (`None`: the slot is
    /// empty), and records it.
    pub fn decide(&mut self, latest: Option<(u64, u64)>, now_ns: u64) -> TickKind {
        let kind = match latest {
            Some((seq, _)) if seq > self.last_seq => {
                self.counts.superseded += seq - self.last_seq - 1;
                self.last_seq = seq;
                TickKind::Fresh
            }
            Some((seq, arrived_ns))
                if seq == self.last_seq
                    && now_ns.saturating_sub(arrived_ns) < self.cfg.gap_threshold_ns =>
            {
                TickKind::Repeat
            }
            _ => TickKind::Placeholder,
        };
        match kind {
            TickKind::Fresh => self.counts.fresh += 1,
            TickKind::Repeat => self.counts.repeat += 1,
            TickKind::Placeholder => self.counts.placeholder += 1,
        }
        kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn cfg30() -> PacerConfig {
        PacerConfig::new(30, 1)
    }

    /// A scripted run: an input producing a frame every `input_period_ns`
    /// from `input_start_ns` until `input_stop_ns` (exclusive), and a clock
    /// thread that wakes exactly at each deadline plus `wake_delay(index)`.
    /// Returns the emitted `(index, pts, kind)` up to `end_ns`, and the
    /// pacer.
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
            let latest_at = |t: u64| -> Option<(u64, u64)> {
                if self.input_period_ns == 0 || t < self.input_start_ns {
                    return None;
                }
                let last_t = t.min(self.input_stop_ns.saturating_sub(1));
                if last_t < self.input_start_ns {
                    return None;
                }
                let k = (last_t - self.input_start_ns) / self.input_period_ns;
                Some((k + 1, self.input_start_ns + k * self.input_period_ns))
            };
            while now < self.end_ns {
                match pacer.step(now) {
                    Step::Wait { until_ns } => {
                        now = until_ns + wake_delay(pacer.next_index());
                    }
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
        assert_eq!(p.decide(Some((1, 0)), t(0)), TickKind::Fresh);
        // No input for 40 ticks: repeats, then placeholders.
        let mut kinds = Vec::new();
        for i in 1..=40 {
            assert!(matches!(p.step(t(i)), Step::Tick { .. }));
            kinds.push(p.decide(Some((1, 0)), t(i)));
        }
        assert_eq!(kinds[..14], [TickKind::Repeat; 14]);
        assert_eq!(kinds[14..], [TickKind::Placeholder; 26]);
        // A new source's first frame (sequence continues) is fresh at once.
        assert!(matches!(p.step(t(41)), Step::Tick { .. }));
        assert_eq!(p.decide(Some((2, t(41) - MS)), t(41)), TickKind::Fresh);
        assert!(matches!(p.step(t(42)), Step::Tick { .. }));
        assert_eq!(p.decide(Some((2, t(41) - MS)), t(42)), TickKind::Repeat);
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

    #[test]
    fn a_stall_under_the_resync_threshold_bursts_to_catch_up() {
        let mut p = Pacer::new(cfg30());
        let t = |i: u64| cfg30().deadline_ns(i);
        assert!(matches!(p.step(0), Step::Tick { index: 0, .. }));
        // Woken 900 ms late: ticks 1..=27 are all due now.
        let now = t(1) + 900 * MS;
        let mut emitted = Vec::new();
        while let Step::Tick { index, .. } = p.step(now) {
            emitted.push(index);
        }
        assert_eq!(emitted, (1..=cfg30().index_at(now)).collect::<Vec<_>>());
        assert_eq!(p.counts().skipped, 0);
        // All but the last were emitted after their successor was due.
        assert_eq!(p.counts().late, emitted.len() as u64 - 1);
    }

    #[test]
    fn the_default_burst_cap_covers_a_stall_just_under_the_resync_threshold() {
        for (num, den) in [(30, 1), (60, 1), (30000, 1001), (240, 1), (1, 1)] {
            let cfg = PacerConfig::new(num, den);
            assert_eq!(cfg.max_burst, cfg.burst_within(cfg.resync_after_ns));
            // The worst case: the next tick was due just under 1 s ago,
            // and the wake-up lands right on a deadline.
            let mut p = Pacer::new(cfg);
            let next = 5;
            for i in 0..next {
                p.step(cfg.deadline_ns(i));
            }
            let now = cfg.deadline_ns(next) + cfg.resync_after_ns - 1;
            let mut burst = 0;
            while let Step::Tick { .. } = p.step(now) {
                burst += 1;
            }
            assert_eq!(p.counts().skipped, 0, "{num}/{den}");
            assert!(burst <= cfg.max_burst, "{num}/{den}: {burst}");
        }
        assert_eq!(cfg30().max_burst, 31);
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
        assert_eq!(p.decide(Some((3, 0)), 0), TickKind::Fresh);
        let before = p.counts();
        assert_eq!(before.superseded, 2);
        // Restarted after frames up to 40 were written: frame 41 is fresh
        // and nothing written before the restart counts as superseded.
        let mut r = Pacer::resume(cfg30(), 40, before);
        assert!(matches!(r.step(0), Step::Tick { index: 0, .. }));
        assert_eq!(r.decide(Some((41, 0)), 0), TickKind::Fresh);
        let after = r.counts();
        assert_eq!(after.superseded, before.superseded);
        assert_eq!(after.fresh, before.fresh + 1);
        // A frame still in the slot at the restart (40) is a repeat.
        let mut r = Pacer::resume(cfg30(), 40, before);
        r.step(0);
        assert_eq!(r.decide(Some((40, 0)), 0), TickKind::Repeat);
    }

    #[test]
    fn superseded_counts_from_the_first_frame() {
        let mut p = Pacer::new(cfg30());
        p.step(0);
        // Five frames arrived before the first tick: four never emitted.
        assert_eq!(p.decide(Some((5, 0)), 0), TickKind::Fresh);
        assert_eq!(p.counts().superseded, 4);
        p.step(cfg30().deadline_ns(1));
        assert_eq!(p.decide(Some((6, 0)), MS), TickKind::Fresh);
        assert_eq!(p.counts().superseded, 4);
    }
}
