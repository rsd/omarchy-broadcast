//! The module over the real NVIDIA Maxine Audio Effects SDK and GPU.
//! Ignored by default, and each test does nothing unless `MAXINE_AFX_ROOT`
//! names the SDK. The process must start as the worker's drop-in
//! (`omarchy-broadcast-audiod.service.d/maxine-afx.conf`) starts the
//! worker: with the package's environment file, whose `LD_LIBRARY_PATH`
//! leads to the feature libraries (the module requires it), and the SDK's
//! os-release shim preloaded where the distribution needs it:
//!
//! ```sh
//! cargo test --manifest-path crates/obc-module-afx/Cargo.toml --no-run
//! set -a; . /usr/share/maxine-afx/maxine-afx.env; set +a
//! env LD_PRELOAD=$MAXINE_AFX_OS_RELEASE_SHIM <the hardware test binary> --ignored --nocapture --test-threads=1
//! ```
//!
//! Each chain is loaded on one thread, run on another and unloaded on a
//! third, as the worker does, with the strengths passed live with each
//! block, as the worker passes them. The tests print what they measured (`AFX`
//! lines) and assert only what must hold anywhere: the stages load, the
//! output is finite and not silent, the delay reported is the delay the
//! output has, and every effect is destroyed. Measured figures of the SDK
//! are not to be published (its licence): they belong in private notes.

mod support;

use std::path::PathBuf;
use std::time::Instant;

use obc_backend::{Availability, Value};
use obc_module_afx::latency;
use obc_module_host::AudioBlock;
use serde_json::json;
use support::*;

fn root() -> Option<PathBuf> {
    std::env::var_os("MAXINE_AFX_ROOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `secs` of the probe's voice, repeated, with a low hiss.
fn noisy_voice(secs: usize) -> Vec<f32> {
    let one = latency::probe_signal(latency::VOICE_SAMPLES);
    let mut s = 0x2545_f491u64;
    (0..secs * 48_000)
        .map(|i| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            one[i % one.len()] + 0.02 * ((s % 2001) as f32 / 1000.0 - 1.0)
        })
        .collect()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

/// Loads `stages` with `params` on a thread, runs `noisy_voice(4)` through
/// it on another with the live parameters `live` (from the second second
/// on: the first runs at the load's strengths), unloads it on a third;
/// prints an `AFX` line.
fn exercise(
    module: &obc_module_host::Module,
    label: &str,
    stages: &[&str],
    params: &[(&str, &str, Value)],
    live: &[(&'static str, &'static str, Value)],
) {
    let mut inst = module.create_audio_instance().unwrap();
    let request = spec(stages, params);
    let t = Instant::now();
    let (o, inst2) = std::thread::spawn(move || {
        let o = inst.load(&request);
        (o, inst)
    })
    .join()
    .unwrap();
    let load_wall = t.elapsed().as_secs_f64() * 1e3;
    let o = o.unwrap_or_else(|e| panic!("{label}: {e:?}"));
    let latency_samples = o.latency_samples as usize;
    let x = noisy_voice(4);
    let x2 = x.clone();
    let live = live.to_vec();
    let (y, times, voice, inst3) = std::thread::spawn(move || {
        let mut inst = inst2;
        let mut out = vec![0.0f32; HOP];
        let (mut y, mut times, mut voice) = (Vec::new(), Vec::new(), Vec::new());
        let live: Vec<(&str, &str, &Value)> = live.iter().map(|(s, k, v)| (*s, *k, v)).collect();
        for (i, b) in x2.chunks(HOP).enumerate() {
            let params = if i * HOP < 48_000 { &[][..] } else { &live[..] };
            let t = Instant::now();
            let v = inst
                .process(&[b], &mut [&mut out], AudioBlock::default(), params)
                .unwrap();
            times.push(t.elapsed().as_secs_f64() * 1e3);
            voice.push(v);
            y.extend_from_slice(&out);
        }
        (y, times, voice, inst)
    })
    .join()
    .unwrap();
    std::thread::spawn(move || {
        let mut inst = inst3;
        inst.unload();
    })
    .join()
    .unwrap();
    assert!(y.iter().all(|v| v.is_finite()), "{label}: not finite");
    assert!(rms(&y[48_000..]) > 1e-4, "{label}: silent");
    // The lag the output has, over the second second (state settled).
    let lag = latency::estimate_lag(&x[48_000..], &y[48_000..], 48_000, latency::MAX_LAG);
    let mut sorted = times.clone();
    sorted.sort_by(f64::total_cmp);
    let known = voice.iter().filter(|v| v.is_some()).count();
    println!(
        "AFX {label}: load_ms={:.0} (wall {load_wall:.0}) warmup_ms={:?} latency_samples={latency_samples} \
         output_lag={lag:?} block_ms p50={:.3} p99={:.3} max={:.3} voice_probability={known} of {} blocks \
         rms_in={:.4} rms_out={:.4}",
        o.load_ms,
        o.warmup_ms,
        pct(&sorted, 0.5),
        pct(&sorted, 0.99),
        sorted.last().unwrap(),
        voice.len(),
        rms(&x[48_000..]),
        rms(&y[48_000..]),
    );
    if let Some(l) = lag {
        assert!(
            l.abs_diff(latency_samples) <= 64,
            "{label}: reported {latency_samples}, output lags {l}"
        );
    }
}

fn installed(module: &obc_module_host::Module, stage: &str) -> bool {
    module
        .probe()
        .unwrap()
        .iter()
        .any(|(s, a)| s.as_str() == stage && *a == Availability::Installed)
}

fn focus_installed(module: &obc_module_host::Module) -> bool {
    installed(module, FOCUS)
}

fn bandwidth_installed(module: &obc_module_host::Module) -> bool {
    installed(module, BANDWIDTH)
}

fn module() -> Option<obc_module_host::Module> {
    let root = root()?;
    let dir = tempfile::tempdir().unwrap();
    let module = open(dir.path(), json!({ "sdk_root": root }));
    std::mem::forget(dir);
    Some(module)
}

#[test]
#[ignore = "needs the NVIDIA Maxine Audio Effects SDK (MAXINE_AFX_ROOT) and an NVIDIA GPU"]
fn the_probe_and_the_check_find_every_stage() {
    let Some(module) = module() else { return };
    let probe = module.probe().unwrap();
    println!("AFX probe: {probe:?}");
    // Speaker focus (an Early Access feature) and bandwidth restoration
    // are not installed by the SDK by default: either installed or told so.
    assert!(
        probe.iter().all(|(s, a)| *a == Availability::Installed
            || ([FOCUS, BANDWIDTH].contains(&s.as_str())
                && *a == Availability::NotInstalled("Models not installed".into()))),
        "{probe:?}"
    );
    let mut stages = vec![s(DEREVERB), s(DENOISE), s(ENHANCE)];
    if focus_installed(&module) {
        stages.push(s(FOCUS));
    }
    if bandwidth_installed(&module) {
        stages.push(s(BANDWIDTH));
    }
    module.check(&stages).unwrap();
}

#[test]
#[ignore = "needs the NVIDIA Maxine Audio Effects SDK (MAXINE_AFX_ROOT) and an NVIDIA GPU"]
fn every_chain_loads_runs_and_reports_its_delay() {
    let Some(module) = module() else { return };
    exercise(&module, "denoise", &[DENOISE], &[], &[]);
    exercise(
        &module,
        "denoise live 100->40",
        &[DENOISE],
        &[],
        &[(DENOISE, "strength", Value::Int(40))],
    );
    exercise(
        &module,
        "dereverb live 100->50",
        &[DEREVERB],
        &[],
        &[(DEREVERB, "strength", Value::Int(50))],
    );
    exercise(
        &module,
        "dereverb+denoise (combined) live 100->80",
        &[DEREVERB, DENOISE],
        &[],
        &[(DENOISE, "strength", Value::Int(80))],
    );
    if focus_installed(&module) {
        exercise(&module, "focus", &[FOCUS], &[], &[]);
    }
    if bandwidth_installed(&module) {
        exercise(&module, "bandwidth", &[BANDWIDTH], &[], &[]);
    }
    exercise(&module, "enhance passthrough", &[ENHANCE], &[], &[]);
    exercise(
        &module,
        "enhance flat",
        &[ENHANCE],
        &[(ENHANCE, "profile", Value::Str("flat".into()))],
        &[],
    );
}

#[test]
#[ignore = "needs the NVIDIA Maxine Audio Effects SDK (MAXINE_AFX_ROOT) and an NVIDIA GPU"]
fn a_switch_in_place_keeps_the_effect() {
    let Some(module) = module() else { return };
    let mut inst = module.create_audio_instance().unwrap();
    let a = inst.load(&spec(&[ENHANCE], &[])).unwrap();
    let t = Instant::now();
    let b = inst
        .load(&spec(
            &[ENHANCE],
            &[(ENHANCE, "profile", Value::Str("warm".into()))],
        ))
        .unwrap();
    println!(
        "AFX switch enhance passthrough->warm: reused={} latency {} -> {} in {:.0} ms",
        b.reused_model,
        a.latency_samples,
        b.latency_samples,
        t.elapsed().as_secs_f64() * 1e3
    );
    assert!(b.reused_model);
    let x = noisy_voice(2);
    let mut out = vec![0.0f32; HOP];
    for blk in x.chunks(HOP) {
        inst.process(&[blk], &mut [&mut out], AudioBlock::default(), &[])
            .unwrap();
    }
    assert!(out.iter().all(|v| v.is_finite()));
    let c = inst
        .load(&spec(&[DENOISE], &[(DENOISE, "strength", Value::Int(60))]))
        .unwrap();
    assert!(!c.reused_model);
    inst.unload();
}
