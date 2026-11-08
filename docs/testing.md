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

**The loopback writer** ([ADR-0011](adr/0011-daemon-lifecycle-and-dbus-api.md)) is tested on the CPU in `crates/obc-pipeline/src/loopback.rs`, with a `fakesink` or a system-memory session in place of the loopback and the CUDA elements:
- `placeholder_timing_rules`: when a placeholder is due (two contract periods after the last forwarded frame) and when it is not.
- `the_black_frame_fills_the_contract_layout`: NV12 and I420, at 1920x1080 and 642x480: the whole frame is covered, luma 16, chroma 128, with the layout `GstVideoInfo` gives.
- `the_writer_pipeline_has_the_contract_caps_and_no_cuda_element`: the `appsrc` carries the contract caps, is live, holds at most 3 buffers and does not block; the `v4l2sink` is `sync=false`; no `nvcodec` or CUDA element is in the pipeline; a full-range contract is refused for a loopback and accepted for a `fakesink`.
- `alone_the_writer_sends_black_frames_at_the_contract_rate`: NV12 and I420 320x240@30 into a `fakesink`: one caps event, 30 black whole frames sharing one memory, the k-th timestamp at least k periods, timestamps strictly increasing.
- `a_forwarded_buffer_is_restamped_without_a_copy`: the sink sees the same memory address as the producer's buffer; the producer's buffer keeps its own timestamp; pushes before start and after stop are refused and counted.
- `a_writer_output_ends_in_an_appsink_with_the_writer_contract`: an output built for the writer ends in an `appsink` named `loopback`; a contract different from the writer's is refused.
- `the_writer_hands_over_between_placeholder_and_output_without_renegotiating`: placeholders, then a session forwarding a live pattern, then the session stopped. The sink sees placeholder, forwarded, placeholder runs with no placeholder among the forwarded frames, the output's caps equal the writer's, one caps event, strictly increasing timestamps, and placeholders resuming no sooner than two periods after the last forwarded frame. The interleaved count and the resume time (66.7 ms measured) are printed, not asserted, so a loaded machine does not fail the test.

`live.rs` tests the statistics behind `Stats` (`a_window_counts_ticks_and_takes_fresh_latencies`, `memory_is_bounded`, `live_stats_follow_a_cpu_stream`: a live pattern held 100 ms by a queue gives a p50 of at least 100 ms less one capture period, and the window's ticks and fresh counts sum to the output's). Ingest failures are tested on the CPU too: `a_failing_ingest_is_reported_and_a_healthy_one_is_not` (a streaming error and an end of stream are reported once, a healthy source and no source report nothing, the output keeps running) and `a_vanished_camera_node_is_reported`.

The phase lock's defaults come from a parameter sweep over simulated cameras with drift and 0.25, 1 and 2 ms of arrival jitter; it is an ignored test that prints its table:

```sh
cargo test --release -p obc-pipeline --lib phase_lock_tuning_sweep -- --ignored --nocapture
```

## Hardware tests

These run the real pipelines and are ignored by default (19 tests in `crates/obc-pipeline/tests/hardware.rs`), each with the reason naming what it needs. Which hardware a test needs:

| Tests | Need |
|---|---|
| `gpu_*` (7: crop and letterbox geometry read back from the GPU, stalled and slow sources, a dead output clock, a failed switch whose hold times out, a switch held across a slow-starting source, a slow sink behind the real output) | An NVIDIA GPU and the GStreamer CUDA elements. No variable, no camera, no loopback |
| `output_restarts_while_a_consumer_holds_the_loopback` | A free v4l2loopback device named by `OBC_TEST_SINK`, and the GPU. No camera. It fails unless `OBC_TEST_SINK` is set, so it never writes to a device that merely carries the default label |
| `the_writer_alone_is_a_capture_device_with_black_frames` | A free v4l2loopback device with `exclusive_caps=1` and no other writer, named by `OBC_TEST_SINK`. No camera, no GPU |
| `a_consumer_keeps_one_stream_across_idle_active_idle` | The camera, the GPU, `nvidia-smi` and a free loopback named by `OBC_TEST_SINK` (required) |
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

`the_writer_alone_is_a_capture_device_with_black_frames` asserts that the device is not a capture device before the writer starts and is one (`VIDIOC_QUERYCAP`) once it streams; that a consumer in another process receives at least 60 black frames with one caps negotiation (NV12 1920x1080 30/1); that the writer's sink sees one caps event and placeholders only; that the device is no capture device again after the writer stops; and that the `nvcodec` plugin was not loaded by the writer.

`a_consumer_keeps_one_stream_across_idle_active_idle` runs the daemon's hand-over without the daemon: writer placeholders, then a CUDA session whose output forwards to the writer, then the session stopped. A consumer in another process must see placeholder, forwarded, placeholder runs with black placeholders; one caps negotiation at the consumer and at the writer's sink; a longest silence of at most three periods plus 30 ms at both; no writer placeholder while frames are forwarded; fresh camera frames; the process listed by `nvidia-smi` while the session runs and not within 5 s of its stop; the camera descriptor closed after the stop; and no failed pushes. It changes no camera control, and prints only the picture content it saw.

The consumer is the helper binary `obc-consumer` (`crates/obc-pipeline/src/bin/obc-consumer.rs`), built with the crate. It reads the device as a separate process with `v4l2src io-mode=mmap ! video/x-raw ! appsink` and prints one line per event (`caps`, `frame` with the mean, minimum and maximum luma of a row and the frame's stamp, `error`, `eos`); it exits when its standard input closes.

On the reference machine all 19 hardware tests pass with `OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193` (a Logitech MX Brio and a laptop-integrated "FHD Camera"), and the earlier 17 also pass on each camera alone, the Brio also with `OBC_TEST_MODE=NV12:640x480@30`.

## Daemon, CLI and D-Bus crate

### Default suite (no camera, no GPU)

```sh
cargo test -p obc-ipc -p oma-broadcastd -p oma-broadcast
```

**`oma-broadcastd`** (`crates/oma-broadcastd`):
- The controller state machine runs against a fake media backend with explicit instants, so no test sleeps for a timer. It covers every transition of [ADR-0011](adr/0011-daemon-lifecycle-and-dbus-api.md): the debounce (a 100 ms open never starts the camera), the grace period (a consumer returning inside it keeps the session), `off` keeping the writer, persistence of `Enabled` and the selection across a restart, `SelectCamera` / `NextCamera` while idle and while streaming, the `NoSuchCamera` / `Unavailable` / `Failed` errors, fallback to the next camera, `error` and recovery, no pre-emption of a working stream, an invalid reload, an invalid config at start, deferral of an output-mode change while a consumer holds the device, a missing loopback retried every 2 s, a failing writer, the 10 s start timeout, every camera failing (the session stops rather than cycling), a damaged state file (daemon `off`, file untouched), `SetEnabled(false)` during a start, and `Stats` only while `active`.
- Consumer counting uses `sleep` children that hold a temporary file open: the daemon's own descriptor is excluded, counts rise and fall, inotify events trigger recounts, and the polling fallback works. Also covered: the inotify wrapper, hotplug debouncing, camera ids, ordering and selectors.
- The D-Bus object is served over a peer-to-peer socket pair and driven through `obc_ipc::OmaBroadcastProxy`: every property (including `GetAll` and the introspection data), every method and error name, the `PropertiesChanged` sequence, `Stats` while active and none for 2.5 s while idle.
- Process tests (`tests/process.rs`) run the binary on a private `dbus-daemon` (`/usr/bin/dbus-daemon` is required; without it the tests fail naming it): a second instance exits with status 3 before touching a device, no session bus gives status 1, `--help` and an unknown option behave.

**`oma-broadcast`** (`crates/oma-broadcast/tests/cli.rs`, plus unit tests of the JSON and human formatting): the CLI runs against a mock daemon on a private `dbus-daemon` with its own configuration and an activation file whose command is `/bin/sh -c 'echo > marker; exit 1'`, so a failed activation is quick and visible. Both `/usr/bin/dbus-daemon` and `/bin/sh` are required. The tests cover `status`, `cameras`, `on`, `off` and `camera` in human and JSON form, exit codes (3 for a daemon that is not running or no session bus, with reads never activating the daemon), `watch --json` following changes, `Stats` and outages, waiting for the daemon to appear, a daemon that holds its name but does not answer, and `watch` exiting quietly when its reader (`| head -1`) goes away.

**`obc-ipc`** (`crates/obc-ipc/tests/mock_server.rs`): a mock server over a peer-to-peer connection checks that the proxy reads every property, calls every method, maps errors back by name, receives `Stats`, and that the no-auto-start read sees every property, carries the `NO_AUTO_START` flag at the server and reports a missing object as an error.

### End-to-end hardware test (ignored)

`crates/oma-broadcastd/tests/hardware.rs::the_daemon_runs_the_lifecycle_on_real_devices` runs the real daemon, a private `dbus-daemon`, temporary `XDG` directories and a grace period of 1 s, with `obc-consumer` as the application.

| Needs | |
|---|---|
| `OBC_TEST_SINK` | A free v4l2loopback device with `exclusive_caps=1`, for example `/dev/video11`. Required |
| `OBC_TEST_CAMERA`, `OBC_TEST_CAMERA_2` | Two different cameras, for the switching checks. Both required |
| GPU | An NVIDIA GPU and `nvidia-smi` |
| `/usr/bin/dbus-daemon`, `cargo` | The test builds `obc-consumer` (the `OBC_CONSUMER` variable names a prebuilt one) |

```sh
OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193 \
  cargo test -p oma-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture
```

It asserts, in order:
1. **Idle.** The loopback is a capture device and the writer holds it; no camera descriptor and no `/dev/nvidia*` descriptor is open; `nvidia-smi` lists no CUDA process.
2. **Active.** With a consumer attached, `Consumers` = 1 and the state becomes `active`; the consumer receives stamped camera frames; the camera descriptor is open and `nvidia-smi` lists the daemon; at least three `Stats` arrive with fps within 25 to 35 and a latency within 0 to 1000 ms.
3. **Switching.** `NextCamera` and `SelectCamera` move the camera descriptor between the two cameras in both directions while `active`; `NoSuchCamera` is returned for an unknown selector.
4. **Off.** `SetEnabled(false)` gives `off`: the camera descriptors are gone within 3 s and the CUDA process is gone, while the consumer keeps receiving placeholder frames. The consumer's runs are placeholder, session, placeholder, with one caps negotiation in total.
5. **Restart.** After restarting the daemon `Enabled` is false and the selection is kept; the state is `off` with a consumer attached; `SetEnabled(true)` leads to `active`.
6. **Idle again.** When the consumer exits, the state returns to `idle` no sooner than the grace period, and the camera and CUDA descriptors are gone.
7. **Reload.** `SIGHUP` and `ReloadConfig` both apply a change; an invalid file gives `InvalidConfig`, sets `Error` and keeps the previous camera order.
8. **Exit.** `SIGTERM` while active exits with status 0 and releases the loopback; the loss of the bus connection exits with status 1.

The test sets the environment variable `OBC_TEST_STAMP_FORWARDED=1` for the daemon, which stamps the frames the output forwards so that the consumer can tell camera frames from placeholders; it exists for this test only. The test changes one UVC control and restores it.

### Measured on the reference machine

Release build, a Logitech MX Brio and an FHD Camera, v4l2loopback 0.15.4, kernel 7.2, default configuration (grace 5 s, debounce 300 ms).

| Measure | Result |
|---|---|
| Idle daemon CPU (60 s) | 0.88 % of one core, almost all in the writer's streaming thread; the watcher threads use none. With a consumer attached and the state `off`: 0.90 % (20 s) |
| Idle resources | `/dev/video11` held (two descriptors), no camera descriptor, no `/dev/nvidia*` descriptor, no CUDA process in `nvidia-smi` |
| Enumeration opens (`v4l2-ctl --all`, `gst-device-monitor-1.0`) | `Consumers` unchanged, no start |
| Consumer open to `active` | 2.54 s: 0.3 s debounce, about 0.5 s output pipeline and CUDA initialisation, the rest the camera starting (the MX Brio takes about 1.7 s) |
| Last consumer closes to `idle` | the grace period plus 0.73 s of teardown on the MX Brio (5.73 s with the default grace); the teardown is 0.08 s on the FHD Camera. Camera and CUDA are released at the end |
| While `active` | fds: camera x2, loopback x2, `/dev/nvidia0` x21, `nvidiactl` x2, `nvidia-uvm` x2; `nvidia-smi` lists the daemon |
| After the return to `idle` | no camera descriptor, no CUDA process; 7 descriptors on `/dev/nvidia*` remain (the `cuInit` handles of [ADR-0011](adr/0011-daemon-lifecycle-and-dbus-api.md)) |
| `Stats` while `active` | 1 Hz; fps 29.99 to 30.00 (a window that contains a camera switch: about 29.8); cpu 3 to 6 % (8 to 11 % for about 6 s after a switch to the FHD Camera); gpu 0.5 to 1 %; `-1` once, right after a switch, when no camera frame fell in the window; none while `idle` |
| Consumer view | one caps negotiation (NV12 1920x1080, PAR 1/1, 30/1, `2:4:5:4`) for the whole run; maximum gap between frames 34 ms across idle, active and idle, 36 ms across four live camera switches; no placeholder during a switch |
| `SetEnabled(false)` while `active` | camera and CUDA released; the consumer's one 67 ms gap (two periods, the placeholder threshold) |

**Latency sawtooth.** `latency_p50_ms` ([dbus-api.md](dbus-api.md)) is the time from the camera buffer leaving the source to its tick being handed to the output. The phase lock of [ADR-0010](adr/0010-phase-locked-output-clock.md) engages only when the camera delivers at close to its nominal rate. In a dark room the cameras ran below it: the MX Brio at 15 fps (14.99 measured, with `exposure_dynamic_framerate` on) and the FHD Camera at 20 fps (19.98), both below the regime in which the lock engages. The output ticks then drift against the camera's frames, and the median latency is wherever the phase sits on a sawtooth: it falls by about 0.8 to 1.2 ms per second from about 32 ms to 2.5 ms, then jumps back, with a period of about 42 s. The observed values of 3 to 30 ms are points on that curve, not a fault. With `--camera-fixed-framerate` the Brio runs at 60 fps, the lock engages, and the added latency is about 4 ms (p50 4.0, p95 6.5 in `obc-bench`).

These figures were measured by hand on the stated machine; no test asserts them.

### Discovery by PipeWire

Applications that open `/dev/video*` directly find the virtual camera whenever the daemon runs. Applications that discover cameras through PipeWire (the camera portal, OBS's PipeWire source, browsers using PipeWire) depend on WirePlumber:
- WirePlumber probes video nodes when it starts. A v4l2loopback device into which no writer is streaming at that moment is recorded as output-only (`device.capabilities = ":video_output:"`). The device sends no udev event when a writer starts, so WirePlumber never probes it again and PipeWire lists no camera for it.
- The daemon requests its bus name only after its loopback writer has started, so a `Type=dbus` unit is ready when the device is already a capture device. The optional drop-in `dist/systemd/wireplumber.service.d/oma-broadcast.conf` orders WirePlumber after the daemon ([setup/daemon.md](setup/daemon.md)).
- Without the drop-in, or after the daemon was restarted by hand, restart WirePlumber once the daemon runs: `systemctl --user restart wireplumber`.
- Meanwhile `gst-device-monitor-1.0 -i Video/Source` (with `-i`, the hidden providers, including the plain v4l2 one) lists "Omarchy Broadcast" at its `/dev/videoN` path, while the default PipeWire provider does not.

### Known limitation: one application at a time

While one application streams from the virtual camera, a second one cannot open it: v4l2loopback refuses the second reader's format request (`S_FMT`, "Device or resource busy"). This was seen with a second `gst-launch-1.0 v4l2src` and `obc-consumer`; it is a property of the loopback device, not of the daemon.

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

## Manual test of the daemon

Prerequisites: the loopback of [setup/loopback.md](setup/loopback.md) and the build of the workspace. Run the daemon in the foreground, with its logs on the terminal, from the build directory (or install it as a service, [setup/daemon.md](setup/daemon.md)):

```sh
cargo run --release -p oma-broadcastd -- -v
```

1. **Idle.** In a second terminal (`oma-broadcast` is the installed CLI, or `cargo run --release -p oma-broadcast --`): `oma-broadcast status` shows `State: idle`; `v4l2-ctl -d /dev/video11 --all` shows a capture device with NV12 1920x1080; `fuser /dev/video0` shows no process holding the camera (the camera LED is off); `nvidia-smi` lists no process of the daemon.
2. **Follow it.** `oma-broadcast watch` prints the changes; `busctl --user monitor --match "type='signal',path='/br/com/dias/OmaBroadcast'"` shows the raw signals.
3. **Consumer.** Open the virtual camera in OBS or Chrome, or run `gst-launch-1.0 v4l2src device=/dev/video11 ! fakesink`. `watch` shows `consumers: 1`, `state: starting`, `state: active` (about 2.5 s) and one `stats:` line per second. The application shows the camera.
4. **Controls.** `oma-broadcast cameras`, `oma-broadcast camera next` and `oma-broadcast camera "Brio"` switch the camera under the open application, which keeps one stream with no black frame; `oma-broadcast off` closes the camera (LED off, application shows black) and `oma-broadcast on` restores it. The same through `busctl --user call br.com.dias.OmaBroadcast /br/com/dias/OmaBroadcast br.com.dias.OmaBroadcast1 SetEnabled b false` ([dbus-api.md](dbus-api.md)).
5. **Close the consumer.** After the grace period (5 s) `state: idle` is printed, the camera is released and `nvidia-smi` lists no process.
6. **Reload.** Change `idle_grace_s` in `~/.config/oma-broadcast/config.toml` and send `kill -HUP $(pidof oma-broadcastd)` (or `busctl ... ReloadConfig`); a typo in a key is rejected with `InvalidConfig` and the old value stays.
7. **Exit.** Ctrl-C stops the daemon; `oma-broadcast status` then exits with status 3 and says the daemon is not running.
