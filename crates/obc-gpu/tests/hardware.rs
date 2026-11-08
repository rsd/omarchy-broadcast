//! GPU tests for the driver helpers. All are `#[ignore]` and need an NVIDIA
//! GPU with its driver; run with
//! `cargo test -p obc-gpu --test hardware -- --ignored --test-threads=1`.
//! Without a GPU they fail (no test passes without running).

use obc_backend::ColourSpec;
use obc_gpu::{
    ContextGuard, DeviceBuffer, DeviceNv12Buffer, Event, GpuError, Lut8, OwnedContext, Stream,
    copy_nv12_async, current_context, download_2d, mem_get_info, memset_2d_async,
    primary_context_active, stream_synchronize,
};

const W: u32 = 64;
const H: u32 = 32;

#[test]
#[ignore = "needs an NVIDIA GPU"]
fn gpu_black_fill_copy_and_events_in_a_created_context() {
    assert!(
        !primary_context_active(0).unwrap(),
        "the primary context is active before the test; another test leaked it"
    );
    let ctx = OwnedContext::create(0).expect("cuCtxCreate");
    assert!(
        current_context().unwrap().is_null(),
        "creation must leave the thread without a current context"
    );
    {
        // SAFETY: `ctx` outlives the guard.
        let guard = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        assert_eq!(current_context().unwrap(), guard.context());
        let (free, total) = mem_get_info().unwrap();
        assert!(free > 0 && free <= total, "free {free} total {total}");

        let stream = Stream::new(true).unwrap();
        let a = DeviceNv12Buffer::alloc(W, H).unwrap();
        let b = DeviceNv12Buffer::alloc(W, H).unwrap();
        let f = a.frame();
        assert!(f.y_pitch >= W as usize);
        assert_eq!(f.uv, f.y + (f.y_pitch * H as usize) as u64);

        for (colour, y) in [
            (ColourSpec::LIMITED_BT601, 16u8),
            (ColourSpec::FULL_BT601, 0),
        ] {
            let start = Event::new().unwrap();
            let end = Event::new().unwrap();
            // SAFETY (record, fill_black): `stream` is live in this context.
            unsafe { start.record(stream.raw()) }.unwrap();
            unsafe { a.fill_black(colour, stream.raw()) }.unwrap();
            // SAFETY: both frames are live allocations of this context.
            unsafe { copy_nv12_async(&a.frame(), &b.frame(), stream.raw()) }.unwrap();
            // SAFETY: as above.
            unsafe { end.record(stream.raw()) }.unwrap();
            stream.synchronize().unwrap();
            assert!(end.is_complete().unwrap());
            let ms = Event::elapsed_ms(&start, &end).unwrap();
            assert!(ms >= 0.0, "{ms}");

            let host = b.download_packed().unwrap();
            let (luma, chroma) = host.split_at((W * H) as usize);
            assert!(luma.iter().all(|&v| v == y), "Y plane not {y}");
            assert!(chroma.iter().all(|&v| v == 128), "UV plane not 128");
        }

        // Upload / download round trip.
        let pattern: Vec<u8> = (0..(W * H * 3 / 2) as usize)
            .map(|i| (i % 251) as u8)
            .collect();
        a.upload_packed(&pattern).unwrap();
        assert_eq!(a.download_packed().unwrap(), pattern);
        // Buffers and the stream drop before the guard (reverse order), with
        // the context still current.
    }
    assert!(current_context().unwrap().is_null());
    drop(ctx);
    assert!(
        !primary_context_active(0).unwrap(),
        "the helpers activated the primary context"
    );
}

/// The hardening ramp of the Maxine backend (`obc-backend-maxine`'s
/// `matte::hardening_lut(128, 224)`), restated so this crate's tests stand
/// alone.
fn ramp(lo: u32, hi: u32) -> [u8; 256] {
    std::array::from_fn(|m| {
        let m = m as u32;
        if m < lo {
            0
        } else {
            (((m - lo) * 255 + (hi - lo) / 2) / (hi - lo)).min(255) as u8
        }
    })
}

/// Bytes the kernel must never write: the pitch padding of the destination.
const SENTINEL: u8 = 0xAB;

/// Runs the kernel with `lut` over a `w x h` plane that holds every byte
/// value, and checks every pixel against the table on the host and every
/// padding byte against [`SENTINEL`]. Returns the destination pitch.
fn check_lut(kernel: &Lut8, stream: &Stream, lut: &[u8; 256], w: usize, h: usize) -> usize {
    let src = DeviceBuffer::alloc_pitch(w, h).unwrap();
    let dst = DeviceBuffer::alloc_pitch(w, h).unwrap();
    let table = DeviceBuffer::alloc_pitch(256, 1).unwrap();
    let input: Vec<u8> = (0..w * h).map(|i| ((i % w) + 7 * (i / w)) as u8).collect();
    src.upload(&input, w).unwrap();
    table.upload(lut, 256).unwrap();
    // SAFETY: both fills cover our own allocations, full pitch.
    unsafe {
        memset_2d_async(
            dst.ptr(),
            dst.pitch(),
            SENTINEL,
            dst.pitch(),
            h,
            stream.raw(),
        )
        .unwrap();
        // The synchronous uploads above ran on the legacy default stream;
        // wait for them before the non-blocking stream reads the buffers.
        stream_synchronize(std::ptr::null_mut()).unwrap();
        kernel
            .launch(
                src.ptr(),
                src.pitch(),
                dst.ptr(),
                dst.pitch(),
                w as u32,
                h as u32,
                table.ptr(),
                stream.raw(),
            )
            .unwrap();
    }
    stream.synchronize().unwrap();
    let pitch = dst.pitch();
    let mut out = vec![0u8; pitch * h];
    // SAFETY: the whole pitched rectangle lies inside our allocation.
    unsafe { download_2d(dst.ptr(), pitch, &mut out, pitch, pitch, h) }.unwrap();
    let mut seen = [false; 256];
    for y in 0..h {
        for x in 0..w {
            let v = input[y * w + x];
            seen[v as usize] = true;
            assert_eq!(
                out[y * pitch + x],
                lut[v as usize],
                "{w}x{h}: pixel ({x}, {y}) = lut[{v}]"
            );
        }
        assert!(
            out[y * pitch + w..(y + 1) * pitch]
                .iter()
                .all(|&b| b == SENTINEL),
            "{w}x{h}: the kernel wrote the padding of row {y}"
        );
    }
    assert!(
        seen.iter().all(|&s| s),
        "{w}x{h}: not every byte value tested"
    );
    pitch
}

/// Pushes a created context around `f` and checks that nothing activated
/// the primary context.
fn in_created_context(f: impl FnOnce(&Stream)) {
    assert!(
        !primary_context_active(0).unwrap(),
        "the primary context is active before the test; another test leaked it"
    );
    let ctx = OwnedContext::create(0).expect("cuCtxCreate");
    {
        // SAFETY: `ctx` outlives the guard; everything below drops first.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let stream = Stream::new(true).unwrap();
        f(&stream);
    }
    drop(ctx);
    assert!(!primary_context_active(0).unwrap());
}

#[test]
#[ignore = "needs an NVIDIA GPU"]
fn gpu_lut8_matches_the_host_table_bit_for_bit() {
    in_created_context(|stream| {
        let kernel = Lut8::load().expect("the embedded PTX loads");
        let identity: [u8; 256] = std::array::from_fn(|i| i as u8);
        let hardening = ramp(128, 224);
        // 167 is odd, so i -> 167 i + 13 permutes the byte values.
        let permutation: [u8; 256] = std::array::from_fn(|i| (i * 167 + 13) as u8);
        for (name, lut) in [
            ("identity", &identity),
            ("hardening", &hardening),
            ("permutation", &permutation),
        ] {
            // 513 is neither a multiple of the 32-thread block row nor of
            // any pitch alignment, so its last block is partial and the
            // pitch has padding; 1080 rows end in a partial 8-row block.
            for (w, h) in [(1920usize, 1080usize), (513, 289)] {
                let pitch = check_lut(&kernel, stream, lut, w, h);
                println!("LUT8 {name} {w}x{h}: exact, pitch {pitch}");
                if w == 513 {
                    assert!(pitch > w, "the 513 px plane has no padding to check");
                }
            }
        }
    });
}

#[test]
#[ignore = "needs an NVIDIA GPU"]
fn gpu_lut8_failures_are_errors_and_leave_the_context_usable() {
    in_created_context(|stream| {
        let e = Lut8::load_ptx(c"this is not PTX")
            .err()
            .expect("garbage PTX must not load");
        println!("garbage PTX: {e}");
        assert!(
            matches!(
                &e,
                GpuError::Call {
                    call: "cuModuleLoadData",
                    ..
                }
            ),
            "{e:?}"
        );

        let kernel = Lut8::load().expect("the context is usable after a failed load");
        let (w, h) = (513usize, 289usize);
        let src = DeviceBuffer::alloc_pitch(w, h).unwrap();
        let dst = DeviceBuffer::alloc_pitch(w, h).unwrap();
        let table = DeviceBuffer::alloc_pitch(256, 1).unwrap();
        // A block of 64 x 32 = 2048 threads exceeds every GPU's limit: the
        // driver refuses the launch synchronously.
        // SAFETY: all three are live allocations of the current context.
        let e = unsafe {
            kernel.launch_with_block(
                src.ptr(),
                src.pitch(),
                dst.ptr(),
                dst.pitch(),
                w as u32,
                h as u32,
                table.ptr(),
                stream.raw(),
                (64, 32),
            )
        }
        .unwrap_err();
        println!("oversized block: {e}");
        assert!(
            matches!(
                &e,
                GpuError::Call {
                    call: "cuLaunchKernel",
                    ..
                }
            ),
            "{e:?}"
        );
        stream
            .synchronize()
            .expect("a refused launch is not sticky");

        // Both failures left the context, the stream and the kernel usable.
        let identity: [u8; 256] = std::array::from_fn(|i| i as u8);
        check_lut(&kernel, stream, &identity, w, h);
    });
}
