//! The algorithmic delay of a loaded effect, measured on this machine at
//! load time.
//!
//! The SDK does not report its effects' delays, and the module states no
//! figure of its own: after each load it runs a short synthetic voice
//! through the effect ([`probe_signal`]), finds the lag at which the output
//! best matches the input ([`estimate_lag`]), and resets the effect. The
//! search is coarse to fine: the lag of the best match of the two signals'
//! loudness envelopes (robust to the phase changes a generative model
//! makes), then the best match of the samples around it. The measurement
//! doubles as the effect's warm-up.

/// The longest delay looked for, in samples (250 ms at 48 kHz).
pub const MAX_LAG: usize = 12_000;

/// The length of the voice of the probe, in samples (1 s at 48 kHz).
pub const VOICE_SAMPLES: usize = 48_000;

/// The hop of the loudness envelope, in samples.
const HOP: usize = 48;

/// The least correlation of the envelopes that counts as a match.
pub const MIN_ENVELOPE_CORRELATION: f64 = 0.5;

/// The least correlation of the samples for the fine lag to be used
/// (otherwise the envelope's lag is).
pub const MIN_SAMPLE_CORRELATION: f64 = 0.3;

/// A deterministic generator (xorshift64*), uniform in [0, 1).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The probe: [`VOICE_SAMPLES`] of a speech-like voice (syllables of 150
/// to 300 ms with pauses of 60 to 160 ms between them, each a harmonic
/// series of a gliding pitch shaped by two formants, under a smooth
/// envelope; peak about 0.3), then silence up to `len` samples.
pub fn probe_signal(len: usize) -> Vec<f32> {
    let rate = 48_000.0;
    let mut out = vec![0.0f32; len];
    let n = VOICE_SAMPLES.min(len);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut t = 0usize;
    while t < n {
        let syl = ((0.15 + 0.15 * rng.next()) * rate) as usize;
        let pause = ((0.06 + 0.10 * rng.next()) * rate) as usize;
        let f0 = 100.0 + 120.0 * rng.next();
        let glide = 0.3 * (rng.next() - 0.5);
        let f1 = 500.0 + 500.0 * rng.next();
        let f2 = 1200.0 + 1000.0 * rng.next();
        let mut phase = 0.0f64;
        for i in 0..syl.min(n - t) {
            let x = i as f64 / syl as f64;
            let env = (std::f64::consts::PI * x).sin().powf(0.7);
            let f = f0 * (1.0 + glide * x);
            phase += std::f64::consts::TAU * f / rate;
            let mut v = 0.0;
            let mut k = 1.0;
            while k * f < 5_000.0 {
                let fk = k * f;
                let formant = 1.0 / (1.0 + ((fk - f1) / 150.0).powi(2))
                    + 0.6 / (1.0 + ((fk - f2) / 250.0).powi(2))
                    + 0.05;
                v += formant / k * (k * phase).sin();
                k += 1.0;
            }
            out[t + i] = (0.3 * env * v) as f32;
        }
        t += syl + pause;
    }
    out
}

/// The loudness envelope of `x`: the RMS of each hop.
fn envelope(x: &[f32]) -> Vec<f64> {
    x.chunks(HOP)
        .map(|c| (c.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / c.len() as f64).sqrt())
        .collect()
}

/// The Pearson correlation of `a` with `b[lag..]` over `a`'s length.
fn pearson(a: &[f64], b: &[f64], lag: usize) -> f64 {
    let n = a.len();
    let b = &b[lag..lag + n];
    let ma = a.iter().sum::<f64>() / n as f64;
    let mb = b.iter().sum::<f64>() / n as f64;
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        ab += (x - ma) * (y - mb);
        aa += (x - ma).powi(2);
        bb += (y - mb).powi(2);
    }
    if aa <= 0.0 || bb <= 0.0 {
        return 0.0;
    }
    ab / (aa * bb).sqrt()
}

/// The normalised correlation of `x` with `y[lag..]` over `x`'s length.
fn correlation(x: &[f32], y: &[f32], lag: usize) -> f64 {
    let (mut xy, mut xx, mut yy) = (0.0f64, 0.0f64, 0.0f64);
    for (a, b) in x.iter().zip(&y[lag..lag + x.len()]) {
        let (a, b) = (f64::from(*a), f64::from(*b));
        xy += a * b;
        xx += a * a;
        yy += b * b;
    }
    if xx <= 0.0 || yy <= 0.0 {
        return 0.0;
    }
    xy / (xx * yy).sqrt()
}

/// The lag in `0..=max_lag` at which `output` best matches the first
/// `voice` samples of `input`, or `None` when nothing matches (the
/// envelopes correlate less than [`MIN_ENVELOPE_CORRELATION`] at every
/// lag). `output` holds at least `voice + max_lag` samples.
pub fn estimate_lag(input: &[f32], output: &[f32], voice: usize, max_lag: usize) -> Option<usize> {
    let voice = voice.min(input.len());
    if output.len() < voice + max_lag || voice < HOP {
        return None;
    }
    let ein = envelope(&input[..voice - voice % HOP]);
    let eout = envelope(output);
    let max_hops = (max_lag / HOP).min(eout.len().saturating_sub(ein.len()));
    let (coarse, r) = (0..=max_hops)
        .map(|l| (l, pearson(&ein, &eout, l)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    if r < MIN_ENVELOPE_CORRELATION {
        return None;
    }
    let centre = coarse * HOP;
    let from = centre.saturating_sub(2 * HOP);
    let to = (centre + 2 * HOP).min(max_lag);
    let (fine, c) = (from..=to)
        .map(|l| (l, correlation(&input[..voice], output, l)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    Some(if c >= MIN_SAMPLE_CORRELATION {
        fine
    } else {
        centre
    })
}

/// Measures the delay of the effect `run` (one call per `frame`-sample
/// block, as the module runs it): the probe through it, then
/// [`estimate_lag`]. `Ok(None)`: no lag found.
pub fn measure<E>(
    frame: usize,
    mut run: impl FnMut(&[f32], &mut [f32]) -> Result<(), E>,
) -> Result<Option<u32>, E> {
    let len = (VOICE_SAMPLES + MAX_LAG).div_ceil(frame) * frame;
    let input = probe_signal(len);
    let mut output = vec![0.0f32; len];
    for (i, o) in input.chunks(frame).zip(output.chunks_mut(frame)) {
        run(i, o)?;
    }
    Ok(estimate_lag(&input, &output, VOICE_SAMPLES, MAX_LAG).map(|l| l as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delayed(x: &[f32], d: usize, gain: f32) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for i in d..x.len() {
            y[i] = x[i - d] * gain;
        }
        y
    }

    #[test]
    fn the_probe_is_a_voice_then_silence() {
        let x = probe_signal(VOICE_SAMPLES + 1000);
        let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((0.15..0.6).contains(&peak), "{peak}");
        assert!(x[VOICE_SAMPLES..].iter().all(|v| *v == 0.0));
        assert_eq!(x, probe_signal(VOICE_SAMPLES + 1000), "deterministic");
    }

    #[test]
    fn a_delay_is_found_to_the_sample() {
        let x = probe_signal(VOICE_SAMPLES + MAX_LAG);
        for d in [0, 1, 480, 1337, 5000, MAX_LAG] {
            let y = delayed(&x, d, 0.4);
            assert_eq!(estimate_lag(&x, &y, VOICE_SAMPLES, MAX_LAG), Some(d), "{d}");
        }
    }

    #[test]
    fn a_blurred_copy_is_found_near_its_delay_and_noise_not_at_all() {
        let x = probe_signal(VOICE_SAMPLES + MAX_LAG);
        // A low-passed copy: the envelope still leads.
        let mut y = delayed(&x, 2000, 1.0);
        let mut lp = 0.0f32;
        for v in &mut y {
            lp = 0.7 * lp + 0.3 * *v;
            *v = lp;
        }
        let lag = estimate_lag(&x, &y, VOICE_SAMPLES, MAX_LAG).unwrap();
        assert!(lag.abs_diff(2000) <= 4, "{lag}");
        let mut s = 7u64;
        let noise: Vec<f32> = (0..x.len())
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s % 1000) as f32 / 1000.0 - 0.5
            })
            .collect();
        assert_eq!(estimate_lag(&x, &noise, VOICE_SAMPLES, MAX_LAG), None);
        assert_eq!(
            estimate_lag(&x, &vec![0.0; x.len()], VOICE_SAMPLES, MAX_LAG),
            None
        );
    }

    #[test]
    fn measuring_runs_the_probe_through_the_effect_block_by_block() {
        let mut queue = std::collections::VecDeque::from(vec![0.0f32; 777]);
        let mut calls = 0;
        let lag = measure(480, |i: &[f32], o: &mut [f32]| -> Result<(), ()> {
            calls += 1;
            assert_eq!(i.len(), 480);
            for (d, s) in o.iter_mut().zip(i) {
                queue.push_back(*s);
                *d = queue.pop_front().unwrap();
            }
            Ok(())
        });
        assert_eq!(lag, Ok(Some(777)));
        assert_eq!(calls, (VOICE_SAMPLES + MAX_LAG).div_ceil(480));
        assert_eq!(measure(480, |_, _| Err("boom")), Err("boom"));
    }
}
