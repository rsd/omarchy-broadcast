# Using the open-model cut-out (ONNX) with omarchy-broadcast

The **open-model cut-out** is a loadable module of omarchy-broadcast ([../modules.md](../modules.md), [ADR-0025](../adr/0025-loadable-modules.md)) with the id `br.com.dias.onnx-cutout`. It provides the `background` stage, the cut-out of the person with the blur or the replacement behind them, using open models under the Apache License 2.0, so the background effect works without NVIDIA Maxine and without an NVIDIA GPU for the model itself. The reasons for its design are in [ADR-0031](../adr/0031-onnx-cutout-module.md).

**What ONNX is.** ONNX is a standard file format for a trained neural network. ONNX Runtime is a library that runs such a file on many kinds of hardware: the CPU, an NVIDIA GPU (CUDA or TensorRT), an Intel NPU, GPU or CPU (OpenVINO). The module hands the model to ONNX Runtime and lets it use the best hardware that the installed build of the library offers. omarchy-broadcast does not contain or link ONNX Runtime: the module opens `libonnxruntime.so` when it needs it, and the library comes from a package you install (section 2).

This page covers installing the module, its runtime and its models, choosing the model and the engine, reading the reasons an engine is not used, and the failures that are specific to omarchy-broadcast. The module's own reference (every setting, key and variable) is its [README](../../crates/obc-module-onnx/README.md).

## Using it on a machine without an NVIDIA GPU

The module needs no NVIDIA GPU, and neither does the daemon's effect path. When the machine has no usable CUDA, the daemon keeps the video frames in system memory ([ADR-0033](../adr/0033-system-memory-pipeline.md)) and hands the module host frames. Install an ONNX Runtime build (`onnxruntime-cpu` is enough; `onnxruntime-full` adds the Intel engines; section 2), the module and the models (section 3), and the blur and the replacement work.

**What the daemon decides, and how to see it.** `[video] memory` in `config.toml` is `auto` by default: CUDA when the NVIDIA driver is loaded and lists a GPU, `libcuda.so.1` opens, `CUDA_VISIBLE_DEVICES` does not hide every GPU and GStreamer's CUDA elements are installed; otherwise system memory. The daemon logs the choice and the reason, once per change:

```bash
journalctl --user -u omarchy-broadcastd | grep "video memory"
# video memory: system (auto: no NVIDIA driver is loaded)
omarchy-broadcast config show | grep -A1 video.memory      # "Now: system memory, ..."
omarchy-broadcast modules                                  # the Maxine stages: unavailable, "Needs an NVIDIA GPU (CUDA)"
```

With both the Maxine and the ONNX module installed, `background` is provided by this module in system memory whatever the profile, and a pin to the Maxine module falls back to it with the reason. Nothing else is needed: with no NVIDIA driver the probe says so without touching a GPU.

**What is unavailable.** The effects that need CUDA frames: the Maxine cut-out, noise removal, relighting, the HDR look, enhancement, eye contact, auto-framing and frame generation (so no "(generated)" output rate is offered). They are greyed in the settings with the reason. This module's cut-out and the host modules work.

**On an NVIDIA machine** you can choose the same path with `[video] memory = "system"` (for instance to keep the GPU asleep, or to run the CPU path for a check). Then the daemon makes no CUDA call, and the module is told so: under `auto` it skips TensorRT and CUDA with the reason `CUDA is off: the video pipeline runs in system memory` and runs on the CPU, or on OpenVINO if your ONNX Runtime has it; an explicit `effect.onnx.engine` of `cuda` or `tensorrt` fails the load with that reason. On a laptop this keeps the NVIDIA GPU out of the effect path, and `auto` then prefers the Intel NPU when the runtime has OpenVINO (the order is in section 4). A change of `memory` applies at the next camera start. If CUDA looks usable but fails when the camera starts (a driver and library that do not match, or `CUDA_VISIBLE_DEVICES` naming a GPU that is not there), `auto` starts again once in system memory, logs a warning with the CUDA error, and stays there until the next reload.

**The cost on the CPU.** Measured on the reference machine (an Intel Core Ultra 9 275HX laptop, the CPU build of ONNX Runtime 1.31.1, the default MediaPipe model, the module's default of 4 threads), with the virtual camera read by an application at 30 fps and the blur on:

| Camera, output | Daemon CPU, blur (effect off) | Effect, per processed frame |
|---|---|---|
| MJPEG 1080p30, 1080p30 | 42 % of one core (17 %) | 6.0 ms median, 7.2 ms at p95 |
| NV12 1080p60, 1080p30 | 29 % of one core (1 %) | 6.3 ms median, 7.4 ms at p95 |
| MJPEG 720p30, 720p30 | 25 % of one core (8.5 %) | not measured |

The virtual camera held 30 fps with no dropped frame in every row. The blur adds about 25 to 27 % of one core at 1080p and about 16 % at 720p; the rest is the camera's own decode and conversion, which is the cost of an MJPEG camera in any case. This is a high-end laptop CPU: a machine with four low-power cores is expected to take two to three times as long per frame, which has not been measured. A 720p camera mode into a 720p output costs about 40 % less (25 % against 42 %); a 720p output from a 1080p camera does not, because the camera is still decoded at 1080p and then scaled (43 %). Method and the per-step figures: [../benchmarks/system-memory.md](../benchmarks/system-memory.md). `threads` in the module's table sets the CPU threads (see the module's [README](../../crates/obc-module-onnx/README.md)); more threads shorten a frame and cost more CPU in total.

The privacy rules are the same as with CUDA: the room is never shown while an effect is on and cannot run (black frames by default, or the image of `[effect] on_failure`), the picture goes black at a start until the matte is ready, and turning the blur on mid-stream keeps the picture for under a frame on the CPU and then switches with no black frame ([ADR-0033](../adr/0033-system-memory-pipeline.md)).

## 1. What you need

| Item | Requirement |
|---|---|
| The module | Installed from the package `omarchy-broadcast-onnx-git` or by `scripts/install-user.sh` (section 3) |
| ONNX Runtime | `libonnxruntime.so`, version 1.29 or newer. Recommended: `onnxruntime-full`, which has the CPU, CUDA, TensorRT and OpenVINO engines in one library. `onnxruntime-cpu` and `onnxruntime-opt-cuda` also work (section 2) |
| The models | Four `.onnx` files, in one of the directories of section 3. A single file is enough to use that model |
| An NVIDIA GPU | Not needed: the CPU engine runs the model, and the daemon runs the effect path in system memory when it finds no usable CUDA (see above). An NVIDIA GPU adds the TensorRT and CUDA engines, and an Intel CPU, GPU or NPU the OpenVINO engines (with a runtime that has them) |

Without ONNX Runtime, or without any model file, the module still loads and `omarchy-broadcast modules` lists its stage as `unavailable`, with the reason: `ONNX Runtime not installed (libonnxruntime.so not found)` or `No cut-out model installed (searched: ...)`. Nothing else is affected.

## 2. Install ONNX Runtime

**Recommended: `onnxruntime-full`.** It is a package of the omarchy-onnxruntime repository, <https://github.com/rsd/omarchy-onnxruntime>, which builds ONNX Runtime 1.29.0 with the CPU, CUDA, TensorRT and OpenVINO providers in one library (OpenVINO runs on an Intel CPU, GPU and NPU). Each provider is a library of its own beside `libonnxruntime.so` and is loaded only when it is asked for, so a vendor runtime that is not installed disables that provider and nothing else. Install the package from that repository, then the vendor runtimes of the engines you want. They are optional dependencies of the package:

| Engine | Packages |
|---|---|
| NVIDIA CUDA | `cuda`, `cudnn`, `nvidia-utils` |
| NVIDIA TensorRT | TensorRT 11: `nvidia-tensorrt-sdk` (under `/opt/nvidia/tensorrt`) or the AUR's `tensorrt` (under `/usr`); see "TensorRT's library path" below |
| OpenVINO, Intel CPU | `openvino` |
| OpenVINO, Intel GPU | `openvino-intel-gpu-plugin`, `intel-compute-runtime` |
| OpenVINO, Intel NPU | `openvino-intel-npu-plugin`, `intel-npu-driver`, `level-zero-loader` |

The repository's [docs/engines.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/docs/engines.md) describes each engine and what it needs, and its [docs/results.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/docs/results.md) has raw-model figures of four models on six targets.

**Alternatives.** Any other package that provides `onnxruntime` 1.29 or newer works with the module:

```bash
sudo pacman -S onnxruntime-opt-cuda   # an NVIDIA GPU: the CPU and the CUDA engines
sudo pacman -S onnxruntime-cpu        # anything else: the CPU engine only
```

The CUDA provider of `onnxruntime-opt-cuda` also needs cuDNN (the `cudnn` package) and the NVIDIA driver's CUDA library; a missing `libcudnn.so.9` shows as a reason (section 5). The Arch packages ship the CPU, CUDA and ROCm providers; the module has no ROCm engine, and these builds have no TensorRT or OpenVINO engine.

Which engines you get depends on what the build includes. The module asks the loaded library which providers it has and checks that each provider's library (`libonnxruntime_providers_<name>.so`) is beside `libonnxruntime.so`; it tries only the engines that pass. A build with more providers lights the other engines up with no change to the module.

The module looks for the library in this order: `$ORT_DYLIB_PATH`; `ort_library` in `[module."br.com.dias.onnx-cutout"]` of `config.toml`; then `libonnxruntime.so.1` and `libonnxruntime.so` in the system's library path. ONNX Runtime allows one environment per process: the first library the daemon opens stays until the daemon restarts. The environment also lives for the whole process: with `onnxruntime-full` 1.29.0 and OpenVINO 2026.4.1, a program that releases the environment after it used OpenVINO crashes when it exits. The module never releases it, not when the cut-out unloads, not when an instance is dropped and not at exit, and a test (`tests/exit.rs`) checks that a process that ran a model on OpenVINO exits cleanly.

### TensorRT's library path

TensorRT needs its library directory on the daemon's `LD_LIBRARY_PATH`. TensorRT 11 opens its per-GPU builder library by name, `libnvinfer_builder_resource_sm<NN>.so.<version>` (on TensorRT 10, `libnvinfer_builder_resource.so.<version>`), and the name is found only through the process's library path. Without it ONNX Runtime's TensorRT provider crashes the process, both when it builds an engine and when it loads a cached one. The module therefore checks, before it starts TensorRT, that the library for the installed GPU can be loaded. If it cannot, TensorRT is refused with a reason (section 5), `auto` goes on to CUDA, and an explicit `tensorrt` fails the load.

With the AUR's `tensorrt`, which installs under `/usr/lib`, nothing is needed: that directory is on the library path already. With `nvidia-tensorrt-sdk`, whose libraries are under `/opt/nvidia/tensorrt/lib`, give the daemon the package's environment file, which sets `LD_LIBRARY_PATH=/opt/nvidia/tensorrt/lib`, with a systemd user drop-in at `~/.config/systemd/user/omarchy-broadcastd.service.d/onnx-tensorrt.conf`:

```ini
[Service]
EnvironmentFile=-/usr/share/nvidia-tensorrt/nvidia-tensorrt.env
```

Then reload systemd and restart the daemon:

```bash
systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd
```

The `-` makes a missing file no error. The setting applies to the daemon's unit only. Do not put TensorRT's directory on `LD_LIBRARY_PATH` in a login profile: the directory holds only TensorRT's own libraries, but the rule of [maxine.md](maxine.md) stands, that a library path set for every program you start can shadow the system's libraries.

**With the Maxine module.** The Maxine drop-in's environment file also sets `LD_LIBRARY_PATH`, and when two files set the same variable the later one wins (the value is replaced, not extended). Drop-ins are read in the order of their file names, and `onnx-tensorrt.conf` sorts after `maxine.conf`, so TensorRT's value is the one the daemon gets. That does no harm to the Maxine module, which loads its libraries from its SDK root by absolute path and does not need `LD_LIBRARY_PATH` ([maxine.md](maxine.md)). Keep this name, or another that sorts after `maxine.conf`.

## 3. Install the module and the models

**From the Arch package.** The module and its models are a package of their own, `omarchy-broadcast-onnx-git`, with its own `PKGBUILD` in `packaging/arch-onnx/`, built after the core `omarchy-broadcast-git` (`packaging/arch/`). It depends on the core of the same commit (the module interface is not pinned across versions) and on `onnxruntime>=1.29`, which any variant of section 2 satisfies. It installs the library and the manifest under `/usr/lib/omarchy-broadcast/`, the models in `/usr/share/omarchy-broadcast/models/onnx/` and the licence files in `/usr/share/licenses/omarchy-broadcast-onnx-git/`. It is a separate package because the ONNX Runtime variants conflict with one another: the core must not pull one in.

The four model files (about 70 MB) are sources of that build. `makepkg` downloads them from the `v1.0.0` release of the omarchy-onnx-models repository, `https://github.com/rsd/omarchy-onnx-models/releases/download/v1.0.0/<file>`, and checks each against the SHA-256 pinned in `packaging/arch-onnx/PKGBUILD`; a file that differs fails the build. From `packaging/arch-onnx`: `makepkg -s`. For an offline build, `OBC_ONNX_MODELS_DIR=/path/to/models makepkg -s` reads the files from a directory instead, with the same checks. The core package never needs them.

**From a checkout.** `scripts/install-user.sh` builds and installs the module for the current user, without root: the library in `~/.local/share/omarchy-broadcast/modules/` and its manifest in `~/.local/share/omarchy-broadcast/modules.d/`. Give it the models with `OBC_ONNX_MODELS_DIR`, a directory that holds the four files under their catalogue names; the script checks each against its SHA-256 and copies them to `~/.local/share/omarchy-broadcast/models/onnx/`:

```bash
OBC_ONNX_MODELS_DIR=/path/to/models scripts/install-user.sh
```

Without the variable the module is installed and shows as unavailable until the models are there; run the script again with the variable to add them. A run without the variable keeps models installed earlier.

**Where the module looks for a model**, the first directory that has the file winning:

1. `model_dir` in `[module."br.com.dias.onnx-cutout"]` of `config.toml`;
2. `$OBC_ONNX_MODEL_DIR`;
3. `$XDG_DATA_HOME/omarchy-broadcast/models/onnx/` (by default `~/.local/share/omarchy-broadcast/models/onnx/`);
4. `/usr/share/omarchy-broadcast/models/onnx/`.

**The models** are the project's own conversions to ONNX of the upstream releases, which keep the weights of the originals. They are published, with the scripts that make them and their checksums, as release assets of the [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models) repository ([`crates/obc-module-onnx/model-sources/README.md`](../../crates/obc-module-onnx/model-sources/README.md) points to it). Each file is checked against its SHA-256 when it loads; a missing or different file fails the load, with a reason that names the file and the directories searched.

| Setting value | File | Upstream |
|---|---|---|
| `mediapipe-landscape` (default) | `selfie_segmentation_landscape.onnx` | Google, MediaPipe selfie segmentation (landscape) |
| `pphumanseg-v2-mobile` | `pp_humansegv2_mobile_192x192.onnx` | Baidu, PaddleSeg PP-HumanSeg v2, mobile |
| `pphumanseg-v2-lite` | `pp_humansegv2_lite_192x192.onnx` | Baidu, PaddleSeg PP-HumanSeg v2, lite |
| `modnet` | `modnet_photographic.onnx` | MODNet, photographic portrait matting |

**Licences.** All four models are released under the Apache License, Version 2.0. The text, the notices and the provenance of each file are in [`model-sources/LICENSES.md`](../../crates/obc-module-onnx/model-sources/LICENSES.md), and the package installs them with the files. The training-data terms of PP-HumanSeg (a dataset available from its publisher on request, with no licence stated publicly) and of MODNet are not confirmed, and the file records that. The default model, MediaPipe, is the one whose provenance is the simplest. If this matters for how you use the camera, the file is the reference.

Restart the daemon, then check:

```bash
systemctl --user restart omarchy-broadcastd
omarchy-broadcast modules
```

The module is listed with its stage `background`, or with the reason it cannot run.

## 4. Choose the module, the model and the engine

The module provides `background` in the `matte` slot with rank 200 and the device `cpu`. The Maxine module provides the same stage with rank 300. With both installed, the `quality` profile picks Maxine and the `battery` profile picks this module; a pin chooses regardless of the profile:

```bash
omarchy-broadcast modules profile battery
omarchy-broadcast modules pin background br.com.dias.onnx-cutout
omarchy-broadcast modules pin background --clear
```

A module choice applies when the camera next starts ([../cli-json.md](../cli-json.md)).

The module's settings are in the group "Open-model cut-out" of `omarchy-broadcast settings` and of the bar's panel, greyed while the background effect is off:

| Setting | Values | Default |
|---|---|---|
| `effect.onnx.model` ("Model") | `mediapipe-landscape` (MediaPipe, fast), `pphumanseg-v2-mobile` (PP-HumanSeg mobile, quality), `pphumanseg-v2-lite` (PP-HumanSeg lite), `modnet` (MODNet, sharp hair, for a GPU) | `mediapipe-landscape` |
| `effect.onnx.engine` ("Runs on") | `auto` ("Automatic"), `tensorrt` ("NVIDIA GPU, TensorRT (fastest)"), `cuda` ("NVIDIA GPU, CUDA"), `openvino-npu` ("Intel NPU (battery saver)"), `openvino-gpu` ("Intel GPU (OpenVINO)"), `openvino-cpu` ("CPU, OpenVINO"), `cpu` ("CPU") | `auto` |

```bash
omarchy-broadcast settings set effect.onnx.model pphumanseg-v2-mobile
omarchy-broadcast settings set effect.onnx.engine cpu
```

The labels are what the settings show; the values are what `settings set` takes. The engine names in log lines and reasons are not the labels: `NVIDIA TensorRT`, `NVIDIA CUDA`, `OpenVINO NPU`, `OpenVINO GPU`, `OpenVINO CPU` and `CPU`.

The blur strength and the replacement image are the daemon's own background settings (`effect.background.*`), shared with the Maxine module.

A change of either setting is a **switch** while the camera runs: the previous model keeps running until the new one is loaded, then the picture changes, with no black frame.

**A switch that cannot load is refused, and the picture does not change.** If the new model or engine cannot start (an explicit `tensorrt` on a machine without TensorRT's libraries, a model file that is missing), the previous one keeps processing every frame: the picture stays the cut-out it was, never black and never the away image, and the effect stays ready. `Error` stays empty, and `Warning` says why, with the clause `<module>: <reason>; the previous setting keeps running`. The setting itself is not rewritten: what you chose stays chosen, and the settings keep showing it, so the engine that runs and the engine that is stored can differ until you act. The refused value is tried again only when you change it or apply the unchanged settings again; other changes meanwhile (turning a stage on, blur to replace, another camera) load against the model and engine that run. Turning the effect off, or a new camera format, ends the refusal.

A refused value that is still stored is tried at the next camera start, where black or the away image is allowed: an explicit engine that cannot run fails the load, as described below. Set the setting back, or to `auto`, to avoid it.

With system-memory frames the new model loads on a second instance of the module while the first keeps running, and the picture changes when it is ready; the two are alive together while it loads, which costs the memory of a second session. With CUDA frames the switch loads on the running instance instead (the module's CUDA stream is bound when it loads, and a second one is not safe to use mid-frame), so a slow switch there, a first TensorRT engine build, holds the last frame for as long as it takes. It is never black.

**Engines under `auto`.** `auto` tries the engines in this order and takes the first that starts:

1. NVIDIA TensorRT
2. NVIDIA CUDA
3. OpenVINO on an Intel NPU
4. OpenVINO on a GPU
5. OpenVINO on the CPU
6. ONNX Runtime's own CPU provider

On a laptop, `[video] memory = "system"` keeps the NVIDIA GPU out of the effect path: `auto` then skips TensorRT and CUDA and, with OpenVINO in the runtime, prefers the NPU (see "Using it on a machine without an NVIDIA GPU").

An engine you choose yourself that cannot run fails the load with its reason, and the module does not fall back to another: a request for `cuda` that cannot get a GPU is an error, not a CPU run you did not ask for. An engine that the files or the libraries rule out (a provider library that is missing, TensorRT without its libraries) is not tried at all: `auto` skips it with the reason, and an explicit choice fails the load with it.

**The matte never fails open.** As with Maxine, a frame that fails in the cut-out fails the whole chain and the effect follows `[effect] on_failure` (black by default): the raw camera image is never shown in place of a cut-out that failed. A model that fails to load leaves what was running as it was.

### Measured on each engine

The same model and frames on each engine of `onnxruntime-full` 1.29.0. Conditions: an Intel Core Ultra 9 275HX laptop with a GeForce RTX 5080 Laptop GPU (compute capability sm120) and the integrated Intel GPU and NPU; TensorRT 11.4.0, CUDA 13 and OpenVINO 2026.4.1; the default model (MediaPipe landscape); the module's hardware test, which runs CUDA NV12 frames of a 1280x720 test image with the blur; medians. Another application was streaming a camera from the GPU during the runs, so the figures are indicative. Every engine kept the same 32.8 % of the picture: the matte is identical.

Times are per frame, in milliseconds. "Mask" is the model's own run; "all steps" is the module's whole frame: convert in, mask, composite and convert out. "Session create" is the time to load the model on the engine, and "first run" the time of the first frame.

| Engine | Mask (model) | All steps | Session create | First run |
|---|---|---|---|---|
| NVIDIA TensorRT | 0.37 | 3.39 | 99 ms (engine cached) | 3 ms |
| NVIDIA CUDA | 0.65 | 3.54 | 83 ms | 307 ms |
| Intel NPU (OpenVINO) | 2.41 | 5.26 | 40 ms | 8 ms |
| Intel GPU (OpenVINO) | 1.85 | 4.31 | 256 ms | 27 ms |
| CPU, OpenVINO | 0.97 | 3.85 | 117 ms | 2 ms |
| CPU | 1.16 | 3.85 | 7 ms | 2 ms |

At this size the compositing and the conversions, not the model, are most of a frame: the slowest and the fastest engine differ by about 2 ms, at the mask and in all steps. The NPU is the slowest engine here; its value is lower power, not speed, and no power figures were measured. A first TensorRT engine build for a model took about 10 s (10.6 s for `pphumanseg-v2-lite`); later loads read the cache (section 6). Raw-model figures of all four models on all six targets are in the omarchy-onnxruntime repository's [docs/results.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/docs/results.md).

## 5. Reading the reasons

The module writes one line per engine to the daemon's log (`journalctl --user -u omarchy-broadcastd`), at three moments:

- **When it is probed** (files only): the engines that the files already rule out, for example a provider library missing beside `libonnxruntime.so`, or a GPU hidden by an empty `CUDA_VISIBLE_DEVICES`.
- **When it is checked** (before the camera starts; the runtime is opened, no device is touched and no CUDA call is made): for each engine, whether the loaded ONNX Runtime build has it. Whether the device runs it is found at the next step.
- **When the cut-out loads**: each engine ruled out without being tried (`ONNX cut-out: <engine> unavailable: <reason>`: a provider library that is missing, TensorRT without its libraries, CUDA turned off), each engine tried that did not start, with the error from ONNX Runtime, then the line that says where the model runs: `ONNX cut-out: <model> runs on <engine> (ONNX Runtime <version>; ...)`. An explicit engine that could not start puts the same reason in the error that the load returns, which the daemon reports.

| Reason | Means |
|---|---|
| `not in this ONNX Runtime build (no libonnxruntime_providers_<x>.so beside ...)` | The loaded library has no such provider, or its provider library is not next to `libonnxruntime.so`. The engine is not tried: `auto` skips it, and an explicit choice fails the load with this reason. Install a build that has it (section 2) |
| `CUDA not available: ...` | The CUDA provider is there; the GPU or its libraries are not. The CUDA error follows, for example `no CUDA-capable device is detected`, or a missing `libcudnn.so.9` |
| `TensorRT not available: TensorRT's libraries are not on the daemon's library path (... not found; add TensorRT's lib directory to LD_LIBRARY_PATH)` | TensorRT's builder library cannot be found by name. TensorRT is not started (it would crash the process), `auto` moves on to CUDA, and an explicit `tensorrt` fails the load. Add TensorRT's directory for the daemon (section 2, "TensorRT's library path") |
| `TensorRT not available: TensorRT <ver> has no builder library for this GPU (sm<NN>)` | TensorRT's libraries are found, but the installed TensorRT ships no builder library for this GPU's architecture. Use a TensorRT version that supports the GPU, or another engine |
| `TensorRT not available: ...` (other) | Another TensorRT library cannot be loaded (the loader's message follows), or no NVIDIA GPU was found through the CUDA driver |
| `OpenVINO NPU: ...`, `OpenVINO GPU: ...` | OpenVINO could not use the device (an NPU needs its user-space driver) |
| `the provider was registered but ran no part of the model` | The provider started and ran nothing: OpenVINO does this when its device is missing |
| `partly on the CPU (k of n kernels)` | The engine runs, but ONNX Runtime left some of the model to the CPU |
| `CUDA is off: the video pipeline runs in system memory` | The daemon runs in system memory and told the module not to use CUDA, so TensorRT and CUDA are skipped (an explicit one fails the load). Set `[video] memory = "cuda"` to use them |

"Partly on the CPU" is measured and not guessed: the module profiles the first run of a session, which tells which provider ran each kernel, and stops profiling after that run. The profile goes in a new private directory under `$XDG_RUNTIME_DIR` (else the temporary directory) and is removed at once.

## 6. The TensorRT first-use build

TensorRT compiles the model into an engine for your GPU. The first time a model runs on TensorRT this takes from several seconds to under a minute; the log says `preparing the TensorRT engine for <model> (first use: this takes a while)...` and then `the TensorRT engine for <model> is ready`. The engine is cached in `$XDG_CACHE_HOME/omarchy-broadcast/trt/`, in a directory named for the model file's checksum, the ONNX Runtime version, the TensorRT version, the GPU and its compute capability: an engine built for another GPU or another TensorRT is never taken for the current one. Later loads read the cache in under a second.

The build runs when the effect is turned on or when the camera starts, and until it ends the picture is black at a start and the camera as it was when the effect is turned on mid-stream.

**Changing the model while the camera runs is different**, because the stream waits for a switch:

- Under `auto`, a switch to a model with no cached TensorRT engine does not build one: it runs on CUDA for now, and the log says `TensorRT engine not built yet; using CUDA for now. It builds at the next camera start.`
- With `effect.onnx.engine` set to `tensorrt`, the switch builds the engine, and the feed pauses until it is ready; the log says so.

## 7. The GPU is left as it was found

ONNX Runtime's CUDA and TensorRT engines run through the CUDA runtime, which activates the device's primary CUDA context. When the last session that used one of them ends (an unload, or a failed attempt of `auto`), the module resets that primary context, so that no CUDA context of the module's outlives its sessions and the GPU can be parked at idle ([ADR-0017](../adr/0017-cuda-checkpoint-parking.md)). The context of the frames, GStreamer's, is another context and is untouched. The rule and its bounds are in [../modules.md](../modules.md#threading-and-cuda-rules).

**The Intel GPU engine and the NVIDIA GPU.** OpenVINO's GPU engine (`openvino-gpu`, or `auto` reaching it) starts OpenCL, and the OpenCL loader opens every driver listed in `/etc/OpenCL/vendors/`, NVIDIA's included. On a laptop with both GPUs that wakes the NVIDIA GPU, which then stays awake with its `/dev/nvidia*` files open until the daemon exits, and it made the first OpenVINO GPU session take about 2.7 s instead of about 0.1 s. So before its first OpenVINO GPU session the module limits the loader to the Intel GPU's driver:

- It makes `$XDG_RUNTIME_DIR/omarchy-broadcast/opencl-vendors/` (mode 0700), links in it the `.icd` files of `/etc/OpenCL/vendors/` that name Intel's compute runtime (`libigdrcl`), and sets `OCL_ICD_VENDORS` to that directory, with a trailing slash (the loader that the CUDA toolkit installs needs it). Both OpenCL loaders that may be installed, ocl-icd's and CUDA's `/opt/cuda/lib64/libOpenCL.so.1`, honour the setting.
- The variable is set once, in the daemon's own environment, and the processes the daemon starts afterwards inherit it. Nothing under `/etc` is changed.
- It is left alone when any `OCL_ICD_*` variable is already set, which is your override (put your own `OCL_ICD_VENDORS` in the daemon's environment to choose the drivers yourself), and when no Intel GPU driver is installed.
- The NPU and CPU engines load no OpenCL and are not affected.

The NVIDIA GPU then stays asleep under `openvino-gpu`; the log says whether the limit was applied.

## 8. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `modules` lists the stage `unavailable: ONNX Runtime not installed (libonnxruntime.so not found)` | No `libonnxruntime.so` where the module looks | Install a package of section 2, or set `ORT_DYLIB_PATH` in the daemon's environment or `ort_library` in the module's table, then restart the daemon |
| `unavailable: No cut-out model installed (searched: ...)` | No model file in the directories listed | Install the models (section 3). The message names the directories searched |
| A model fails to load with a checksum or file error | The file is not the catalogue's: a different conversion, a truncated copy | Use the files that the package or the checkout's scripts produce; their checksums are in the module's README |
| The model runs on the CPU although you have an NVIDIA GPU | `auto` found nothing better: the installed build is `onnxruntime-cpu`, or the CUDA reason says why (`CUDA not available: ...`) | Install `onnxruntime-full` (or `onnxruntime-opt-cuda`) and `cudnn`, and read the reason in the log (section 5) |
| The log says TensorRT's libraries are not on the daemon's library path, and the model runs on CUDA | The daemon's `LD_LIBRARY_PATH` lacks TensorRT's directory (`/opt/nvidia/tensorrt/lib` with `nvidia-tensorrt-sdk`) | Add the drop-in of section 2, "TensorRT's library path", then reload systemd and restart the daemon |
| An explicit engine fails the load | By design there is no fallback | Read the reason; choose `auto` or another engine |
| After changing the engine or model the picture did not change, and `Warning` ends `the previous setting keeps running` | The new setting could not load; the previous one keeps running by design | Read the reason before the semicolon and fix it, or set the setting back; change it again or apply it again to try it once more |
| The NVIDIA GPU wakes up when `openvino-gpu` is chosen | The OpenCL loader opened NVIDIA's driver: the module's limit was not applied (an `OCL_ICD_*` variable was already set, or the loader was in the process before) | Remove your `OCL_ICD_*` setting from the daemon's environment, or make it list the Intel driver only; the log says why the limit was left alone |
| The picture is black for many seconds the first time the effect is on | The TensorRT engine is being built | Wait for `the TensorRT engine for <model> is ready` in the log (section 6) |
| On a machine without an NVIDIA GPU the picture is black and `Warning` says the Maxine module cannot run | Only the Maxine module provides `background` (the ONNX module or its models or its runtime are missing, so its stage is `unavailable`) | Install the module, the models and `onnxruntime-cpu` (sections 2 and 3); `omarchy-broadcast modules` shows the reason |
| The log says `video memory: system` on an NVIDIA machine | `[video] memory` is `system`, or the probe found no usable CUDA (`auto`), or a start fell back after CUDA failed | The log line gives the reason; set `memory = "cuda"` to require CUDA, and reload |
| After a first install the module does not appear | The daemon reads manifests at start and on reload | `systemctl --user restart omarchy-broadcastd` |

## 9. Uninstall

`scripts/install-user.sh --uninstall` removes what the script installed, the module's library, manifest and models included. The package is removed with `sudo pacman -R omarchy-broadcast-onnx-git`. The TensorRT engine cache is under `$XDG_CACHE_HOME/omarchy-broadcast/` (by default `~/.cache/omarchy-broadcast/`) and may be deleted at any time; it is rebuilt on the next use.
