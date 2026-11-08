//! GPU tests for the driver helpers. All are `#[ignore]` and need an NVIDIA
//! GPU with its driver; run with
//! `cargo test -p obc-gpu --test hardware -- --ignored --test-threads=1`.
//! Without a GPU they fail (no test passes without running).

use obc_backend::ColourSpec;
use obc_gpu::{
    ContextGuard, DeviceNv12Buffer, Event, OwnedContext, Stream, copy_nv12_async, current_context,
    mem_get_info, primary_context_active,
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
