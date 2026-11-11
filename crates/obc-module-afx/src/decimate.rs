//! The 48 kHz to 16 kHz decimator in front of an effect that takes 16 kHz
//! input (bandwidth extension, [`crate::tree::Effect::SuperRes`]).
//!
//! A linear-phase low-pass FIR (a windowed sinc: [`TAPS`] taps, a Kaiser
//! window of shape [`BETA`], half amplitude at [`CUTOFF_HZ`]) removes what
//! would fold below 8 kHz, then every [`FACTOR`]rd sample is kept. The
//! filter's state is carried from one block to the next, so a stream cut
//! into blocks comes out as it would in one piece. Its group delay is
//! [`DELAY`] samples at 48 kHz.

/// The decimation factor: 48 kHz to 16 kHz.
pub const FACTOR: usize = 3;

/// The number of taps of the low-pass filter (odd: linear phase with a
/// whole-sample delay).
pub const TAPS: usize = 121;

/// The filter's group delay, in input (48 kHz) samples.
pub const DELAY: usize = (TAPS - 1) / 2;

/// The filter's cutoff (half amplitude), in Hz.
pub const CUTOFF_HZ: f64 = 7_200.0;

/// The shape of the Kaiser window: about 70 dB of stopband attenuation.
pub const BETA: f64 = 7.0;

/// The input sample rate.
const RATE: f64 = 48_000.0;

/// The zeroth-order modified Bessel function of the first kind (its power
/// series, to double precision for the window's arguments).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0f64, 1.0f64, 1.0f64);
    let q = x * x / 4.0;
    while term > sum * 1e-17 {
        term *= q / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

/// The taps of the low-pass filter, normalised to a gain of one at 0 Hz.
pub fn taps() -> Vec<f32> {
    let m = DELAY as f64;
    let fc = CUTOFF_HZ / RATE;
    let norm = bessel_i0(BETA);
    let h: Vec<f64> = (0..TAPS)
        .map(|n| {
            let t = n as f64 - m;
            let sinc = if t == 0.0 {
                2.0 * fc
            } else {
                (std::f64::consts::TAU * fc * t).sin() / (std::f64::consts::PI * t)
            };
            let r = t / m;
            sinc * bessel_i0(BETA * (1.0 - r * r).max(0.0).sqrt()) / norm
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter().map(|v| (v / sum) as f32).collect()
}

/// A 48 kHz to 16 kHz decimator with its state.
#[derive(Debug, Clone)]
pub struct Decimator {
    taps: Vec<f32>,
    /// The last `TAPS - 1` inputs, then the block being filtered.
    buf: Vec<f32>,
    /// The input samples to skip before the next kept one.
    phase: usize,
}

impl Default for Decimator {
    fn default() -> Self {
        Decimator::new()
    }
}

impl Decimator {
    /// A decimator whose past input is silence.
    pub fn new() -> Decimator {
        let mut buf = Vec::with_capacity(TAPS - 1 + 4096);
        buf.resize(TAPS - 1, 0.0);
        Decimator {
            taps: taps(),
            buf,
            phase: 0,
        }
    }

    /// Silence again.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf.resize(TAPS - 1, 0.0);
        self.phase = 0;
    }

    /// Filters `input` (48 kHz) and writes every third sample into `out`
    /// (cleared first). With blocks that are multiples of [`FACTOR`] (the
    /// module's 480), each block gives a third as many samples.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        self.buf.extend_from_slice(input);
        let mut i = self.phase;
        while i < input.len() {
            // The window ending at input sample `i`.
            let window = &self.buf[i..i + TAPS];
            // Symmetric taps: the reversed window is the same product.
            let y: f32 = window.iter().zip(&self.taps).map(|(x, h)| x * h).sum();
            out.push(y);
            i += FACTOR;
        }
        self.phase = i - input.len();
        let keep = self.buf.len() - (TAPS - 1);
        self.buf.drain(..keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, n: usize, amp: f64) -> Vec<f32> {
        (0..n)
            .map(|i| (amp * (std::f64::consts::TAU * hz * i as f64 / RATE).sin()) as f32)
            .collect()
    }

    fn decimate(x: &[f32]) -> Vec<f32> {
        let mut d = Decimator::new();
        let mut out = Vec::new();
        d.process(x, &mut out);
        out
    }

    /// The RMS of `y` past the filter's start-up.
    fn rms_settled(y: &[f32]) -> f64 {
        let y = &y[TAPS..];
        (y.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / y.len() as f64).sqrt()
    }

    #[test]
    fn the_filter_is_symmetric_with_unit_gain_at_dc() {
        let h = taps();
        assert_eq!(h.len(), TAPS);
        for k in 0..TAPS {
            assert!((h[k] - h[TAPS - 1 - k]).abs() < 1e-9, "tap {k}");
        }
        let sum: f32 = h.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5, "{sum}");
        assert_eq!(DELAY % FACTOR, 0, "a whole number of output samples");
    }

    #[test]
    fn a_1_khz_tone_keeps_its_level_and_comes_out_delayed_by_the_group_delay() {
        let amp = 0.5;
        let x = sine(1_000.0, 48_000, amp);
        let y = decimate(&x);
        assert_eq!(y.len(), 16_000);
        let db = 20.0 * (rms_settled(&y) / (amp / 2f64.sqrt())).log10();
        assert!(db.abs() < 0.1, "{db} dB");
        // Output sample m is the input at 3m, DELAY samples late.
        for m in (DELAY / FACTOR + 10)..y.len() {
            let want = x[FACTOR * m - DELAY];
            assert!((y[m] - want).abs() < 1e-3, "{m}: {} vs {want}", y[m]);
        }
    }

    #[test]
    fn a_10_khz_tone_is_attenuated_by_more_than_60_db() {
        let amp = 0.5;
        let y = decimate(&sine(10_000.0, 48_000, amp));
        let db = 20.0 * (rms_settled(&y) / (amp / 2f64.sqrt())).log10();
        assert!(db < -60.0, "{db} dB");
        // And everything that would fold onto the band kept.
        for hz in [8_800.0, 12_000.0, 16_000.0, 23_000.0] {
            let y = decimate(&sine(hz, 48_000, amp));
            let db = 20.0 * (rms_settled(&y) / (amp / 2f64.sqrt())).log10();
            assert!(db < -60.0, "{hz} Hz: {db} dB");
        }
    }

    #[test]
    fn blocks_come_out_as_one_piece_would() {
        let x: Vec<f32> = sine(440.0, 4_800, 0.3)
            .iter()
            .zip(sine(9_000.0, 4_800, 0.2))
            .map(|(a, b)| a + b)
            .collect();
        let whole = decimate(&x);
        let mut d = Decimator::new();
        let mut out = Vec::new();
        let mut blocks = Vec::new();
        for b in x.chunks(480) {
            d.process(b, &mut out);
            assert_eq!(out.len(), 160);
            blocks.extend_from_slice(&out);
        }
        assert_eq!(blocks, whole);
        // Blocks that are not multiples of three keep the phase too.
        let mut d = Decimator::new();
        let mut odd = Vec::new();
        for b in x.chunks(7) {
            d.process(b, &mut out);
            odd.extend_from_slice(&out);
        }
        assert_eq!(odd.len(), whole.len());
        for (a, b) in odd.iter().zip(&whole) {
            assert!((a - b).abs() < 1e-6);
        }
        // A reset forgets the past input.
        d.reset();
        d.process(&x[..480], &mut out);
        assert_eq!(out, whole[..160]);
    }
}
