//! The system C++ runtime already loaded, locally, before the AR SDK: as
//! the daemon has it, GStreamer's C++ plugins having opened it. The SDK's
//! `libNVCVImage.so` exports its own GCC 8 `libstdc++`; without the
//! module's global preload of the system runtime, `NvAR_Load` of the
//! face-box model aborts with `free(): invalid size`. A test binary of its
//! own, so that the load order is that of a fresh process. `#[ignore]`: it
//! needs the AR SDK, the os-release override and an NVIDIA GPU (the command
//! is in `hardware.rs`).

mod support;

use std::collections::BTreeMap;

use obc_gpu::DeviceNv12Buffer;
use support::gpu;

const W: u32 = 1920;
const H: u32 = 1080;

#[test]
#[ignore = "needs the NVIDIA Maxine AR SDK, the os-release override and an NVIDIA GPU"]
fn gpu_auto_frame_loads_with_the_system_cxx_runtime_already_loaded() {
    // As GStreamer's plugins do in the daemon: the system runtime, local.
    let name = c"libstdc++.so.6";
    // SAFETY: a NUL-terminated library name; the handle is leaked.
    let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!h.is_null(), "libstdc++.so.6 did not load");

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
        let mut chain = gpu::ar_chain(&module, g, W, H, &[gpu::auto_frame()], "tight");
        for i in 0..10 {
            frame.upload_packed(&input).unwrap();
            gpu::run_chain(&mut *chain, g, &frame, &none, i % 5 == 4);
            stream.synchronize().unwrap();
        }
        let out = frame.download_packed().unwrap();
        assert_eq!(gpu::diff(&input, &out).1, 0, "the frame changed");
    });
}
