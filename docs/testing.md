# Testing

## Unit tests

```sh
cargo test --workspace
```

The default run needs no camera and no GPU, but it needs GStreamer (1.28 or later) with its core elements, gst-plugins-base and gst-plugins-good: tests build pipelines from elements such as `videotestsrc`, `appsrc`, `appsink`, `fakesink`, `videoconvert`, `videoconvertscale`, `videocrop` and `capssetter`, and look up the `v4l2src` factory. A test that runs without what it needs fails, with a message naming the missing element or setting; no test passes without running.

Tests that need more than that are ignored by default, each with the reason (shown in the test listing), and run with `--ignored`:

- **GPU only** (`gpu_*`): an NVIDIA GPU and the GStreamer CUDA elements (`cudaupload`, `cudaconvertscale`, `cudaconvert`, `cudadownload`, from the nvcodec plugin of gst-plugins-bad, which registers them only where the GPU and its driver are present). No camera, no loopback. Eight unit tests in the library (the CUDA ingest and output pipelines as built, the roundtrip Process stage) and seven hardware tests. The `gpu_` filter selects exactly these; they run one at a time, since the hardware ones are timing tests sharing one GPU:

  ```sh
  cargo test --workspace -- --ignored gpu_ --test-threads=1
  ```

- **Camera and loopback**: the hardware tests below.
- `phase_lock_tuning_sweep`: a slow parameter sweep, see below.

Camera-capability tests use fixtures in `crates/obc-pipeline/tests/data/`: `v4l2-ctl --list-formats-ext` and `gst-device-monitor-1.0` output captured from two real cameras (a Logitech MX Brio and a laptop-integrated "FHD Camera", whose colour and infrared nodes are captured separately), and one synthetic camera, `synthetic-yuyv-only-1209-0001.v4l2-ctl.txt`, written by hand and marked as such in its header. They cover V4L2 capability parsing, mode selection (size, then frame rate, then format) and ingest-path choice.

Device discovery is tested by fixture replay, also without hardware (`crates/obc-pipeline/src/discovery.rs`). Each test builds a video4linux sysfs tree in a temporary directory, laid out as the kernel does: class entries linked into USB interface and device directories that carry `idVendor` and `idProduct`, and a v4l2loopback node under `devices/virtual` with its driver attributes. The node queries are answered from data: `VIDIOC_QUERYCAP` from `tests/data/v4l2-querycap.txt` (driver, card, bus info and capability bits of every node of a camera, keyed by USB vendor:product), and the format enumeration from the `v4l2-ctl --list-formats-ext` dumps. The tests cover:

- **Scan.** A machine with the MX Brio, the FHD Camera (a colour node, an infrared node and their metadata nodes) and a loopback yields the device list the selection tests build by hand: node paths in order, card names, USB ids, capture and loopback flags, and capabilities equal to the parsed dumps. Metadata nodes are queried but never enumerated, the infrared node is not taken as a colour camera, and the loopback is identified from sysfs alone, never queried or opened.
- **Selection.** Camera selection by USB id or name, and mode choice, on the discovered list match the tests over hand-built lists.
- **Direct probe.** Probing a node by path (an explicit `/dev/videoN`) follows the same rules: a loopback is identified without being opened, and a node without a sysfs entry is unknown.
- **YUYV-only camera.** The synthetic camera models a USB 2.0 camera that offers only discrete YUYV, with no MJPEG and no NV12. It is discovered like any other; a 1080p60 request keeps the size and drops to its 5 fps, and every choice takes the CPU-convert ingest path.

The output clock ([ADR-0010](adr/0010-phase-locked-output-clock.md)) is tested on scripted time (`crates/obc-pipeline/src/clock.rs`), without a pipeline:

- **Phase lock.** A 60 fps camera into a 30 fps contract at input phases across the whole period (including both ends of the grid and input periods): after acquisition every frame waits at most the margin plus 0.5 ms. Ten simulated minutes per camera rate (59.99, 59.993, 60.01, 29.97, 30.03 and 30 fps) with +-1 ms of uniform arrival jitter: the count emitted in the 600 s is 18000 +- 1, every wall-clock 1 s window (sliding, at every position) holds 30 +- 1, every PTS is on the grid and every index is emitted once in order, nothing is skipped, and the median wait stays within the margin plus 1 ms. A 15 fps and an irregular ~20 fps camera never engage the lock and produce exactly the ticks of a clock without it. Cameras whose drift against the ticks exceeds the slew (29.5, 29.6, 29.75, 30.25 and 31 fps against 30 fps, 58 fps against 60; 0.5 and 1 ms of jitter, 120 s each) are let go within a second, with at most one acquisition and no wrap; from then on every tick matches the free-running clock fed the same arrivals at the held phase, and the superseded frames, repeats and median wait are no worse than the free-running clock's at that phase (at most one frame more, from the ticks before the drift was measured) and within its range over eight camera phases. Cameras just beyond the slow side's limit of half the slew (29.82, 29.85 and 29.9 fps; 1 and 2 ms of jitter, 120 s each) are let go within five seconds, for good, with at most one acquisition and no wrap; from then on every tick matches the free-running clock at the held phase, and the superseded frames and repeats are within eight frames of it (the ticks before the drift was measured: up to 7 frames more at 2 ms of jitter, none at 1 ms), the median wait within 0.1 ms. A new source, and a phase jump of the same source, re-acquire within a few fresh frames.
- **Hold.** A switch holds the last frame past the 500 ms gap threshold until the awaited source's first frame; a hold that times out falls back to placeholders; a late frame of the previous source is emitted without ending the hold; a source lost without a switch still turns black after 500 ms; a hold never brings back a frame after placeholders.
- **Queue bound.** `a_slow_sink_sees_bounded_queue_and_frame_age` (CPU only, `crates/obc-pipeline/src/output.rs`): an `appsrc` built by the output's own builder, in system memory, feeding a `fakesink` that takes 100 ms per buffer while buffers are pushed at 30 fps. The queue level never exceeds the bound (3), every buffer is either delivered (in push order) or dropped and counted, the last three buffers pushed are the ones delivered at the end (the oldest are dropped, not the newest), and when a buffer reaches the sink the queue behind it holds at most the bound (read as pushed minus dropped minus delivered, under the lock the pushes take, so a late callback does not inflate it). The assertions count buffers rather than time, so a loaded machine does not change the outcome.

**Source switching** is tested on the CPU (`crates/obc-pipeline/src/session.rs`), with the real `Session`, ingest and output code and canonical frames in system memory instead of CUDA memory: test-pattern sources (`videotestsrc`, NV12 160x120 at 30 fps), the ingest tail with `videoconvertscale` in place of `cudaconvertscale`, an output contract of NV12 320x240 at 30 fps into a `fakesink`, without `cudadownload`, with a black placeholder made on the CPU, and a 100 ms gap threshold. Every tick is recorded by the output's observer, with its kind and the source and sequence number of its frame; "repeats the old frame" below means a repeat of the last frame the old source delivered.
- `a_switch_holds_stops_then_starts_and_is_timed`: the old source's stop is slowed to 400 ms after its frames stop; every tick until the new source's first frame repeats the old source's frame (a hold armed after the stop would show placeholders after 100 ms). The old pipeline is in NULL when the new one makes its first state change. The timings are ordered (requested, previous stopped, started, first fresh frame), the first fresh frame is the emission of the new source's first tick, and the hold is released by it. The switch recorded by the session is the one `switch_to` returned.
- `a_failed_switch_is_recorded_and_the_next_one_succeeds`: a new source that cannot start (`filesrc` on a missing file) is recorded with the error `switch_to` returned, after the old source was stopped, and leaves the session without a source. The output keeps emitting: the old frame repeated until the 300 ms hold times out, then placeholders. A later switch succeeds.
- `a_switch_repeats_the_old_frame_until_the_new_source_delivers`: the new source delivers its first frame once its queue holds 400 ms of frames; every tick in between repeats the old frame, none is a placeholder, and the hold lasts at least 300 ms (the queue's release less one frame period of capture, with margin).
- `a_hold_that_times_out_falls_back_to_placeholders`: the new source never delivers; the old frame is repeated until the 400 ms hold times out, then only placeholders follow, none before the timeout.
- `a_failed_stop_drops_the_next_source_and_the_hold_times_out`: the old source's pipeline does not end in NULL (the test sets it back to READY from its bus when it reaches NULL), so its stop fails. The new source is never started (its pipeline makes no state change) and none of its frames is emitted; the switch is recorded with the stop's error and no stop or start time, and the session is left without a source. The hold armed for the new source runs to its 400 ms timeout, the old frame repeated, then placeholders, none before the timeout.
- `a_system_memory_output_streams_an_i420_contract`: an I420 320x240 contract converts the canonical NV12 frames with `videoconvert`; every buffer reaching the `fakesink` carries I420 320x240 caps and is a whole frame of that size, and the test pattern's white bars reach it.

The assertions count and order ticks; the only times compared are lower bounds, which a loaded machine can only lengthen.

The phase lock's defaults come from a parameter sweep over simulated cameras with drift and 0.25, 1 and 2 ms of arrival jitter; it is an ignored test that prints its table:

```sh
cargo test --release -p obc-pipeline --lib phase_lock_tuning_sweep -- --ignored --nocapture
```

## Hardware tests

These run the real pipelines and are ignored by default (17 tests in `crates/obc-pipeline/tests/hardware.rs`), each with the reason naming what it needs. Which hardware a test needs:

| Tests | Need |
|---|---|
| `gpu_*` (7: crop and letterbox geometry read back from the GPU, stalled and slow sources, a dead output clock, a failed switch whose hold times out, a switch held across a slow-starting source, a slow sink behind the real output) | An NVIDIA GPU and the GStreamer CUDA elements. No variable, no camera, no loopback |
| `output_restarts_while_a_consumer_holds_the_loopback` | A free v4l2loopback device named by `OBC_TEST_SINK`, and the GPU. No camera. It fails unless `OBC_TEST_SINK` is set, so it never writes to a device that merely carries the default label |
| `selects_test_camera_by_both_routes`, `control_guard_restores_on_drop_and_unwind` | The camera named by `OBC_TEST_CAMERA`, not streaming. The control test needs its `exposure_dynamic_framerate` control writable |
| The other camera tests (`ingest_restores_the_camera_control_on_stop`, pipeline `passthrough` / `roundtrip`, `every_ingest_path_outputs_requested_format`, `output_holds_the_contract_frame_rate`, `switching_sources_keeps_the_output_and_its_consumer`, `held_loopback_format_is_reported_before_streaming`) | The camera, the GPU and a **free** v4l2loopback device as the sink (no other process may hold it; see [setup/loopback.md](setup/loopback.md)) |

An ignored test that is run without its prerequisite fails, with a message naming what is missing and how to provide it: a camera test when `OBC_TEST_CAMERA` is unset, a control test (`control_guard_restores_on_drop_and_unwind`, `ingest_restores_the_camera_control_on_stop`) when the camera's `exposure_dynamic_framerate` control is missing or read-only, a test that writes to the loopback when no sink can be found (without `OBC_TEST_SINK` it falls back to the device labelled "Omarchy Broadcast", and fails if there is none), and any test when a GStreamer element it builds is not installed. On a machine with the GPU but no camera or loopback, run the `gpu_` tests only; the full run needs all three.

| Variable | Meaning |
|---|---|
| `OBC_TEST_CAMERA` | Camera: name substring, `vid:pid`, or `/dev/videoN` (`/dev/v4l/by-id` links work). The camera tests fail if unset |
| `OBC_TEST_CAMERA_2` | Optional second camera for the source-switching test. Unset, that test switches between two openings of `OBC_TEST_CAMERA`. The second camera always uses the default mode request |
| `OBC_TEST_MODE` | Optional camera mode for `OBC_TEST_CAMERA`, `WxH@FPS` or `FORMAT:WxH@FPS`. The output contract is the default (NV12 1920x1080@30, limited range) whatever the camera mode; `every_ingest_path_outputs_requested_format` also runs an I420 contract |
| `OBC_TEST_SINK` | Loopback device, for example `/dev/video11`. Must be a v4l2loopback device |

GPU-only hardware tests, then all of them (camera, loopback and GPU):

```sh
cargo test -p obc-pipeline --test hardware -- --ignored gpu_ --test-threads=1
OBC_TEST_CAMERA=046d:0944 OBC_TEST_SINK=/dev/video11 \
  cargo test -p obc-pipeline --test hardware -- --ignored --test-threads=1
```

`gpu_a_switch_holds_the_last_frame_until_the_new_source` switches to a test source that delivers its first frame 1 s after it starts: every tick in between is a repeat of the previous source's (white) frame, none is a placeholder. `gpu_a_failed_switch_is_recorded_with_its_error` runs with a 1 s hold: the failed switch repeats the last frame for the hold, then sends placeholders. `gpu_a_slow_sink_never_queues_more_than_the_bound` puts the real output (CUDA frames, download included) in front of a `fakesink` that takes 100 ms per frame: the `appsrc` queue level never exceeds 3, the drops are counted, the clock skips nothing, and no frame reaches the sink more than about 300 ms after its emission.

The switching test asserts that the output never stalls (the longest interval without a frame, at the loopback sink and at an in-process consumer, stays within two output periods plus 20 ms), that the sink negotiates caps exactly once for the whole test, that the consumer sees no renegotiation, and that the UVC control is restored across a switch. A failed assertion still restores the camera's original `exposure_dynamic_framerate`.

On the reference machine all 17 hardware tests pass on both test cameras: a Logitech MX Brio (046d:0944, also with `OBC_TEST_MODE=NV12:640x480@30`) and a laptop-integrated "FHD Camera" (5986:1193). The switching test also passes between the two (`OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193`).

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
   | `--hold-timeout` | Seconds a switch holds the last frame while waiting for the next camera's first frame; default 3. Placeholders follow if it has not arrived by then |
   | `--output-queue-buffers` | Buffers the output's `appsrc` holds before dropping the oldest, and the clock's longest catch-up burst; default 3 |
   | `--no-phase-lock` | Emit every tick at its timestamp instead of locking the ticks' phase to the camera (the behaviour before [ADR-0010](adr/0010-phase-locked-output-clock.md)), for comparison |
   | `--phase-margin-ms`, `--phase-window` | Phase lock tuning: the margin after the latest-arriving frame, and the number of fresh frames it is taken over (defaults from the library) |
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
   - The OBS source never shows an error and never goes black: during the switch gap (0.65 to 1.05 s in measurement, [benchmarks/io-split.md](benchmarks/io-split.md)) it shows the previous camera's last frame, held until the new camera's first frame.
   - The resolution stays 1920x1080 throughout (source properties, or the source's size in the preview).
   - No black frame appears at a switch. Black appears only if the new camera takes longer than the hold timeout (`--hold-timeout`, default 3 s) to deliver, or if a camera stops without a switch (after 500 ms, `--input-gap-ms`).
   - The console prints one line per switch, with the measured gap, how long the last frame was held, and the repeats and placeholders sent during it, and a status line every 5 s with output fps at 30.0.
4. Send `pkill -USR1 obc-bench`: the stream switches to the next camera immediately, without waiting for the 10 s timer, with the same behaviour as above.
5. Stop `obc-bench` with Ctrl-C and again verify that the control was restored on both cameras (`v4l2-ctl -d /dev/videoN --get-ctrl=exposure_dynamic_framerate`; relevant when `--camera-fixed-framerate` was added to the command).
6. Aspect check: restart with a 4:3 camera mode and `--aspect letterbox`, for example `--camera 046d:0944 --mode NV12:640x480@30 --aspect letterbox`. OBS shows the 4:3 image with black pillars on both sides in a 1920x1080 frame; with the default `--aspect crop` the image fills the frame, with the top and bottom cropped.
7. Held-format check: with OBS still showing "Omarchy Broadcast", stop the stream and start it again with another contract, for example `--output 1280x720@30`. If OBS still holds the device, the start fails with the "now accepts only ..." message above, before any frame is sent. Close the OBS source (or remove the source) and the start succeeds.
