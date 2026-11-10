//! GPU tests for the driver helpers. All are `#[ignore]` and need an NVIDIA
//! GPU with its driver; run with
//! `cargo test -p obc-gpu --test hardware -- --ignored --test-threads=1`.
//! Without a GPU they fail (no test passes without running).
//!
//! The checkpoint tests (`checkpoint_*`) need a process of their own (CUDA's
//! state, initialised or parked, is the whole process's): each runs its body
//! in a child process of this test binary ([`in_own_process`]).

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use obc_backend::ColourSpec;
use obc_gpu::{
    ContextGuard, DeviceBuffer, DeviceNv12Buffer, Event, GpuError, Lut8, NothingToPark,
    OwnedContext, ParkOutcome, PinnedHostBuffer, ProcessState, Stream, UnparkOutcome,
    copy_nv12_async, current_context, download_2d, download_nv12_async, mem_get_info,
    memset_2d_async, park, parked, parked_refusals, primary_context_active, process_state,
    stream_synchronize, unpark, upload_nv12_async,
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

/// A frame downloaded into page-locked memory, changed on the host and
/// uploaded back, all ordered on one stream, comes back as written; a
/// page-locked buffer too small for a frame is refused.
#[test]
#[ignore = "needs an NVIDIA GPU"]
fn gpu_pinned_host_copies_round_trip_on_a_stream() {
    in_created_context(|stream| {
        let dev = DeviceNv12Buffer::alloc(W, H).unwrap();
        let mut packed = vec![0u8; (W * H * 3 / 2) as usize];
        for (i, b) in packed.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        dev.upload_packed(&packed).unwrap();
        let pitch = W as usize + 32;
        let mut pinned = PinnedHostBuffer::alloc(pitch * (H as usize * 3 / 2)).unwrap();
        assert!(
            pinned.nv12(W, H, pitch - 64).is_err(),
            "pitch below the width"
        );
        assert!(pinned.nv12(W, H * 2, pitch).is_err(), "too small");
        let host = pinned.nv12(W, H, pitch).unwrap();
        // SAFETY: our own frames of this context, synchronised below.
        unsafe {
            download_nv12_async(&dev.frame(), &host, stream.raw()).unwrap();
            stream_synchronize(stream.raw()).unwrap();
        }
        let bytes = pinned.as_slice().to_vec();
        let (w, h) = (W as usize, H as usize);
        for r in 0..h {
            assert_eq!(
                &bytes[r * pitch..r * pitch + w],
                &packed[r * w..(r + 1) * w]
            );
        }
        for r in 0..h / 2 {
            let at = (h + r) * pitch;
            assert_eq!(&bytes[at..at + w], &packed[(h + r) * w..(h + r + 1) * w]);
        }
        let buf = pinned.as_mut_slice();
        for r in 0..h + h / 2 {
            buf[r * pitch..r * pitch + w].fill(7);
        }
        let host = pinned.nv12(W, H, pitch).unwrap();
        // SAFETY: as above.
        unsafe {
            upload_nv12_async(&host, &dev.frame(), stream.raw()).unwrap();
            stream_synchronize(stream.raw()).unwrap();
        }
        assert!(dev.download_packed().unwrap().iter().all(|b| *b == 7));
        let small = DeviceNv12Buffer::alloc(W / 2, H).unwrap();
        // SAFETY: refused before any copy.
        let e = unsafe { download_nv12_async(&small.frame(), &host, stream.raw()) }.unwrap_err();
        assert!(matches!(e, GpuError::InvalidArgument(_)), "{e}");
    });
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

/// The variable that tells a child process of this binary which test body
/// to run ([`in_own_process`]).
const CHILD_VAR: &str = "OBC_GPU_TEST_CHILD";

/// What a child run of a test printed and how long it ran.
struct ChildRun {
    stdout: String,
    /// When the child exited, as wall-clock time.
    exited: SystemTime,
}

/// Runs test `name` again in a child process of this test binary, with
/// [`CHILD_VAR`] naming it, and fails if the child fails or does not exit
/// within `limit`. Returns `None` in the child itself, which then runs the
/// body; the parent gets what the child printed.
fn in_own_process(name: &str, limit: Duration) -> Option<ChildRun> {
    if std::env::var(CHILD_VAR).as_deref() == Ok(name) {
        return None;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            name,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_VAR, name)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("re-run this test binary");
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name}: the child process did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let exited = SystemTime::now();
    let stdout = reader.join().unwrap();
    print!("{stdout}");
    assert!(
        status.success(),
        "{name}: the child process failed: {status}"
    );
    assert!(
        stdout.contains("1 passed"),
        "{name}: the child ran no test:\n{stdout}"
    );
    Some(ChildRun { stdout, exited })
}

/// The `/dev/nvidia*` nodes process `pid` holds open.
fn nvidia_fds(pid: u32) -> Vec<PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter(|t| t.to_string_lossy().starts_with("/dev/nvidia"))
        .collect()
}

/// The lines of `/proc/self/maps` that map a `/dev/nvidia*` node.
fn nvidia_mappings() -> Vec<String> {
    std::fs::read_to_string("/proc/self/maps")
        .unwrap()
        .lines()
        .filter(|l| l.contains("/dev/nvidia"))
        .map(str::to_string)
        .collect()
}

/// The pids other than `pid` that hold `/dev/nvidia-uvm` open: the GPU
/// list of `nvidia-uvm` is system-wide, so it only says something about
/// this process when no other process has registered a GPU.
fn other_uvm_holders(pid: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&p| p != pid)
        .filter(|&p| {
            nvidia_fds(p)
                .iter()
                .any(|n| n.as_os_str() == "/dev/nvidia-uvm")
        })
        .collect()
}

/// The entries of `/proc/driver/nvidia-uvm/gpus/` (the GPUs `nvidia-uvm`
/// has registered); `None` when the driver does not offer the list.
fn uvm_gpus() -> Option<Vec<String>> {
    let dir = std::fs::read_dir("/proc/driver/nvidia-uvm/gpus").ok()?;
    Some(
        dir.flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
    )
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

#[test]
#[ignore = "needs an NVIDIA GPU with a driver that has the CUDA checkpoint API"]
fn checkpoint_a_process_that_never_initialised_cuda_has_nothing_to_park() {
    if in_own_process(
        "checkpoint_a_process_that_never_initialised_cuda_has_nothing_to_park",
        Duration::from_secs(30),
    )
    .is_some()
    {
        return;
    }
    let pid = std::process::id();
    assert_eq!(process_state(), Ok(None), "CUDA is not initialised here");
    assert_eq!(
        park(),
        Ok(ParkOutcome::NothingToPark(NothingToPark::NotInitialised))
    );
    assert!(!parked());
    assert_eq!(unpark(), Ok(UnparkOutcome::NotParked));
    assert!(
        nvidia_fds(pid).is_empty(),
        "asking opened a device: {:?}",
        nvidia_fds(pid)
    );
    assert_eq!(process_state(), Ok(None), "asking initialised CUDA");
}

#[test]
#[ignore = "needs an NVIDIA GPU with a driver that has the CUDA checkpoint API"]
fn checkpoint_parks_a_live_context_three_times_and_keeps_its_data() {
    if in_own_process(
        "checkpoint_parks_a_live_context_three_times_and_keeps_its_data",
        Duration::from_secs(60),
    )
    .is_some()
    {
        return;
    }
    let pid = std::process::id();
    let ctx = OwnedContext::create(0).expect("cuCtxCreate");
    let (w, h) = (1920usize, 1080usize);
    let pattern: Vec<u8> = (0..w * h).map(|i| (i % 253) as u8).collect();
    let buf = {
        // SAFETY: `ctx` outlives the guard.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let buf = DeviceBuffer::alloc_pitch(w, h).unwrap();
        buf.upload(&pattern, w).unwrap();
        buf
    };
    assert!(!nvidia_fds(pid).is_empty(), "a live context holds devices");
    let uvm_others = other_uvm_holders(pid);
    for cycle in 1..=3 {
        assert_eq!(process_state(), Ok(Some(ProcessState::Running)));
        let out = park().expect("park");
        let ParkOutcome::Parked { lock, checkpoint } = out else {
            panic!("cycle {cycle}: not parked: {out:?}");
        };
        assert!(parked());
        assert_eq!(process_state(), Ok(Some(ProcessState::Checkpointed)));
        assert_eq!(
            park(),
            Ok(ParkOutcome::NothingToPark(NothingToPark::AlreadyParked))
        );
        let fds = nvidia_fds(pid);
        assert!(fds.is_empty(), "cycle {cycle}: parked with {fds:?}");
        let maps = nvidia_mappings();
        assert!(maps.is_empty(), "cycle {cycle}: parked with {maps:?}");
        match (uvm_others.is_empty(), uvm_gpus()) {
            (true, Some(gpus)) => {
                assert!(gpus.is_empty(), "cycle {cycle}: UVM still lists {gpus:?}")
            }
            (true, None) => println!("cycle {cycle}: no nvidia-uvm GPU list to check"),
            (false, _) => println!(
                "cycle {cycle}: UVM registration not checked: pids {uvm_others:?} hold \
                 /dev/nvidia-uvm (the list is system-wide)"
            ),
        }
        let out = unpark().expect("unpark");
        let UnparkOutcome::Unparked { restore, unlock } = out else {
            panic!("cycle {cycle}: not unparked: {out:?}");
        };
        assert!(!parked());
        println!(
            "cycle {cycle}: park {:.1} ms (lock {:.1} + checkpoint {:.1}), unpark {:.1} ms \
             (restore {:.1} + unlock {:.1}), {:.1} MiB on the GPU",
            ms(lock + checkpoint),
            ms(lock),
            ms(checkpoint),
            ms(restore + unlock),
            ms(restore),
            ms(unlock),
            (w * h) as f64 / f64::from(1u32 << 20)
        );
        // SAFETY: `ctx` outlives the guard.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let mut back = vec![0u8; w * h];
        buf.download(&mut back, w).unwrap();
        assert!(back == pattern, "cycle {cycle}: the data changed");
    }
    assert_eq!(unpark(), Ok(UnparkOutcome::NotParked));
    assert_eq!(parked_refusals(), 0);
    {
        // SAFETY: `ctx` outlives the guard.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        drop(buf);
    }
    drop(ctx);
}

/// `CUcheckpointLockArgs`, for the raw calls of
/// [`checkpoint_unpark_brings_back_a_process_parked_by_another_tool`].
#[repr(C)]
struct RawLockArgs {
    timeout_ms: u32,
    reserved0: u32,
    reserved1: [u64; 7],
}

/// The checkpoint API called on this process directly, as another tool
/// (`cuda-checkpoint`) would: `obc_gpu` does not know about it.
struct OtherTool {
    lib: libloading::Library,
}

impl OtherTool {
    fn open() -> Self {
        // SAFETY: loading the NVIDIA driver library has no preconditions.
        let lib = unsafe { libloading::Library::new("libcuda.so.1") }.expect("libcuda.so.1");
        OtherTool { lib }
    }

    fn lock(&self) {
        let mut args = RawLockArgs {
            timeout_ms: 2000,
            reserved0: 0,
            reserved1: [0; 7],
        };
        // SAFETY: the documented signature; `args` is a valid, zero-reserved
        // struct.
        let rc = unsafe {
            let f = self
                .lib
                .get::<unsafe extern "C" fn(i32, *mut RawLockArgs) -> i32>(
                    b"cuCheckpointProcessLock\0",
                )
                .unwrap();
            f(std::process::id() as i32, &mut args)
        };
        assert_eq!(rc, 0, "cuCheckpointProcessLock");
    }

    fn checkpoint(&self) {
        // SAFETY: the documented signature; null args are the defaults.
        let rc = unsafe {
            let f = self
                .lib
                .get::<unsafe extern "C" fn(i32, *mut std::ffi::c_void) -> i32>(
                    b"cuCheckpointProcessCheckpoint\0",
                )
                .unwrap();
            f(std::process::id() as i32, std::ptr::null_mut())
        };
        assert_eq!(rc, 0, "cuCheckpointProcessCheckpoint");
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU with a driver that has the CUDA checkpoint API"]
fn checkpoint_unpark_brings_back_a_process_parked_by_another_tool() {
    if in_own_process(
        "checkpoint_unpark_brings_back_a_process_parked_by_another_tool",
        Duration::from_secs(60),
    )
    .is_some()
    {
        return;
    }
    let ctx = OwnedContext::create(0).expect("cuCtxCreate");
    let (w, h) = (1024usize, 512usize);
    let pattern: Vec<u8> = (0..w * h).map(|i| (i % 251) as u8).collect();
    let buf = {
        // SAFETY: `ctx` outlives the guard.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let buf = DeviceBuffer::alloc_pitch(w, h).unwrap();
        buf.upload(&pattern, w).unwrap();
        buf
    };
    let tool = OtherTool::open();
    let check_data = |what: &str| {
        // SAFETY: `ctx` outlives the guard.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        let mut back = vec![0u8; w * h];
        buf.download(&mut back, w).unwrap();
        assert!(back == pattern, "{what}: the data changed");
    };
    // Checkpointed behind obc_gpu's back: restored and unlocked.
    tool.lock();
    tool.checkpoint();
    assert!(!parked(), "obc_gpu did not park it");
    assert_eq!(process_state(), Ok(Some(ProcessState::Checkpointed)));
    let out = unpark().expect("unpark a checkpointed process");
    let UnparkOutcome::Unparked { restore, unlock } = out else {
        panic!("checkpointed by another tool: not unparked: {out:?}");
    };
    assert!(!restore.is_zero(), "restored");
    println!(
        "checkpointed by another tool: unparked in {:.1} ms (restore {:.1} + unlock {:.1})",
        ms(restore + unlock),
        ms(restore),
        ms(unlock)
    );
    assert_eq!(process_state(), Ok(Some(ProcessState::Running)));
    check_data("after a restore");
    // Only locked behind its back: unlocked, nothing to restore.
    tool.lock();
    assert_eq!(process_state(), Ok(Some(ProcessState::Locked)));
    let out = unpark().expect("unpark a locked process");
    assert!(
        matches!(out, UnparkOutcome::Unparked { restore, .. } if restore.is_zero()),
        "locked by another tool: {out:?}"
    );
    assert_eq!(process_state(), Ok(Some(ProcessState::Running)));
    check_data("after an unlock");
    // Running: nothing to do.
    assert_eq!(unpark(), Ok(UnparkOutcome::NotParked));
    assert_eq!(parked_refusals(), 0);
    {
        // SAFETY: `ctx` outlives the guard.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        drop(buf);
    }
    drop(ctx);
}

#[test]
#[ignore = "needs an NVIDIA GPU with a driver that has the CUDA checkpoint API"]
fn checkpoint_a_parked_process_exits_promptly() {
    const NAME: &str = "checkpoint_a_parked_process_exits_promptly";
    const MARK: &str = "parked, exiting at ";
    if let Some(run) = in_own_process(NAME, Duration::from_secs(30)) {
        let at: u128 = run
            .stdout
            .lines()
            .find_map(|l| l.split_once(MARK).map(|(_, v)| v))
            .and_then(|v| v.trim().parse().ok())
            .expect("the child's mark");
        let exited = run.exited.duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let took = Duration::from_nanos(u64::try_from(exited.saturating_sub(at)).unwrap());
        println!(
            "a parked process exited {:.1} ms after its test body",
            ms(took)
        );
        assert!(took < Duration::from_secs(5), "exit took {took:?}");
        return;
    }
    // A live context with memory, left to the exit handlers, parked.
    let ctx = OwnedContext::create(0).expect("cuCtxCreate");
    {
        // SAFETY: `ctx` outlives the guard.
        let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
        std::mem::forget(DeviceBuffer::alloc_pitch(4096, 1024).unwrap());
    }
    std::mem::forget(ctx);
    assert!(matches!(park(), Ok(ParkOutcome::Parked { .. })));
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    println!("{MARK}{}", now.as_nanos());
}
