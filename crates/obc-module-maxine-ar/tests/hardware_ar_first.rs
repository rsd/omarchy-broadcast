//! The two Maxine SDKs in one process, the AR SDK loaded first
//! (both ship a `libNVCVImage.so` of that soname): see
//! `support::coexist`. A test binary of its own, so that the load order
//! is that of a fresh process. `#[ignore]`: it needs both SDKs, the
//! os-release override and an NVIDIA GPU (the command is in `hardware.rs`,
//! with `/usr/share/maxine-vfx/maxine-vfx.env` loaded too).

mod support;

#[test]
#[ignore = "needs the NVIDIA Maxine AR and Video Effects SDKs, the os-release override and an NVIDIA GPU"]
fn gpu_eye_contact_and_blur_in_one_process_ar_first() {
    support::coexist::run(support::coexist::First::Ar);
}
