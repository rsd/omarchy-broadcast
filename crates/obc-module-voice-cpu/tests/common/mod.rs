//! Test signals made in the test (no recorded audio is committed): a
//! speech-like voice of syllables, a noise, and the measures the quality
//! tests take.

#![allow(dead_code)] // each test binary uses part of it

use std::f64::consts::TAU;

/// Samples per second.
pub const RATE: usize = 48_000;
/// Samples per block.
pub const HOP: usize = 480;

/// A deterministic generator (xorshift64*), uniform in [-1, 1).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    pub fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (v >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// `secs` seconds of a speech-like voice: syllables of 180 to 300 ms with
/// pauses of 80 to 200 ms between them, each a glottal-like harmonic series
/// (pitch 100 to 220 Hz, gliding) shaped by two formants, under a smooth
/// envelope; the first 0.5 s are a pause. Peak about 0.3.
pub fn voice(secs: f64, seed: u64) -> Vec<f32> {
    let n = (secs * RATE as f64) as usize;
    let mut out = vec![0.0f32; n];
    let mut rng = Rng::new(seed);
    let mut t = (0.5 * RATE as f64) as usize;
    while t < n {
        let len = ((0.18 + 0.12 * (rng.next() + 1.0) / 2.0) * RATE as f64) as usize;
        let pause = ((0.08 + 0.12 * (rng.next() + 1.0) / 2.0) * RATE as f64) as usize;
        let f0 = 100.0 + 60.0 * (rng.next() + 1.0);
        let glide = 0.25 * rng.next();
        let f1 = 500.0 + 300.0 * (rng.next() + 1.0);
        let f2 = 1200.0 + 600.0 * (rng.next() + 1.0);
        let mut phase = 0.0f64;
        for i in 0..len.min(n - t) {
            let x = i as f64 / len as f64;
            let env = (std::f64::consts::PI * x).sin().powf(0.7);
            let f = f0 * (1.0 + glide * x);
            phase += TAU * f / RATE as f64;
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
            out[t + i] = (0.12 * env * v) as f32;
        }
        t += len + pause;
    }
    out
}

/// `n` samples of noise: white noise through a one-pole low-pass (a fan or
/// a room's hiss), plus a little 50 Hz hum, at an RMS of 1.
pub fn noise(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut lp = 0.0f64;
    let mut v: Vec<f64> = (0..n)
        .map(|i| {
            lp = 0.85 * lp + 0.15 * rng.next();
            lp + 0.02 * (TAU * 50.0 * i as f64 / RATE as f64).sin()
        })
        .collect();
    let rms = (v.iter().map(|x| x * x).sum::<f64>() / n as f64).sqrt();
    for x in &mut v {
        *x /= rms;
    }
    v.into_iter().map(|x| x as f32).collect()
}

/// The RMS of `x`.
pub fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// `clean` plus `noise` scaled to `snr_db` below the voice's level.
pub fn mix(clean: &[f32], noise: &[f32], snr_db: f64) -> Vec<f32> {
    let k = rms(clean) / 10f64.powf(snr_db / 20.0) / rms(noise);
    clean
        .iter()
        .zip(noise)
        .map(|(c, n)| c + (k * f64::from(*n)) as f32)
        .collect()
}

/// The SNR in dB of `out`, `delay` samples late, against `clean`, over
/// `from..to` of `clean`.
pub fn snr_db(clean: &[f32], out: &[f32], delay: usize, from: usize, to: usize) -> f64 {
    let (mut s, mut e) = (0.0f64, 0.0f64);
    for i in from..to {
        let c = f64::from(clean[i]);
        let o = f64::from(out[i + delay]);
        s += c * c;
        e += (o - c).powi(2);
    }
    10.0 * (s / e).log10()
}

/// The lag in `0..max` at which `out` best matches `input` (normalised
/// cross-correlation).
pub fn best_lag(input: &[f32], out: &[f32], max: usize) -> usize {
    let n = input.len() - max;
    (0..max)
        .map(|lag| {
            let (mut xy, mut yy) = (0.0f64, 0.0f64);
            for i in 0..n {
                let y = f64::from(out[i + lag]);
                xy += f64::from(input[i]) * y;
                yy += y * y;
            }
            (lag, xy / yy.sqrt().max(1e-12))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
        .0
}

/// The indices of `clean`'s pauses (samples where a 10 ms window around
/// holds no voice), within `from..to`.
pub fn pauses(clean: &[f32], from: usize, to: usize) -> Vec<usize> {
    (from..to)
        .filter(|&i| {
            let a = i.saturating_sub(HOP);
            let b = (i + HOP).min(clean.len());
            clean[a..b].iter().all(|v| *v == 0.0)
        })
        .collect()
}
