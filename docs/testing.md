# Testing

## Unit tests

```sh
cargo test --workspace
```

Unit tests need no camera or GPU. Camera-capability tests use fixtures captured from two real cameras (a Logitech MX Brio and a laptop-integrated "FHD Camera") in `crates/obc-pipeline/tests/data/`. They cover V4L2 capability parsing, mode selection (size, then frame rate, then format) and ingest-path choice.

## Hardware tests

These run the real pipeline and are ignored by default. They need a camera, an NVIDIA GPU with CUDA, and a **free** v4l2loopback device as the sink (no other process may hold it; see [setup/loopback.md](setup/loopback.md)).

| Variable | Meaning |
|---|---|
| `OBC_TEST_CAMERA` | Camera: name substring, `vid:pid`, or `/dev/videoN` (`/dev/v4l/by-id` links work). The tests skip if unset |
| `OBC_TEST_MODE` | Optional mode, `WxH@FPS` or `FORMAT:WxH@FPS` |
| `OBC_TEST_SINK` | Loopback device, for example `/dev/video11` |

```sh
OBC_TEST_CAMERA=046d:0944 OBC_TEST_SINK=/dev/video11 \
  cargo test -p obc-pipeline --test hardware -- --ignored
```

On the reference machine all 6 hardware tests pass on both test cameras: a Logitech MX Brio (046d:0944) and a laptop-integrated "FHD Camera" (5986:1193).

## Manual test

1. Create the loopback device as described in [setup/loopback.md](setup/loopback.md).
2. Stream until interrupted:

   ```sh
   cargo run --release -p obc-pipeline --bin obc-bench -- --duration 0 [--camera <name|vid:pid|/dev/videoN>] [--mode 1920x1080@30]
   ```

   A status line is printed every 5 s: camera, mode, ingest path, sink fps, added latency p50, drops, CPU %. Streaming mode never applies the latency luma stamp, rejects `--consumer`, and writes JSON only with `--json`. `-v` prints debug lines.
3. Open the virtual camera. Because of `exclusive_caps=1`, the device appears to applications **only while streaming**, so start the stream first.
   - OBS: add a "Video Capture Device (V4L2)" source and choose "Omarchy Broadcast".
   - Chrome: open the camera settings or a camera test page and choose "Omarchy Broadcast".
4. Stop with Ctrl-C (SIGTERM works too). When the producer stops, v4l2loopback keeps the last frame, so consumers such as OBS show a frozen image until they reopen the device. This is expected; a future daemon may send a placeholder frame on stop.
   While a consumer keeps the device open, v4l2loopback also keeps the format, size and frame rate of the stream that fed it last. A new stream in another format or rate is refused at start with an error naming both formats ("loopback ... now accepts only NV12 1920x1080@60, but this stream is NV12 1920x1080@30"). Close or reopen the consumer's source, or stream in the held format.
5. Check that the camera controls were restored. If `--camera-fixed-framerate` was used, `v4l2-ctl -d /dev/videoN --get-ctrl=exposure_dynamic_framerate` must show the value it had before the run.
