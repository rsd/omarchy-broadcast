# Testing

## Unit tests

```sh
cargo test --workspace
```

Unit tests need no camera or GPU. Camera-capability tests use fixtures captured from two real cameras (a Logitech MX Brio and a laptop-integrated "FHD Camera") in `crates/obc-pipeline/tests/data/`. They cover V4L2 capability parsing, mode selection (size, then frame rate, then format) and ingest-path choice.

## Hardware tests

These run the real pipelines and are ignored by default (15 tests in `crates/obc-pipeline/tests/hardware.rs`). Which hardware a test needs:

| Tests | Need |
|---|---|
| `gpu_*` (5: crop and letterbox geometry read back from the GPU, stalled and slow sources, a dead output clock, a failed switch) | An NVIDIA GPU with CUDA. No variable, no camera, no loopback |
| `output_restarts_while_a_consumer_holds_the_loopback` | A free v4l2loopback device named by `OBC_TEST_SINK`, and the GPU. No camera. It skips unless `OBC_TEST_SINK` is set, so it never writes to a device that merely carries the default label |
| `selects_test_camera_by_both_routes`, `control_guard_restores_on_drop_and_unwind` | The camera named by `OBC_TEST_CAMERA`, not streaming |
| The other camera tests (`ingest_restores_the_camera_control_on_stop`, pipeline `passthrough` / `roundtrip`, `every_ingest_path_outputs_requested_format`, `output_holds_the_contract_frame_rate`, `switching_sources_keeps_the_output_and_its_consumer`, `held_loopback_format_is_reported_before_streaming`) | The camera, the GPU and a **free** v4l2loopback device as the sink (no other process may hold it; see [setup/loopback.md](setup/loopback.md)) |

A camera test skips (printing `SKIPPED`, visible with `--nocapture`) when `OBC_TEST_CAMERA` is unset, and a test that writes to the loopback skips when no sink can be found: without `OBC_TEST_SINK` it falls back to the device labelled "Omarchy Broadcast" and skips if there is none. A skipped test passes vacuously; check the output when a run must have exercised hardware.

| Variable | Meaning |
|---|---|
| `OBC_TEST_CAMERA` | Camera: name substring, `vid:pid`, or `/dev/videoN` (`/dev/v4l/by-id` links work). The camera tests skip if unset |
| `OBC_TEST_CAMERA_2` | Optional second camera for the source-switching test. Unset, that test switches between two openings of `OBC_TEST_CAMERA`. The second camera always uses the default mode request |
| `OBC_TEST_MODE` | Optional camera mode for `OBC_TEST_CAMERA`, `WxH@FPS` or `FORMAT:WxH@FPS`. The output contract is the default (NV12 1920x1080@30, limited range) whatever the camera mode; `every_ingest_path_outputs_requested_format` also runs an I420 contract |
| `OBC_TEST_SINK` | Loopback device, for example `/dev/video11`. Must be a v4l2loopback device |

```sh
OBC_TEST_CAMERA=046d:0944 OBC_TEST_SINK=/dev/video11 \
  cargo test -p obc-pipeline --test hardware -- --ignored --test-threads=1
```

The switching test asserts that the output never stalls (the longest interval without a frame, at the loopback sink and at an in-process consumer, stays within two output periods plus 20 ms), that the sink negotiates caps exactly once for the whole test, that the consumer sees no renegotiation, and that the UVC control is restored across a switch. A failed assertion still restores the camera's original `exposure_dynamic_framerate`.

On the reference machine all 15 hardware tests pass on both test cameras: a Logitech MX Brio (046d:0944, also with `OBC_TEST_MODE=NV12:640x480@30`) and a laptop-integrated "FHD Camera" (5986:1193). The switching test also passes between the two (`OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193`).

## Manual test

1. Create the loopback device as described in [setup/loopback.md](setup/loopback.md).
2. Stream until interrupted:

   ```sh
   cargo run --release -p obc-pipeline --bin obc-bench -- --duration 0 [--camera <name|vid:pid|/dev/videoN>] [--mode 1920x1080@30]
   ```

   Options that shape the stream:

   | Flag | Meaning |
   |---|---|
   | `--camera` | Repeatable. Two or more cameras give the switching cycle, in order. With one or none, the cycle continues with every other discovered colour camera |
   | `--mode` | Camera mode (`WxH@FPS` or `FORMAT:WxH@FPS`), resolved against each camera's capabilities; default `1920x1080@60` |
   | `--output` | Output contract size and frame rate, `WxH@FPS` (FPS may be `N/D`); default `1920x1080@30` |
   | `--output-format` | `nv12` (default) or `i420` |
   | `--output-range` | `limited` (default) or `full`. `full` is **refused for a v4l2loopback sink** before anything streams, because the device carries no colour range and consumers would read the stream as limited; the help text still lists it |
   | `--aspect` | `crop` (default, centre crop to fill) or `letterbox` (black borders) for a source of another aspect ratio |
   | `--input-gap-ms` | Milliseconds without input before the output sends placeholders instead of repeating the last frame; default 500 |
   | `--switch-every` | Move to the next camera every N seconds (after warm-up) |
   | `--camera-fixed-framerate` | Pin `exposure_dynamic_framerate` to 0 while a camera streams |

   **Signal:** `SIGUSR1` moves to the next camera at once (for example `pkill -USR1 obc-bench`). A `SIGUSR1` received before streaming begins (camera start, warm-up) is discarded.

   **Streaming mode** (`--duration 0`): a status line is printed every 5 s, and one line per switch. The status line shows camera, mode and ingest path, input fps, output fps, consumer fps (with `--consumer`), the counts of fresh, repeat, placeholder and superseded frames, added latency p50 and CPU %. Streaming mode never applies the latency luma stamp. `--consumer` is accepted there and reports the consumer frame rate only; end-to-end latency needs a measured run. JSON is written only with `--json`. `-v` prints debug lines.

3. Open the virtual camera. Because of `exclusive_caps=1`, the device appears to applications **only while streaming**, so start the stream first.
   - OBS: add a "Video Capture Device (V4L2)" source and choose "Omarchy Broadcast".
   - Chrome: open the camera settings or a camera test page and choose "Omarchy Broadcast".
4. Stop with Ctrl-C (SIGTERM works too). When the producer stops, v4l2loopback keeps the last frame, so consumers such as OBS show a frozen image until they reopen the device. This is expected.
5. Check that the camera controls were restored. If `--camera-fixed-framerate` was used, `v4l2-ctl -d /dev/videoN --get-ctrl=exposure_dynamic_framerate` must show, for every camera used, the value it had before the run. Controls are also restored on every source switch; they are not restored if the process is killed with SIGKILL or SIGQUIT.

### What happens when a consumer holds the device in another format

While a consumer keeps the device open, v4l2loopback keeps the format, size and frame rate of the stream that fed it last and refuses every other. `obc-bench` checks this once, before streaming, when the device's accepted caps are fixed to one format. It fails with a message of this form:

```text
loopback /dev/video11 now accepts only NV12 1920x1080@60, but this stream is NV12 1920x1080@30. v4l2loopback keeps the format of an earlier stream while any consumer (e.g. OBS) still holds the device open: close or reopen the consumer's video source, or stream in that format
```

Close or reopen the consumer's source, or stream in the held format (`--output`). The device's colour label is part of the check and is compared by meaning; the output always labels its frames `2:4:5:4`, which the loopback reports. A device that simply does not offer the contract (for example a loopback restricted to other formats) fails with the generic "does not accept" message instead.

### Manual OBS test: switching cameras under a live consumer

This checks what the stream promises to a real application: the virtual camera keeps one format and a steady rate while the camera behind it changes. It needs two cameras.

1. Start the stream with both cameras, named by card name or `vid:pid` (not `/dev/videoN`, which can change between runs), switching every 10 s:

   ```sh
   cargo run --release -p obc-pipeline --bin obc-bench -- --duration 0 \
     --camera 046d:0944 --camera 5986:1193 --switch-every 10
   ```

2. In OBS, add a "Video Capture Device (V4L2)" source and choose "Omarchy Broadcast". Leave the resolution and format settings at their defaults.
3. Verify, for at least three switches:
   - The image changes between the two cameras every 10 s.
   - The OBS source never shows an error and does not go blank for longer than the switch gap (about 0.65 to 1.05 s).
   - The resolution stays 1920x1080 throughout (source properties, or the source's size in the preview).
   - A brief black frame about 0.5 s into a switch is expected: the output repeats the last frame, then sends placeholders once 500 ms pass without input, until the new camera delivers.
   - The console prints one line per switch, with the measured gap and the repeats and placeholders sent during it, and a status line every 5 s with output fps at 30.0.
4. Send `pkill -USR1 obc-bench`: the stream switches to the next camera immediately, without waiting for the 10 s timer, with the same behaviour as above.
5. Stop `obc-bench` with Ctrl-C and again verify that the control was restored on both cameras (`v4l2-ctl -d /dev/videoN --get-ctrl=exposure_dynamic_framerate`; relevant when `--camera-fixed-framerate` was added to the command).
6. Aspect check: restart with a 4:3 camera mode and `--aspect letterbox`, for example `--camera 046d:0944 --mode NV12:640x480@30 --aspect letterbox`. OBS shows the 4:3 image with black pillars on both sides in a 1920x1080 frame; with the default `--aspect crop` the image fills the frame, with the top and bottom cropped.
7. Held-format check: with OBS still showing "Omarchy Broadcast", stop the stream and start it again with another contract, for example `--output 1280x720@30`. If OBS still holds the device, the start fails with the "now accepts only ..." message above, before any frame is sent. Close the OBS source (or remove the source) and the start succeeds.
