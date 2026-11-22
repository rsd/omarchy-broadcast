# Passthrough and roundtrip benchmark

**Tool:** `obc-bench` (`crates/obc-pipeline/src/bin/obc-bench.rs`), release build.
**Raw results:** one JSON per run, listed under [Results](#results).
**Other cameras and ingest paths:** see [ingest-paths.md](ingest-paths.md).

## Summary against targets

| Target | Result | Evidence |
|---|---|---|
| Sustained 1920x1080 @ 60 fps from the reference camera to the loopback, verified in a consumer, no drop over 10 minutes | **PASS**, with a caveat about the camera's dim-light frame rate (below) | `roundtrip-600s.json` and `passthrough-600s.json`: 60.00 fps at the sink and at the consumer over 600 one-second windows, with none below target. |
| Added latency ≤ 1 frame (16.7 ms) versus reading the camera directly; method recorded | **PASS** | Worst p99 across all runs is 6.9 ms (roundtrip) and worst max is 8.0 ms. Through the loopback into a consumer, the worst max is 9.0 ms. The method is described [below](#latency-method). |
| CPU % and GPU % recorded at steady state | **Recorded**, but GPU % is contaminated | CPU is 10 % of one core in passthrough and 22 % in roundtrip. Device-wide GPU % is dominated by another process; see the [GPU note](#gpu-note). |
| Frames in `memory:CUDAMemory` between upload and the sink, with no CPU round-trip mid-pipeline | **PASS** | `caps.cuda_memory_between_upload_and_download = true` in every run. Every link from `cudaupload` to `cudadownload` is checked programmatically. The dot graph is [roundtrip-pipeline.dot](roundtrip-pipeline.dot). |
| After teardown, the dGPU reaches `suspended` | **Not judged here** (see [dgpu-idle.md](dgpu-idle.md)), but see the [teardown finding](#teardown-finding) | GStreamer destroys its CUDA context at teardown. However, the process keeps 7 `/dev/nvidia*` fds until it exits. |

**Camera caveat.** The reference camera had `exposure_dynamic_framerate=1`, which is not the camera default (`0`). With it on, the camera lowers its own frame rate in dim light.
- Run `passthrough-30s-dynamic-framerate.json` shows this: only 23.4 fps arrived, with 1-second windows down to 15. All 1108 missing frames were missing at the camera's src pad (PTS gaps). The pipeline dropped none.
- Every other run passed `--camera-fixed-framerate`. That flag sets the control to 0 for the run and restores the original value afterwards. The restore runs on every exit path that unwinds: success, error, panic, and SIGINT/SIGTERM (Ctrl-C ends the run early, then teardown, restore and a partial report follow). Only SIGKILL or SIGQUIT skips it. The JSON records `changed_and_restored: Ok` and `v4l2-ctl` confirmed the value was 1 after each run.
- Whether the daemon should pin this control is a UX trade-off (a darker image in dim light against a steady 60 fps) and is still to be settled. The bench never changes the control unless that flag is passed.

## Hardware and software

| | |
|---|---|
| Machine | Intel Core Ultra 9 275HX (24 CPUs), kernel 7.2.15-1-omarchy |
| GPU | NVIDIA GeForce RTX 5080 Laptop GPU (PCI 0000:01:00.0, 16 GB), driver 615.78.08 |
| Camera | Logitech MX Brio (the reference test camera), USB 046d:0944, found by name, NV12 1920x1080 @ 60 |
| Loopback | v4l2loopback 0.15.4. A single device, `/dev/video10`, with `exclusive_caps=1` and `max_buffers=4` (see [../setup/loopback.md](../setup/loopback.md)) |
| GStreamer | 1.28.8 (`v4l2src`, `cudaupload`, `cudaconvert`, `cudadownload`, `v4l2sink`) |
| Rust | 1.99.1; gstreamer-rs 0.25.4 (feature `v1_28`), nvml-wrapper 0.13.0 |

## Pipeline

The pipeline is built element by element in `obc-pipeline` (`MediaPipeline::build`), not with `parse_launch`:

```text
v4l2src device=<found by name/USB id> io-mode=mmap
  ! capsfilter video/x-raw,format=NV12,width=1920,height=1080,framerate=60/1
  ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream
  ! cudaupload ! capsfilter video/x-raw(memory:CUDAMemory),format=NV12
  [roundtrip:  ! cudaconvert ! capsfilter video/x-raw(memory:CUDAMemory),format=BGR]
  [roundtrip, or output != NV12: ! cudaconvert ! capsfilter video/x-raw(memory:CUDAMemory),format=NV12|I420]
  ! cudadownload ! capsfilter video/x-raw,format=NV12|I420
  ! v4l2sink device=<loopback> sync=false enable-last-sample=false
```

- **`passthrough`:** the frame is uploaded and downloaded, with no conversion.
- **`roundtrip`:** NV12 → BGR → NV12 inside CUDA memory. This is the conversion cost an effect path pays, since effects consume BGR in CUDA memory (see [../architecture.md](../architecture.md)). It is the closer stand-in for the effect path, so the first 10-minute run used it.
- **The CUDAMemory capsfilter after `cudaupload` is load-bearing.** Without it, upload and download may negotiate system memory and pass frames through on the CPU.
- **`sync=false`:** pacing comes from the camera, and the sink presents each frame on arrival.
- **`enable-last-sample=false`:** the sink does not pin an extra buffer.
- **Output colorimetry is pinned to the camera's.** When a `cudaconvert` produces the output, the camera's negotiated `colorimetry` (`2:4:5:1` on the reference camera) is copied onto the output CUDA capsfilter, so the label round-trips unchanged. The recorded roundtrip runs predate this: their output was relabelled `2:4:7:1` (sRGB transfer). After the fix, a 20 s roundtrip check showed `2:4:5:1` on every link from `to_out` to `v4l2sink`, at 60.00 fps.
- **Low-latency knobs were not tuned.** Every run met both targets with the defaults above (queue 2, `io-mode=mmap`, `max_buffers=4`), so no tuning was tried.

**Device discovery.**
- **Camera:** GStreamer `DeviceMonitor` (`Video/Source`). Here it is answered by the PipeWire provider, which reports `api.v4l2.path`. Each node is then enriched from sysfs and `VIDIOC_QUERYCAP`: driver, capability bits and USB `idVendor`/`idProduct`. If the monitor lists nothing, the code falls back to a sysfs scan. The hardware test checks that both routes pick the same node.
- **Loopback sink:** always sysfs, using `/sys/class/video4linux/*/name` and the v4l2loopback attributes. The PipeWire provider does not list `Video/Sink` loopback nodes, and opening a loopback node to query it would count as a consumer.

## Commands

All runs were made from the repo root after `cargo build --release -p obc-pipeline`, and after checking with `fuser -v` that no other process held the camera or the loopback device:

```sh
B=./target/release/obc-bench
$B --sink-device /dev/video10 --gpu-stage passthrough --duration 60  --consumer --camera-fixed-framerate --dot-dir <tmp> --json docs/benchmarks/passthrough-60s.json
$B --sink-device /dev/video10 --gpu-stage roundtrip   --duration 60  --consumer --camera-fixed-framerate --dot-dir <tmp> --json docs/benchmarks/roundtrip-60s.json
$B --sink-device /dev/video10 --gpu-stage roundtrip   --duration 600 --consumer --camera-fixed-framerate --linger-after-teardown 5 --json docs/benchmarks/roundtrip-600s.json
$B --sink-device /dev/video10 --gpu-stage passthrough --duration 600 --consumer --camera-fixed-framerate --linger-after-teardown 5 --json docs/benchmarks/passthrough-600s.json
$B --sink-device /dev/video10 --gpu-stage passthrough --output-format i420 --duration 30 --consumer --camera-fixed-framerate --json docs/benchmarks/passthrough-i420-30s.json
$B --sink-device /dev/video10 --gpu-stage passthrough --duration 30  --consumer --json docs/benchmarks/passthrough-30s-dynamic-framerate.json
```

**Bench changes after these runs.** The recorded JSONs stay as the measurement record. Three things differ in later runs:
- `--dot-dir` now writes the graph before the measured window opens. In the two 60 s runs above, that one-off write fell at the start of the window.
- The per-second CPU series now leaves out a final segment shorter than 0.5 s. One tick in such a tail can read as 10 % of a core. The whole-window CPU figure was never affected.
- `latency.stamp_copies` (buffer-struct copies only) is replaced by `stamp_buffer_copies` and `stamp_memory_copies`. The second counts copies of the pixel data, detected by comparing the buffer's memory before and after the writable map.

The warm-up is 3 s (the default). It starts when the first frame reaches the sink and is excluded from every statistic. The GPU baseline is 3 NVML samples taken before the pipeline is built. Device-wide GPU figures in the "running" columns are 1 Hz NVML samples inside the measured window.

## Results

The latency columns are p50 / p95 / max in ms.
- "Added" is camera src pad → frame ready in system memory at `v4l2sink`.
- "Via consumer" is camera src pad → receipt in a separate pipeline reading the loopback.

| Run | Stage → output | Seconds | Sink fps (overall / min 1 s) | Consumer fps (overall / min 1 s) | Frames lost (camera / pipeline / queue) | Added latency | Via consumer | CPU (% of one core / % of all 24) | GPU util % device (baseline → run) | GPU mem MiB device (baseline → run) | Our GPU mem MiB | CUDAMemory links |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| [passthrough-60s](passthrough-60s.json) | passthrough → NV12 | 60 | 60.00 / 60 | 60.00 / 60 | 0 / 0 / 0 | 0.64 / 1.69 / 3.00 | 1.34 / 2.38 / 3.92 | 10.2 / 0.42 | 90 → 90 | 3774 → 3982 | 200 | true |
| [roundtrip-60s](roundtrip-60s.json) | roundtrip → NV12 | 60 | 60.00 / 60 | 60.00 / 60 | 0 / 0 / 0 | 2.84 / 4.98 / 7.47 | 3.57 / 5.78 / 8.78 | 22.5 / 0.94 | 90 → 91 | 3774 → 4002 | 220 | true |
| [roundtrip-600s](roundtrip-600s.json) | roundtrip → NV12 | **600** | 60.00 / 59 | 60.00 / 59 | 0 / 1\* / 0 | 2.73 / 4.90 / 7.96 | 3.44 / 5.73 / 9.01 | 21.8 / 0.91 | 85 → 91 | 3774 → 4002 | 220 | true |
| [passthrough-600s](passthrough-600s.json) | passthrough → NV12 | **600** | 60.00 / 59 | 60.00 / 59 | 0 / −1† / 0 | 0.64 / 1.69 / 3.87 | 1.33 / 2.36 / 7.37 | 9.7 / 0.40 | 85 → 73 | 3774 → 3982 | 200 | true |
| [passthrough-i420-30s](passthrough-i420-30s.json) | passthrough → I420 (one `cudaconvert`) | 30 | 60.00 / 60 | 60.00 / 60 | 0 / 0 / 0 | 1.69 / 3.30 / 6.05 | 2.42 / 4.05 / 6.89 | 16.2 / 0.68 | 92 → 90 | 3774 → 3988 | 206 | true |
| [passthrough-30s-dynamic-framerate](passthrough-30s-dynamic-framerate.json) | passthrough → NV12, camera control left at 1 | 30 | 23.40 / 15 | 23.40 / 15 | 1108 / 0 / 0 | 0.62 / 1.66 / 1.92 | 1.29 / 2.29 / 3.25 | 4.3 / 0.18 | 90 → 85 | 3774 → 3982 | 200 | true |

How to read the table:
- **"min 1 s" of 59 is a window-boundary effect.** In those runs the same number of complete windows also read 61, every window stayed within ±1 frame of 60, and `windows_below_target` = 0.
- **\* The 1-frame "pipeline" difference in `roundtrip-600s` is not a drop.** Its whole-run totals are `src_buffers` = `sink_buffers` = 36173. The difference is one frame in flight at a window edge.
- **That run also shows `criteria.no_pipeline_drops = false`. This is a false negative:**
  - v4l2loopback delivered one frame to the consumer twice (36174 consumer buffers against 36173 written).
  - The bench version used for that run counted the repeat as `consumer_unmatched`.
  - The recorder now counts re-delivered frames separately (`totals.consumer_duplicates`), and the 600 s passthrough run used the fixed version.
- **† The −1 in `passthrough-600s` is also a window-edge effect.** Its whole-run totals are `src_buffers` = `sink_buffers` = `consumer_buffers` = 36173, with 0 unmatched, 0 duplicate and 0 stalled frames. It is the cleanest 10-minute run.
- **Stalls** count inter-arrival gaps longer than 1.5 frames (25 ms).
  - `roundtrip-600s` had 0 at the camera, 5 at the sink and 8 at the consumer over 36,000 frames.
  - They come from camera arrival jitter compounding with GPU-side jitter while the GPU was shared at 85–100 %. No frame was lost.
- **Our process's GPU share:**
  - Device memory: 200 MiB in passthrough and 220 MiB in roundtrip (NVML compute-process list).
  - SM utilisation (`gpu.running.proc_sm_pct` in the JSON): mean 40 %, p95 88 %, over the 91 of 600 samples NVML reported for us in `roundtrip-600s`.
  - NVML reports per-process utilisation only for processes it saw as busy in its sample window, so that SM figure is biased high and should be treated as indicative only.
- **CPU** is the whole process: the capture, upload and download threads, plus the in-process consumer pipeline and the 1 Hz NVML sampler. The per-second p95 is 11 % in passthrough and 24–28 % in roundtrip.
- **`gst_latency_query`** = `live=true, min=16.67 ms, max=none` in every run. That is what the elements declare, not a measurement (see method item 3).

### GPU note

A GPU-heavy 3D workload (three processes, listed under `gpu.other_gpu_pids` in each JSON) was running concurrently on the dGPU for all runs, at 85–92 % device utilisation. Two consequences:
- **Device-wide utilisation and memory cannot be attributed to us.** The baseline → run deltas are noise, apart from memory: +208 MiB in passthrough and +228 MiB in roundtrip, which matches our process's own figure.
- **Our latency numbers come from a contended GPU, so they are a pessimistic bound.** The `cudaconvert` kernels queue behind the other workload's work. That is the likely reason the roundtrip sink-ready latency (p95 about 5 ms) is so much higher than its pad-arrival latency (p95 about 1.4 ms).

A re-run on an idle GPU would give the uncontended figures. The commands above are repeatable.

### Teardown finding

What happens at teardown:
- **GStreamer releases its CUDA context.** At NULL, `GST_DEBUG=cudacontext:6` logs `gst_cuda_context_finalize: Destroying CUDA context`.
- **NVML is shut down** (`gpu.nvml_shutdown: Ok`) before the pipeline is torn down.
- **The process still holds `/dev/nvidia*` handles.** Right after teardown, and still at the end of the 5 s linger, it held 7 of them: `/dev/nvidiactl`, `/dev/nvidia-uvm` ×2 and `/dev/nvidia0` ×4. `libcuda.so` also stays mapped.
- **This happens with `--no-gpu-stats` too, so the handles come from `libcuda`, not NVML.** The CUDA driver opens them at `cuInit` (the GStreamer nvcodec plugin calls it) and never closes them before process exit; there is no `cuDeinit`.
- **`runtime_status` read `active` during the linger.** That reading is not meaningful while another workload is keeping the dGPU awake.
- **[dgpu-idle.md](dgpu-idle.md) addresses** whether these handles alone block runtime D3 on driver 615.78. If they do, the rule "idle = zero CUDA contexts" needs a process boundary: the daemon would run the GPU pipeline in a child process that exits at teardown.
- **How to reproduce:** `--linger-after-teardown N` keeps the process alive after teardown, with no pipeline and no NVML. The JSON field `teardown.nvidia_fds_after_teardown` lists the handles.

## Latency method

Two in-process measurements on one `CLOCK_MONOTONIC` clock, plus the GStreamer latency query for reference.

1. **Primary: added latency.** A pad probe on the `v4l2src` src pad and another on the `v4l2sink` sink pad timestamp every buffer, keyed by buffer PTS. The PTS does not change through `queue`, `cudaupload`, `cudaconvert` or `cudadownload`.
   - Added latency = *sink-ready* time − src-pad time.
   - This is exactly the delay our stages add compared with an app that reads the camera directly. A direct reader receives the same buffer at the src-pad moment.
   - **Why "sink-ready" and not pad arrival:** `cudadownload` hands `v4l2sink` buffers that are still `GstCudaMemory`. They carry system-memory caps, but the data is device-resident (allocator `GstCudaPoolAllocator`, memory type `gst.cuda.memory`, recorded in the JSON as `caps.sink_buffer_memory`). CUDA work is asynchronous, so when a buffer arrives at the pad, the `cudaconvert` kernels and the device→host copy may not have run yet. They complete when something first maps the buffer.
   - The sink probe therefore maps the buffer readable and takes the timestamp after that map. This is the same sync and download that `v4l2sink`'s own map does next, so the probe adds no work.
   - The pad-arrival delta is still reported (`added_at_sink_pad_ms`), but it understates the cost. In roundtrip the mean is about 0.6 ms at pad arrival against about 2.8 ms sink-ready.
2. **Through the loopback (`--consumer`).** A bench-only probe writes the PTS into the first 16 luma bytes just before `v4l2sink`: magic `OBC1`, the PTS as u64, and a check word. An in-process consumer pipeline (`v4l2src device=<loopback> io-mode=mmap ! capsfilter ! fakesink sync=false`) decodes the stamp on arrival.
   - It measures camera src pad → consumer receipt, which covers `v4l2sink`'s copy into the loopback ring, `QBUF`/`DQBUF`, and the consumer's wake-up.
   - Its frame count is the consumer-side fps, the "verified in a consumer" figure.
   - The stamp write costs about 0.5 µs, because the buffer is already host-mapped and writable. The recorded runs show `stamp_copies: 0` (buffer-struct copies). The post-fix 20 s check also counted pixel-data copies (`stamp_memory_copies`): 0.
3. **GStreamer latency query.** It is sent upstream from the `v4l2sink` sink pad and gives `live=true, min=16.67 ms, max=none`.
   - This is what the elements *declare*: `v4l2src` declares one frame period, and the leaky queue leaves max unbounded. It is not a measurement.
   - Querying the pipeline itself is useless here: a `sync=false` sink answers `live=false, min=0`.

**Limits**
- **Sensor and USB time is excluded.** Sensor exposure and readout, USB transfer and USB Video Class (UVC) driver time (UVC is the standard protocol USB and built-in webcams use) come before the src pad. They are identical for a direct reader, so they cancel out of "added" latency.
- **No glass-to-glass.** The consumer app's own processing, rendering and display are excluded. That would need an on-screen clock and a camera pointed at it, which is out of scope here.
- **The stamp alters the picture in bench mode only.** It changes 16 luma pixels in the top-left corner, and only in `--consumer` runs.
- **The GPU is shared.** It was shared with another workload (see the GPU note), so the GPU-side numbers are pessimistic, not best case.

## Output format

- **Default: NV12.** The camera's native format passes through with no conversion in passthrough mode. In the effect path it is produced on the GPU by `cudaconvert`.
- **Optional: I420** (`--output-format i420`). Also produced on the GPU by `cudaconvert` inside CUDA memory.
- **YUYV deferred.** `cudaconvert` in GStreamer 1.28.8 cannot output YUY2 (its formats include NV12, I420, BGR, BGRx and BGRA, but not YUY2). YUYV would therefore need a CPU `videoconvert`, which would add a GPU-to-CPU round trip mid-pipeline, or a custom CUDA kernel. The kernel is future work (`obc-gpu`) if a consumer turns out to need YUYV.
- **Consumer compatibility is not verified yet.** Chrome, Zoom and OBS were not exercised in these runs. The in-process consumer reads NV12 and I420 from the loopback correctly.

## Loopback device

The runs above wrote into a v4l2loopback device at `/dev/video10`, selected with `--sink-device /dev/video10`, with `exclusive_caps=1` and `max_buffers=4`. The product selects its sink by the label "Omarchy Broadcast"; `obc-bench` does so by default (`--sink-label "Omarchy Broadcast"`). Creating and configuring the device, including the correct `modprobe` option syntax, is described in [../setup/loopback.md](../setup/loopback.md).
