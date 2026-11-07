# oma-broadcast

A GPU webcam-effects pipeline for Linux. Frames flow from a camera through GStreamer and CUDA memory into a [v4l2loopback](https://github.com/v4l2loopback/v4l2loopback) virtual camera that any application can select. The planned effects (background blur and replacement) use the NVIDIA Maxine Video Effects SDK behind a replaceable inference-backend interface.

Any UVC webcam works: the pipeline picks a capture mode from what the camera advertises, and adds a CPU decode step for MJPEG cameras and a CPU convert step for YUYV/UYVY.

## Status

Early development. The camera -> CUDA -> virtual camera passthrough works, with sustained 1080p60 on the reference machine. Effects are not implemented yet. Measurements are in [docs/benchmarks/](docs/benchmarks/passthrough.md).

## Requirements

- NVIDIA GPU with the proprietary driver
- CUDA toolkit (for `nvrtc`)
- GStreamer 1.x with the `nvcodec` plugin (`cudaupload`, `cudaconvert`, `cudadownload`) and the base/good/bad plugin sets (`videoconvert`, `jpegdec`)
- v4l2loopback, configured as described in [docs/setup/loopback.md](docs/setup/loopback.md)
- Rust toolchain
- Optional, for effects: the NVIDIA Maxine VFX SDK ([docs/setup/maxine.md](docs/setup/maxine.md))

## Build

```sh
cargo build --release
```

## Run

```sh
cargo run --release -p obc-pipeline --bin obc-bench -- --duration 0
```

This streams to the "Omarchy Broadcast" loopback device until Ctrl-C. Then select "Omarchy Broadcast" as the camera in OBS or Chrome.

Optional arguments:

- `--camera <name|vid:pid|/dev/videoN>` chooses the camera (default: automatic; a name or ID matching two colour cameras is an error, and greyscale IR nodes are never picked).
- `--mode WxH@FPS` or `FORMAT:WxH@FPS`, for example `--mode 1920x1080@30` or `--mode NV12:1920x1080@60`, chooses the capture mode (default: `1920x1080@60`, any format; the closest mode the camera offers is used).

Tests, including the manual check with OBS or Chrome, are described in [docs/testing.md](docs/testing.md).

## Documentation

See [docs/README.md](docs/README.md) for the architecture, decision records, setup guides and benchmarks.

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
