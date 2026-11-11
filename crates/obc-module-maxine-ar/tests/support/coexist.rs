//! The coexistence of the two Maxine SDKs in one process: the Video Effects
//! module's background blur and this module's eye contact, loaded in the
//! order given, then frames through both (eye contact in place, then the
//! blur into a second frame), then the timing of the pair; then the same
//! with eye contact and auto-frame (whose scaling uses the AR SDK's bundled
//! NPP, a library the Video Effects SDK bundles too). With
//! `OBC_TEST_FACE_IMAGE` naming a 1920x1080 binary PPM of a face, the frames
//! are that still.

use std::path::Path;

use obc_gpu::DeviceNv12Buffer;

use super::gpu::{self, H, W};

/// Which module's SDK is loaded first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum First {
    Vfx,
    Ar,
}

pub fn run(first: First) {
    let dir_ar = tempfile::tempdir().unwrap();
    let dir_vfx = tempfile::tempdir().unwrap();
    let (ar, vfx) = (|p: &Path| gpu::ar_module(p), |p: &Path| gpu::vfx_module(p));
    let (ar, vfx) = match first {
        First::Vfx => {
            let v = vfx(dir_vfx.path());
            (ar(dir_ar.path()), v)
        }
        First::Ar => {
            let a = ar(dir_ar.path());
            (a, vfx(dir_vfx.path()))
        }
    };
    gpu::rig(|g, stream| {
        // The face still when one is given at 1920x1080 (the feature's
        // whole work, through the SDK's image functions), else a room.
        let face = std::env::var_os("OBC_TEST_FACE_IMAGE")
            .map(|p| gpu::read_ppm(Path::new(&p)))
            .filter(|(_, w, h)| (*w, *h) == (W as usize, H as usize));
        let bgr = match &face {
            Some((bgr, _, _)) => bgr.clone(),
            None => gpu::faceless_room(W as usize, H as usize),
        };
        let input = gpu::bgr_to_nv12(&bgr, W as usize, H as usize);
        let frame = DeviceNv12Buffer::alloc(W, H).unwrap();
        let out = DeviceNv12Buffer::alloc(W, H).unwrap();
        // The SDKs' own loads in the order given: the first one's libraries
        // are in the process before the second's.
        let (mut blur, mut eye) = match first {
            First::Vfx => {
                let b = gpu::blur_backend(&vfx, g, W, H);
                (b, gpu::eye_chain(&ar, g, W, H))
            }
            First::Ar => {
                let e = gpu::eye_chain(&ar, g, W, H);
                (gpu::blur_backend(&vfx, g, W, H), e)
            }
        };
        print_loaded_nvcvimage();
        for _ in 0..30 {
            frame.upload_packed(&input).unwrap();
            gpu::run_eye(&mut *eye, g, &frame);
            gpu::run_blur(&mut *blur, g, &frame, &out);
            stream.synchronize().unwrap();
        }
        let eyed = frame.download_packed().unwrap();
        let blurred = out.download_packed().unwrap();
        let (mean, max) = gpu::diff(&input, &eyed);
        println!(
            "COEXIST {first:?} first ({}): eye contact mean difference {mean:.3}, largest {max}",
            if face.is_some() { "a face" } else { "no face" }
        );
        if face.is_none() {
            assert!(mean < 1.5, "eye contact changed a frame without a face");
        }
        assert_ne!(blurred, eyed, "the blur changed nothing");
        assert!(
            blurred[..(W * H) as usize].iter().any(|&y| y > 40),
            "the blur output is black"
        );
        let label = format!("eye-contact + blur 1920x1080 ({first:?} first)");
        gpu::time(&label, 300, 30, stream, || {
            gpu::run_eye(&mut *eye, g, &frame);
            gpu::run_blur(&mut *blur, g, &frame, &out);
        });
        gpu::time(
            &format!("blur alone 1920x1080 ({first:?} first)"),
            300,
            30,
            stream,
            || gpu::run_blur(&mut *blur, g, &frame, &out),
        );
        eye.unload();

        // Eye contact and auto-frame, then the blur. Each frame starts from
        // the input (the chain zooms in place).
        let both = [gpu::eye_contact(), gpu::auto_frame()];
        let mut chain = gpu::ar_chain(&ar, g, W, H, &both, "tight");
        let none = std::collections::BTreeMap::new();
        for _ in 0..30 {
            frame.upload_packed(&input).unwrap();
            gpu::run_chain(&mut *chain, g, &frame, &none, false);
            gpu::run_blur(&mut *blur, g, &frame, &out);
            stream.synchronize().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
        let framed = frame.download_packed().unwrap();
        let blurred = out.download_packed().unwrap();
        let (mean, max) = gpu::diff(&input, &framed);
        println!(
            "COEXIST {first:?} first: eye contact + auto-frame mean difference {mean:.3}, \
             largest {max}"
        );
        if face.is_none() {
            assert!(mean < 1.5, "auto-frame changed a frame without a face");
        }
        assert!(
            blurred[..(W * H) as usize].iter().any(|&y| y > 40),
            "the blur output is black"
        );
        let pristine = obc_gpu::DeviceNv12Buffer::alloc(W, H).unwrap();
        pristine.upload_packed(&input).unwrap();
        gpu::time(
            &format!(
                "eye-contact + auto-frame + blur 1920x1080 ({first:?} first, with a restore copy)"
            ),
            300,
            30,
            stream,
            || {
                // SAFETY: two live frames of the current context, distinct.
                unsafe {
                    obc_gpu::copy_nv12_async(&pristine.frame(), &frame.frame(), stream.raw())
                }
                .unwrap();
                gpu::run_chain(&mut *chain, g, &frame, &none, false);
                gpu::run_blur(&mut *blur, g, &frame, &out);
            },
        );
        chain.unload();
        blur.unload();
    });
}

/// Prints the copies of `libNVCVImage.so` mapped in the process.
fn print_loaded_nvcvimage() {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut seen: Vec<&str> = maps
        .lines()
        .filter_map(|l| l.split_whitespace().nth(5))
        .filter(|p| p.contains("libNVCVImage"))
        .collect();
    seen.sort();
    seen.dedup();
    println!("NVCVIMAGE mapped: {seen:?}");
}
