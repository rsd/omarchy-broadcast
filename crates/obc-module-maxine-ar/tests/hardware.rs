//! The eye-contact and auto-frame stages on the GPU, loaded as the daemon
//! loads them: the
//! built library through `obc-module-host`, run through the host's chain
//! adapter on CUDA frames in a context created like GStreamer's.
//!
//! All are `#[ignore]`: they need the NVIDIA Maxine AR SDK, the os-release
//! override on a distribution the SDK does not list, and an NVIDIA GPU
//! (docs/testing.md, "Maxine hardware tests"):
//!
//! ```text
//! set -a; . /usr/share/maxine-ar/maxine-ar.env; set +a
//! CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env -u LD_LIBRARY_PATH \
//!   LD_PRELOAD=/usr/lib/maxine-ar/os-release-shim.so \
//!   MAXINE_OS_RELEASE=/usr/lib/maxine-ar/fake-os-release" \
//!   cargo test -p obc-module-maxine-ar --test hardware -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `OBC_TEST_FACE_IMAGE`, when set, names a binary PPM (P6) still of a face
//! that tests run through the stages too (`magick face.jpg face.ppm`); for
//! auto-frame the face must be small enough to frame (its box under about
//! 40 % of the height) and is best off centre. With `OBC_TEST_FACE_OUT`
//! also set, the luma of the eye-contact result is written there as a PGM,
//! and those of auto-frame beside it (`<path>.auto-frame.pgm`,
//! `<path>.eye-auto-frame.pgm`, `<path>.auto-frame-eye.pgm`).
//! The coexistence with the Video Effects module is in the `hardware_vfx_first`
//! and `hardware_ar_first` test binaries, one load order each.

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use obc_gpu::DeviceNv12Buffer;
use support::gpu::{self, H, W};

/// The time between two frames of the tests that let the crop move: the
/// stage's clock is the wall clock (the host gives no presentation times).
const FRAME: Duration = Duration::from_millis(33);

#[test]
#[ignore = "needs the NVIDIA Maxine AR SDK, the os-release override and an NVIDIA GPU"]
fn gpu_eye_contact_passes_a_frame_without_a_face_through() {
    let dir = tempfile::tempdir().unwrap();
    let module = gpu::ar_module(dir.path());
    module
        .check(&[gpu::eye_contact()])
        .unwrap_or_else(|e| panic!("the check failed: {e}"));
    gpu::rig(|g, stream| {
        let input = gpu::bgr_to_nv12(
            &gpu::faceless_room(W as usize, H as usize),
            W as usize,
            H as usize,
        );
        let frame = DeviceNv12Buffer::alloc(W, H).unwrap();
        let mut chain = gpu::eye_chain(&module, g, W, H);
        for _ in 0..30 {
            frame.upload_packed(&input).unwrap();
            gpu::run_eye(&mut *chain, g, &frame);
            stream.synchronize().unwrap();
        }
        let out = frame.download_packed().unwrap();
        let (mean, max) = gpu::diff(&input, &out);
        println!("NO FACE: mean difference {mean:.3}, largest {max}");
        // The NV12 to BGR to NV12 round trip only.
        assert!(mean < 1.5, "the frame changed: mean difference {mean:.3}");
        assert!(max <= 16, "the frame changed: largest difference {max}");

        // A switch to the look-away loads the feature again; the same
        // request again reuses it.
        let r = chain.load(&gpu::eye_request(g, W, H, true)).unwrap();
        assert!(!r.reused_model, "{r:?}");
        let r = chain.load(&gpu::eye_request(g, W, H, true)).unwrap();
        assert!(r.reused_model, "{r:?}");
        gpu::run_eye(&mut *chain, g, &frame);
        // A temporal reset keeps the stage.
        assert!(chain.reset_temporal().unwrap().is_empty());
        gpu::run_eye(&mut *chain, g, &frame);
        stream.synchronize().unwrap();
        // Another size: a reload.
        let small = DeviceNv12Buffer::alloc(1280, 720).unwrap();
        chain.load(&gpu::eye_request(g, 1280, 720, false)).unwrap();
        small
            .upload_packed(&gpu::bgr_to_nv12(&gpu::faceless_room(1280, 720), 1280, 720))
            .unwrap();
        gpu::run_eye(&mut *chain, g, &small);
        stream.synchronize().unwrap();
        chain.unload();
        chain.unload();
    });
}

#[test]
#[ignore = "needs the NVIDIA Maxine AR SDK, the os-release override and an NVIDIA GPU"]
fn gpu_eye_contact_runs_on_a_still_face_when_one_is_given() {
    let Some(path) = std::env::var_os("OBC_TEST_FACE_IMAGE").map(PathBuf::from) else {
        println!("OBC_TEST_FACE_IMAGE is not set: skipped");
        return;
    };
    let (bgr, w, h) = gpu::read_ppm(&path);
    let dir = tempfile::tempdir().unwrap();
    let module = gpu::ar_module(dir.path());
    gpu::rig(|g, stream| {
        let input = gpu::bgr_to_nv12(&bgr, w, h);
        let frame = DeviceNv12Buffer::alloc(w as u32, h as u32).unwrap();
        let mut chain = gpu::eye_chain(&module, g, w as u32, h as u32);
        for _ in 0..30 {
            frame.upload_packed(&input).unwrap();
            gpu::run_eye(&mut *chain, g, &frame);
            stream.synchronize().unwrap();
        }
        let out = frame.download_packed().unwrap();
        let (mean, max) = gpu::diff(&input, &out);
        println!(
            "FACE {}: mean difference {mean:.3}, largest {max}",
            path.display()
        );
        // The luma of the result, to look at, when asked for.
        if let Some(out_path) = std::env::var_os("OBC_TEST_FACE_OUT") {
            let mut pgm = format!("P5\n{w} {h}\n255\n").into_bytes();
            pgm.extend_from_slice(&out[..w * h]);
            std::fs::write(&out_path, pgm).unwrap();
        }
        chain.unload();
    });
}

#[test]
#[ignore = "needs the NVIDIA Maxine AR SDK, the os-release override and an NVIDIA GPU"]
fn gpu_eye_contact_timing() {
    let dir = tempfile::tempdir().unwrap();
    let module = gpu::ar_module(dir.path());
    gpu::rig(|g, stream| {
        let input = gpu::bgr_to_nv12(
            &gpu::faceless_room(W as usize, H as usize),
            W as usize,
            H as usize,
        );
        let frame = DeviceNv12Buffer::alloc(W, H).unwrap();
        frame.upload_packed(&input).unwrap();
        let mut chain = gpu::eye_chain(&module, g, W, H);
        gpu::time("eye-contact 1920x1080, no face", 300, 30, stream, || {
            gpu::run_eye(&mut *chain, g, &frame)
        });
        chain.unload();
    });
    // With a face, the feature's whole work: the still given, when it is
    // 1920x1080.
    let Some(path) = std::env::var_os("OBC_TEST_FACE_IMAGE").map(PathBuf::from) else {
        return;
    };
    let (bgr, w, h) = gpu::read_ppm(&path);
    if (w, h) != (W as usize, H as usize) {
        println!("the face still is {w}x{h}, not {W}x{H}: no timing with a face");
        return;
    }
    gpu::rig(|g, stream| {
        let frame = DeviceNv12Buffer::alloc(W, H).unwrap();
        frame.upload_packed(&gpu::bgr_to_nv12(&bgr, w, h)).unwrap();
        let mut chain = gpu::eye_chain(&module, g, W, H);
        gpu::time("eye-contact 1920x1080, a face", 300, 30, stream, || {
            gpu::run_eye(&mut *chain, g, &frame)
        });
        chain.unload();
    });
}

/// The chains with auto-frame: alone, after eye contact (the default
/// order), and before it (eye contact on the framed picture).
fn auto_chains() -> [(&'static str, Vec<obc_backend::StageId>); 3] {
    [
        ("auto-frame", vec![gpu::auto_frame()]),
        (
            "eye-auto-frame",
            vec![gpu::eye_contact(), gpu::auto_frame()],
        ),
        (
            "auto-frame-eye",
            vec![gpu::auto_frame(), gpu::eye_contact()],
        ),
    ]
}

#[test]
#[ignore = "needs the NVIDIA Maxine AR SDK, the os-release override and an NVIDIA GPU"]
fn gpu_auto_frame_passes_a_frame_without_a_face_through() {
    let dir = tempfile::tempdir().unwrap();
    let module = gpu::ar_module(dir.path());
    module
        .check(&[gpu::auto_frame()])
        .unwrap_or_else(|e| panic!("the check failed: {e}"));
    let none = BTreeMap::new();
    gpu::rig(|g, stream| {
        let input = gpu::bgr_to_nv12(
            &gpu::faceless_room(W as usize, H as usize),
            W as usize,
            H as usize,
        );
        let frame = DeviceNv12Buffer::alloc(W, H).unwrap();
        for (label, stages) in auto_chains() {
            let mut chain = gpu::ar_chain(&module, g, W, H, &stages, "tight");
            for i in 0..30 {
                frame.upload_packed(&input).unwrap();
                gpu::run_chain(&mut *chain, g, &frame, &none, i % 5 == 4);
                stream.synchronize().unwrap();
            }
            let out = frame.download_packed().unwrap();
            let (mean, max) = gpu::diff(&input, &out);
            println!("NO FACE {label}: mean difference {mean:.3}, largest {max}");
            // No face, no crop: the NV12 to BGR to NV12 round trip only,
            // and auto-frame alone leaves the frame untouched.
            assert!(mean < 1.5, "{label}: the frame changed: mean {mean:.3}");
            assert!(max <= 16, "{label}: the frame changed: largest {max}");
            if stages.len() == 1 {
                assert_eq!(max, 0, "{label}: the frame went through BGR");
            }
            // The same request again reuses the stages; a live framing and
            // a temporal reset keep them.
            let r = chain
                .load(&gpu::ar_request(g, W, H, &stages, "tight"))
                .unwrap();
            assert!(r.reused_model, "{label}: {r:?}");
            gpu::run_chain(&mut *chain, g, &frame, &gpu::live_framing("wide"), false);
            assert!(chain.reset_temporal().unwrap().is_empty());
            gpu::run_chain(&mut *chain, g, &frame, &none, false);
            stream.synchronize().unwrap();
            chain.unload();
        }
        // A switch between the two chains, both ways, on one instance.
        let both = [gpu::eye_contact(), gpu::auto_frame()];
        let factory = obc_module_host::ModuleChainFactory::new(module.clone(), &both).unwrap();
        let mut chain = obc_backend::StageChainFactory::create(&factory).unwrap();
        chain
            .load(&gpu::ar_request(g, W, H, &both, "medium"))
            .unwrap();
        gpu::run_chain(&mut *chain, g, &frame, &none, false);
        let r = chain
            .load(&gpu::ar_request(g, W, H, &[gpu::auto_frame()], "medium"))
            .unwrap();
        assert!(r.reused_model, "{r:?}");
        gpu::run_chain(&mut *chain, g, &frame, &none, false);
        let r = chain
            .load(&gpu::ar_request(g, W, H, &[gpu::eye_contact()], "medium"))
            .unwrap();
        assert!(!r.reused_model, "{r:?}");
        gpu::run_chain(&mut *chain, g, &frame, &none, false);
        let r = chain
            .load(&gpu::ar_request(g, W, H, &both, "medium"))
            .unwrap();
        assert!(!r.reused_model, "{r:?}");
        gpu::run_chain(&mut *chain, g, &frame, &none, false);
        stream.synchronize().unwrap();
        chain.unload();
    });
}

#[test]
#[ignore = "needs the NVIDIA Maxine AR SDK, the os-release override and an NVIDIA GPU"]
fn gpu_auto_frame_moves_towards_a_still_face_when_one_is_given() {
    let Some(path) = std::env::var_os("OBC_TEST_FACE_IMAGE").map(PathBuf::from) else {
        println!("OBC_TEST_FACE_IMAGE is not set: skipped");
        return;
    };
    let (bgr, w, h) = gpu::read_ppm(&path);
    let dir = tempfile::tempdir().unwrap();
    let module = gpu::ar_module(dir.path());
    let none = BTreeMap::new();
    gpu::rig(|g, stream| {
        let input = gpu::bgr_to_nv12(&bgr, w, h);
        let frame = DeviceNv12Buffer::alloc(w as u32, h as u32).unwrap();
        for (label, stages) in auto_chains() {
            let mut chain = gpu::ar_chain(&module, g, w as u32, h as u32, &stages, "tight");
            // The difference from the input, frame by frame, over three
            // seconds at about 30 frames a second.
            let mut diffs = Vec::new();
            let mut last = Vec::new();
            for _ in 0..90 {
                frame.upload_packed(&input).unwrap();
                gpu::run_chain(&mut *chain, g, &frame, &none, false);
                stream.synchronize().unwrap();
                last = frame.download_packed().unwrap();
                diffs.push(gpu::diff(&input, &last).0);
                std::thread::sleep(FRAME);
            }
            println!(
                "FACE {label} {}: mean difference after 1, 10, 30, 90 frames: \
                 {:.2} {:.2} {:.2} {:.2}",
                path.display(),
                diffs[0],
                diffs[9],
                diffs[29],
                diffs[89]
            );
            // It starts at the full frame and moves, gradually.
            assert!(diffs[0] < 1.5, "{label}: the first frame was cropped");
            assert!(
                diffs[29] > diffs[9] && diffs[89] > diffs[29],
                "{label}: the crop does not move gradually: {diffs:?}"
            );
            assert!(diffs[89] > 4.0, "{label}: the frame is not framed");
            // A repeat replays the last output exactly, the crop unmoved.
            frame.upload_packed(&input).unwrap();
            gpu::run_chain(&mut *chain, g, &frame, &none, true);
            stream.synchronize().unwrap();
            assert!(
                frame.download_packed().unwrap() == last,
                "{label}: a repeat moved"
            );
            if let Some(out) = std::env::var_os("OBC_TEST_FACE_OUT") {
                let mut out = PathBuf::from(out).into_os_string();
                out.push(format!(".{label}.pgm"));
                let mut pgm = format!("P5\n{w} {h}\n255\n").into_bytes();
                pgm.extend_from_slice(&last[..w * h]);
                std::fs::write(&out, pgm).unwrap();
            }
            // A live change to the widest framing and a temporal reset: the
            // reset snaps back to the full frame at once.
            gpu::run_chain(&mut *chain, g, &frame, &gpu::live_framing("wide"), false);
            // The framed write of that frame lands on the stream: let it
            // finish before the input is uploaded over it, as the host does
            // before it reuses a buffer, or it can overwrite the input.
            stream.synchronize().unwrap();
            assert!(chain.reset_temporal().unwrap().is_empty());
            frame.upload_packed(&input).unwrap();
            gpu::run_chain(&mut *chain, g, &frame, &none, false);
            stream.synchronize().unwrap();
            let (mean, _) = gpu::diff(&input, &frame.download_packed().unwrap());
            assert!(
                mean < 1.5,
                "{label}: the reset did not snap back: {mean:.3}"
            );
            chain.unload();
        }
    });
}

#[test]
#[ignore = "needs the NVIDIA Maxine AR SDK, the os-release override and an NVIDIA GPU"]
fn gpu_eye_contact_and_auto_frame_timing() {
    let dir = tempfile::tempdir().unwrap();
    let module = gpu::ar_module(dir.path());
    let none = BTreeMap::new();
    let face = std::env::var_os("OBC_TEST_FACE_IMAGE")
        .map(|p| gpu::read_ppm(std::path::Path::new(&p)))
        .filter(|(_, w, h)| (*w, *h) == (W as usize, H as usize));
    let (bgr, what) = match &face {
        Some((bgr, _, _)) => (bgr.clone(), "a face"),
        None => (gpu::faceless_room(W as usize, H as usize), "no face"),
    };
    gpu::rig(|g, stream| {
        // The chain writes the frame in place: each step starts from a
        // pristine copy (a device-to-device copy, timed alone below), or
        // the crop would zoom into its own output.
        let pristine = DeviceNv12Buffer::alloc(W, H).unwrap();
        pristine
            .upload_packed(&gpu::bgr_to_nv12(&bgr, W as usize, H as usize))
            .unwrap();
        let frame = DeviceNv12Buffer::alloc(W, H).unwrap();
        let restore = || {
            // SAFETY: two live frames of the current context, distinct.
            unsafe { obc_gpu::copy_nv12_async(&pristine.frame(), &frame.frame(), stream.raw()) }
                .unwrap()
        };
        gpu::time("restore copy alone 1920x1080", 300, 30, stream, restore);
        for (label, stages) in auto_chains() {
            let mut chain = gpu::ar_chain(&module, g, W, H, &stages, "tight");
            // Some real time first, so that a face is framed (scaled) while
            // timed.
            for _ in 0..60 {
                restore();
                gpu::run_chain(&mut *chain, g, &frame, &none, false);
                stream.synchronize().unwrap();
                std::thread::sleep(FRAME);
            }
            gpu::time(
                &format!("{label} 1920x1080, {what} (with the restore copy)"),
                300,
                30,
                stream,
                || {
                    restore();
                    gpu::run_chain(&mut *chain, g, &frame, &none, false)
                },
            );
            chain.unload();
        }
    });
}
