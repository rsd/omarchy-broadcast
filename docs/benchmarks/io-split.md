# Ingest / Output split results

Measurements of the three-stage stream of [ADR-0009](../adr/0009-ingest-process-output.md) (Ingest per camera, a latest-frame slot, an Output with a fixed contract and its own clock), made with `obc-bench`. They answer four questions: does the output hold its frame rate, does switching cameras disturb the output or its consumer, is a source of another aspect ratio fitted correctly, and what does each ingest path cost now compared with the single pipeline of [ingest-paths.md](ingest-paths.md).

## Method

- **Tool:** `obc-bench`, release build, passthrough Process stage, output contract NV12 1920x1080@30, limited range, crop-to-fill unless stated. Sink: a v4l2loopback device.
- **Warm-up:** the first 3 s after the first camera frame reaches the output are excluded.
- **Output frame rate:** every buffer arriving at the `v4l2sink` sink pad is timestamped on arrival. Two views are reported:
  - plain 1 s windows (count of arrivals per window);
  - **grid windows**: 1 s windows aligned to the output frame grid (the arrival phase is estimated, and each window is placed on the grid points it spans). A window passes only if it holds exactly the number of grid points it spans, which is exactly the contract rate for an integer rate. This is stricter than plain windows, which are sensitive to arrival jitter at the window edges.
- **Pass/fail criteria:** each report carries 12 booleans. `run_completed` (not interrupted, measured the requested time), `fps_sustained` (grid windows exact, and the whole span within max(0.1 %, 1 frame) of the contract), `no_output_drops` (queue drops and refused pushes are zero; with a consumer, consumer and sink frame counts within 3 and no unmatched stamps), `no_skipped_ticks`, `switches_completed`, `no_placeholders_outside_switches`, `no_ingest_errors` (no bus error or EOS on any ingest pipeline), `inputs_delivered` (every camera run in the window delivered frames), `no_ingest_drops`, `added_latency_p95_within_budget`, `added_latency_p99_within_budget` and `cuda_memory_links`. The latency budget is one output period plus one camera period of the slowest camera that streamed in the window.
- **Added latency:** the time from the camera `v4l2src` src pad to the buffer being ready at the `v4l2sink` sink pad, for fresh frames. See "Added latency is not comparable 1:1 with the single pipeline" below. The full method text is in the `latency.method` field of every JSON file and in [passthrough.md](passthrough.md#latency-method).
- **Consumer:** where stated, `--consumer` reads the loopback in-process. Measured runs also stamp 16 luma bytes before the sink to measure end-to-end latency (`via_consumer_ms`); the stamp is never applied in streaming mode.
- **CPU:** percent of one core, whole process. **GPU memory:** process GPU memory reported by NVML.
- **Frame counters** come from the output clock: *fresh* (a frame newer than the last emitted), *repeat* (the last frame again), *placeholder* (black frame after 500 ms without input), *superseded* (input frames replaced in the slot before the output used them: a 60 fps camera feeding a 30 fps contract supersedes about half its frames by design).

## Conditions

| | |
|---|---|
| Machine | Intel Core Ultra 9 275HX (24 CPUs), kernel 7.2.15-1-omarchy |
| GPU / driver | NVIDIA GeForce RTX 5080 Laptop GPU, driver 615.78.08 |
| GStreamer | 1.28.8 |
| Cameras | Logitech MX Brio (046d:0944); laptop-integrated "FHD Camera" (5986:1193) |
| Sink | v4l2loopback "Omarchy Broadcast", `exclusive_caps=1` |
| Concurrent load | A GPU workload belonging to another process ran during the runs. Device-level GPU utilisation is therefore not attributable to the stream (see "GPU utilisation") |

## 1. Output rate, 10 minutes

MX Brio NV12 1920x1080@60 into the NV12 1920x1080@30 contract, `--duration 600 --consumer`.

| Metric | Result |
|---|---|
| Measured / completed | 600.0 s, not interrupted |
| Output fps (sink pad) | 30.00; 600 of 600 plain 1 s windows at exactly 30 |
| Grid windows | 599 of 599 exact, 0 off-grid; 18000 frames of 18000 expected |
| Consumer fps | 30.00; 18000 frames |
| Camera input | 35996 frames at 59.99 fps (2 missing by PTS) |
| Clock | fresh 18000, repeat 0, placeholder 0, superseded 17996, late 0, skipped 0, queue dropped 0, push failed 0 |
| Added latency p50 / p95 / p99 / max | 9.93 / 17.98 / 20.71 / 22.02 ms |
| End-to-end via consumer p50 / p95 / max | 10.69 / 18.78 / 23.32 ms |
| CPU | 7.8 % of a core (p95 per second 9.0 %) |
| Process GPU memory | 208 MiB |
| Criteria | all 12 true |

The output held its contract for the whole run without a single repeat, placeholder or late tick. The 17996 superseded frames are the 60 fps camera thinned to 30 fps by the slot.

Raw result: [io-split-output-rate-mxbrio-nv12-1080p60-600s.json](io-split-output-rate-mxbrio-nv12-1080p60-600s.json).

## 2. Live switching, 2 minutes

MX Brio NV12 1920x1080@30 (GPU-direct) and FHD Camera MJPEG 1920x1080@30 (CPU decode), `--camera` twice, `--switch-every 10`, 130 s measured. An **external** consumer, a separate process running `v4l2src ! fpsdisplaysink` on the loopback, read the device throughout, as an application would. `--camera-fixed-framerate` was not used.

| Metric | Result |
|---|---|
| Output fps | 30.00; 130 of 130 plain windows and 130 of 130 grid windows exact; 3900 frames of 3900 expected |
| Switches | 12 requested, 12 completed, 0 failed, 0 abandoned |
| Switch gap (request to first fresh frame of the new camera) | min 653 / p50 937 / mean 901 / max 1053 ms |
| During the 12 gaps | 168 repeats, then 143 placeholders |
| Clock overall | fresh 2986, repeat 771, placeholder 143, late 0, skipped 0, queue dropped 0, push failed 0 |
| Placeholders outside switch gaps | 0 |
| Added latency p50 / p95 / max | 21.78 / 33.88 / 40.80 ms (budget 66.67 ms) |
| CPU | 8.8 % of a core |
| External consumer | exactly one caps negotiation for the whole run (none at any switch), 0 errors or warnings, 3913 frames rendered, 0 dropped, 30.00 fps in every 5 s interval |
| Camera controls | both cameras' original `exposure_dynamic_framerate` restored |
| Criteria | all 12 true |

Reading the numbers:

- **The consumer never saw a format change.** The output pipeline is not touched by a switch, so its caps are negotiated once at start.
- **The switch gap is 0.65 to 1.05 s.** It is the time to stop the previous camera (the device is released and its controls restored), open the next one and receive its first frame. During it the output repeats the last frame; once 500 ms pass without input it sends black placeholders, so a switch shows a short black interval before the new camera appears. The bench records the total only, not its parts.
- **Repeats outside the gaps** (771 repeats in total against 168 during gaps) come from the FHD Camera, which delivered about 20 fps instead of 30 in this run; its dynamic frame rate was not pinned, and a camera may lower its rate in dim light (see [ingest-paths.md](ingest-paths.md)). The output padded the missing frames with repeats and held 30 fps; no placeholder was sent while a camera was live.
- The first camera's start (1030 ms from the start request to its first frame) is not a switch and is recorded separately as `initial_start_ms`.

Raw result: [io-split-switching-mxbrio-fhd-130s.json](io-split-switching-mxbrio-fhd-130s.json). The consumer figures come from the external process and are not part of the JSON.

## 3. Aspect ratio: crop versus letterbox

MX Brio NV12 640x480@30 (4:3) into the 1920x1080 contract, 10 s each. A frame was read from the loopback by an external reader and the non-black extent measured.

| Policy | Content columns | Content width | Result |
|---|---|---|---|
| `--aspect crop` | x = 0 to 1919 | 1920 px (full width) | 4:3 source centre-cropped to 16:9, no bars, not stretched |
| `--aspect letterbox` | x = 240 to 1679 | 1440 px = 1080 x 4/3 | full 4:3 image, black pillars of 240 px on each side |

Both runs delivered 300 of 300 frames at 30.00 fps with all 12 criteria true. Added latency was 18.39 ms p50 for crop and 10.22 ms p50 for letterbox; the difference is attributed to the phase of the 30 fps camera against the 30 fps output grid (see below), which stays nearly constant within a run, and not to the aspect policy: both go through one `cudaconvertscale` pass. The geometry (even, centred crop rectangles, including side crops and letterboxing a wider source) is also covered by the GPU-only hardware test `gpu_crop_fills_and_letterbox_pads` ([../testing.md](../testing.md)).

Raw results:
[io-split-aspect-crop-mxbrio-nv12-640x480.json](io-split-aspect-crop-mxbrio-nv12-640x480.json),
[io-split-aspect-letterbox-mxbrio-nv12-640x480.json](io-split-aspect-letterbox-mxbrio-nv12-640x480.json).

## 4. Cost per ingest path, against the single pipeline

60 s per run, `--consumer`, contract NV12 1920x1080@30, camera modes as in [ingest-paths.md](ingest-paths.md). "Single pipeline" is the earlier design measured there (one pipeline, output at the camera's rate).

| Path | Camera / mode | Output fps | Single pipeline: added p50, CPU | Split: added p50 / p95 / max | Split: CPU | Process GPU memory (single) |
|---|---|---|---|---|---|---|
| GPU-direct | MX Brio NV12 1920x1080@60 | 30.00 | 0.68 ms, 9.4 % | 11.75 / 13.39 / 14.86 ms | 7.5 % | 208 MiB (200) |
| CPU decode | FHD Camera MJPEG 1920x1080@30, fixed frame rate | 30.00 | 4.78 ms, 18.0 % | 23.38 / 36.68 / 40.72 ms | 22.8 % | 214 MiB (206) |
| CPU convert | MX Brio YUY2 1920x1080@60 | 30.00 | 3.28 ms, 24.5 % | 13.20 / 15.08 / 16.27 ms | 18.8 % | 208 MiB (200) |

Every run delivered 1800 of 1800 frames at the contract rate with all 12 criteria true, 0 queue drops and 0 refused pushes. The 60 fps runs superseded 1800 frames each (the camera thinned to the contract). The MJPEG run, with a 30 fps camera on a 30 fps grid, repeated 33 ticks and superseded 32 frames, because the camera and output clocks are independent, so an occasional tick sees no new frame and an occasional frame is replaced before a tick.

Raw results:
[io-split-cost-gpu-direct-nv12-1080p60-60s.json](io-split-cost-gpu-direct-nv12-1080p60-60s.json),
[io-split-cost-cpu-decode-mjpeg-1080p30-60s.json](io-split-cost-cpu-decode-mjpeg-1080p30-60s.json),
[io-split-cost-cpu-convert-yuy2-1080p60-60s.json](io-split-cost-cpu-convert-yuy2-1080p60-60s.json).

### Added latency is not comparable 1:1 with the single pipeline

In the single pipeline a frame left for the sink as soon as it was processed. Now a processed frame waits in the slot for the next output tick before it is emitted, and the added latency includes that wait: between 0 and one output period (0 to 33.3 ms at 30 fps), on average about half a period. The added latency figures above are therefore higher than the single-pipeline ones even where the ingest work is unchanged or cheaper. For a 60 fps camera on a 30 fps grid the phase between camera frames and output ticks is nearly constant for a whole run, so the tick wait is a nearly constant offset (about 11 ms in the GPU-direct run) rather than an average over the period, and two runs with the same ingest work can differ by several ms (the two 30 fps runs of section 3 differ by 8 ms). The bench does not separate ingest-only latency from the tick wait. This wait is the price of an output rate that does not follow the camera; the trade-off is recorded in [ADR-0009](../adr/0009-ingest-process-output.md).

### CPU and GPU memory

CPU falls on the 60 fps paths (9.4 % to 7.5 %, 24.5 % to 18.8 %), because the output side now runs at 30 fps and the slot discards the surplus frames. The MJPEG decode path still decodes every camera frame and costs about the same as before (18.0 % to 22.8 %, with the additional output work and a loaded machine). Process GPU memory is 8 MiB higher on every path than in the single pipeline.

### GPU utilisation

GPU utilisation is not reported per path. A GPU-heavy workload from another process ran throughout, and the device-level baseline sampled before each run (15 to 37 % across runs) varied by more than the stream's own contribution; the running means (15.5 to 16.5 %) cannot be separated from that baseline. Process GPU memory, which NVML reports per process, is attributable and is given above.
