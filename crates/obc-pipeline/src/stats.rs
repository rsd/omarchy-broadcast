//! Pure measurement math: percentiles, windowed frame rates, CPU usage.
//!
//! Everything here works on plain numbers so it is unit-tested without a
//! pipeline. Timestamps are nanoseconds on one monotonic clock.

use serde::Serialize;

/// Summary of a sample set (values in the caller's unit).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Summary {
    pub count: usize,
    pub mean: f64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

/// Linear-interpolated percentile (`p` in 0..=100) of an ascending slice,
/// the same definition as NumPy's default. `None` for an empty slice.
pub fn percentile_sorted(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let p = p.clamp(0.0, 100.0);
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    Some(sorted[lo] + (sorted[hi] - sorted[lo]) * frac)
}

/// Summarises `values` (any order). `None` when empty.
pub fn summarize(values: &[f64]) -> Option<Summary> {
    if values.is_empty() {
        return None;
    }
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    Some(Summary {
        count: v.len(),
        mean,
        min: v[0],
        p50: percentile_sorted(&v, 50.0)?,
        p95: percentile_sorted(&v, 95.0)?,
        p99: percentile_sorted(&v, 99.0)?,
        max: v[v.len() - 1],
    })
}

/// Counts events per consecutive window of `window_ns` starting at
/// `start_ns`, over `[start_ns, end_ns)`. Only complete windows are returned,
/// so a trailing partial window never reads as a frame-rate drop.
pub fn windowed_counts(times_ns: &[u64], start_ns: u64, end_ns: u64, window_ns: u64) -> Vec<u32> {
    if window_ns == 0 || end_ns <= start_ns {
        return Vec::new();
    }
    let n = ((end_ns - start_ns) / window_ns) as usize;
    let mut counts = vec![0u32; n];
    for &t in times_ns {
        if t < start_ns {
            continue;
        }
        let idx = ((t - start_ns) / window_ns) as usize;
        if idx < n {
            counts[idx] += 1;
        }
    }
    counts
}

/// Frame-rate figures for one measurement window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FpsReport {
    /// Frames in the window divided by its length.
    pub overall: f64,
    /// Lowest / highest frame count over complete 1 s windows.
    pub min_1s: Option<f64>,
    pub max_1s: Option<f64>,
    /// Number of complete 1 s windows.
    pub windows: usize,
    /// Complete 1 s windows whose count fell below `target - tolerance`.
    pub windows_below_target: usize,
    pub frames: usize,
}

/// Computes [`FpsReport`] from arrival times. A 1 s window counts as below
/// target when it delivered fewer than `target_fps - tolerance` frames
/// (tolerance absorbs the ±1 frame that lands on a window boundary).
pub fn fps_report(
    times_ns: &[u64],
    start_ns: u64,
    end_ns: u64,
    target_fps: f64,
    tolerance: f64,
) -> FpsReport {
    let in_window = times_ns
        .iter()
        .filter(|&&t| t >= start_ns && t < end_ns)
        .count();
    let secs = end_ns.saturating_sub(start_ns) as f64 / 1e9;
    let counts = windowed_counts(times_ns, start_ns, end_ns, 1_000_000_000);
    FpsReport {
        overall: if secs > 0.0 {
            in_window as f64 / secs
        } else {
            0.0
        },
        min_1s: counts.iter().min().map(|&c| f64::from(c)),
        max_1s: counts.iter().max().map(|&c| f64::from(c)),
        windows: counts.len(),
        windows_below_target: counts
            .iter()
            .filter(|&&c| f64::from(c) < target_fps - tolerance)
            .count(),
        frames: in_window,
    }
}

/// Counts inter-arrival gaps longer than `factor` frame durations: each is a
/// moment where at least one frame was late or missing.
pub fn count_stalls(times_ns: &[u64], frame_ns: u64, factor: f64) -> usize {
    let limit = frame_ns as f64 * factor;
    times_ns
        .windows(2)
        .filter(|w| w[1].saturating_sub(w[0]) as f64 > limit)
        .count()
}

/// Frames missing from a timestamp sequence: for each gap, `round(gap /
/// frame) - 1`. Used on the camera's PTS to separate camera-side drops from
/// ours.
pub fn missing_frames_from_gaps(pts_ns: &[u64], frame_ns: u64) -> u64 {
    if frame_ns == 0 {
        return 0;
    }
    pts_ns
        .windows(2)
        .map(|w| {
            let gap = w[1].saturating_sub(w[0]) as f64 / frame_ns as f64;
            (gap.round() as u64).saturating_sub(1)
        })
        .sum()
}

/// Parses `utime` and `stime` (clock ticks) from `/proc/<pid>/stat`.
///
/// The `comm` field may contain spaces and parentheses, so fields are
/// counted after the last `)`: `state` is field 3, `utime` 14, `stime` 15.
pub fn parse_proc_stat_ticks(stat: &str) -> Option<(u64, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime = fields.get(11)?.parse().ok()?;
    let stime = fields.get(12)?.parse().ok()?;
    Some((utime, stime))
}

/// CPU usage over a wall-clock interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CpuUsage {
    /// Percent of one core (can exceed 100).
    pub one_core_pct: f64,
    /// Percent of all online cores (0..=100).
    pub all_cores_pct: f64,
}

/// CPU % from a tick delta: `ticks / ticks_per_sec / wall_secs`.
pub fn cpu_usage(delta_ticks: u64, ticks_per_sec: u64, wall_secs: f64, ncpu: usize) -> CpuUsage {
    if ticks_per_sec == 0 || wall_secs <= 0.0 {
        return CpuUsage {
            one_core_pct: 0.0,
            all_cores_pct: 0.0,
        };
    }
    let one = delta_ticks as f64 / ticks_per_sec as f64 / wall_secs * 100.0;
    CpuUsage {
        one_core_pct: one,
        all_cores_pct: one / ncpu.max(1) as f64,
    }
}

/// Reads this process's cumulative `utime + stime` in clock ticks.
pub fn self_cpu_ticks() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let (u, st) = parse_proc_stat_ticks(&s)?;
    Some(u + st)
}

/// `sysconf(_SC_CLK_TCK)`.
pub fn ticks_per_sec() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

/// Number of online CPUs.
pub fn online_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_match_numpy_linear() {
        let v: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile_sorted(&v, 0.0), Some(1.0));
        assert_eq!(percentile_sorted(&v, 100.0), Some(10.0));
        assert!((percentile_sorted(&v, 50.0).unwrap() - 5.5).abs() < 1e-9);
        assert!((percentile_sorted(&v, 95.0).unwrap() - 9.55).abs() < 1e-9);
        assert!((percentile_sorted(&v, 99.0).unwrap() - 9.91).abs() < 1e-9);
        assert_eq!(percentile_sorted(&[], 50.0), None);
        assert_eq!(percentile_sorted(&[7.0], 99.0), Some(7.0));
    }

    #[test]
    fn summary_sorts_and_ignores_nan() {
        let s = summarize(&[3.0, f64::NAN, 1.0, 2.0]).unwrap();
        assert_eq!(s.count, 3);
        assert_eq!((s.min, s.p50, s.max), (1.0, 2.0, 3.0));
        assert!((s.mean - 2.0).abs() < 1e-9);
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn windowed_counts_complete_windows_only() {
        let s = 1_000_000_000u64;
        // 60 events per second for 2.5 s, starting at t=10 s.
        let times: Vec<u64> = (0..150).map(|i| 10 * s + i * s / 60).collect();
        let c = windowed_counts(&times, 10 * s, 10 * s + 2 * s + s / 2, s);
        assert_eq!(c, vec![60, 60]);
        assert!(windowed_counts(&times, 5, 5, s).is_empty());
        assert!(windowed_counts(&times, 0, 10, 0).is_empty());
    }

    #[test]
    fn fps_report_detects_a_slow_second() {
        let s = 1_000_000_000u64;
        let mut times: Vec<u64> = (0..180).map(|i| i * s / 60).collect();
        // Remove 10 frames from the second window.
        times.retain(|&t| !(t >= s && t < s + s / 6));
        let r = fps_report(&times, 0, 3 * s, 60.0, 1.0);
        assert_eq!(r.windows, 3);
        assert_eq!(r.min_1s, Some(50.0));
        assert_eq!(r.max_1s, Some(60.0));
        assert_eq!(r.windows_below_target, 1);
        assert_eq!(r.frames, 170);
        assert!((r.overall - 170.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn stalls_and_gaps() {
        let f = 16_666_667u64;
        let t = [0, f, 2 * f, 5 * f, 6 * f];
        assert_eq!(count_stalls(&t, f, 1.5), 1);
        assert_eq!(missing_frames_from_gaps(&t, f), 2);
        assert_eq!(missing_frames_from_gaps(&[0, f, 2 * f], f), 0);
        // Jitter below half a frame is not a missing frame.
        assert_eq!(missing_frames_from_gaps(&[0, f + f / 3], f), 0);
        assert_eq!(missing_frames_from_gaps(&t, 0), 0);
    }

    #[test]
    fn proc_stat_parsing_handles_spaces_in_comm() {
        let line = "1234 (obc bench) (x)) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 75 0 0 20 0 8 0 100 0 0";
        assert_eq!(parse_proc_stat_ticks(line), Some((250, 75)));
        assert_eq!(parse_proc_stat_ticks("garbage"), None);
    }

    #[test]
    fn cpu_usage_math() {
        let u = cpu_usage(150, 100, 10.0, 24);
        assert!((u.one_core_pct - 15.0).abs() < 1e-9);
        assert!((u.all_cores_pct - 0.625).abs() < 1e-9);
        assert_eq!(cpu_usage(1, 0, 1.0, 1).one_core_pct, 0.0);
        assert_eq!(cpu_usage(1, 100, 0.0, 1).one_core_pct, 0.0);
    }

    #[test]
    fn self_cpu_ticks_reads() {
        assert!(self_cpu_ticks().is_some());
        assert!(ticks_per_sec() > 0);
    }
}
