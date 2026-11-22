# omarchy-broadcast documentation

Find the page for your task below. The setup guides get a virtual camera or microphone working, and the reference pages describe the interfaces and the design.

## Find a page by task

| I want to... | Read |
|---|---|
| Install omarchy-broadcast on Arch Linux or Omarchy | [INSTALL.md](../INSTALL.md) |
| Build and install from a checkout, on another distribution or in your home directory | [building.md](building.md) |
| Find out what omarchy-broadcast cannot do, and what to do about it | [LIMITATIONS.md](../LIMITATIONS.md) |
| Run, configure and troubleshoot the daemon as a service | [setup/daemon.md](setup/daemon.md) |
| Create the "Omarchy Broadcast" virtual camera device | [setup/loopback.md](setup/loopback.md) |
| Change the virtual camera's size or frame rate | [setup/loopback.md](setup/loopback.md#changing-the-output-size-and-rate) |
| Use the camera in several applications at once | [setup/loopback.md](setup/loopback.md#one-application-at-a-time) and [the PipeWire camera output](setup/daemon.md#the-pipewire-output) |
| Blur or replace my background without an NVIDIA GPU | [setup/onnx.md](setup/onnx.md) |
| Use NVIDIA Maxine effects: noise removal, relighting, enhancement, eye contact, auto-framing | [setup/maxine.md](setup/maxine.md) |
| Get a microphone with noise removal | [setup/microphone.md](setup/microphone.md) |
| Fix or understand a camera's behaviour (MX Brio, C930e) | [cameras/mx-brio.md](cameras/mx-brio.md), [cameras/logitech-c930e.md](cameras/logitech-c930e.md) |
| Add a camera, or measure one | [camera-tune.md](camera-tune.md) |
| Change a configuration key | [config.example.toml](config.example.toml) |
| Script the CLI, or write a client that cannot speak D-Bus | [cli-json.md](cli-json.md) |
| Write a client that talks D-Bus | [dbus-api.md](dbus-api.md) |
| Write or install a module | [modules.md](modules.md) |
| See how the daemon works inside | [architecture.md](architecture.md) |
| Run the tests | [testing.md](testing.md) |
| Build the Arch package from a checkout, run the benchmark binary, check the repository or publish the AUR packages | [development.md](development.md) |
| Read the benchmarks, or check that the GPU idles | [Benchmarks](#benchmarks-and-tools) |

## Setup guides

| Page | What it covers |
|---|---|
| [INSTALL.md](../INSTALL.md) | Installing omarchy-broadcast on Arch Linux and Omarchy as a walkthrough: finding your case, ONNX Runtime, the prebuilt `-bin` or source `-git` packages, more models, creating the virtual camera device, starting the daemon, checking it, the NVIDIA Maxine modules, the virtual microphone, the bar plugin and uninstalling. The ONNX Runtime packages, the models, the NVIDIA SDKs and the bar plugin are in the repositories that own them, which it links |
| [building.md](building.md) | Building and installing from a checkout, for other Linux distributions and for a home-directory install: what you need, the per-user install script (what it installs, its behaviour, options and exit status), the manual `cargo install` route and uninstalling a build from source |
| [LIMITATIONS.md](../LIMITATIONS.md) | What omarchy-broadcast cannot do: one application at a time on the virtual camera, cameras that are not standard webcams, the PipeWire node's mixed formats, a daemon crash, the TensorRT, OpenVINO and eye contact limits, unpublished Maxine figures and the project's status. Each has its cause and a workaround. The developer's limits are in [architecture.md](architecture.md#known-limitations--future-work) |
| [setup/loopback.md](setup/loopback.md) | Creating the "Omarchy Broadcast" v4l2loopback virtual camera: `modprobe` options, reload and why `exclusive_caps=1`. The one-application-at-a-time limit of the device, and the PipeWire node that serves several. Changing the output size and rate: deferral while applications hold the camera, retries, the fallback order and known limits |
| [setup/daemon.md](setup/daemon.md) | Running and configuring `omarchy-broadcastd` as a systemd user service (installing it is in [INSTALL.md](../INSTALL.md)). The unit, starting and checking the daemon, troubleshooting, the udev rule, the WirePlumber drop-in, configuration and state files, reload and logs. The optional PipeWire camera output: build feature, configuration, checks, application settings and hiding the duplicate camera |
| [setup/onnx.md](setup/onnx.md) | The open-model cut-out module (`br.com.dias.onnx-cutout`). What ONNX and ONNX Runtime are, the ONNX Runtime version the module needs (the choice of package is in omarchy-onnxruntime), the module and its four models (the module package, which brings the default model, one `omarchy-onnx-model-<id>` package per model, or `scripts/install-user.sh` with `OBC_ONNX_MODELS_DIR`), the model search order, choosing the model and the engine, the engine order under `auto` and the reasons an engine is not used, the TensorRT first-use build, using the module without an NVIDIA GPU (system memory, what is unavailable, the CPU cost), troubleshooting and uninstall |
| [setup/maxine.md](setup/maxine.md) | The NVIDIA Maxine VFX SDK through the Maxine module: requirements, the SDK package `maxine-vfx-sdk-local` (built with the omarchy-nvidia-sdks repository), the SDK environment the daemon applies to itself (no drop-in: the log line, the merge rules, the opt-out, removing an old drop-in) and the os-release shim, the environment the daemon reads, an SDK tree without the package, which SDK features each effect needs, selecting effects, noise removal, enhancement, relighting, troubleshooting and uninstall. Also the Maxine AR module (eye contact, auto-framing) and the Maxine audio effects module (the microphone's GPU effects, the worker applying the SDK's environment, the required `LD_LIBRARY_PATH` and the SDK root order) |
| [setup/microphone.md](setup/microphone.md) | The "Omarchy Broadcast Mic" virtual microphone with audio modules: installing it, `omarchy-broadcast mic on` and `off`, using it as the default microphone and restoring, choosing the microphone, DeepFilterNet (default) and RNNoise, strength. The GPU module on NVIDIA hardware: packages, the SDK root, the Mic tab's noise, room echo and studio voice settings, studio voice's delay and its exclusion of noise and echo removal (the `force_conflicts` override), provider choice. What happens when something stops, troubleshooting, what it touches and undo. The older filter-chain units as a lightweight alternative that excludes it |

## Camera pages

| Page | What it covers |
|---|---|
| [camera-tune.md](camera-tune.md) | Adding a camera: measuring one camera with `scripts/camera-tune.py` (modes, delivered rate, damaged frames, colour range, noise, control sweeps), its safety rules, and turning its proposals into quirk rows and camera profiles |
| [cameras/mx-brio.md](cameras/mx-brio.md) | Logitech MX Brio (046d:0944) on Linux: unit topology and extension-unit GUIDs, standard controls with their V4L2 names, the decoded-but-unverified Logitech extension controls (Show Mode, exposure compensation, vibrance, white balance offset; and why RightSight is not supported), how to query them read-only, what works on plain Linux, what omarchy-broadcast adds (including the shutter and Show Mode status) and what is missing |
| [cameras/logitech-c930e.md](cameras/logitech-c930e.md) | Logitech Webcam C930e (046d:0843) on Linux: formats and modes (no 60 fps, 1080p30 in MJPEG only), standard controls as measured (quantised exposure, gain under automatic exposure, pan and tilt only while zoomed), its seven extension units read-only with their public Logitech names (and the LED the kernel guards), the stereo microphone and its levels with noise removal, and how omarchy-broadcast selects its mode and runs the effects on it |

## Reference

| Page | What it covers |
|---|---|
| [config.example.toml](config.example.toml) | Every `config.toml` key with its default, as a commented example, and how `config.d` and the managed `50-panel.toml` work. Covers the `[effect]` stages (`denoise`, `enhance`, `relight` with its light, direction and brightness), the `[modules]` and `[module."<id>"]` tables, `[mic]`, and `[video]` (`memory`: GPU or system memory; `battery_saver`: when `auto` keeps the frames in system memory so that the NVIDIA GPU is not woken). `background_image` may be a video |
| [cli-json.md](cli-json.md) | The `omarchy-broadcast` commands (including `effect`, `denoise`, `preview`, `controls`, `settings`, `modules`, `order`, `config`, `mic`, `media`, `recommended`), exit codes and the JSON line format for clients that cannot speak D-Bus: `status`, `cameras`, `controls --json`, `settings --json` (`settings_group`, `settings_delta`, `settings_clear`), `modules --json`, `order --json`, `config --json` (`config_group`, `config_files`), `watch --json` (with `config_changed` and `modules_changed`), `preview --json`, the `mic` line and the snapshot's `mic`, the `media`, `media_item` and `media_import` lines (videos and animated images from revision 18), the `stage_values` field, `recommended` and `recommended apply` (interface revision 20), the `Delivers:`, `Damaged:`, `Warning:` and `Suggest:` lines of `status`, and the `camera_health` line |
| [dbus-api.md](dbus-api.md) | Reference for `br.com.dias.OmarchyBroadcast1` (revision 28): states, properties, methods, errors and signals; video effects and their stages; camera image controls; the schema-driven camera settings tree (`GetSettings`, `SetSetting`, `ResetSettings`, `SettingsChanged`) and the global settings of revision 8; the modules of revision 9; the configuration of revision 10 (`config.d`, the managed `50-panel.toml`); the capture and output modes of revision 12; the microphone of revision 13; the background gallery of revision 15 and the video backgrounds of revisions 18 and 19; stage values (16); the away image (17); camera health and recommended settings (20); privacy as an option (21); the battery saver (22 and 23). Also live preview, the `Stats` signal, activation without starting the daemon, and `busctl` and `gdbus` examples |
| [modules.md](modules.md) | Writing and installing modules. What a module provides (stages, slots, rank, device, frame kinds, formats), the manifest reference, discovery directories (including `OBC_MODULE_PATH`), module configuration, the C ABI and the Rust SDK, the frame-generation stage, the settings description, threading and CUDA rules, failure semantics, the crash ledger, why a rebuilt library needs a daemon restart, reserved names and how to test a module. Includes `examples/module-hello`, audio modules (the `audio-filter` kind, `mic-` stages, `examples/module-hello-audio`) and the two audio modules in this repository (the CPU voice module and the Maxine audio effects module) |
| [architecture.md](architecture.md) | Mermaid diagrams of the component overview, the stage and caps model (ingest, process, output), the persistent loopback writer, the daemon lifecycle and threads, the camera settings model, the effect stages, the module layer, the choice of frame memory (GPU or system), the D-Bus sequence, the microphone, the video backgrounds, camera health and recommended settings, and the two-phase effect switch |
| [development.md](development.md) | Working on the code and the packages: the repository check, the benchmark binary, the Arch package built from a checkout, the five packages with their dependencies and the module ABI range, the files the core package installs, and how the AUR packages (source and prebuilt) are published |
| [testing.md](testing.md) | Unit tests, hardware tests (environment variables), the daemon, CLI and `obc-ipc` tests, the video-effects, module-system, system-memory, video-background, microphone, camera-health and two-phase-switch tests, discovery by PipeWire, the PipeWire output's tests, the `obc-bench` flags, and the manual OBS and Chrome tests |

## Benchmarks and tools

| Page | What it covers |
|---|---|
| [research/maxine-linux.md](research/maxine-linux.md) | Technical notes on Maxine on Linux: distribution, licence, versions, Blackwell support, features, API shape, existing projects |
| [benchmarks/passthrough.md](benchmarks/passthrough.md) | Passthrough and roundtrip results: pipeline, latency method, output format, camera frame-rate caveat, teardown finding. Raw results are the JSON files alongside |
| [benchmarks/ingest-paths.md](benchmarks/ingest-paths.md) | Camera and ingest-path results: GPU-direct, CPU convert and CPU decode cost on two cameras, with variance under load |
| [benchmarks/io-split.md](benchmarks/io-split.md) | Ingest and Output split results: output frame rate over 10 minutes, live camera switching with an external consumer, crop versus letterbox, cost per ingest path against the single pipeline, and the phase-locked clock with the held frame at a switch. Raw results are the JSON files alongside |
| [benchmarks/system-memory.md](benchmarks/system-memory.md) | System-memory pipeline results (no NVIDIA GPU): daemon CPU, frame rate and added latency with the CPU cut-out blurring at 1080p and 720p on a high-end laptop CPU, the per-step costs (decode, convert, the range conversion, the module's steps), and the colour range of the cameras' raw formats |
| [benchmarks/dgpu-idle.md](benchmarks/dgpu-idle.md) | dGPU idle check after teardown: findings and the quiet-desktop procedure |
| `../scripts/dgpu-idle-check.sh` | Reads sysfs and `/proc` only (never NVML) to report whether the dGPU runtime-suspends and who holds it awake |
| `obc-bench` (`crates/obc-pipeline`) | Development binary: camera, then CUDA memory, then v4l2loopback, with latency, fps and GPU stats as JSON. Not shipped. How to run it: [development.md](development.md#run-the-benchmark-binary) |
