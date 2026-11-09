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

- **Phase lock.** A 60 fps camera into a 30 fps contract at input phases across the whole period (including both ends of the grid and input periods): after acquisition every frame waits at most the margin plus 0.5 ms. Ten simulated minutes per camera rate (59.99, 59.993, 60.01, 29.97, 30.03 and 30 fps) with +-1 ms of uniform arrival jitter: the count emitted in the 600 s is 18000 +- 1, every wall-clock 1 s window (sliding, at every position) holds 30 +- 1, every PTS is on the grid and every index is emitted once in order, nothing is skipped, and the median wait stays within the margin plus 1 ms. A camera at half the contract rate (15 fps into 30) engages the lock once a full fit window of its frames is held, with each fresh frame emitted on the next tick after the margin and the repeat on the tick between ([ADR-0010](adr/0010-phase-locked-output-clock.md)); an irregular ~20 fps camera engages neither the lock nor cadence mode and produces exactly the ticks of a clock without it. Cameras whose drift against the ticks exceeds the slew (29.5, 29.6, 29.75, 30.25 and 31 fps against 30 fps, 58 fps against 60; 0.5 and 1 ms of jitter, 120 s each) are let go within a second, with at most one acquisition and no wrap; from then on every tick matches the free-running clock fed the same arrivals at the held phase, and the superseded frames, repeats and median wait are no worse than the free-running clock's at that phase (at most one frame more, from the ticks before the drift was measured) and within its range over eight camera phases. Cameras just beyond the slow side's limit of half the slew (29.82, 29.85 and 29.9 fps; 1 and 2 ms of jitter, 120 s each) are let go within five seconds, for good, with at most one acquisition and no wrap; from then on every tick matches the free-running clock at the held phase, and the superseded frames and repeats are within eight frames of it (the ticks before the drift was measured: up to 7 frames more at 2 ms of jitter, none at 1 ms), the median wait within 0.1 ms. A new source, and a phase jump of the same source, re-acquire within a few fresh frames.
- **Cadence mode** ([ADR-0015](adr/0015-cadence-emission-for-sub-rate-inputs.md)), same file, on scripted time: `phase_lock_follows_a_steady_sub_rate_input_in_cadence_mode` (a steady sub-rate input on the 4 ms arrival grid engages it with one acquisition and no wrap, short slot waits, every PTS on the grid once and in order, 1 s windows of 29 to 31 frames); `cadence_mode_follows_a_camera_changing_its_rate_within_a_run` (30 to 20 fps within a run, history cleared); `cadence_mode_reacquires_after_a_switch`; `cadence_mode_rides_out_late_and_dropped_frames`; `cadence_mode_rides_out_late_wakeups`; `cadence_mode_bursts_stay_within_the_cap` (stalls of 33 to 166 ms never release more than the cap back to back); `cadence_mode_resynchronises_after_a_long_stall` (over 1 s); `cadence_mode_reacquires_after_the_input_stalls_to_the_placeholder`. No test pins the release threshold between `P/16` and `P/12`.
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
cargo test -p obc-ipc -p omarchy-broadcastd -p omarchy-broadcast
```

**`omarchy-broadcastd`** (`crates/omarchy-broadcastd`):
- The controller state machine runs against a fake media backend with explicit instants, so no test sleeps for a timer. It covers every transition of [ADR-0011](adr/0011-daemon-lifecycle-and-dbus-api.md): the debounce (a 100 ms open never starts the camera), the grace period (a consumer returning inside it keeps the session), `off` keeping the writer, persistence of `Enabled` and the selection across a restart, `SelectCamera` / `NextCamera` while idle and while streaming, the `NoSuchCamera` / `Unavailable` / `Failed` errors, fallback to the next camera, `error` and recovery, no pre-emption of a working stream, an invalid reload, an invalid config at start, deferral of an output-mode change while a consumer holds the device, a missing loopback retried every 2 s, a failing writer, the 10 s start timeout, every camera failing (the session stops rather than cycling), a damaged state file (daemon `off`, file untouched), `SetEnabled(false)` during a start, and `Stats` only while `active`.
- Parking ([ADR-0017](adr/0017-cuda-checkpoint-parking.md); see [Parking the GPU at Idle](#parking-the-gpu-at-idle)): unit tests of the `Parker` state machine through a test ops seam and of its ordering through a `StopSession` seam, and controller tests through the fake backend, which drives the real `Parker`.
- Consumer counting uses `sleep` children that hold a temporary file open: the daemon's own descriptor is excluded, counts rise and fall, inotify events trigger recounts, and the polling fallback works. Also covered: the inotify wrapper, hotplug debouncing, camera ids, ordering and selectors.
- The D-Bus object is served over a peer-to-peer socket pair and driven through `obc_ipc::OmarchyBroadcastProxy`: every property (including `GetAll` and the introspection data), every method and error name, the `PropertiesChanged` sequence, `Stats` while active and none for 2.5 s while idle.
- Process tests (`tests/process.rs`) run the binary on a private `dbus-daemon` (`/usr/bin/dbus-daemon` is required; without it the tests fail naming it): a second instance exits with status 3 before touching a device, no session bus gives status 1, `--help` and an unknown option behave.
- The live preview ([ADR-0019](adr/0019-live-preview.md); see [The live preview](#the-live-preview)): in the controller (fake backend, explicit instants, a temporary runtime directory), `preview.rs` and the D-Bus object, as listed there.

**`omarchy-broadcast`** (`crates/omarchy-broadcast/tests/cli.rs`, plus unit tests of the JSON and human formatting): the CLI runs against a mock daemon on a private `dbus-daemon` with its own configuration and an activation file whose command is `/bin/sh -c 'echo > marker; exit 1'`, so a failed activation is quick and visible. Both `/usr/bin/dbus-daemon` and `/bin/sh` are required. The tests cover `status`, `cameras`, `on`, `off` and `camera` in human and JSON form, exit codes (3 for a daemon that is not running or no session bus, with reads never activating the daemon), `watch --json` following changes, `Stats` and outages, waiting for the daemon to appear, a daemon that holds its name but does not answer, and `watch` exiting quietly when its reader (`| head -1`) goes away. The `preview` command is covered in [The live preview](#the-live-preview).

**`obc-ipc`** (`crates/obc-ipc/tests/mock_server.rs`): a mock server over a peer-to-peer connection checks that the proxy reads every property, calls every method, maps errors back by name, receives `Stats`, and that the no-auto-start read sees every property, carries the `NO_AUTO_START` flag at the server and reports a missing object as an error.

### End-to-end hardware test (ignored)

`crates/omarchy-broadcastd/tests/hardware.rs::the_daemon_runs_the_lifecycle_on_real_devices` runs the real daemon, a private `dbus-daemon`, temporary `XDG` directories and a grace period of 1 s, with `obc-consumer` as the application.

| Needs | |
|---|---|
| `OBC_TEST_SINK` | A free v4l2loopback device with `exclusive_caps=1`, for example `/dev/video11`. Required |
| `OBC_TEST_CAMERA`, `OBC_TEST_CAMERA_2` | Two different cameras, for the switching checks. Both required |
| GPU | An NVIDIA GPU and `nvidia-smi` |
| `/usr/bin/dbus-daemon`, `cargo` | The test builds `obc-consumer` (the `OBC_CONSUMER` variable names a prebuilt one) |

```sh
OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193 \
  cargo test -p omarchy-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture
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

**Latency sawtooth.** `latency_p50_ms` ([dbus-api.md](dbus-api.md)) is the time from the camera buffer leaving the source to its tick being handed to the output. The phase lock of [ADR-0010](adr/0010-phase-locked-output-clock.md) engages when the camera delivers at close to the contract rate, a multiple of it, or exactly half of it. In a dark room the cameras ran below it: the MX Brio at 15 fps (14.99 measured, with `exposure_dynamic_framerate` on) and the FHD Camera at 20 fps (19.98). Half the contract rate is locked (the 15 fps case), and a steady 16 to 28 fps input is followed by cadence emission ([ADR-0015](adr/0015-cadence-emission-for-sub-rate-inputs.md)): the FHD Camera at 20.07 fps, 120 s with a consumer, added p50 9.2 ms and p95 12.6 ms. Before ADR-0015, 20 fps into 30 was not followed: the output ticks drifted against the camera's frames, and the median latency sat wherever the phase was on a sawtooth, falling by about 0.8 to 1.2 ms per second from about 32 ms to 2.5 ms, then jumping back, with a period of about 42 s (observed values of 3 to 30 ms; free-running, 21.8 ms median at 20.06 fps). A camera in dim light may still show other values while its rate is changing or irregular. With `--camera-fixed-framerate` the Brio runs at 60 fps, the lock engages, and the added latency is about 4 ms (p50 4.0, p95 6.5 in `obc-bench`).

These figures were measured by hand on the stated machine; no test asserts them.

### Discovery by PipeWire

Applications that open `/dev/video*` directly find the virtual camera whenever the daemon runs. Applications that discover cameras through PipeWire (the camera portal, OBS's PipeWire source, browsers using PipeWire) depend on WirePlumber:
- WirePlumber probes video nodes when it starts. A v4l2loopback device into which no writer is streaming at that moment is recorded as output-only (`device.capabilities = ":video_output:"`). The device sends no udev event when a writer starts, so WirePlumber never probes it again and PipeWire lists no camera for it.
- The daemon requests its bus name only after its loopback writer has started, so a `Type=dbus` unit is ready when the device is already a capture device. The optional drop-in `dist/systemd/wireplumber.service.d/omarchy-broadcast.conf` orders WirePlumber after the daemon ([setup/daemon.md](setup/daemon.md)).
- Without the drop-in, or after the daemon was restarted by hand, restart WirePlumber once the daemon runs: `systemctl --user restart wireplumber`.
- Meanwhile `gst-device-monitor-1.0 -i Video/Source` (with `-i`, the hidden providers, including the plain v4l2 one) lists "Omarchy Broadcast" at its `/dev/videoN` path, while the default PipeWire provider does not.

### The PipeWire output

The optional PipeWire node ([ADR-0016](adr/0016-pipewire-output.md)) is compiled only with the cargo feature `pipewire`, so its tests run in a separate suite. The default `cargo test --workspace` does not build them. Building them needs the libpipewire headers and clang; the unit tests create a PipeWire context from its `client.conf` (package `pipewire`) but contact no PipeWire daemon.

```sh
cargo test -p obc-pipeline -p omarchy-broadcastd --features omarchy-broadcastd/pipewire
```

The continuous integration runs the same command, and clippy with the feature, as a second job; the default job is unchanged.

**Unit tests** (`crates/obc-pipeline/src/pipewire/`, and the controller, D-Bus and CLI tests of the other crates):
- the format pods and the negotiation: the offer order YUY2, NV12, I420 whatever the contract format (YUY2, I420 without `offer_nv12`, which also refuses an NV12 request), the exact size and rate, and the refusal of any other request;
- the buffer layouts, compared with GStreamer's `GstVideoInfo` default layout, and the CPU conversions into the PipeWire buffer;
- the link table behind the consumer count: links to the node are counted, links in the error state are not, and links to other nodes are ignored;
- failure and retry: with no PipeWire daemon the output is `failed` and retried, never puts the daemon in `error`, and events of a replaced output are discarded;
- the reporting of a panic in a PipeWire callback;
- in the controller: the lifecycle keyed off the total of loopback and PipeWire consumers, enabling and disabling on reload (immediately, or when the last PipeWire consumer leaves), no loopback rebuild for a PipeWire setting, and the rebuild of both outputs on an output-mode change;
- in `omarchy-broadcastd`, `omarchy-broadcast` and `obc-ipc`: the three revision 3 properties and `Consumers` as their total, an older daemon without them, the `Consumers:  2 (loopback 1, PipeWire 1)` and `PipeWire:` lines, and the three JSON fields with the 32-byte bound on `pipewire_output`.

**Live tests (ignored).** Both change no PipeWire or WirePlumber setting, and their consumers use `node.dont-fallback`, so a missing node never falls back to a real camera. A consumer whose link does not appear within 2 s is stopped and the test fails.

`crates/obc-pipeline/tests/pipewire_live.rs` needs the user PipeWire daemon (with WirePlumber) and `gst-plugin-pipewire` (`pipewiresrc`). It registers a temporary node, `omarchy-broadcast-test-<pid>`, fed by a loopback writer into a `fakesink` (no device), and checks that NV12 and YUY2 consumers negotiate and receive frames, and that RGBA and 640x480 consumers are refused without disturbing the node, its id or the other consumers.

```sh
cargo test -p obc-pipeline --features pipewire --test pipewire_live -- --ignored --nocapture --test-threads=1
```

`crates/omarchy-broadcastd/tests/pipewire.rs` runs the daemon on a private bus with `[pipewire] enabled = true`, a loopback, a camera and the GPU, with a `pipewiresrc` consumer that drives the lifecycle `starting` to `active` to `idle`, and checks the consumer counts and `PipeWireOutput` along the way. Before it starts the daemon it checks with `pw-dump` that no other node is called `omarchy-broadcast` (an installed daemon with the node enabled).

| Needs | |
|---|---|
| `OBC_TEST_SINK` | A free v4l2loopback device with `exclusive_caps=1`, for example `/dev/video11` |
| `OBC_TEST_CAMERA` | A colour camera |
| PipeWire | The user PipeWire daemon, `gst-plugin-pipewire`, `pw-dump` |
| GPU | An NVIDIA GPU |
| `/usr/bin/dbus-daemon` | For the private bus |

```sh
OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 \
  cargo test -p omarchy-broadcastd --features pipewire --test pipewire -- --ignored --nocapture
```

### The live preview

The preview ([ADR-0019](adr/0019-live-preview.md)) has no feature flag: its tests are in the default suite, and need no camera, no GPU and no loopback device (the encoder uses `videoscale`, `videoconvert` and `jpegenc` of gst-plugins-base and gst-plugins-good). The daemon's tests give each daemon a temporary `XDG_RUNTIME_DIR`, so they never touch the user's `$XDG_RUNTIME_DIR/omarchy-broadcast`.

**Encoder** (`crates/obc-pipeline/src/preview.rs`, on the CPU, with frames made in system memory):
- the size keeps the aspect: 480 wide, the height in proportion and even (480x270 for 1920x1080);
- the decimator by timestamp: at half the contract rate it keeps every second frame, and the writer's timing jitter does not skip one;
- the encoder writes decimated JPEGs of the right size, each by a rename (a reader never sees a partial file), reports an increasing sequence number per frame and removes both files when it stops; the decoded JPEG keeps a bright and a dark area of the test frame apart;
- a file in the way is replaced and a planted link is not followed (a symbolic link, a hard link; the link's target is untouched), and a planted FIFO neither blocks the encoder nor gets the frame.

**Backend** (`crates/omarchy-broadcastd/src/backend/real.rs`): the writer's composed tap feeds whichever of the PipeWire output and the preview runs, in order when both do; each part of the tap outlives the other (stopping one keeps the other's frames), and a new writer gets a new encoder, with sequence numbers that keep increasing.

**Daemon** (`crates/omarchy-broadcastd/src/preview.rs`, the controller tests and the D-Bus tests):
- the leases: only 15 and 30 are accepted, one lease per bus name at its latest rate, the highest rate of the holders, a release without a lease does nothing;
- the directory: created mode 0700, a loose mode tightened, stale files removed, a symbolic link or a file in its place refused (and nothing removed through the link), a missing or relative runtime directory refused;
- the controller: a lease alone starts the session after the debounce and its end parks it after the grace; leases add to the loopback and PipeWire consumers in `Consumers`; a bad rate is refused; a holder that leaves the bus loses its lease while the others keep theirs; a lease is granted while `off` and the placeholder is encoded without a camera; without a runtime directory it fails; a deferred writer rebuild waits for the leases and the new writer is previewed; shutdown ends every lease;
- the D-Bus object, on a private `dbus-daemon` (the lease needs the caller's unique name): the leases by bus name, `PreviewConsumers` and `PreviewFrame`; a caller that left before its lease was recorded is seen as gone, and one that sends `AcquirePreview` without waiting and leaves holds no lease however late the call is served; a peer-to-peer call, with no sender, and a missing runtime directory fail with `Failed`. In `obc-ipc`: the property in the snapshot, `0` from an older daemon, and the proxy's methods and signal against the mock.

**CLI** (`crates/omarchy-broadcast/tests/cli.rs` and the unit tests of `json.rs`, `human.rs` and `bus.rs`), against a mock daemon that records the leases by caller and the calls that lacked `NO_AUTO_START`:
- `preview --json` holds a lease, prints the `preview` line and the `frame` lines for the signals it receives, releases on `SIGTERM` and exits 0; the calls never start the daemon;
- `--fps 30` is asked for, and any other value is a usage error (status 2);
- through an outage: one `error` line, the lease asked for again when the daemon is back;
- against an older daemon: one `unsupported-daemon` line, and the command waits;
- it releases and exits quietly when its reader goes away, and prints `"active":false` before the error when the bus goes away;
- a frame path too long for a line is refused (`preview-path-too-long`, the lease released);
- the JSON lines: the exact `preview` and `frame` lines, a path at the bound carried whole and one byte more (or one whose escapes take it over the bound) refused, and `preview_consumers` in `snapshot` and `state`; the human `consumers` label with the preview leases.

**Live test (ignored)**, `crates/omarchy-broadcastd/tests/preview.rs::the_preview_streams_at_the_rate_asked_and_ends_with_its_holder`: the real daemon on a private `dbus-daemon`, a real loopback, and `omarchy-broadcast preview --json` in its own process (the test builds the CLI into the same target directory; `OBC_CLI=<path>` overrides it).

| Needs | |
|---|---|
| `OBC_TEST_SINK` | A free v4l2loopback device with `exclusive_caps=1`, for example `/dev/video11`. Required |
| `OBC_TEST_CAMERA` | Optional. A colour camera. Without it the daemon is switched off first, so the lease opens no camera and the preview carries the writer's black placeholders; with it the lease starts the camera and the preview carries its frames. This case also needs the GPU |
| `/usr/bin/dbus-daemon`, `cargo` | For the private bus and to build the CLI |

```sh
OBC_TEST_SINK=/dev/video11 [OBC_TEST_CAMERA=046d:0944] \
  cargo test -p omarchy-broadcastd --test preview -- --ignored --nocapture
```

It proves, at 15 and then at 30 frames per second: the lease is announced with the frame file's path and `PreviewConsumers` is 1; the lease starts the camera (`active`), or leaves the daemon `off` without a camera; the frames arrive at the rate asked, within 20 % over at least 5 s, with strictly increasing sequence numbers; the frame file is a 480x270 JPEG of mode 0600 in a directory of mode 0700; after `SIGTERM` the CLI exits 0, `PreviewConsumers` returns to 0 and the file is gone. After that, a `kill -9` of the CLI ends its lease through the bus (`PreviewConsumers` back to 0) and removes the file. The daemon's CPU use without a lease and with each lease, and the encoder's time per frame from the daemon's debug log, are printed, not checked.

### Known limitation: one application at a time

While one application streams from the virtual camera, a second one cannot open it: v4l2loopback refuses the second reader's format request (`S_FMT`, "Device or resource busy"). This was seen with a second `gst-launch-1.0 v4l2src` and `obc-consumer`; it is a property of the loopback device, not of the daemon.

## Video effects

The effects ([ADR-0012](adr/0012-video-effects-and-maxine-backend.md)) are tested in layers, so that everything except the real SDK runs without it.

### Default suite (no SDK, no GPU, no camera)

```sh
cargo test -p obc-core -p obc-gpu -p obc-backend -p obc-backend-maxine -p gst-obc \
  -p obc-pipeline -p obc-ipc -p omarchy-broadcastd -p omarchy-broadcast
```

To show that nothing in this run depends on the SDK, run it with the SDK variables removed:

```sh
env -u MAXINE_VFX_ROOT -u LD_LIBRARY_PATH -u LD_PRELOAD -u OMARCHY_BROADCAST_OS_RELEASE \
  cargo test -p obc-core -p obc-gpu -p obc-backend -p obc-backend-maxine -p gst-obc \
    -p obc-pipeline -p obc-ipc -p omarchy-broadcastd -p omarchy-broadcast
```

On the reference machine this passed with every hardware test ignored (the counts change with every release and are not recorded here), and `ldd` on `omarchy-broadcastd` and `obc-bench` shows no link to the SDK, CUDA or TensorRT libraries. What the default suite covers:

- **Backend seam** (`obc-backend`, `obc-gpu`, `obc-backend-maxine`): the trait types, the validation of load requests, the error texts, and a Maxine backend that reports itself unavailable with the reason when the SDK is not found.
- **The element** (`crates/gst-obc`): registration and caps (CUDA NV12 on both pads), properties and their validation, status messages on the bus, the background image (crop to fill, the size limit, a JPEG, an undecodable or missing file), and the refusal of hostile images in under half a second: a 50-byte PNG declaring 60000x60000 pixels and a JPEG declaring 65000x65000 are refused from their headers before a decoder runs; a file that is a FIFO is refused without blocking. The first-image rule is tested too: a valid image followed by a second one declaring a huge size is decoded as the first image only, data after the image (random bytes, an MP4 box, zero padding) is ignored, and a truncated image is refused.
- **The pipeline** (`obc-pipeline`): the effect settings and their checks, the effect statistics, that an output built without the element refuses an effect, and that an effect is refused with the roundtrip stage or with a system-memory output.
- **The daemon** (`omarchy-broadcastd`), against its fake backend: the `[effect]` configuration, the runtime state keys, the setters and their errors, persistence and precedence, the `Error` and `State` outcomes of an effect failure with `on_failure` `"black"` and `"none"`, the clearing of that error when the session ends, and a damaged state file.
- **Noise removal** ([ADR-0018](adr/0018-video-noise-removal.md)): the element with the fake backend (`crates/gst-obc/tests/element.rs`: `denoise` is a property of its own with a status of its own, and the new-source flag is a free video buffer flag); the output clock (`obc-pipeline`, `the_first_fresh_frame_of_each_source_is_flagged_down_to_the_sink`); the daemon's `[effect] denoise` configuration, the `denoise` state key (a non-boolean value is a damaged state file), `SetDenoise` and its persistence, the fail-open outcome and the `Error` precedence (`controller/tests.rs`, `dbus/tests.rs`); and the CLI's `denoise on|off`, the revision-4 message and the `null` JSON field (`tests/cli.rs`, `json.rs`, `human.rs`).
- **The CLI and the IPC crate**: `effect` and its option rules, the revision-1 daemon message, the JSON fields and their bounds, the escaping of daemon strings in the human output, and the proxy against a mock server.

### GPU tests with the fake backend (`gpu_` prefix)

These need an NVIDIA GPU and the GStreamer CUDA elements, but no SDK and no camera: a fake `InferenceBackend` (a test double in `crates/gst-obc/tests/support/fake_backend.rs`) marks the pixels it processes, so the tests can tell a processed frame from a raw one by value.

```sh
cargo test -p gst-obc -p obc-pipeline -- --ignored gpu_ --test-threads=1
```

They also check the noise-removal stage with the fake backend: `none` with noise removal passes the input through until the stage is ready and then shows only processed frames; a failed load or a failed frame drops the stage and keeps the effect (or the input, with `none`) with the reason reported; an effect that fails again without the stage takes the effect's failure path; the green screen from `none` with noise removal loads black without blocking; and the new-source flag resets the temporal state once. They check that `none` passes the input buffer through untouched and touches no CUDA; that going from `none` to an effect shows only black until the effect is ready, and never the raw input; that a switch from one effect to another shows only processed frames; that each failure (backend missing, load, frame, background image) shows black or the input as `on_failure` says; that repeated pushes of one buffer leave it unmodified; that every stop releases the CUDA context exactly once; and, through a whole output (`gpu_an_output_switches_effects_while_streaming`), that a stream switching effects keeps its frame rate and negotiates its caps once.

### Maxine hardware tests

These load the real SDK. They need `MAXINE_VFX_ROOT` and the SDK's library directories on `LD_LIBRARY_PATH` ([setup/maxine.md](setup/maxine.md), steps 6 to 8), and, on a distribution the SDK does not list, the os-release shim (step 7). Set `LD_PRELOAD` through the cargo runner, so that only the test binaries load the shim; `LD_PRELOAD=... cargo test` would be dropped by cargo:

```sh
make -C tools/os-release-shim
source tools/maxine-probe/env.sh        # or your env.sh; sets MAXINE_VFX_ROOT and LD_LIBRARY_PATH
export OMARCHY_BROADCAST_OS_RELEASE=$PWD/tools/maxine-probe/fake-os-release
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$PWD/target/os-release-shim/os-release-shim.so"

cargo test -p obc-gpu -p obc-backend-maxine -p gst-obc -- --ignored --test-threads=1
OBC_TEST_CAMERA=046d:0944 OBC_TEST_SINK=/dev/video11 \
  cargo test -p obc-pipeline --test hardware -- --ignored maxine_ --test-threads=1
```

- `obc-backend-maxine` (13 tests in `tests/hardware.rs`; the 8 `maxine_denoise_` tests are described under [Video noise removal](#video-noise-removal)): the backend refuses calls without the CUDA context or a load; blur and replace in a context created like GStreamer's, with per-stage GPU times, load times and GPU memory logged. The SDK works in the pushed context: afterwards the device's primary context is inactive. The mask hardening has three more (see [The hardened mask](#the-hardened-mask)).
- `gst-obc` (16 tests in `tests/hardware.rs`, including `gpu_` ones and `maxine_denoise_alone_under_blur_and_toggled_while_playing`) and `obc-gpu` (3 tests, plus the four `checkpoint_` tests of [Parking the GPU at Idle](#parking-the-gpu-at-idle)): the element against the real backend, and the driver helpers and the mask kernel. `gst-obc` has `obc-backend-maxine` as a dev-dependency for `maxine_mask_kernel_failures_show_black_with_the_reason`, which needs the Maxine environment above.
- `maxine_effects_end_to_end_keep_the_rate_and_release_the_gpu` (`obc-pipeline`): camera, session and output with `blur`, then `replace`, then `none`; each phase keeps the contract rate at the sink, the effect becomes ready, its statistics are sane, and after the output stops the process holds no CUDA context.

The frames in these tests are synthetic, so the mask the model produces for them is not predictable; the tests assert what holds for any mask and log the rest.

### Video noise removal

The stage is tested at each level described above; this section lists what needs the SDK and its `nvvfxdenoising` feature and models ([setup/maxine.md](setup/maxine.md), steps 5 and 6; `features/nvvfxdenoising/lib` must be on `LD_LIBRARY_PATH`).

- **Backend** (`crates/obc-backend-maxine/tests/hardware.rs`, run with the command of [Maxine hardware tests](#maxine-hardware-tests)): `maxine_denoise_alone_removes_noise_from_a_flat_frame`, `maxine_denoise_switches_on_and_off_on_a_loaded_chain`, `maxine_denoise_before_blur_and_replace_gives_sane_output`, `maxine_denoise_reset_temporal_starts_a_new_video`, `maxine_denoise_strength_0_and_1_both_load_and_run`, `maxine_denoise_gpu_memory_returns_after_unload`, `maxine_denoise_faults_degrade_to_the_chain_without_it` (fault injection for the load and the run) and `maxine_denoise_loads_again_after_the_gpu_was_parked` (park and unpark, in its own process).
- **Element** (`crates/gst-obc/tests/hardware.rs`): `maxine_denoise_alone_under_blur_and_toggled_while_playing`.
- **Daemon** (`crates/omarchy-broadcastd/tests/hardware.rs`): `denoise_stacks_with_the_effect_while_streaming_and_the_gpu_parks` streams `none` with noise removal, adds `blur`, toggles, stops and checks that the GPU parks. It needs `OBC_TEST_SINK`, `OBC_TEST_CAMERA`, `OBC_TEST_PRELOAD` (with `OMARCHY_BROADCAST_OS_RELEASE`) and the Maxine environment, as the daemon effects test below, and a driver with the CUDA checkpoint API.
- **Recorded frames** (`crates/obc-backend-maxine/tests/denoise_frames.rs`, ignored): runs the stage at both strengths over frames recorded from a real camera and writes the camera frame and each result as PPM images for comparison by eye, with the spatial and temporal noise logged. `OBC_DENOISE_FRAMES` names the directory of recorded sequences and `OBC_DENOISE_OUT` the directory for the images; the test fails without them.

  ```sh
  OBC_DENOISE_FRAMES=$HOME/frames OBC_DENOISE_OUT=$HOME/denoise-out \
    cargo test -p obc-backend-maxine --test denoise_frames -- --ignored --nocapture
  ```

  with the Maxine environment and the shim runner of the section above.
- **The strength knob.** The daemon runs the stage at strength 1 and has no setting. `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process under test (a backend test, `obc-bench`, the daemon) overrides it, to compare the two models. It is for development only.
- **`obc-bench`.** `--denoise` requests the stage, with `--effect none|blur|replace` (`--effect none` for the stage alone; `--denoise` without `--effect` is refused). The JSON effect section gains `denoise`, `denoise_status`, `denoise_reason`, `frames_denoised` and `denoise_ms_p50` / `denoise_ms_p95`. Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce them locally with these.

### The hardened mask

The mask hardening ([ADR-0013](adr/0013-mask-hardening-cuda-kernel.md)) is tested at four levels.

Default suite, no GPU:

- `obc-backend-maxine`, `src/matte.rs`: the table's exact entries (`the_fixed_ramp_has_the_documented_entries`), that it is monotone and equals a floating-point reference for all 256 values, that a ramp that does not rise is refused, and the host reference `apply_lut` on pitched and odd-sized planes (padding untouched).
- `obc-gpu`, `src/kernel.rs`: `the_embedded_ptx_is_pinned_to_isa_7_0_and_sm_75`, which fails if the PTX is regenerated with a newer nvcc (a newer `.version` would silently raise the driver floor), the single NUL-terminated string, the seven-parameter entry point, the block size, and that the load errors for CUDA codes 222 and 221 say what to fix.
- `obc-backend-maxine`, `src/lib.rs`: the fault switch is off unless armed from Rust, and a kernel setup failure fails the load.

```sh
cargo test -p obc-gpu -- --ignored ptxas
```

`ptxas_assembles_the_committed_ptx_for_sm_75_and_sm_120` (ignored, needs the CUDA toolkit's `ptxas` on the path) assembles the committed PTX for both architectures.

GPU tests (need an NVIDIA GPU and, for the last two rows, the Maxine environment of the previous section; run with `--ignored --test-threads=1`):

| Test | What it shows |
|---|---|
| `obc-gpu`: `gpu_lut8_matches_the_host_table_bit_for_bit` | The kernel equals the host table at 1920x1080 and at 513x289 with a pitch larger than the width, for an identity, the hardening and a permutation table; padding bytes keep their sentinel value |
| `obc-gpu`: `gpu_lut8_failures_are_errors_and_leave_the_context_usable` | Invalid PTX and a bad launch are errors, and the context works afterwards |
| `obc-backend-maxine`: `maxine_the_effects_read_the_hardened_mask_not_the_raw_one` | The hardened mask is the table applied to the raw mask; where the raw mask is below 128 the output is the background in `replace`, and where the hardened mask is 255 the output is the camera frame; the counts are logged so a mask with no soft band is visible |
| `obc-backend-maxine`: `maxine_a_mask_kernel_that_does_not_load_fails_the_load`, `maxine_a_mask_kernel_that_does_not_launch_fails_the_frame` | A forced kernel failure is a load error or a frame error, and never produces a frame from the raw mask |
| `gst-obc`: `maxine_mask_kernel_failures_show_black_with_the_reason` | Through the element, a kernel setup, launch or asynchronous failure shows black with the reason in the status, and no frame carries the raw mask |

The synthetic frames of these tests contain no person, so the model's soft edge is not exercised. `crates/obc-backend-maxine/tests/mask_frames.rs` does it on recorded camera frames:

```sh
OBC_MASK_FRAMES=/path/to/frames \
  cargo test -p obc-backend-maxine --test mask_frames -- --ignored --test-threads=1 --nocapture
```

`the_hardened_mask_hides_the_room_on_recorded_camera_frames` fails if `OBC_MASK_FRAMES` is unset. The directory holds one subdirectory per camera sequence, each with consecutive packed NV12 1920x1080 frames named `*.nv12` (limited-range BT.601, a person in frame, about 40 frames); the module documentation of the test has the layout and a `gst-launch-1.0` recording command. The frames are local and are never committed. Each sequence is replayed through `replace` on a solid background, and on every frame the test checks that the hardened mask is the table applied to the raw one, that no pixel the model rates below 128 shows anything but the background, and that the partly mixed edge band is at most half as wide in pixels as with the raw mask, in the mask and in the output. It also checks that the frames are able to tell the two masks apart (some background-side pixels would have shown the room with the raw mask), and logs the edge flicker for reference.

### Parking the GPU at Idle

[ADR-0017](adr/0017-cuda-checkpoint-parking.md). The default suite needs no GPU.

- **`obc-gpu`** (`src/checkpoint.rs`): a missing library and a driver without the symbols make parking unsupported; the lock arguments' struct layout, the timeout clamp, the mapping of process states, the guard that refuses while parked and counts refusals, and the call sequences against a fake driver (park then unpark in order, a lock timeout leaves the process running, a failed checkpoint unlocks again, a failed unlock stays parked until the next unpark, a failed restore can be retried, an unsupported call fails that park only, unpark follows the driver's state, and the flag of a process parked by another tool is set before its restore or unlock runs, so a failed one still refuses CUDA). The process-wide parked flag is tested in a child process.
- **`omarchy-broadcastd`**: `backend/park.rs` tests the `Parker` (logging with durations, a failed park, an unsupported driver said once, a failed unpark, a failed start parks again, the unlock after a blocked park, refusals warned once per park, stop ordering, and the taint paths through the seams: an unclean session stop, restart, failed start or camera switch disables parking and a later clean stop does not clear it, a stop that failed with every pipeline in NULL still parks with a warning, a tainted parker still unparks). `backend/real.rs` has pure-function tests of the clean/unclean mapping of stop results, and `obc-pipeline` session tests cover a camera switch whose stop is unclean. The controller tests (`controller/tests.rs`, fake backend with the real `Parker`) cover: Active to Idle parks; a restart unparks before the build; an unpark failure gives `error` with no session and the writer kept; a retry works once the failure clears; `off` after an activation stays parked; a park failure keeps working; a failed first start parks nothing; an unclean stop does not park, and neither does a later clean stop; a failed start whose cleanup was unclean does not park again.

Ignored hardware tests, all needing an NVIDIA GPU with a driver that has the CUDA checkpoint API:

```sh
# obc-gpu: never initialised means nothing to park; a live context's data survives 3 cycles;
# a process checkpointed by another tool is restored by unpark; a parked process exits promptly
cargo test -p obc-gpu --test hardware -- --ignored --test-threads=1 checkpoint_

# daemon backend, no camera: videotestsrc, a loopback and 3 cycles
OBC_TEST_SINK=/dev/video11 \
  cargo test -p omarchy-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture \
  the_backend_parks_the_gpu_between_sessions_without_a_camera

# the daemon: needs a camera and a free loopback
OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 \
  cargo test -p omarchy-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture \
  the_daemon_parks_the_gpu_when_idle
```

`the_daemon_parks_the_gpu_when_idle` runs three sessions (four with a blur session when the Maxine environment of the effects test below is set). After each park it checks 0 `/dev/nvidia*` fds and no mappings, D-Bus queries answered promptly while parked, fresh frames after each unpark, and a prompt `SIGTERM` while parked. The `obc-gpu` tests run the checkpoint body in a child process, since CUDA's state is the whole process's. The check that the GPU is missing from `/proc/driver/nvidia-uvm/gpus/` is system-wide, so it is skipped when another process holds `/dev/nvidia-uvm`.

### Daemon effects test (ignored)

`crates/omarchy-broadcastd/tests/hardware.rs::effects_switch_while_streaming_and_release_the_gpu` runs the real daemon like the lifecycle test above, and switches `blur`, `replace`, `none` and `blur` again while a consumer in another process reads the virtual camera. It checks the rate and the single caps negotiation at the consumer, the readiness time of each effect, the daemon's GPU memory in each phase, and that the GPU is released within seconds of the last consumer leaving. The daemon is a child process of the test, so the shim is passed to it explicitly, with `OBC_TEST_PRELOAD`, instead of through the cargo runner:

```sh
source tools/maxine-probe/env.sh
OMARCHY_BROADCAST_OS_RELEASE=$PWD/tools/maxine-probe/fake-os-release \
OBC_TEST_PRELOAD=$PWD/target/os-release-shim/os-release-shim.so \
OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193 \
  cargo test -p omarchy-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture
```

Without `OBC_TEST_PRELOAD` the effects test fails with the daemon's `Error` text (`NvVFX_Load(GreenScreen) returned -1209 ...`), which shows that the variable is what makes it pass. With `--nocapture` the test prints, for each phase, the camera's fresh frame rate and whether the output clock was phase locked (the daemon logs this at debug level with `-v`).

### The shim's own test

```sh
make -C tools/os-release-shim test
```

builds the library and runs `tools/os-release-shim/test.sh`: every interposed entry point, with and without the shim; that only a read-only open of exactly `/etc/os-release` is redirected; that ordinary programs (`cat`, `grep`, `python3`) and child processes are unaffected; that `LD_PRELOAD` is removed from the process environment; and that the process stays in the user's own namespaces. The test fails against a library that redirects nothing, so it cannot pass vacuously.

### Measured on the reference machine

Daemon on the real deployment path (a systemd transient unit with the drop-in's environment, no namespace), MX Brio NV12 1080p60, fixed frame rate, output NV12 1080p30, an application reading the virtual camera as a separate process.

| Measure | Result |
|---|---|
| Consumer detection | the application's open is seen 7 ms after it happens |
| Effect switching, `none`, `blur`, `replace`, `none`, `blur` | output stays on the configured contract in every phase; one caps negotiation for the whole run, switches included (performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence) |
| Idle | last application closes: camera and GPU stopped after the 5 s grace and 436 ms; GPU memory back to the pre-start value |
| Effect failure, daemon without the shim | `State` `error`, `Error` with the SDK's reason, 90 of 90 frames at the application black (luma 16) while the camera delivered a real image |
| Frames of the two reference cameras, `none`, `blur`, `replace` | 114 captured frames, none dark or blank |

Latency, CPU, GPU utilisation and memory, and model load times with an effect were measured as well. Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce locally with `obc-bench`. The rows above were measured by hand on the stated machine; no test asserts them.

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
cargo run --release -p omarchy-broadcastd -- -v
```

1. **Idle.** In a second terminal (`omarchy-broadcast` is the installed CLI, or `cargo run --release -p omarchy-broadcast --`): `omarchy-broadcast status` shows `State: idle`; `v4l2-ctl -d /dev/video11 --all` shows a capture device with NV12 1920x1080; `fuser /dev/video0` shows no process holding the camera (the camera LED is off); `nvidia-smi` lists no process of the daemon.
2. **Follow it.** `omarchy-broadcast watch` prints the changes; `busctl --user monitor --match "type='signal',path='/br/com/dias/OmarchyBroadcast'"` shows the raw signals.
3. **Consumer.** Open the virtual camera in OBS or Chrome, or run `gst-launch-1.0 v4l2src device=/dev/video11 ! fakesink`. `watch` shows `consumers: 1`, `state: starting`, `state: active` (about 2.5 s) and one `stats:` line per second. The application shows the camera.
4. **Controls.** `omarchy-broadcast cameras`, `omarchy-broadcast camera next` and `omarchy-broadcast camera "Brio"` switch the camera under the open application, which keeps one stream with no black frame; `omarchy-broadcast off` closes the camera (LED off, application shows black) and `omarchy-broadcast on` restores it. The same through `busctl --user call br.com.dias.OmarchyBroadcast /br/com/dias/OmarchyBroadcast br.com.dias.OmarchyBroadcast1 SetEnabled b false` ([dbus-api.md](dbus-api.md)).
5. **Close the consumer.** After the grace period (5 s) `state: idle` is printed, the camera is released and `nvidia-smi` lists no process.
6. **Reload.** Change `idle_grace_s` in `~/.config/omarchy-broadcast/config.toml` and send `kill -HUP $(pidof omarchy-broadcastd)` (or `busctl ... ReloadConfig`); a typo in a key is rejected with `InvalidConfig` and the old value stays.
7. **Exit.** Ctrl-C stops the daemon; `omarchy-broadcast status` then exits with status 3 and says the daemon is not running.
