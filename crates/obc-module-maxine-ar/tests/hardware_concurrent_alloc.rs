//! Eye contact's first frames while another thread allocates GPU memory, as
//! the Video Effects module's loads (TensorRT engines, `NvCVImage_Alloc`) do
//! in the daemon while eye contact already runs.
//!
//! The SDK's `GazeRedirection` captures CUDA graphs on its first frames,
//! by default, with `cudaStreamCaptureModeGlobal`: while that capture runs,
//! a potentially unsafe call such as `cuMemAlloc` from any other thread
//! fails with "operation not permitted when stream is capturing", the
//! capture is invalidated, the feature's run fails (`NvAR_Run` -26) and
//! leaves its stream capturing, and every later unsafe call in the process
//! fails too; a TensorRT engine load that meets it throws an uncaught
//! exception and aborts the daemon. The module turns the feature's CUDA
//! graphs off; this test pins that no capture gets in another thread's way.
//!
//! The feature captures its graphs only once it has a face, so the test
//! needs `OBC_TEST_FACE_IMAGE` (a 1920x1080 still, as in `hardware.rs`) to
//! be able to fail; without it the frames have no face and it says so.
//!
//! A test binary of its own: a failure can leave the process's CUDA state
//! broken for any test after it. `#[ignore]`: it needs the AR SDK, the
//! os-release override and an NVIDIA GPU (the command is in `hardware.rs`).

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use obc_backend::ChainFrame;
use obc_gpu::{ContextGuard, DeviceBuffer, DeviceNv12Buffer};
use support::gpu::{self, H, W};

/// Fresh loads of the feature: each one captures its graphs anew on its
/// first frames when the graphs are on.
const LOADS: usize = 6;
/// Frames per load: the captures are on the first ones.
const FRAMES: usize = 5;

#[test]
#[ignore = "needs the NVIDIA Maxine AR SDK, the os-release override and an NVIDIA GPU"]
fn gpu_eye_contact_runs_while_another_thread_allocates() {
    let dir = tempfile::tempdir().unwrap();
    let module = gpu::ar_module(dir.path());
    gpu::rig(|g, stream| {
        let input = match std::env::var_os("OBC_TEST_FACE_IMAGE").map(PathBuf::from) {
            Some(path) => {
                let (bgr, w, h) = gpu::read_ppm(&path);
                assert_eq!(
                    (w, h),
                    (W as usize, H as usize),
                    "the face still is not {W}x{H}"
                );
                gpu::bgr_to_nv12(&bgr, w, h)
            }
            None => {
                println!(
                    "no OBC_TEST_FACE_IMAGE: the feature captures its graphs only on a face, \
                     so this run without one cannot fail"
                );
                gpu::bgr_to_nv12(
                    &gpu::faceless_room(W as usize, H as usize),
                    W as usize,
                    H as usize,
                )
            }
        };
        let frame = DeviceNv12Buffer::alloc(W, H).unwrap();
        frame.upload_packed(&input).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let allocator = {
            let stop = Arc::clone(&stop);
            let ctx = *g;
            std::thread::spawn(move || {
                // The whole (Send) pair, not its raw pointer field.
                let ctx = ctx;
                // SAFETY: the rig's context outlives this thread (joined
                // below, inside the rig).
                let _g = unsafe { ContextGuard::push(ctx.context) }.unwrap();
                let (mut allocs, mut errors) = (0u64, Vec::new());
                while !stop.load(Ordering::Relaxed) && errors.len() < 5 {
                    match DeviceBuffer::alloc_pitch(1920, 16) {
                        Ok(b) => drop(b),
                        Err(e) => errors.push(e.to_string()),
                    }
                    allocs += 1;
                }
                (allocs, errors)
            })
        };

        let mut failures = Vec::new();
        for load in 0..LOADS {
            let mut chain = gpu::eye_chain(&module, g, W, H);
            for i in 0..FRAMES {
                let f = frame.frame();
                if let Err(e) =
                    chain.process(ChainFrame::Cuda { gpu: g, frame: &f }, &BTreeMap::new())
                {
                    failures.push(format!("load {load}, frame {i}: {e}"));
                }
                if let Err(e) = stream.synchronize() {
                    failures.push(format!("load {load}, frame {i}: synchronize: {e}"));
                }
            }
            chain.unload();
        }
        stop.store(true, Ordering::Relaxed);
        let (allocs, errors) = allocator.join().unwrap();
        println!("{allocs} allocations on the other thread");
        assert!(allocs > 0, "the other thread never allocated");
        assert!(
            errors.is_empty(),
            "the other thread's allocations failed while eye contact ran: {errors:?}"
        );
        assert!(failures.is_empty(), "eye contact failed: {failures:?}");
    });
}
