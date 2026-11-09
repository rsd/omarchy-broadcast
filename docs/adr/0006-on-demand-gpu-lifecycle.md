# ADR-0006: On-demand lifecycle; idle means zero CUDA contexts

- **Status:** Accepted. Amended by [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md): the states are `off`, `idle`, `starting`, `active` and `error` (the grace period is inside `active`, replacing "Draining"), and a CPU-only loopback writer runs while the camera and the CUDA context are released. Amended by [ADR-0017](0017-cuda-checkpoint-parking.md): after an activation, Idle also parks the process with the CUDA checkpoint API, which releases the driver handles and the `nvidia-uvm` registration

## Context
- The target is a hybrid laptop.
- The NVIDIA dGPU can only runtime-suspend when no process holds a CUDA context. Pausing frames does not release the GPU.

## Decision
- Lifecycle states: Idle, Starting, Streaming, Draining, then back to Idle.
- **Idle:** no CUDA context and the camera is closed. Only the D-Bus service and the consumer watcher run.
- **Start:** when a consumer opens the virtual camera, or on ForceOn.
- **Teardown:** after the last consumer closes and a grace period passes (about 5 s, configurable). Teardown releases every GPU resource.

## Consequences
- Start-up latency is paid on each first open: CUDA init, backend load, pipeline build. It must be measured and kept low, for example by keeping model files on disk and rebuilding the cache once.
- Consumer detection on v4l2loopback (open count, events or inotify) is still to be chosen.
- Acceptance check: `/sys/bus/pci/devices/<dGPU>/power/runtime_status` shows `suspended` while the daemon is idle. `scripts/dgpu-idle-check.sh` performs it.
- **Measured caveat:** the CUDA context is released when the pipeline goes to NULL, but libcuda driver handles (7 `/dev/nvidia*` fds) persist until process exit, and `cuInit` leaves the GPU registered with `nvidia-uvm`, which holds a keep-awake reference. [ADR-0017](0017-cuda-checkpoint-parking.md) releases both by parking the process at Idle, instead of the worker process first considered. See [../benchmarks/dgpu-idle.md](../benchmarks/dgpu-idle.md).

## Alternatives considered
- Keeping the pipeline paused while idle: pausing frames does not release the GPU, so the dGPU never suspends.
- A separate worker process per stream: first kept as a fallback, replaced by the checkpoint of [ADR-0017](0017-cuda-checkpoint-parking.md), which needs no second process.
