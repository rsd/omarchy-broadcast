//! GPU tests of the module's chain run in this process, where Rust code can
//! arm a backend's fault switch (which nothing that crosses the ABI can):
//! the module built as the daemon loads it is tested in `tests/hardware.rs`.
//!
//! All are `#[ignore]`: they need the NVIDIA Maxine Video Effects SDK, the
//! os-release override and an NVIDIA GPU, with the environment of
//! `tests/hardware.rs`:
//!
//! ```text
//! set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a
//! CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$MAXINE_VFX_OS_RELEASE_SHIM" \
//!   cargo test -p obc-module-maxine --lib gpu_ -- --ignored --test-threads=1 --nocapture
//! ```

use std::ffi::{CString, c_char};

use obc_backend_maxine::truehdr::{TrueHdrFault, TrueHdrFaultSwitch};
use obc_gpu::{ContextGuard, DeviceNv12Buffer, OwnedContext, Stream};
use obc_module_sdk::ErrorKind;
use obc_module_sdk::abi::{
    OBC_FORMAT_NV12, OBC_FRAME_CUDA, OBC_MATRIX_BT601, obc_chain_request, obc_cuda, obc_frame,
    obc_param, obc_plane, obc_value,
};

use super::*;

const W: u32 = 1280;
const H: u32 = 720;

/// A head-and-shoulders silhouette over a gradient room, BT.601 limited
/// range NV12, as `tests/hardware.rs` draws it.
fn person() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let (cx, head_cy, head_r) = (w as f64 / 2.0, h as f64 * 0.38, h as f64 * 0.17);
    let rgb = |x: usize, y: usize| -> (f64, f64, f64) {
        let (fx, fy) = (x as f64, y as f64);
        let head = ((fx - cx) / (head_r * 0.8)).powi(2) + ((fy - head_cy) / head_r).powi(2) <= 1.0;
        let shoulders = fy > h as f64 * 0.58
            && (fx - cx).abs() < w as f64 * 0.22 + (fy - h as f64 * 0.58) * 0.6;
        if head {
            (200.0, 130.0, 90.0)
        } else if shoulders {
            (30.0, 40.0, 60.0)
        } else {
            (120.0, (140 + y * 40 / h) as f64, (150 + x * 60 / w) as f64)
        }
    };
    let mut out = vec![0u8; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = rgb(x, y);
            let l = 16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0;
            out[y * w + x] = l.round().clamp(0.0, 255.0) as u8;
        }
    }
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let (r, g, b) = rgb(2 * x, 2 * y);
            let u = 128.0 + (-37.797 * r - 74.203 * g + 112.0 * b) / 255.0;
            let v = 128.0 + (112.0 * r - 93.786 * g - 18.214 * b) / 255.0;
            out[w * h + y * w + 2 * x] = u.round().clamp(0.0, 255.0) as u8;
            out[w * h + y * w + 2 * x + 1] = v.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let sum: u64 = a.iter().zip(b).map(|(&x, &y)| x.abs_diff(y) as u64).sum();
    sum as f64 / a.len() as f64
}

/// The CUDA NV12 frame over `buf`, on `gpu`.
fn frame_of(buf: &DeviceNv12Buffer, gpu: &GpuContext) -> obc_frame {
    let f = buf.frame();
    let mut o = obc_frame::empty();
    o.kind = OBC_FRAME_CUDA;
    o.format = OBC_FORMAT_NV12;
    o.width = f.width;
    o.height = f.height;
    o.n_planes = 2;
    o.planes[0] = obc_plane {
        ptr_or_fd: f.y,
        pitch: f.y_pitch as u64,
        offset: 0,
    };
    o.planes[1] = obc_plane {
        ptr_or_fd: f.uv,
        pitch: f.uv_pitch as u64,
        offset: 0,
    };
    o.cuda = obc_cuda {
        context: gpu.context,
        stream: gpu.stream,
    };
    o
}

/// Loads `stages` (with the background's `mode` `blur`) for CUDA NV12
/// frames of `W` x `H` on `gpu`.
fn load(instance: &mut MaxineInstance, gpu: &GpuContext, stages: &[&str]) -> Result<(), Error> {
    let ids: Vec<CString> = stages.iter().map(|s| CString::new(*s).unwrap()).collect();
    let ptrs: Vec<*const c_char> = ids.iter().map(|s| s.as_ptr()).collect();
    let (background, mode, blur) = (c"background", c"mode", c"blur");
    let params = [obc_param {
        stage: background.as_ptr(),
        key: mode.as_ptr(),
        value: obc_value::from_str_ptr(blur.as_ptr()),
    }];
    let mut raw = obc_chain_request::empty();
    raw.stages = ptrs.as_ptr();
    raw.n_stages = ptrs.len();
    raw.input_kind = OBC_FRAME_CUDA;
    raw.input_format = OBC_FORMAT_NV12;
    raw.input_width = W;
    raw.input_height = H;
    raw.output_format = OBC_FORMAT_NV12;
    raw.output_width = W;
    raw.output_height = H;
    raw.matrix = OBC_MATRIX_BT601;
    raw.cuda = obc_cuda {
        context: gpu.context,
        stream: gpu.stream,
    };
    raw.params = params.as_ptr();
    raw.n_params = params.len();
    // SAFETY: `raw` and everything it points to live until the end of the
    // call.
    let req = unsafe { ChainRequest::from_ptr(&raw) }?;
    instance.load(&req).map(|_| ())
}

/// A `[background, hdr]` chain whose HDR look fails on a frame: the failure
/// is the HDR look's, the output of that frame holds the matted frame the
/// cut-out wrote (processed, not the input, not black), the HDR look is
/// dropped, and the next frames go out matted, without it.
#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK, the os-release override and an NVIDIA GPU"]
fn gpu_a_failed_hdr_look_after_the_cut_out_leaves_the_matted_frame() {
    let switch = TrueHdrFaultSwitch::default();
    let module = MaxineModule {
        effects: MaxineFactory::from_env(),
        enhance: MaxineVsrFactory::from_env(),
        framegen: MaxineVfgFactory::from_env(),
        truehdr: MaxineTrueHdrFactory::from_env().with_faults(switch.clone()),
        compute_capability: None,
    };
    assert!(
        module.effects.root().is_some(),
        "this test needs the Maxine SDK's environment (see the module documentation)"
    );
    let ctx = OwnedContext::create(0).unwrap();
    // SAFETY: `ctx` outlives the guard; everything below drops first.
    let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
    let stream = Stream::new(true).unwrap();
    let gpu = GpuContext {
        context: ctx.raw(),
        stream: stream.raw(),
    };
    let input = person();
    let src = DeviceNv12Buffer::alloc(W, H).unwrap();
    let dst = DeviceNv12Buffer::alloc(W, H).unwrap();
    src.upload_packed(&input).unwrap();
    let (raw_src, raw_dst) = (frame_of(&src, &gpu), frame_of(&dst, &gpu));
    let mut instance = module.create_instance().unwrap();
    let run = |instance: &mut MaxineInstance| -> Result<Vec<u8>, Error> {
        let r = instance.process(
            &Frame::new(&raw_src).unwrap(),
            &Frame::new(&raw_dst).unwrap(),
            &[],
        );
        stream.synchronize().unwrap();
        r.map(|()| dst.download_packed().unwrap())
    };
    load(&mut instance, &gpu, &["background", HDR])
        .unwrap_or_else(|e| panic!("[background, hdr] load failed: {e}"));
    assert!(instance.hdr_in_chain);
    let mut looked = Vec::new();
    for _ in 0..10 {
        looked = run(&mut instance).unwrap_or_else(|e| panic!("a frame failed: {e}"));
    }
    assert_ne!(looked, input, "the chain changed nothing");

    switch.set(Some(TrueHdrFault::Run));
    let e = run(&mut instance).unwrap_err();
    println!("FAULT hdr: {e}");
    assert_eq!(e.kind(), ErrorKind::Stage, "{e}");
    assert_eq!(e.failed_stage(), Some(HDR), "{e}");
    assert!(!instance.hdr_in_chain, "the HDR look is still in the chain");
    let matted = dst.download_packed().unwrap();
    assert_ne!(matted, input, "the failed frame holds the input");
    assert!(
        matted[..(W * H) as usize].iter().any(|&y| y > 40),
        "the failed frame is black"
    );
    let luma = (W * H) as usize;
    let look_d = mean_abs_diff(&matted[..luma], &looked[..luma]);
    println!("HDR FAULT: the look changes the luma by {look_d:.3} on average");
    // Measured 0.834 here; cut-out frame-to-frame noise measures 0.000.
    assert!(
        look_d > 0.3,
        "the failed frame holds the HDR look (luma differs by {look_d:.3})"
    );
    switch.set(None);

    // The next frames: matted, without the HDR look, like the failed one.
    let mut next = Vec::new();
    for _ in 0..3 {
        next = run(&mut instance).unwrap_or_else(|e| panic!("a frame after it failed: {e}"));
    }
    assert_ne!(next, input, "the cut-out stopped too");
    let d = mean_abs_diff(&next, &matted);
    println!("HDR FAULT: matted frames after it differ by {d:.3} on average");
    assert!(
        d < 1.0,
        "the frames after the failure are not the matted frame"
    );
    instance.unload();
}
