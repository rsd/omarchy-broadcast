//! The denoise stage on recorded camera frames. Ignored: it needs the NVIDIA
//! Maxine Video Effects SDK, an NVIDIA GPU, the os-release override (as
//! `tests/hardware.rs` describes), a directory of recorded frames named by
//! `OBC_DENOISE_FRAMES` and a directory for the images it writes named by
//! `OBC_DENOISE_OUT`. Without either variable it fails.
//!
//! The synthetic noise of `tests/hardware.rs` is not what a webcam's sensor
//! makes. This test replays real camera frames through the denoise stage
//! alone (`none` with denoise), once at strength 0 and once at strength 1,
//! each through a freshly loaded stage, and logs per sequence how much
//! noise is left:
//!
//! - **spatial**: the standard deviation of the luma minus the mean of its
//!   four neighbours (fine texture and noise), over the whole frame;
//! - **temporal**: the mean absolute luma change between consecutive frames
//!   (noise and motion; on a still scene, mostly noise).
//!
//! Both are averaged over the frames after the stage's temporal state has
//! had [`SETTLE`] frames, for the input and for the output. A metric only
//! points; the images are what to look at: for the first, the middle and
//! the last frame of each sequence it writes the camera frame and both
//! outputs to `OBC_DENOISE_OUT`, as binary PPM (no PNG encoder is a
//! dependency of the workspace):
//!
//! ```text
//! $OBC_DENOISE_OUT/<sequence>-<frame>-camera.ppm
//! $OBC_DENOISE_OUT/<sequence>-<frame>-strength0.ppm
//! $OBC_DENOISE_OUT/<sequence>-<frame>-strength1.ppm
//! ```
//!
//! It fails if an output frame is far from its input in level (mean luma
//! more than [`MAX_LEVEL_SHIFT`] code values apart: a black, blank or
//! garbled output), and otherwise only logs.
//!
//! # Frame directory layout
//!
//! The layout of `tests/mask_frames.rs`: one subdirectory per sequence of
//! consecutive frames from one camera, replayed in file-name order; every
//! `*.nv12` file is one packed 1920x1080 NV12 frame (3,110,400 bytes),
//! limited-range BT.601. The frames are local and never committed. Low
//! light shows the most noise; a person need not be in frame. The module
//! documentation of `tests/mask_frames.rs` has the recording commands.
//!
//! From the repository root, with the environment of `tests/hardware.rs`:
//!
//! ```text
//! OBC_DENOISE_FRAMES=/path/to/frames OBC_DENOISE_OUT=/path/to/out \
//! CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$MAXINE_VFX_OS_RELEASE_SHIM" \
//!   cargo test -p obc-backend-maxine --test denoise_frames -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};

use obc_backend::{
    BackendFactory, ColourSpec, EffectKind, FrameParams, GpuContext, InferenceBackend, LoadRequest,
    StageId,
};
use obc_backend_maxine::MaxineFactory;
use obc_gpu::{ContextGuard, DeviceNv12Buffer, OwnedContext, Stream};

const W: usize = 1920;
const H: usize = 1080;
const FRAME_BYTES: usize = W * H * 3 / 2;
const FRAMES_ENV: &str = "OBC_DENOISE_FRAMES";
const OUT_ENV: &str = "OBC_DENOISE_OUT";

/// Frames the stage's temporal state gets before the metrics are taken.
const SETTLE: usize = 5;

/// How far, in code values, an output frame's mean luma may be from its
/// input's before it counts as broken.
const MAX_LEVEL_SHIFT: f64 = 8.0;

/// What a test needs before the factory can find the SDK.
const SDK_HINT: &str = "this test needs the Maxine SDK: install the maxine-vfx-sdk-local package \
     and load its environment (set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a), or set \
     MAXINE_VFX_ROOT and MAXINE_OS_RELEASE as omarchy-nvidia-sdks docs/usage.md describes (the \
     SDK's libraries are loaded from the root: LD_LIBRARY_PATH is not needed)";

fn factory(strength: f32) -> MaxineFactory {
    let f = MaxineFactory::from_env()
        .with_denoise_strength(strength)
        .unwrap();
    assert!(f.root().is_some(), "{SDK_HINT}");
    f.probe()
        .unwrap_or_else(|e| panic!("Maxine probe failed: {e}"));
    f
}

fn dir_from_env(var: &str, what: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => panic!(
            "this test needs {what}: set {var} (see the module documentation of \
             crates/obc-backend-maxine/tests/denoise_frames.rs)"
        ),
    }
}

/// The sequences under `OBC_DENOISE_FRAMES`: each subdirectory with its
/// `*.nv12` files in name order.
fn sequences(root: &Path) -> Vec<(String, Vec<PathBuf>)> {
    let entries =
        std::fs::read_dir(root).unwrap_or_else(|e| panic!("{FRAMES_ENV}={}: {e}", root.display()));
    let mut dirs: Vec<PathBuf> = entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    let seqs: Vec<(String, Vec<PathBuf>)> = dirs
        .into_iter()
        .map(|d| {
            let mut frames: Vec<PathBuf> = std::fs::read_dir(&d)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|x| x == "nv12"))
                .collect();
            frames.sort();
            (
                d.file_name().unwrap().to_string_lossy().into_owned(),
                frames,
            )
        })
        .filter(|(_, frames)| !frames.is_empty())
        .collect();
    assert!(
        !seqs.is_empty(),
        "{FRAMES_ENV}={}: no subdirectory with *.nv12 frames",
        root.display()
    );
    seqs
}

fn read_frame(path: &Path) -> Vec<u8> {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(
        data.len(),
        FRAME_BYTES,
        "{}: not a packed 1920x1080 NV12 frame",
        path.display()
    );
    data
}

fn mean_luma(nv12: &[u8]) -> f64 {
    nv12[..W * H].iter().map(|&v| v as f64).sum::<f64>() / (W * H) as f64
}

/// The standard deviation of the luma minus the mean of its four
/// neighbours, over the interior of the frame.
fn spatial_noise(nv12: &[u8]) -> f64 {
    let y = |x: usize, r: usize| nv12[r * W + x] as f64;
    let (mut n, mut sum, mut sq) = (0f64, 0f64, 0f64);
    for r in 1..H - 1 {
        for x in 1..W - 1 {
            let d = y(x, r) - (y(x - 1, r) + y(x + 1, r) + y(x, r - 1) + y(x, r + 1)) / 4.0;
            n += 1.0;
            sum += d;
            sq += d * d;
        }
    }
    let mean = sum / n;
    (sq / n - mean * mean).max(0.0).sqrt()
}

/// The mean absolute luma change between two frames.
fn temporal_noise(a: &[u8], b: &[u8]) -> f64 {
    a[..W * H]
        .iter()
        .zip(&b[..W * H])
        .map(|(&p, &q)| (p as f64 - q as f64).abs())
        .sum::<f64>()
        / (W * H) as f64
}

/// Writes a packed limited-range BT.601 NV12 frame as a binary PPM (RGB).
fn write_ppm(path: &Path, nv12: &[u8]) {
    let mut out = format!("P6\n{W} {H}\n255\n").into_bytes();
    out.reserve(W * H * 3);
    let uv = &nv12[W * H..];
    for r in 0..H {
        for x in 0..W {
            let y = (nv12[r * W + x] as f64 - 16.0) * 255.0 / 219.0;
            let i = (r / 2) * W + (x / 2) * 2;
            let u = (uv[i] as f64 - 128.0) * 255.0 / 224.0;
            let v = (uv[i + 1] as f64 - 128.0) * 255.0 / 224.0;
            let px = [
                y + 1.402 * v,
                y - 0.344_136 * u - 0.714_136 * v,
                y + 1.772 * u,
            ];
            out.extend(px.map(|c| c.round().clamp(0.0, 255.0) as u8));
        }
    }
    std::fs::write(path, out).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

/// The metrics of one replay, averaged over the frames after [`SETTLE`].
#[derive(Default)]
struct Noise {
    frames: usize,
    spatial_in: f64,
    spatial_out: f64,
    temporal_in: f64,
    temporal_out: f64,
    temporal_pairs: usize,
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK (MAXINE_VFX_ROOT), the os-release override, an NVIDIA \
            GPU, recorded camera frames (OBC_DENOISE_FRAMES) and an output directory \
            (OBC_DENOISE_OUT)"]
fn the_denoise_stage_on_recorded_camera_frames_at_both_strengths() {
    let root = dir_from_env(FRAMES_ENV, "recorded camera frames");
    let out_dir = dir_from_env(OUT_ENV, "a directory for the before and after images");
    std::fs::create_dir_all(&out_dir)
        .unwrap_or_else(|e| panic!("{OUT_ENV}={}: {e}", out_dir.display()));
    let seqs = sequences(&root);
    let factories = [(0u8, factory(0.0)), (1u8, factory(1.0))];
    let ctx = OwnedContext::create(0).unwrap();
    let mut failures = Vec::new();
    {
        // SAFETY: `ctx` outlives the guard; everything below drops first.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let stream = Stream::new(true).unwrap();
        let gpu = GpuContext {
            context: ctx.raw(),
            stream: stream.raw(),
        };
        let src = DeviceNv12Buffer::alloc(W as u32, H as u32).unwrap();
        let dst = DeviceNv12Buffer::alloc(W as u32, H as u32).unwrap();
        let req = LoadRequest {
            effect: EffectKind::None,
            stages: StageId::DENOISE.into(),
            width: W as u32,
            height: H as u32,
            colour: ColourSpec::LIMITED_BT601,
            background: None,
            params: Default::default(),
        };

        for (name, frames) in &seqs {
            let n = frames.len();
            let shown = [0, n / 2, n - 1];
            for &k in &shown {
                let stem = frames[k]
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                write_ppm(
                    &out_dir.join(format!("{name}-{stem}-camera.ppm")),
                    &read_frame(&frames[k]),
                );
            }
            for (strength, f) in &factories {
                // A fresh stage per sequence and strength: the temporal state
                // must not carry over.
                let mut backend = f.create_maxine().unwrap();
                backend
                    .load(&gpu, &req)
                    .unwrap_or_else(|e| panic!("{name}: denoise load failed: {e}"));
                let mut noise = Noise::default();
                let mut prev: Option<(Vec<u8>, Vec<u8>)> = None;
                for (k, path) in frames.iter().enumerate() {
                    let label = format!(
                        "{name}/{} strength {strength}",
                        path.file_name().unwrap().to_string_lossy()
                    );
                    let input = read_frame(path);
                    src.upload_packed(&input).unwrap();
                    backend
                        .process(&gpu, &FrameParams::default(), &src.frame(), &dst.frame())
                        .unwrap_or_else(|e| panic!("{label}: process failed: {e}"));
                    stream.synchronize().unwrap();
                    let out = dst.download_packed().unwrap();
                    let shift = mean_luma(&out) - mean_luma(&input);
                    if shift.abs() > MAX_LEVEL_SHIFT {
                        failures.push(format!(
                            "{label}: the output's mean luma is {shift:+.1} code values from \
                             the input's"
                        ));
                    }
                    if k >= SETTLE.min(n - 1) {
                        noise.frames += 1;
                        noise.spatial_in += spatial_noise(&input);
                        noise.spatial_out += spatial_noise(&out);
                        if let Some((pi, po)) = &prev {
                            noise.temporal_pairs += 1;
                            noise.temporal_in += temporal_noise(pi, &input);
                            noise.temporal_out += temporal_noise(po, &out);
                        }
                    }
                    if shown.contains(&k) {
                        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
                        write_ppm(
                            &out_dir.join(format!("{name}-{stem}-strength{strength}.ppm")),
                            &out,
                        );
                    }
                    prev = Some((input, out));
                }
                backend.unload();
                let (fr, tp) = (
                    noise.frames.max(1) as f64,
                    noise.temporal_pairs.max(1) as f64,
                );
                println!(
                    "NOISE {name} strength {strength}: {} frames after {SETTLE} settling frames; \
                     spatial sd in {:.3} out {:.3} (ratio {:.2}); temporal mean |dY| in {:.3} \
                     out {:.3} (ratio {:.2}) over {} pairs",
                    noise.frames,
                    noise.spatial_in / fr,
                    noise.spatial_out / fr,
                    noise.spatial_out / noise.spatial_in.max(f64::MIN_POSITIVE),
                    noise.temporal_in / tp,
                    noise.temporal_out / tp,
                    noise.temporal_out / noise.temporal_in.max(f64::MIN_POSITIVE),
                    noise.temporal_pairs,
                );
            }
        }
    }
    drop(ctx);
    println!("IMAGES written to {}", out_dir.display());
    assert!(
        failures.is_empty(),
        "{} failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
