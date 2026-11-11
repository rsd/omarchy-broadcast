//! The engines' quality and timing, on a speech-like voice and a noise made
//! in the test (see `common`): the noise removed, the voice kept, the delay
//! constant and as reported, the output the same on every run, and no step
//! in the output when the strength changes.

mod common;

use common::*;
use df::tract::{DfParams, DfTract, RuntimeParams};
use obc_module_voice_cpu::engine::{
    DF_THRESHOLDS, DeepFilter, Engine, EngineKind, RNNOISE_LATENCY, df_atten_db,
};

/// The output of `engine` for `input` (whole blocks), with the voice
/// probability of each block.
fn run(engine: &mut Engine, input: &[f32]) -> (Vec<f32>, Vec<Option<f32>>) {
    let mut out = vec![0.0; input.len()];
    let mut voice = Vec::new();
    for (i, o) in input
        .as_chunks::<HOP>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<HOP>().0.iter_mut())
    {
        voice.push(engine.process(i, o).unwrap());
    }
    (out, voice)
}

/// A noisy voice of `secs` seconds at `snr_db`, and its clean voice, both
/// padded with silence (0.1 s) so that the delayed tail comes out.
fn noisy(secs: f64, snr_db: f64) -> (Vec<f32>, Vec<f32>) {
    let mut clean = voice(secs, 7);
    let n = noise(clean.len(), 11);
    let mut x = mix(&clean, &n, snr_db);
    clean.extend(std::iter::repeat_n(0.0, 4800));
    x.extend(std::iter::repeat_n(0.0, 4800));
    (clean, x)
}

/// The level of the pauses of `out` (`delay` late) against `input`'s, in dB,
/// past the first second (the engines' adaptation).
fn pause_db(clean: &[f32], input: &[f32], out: &[f32], delay: usize) -> f64 {
    let p = pauses(clean, RATE, clean.len() - 4800);
    assert!(p.len() > RATE / 4, "{} pause samples", p.len());
    let level = |x: &[f32], d: usize| {
        (p.iter().map(|&i| f64::from(x[i + d]).powi(2)).sum::<f64>() / p.len() as f64).sqrt()
    };
    20.0 * (level(out, delay) / level(input, 0)).log10()
}

/// The SNR of `input` and of `out` (`delay` late), past the first second.
fn snrs(clean: &[f32], input: &[f32], out: &[f32], delay: usize) -> (f64, f64) {
    let to = clean.len() - 4800;
    (
        snr_db(clean, input, 0, RATE, to),
        snr_db(clean, out, delay, RATE, to),
    )
}

#[test]
fn deepfilternet_removes_the_noise_and_keeps_the_voice() {
    let (clean, x) = noisy(4.0, 5.0);
    let mut e = Engine::new(EngineKind::DeepFilterNet, 100).unwrap();
    let (y, voice) = run(&mut e, &x);
    let d = e.latency_samples() as usize;
    let (before, after) = snrs(&clean, &x, &y, d);
    let pause = pause_db(&clean, &x, &y, d);
    println!("DeepFilterNet: SNR {before:.2} -> {after:.2} dB, pauses {pause:.1} dB");
    // Measured: 5.7 -> 13.1 dB, pauses -31 dB.
    assert!(after - before >= 5.0, "SNR {before:.2} -> {after:.2}");
    assert!(pause <= -20.0, "pauses {pause:.1} dB");
    assert!(voice.iter().all(Option::is_none), "no voice probability");
}

#[test]
fn rnnoise_removes_the_noise_and_tells_the_voice() {
    let (clean, x) = noisy(4.0, 5.0);
    let mut e = Engine::new(EngineKind::RnNoise, 100).unwrap();
    let (y, voice) = run(&mut e, &x);
    let d = e.latency_samples() as usize;
    let (before, after) = snrs(&clean, &x, &y, d);
    let pause = pause_db(&clean, &x, &y, d);
    println!("RNNoise: SNR {before:.2} -> {after:.2} dB, pauses {pause:.1} dB");
    // Measured: 5.7 -> 10.9 dB, pauses -18 dB.
    assert!(after - before >= 3.0, "SNR {before:.2} -> {after:.2}");
    assert!(pause <= -12.0, "pauses {pause:.1} dB");
    // The voice probability: higher in the syllables than in the pauses.
    let (mut in_voice, mut in_pause) = (Vec::new(), Vec::new());
    for (b, v) in voice.iter().enumerate().skip(100) {
        let block = &clean[b * HOP..(b + 1) * HOP];
        let v = v.expect("RNNoise tells the voice probability");
        assert!((0.0..=1.0).contains(&v));
        if block.iter().all(|s| *s == 0.0) {
            in_pause.push(v);
        } else if rms(block) > 0.02 {
            in_voice.push(v);
        }
    }
    assert!(in_voice.len() > 50 && in_pause.len() > 20);
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    assert!(
        mean(&in_voice) > mean(&in_pause) + 0.3,
        "voice {} pause {}",
        mean(&in_voice),
        mean(&in_pause)
    );
}

#[test]
fn the_strength_sets_how_much_noise_goes() {
    let (clean, x) = noisy(3.0, 5.0);
    let pause_at = |kind, s| {
        let mut e = Engine::new(kind, s).unwrap();
        let (y, _) = run(&mut e, &x);
        pause_db(&clean, &x, &y, e.latency_samples() as usize)
    };
    // DeepFilterNet: the attenuation limit, 1 dB, 10 dB, none.
    let df: Vec<f64> = [0, 50, 100]
        .map(|s| pause_at(EngineKind::DeepFilterNet, s))
        .into();
    println!("DeepFilterNet pauses at 0/50/100 %: {df:.1?} dB");
    assert!((df[0] + f64::from(df_atten_db(0))).abs() < 0.5, "{df:?}");
    assert!((df[1] + f64::from(df_atten_db(50))).abs() < 2.0, "{df:?}");
    assert!(df[2] < -20.0, "{df:?}");
    // RNNoise: the wet/dry mix; at 0 % the input itself, one block late.
    let rn: Vec<f64> = [50, 100].map(|s| pause_at(EngineKind::RnNoise, s)).into();
    println!("RNNoise pauses at 50/100 %: {rn:.1?} dB");
    assert!(rn[0] < -3.0 && rn[1] < rn[0] - 6.0, "{rn:?}");
    let mut e = Engine::new(EngineKind::RnNoise, 0).unwrap();
    let (y, _) = run(&mut e, &x);
    assert_eq!(y[HOP..], x[..x.len() - HOP], "a delayed passthrough");
}

#[test]
fn deepfilternet_runs_with_stage_skipping_off() {
    let e = DeepFilter::new(100).unwrap();
    assert_eq!(e.thresholds(), DF_THRESHOLDS);
    assert_eq!(DF_THRESHOLDS, (-100.0, 100.0, 100.0));

    // Why: with libDF's defaults, a voice 20 dB above the noise comes out
    // far worse than it went in (frames the model takes for clean speech
    // skip processing); with skipping off it comes out better.
    let (clean, x) = noisy(4.0, 20.0);
    let mut ours = Engine::new(EngineKind::DeepFilterNet, 100).unwrap();
    let d = ours.latency_samples() as usize;
    let (y, _) = run(&mut ours, &x);
    let mut defaults =
        DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(1)).unwrap();
    assert_ne!(
        (
            defaults.min_db_thresh,
            defaults.max_db_erb_thresh,
            defaults.max_db_df_thresh
        ),
        DF_THRESHOLDS
    );
    let mut z = vec![0.0f32; x.len()];
    for (i, o) in x
        .as_chunks::<HOP>()
        .0
        .iter()
        .zip(z.as_chunks_mut::<HOP>().0.iter_mut())
    {
        defaults
            .process(
                ndarray::ArrayView2::from_shape((1, HOP), i).unwrap(),
                ndarray::ArrayViewMut2::from_shape((1, HOP), o).unwrap(),
            )
            .unwrap();
    }
    let (before, after) = snrs(&clean, &x, &y, d);
    let (_, with_defaults) = snrs(&clean, &x, &z, d);
    println!("SNR {before:.2}: ours {after:.2} dB, libDF defaults {with_defaults:.2} dB");
    // Measured: 20.7 in, 23.7 ours, 4.4 with the defaults.
    assert!(after > before, "ours {after:.2} <= input {before:.2}");
    assert!(
        with_defaults < before - 6.0,
        "the defaults no longer harm: {with_defaults:.2}"
    );
}

#[test]
fn the_output_is_the_same_on_every_run_and_after_a_reset() {
    let (_, x) = noisy(2.0, 5.0);
    for kind in EngineKind::ALL {
        let mut a = Engine::new(kind, 80).unwrap();
        let (ya, va) = run(&mut a, &x);
        let mut b = Engine::new(kind, 80).unwrap();
        let (yb, vb) = run(&mut b, &x);
        assert!(ya == yb && va == vb, "{kind:?}: two runs differ");
        // A reset goes back to the start: the same output again.
        a.reset();
        let (yr, vr) = run(&mut a, &x);
        assert!(
            ya == yr && va == vr,
            "{kind:?}: the run after a reset differs"
        );
    }
}

#[test]
fn the_delay_is_the_reported_latency_at_every_strength() {
    let (_, x) = noisy(2.0, 5.0);
    for kind in EngineKind::ALL {
        for s in [0, 50, 100] {
            let mut e = Engine::new(kind, s).unwrap();
            let (y, _) = run(&mut e, &x);
            let lag = best_lag(&x, &y, 3000);
            assert_eq!(
                lag as u32,
                e.latency_samples(),
                "{kind:?} at {s} %: measured {lag}"
            );
        }
    }
    let df = Engine::new(EngineKind::DeepFilterNet, 100).unwrap();
    // 960-sample window less a 480 hop, plus two lookahead frames: 30 ms.
    assert_eq!(df.latency_samples(), 1440);
    assert_eq!(RNNOISE_LATENCY, 480);
}

#[test]
fn deepfilternet_keeps_its_delay_across_digital_silence() {
    // A voice, a second of digital silence (libDF alone stops there: it
    // returns zeros without moving its buffers), then clicks.
    let (_, x) = noisy(2.0, 20.0);
    let mut z = x[..RATE * 2].to_vec();
    z.extend(std::iter::repeat_n(0.0, RATE));
    let mut clicks = vec![0.0f32; RATE];
    for k in 0..10 {
        clicks[k * 4800 + 100] = 0.5;
    }
    z.extend(&clicks);
    let mut e = Engine::new(EngineKind::DeepFilterNet, 0).unwrap();
    let (y, _) = run(&mut e, &z);
    let from = RATE * 3;
    assert_eq!(best_lag(&z[from..], &y[from..], 3000), 1440);
    // A quiet room (the noise under libDF's threshold in the pauses) keeps
    // the voice: measured 28.7 dB out of 41 (1.2 dB without the dither, the
    // voice out of step with itself after each pause).
    let (clean, x) = noisy(3.0, 40.0);
    let mut e = Engine::new(EngineKind::DeepFilterNet, 100).unwrap();
    let (y, _) = run(&mut e, &x);
    let (before, after) = snrs(&clean, &x, &y, 1440);
    println!("quiet room: SNR {before:.2} -> {after:.2} dB");
    assert!(after > 25.0, "SNR {before:.2} -> {after:.2}");
}

#[test]
fn a_non_finite_sample_never_poisons_the_output() {
    let (_, x) = noisy(1.0, 10.0);
    for kind in EngineKind::ALL {
        let mut e = Engine::new(kind, 100).unwrap();
        let (_, _) = run(&mut e, &x);
        let mut bad = [0.0f32; HOP];
        bad.copy_from_slice(&x[..HOP]);
        bad[10] = f32::NAN;
        bad[200] = f32::INFINITY;
        bad[300] = f32::NEG_INFINITY;
        let mut out = [0.0f32; HOP];
        // Within the block: finite output, or a reported error.
        if e.process(&bad, &mut out).is_ok() {
            assert!(out.iter().all(|v| v.is_finite()), "{kind:?}: {out:?}");
        }
        // And the next blocks are sound, for far longer than the poison of
        // a NaN in the model's state would last.
        let (y, _) = run(&mut e, &x[..RATE]);
        assert!(y.iter().all(|v| v.is_finite()), "{kind:?}");
        assert!(rms(&y[RATE / 2..]) > 0.0, "{kind:?}: silent after a NaN");
    }
}

#[test]
fn digital_silence_in_is_exact_zeros_out_and_the_delay_stays() {
    let (_, x) = noisy(1.0, 20.0);
    for s in [0, 50, 100] {
        let mut z = vec![0.0f32; RATE];
        z.extend(&x[..RATE]);
        let mut e = Engine::new(EngineKind::DeepFilterNet, s).unwrap();
        let (y, _) = run(&mut e, &z);
        assert!(
            y[..RATE].iter().all(|v| *v == 0.0),
            "{s} %: not exact zeros in the silence"
        );
        // The speech after the silence is as late as ever.
        assert_eq!(best_lag(&z[RATE..], &y[RATE..], 3000), 1440, "{s} %");
        // The silence after speech: zeros again once the delay is out.
        let mut w = x[..RATE].to_vec();
        w.extend(vec![0.0f32; RATE]);
        let mut e = Engine::new(EngineKind::DeepFilterNet, s).unwrap();
        let (y, _) = run(&mut e, &w);
        assert!(
            y[RATE + 4 * HOP..].iter().all(|v| *v == 0.0),
            "{s} %: not zeros after the voice"
        );
    }
}

/// The largest step between two samples in `x[from..to]`.
fn largest_step(x: &[f32], from: usize, to: usize) -> f32 {
    x[from..to]
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn a_live_strength_change_makes_no_step() {
    let (_, x) = noisy(3.0, 5.0);
    let switch = 150;
    for kind in EngineKind::ALL {
        for (s0, s1) in [(100, 0), (0, 100), (100, 30)] {
            let mut a = Engine::new(kind, s0).unwrap();
            let (ya, _) = run(&mut a, &x);
            let mut b = Engine::new(kind, s1).unwrap();
            let (yb, _) = run(&mut b, &x);
            let mut e = Engine::new(kind, s0).unwrap();
            let mut ys = vec![0.0f32; x.len()];
            for (n, (i, o)) in x
                .as_chunks::<HOP>()
                .0
                .iter()
                .zip(ys.as_chunks_mut::<HOP>().0.iter_mut())
                .enumerate()
            {
                if n == switch {
                    e.set_strength(s1);
                }
                e.process(i, o).unwrap();
            }
            let at = switch * HOP;
            assert_eq!(ys[..at], ya[..at], "{kind:?}: changed before the switch");
            // The window where the change comes out, the delay included.
            let (from, to) = (at - HOP, at + 6 * HOP);
            let step = largest_step(&ys, from, to);
            let bound = largest_step(&ya, from, to).max(largest_step(&yb, from, to));
            assert!(
                step <= 1.1 * bound + 1e-4,
                "{kind:?} {s0}->{s1}: a step of {step} where the runs have {bound}"
            );
            // Then it runs as if it had started at the new strength.
            let late = at + 8 * HOP;
            let diff = ys[late..]
                .iter()
                .zip(&yb[late..])
                .map(|(p, q)| (p - q).abs())
                .fold(0.0, f32::max);
            assert!(diff < 1e-3, "{kind:?} {s0}->{s1}: {diff} from the new run");
        }
    }
}

#[test]
fn blocks_of_another_size_are_refused() {
    let mut e = Engine::new(EngineKind::RnNoise, 100).unwrap();
    let mut out = vec![0.0; 960];
    assert!(e.process(&[0.0; 960], &mut out).is_err());
}

/// The CPU an engine takes at real time: blocks paced every 10 ms for
/// `OBC_VOICE_CPU_SECS` seconds (default 10), the process's CPU time over
/// the wall time. Run in release: `cargo test --release -p
/// obc-module-voice-cpu --test quality -- --ignored --nocapture cpu_at`.
#[test]
#[ignore = "a measurement; run in release"]
fn cpu_at_real_time() {
    use std::time::{Duration, Instant};
    fn cpu_s() -> f64 {
        let s = std::fs::read_to_string("/proc/self/stat").unwrap();
        let f: Vec<&str> = s[s.rfind(')').unwrap() + 2..].split_whitespace().collect();
        (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / 100.0
    }
    let secs: f64 = std::env::var("OBC_VOICE_CPU_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10.0);
    let (_, x) = noisy(4.0, 5.0);
    for kind in EngineKind::ALL {
        let mut e = Engine::new(kind, 100).unwrap();
        let blocks = (secs * 100.0) as usize;
        let mut out = [0.0f32; HOP];
        let mut times = Vec::with_capacity(blocks);
        let (c0, w0) = (cpu_s(), Instant::now());
        let mut next = Instant::now();
        for n in 0..blocks {
            next += Duration::from_millis(10);
            let at = (n * HOP) % (x.len() - HOP);
            let at = at - at % HOP;
            let t = Instant::now();
            e.process(&x[at..at + HOP], &mut out).unwrap();
            times.push(t.elapsed().as_secs_f64() * 1e3);
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        let cpu = 100.0 * (cpu_s() - c0) / w0.elapsed().as_secs_f64();
        times.sort_by(f64::total_cmp);
        println!(
            "{kind:?}: {cpu:.1} % of a core at real time; block p50 {:.3} ms, p99 {:.3} ms",
            times[times.len() / 2],
            times[times.len() * 99 / 100]
        );
    }
}
