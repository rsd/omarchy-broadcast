//! Hardware tests for the Maxine backend. All are `#[ignore]`: they need the
//! NVIDIA Maxine Video Effects SDK (`MAXINE_VFX_ROOT`, its library
//! directories on `LD_LIBRARY_PATH`), the os-release override on an unlisted
//! distribution (the `tools/os-release-shim` preload library), and an NVIDIA
//! GPU. Without them they fail; no test passes without running. From the
//! repository root, after `make -C tools/os-release-shim`:
//!
//! ```text
//! export MAXINE_VFX_ROOT=~/.local/opt/maxine/VideoFX
//! source tools/maxine-probe/env.sh
//! export OMA_BROADCAST_OS_RELEASE=$PWD/tools/maxine-probe/fake-os-release
//! CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$PWD/target/os-release-shim/os-release-shim.so" \
//!   cargo test -p obc-backend-maxine --test hardware -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! The shim is preloaded through the cargo runner so that only the test
//! binary gets it: `LD_PRELOAD=... cargo test` would be dropped by cargo.
//!
//! The frames are synthetic, so the mask the model produces for them is not
//! predictable: the tests assert what holds for any mask and log the rest
//! (mask coverage, per-stage GPU times, load times, GPU memory).

use std::sync::Arc;
use std::time::Instant;

use obc_backend::{
    BackendError, BackendFactory, ColourSpec, EffectKind, FrameParams, GpuContext, HostBgr,
    InferenceBackend, LoadRequest, StageTimings,
};
use obc_backend_maxine::{MaxineBackend, MaxineFactory, ROOT_ENV};
use obc_gpu::{
    ContextGuard, DeviceNv12Buffer, OwnedContext, Stream, mem_get_info, primary_context_active,
};

const W: u32 = 1920;
const H: u32 = 1080;
const FRAMES: usize = 100;
const MIB: f64 = 1024.0 * 1024.0;

fn factory() -> MaxineFactory {
    assert!(
        std::env::var_os(ROOT_ENV).is_some_and(|v| !v.is_empty()),
        "this test needs the Maxine SDK: set {ROOT_ENV} to its directory and put its library \
         directories on LD_LIBRARY_PATH (source tools/maxine-probe/env.sh)"
    );
    let f = MaxineFactory::from_env();
    f.probe()
        .unwrap_or_else(|e| panic!("Maxine probe failed: {e}"));
    f
}

/// BT.601 limited-range luma of a BGR pixel.
fn luma(b: u8, g: u8, r: u8) -> f64 {
    16.0 + (65.481 * r as f64 + 128.553 * g as f64 + 24.966 * b as f64) / 255.0
}

/// BT.601 limited-range NV12 of a packed BGR image.
fn bgr_to_nv12(bgr: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3 / 2];
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        (bgr[i] as f64, bgr[i + 1] as f64, bgr[i + 2] as f64)
    };
    for y in 0..h {
        for x in 0..w {
            let (b, g, r) = px(x, y);
            out[y * w + x] = luma(b as u8, g as u8, r as u8).round().clamp(0.0, 255.0) as u8;
        }
    }
    let uv = &mut out[w * h..];
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let (b, g, r) = px(2 * x, 2 * y);
            let u = 128.0 + (-37.797 * r - 74.203 * g + 112.0 * b) / 255.0;
            let v = 128.0 + (112.0 * r - 93.786 * g - 18.214 * b) / 255.0;
            uv[y * w + 2 * x] = u.round().clamp(0.0, 255.0) as u8;
            uv[y * w + 2 * x + 1] = v.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// A rough head-and-shoulders silhouette in a skin tone over a grey-blue
/// gradient room, packed BGR.
fn synthetic_person() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut bgr = vec![0u8; w * h * 3];
    let (cx, head_cy, head_r) = (w as f64 / 2.0, h as f64 * 0.38, h as f64 * 0.17);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f64, y as f64);
            let head =
                ((fx - cx) / (head_r * 0.8)).powi(2) + ((fy - head_cy) / head_r).powi(2) <= 1.0;
            let shoulders = fy > h as f64 * 0.58
                && (fx - cx).abs() < w as f64 * 0.22 + (fy - h as f64 * 0.58) * 0.6;
            let i = (y * w + x) * 3;
            let px: [u8; 3] = if head {
                [90, 130, 200] // skin (B, G, R)
            } else if shoulders {
                [60, 40, 30] // dark shirt
            } else {
                [(150 + x * 60 / w) as u8, (140 + y * 40 / h) as u8, 120]
            };
            bgr[i..i + 3].copy_from_slice(&px);
        }
    }
    bgr
}

/// A distinct background: an orange-to-green ramp, packed BGR.
fn background() -> HostBgr {
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            data[i] = 20;
            data[i + 1] = (60 + x * 150 / w) as u8;
            data[i + 2] = (230 - y * 150 / h) as u8;
        }
    }
    HostBgr {
        width: W,
        height: H,
        stride: w * 3,
        data,
    }
}

fn percentile(v: &mut [f32], p: f64) -> f32 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// Runs `FRAMES` frames, synchronising after each so every frame's stage
/// times are read, and logs p50/p95 per stage and of the wall time.
fn run_frames(
    label: &str,
    backend: &mut MaxineBackend,
    gpu: &GpuContext,
    stream: &Stream,
    params: FrameParams,
    src: &DeviceNv12Buffer,
    dst: &DeviceNv12Buffer,
) -> Vec<StageTimings> {
    let mut all = Vec::with_capacity(FRAMES);
    let mut wall = Vec::with_capacity(FRAMES);
    for _ in 0..FRAMES {
        let t = Instant::now();
        backend
            .process(gpu, &params, &src.frame(), &dst.frame())
            .unwrap_or_else(|e| panic!("{label}: process failed: {e}"));
        stream.synchronize().unwrap();
        wall.push(t.elapsed().as_secs_f32() * 1000.0);
        all.push(
            backend
                .timings()
                .expect("timings after a synchronised frame"),
        );
    }
    let col = |f: fn(&StageTimings) -> f32| all.iter().map(f).collect::<Vec<f32>>();
    let mut stages = [
        ("convert_in", col(|t| t.convert_in_ms)),
        ("mask", col(|t| t.mask_ms)),
        ("effect", col(|t| t.effect_ms)),
        ("convert_out", col(|t| t.convert_out_ms)),
        ("gpu_total", col(|t| t.total_ms())),
        ("wall", wall),
    ];
    for (name, v) in stages.iter_mut() {
        let p50 = percentile(v, 0.5);
        let p95 = percentile(v, 0.95);
        println!("TIMING {label} {name}: p50 {p50:.3} ms, p95 {p95:.3} ms ({FRAMES} frames)");
    }
    all
}

fn free_mib() -> f64 {
    mem_get_info().unwrap().0 as f64 / MIB
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK (MAXINE_VFX_ROOT), the os-release override and an NVIDIA GPU"]
fn maxine_rejects_calls_without_the_context_or_a_load() {
    let f = factory();
    let ctx = OwnedContext::create(0).unwrap();
    let mut backend = f.create_maxine().unwrap();
    let gpu = GpuContext {
        context: ctx.raw(),
        stream: std::ptr::null_mut(),
    };
    let req = LoadRequest {
        effect: EffectKind::Blur,
        width: W,
        height: H,
        colour: ColourSpec::LIMITED_BT601,
        background: None,
    };
    // Not pushed: the backend must refuse rather than run in no context.
    let e = backend.load(&gpu, &req).unwrap_err();
    assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
    {
        // SAFETY: `ctx` outlives the guard.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let buf = DeviceNv12Buffer::alloc(W, H).unwrap();
        let e = backend
            .process(&gpu, &FrameParams::default(), &buf.frame(), &buf.frame())
            .unwrap_err();
        assert!(e.to_string().contains("no effect loaded"), "{e}");
        assert!(backend.timings().is_none());
        backend.unload();
    }
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK (MAXINE_VFX_ROOT), the os-release override and an NVIDIA GPU"]
fn maxine_blur_and_replace_in_a_created_context() {
    let f = factory();
    println!("model dir: {}", f.model_dir().unwrap().display());
    assert!(
        !primary_context_active(0).unwrap(),
        "the primary context is active before the test"
    );
    // GStreamer's CUDA library creates its context with cuCtxCreate; so do we.
    let ctx = OwnedContext::create(0).unwrap();
    {
        // SAFETY: `ctx` outlives the guard; everything below drops first.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let stream = Stream::new(true).unwrap();
        let gpu = GpuContext {
            context: ctx.raw(),
            stream: stream.raw(),
        };
        let person = synthetic_person();
        let input = bgr_to_nv12(&person, W as usize, H as usize);
        let src = DeviceNv12Buffer::alloc(W, H).unwrap();
        let dst = DeviceNv12Buffer::alloc(W, H).unwrap();
        src.upload_packed(&input).unwrap();
        let bg = background();
        let bg_nv12 = bgr_to_nv12(&bg.data, W as usize, H as usize);
        let bg = Arc::new(bg);

        let mem_before = free_mib();
        let mut backend = f.create_maxine().unwrap();

        // Blur: a cold load (models + warm-up run).
        let blur_req = LoadRequest {
            effect: EffectKind::Blur,
            width: W,
            height: H,
            colour: ColourSpec::LIMITED_BT601,
            background: None,
        };
        let t = Instant::now();
        let r = backend
            .load(&gpu, &blur_req)
            .unwrap_or_else(|e| panic!("blur load failed: {e}"));
        println!(
            "LOAD blur (cold): load {:.1} ms, warm-up {:?} ms, reused {}, wall {:.1} ms",
            r.load_ms,
            r.warmup_ms,
            r.reused_model,
            t.elapsed().as_secs_f64() * 1000.0
        );
        assert!(!r.reused_model);
        assert!(r.warmup_ms.is_some());
        let mem_loaded = free_mib();

        run_frames(
            "blur",
            &mut backend,
            &gpu,
            &stream,
            FrameParams { blur_strength: 0.5 },
            &src,
            &dst,
        );
        let out = dst.download_packed().unwrap();
        let src_after = src.download_packed().unwrap();
        assert_eq!(src_after, input, "process() wrote its source");
        assert_ne!(out, input, "blur output equals the input");
        let ys = &out[..(W * H) as usize];
        assert!(ys.iter().any(|&y| y > 40), "blur output is black");
        let mask = backend.read_mask().unwrap();
        let fg = mask.iter().filter(|&&m| m >= 250).count() as f64 / mask.len() as f64;
        let bgc = mask.iter().filter(|&&m| m <= 5).count() as f64 / mask.len() as f64;
        println!(
            "MASK blur: {:.1}% foreground (>=250), {:.1}% background (<=5)",
            fg * 100.0,
            bgc * 100.0
        );

        // Strength changes apply on the next frame, without a reload.
        let mut by_strength = Vec::new();
        for s in [0.0f32, 1.0] {
            for _ in 0..3 {
                backend
                    .process(
                        &gpu,
                        &FrameParams { blur_strength: s },
                        &src.frame(),
                        &dst.frame(),
                    )
                    .unwrap();
            }
            stream.synchronize().unwrap();
            by_strength.push(dst.download_packed().unwrap());
        }
        let differing = by_strength[0]
            .iter()
            .zip(&by_strength[1])
            .filter(|(a, b)| a != b)
            .count();
        let y_n = (W * H) as usize;
        let mean_diff = by_strength[0][..y_n]
            .iter()
            .zip(&input[..y_n])
            .map(|(a, b)| (*a as f64 - *b as f64).abs())
            .sum::<f64>()
            / y_n as f64;
        println!(
            "STRENGTH 0.0 vs 1.0 without reload: {differing} bytes differ; \
             strength 0.0 mean |Y - input| {mean_diff:.2}"
        );
        assert!(
            differing > 0,
            "a strength change without reload had no effect"
        );

        // Replace: reuses the green-screen model.
        let replace_req = LoadRequest {
            effect: EffectKind::Replace,
            background: Some(bg.clone()),
            ..blur_req.clone()
        };
        let r = backend
            .load(&gpu, &replace_req)
            .unwrap_or_else(|e| panic!("replace load failed: {e}"));
        println!(
            "LOAD replace (switch): load {:.2} ms, warm-up {:?}, reused {}",
            r.load_ms, r.warmup_ms, r.reused_model
        );
        assert!(
            r.reused_model,
            "switching blur -> replace reloaded the model"
        );
        assert_eq!(backend.loaded_effect(), Some(EffectKind::Replace));
        run_frames(
            "replace",
            &mut backend,
            &gpu,
            &stream,
            FrameParams::default(),
            &src,
            &dst,
        );
        let out = dst.download_packed().unwrap();
        assert_ne!(out, input, "replace output equals the input");
        assert!(
            out[..(W * H) as usize].iter().any(|&y| y > 40),
            "replace output is black"
        );
        let mask = backend.read_mask().unwrap();
        let (mut n_bg, mut err_bg, mut n_fg, mut err_fg) = (0usize, 0f64, 0usize, 0f64);
        for (i, &m) in mask.iter().enumerate() {
            let y = out[i] as f64;
            if m == 0 {
                n_bg += 1;
                err_bg += (y - bg_nv12[i] as f64).abs();
            } else if m == 255 {
                n_fg += 1;
                err_fg += (y - input[i] as f64).abs();
            }
        }
        let mean = |e: f64, n: usize| if n == 0 { f64::NAN } else { e / n as f64 };
        println!(
            "REPLACE: {n_bg} px with mask 0 (mean |Y - bg| {:.2}), {n_fg} px with mask 255 \
             (mean |Y - input| {:.2})",
            mean(err_bg, n_bg),
            mean(err_fg, n_fg)
        );
        if n_bg > 0 {
            assert!(
                mean(err_bg, n_bg) < 3.0,
                "where the mask is 0 the output is not the background"
            );
        }
        if n_fg > 0 {
            assert!(
                mean(err_fg, n_fg) < 3.0,
                "where the mask is 255 the output is not the input"
            );
        }

        // A rejected switch leaves the running effect in place.
        let bad = LoadRequest {
            background: Some(Arc::new(HostBgr {
                width: 640,
                height: 360,
                stride: 640 * 3,
                data: vec![0; 640 * 360 * 3],
            })),
            ..replace_req.clone()
        };
        assert!(backend.load(&gpu, &bad).is_err());
        assert_eq!(backend.loaded_effect(), Some(EffectKind::Replace));

        // And back to blur, again without a model reload.
        let r = backend.load(&gpu, &blur_req).unwrap();
        println!(
            "LOAD blur (switch back): load {:.2} ms, reused {}",
            r.load_ms, r.reused_model
        );
        assert!(r.reused_model);
        backend
            .process(&gpu, &FrameParams::default(), &src.frame(), &dst.frame())
            .unwrap();
        stream.synchronize().unwrap();

        backend.unload();
        let mem_unloaded = free_mib();
        // A cold load again after an unload (as after `none`): the steady
        // cold-load cost once the SDK is initialised in the process.
        let r = backend.load(&gpu, &blur_req).unwrap();
        println!(
            "LOAD blur (cold, after unload): load {:.1} ms, warm-up {:?} ms, reused {}",
            r.load_ms, r.warmup_ms, r.reused_model
        );
        assert!(!r.reused_model);
        backend.unload();
        drop(backend);
        let mem_dropped = free_mib();
        println!(
            "MEMORY free: before load {mem_before:.0} MiB, after load {mem_loaded:.0} MiB \
             ({:.0} MiB used), after unload {mem_unloaded:.0} MiB, after drop {mem_dropped:.0} MiB",
            mem_before - mem_loaded
        );
        assert!(
            mem_before - mem_unloaded < (mem_before - mem_loaded) / 4.0 + 16.0,
            "unload returned too little GPU memory"
        );
    }
    drop(ctx);
    let active = primary_context_active(0).unwrap();
    println!("PRIMARY context active after the run: {active}");
    assert!(
        !active,
        "the SDK activated the device's primary context instead of using the pushed one"
    );
}
