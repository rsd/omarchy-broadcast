# Ingest / Output split results

Measurements of the three-stage stream described in [../architecture.md](../architecture.md#2-stage-and-caps-model) (Ingest per camera, a latest-frame slot, an Output with a fixed contract and its own clock), made with `obc-bench`. They answer four questions: does the output hold its frame rate, does switching cameras disturb the output or its consumer, is a source of another aspect ratio fitted correctly, and what does each ingest path cost now compared with the single pipeline of [ingest-paths.md](ingest-paths.md). Sections 1 to 4 measure the output clock, with ticks emitted on a fixed grid; sections 5 to 8 measure the phase-locked clock, the held frame at a switch and the bounded output queue.

## Method

- **Tool:** `obc-bench`, release build, passthrough Process stage, output contract NV12 1920x1080@30, limited range, crop-to-fill unless stated. Sink: a v4l2loopback device.
- **Warm-up:** the first 3 s after the first camera frame reaches the output are excluded.
- **Output frame rate:** every buffer arriving at the `v4l2sink` sink pad is timestamped on arrival. Two views are reported:
  - plain 1 s windows (count of arrivals per window);
  - **grid windows**: 1 s windows aligned to the output frame grid (the arrival phase is estimated, and each window is placed on the grid points it spans). A window passes only if it holds exactly the number of grid points it spans, which is exactly the contract rate for an integer rate. This is stricter than plain windows, which are sensitive to arrival jitter at the window edges.
- **Pass/fail criteria:** each report carries 12 booleans. `run_completed` (not interrupted, measured the requested time), `fps_sustained` (sections 1 to 4: grid windows exact, and the whole span within max(0.1 %, 1 frame) of the contract; from section 5 on, see "Output rate with a phase-locked clock" below), `no_output_drops` (queue drops and refused pushes are zero; with a consumer, consumer and sink frame counts within 3 and no unmatched stamps), `no_skipped_ticks`, `switches_completed`, `no_placeholders_outside_switches`, `no_ingest_errors` (no bus error or EOS on any ingest pipeline), `inputs_delivered` (every camera run in the window delivered frames), `no_ingest_drops`, `added_latency_p95_within_budget`, `added_latency_p99_within_budget` and `cuda_memory_links`. The latency budget is one output period plus one camera period of the slowest camera that streamed in the window.
- **Added latency:** the time from the camera `v4l2src` src pad to the buffer being ready at the `v4l2sink` sink pad, for fresh frames. See "Added latency is not comparable 1:1 with the single pipeline" below. The full method text is in the `latency.method` field of every JSON file and in [passthrough.md](passthrough.md#latency-method).
- **Consumer:** where stated, `--consumer` reads the loopback in-process. Measured runs also stamp 16 luma bytes before the sink to measure end-to-end latency (`via_consumer_ms`); the stamp is never applied in streaming mode.
- **CPU:** percent of one core, whole process. **GPU memory:** process GPU memory reported by NVML.
- **Frame counters** come from the output clock: *fresh* (a frame newer than the last emitted), *repeat* (the last frame again), *placeholder* (black frame after 500 ms without input), *superseded* (input frames replaced in the slot before the output used them: a 60 fps camera feeding a 30 fps contract supersedes about half its frames by design).

### Output rate with a phase-locked clock

With the phase lock each tick is emitted up to one period before its timestamp, at an offset that follows the camera and moves within that period (it jumps when the lock acquires a new camera and wraps when camera drift reaches the edge of the band). Grid windows place their boundaries on one mean arrival phase for the whole run, so a run whose phase moved can show windows "off the grid" although no frame was lost or added. From section 5 on, `fps_sustained` therefore checks what the contract promises, at the sink pad and, with `--consumer` in measured runs, at the consumer through the stamped PTS:

- **PTS sequence exact:** every tick index from the first to the last frame of the window arrives once, in order (`fps.output_pts`).
- **Count:** the frames in the window are within max(0.1 %, 1 frame) of the contract rate times the window.
- **Deviation from the grid bounded:** each frame's arrival minus its PTS grid instant (the clock's start plus the PTS) spans at most one output period plus a 5 ms allowance for hand-off jitter (`fps.output_deviation`). A span of at most one period bounds every wall-clock 1 s window to the rate plus or minus one frame; the allowance can let a window catch one more frame when a wrap coincides with jitter. The fewest and most frames actually seen in any 1 s window, at every position, are reported as `fps.output_sliding_1s`.

Plain 1 s windows and the grid windows are still reported, for comparison.

## Conditions

| | |
|---|---|
| Machine | Intel Core Ultra 9 275HX (24 CPUs), kernel 7.2.15-1-omarchy |
| GPU / driver | NVIDIA GeForce RTX 5080 Laptop GPU, driver 615.78.08 |
| GStreamer | 1.28.8 |
| Cameras | Logitech MX Brio (046d:0944); laptop-integrated "FHD Camera" (5986:1193) |
| Sink | v4l2loopback "Omarchy Broadcast", `exclusive_caps=1` |
| Concurrent load | Sections 1 to 4: a GPU workload belonging to another process ran during the runs, so device-level GPU utilisation is not attributable to the stream (see "GPU utilisation"). Sections 5 to 8: device GPU utilisation sampled before each run was 0 to 3 % |

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
- **Repeats outside the gaps** (771 repeats in total against 168 during gaps) come from the FHD Camera, which delivered about 20 fps instead of 30 in this run; its dynamic frame rate was not pinned, and a camera may lower its rate in dim light (see [ingest-paths.md](ingest-paths.md)). The output padded the missing frames with repeats and held 30 fps; no placeholder was sent while a camera was live. A camera that lowers its rate to exactly half the contract rate (15 fps into 30) is now followed by the phase lock once a full fit window of its frames is held; a steady 16 to 28 fps input, such as 20 fps into 30, is followed by cadence emission; other irregular rates keep the free-running clock.
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

In the single pipeline a frame left for the sink as soon as it was processed. Now a processed frame waits in the slot for the next output tick before it is emitted, and the added latency includes that wait: between 0 and one output period (0 to 33.3 ms at 30 fps), on average about half a period. The added latency figures above are therefore higher than the single-pipeline ones even where the ingest work is unchanged or cheaper. For a 60 fps camera on a 30 fps grid the phase between camera frames and output ticks is nearly constant for a whole run, so the tick wait is a nearly constant offset (about 11 ms in the GPU-direct run) rather than an average over the period, and two runs with the same ingest work can differ by several ms (the two 30 fps runs of section 3 differ by 8 ms). The bench does not separate ingest-only latency from the tick wait. This wait is the price of an output rate that does not follow the camera. The output keeps a fixed contract and its own clock, so consumers see a steady rate whatever the camera does.

### CPU and GPU memory

CPU falls on the 60 fps paths (9.4 % to 7.5 %, 24.5 % to 18.8 %), because the output side now runs at 30 fps and the slot discards the surplus frames. The MJPEG decode path still decodes every camera frame and costs about the same as before (18.0 % to 22.8 %, with the additional output work and a loaded machine). Process GPU memory is 8 MiB higher on every path than in the single pipeline.

### GPU utilisation

GPU utilisation is not reported per path. A GPU-heavy workload from another process ran throughout, and the device-level baseline sampled before each run (15 to 37 % across runs) varied by more than the stream's own contribution; the running means (15.5 to 16.5 %) cannot be separated from that baseline. Process GPU memory, which NVML reports per process, is attributable and is given above.

## 5. Phase-locked output clock: cost per ingest path

The runs of section 4 repeated with the phase-locked clock and the bounded output queue, default settings (margin 1.5 ms, window 8): 60 s, `--consumer`, contract NV12 1920x1080@30. Every camera ran with its frame rate pinned by `--camera-fixed-framerate` (`exposure_dynamic_framerate` off while streaming, so the camera keeps its nominal rate whatever the lighting); each camera's original value was restored after every run.

| Path | Camera / mode | Single pipeline: added p50 | Fixed grid (section 4): p50 / p95 / max | Phase lock: p50 / p95 / p99 / max | Slot wait p50 | Ingest (src pad to slot) p50 | CPU |
|---|---|---|---|---|---|---|---|
| GPU-direct | MX Brio NV12 1920x1080@60 | 0.68 ms | 11.75 / 13.39 / 14.86 ms | 3.93 / 6.37 / 6.60 / 21.31 ms | 2.95 ms | 0.28 ms | 5.4 % |
| CPU decode | FHD Camera MJPEG 1920x1080@30 | 4.78 ms | 23.38 / 36.68 / 40.72 ms | 6.81 / 8.58 / 8.83 / 9.32 ms | 2.91 ms | 3.18 ms | 13.4 % |
| CPU convert | MX Brio YUY2 1920x1080@60 | 3.28 ms | 13.20 / 15.08 / 16.27 ms | 5.95 / 8.10 / 8.74 / 22.73 ms | 3.01 ms | 1.98 ms | 16.1 % |

Every run delivered 1800 of 1800 frames with an exact PTS sequence, 0 skipped or late ticks, 0 queue drops, a queue level of at most 1, no lock acquisition inside the window (the lock acquired during warm-up) and at most one wrap, and met all 12 criteria. Every 1 s window held 29 to 31 frames. The CPU-decode run wrapped once inside the window, and its frames' arrival at the sink deviated from the grid over 32.6 ms, within one period (`fps_sustained` allows one period plus 5 ms, see "Output rate with a phase-locked clock"); on the 60 fps paths it spanned 3.7 ms. The maximum added latency on the 60 fps paths (21.3 and 22.7 ms) is the one frame of each run that missed its tick (below).

**What the median is made of.** Added latency = ingest (camera src pad to slot) + the wait in the slot + the output (tick to sink-ready). The wait is the part the clock controls. The lock places each tick 1.5 ms after the latest-arriving frame of the last eight, so the median frame waits that margin plus the distance between a typical frame's arrival and the late frames'. That distance is set by the camera: on all three paths the camera's own driver timestamps scatter by 2.0 to 2.5 ms either way around a regular cadence (1st to 99th percentile; `fps.inputs[].pts_jitter_ms`), and the arrivals at the src pad scatter the same way (`arrival_jitter_ms`). The median wait is therefore about 2.9 to 3.0 ms on every path, where the fixed grid waited up to a period. The rest of the difference to the single pipeline on the GPU-direct path is the work outside the wait (median added latency minus median wait: about 1.0 ms, against 0.68 ms for the whole single pipeline), which includes the hand-off between the two pipelines.

**Missed ticks.** A frame that arrives after its tick misses it. For a 60 fps camera the tick then emits the camera's previous frame, which has waited more than one camera frame interval in the slot; for a 30 fps camera the tick repeats the previous frame, and the late frame is emitted at the next tick or superseded by a newer one. The bench counts the fresh frames that waited longer than one camera frame interval (`clock.slot_wait_over_camera_frame`). That count covers a camera faster than the contract; at the contract's own rate a miss, like a frame the camera dropped, shows as a repeat (the late frame waits less than a frame interval for the next tick, or is superseded), so the count stays at zero there and the repeats and superseded frames bound the misses. The default margin is the smallest value that missed no frame in a simulation sweep (uniform jitter up to 2 ms); a camera's jitter can have a longer tail, so misses are rare, not excluded. In these runs: 1 of 1800 fresh frames on each 60 fps path, emitted 16.7 ms older; on the CPU-decode path no such wait, and 2 superseded frames with 3 repeats in 1800 ticks, that is at most two frames that missed their tick (the camera delivered 1799 frames in the window, one fewer than the ticks). Over the ten minutes of section 8 on the GPU-direct path, 4 of 18000 fresh frames waited over a camera frame interval.

**Before and after the lock engages.** The count is also reported split by the state of the phase lock at the tick that emitted the frame (`clock.slot_wait_over_camera_frame_locked` and `_unlocked`, with the fresh frames behind each part in `slot_wait_fresh_locked` and `_unlocked`; the two parts add up to `slot_wait_over_camera_frame`, which is unchanged). The split matters for a camera that runs at half the contract rate, such as one at 15 fps into 30: the lock follows it only after a full fit window of its frames, about 4 s after the start, while the measurement window opens 3 s after the first frame. The 15 to 18 frames of the window that were emitted before the lock engaged come from the free-running clock, whose wait depends on the phase it happened to start at, and they are counted when that wait exceeds the interval. Read the `_locked` part for the miss rate of the locked clock. For the reference camera lowered to 15 fps in its 60 fps mode (a 200 ms exposure, so the nominal interval is 16.7 ms), three 120 s runs traced frame by frame gave 0.17 to 0.34 % of the frames after the lock engaged missing their tick (3 to 6 of about 1780); the earlier whole-window counts of 3 to 30 in 1800, read as 0.2 to 1.7 %, were mostly frames before the lock. A 40 s run of the split bench in that setup counted 25 over the interval: 3 of 574 after the lock engaged and 22 of 26 before it. These are figures of the output clock, not of any processing stage. When the lock does not engage in the window (an irregular input, which neither the lock nor cadence mode follows, or `--no-phase-lock`) there is no locked part: the `_locked` count is 0 over 0 frames, the console line reads "lock not engaged", and the whole-window count is the one to read. The lock state is the one reported after the tick, so a frame at the tick that engages the lock may fall on either side. The interval remains the camera mode's nominal one: in a camera mode fixed at 15 fps it is 66.7 ms, a miss there waits about 33 ms, and the count cannot show it (the repeats and superseded frames bound the misses, as at the contract's own rate).

The same GPU-direct run with the lock switched off and with smaller margins, under the same conditions (60 s each):

| Setting | Added p50 / p95 / max | Slot wait p50 | Fresh frames that waited over a camera frame |
|---|---|---|---|
| Lock off (`--no-phase-lock`): every tick at its timestamp | 16.13 / 19.08 / 20.45 ms | 15.08 ms | 354 of 1800 |
| Margin 1.5 ms (default) | 3.93 / 6.37 / 21.31 ms | 2.95 ms | 1 of 1800 |
| Margin 1.0 ms (`--phase-margin-ms 1.0`) | 3.42 / 20.82 / 21.20 ms | 2.42 ms | 166 of 1799 |
| Margin 0.75 ms (`--phase-margin-ms 0.75`) | 3.24 / 20.54 / 21.15 ms | 2.24 ms | 360 of 1801 |

A smaller margin lowers the median by about the margin it saves and makes a tenth to a fifth of the frames miss their tick, each emitted 16.7 ms older. With the lock off, the tick's phase against this camera's frames is set by chance at start; here it fell about 15 ms after a frame, and a fifth of the ticks came just before the next frame.

Raw results:
[io-split-phase-lock-cost-gpu-direct-nv12-1080p60-60s.json](io-split-phase-lock-cost-gpu-direct-nv12-1080p60-60s.json),
[io-split-phase-lock-cost-cpu-decode-mjpeg-1080p30-60s.json](io-split-phase-lock-cost-cpu-decode-mjpeg-1080p30-60s.json),
[io-split-phase-lock-cost-cpu-convert-yuy2-1080p60-60s.json](io-split-phase-lock-cost-cpu-convert-yuy2-1080p60-60s.json),
[io-split-phase-lock-off-gpu-direct-nv12-1080p60-60s.json](io-split-phase-lock-off-gpu-direct-nv12-1080p60-60s.json),
[io-split-phase-lock-margin-1ms-gpu-direct-nv12-1080p60-60s.json](io-split-phase-lock-margin-1ms-gpu-direct-nv12-1080p60-60s.json),
[io-split-phase-lock-margin-0.75ms-gpu-direct-nv12-1080p60-60s.json](io-split-phase-lock-margin-0.75ms-gpu-direct-nv12-1080p60-60s.json).

## 6. Live switching with the held frame

Section 2 repeated with the held frame at a switch: MX Brio NV12 1920x1080@30 and FHD Camera MJPEG 1920x1080@30, `--switch-every 10`, 130 s measured, both cameras' frame rate pinned with `--camera-fixed-framerate`, and the same external consumer:

```sh
gst-launch-1.0 -v v4l2src device=/dev/video11 ! fpsdisplaysink video-sink=fakesink text-overlay=false sync=false
```

| Metric | Section 2 (fixed grid, no hold) | Held frame |
|---|---|---|
| Switches | 12 requested, 12 completed | 12 requested, 12 completed, 0 failed, 0 abandoned |
| Switch gap (request to the new camera's first frame) | min 653 / p50 937 / max 1053 ms | min 696 / p50 871 / max 1007 ms (696 to 754 ms to the FHD Camera, 988 to 1007 ms to the MX Brio) |
| Last frame held (the previous camera's last frame to the new camera's first) | not measured | min 700 / p50 883 / max 1033 ms |
| Black frames (placeholders) per switch | about 12 (143 in total) | 0 |
| Repeats during the switch gaps | 168 (then the placeholders) | 307, 20 to 30 per switch |
| Output | 3900 of 3900 frames | 3899 frames, PTS sequence exact, 29 to 31 frames in any 1 s window, 0 late, 0 skipped, 0 queue drops, queue level at most 1 |
| Holds | | 12 armed, 12 ended by the new camera's first frame, 0 timed out |
| Added latency p50 / p95 / max | 21.78 / 33.88 / 40.80 ms | 5.42 / 8.37 / 29.85 ms |
| External consumer | one caps negotiation, 0 dropped, 30.00 fps | one caps negotiation for the whole run (none at a switch), 0 errors or warnings, 3989 frames rendered, 0 dropped, 30.01 fps on average (28.3 to 31.9 over its 0.5 s intervals after the first) |
| Criteria | all 12 true | all 12 true |
| Camera controls | restored | both cameras' original `exposure_dynamic_framerate` restored |

During each switch the output repeats the previous camera's last frame past the 500 ms gap threshold until the new camera's first frame; no black frame was sent. The switch gap itself (stopping one camera, opening the other) is the cameras', unchanged by the hold. The lock acquired 18 times: once per switch, and once more on each of the six MX Brio runs, whose frames resumed after the gap of the dropped frames described below; a gap longer than 2.5 periods starts a new input run (1 wrap in the window).

Each MX Brio run after the first lost 8 frames by its own PTS sequence (`fps.inputs[].missing_frames_by_pts`), seven or eight of them within one second (a 1 s window of 22 or 23 frames); apart from that the camera delivered its nominal 30 fps, so its average over the 9 s of a run was 29.15 to 29.17 fps. The output padded the missing frames with repeats and held the contract rate. The window holds 3899 frames rather than 3900 because the emission phase at its end differs from the phase at its start (a count over a wall-clock span is within one frame of the rate), and 9 of the 129 grid windows are off the grid because that method places one phase on a run whose phase moved at every acquisition; the PTS sequence has no gap.

Raw result: [io-split-phase-lock-switching-mxbrio-fhd-130s.json](io-split-phase-lock-switching-mxbrio-fhd-130s.json). The consumer figures come from the external process and are not part of the JSON.

## 7. Output queue behind a slow consumer

The output's `appsrc` holds at most 3 frames and drops the oldest beyond that. Checked by two tests ([../testing.md](../testing.md)), each feeding a sink that takes 100 ms per frame at a 30 fps push rate:

| Test | Frames | Queue level max | Dropped (counted) | At the sink |
|---|---|---|---|---|
| `appsrc` alone, system memory | 60 pushed, 23 delivered | 3 | 37 | the three newest pushed are the last delivered (oldest dropped first); at most 2 newer frames queued behind a frame reaching the sink; oldest 236 ms after its push |
| Real output, CUDA frames and the download | 122 emitted, 40 at the sink | 3 | 79 | the clock skipped nothing and was never late; oldest 100 ms after its emission |

With the earlier queue of 31 frames the same sink would have received frames about a second old. In the runs of sections 5, 6 and 8, with a normal consumer, the queue never held more than one frame.

## 8. Output rate with the phase lock, 10 minutes

Section 1 repeated with the phase-locked clock: MX Brio NV12 1920x1080@60 into the NV12 1920x1080@30 contract, `--duration 600 --consumer`, frame rate pinned with `--camera-fixed-framerate` (restored after the run).

| Metric | Section 1 (fixed grid) | Phase lock |
|---|---|---|
| Measured / completed | 600.0 s, not interrupted | 600.0 s, not interrupted |
| PTS sequence at the sink and at the consumer | | exact: 18000 frames, every index once, in order, none off the grid |
| Frames in any 1 s window (sliding, every position) | | 29 to 31, at the sink and at the consumer |
| Deviation of the sink arrival from the PTS grid | | spread 17.95 ms (limit: one period plus 5 ms, 38.33 ms) |
| Output fps (sink pad) | 30.00; 600 of 600 plain 1 s windows at exactly 30 | 30.00; plain 1 s windows 29 to 31 |
| Grid windows (method of sections 1 to 4) | 599 of 599 exact; 18000 frames of 18000 expected | 599 of 599 exact, 0 off the grid; 18000 frames of 18000 expected |
| Consumer fps | 30.00; 18000 frames | 30.00; 18000 frames |
| Camera input | 35996 frames at 59.99 fps (2 missing by PTS) | 35997 frames at 60.00 fps (2 missing by PTS) |
| Clock | fresh 18000, repeat 0, placeholder 0, superseded 17996, late 0, skipped 0, queue dropped 0 | fresh 18000, repeat 0, placeholder 0, superseded 17997, late 0, skipped 0, queue dropped 0, queue level at most 1; 0 lock acquisitions and 1 wrap in the window |
| Slot wait p50 / max | | 2.94 / 20.13 ms; 4 of 18000 fresh frames waited longer than a camera frame interval |
| Added latency p50 / p95 / p99 / max | 9.93 / 17.98 / 20.71 / 22.02 ms | 3.95 / 6.40 / 6.64 / 21.21 ms |
| End-to-end via consumer p50 / p95 / max | 10.69 / 18.78 / 23.32 ms | 4.48 / 6.92 / 21.73 ms |
| CPU | 7.8 % of a core | 5.1 % of a core |
| Process GPU memory | 208 MiB | 208 MiB |
| Criteria | all 12 true | all 12 true (`fps_sustained` as defined for the phase-locked clock) |

The output held exactly 30.00 fps over the ten minutes: the PTS sequence has no gap, and the count in every wall-clock second stays within one frame of 30. The emission phase moved over 18 ms in the run (one wrap by one camera frame interval, 16.7 ms, to the camera's other frame train, plus jitter), less than a period, so the grid windows of sections 1 to 4 also stayed exact. The 4 frames that waited longer than a camera frame interval missed their tick and were emitted 16.7 ms older; the camera itself dropped 2 frames in the run, which count there when the dropped frame is the one a tick would have caught.

Raw result: [io-split-phase-lock-output-rate-mxbrio-nv12-1080p60-600s.json](io-split-phase-lock-output-rate-mxbrio-nv12-1080p60-600s.json).
