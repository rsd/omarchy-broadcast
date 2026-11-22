# dGPU idle check after teardown

Criterion: after teardown, the dGPU reaches `suspended`. Reframed as what can be measured: **the process holds no
CUDA context when it is idle**, verified from sysfs (`power/runtime_status`) and `/proc`. The daemon's design rule is that idle means zero CUDA contexts, so the NVIDIA GPU can suspend ([architecture.md](../architecture.md)).

## Update: parked at Idle

The findings below are those of a daemon that tears down in-process only. The daemon now also parks its process with the CUDA checkpoint API once a session has fully stopped.

- **What kept the GPU awake.** Besides the 7 fds, `cuInit` makes `nvidia-uvm` register the GPU, which takes a keep-awake reference that nothing in the process releases. Opening `/dev/nvidia0` alone does not block suspend under fine-grained RTD3 (`DynamicPowerManagement=3`); the UVM registration does. Destroying the context, resetting the primary context or taking the GStreamer CUDA elements to NULL brings the fds from 21 to 7 but leaves the GPU registered; `dlclose(libcuda)` leaves the fds open.
- **After a park:** the daemon holds 0 `/dev/nvidia*` fds and no `/dev/nvidia` mappings, and the GPU is missing from `/proc/driver/nvidia-uvm/gpus/` (system-wide: meaningful only when no other process holds `/dev/nvidia-uvm`). Park takes 15 to 18 ms at Idle and unpark 16 to 20 ms (effect off).
- **Still not observed: an actual runtime suspend.** On the reference machine the dGPU drives the external displays, so it never suspends. The per-process result above is the acceptance proxy. A real suspend needs the dGPU to drive no display and no other process to hold it; the procedure below checks it. The expected unpark from a suspended GPU, about 2.6 s like a cold `cuInit`, is not measured yet.

## Verdict (before parking): partial; the suspend itself could not be observed on the reference machine

- **CUDA context: none after teardown.** The positive control works: while streaming, `nvidia-smi` lists
  `obc-bench` as a compute (`C`) process using 200 MiB. 3 s and 57 s after in-process teardown it lists nothing for our
  pid, either as a compute or as a graphics process. GStreamer does release its CUDA context at NULL.
- **Driver handles: still held until the process exits.** After teardown the process keeps 7 fds open
  (`/dev/nvidiactl` x1, `/dev/nvidia-uvm` x2, `/dev/nvidia0` x4) and keeps `libcuda`, `libnvcuvid` and
  `libnvidia-encode` mapped. The count does not change over a 90 s linger. These are the CUDA driver's process-wide
  state from `cuInit`, plus the GStreamer `nvcodec` plugin, which is never unloaded. All of it is gone once the
  process exits.
- **Suspend: not verified.** The dGPU has not runtime-suspended once since boot (`runtime_suspended_time` = 0 ms over
  about 14 h of uptime). Two external monitors are lit on dGPU outputs, and a GPU-heavy 3D workload is rendering on it.
  Neither is related to this project, so whether the 7 leftover handles would block RTD3 on their own remains open.

## Tool

`scripts/dgpu-idle-check.sh` reads only sysfs and `/proc`. It never calls `nvidia-smi` or NVML, because both wake a
suspended dGPU. What it does:

- Finds the NVIDIA display-class PCI device (vendor `0x10de`, class `0x03xxxx`). A BDF passed as an argument or with
  `--device` overrides it.
- Prints `power/control`, `runtime_status`, and the `runtime_suspended_time` / `runtime_active_time` deltas.
- Prints the RTD3 mode from `/proc/driver/nvidia/gpus/<BDF>/power`, the driver's documented status file.
- Lists connected and enabled connectors on the dGPU's DRM card.
- Reports whether `nvidia-persistenced` and `nvidia-powerd` are running.
- Lists every process holding `/dev/nvidia*` fds. Each entry is flagged `libcuda`, `libnvidia-ml` or `uvm`. Processes
  whose `/proc/<pid>/fd` cannot be read are counted, and the NVIDIA-named ones are listed by name.

| Mode | Behaviour | Exit status |
|---|---|---|
| default | One-shot report; the time deltas are taken over `--sample` seconds (default 2) | 0 |
| `--pid PID` | Adds that pid's `/dev/nvidia*` fds and the NVIDIA libraries it maps | 3 if it holds `/dev/nvidia*` fds |
| `--wait SECS` | Polls `runtime_status` every `--interval` seconds (at least 1), bounded by SECS | 0 if it reaches `suspended`; 1 if not, with the pids holding the GPU |
| usage error, or no device | | 2 |

`--wait` takes precedence over `--pid` in the exit status.

## Machine state during the measurement

- dGPU `0000:01:00.0` (`10de:2c19`, RTX 5080 Laptop), open kernel module 615.78.08, `power/control=auto`.
- `/proc/driver/nvidia/params`: `DynamicPowerManagement: 3`, `DynamicPowerManagementVideoMemoryThreshold: 200`.
  The procfs power file shows `Runtime D3 status: Enabled (fine-grained)` and `Video Memory: Active`.
- DRM: `card0` is the iGPU (`0000:00:02.0`) and drives the internal panel `eDP-2`. `card1` is the dGPU and owns
  `DP-1`, `DP-2`, `HDMI-A-1` and `eDP-1`. **`DP-2` and `HDMI-A-1` are connected and enabled**, which means every
  external port on this laptop is wired to the dGPU.
- `nvidia-persistenced` is not running. `nvidia-powerd` (root, pid 1163) is running; its fds cannot be read without
  root.
- Other GPU clients: the Hyprland compositor (18 fds, maps `libnvidia-gpucomp`), plus a GPU-heavy 3D workload
  (3569 MiB, `C+G`) with its two helper processes.

## (a) Baseline: no obc-bench running

`scripts/dgpu-idle-check.sh --wait 30`, exit 1:

```
runtime_status: active
dGPU outputs connected+enabled: card1-DP-2 card1-HDMI-A-1  (a lit output keeps the dGPU awake)
nvidia-persistenced: not running
nvidia-powerd: running
runtime_status still 'active' after 30s
over 30s: runtime_suspended_time +0 ms, runtime_active_time +30068 ms
since boot: runtime_suspended_time 0 ms, runtime_active_time 50785327 ms
    1591  Hyprland         18 fds: /dev/nvidiactl=4 /dev/nvidia-modeset=2 /dev/nvidia0=12
 (three further processes of the 3D workload: 18, 12 and 46 fds)
Unreadable /proc/<pid>/fd (other users' userspace processes, needs root): 63
  NVIDIA-named among them (fds not inspectable): 1163:nvidia-powerd
```

Why the dGPU does not suspend, in order of certainty:

1. Two lit display outputs are on the dGPU. As long as it scans out to a monitor, it cannot enter D3.
2. The 3D workload is actively rendering on the dGPU (3.5 GiB of VRAM, far above the 200 MiB threshold).
3. Unknown: whether Hyprland's open `card1` handles, or `nvidia-powerd`, would also block suspend on an otherwise
   quiet desktop.

A `runtime_active_time` equal to the uptime shows the dGPU has been awake since boot. **The environment cannot show a
suspend at all right now**, whatever our process does.

## (b) During an obc-bench linger after teardown

The run:

```sh
./target/release/obc-bench --sink-device /dev/video10 --gpu-stage passthrough --duration 10 \
  --linger-after-teardown 90
```

It was run twice, once with `--no-gpu-stats` and once with NVML sampling on; both gave
the same result. The camera and sink were checked free with `fuser` first.

`nvidia-smi` was run during the linger because the dGPU was already awake for the other clients (section a), so the
call could not disturb the measurement. It is not part of the check procedure.

| When | Our pid in `nvidia-smi` | `/dev/nvidia*` fds (sysfs/proc) | Maps | `runtime_status` |
|---|---|---|---|---|
| Streaming (control) | **yes**, type `C`, 200 MiB | 26: nvidia0 x21, nvidiactl x2, nvidia-uvm x2, nvidia-caps/nvidia-cap2 x1 | libcuda, libnvidia-ml | active |
| Linger, T+3 s | **no** (no `C`, no `G` row) | 7: nvidia0 x4, nvidiactl x1, nvidia-uvm x2 | libcuda, libnvcuvid, libnvidia-encode (not libnvidia-ml) | active |
| Linger, T+35..55 s (`--wait 20`) | — | 7, unchanged | same | active the whole 20 s; suspended_time +0 ms |
| Linger, T+57 s | **no** | — | — | — |
| Linger end, T+90 s | — | 7, unchanged (the bench's own end-of-linger read) | — | active |

Notes:

- NVML releases cleanly. With sampling on, `libnvidia-ml` is unmapped and `nvidia-cap2` is closed after teardown.
  The 7 leftover fds are the same with and without NVML.
- The process at linger: 5 threads, RSS about 122 MiB.
- The `--wait 20` during the linger listed Hyprland, the 3D workload's processes and our pid as holders,
  and the two lit outputs. Because of the outputs and the 3D workload, this run cannot tell whether our pid on its own
  would have blocked the suspend.

## (c) After our process exited

Measured about 2 s after exit (exit 1 because of the other clients) with:

```sh
scripts/dgpu-idle-check.sh --pid <bench pid> --wait 20
```

- Our pid is gone; the holders are back to the baseline set (Hyprland and the 3D workload's processes).
- `nvidia-smi --query-compute-apps` lists only the 3D workload.
- `runtime_status` is still `active`, for the reasons in (a).

## Conclusion against the criterion

| Property | Result | Evidence |
|---|---|---|
| No CUDA context after in-process teardown | **Yes** | Absent from `nvidia-smi` at T+3 s and T+57 s; present as `C`/200 MiB while streaming |
| No driver handles after in-process teardown | **No** | 7 `/dev/nvidia*` fds and `libcuda` stay until exit. The CUDA driver API has no call that undoes `cuInit` |
| No handles after process exit | **Yes** | Our pid holds nothing; holders are back to baseline |
| dGPU reaches `suspended` | **Not verifiable here** | Lit dGPU outputs and a running 3D workload; `runtime_suspended_time` = 0 since boot |

## Unresolved question (answered by parking)

The 7 fds and the UVM registration are now released by parking the process, so the worker-process branch below is not needed. What remains open is observing the suspend itself.

The design needs the dGPU to suspend while the daemon idles in a live process. The evidence above leaves one question
open: **in fine-grained RTD3 mode, do the 7 `cuInit` handles that survive in-process teardown (no context, no
memory) block runtime suspend?**

- **If they do not block it:** a single-process daemon that builds and tears down pipelines in-process meets
  the design as intended.
- **If they do block it:** the CUDA/GStreamer work must live in a process that exits on teardown. Candidates are a
  separate worker process, or a daemon that re-execs. In that case the daemon itself must never load the `nvcodec`
  plugin or `libcuda`.

The procedure below settles this in one run on a quiet desktop. It should be run before the daemon's process model is chosen.

## Procedure: verify end to end on a quiet desktop

Preconditions:

- No game or other GPU-heavy application running.
- No external monitor on a dGPU port. On this laptop every external port is on `card1` (the dGPU), so this means
  undocked, internal panel only.
- No other GPU apps running.
- Do not run `nvidia-smi` or anything that uses NVML at any point: it wakes the dGPU.

```bash
cd <repo root>

# 1. Baseline. It must reach "suspended", otherwise this desktop cannot verify anything:
#    read the "Not suspended. Likely reasons" block, fix that, and repeat.
scripts/dgpu-idle-check.sh --wait 60; echo "baseline exit=$?"

# 2. Run a pipeline, then linger after teardown. Check the camera and sink are free first.
fuser -v /dev/video4 /dev/video10   # camera and loopback nodes; adjust to your system
./target/release/obc-bench --sink-device /dev/video10 --gpu-stage passthrough \
  --duration 10 --no-gpu-stats --linger-after-teardown 180 &
BENCH=$!
# The dGPU wakes for the run. Wait for the "lingering 180 s after teardown" line (about 15-20 s), then:
scripts/dgpu-idle-check.sh --pid "$BENCH" --wait 120; echo "linger exit=$?"
#    exit 0 -> the leftover cuInit handles do NOT block RTD3 (an in-process daemon is fine)
#    exit 1 with only $BENCH as holder -> they DO block it (the daemon needs exit/re-exec or a worker process)
#    exit 1 with other holders -> the desktop is not quiet; go back to step 1

# 3. After exit.
wait "$BENCH"
scripts/dgpu-idle-check.sh --wait 60; echo "after-exit exit=$?"
```

Expected on a quiet desktop: step 1 gives exit 0, and step 3 gives exit 0. Step 2's result answers the unresolved question above.

### With the daemon, to observe a real suspend

Same preconditions (no dGPU display, no other GPU client, no `nvidia-smi`). Start the daemon, open the virtual camera once with any application and close it, then wait for the idle grace period and the park (the daemon logs the park and its duration at info):

```bash
scripts/dgpu-idle-check.sh --pid "$(pidof -s omarchy-broadcastd)" --wait 120; echo "daemon exit=$?"
#    exit 0 -> the dGPU reached "suspended" with the daemon idle after an activation
#    exit 1 with the daemon as a holder -> the park did not happen; read the daemon log
#    exit 1 with other holders -> the desktop is not quiet; go back to step 1
```

The next activation should take about 2.6 s longer to produce its first frame than from a parked, awake GPU (expected, not measured).

## Not verified

- **Runtime suspend:** never observed on the reference machine during these measurements, for the environmental reasons in (a).
- **Hyprland, `nvidia-powerd` and the internal panel:** not shown to allow suspend. Hyprland may keep `card1` open even
  with no dGPU monitor (not checked), and `nvidia-powerd`'s fds need root to inspect. Step 1 of the procedure answers both.
- **Context detection:** "no CUDA context" rests on `nvidia-smi`/NVML per-process accounting. No `/proc`-only signal
  tells a context apart from bare `cuInit` handles; the fd count dropping from 26 to 7 and the 200 MiB disappearing
  are consistent with the context being gone.
- **The camera's frame rate:** with dynamic exposure framerate on, the reference camera ran at about 15 fps in dim light
  during the linger runs. This does not affect the idle check.
