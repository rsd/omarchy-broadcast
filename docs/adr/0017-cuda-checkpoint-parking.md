# ADR-0017: Park the daemon's GPU state at Idle with the CUDA checkpoint API

- **Status:** Accepted (2026-11-09; amended 2026-11-09: a session that does not stop cleanly disables parking until the daemon restarts, and `unpark` sets the parked flag before it restores a process checkpointed by another tool). Amends [ADR-0006](0006-on-demand-gpu-lifecycle.md) (what Idle holds after an activation) and [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) (the worker-process fallback is replaced by this).

## Context
- [ADR-0006](0006-on-demand-gpu-lifecycle.md) needs the dGPU to be able to runtime-suspend while the daemon idles. [../benchmarks/dgpu-idle.md](../benchmarks/dgpu-idle.md) showed that tearing the pipeline down releases the CUDA context but leaves 7 `/dev/nvidia*` fds (`nvidia0` x4, `nvidiactl` x1, `nvidia-uvm` x2) until the process exits, and left open whether they block suspend. [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) named a worker process as the fallback.
- Two things keep the GPU awake after the first activation:
  - `cuInit`, run by GStreamer's CUDA library on the first session, makes `nvidia-uvm` register the GPU. That takes a keep-awake reference that nothing in the process releases until it exits.
  - The 7 fds. Opening `/dev/nvidia0` alone does not block suspend under fine-grained RTD3 (`DynamicPowerManagement=3`); the UVM registration does.
- The CUDA driver has a checkpoint API (`cuCheckpointProcess*`) that, on a process's own PID, releases its GPU state and restores it later.

## Decision
**The daemon parks itself: it checkpoints its own process when a session has fully stopped, and restores it before the next session touches CUDA. No second process.**

- **Park:** `cuCheckpointProcessLock`, then `cuCheckpointProcessCheckpoint`. It runs at the end of the session stop, after NVML has stopped, the GStreamer CUDA elements are at NULL, the effect worker has been joined and the Maxine instances destroyed.
- **Unpark:** `cuCheckpointProcessRestore`, then `cuCheckpointProcessUnlock`. It runs at the start of a session start, before anything touches CUDA (the output build, NVML).
- **After a park** the process holds 0 `/dev/nvidia*` fds and no `/dev/nvidia` mappings, and the GPU is no longer registered with `nvidia-uvm`.

### `obc-gpu::checkpoint`
- It loads the five symbols (`cuCheckpointProcessGetState`, `Lock`, `Checkpoint`, `Restore`, `Unlock`) from `libcuda.so.1` by itself. It does not use `driver()` and never calls `cuInit`, which would wake the GPU.
- `GetState` returning `CUDA_ERROR_NOT_INITIALIZED` means CUDA was never initialised in the process: nothing to park.
- A missing library or missing symbols (an older driver) means parking is unsupported. This is logged once and the daemon behaves as before.
- The lock has a timeout of 2000 ms (at least 1 ms). If it expires the process keeps running.
- If the checkpoint fails after a successful lock, the process is unlocked again. If that unlock also fails the error is `ParkError::Blocked`, and the next start unlocks first. Any other failure is `ParkError::Awake`: the process keeps running and parking is retried at the next idle.
- `unpark` reads the driver's process state, so a process checkpointed or locked from outside (for example by the `cuda-checkpoint` tool) is restored or unlocked too. In that case the parked flag is set before the restore or unlock runs, so if it fails, CUDA entry through `obc-gpu` is refused instead of blocking.
- The public API is `park()`, `unpark()`, `parked()`, the outcome types and `ParkError`.

### The daemon
- `backend/park.rs` holds a `Parker` that the real backend drives.
- A park failure keeps the GPU awake (the previous behaviour) and logs a warning.
- An unpark failure fails the session start with "the GPU could not be restored from its parked state: <err>; restart the daemon". The controller enters `error` and the loopback keeps sending its black placeholder frames (fail closed).
- A start that fails after a successful unpark parks again.
- **Taint.** If a teardown leaves a pipeline that did not reach NULL, something of the session may still be alive and call CUDA, which would block once the process is parked. **Taint disables parking until the daemon restarts**: the daemon logs one warning and the GPU stays awake, as before parking existed. It covers:
  - a session stop that did not reach NULL;
  - a restart whose stop of the previous session did not reach NULL;
  - a failed session start whose cleanup of the partly built output did not reach NULL (the backend starts the output itself and checks the result of its stop);
  - a camera switch whose old ingest, or whose failed new ingest, did not reach NULL.

  A later clean stop does not clear the taint, because it cannot prove that the earlier leftovers are gone. Unparking still works while tainted.
- **Stop errors are classified** by `obc_pipeline::StopError { error, reached_null }`, returned by the `*_checked` stop methods (the plain `stop()` methods are unchanged). Only a pipeline outside NULL taints. A stop that failed with every pipeline in NULL leaves nothing CUDA-related alive, so it is warned and the process is parked. Two such errors are known: a failed V4L2 control restore (an ioctl) and a panic of the output's clock thread after the pipeline reached NULL.
- Park and unpark are logged at info with their durations.
- No D-Bus property or status field was added: it would touch the backend, the controller, the IPC status, D-Bus and the CLI, which was judged too much for what it reports. [../dbus-api.md](../dbus-api.md) is unchanged.

### Paths that must make no CUDA call while parked
Each was checked:
- status and D-Bus queries, stats and effect reports at Idle (no session exists);
- NVML, started only after the unpark and stopped before the park;
- GStreamer plugin probing (nothing at Idle builds a CUDA element);
- the effect capability check, which the controller runs before the session starts, so while parked: it only `dlopen`s the SDK libraries and checks the model directory;
- the loopback writer and the PipeWire tap: forwarded buffers are system memory from a `GstVideoBufferPool` (observed with GStreamer 1.28.8), so releasing them needs no CUDA;
- process exit while parked, which is prompt.

## Consequences
- **Hazard: a CUDA call made while parked blocks until the next unpark** (one was measured waiting 2.3 s). While parked, `driver()` refuses with `GpuError::Parked` (mapped to `BackendError::Unavailable`) instead of blocking. Refusals are counted and the daemon logs a warning when the count grows. A future code path at Idle must not call CUDA, directly or through a library; the list above is the set that was checked.
- Idle after an activation now holds no GPU fds, mappings or UVM registration, as before the first activation.
- A restart from parked costs the unpark: about 16 to 19 ms added in-process (measurements below). From a suspended GPU the unpark should take about as long as a cold `cuInit` (about 2.6 s); this is expected, not measured.
- The daemon depends on a driver with the checkpoint API for the benefit; without it nothing changes.
- Tests: [../testing.md](../testing.md#parking-the-gpu-at-idle).

## Measurements
Reference machine, effect off.

| What | Result |
|---|---|
| Daemon at Idle (CUDA initialised, no context): park, unpark | 15 to 18 ms, 16 to 20 ms |
| `obc-gpu` test with a live context holding 2 MiB: park, unpark | 122 to 133 ms, 110 to 112 ms; data intact across cycles |
| Daemon, consumer open to first frame | 477 to 489 ms cold, 423 to 432 ms from parked |
| In-process, no camera, first frame | 193 ms cold start; 97 ms (median) restarting after a parked idle; 78 ms (median) restarting after an awake idle |
| Daemon SIGTERM while parked | exits 0 in 61 to 563 ms |

## Not observed: an actual runtime suspend
On the reference machine the dGPU drives the external displays, so it never suspends. The acceptance proxy is per process: 0 `/dev/nvidia*` fds and no mappings after a park, and the GPU missing from `/proc/driver/nvidia-uvm/gpus/`. The last check is system-wide, so it is meaningful only when no other process holds `/dev/nvidia-uvm`; the tests skip it otherwise.

**An actual runtime suspend needs the dGPU to drive no display and no other process to hold it.** To verify it, follow the quiet-desktop procedure in [../benchmarks/dgpu-idle.md](../benchmarks/dgpu-idle.md#procedure-verify-end-to-end-on-a-quiet-desktop): after the daemon idles, `scripts/dgpu-idle-check.sh --wait` should report `suspended`.

## Known limitations
Park runs on the controller thread. It normally takes tens of milliseconds. If a thread is still inside a CUDA call, the lock waits for it, which can block the controller, D-Bus included, for up to the 2000 ms lock timeout plus the checkpoint.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| A worker process that exits at teardown (the fallback of [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md)) | The checkpoint gives the same release with no IPC and no second process |
| Disabling UVM through a libcuda environment variable | There is none |
| `cuCtxDestroy`, a primary context reset, or the GStreamer CUDA elements at NULL | The fds drop from 21 to 7, but the GPU stays registered with `nvidia-uvm` |
| `dlclose(libcuda)` | The fds stay open |
| A D-Bus or status property for the parked state | Touches the backend, the controller, the IPC status, D-Bus and the CLI; not cheap |
