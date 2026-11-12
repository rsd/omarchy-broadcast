//! The CUDA-frame path on a real GPU: a context made the way GStreamer's
//! CUDA library makes one, NV12 frames in device memory, the module loaded
//! for CUDA frames through `obc-module-host` and run with the context
//! current, as the effect element runs it. Ignored by default:
//!
//! ```sh
//! ORT_DYLIB_PATH=... OBC_ONNX_MODEL_DIR=... \
//!   cargo test --manifest-path crates/obc-module-onnx/Cargo.toml --test hardware -- --ignored
//! ```
//!
//! It also checks that the host's context is still the current one after
//! every call, whatever engine the CUDA runtime ran on, and that the
//! device's primary context (which ONNX Runtime's NVIDIA engines activate
//! through the CUDA runtime) is not left behind: the probe and the check
//! never activate it, a load that ends on the CPU leaves it inactive, and
//! an unload resets it, after which a load runs again.
//!
//! The case of `auto` with every GPU hidden (`CUDA_VISIBLE_DEVICES=""`)
//! cannot be run here: the variable is read once, when the process
//! initialises CUDA, and this test needs the GPU for its frames. There the
//! driver has no device at all, so no primary context can exist; the
//! `runtime` test covers that run.

mod common;

use common::{Log, host_chain, model_dir, nv12_of_rgb, open, runtime_env, s, test_image};
use obc_backend::Value;
use obc_gpu::{ContextGuard, DeviceNv12Buffer, OwnedContext, Stream, primary_context_active};
use obc_module_host::module::ChainSpec;
use obc_module_sdk::abi::{
    OBC_CHROMA_SITING_MPEG2, OBC_FORMAT_NV12, OBC_FRAME_CUDA, OBC_FRAME_FLAG_NEW_SOURCE,
    OBC_MATRIX_BT709, obc_cuda, obc_frame, obc_plane,
};
use obc_module_sdk::serde_json::json;

fn cuda_frame(b: &DeviceNv12Buffer, cuda: obc_cuda, flags: u32) -> obc_frame {
    let d = b.frame();
    let mut f = obc_frame::empty();
    f.kind = OBC_FRAME_CUDA;
    f.format = OBC_FORMAT_NV12;
    f.width = d.width;
    f.height = d.height;
    f.n_planes = 2;
    f.planes[0] = obc_plane {
        ptr_or_fd: d.y,
        pitch: d.y_pitch as u64,
        offset: 0,
    };
    f.planes[1] = obc_plane {
        ptr_or_fd: d.uv,
        pitch: d.uv_pitch as u64,
        offset: 0,
    };
    f.cuda = cuda;
    f.flags = flags;
    f
}

#[test]
#[ignore = "needs an NVIDIA GPU, ONNX Runtime and the models"]
fn cuda_frames_are_processed_and_the_hosts_context_stays_current() {
    let (_, models) = match runtime_env() {
        Ok(e) => e,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let tmp = tempfile::tempdir().unwrap();
    let models = model_dir(&models, tmp.path());
    let log = Log::default();
    assert!(
        !primary_context_active(0).unwrap(),
        "the primary context is active before the test"
    );
    let module = open(tmp.path(), json!({"model_dir": models}), &log);
    // The probe and the check make no CUDA call.
    module.probe().unwrap();
    module.check(&[s("background")]).unwrap();
    assert!(
        !primary_context_active(0).unwrap(),
        "the probe or the check activated the primary context"
    );

    let (w, h) = (1280usize, 720usize);
    let ctx = OwnedContext::create(0).expect("a CUDA device");
    let _current = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
    let stream = Stream::new(true).unwrap();
    let cuda = obc_cuda {
        context: ctx.raw(),
        stream: stream.raw(),
    };
    let (rgb, iw, ih) = test_image();
    let host = nv12_of_rgb(&rgb, iw, ih, w, h, w);
    let src = DeviceNv12Buffer::alloc(w as u32, h as u32).unwrap();
    let dst = DeviceNv12Buffer::alloc(w as u32, h as u32).unwrap();
    src.upload_packed(&host.data).unwrap();

    let engines: Vec<String> = std::env::var("OBC_ONNX_HW_ENGINES")
        .unwrap_or_else(|_| "auto,cpu,cuda,auto".into())
        .split(',')
        .map(str::to_string)
        .collect();
    // Whether an NVIDIA engine ran (not when the providers' libraries are
    // left off the library path, which makes `auto` end on the CPU).
    let mut nvidia = false;
    for engine in engines.iter().map(String::as_str) {
        let mut inst = module.create_instance().unwrap();
        let spec = ChainSpec {
            stages: vec![s("background")],
            kind: OBC_FRAME_CUDA,
            input_format: OBC_FORMAT_NV12,
            input_size: (w as u32, h as u32),
            output_format: OBC_FORMAT_NV12,
            output_size: (w as u32, h as u32),
            matrix: OBC_MATRIX_BT709,
            chroma_siting: OBC_CHROMA_SITING_MPEG2,
            full_range: false,
            cuda,
            params: vec![
                (s("background"), "mode".into(), Value::Str("blur".into())),
                (s("background"), "engine".into(), Value::Str(engine.into())),
            ],
            background: None,
        };
        log.take();
        let report = inst.load(&spec).unwrap();
        assert_eq!(
            obc_gpu::current_context().unwrap(),
            ctx.raw(),
            "after the load"
        );
        let lines = log.take();
        let on = lines
            .iter()
            .rev()
            .find_map(|l| l.split(" runs on ").nth(1))
            .map(|r| r.split([',', '(']).next().unwrap().trim().to_string())
            .unwrap_or_default();
        eprintln!(
            "{engine}: on {on}, {report:?}; primary context active: {}",
            primary_context_active(0).unwrap()
        );
        nvidia |= on.starts_with("NVIDIA");
        if on == "CPU" {
            // Every NVIDIA attempt that failed is gone, and with it the
            // primary context it may have activated.
            assert!(
                !primary_context_active(0).unwrap(),
                "a load on the CPU left the primary context active: {lines:#?}"
            );
        }
        let strength = Value::Float(1.0);
        let live = [("background", "blur_strength", &strength)];
        let mut ms = Vec::new();
        for i in 0..20 {
            let (fi, fo) = (
                cuda_frame(
                    &src,
                    cuda,
                    if i == 0 { OBC_FRAME_FLAG_NEW_SOURCE } else { 0 },
                ),
                cuda_frame(&dst, cuda, 0),
            );
            // SAFETY: device frames of the current context, valid during
            // the call.
            unsafe { inst.process(&fi, &fo, &live) }.unwrap();
            assert_eq!(
                obc_gpu::current_context().unwrap(),
                ctx.raw(),
                "after a frame"
            );
            ms.push(inst.timings().iter().map(|(_, t)| t).sum::<f32>());
        }
        stream.synchronize().unwrap();
        let out = dst.download_packed().unwrap();
        let same = out
            .iter()
            .zip(&host.data)
            .take(w * h)
            .filter(|(a, b)| (i32::from(**a) - i32::from(**b)).abs() <= 2)
            .count() as f64
            / (w * h) as f64;
        ms.sort_by(f32::total_cmp);
        eprintln!(
            "{engine}: {:.1} % of the picture kept, {:.2} ms a frame (median); steps {:?}",
            100.0 * same,
            ms[ms.len() / 2],
            inst.timings()
        );
        assert!((0.02..0.98).contains(&same), "{same}");
        inst.unload();
        assert_eq!(
            obc_gpu::current_context().unwrap(),
            ctx.raw(),
            "after the unload"
        );
        assert!(
            !primary_context_active(0).unwrap(),
            "the unload left the primary context active"
        );
        // The host's context and its memory still work after the reset.
        stream.synchronize().unwrap();
        assert_eq!(dst.download_packed().unwrap().len(), out.len());
        src.upload_packed(&host.data).unwrap();
    }

    if !nvidia {
        eprintln!("no NVIDIA engine ran: the two-chain and host-frame steps are not run");
        return;
    }
    // Two GPU chains at once: the primary context is reset only when the
    // last goes, and the other keeps running meanwhile.
    let mut a = module.create_instance().unwrap();
    let mut b = module.create_instance().unwrap();
    let chain = |engine: &str| ChainSpec {
        stages: vec![s("background")],
        kind: OBC_FRAME_CUDA,
        input_format: OBC_FORMAT_NV12,
        input_size: (w as u32, h as u32),
        output_format: OBC_FORMAT_NV12,
        output_size: (w as u32, h as u32),
        matrix: OBC_MATRIX_BT709,
        chroma_siting: OBC_CHROMA_SITING_MPEG2,
        full_range: false,
        cuda,
        params: vec![
            (s("background"), "mode".into(), Value::Str("blur".into())),
            (s("background"), "engine".into(), Value::Str(engine.into())),
        ],
        background: None,
    };
    a.load(&chain("cuda")).unwrap();
    b.load(&chain("cuda")).unwrap();
    a.unload();
    assert!(
        primary_context_active(0).unwrap(),
        "the primary context was reset while another GPU session was alive"
    );
    let (fi, fo) = (
        cuda_frame(&src, cuda, OBC_FRAME_FLAG_NEW_SOURCE),
        cuda_frame(&dst, cuda, 0),
    );
    // SAFETY: device frames of the current context, valid during the call.
    unsafe { b.process(&fi, &fo, &[]) }.unwrap();
    b.unload();
    assert!(!primary_context_active(0).unwrap(), "after the last unload");
    assert_eq!(obc_gpu::current_context().unwrap(), ctx.raw());

    drop(_current);
    assert!(
        obc_gpu::current_context().unwrap().is_null(),
        "the host's context was popped"
    );

    // Host frames with no context current: the load leaves none current
    // (not the primary context), and the unload resets it.
    let mut inst = module.create_instance().unwrap();
    inst.load(&host_chain(
        w as u32,
        h as u32,
        &[("mode", "blur"), ("engine", "cuda")],
        None,
    ))
    .unwrap();
    assert!(
        obc_gpu::current_context().unwrap().is_null(),
        "the load left a context current on a thread that had none"
    );
    let mut hin = host.clone();
    let mut hout = host.clone();
    let (fi, fo) = (hin.frame(OBC_FRAME_FLAG_NEW_SOURCE), hout.frame(0));
    // SAFETY: host memory of this test, valid during the call.
    unsafe { inst.process(&fi, &fo, &[]) }.unwrap();
    assert!(
        obc_gpu::current_context().unwrap().is_null(),
        "after a frame"
    );
    inst.unload();
    assert!(
        obc_gpu::current_context().unwrap().is_null(),
        "after the unload"
    );
    assert!(
        !primary_context_active(0).unwrap(),
        "host frames: after the unload"
    );

    // Frames in the primary context itself: the unload must not reset it
    // (that would free the frames under the host), and says why. Last, as
    // it leaves the primary context active.
    let primary = PrimaryContext::retain();
    let _current = unsafe { ContextGuard::push(primary.0) }.unwrap();
    let stream = Stream::new(true).unwrap();
    let cuda = obc_cuda {
        context: primary.0,
        stream: stream.raw(),
    };
    let src = DeviceNv12Buffer::alloc(w as u32, h as u32).unwrap();
    let dst = DeviceNv12Buffer::alloc(w as u32, h as u32).unwrap();
    src.upload_packed(&host.data).unwrap();
    let mut inst = module.create_instance().unwrap();
    inst.load(&ChainSpec {
        cuda,
        ..chain("cuda")
    })
    .unwrap();
    let (fi, fo) = (
        cuda_frame(&src, cuda, OBC_FRAME_FLAG_NEW_SOURCE),
        cuda_frame(&dst, cuda, 0),
    );
    // SAFETY: device frames of the current context, valid during the call.
    unsafe { inst.process(&fi, &fo, &[]) }.unwrap();
    log.take();
    inst.unload();
    let lines = log.take();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the CUDA primary context is not reset")),
        "{lines:#?}"
    );
    assert!(
        primary_context_active(0).unwrap(),
        "frames in the primary context"
    );
    assert_eq!(obc_gpu::current_context().unwrap(), primary.0);
    // Its memory and stream still work.
    stream.synchronize().unwrap();
    assert_eq!(dst.download_packed().unwrap().len(), host.data.len());
    drop((src, dst, stream));
    drop(_current);
}

/// Device 0's primary context, retained for the test (released on drop).
struct PrimaryContext(obc_gpu::CuContext);

type Retain = unsafe extern "C" fn(*mut obc_gpu::CuContext, i32) -> i32;
type Release = unsafe extern "C" fn(i32) -> i32;

fn libcuda() -> &'static libloading::Library {
    static LIB: std::sync::OnceLock<libloading::Library> = std::sync::OnceLock::new();
    // SAFETY: the driver library, kept for the process.
    LIB.get_or_init(|| unsafe { libloading::Library::new("libcuda.so.1") }.unwrap())
}

impl PrimaryContext {
    fn retain() -> PrimaryContext {
        let mut ctx: obc_gpu::CuContext = std::ptr::null_mut();
        // SAFETY: the driver's signature; device 0 (cuDeviceGet(0) is 0 on
        // this machine's only GPU, and the driver is initialised).
        let rc = unsafe {
            let f: libloading::Symbol<Retain> = libcuda().get(b"cuDevicePrimaryCtxRetain").unwrap();
            f(&mut ctx, 0)
        };
        assert_eq!(rc, 0, "cuDevicePrimaryCtxRetain");
        PrimaryContext(ctx)
    }
}

impl Drop for PrimaryContext {
    fn drop(&mut self) {
        // SAFETY: releases the use retained above.
        unsafe {
            let f: libloading::Symbol<Release> =
                libcuda().get(b"cuDevicePrimaryCtxRelease_v2").unwrap();
            f(0);
        }
    }
}
