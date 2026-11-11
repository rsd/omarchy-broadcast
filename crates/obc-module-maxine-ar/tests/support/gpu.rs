//! The GPU side of the hardware tests: a CUDA context made like
//! GStreamer's, synthetic frames, the chains of this module's stages and
//! the Video Effects blur through the host's adapters, and a timing loop.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use obc_backend::{
    Availability, BackendFactory, ChainFrame, ChainRequest, ColourSpec, EffectKind, FrameKind,
    FrameParams, GpuContext, InferenceBackend, LoadRequest, Params, StageChain, StageChainFactory,
    StageId, StageList, Value,
};
use obc_gpu::{ContextGuard, DeviceNv12Buffer, OwnedContext, Stream};
use obc_module_host::{Module, ModuleBackendFactory, ModuleChainFactory};

pub const W: u32 = 1920;
pub const H: u32 = 1080;

pub const AR_HINT: &str = "this test needs the Maxine AR SDK: install the maxine-ar-sdk-local \
     package and load its environment (set -a; . /usr/share/maxine-ar/maxine-ar.env; set +a), \
     with LD_PRELOAD of its os-release shim and MAXINE_OS_RELEASE (docs/testing.md)";
pub const VFX_HINT: &str = "this test needs the Maxine Video Effects SDK too: install the \
     maxine-vfx-sdk-local package and load its environment (set -a; . \
     /usr/share/maxine-vfx/maxine-vfx.env; set +a)";

/// The eye-contact stage id.
pub fn eye_contact() -> StageId {
    StageId::new("eye-contact").unwrap()
}

/// The auto-frame stage id.
pub fn auto_frame() -> StageId {
    StageId::new("auto-frame").unwrap()
}

/// This crate's module, opened with the environment's SDK root, its stages
/// installed.
pub fn ar_module(dir: &Path) -> Module {
    let module = super::open(dir, serde_json::json!({}));
    let probe = module.probe().unwrap();
    for (stage, a) in &probe {
        assert_eq!(*a, Availability::Installed, "{stage}: {AR_HINT}");
    }
    module.settings().unwrap();
    module
}

/// The Video Effects module, its background stage installed.
pub fn vfx_module(dir: &Path) -> Module {
    let module = super::open_vfx(dir, serde_json::json!({}));
    let probe = module.probe().unwrap();
    let background = probe
        .iter()
        .find(|(s, _)| s.as_str() == "background")
        .expect("a background stage");
    assert_eq!(background.1, Availability::Installed, "{VFX_HINT}");
    module.settings().unwrap();
    module
}

/// A created context with a stream; `f` runs with it current.
pub fn rig(f: impl FnOnce(&GpuContext, &Stream)) {
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

/// BT.601 limited-range NV12 of a packed BGR image.
pub fn bgr_to_nv12(bgr: &[u8], w: usize, h: usize) -> Vec<u8> {
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

/// A smooth room with no face in it: gradients and a few soft blocks,
/// packed BGR.
pub fn faceless_room(w: usize, h: usize) -> Vec<u8> {
    let mut bgr = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            let block = (x * 6 / w + y * 4 / h).is_multiple_of(2);
            let base = [
                (110 + x * 60 / w) as u8,
                (120 + y * 50 / h) as u8,
                (130 - x * 40 / w) as u8,
            ];
            let px = if block {
                base
            } else {
                base.map(|c| c.saturating_sub(25))
            };
            bgr[i..i + 3].copy_from_slice(&px);
        }
    }
    bgr
}

/// The mean and the largest absolute difference of two equal-size frames.
pub fn diff(a: &[u8], b: &[u8]) -> (f64, u8) {
    assert_eq!(a.len(), b.len());
    let mut sum = 0u64;
    let mut max = 0u8;
    for (&x, &y) in a.iter().zip(b) {
        let d = x.abs_diff(y);
        sum += u64::from(d);
        max = max.max(d);
    }
    (sum as f64 / a.len() as f64, max)
}

/// A binary PPM (P6, 8-bit) image as packed BGR, cropped to even sizes.
pub fn read_ppm(path: &Path) -> (Vec<u8>, usize, usize) {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut fields = Vec::new();
    let mut at = 0;
    while fields.len() < 4 {
        while at < data.len() && data[at].is_ascii_whitespace() {
            at += 1;
        }
        if data.get(at) == Some(&b'#') {
            while at < data.len() && data[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        let start = at;
        while at < data.len() && !data[at].is_ascii_whitespace() {
            at += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..at]).into_owned());
    }
    assert_eq!(fields[0], "P6", "{}: not a binary PPM", path.display());
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    assert_eq!(fields[3], "255", "{}: not 8-bit", path.display());
    let pixels = &data[at + 1..];
    assert!(pixels.len() >= w * h * 3, "{}: truncated", path.display());
    let (ew, eh) = (w & !1, h & !1);
    let mut bgr = Vec::with_capacity(ew * eh * 3);
    for y in 0..eh {
        for x in 0..ew {
            let i = (y * w + x) * 3;
            bgr.extend_from_slice(&[pixels[i + 2], pixels[i + 1], pixels[i]]);
        }
    }
    (bgr, ew, eh)
}

/// The chain request of eye contact for `w` x `h` frames.
pub fn eye_request(gpu: &GpuContext, w: u32, h: u32, look_away: bool) -> ChainRequest {
    let mut params: BTreeMap<StageId, Params> = BTreeMap::new();
    params.insert(
        eye_contact(),
        [("look_away".to_string(), Value::Bool(look_away))]
            .into_iter()
            .collect(),
    );
    ChainRequest {
        stages: StageList::from(eye_contact()),
        params,
        width: w,
        height: h,
        colour: ColourSpec::LIMITED_BT601,
        kind: FrameKind::Cuda,
        gpu: Some(*gpu),
    }
}

/// The eye-contact chain of `module`, loaded for `w` x `h` frames.
pub fn eye_chain(module: &Module, gpu: &GpuContext, w: u32, h: u32) -> Box<dyn StageChain> {
    let factory = ModuleChainFactory::new(module.clone(), &[eye_contact()]).unwrap();
    assert_eq!(factory.kinds(), [FrameKind::Cuda]);
    let mut chain = factory.create().unwrap();
    let t = Instant::now();
    let r = chain
        .load(&eye_request(gpu, w, h, false))
        .unwrap_or_else(|e| panic!("eye contact load failed: {e}"));
    println!("LOAD eye-contact {w}x{h}: {r:?} ({:?})", t.elapsed());
    chain
}

/// The chain request of `stages` for `w` x `h` frames, auto-frame with the
/// framing `framing`.
pub fn ar_request(
    gpu: &GpuContext,
    w: u32,
    h: u32,
    stages: &[StageId],
    framing: &str,
) -> ChainRequest {
    let mut params: BTreeMap<StageId, Params> = BTreeMap::new();
    params.insert(
        eye_contact(),
        [("look_away".to_string(), Value::Bool(false))]
            .into_iter()
            .collect(),
    );
    params.insert(
        auto_frame(),
        [("framing".to_string(), Value::Str(framing.to_string()))]
            .into_iter()
            .collect(),
    );
    params.retain(|s, _| stages.contains(s));
    ChainRequest {
        stages: StageList::new(stages.iter().cloned()).unwrap(),
        params,
        width: w,
        height: h,
        colour: ColourSpec::LIMITED_BT601,
        kind: FrameKind::Cuda,
        gpu: Some(*gpu),
    }
}

/// The chain of `stages` of `module`, loaded for `w` x `h` frames with the
/// framing `framing`.
pub fn ar_chain(
    module: &Module,
    gpu: &GpuContext,
    w: u32,
    h: u32,
    stages: &[StageId],
    framing: &str,
) -> Box<dyn StageChain> {
    let factory = ModuleChainFactory::new(module.clone(), stages).unwrap();
    assert_eq!(factory.kinds(), [FrameKind::Cuda]);
    let mut chain = factory.create().unwrap();
    let t = Instant::now();
    let r = chain
        .load(&ar_request(gpu, w, h, stages, framing))
        .unwrap_or_else(|e| panic!("{stages:?} load failed: {e}"));
    println!("LOAD {stages:?} {w}x{h}: {r:?} ({:?})", t.elapsed());
    chain
}

/// Runs `frame` through `chain`, in place, with the per-frame parameters
/// `live`; a repeat when `repeat`.
pub fn run_chain(
    chain: &mut dyn StageChain,
    gpu: &GpuContext,
    frame: &DeviceNv12Buffer,
    live: &BTreeMap<StageId, Params>,
    repeat: bool,
) {
    let f = frame.frame();
    let cf = ChainFrame::Cuda { gpu, frame: &f };
    let r = if repeat {
        chain.process_repeat(cf, live)
    } else {
        chain.process(cf, live)
    };
    r.unwrap_or_else(|e| panic!("a frame failed: {e}"));
}

/// The per-frame parameters that set auto-frame's framing.
pub fn live_framing(framing: &str) -> BTreeMap<StageId, Params> {
    [(
        auto_frame(),
        [("framing".to_string(), Value::Str(framing.to_string()))]
            .into_iter()
            .collect(),
    )]
    .into_iter()
    .collect()
}

/// The Video Effects blur of `module`, loaded for `w` x `h` frames.
pub fn blur_backend(
    module: &Module,
    gpu: &GpuContext,
    w: u32,
    h: u32,
) -> Box<dyn InferenceBackend> {
    let factory = ModuleBackendFactory::new(module.clone());
    factory.probe().unwrap();
    let mut backend = factory.create().unwrap();
    let t = Instant::now();
    let r = backend
        .load(
            gpu,
            &LoadRequest {
                effect: EffectKind::Blur,
                stages: StageList::new([]).unwrap(),
                width: w,
                height: h,
                colour: ColourSpec::LIMITED_BT601,
                background: None,
                params: Default::default(),
            },
        )
        .unwrap_or_else(|e| panic!("blur load failed: {e}"));
    println!("LOAD blur {w}x{h}: {r:?} ({:?})", t.elapsed());
    backend
}

/// Runs `frame` through the eye-contact chain, in place.
pub fn run_eye(chain: &mut dyn StageChain, gpu: &GpuContext, frame: &DeviceNv12Buffer) {
    let f = frame.frame();
    chain
        .process(ChainFrame::Cuda { gpu, frame: &f }, &BTreeMap::new())
        .unwrap_or_else(|e| panic!("eye contact frame failed: {e}"));
}

/// Runs the blur from `src` into `dst`.
pub fn run_blur(
    backend: &mut dyn InferenceBackend,
    gpu: &GpuContext,
    src: &DeviceNv12Buffer,
    dst: &DeviceNv12Buffer,
) {
    backend
        .process(gpu, &FrameParams::default(), &src.frame(), &dst.frame())
        .unwrap_or_else(|e| panic!("blur frame failed: {e}"));
}

/// The frame times of `n` runs of `step`, each waited for on `stream`, after
/// `warmup` runs that are not counted: mean, median and 95th percentile.
pub fn time(
    label: &str,
    n: usize,
    warmup: usize,
    stream: &Stream,
    mut step: impl FnMut(),
) -> (f64, f64, f64) {
    for _ in 0..warmup {
        step();
        stream.synchronize().unwrap();
    }
    let mut times: Vec<Duration> = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        step();
        stream.synchronize().unwrap();
        times.push(t.elapsed());
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let mean = times.iter().map(|d| ms(*d)).sum::<f64>() / n as f64;
    let (p50, p95) = (ms(times[n / 2]), ms(times[n * 95 / 100]));
    println!("TIMING {label}: {n} frames, mean {mean:.2} ms, p50 {p50:.2} ms, p95 {p95:.2} ms");
    (mean, p50, p95)
}
