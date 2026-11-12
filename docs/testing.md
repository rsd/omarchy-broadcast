# Testing

## Unit tests

```sh
cargo test --workspace
```

The run includes the tests of the module system ([Modules](#modules)). Some of them run `cargo build --locked` on the example module and on a fixture module, which are outside the workspace, so the first run also compiles those.

The default run needs no camera and no GPU, but it needs GStreamer (1.28 or later) with its core elements, gst-plugins-base and gst-plugins-good: tests build pipelines from elements such as `videotestsrc`, `appsrc`, `appsink`, `fakesink`, `videoconvert`, `videoconvertscale`, `videocrop` and `capssetter`, and look up the `v4l2src` factory. A test that runs without what it needs fails, with a message naming the missing element or setting; no test passes without running.

Tests that need more than that are ignored by default, each with the reason (shown in the test listing), and run with `--ignored`:

- **GPU only** (`gpu_*`): an NVIDIA GPU and the GStreamer CUDA elements (`cudaupload`, `cudaconvertscale`, `cudaconvert`, `cudadownload`, from the nvcodec plugin of gst-plugins-bad, which registers them only where the GPU and its driver are present). No camera, no loopback. Ten unit tests in the library (the CUDA ingest and output pipelines as built, the roundtrip Process stage, the enhance stage's fallbacks) and seven hardware tests. (Three more `gpu_` tests, in `crates/obc-backend-maxine/tests/hardware.rs`, run the real SDK and are listed under [Maxine hardware tests, the enhancement method and the chair](#maxine-hardware-tests-the-enhancement-method-and-the-chair); the workspace-wide filter reaches them too and they fail, naming what is missing, without the Maxine environment.) The `gpu_` filter selects these; they run one at a time, since the hardware ones are timing tests sharing one GPU:

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

**The crash cover** (`crates/obc-pipeline/src/loopback_cover.rs`, no device) has unit tests for: the `timeout` formula (at least 250 ms, above the writer's worst gap by the margin, capped at the control's maximum, refused when the maximum is not positive); the black fill (luma, then chroma up to `sizeimage`; a fill that does not fit is refused); finding the loopback's controls by name; a missing node failing the cover; the slot that keeps the outcome of the current start; and taking back a pending `timeout_image_io` mode, also after a failure following the button press. The cover itself, against a real driver, is tested by the hardware tests below.

**The loopback writer** ([ADR-0011](adr/0011-daemon-lifecycle-and-dbus-api.md)) is tested on the CPU in `crates/obc-pipeline/src/loopback.rs`, with a `fakesink` or a system-memory session in place of the loopback and the CUDA elements:
- `placeholder_timing_rules`: when a placeholder is due (two contract periods after the last forwarded frame) and when it is not.
- `an_image_asked_for_again_after_black_is_shown_at_once`: the image last decoded is shown at once when the same file is asked for again after black, with no decode and never black meanwhile; another image asked for in between, or a file replaced at the same path, is read afresh, and a failed decode is not kept.
- `the_black_frame_fills_the_contract_layout`: NV12 and I420, at 1920x1080 and 642x480: the whole frame is covered, luma 16, chroma 128, with the layout `GstVideoInfo` gives.
- `the_writer_pipeline_has_the_contract_caps_and_no_cuda_element`: the `appsrc` carries the contract caps, is live, holds at most 3 buffers and does not block; the `v4l2sink` is `sync=false`; no `nvcodec` or CUDA element is in the pipeline; a full-range contract is refused for a loopback and accepted for a `fakesink`.
- `alone_the_writer_sends_black_frames_at_the_contract_rate`: NV12 and I420 320x240@30 into a `fakesink`: one caps event, 30 black whole frames sharing one memory, the k-th timestamp at least k periods, timestamps strictly increasing and, since [ADR-0022](adr/0022-crash-recovery-for-consumers.md), `CLOCK_MONOTONIC` times of the push (between the clock reads taken before the start and after the stop), and no crash cover on a `fakesink`.
- `a_new_writer_continues_the_timeline_of_the_previous_one`: two writers one after the other (a new daemon process in effect): the second one's first timestamp is after the first one's last, not back at 0.
- `a_forwarded_buffer_is_restamped_without_a_copy`: the sink sees the same memory address as the producer's buffer; the producer's buffer keeps its own timestamp; pushes before start and after stop are refused and counted.
- `a_writer_output_ends_in_an_appsink_with_the_writer_contract`: an output built for the writer ends in an `appsink` named `loopback`; a contract different from the writer's is refused.
- `the_writer_hands_over_between_placeholder_and_output_without_renegotiating`: placeholders, then a session forwarding a live pattern, then the session stopped. The sink sees placeholder, forwarded, placeholder runs with no placeholder among the forwarded frames, the output's caps equal the writer's, one caps event, strictly increasing timestamps, and placeholders resuming no sooner than two periods after the last forwarded frame. The interleaved count and the resume time (66.7 ms measured) are printed, not asserted, so a loaded machine does not fail the test.

`live.rs` tests the statistics behind `Stats` (`a_window_counts_ticks_and_takes_fresh_latencies`, `memory_is_bounded`, `live_stats_follow_a_cpu_stream`: a live pattern held 100 ms by a queue gives a p50 of at least 100 ms less one capture period, and the window's ticks and fresh counts sum to the output's). Ingest failures are tested on the CPU too: `a_failing_ingest_is_reported_and_a_healthy_one_is_not` (a streaming error and an end of stream are reported once, a healthy source and no source report nothing, the output keeps running) and `a_vanished_camera_node_is_reported`.

The phase lock's defaults come from a parameter sweep over simulated cameras with drift and 0.25, 1 and 2 ms of arrival jitter; it is an ignored test that prints its table:

```sh
cargo test --release -p obc-pipeline --lib phase_lock_tuning_sweep -- --ignored --nocapture
```

The camera settings ([ADR-0023](adr/0023-settings-schema.md); see [Camera settings](#camera-settings)) add device-level unit tests to `obc-pipeline`, with no camera: `control_node.rs` (the V4L2 ioctl numbers and struct sizes; names normalised like `v4l2-ctl`; the never-exposed list and its name rule; ids carrying query flag bits and ids outside the user and camera classes refused before any ioctl; enumeration keeping the exposed classes and types, a failing walk keeping what it found, a walk that does not advance ending, menu ranges bounded; the capture format as the first three words of `VIDIOC_G_FMT`), `ctrl_events.rs` (the event ioctl numbers, an event decoded, a watcher without a subscription ending at once and saying so, a running watcher stopping at once without a notice, a node that hangs up ending the watcher once, a panicking callback reported as an end, a missing node an error) and `xu.rs` (the `UVCIOC_CTRL_QUERY` number and the 16-byte struct, the field-of-view row, rows looked up by USB id, GUIDs in descriptor byte order, only the active configuration walked, the unit found by GUID inside VideoControl only, so that a VideoStreaming descriptor with the same subtype never matches, malformed descriptors ending the walk, the allowlist admitting only a row's unit and selector and only the five query codes, queries refused before the ioctl, the device's state mapped through the options, and the node found in sysfs by its device number and not its name, over a fake sysfs tree).

## Hardware tests

These run the real pipelines and are ignored by default (28 tests in `crates/obc-pipeline/tests/hardware.rs`), each with the reason naming what it needs. Which hardware a test needs:

| Tests | Need |
|---|---|
| `gpu_*` (9: crop and letterbox geometry read back from the GPU, stalled and slow sources, a dead output clock, a failed switch whose hold times out, a switch held across a slow-starting source, a slow sink behind the real output, and the two of [Video effects](#video-effects) and [Modules](#modules): effects switched while streaming, module stages around the effect element) | An NVIDIA GPU and the GStreamer CUDA elements. No variable, no camera, no loopback |
| `output_restarts_while_a_consumer_holds_the_loopback` | A free v4l2loopback device named by `OBC_TEST_SINK`, and the GPU. No camera. It fails unless `OBC_TEST_SINK` is set, so it never writes to a device that merely carries the default label |
| `the_writer_alone_is_a_capture_device_with_black_frames` | A free v4l2loopback device with `exclusive_caps=1` and no other writer, named by `OBC_TEST_SINK`, with the timeout controls (v4l2loopback 0.15.4 has them). No camera, no GPU. It sets `timeout` on the device and puts it back |
| `a_consumer_keeps_one_stream_across_idle_active_idle` | The camera, the GPU, `nvidia-smi` and a free loopback named by `OBC_TEST_SINK` (required) |
| `selects_test_camera_by_both_routes`, `control_guard_restores_on_drop_and_unwind` | The camera named by `OBC_TEST_CAMERA`, not streaming. The control test needs its `exposure_dynamic_framerate` control writable |
| `image_controls_query_write_and_read_back` | The camera named by `OBC_TEST_CAMERA`, not streaming, exposing at least one of the image controls. No GPU, no loopback. It changes the camera's image controls and writes every default back at the end, also when it fails (see [Camera image controls](#camera-image-controls)) |
| `settings_hw_*` (5: `enumerates_every_control`, `auto_exposure_flips_the_exposure_time_inactive_flag`, `a_slider_writes_and_reads_back`, `field_of_view_while_streaming`, `a_control_change_raises_an_event`) | The camera named by `OBC_TEST_CAMERA`, attached and free. No GPU, no loopback, no variable but the camera. Each puts every control it touches back to the value it read at its start, also when it fails, and never resets the whole camera. The checks of specific controls run only on an MX Brio (`046d:0944`); on another camera only the generic checks run (see [Camera settings](#camera-settings)) |
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

`the_writer_alone_is_a_capture_device_with_black_frames` asserts that the device is not a capture device before the writer starts and is one (`VIDIOC_QUERYCAP`) once it streams; that a consumer in another process receives at least 60 black frames with one caps negotiation (NV12 1920x1080 30/1); that the writer's sink sees one caps event and placeholders only; that the device is no capture device again after the writer stops; and that the `nvcodec` plugin was not loaded by the writer. It also checks the crash cover ([ADR-0022](adr/0022-crash-recovery-for-consumers.md)): the cover is ready with `timeout` 250 ms, the device's `timeout` control reads 250, and no timeout-image mode is left pending (a pending one is taken back, and the test presses the button once to check that the clearing works). The test restores `keep_format`, `sustain_framerate` and `timeout` to their earlier values when it ends, also when it fails.

`a_consumer_keeps_one_stream_across_idle_active_idle` runs the daemon's hand-over without the daemon: writer placeholders, then a CUDA session whose output forwards to the writer, then the session stopped. A consumer in another process must see placeholder, forwarded, placeholder runs with black placeholders; one caps negotiation at the consumer and at the writer's sink; a longest silence of at most three periods plus 30 ms at both; no writer placeholder while frames are forwarded; fresh camera frames; the process listed by `nvidia-smi` while the session runs and not within 5 s of its stop; the camera descriptor closed after the stop; and no failed pushes. It changes no camera control, and prints only the picture content it saw.

The consumer is the helper binary `obc-consumer` (`crates/obc-pipeline/src/bin/obc-consumer.rs`), built with the crate. It reads the device as a separate process with `v4l2src io-mode=mmap ! video/x-raw ! appsink` and prints one line per event (`caps`, `frame` with the mean, minimum and maximum luma of a row and the frame's stamp, `error`, `eos`); it exits when its standard input closes.

On the reference machine the 19 earlier hardware tests pass with `OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193` (a Logitech MX Brio and a laptop-integrated "FHD Camera"), and the earlier 17 also pass on each camera alone, the Brio also with `OBC_TEST_MODE=NV12:640x480@30`. `image_controls_query_write_and_read_back` and the five `settings_hw_` tests pass on the Brio alone (`OBC_TEST_CAMERA=046d:0944`).

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
- The camera image controls ([ADR-0020](adr/0020-camera-image-controls.md); see [Camera image controls](#camera-image-controls)): in `obc-core` (the state file), `obc-pipeline` (the allowlist and the range and step check), the controller (a fake device behind the backend seam), the D-Bus object, `obc-ipc` and the CLI.
- The camera settings ([ADR-0023](adr/0023-settings-schema.md); see [Camera settings](#camera-settings)): in `obc-core` (the state file), `obc-pipeline` (the device-level tests above), `obc-ipc` (the typed model and its codec), the settings model (pure, over fake control sets), the controller (the same fake device), the D-Bus object and the CLI.

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

### Consumer across a daemon crash (ignored)

`crates/omarchy-broadcastd/tests/crash.rs::a_consumer_keeps_receiving_black_frames_across_a_daemon_crash` ([ADR-0022](adr/0022-crash-recovery-for-consumers.md)) runs the real `omarchy-broadcastd` binary on a private `dbus-daemon`, with the loopback named by `OBC_TEST_SINK` and a raw V4L2 reader of that node in the test process. No camera and no GPU: the daemon starts `off` (its state file says so), so it never opens a session, and its writer sends black placeholders. The test kills the daemon with SIGKILL, leaves the loopback without a writer for 1.5 s, and starts a new daemon.

| Needs | |
|---|---|
| `OBC_TEST_SINK` | A free v4l2loopback device with `exclusive_caps=1` and the timeout controls (v4l2loopback 0.15.4 has them), for example `/dev/video11`. Required; the test refuses a device another process holds |
| `/usr/bin/dbus-daemon` | For the private bus |

```sh
OBC_TEST_SINK=/dev/video11 cargo test -p omarchy-broadcastd --test crash -- --ignored --nocapture --test-threads=1
```

It asserts that the reader gets no error and never goes without a frame for longer than 600 ms (Chromium restarts its capture after about a second); that the frames are the first daemon's, then black ones from the timeout image (Y=16 and chroma 128, not the driver's zero-filled green), then the second daemon's, in that order, so the driver's timer never fires while a writer runs; that the buffer timestamps never go back across the kill and the restart and the new writer's are `CLOCK_MONOTONIC` times; and that each daemon's `LoopbackConsumers` leaves out the daemon's own descriptors (the writer's and the cover's). With `--nocapture` it prints the outage as the reader saw it.

The test reads the loopback's `keep_format`, `sustain_framerate` and `timeout` controls before it starts and puts them back at the end, also when it fails, because the daemon's cover sets `timeout` and never resets it. `timeout_image_io` is a button with no value to restore, but pressing it leaves a device-wide timeout-image mode pending until a descriptor takes the image, which the cover does at once; if the cover failed and left it pending, the test takes it back. The other daemon hardware tests that start the daemon on a loopback leave `timeout` set on the device (it persists until the module is reloaded); this one and the writer-alone test of [Hardware tests](#hardware-tests) restore it.

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

### Camera image controls

The image controls ([ADR-0020](adr/0020-camera-image-controls.md)) have no feature flag: the default suite covers them with no camera, no GPU and no loopback device. The controller tests use a fake device behind the backend seam (`ControlNode` in the fake backend): each fake camera has its own controls with ranges, steps and values, and the fake can refuse a write, fail to open a node, leave a control unreadable, or round a written value, and it counts the nodes opened, the queries and the single reads.

**State file** (`crates/obc-core/src/config.rs`):
- `[camera_controls."<id>"]` round-trips in its shape (`values` as an inline table, `reset_all` only when true, `reset_names` as an array only when not empty, no table when there is nothing to keep, entries in the order of the ids), including negative values;
- camera ids that need escaping (quotes, backslashes, control characters, a newline, both kinds of quote, an empty id, text the `toml` crate would otherwise write as a literal or multi-line string) are written as basic strings and read back, all in one file;
- bad entries are dropped one by one with a warning that names the file and the part (a control name that does not follow the grammar, a value that is not an integer or does not fit 32 bits, a `values`, `reset_all`, `reset_pending` or `reset_names` of the wrong type or with an element of the wrong type, an entry or the whole section that is not a table), the rest of the file is read as usual, and the file is never reported damaged; a damaged file elsewhere still is. `control_names_follow_the_grammar` (a lower-case letter, then up to 31 of `a-z`, `0-9` and `_`) and `reset_names_and_any_control_name_round_trip` cover the names (the state file keeps any such name, no longer only the five), and `a_revision_6_pending_reset_becomes_the_five_image_names` the migration: a revision-6 (0.21) `reset_pending = true` is read as `reset_names` of the five image controls, never as `reset_all`, and is not written back.

**Allowlist and check** (`crates/obc-pipeline/src/controls.rs`, no device): the five names and their V4L2 ids, in their order; a control is adjustable only if it is not disabled, read-only or inactive and is an integer or a boolean; `check_value` takes the range and the step counted from the minimum (a step of zero or less counts as 1); a node that cannot be opened is an error; a control that cannot be queried or read is skipped without taking the list with it.

**Controller** (`crates/omarchy-broadcastd/src/controller/tests.rs`, fake backend, explicit instants):
- reads in any state; a set while `active` writes, stores, saves and announces; while the camera is not open it only stores; in `starting`, a set and a reset write to the open camera;
- a name that is not allowed, a control the camera lacks, a value outside the range or off the step: `InvalidArgument`, nothing written or stored; a write the device refuses while streaming fails the call and stores nothing; a failed write at an open does not fail the session;
- the stored values survive a restart and are written at the next open; a reset while streaming writes every default and forgets the values, and while idle it is pending until the next open (defaults first, then any value stored since). From revision 7 a closed `ResetCameraControls` leaves the five image names in `reset_names` and not `reset_all` (`the_revision_six_methods_reach_only_the_five_image_controls`); a stored value the device no longer takes is skipped and kept;
- each camera keeps its own values and a switch applies those of the new camera; a fail-over and a mode change that reopen the camera apply them again; a rename of the open camera to an id with stored values applies them;
- the `CameraControls` view is refreshed on a rescan, once at the camera's first frame, and not otherwise; a set opens the node once and reads back only the control it wrote; a control that cannot be read is left out and warned about once;
- a bad entry in the state file does not turn the daemon off; the controller's allowlist is the same list as the other crates' (`obc-core`, `obc-pipeline`, `obc-ipc`).

**D-Bus object and `obc-ipc`** (`crates/omarchy-broadcastd/src/dbus/tests.rs`, `crates/obc-ipc`): the three methods, the property and its change notifications, and the error names (`Unavailable`, `NoSuchCamera`, `InvalidArgument`, `Failed`) through the typed proxy; the signatures `(siiiiib)` and `(sa(siiiiib))`; the property in the snapshot and `None` from a daemon below revision 6.

**CLI** (`crates/omarchy-broadcast/tests/cli.rs` and the unit tests of `json.rs`, `human.rs`, `model.rs` and `bus.rs`), against a mock daemon:
- `controls` in human and JSON form; an empty list, not an error, with no camera; one retry when the active camera changed between the properties and the call; `Applied live.` against the not-in-use note by state;
- `controls set` and `controls reset` print the controls read back, say so when only the read-back failed, and exit 1 with the daemon's message when it rejects the call; usage errors (an unknown name, a missing or non-integer value, `--json` with a subcommand) exit 2 before any call; negative values are accepted;
- reads never start the daemon, `set` and `reset` may; a revision 5 daemon gives the unsupported message (and the `unsupported-daemon` code with `--json`);
- `watch --json` prints a `controls` line on a change of the property and none otherwise; the `snapshot` carries `camera_controls` (`null` below revision 6) and the `state` line does not;
- the JSON bounds: the exact `controls` line, a name over 32 bytes and a list over 16 controls clipped with `controls_truncated`, and worst-case `snapshot` lines that keep the 4096-byte cap by cutting the controls first and then the cameras.

**Hardware tests (ignored).** Both print what the camera exposes and put the camera back also when they fail: the first writes every image control's default at its end, the second every control and the field of view it touched back to the value it had when the test started (it no longer leaves the defaults).

| Test | Needs |
|---|---|
| `crates/obc-pipeline/tests/hardware.rs::image_controls_query_write_and_read_back` | `OBC_TEST_CAMERA`, a camera that exposes at least one of the image controls (it fails, naming them, otherwise). No GPU, no loopback. It queries the controls (order, range, default on the step, value in range), writes another valid value (the default plus one step) to a wide control, sharpness if there is one, and reads it back, then writes every default and reads each back |
| `crates/omarchy-broadcastd/tests/hardware.rs::camera_controls_are_stored_and_reapplied_at_each_open` | `OBC_TEST_SINK` (a free v4l2loopback device with `exclusive_caps=1`), `OBC_TEST_CAMERA` (a colour camera with a writable `sharpness` control), the GPU, `/usr/bin/dbus-daemon` and `cargo`. The real daemon on a private `dbus-daemon`: it reads the controls over D-Bus while idle, sets while streaming (written at once), closes the camera (the values stay applied), puts the device back to its defaults behind the daemon's back, reopens and finds the stored values on the device again, checks that the property shows the device's values once the camera streams, and that a reset while streaming writes every default and leaves nothing in the state file (the state file is saved write-behind, so the test waits up to 3 s for it). At its end every control and the field of view are written back to their values at the test's start |

```sh
OBC_TEST_CAMERA=046d:0944 \
  cargo test -p obc-pipeline --test hardware -- --ignored --test-threads=1 image_controls_
OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 \
  cargo test -p omarchy-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture \
  camera_controls_are_stored
```

On the reference machine both pass on the Logitech MX Brio (`046d:0944`); the first leaves its image controls at their defaults, the second at the values it found.

### Camera settings

The camera settings channel ([ADR-0023](adr/0023-settings-schema.md), D-Bus revision 7) is covered by the default suite with no camera, no GPU and no loopback device, and by hardware tests that need a camera. The controller tests use the fake device of [Camera image controls](#camera-image-controls), extended with a fake MX Brio: its controls with their menus, flags and dependencies (writing `auto_exposure` flips the inactive flag of the exposure time), a field-of-view Extension Unit row, a capture format, and control events that the test can send. The fake can also refuse a write, leave a control unreadable or change the format on a field-of-view write.

**`obc-ipc`** (`crates/obc-ipc/src/settings.rs`, `lib.rs`, `tests/mock_server.rs`):
- the setting-id grammar (`^[a-z0-9]+(\.[a-z0-9_]+){1,3}$`, 48 bytes);
- every setting type round-trips in memory and over the wire; the encoded dictionaries are exactly the shapes of [dbus-api.md](dbus-api.md#camera-settings); decoding ignores unknown keys, reads integers of any D-Bus integer type (a `t` above the largest `x` is not a value), and decodes a setting of an unknown type as such (a client skips it); a malformed setting, condition, option, group or change is dropped alone, never the whole tree; reset scopes and operators parse back; the `camera` option;
- the properties in the snapshot (`None` from a daemon below revision 7), the three new error names, and against a mock server over a peer-to-peer connection: the typed proxy for the two properties and the three methods, the introspection data of `GetSettings` with its four out arguments, the errors mapped back by name, `SettingsChanged` as one signal per operation, and a revision-6 daemon that has none of it.

**The settings model** (`crates/omarchy-broadcastd/src/settings/tests.rs`, pure, over fake control sets):
- the MX Brio's tree has its groups in order and leaves out what is never exposed; enum options are exactly the items the device reports (`aperture_priority` labelled `Auto` with no item 0);
- the exposure time follows the exposure mode, white balance and focus their automatic switches, each with its reason; the device's flags grey a setting with their reason; effective values while open and while closed (stored, else the default after a pending reset, else the device's);
- the inverted `keep_framerate` (raw 0 is `true`, the default is the opposite of the raw default, the stored value is raw);
- an unknown control falls back to `other`, and a name that is not storable or duplicates a known one is left out; the caps on settings and options;
- validation in the order unknown, disabled, invalid, with its message; masters written first and reset last;
- a value change is a delta and a new shape a structural change; the sanitiser removes controls, bidirectional marks and zero-width characters and cuts on a character boundary.

**Controller** (`crates/omarchy-broadcastd/src/controller/tests.rs`, fake backend, explicit instants; no test sleeps):
- the tree from revision 1, and no camera is no error (`GetSettings`); a mode change and the exposure it enables come in one signal; the serial moves with the structure and the revision with every change;
- a set while closed is stored and the open writes the masters first; `SetSetting` refuses in the order unknown, disabled, invalid; a set within the 50 ms window is held with its reply, replaced by a later one (answered `Ok`) and answered with the real outcome when written; over 100 writes a second is `RateLimited` and stores nothing; a reset supersedes the held writes in its scope; a held write whose camera closes or switches is stored and never written; one follows a rename of the open camera; every held reply is answered at shutdown;
- a reset by scope, open and closed (`reset_all`, `reset_names`); the revision-6 members reach only the five image controls;
- the state file is saved after 300 ms without another change and flushed by any other save; a change the state file cannot take fails at once, as in revision 6;
- the watcher runs only while the camera is open, an event publishes the delta, a rename of the open camera restarts it; a set on the open camera reads back only what depends on it;
- a stored `keep_framerate` wins over the configured pin; a reset keeps the pin; the field of view is written only while open and a format change is undone; a stored dependent that a read left out before its masters is applied after them, and one unreadable afterwards is kept and nothing else is written.

**D-Bus object** (`crates/omarchy-broadcastd/src/dbus/tests.rs`): through the typed proxy over a peer-to-peer socket, `settings_methods_their_signal_their_properties_and_their_errors`: the properties, the three methods and their error names, `SettingsChanged` arriving after the property change of the same step, and the introspection data (`GetSettings` has four out arguments).

**CLI** (`crates/omarchy-broadcast/tests/cli.rs` and the unit tests of `json.rs`, `human.rs`, `watch.rs` and `bus.rs`), against a mock daemon that holds a settings tree and applies dependencies:
- `settings` in human and JSON form, the clear line with no camera, `settings set` typing the value from the tree (a switch, a number negative after `--`, a choice, an action), `settings reset` by camera, group and setting, `--camera` before or after the subcommand, usage errors (a value of the wrong type, `--json` with a subcommand) exiting 2 before any write, the daemon's rejections exiting 1 with its message, reads that never start the daemon while `set` and `reset` may, and a revision-6 daemon giving the unsupported message (and the `unsupported-daemon` code with `--json`);
- `watch --json` prints the full set after the snapshot, a delta per signal, a new set on a serial change or a revision gap, and nothing without settings; `watch` for a person prints nothing of the settings; the rule that decides stale signal, delta or full set is tested over the whole `u32` range, the wrap included;
- the snapshot carries `settings_revision` and `settings_schema` (`null` below revision 7, absent from `state`);
- the lines: the exact `settings_group`, `settings_delta` and `settings_clear` lines, a big group cut into parts of at most 8 settings, and worst-case lines (every string at its bound, 16 options and 4 conditions, the longest numbers, hostile strings made of 6-byte escapes) that stay under 4096 bytes and stay valid JSON, a single setting at its bounds fitting a line alone.

**Hardware tests (ignored).** Each puts every control it touches back to the value it had when the test started, also when it fails (a guard that drops after the daemon and after any stream), and never resets the whole camera. They print what the camera exposes. On a camera that is not an MX Brio only the generic checks run.

| Test | Needs |
|---|---|
| `crates/obc-pipeline/tests/hardware.rs::settings_hw_enumerates_every_control` | `OBC_TEST_CAMERA`. Every control is listed in ascending id order, of an exposed class, with a consistent range and default; on an MX Brio its known controls are all there with their ids, types, ranges and menus (`auto_exposure` has only items 1 and 3, `power_line_frequency` 0 to 2) and the field-of-view row resolves to unit 10 |
| `…::settings_hw_auto_exposure_flips_the_exposure_time_inactive_flag` | `OBC_TEST_CAMERA`, with automatic exposure on at the start. The exposure time is inactive under the automatic mode, active under manual, inactive again after the mode is put back |
| `…::settings_hw_a_slider_writes_and_reads_back` | `OBC_TEST_CAMERA` with `sharpness` or `gain`. The default plus one step is written and read back, then the default |
| `…::settings_hw_field_of_view_while_streaming` | `OBC_TEST_CAMERA` with the field-of-view row. The current byte is read and printed, the camera streams, 78° (or 90° when it starts at 78°) is written and read back, the capture format is unchanged and frames keep flowing, then the starting value is written back; the capture format and frame interval are the starting ones at the end |
| `…::settings_hw_a_control_change_raises_an_event` | `OBC_TEST_CAMERA` with `exposure_dynamic_framerate` or `gain`. A watcher receives, within a bounded wait, the event for a change made through another descriptor, and for the restore |
| `crates/omarchy-broadcastd/tests/hardware.rs::settings_follow_the_camera_over_dbus` | `OBC_TEST_SINK` (a free v4l2loopback device with `exclusive_caps=1`), `OBC_TEST_CAMERA` (a colour camera with an `auto_exposure` menu, `sharpness` and `gain`; the field of view is checked on a camera with the row), the GPU, `/usr/bin/dbus-daemon` and `cargo`. The real daemon on a private `dbus-daemon`. While idle: the tree has its groups and the field of view, and the exposure time is greyed under the automatic mode. While streaming: the manual mode and the exposure time it enables arrive in one signal, and back; a slider is written, read back on the device, reset to its default and forgotten; the field of view is changed and back without the consumer's caps or the capture format moving; a change made behind the daemon's back is announced by the watcher as a delta and is not stored. It never resets the whole camera |
| `…::settings_tree_of_the_real_camera` | `OBC_TEST_CAMERA`, read only: no daemon, no stream, no sink. It builds the tree the model makes from the real camera and prints it, and checks that every id is valid, there are at most 64 settings, and every setting is enabled or has a reason; on an MX Brio, that `field_of_view`, `keep_framerate` and `sharpness` are in it |

```sh
OBC_TEST_CAMERA=046d:0944 \
  cargo test -p obc-pipeline --test hardware -- --ignored --test-threads=1 settings_hw_
OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 \
  cargo test -p omarchy-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture \
  settings_
```

The camera must be free (no application, no daemon holding it): stop the daemon first. On the reference machine all of them pass on the Logitech MX Brio (`046d:0944`) with `OBC_TEST_SINK=/dev/video11`, and leave every control and the field of view at the value they found. The field of view switch while the camera streams 1920x1080 NV12 at 60 frames per second leaves the capture format and the consumer's caps unchanged.

### Known limitation: one application at a time

While one application streams from the virtual camera, a second one cannot open it: v4l2loopback refuses the second reader's format request (`S_FMT`, "Device or resource busy"). This was seen with a second `gst-launch-1.0 v4l2src` and `obc-consumer`; it is a property of the loopback device, not of the daemon.

## Video effects

The effects ([ADR-0012](adr/0012-video-effects-and-maxine-backend.md)) are tested in layers, so that everything except the real SDK runs without it.

### Default suite (no SDK, no GPU, no camera)

```sh
cargo test -p obc-core -p obc-gpu -p obc-backend -p obc-backend-maxine -p gst-obc \
  -p obc-pipeline -p obc-ipc -p omarchy-broadcastd -p omarchy-broadcast \
  -p obc-module-abi -p obc-module-sdk -p obc-module-host -p obc-module-maxine
```

To show that nothing in this run depends on the SDK, run it with the SDK variables removed:

```sh
env -u MAXINE_VFX_ROOT -u LD_LIBRARY_PATH -u LD_PRELOAD -u MAXINE_OS_RELEASE \
  cargo test -p obc-core -p obc-gpu -p obc-backend -p obc-backend-maxine -p gst-obc \
    -p obc-pipeline -p obc-ipc -p omarchy-broadcastd -p omarchy-broadcast \
    -p obc-module-abi -p obc-module-sdk -p obc-module-host -p obc-module-maxine
```

On the reference machine this passed with every hardware test ignored (the counts change with every release and are not recorded here), and `ldd` on `omarchy-broadcastd` and `obc-bench` shows no link to the SDK, CUDA or TensorRT libraries. What the default suite covers:

- **Backend seam** (`obc-backend`, `obc-gpu`, `obc-backend-maxine`): the trait types, the validation of load requests, the error texts, the generic chain seam (`chain.rs`: stage ids and their reserved names, host and CUDA frame descriptions), and a Maxine backend that reports itself unavailable with the reason when the SDK is not found.
- **Modules** ([ADR-0025](adr/0025-loadable-modules.md)): the C ABI and its header, the SDK, the host (manifests, discovery, the loader, the registry, the crash ledger), the Maxine module, the `obcmodule` element, the daemon's handling of modules and the example module; listed under [Modules](#modules).
- **The element** (`crates/gst-obc`): registration and caps (CUDA NV12 on both pads), properties and their validation, status messages on the bus, the background image (crop to fill, the size limit, a JPEG, an undecodable or missing file), and the refusal of hostile images in under half a second: a 50-byte PNG declaring 60000x60000 pixels and a JPEG declaring 65000x65000 are refused from their headers before a decoder runs; a file that is a FIFO is refused without blocking. The first-image rule is tested too: a valid image followed by a second one declaring a huge size is decoded as the first image only, data after the image (random bytes, an MP4 box, zero padding) is ignored, and a truncated image is refused.
- **The pipeline** (`obc-pipeline`): the effect settings and their checks, the effect statistics, that an output built without the element refuses an effect, and that an effect is refused with the roundtrip stage or with a system-memory output.
- **The daemon** (`omarchy-broadcastd`), against its fake backend: the `[effect]` configuration, the runtime state keys, the setters and their errors, persistence and precedence, the `Warning`, `Error` and `State` outcomes of an effect failure with `on_failure` `"black"` (the session kept, black frames, the reason in `Warning`, retried by a call or a reload) and `"none"` (the camera image, the reason in `Warning`, never in `Error`), the clearing of that warning when the session ends or the effect is dropped, the reason kept whole up to 400 bytes in `Warning` and in the journal, a `Warning` that stays one line whatever a module calls itself, every selected stage other than the background effect that no module provides (dropped and named in `Warning` with its module's reason, the session kept streaming, and given back when its module is enabled again), a background effect that no module can run (`a_background_effect_with_no_module_to_run_it_follows_on_failure`, and `a_background_effect_selected_mid_session_with_no_module_follows_on_failure` for one selected later: black, the fallback image or the camera image by `on_failure`, the camera kept open, the reason in `Warning`), the modules' conflicts, requirements and override (listed under [Modules](#modules)), a session start that learns another provider and starts again in the same step, and a damaged state file.
- **Noise removal** ([ADR-0018](adr/0018-video-noise-removal.md)): the element with the fake backend (`crates/gst-obc/tests/element.rs`: `denoise` is a property of its own with a status of its own, and the new-source flag is a free video buffer flag); the output clock (`obc-pipeline`, `the_first_fresh_frame_of_each_source_is_flagged_down_to_the_sink`); the daemon's `[effect] denoise` configuration, the `denoise` state key (a non-boolean value is a damaged state file), `SetDenoise` and its persistence, the fail-open outcome and the `Error` precedence (`controller/tests.rs`, `dbus/tests.rs`); and the CLI's `denoise on|off`, the revision-4 message and the `null` JSON field (`tests/cli.rs`, `json.rs`, `human.rs`).
- **Stages, enhancement and the global settings** ([ADR-0024](adr/0024-stackable-stages-and-effects-in-the-settings-schema.md)): the stage list and the feature availability in the seam, the filesystem probe, the relighting environments, the enhance seam and the factor rule, the effect settings of the tree and their writes, the controller's handling of installed features, relighting and enhancement, and the CLI's and the JSON lines' global groups; listed under [Stages, enhancement and the global settings](#stages-enhancement-and-the-global-settings).
- **HDR look** ([ADR-0030](adr/0030-hdr-look-and-truehdr.md)): the tone map's reference in `obc-gpu/src/tonemap.rs` (`the_tone_map_matches_the_python_reference`, whose values a reference implementation of the tone map in Python gives for the same pixels; `the_pq_curve_has_its_standard_anchors`; `the_eetf_keeps_the_mid_tones_and_rolls_off_to_white`; `rgb10a2_has_red_in_the_low_bits_and_alpha_in_the_top_two`; `a_source_peak_at_or_below_sdr_white_is_refused`, where `ToneMap::new` refuses a peak at or below the point where the gain puts 100 nits (gain × 100), a gain that is not positive and finite, and a peak that is not finite, and accepts a peak just above the point; and `the_embedded_ptx_is_pinned_and_takes_the_eight_parameters_launch_passes`); the two reasons of the TrueHDR backend in `obc-backend-maxine/src/truehdr.rs` (`an_empty_root_is_unavailable_with_a_reason`, `a_missing_feature_library_is_named_with_its_directory`); the module's probe in `obc-module-maxine/src/lib.rs` (`the_hdr_look_is_offered_with_the_truehdr_feature_alone`: TrueHDR ships no model, so its library alone makes the stage available whatever the GPU); the switch in the settings tree (`omarchy-broadcastd/src/settings/effects/tests.rs`, `the_effects_are_three_global_groups_with_valid_ids` and `the_values_and_defaults_are_the_effect_settings`: the switch is in the `video` group and off by default); and the daemon's plan (`omarchy-broadcastd/src/modules.rs`, `the_hdr_look_runs_in_the_effect_element_before_frame_generation`: last of the effect element's stages, with frame generation after it). The manifest and the registry order are in [Modules](#modules).
- **The CLI and the IPC crate**: `effect` and its option rules, the revision-1 daemon message, the JSON fields and their bounds, the escaping of daemon strings in the human output, and the proxy against a mock server.

### GPU tests with the fake backend (`gpu_` prefix)

These need an NVIDIA GPU and the GStreamer CUDA elements, but no SDK and no camera: a fake `InferenceBackend` (a test double in `crates/gst-obc/tests/support/fake_backend.rs`) marks the pixels it processes, so the tests can tell a processed frame from a raw one by value.

```sh
cargo test -p gst-obc -p obc-pipeline -- --ignored gpu_ --test-threads=1
```

They also check the noise-removal stage with the fake backend: `none` with noise removal passes the input through until the stage is ready and then shows only processed frames; a failed load or a failed frame drops the stage and keeps the effect (or the input, with `none`) with the reason reported; an effect that fails again without the stage takes the effect's failure path; the green screen from `none` with noise removal loads without blocking and without a black frame (`gpu_the_green_screen_from_none_with_denoise_loads_without_black_or_blocking`: the frames are the stages' while they are loaded, the input otherwise); and the new-source flag resets the temporal state once. They check that `none` passes the input buffer through untouched and touches no CUDA; that turning an effect on mid-stream keeps sending the input until the effect is ready, then only the effect, with no black frame and never the input again (`gpu_a_toggle_mid_stream_keeps_the_input_until_ready_and_switches_never_show_it`); that a start with the effect requested, and a new source while it loads, show only black until the effect is ready, and never the raw input (`gpu_a_start_or_a_new_source_while_the_effect_loads_is_black`); that a switch from one effect to another shows only processed frames; that each failure (backend missing, load, frame, background image) shows black or the input as `on_failure` says; that repeated pushes of one buffer leave it unmodified; that every stop releases the CUDA context exactly once; and, through a whole output (`gpu_an_output_switches_effects_while_streaming`), that a stream switching effects keeps its frame rate and negotiates its caps once, and that turning an effect on mid-stream sends the source frames, none of them black, until the effect is ready (an 800 ms load is about 24 source frames) and the effect frames after, never the source again.

### Maxine hardware tests

These load the real SDK. They need the SDK root and, on a distribution the SDK does not list, the os-release shim ([setup/maxine.md](setup/maxine.md), sections 2 to 4). `LD_LIBRARY_PATH` is not needed: the backend loads the SDK's libraries from the root itself ([ADR-0021](adr/0021-nvidia-sdk-handover.md), amendment). The root is `MAXINE_VFX_ROOT` when set, else, with `maxine-vfx-sdk-local` installed, the default `/opt/nvidia/maxine/videofx`; the package's environment file sets the variable and `MAXINE_OS_RELEASE` (the shim's input), and the shim is the package's. Set `LD_PRELOAD` through the cargo runner, so that only the test binaries load the shim; `LD_PRELOAD=... cargo test` would be dropped by cargo. With an SDK tree that is not the package, set `MAXINE_VFX_ROOT` and `MAXINE_OS_RELEASE` as the omarchy-nvidia-sdks documentation (`docs/usage.md`) describes. The default test suite stays hermetic regardless, because none of its tests probes or loads the SDK through the environment: the tests that probe or load construct the factory with an explicit root or none (`MaxineFactory::with_root`), or inject a test factory, and the few that build the default factory only check its name and its fault switches. The module's tests probe temporary SDK trees, and the daemon's use fake modules, so none reaches the default root either. The `env -u` command above cannot show this on its own, because it does not disable the directory-based default root.

```sh
set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a    # MAXINE_VFX_ROOT, MAXINE_OS_RELEASE (and LD_LIBRARY_PATH, not needed)
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$MAXINE_VFX_OS_RELEASE_SHIM"

cargo test -p obc-gpu -p obc-backend-maxine -p gst-obc -- --ignored --test-threads=1
OBC_TEST_CAMERA=046d:0944 OBC_TEST_SINK=/dev/video11 \
  cargo test -p obc-pipeline --test hardware -- --ignored maxine_ --test-threads=1
```

- `obc-backend-maxine` (26 tests in `tests/hardware.rs`; the 8 `maxine_denoise_` tests are described under [Video noise removal](#video-noise-removal), the 8 `maxine_relight_` tests and `maxine_a_stage_change_…` under [Stages, enhancement and the global settings](#stages-enhancement-and-the-global-settings), the 3 `gpu_` tests under [Maxine hardware tests, the enhancement method and the chair](#maxine-hardware-tests-the-enhancement-method-and-the-chair), and the `gpu_truehdr_…` test under [Maxine hardware tests, the HDR look](#maxine-hardware-tests-the-hdr-look)): the backend refuses calls without the CUDA context or a load; blur and replace in a context created like GStreamer's, with per-stage GPU times, load times and GPU memory logged. The SDK works in the pushed context: afterwards the device's primary context is inactive. The mask hardening has three more (see [The hardened mask](#the-hardened-mask)).
- `gst-obc` (25 tests in `tests/hardware.rs`, including `gpu_` ones, `maxine_denoise_alone_under_blur_and_toggled_while_playing` and the relighting ones listed in the next section) and `obc-gpu` (5 tests in `tests/hardware.rs`, plus the four `checkpoint_` tests of [Parking the GPU at Idle](#parking-the-gpu-at-idle)): the element against the real backend, the driver helpers and the mask kernel, and the tone map kernel (`gpu_pq_to_sdr_…`, see [Maxine hardware tests, the HDR look](#maxine-hardware-tests-the-hdr-look)). `gst-obc` has `obc-backend-maxine` as a dev-dependency for `maxine_mask_kernel_failures_show_black_with_the_reason`, which needs the Maxine environment above.
- `obc-module-maxine` `tests/hardware.rs` (6 ignored tests, the HDR look's `gpu_the_hdr_look_…` among them; and 1 ignored test in `src/gpu_tests.rs`, `gpu_a_failed_hdr_look_…`, which runs as a library test): the built module through the host, on CUDA frames; see [Maxine module hardware tests](#maxine-module-hardware-tests-ignored) and [Maxine hardware tests, the HDR look](#maxine-hardware-tests-the-hdr-look). The daemon loads the SDK only through this module, so the end-to-end checks of the daemon go through it as well.
- `obc-backend-maxine` `tests/sdk_root.rs` (3 ignored tests): the SDK found from its root alone, with no `LD_LIBRARY_PATH`. Each test re-runs the test binary as a child process, because the loader reads `LD_LIBRARY_PATH` and `LD_PRELOAD` only at process start. The child has `LD_LIBRARY_PATH` removed, the shim preloaded (`OBC_TEST_PRELOAD`, with `MAXINE_OS_RELEASE`) and the root given one way only: as `MAXINE_VFX_ROOT` (`maxine_effects_run_from_the_variable_alone`); as the configured root (the root the Maxine module sets from its `sdk_root`, which `[effect] maxine_root` also gives it), with the variable removed (`maxine_effects_run_from_the_configured_root_alone`); or not at all, the end user's setup with the package, where the default root must be found (`maxine_effects_run_from_the_default_root_with_nothing_set`). In the child, blur and noise removal each load and run a frame, and `/proc/self/maps` must show the SDK's libraries mapped from that root. The first two use the root this process would use (`MAXINE_VFX_ROOT`, else the default); the third needs the package. The unit tests of the backend cover the root precedence, the pin (a different root after the SDK is loaded is refused), the ELF `DT_NEEDED` reader (including crafted and malformed input) and the load order; those of `obc-core` the validation of `maxine_root`, and those of the module the configuration key (see [Modules](#modules)).

```sh
env -u LD_LIBRARY_PATH -u MAXINE_VFX_ROOT MAXINE_OS_RELEASE=/usr/lib/maxine-vfx/fake-os-release \
  OBC_TEST_PRELOAD=/usr/lib/maxine-vfx/os-release-shim.so \
  cargo test -p obc-backend-maxine --test sdk_root -- --ignored --test-threads=1
```

- `maxine_effects_end_to_end_keep_the_rate_and_release_the_gpu` (`obc-pipeline`): camera, session and output with `blur`, then `replace`, then `none`; each phase keeps the contract rate at the sink, the effect becomes ready, its statistics are sane, and after the output stops the process holds no CUDA context.

The frames in these tests are synthetic, so the mask the model produces for them is not predictable; the tests assert what holds for any mask and log the rest.

### Video noise removal

The stage is tested at each level described above; this section lists what needs the SDK and its `nvvfxdenoising` feature and models ([setup/maxine.md](setup/maxine.md), sections 1 and 2; the daemon and the backend find the feature's library under the SDK root themselves, so `LD_LIBRARY_PATH` is not needed).

- **Backend** (`crates/obc-backend-maxine/tests/hardware.rs`, run with the command of [Maxine hardware tests](#maxine-hardware-tests)): `maxine_denoise_alone_removes_noise_from_a_flat_frame`, `maxine_denoise_switches_on_and_off_on_a_loaded_chain`, `maxine_denoise_before_blur_and_replace_gives_sane_output`, `maxine_denoise_reset_temporal_starts_a_new_video`, `maxine_denoise_strength_0_and_1_both_load_and_run`, `maxine_denoise_gpu_memory_returns_after_unload`, `maxine_denoise_faults_degrade_to_the_chain_without_it` (fault injection for the load and the run) and `maxine_denoise_loads_again_after_the_gpu_was_parked` (park and unpark, in its own process).
- **Element** (`crates/gst-obc/tests/hardware.rs`): `maxine_denoise_alone_under_blur_and_toggled_while_playing`.
- **Daemon** (`crates/omarchy-broadcastd/tests/hardware.rs`): `denoise_stacks_with_the_effect_while_streaming_and_the_gpu_parks` streams `none` with noise removal, adds `blur`, toggles, stops and checks that the GPU parks. It needs `OBC_TEST_SINK`, `OBC_TEST_CAMERA`, `OBC_TEST_PRELOAD` (with `MAXINE_OS_RELEASE`) and the Maxine environment, as the daemon effects test below, and a driver with the CUDA checkpoint API.
- **Recorded frames** (`crates/obc-backend-maxine/tests/denoise_frames.rs`, ignored): runs the stage at both strengths over frames recorded from a real camera and writes the camera frame and each result as PPM images for comparison by eye, with the spatial and temporal noise logged. `OBC_DENOISE_FRAMES` names the directory of recorded sequences and `OBC_DENOISE_OUT` the directory for the images; the test fails without them.

  ```sh
  OBC_DENOISE_FRAMES=$HOME/frames OBC_DENOISE_OUT=$HOME/denoise-out \
    cargo test -p obc-backend-maxine --test denoise_frames -- --ignored --nocapture
  ```

  with the Maxine environment and the shim runner of the section above.
- **The strength knob.** The daemon runs the stage at strength 1 and has no setting. `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process under test (a backend test, `obc-bench`, the daemon) overrides it, to compare the two models. It is for development only.
- **`obc-bench`.** `--denoise` requests the stage, with `--effect none|blur|replace` (`--effect none` for the stage alone; `--denoise` without `--effect` is refused). The JSON effect section gains `denoise`, `denoise_status`, `denoise_reason`, `frames_denoised` and `denoise_ms_p50` / `denoise_ms_p95`. Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce them locally with these.

### Stages, enhancement and the global settings

[ADR-0024](adr/0024-stackable-stages-and-effects-in-the-settings-schema.md). Everything except the real SDK runs without a GPU or an SDK; each level below says what it needs. Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce locally with `obc-bench` (flags below).

**Default suite (no SDK, no GPU, no camera):**
- **The seam** (`crates/obc-backend/src/lib.rs`): `stage_lists_keep_the_canonical_order_once` (a list in another order, or with a stage twice, is refused; the empty list is empty), `denoise_is_a_pre_mask_stage`, `relight_is_a_post_mask_stage_after_denoise`, `a_stage_is_available_when_all_its_features_are` (relighting needs its feature and the green screen), `relight_lights_parse_and_display_lower_case`, `relight_params_clamp_and_validate`, `features_have_stable_names_and_availability_its_reason`; and `src/enhance.rs` (the enhance request: even sizes and factors 2 to 4, frame validation of NV12 and I420 pitches).
- **Which features are installed** (`crates/obc-backend-maxine/src/features.rs`, over temporary SDK trees): `library_and_models_for_this_gpu_make_a_feature_installed`, `models_only_for_another_gpu_are_not_enough`, `a_library_without_models_is_not_installed`, `relighting_needs_both_of_its_models`, `models_without_their_library_are_not_installed`, `model_names_give_their_architecture`, `the_directories_are_the_sdk_feature_names`, `every_feature_is_reported_once_in_order`; in `src/lib.rs` `relight_stage_availability_needs_the_green_screen_too`, `the_usable_root_is_the_loaded_one_once_the_sdk_is_loaded` and the fault switches' tests. The probe reads files only, so these tests need no SDK.
- **The relighting environments** (`src/hdri.rs`): `every_map_is_2_to_1_finite_and_positive`, `the_key_light_is_bright_soft_and_where_it_is_said_to_be`, `warm_is_redder_and_cool_is_bluer_at_the_same_brightness`, `black_body_colours_have_unit_luminance`.
- **The enhance element** (`crates/gst-obc/src/enhance`): the source caps are `k` times the sink caps in NV12, ranges and lists scale too, the chroma siting follows the caps, colour is BT.601 or BT.709, full or limited. In `obc-backend-maxine/src/vsr.rs`: the transfer flags carry the chroma siting, an empty root is unavailable with a reason, the fault switch is off unless armed from Rust, the two methods parse exactly (`ai`, `fast`) and each names its feature, and a missing method library is named with its directory. In `obc-backend-maxine/src/stages.rs`: the stage is available with either feature installed, and the chair parameter chooses the green screen's mode and is checked (a value that is not a boolean is refused). The enhance element carries the stage's parameters (`the_enhance_element_carries_the_stage_parameters`, in `obc-pipeline`'s `ingest.rs`); the host hands them to the module as the enhance stage's (`crates/obc-module-host/tests/modules.rs`), and the background stage's own parameters go with its effect.
- **The enhance plan** (`crates/obc-pipeline/src/ingest.rs`): `the_enhance_factor_is_the_smallest_that_covers_the_output`, `the_enhanced_frame_stays_within_4k`, `the_enhance_plan_refuses_formats_the_stage_cannot_read`, `the_config_says_whether_enhance_applies_to_its_mode`, `the_enhance_stage_sits_before_the_scaler_only_when_on`, `a_system_memory_ingest_has_no_enhance_stage`.
- **The element's settings** (`crates/gst-obc/src/settings.rs`, `tests/element.rs`): relighting direction and gain are validated, the target carries the relighting request and its light only when it is on, a change of the frame parameters alone is told apart (it reloads nothing), and `relight_has_properties_and_a_status_of_its_own`.
- **Configuration** (`crates/obc-core/src/config.rs`): the keys `enhance`, `relight`, `relight_light`, `relight_direction` and `relight_brightness` and their state-file overrides, validation of the direction and the brightness to their range and to the step of 5, a damaged value in the state file (it fails closed), `example_file_is_the_defaults` (the example file parses to the defaults and every key appears in it), and the cache directory.
- **The effect settings of the tree** (`crates/omarchy-broadcastd/src/settings/effects.rs`, pure): the three global groups and their valid ids, values and defaults, the blur strength enabled only with `blur`, noise removal and relighting greyed and off when not installed with the choice kept, enhancement greyed for "Not installed", a camera at full resolution and a format it cannot read, enhancement applicability changes as deltas, turning relighting on as one delta with the settings it enables, writes checked in the order unknown, disabled, invalid, a mode change as one delta with the strength it enables, and forgetting a key.
- **The compute-capability cache** (`gpu_cache.rs`): a saved value loads back; anything but `major.minor` is unknown.
- **Controller** (`controller/tests.rs`, fake backend): `set_setting_on_a_global_id_is_the_effect_method_it_stands_for`, `global_writes_refuse_as_the_methods_do_in_the_order_of_the_checks`, `a_global_write_clears_a_fault_as_the_methods_do`, `the_effect_methods_and_a_reload_announce_the_global_settings`, `a_global_reset_puts_the_configurations_value_back_and_camera_reset_leaves_it`, `global_settings_are_there_without_a_camera_and_work`, `a_global_write_the_state_file_cannot_take_fails_as_the_setter_does`; the enhancement method and the chair: `the_enhance_method_reaches_the_ingest_and_a_change_reopens_the_camera`, `an_enhance_method_that_failed_is_tried_again_by_its_setting` and `the_chair_reaches_the_background_stage_and_a_change_reloads_the_effect_only`; availability: `noise_removal_not_installed_is_never_requested_and_raises_no_error`, `unknown_availability_requests_noise_removal_as_before`, `the_compute_capability_is_read_once_with_a_session_never_at_idle`, `the_compute_capability_is_cached_across_restarts_and_refreshed_by_a_session`, `a_change_of_what_is_installed_never_requests_an_effect_the_session_started_without`, `a_session_start_that_learns_what_is_installed_sends_one_settings_changed`; enhancement: `enhance_toggled_while_streaming_reopens_the_camera_once_and_keeps_active`, `enhance_set_while_the_camera_is_closed_applies_at_the_next_open`, `enhance_not_installed_is_never_requested_and_raises_no_error`, `enhance_is_not_requested_for_a_camera_at_full_resolution_and_follows_a_switch`, `enhance_is_not_requested_for_a_camera_in_a_format_it_cannot_read`, `enhance_applies_only_where_it_pays`, `an_enhance_failure_keeps_streaming_and_shows_in_error_after_noise_removal`, `an_enhance_failure_is_cleared_by_the_setting_a_reload_a_switch_and_the_end`, `reopens_before_the_first_frame_keep_the_start_deadline`; relighting: `each_relighting_setting_is_saved_applied_live_and_announced_once`, `relighting_settings_at_idle_are_only_stored`, `relighting_not_installed_is_never_requested_and_raises_no_error`, `a_relighting_failure_keeps_streaming_and_is_cleared_like_noise_removals`, `a_relighting_failure_text_is_sanitised_and_bounded`, `a_whole_chain_failure_is_the_effects_not_relightings`, `a_relighting_failure_sits_between_noise_removal_and_enhancement`, `a_damaged_relighting_value_in_the_state_file_fails_closed`; and `stage_failures_never_overwrite_a_higher_cause_in_error` (the `Error` precedence).
- **IPC, JSON lines and the CLI**: `a_global_tree_without_a_camera_round_trips` (`obc-ipc`); `global_groups_follow_the_cameras_and_show_without_a_camera` (the `scope` key, the groups after the camera's, `camera` `""`, and `scope` bounded like the other strings, in the worst-case line test) and the human `global_settings_follow_the_cameras_under_their_own_heading` (`All cameras:`); in `tests/cli.rs` `settings_show_the_global_settings_after_the_cameras_and_without_one`, `settings_set_and_reset_take_global_ids`, `watch_json_carries_the_global_groups_in_the_set_and_their_deltas`, `global_ids_ignore_the_camera_option_and_need_no_camera`, `settings_show_and_set_the_enhance_switch`, `settings_show_the_lighting_group_and_set_a_negative_direction`, against a mock daemon that holds the global groups.

**GPU tests with the fake backend** (an NVIDIA GPU, no SDK, no camera), the same command as above (`cargo test -p gst-obc -p obc-pipeline -- --ignored gpu_ --test-threads=1`): in `crates/gst-obc/tests/hardware.rs` `gpu_none_with_relight_passes_through_until_loaded_then_relights`, `gpu_relight_toggles_and_new_lights_are_switches_on_the_loaded_chain`, `gpu_a_stage_change_loads_or_releases_that_stage_alone`, `gpu_a_relight_load_failure_runs_the_chain_without_it`, `gpu_a_relight_frame_failure_drops_the_stage_and_keeps_the_rest`, `gpu_a_failed_new_light_drops_the_stage_and_the_status_says_so`, `gpu_a_new_source_rebuilds_the_relight_stage_before_it_relights_again` (a second source never shows the first one's relighting) and `gpu_frame_parameters_after_a_failure_load_nothing`; in `crates/obc-pipeline/src/ingest.rs` `gpu_enhance_that_cannot_load_passes_the_camera_frames_through` (one frame dropped, then the camera frames at the output contract with the crop in force) and `gpu_enhance_is_left_out_for_formats_it_cannot_read` (NV21 and YV12 stream without the stage).

**Maxine hardware tests, relighting** (the SDK with the relighting and green-screen features and their models for the GPU; the environment of [Maxine hardware tests](#maxine-hardware-tests)):

```sh
cargo test -p obc-backend-maxine --test hardware -- --ignored maxine_relight_ --test-threads=1
cargo test -p gst-obc --test hardware -- --ignored maxine_a_new_source --test-threads=1
```

- `crates/obc-backend-maxine/tests/hardware.rs`: `maxine_relight_lights_on_every_load_and_every_new_light` (ten loads in one process and twelve live light changes on one load, each checked for a lit result, and each light change reusing the loaded model; it fails if the stage uses a non-blocking stream, so it is the regression test for the stream rule of [ADR-0024](adr/0024-stackable-stages-and-effects-in-the-settings-schema.md)), `maxine_relight_under_none_blur_and_replace_keeps_the_background` (the background is never relit), `maxine_relight_faults_degrade_to_the_chain_without_it`, `maxine_relight_a_failed_new_light_drops_the_stage_alone`, `maxine_relight_reset_temporal_drops_the_stage_and_a_new_load_lights_again`, `maxine_relight_switches_on_and_off_and_returns_its_memory`, `maxine_a_stage_change_loads_or_releases_that_stage_alone`, `maxine_relight_loads_and_lights_again_after_the_gpu_was_parked` (park and unpark, in its own process) and `maxine_relight_stage_times`.
- `crates/gst-obc/tests/hardware.rs`: `maxine_a_new_source_rebuilds_the_relighting_before_it_relights` (two sources through the element; no frame of the second is lit by the first one's instance).

**Maxine hardware tests, enhancement** (the SDK's video super resolution feature; no camera, a test pattern through the ingest tail):

```sh
cargo test -p obc-pipeline --lib -- --ignored maxine_enhance_ --test-threads=1
```

- `maxine_enhance_upscales_small_modes_to_the_output` (`crates/obc-pipeline/src/ingest.rs`): 960x540 (x2) and 640x360 (x3) in NV12, 4:3 I420 modes cropped to fill (through the crop region, one as `jpegdec` makes them), BT.709 and letterbox come out at the output contract, with the same image in luma and chroma as the frame scaled without the stage and at least as sharp; after the stop the process holds no CUDA context.
- `maxine_enhance_failures_fall_back_to_the_camera_frames`: a forced load failure and a forced frame failure both fail the stage open, with the crop region kept.

**`obc-bench`.** `--relight[=warm|neutral|cool]` adds the relighting stage after the effect's mask (alone with `--effect none`; it needs `--effect`), with `--relight-pan` (degrees) and `--relight-gain`; the JSON effect section gains `relight`, `relight_pan_deg`, `relight_gain`, `relight_status`, `relight_reason`, `frames_relit` and `relight_ms_p50` / `relight_ms_p95`. Reproduce the stage's cost locally with it, for example with `--effect blur --denoise --relight`.

### Maxine hardware tests, the enhancement method and the chair

Three `gpu_` tests in `crates/obc-backend-maxine/tests/hardware.rs` that run the real SDK, with the environment of [Maxine hardware tests](#maxine-hardware-tests). Each is ignored by default, and without the SDK root it fails, naming what is missing:

```sh
cargo test -p obc-backend-maxine --test hardware -- --ignored gpu_ --test-threads=1
```

- `gpu_enhance_upscales_with_either_method`: the upscaler loads and runs a 640x360 frame at the factors 2, 3 and 4 with both methods (`ai`, `fast`, through `MaxineVsr::load_with`), reports the method it loaded, keeps the picture's level, records its upscale time, and unloads. The `fast` method needs the SDK's `nvvfxupscale` feature, the `ai` method `nvvfxvideosuperres`; the test fails if either is not installed.
- `gpu_a_chair_switch_loads_the_green_screen_again_and_keeps_the_cut_out`: under `blur`, `replace` and `blur` with relighting, a switch of the background stage's `chair` parameter loads the green screen again in the other mode, with the effects that read its mask, while a switch that keeps `chair` reuses the model, and the cut-out is still there in both modes.
- `gpu_a_failed_chair_switch_keeps_the_previous_chain`: a chair switch whose new green screen (or the relighting bound to it) fails to load, by fault injection, fails the load and leaves the previous chain running: the mask keeps its mode, no model is counted, and the next frames still run through the loaded mask. With the fault disarmed the same switch then succeeds.

### Maxine hardware tests, the HDR look

The HDR look ([ADR-0030](adr/0030-hdr-look-and-truehdr.md)) runs NVIDIA's TrueHDR on the composite and tone-maps the HDR10 image back to SDR in a CUDA kernel. Its four ignored GPU tests check the kernel and the stage against Rust references, not against a picture: the kernel against `obc_gpu::tonemap::pq_to_sdr`; TrueHDR and the tone map against the reference of each frame's own HDR image; and the stage through the module's ABI, and in the library, where a fault switch can be armed. ADR-0030 has the kernel tested against its Rust reference, and the first test is that check. They need an NVIDIA GPU and the Maxine environment of [Maxine hardware tests](#maxine-hardware-tests); the first needs no SDK, but the same environment serves all four:

```sh
set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a    # MAXINE_VFX_ROOT, MAXINE_OS_RELEASE
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$MAXINE_VFX_OS_RELEASE_SHIM"

cargo test -p obc-gpu --test hardware -- --ignored gpu_pq_to_sdr --test-threads=1
cargo test -p obc-backend-maxine --test hardware -- --ignored gpu_truehdr --test-threads=1
cargo test -p obc-module-maxine --test hardware -- --ignored gpu_the_hdr_look --test-threads=1 --nocapture
cargo test -p obc-module-maxine --lib gpu_a_failed_hdr_look -- --ignored --test-threads=1 --nocapture
```

- `obc-gpu` (`tests/hardware.rs`), `gpu_pq_to_sdr_matches_the_rust_reference_within_one_code_value`: the tone-map kernel (`src/kernels/pq_to_sdr.cu`, with its PTX embedded) against `obc_gpu::tonemap::pq_to_sdr`, at TrueHDR's default peak and at a lower one. Every grey code and pseudo-random colours must match within one code value per channel; alpha is 255, and the pitch padding of the output is never written.
- `obc-backend-maxine` (`tests/hardware.rs`), `gpu_truehdr_runs_and_its_output_is_the_reference_tone_map`: TrueHDR loads at its default peak and its RGB10A2 image is opaque. A mid-grey, then the synthetic person, each run once: each frame's output is, pixel for pixel to the rounding of the two conversions, the Rust tone map of the effect's image of that same frame (its luma, BT.601 limited range). So the person's output is neither the grey's nor zero. A mid-grey stays mid-grey, a frame is converted in place, and calls before a load, on another stream or of another size are refused.
- `obc-module-maxine` (`tests/hardware.rs`), `gpu_the_hdr_look_runs_on_every_frame_and_a_failure_drops_only_it`: the stage alone, as the daemon runs it when no other effect stage is on, through the built module's ABI on CUDA frames. Every frame goes out as the look of its own input, a repeat included: two different frames in a row give two different outputs, and the same input gives the same output, so a repeat is never answered with the frame before. The stage sees a repeat only when the effect element's cached output is not valid (ADR-0030), which is why the repeat is checked. A failure on one frame (here a frame on another stream than the load's) is the stage's: the chain drops it, and the following frames pass through as they came.
- `obc-module-maxine` (`src/gpu_tests.rs`, run as a library test), `gpu_a_failed_hdr_look_after_the_cut_out_leaves_the_matted_frame`: a `[background, hdr]` chain whose HDR look fails on a frame, by fault injection (`TrueHdrFault::Run`). That frame holds the matted frame the cut-out wrote, processed: neither the input nor black. The HDR look is then dropped, and the next frames go out matted without it. The test lives in the library, not in `tests/hardware.rs`, because the module is built as a cdylib only and a Rust fault switch cannot cross the ABI; the built library is covered by the test above.

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
set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a
OBC_TEST_PRELOAD=$MAXINE_VFX_OS_RELEASE_SHIM \
OBC_TEST_SINK=/dev/video11 OBC_TEST_CAMERA=046d:0944 OBC_TEST_CAMERA_2=5986:1193 \
  cargo test -p omarchy-broadcastd --test hardware -- --ignored --test-threads=1 --nocapture
```

Without `OBC_TEST_PRELOAD` the effects test fails with the daemon's `Error` text (`NvVFX_Load(GreenScreen) returned -1209 ...`), which shows that the variable is what makes it pass. With `--nocapture` the test prints, for each phase, the camera's fresh frame rate and whether the output clock was phase locked (the daemon logs this at debug level with `-v`).

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

## Capture and output modes, frame generation and the privacy fallback

These are the tests of [ADR-0027](adr/0027-output-modes-frame-generation-and-privacy-fallback.md) (daemon 0.27.0, D-Bus revision 12, module ABI 1.1). The default suites need no camera, no GPU and no SDK; the `gpu_` tests need an NVIDIA GPU and the GStreamer CUDA elements, and the Maxine one needs the SDK as well.

```sh
cargo test --locked -p obc-core -p obc-pipeline -p obc-ipc -p obc-module-abi -p obc-module-sdk -p obc-module-host -p obc-module-maxine -p gst-obc -p omarchy-broadcastd -p omarchy-broadcast
cargo test --locked -p omarchy-broadcastd --features pipewire
```

By area (test names are in the files named; counts change with every release and are not recorded):

- **Capture and output mode choices** (`obc-pipeline`, `src/capability.rs`, pure, over a fake MX Brio and a fake FHD camera): the camera choices keep each size at its fastest rate; a camera with no 4K mode offers none, and no slow (under 15 fps) or narrow (under 640 pixels) mode is offered; over the limit of 15 the slowest rates go first, then the smallest sizes; a fractional rate is labelled with two decimals so that two rates of a size never share a label (`mode_labels_show_a_fractional_rate_with_two_decimals`); the output choices are the camera's 16:9 sizes and rates, with `(generated)` rates at twice a native rate only when frame generation is available, and the value in force is always an option (`output_choices_always_offer_the_current_value`).
- **The capture mode setting** (`omarchy-broadcastd`, `src/settings/tests.rs` and `src/controller/tests.rs`): the setting is `auto` then the camera's modes and the first group of the tree; a mode written is checked against the options, stored per camera in `[camera_modes]` and applied live by reopening the camera, with the loopback writer and the session untouched (`the_capture_mode_row_offers_the_camera_modes_and_applies_live`); a camera with no known modes has no row; a stored mode the camera no longer offers is labelled ` (gone)` within the label bound and falls back to the nearest mode with a `Warning`; a chosen mode that cannot open falls back to `auto` with a `Warning`, is tried again only when chosen again, is not blamed on a camera that fails after its first frame, and is not blamed when `auto` cannot open either.
- **The output mode in the configuration** (`obc-core`, `src/config_schema/tests.rs`, `src/config_d/tests.rs`, `src/config.rs`; the daemon's `output_mode_is_set_over_set_config_into_the_managed_file_only`; the CLI's `the_capture_mode_and_the_output_mode_are_set_through_the_generic_paths`): `output.mode` offers the runtime options and the value in force; a write goes to `config.d/50-panel.toml` and `config.toml` is untouched; `effect.on_failure` offers `image` only with a fallback image and is written as one of the options the daemon computed; the frame-generation pin is shown only with a choice of module; the state file's `output_good` and `[camera_modes]` round-trip, and a bad entry is dropped alone.
- **Deferral, retries and reverts** (the controller tests, with the fake backend and explicit instants): a change that waits for applications names them in `Warning` and clears after the rebuild; a held loopback keeps the last writer and is tried again at 2 s, doubling to the 32 s cap, and starts again at 2 s when the configuration returns to the format; a change of loopback is not delayed by a held retry; a held output whose last good writer cannot start either is the writer's error; a start on a loopback held in another format streams that format and waits; a restart streams the last good format of the earlier run while the configured one is held; a refused format at the first start falls back to the default; a missing loopback at a format change is retried and never a failed format; a format that cannot start reverts to the last one that did; the last good format is kept only once a frame was streamed; a GPU fault at a session start never fails the format; a session output that cannot start, even without effects, reverts once the camera is released.
- **A session without its effects** (the controller tests): a session output that cannot start at a new format streams without its effects; a blur switched on later in such a session is never shown raw, a reload to `black` stops it, and a control call retries the effects under `none`; a blur with `black` on failure is never dropped to start the output; a start whose output cannot take its effect module opens no camera (`a_start_whose_output_cannot_take_its_effect_module_never_opens_the_camera`, and `a_second_start_that_cannot_build_its_effects_never_opens_the_camera_without_them` for the start that follows); an effect asked while its module could not yet say it can run it is given again once it can, with no restart (`an_effect_asked_while_its_module_could_not_run_it_is_given_again_once_it_can`); a session whose module cannot run the effect never streams without its effects (`a_session_whose_module_cannot_run_the_effect_never_streams_without_its_effects`).
- **The `Warning` bound** (`the_warning_is_bounded_on_a_character_boundary`, `holders_are_named_without_duplicates_up_to_three`): at most 512 bytes, cut on a character boundary; the holders named are deduplicated, three at most, then a count.
- **Frame generation in the daemon** (the controller tests): the stage runs last when the output rate is twice the camera's, is left out unless the rate doubles, follows a capture mode change, starts mid-session when the camera's rate halves and follows a fail-over to a camera of half the rate. In `obc-module-host` the registry keeps it out of every order and refuses an order that names it (`frame_generation_is_in_no_order_but_has_a_provider`).
- **Module ABI 1.1** (`obc-module-abi`: the flag values and the version; `obc-module-sdk`: `Frame::is_repeat` and the exported vtable, where a module that leaves a repeat as it came shows that it was told apart; `obc-module-host`: `a_module_of_abi_1_0_is_handed_only_the_flags_it_knows` and `a_repeat_is_flagged_for_a_module_of_abi_1_1_and_reprocessed_by_one_of_1_0`; `obc-module-maxine`: the four steps of frame generation in `src/framegen.rs`, and the manifest). The output clock sets `REPEAT_FLAG` on repeat ticks and on no other buffer (`obc-pipeline`, `the_first_fresh_frame_of_each_source_is_flagged_down_to_the_sink`); the `obcmodule` element hands a repeat to the chain as one, also through a copying element to a second module (`gst-obc/tests/module.rs`: `a_repeat_goes_to_the_chain_as_a_repeat`, `a_repeat_reaches_a_second_module_through_a_copying_element`).
- **The privacy fallback** (`gst-obc`, `tests/element.rs`, `tests/background.rs`; `obc-pipeline`, `src/loopback.rs`; the controller tests): the element's `fallback-image` property (an absolute path, empty for none) and `on-failure=image`; the image decodes to YUV 4:2:0 in the colour asked; the writer's placeholder switches between black and an image and fills the contract's layout; a start refused under `image` shows the image until the effect is off and goes black again when the camera stops; a failed `replace` background under `image` shows the image; an unreadable image and `image` with no path are black with a `Warning`; a session under `image` gives the element the image and reports what it cannot read; a reload to `image` stops a session without its effect and shows the image; with `black`, a refused start stays black and other faults stay black; an unreadable image is said within a poll of the decode ending (`an_unreadable_fallback_image_is_said_within_a_poll_of_its_decode_ending`) and a decode that never ends is said unreadable at the 10 s limit (`a_fallback_image_decode_that_never_ends_is_said_unreadable_at_its_limit`); a fault that is not privacy's ends the guard's hold and shows black (`a_fault_that_is_not_privacys_ends_the_guards_hold_and_shows_black`); the element's image failure ends with its session (`the_element_s_fallback_image_failure_ends_with_its_session`); a reload gives the image again to be read if it was replaced (`a_reload_gives_the_fallback_image_again_to_be_read_if_replaced`); a start tried again under `image` keeps the image through a refused start and until its first frame when the camera is slow to deliver (`a_start_tried_again_under_image_keeps_the_fallback_image_until_it_streams`); a start the guard drops under the hold keeps the image, the session being dropped before its camera opens (`a_start_refused_again_under_the_hold_keeps_the_fallback_image`); a start tried again under the hold goes black when the hold ends before it streams, with the effect off, `on_failure` set to `black` or `none`, no consumer, the daemon disabled or every camera failed, and shows the image while the next camera is tried (`a_start_tried_again_under_the_hold_goes_black_when_the_hold_ends_before_it_streams`). A file replaced at the same path has another stamp (`a_file_replaced_at_the_same_path_has_another_stamp`, `gst-obc`, `src/background.rs`), so another fallback key (`a_fallback_image_replaced_at_its_path_is_another_key`, `gst-obc`, `src/effect/imp.rs`), and the writer's placeholder reads it again (`the_placeholder_switches_between_black_and_an_image`, `obc-pipeline`, `src/loopback.rs`). `a_link_failure_at_a_module_or_effect_element_is_theirs_not_a_link_error` (`obc-pipeline`, `src/output.rs`) keeps a module's link failure from being read as a fault of the output format.
- **The config example** (`obc-core`, `src/config.rs`): the test that checks that every key of the schema appears in `docs/config.example.toml` lists `fallback_image` too, and the example, uncommented, parses to the defaults plus the example values.

### GPU tests (`gpu_` prefix)

```sh
cargo test -p gst-obc --test hardware -- --ignored gpu_ --test-threads=1
```

The new ones, with the fake backend, in `crates/gst-obc/tests/hardware.rs`:
- `gpu_a_repeat_re_sends_the_last_processed_frame_and_runs_no_effect`: a repeat gets the same memory as the last processed output, with the repeat's timestamp and flag, and the backend is not called; the next fresh frame is processed again;
- `gpu_a_repeat_after_a_settings_change_is_processed_never_the_cached_frame`: a cached output made under old settings is never sent again;
- `gpu_a_repeat_while_the_effect_loads_is_black_under_black`: a repeat is never the raw frame (here the effect is requested at the start, so the hold is black);
- `gpu_a_downstream_in_place_write_does_not_change_the_cached_frame`: with an `obcmodule` element behind the effect that writes in place (a stage that adds to every luma byte), the write lands in a copy, so the repeat shows the cached picture changed once by the module and not twice;
- `gpu_on_failure_image_shows_the_fallback_image_where_black_would_be`, `gpu_an_unreadable_fallback_image_shows_black_and_says_why` and `gpu_a_fallback_image_put_at_its_path_is_read_when_the_settings_are_applied_again`.

With the Maxine SDK, one more in `crates/obc-module-maxine/tests/hardware.rs`, run as [Maxine module hardware tests](#maxine-module-hardware-tests-ignored) are: `gpu_frame_generation_interpolates_fresh_frames_and_holds_repeats` runs the real stage through the host's chain adapter on synthetic frames and checks that the first frame goes out as it is, a repeat goes out as the last fresh frame, and a fresh frame goes out as a picture between the previous one and itself, for a pattern that moves. It asserts what holds for any picture and judges no look.

## Modules

The module system ([ADR-0025](adr/0025-loadable-modules.md), [modules.md](modules.md)) is tested in layers, from the C header up to the daemon, so that everything except the real Maxine SDK and the GPU runs on any machine.

### Default suite (no SDK, no GPU, no camera)

```sh
cargo test --locked -p obc-module-abi -p obc-module-sdk -p obc-module-host -p obc-module-maxine
cargo test --locked -p gst-obc --test module
cargo test --locked -p obc-core -p obc-ipc -p omarchy-broadcastd -p omarchy-broadcast
cargo test --manifest-path examples/module-hello/Cargo.toml --locked
```

The first command builds two modules itself: the host's tests run `cargo build --locked` on `examples/module-hello` and on the fixture module (`crates/obc-module-host/tests/fixtures/module-fixture`), and the Maxine module's tests on `obc-module-maxine`, each into a directory under `CARGO_TARGET_TMPDIR`; the first run compiles their dependencies again. `--locked` is part of the design: the committed `Cargo.lock` of the example and the fixture must not be rewritten by a test. The `obc-module-abi` and `obc-module-sdk` crates carry a version of their own (1.2.0, the ABI's), separate from the workspace version, so that a release bump does not change those lock files. A test of `obc-module-maxine` checks that `version` in `maxine-vfx.toml` is the crate's version: a version bump must update that line too.

- **The ABI** (`crates/obc-module-abi`: 5 unit tests and 2 in `tests/header.rs`). The unit tests pin the layout (sizes of the structures, the numeric codes, the packing of a version). `committed_header_is_current` regenerates `include/omarchy_broadcast_module.h` from `src/lib.rs` with cbindgen and fails on any difference with the committed file; after a change to the ABI, `OBC_REGENERATE_HEADER=1 cargo test -p obc-module-abi --test header` rewrites it. `c_compiler_accepts_the_header_with_rust_layout` compiles the header with `cc` as C11 (and with `c++` as C++17), with warnings as errors, and checks with `_Static_assert` that every structure's size and every field's offset are what Rust says. It prints a note and passes when the machine has no `cc` (or no `c++`).
- **The SDK** (`crates/obc-module-sdk`: 14 unit tests, 13 in `tests/vtable.rs` and 3 doc tests, one of which is only shown). The unit tests cover the JSON documents (the probe report and the settings description), the error kinds and their status codes, and the safe views of frames and requests (plane geometry, refused frames, parameters). `tests/vtable.rs` drives a module exported with `export_module!` exactly as a C host does, through the exported symbols and the vtable with raw ABI structures: init, probe and describe; load and process of a host NV12 frame; a panic becomes an error and the process survives; null pointers and structures smaller than the ABI's are refused; an entry refuses a bad host; the reason of a failed `init` belongs to its thread; a stage failure names its stage and keeps the chain loaded; a failed switch keeps the previous chain while other failures unload; `check` runs the module's deeper check.
- **The host** (`crates/obc-module-host`: 74 unit tests and 21 in `tests/modules.rs`). The unit tests are over the pure parts: the manifest (a full manifest, unknown keys refused, another ABI major version parsed but not loadable, field rules), discovery (directory order, override by id, a malformed or unreadable manifest as an entry with its reason, `OBC_MODULE_PATH`), the library checks (a file that is not a library, a library without the symbols, a missing library, the ABI version rules, a vtable read up to its `struct_size`), the settings model (a valid description, invalid settings dropped alone, limits), the registry (statuses and their reasons; providers by the `quality` and `battery` profiles, ties, pins that win and pins that cannot be honoured; the default order; order validation with the offending stages; a stored order that no longer fits; module and stage conflicts, module requirements and the force, listed under the next bullets), the crash ledger (two strikes in a row, a clean stop resets, a damaged file counts as empty) and the adapters (parameter checks against the description, error kinds). `tests/modules.rs` loads real libraries with `dlopen` as the daemon does: the hello example (discovered, loaded, probed, described, and a host frame tinted), and the fixture, which exports a library of another major version, of a newer minor version, and one that refuses the host (cargo features of the fixture), so that broken modules are unavailable with their reasons and the process lives; a panic in `instance_process` is an error and the process survives; a stage failure crosses as a stage error; matte stages never fail open; a failed switch keeps the previous chain processing; a library opened twice is shared; the effect, enhance and chain factories carry their stages through the ABI.
- **The Maxine module** (`crates/obc-module-maxine`: 19 unit tests, 15 in `src/lib.rs` and 4 in `src/framegen.rs`; 3 in `tests/module.rs`; and 6 hardware tests below, plus 1 in `src/gpu_tests.rs`). They need no SDK: the manifest is valid and declares its stages in the revision-8 order, the HDR look after relighting and before frame generation, on CUDA NV12 frames and with no cut-out needed (`the_manifest_is_valid_and_declares_the_maxine_stages`, `the_registry_offers_the_stages_in_the_rev8_order`); the library builds, loads and passes the ABI checks on a machine without the SDK and describes its settings (the `cutout`, `video` and `lighting` groups, with the enhancement method and the chair as parameters of the `enhance` and `background` stages), which pass the host's validation unchanged; the enhancement method is read from the stage's parameter and an unknown value is refused, a requested `ai` runs `fast` instead when only the upscale is installed while a requested `fast` is never replaced by `ai`, and the chair reaches the backend and the effect's `mode` does not; the probe reads a temporary SDK tree (a library and the models for the GPU make a stage available; relighting is unavailable whenever the cut-out is; the HDR look needs its TrueHDR library alone, whatever the GPU; with no root nothing can be known and every stage is offered); the configuration takes `sdk_root` and the reserved `_host` keys, and the configured root reaches the backend. Three tests of `obc-pipeline` that belonged to the Maxine default (that the default backend is Maxine, that a root that points nowhere makes it unavailable, that the configured root reaches it) are gone with it: the pipeline has no default backend and no root, and the root is the module's, tested here and in `obc-backend-maxine` (`src/ffi.rs`: the pin, and a different root refused once the SDK is loaded).
- **The `obcmodule` element** (`crates/gst-obc/tests/module.rs`: 27 tests, 22 in the default run and 5 `gpu_`). With a fake chain (`tests/support/fake_chain.rs`): registration and caps in system and CUDA memory; frames pass through until the chain is loaded and then every stage runs; every stage off is passthrough with no copy and no chain call; live parameters apply on the next frame without a load; a load-time parameter reloads the chain; a stage that fails to load or on a frame is dropped and the others run (and a frame it failed on runs again from the restored input, never from partly processed data); a chain that fails as a whole outputs the untouched input; a CUDA-only chain on system memory is refused with the reason; the new-source flag resets temporal state and a dropped stage is loaded again; stopping unloads and a restart loads again; without a factory every stage fails with the reason.
- **The daemon** (`crates/omarchy-broadcastd`, against the fake backend, with modules described without a library): `cargo test -p omarchy-broadcastd --lib module` runs the controller tests of the modules (the Maxine module offers the groups and stages of revision 8; with no module the camera passes through and no effect group is shown; a module from a `modules.d` directory shows its group and a write reaches its stage; broken, missing and mismatched modules are listed with their reasons; disabling a module is kept and applies at the next start; a module that crashed the daemon twice in a row is disabled until enabled; a plain video session marks no module in the ledger; a reload reopens only the modules that changed; the profile and pins are reported; the `sdk_root` alias and the module table reach the module; a background effect with no module to run it follows `on_failure`, below) and `dbus::tests::the_module_members_over_the_bus`. The pure model of the global settings is in `settings/effects/tests.rs`.
- **Conflicts, requirements and the override** (D-Bus revision 14). In `obc-module-host`, `manifest.rs`: `module_conflicts_and_required_modules_parse_and_are_checked` (the lists are module ids, each once, never the module's own, deduplicated in order). In `registry.rs`: `a_module_that_declares_a_conflict_with_an_enabled_one_is_refused`, `a_mutual_module_conflict_refuses_only_the_higher_id`, `the_cut_out_module_never_loses_a_module_conflict` (the other side is refused, with the lower id or none declared), `a_conflict_reason_bounds_the_name`, `a_conflict_between_two_modules_is_either_way_and_lifted_by_a_force`, `a_forced_side_lifts_a_module_conflict_and_is_reported`, `a_forced_conflict_is_reported_only_while_both_run`, `only_a_module_of_a_kind_the_host_loads_is_listed_forced`, `a_refused_module_is_never_opened` (with `tried_and_probed_tell_what_opening_gave`), `a_module_that_needs_a_module_not_enabled_is_refused_even_forced`, `a_module_that_needs_one_refused_by_a_conflict_is_not_refused_for_it` (the requirement is not transitive), `a_provider_whose_required_stage_or_slot_has_no_provider_cannot_run_it` (`Needs <stage>`, `Needs a <slot> stage`), `a_matte_or_post_matte_stage_runs_only_from_the_cut_out_module` and `a_stage_conflict_turns_off_the_loser` (the declarer wins, the earlier loses when both declare, `background` never loses, a force lifts it). In `obc-core`: `the_forced_table_is_read_and_checked` (`src/modules.rs`), `the_forced_table_merges_across_files_and_takes_writes` (`config_d/tests.rs`) and `the_module_rows_come_from_the_registry` (`config_schema/tests.rs`: a `modules.forced.<id>` row for each video module and none for an audio module). In `obc-ipc`: the `forced` key of the module model, read as `false` when missing or of another type (`forced_is_false_when_missing`), and the revision in `tests/mock_server.rs`. In the daemon (`controller/tests.rs`, `conflict_module` and `cutout_module` describe the modules): `a_module_in_conflict_with_an_enabled_one_is_listed_refused_and_never_opened`, `a_module_whose_required_module_is_not_enabled_is_refused`, `a_module_conflict_never_refuses_the_cut_out_module`, `a_module_conflict_that_leaves_no_cut_out_follows_on_failure`, `of_two_conflicting_stages_on_the_loser_is_dropped_and_the_feed_kept`, `a_stage_in_conflict_with_the_background_is_dropped_and_the_effect_kept`, `a_forced_module_runs_both_conflicting_stages_and_the_warning_says_so`, `a_module_refused_mid_session_drops_its_stage_and_keeps_the_feed_and_the_effect`, `a_refusal_mid_session_never_turns_the_background_effect_into_the_raw_picture` (the module that runs the effect is kept until the session ends, with the `unavailable from the next start` clause), `a_pin_or_profile_change_mid_session_keeps_the_held_module_until_the_next_start`, `a_failed_start_releases_the_hold`, `a_reload_reopens_the_held_module_when_it_changed` and `a_module_that_failed_to_open_is_not_opened_again_by_another_change`. In the CLI: `a_forced_module_says_so_in_text_and_json` (`modules.rs`) and `modules_force_is_shown_set_cleared_and_refused` (`tests/cli.rs`: `enable --force` writes the force first, a plain `enable` clears it, an audio module and an older daemon are refused).
- **The background effect is never left raw** (D-Bus revision 14). The element drops and takes back its backend (`gst-obc`, `src/effect/imp.rs`, no GPU): `a_revoked_backend_fails_the_effect_at_once`, `a_revocation_without_a_factory_does_nothing`, `a_factory_set_after_a_revocation_starts_a_new_request`, `a_factory_set_without_a_revocation_starts_no_request`, `a_revocation_under_none_fails_the_stages_alone_and_passes_through`, `a_rearm_forgets_the_stage_failures_of_the_revocation`, `a_rearm_fails_what_its_factory_refuses_of_the_settings_applied_meanwhile` and `a_job_built_before_a_revocation_publishes_nothing_loaded`; through the output (`obc-pipeline`, `src/output.rs`): `a_revoked_effect_backend_fails_at_once_and_a_new_one_rearms`; and, with a GPU and the fake backend (`gst-obc`, `tests/hardware.rs`, `gpu_` prefix): `gpu_a_revoked_backend_is_black_at_once_and_a_new_factory_loads_again`. In the daemon (`controller/tests.rs`, against the fake backend, which records the revocations and what is shown): `a_background_effect_with_no_module_to_run_it_follows_on_failure`, `a_session_with_no_module_keeps_its_backendless_element_and_needs_no_bare_start`, `a_session_started_with_its_effect_module_disabled_follows_on_failure_and_restarts_once_enabled`, `an_effect_whose_module_the_crash_ledger_disabled_follows_on_failure_not_a_fault`, `a_background_effect_whose_module_failed_to_open_follows_on_failure`, and, for the loss of the module of a running session, `a_session_that_loses_its_background_module_shows_black_or_the_image_at_once` (disabled, failed re-probe and failed reopen, each under `black` and `image`), `a_session_that_loses_its_background_module_under_none_shows_the_camera_image`, `a_session_whose_element_cannot_revoke_drops_the_effect_of_a_lost_module`, `an_effect_turned_off_and_on_while_its_module_is_lost_stays_hidden_and_comes_back`, `a_lost_background_stage_with_the_effect_off_keeps_the_stages_of_its_module` and `a_restored_backend_reads_as_loading_not_as_missing`.
- **The other crates.** `obc-core` (the `[modules]` and `[module."<id>"]` keys, the state file's `[modules]` and `[settings]` tables, the state over the config, the revision-8 typed keys read and written), `obc-ipc` (the `GetModules` model and its tolerant reading, and the six members and two properties against a mock server) and the CLI (`modules.rs` unit tests for the text and the bounded JSON lines; `tests/cli.rs`: `modules_and_order_list_change_and_refuse` and `modules_against_an_older_daemon_say_they_are_unsupported`).
- **The example.** `examples/module-hello` has 9 tests of its own (chroma moves with strength and stays in range, luma is untouched, the description, load and live strengths, refused frames, row padding). CI runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test --locked` on it, outside the workspace.

### GPU tests with a fake chain (`gpu_` prefix)

An NVIDIA GPU and the GStreamer CUDA elements; no SDK and no camera.

```sh
cargo test -p gst-obc --test module -- --ignored gpu_ --test-threads=1
cargo test -p obc-pipeline --test hardware -- --ignored gpu_module_stages_run_around_the_effect_on_cuda_frames --test-threads=1
```

The five `gpu_` tests of the element: a CUDA chain runs in place on CUDA frames; a host chain runs on CUDA frames through the host copy; every stage off passes CUDA frames through without touching CUDA; a CUDA chain that fails halfway restores the frame, and so does a host chain through the host copy. The pipeline test runs a host chain before and a CUDA chain after the effect element (with the fake effect backend) on a white source: the sink sees the source until the first chain loads, then each chain's lift, and again the first one's when the second is turned off; one caps event in all.

### Maxine module hardware tests (ignored)

The built Maxine module, loaded through the host as the daemon loads it, on CUDA frames in a context created like GStreamer's: blur and replace, noise removal, relighting, enhancement and the HDR look through the ABI (6 tests in `crates/obc-module-maxine/tests/hardware.rs`; the HDR look's are described under [Maxine hardware tests, the HDR look](#maxine-hardware-tests-the-hdr-look)). They need the Maxine environment of [Maxine hardware tests](#maxine-hardware-tests) (the SDK root, the os-release shim through the cargo runner) and an NVIDIA GPU:

```sh
set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$MAXINE_VFX_OS_RELEASE_SHIM"
cargo test -p obc-module-maxine --test hardware -- --ignored --test-threads=1 --nocapture
```

The frames are synthetic, so the tests assert what holds for any cut-out. The daemon's hardware tests (`crates/omarchy-broadcastd/tests/hardware.rs`, [Daemon effects test](#daemon-effects-test-ignored)) check first that the SDK can be loaded, then build the Maxine module (`cargo build -p obc-module-maxine`, in the profile of the test binary), write its manifest with the library's path into a directory and give it to the daemon they spawn as `OBC_MODULE_PATH`.

### Maxine AR module tests (ignored)

The module `br.com.dias.maxine-ar` ([ADR-0028](adr/0028-maxine-ar-module.md), `crates/obc-module-maxine-ar`) has a default suite that needs no SDK and no GPU:

```sh
cargo test -p obc-module-maxine-ar
```

It covers the manifest, the probe against temporary SDK trees (each stage's reason, by missing core library, feature or model), the configuration keys and the root precedence (`MAXINE_AR_ROOT`, then `sdk_root`, then the default), the settings, the framing arithmetic (the crop, the dead zone, the smoothing, the hold and the return to the full frame, at many frame sizes), the order the stages run in, and the exported library through the C ABI on a machine without the SDK.

The hardware tests are `#[ignore]` and need the AR SDK, the os-release override and an NVIDIA GPU. They load the built library through the host as the daemon does, on CUDA frames in a context created like GStreamer's. Four test binaries: `hardware` (the two stages), `hardware_vfx_first` and `hardware_ar_first`, which load the Video Effects module and the AR module in one process in the two possible orders (the two SDKs ship a `libNVCVImage` of the same soname), and `hardware_cxx_runtime_first`, which loads the system `libstdc++` locally before the AR module, as GStreamer's plugins do in the daemon, and pins that the face-box model still loads (the SDK exports a private GCC 8 `libstdc++`, and the module opens the system one globally first); each is a binary of its own so that the load order is that of a fresh process. They need both SDKs' environment files:

```sh
set -a
. /usr/share/maxine-ar/maxine-ar.env
. /usr/share/maxine-vfx/maxine-vfx.env
set +a
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env -u LD_LIBRARY_PATH LD_PRELOAD=/usr/lib/maxine-ar/os-release-shim.so MAXINE_OS_RELEASE=/usr/lib/maxine-ar/fake-os-release" \
  cargo test -p obc-module-maxine-ar --test hardware --test hardware_vfx_first --test hardware_ar_first --test hardware_cxx_runtime_first -- --ignored --test-threads=1 --nocapture
```

`OBC_TEST_FACE_IMAGE` names a binary PPM (P6) still of a face, 1920x1080 (`magick face.jpg face.ppm`), that the face cases run through the stages; without it those cases are skipped and the others run. For auto-framing the face should be off centre and not so large that the setting under test cannot zoom: a face box is framed when it is under about 60 % of the height with Tight, 48 % with Medium and 41 % with Wide (the zoom must pass the dead zone's 1.087), so a face at desk distance (35 to 45 %) is framed by Tight and Medium, and a smaller face shows the zoom better. `OBC_TEST_FACE_OUT`, when also set, writes the luma of the results as PGM files for a look at the pictures. The tests assert what holds for any face: a frame with no face passes through unchanged, a face changes the picture, an auto-framed picture is framed after a number of frames while the first frame is not, a temporal reset keeps the stages, and in both load orders each module's effect works with the other's loaded; they do not judge how the result looks. Some of them also print timings, which are for a local comparison and are not published.

## The microphone and audio modules

The Omarchy Broadcast Mic ([ADR-0029](adr/0029-audio-modules-mic-host-and-worker.md), [setup/microphone.md](setup/microphone.md)) is tested in layers, from the transport and the real-time plugin to the worker and the daemon's façade, so that everything but a real PipeWire runs on any machine with no microphone, no GPU and no camera. The test signals are made in the tests (a speech-like voice of syllables, and noise), so no recorded audio is committed.

### Default suite (no PipeWire, no GPU, no camera)

```sh
cargo test --locked -p obc-audio -p obc-mic-shim -p omarchy-broadcast-audiod -p obc-module-voice-cpu -p obc-module-afx
cargo test --locked -p obc-module-abi -p obc-module-sdk -p obc-module-host
cargo test --locked -p obc-core -p obc-ipc -p omarchy-broadcastd -p omarchy-broadcast
cargo test --manifest-path examples/module-hello-audio/Cargo.toml --locked
```

- **The transport** (`crates/obc-audio`). Unit tests of the pieces: the delay line, the reblocker (quanta that are not multiples of 480), the crossfade, the transport-delay formula for every quantum and depth, the shared region (its geometry and seals, the rings, counters that are implausible reported as corruption) and the hello and reply encoding. `tests/handshake.rs` runs the handshake and the rings across a real socket with both halves of the API: connect, accept and matching sequence numbers; a malformed hello and a hello of another version refused with the worker's version; a short memfd refused at accept; refusals and bad accepts seen by the client; a worker that cannot make the wake socket block the plugin; the wake reaching the worker and a closed wake read as a hang-up; and a connect that is cancellable and never blocks on a full backlog.
- **The forwarding plugin** (`crates/obc-mic-shim`, `tests/host.rs`). A LADSPA test host goes through `ladspa_descriptor` and the descriptor's function pointers exactly as filter-chain does, against an in-test fake worker built on the worker side of `obc-audio`. Every `run()` call is made with a **counting allocator** armed on the calling thread: an allocation or a free inside `run()` fails the test, which is the plugin's real-time rule. The tests assert bit-exact passthrough at a constant delay with no worker; processed output for quanta from 32 to 2048 samples, including non-multiples of 480; a missed block crossfaded to the aligned raw audio and counted; worker loss crossfaded and the reconnect; an idle plugin that does not reconnect; a refused version that stays in passthrough; bypass; a stalled worker dropped and retried; larger runs than the maximum quantum that stay raw; quantum changes that keep the largest transport delay; a cleanup during a hung handshake that returns promptly; corrupt counters, wrong sequence numbers and absurd latencies from a misbehaving worker; and the descriptor's ports.
- **The worker** (`crates/omarchy-broadcast-audiod`). Unit tests of the snapshot (a missing, damaged or foreign file gives the defaults and one bad field keeps the others), the chain (failing open on a stage error, a panic and an overrun) and the plan (rank, pin, profile, conflicts, the GPU gate) and, without PipeWire, of the graph decisions: the device list, the consumers (a level meter is not one), and the metadata writes of "use as default microphone" and its restore. `tests/worker.rs` runs the worker's binary as systemd does (the test holds the listening socket and hands it over as descriptor 3) with real module libraries (the audio example and the host's fixture module, built with `--locked`) and a fake mic plugin that drives the rings at simulated quanta: blocks come back in order with their sequence numbers; a second plugin is refused while one is connected; a reload switches chains without a gap; a stage over its deadline fails open after 50 blocks; a stage error fails only that stage; **after `kill -9` a restarted worker processes again**; the worker exits when idle (and not while calls arrive); **a module that crashes twice is disabled and the microphone stays live**; a module that hangs in a block aborts the worker with a strike and a clean stop leaves no marker; a missing or corrupt snapshot gives the defaults; the control interface lists, describes and applies; the pipeline depth follows the devices unless overridden and is negotiated again when a deeper one is needed; the GPU policy is enforced and `with-camera` is decided per session; a crash-disabled module is enabled again by `ResetModule`; and a chain that fails while the next warms up hands over to it. The fixture module's behaviour is set by `OBC_FIXTURE_AUDIO_FAIL_LOAD`, `_FAIL_AT`, `_ABORT_AT` and `_SLEEP_MS`.
- **The CPU module** (`crates/obc-module-voice-cpu`). `tests/quality.rs` runs both engines on the generated voice and noise: the noise is removed and the voice kept (the signal-to-noise ratio improves, and the pauses between syllables get quieter), DeepFilterNet runs with its stage skipping off, the strength sets how much noise goes, the output is the same on every run and after a reset, the delay is constant and equals the reported latency at every strength and across digital silence, digital silence in gives exact zeros out, a non-finite sample never poisons the output, a live strength change makes no step, and a block of another size is refused. `tests/module.rs` builds the library and opens it with `obc-module-host` as the worker does: the manifest, the settings on the `mic` surface, the engine reload and the live strength, mono 48 kHz in blocks of 480 only, a load on one thread and processing on another, a new source resetting the state, and the video daemon's registry listing the module without opening it. `cpu_at_real_time` (ignored) measures the CPU an engine takes at real time; it prints, and asserts nothing: `cargo test --release -p obc-module-voice-cpu --test quality -- --ignored --nocapture cpu_at`.
- **The ABI, the SDK and the host.** The header test regenerates `include/omarchy_broadcast_module.h` and checks the new audio structures' sizes and offsets with a C and a C++ compiler, as for the video ones. `crates/obc-module-sdk/tests/audio_vtable.rs` drives a module exported with `export_module!(_, audio)` through the vtable as the worker does: the audio entries are filled, a block is processed with the load-time and live parameters, an audio-only instance refuses video, a stage failure names the stage, a failed switch keeps the chain, and small or null audio structures are refused. In `crates/obc-module-host/tests/modules.rs`, an audio module loads only through the audio kinds and runs blocks, an audio module without the audio entries and a video module in the worker are refused, the audio fixture fails and aborts as its environment says, and the example is discovered in the worker and shapes the voice.
- **The daemon and the CLI.** `obc-core` (the `[mic]` rows of the config schema and the example configuration: a test reads `docs/config.example.toml`, so the example must stay a valid configuration), `obc-ipc` (the typed `MicStatus` and the mock server's Mic members), the daemon (`src/mic/tests.rs`: the `mic.*` groups and their greyed reasons, the device tokens and their collisions, what each write does, the snapshot, the status mapping, the merge of the worker's modules into `GetModules`; the controller and D-Bus tests for the properties, `SetMicHost` and `MicStats`), and the CLI (`tests/cli.rs`: `mic`, `mic on|off|device|default|restore-default`, the `mic` line and the snapshot's `mic`, against an older daemon; the bounded JSON lines in `json.rs`, including the worst case: with every string at its bound a `snapshot` lists 6 of the 12 cameras). `crates/omarchy-broadcastd/tests/mic_bus.rs` runs the daemon's bus clients against fakes on a private D-Bus daemon (the worker's internal interface, and the systemd user manager); the private bus has no service activation and no systemd, so the user's own worker and service manager are never reached.
- **The example.** `examples/module-hello-audio` has unit tests of its own (the gain, the tone, the description, the loads), and CI runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test --locked` on it, outside the workspace, as for `examples/module-hello`.

### The microphone in a private PipeWire (ignored)

The one test that needs a PipeWire server runs the real mic host (`dist/pipewire/omarchy-broadcast-mic.conf`, loading the plugin built from `obc-mic-shim`) and the worker's PipeWire control against a **private, headless PipeWire and WirePlumber**. It needs `pipewire`, `wireplumber`, the `pw-cat`, `pw-cli`, `pw-dump` and `pw-metadata` tools, `dbus-run-session` and `python3`, and is ignored by default:

```sh
cargo test -p omarchy-broadcast-audiod --test pipewire -- --ignored
```

The test re-runs itself inside `scripts/mic-pw-sandbox.sh`, which guarantees that **the user's session is never touched**:

- everything runs inside `dbus-run-session`, on a private session bus that listens inside the sandbox and activates nothing;
- `XDG_RUNTIME_DIR`, `PIPEWIRE_RUNTIME_DIR` and the XDG config, state, data and cache directories all point into a fresh temporary directory that is removed at exit, so neither the user's PipeWire socket, nor `~/.config`, nor WirePlumber's saved state is read or written; the worker's socket is the sandbox's too;
- the PipeWire server loads no device support (no ALSA, bluez, v4l2 or libcamera, no udev), and WirePlumber runs its policy profile with an isolated configuration, with no hardware monitors;
- the server carries two virtual microphones of equal priority, fed by `pw-cat --playback` with a test tone, one of them the configured default source;
- the inner run refuses to start unless every directory PipeWire, WirePlumber and the worker use is the sandbox's temporary one.

It covers what no fake can: the device list (both virtual microphones, never the Omarchy Broadcast Mic's own nodes); "use as default microphone", which makes the node the default and pins the capture side to the previous default, and again to the other microphone, with the previous default still the first and never the Omarchy Broadcast Mic; the pin surviving a restart of the mic host (WirePlumber's restore-stream sets it on the new capture node) and of the worker; consumers (a level meter is not one, a recorder is); the node's `ProcessLatency` published for the graph's quantum; and the restore, which makes the first microphone the default again and removes the pin, idempotently and across a mic host restart. The sandbox script can run any command, so another PipeWire test is `scripts/mic-pw-sandbox.sh -- COMMAND` (`--plugin` and `--conf` choose the plugin and the mic host configuration).

### The Maxine audio effects module

`crates/obc-module-afx` (`br.com.dias.maxine-afx`, [setup/microphone.md](setup/microphone.md#on-an-nvidia-gpu-maxine-audio-effects)) runs the NVIDIA Maxine Audio Effects SDK, which cannot be in this repository, so its default suite runs over a **stand-in SDK** and needs no NVIDIA software, GPU, PipeWire or camera:

```sh
cargo test --locked -p obc-module-afx
```

The stand-in is `crates/obc-module-afx/tests/fixtures/mock-afx`, a small library of its own (built by the tests with `cargo build --locked` into a directory under `CARGO_TARGET_TMPDIR`; its `Cargo.lock` is committed and must not be rewritten). It contains no NVIDIA code: it exports the entry points the module calls, as effects that are a delay and a gain, finds each effect's feature library by name on the library search path as the real SDK does, logs every call, and is steered by a file beside it (which call fails, each effect's delay and gain). A fixture SDK tree (a core library, a feature library and model files per effect) is made for each test.

- **Unit tests** (20, in `src/`): the planning of a chain from its stages (which effect each stage is, where a strength is honoured, the combined effect where installed, unknown and repeated stages refused, a failed combined effect naming both stages), the delay-matched mix and its one-block ramp, the delay measurement (a probe signal, a delay found to the sample), the SDK's file layout and the probe's reasons (the core library, the driver, a feature, its models), the root precedence, the configuration keys (`sdk_root` and `_host`; any other refused), the profiles' mapping and the strengths read and clamped, the search-path check and that the audio entries are exported.
- **The module as the worker sees it** (`tests/module.rs`, 26 tests), through `obc-module-host` with the audio kinds: the manifest declares three GPU stages ranked above the CPU provider; the worker opens the library and the video daemon's registry lists it without opening it; the probe and the check read the tree and touch no effect; the probe names what is missing; **the configured root comes first**, then `MAXINE_AFX_ROOT`, then the default root, and a missing SDK is said; the settings of the `mic` surface and their greyed reasons; noise removal as the version 2 model with its parameters set in order; a lower strength mixes in the delayed input; **a strength is live**, changing the mix between blocks with no reload and no change of the delay, including on the combined model; a request of the same effects keeps them in place (a switch); noise and echo together run as the combined model, and as two effects one after the other when it is absent or fails to load; studio voice takes its profile after the load; a stage that fails to load fails alone and leaks no effect; an effect that fails on a block is replaced by its delay and the rest runs; a delay that cannot be measured fails the stage; a feature library that cannot be found fails its stage; unload and drop destroy every effect; a new source resets the effects; only mono 48 kHz blocks of 480 samples are taken; load, process and unload on three threads; and the search path: libraries reached through links are found, and **without the search path every stage fails with a reason that names the drop-in**.

Because the dynamic linker reads `LD_LIBRARY_PATH` once, when a process starts, a test that uses the effects runs in a child process of the test binary started with the path leading to its fixture tree, as the worker's drop-in starts the worker.

**Hardware tests (ignored).** `crates/obc-module-afx/tests/hardware.rs` runs the module over the real SDK and an NVIDIA GPU: the probe and the check find every stage, every chain loads, runs and reports the delay the output has, and a switch in place keeps the effect. Each test does nothing unless `MAXINE_AFX_ROOT` names the SDK, and the process must start as the worker's drop-in starts the worker, with the package's environment file (whose `LD_LIBRARY_PATH` the module requires) and the os-release shim preloaded:

```sh
cargo test --manifest-path crates/obc-module-afx/Cargo.toml --no-run
set -a; . /usr/share/maxine-afx/maxine-afx.env; set +a
env LD_PRELOAD=$MAXINE_AFX_OS_RELEASE_SHIM <the hardware test binary> --ignored --nocapture --test-threads=1
```

Each chain is loaded on one thread, run on another and unloaded on a third, as the worker does, with the strengths passed live. The tests assert only what must hold anywhere (the stages load, the output is finite and not silent, the reported delay is the delay the output has, every effect is destroyed); they print what they measured, and those figures are for local comparison and are not published.

## The system-memory pipeline

These are the tests of [ADR-0033](adr/0033-system-memory-pipeline.md): the video pipeline, the effect element and the module adapters on frames in system memory, and the choice of memory. The default suites need no GPU, no camera and no SDK, and they pass on a machine where CUDA is hidden.

```sh
cargo test --locked -p obc-gpu -p obc-core -p obc-backend -p gst-obc -p obc-module-host -p obc-module-onnx -p obc-pipeline -p omarchy-broadcastd
cargo test --locked -p obc-pipeline --test system_memory                  # one whole session in system memory
CUDA_VISIBLE_DEVICES="" cargo test --locked -p obc-pipeline -p gst-obc    # the same, with CUDA hidden
```

By area (test names are in the files named; counts change with every release and are not recorded):

- **The probe** (`obc-gpu`, `src/presence.rs`): the probe as a pure function over a hand-made environment (every spelling of `CUDA_VISIBLE_DEVICES` that hides the GPUs, a missing driver directory and a directory with no GPU told apart, a library that does not open, the CUDA elements not registered), the reasons verbatim, and one test on the real machine that the probe opens no `/dev/nvidia*` descriptor and maps none.
- **The setting** (`obc-core`, `src/config.rs`, `src/config_schema.rs`): `[video] memory` reads `auto`, `cuda` and `system`, defaults to `auto` and refuses another value naming the key; the schema row (an enum, editable, applies at the next camera start) and its description, which states the memory in force and why; `docs/config.example.toml` holds the key (the test of [Capture and output modes](#capture-and-output-modes-frame-generation-and-the-privacy-fallback) that every schema key appears in the example lists `memory`).
- **The ingest and the output** (`obc-pipeline`, `src/ingest.rs`, `src/output.rs`, `src/pipeline.rs`): the system-memory tail and every camera path (NV12, YUY2 with no extra `videoconvert`, MJPEG from a `jpegenc` source) hold no CUDA element and stream canonical frames; an I420 contract adds `videoconvert` and nothing else; no enhance stage and no roundtrip stage in system memory; the effect element and module stages are accepted around it. **The range**: a full-range MJPEG black reaches the canonical caps at Y=16 under a limited contract, whites at 235, chroma 16 to 240 with neutral at 128 (`system_memory_mjpeg_is_converted_to_limited_range`, `system_memory_mjpeg_chroma_is_scaled_to_the_limited_range`), and a full-range contract adds no balance step (`full_range_contract_adds_no_balance_step`). If a GStreamer update starts converting the range itself, these tests are what show the adapter has become redundant. `tests/system_memory.rs` runs a session through the public API a daemon uses (a YUY2 test source, the real effect element with the fake host backend of `gst-obc`, the output forwarding to a loopback writer): frames arrive, the fake backend is called on host frames only, no `nvcodec` element is in either pipeline, and a CUDA-only backend fails the effect with its reason.
- **The effect element** (`gst-obc`, `tests/element.rs`, the `host_` tests, over the fake backend in `tests/support/fake_backend.rs`, which has host methods): the pads offer both memories; `none` passes the input buffer through with no copy; black is the black of the caps' range, exactly (Y=16, UV=128 in the buffer); `on_failure` black, image (the image's bytes in the output) and none (the input's bytes); turning an effect on mid-stream keeps the input until the effect loads and then switches, never showing the input after black or an effect frame went out; a start and a new source while the effect loads are black; a revoked backend is black from the next frame and a new factory loads again; a backend that lost its chain loads again on a retry; the repeat cache re-sends the processed buffer and runs no effect; the input is never modified; each stop unloads the backend; a CUDA-only factory fails the effect with its reason and follows `on_failure`. The pixel operations are unit-tested in `src/video.rs` (the fill, the row-by-row copy and the packed copy touch only the visible bytes and leave the padding; an odd size is refused and writes nothing). The CUDA path was moved into the code the two memories share, so the CUDA suites must run after a change to it ([GPU tests](#gpu-tests-gpu_-prefix)).
- **The seam and the adapters** (`obc-backend`, `src/lib.rs`: a CUDA-only backend refuses the host methods with "takes CUDA frames only"; `obc-module-host`, `tests/modules.rs` and `src/registry.rs`): the adapter issues `OBC_FRAME_HOST` with null CUDA handles through the real ABI (`the_backend_factory_carries_the_effect_on_host_frames_through_the_abi`), `kinds` comes from the manifest, a CUDA-only `background` takes no host frames, and a call of the other kind on a loaded chain is refused. The registry in system memory: a CUDA-only stage is unavailable with each reason, a provider that lists `host` is chosen over a higher-ranked CUDA-only one, a pin to the CUDA-only one falls back and says why, a module with no runnable stage is unavailable, a probe that already said no keeps its own reason, and CUDA memory filters nothing.
- **The ONNX module** (`obc-module-onnx`, `src/engine.rs`, `tests/module.rs`): with `_host.cuda = false`, `auto` skips TensorRT and CUDA and tries the rest in order with the reason "CUDA is off: the video pipeline runs in system memory", an explicit `cuda` or `tensorrt` fails the load with it, and no CUDA call is made (the TensorRT cache key, which reads the device, is not asked). `tests/runtime.rs` needs an ONNX Runtime and the models, as before; with the CPU build, `ORT_DYLIB_PATH=/path/to/libonnxruntime.so OBC_ONNX_MODEL_DIR=/path/to/models cargo test -p obc-module-onnx` runs it on the CPU.
- **The daemon** (`omarchy-broadcastd`, `src/controller/tests.rs`, `src/modules.rs`, `src/backend/park.rs`, with the fake backend and fake clock): `auto` with no usable CUDA runs in system memory and the fake backend sees no CUDA call, no unpark and no park; the plan in system memory contains no enhance and no frame generation, and the cut-out is the ONNX one; the modules' configuration carries `_host.cuda = false` and is re-opened when the memory changes; `system` set in the configuration gives the "set to system memory" reason and `cuda` never falls back; **`auto` falls back to system memory once when the CUDA output wants CUDA** (`auto_falls_back_to_system_memory_once_when_the_cuda_output_wants_cuda`, `the_fallback_to_system_memory_is_tried_once_only`) and never for an unpark failure or a contract fault; a start that failed for want of CUDA is not parked (the `Parker` tests); no "(generated)" output mode is offered; a change of `video.memory` applies at the next camera start while a running session keeps its memory; with only a CUDA cut-out installed the effect fails and follows `on_failure`; an unclean stop of a system session warns and does not taint.

### GPU regression (ignored)

The CUDA path now shares code with the system-memory one, so after a change to the effect element, the output or the session run the CUDA suites on a machine with a GPU, as above ([GPU tests](#gpu-tests-gpu_-prefix), [Hardware tests](#hardware-tests)):

```sh
cargo test -p gst-obc --test hardware --test module -- --ignored gpu_ --test-threads=1
OBC_TEST_CAMERA=046d:0944 OBC_TEST_SINK=/dev/video11 \
  cargo test -p obc-pipeline --test hardware -- --ignored --test-threads=1
```

`output_holds_the_contract_frame_rate` can fail on a camera that delivers 20 fps in dim light (its input is then 20 fps against a 30 fps grid and the arrival jitter is near one period); it passes on a rerun and is not tied to the memory.

### The daemon with CUDA hidden (manual)

The pipeline in system memory is checked on a machine that has an NVIDIA GPU by hiding it from the daemon. Start a development daemon on a free loopback with `CUDA_VISIBLE_DEVICES=""` in its environment (a transient `systemd-run --user` unit with `--setenv=CUDA_VISIBLE_DEVICES=`, or the development script's environment), the ONNX module installed with the CPU build of ONNX Runtime (`ORT_DYLIB_PATH`, or `ort_library` in the module's table) and the models, and the Maxine module installed too. Expect:

1. The log says `video memory: system (auto: CUDA_VISIBLE_DEVICES hides every GPU)`, and `omarchy-broadcast config show` describes `video.memory` as "Now: system memory, CUDA_VISIBLE_DEVICES hides every GPU. Applies at the next camera start."
2. `omarchy-broadcast modules` lists every Maxine stage unavailable with "Needs an NVIDIA GPU (CUDA)" and `background` provided by the ONNX module; the settings tree greys eye contact, auto-framing, noise removal, enhancement, the HDR look and relighting with that reason; `output.mode` offers no "(generated)" rate.
3. With an application (or `gst-launch-1.0 v4l2src device=/dev/video11 ! fpsdisplaysink video-sink=fakesink`) reading the virtual camera: the camera at its rate, then `effect blur`, `replace`, `none`. The daemon's `/proc/<pid>/maps` has no `libcuda`, `libgstcuda`, `nvcodec`, `libnvinfer` or `libcudart`, before and after a session, and `/proc/<pid>/fd` has no `/dev/nvidia*`. The ONNX module logs TensorRT and CUDA as unavailable with "CUDA is off: the video pipeline runs in system memory".
4. The privacy checks, with frame dumps of the virtual camera and no person in front of the camera: blur requested at the camera start shows black and then the blurred picture, never the sharp one; turning blur on mid-stream shows the sharp picture and then the blurred one with no black frame between; with the model file missing and `on_failure = "black"` every frame is black, exactly Y=16 and UV=128; disabling the module mid-session gives black from the next frame; `on_failure = "image"` shows the image; `on_failure = "none"` shows the camera.

With CUDA visible, `[video] memory = "system"` in a file of `config.d/` gives the same without the hiding: expect `video memory: system (set in the configuration)`, the Maxine stages reading "Needs CUDA; the video pipeline is set to system memory", no `/dev/nvidia*` descriptor and no `libcuda` map during or after a blur session, and the GPU missing from `/proc/driver/nvidia-uvm/gpus` for the process.

**The fallback.** `CUDA_VISIBLE_DEVICES=7` (a GPU that is not there) passes the probe but not the CUDA output, so it exercises the retry: with a consumer on the virtual camera, the log shows `video memory: cuda (auto)`, then one warning, "the CUDA video pipeline could not start (...): starting in system memory until the next reload", then `video memory: system (auto: CUDA could not start: ...)`, the ONNX module skipping TensorRT and CUDA, and the daemon stays active (an earlier build crashed inside `libcuda` here, when a failed start was parked).

### Measured on the reference machine

The CPU figures, with the method, are in [benchmarks/system-memory.md](benchmarks/system-memory.md). They were measured by hand on the stated machine; no test asserts them.

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
