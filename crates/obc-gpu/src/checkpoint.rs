//! Parking this process's CUDA state with the driver's checkpoint API.
//!
//! Once CUDA has been initialised in a process (`cuInit`, which GStreamer's
//! CUDA library runs on the first session), `nvidia-uvm` keeps the GPU
//! registered, and awake, until the process exits: destroying every context
//! does not undo it. The checkpoint API does. [`park`] locks the process's
//! CUDA API (`cuCheckpointProcessLock`) and checkpoints it
//! (`cuCheckpointProcessCheckpoint`): GPU memory is copied to host memory
//! and every device handle is released, leaving no `/dev/nvidia*`
//! descriptor or mapping open. [`unpark`] restores it
//! (`cuCheckpointProcessRestore`) and unlocks it (`cuCheckpointProcessUnlock`):
//! contexts and memory come back as they were.
//!
//! **While parked, any CUDA call blocks** until the process is unparked, on
//! whichever thread makes it. The helpers of this crate refuse instead:
//! [`crate::driver`] returns [`GpuError::Parked`] while [`parked`] is true,
//! counted in [`parked_refusals`] for the caller to watch (a refusal is a
//! caller that ran while it should not have; it never panics, as it may run
//! in a `Drop` or on a GStreamer streaming thread). Callers that reach the
//! driver some other way (GStreamer's CUDA library, the Maxine SDK) must
//! simply not run while parked.
//!
//! The entry points are resolved from `libcuda.so.1` on their own, without
//! [`crate::driver`] and without `cuInit`: opening the library opens no
//! device, and initialising CUDA just to find out there is nothing to park
//! would register the GPU. A driver without the entry points (older than the
//! checkpoint API), or no driver at all, makes both calls return
//! `Unsupported` with the reason, as values: nothing else changes. An error
//! returned by one of the calls themselves, `CUDA_ERROR_NOT_SUPPORTED`
//! included (it can depend on what the process used the GPU for), is an
//! error of that call only: the next one may succeed.
//!
//! Only the calling process is ever parked: the pid passed to the driver is
//! always [`std::process::id`]. Calls are serialised.

use std::ffi::{CStr, OsStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use libloading::Library;

use crate::GpuError;
use crate::driver::{CUDA_ERROR_NOT_READY, CUDA_SUCCESS, CuResult, DRIVER_LIBRARY, sym};

/// `CUDA_ERROR_NOT_INITIALIZED`: what `cuCheckpointProcessGetState` returns
/// for a process that never called `cuInit` (measured: the state is not
/// written).
const CUDA_ERROR_NOT_INITIALIZED: CuResult = 3;

/// How long [`park`] waits for CUDA calls in progress on other threads to
/// return before giving up (`CUcheckpointLockArgs::timeoutMs`). On timeout
/// the process stays running and [`park`] returns an error.
pub const PARK_LOCK_TIMEOUT: Duration = Duration::from_millis(2000);

/// `CUprocessState`: where a process is in the checkpoint cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// The default: CUDA calls run.
    Running,
    /// The CUDA API is locked: further calls block.
    Locked,
    /// Locked, GPU memory in host memory and device handles released.
    Checkpointed,
    /// An uncorrectable error during a checkpoint or restore.
    Failed,
    /// A checkpoint is under way.
    Checkpointing,
    /// A restore is under way.
    Restoring,
    /// A value this crate does not know.
    Unknown(i32),
}

impl ProcessState {
    fn from_raw(v: i32) -> Self {
        match v {
            0 => ProcessState::Running,
            1 => ProcessState::Locked,
            2 => ProcessState::Checkpointed,
            3 => ProcessState::Failed,
            4 => ProcessState::Checkpointing,
            5 => ProcessState::Restoring,
            other => ProcessState::Unknown(other),
        }
    }
}

/// Why [`park`] had nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NothingToPark {
    /// CUDA was never initialised in this process: the GPU is not held.
    NotInitialised,
    /// The process is parked already.
    AlreadyParked,
}

/// What [`park`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParkOutcome {
    /// The process is parked; how long the lock and the checkpoint took.
    Parked {
        lock: Duration,
        checkpoint: Duration,
    },
    /// Nothing was done.
    NothingToPark(NothingToPark),
    /// Parking is not possible here, with the reason (no driver, or one
    /// without the checkpoint API): it never will be in this process.
    /// Nothing was done.
    Unsupported(String),
}

/// Why [`park`] did not park, by what it leaves behind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParkError {
    /// The process runs as before the call: CUDA calls run, and [`parked`]
    /// is false. A later [`park`] may succeed.
    #[error("{0}")]
    Awake(GpuError),
    /// The process does not run normally: a checkpoint failed and unlocking
    /// again failed too, so it stays locked, or another tool left it locked
    /// or in a state this crate does not park from. CUDA calls block (or
    /// fail) until [`unpark`] brings it back; [`parked`] is true meanwhile.
    #[error("{0}")]
    Blocked(GpuError),
}

impl ParkError {
    /// The driver's error, whatever the process was left in.
    pub fn error(&self) -> &GpuError {
        match self {
            ParkError::Awake(e) | ParkError::Blocked(e) => e,
        }
    }
}

/// What [`unpark`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnparkOutcome {
    /// The process runs again; how long the restore and the unlock took
    /// (the restore is zero when the process was only locked). Also what a
    /// process checkpointed or locked by another tool gets.
    Unparked { restore: Duration, unlock: Duration },
    /// The process was not parked: nothing was done.
    NotParked,
    /// Parking is not possible here, with the reason. Nothing was done.
    Unsupported(String),
}

/// `CUcheckpointLockArgs`: a timeout in milliseconds (0: none, so never
/// passed), then reserved words that must be zero; 64 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct LockArgs {
    timeout_ms: u32,
    reserved0: u32,
    reserved1: [u64; 7],
}

impl LockArgs {
    /// At least 1 ms: 0 would wait for ever.
    fn with_timeout(timeout: Duration) -> Self {
        LockArgs {
            timeout_ms: u32::try_from(timeout.as_millis())
                .unwrap_or(u32::MAX)
                .max(1),
            reserved0: 0,
            reserved1: [0; 7],
        }
    }
}

type GetStateFn = unsafe extern "C" fn(i32, *mut i32) -> CuResult;
type LockFn = unsafe extern "C" fn(i32, *mut LockArgs) -> CuResult;
/// Checkpoint, restore and unlock: the arguments are always null (their
/// layouts differ between CUDA versions, and the defaults are wanted).
type ArgsFn = unsafe extern "C" fn(i32, *mut c_void) -> CuResult;
type ErrorTextFn = unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult;

/// The checkpoint entry points.
struct Api {
    _lib: Option<Library>,
    get_state: GetStateFn,
    lock: LockFn,
    checkpoint: ArgsFn,
    restore: ArgsFn,
    unlock: ArgsFn,
    error_name: ErrorTextFn,
    error_string: ErrorTextFn,
}

impl Api {
    /// Opens the driver library at `path` and resolves the entry points;
    /// the reason parking is unsupported otherwise. Calls nothing.
    fn open(path: &OsStr) -> Result<Api, String> {
        // SAFETY: loading the NVIDIA driver library runs its initialisers,
        // which have no preconditions (and open no device); any other library
        // at `path` is the caller's choice (tests).
        let lib = unsafe { Library::new(path) }.map_err(|e| {
            format!(
                "the NVIDIA CUDA driver could not be loaded ({}: {e})",
                path.to_string_lossy()
            )
        })?;
        let unsupported = |e: GpuError| {
            format!("{e}; this NVIDIA driver has no CUDA checkpoint API, so the GPU stays awake")
        };
        // SAFETY (all `sym` calls): each type is the documented C signature
        // of the entry point (`CUprocessState` is an int-sized enum; the
        // args structs are only ever passed as these pointers).
        let api = unsafe {
            Api {
                get_state: sym(&lib, "cuCheckpointProcessGetState").map_err(unsupported)?,
                lock: sym(&lib, "cuCheckpointProcessLock").map_err(unsupported)?,
                checkpoint: sym(&lib, "cuCheckpointProcessCheckpoint").map_err(unsupported)?,
                restore: sym(&lib, "cuCheckpointProcessRestore").map_err(unsupported)?,
                unlock: sym(&lib, "cuCheckpointProcessUnlock").map_err(unsupported)?,
                error_name: sym(&lib, "cuGetErrorName").map_err(unsupported)?,
                error_string: sym(&lib, "cuGetErrorString").map_err(unsupported)?,
                _lib: Some(lib),
            }
        };
        Ok(api)
    }

    fn text(&self, f: ErrorTextFn, code: CuResult) -> Option<String> {
        let mut p: *const c_char = std::ptr::null();
        // SAFETY: `p` is a valid out-pointer; on success the driver stores a
        // pointer to a static NUL-terminated string. Needs no `cuInit`.
        let rc = unsafe { f(code, &mut p) };
        if rc != CUDA_SUCCESS || p.is_null() {
            return None;
        }
        // SAFETY: the driver returned a static, NUL-terminated string.
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }

    fn call_error(&self, call: &'static str, code: CuResult) -> GpuError {
        GpuError::Call {
            call,
            code,
            name: self
                .text(self.error_name, code)
                .unwrap_or_else(|| format!("CUresult {code}")),
            description: self
                .text(self.error_string, code)
                .unwrap_or_else(|| "unknown error".into()),
        }
    }

    fn check(&self, call: &'static str, rc: CuResult) -> Result<(), GpuError> {
        if rc == CUDA_SUCCESS {
            Ok(())
        } else {
            Err(self.call_error(call, rc))
        }
    }

    /// The state of process `pid`: `Ok(None)` when CUDA was never
    /// initialised in it. Never initialises CUDA.
    fn state(&self, pid: i32) -> Result<Option<ProcessState>, GpuError> {
        let mut raw = -1i32;
        // SAFETY: `raw` is a valid out-pointer; the call has no side effect
        // and does not block while the process is locked or checkpointed.
        let rc = unsafe { (self.get_state)(pid, &mut raw) };
        match rc {
            CUDA_SUCCESS => Ok(Some(ProcessState::from_raw(raw))),
            CUDA_ERROR_NOT_INITIALIZED => Ok(None),
            _ => Err(self.call_error("cuCheckpointProcessGetState", rc)),
        }
    }
}

/// Whether the process is parked (a CUDA call would block): set by [`park`]
/// before it locks, and by [`park`] or [`unpark`] whenever they find the
/// process in any state other than running (checkpointed, locked or
/// failed, by another tool or by an earlier attempt); [`unpark`] sets it
/// before its restore or unlock, and it stays set when that restore or
/// unlock fails, or the state allows neither. Cleared by [`unpark`] once
/// the process runs again (or is found running, or CUDA never
/// initialised), and by a [`park`] that failed and left it running.
static PARKED: AtomicBool = AtomicBool::new(false);
/// How many times a helper refused to call the driver while parked.
static REFUSALS: AtomicU64 = AtomicU64::new(0);
/// Serialises [`park`] and [`unpark`].
static SERIAL: Mutex<()> = Mutex::new(());
static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(|| Api::open(OsStr::new(DRIVER_LIBRARY)))
        .as_ref()
        .map_err(Clone::clone)
}

/// Whether this process is parked (between a [`park`] that parked and the
/// [`unpark`] that restored it). Every helper of this crate refuses to run
/// meanwhile ([`GpuError::Parked`]).
pub fn parked() -> bool {
    PARKED.load(Ordering::SeqCst)
}

/// How many driver calls this crate refused because the process was
/// parked: each one is a caller that ran while it should not have. Nothing
/// here logs or panics on a refusal (it may happen in a `Drop`, or on a
/// streaming thread): the caller watches this count, and tests check it
/// stays 0.
pub fn parked_refusals() -> u64 {
    REFUSALS.load(Ordering::SeqCst)
}

/// [`GpuError::Parked`] when `flag` is set (the guard of
/// [`crate::driver`]), counted in `refusals`.
pub(crate) fn refuse_if_parked(flag: &AtomicBool, refusals: &AtomicU64) -> Result<(), GpuError> {
    if !flag.load(Ordering::SeqCst) {
        return Ok(());
    }
    refusals.fetch_add(1, Ordering::SeqCst);
    Err(GpuError::Parked)
}

/// The guard every helper of this crate passes through ([`crate::driver`]):
/// a refusal is an error value and a count, never a panic.
pub(crate) fn guard() -> Result<(), GpuError> {
    refuse_if_parked(&PARKED, &REFUSALS)
}

/// The calling process's checkpoint state: `None` when CUDA was never
/// initialised in it. Never initialises CUDA and never blocks.
pub fn process_state() -> Result<Option<ProcessState>, GpuError> {
    let api = api().map_err(GpuError::Unavailable)?;
    api.state(own_pid())
}

fn own_pid() -> i32 {
    i32::try_from(std::process::id()).unwrap_or(i32::MAX)
}

/// Parks this process (see the module docs): afterwards it holds no GPU
/// device, and every CUDA call, from any thread, blocks until [`unpark`].
/// Call it only once nothing in the process uses CUDA any more.
///
/// Does nothing when CUDA was never initialised here or the process is
/// parked already ([`ParkOutcome::NothingToPark`]; checkpointed by another
/// tool counts as parked), or when the driver cannot
/// ([`ParkOutcome::Unsupported`]: no driver library, or one without the
/// entry points). An error says what the process was left in. Usually
/// [`ParkError::Awake`], running as before the call: the state could not
/// be read, the lock timed out ([`PARK_LOCK_TIMEOUT`]: another thread
/// stayed inside a CUDA call), the lock or the checkpoint failed (the
/// process is unlocked again), `CUDA_ERROR_NOT_SUPPORTED` included.
/// [`ParkError::Blocked`] when unlocking after a failed checkpoint failed
/// too, or the process was found locked (or mid-checkpoint, or failed):
/// [`parked`] is then true, and an [`unpark`] must bring it back before
/// CUDA is used.
pub fn park() -> Result<ParkOutcome, ParkError> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let api = match api() {
        Ok(a) => a,
        Err(reason) => return Ok(ParkOutcome::Unsupported(reason)),
    };
    park_with(api, own_pid(), &PARKED, PARK_LOCK_TIMEOUT)
}

/// Unparks this process: its CUDA state is restored and unlocked, and CUDA
/// calls run again. The driver's own state decides, not only [`parked`]: a
/// process checkpointed or locked by another tool (e.g. `cuda-checkpoint`)
/// is restored or unlocked all the same. Does nothing when it is running
/// or CUDA was never initialised ([`UnparkOutcome::NotParked`]); the state
/// query initialises nothing. An error leaves it as it was, and [`parked`]
/// true whenever the driver said it is not running (also when another tool
/// parked it): no CUDA call may be made.
pub fn unpark() -> Result<UnparkOutcome, GpuError> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let api = match api() {
        Ok(a) => a,
        // Without the entry points nothing can have parked the process.
        Err(reason) if PARKED.load(Ordering::SeqCst) => {
            return Ok(UnparkOutcome::Unsupported(reason));
        }
        Err(_) => return Ok(UnparkOutcome::NotParked),
    };
    unpark_with(api, own_pid(), &PARKED)
}

fn park_with(
    api: &Api,
    pid: i32,
    flag: &AtomicBool,
    timeout: Duration,
) -> Result<ParkOutcome, ParkError> {
    if flag.load(Ordering::SeqCst) {
        return Ok(ParkOutcome::NothingToPark(NothingToPark::AlreadyParked));
    }
    let state = match api.state(pid) {
        Ok(None) => return Ok(ParkOutcome::NothingToPark(NothingToPark::NotInitialised)),
        Ok(Some(s)) => s,
        Err(e) => return Err(ParkError::Awake(e)),
    };
    match state {
        ProcessState::Running => {}
        // Checkpointed by someone else (e.g. the `cuda-checkpoint` tool):
        // parked all the same, and `unpark` restores it.
        ProcessState::Checkpointed => {
            flag.store(true, Ordering::SeqCst);
            return Ok(ParkOutcome::NothingToPark(NothingToPark::AlreadyParked));
        }
        // Locked (or mid-checkpoint, or failed) by someone else: CUDA calls
        // would block, so the helpers refuse; `unpark` brings it back if
        // the state allows.
        other => {
            flag.store(true, Ordering::SeqCst);
            return Err(ParkError::Blocked(GpuError::Unavailable(format!(
                "cannot park the GPU: this process's CUDA state is {other:?}, not Running"
            ))));
        }
    }
    // Set first: a helper racing the lock is refused rather than blocked.
    flag.store(true, Ordering::SeqCst);
    let mut args = LockArgs::with_timeout(timeout);
    let t = Instant::now();
    // SAFETY: `args` is a valid, zero-reserved CUcheckpointLockArgs; the
    // call returns at the latest after the timeout.
    let rc = unsafe { (api.lock)(pid, &mut args) };
    let lock = t.elapsed();
    if rc != CUDA_SUCCESS {
        // The lock was not taken: the process runs on.
        flag.store(false, Ordering::SeqCst);
        let e = api.call_error("cuCheckpointProcessLock", rc);
        return Err(ParkError::Awake(if rc == CUDA_ERROR_NOT_READY {
            GpuError::Unavailable(format!(
                "{e} (another thread stayed inside a CUDA call for {} ms)",
                args.timeout_ms
            ))
        } else {
            e
        }));
    }
    let t = Instant::now();
    // SAFETY: the process is locked; null args checkpoint to host memory.
    let rc = unsafe { (api.checkpoint)(pid, std::ptr::null_mut()) };
    let checkpoint = t.elapsed();
    if rc != CUDA_SUCCESS {
        let e = api.call_error("cuCheckpointProcessCheckpoint", rc);
        // SAFETY: the process is locked (a failed checkpoint leaves it so);
        // null args.
        let urc = unsafe { (api.unlock)(pid, std::ptr::null_mut()) };
        if urc != CUDA_SUCCESS {
            // Still locked: every CUDA call would block; `unpark` unlocks.
            return Err(ParkError::Blocked(GpuError::Unavailable(format!(
                "{e}; unlocking again failed too ({}): the process stays locked until it is \
                 unparked",
                api.call_error("cuCheckpointProcessUnlock", urc)
            ))));
        }
        flag.store(false, Ordering::SeqCst);
        return Err(ParkError::Awake(e));
    }
    Ok(ParkOutcome::Parked { lock, checkpoint })
}

fn unpark_with(api: &Api, pid: i32, flag: &AtomicBool) -> Result<UnparkOutcome, GpuError> {
    let state = match api.state(pid) {
        Ok(s) => s,
        // Not parked here and its state unreadable: CUDA is used as it was
        // before parking existed, rather than failing every start.
        Err(_) if !flag.load(Ordering::SeqCst) => return Ok(UnparkOutcome::NotParked),
        Err(e) => return Err(e),
    };
    let state = match state {
        None | Some(ProcessState::Running) => {
            flag.store(false, Ordering::SeqCst);
            return Ok(UnparkOutcome::NotParked);
        }
        Some(s) => s,
    };
    // Not running: CUDA calls would block. Set before anything else (it is
    // clear when another tool parked the process), so that a restore or an
    // unlock that fails leaves the helpers refusing rather than blocking;
    // cleared only once the process runs again.
    flag.store(true, Ordering::SeqCst);
    let restore = match state {
        ProcessState::Checkpointed => {
            let t = Instant::now();
            // SAFETY: the process is checkpointed; null args restore from
            // host memory onto the same GPU.
            let rc = unsafe { (api.restore)(pid, std::ptr::null_mut()) };
            api.check("cuCheckpointProcessRestore", rc)?;
            t.elapsed()
        }
        // Restored but not unlocked (an earlier unlock failed), or a park
        // whose unlock failed, or locked by another tool.
        ProcessState::Locked => Duration::ZERO,
        other => {
            return Err(GpuError::Unavailable(format!(
                "cannot unpark the GPU: this process's CUDA state is {other:?}"
            )));
        }
    };
    let t = Instant::now();
    // SAFETY: the process is locked (restored); null args.
    let rc = unsafe { (api.unlock)(pid, std::ptr::null_mut()) };
    api.check("cuCheckpointProcessUnlock", rc)?;
    let unlock = t.elapsed();
    flag.store(false, Ordering::SeqCst);
    Ok(UnparkOutcome::Unparked { restore, unlock })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn a_missing_driver_library_makes_parking_unsupported() {
        let reason = Api::open(OsStr::new("libobc-no-such-cuda.so.1"))
            .err()
            .expect("opening a missing library must fail");
        assert!(reason.contains("libobc-no-such-cuda.so.1"), "{reason}");
        assert!(reason.contains("could not be loaded"), "{reason}");
    }

    #[test]
    fn a_driver_without_the_checkpoint_api_makes_parking_unsupported() {
        // libc is always present and exports none of the CUDA symbols.
        let reason = Api::open(OsStr::new("libc.so.6"))
            .err()
            .expect("libc has no checkpoint API");
        assert!(
            reason.contains("has no cuCheckpointProcessGetState"),
            "{reason}"
        );
        assert!(reason.contains("no CUDA checkpoint API"), "{reason}");
    }

    #[test]
    fn lock_args_match_the_documented_layout() {
        // unsigned int timeoutMs, unsigned int reserved0, cuuint64_t[7].
        assert_eq!(std::mem::size_of::<LockArgs>(), 64);
        assert_eq!(std::mem::offset_of!(LockArgs, timeout_ms), 0);
        assert_eq!(std::mem::offset_of!(LockArgs, reserved0), 4);
        assert_eq!(std::mem::offset_of!(LockArgs, reserved1), 8);
        let a = LockArgs::with_timeout(PARK_LOCK_TIMEOUT);
        assert_eq!(a.timeout_ms, 2000);
        assert_eq!((a.reserved0, a.reserved1), (0, [0; 7]));
        assert_eq!(
            LockArgs::with_timeout(Duration::from_secs(u64::MAX)).timeout_ms,
            u32::MAX
        );
        // 0 is no timeout at all: anything shorter than 1 ms waits 1 ms.
        assert_eq!(LockArgs::with_timeout(Duration::ZERO).timeout_ms, 1);
        assert_eq!(
            LockArgs::with_timeout(Duration::from_micros(999)).timeout_ms,
            1
        );
        assert_eq!(
            LockArgs::with_timeout(Duration::from_millis(7)).timeout_ms,
            7
        );
    }

    #[test]
    fn process_states_map_from_the_enum() {
        let all = [
            ProcessState::Running,
            ProcessState::Locked,
            ProcessState::Checkpointed,
            ProcessState::Failed,
            ProcessState::Checkpointing,
            ProcessState::Restoring,
        ];
        for (i, s) in all.into_iter().enumerate() {
            assert_eq!(ProcessState::from_raw(i as i32), s);
        }
        assert_eq!(ProcessState::from_raw(9), ProcessState::Unknown(9));
    }

    #[test]
    fn the_guard_refuses_while_parked_and_counts() {
        let flag = AtomicBool::new(false);
        let refusals = AtomicU64::new(0);
        assert_eq!(refuse_if_parked(&flag, &refusals), Ok(()));
        flag.store(true, Ordering::SeqCst);
        assert_eq!(refuse_if_parked(&flag, &refusals), Err(GpuError::Parked));
        assert_eq!(refuse_if_parked(&flag, &refusals), Err(GpuError::Parked));
        assert_eq!(refusals.load(Ordering::SeqCst), 2);
        flag.store(false, Ordering::SeqCst);
        assert_eq!(refuse_if_parked(&flag, &refusals), Ok(()));
    }

    /// The variable naming the test a child run of this test binary is for
    /// ([`in_own_process`]).
    const CHILD_VAR: &str = "OBC_GPU_UNIT_TEST_CHILD";

    /// Whether this is the child run of test `name` (its path in this test
    /// binary). Otherwise runs this test binary again in a child process,
    /// with only that test, and fails if the child fails: a test that sets
    /// the process-wide [`PARKED`] then cannot race another test that goes
    /// through `driver()`.
    fn in_own_process(name: &str) -> bool {
        if std::env::var(CHILD_VAR).as_deref() == Ok(name) {
            return true;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_VAR, name)
            .output()
            .expect("re-run this test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{name}: the child process failed: {}\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "{name}: the child ran no test:\n{stdout}"
        );
        false
    }

    #[test]
    fn every_helper_refuses_while_the_process_is_parked() {
        // Sets the process-wide flag: in a child process of its own, so no
        // other test of this binary sees it. With the flag set, `driver()`
        // returns before opening the library.
        if !in_own_process("checkpoint::tests::every_helper_refuses_while_the_process_is_parked") {
            return;
        }
        let before = parked_refusals();
        PARKED.store(true, Ordering::SeqCst);
        let results = [
            crate::driver().map(|_| ()),
            crate::device_count().map(|_| ()),
            crate::current_context().map(|_| ()),
            crate::OwnedContext::create(0).map(|_| ()),
            crate::DeviceBuffer::alloc_pitch(64, 4).map(|_| ()),
        ];
        PARKED.store(false, Ordering::SeqCst);
        for r in results {
            assert_eq!(r, Err(GpuError::Parked));
        }
        assert_eq!(parked_refusals() - before, 5);
        let e = obc_backend::BackendError::from(GpuError::Parked);
        assert!(
            matches!(&e, obc_backend::BackendError::Unavailable(m) if m.contains("parked")),
            "{e:?}"
        );
    }

    /// A scripted driver: the state and the result of each call, and the
    /// calls made, per thread (the fake entry points run on the test's
    /// thread).
    /// `CUDA_ERROR_NOT_SUPPORTED`.
    const CUDA_ERROR_NOT_SUPPORTED: CuResult = 801;

    #[derive(Default)]
    struct Script {
        /// `Err(rc)`: GetState returns `rc`.
        state: Option<Result<i32, CuResult>>,
        lock: CuResult,
        checkpoint: CuResult,
        restore: CuResult,
        unlock: CuResult,
        calls: Vec<&'static str>,
        lock_timeout_ms: Option<u32>,
    }

    thread_local! {
        static SCRIPT: RefCell<Script> = RefCell::new(Script::default());
    }

    fn script<R>(f: impl FnOnce(&mut Script) -> R) -> R {
        SCRIPT.with(|s| f(&mut s.borrow_mut()))
    }

    unsafe extern "C" fn fake_state(_pid: i32, out: *mut i32) -> CuResult {
        script(|s| {
            s.calls.push("state");
            match s.state.unwrap_or(Ok(0)) {
                Ok(v) => {
                    // SAFETY: the caller passes a valid out-pointer.
                    unsafe { *out = v };
                    CUDA_SUCCESS
                }
                Err(rc) => rc,
            }
        })
    }
    unsafe extern "C" fn fake_lock(_pid: i32, args: *mut LockArgs) -> CuResult {
        script(|s| {
            s.calls.push("lock");
            // SAFETY: the caller passes a valid args struct.
            s.lock_timeout_ms = Some(unsafe { (*args).timeout_ms });
            if s.lock == CUDA_SUCCESS {
                s.state = Some(Ok(1));
            }
            s.lock
        })
    }
    unsafe extern "C" fn fake_checkpoint(_pid: i32, args: *mut c_void) -> CuResult {
        script(|s| {
            assert!(args.is_null());
            s.calls.push("checkpoint");
            if s.checkpoint == CUDA_SUCCESS {
                s.state = Some(Ok(2));
            }
            s.checkpoint
        })
    }
    unsafe extern "C" fn fake_restore(_pid: i32, args: *mut c_void) -> CuResult {
        script(|s| {
            assert!(args.is_null());
            s.calls.push("restore");
            if s.restore == CUDA_SUCCESS {
                s.state = Some(Ok(1));
            }
            s.restore
        })
    }
    unsafe extern "C" fn fake_unlock(_pid: i32, args: *mut c_void) -> CuResult {
        script(|s| {
            assert!(args.is_null());
            s.calls.push("unlock");
            if s.unlock == CUDA_SUCCESS {
                s.state = Some(Ok(0));
            }
            s.unlock
        })
    }
    unsafe extern "C" fn fake_text(_code: CuResult, out: *mut *const c_char) -> CuResult {
        // SAFETY: the caller passes a valid out-pointer; the string is static.
        unsafe { *out = c"FAKE_ERROR".as_ptr() };
        CUDA_SUCCESS
    }

    fn fake_api(s: Script) -> Api {
        SCRIPT.with(|c| *c.borrow_mut() = s);
        Api {
            _lib: None,
            get_state: fake_state,
            lock: fake_lock,
            checkpoint: fake_checkpoint,
            restore: fake_restore,
            unlock: fake_unlock,
            error_name: fake_text,
            error_string: fake_text,
        }
    }

    fn calls() -> Vec<&'static str> {
        script(|s| std::mem::take(&mut s.calls))
    }

    const T: Duration = Duration::from_millis(1500);

    #[test]
    fn a_process_that_never_initialised_cuda_has_nothing_to_park() {
        let api = fake_api(Script {
            state: Some(Err(CUDA_ERROR_NOT_INITIALIZED)),
            ..Script::default()
        });
        let flag = AtomicBool::new(false);
        assert_eq!(
            park_with(&api, 1, &flag, T),
            Ok(ParkOutcome::NothingToPark(NothingToPark::NotInitialised))
        );
        assert_eq!(calls(), ["state"], "no lock attempted");
        assert!(!flag.load(Ordering::SeqCst));
        // Not parked: unpark asks nothing of the driver beyond the state.
        assert_eq!(unpark_with(&api, 1, &flag), Ok(UnparkOutcome::NotParked));
    }

    #[test]
    fn park_then_unpark_runs_the_four_calls_in_order() {
        let api = fake_api(Script::default());
        let flag = AtomicBool::new(false);
        let out = park_with(&api, 1, &flag, T).unwrap();
        assert!(matches!(out, ParkOutcome::Parked { .. }), "{out:?}");
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(calls(), ["state", "lock", "checkpoint"]);
        assert_eq!(script(|s| s.lock_timeout_ms), Some(1500));
        // Parked: a second park does nothing, without asking the driver.
        assert_eq!(
            park_with(&api, 1, &flag, T),
            Ok(ParkOutcome::NothingToPark(NothingToPark::AlreadyParked))
        );
        assert!(calls().is_empty());
        let out = unpark_with(&api, 1, &flag).unwrap();
        assert!(matches!(out, UnparkOutcome::Unparked { .. }), "{out:?}");
        assert_eq!(calls(), ["state", "restore", "unlock"]);
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn a_lock_timeout_leaves_the_process_running() {
        let api = fake_api(Script {
            lock: CUDA_ERROR_NOT_READY,
            ..Script::default()
        });
        let flag = AtomicBool::new(false);
        let e = park_with(&api, 1, &flag, T).unwrap_err();
        assert!(matches!(e, ParkError::Awake(_)), "{e:?}");
        assert!(
            e.to_string()
                .contains("stayed inside a CUDA call for 1500 ms"),
            "{e}"
        );
        assert_eq!(calls(), ["state", "lock"]);
        assert!(!flag.load(Ordering::SeqCst), "not parked");
    }

    #[test]
    fn a_failed_checkpoint_unlocks_again() {
        let api = fake_api(Script {
            checkpoint: 2,
            ..Script::default()
        });
        let flag = AtomicBool::new(false);
        let e = park_with(&api, 1, &flag, T).unwrap_err();
        assert!(
            matches!(
                &e,
                ParkError::Awake(GpuError::Call {
                    call: "cuCheckpointProcessCheckpoint",
                    code: 2,
                    ..
                })
            ),
            "{e:?}"
        );
        assert_eq!(calls(), ["state", "lock", "checkpoint", "unlock"]);
        assert!(!flag.load(Ordering::SeqCst));
        assert_eq!(script(|s| s.state), Some(Ok(0)), "running again");
    }

    #[test]
    fn a_failed_checkpoint_and_unlock_stay_parked_until_unpark_unlocks() {
        let api = fake_api(Script {
            checkpoint: 2,
            unlock: 999,
            ..Script::default()
        });
        let flag = AtomicBool::new(false);
        let e = park_with(&api, 1, &flag, T).unwrap_err();
        assert!(matches!(e, ParkError::Blocked(_)), "{e:?}");
        assert!(e.to_string().contains("stays locked"), "{e}");
        assert!(flag.load(Ordering::SeqCst), "locked: helpers must refuse");
        assert_eq!(script(|s| s.state), Some(Ok(1)), "locked");
        calls();
        script(|s| s.unlock = CUDA_SUCCESS);
        let out = unpark_with(&api, 1, &flag).unwrap();
        assert_eq!(
            calls(),
            ["state", "unlock"],
            "locked, not checkpointed: no restore"
        );
        assert!(
            matches!(out, UnparkOutcome::Unparked { restore, .. } if restore.is_zero()),
            "{out:?}"
        );
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn a_failed_restore_stays_parked_and_can_be_retried() {
        let api = fake_api(Script::default());
        let flag = AtomicBool::new(false);
        park_with(&api, 1, &flag, T).unwrap();
        script(|s| s.restore = 2);
        calls();
        let e = unpark_with(&api, 1, &flag).unwrap_err();
        assert!(
            matches!(
                &e,
                GpuError::Call {
                    call: "cuCheckpointProcessRestore",
                    ..
                }
            ),
            "{e:?}"
        );
        assert_eq!(calls(), ["state", "restore"]);
        assert!(flag.load(Ordering::SeqCst), "still parked");
        script(|s| s.restore = CUDA_SUCCESS);
        assert!(matches!(
            unpark_with(&api, 1, &flag),
            Ok(UnparkOutcome::Unparked { .. })
        ));
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn a_call_that_is_not_supported_fails_this_park_only() {
        // From the state query, the lock or the checkpoint: an error of that
        // park, the process left running, and the next park tries again.
        for (sc, made) in [
            (
                Script {
                    state: Some(Err(CUDA_ERROR_NOT_SUPPORTED)),
                    ..Script::default()
                },
                &["state"][..],
            ),
            (
                Script {
                    lock: CUDA_ERROR_NOT_SUPPORTED,
                    ..Script::default()
                },
                &["state", "lock"][..],
            ),
            (
                Script {
                    checkpoint: CUDA_ERROR_NOT_SUPPORTED,
                    ..Script::default()
                },
                &["state", "lock", "checkpoint", "unlock"][..],
            ),
        ] {
            let api = fake_api(sc);
            let flag = AtomicBool::new(false);
            let e = park_with(&api, 1, &flag, T).unwrap_err();
            assert!(
                matches!(&e, ParkError::Awake(GpuError::Call { code: 801, .. })),
                "{made:?}: {e:?}"
            );
            assert_eq!(calls(), made);
            assert!(!flag.load(Ordering::SeqCst), "{made:?}: not parked");
            script(|s| {
                s.state = Some(Ok(0));
                s.lock = CUDA_SUCCESS;
                s.checkpoint = CUDA_SUCCESS;
            });
            assert!(
                matches!(park_with(&api, 1, &flag, T), Ok(ParkOutcome::Parked { .. })),
                "{made:?}: the next park"
            );
            calls();
        }
    }

    #[test]
    fn unpark_follows_the_driver_state_even_when_this_crate_did_not_park() {
        // The flag clear: e.g. checkpointed or locked by `cuda-checkpoint`.
        for (raw, made, unparked) in [
            (Err(CUDA_ERROR_NOT_INITIALIZED), &["state"][..], false),
            (Ok(0), &["state"][..], false),
            (Ok(2), &["state", "restore", "unlock"][..], true),
            (Ok(1), &["state", "unlock"][..], true),
        ] {
            let api = fake_api(Script {
                state: Some(raw),
                ..Script::default()
            });
            let flag = AtomicBool::new(false);
            let out = unpark_with(&api, 1, &flag).unwrap();
            assert_eq!(calls(), made, "state {raw:?}");
            if unparked {
                assert!(
                    matches!(out, UnparkOutcome::Unparked { .. }),
                    "state {raw:?}: {out:?}"
                );
                assert_eq!(script(|s| s.state), Some(Ok(0)), "running again");
            } else {
                assert_eq!(out, UnparkOutcome::NotParked, "state {raw:?}");
            }
            assert!(!flag.load(Ordering::SeqCst));
        }
        // A state that cannot be unparked from is an error.
        let api = fake_api(Script {
            state: Some(Ok(3)),
            ..Script::default()
        });
        let flag = AtomicBool::new(false);
        let e = unpark_with(&api, 1, &flag).unwrap_err();
        assert!(e.to_string().contains("Failed"), "{e}");
        assert_eq!(calls(), ["state"]);
    }

    #[test]
    fn an_unpark_of_a_process_another_tool_parked_that_fails_still_refuses_cuda() {
        // The flag clear (parked by `cuda-checkpoint`, say): it is set
        // before the restore or the unlock, and stays set when they fail.
        for (raw, sc, made) in [
            (
                2,
                Script {
                    restore: 2,
                    ..Script::default()
                },
                &["state", "restore"][..],
            ),
            (
                1,
                Script {
                    unlock: 999,
                    ..Script::default()
                },
                &["state", "unlock"][..],
            ),
            // Restored, then the unlock failed: still locked.
            (
                2,
                Script {
                    unlock: 999,
                    ..Script::default()
                },
                &["state", "restore", "unlock"][..],
            ),
            // A state that cannot be unparked from: CUDA calls would block
            // (or fail) all the same.
            (3, Script::default(), &["state"][..]),
        ] {
            let api = fake_api(Script {
                state: Some(Ok(raw)),
                ..sc
            });
            let flag = AtomicBool::new(false);
            assert!(unpark_with(&api, 1, &flag).is_err(), "state {raw}");
            assert_eq!(calls(), made, "state {raw}");
            assert!(flag.load(Ordering::SeqCst), "state {raw}: helpers refuse");
        }
        // A retry that works clears it.
        let api = fake_api(Script {
            state: Some(Ok(2)),
            restore: 2,
            ..Script::default()
        });
        let flag = AtomicBool::new(false);
        assert!(unpark_with(&api, 1, &flag).is_err());
        assert!(flag.load(Ordering::SeqCst));
        script(|s| s.restore = CUDA_SUCCESS);
        calls();
        assert!(matches!(
            unpark_with(&api, 1, &flag),
            Ok(UnparkOutcome::Unparked { .. })
        ));
        assert_eq!(calls(), ["state", "restore", "unlock"]);
        assert!(!flag.load(Ordering::SeqCst), "running again");
    }

    #[test]
    fn the_flag_is_set_before_a_foreign_restore_or_unlock_runs() {
        // Seen from inside the restore and the unlock: no moment where the
        // process is not running and the flag is clear.
        thread_local! {
            static FLAG: std::cell::Cell<*const AtomicBool> =
                const { std::cell::Cell::new(std::ptr::null()) };
            static SEEN: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
        }
        fn note() {
            FLAG.with(|f| {
                // SAFETY: set below to a flag that outlives the call.
                let set = unsafe { &*f.get() }.load(Ordering::SeqCst);
                SEEN.with(|s| s.borrow_mut().push(set));
            });
        }
        unsafe extern "C" fn restore(pid: i32, args: *mut c_void) -> CuResult {
            note();
            // SAFETY: forwards the caller's arguments.
            unsafe { fake_restore(pid, args) }
        }
        unsafe extern "C" fn unlock(pid: i32, args: *mut c_void) -> CuResult {
            note();
            // SAFETY: forwards the caller's arguments.
            unsafe { fake_unlock(pid, args) }
        }
        let mut api = fake_api(Script {
            state: Some(Ok(2)),
            ..Script::default()
        });
        api.restore = restore;
        api.unlock = unlock;
        let flag = AtomicBool::new(false);
        FLAG.with(|f| f.set(&flag));
        assert!(matches!(
            unpark_with(&api, 1, &flag),
            Ok(UnparkOutcome::Unparked { .. })
        ));
        FLAG.with(|f| f.set(std::ptr::null()));
        assert_eq!(SEEN.with(|s| s.take()), [true, true]);
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn an_unreadable_state_fails_an_unpark_only_when_parked_here() {
        let api = fake_api(Script {
            state: Some(Err(999)),
            ..Script::default()
        });
        let flag = AtomicBool::new(false);
        // Not parked here: used as before parking existed.
        assert_eq!(unpark_with(&api, 1, &flag), Ok(UnparkOutcome::NotParked));
        flag.store(true, Ordering::SeqCst);
        let e = unpark_with(&api, 1, &flag).unwrap_err();
        assert!(
            matches!(
                &e,
                GpuError::Call {
                    call: "cuCheckpointProcessGetState",
                    code: 999,
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(flag.load(Ordering::SeqCst), "still parked");
        assert_eq!(calls(), ["state", "state"]);
    }

    #[test]
    fn a_process_in_another_state_is_not_parked() {
        for (raw, blocked) in [(1, true), (3, true), (2, false)] {
            let api = fake_api(Script {
                state: Some(Ok(raw)),
                ..Script::default()
            });
            let flag = AtomicBool::new(false);
            let out = park_with(&api, 1, &flag, T);
            assert_eq!(calls(), ["state"], "state {raw}: no lock");
            // Either way the helpers must refuse: CUDA calls would block.
            assert!(flag.load(Ordering::SeqCst), "state {raw}");
            if blocked {
                // Locked or failed from outside: not running, not parked.
                let e = out.unwrap_err();
                assert!(matches!(e, ParkError::Blocked(_)), "{e:?}");
                assert!(e.to_string().contains("not Running"), "{e}");
            } else {
                // Checkpointed from outside: parked, and unpark restores it.
                assert_eq!(
                    out,
                    Ok(ParkOutcome::NothingToPark(NothingToPark::AlreadyParked))
                );
            }
        }
    }
}
