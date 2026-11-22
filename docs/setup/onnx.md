# Open-model cut-out (ONNX)

The open-model cut-out gives you background blur and background replacement without NVIDIA Maxine and without an NVIDIA GPU. It is a loadable module with the id `br.com.dias.onnx-cutout` ([modules.md](../modules.md)). It provides the `background` stage: it cuts the person out and blurs or replaces the room behind them. It uses open models under the Apache License 2.0. The module loads ONNX Runtime at run time, so one build works with whichever `onnxruntime-*` package you install.

This page covers how to install the module, its runtime and its models, how to choose the model and the engine, how to read the reasons an engine is not used, and the failures specific to omarchy-broadcast. Every setting, key and variable of the module is in its [README](../../crates/obc-module-onnx/README.md).

**What ONNX is.** ONNX is a standard file format for a trained neural network. ONNX Runtime is a library that runs such a file on many kinds of hardware: the CPU, an NVIDIA GPU (CUDA or TensorRT), and an Intel NPU, GPU or CPU (OpenVINO). The module hands the model to ONNX Runtime, which uses the best hardware the installed build offers. omarchy-broadcast contains and links no ONNX Runtime. The module opens `libonnxruntime.so` when it needs it, and the library comes from a package you install (section 2).

## Quick start

On a machine without an NVIDIA GPU, install the CPU runtime and the module, restart the daemon and check the module:

```bash
yay -S onnxruntime-cpu omarchy-broadcast-onnx-git
systemctl --user restart omarchy-broadcastd
omarchy-broadcast modules
```

`omarchy-broadcast-onnx-bin` is the prebuilt twin of `omarchy-broadcast-onnx-git` and works with either kind of core ([INSTALL.md](../../INSTALL.md#prebuilt-or-built-from-source)). The module package pulls in the default model. The `modules` output lists the module with its stage `background`, or with the reason it cannot run.

On an NVIDIA GPU, or for an Intel NPU or GPU, install the runtime and provider packages of sections 1 and 2 instead of `onnxruntime-cpu`.

## Using it on a machine without an NVIDIA GPU

The module needs no NVIDIA GPU, and neither does the daemon's effect path. When the machine has no usable CUDA, the daemon keeps video frames in system memory and hands the module host frames.

### Install

Install an ONNX Runtime build, the module and the models (section 3). `onnxruntime-cpu` or `onnxruntime-core` is enough. The omarchy-onnxruntime provider packages add the Intel and NVIDIA engines (section 2).

```bash
yay -S onnxruntime-cpu omarchy-broadcast-onnx-git
```

Install `onnxruntime-cpu` first, as above, or pick it when yay asks which provider of `onnxruntime` to install. The module depends on `onnxruntime>=1.29`, which several packages provide. The AUR's `onnxruntime-core` builds with CUDA, cuDNN, OpenVINO and TensorRT. That build is much larger than a machine without an NVIDIA GPU needs. `onnxruntime-cpu`, from the Arch repositories, is the CPU engine only.

### What the daemon decides

`[video] memory` in `config.toml` is `auto` by default. Under `auto` the daemon uses CUDA only when all of these hold:

- the NVIDIA driver is loaded and lists a GPU;
- `libcuda.so.1` opens;
- `CUDA_VISIBLE_DEVICES` does not hide every GPU;
- NVRTC (`libnvrtc.so`, from the `cuda` package, or the library named by `GST_CUDA_NVRTC_LIBNAME`) opens;
- GStreamer's CUDA elements are installed.

Otherwise it uses system memory. The daemon logs the choice and the reason once per change:

```bash
journalctl --user -u omarchy-broadcastd | grep "video memory"
# video memory: system (auto: no NVIDIA driver is loaded)
omarchy-broadcast config show | grep -A1 video.memory      # "Now: system memory, ..."
omarchy-broadcast modules                                  # the Maxine stages: unavailable, "Needs an NVIDIA GPU (CUDA)"
```

With both the Maxine and the ONNX module installed, this module provides `background` in system memory whatever the profile. A pin to the Maxine module falls back to it, with the reason. You need no other setting: with no NVIDIA driver, the probe says so without touching a GPU.

### What is unavailable

The effects that need CUDA frames are unavailable: the Maxine cut-out, noise removal, relighting, the HDR look, enhancement, eye contact, auto-framing and frame generation (so no "(generated)" output rate is offered). The settings grey them out and show the reason. This module's cut-out and the host modules work.

### On an NVIDIA machine

You can choose the same path with `[video] memory = "system"`, for instance to keep the GPU asleep or to run the CPU path for a check. The daemon then makes no CUDA call, and it tells the module so.

- **Under `auto`,** the module skips TensorRT and CUDA with the reason `CUDA is off: the video pipeline runs in system memory`. It runs on the CPU, or on OpenVINO if your ONNX Runtime has it.
- **With an explicit `effect.onnx.engine` of `cuda` or `tensorrt`,** the module runs the first later engine that starts (section 4, "A chosen engine that cannot run"), and `Warning` says so.
- **On a laptop,** this keeps the NVIDIA GPU out of the effect path. `auto` then prefers the Intel NPU when the runtime has OpenVINO (the order is in section 4).
- **A change of `memory`** applies at the next camera start.
- **If CUDA looks usable but fails at camera start** (a driver and library that do not match, or `CUDA_VISIBLE_DEVICES` naming a GPU that is not there), `auto` starts again once in system memory. It logs a warning with the CUDA error and stays there until the next reload.

### The battery saver

`[video] battery_saver` runs the same system-memory path on a laptop's battery. It is the "Battery saver" row of "Video processing" in the settings (D-Bus revision 22; the default `auto` from revision 23).

Under `battery_saver = "auto"` with `memory = "auto"`, the daemon keeps frames in system memory even though CUDA is usable when either of these holds:

- the machine runs on its battery;
- power-profiles-daemon's profile is `power-saver`. This applies on AC power too, when you choose Power saver in the desktop's power menu.

The NVIDIA GPU then stays asleep. The module is told CUDA is off, as under `memory = "system"`:

- `auto` skips TensorRT and CUDA and, with OpenVINO in your ONNX Runtime, prefers the Intel NPU, then the Intel GPU, then the CPU.
- An explicit `cuda` or `tensorrt` falls back the same way, with the notice in `Warning`.

This is how you run the cut-out on the NPU on battery with no setting to change by hand. Without power-profiles-daemon, `auto` follows the battery alone.

The modes are:

| Mode | The saver is active |
|---|---|
| `auto` (default) | On battery, or in the `power-saver` profile. Without power-profiles-daemon, on battery alone |
| `on_battery` | On battery alone |
| `power_profile` | In the `power-saver` profile alone. Without that service, on battery |
| `always` | Always |
| `off` | Never |

The saver acts only under `memory = "auto"`. With `cuda` or `system` the row is greyed.

- **A change applies at the next camera start.** A running camera keeps its memory. The row says "Applies when the camera restarts" while the saver now decides otherwise.
- **What it takes away.** In system memory the CUDA-only stages are unavailable (the list above), with the reason "Needs CUDA; battery saver runs the video pipeline in system memory". Set `battery_saver = "off"` to keep them on the battery.

To see what the saver decided:

```bash
journalctl --user -u omarchy-broadcastd | grep "video memory"
# video memory: system (auto: battery saver, on battery)      (or ... power profile power-saver)
omarchy-broadcast config show | grep -A1 battery_saver     # "Now: active, on battery."
```

### The cost on the CPU

Measured on the reference machine: an Intel Core Ultra 9 275HX laptop, the CPU build of ONNX Runtime 1.31.1, the default MediaPipe model and the module's default of 4 threads. An application read the virtual camera at 30 fps with the blur on.

| Camera, output | Daemon CPU, blur (effect off) | Effect, per processed frame |
|---|---|---|
| MJPEG 1080p30, 1080p30 | 42 % of one core (17 %) | 6.0 ms median, 7.2 ms at p95 |
| NV12 1080p60, 1080p30 | 29 % of one core (1 %) | 6.3 ms median, 7.4 ms at p95 |
| MJPEG 720p30, 720p30 | 25 % of one core (8.5 %) | not measured |

- The virtual camera held 30 fps with no dropped frame in every row.
- The blur adds about 25 to 27 % of one core at 1080p and about 16 % at 720p. The rest is the camera's own decode and conversion, which is the cost of an MJPEG camera in any case.
- This is a high-end laptop CPU. A machine with four low-power cores is expected to take two to three times as long per frame. That has not been measured.
- A 720p camera mode into a 720p output costs about 40 % less (25 % against 42 %). A 720p output from a 1080p camera does not, because the camera is still decoded at 1080p and then scaled (43 %).
- Known limit: with a camera that delivered about 20 fps in dim light, two black frames appeared in about 5,300 under blur, each after an output gap of 70 to 74 ms. None appeared in 60 s without an effect or with the other camera. The writer shows its placeholder when no frame arrives for two output periods (66.7 ms at 30 fps). The cause is not isolated.
- `threads` in the module's table sets the CPU threads (see the module's [README](../../crates/obc-module-onnx/README.md)). More threads shorten a frame and cost more CPU in total.

Method and per-step figures: [../benchmarks/system-memory.md](../benchmarks/system-memory.md).

### Privacy on the CPU path

The privacy rules are the same as with CUDA:

- **Privacy on** (`[effect] privacy`): the room is never shown while an effect is on and cannot run. You get black frames, the away image or the last frame, by `privacy_mode`. At a start the picture goes black until the matte is ready.
- **Privacy off** (the default): the camera is shown instead. Turning the blur on mid-stream keeps the picture for under a frame on the CPU, then switches with no black frame.

## 1. What you need

| Item | Requirement |
|---|---|
| The module | Installed from the package `omarchy-broadcast-onnx-git` (or its prebuilt twin `omarchy-broadcast-onnx-bin`), or by `scripts/install-user.sh` (section 3) |
| ONNX Runtime | `libonnxruntime.so`, version 1.29 or newer. With no NVIDIA GPU: `onnxruntime-cpu` (see above). Otherwise recommended: `onnxruntime-core` plus the provider packages of the engines you use (CUDA, TensorRT, OpenVINO), from omarchy-onnxruntime. `onnxruntime-cpu` and `onnxruntime-opt-cuda` also work (section 2) |
| The models | One package per model, `omarchy-onnx-model-<id>`, or the `.onnx` files in one of the directories of section 3. A single file is enough to use that model |
| An NVIDIA GPU | Not needed. The CPU engine runs the model, and the daemon runs the effect path in system memory when it finds no usable CUDA (see above). An NVIDIA GPU adds the TensorRT and CUDA engines. An Intel CPU, GPU or NPU adds the OpenVINO engines, with a runtime that has them |

Without ONNX Runtime, or without any model file, the module still loads. `omarchy-broadcast modules` then lists its stage as `unavailable`, with the reason `ONNX Runtime not installed (libonnxruntime.so not found)` or `No cut-out model installed (searched: ...)`. Nothing else is affected.

## 2. Install ONNX Runtime

### Arch's packages or omarchy-onnxruntime

Two package sets provide ONNX Runtime 1.29.0 with the same API, and the module works with either. They differ in the hardware they cover.

- **Arch's packages** come from the official `extra` repository and make the smallest install. Each one is a complete build: `onnxruntime-cpu` (the CPU engine), `onnxruntime-cuda` and `onnxruntime-opt-cuda` (the CPU and CUDA engines; `-opt` is built with AVX2), and the ROCm builds. You install one of them.
- **omarchy-onnxruntime** ([repository](https://github.com/rsd/omarchy-onnxruntime)) is one build split into one package per engine, so you install only the engines your hardware has. It covers more hardware: it adds TensorRT on NVIDIA GPUs, OpenVINO on an Intel CPU, GPU or NPU, and a CUDA 12 line for older NVIDIA GPUs (Pascal and Volta, untested on that hardware). Each engine's vendor runtime carries version bounds, so pacman refuses a system update that would break an engine instead of leaving one that fails to load. Prebuilt `-bin` packages are on its releases page. It depends on `nccl` even on a machine without an NVIDIA GPU.

The two sets conflict, because both install `libonnxruntime.so`. Install one set per machine.

| Your hardware | Take | Engines the module then uses |
|---|---|---|
| A CPU only | Arch: `onnxruntime-cpu` | CPU |
| An NVIDIA GPU, the CUDA engine is enough | Arch: `onnxruntime-opt-cuda`, or omarchy-onnxruntime: `onnxruntime-core` and `onnxruntime-provider-cuda13-<preset>` | CUDA, CPU |
| An NVIDIA GPU, with TensorRT | omarchy-onnxruntime: `onnxruntime-core`, `onnxruntime-provider-cuda13-<preset>` and `onnxruntime-provider-tensorrt-cuda13` | TensorRT, CUDA, CPU |
| An Intel CPU, GPU or NPU | omarchy-onnxruntime: `onnxruntime-core` and `onnxruntime-provider-openvino` | OpenVINO (NPU, GPU, CPU), CPU |
| An AMD GPU | Either set | CPU. The module has no AMD engine yet |

The repository's [docs/choosing.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/docs/choosing.md) compares the two sets in full, with sources.

### Recommended: `onnxruntime-core` and the provider packages you need

The omarchy-onnxruntime repository, <https://github.com/rsd/omarchy-onnxruntime>, builds these packages from one build of ONNX Runtime 1.29.0, split into one package per engine:

| Package | Engine |
|---|---|
| `onnxruntime-core` | `libonnxruntime.so` with the CPU engine. It works on its own. Replaces and conflicts with the earlier single package `onnxruntime-full` |
| `onnxruntime-provider-cuda13-<preset>` (preset `sm120`, `rtx`, `all` or a list of architectures) | The CUDA engine. Needs `cuda`, `cudnn` and `nvidia-utils` |
| `onnxruntime-provider-tensorrt-cuda13` | The TensorRT engine. Needs the CUDA provider and `tensorrt` |
| `onnxruntime-provider-openvino` | The OpenVINO engines (Intel CPU, GPU and NPU). Needs `openvino` |
| `python-onnxruntime-core` | The Python module (the module does not use it) |

Each provider is a library of its own beside `libonnxruntime.so`, loaded only when it is asked for. A provider that is not installed, or whose vendor runtime is missing, disables that engine and nothing else.

Install `onnxruntime-core` and the providers for the hardware you have (a subset is fine) together in one `pacman -U`, after the vendor runtimes of those engines. The repository's [INSTALL.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/INSTALL.md) gives the procedure and the order of the steps.

AMD GPUs have no engine in the module yet. Arch's ROCm builds and the omarchy-onnxruntime package `onnxruntime-provider-migraphx` (untested on AMD hardware) carry AMD providers, but the module does not use them. On an AMD machine it runs the CPU engine.

The vendor runtimes of each engine are:

| Engine | Packages |
|---|---|
| NVIDIA CUDA | `cuda`, `cudnn`, `nvidia-utils` |
| NVIDIA TensorRT | TensorRT 11, as the package `tensorrt`: the AUR's `tensorrt` (under `/usr`) or `nvidia-tensorrt-sdk-local` from omarchy-nvidia-sdks (under `/opt/nvidia/tensorrt`); see "TensorRT's library path" below |
| OpenVINO, Intel CPU | `openvino` |
| OpenVINO, Intel GPU | `openvino-intel-gpu-plugin`, `intel-compute-runtime`, and an Intel GPU present in the running system (on a laptop with a GPU switch, hybrid mode rather than discrete-only) |
| OpenVINO, Intel NPU | `openvino-intel-npu-plugin`, `intel-npu-driver`, `level-zero-loader`, and access to `/dev/accel/accel0` (normally through the `render` group) |

The repository's [docs/engines.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/docs/engines.md) describes each engine and what it needs. Its [docs/results.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/docs/results.md) has raw-model figures of four models on six targets.

### Alternatives

Any other package that provides `onnxruntime` 1.29 or newer works with the module. With no NVIDIA GPU, take `onnxruntime-cpu`, which the install command [above](#using-it-on-a-machine-without-an-nvidia-gpu) also uses:

```bash
sudo pacman -S onnxruntime-opt-cuda   # an NVIDIA GPU: the CPU and the CUDA engines
sudo pacman -S onnxruntime-cpu        # anything else: the CPU engine only
```

The CUDA provider of `onnxruntime-opt-cuda` also needs cuDNN (the `cudnn` package) and the NVIDIA driver's CUDA library. A missing `libcudnn.so.9` shows as a reason (section 5). The Arch packages ship the CPU, CUDA and ROCm providers. The module has no ROCm engine, and these builds have no TensorRT or OpenVINO engine.

### Which engines you get

The engines you get depend on what the build includes. The module asks the loaded library which providers it has. It checks that each provider's library (`libonnxruntime_providers_<name>.so`) is beside `libonnxruntime.so`, and it tries only the engines that pass. A build with more providers lights the other engines up with no change to the module.

### Where the module looks for the library

The module looks for the library in this order:

1. `$ORT_DYLIB_PATH`;
2. `ort_library` in `[module."br.com.dias.onnx-cutout"]` of `config.toml`;
3. `libonnxruntime.so.1` and `libonnxruntime.so` in the system's library path.

ONNX Runtime allows one environment per process, and the first library the daemon opens stays until the daemon restarts. The environment also lives for the whole process. With the omarchy-onnxruntime build of 1.29.0 and OpenVINO 2026.4.1, a program that releases the environment after it used OpenVINO crashes when it exits. So the module never releases it: not when the cut-out unloads, not when an instance is dropped and not at exit. A test (`tests/exit.rs`) checks that a process that ran a model on OpenVINO exits cleanly.

### TensorRT's library path

TensorRT needs its library directory on the daemon's `LD_LIBRARY_PATH`. TensorRT 11 opens its per-GPU builder library by name, `libnvinfer_builder_resource_sm<NN>.so.<version>` (on TensorRT 10, `libnvinfer_builder_resource.so.<version>`), and only the process's library path finds that name. Without the path, ONNX Runtime's TensorRT provider crashes the process, both when it builds an engine and when it loads a cached one.

The module therefore checks, before it starts TensorRT, that the library for the installed GPU can be loaded. If it cannot, TensorRT is refused with a reason (section 5). `auto` goes on to CUDA, and an explicit `tensorrt` fails the load.

**With the AUR's `tensorrt`,** which installs under `/usr/lib`, you need nothing: that directory is on the library path already.

**With `nvidia-tensorrt-sdk`,** whose libraries are under `/opt/nvidia/tensorrt/lib`, give the daemon the package's environment file. It sets `LD_LIBRARY_PATH=/opt/nvidia/tensorrt/lib`. Use a systemd user drop-in at `~/.config/systemd/user/omarchy-broadcastd.service.d/onnx-tensorrt.conf`:

```ini
[Service]
EnvironmentFile=-/usr/share/nvidia-tensorrt/nvidia-tensorrt.env
```

Then reload systemd and restart the daemon:

```bash
systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd
```

The `-` makes a missing file no error. The setting applies to the daemon's unit only. Do not put TensorRT's directory on `LD_LIBRARY_PATH` in a login profile. The directory holds only TensorRT's own libraries, but the rule of [maxine.md](maxine.md) stands: a library path set for every program you start can shadow the system's libraries.

**With the Maxine module.** The daemon applies the Maxine SDK package's environment itself. It never applies that environment's `LD_LIBRARY_PATH`, which would put the SDK's bundled CUDA and TensorRT libraries ahead of the engines of ONNX Runtime ([maxine.md](maxine.md#3-the-sdk-environment)). The value you give the unit above is the one the daemon runs with. The Maxine module loads its libraries from its SDK root by absolute path and does not need it ([maxine.md](maxine.md)).

If you still have the `maxine.conf` drop-in of an earlier version, remove it. It is not needed any more, and it sets `LD_LIBRARY_PATH` too. When two files set a variable, the later one in the order of the file names wins, so the TensorRT drop-in would have to sort after it.

### TensorRT after the Maxine effects in the same daemon

The Maxine SDKs bring their own TensorRT (10.9), which the Maxine modules load into the daemon so that the SDK finds it. TensorRT's entry points have the same names in every release. So when ONNX Runtime's TensorRT provider is first loaded after a Maxine effect ran, it calls the SDK's TensorRT instead of its own TensorRT 11, and crashes the daemon.

The module checks for this before it starts TensorRT. When another TensorRT is already loaded and the provider is not, TensorRT is refused with the reason `TensorRT not available: TensorRT 10.9.0 (<its file>), loaded in the daemon by another effect, is not the one ONNX Runtime's TensorRT provider uses (11.4.0), ...`.

- `auto` goes on to CUDA.
- An explicit `tensorrt` runs on CUDA, with the notice in `Warning`.
- A cut-out that ran on TensorRT before the Maxine effects runs on TensorRT again afterwards, because the provider is bound to its own TensorRT once loaded.

To get TensorRT back after a switch from Maxine, restart the daemon with the cut-out selected:

```bash
systemctl --user restart omarchy-broadcastd
```

## 3. Install the module and the models

### From the AUR or the Arch package

With an AUR helper, install `omarchy-broadcast-onnx-git`, or `omarchy-broadcast-onnx-bin`, the prebuilt package with the same files. Either works with the core of either kind ([INSTALL.md](../../INSTALL.md#prebuilt-or-built-from-source)). With no NVIDIA GPU, install it together with the CPU runtime ([above](#using-it-on-a-machine-without-an-nvidia-gpu)):

```bash
yay -S onnxruntime-cpu omarchy-broadcast-onnx-git
```

The module is one package of the split build of `packaging/arch/PKGBUILD`, built with the core `omarchy-broadcast-git` and the other module packages. It depends on:

- the module ABI that the core provides (`omarchy-broadcast-module-abi>=1.6` and `<2`, [../modules.md](../modules.md#packaging-a-module));
- `onnxruntime>=1.29`, which any variant of section 2 satisfies;
- the package of the default model, `omarchy-onnx-model-mediapipe-landscape`.

It installs the library and the manifest under `/usr/lib/omarchy-broadcast/`. It is a separate package because the ONNX Runtime variants conflict with one another: the core must not pull one in. Its optional dependencies are the other model packages and the engines, as the virtual names `onnxruntime-provider-cuda`, `onnxruntime-provider-tensorrt` and `onnxruntime-provider-openvino` (section 2).

### The model packages

The models are packages of their own, one per model, from the [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models) repository (`packaging/arch/PKGBUILD` there). The name is `omarchy-onnx-model-<id>`, with the model's id from the table below. The version is that of the repository's release: 1.0.0, from the `v1.0.0` release assets, each checked against the release's SHA-256 at build time.

Each package installs its one `.onnx` file in `/usr/share/omarchy-onnx-models/`, with that model's licence and notice in `/usr/share/licenses/omarchy-onnx-model-<id>/`. The models are kept apart from the module for two reasons:

- the four files are about 70 MB, which a `-git` rebuild of the module would otherwise download again each time;
- each model has its own licence terms.

The default model's package is a dependency of the module. Install the others when you want to choose them:

```bash
sudo pacman -U omarchy-onnx-model-mediapipe-landscape-*.pkg.tar.zst   # the default
sudo pacman -U omarchy-onnx-model-modnet-*.pkg.tar.zst                  # optional
```

### From a checkout

`scripts/install-user.sh` builds and installs the module for the current user, without root. It puts the library in `~/.local/share/omarchy-broadcast/modules/` and the manifest in `~/.local/share/omarchy-broadcast/modules.d/`.

Give it the models with `OBC_ONNX_MODELS_DIR`, a directory that holds the four files under their catalogue names. The script checks each against its SHA-256 and copies it to `~/.local/share/omarchy-broadcast/models/onnx/`:

```bash
OBC_ONNX_MODELS_DIR=/path/to/models scripts/install-user.sh
```

Without the variable, the module is installed and shows as unavailable until the models are there. Run the script again with the variable to add them. A run without the variable keeps models installed earlier.

### Where the module looks for a model

The first directory that has the file wins:

1. `model_dir` in `[module."br.com.dias.onnx-cutout"]` of `config.toml`;
2. `$OBC_ONNX_MODEL_DIR`;
3. `$XDG_DATA_HOME/omarchy-broadcast/models/onnx/` (by default `~/.local/share/omarchy-broadcast/models/onnx/`);
4. `/usr/share/omarchy-onnx-models/`, where the model packages install;
5. `/usr/share/omarchy-broadcast/models/onnx/` (the directory of the earlier single package, still searched).

### The models

The models are the project's own conversions to ONNX of the upstream releases, which keep the weights of the originals. They are published, with the scripts that make them and their checksums, as release assets of the [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models) repository ([`crates/obc-module-onnx/model-sources/README.md`](../../crates/obc-module-onnx/model-sources/README.md) points to it). The module checks each file against its SHA-256 when it loads. A missing or different file fails the load, with a reason that names the file, the package to install (`omarchy-onnx-model-<id>`) and the directories searched.

| Setting value | File | Upstream |
|---|---|---|
| `mediapipe-landscape` (default) | `selfie_segmentation_landscape.onnx` | Google, MediaPipe selfie segmentation (landscape) |
| `pphumanseg-v2-mobile` | `pp_humansegv2_mobile_192x192.onnx` | Baidu, PaddleSeg PP-HumanSeg v2, mobile |
| `pphumanseg-v2-lite` | `pp_humansegv2_lite_192x192.onnx` | Baidu, PaddleSeg PP-HumanSeg v2, lite |
| `modnet` | `modnet_photographic.onnx` | MODNet, photographic portrait matting |

**Licences.** All four models are released under the Apache License, Version 2.0. The text, the notices and the provenance of each file are in [`model-sources/LICENSES.md`](../../crates/obc-module-onnx/model-sources/LICENSES.md), and the package installs them with the files. The training-data terms of PP-HumanSeg (a dataset available from its publisher on request, with no licence stated publicly) and of MODNet are not confirmed, and the file records that. The default model, MediaPipe, has the simplest provenance. If this matters for how you use the camera, the file is the reference.

### Check the install

Restart the daemon, then list the modules:

```bash
systemctl --user restart omarchy-broadcastd
omarchy-broadcast modules
```

The module is listed with its stage `background`, or with the reason it cannot run.

## 4. Choose the module, the model and the engine

### Choose the module

The module provides `background` in the `matte` slot with rank 200 and the device `cpu`. The Maxine module provides the same stage with rank 300. With both installed:

- the `quality` profile picks Maxine;
- the `battery` profile picks this module;
- a pin chooses regardless of the profile.

```bash
omarchy-broadcast modules profile battery
omarchy-broadcast modules pin background br.com.dias.onnx-cutout
omarchy-broadcast modules pin background --clear
```

A module choice applies when the camera next starts ([../cli-json.md](../cli-json.md)).

### Choose the model and the engine

The module's settings are in the group "Open-model cut-out" of `omarchy-broadcast settings` and of the bar plugin's panel. They are greyed while the background effect is off.

| Setting | Values | Default |
|---|---|---|
| `effect.onnx.model` ("Model") | `mediapipe-landscape` ("MediaPipe (fast)"), `pphumanseg-v2-mobile` ("PP-HumanSeg mobile (quality)"), `pphumanseg-v2-lite` ("PP-HumanSeg lite"), `modnet` ("MODNet (sharp hair, GPU)") | `mediapipe-landscape` |
| `effect.onnx.engine` ("Runs on") | `auto` ("Automatic"), `tensorrt` ("NVIDIA GPU, TensorRT (fastest)"), `cuda` ("NVIDIA GPU, CUDA"), `openvino-npu` ("Intel NPU (battery saver)"), `openvino-gpu` ("Intel GPU (OpenVINO)"), `openvino-cpu` ("CPU, OpenVINO"), `cpu` ("CPU") | `auto` |

```bash
omarchy-broadcast settings set effect.onnx.model pphumanseg-v2-mobile
omarchy-broadcast settings set effect.onnx.engine cpu
```

The labels are what the settings show. The values are what `settings set` takes. The engine names in log lines and reasons are not the labels: `NVIDIA TensorRT`, `NVIDIA CUDA`, `OpenVINO NPU`, `OpenVINO GPU`, `OpenVINO CPU` and `CPU`.

The blur strength and the replacement image are the daemon's own background settings (`effect.background.*`), shared with the Maxine module.

### What a change does while the camera runs

A change of either setting is a **switch**. The previous model keeps running until the new one is loaded, then the picture changes, with no black frame.

**A switch that cannot load is refused, and the picture does not change.** Causes are a model file or replacement image that cannot be read, or a stage that fails to load. An engine you chose that cannot run is not one of these: it falls back (see "A chosen engine that cannot run" below). When a switch is refused:

- The previous model keeps processing every frame. The picture stays the cut-out it was, never black and never the away image, and the effect stays ready.
- `Error` stays empty. `Warning` says why, with the clause `<module>: <reason>; the previous setting keeps running`.
- The setting itself is not rewritten. What you chose stays chosen, and the settings keep showing it. So the engine that runs and the engine that is stored can differ until you act.
- The refused value is tried again only when you change it or apply the unchanged settings again. Other changes in the meantime (turning a stage on, blur to replace, another camera) load against the model and engine that run.
- Turning the effect off, or a new camera format, ends the refusal.

A refused value that is still stored is tried at the next camera start, where black or the away image is allowed. If it fails to load there, the effect fails as the privacy setting says. Set the setting back to avoid that.

Each refused value is remembered separately. Two values that both cannot load are not retried alternately, and selecting a background video does not try a refused value again. If the running chain is lost while a new setting is loading, the effect fails with the reason for the loss, not with the refusal.

**With system-memory frames,** the new model loads on a second instance of the module while the first keeps running, and the picture changes when it is ready. The two are alive together while it loads, which costs the memory of a second session. Any failure of that load, a module panic or a failing stage included, is a refusal and leaves the running chain untouched. Only when a second instance cannot be created at all does the switch happen on the loaded instance, as on CUDA frames.

**With CUDA frames,** the switch loads on the running instance instead. The module's CUDA stream is bound when it loads, and a second one is not safe to use mid-frame. So a slow switch, such as a first TensorRT engine build or the fallback engines being tried, holds the last frame for as long as it takes. It is never black.

### Engines under `auto`

`auto` tries the engines in this order and takes the first that starts:

1. NVIDIA TensorRT
2. NVIDIA CUDA
3. OpenVINO on an Intel NPU
4. OpenVINO on a GPU
5. OpenVINO on the CPU
6. ONNX Runtime's own CPU provider

On a laptop, three things keep the NVIDIA GPU out of the effect path: `[video] memory = "system"`, and the battery saver while the machine runs on its battery or in the `power-saver` profile. `auto` then skips TensorRT and CUDA and, with OpenVINO in the runtime, prefers the NPU (see "Using it on a machine without an NVIDIA GPU").

### A chosen engine that cannot run

An engine you choose yourself (`effect.onnx.engine` other than `auto`) that cannot run does not fail the load. It can be ruled out without being tried (a provider library that is missing, TensorRT without its libraries, CUDA off because the video pipeline runs in system memory), or it can fail to start. The module then tries the engines **after** it in the order above and runs the first that starts.

- A choice of an OpenVINO or CPU engine therefore never falls back to TensorRT or CUDA, and never wakes the NVIDIA GPU.
- The setting you stored is not changed.

The load says so: `Warning` gains the clause `<module name>: <Requested> was chosen and cannot run: <reason>; using <engine>`. The clause ends when a load runs the chosen engine, at stop, with a new camera format and when the effect is turned off. A live switch to an engine that cannot run succeeds the same way, on the fallback. It is not a refused switch.

Only when no later engine starts does the load fail, with `<Chosen> was chosen and cannot run: <reason>; no later engine could run it (<engine>: <reason>; ...)`. The list is absent when no engine follows the chosen one (the CPU provider, the last). The privacy setting then applies. Turning the effect off and on again with such an engine stored therefore does not go black.

### The matte never fails open

As with Maxine, a frame that fails in the cut-out fails the whole chain, and the effect follows the privacy setting (`[effect] privacy`, off by default; its `privacy_mode` says what is shown):

- With privacy on, the raw camera image is never shown in place of a cut-out that failed.
- With privacy off, the camera is shown.

A model that fails to load leaves what was running as it was.

### Measured on each engine

These are the same model and frames on each engine of the omarchy-onnxruntime build of 1.29.0. They were measured on its earlier single package, `onnxruntime-full`; the library is the same.

Conditions:

- an Intel Core Ultra 9 275HX laptop with a GeForce RTX 5080 Laptop GPU (compute capability sm120) and the integrated Intel GPU and NPU;
- TensorRT 11.4.0, CUDA 13 and OpenVINO 2026.4.1;
- the default model (MediaPipe landscape);
- the module's hardware test, which runs CUDA NV12 frames of a 1280x720 test image with the blur; medians;
- another application was streaming a camera from the GPU during the runs, so the figures are indicative.

Every engine kept the same 32.8 % of the picture: the matte is identical.

Times are per frame, in milliseconds. "Mask" is the model's own run. "All steps" is the module's whole frame: convert in, mask, composite and convert out. "Session create" is the time to load the model on the engine, and "first run" the time of the first frame.

| Engine | Mask (model) | All steps | Session create | First run |
|---|---|---|---|---|
| NVIDIA TensorRT | 0.37 | 3.39 | 99 ms (engine cached) | 3 ms |
| NVIDIA CUDA | 0.65 | 3.54 | 83 ms | 307 ms |
| Intel NPU (OpenVINO) | 2.41 | 5.26 | 40 ms | 8 ms |
| Intel GPU (OpenVINO) | 1.85 | 4.31 | 256 ms | 27 ms |
| CPU, OpenVINO | 0.97 | 3.85 | 117 ms | 2 ms |
| CPU | 1.16 | 3.85 | 7 ms | 2 ms |

At this size the compositing and the conversions, not the model, are most of a frame. The slowest and the fastest engine differ by about 2 ms, at the mask and in all steps. The NPU is the slowest engine here. Its value is lower power, not speed, and no power figures were measured. A first TensorRT engine build for a model took about 10 s (10.6 s for `pphumanseg-v2-lite`). Later loads read the cache (section 6). Raw-model figures of all four models on all six targets are in the omarchy-onnxruntime repository's [docs/results.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/docs/results.md).

## 5. Reading the reasons

The module writes one line per engine to the daemon's log (`journalctl --user -u omarchy-broadcastd`), at three moments:

- **When it is probed** (files only). The engines that the files already rule out, for example a provider library missing beside `libonnxruntime.so`, or a GPU hidden by an empty `CUDA_VISIBLE_DEVICES`.
- **When it is checked** (before the camera starts; the runtime is opened, no device is touched and no CUDA call is made). For each engine, whether the loaded ONNX Runtime build has it. The next step finds out whether the device runs it.
- **When the cut-out loads.** Each engine ruled out without being tried (`ONNX cut-out: <engine> unavailable: <reason>`: a provider library that is missing, TensorRT without its libraries, CUDA turned off). Each engine tried that did not start, with the error from ONNX Runtime. Then the line that says where the model runs: `ONNX cut-out: <model> runs on <engine> (ONNX Runtime <version>; ...)`. A chosen engine that could not run puts its reason in the notice of the load (the `Warning` clause above). When no later engine starts either, the reason goes in the error that the load returns, which the daemon reports.

| Reason | Means |
|---|---|
| `not in this ONNX Runtime build (no libonnxruntime_providers_<x>.so beside ...)` | The loaded library has no such provider, or its provider library is not next to `libonnxruntime.so`. The engine is not tried: `auto` skips it, and a chosen engine falls back to a later one with this reason in the notice. Install the provider package for the engine (the message names it) or a build that has it (section 2) |
| `CUDA not available: ...` | The CUDA provider is there; the GPU or its libraries are not. The CUDA error follows, for example `no CUDA-capable device is detected`, or a missing `libcudnn.so.9` |
| `TensorRT not available: TensorRT's libraries are not on the daemon's library path (... not found; add TensorRT's lib directory to LD_LIBRARY_PATH)` | TensorRT's builder library cannot be found by name. TensorRT is not started (it would crash the process), `auto` moves on to CUDA, and a chosen `tensorrt` falls back to CUDA (or a later engine) with a notice. Add TensorRT's directory for the daemon (section 2, "TensorRT's library path") |
| `TensorRT not available: TensorRT <ver> has no builder library for this GPU (sm<NN>)` | TensorRT's libraries are found, but the installed TensorRT ships no builder library for this GPU's architecture. Use a TensorRT version that supports the GPU, or another engine |
| `TensorRT not available: ...` (other) | Another TensorRT library cannot be loaded (the loader's message follows), or no NVIDIA GPU was found through the CUDA driver |
| `OpenVINO NPU: ...`, `OpenVINO GPU: ...` | OpenVINO could not use the device. An NPU needs its user-space driver and read and write access to `/dev/accel/accel0` (the `render` group). The GPU engine needs an Intel GPU that is present in the running system (see section 8) |
| `the provider was registered but ran no part of the model` | The provider started and ran nothing. OpenVINO does this when its device is missing |
| `partly on the CPU (k of n kernels)` | The engine runs, but ONNX Runtime left some of the model to the CPU |
| `CUDA is off: the video pipeline runs in system memory` | The daemon runs in system memory and told the module not to use CUDA, so TensorRT and CUDA are skipped (a chosen one falls back to a later engine, with a notice). Set `[video] memory = "cuda"` to use them |

"Partly on the CPU" is measured, not guessed. The module profiles the first run of a session, which tells which provider ran each kernel, and stops profiling after that run. The profile goes in a new private directory under `$XDG_RUNTIME_DIR` (else the temporary directory) and is removed at once.

## 6. The TensorRT first-use build

TensorRT compiles the model into an engine for your GPU. The first time a model runs on TensorRT, this takes from several seconds to under a minute. The log says `preparing the TensorRT engine for <model> (first use: this takes a while)...` and then `the TensorRT engine for <model> is ready`.

The engine is cached in `$XDG_CACHE_HOME/omarchy-broadcast/trt/`, in a directory named for the model file's checksum, the ONNX Runtime version, the TensorRT version, the GPU and its compute capability. An engine built for another GPU or another TensorRT is never taken for the current one. Later loads read the cache in under a second.

The build runs when the effect is turned on or when the camera starts. Until it ends, with privacy on (`[effect] privacy`) the picture is black (or the away image, or the last frame) at a start; with privacy off, the default, it is the camera as it is. When the effect is turned on mid-stream, the picture stays as it was until the effect is ready.

**Changing the model while the camera runs is different,** because the stream waits for a switch:

- Under `auto`, a switch to a model with no cached TensorRT engine does not build one. It runs on CUDA for now, and the log says `TensorRT engine not built yet; using CUDA for now. It builds at the next camera start.`
- With `effect.onnx.engine` set to `tensorrt`, the switch builds the engine, and the feed pauses until it is ready. The log says so.

## 7. The GPU is left as it was found

ONNX Runtime's CUDA and TensorRT engines run through the CUDA runtime, which activates the device's primary CUDA context. When the last session that used one of them ends (an unload, or a failed attempt of `auto`), the module resets that primary context. No CUDA context of the module's outlives its sessions, so the GPU can be parked at idle. The context of the frames, GStreamer's, is another context and is untouched. The rule and its bounds are in [../modules.md](../modules.md#threading-and-cuda-rules).

### The Intel GPU engine and the NVIDIA GPU

OpenVINO's GPU engine (`openvino-gpu`, or `auto` reaching it) starts OpenCL. The OpenCL loader opens every driver listed in `/etc/OpenCL/vendors/`, NVIDIA's included. On a laptop with both GPUs that wakes the NVIDIA GPU. It then stays awake with its `/dev/nvidia*` files open until the daemon exits, and the first OpenVINO GPU session takes about 2.7 s instead of about 0.1 s.

So the module limits the loader to the Intel GPU's driver, once, when the daemon opens the module. It does this on the daemon's controller thread, before ONNX Runtime or OpenVINO has started any thread of its own, and whether or not `openvino-gpu` is used.

- It makes `$XDG_RUNTIME_DIR/omarchy-broadcast/opencl-vendors/` (mode 0700), links in it the `.icd` files of `/etc/OpenCL/vendors/` that name Intel's compute runtime (`libigdrcl`), and sets `OCL_ICD_VENDORS` to that directory, with a trailing slash (the loader that the CUDA toolkit installs needs it). Both OpenCL loaders that may be installed, ocl-icd's and CUDA's `/opt/cuda/lib64/libOpenCL.so.1`, honour the setting.
- The variable is set once, in the environment of the process that opens the module. The processes that process starts afterwards inherit it. That is every such process: the daemon, and the command-line tools that open the module to probe it. Nothing under `/etc` is changed.
- It is left alone when any `OCL_ICD_*` variable is already set, which is your override (put your own `OCL_ICD_VENDORS` in the daemon's environment to choose the drivers yourself), and when no Intel GPU driver is installed.
- The NPU and CPU engines load no OpenCL and are not affected.

The NVIDIA GPU then stays asleep under `openvino-gpu`. The log says whether the limit was applied.

## 8. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `modules` lists the stage `unavailable: ONNX Runtime not installed (libonnxruntime.so not found)` | No `libonnxruntime.so` where the module looks | Install a package of section 2, or set `ORT_DYLIB_PATH` in the daemon's environment or `ort_library` in the module's table, then restart the daemon |
| `unavailable: No cut-out model installed (searched: ...)` | No model file in the directories listed | Install the models (section 3). The message names the directories searched |
| A model fails to load with a checksum or file error | The file is not the catalogue's: a different conversion, a truncated copy | Use the files that the package or the checkout's scripts produce. Their checksums are in the module's README |
| The model runs on the CPU although you have an NVIDIA GPU | `auto` found nothing better: the installed build is `onnxruntime-cpu`, or the CUDA reason says why (`CUDA not available: ...`) | Install `onnxruntime-core` with a CUDA provider package (or `onnxruntime-opt-cuda`) and `cudnn`, and read the reason in the log (section 5) |
| The log says TensorRT's libraries are not on the daemon's library path, and the model runs on CUDA | The daemon's `LD_LIBRARY_PATH` lacks TensorRT's directory (`/opt/nvidia/tensorrt/lib` with `nvidia-tensorrt-sdk-local`) | Add the drop-in of section 2, "TensorRT's library path", then reload systemd and restart the daemon |
| `Warning` says `<engine> was chosen and cannot run: <reason>; using <other engine>` | The engine you chose cannot run here. The module runs the first later engine of the order that starts. The stored setting is unchanged | Read the reason and fix it, or choose `auto` or the engine that runs |
| The load fails with `<engine> was chosen and cannot run: ...; no later engine could run it (...)` | The chosen engine and every engine after it in the order failed | Read the reasons in the list. Choose `auto` or an engine that can run |
| After changing the engine or model the picture did not change, and `Warning` ends `the previous setting keeps running` | The new setting could not load. The previous one keeps running by design | Read the reason before the semicolon and fix it, or set the setting back. Change it again or apply it again to try it once more |
| The NVIDIA GPU wakes up when `openvino-gpu` is chosen | The OpenCL loader opened NVIDIA's driver: the module's limit was not applied (an `OCL_ICD_*` variable was already set, or the loader was in the process before) | Remove your `OCL_ICD_*` setting from the daemon's environment, or make it list the Intel driver only. The log says why the limit was left alone |
| The picture is black for many seconds the first time the effect is on (with privacy on; with privacy off the camera is shown until the engine is ready) | The TensorRT engine is being built | Wait for `the TensorRT engine for <model> is ready` in the log (section 6) |
| On a machine without an NVIDIA GPU the effect does not run (the picture is black with privacy on) and `Warning` says the Maxine module cannot run | Only the Maxine module provides `background`: the ONNX module, its models or its runtime are missing, so its stage is `unavailable` | Install the module, the models and `onnxruntime-cpu` (sections 2 and 3). `omarchy-broadcast modules` shows the reason |
| The log says `video memory: system` on an NVIDIA machine | `[video] memory` is `system`, or the probe found no usable CUDA (`auto`: for example ``the CUDA toolkit (NVRTC) is not installed; install `cuda` for the GPU path``), or a start fell back after CUDA failed, or the battery saver is active (`auto: battery saver, on battery` or `... power profile power-saver`) | The log line gives the reason. Set `memory = "cuda"` to require CUDA, or `battery_saver = "off"` to keep CUDA on the battery, and reload |
| `OpenVINO GPU: ... Device GPU is not available`, and the model runs on another engine | There is no Intel GPU in the running system. On a laptop with a GPU switch (a MUX), the discrete-only mode leaves the integrated Intel GPU off the bus (`lspci` shows no Intel display device, and `/dev/dri` holds only the NVIDIA GPU). The module limits OpenCL to Intel's driver (section 7), so the NVIDIA GPU is not offered to OpenVINO in its place | Use another engine, or switch the laptop to its hybrid mode and reboot if you want the Intel GPU. With only an NVIDIA GPU, `auto` is correct to use TensorRT, CUDA or the CPU |
| `OpenVINO NPU: ... Device NPU is not available`, with the NPU's driver installed | The daemon cannot open `/dev/accel/accel0`: the node belongs to the group `render` and your user is not in it (check with `ls -l /dev/accel/accel0` and `id`). The NPU's user-space packages are not enough | Add your user to the group with `sudo usermod -aG render $USER`, log in again and restart the daemon. Or give the node the access you want with a udev rule of your own, then `sudo udevadm trigger -s accel` |
| After a first install the module does not appear | The daemon reads manifests at start and on reload | `systemctl --user restart omarchy-broadcastd` |

## 9. Uninstall

Remove the packages:

```bash
sudo pacman -R omarchy-broadcast-onnx-git
sudo pacman -R omarchy-onnx-model-<id>      # for each model package you installed
```

- For the prebuilt module, remove `omarchy-broadcast-onnx-bin` instead.
- The module package leaves the default model's package installed, as a dependency that nothing else needs. To remove it with the module:

  ```bash
  sudo pacman -Rs omarchy-broadcast-onnx-git
  # or, for the prebuilt module:
  sudo pacman -Rs omarchy-broadcast-onnx-bin
  ```

- For a checkout, this removes what the script installed, the module's library, manifest and models included:

  ```bash
  scripts/install-user.sh --uninstall
  ```
- The TensorRT engine cache is under `$XDG_CACHE_HOME/omarchy-broadcast/` (by default `~/.cache/omarchy-broadcast/`). You can delete it at any time. It is rebuilt on the next use.
