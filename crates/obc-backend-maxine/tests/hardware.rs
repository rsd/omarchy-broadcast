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
//! export OMARCHY_BROADCAST_OS_RELEASE=$PWD/tools/maxine-probe/fake-os-release
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
use obc_backend_maxine::matte::{HARDENING_LUT, apply_lut};
use obc_backend_maxine::{MaskFault, MaskFaultSwitch, MaxineBackend, MaxineFactory, ROOT_ENV};
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

/// A value the hardened mask is filled with before a checked frame, so a
/// frame the kernel did not run on cannot pass as hardened.
const POISON: u8 = 0x5A;

/// Runs one more frame with the hardened mask poisoned first, then checks
/// that the hardened mask is `table` (the host copy of the table the
/// backend uploaded) applied to the raw mask: the kernel ran on this
/// frame's mask, with this table. Logs how much of it is soft, so a frame
/// without a soft band is visible. Returns the raw and the hardened mask.
fn check_hardened(
    label: &str,
    backend: &mut MaxineBackend,
    gpu: &GpuContext,
    params: FrameParams,
    src: &DeviceNv12Buffer,
    dst: &DeviceNv12Buffer,
    table: &[u8; 256],
) -> (Vec<u8>, Vec<u8>) {
    backend.fill_hardened_mask(POISON).unwrap();
    backend
        .process(gpu, &params, &src.frame(), &dst.frame())
        .unwrap_or_else(|e| panic!("{label}: process failed: {e}"));
    let raw = backend.read_mask().unwrap();
    let hard = backend.read_hardened_mask().unwrap();
    let w = W as usize;
    let mut want = vec![0u8; raw.len()];
    apply_lut(table, &raw, w, &mut want, w, w, H as usize);
    let wrong = hard.iter().zip(&want).filter(|(a, b)| a != b).count();
    let count = |v: &[u8], f: fn(u8) -> bool| v.iter().filter(|&&m| f(m)).count();
    println!(
        "HARDEN {label}: raw 1..=127 {} px, raw 128 {} px, raw soft band 129..=223 {} px, \
         raw max {}; hardened 0 {} px, soft {} px, 255 {} px",
        count(&raw, |m| (1..=127).contains(&m)),
        count(&raw, |m| m == 128),
        count(&raw, |m| (129..=223).contains(&m)),
        raw.iter().max().unwrap(),
        count(&hard, |m| m == 0),
        count(&hard, |m| m > 0 && m < 255),
        count(&hard, |m| m == 255),
    );
    assert_eq!(wrong, 0, "{label}: hardened mask != table(raw mask)");
    if !raw.iter().any(|&m| m >= 128) {
        println!(
            "HARDEN {label}: the model found no foreground in this frame; only the \
             background side of the table was exercised"
        );
    }
    (raw, hard)
}

/// A luma difference above which a pixel counts as visibly changed.
const VISIBLE_DY: u8 = 8;

/// Where the hardened mask is opaque the output is the camera frame, up to
/// the colour round trip NV12 -> BGR -> NV12 around the effect. Luma
/// survives that round trip up to rounding, except where the first step
/// clips a colour outside the RGB cube (saturated chroma at sharp edges).
/// The synthetic frame stays inside it (max 1), but on recorded camera
/// frames (tests/mask_frames.rs logs it) the opaque pixels differ by a mean
/// of 0.03 to 0.53 code values, a 99.9th percentile of 3 to 6 and a rare
/// maximum of 5 to 20. So the mean and the 99.9th percentile are bounded,
/// never the maximum, with room for other cameras: fewer than 1 in 1000
/// opaque pixels may change visibly. An effect that ignored the mask is
/// far outside (replace showing the background: mean above 30; blur
/// blurring the frame: 99.9th percentile above 50).
const OPAQUE_MEAN_DY: f64 = 1.0;
/// See [`OPAQUE_MEAN_DY`].
const OPAQUE_P999_DY: u8 = VISIBLE_DY;

/// Absolute luma differences between two packed NV12 frames over the
/// pixels `pick` selects.
struct LumaDiff {
    n: usize,
    mean: f64,
    p999: u8,
    max: u8,
    /// Pixels differing by more than [`VISIBLE_DY`].
    visible: usize,
}

impl std::fmt::Display for LumaDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "over {} px: mean {:.3}, p99.9 {}, max {}, > {VISIBLE_DY}: {} px",
            self.n, self.mean, self.p999, self.max, self.visible
        )
    }
}

fn luma_diff(a: &[u8], b: &[u8], pick: impl Fn(usize) -> bool) -> LumaDiff {
    let mut hist = [0usize; 256];
    for i in (0..(W * H) as usize).filter(|&i| pick(i)) {
        hist[a[i].abs_diff(b[i]) as usize] += 1;
    }
    let n: usize = hist.iter().sum();
    let sum: usize = hist.iter().enumerate().map(|(d, &c)| d * c).sum();
    let target = (n as f64 * 0.999).ceil() as usize;
    let mut acc = 0;
    let p999 = hist
        .iter()
        .position(|&c| {
            acc += c;
            acc >= target
        })
        .unwrap_or(0) as u8;
    LumaDiff {
        n,
        mean: sum as f64 / n.max(1) as f64,
        p999,
        max: hist.iter().rposition(|&c| c > 0).unwrap_or(0) as u8,
        visible: hist[VISIBLE_DY as usize + 1..].iter().sum(),
    }
}

/// Asserts that `d`, the output against the camera frame where the
/// hardened mask is opaque, is within the colour round trip.
fn assert_opaque_is_input(label: &str, d: &LumaDiff) {
    assert!(
        d.mean <= OPAQUE_MEAN_DY && d.p999 <= OPAQUE_P999_DY,
        "{label}: where the hardened mask is 255 the output is not the input ({d})"
    );
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
        check_hardened(
            "blur",
            &mut backend,
            &gpu,
            FrameParams { blur_strength: 0.5 },
            &src,
            &dst,
            &HARDENING_LUT,
        );
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
        let (raw, hard) = check_hardened(
            "replace",
            &mut backend,
            &gpu,
            FrameParams::default(),
            &src,
            &dst,
            &HARDENING_LUT,
        );
        let out = dst.download_packed().unwrap();
        // Where the model is below its decision boundary the output is the
        // background, exactly up to the colour conversion: the raw mask's
        // low values must not let the room through. Where the hardened mask
        // is 255 the output is the camera frame.
        let d_bg = luma_diff(&out, &bg_nv12, |i| raw[i] < 128);
        let d_fg = luma_diff(&out, &input, |i| hard[i] == 255);
        println!("REPLACE: raw mask < 128: |Y - bg| {d_bg}; hardened mask 255: |Y - input| {d_fg}");
        assert!(d_bg.n > 0, "the synthetic frame has no background");
        assert!(
            d_bg.max <= 1,
            "where the raw mask is below 128 the output is not the background"
        );
        if d_fg.n == 0 {
            println!(
                "REPLACE: no opaque foreground on this synthetic frame; the hardened == 255 \
                 check tested nothing (maxine_the_effects_read_the_hardened_mask_not_the_raw_one \
                 covers it)"
            );
        }
        assert_opaque_is_input("replace", &d_fg);

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

/// A created context with a stream and two frames of the synthetic person,
/// for the fault tests.
fn fault_rig(f: impl FnOnce(&GpuContext, &Stream, &DeviceNv12Buffer, &DeviceNv12Buffer)) {
    let ctx = OwnedContext::create(0).unwrap();
    {
        // SAFETY: `ctx` outlives the guard; everything below drops first.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let stream = Stream::new(true).unwrap();
        let gpu = GpuContext {
            context: ctx.raw(),
            stream: stream.raw(),
        };
        let src = DeviceNv12Buffer::alloc(W, H).unwrap();
        let dst = DeviceNv12Buffer::alloc(W, H).unwrap();
        src.upload_packed(&bgr_to_nv12(&synthetic_person(), W as usize, H as usize))
            .unwrap();
        f(&gpu, &stream, &src, &dst);
    }
}

fn blur_request() -> LoadRequest {
    LoadRequest {
        effect: EffectKind::Blur,
        width: W,
        height: H,
        colour: ColourSpec::LIMITED_BT601,
        background: None,
    }
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK (MAXINE_VFX_ROOT), the os-release override and an NVIDIA GPU"]
fn maxine_a_mask_kernel_that_does_not_load_fails_the_load() {
    let switch = MaskFaultSwitch::default();
    let f = factory().with_mask_faults(switch.clone());
    fault_rig(|gpu, stream, src, dst| {
        let mut backend = f.create_maxine().unwrap();
        switch.set(Some(MaskFault::ModuleLoad));
        let e = backend.load(gpu, &blur_request()).unwrap_err();
        println!("forced module-load failure: {e}");
        match &e {
            BackendError::Load { message, .. } => {
                assert!(message.contains("cuModuleLoadData"), "{message}");
                assert!(message.contains("mask-hardening kernel"), "{message}");
            }
            other => panic!("expected a load error, got {other:?}"),
        }
        assert_eq!(
            backend.loaded_effect(),
            None,
            "a failed load left an effect"
        );
        let e = backend
            .process(gpu, &FrameParams::default(), &src.frame(), &dst.frame())
            .unwrap_err();
        assert!(e.to_string().contains("no effect loaded"), "{e}");

        // The failure left the context usable: disarmed, the same backend
        // loads and runs.
        switch.set(None);
        backend.load(gpu, &blur_request()).unwrap();
        backend
            .process(gpu, &FrameParams::default(), &src.frame(), &dst.frame())
            .unwrap();
        stream.synchronize().unwrap();
        backend.unload();
    });
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK (MAXINE_VFX_ROOT), the os-release override and an NVIDIA GPU"]
fn maxine_a_mask_kernel_that_does_not_launch_fails_the_frame() {
    let switch = MaskFaultSwitch::default();
    let f = factory().with_mask_faults(switch.clone());
    fault_rig(|gpu, stream, src, dst| {
        let mut backend = f.create_maxine().unwrap();
        // Armed before the load: the fault hits frame launches only, so the
        // load (and its warm-up launch) succeeds.
        switch.set(Some(MaskFault::Launch));
        backend
            .load(gpu, &blur_request())
            .unwrap_or_else(|e| panic!("the launch fault reached the load: {e}"));
        for _ in 0..2 {
            let e = backend
                .process(gpu, &FrameParams::default(), &src.frame(), &dst.frame())
                .unwrap_err();
            println!("forced launch failure: {e}");
            match &e {
                BackendError::Run { message, .. } => {
                    assert!(message.contains("cuLaunchKernel"), "{message}")
                }
                other => panic!("expected a run error, got {other:?}"),
            }
        }
        stream
            .synchronize()
            .expect("a refused launch leaves the stream usable");

        switch.set(None);
        check_hardened(
            "after a refused launch",
            &mut backend,
            gpu,
            FrameParams::default(),
            src,
            dst,
            &HARDENING_LUT,
        );
        stream.synchronize().unwrap();
        backend.unload();
    });
}

/// With a constant table the hardened mask is that constant wherever the
/// model looks, while the raw mask of the synthetic frame is background
/// nearly everywhere (the model finds no person in it). An effect that read
/// the raw mask gives itself away: with an all-opaque table replace must
/// return the camera frame and blur must leave it sharp, where the raw mask
/// would show the background and blur the whole frame. The all-transparent
/// table shows that both effects do change the frame where the hardened
/// mask says background, so "unchanged" above is not a no-op passing.
#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK (MAXINE_VFX_ROOT), the os-release override and an NVIDIA GPU"]
fn maxine_the_effects_read_the_hardened_mask_not_the_raw_one() {
    let switch = MaskFaultSwitch::default();
    let f = factory().with_mask_faults(switch.clone());
    fault_rig(|gpu, stream, src, dst| {
        let input = src.download_packed().unwrap();
        let bg = background();
        let bg_nv12 = bgr_to_nv12(&bg.data, W as usize, H as usize);
        let bg = Arc::new(bg);
        let params = FrameParams { blur_strength: 1.0 };
        let mut blur_visible = Vec::new();
        for value in [0xFFu8, 0x00] {
            let table = [value; 256];
            let mut backend = f.create_maxine().unwrap();
            // The table is uploaded by the cold load and kept by the switch
            // to replace below.
            switch.set(Some(MaskFault::ConstantTable(value)));
            let r = backend.load(gpu, &blur_request());
            switch.set(None);
            r.unwrap_or_else(|e| panic!("blur load failed: {e}"));
            let run = |label: &str, backend: &mut MaxineBackend| {
                for _ in 0..5 {
                    backend
                        .process(gpu, &params, &src.frame(), &dst.frame())
                        .unwrap_or_else(|e| panic!("{label}: process failed: {e}"));
                }
                stream.synchronize().unwrap();
                let (raw, _) = check_hardened(label, backend, gpu, params, src, dst, &table);
                let raw_bg = raw.iter().filter(|&&m| m < 128).count();
                assert!(
                    raw_bg * 100 >= raw.len() * 99,
                    "{label}: the raw mask rates only {raw_bg} px background; on the synthetic \
                     frame it must be background nearly everywhere for this test to tell it \
                     from the hardened mask"
                );
                dst.download_packed().unwrap()
            };

            let label = format!("blur, table {value:#04x}");
            let out = run(&label, &mut backend);
            let d_in = luma_diff(&out, &input, |_| true);
            println!("CONSTANT {label}: |Y - input| {d_in}");
            if value == 0xFF {
                // All foreground: nothing to blur.
                assert_opaque_is_input(&label, &d_in);
            }
            blur_visible.push(d_in.visible);

            let replace = LoadRequest {
                effect: EffectKind::Replace,
                background: Some(bg.clone()),
                ..blur_request()
            };
            let r = backend.load(gpu, &replace).unwrap();
            assert!(r.reused_model, "blur -> replace reloaded the model");
            let label = format!("replace, table {value:#04x}");
            let out = run(&label, &mut backend);
            let d_in = luma_diff(&out, &input, |_| true);
            let d_bg = luma_diff(&out, &bg_nv12, |_| true);
            println!("CONSTANT {label}: |Y - input| {d_in}; |Y - bg| {d_bg}");
            if value == 0xFF {
                assert_opaque_is_input(&label, &d_in);
            } else {
                assert!(
                    d_bg.max <= 1,
                    "{label}: with an all-background mask the output is not the background \
                     everywhere ({d_bg})"
                );
            }
            backend.unload();
        }
        // The silhouette's edges blur visibly when the mask says background
        // everywhere; with an all-foreground mask nothing does.
        let (opaque, clear) = (blur_visible[0], blur_visible[1]);
        assert!(
            clear >= (W * H) as usize / 1000 && clear >= 20 * opaque.max(1),
            "blur with an all-background mask changed {clear} px visibly, with an \
             all-foreground mask {opaque} px: the blur does not follow the hardened mask"
        );
    });
}
