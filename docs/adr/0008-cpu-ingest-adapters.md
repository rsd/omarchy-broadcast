# ADR-0008: CPU ingest adapters for MJPEG and YUYV/UYVY

- **Status:** Accepted

## Context
- The pipeline must accept any UVC webcam, and cameras differ in what they offer: NV12 (some), YUYV (most), MJPEG (nearly all, often the only way to reach 1080p30 or 4K30).
- Every link from `cudaupload` to the sink must carry `memory:CUDAMemory`.
- On the reference machine (RTX 5080 Laptop GPU, driver 615.78.08, GStreamer 1.28.8):
  - `cudaconvert`, `cudaconvertscale` and `cudascale` accept no YUY2 or UYVY input.
  - `nvjpegdec` (NVDEC) fails on the 4:2:2 MJPEG sent by typical UVC webcams with `unhandled chroma format 2`.

## Decision
The builder picks one ingest adapter per selected camera format:
- **GPU-direct:** NV12, and any raw format `cudaconvert` accepts: `v4l2src ! cudaupload`.
- **CPU convert:** YUYV/UYVY: `v4l2src ! videoconvert (to NV12) ! cudaupload`. The colour label is preserved.
- **CPU decode:** MJPEG: `v4l2src ! jpegdec ! cudaupload`. Decoded frames are full-range BT.601 (`1:4:0:0`); `cudaconvert` always rescales them to limited-range BT.601 (`2:4:5:1`) for the loopback, since many consumers ignore the range flag.

Mode selection prefers the paths in that cost order when the camera offers a choice at the same size and frame rate (see [architecture.md](../architecture.md), section 2).

## Alternatives considered

| Alternative | Why rejected or deferred |
|---|---|
| `nvjpegdec` for MJPEG | Rejects 4:2:2 MJPEG, which is what the test cameras send. It cannot be the general path |
| `nvjpegdec` with `jpegdec` as fallback | Adds a second path that the common cameras never take; more states to test for no benefit on the target hardware |
| Custom CUDA kernel for YUYV/UYVY | Deferred: removes the CPU convert, but adds kernel code to maintain. Revisit if CPU convert cost proves limiting |
| GPU JPEG decode through the nvJPEG library | Future work: would remove the dominant CPU cost at high resolutions |
| Force NV12 only | Excludes most webcams |

## Consequences
- **Measured cost** ([benchmarks/ingest-paths.md](../benchmarks/ingest-paths.md)): GPU-direct is cheapest (9.4 % of one core at 1080p60); CPU convert at 1080p60 costs 24.5 %; CPU decode of MJPEG costs 18.0 % at 1080p30 and 63.8 % at 2160p30.
- Added latency grows with the CPU work: p50 0.68 ms (GPU-direct), 3.28 ms (CPU convert 1080p60), 4.78 ms (MJPEG 1080p30), 18.79 ms (MJPEG 2160p30).
- Known limitation: high-resolution MJPEG ingest is CPU-bound until GPU JPEG decode exists.
- Downstream stages are unaffected: they see the same CUDA caps whichever adapter ran.
