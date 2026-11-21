# Camera and ingest-path results

Cost of each ingest path (see [../architecture.md](../architecture.md), section 2) measured with `obc-bench` on two cameras.

## Method

- **Tool:** `obc-bench`, release build, passthrough stage, `--consumer` reading the loopback, mode selected with `--camera` and `--mode`.
- **Duration:** 60 s per run, except the 2160p30 run (30 s). The first 3 s are excluded as warm-up.
- **Frame rate pinning:** both MJPEG runs used `--camera-fixed-framerate`, so the camera cannot lower its rate in dim light.
- **Re-measured rows:** the two MJPEG rows were measured again after the CPU decode path gained its GPU conversion to limited range (`cudaconvert` with a fixed BT.601 limited-range output, NV12 to the loopback). The other rows predate that change, which does not touch their paths.
- **Latency:** added latency is the time from the camera `v4l2src` src pad to the buffer being ready at the `v4l2sink` sink pad. It excludes sensor exposure and USB transfer, which an application reading the camera directly pays too. The full method is in [passthrough.md](passthrough.md#latency-method).
- **CPU:** percent of one core, for the whole process.
- **GPU memory:** process GPU memory reported by NVML.

## Conditions

| | |
|---|---|
| Machine | Intel Core Ultra 9 275HX (24 CPUs), kernel 7.2.15-1-omarchy |
| GPU / driver | NVIDIA GeForce RTX 5080 Laptop GPU, driver 615.78.08 |
| GStreamer | 1.28.8 |
| Cameras | Logitech MX Brio (046d:0944); laptop-integrated "FHD Camera" (5986:1193: MJPEG 720p30 and 1080p30, YUYV up to 960x540@15, separate IR node) |
| Concurrent load | A GPU-heavy 3D workload ran throughout: device GPU utilisation 50-79 %, load average 4-7. These are not quiet-machine figures |

## Results

| Path | Camera / mode | Sink / consumer fps | Added p50 / p95 / max ms | CPU | GPU mem |
|---|---|---|---|---|---|
| GPU-direct | MX Brio NV12 1920x1080@60 | 60.00 / 60.00 | 0.68 / 1.70 / 3.33 | 9.4 % | 200 MiB |
| CPU decode | FHD Camera MJPEG 1920x1080@30 | 29.95 / 29.95 | 4.78 / 7.58 / 10.31 | 18.0 % | 206 MiB |
| CPU convert | MX Brio YUY2 1920x1080@60 | 60.00 / 60.00 | 3.28 / 4.42 / 5.98 | 24.5 % | 200 MiB |
| CPU decode | MX Brio MJPEG 3840x2160@30 (30 s) | 30.00 / 30.00 | 18.79 / 21.70 / 30.46 | 63.8 % | 228 MiB |

Raw results:
[ingest-gpu-direct-nv12-1080p60-60s.json](ingest-gpu-direct-nv12-1080p60-60s.json),
[ingest-cpu-decode-mjpeg-1080p30-60s.json](ingest-cpu-decode-mjpeg-1080p30-60s.json),
[ingest-cpu-convert-yuy2-1080p60-60s.json](ingest-cpu-convert-yuy2-1080p60-60s.json),
[ingest-cpu-decode-mjpeg-2160p30-30s.json](ingest-cpu-decode-mjpeg-2160p30-30s.json).
These files record no hostname.

## Variance under contention

A repeat of the FHD MJPEG 1080p30 run during a load spike on the machine gave p95 64.6 ms and max 315 ms, with queue overruns. The table above shows a run without such a spike. Tail latency of the CPU paths depends on CPU scheduling; the GPU-direct path has the least CPU work in the stream and is the least exposed.

## Interpretation

- **CPU cost scales with the decode path.** GPU-direct is cheapest (9.4 % of a core). CPU convert of YUY2 at the same size and rate costs about 2.6 times as much (24.5 %). MJPEG decode costs 18.0 % at 1080p30 and 63.8 % at 2160p30.
- **Added latency follows the same order**, from 0.68 ms (GPU-direct) to 18.79 ms (MJPEG 2160p30), all measured p50. The 1080p paths stay below one frame period at 60 fps (16.7 ms); the 2160p30 MJPEG path stays below one frame period at 30 fps (33.3 ms), with its max at 30.46 ms close to that bound.
- **GPU memory is nearly constant** across paths (200-228 MiB); the ingest path changes CPU work, not the CUDA footprint.
- **Frame rate is held** on every path: the sink delivers the camera's rate.
- Drop counts are not tabulated here. The drop counts in the older JSON files in this directory (`passthrough-*.json`, `roundtrip-*.json`) over-count, because running totals were summed; use their frame-rate and PTS-gap fields instead.
