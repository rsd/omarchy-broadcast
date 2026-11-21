# System-memory pipeline results (CPU, no NVIDIA GPU)

Cost of the video pipeline when the frames stay in system memory ([../architecture.md](../architecture.md#2-stage-and-caps-model)), with the open-model cut-out blurring the background on the CPU. These are figures for one high-end laptop CPU; they say how much headroom this machine has, not what a small machine will do.

## Method

- **The daemon:** the release build of `omarchy-broadcastd` with `CUDA_VISIBLE_DEVICES=""` in its environment, so that the probe chose system memory (`video memory: system (auto: CUDA_VISIBLE_DEVICES hides every GPU)`). In these runs its process mapped no CUDA library and held no `/dev/nvidia*` descriptor.
- **The consumer:** one `gst-launch-1.0 v4l2src device=<virtual camera> ! fpsdisplaysink video-sink=fakesink` reading the virtual camera, per run. The rate is what it received; dropped frames are its own count.
- **CPU:** the daemon process's user plus system time from `/proc/<pid>/stat`, as a percentage of one core, after a 10 s warm-up. It covers the camera capture, the ingest, the effect, the output and the writer, and the module's own threads.
- **Added latency and the effect's time:** the `Stats` signal's `latency_p50_ms` is camera to the output's `appsrc` and **excludes the effect** ([../dbus-api.md](../dbus-api.md)). The effect's own time comes from GStreamer's `latency` tracer on the effect element, over the frames it processed (a repeat is a cached re-send and costs about nothing).
- **Per-step figures** (the second and third tables) were measured apart from the daemon: `gst-launch-1.0` chains over a recorded MJPEG clip, 1200 frames, as fast as the chain runs (not live), and the module opened through `obc-module-host` exactly as the daemon opens it, 300 frames. *Wall* is the per-frame latency on one streaming thread; *CPU* is the process's user plus system time per frame, which includes the ONNX Runtime threads and so can exceed the wall time. Other processes were running on the machine (load average 4 to 12 on 24 threads); the figures were stable across repeats.

## Conditions

| | |
|---|---|
| Machine | Intel Core Ultra 9 275HX (24 CPUs), kernel 7.2.15-1-omarchy |
| Module | `br.com.dias.onnx-cutout`, engine `cpu` (`auto` with CUDA off), 4 threads, ONNX Runtime 1.31.1 (the CPU build), the default model `mediapipe-landscape`, `blur` |
| GStreamer | 1.28.8 |
| Cameras | Logitech MX Brio (046d:0944) at NV12 1080p60; laptop-integrated "FHD Camera" (5986:1193) at MJPEG 1080p30 and 720p30 |
| Output | NV12, 30 fps, limited range, into v4l2loopback; 1080p unless stated |
| Light | Dim. The FHD camera delivered 18 to 20 fps, so about half of its effect calls were repeats, which the element's repeat cache answers at no cost |

## Results: the daemon

| Camera, mode | Output | Effect | Consumer fps (dropped) | Daemon CPU, % of one core | `latency_p50` | Effect element, processed frames |
|---|---|---|---|---|---|---|
| FHD, MJPEG 1080p30 | 1080p30 | blur | 30.05 (0) | 41.8 | 11.4 ms | p50 6.0, p95 7.2, max 8.7 ms |
| FHD, MJPEG 1080p30 | 1080p30 | none | 30.05 | 16.8 | 11.9 ms | |
| FHD, MJPEG 1080p30 | 720p30 | blur | 30.04 | 42.7 | 16.8 ms | |
| FHD, MJPEG 1080p30 | 720p30 | none | 30.04 | 25.0 | 16.9 ms | |
| FHD, MJPEG 720p30 | 720p30 | blur | 30.04 | 25.0 | 7.2 ms | |
| FHD, MJPEG 720p30 | 720p30 | none | 30.02 | 8.5 | 7.6 ms | |
| MX Brio, NV12 1080p60 | 1080p30 | blur | 30.03 | 28.7 | 4.2 ms | p50 6.3, p95 7.4, max 20.3 ms |
| MX Brio, NV12 1080p60 | 1080p30 | none | 30.05 | 1.3 | 4.1 ms | |

## Results: the steps

Ingest, system memory (`gst-launch-1.0`, one chain, ms per frame, wall / CPU):

| Chain | 720p | 1080p |
|---|---|---|
| `jpegdec` only, 4:2:2 MJPEG re-encoded from a webcam clip at quality 85 | 1.40 / 1.39 | 2.65 / 2.65 |
| `jpegdec` of the camera's own 720p MJPEG (about 290 KB a frame) | 2.57 / 2.56 | not measured |
| `jpegdec ! I420 ! videoconvertscale ! NV12` at the same size (without the range adapter: it converts the format, but not the range, see the colour range below) | 1.55 / 1.54 | 3.04 / 3.04 |
| the same with a scale (1080p camera to 720p; 720p camera to 1080p) | 5.78 / 5.76 | 4.43 / 4.41 (from 720p) |
| YUY2 to NV12 at the same size, `videoconvertscale` (the test source's own cost subtracted) | about 0.7 | about 1.6 |
| NV12 to I420 for an I420 contract, `videoconvert` | about 0.12 | about 0.28 |
| the colour relabel, `capssetter` | about 0 | about 0 |
| the range conversion of the MJPEG path, `videobalance` (added) | +0.25 | +0.55 |

In the running daemon the MJPEG decode of the FHD camera's 1080p frames takes about 5.5 ms per camera frame (the tracer's `decode`): real webcam JPEGs are larger than the re-encoded clip above.

The effect, the module alone on host NV12 frames (4 ONNX Runtime threads; steps: convert in / matte / composite):

| Model, mode | 720p wall (steps) | 720p CPU | 1080p wall (steps) | 1080p CPU |
|---|---|---|---|---|
| MediaPipe landscape, blur | 3.8 ms (1.1 / 1.1 / 1.5) | 9.0 ms | 5.2 ms (0.7 / 1.1 / 3.3) | 12.9 ms |
| MediaPipe landscape, replace | 2.7 ms (0.9 / 1.0 / 0.7) | 5.6 ms | 3.1 ms (0.7 / 1.0 / 1.4) | 8.0 ms |
| PP-HumanSeg v2 mobile, blur | 7.7 ms (0.6 / 5.3 / 1.7) | 21.2 ms | 9.8 ms (0.9 / 5.3 / 3.5) | 27.1 ms |

The effect element's median of 6.0 to 6.3 ms at 1080p in the daemon includes copying the frame into the module and out of it, in addition to the module's 5.2 ms. The module's CPU figures include a copy of each input frame (about 0.3 ms at 1080p), so they are slight upper bounds.

### The colour range of the raw formats

The cameras' colour description against the levels they send (the luma range over 40 raw frames of the test scene):

| Camera, format | Label | Measured luma | Verdict |
|---|---|---|---|
| FHD, MJPEG | decoded as full range (`1:4:0:0`) | 0 to 255 | full, as labelled |
| FHD, YUYV | limited (`2:4:5:1`) | 22 to 235 | limited, as labelled |
| MX Brio, MJPEG | decoded as full range | 0 to 255 | full, as labelled |
| MX Brio, YUYV and NV12 | limited (`2:4:5:1`) | 0 to 255 (not reproduced, see below) | ~~full range, labelled limited~~ limited, as labelled |

The loopback's output (NV12, limited): luma 18 to 235 from the FHD camera (MJPEG, correct), and 0 to 255 from the MX Brio's NV12, because the pipeline trusts the label of a raw camera on both memory paths. A raw camera that labels full range as limited is a known limitation.

**Correction (2026-11-14): the MX Brio's 0 to 255 did not reproduce.** Controlled re-measurements with the camera connected (fixed settings, formats alternated MJPEG, YUYV, NV12, MJPEG at 1920x1080@30) show its YUYV and NV12 to be limited range as labelled: black at 16, highlights stopping at 235, the raw luma within 1 to 4 levels of 16 + MJPEG x 219/255 at every percentile, and 0.00 to 0.13 % of the samples outside 16 to 235. The MX Brio YUYV and NV12 row and the MX Brio output figure above are kept as the record of that run and are superseded by this paragraph; the cause of the first reading is not known. The FHD camera's rows are unaffected.

## Interpretation

- **The blur costs about 25 to 27 % of one core at 1080p30 and about 16 % at 720p30** on this machine; the camera's own ingest is the rest, from 1 % (NV12, no decode) to 17 % (MJPEG 1080p) with the effect off. With an MJPEG camera the daemon takes about 42 % of one core at 1080p30 and 25 % at 720p30, about 2 % of the whole 24-thread machine.
- **The output rate was held in every run:** 30 fps at the consumer with no dropped frame, at 1080p and 720p.
- **Added latency** is under one frame period: 4 to 17 ms to the output's `appsrc` with the camera at the contract size or a scale, plus about 6 ms for the effect itself at 1080p. The scale of a 1080p camera to a 720p output costs 5 to 6 ms per frame and its CPU time; a camera mode at the output size avoids it, and mode selection prefers one.
- **A 720p camera mode into a 720p output costs about 40 % less CPU than 1080p** (25 % against 42 %). A 720p output from a 1080p camera does not, because the camera is still decoded and then scaled.
- **`replace` costs less than `blur`** in the module: 3.4 ms (720p) and 4.9 ms (1080p) of CPU per frame less.
- **The model matters more than the resolution.** PP-HumanSeg v2 mobile costs about 2.1 to 2.4 times the CPU of the default MediaPipe model for blur, and takes about twice as long per frame, as its matte step is 5.3 ms against 1.1 ms.
- **The range conversion of the MJPEG path is small**: +0.55 ms per 1080p frame for `videobalance`, against +2.1 ms for a conversion through RGB, which was the alternative.

## Not measured

A machine with fewer or slower cores: from the per-frame CPU times above it is expected to take two to three times as long per frame, which was not measured. The `replace` effect and a 60 fps output through the daemon (the repeat path), the other models in the daemon, the OpenVINO engines, a PipeWire consumer, and the GPU's runtime suspend while the daemon runs in system memory were not measured either.
