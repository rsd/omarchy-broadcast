# Using the open-model cut-out (ONNX) with omarchy-broadcast

The **open-model cut-out** is a loadable module of omarchy-broadcast ([../modules.md](../modules.md), [ADR-0025](../adr/0025-loadable-modules.md)) with the id `br.com.dias.onnx-cutout`. It provides the `background` stage, the cut-out of the person with the blur or the replacement behind them, using open models under the Apache License 2.0, so the background effect works without NVIDIA Maxine and without an NVIDIA GPU for the model itself. The reasons for its design are in [ADR-0031](../adr/0031-onnx-cutout-module.md).

**What ONNX is.** ONNX is a standard file format for a trained neural network. ONNX Runtime is a library that runs such a file on many kinds of hardware: the CPU, an NVIDIA GPU (CUDA or TensorRT), an Intel NPU or GPU (OpenVINO). The module hands the model to ONNX Runtime and lets it use the best hardware that the installed build of the library offers. omarchy-broadcast does not contain or link ONNX Runtime: the module opens `libonnxruntime.so` when it needs it, and the library comes from your distribution.

This page covers installing the module, its runtime and its models, choosing the model and the engine, reading the reasons an engine is not used, and the failures that are specific to omarchy-broadcast. The module's own reference (every setting, key and variable) is its [README](../../crates/obc-module-onnx/README.md).

## Using it on a machine without an NVIDIA GPU

The module needs no NVIDIA GPU, and neither does the daemon's effect path. When the machine has no usable CUDA, the daemon keeps the video frames in system memory ([ADR-0033](../adr/0033-system-memory-pipeline.md)) and hands the module host frames. Install the CPU build of ONNX Runtime (`onnxruntime-cpu`, section 2), the module and the models (section 3), and the blur and the replacement work.

**What the daemon decides, and how to see it.** `[video] memory` in `config.toml` is `auto` by default: CUDA when the NVIDIA driver is loaded and lists a GPU, `libcuda.so.1` opens, `CUDA_VISIBLE_DEVICES` does not hide every GPU and GStreamer's CUDA elements are installed; otherwise system memory. The daemon logs the choice and the reason, once per change:

```bash
journalctl --user -u omarchy-broadcastd | grep "video memory"
# video memory: system (auto: no NVIDIA driver is loaded)
omarchy-broadcast config show | grep -A1 video.memory      # "Now: system memory, ..."
omarchy-broadcast modules                                  # the Maxine stages: unavailable, "Needs an NVIDIA GPU (CUDA)"
```

With both the Maxine and the ONNX module installed, `background` is provided by this module in system memory whatever the profile, and a pin to the Maxine module falls back to it with the reason. Nothing else is needed: with no NVIDIA driver the probe says so without touching a GPU.

**What is unavailable.** The effects that need CUDA frames: the Maxine cut-out, noise removal, relighting, the HDR look, enhancement, eye contact, auto-framing and frame generation (so no "(generated)" output rate is offered). They are greyed in the settings with the reason. This module's cut-out and the host modules work.

**On an NVIDIA machine** you can choose the same path with `[video] memory = "system"` (for instance to keep the GPU asleep, or to run the CPU path for a check). Then the daemon makes no CUDA call, and the module is told so: under `auto` it skips TensorRT and CUDA with the reason `CUDA is off: the video pipeline runs in system memory` and runs on the CPU, or on OpenVINO if your ONNX Runtime has it; an explicit `effect.onnx.engine` of `cuda` or `tensorrt` fails the load with that reason. A change of `memory` applies at the next camera start. If CUDA looks usable but fails when the camera starts (a driver and library that do not match, or `CUDA_VISIBLE_DEVICES` naming a GPU that is not there), `auto` starts again once in system memory, logs a warning with the CUDA error, and stays there until the next reload.

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
| ONNX Runtime | `libonnxruntime.so`, version 1.29 or newer, from any Arch `onnxruntime-*` package (section 2) |
| The models | Four `.onnx` files, in one of the directories of section 3. A single file is enough to use that model |
| An NVIDIA GPU | Not needed: the CPU engine runs the model, and the daemon runs the effect path in system memory when it finds no usable CUDA (see above). An NVIDIA GPU adds the TensorRT and CUDA engines |

Without ONNX Runtime, or without any model file, the module still loads and `omarchy-broadcast modules` lists its stage as `unavailable`, with the reason: `ONNX Runtime not installed (libonnxruntime.so not found)` or `No cut-out model installed (searched: ...)`. Nothing else is affected.

## 2. Install ONNX Runtime

On Arch, install one of the packages that provide `onnxruntime`:

```bash
sudo pacman -S onnxruntime-opt-cuda   # an NVIDIA GPU: the CPU and the CUDA engines
sudo pacman -S onnxruntime-cpu        # anything else: the CPU engine only
```

The CUDA provider of `onnxruntime-opt-cuda` also needs cuDNN (the `cudnn` package) and the NVIDIA driver's CUDA library; a missing `libcudnn.so.9` shows as a reason (section 5). Any other `onnxruntime-*` variant that provides `onnxruntime` 1.29 or newer works with the module.

Which engines you get depends on what the build includes. The Arch packages ship the CPU, CUDA and ROCm providers; the module has no ROCm engine. The TensorRT and OpenVINO engines need an ONNX Runtime build that includes those providers (and, for TensorRT, the TensorRT libraries). The module asks the loaded library which providers it has and tries each, so such a build lights those engines up with no change to the module.

The module looks for the library in this order: `$ORT_DYLIB_PATH`; `ort_library` in `[module."br.com.dias.onnx-cutout"]` of `config.toml`; then `libonnxruntime.so.1` and `libonnxruntime.so` in the system's library path. ONNX Runtime allows one environment per process: the first library the daemon opens stays until the daemon restarts.

## 3. Install the module and the models

**From the Arch package.** The module and its models are a package of their own, `omarchy-broadcast-onnx-git`, with its own `PKGBUILD` in `packaging/arch-onnx/`, built after the core `omarchy-broadcast-git` (`packaging/arch/`). It depends on the core of the same commit (the module interface is not pinned across versions) and on `onnxruntime>=1.29`, which any variant of section 2 satisfies. It installs the library and the manifest under `/usr/lib/omarchy-broadcast/`, the models in `/usr/share/omarchy-broadcast/models/onnx/` and the licence files in `/usr/share/licenses/omarchy-broadcast-onnx-git/`. It is a separate package because the ONNX Runtime variants conflict with one another: the core must not pull one in.

The four model files are sources of that build, checked by SHA-256. Until a release host for them is published, the model download address in `packaging/arch-onnx/PKGBUILD` is empty, and `makepkg` reads the files from a local directory (`$startdir/models`, or `OBC_ONNX_MODELS_DIR`): from `packaging/arch-onnx`, `OBC_ONNX_MODELS_DIR=/path/to/models makepkg -s`. Without the files, `makepkg` names the missing ones and stops at the missing sources. The core package never needs them.

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

**The models** are the project's own conversions to ONNX of the upstream releases, which keep the weights of the originals. They are published, with the scripts that make them and their checksums, by the omarchy-onnx-models repository ([`crates/obc-module-onnx/model-sources/README.md`](../../crates/obc-module-onnx/model-sources/README.md) points to it). Each file is checked against its SHA-256 when it loads; a missing or different file fails the load, with a reason that names the file and the directories searched.

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
| `effect.onnx.engine` ("Runs on") | `auto`, `tensorrt`, `cuda`, `openvino-npu`, `openvino-gpu`, `openvino-cpu`, `cpu` | `auto` |

```bash
omarchy-broadcast settings set effect.onnx.model pphumanseg-v2-mobile
omarchy-broadcast settings set effect.onnx.engine cpu
```

The blur strength and the replacement image are the daemon's own background settings (`effect.background.*`), shared with the Maxine module.

A change of either setting is a **switch** while the camera runs: the previous model keeps running until the new one is loaded, then the picture changes, with no black frame.

**Engines under `auto`.** `auto` tries the engines in this order and takes the first that starts:

1. NVIDIA TensorRT
2. NVIDIA CUDA
3. OpenVINO on an Intel NPU
4. OpenVINO on a GPU
5. OpenVINO on the CPU
6. ONNX Runtime's own CPU provider

An engine you choose yourself that cannot run fails the load with its reason, and the module does not fall back to another: a request for `cuda` that cannot get a GPU is an error, not a CPU run you did not ask for.

**The matte never fails open.** As with Maxine, a frame that fails in the cut-out fails the whole chain and the effect follows `[effect] on_failure` (black by default): the raw camera image is never shown in place of a cut-out that failed. A model that fails to load leaves what was running as it was.

## 5. Reading the reasons

The module writes one line per engine to the daemon's log (`journalctl --user -u omarchy-broadcastd`), at three moments:

- **When it is probed** (files only): the engines that the files already rule out, for example a provider library missing beside `libonnxruntime.so`, or a GPU hidden by an empty `CUDA_VISIBLE_DEVICES`.
- **When it is checked** (before the camera starts; the runtime is opened, no device is touched and no CUDA call is made): for each engine, whether the loaded ONNX Runtime build has it. Whether the device runs it is found at the next step.
- **When the cut-out loads**: each engine tried that did not start, with the error from ONNX Runtime, then the line that says where the model runs: `ONNX cut-out: <model> runs on <engine> (ONNX Runtime <version>; ...)`. An explicit engine that could not start puts the same reason in the error that the load returns, which the daemon reports.

| Reason | Means |
|---|---|
| `not in this ONNX Runtime build` | The loaded library has no such provider. Install a build that has it (section 2) |
| `CUDA not available: ...` | The CUDA provider is there; the GPU or its libraries are not. The CUDA error follows, for example `no CUDA-capable device is detected`, or a missing `libcudnn.so.9` |
| `TensorRT not available: ...` | The same for TensorRT, whose provider needs the TensorRT 10 libraries |
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

## 8. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `modules` lists the stage `unavailable: ONNX Runtime not installed (libonnxruntime.so not found)` | No `libonnxruntime.so` where the module looks | Install a package of section 2, or set `ORT_DYLIB_PATH` in the daemon's environment or `ort_library` in the module's table, then restart the daemon |
| `unavailable: No cut-out model installed (searched: ...)` | No model file in the directories listed | Install the models (section 3). The message names the directories searched |
| A model fails to load with a checksum or file error | The file is not the catalogue's: a different conversion, a truncated copy | Use the files that the package or the checkout's scripts produce; their checksums are in the module's README |
| The model runs on the CPU although you have an NVIDIA GPU | `auto` found nothing better: the installed build is `onnxruntime-cpu`, or the CUDA reason says why (`CUDA not available: ...`) | Install `onnxruntime-opt-cuda` and `cudnn`, and read the reason in the log (section 5) |
| An explicit engine fails the load | By design there is no fallback | Read the reason; choose `auto` or another engine |
| The picture is black for many seconds the first time the effect is on | The TensorRT engine is being built | Wait for `the TensorRT engine for <model> is ready` in the log (section 6) |
| On a machine without an NVIDIA GPU the picture is black and `Warning` says the Maxine module cannot run | Only the Maxine module provides `background` (the ONNX module or its models or its runtime are missing, so its stage is `unavailable`) | Install the module, the models and `onnxruntime-cpu` (sections 2 and 3); `omarchy-broadcast modules` shows the reason |
| The log says `video memory: system` on an NVIDIA machine | `[video] memory` is `system`, or the probe found no usable CUDA (`auto`), or a start fell back after CUDA failed | The log line gives the reason; set `memory = "cuda"` to require CUDA, and reload |
| After a first install the module does not appear | The daemon reads manifests at start and on reload | `systemctl --user restart omarchy-broadcastd` |

## 9. Uninstall

`scripts/install-user.sh --uninstall` removes what the script installed, the module's library, manifest and models included. The package is removed with `sudo pacman -R omarchy-broadcast-onnx-git`. The TensorRT engine cache is under `$XDG_CACHE_HOME/omarchy-broadcast/` (by default `~/.cache/omarchy-broadcast/`) and may be deleted at any time; it is rebuilt on the next use.
