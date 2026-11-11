//! The Maxine module on the GPU, loaded as the daemon loads it: the built
//! library through `obc-module-host`, its stages run through the host's
//! adapters ([`ModuleBackendFactory`], [`ModuleEnhancerFactory`]) on CUDA
//! frames in a context created like GStreamer's.
//!
//! All are `#[ignore]`: they need the NVIDIA Maxine Video Effects SDK, the
//! os-release override on a distribution the SDK does not list, and an
//! NVIDIA GPU, with the environment of `obc-backend-maxine`'s hardware
//! tests:
//!
//! ```text
//! set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a
//! CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$MAXINE_VFX_OS_RELEASE_SHIM" \
//!   cargo test -p obc-module-maxine --test hardware -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! The frames are synthetic, so the cut-out the model makes of them is not
//! predictable: the tests assert what holds for any cut-out.

mod support;

use std::sync::{Arc, OnceLock};

use obc_backend::enhance::{
    ChromaSiting, DeviceYuv420, EnhanceRequest, EnhancerFactory, YuvLayout,
};
use obc_backend::{
    Availability, BackendFactory, ColourSpec, EffectKind, FrameParams, GpuContext, HostBgr,
    LoadRequest, Params, StageId, StageList, Value,
};
use obc_gpu::{ContextGuard, DeviceNv12Buffer, OwnedContext, Stream};
use obc_module_host::{Module, ModuleBackendFactory, ModuleEnhancerFactory};

const W: u32 = 1280;
const H: u32 = 720;
const FRAMES: usize = 30;

const SDK_HINT: &str = "this test needs the Maxine SDK: install the maxine-vfx-sdk-local package \
     and load its environment (set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a), or set \
     MAXINE_VFX_ROOT and MAXINE_OS_RELEASE as omarchy-nvidia-sdks docs/usage.md describes";

/// The module, opened once for the test binary (the SDK is loaded once per
/// process anyway), with every stage installed.
fn module() -> Module {
    static MODULE: OnceLock<(tempfile::TempDir, Module)> = OnceLock::new();
    MODULE
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let (module, _lines) = support::open(dir.path(), serde_json::json!({}));
            let probe = module.probe().unwrap();
            for (stage, a) in &probe {
                assert_eq!(*a, Availability::Installed, "{stage}: {SDK_HINT}");
            }
            // As the registry's open does: the description the factories
            // check parameters against.
            module.settings().unwrap();
            (dir, module)
        })
        .1
        .clone()
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
            let l = 16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0;
            out[y * w + x] = l.round().clamp(0.0, 255.0) as u8;
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

/// A rough head-and-shoulders silhouette over a gradient room, packed BGR.
fn synthetic_person(w: usize, h: usize) -> Vec<u8> {
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
                [90, 130, 200]
            } else if shoulders {
                [60, 40, 30]
            } else {
                [(150 + x * 60 / w) as u8, (140 + y * 40 / h) as u8, 120]
            };
            bgr[i..i + 3].copy_from_slice(&px);
        }
    }
    bgr
}

/// An orange-to-green ramp, packed BGR.
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

/// A flat grey NV12 frame with noise of standard deviation about 8.
fn noisy_grey(seed: u64) -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut uniform = || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut out = vec![128u8; w * h * 3 / 2];
    for y in out[..w * h].iter_mut() {
        let n: f64 = (0..4).map(|_| uniform()).sum::<f64>() - 2.0;
        *y = (120.0 + 8.0 * n * 3f64.sqrt()).round().clamp(16.0, 235.0) as u8;
    }
    out
}

/// Mean and standard deviation of the luma of the central quarter of a
/// `w` x `h` packed NV12 frame.
fn centre_luma(nv12: &[u8], w: usize, h: usize) -> (f64, f64) {
    let (mut n, mut sum, mut sq) = (0f64, 0f64, 0f64);
    for y in h / 4..3 * h / 4 {
        for &v in &nv12[y * w + w / 4..y * w + 3 * w / 4] {
            let v = v as f64;
            n += 1.0;
            sum += v;
            sq += v * v;
        }
    }
    let mean = sum / n;
    (mean, (sq / n - mean * mean).max(0.0).sqrt())
}

/// A created context with a stream; `f` runs with it current.
fn rig(f: impl FnOnce(&GpuContext, &Stream)) {
    let ctx = OwnedContext::create(0).unwrap();
    // SAFETY: `ctx` outlives the guard; everything in `f` drops first.
    let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
    let stream = Stream::new(true).unwrap();
    let gpu = GpuContext {
        context: ctx.raw(),
        stream: stream.raw(),
    };
    f(&gpu, &stream);
}

fn request(effect: EffectKind, stages: &[StageId]) -> LoadRequest {
    LoadRequest {
        effect,
        stages: StageList::new(stages.iter().cloned()).unwrap(),
        width: W,
        height: H,
        colour: ColourSpec::LIMITED_BT601,
        background: None,
        params: Default::default(),
    }
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK, the os-release override and an NVIDIA GPU"]
fn maxine_module_blur_and_replace_through_the_abi() {
    let factory = ModuleBackendFactory::new(module());
    factory.probe().unwrap();
    rig(|gpu, stream| {
        let input = bgr_to_nv12(
            &synthetic_person(W as usize, H as usize),
            W as usize,
            H as usize,
        );
        let src = DeviceNv12Buffer::alloc(W, H).unwrap();
        let dst = DeviceNv12Buffer::alloc(W, H).unwrap();
        src.upload_packed(&input).unwrap();
        let mut backend = factory.create().unwrap();
        let r = backend
            .load(gpu, &request(EffectKind::Blur, &[]))
            .unwrap_or_else(|e| panic!("blur load failed: {e}"));
        println!("LOAD blur: {r:?}");
        assert!(!r.reused_model);
        for _ in 0..FRAMES {
            backend
                .process(gpu, &FrameParams::default(), &src.frame(), &dst.frame())
                .unwrap_or_else(|e| panic!("blur frame failed: {e}"));
        }
        stream.synchronize().unwrap();
        let blurred = dst.download_packed().unwrap();
        assert_eq!(
            src.download_packed().unwrap(),
            input,
            "the source was written"
        );
        assert_ne!(blurred, input, "blur output equals the input");
        assert!(
            blurred[..(W * H) as usize].iter().any(|&y| y > 40),
            "blur output is black"
        );
        let t = backend
            .timings()
            .expect("timings after a synchronised frame");
        println!("TIMINGS blur: {t:?}");
        assert!(t.step_ms("mask").is_some(), "{t:?}");

        // Replace: a switch that keeps the cut-out model.
        let mut req = request(EffectKind::Replace, &[]);
        req.background = Some(Arc::new(background()));
        let r = backend
            .load(gpu, &req)
            .unwrap_or_else(|e| panic!("replace load failed: {e}"));
        println!("LOAD replace: {r:?}");
        assert!(r.reused_model, "blur to replace reloads the cut-out model");
        for _ in 0..FRAMES {
            backend
                .process(gpu, &FrameParams::default(), &src.frame(), &dst.frame())
                .unwrap();
        }
        stream.synchronize().unwrap();
        let replaced = dst.download_packed().unwrap();
        assert_ne!(replaced, input);
        assert_ne!(replaced, blurred);
        backend.unload();
    });
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK, the os-release override and an NVIDIA GPU"]
fn maxine_module_denoise_removes_noise_through_the_abi() {
    let factory = ModuleBackendFactory::new(module());
    rig(|gpu, stream| {
        let src = DeviceNv12Buffer::alloc(W, H).unwrap();
        let dst = DeviceNv12Buffer::alloc(W, H).unwrap();
        let mut backend = factory.create().unwrap();
        backend
            .load(gpu, &request(EffectKind::None, &[StageId::DENOISE]))
            .unwrap_or_else(|e| panic!("denoise load failed: {e}"));
        let mut input = Vec::new();
        for seed in 0..FRAMES as u64 {
            input = noisy_grey(seed);
            src.upload_packed(&input).unwrap();
            backend
                .process(gpu, &FrameParams::default(), &src.frame(), &dst.frame())
                .unwrap_or_else(|e| panic!("denoise frame failed: {e}"));
            stream.synchronize().unwrap();
        }
        let out = dst.download_packed().unwrap();
        let (i, o) = (
            centre_luma(&input, W as usize, H as usize),
            centre_luma(&out, W as usize, H as usize),
        );
        println!(
            "DENOISE in mean {:.2} sd {:.2}; out mean {:.2} sd {:.2}",
            i.0, i.1, o.0, o.1
        );
        assert!(o.1 < i.1 * 0.6, "the noise did not drop clearly");
        assert!((o.0 - i.0).abs() < 4.0, "the level moved");
        let t = backend.timings().unwrap();
        assert!(t.step_ms("denoise").is_some(), "{t:?}");
        // A temporal reset keeps the stage.
        assert!(backend.reset_temporal(gpu).unwrap().is_empty());
        backend.unload();
    });
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK, the os-release override and an NVIDIA GPU"]
fn maxine_module_relight_lights_the_person_through_the_abi() {
    let factory = ModuleBackendFactory::new(module());
    let p = |k: &str, v: Value| -> Params { [(k.to_string(), v)].into_iter().collect() };
    // Its parameters are checked against the module's settings.
    assert!(
        factory
            .check_params(
                &StageId::RELIGHT,
                &p("light", Value::from("warm")),
                &Params::new()
            )
            .is_ok()
    );
    assert!(
        factory
            .check_params(
                &StageId::RELIGHT,
                &Params::new(),
                &p("direction", Value::Int(120))
            )
            .is_err()
    );
    rig(|gpu, stream| {
        let input = bgr_to_nv12(
            &synthetic_person(W as usize, H as usize),
            W as usize,
            H as usize,
        );
        let src = DeviceNv12Buffer::alloc(W, H).unwrap();
        let dst = DeviceNv12Buffer::alloc(W, H).unwrap();
        src.upload_packed(&input).unwrap();
        let mut backend = factory.create().unwrap();
        let mut req = request(EffectKind::None, &[StageId::RELIGHT]);
        req.params
            .insert(StageId::RELIGHT, p("light", Value::from("warm")));
        backend
            .load(gpu, &req)
            .unwrap_or_else(|e| panic!("relight load failed: {e}"));
        let mut frame = FrameParams::default();
        frame
            .live
            .insert(StageId::RELIGHT, p("direction", Value::Int(45)));
        for _ in 0..FRAMES {
            backend
                .process(gpu, &frame, &src.frame(), &dst.frame())
                .unwrap_or_else(|e| panic!("relight frame failed: {e}"));
        }
        stream.synchronize().unwrap();
        let out = dst.download_packed().unwrap();
        assert_ne!(out, input, "relighting changed nothing");
        let t = backend.timings().unwrap();
        println!("TIMINGS relight: {t:?}");
        assert!(t.step_ms("relight").is_some(), "{t:?}");
        // The stage's temporal state cannot be reset: it is dropped, and
        // loads again.
        assert_eq!(
            backend.reset_temporal(gpu).unwrap(),
            StageList::from(StageId::RELIGHT)
        );
        backend.load(gpu, &req).unwrap();
        backend
            .process(gpu, &frame, &src.frame(), &dst.frame())
            .unwrap();
        stream.synchronize().unwrap();
        backend.unload();
    });
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK, the os-release override and an NVIDIA GPU"]
fn maxine_module_enhance_upscales_through_the_abi() {
    let factory = ModuleEnhancerFactory::new(module());
    factory.probe().unwrap();
    let (w, h) = (640u32, 360u32);
    rig(|gpu, stream| {
        let person = synthetic_person(w as usize, h as usize);
        let input = bgr_to_nv12(&person, w as usize, h as usize);
        let src = DeviceNv12Buffer::alloc(w, h).unwrap();
        let dst = DeviceNv12Buffer::alloc(2 * w, 2 * h).unwrap();
        src.upload_packed(&input).unwrap();
        let mut e = factory.create().unwrap();
        let req = EnhanceRequest {
            width: w,
            height: h,
            scale: 2,
            layout: YuvLayout::Nv12,
            colour: ColourSpec::LIMITED_BT601,
            chroma: ChromaSiting::Cosited,
        };
        e.load(gpu, &req)
            .unwrap_or_else(|err| panic!("enhance load failed: {err}"));
        let frame = src.frame();
        for _ in 0..FRAMES {
            e.process(gpu, &DeviceYuv420::from_nv12(&frame), &dst.frame())
                .unwrap_or_else(|err| panic!("enhance frame failed: {err}"));
        }
        stream.synchronize().unwrap();
        let out = dst.download_packed().unwrap();
        let (i, o) = (
            centre_luma(&input, w as usize, h as usize),
            centre_luma(&out, 2 * w as usize, 2 * h as usize),
        );
        println!("ENHANCE in mean {:.2}; out mean {:.2}", i.0, o.0);
        assert!(
            (o.0 - i.0).abs() < 6.0,
            "the upscaled picture's level moved"
        );
        let t = e.timings().expect("timings after a synchronised frame");
        println!("TIMINGS enhance: {t:?}");
        assert!(t.upscale_ms > 0.0, "{t:?}");
        e.unload();
    });
}

/// A grey 1920x1080 frame with a white square whose left edge is at `x`,
/// packed BT.601 limited-range NV12.
fn square_at(x: usize) -> Vec<u8> {
    let (w, h) = (FG_W as usize, FG_H as usize);
    let mut bgr = vec![90u8; w * h * 3];
    for y in h / 2 - 120..h / 2 + 120 {
        for px in x..x + 240 {
            let i = (y * w + px) * 3;
            bgr[i..i + 3].copy_from_slice(&[235, 235, 235]);
        }
    }
    bgr_to_nv12(&bgr, w, h)
}

/// The mean absolute difference of two equal-size byte frames.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let sum: u64 = a.iter().zip(b).map(|(&x, &y)| x.abs_diff(y) as u64).sum();
    sum as f64 / a.len() as f64
}

const FG_W: u32 = 1920;
const FG_H: u32 = 1080;

/// Frame generation alone, as the daemon runs it after the effect: the
/// first frame goes out as it is, a repeat goes out as the last fresh
/// frame (with the repeat flag of ABI 1.1, through the host's chain
/// adapter), and a fresh frame goes out as a frame between the previous
/// one and itself, for a pattern that moves.
#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK, the os-release override and an NVIDIA GPU"]
fn gpu_frame_generation_interpolates_fresh_frames_and_holds_repeats() {
    use obc_backend::{ChainFrame, ChainRequest, FrameKind, StageChain, StageChainFactory};
    use obc_module_host::ModuleChainFactory;
    use std::collections::BTreeMap;

    let module = module();
    let stage = StageId::new("frame-generation").unwrap();
    let factory = ModuleChainFactory::new(module, std::slice::from_ref(&stage)).unwrap();
    assert_eq!(factory.kinds(), [FrameKind::Cuda]);
    rig(|gpu, stream| {
        let frame = DeviceNv12Buffer::alloc(FG_W, FG_H).unwrap();
        let mut chain = factory.create().unwrap();
        let r = chain
            .load(&ChainRequest {
                stages: StageList::from(stage.clone()),
                params: BTreeMap::new(),
                width: FG_W,
                height: FG_H,
                colour: ColourSpec::LIMITED_BT601,
                kind: FrameKind::Cuda,
                gpu: Some(*gpu),
            })
            .unwrap_or_else(|e| panic!("frame generation load failed: {e}"));
        println!("LOAD frame-generation: {r:?}");
        let none = BTreeMap::new();
        // Runs `input` through the chain in place and returns the output.
        let run = |chain: &mut dyn StageChain, input: &[u8], repeat: bool| -> Vec<u8> {
            frame.upload_packed(input).unwrap();
            let f = frame.frame();
            let cf = ChainFrame::Cuda { gpu, frame: &f };
            let result = if repeat {
                chain.process_repeat(cf, &none)
            } else {
                chain.process(cf, &none)
            };
            result.unwrap_or_else(|e| panic!("frame generation frame failed: {e}"));
            stream.synchronize().unwrap();
            frame.download_packed().unwrap()
        };
        let (a, b, c) = (square_at(400), square_at(560), square_at(1200));
        assert_eq!(
            run(&mut *chain, &a, false),
            a,
            "the first frame goes out as it is"
        );
        // A repeat of a: the held frame, whatever the input says.
        assert_eq!(
            run(&mut *chain, &b, true),
            a,
            "a repeat goes out as the held frame"
        );
        let mid = run(&mut *chain, &b, false);
        let (to_a, to_b) = (mean_abs_diff(&mid, &a), mean_abs_diff(&mid, &b));
        println!("FRAME GENERATION mean difference to a {to_a:.3}, to b {to_b:.3}");
        assert!(
            to_a > 0.1 && to_b > 0.1,
            "the generated frame is one of its inputs"
        );
        let ab = mean_abs_diff(&a, &b);
        assert!(
            to_a < ab && to_b < ab,
            "the generated frame is not between its inputs ({ab:.3} between them)"
        );
        assert_eq!(
            run(&mut *chain, &a, true),
            b,
            "the repeat after it is the fresh frame"
        );
        // After a reset there is no previous frame.
        assert!(chain.reset_temporal().unwrap().is_empty());
        assert_eq!(
            run(&mut *chain, &c, false),
            c,
            "the first frame after a reset"
        );
        chain.unload();
    });
}
