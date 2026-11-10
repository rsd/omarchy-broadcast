# omarchy-broadcast documentation

| Doc | What it is |
|---|---|
| [architecture.md](architecture.md) | Mermaid diagrams: component overview, stage/caps model (ingest, process, output) with capability-driven source selection, persistent loopback writer, daemon lifecycle, threads, D-Bus sequence |
| [setup/loopback.md](setup/loopback.md) | Creating the "Omarchy Broadcast" v4l2loopback virtual camera: `modprobe` options, reload, and why `exclusive_caps=1` |
| [setup/daemon.md](setup/daemon.md) | Installing and running `omarchy-broadcastd` as a systemd user service: `cargo install`, unit and optional D-Bus activation and WirePlumber drop-in files, configuration and state files, reload, logs, and the optional PipeWire camera output (build feature, configuration, checks, application settings, hiding the duplicate camera) |
| [dbus-api.md](dbus-api.md) | Reference for `br.com.dias.OmarchyBroadcast1` (revision 6): states, properties, methods, errors, video effects, camera image controls, live preview, the `Stats` signal, activation without starting the daemon, `busctl` and `gdbus` examples |
| [cli-json.md](cli-json.md) | The `omarchy-broadcast` commands (including `effect`, `denoise`, `preview` and `controls`), exit codes and the JSON line format (`status`, `cameras`, `controls --json`, `watch --json`, `preview --json`) for clients that cannot speak D-Bus |
| [config.example.toml](config.example.toml) | Every `config.toml` key with its default, as a commented example |
| [setup/maxine.md](setup/maxine.md) | Using the NVIDIA Maxine VFX SDK with omarchy-broadcast: requirements, building `maxine-vfx-sdk-local` (from the omarchy-nvidia-sdks repository), the daemon drop-in and the os-release shim, the environment the daemon reads, an SDK tree without the package, selecting the effects and noise removal, troubleshooting, uninstall |
| [setup/microphone.md](setup/microphone.md) | Optional noise-removed virtual microphone "Omarchy Broadcast Mic": a PipeWire filter-chain with an RNNoise (default) or DeepFilterNet LADSPA plugin, run per engine by the `omarchy-broadcast-mic@.service` user unit; install from the Arch package, `scripts/install-user.sh --mic` or `dist/`, selecting and pinning the microphone, choosing and tuning the engine, troubleshooting, undo |
| [research/maxine-linux.md](research/maxine-linux.md) | Technical notes on Maxine on Linux: distribution, licence, versions, Blackwell support, features, API shape, existing projects |
| [benchmarks/passthrough.md](benchmarks/passthrough.md) | Passthrough and roundtrip results: pipeline, latency method, output format, camera frame-rate caveat, teardown finding; raw results are the JSON files alongside |
| [benchmarks/ingest-paths.md](benchmarks/ingest-paths.md) | Camera and ingest-path results: GPU-direct, CPU convert and CPU decode cost on two cameras, with variance under load |
| [benchmarks/io-split.md](benchmarks/io-split.md) | Ingest / Output split results: output frame rate over 10 minutes, live camera switching with an external consumer, crop versus letterbox, cost per ingest path against the single pipeline, and the phase-locked clock with the held frame at a switch; raw results are the JSON files alongside |
| [testing.md](testing.md) | Unit tests, hardware tests (environment variables), the daemon, CLI and `obc-ipc` tests, measured daemon figures, the video-effects tests (fake backend, Maxine hardware tests), discovery by PipeWire, the PipeWire output's unit and live tests, video noise removal tests, the live preview and camera image controls tests, the `obc-bench` flags and the manual OBS/Chrome and daemon tests |
| [benchmarks/dgpu-idle.md](benchmarks/dgpu-idle.md) | dGPU idle check after teardown: findings and the quiet-desktop procedure |
| `../scripts/dgpu-idle-check.sh` | Reads sysfs and `/proc` only (never NVML) to report whether the dGPU runtime-suspends and who holds it awake |
| `obc-bench` (`crates/obc-pipeline`) | Development binary: camera, then CUDA memory, then v4l2loopback, with latency, fps and GPU stats as JSON; not shipped |

## Architecture decision records

| ADR | Decision |
|---|---|
| [0001](adr/0001-rust.md) | Rust for daemon, elements, clients |
| [0002](adr/0002-gstreamer-engine.md) | GStreamer (gstreamer-rs) as the media engine |
| [0003](adr/0003-maxine-first-behind-backend-seam.md) | Maxine first, behind an `InferenceBackend` seam |
| [0004](adr/0004-v4l2loopback-first.md) | v4l2loopback is the first sink |
| [0005](adr/0005-dbus-zbus-control-plane.md) | D-Bus via zbus as the control plane |
| [0006](adr/0006-on-demand-gpu-lifecycle.md) | On-demand lifecycle; idle means zero CUDA contexts |
| [0007](adr/0007-headless-daemon-toml-config.md) | Headless daemon with a TOML config file |
| [0008](adr/0008-cpu-ingest-adapters.md) | CPU ingest adapters for MJPEG and YUYV/UYVY |
| [0009](adr/0009-ingest-process-output.md) | Ingest, Process and Output stages joined by a latest-frame slot; the output has a fixed contract and its own clock |
| [0010](adr/0010-phase-locked-output-clock.md) | The output clock locks the phase of its ticks to the camera (timestamps stay on the exact grid), a source switch holds the last frame, and the output queue holds at most 3 frames |
| [0011](adr/0011-daemon-lifecycle-and-dbus-api.md) | Daemon lifecycle (`off`/`idle`/`starting`/`active`/`error`), a persistent loopback writer, consumer detection, runtime state file, and the D-Bus v1 API; amends 0006 and 0007 |
| [0012](adr/0012-video-effects-and-maxine-backend.md) | Video effects (none, blur, replace) in the Process stage as one always-present `obceffect` element, the `InferenceBackend` seam with a dlopen'd Maxine backend, models loaded only while an effect is on, black rather than the raw image while an effect starts or fails, and an `LD_PRELOAD` shim for the SDK's os-release check; extends the D-Bus API to revision 2 |
| [0014](adr/0014-mic-noise-removal-filter-chain.md) | Accepted: microphone noise removal as a PipeWire filter-chain hosting a CPU LADSPA plugin (RNNoise by default, DeepFilterNet for higher quality), one dedicated `pipewire -c` process per engine under a template user unit, independent of the daemon; Maxine Audio Effects deferred |
| [0015](adr/0015-cadence-emission-for-sub-rate-inputs.md) | A steady camera slower than the contract (1.1 to 1.9 output periods per frame, such as 20 fps into 30) is followed by emitting each tick at the predicted arrival of its frame; amends 0010 |
| [0016](adr/0016-pipewire-output.md) | An optional native PipeWire `Video/Source` node next to the loopback, fed by a tap on the loopback writer (same stream, pacing and timeline), with three offered formats and CPU conversion; PipeWire links count as consumers; D-Bus revision 3; amends 0004 and 0011 |
| [0017](adr/0017-cuda-checkpoint-parking.md) | The daemon checkpoints its own process with the CUDA checkpoint API when a session has stopped, so Idle after an activation holds no `/dev/nvidia*` fds and no UVM registration; replaces the worker-process fallback; amends 0006 and 0011 |
| [0018](adr/0018-video-noise-removal.md) | Video noise removal (the Maxine `nvvfxdenoising` feature) as a boolean stage that stacks with the effects and runs before them, not a fourth effect; it fails open, because it hides nothing; temporal state is reset by a flag the output clock sets on a new source; D-Bus revision 4; amends 0012 and extends 0011 |
| [0019](adr/0019-live-preview.md) | A live preview for clients: a lease (D-Bus revision 5, `AcquirePreview`) that counts as a consumer, small JPEG frames encoded from the writer's tap into a private file in `$XDG_RUNTIME_DIR` and announced with a signal, the `omarchy-broadcast preview` command; extends 0011 and 0016 |
| [0020](adr/0020-camera-image-controls.md) | Per-camera image controls (brightness, contrast, saturation, sharpness, backlight compensation) from a camera-agnostic allowlist, ranges from the device, values stored by camera id and written whenever that camera opens (never to a closed camera), reads in any state, no polling, no restore at close; D-Bus revision 6; extends 0011 |
| [0021](adr/0021-nvidia-sdk-handover.md) | The NVIDIA SDK tooling (os-release shim, packaging, SDK checks) lives in the omarchy-nvidia-sdks repository; omarchy-broadcast keeps only the drop-in, an optional dependency on `maxine-vfx-sdk-local` and a default SDK root; the shim variable is `MAXINE_OS_RELEASE`; amends 0012; amended: the SDK root can come from `[effect] maxine_root`, and the daemon loads the SDK's libraries itself, so `LD_LIBRARY_PATH` is not needed
| [0022](adr/0022-crash-recovery-for-consumers.md) | Crash recovery for consumers: at each loopback writer start the daemon uploads a black timeout image and sets the driver's `timeout`, so Chromium-based applications keep the camera across a daemon crash; the writer stamps with `CLOCK_MONOTONIC`; the unit restarts after 250 ms with an explicit start limit; PipeWire consumers do not survive a crash; extends 0011, amends 0010

Conventions: diagrams are mermaid. New decisions get the next ADR number; superseded ADRs keep their file and change their status line.
