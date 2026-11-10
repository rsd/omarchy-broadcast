//! The hardened green-screen mask on recorded camera frames. Ignored: it
//! needs the NVIDIA Maxine Video Effects SDK, an NVIDIA GPU, the os-release
//! override (as `tests/hardware.rs` describes) and a directory of recorded
//! frames named by `OBC_MASK_FRAMES`. Without the variable it fails.
//!
//! The synthetic frames of `tests/hardware.rs` give the model no person to
//! find, so its soft edge is never exercised there. This test replays real
//! camera frames through replace on a solid background and checks, on every
//! frame, what the hardening promises:
//!
//! - Where the model rates a pixel below its decision boundary (raw mask
//!   below 128) the output is the background, within one code value of the
//!   colour conversion, in luma and in chroma: no part of the room shows.
//!   With the raw mask as the alpha, the faint room behind the subject came
//!   through here; the test logs, per frame, how many of these pixels would
//!   have shown it, and fails if no frame of a sequence would have (the
//!   frames could not tell a hardened mask from a raw one).
//! - The edge band where camera and background are mixed (alpha between 10%
//!   and 90%) is at most half as wide, in pixels, as with the raw mask: in
//!   the hardened mask, and in the output itself, where the mix is read back
//!   from the luma of pixels whose camera luma is far enough from the
//!   background's.
//! - The hardened mask the effect used is the table applied to the raw mask.
//!   Two tables can only be told apart where they differ, and a wrong one
//!   most likely differs on the ramp (raw 129 to 223, the soft band), so
//!   every frame must hold soft-band pixels and every sequence must hold
//!   every soft-band value: then each entry of the ramp is compared with
//!   the GPU's output.
//!
//! It also logs, for reference, where the hardened mask is opaque how far
//! the output is from the camera frame (the colour round trip, which
//! `tests/hardware.rs` bounds), and the edge flicker: per pair of
//! consecutive frames, the pixels of the soft band whose mask value changes
//! by 32 or more (12.5% of full scale), raw and hardened.
//!
//! # Frame directory layout
//!
//! `OBC_MASK_FRAMES` names a local directory, never committed. Each
//! subdirectory is one sequence of consecutive frames from one camera,
//! replayed in file-name order through a freshly loaded effect:
//!
//! ```text
//! $OBC_MASK_FRAMES/
//!   brio/00.nv12 01.nv12 ... 39.nv12
//!   laptop/00.nv12 ...
//! ```
//!
//! Every `*.nv12` file is one 1920x1080 frame, packed NV12: the 1920x1080
//! luma plane, then the 1920x540 interleaved UV plane, no padding (3,110,400
//! bytes), limited-range BT.601 as the cameras deliver it. Other files are
//! ignored. A person should be in frame; around 40 frames per sequence give
//! the model's temporal state time to settle before the flicker figure.
//! Recording them, from a camera that delivers NV12 at 1920x1080, and from
//! one that delivers only MJPEG there (the first frames, taken while the
//! camera's exposure settles, are better deleted):
//!
//! ```text
//! gst-launch-1.0 v4l2src device=/dev/video0 num-buffers=50 \
//!   ! video/x-raw,format=NV12,width=1920,height=1080 \
//!   ! multifilesink location=brio/%02d.nv12
//! gst-launch-1.0 v4l2src device=/dev/video2 num-buffers=50 \
//!   ! image/jpeg,width=1920,height=1080 ! jpegdec ! videoconvert \
//!   ! video/x-raw,format=NV12,colorimetry=bt601 \
//!   ! multifilesink location=laptop/%02d.nv12
//! ```
//!
//! From the repository root, with the environment of `tests/hardware.rs`:
//!
//! ```text
//! OBC_MASK_FRAMES=/path/to/frames \
//! CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$MAXINE_VFX_OS_RELEASE_SHIM" \
//!   cargo test -p obc-backend-maxine --test mask_frames -- --ignored --nocapture
//! ```

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use obc_backend::{
    BackendFactory, ColourSpec, EffectKind, FrameParams, GpuContext, HostBgr, InferenceBackend,
    LoadRequest,
};
use obc_backend_maxine::MaxineFactory;
use obc_backend_maxine::matte::{HARDENING_LUT, MATTE_HI, MATTE_LO, apply_lut};
use obc_gpu::{ContextGuard, DeviceNv12Buffer, OwnedContext, Stream};

const W: usize = 1920;
const H: usize = 1080;
const FRAME_BYTES: usize = W * H * 3 / 2;
const FRAMES_ENV: &str = "OBC_MASK_FRAMES";

/// The replacement background, packed BGR: a saturated magenta, far from
/// skin and from most rooms in both luma and chroma.
const BG: [u8; 3] = [255, 0, 255];

/// Below this raw mask value the model rates a pixel background.
const BOUNDARY: u8 = 128;

/// Mask values where camera and background each contribute 10% to 90%
/// (alpha = value / 255).
const MIXED: RangeInclusive<u8> = 26..=229;

/// A mask change of at least this much between consecutive frames counts as
/// flicker (12.5% of full scale).
const FLICKER_STEP: i32 = 32;

/// Frames the model's temporal state gets before the flicker figure is
/// taken.
const SETTLE: usize = 9;

/// Camera-to-background luma difference from which the output's mix is
/// estimated.
const CONTRAST: i32 = 32;

/// Code values the colour conversion may be off by.
const TOLERANCE: i32 = 1;

/// Raw mask values the hardening table maps strictly between 0 and 255:
/// above `MATTE_LO`, below `MATTE_HI`.
const SOFT_BAND: RangeInclusive<u8> = MATTE_LO + 1..=MATTE_HI - 1;

/// What a test needs before the factory can find the SDK.
const SDK_HINT: &str = "this test needs the Maxine SDK: install the maxine-vfx-sdk-local package \
     and load its environment (set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a), or set \
     MAXINE_VFX_ROOT and MAXINE_OS_RELEASE as omarchy-nvidia-sdks docs/usage.md describes (the \
     SDK's libraries are loaded from the root: LD_LIBRARY_PATH is not needed)";

fn factory() -> MaxineFactory {
    let f = MaxineFactory::from_env();
    assert!(f.root().is_some(), "{SDK_HINT}");
    f.probe()
        .unwrap_or_else(|e| panic!("Maxine probe failed: {e}"));
    f
}

/// The sequences under `OBC_MASK_FRAMES`: each subdirectory with its
/// `*.nv12` files in name order. Panics, naming what is wrong, if the
/// variable is unset or there is nothing to replay.
fn sequences() -> Vec<(String, Vec<PathBuf>)> {
    let root = match std::env::var_os(FRAMES_ENV) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => panic!(
            "this test needs recorded camera frames: set {FRAMES_ENV} to a directory with one \
             subdirectory per sequence of 1920x1080 NV12 frames (see the module documentation \
             of crates/obc-backend-maxine/tests/mask_frames.rs)"
        ),
    };
    let entries =
        std::fs::read_dir(&root).unwrap_or_else(|e| panic!("{FRAMES_ENV}={}: {e}", root.display()));
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

/// Limited-range BT.601 Y, U and V of a BGR pixel.
fn yuv(bgr: [u8; 3]) -> (i32, i32, i32) {
    let (b, g, r) = (bgr[0] as f64, bgr[1] as f64, bgr[2] as f64);
    let y = 16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0;
    let u = 128.0 + (-37.797 * r - 74.203 * g + 112.0 * b) / 255.0;
    let v = 128.0 + (112.0 * r - 93.786 * g - 18.214 * b) / 255.0;
    (y.round() as i32, u.round() as i32, v.round() as i32)
}

fn solid_background() -> HostBgr {
    HostBgr {
        width: W as u32,
        height: H as u32,
        stride: W * 3,
        data: BG.repeat(W * H),
    }
}

/// What one frame showed.
#[derive(Default)]
struct Counts {
    /// Pixels the model rates background (raw < 128).
    bg_side: usize,
    /// Of those, the ones whose output luma is not the background.
    room_y: usize,
    /// Chroma samples whose whole neighbourhood is background-side, and of
    /// those the ones whose output chroma is not the background.
    bg_side_uv: usize,
    room_uv: usize,
    /// Background-side pixels with a raw mask above 0, and the ones whose
    /// luma would not be the background with the raw mask as the alpha.
    raw_nonzero: usize,
    raw_room_y: usize,
    /// Mixed pixels (alpha 10% to 90%) with the raw and the hardened mask.
    mixed_raw: usize,
    mixed_hard: usize,
    /// Pixels whose camera luma is at least `CONTRAST` from the
    /// background's, where the output tells how much of each it holds; of
    /// those, the ones whose output is a 10% to 90% mix, and the ones the
    /// raw mask would have mixed.
    contrast: usize,
    mixed_out: usize,
    mixed_raw_contrast: usize,
    /// Pixels the hardened mask makes opaque, and the sum and the largest
    /// of the luma differences there between output and camera frame.
    opaque: usize,
    opaque_dy_sum: u64,
    opaque_max_dy: i32,
    /// Opaque pixels per luma difference (0 to 255).
    opaque_dy_hist: Vec<usize>,
    /// Pixels the model rates foreground (raw >= 128).
    fg_side: usize,
}

fn measure(input: &[u8], out: &[u8], raw: &[u8], hard: &[u8]) -> Counts {
    let (bg_y, bg_u, bg_v) = yuv(BG);
    let mut c = Counts {
        opaque_dy_hist: vec![0; 256],
        ..Counts::default()
    };
    for i in 0..W * H {
        let (r, m) = (raw[i], hard[i]);
        let (y_in, y_out) = (input[i] as i32, out[i] as i32);
        if r < BOUNDARY {
            c.bg_side += 1;
            if (y_out - bg_y).abs() > TOLERANCE {
                c.room_y += 1;
            }
            if r > 0 {
                c.raw_nonzero += 1;
                let a = r as f64 / 255.0;
                let y_raw = (bg_y as f64 + a * (y_in - bg_y) as f64).round() as i32;
                if (y_raw - bg_y).abs() > TOLERANCE {
                    c.raw_room_y += 1;
                }
            }
        } else {
            c.fg_side += 1;
        }
        c.mixed_raw += MIXED.contains(&r) as usize;
        c.mixed_hard += MIXED.contains(&m) as usize;
        let d = y_in - bg_y;
        if d.abs() >= CONTRAST {
            c.contrast += 1;
            let a = (y_out - bg_y) as f64 / d as f64;
            c.mixed_out += (0.1..=0.9).contains(&a) as usize;
            c.mixed_raw_contrast += MIXED.contains(&r) as usize;
        }
        if m == 255 {
            c.opaque += 1;
            c.opaque_max_dy = c.opaque_max_dy.max((y_out - y_in).abs());
            c.opaque_dy_sum += (y_out - y_in).unsigned_abs() as u64;
            c.opaque_dy_hist[(y_out - y_in).unsigned_abs() as usize] += 1;
        }
    }
    // A chroma sample covers a 2x2 block of luma; the conversion may filter
    // across the block's neighbours, so a sample counts as background-side
    // only if the 4x4 luma window around its block is.
    let uv = &out[W * H..];
    for cy in 0..H / 2 {
        for cx in 0..W / 2 {
            let ys = (2 * cy).saturating_sub(1)..(2 * cy + 3).min(H);
            let all_bg = ys.into_iter().all(|y| {
                let xs = (2 * cx).saturating_sub(1)..(2 * cx + 3).min(W);
                raw[y * W + xs.start..y * W + xs.end]
                    .iter()
                    .all(|&r| r < BOUNDARY)
            });
            if !all_bg {
                continue;
            }
            c.bg_side_uv += 1;
            let (u, v) = (uv[cy * W + 2 * cx] as i32, uv[cy * W + 2 * cx + 1] as i32);
            if (u - bg_u).abs() > TOLERANCE || (v - bg_v).abs() > TOLERANCE {
                c.room_uv += 1;
            }
        }
    }
    c
}

/// Per pair of consecutive masks, the pixels of the soft band (raw mask
/// strictly between 0 and 250 in any of the masks) whose value changes by
/// `FLICKER_STEP` or more.
fn flicker(raw: &[Vec<u8>], masks: &[Vec<u8>]) -> Vec<usize> {
    let band: Vec<bool> = (0..W * H)
        .map(|i| raw.iter().any(|m| m[i] > 0 && m[i] < 250))
        .collect();
    masks
        .windows(2)
        .map(|p| {
            (0..W * H)
                .filter(|&i| band[i] && (p[1][i] as i32 - p[0][i] as i32).abs() >= FLICKER_STEP)
                .count()
        })
        .collect()
}

/// The 99.9th percentile of a histogram (0 if it is empty).
fn p999(hist: &[usize]) -> usize {
    let target = (hist.iter().sum::<usize>() as f64 * 0.999).ceil() as usize;
    let mut acc = 0;
    hist.iter()
        .position(|&c| {
            acc += c;
            acc >= target
        })
        .unwrap_or(0)
}

fn mean(v: &[usize]) -> f64 {
    v.iter().sum::<usize>() as f64 / v.len().max(1) as f64
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK (MAXINE_VFX_ROOT), the os-release override, an NVIDIA \
            GPU and recorded camera frames (OBC_MASK_FRAMES)"]
fn the_hardened_mask_hides_the_room_on_recorded_camera_frames() {
    let seqs = sequences();
    let f = factory();
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
            effect: EffectKind::Replace,
            denoise: false,
            width: W as u32,
            height: H as u32,
            colour: ColourSpec::LIMITED_BT601,
            background: Some(Arc::new(solid_background())),
        };

        for (name, frames) in &seqs {
            // A fresh effect per sequence: the model's temporal state must
            // not carry over from another camera.
            let mut backend = f.create_maxine().unwrap();
            backend
                .load(&gpu, &req)
                .unwrap_or_else(|e| panic!("{name}: replace load failed: {e}"));
            let (mut raws, mut hards) = (Vec::new(), Vec::new());
            let (mut any_fg, mut any_raw_room) = (false, false);
            // Which raw values of the soft band some frame of the sequence
            // held.
            let mut soft_seen = [false; 256];
            for (k, path) in frames.iter().enumerate() {
                let label = format!("{name}/{}", path.file_name().unwrap().to_string_lossy());
                let input = read_frame(path);
                src.upload_packed(&input).unwrap();
                backend
                    .process(&gpu, &FrameParams::default(), &src.frame(), &dst.frame())
                    .unwrap_or_else(|e| panic!("{label}: process failed: {e}"));
                stream.synchronize().unwrap();
                let out = dst.download_packed().unwrap();
                let raw = backend.read_mask().unwrap();
                let hard = backend.read_hardened_mask().unwrap();

                let mut want = vec![0u8; W * H];
                apply_lut(&HARDENING_LUT, &raw, W, &mut want, W, W, H);
                let wrong = hard.iter().zip(&want).filter(|(a, b)| a != b).count();
                let mut soft = 0usize;
                for &r in raw.iter().filter(|r| SOFT_BAND.contains(r)) {
                    soft += 1;
                    soft_seen[r as usize] = true;
                }

                let c = measure(&input, &out, &raw, &hard);
                any_fg |= c.fg_side > 0;
                any_raw_room |= c.raw_room_y > 0;
                println!(
                    "FRAME {label}: soft band {soft} px; background-side {} px, room shown {} px \
                     luma / {} of {} chroma (raw-mask alpha would show {} px; raw 1..127 {} px); mask mixed {} \
                     px vs raw {} px (ratio {:.2}); output mixed {} px vs raw {} px of {} \
                     (ratio {:.2}); opaque {} px, |Y - input| mean {:.3} p99.9 {} max {}",
                    c.bg_side,
                    c.room_y,
                    c.room_uv,
                    c.bg_side_uv,
                    c.raw_room_y,
                    c.raw_nonzero,
                    c.mixed_hard,
                    c.mixed_raw,
                    c.mixed_hard as f64 / c.mixed_raw.max(1) as f64,
                    c.mixed_out,
                    c.mixed_raw_contrast,
                    c.contrast,
                    c.mixed_out as f64 / c.mixed_raw_contrast.max(1) as f64,
                    c.opaque,
                    c.opaque_dy_sum as f64 / c.opaque.max(1) as f64,
                    p999(&c.opaque_dy_hist),
                    c.opaque_max_dy,
                );
                if wrong != 0 {
                    failures.push(format!(
                        "{label}: {wrong} px of the hardened mask != table(raw)"
                    ));
                }
                if soft == 0 {
                    failures.push(format!(
                        "{label}: no raw mask value in the soft band {SOFT_BAND:?}; the \
                         hardened == table(raw) check cannot tell the ramp from another table"
                    ));
                }
                if c.room_y != 0 || c.room_uv != 0 {
                    failures.push(format!(
                        "{label}: {} luma px and {} chroma samples rated background show the \
                         room instead of the background",
                        c.room_y, c.room_uv
                    ));
                }
                if c.mixed_hard * 2 > c.mixed_raw {
                    failures.push(format!(
                        "{label}: {} mixed px in the hardened mask is more than half the raw \
                         mask's {}",
                        c.mixed_hard, c.mixed_raw
                    ));
                }
                if c.mixed_out * 2 > c.mixed_raw_contrast {
                    failures.push(format!(
                        "{label}: {} mixed px in the output is more than half the {} the raw \
                         mask mixes there",
                        c.mixed_out, c.mixed_raw_contrast
                    ));
                }
                if k >= SETTLE.min(frames.len().saturating_sub(2)) {
                    raws.push(raw);
                    hards.push(hard);
                }
            }
            backend.unload();

            let (fr, fh) = (flicker(&raws, &raws), flicker(&raws, &hards));
            println!(
                "FLICKER {name}: {} frame pairs after {SETTLE} settling frames, px/frame changing \
                 by >= {FLICKER_STEP}: raw mean {:.0}, hardened mean {:.0} (hardened per pair: \
                 {fh:?})",
                fh.len(),
                mean(&fr),
                mean(&fh),
            );
            if !any_fg {
                failures.push(format!(
                    "{name}: the model found no subject in any frame; the sequence tests nothing"
                ));
            }
            let missing: Vec<u8> = SOFT_BAND.filter(|&r| !soft_seen[r as usize]).collect();
            println!(
                "SOFT {name}: {} of the {} soft-band values seen in the sequence",
                SOFT_BAND.count() - missing.len(),
                SOFT_BAND.count()
            );
            if !missing.is_empty() {
                failures.push(format!(
                    "{name}: no frame held the raw mask values {missing:?}; their table \
                     entries were never compared with the GPU's"
                ));
            }
            if !any_raw_room {
                failures.push(format!(
                    "{name}: with the raw mask no frame would have shown the room; the sequence \
                     cannot tell a hardened mask from a raw one"
                ));
            }
        }
    }
    drop(ctx);
    assert!(
        failures.is_empty(),
        "{} failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
