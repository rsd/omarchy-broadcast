# Open-model cut-out (`br.com.dias.onnx-cutout`)

An omarchy-broadcast module that gives you background blur and replacement without NVIDIA Maxine. It provides the `background` stage (the person cut out, the room behind blurred or replaced) with open ONNX models run by ONNX Runtime. It runs on the CPU, or on whatever accelerator the installed ONNX Runtime offers (TensorRT, CUDA, OpenVINO).

This README is the module's reference: build, install, every setting, key and variable, the engines and the reasons an engine is not used. For the install guide and troubleshooting, see [docs/setup/onnx.md](../../docs/setup/onnx.md).

| | |
|---|---|
| Stage | `background`, slot `matte`, rank 200 (below the Maxine module's 300), device `cpu` |
| Frames | NV12, CUDA or host memory, any even size |
| Models | MediaPipe selfie segmentation (default), PP-HumanSeg v2 mobile and lite, MODNet; see [model-sources/LICENSES.md](model-sources/LICENSES.md) |
| Runtime | `libonnxruntime.so` 1.29 or later, opened at run time |
| Licence | MIT OR Apache-2.0 (the module); the models Apache-2.0 |

With both modules installed, the `quality` profile picks Maxine (the higher rank) and the `battery` profile picks this one (a CPU device). This command picks it whatever the profile:

```sh
omarchy-broadcast modules pin background br.com.dias.onnx-cutout
```

## Building

The crate is a workspace of its own, with its own `Cargo.lock`, so that ONNX Runtime's bindings stay out of the repository's lock file:

```sh
cargo build --release --locked --manifest-path crates/obc-module-onnx/Cargo.toml
```

The library is `crates/obc-module-onnx/target/release/libobc_module_onnx.so`. Nothing links against ONNX Runtime or CUDA, so the module builds anywhere.

## Installing

On Arch, install the package `omarchy-broadcast-onnx-git` (or the prebuilt `omarchy-broadcast-onnx-bin`) instead of the steps below. The steps below install from a build.

1. **Install ONNX Runtime**, 1.29 or later: any package that provides `onnxruntime`. [omarchy-onnxruntime](https://github.com/rsd/omarchy-onnxruntime) provides ONNX Runtime 1.29.0 as pacman packages for Arch and Omarchy, and says which package fits which hardware: see [step 1 of its INSTALL.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/INSTALL.md#1-choose-your-packages).
   - TensorRT also needs its libraries on the daemon's library path. A package that installs them under `/usr/lib` or registers their directory with the loader needs nothing more; otherwise set `LD_LIBRARY_PATH` for the daemon (see "TensorRT's library path" below).
2. **Install the module**: the library and the manifest in a `modules.d` directory, with `library` pointing at the library:

   ```sh
   dir=~/.local/share/omarchy-broadcast/modules.d
   mkdir -p "$dir"
   cp crates/obc-module-onnx/onnx.toml crates/obc-module-onnx/target/release/libobc_module_onnx.so "$dir/"
   sed -i 's|^library = .*|library = "libobc_module_onnx.so"|' "$dir/onnx.toml"
   ```

3. **Install the models**<a id="models"></a>. On Arch, install the per-model packages `omarchy-onnx-model-<id>` (the default is `omarchy-onnx-model-mediapipe-landscape`) from <https://github.com/rsd/omarchy-onnx-models>. They install the files in `/usr/share/omarchy-onnx-models/`. Otherwise, put the files, under the file names of the catalogue, in a model directory. The first directory that has the file is used:

   1. the module configuration's `model_dir`;
   2. `$OBC_ONNX_MODEL_DIR`;
   3. `$XDG_DATA_HOME/omarchy-broadcast/models/onnx/` (by default `~/.local/share/omarchy-broadcast/models/onnx/`);
   4. `/usr/share/omarchy-onnx-models/` (the model packages);
   5. `/usr/share/omarchy-broadcast/models/onnx/`.

   | Model setting | File | SHA-256 |
   |---|---|---|
   | `mediapipe-landscape` | `selfie_segmentation_landscape.onnx` | `fa02222275c455638fa1190493b659d25ad31e86f1b6459f17bd501f02cbe43d` |
   | `pphumanseg-v2-mobile` | `pp_humansegv2_mobile_192x192.onnx` | `2bca11ec1f2a662146147d97523d613f5ef4ebf2a89803a95a0cbabfe2bd70a9` |
   | `pphumanseg-v2-lite` | `pp_humansegv2_lite_192x192.onnx` | `ca96b4436f1ce6a76059e8902e20a9617f6829fb11f6c6c9e876563e6352e6b4` |
   | `modnet` | `modnet_photographic.onnx` | `07c308cf0fc7e6e8b2065a12ed7fc07e1de8febb7dc7839d7b7f15dd66584df9` |

   The module checks each file against its SHA-256 when it loads. A missing or different file fails the load, with a reason that names the file, the package to install and the directories searched. Install [model-sources/LICENSE-APACHE-2.0](model-sources/LICENSE-APACHE-2.0) and [model-sources/LICENSES.md](model-sources/LICENSES.md) with the files. The omarchy-onnx-models repository publishes the files, their conversion scripts and their checksums; see [model-sources/README.md](model-sources/README.md).

4. **Restart the daemon and check the module**:

   ```sh
   systemctl --user restart omarchy-broadcastd
   omarchy-broadcast modules
   ```

   The module is listed with its stage, or with the reason it cannot run.

## Settings

The group "Open-model cut-out" of the effect settings, greyed while the background effect is off. Both settings are parameters of the `background` stage that load the chain again. A change is a switch: the previous chain runs until the new one is in force (with a GPU engine, a first use of TensorRT takes a while; see below).

A switch that cannot load is refused. Causes are a model or image that cannot be read, or a stage that fails to load. An engine that cannot run is not one of these: it falls back (see "Engines"). When a switch is refused, the previous chain keeps running, the daemon's `Warning` says why and the stored setting stays as chosen ([docs/setup/onnx.md](../../docs/setup/onnx.md)).

| Setting | Values | Default |
|---|---|---|
| `effect.onnx.model` | `mediapipe-landscape` ("MediaPipe (fast)"), `pphumanseg-v2-mobile` ("PP-HumanSeg mobile (quality)"), `pphumanseg-v2-lite` ("PP-HumanSeg lite"), `modnet` ("MODNet (sharp hair, GPU)") | `mediapipe-landscape` |
| `effect.onnx.engine` | `auto` ("Automatic"), `tensorrt` ("NVIDIA GPU, TensorRT (fastest)"), `cuda` ("NVIDIA GPU, CUDA"), `openvino-npu` ("Intel NPU (battery saver)"), `openvino-gpu` ("Intel GPU (OpenVINO)"), `openvino-cpu` ("CPU, OpenVINO"), `cpu` ("CPU") | `auto` |

The text in brackets is the label the settings show. The value is what is stored. The engine names in log lines and reasons are not the labels (`NVIDIA TensorRT`, `OpenVINO NPU`, ...).

The blur strength and the replacement image are the daemon's background settings (`effect.background.*`).

## Configuration

`[module."br.com.dias.onnx-cutout"]` in `config.toml`, every key optional:

| Key | Meaning |
|---|---|
| `ort_library` | The ONNX Runtime library, a path. `$ORT_DYLIB_PATH` wins over it |
| `model_dir` | The first model directory searched |
| `threads` | The CPU threads of a model session, 1 to 64 (default 4) |

Environment:

| Variable | Meaning |
|---|---|
| `ORT_DYLIB_PATH` | The ONNX Runtime library. Without it and without `ort_library`, `libonnxruntime.so.1` then `libonnxruntime.so` from the system's library path |
| `OBC_ONNX_MODEL_DIR` | A model directory, searched after `model_dir` |
| `OCL_ICD_VENDORS`, `OCL_ICD_FILENAMES`, ... (`OCL_ICD_*`) | Read by the OpenCL loader. When none is set, the module sets `OCL_ICD_VENDORS` itself when it is opened, whether or not `openvino-gpu` is used (see "OpenCL and the NVIDIA GPU"). When any is set, it leaves them alone |
| `XDG_DATA_HOME`, `XDG_CACHE_HOME` | The user's model directory and the engine caches (`$XDG_CACHE_HOME/omarchy-broadcast/trt/`, `.../openvino/`) |

ONNX Runtime allows one environment per process, and the first library the daemon opens stays until it restarts. The environment lives for the whole process and is never released. With the omarchy-onnxruntime build of 1.29.0 and OpenVINO 2026.4.1, releasing it after OpenVINO was used crashes the process at exit. The module keeps it in a `static` (`RUNTIME` in `src/runtime.rs`) that nothing drops, at unload, at instance drop or at exit. `tests/exit.rs` guards this.

## Engines

`auto` tries the engines in this order and takes the first that starts:

1. **TensorRT** (`TensorrtExecutionProvider`), fp16, with an engine cache in `$XDG_CACHE_HOME/omarchy-broadcast/trt/<model checksum prefix>-ort<ONNX Runtime version>-trt<TensorRT version>-<GPU name>-sm<compute capability>[-<input size>]/`. The directory's name covers:
   - the model file and the runtime;
   - TensorRT (its version read from `getInferLibVersion` of the `libnvinfer` the loader finds; `trtunknown` when it cannot be read);
   - the GPU (its name and compute capability, from the CUDA driver);
   - the input size of a model of dynamic size.

   So an engine built on another GPU or by another TensorRT is never taken for a cached one. CUDA runs what TensorRT cannot.
2. **CUDA** (`CUDAExecutionProvider`).
3. **OpenVINO** on the NPU, then on the GPU, then on the CPU (`OpenVINOExecutionProvider`).
4. **The CPU** (ONNX Runtime's own provider).

The module asks the loaded runtime which providers it was built with. It checks that each provider's library (`libonnxruntime_providers_<name>.so`) is beside `libonnxruntime.so`, then tries to start the model on each engine that passes. An engine that is ruled out is not tried: `auto` skips it with the reason, and an engine you chose falls back (below). A build of ONNX Runtime with more providers lights the other engines up with no change to the module.

**A chosen engine that cannot run.** An engine chosen explicitly (not `auto`) that cannot run does not fail the load. It may be ruled out, CUDA may be off because the video pipeline runs in system memory, or it may fail to start. The module tries the engines **after** it in the order above and runs the first that starts. A choice of an OpenVINO or CPU engine therefore never falls back to TensorRT or CUDA and never wakes the NVIDIA GPU.

- The stored setting is kept.
- The fallback is not silent. The load reports a notice (module ABI 1.5, `LoadReport::notice`) that the daemon shows as a clause of `Warning`: `<module name>: <Requested> was chosen and cannot run: <reason>; using <engine>`. The clause ends when a load runs the chosen engine, at stop, with new caps and when the effect is turned off.
- Only when no later engine starts does the load fail, with `<Chosen> was chosen and cannot run: <reason>; no later engine could run it (<engine>: <reason>; ...)` (without the list when no engine follows). The daemon's privacy setting then applies.
- A live switch to such an engine succeeds on the fallback. On CUDA frames it runs under the element's lock, so the feed holds its last frame while the fallback engines are tried.

### TensorRT's library path

TensorRT 11 opens its per-GPU builder library by name (`libnvinfer_builder_resource_sm<NN>.so.<version>`; TensorRT 10, `libnvinfer_builder_resource.so.<version>`), which only the process's library path resolves. Without TensorRT's builder library on the daemon's library path (`LD_LIBRARY_PATH`, the loader's cache or its default directories), ONNX Runtime's TensorRT provider crashes the process, on an engine build and on loading a cached engine.

The module opens that library itself before it starts TensorRT. When it cannot, TensorRT is refused with a reason, `auto` goes on to CUDA and an explicit `tensorrt` fails the load.

The current `nvidia-tensorrt-sdk-local` (pkgrel 4 or later) needs nothing: it ships a loader entry for `/opt/nvidia/tensorrt/lib` and links the builder libraries into `/usr/lib`. For an older release (`/opt/nvidia/tensorrt`), set the path for the daemon's unit with a systemd user drop-in, `~/.config/systemd/user/omarchy-broadcastd.service.d/onnx-tensorrt.conf`:

```ini
[Service]
EnvironmentFile=-/usr/share/nvidia-tensorrt/nvidia-tensorrt.env
```

Then reload systemd and restart the daemon:

```sh
systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd
```

A TensorRT under `/usr/lib` (the AUR's `tensorrt`) needs nothing either.

The daemon never applies the Maxine SDK environment's own `LD_LIBRARY_PATH`, because its bundled CUDA and TensorRT libraries would shadow these engines. So the value of this drop-in is the one the daemon runs with. A `maxine.conf` drop-in of an earlier version also sets it; you no longer need it, so remove it. The full procedure is in [docs/setup/onnx.md](../../docs/setup/onnx.md).

### The first use of TensorRT

The first use builds an engine for the model and the GPU, from about 6 to 45 seconds. The log says `preparing the TensorRT engine for <model> (first use: this takes a while)…`, then `the TensorRT engine for <model> is ready`, and the load's time (`load_ms`) includes it. Later loads read the cache in under a second.

A load runs on the effect element's worker thread when the effect is turned on or the camera starts. The picture is then black (at a start) or the camera as it was (turning the effect on mid-stream) until the engine is ready.

A **change of a setting** while the effect runs is a switch, which the streaming thread waits for:

- Under `auto`, a switch to a model whose TensorRT engine is not in the cache does not build it. It runs on CUDA for now, and the log says `TensorRT engine not built yet; using CUDA for now. It builds at the next camera start.`
- An explicit `tensorrt` never falls back. Its switch builds the engine, and the log says the feed pauses until it is ready.

### OpenCL and the NVIDIA GPU

OpenVINO's GPU engine initialises OpenCL. The OpenCL loader opens every driver in `/etc/OpenCL/vendors/`, NVIDIA's included. That wakes a suspended NVIDIA GPU, keeps `/dev/nvidia*` open until the process exits, and makes the first session take about 2.7 s instead of about 0.1 s.

The module therefore limits the loader when it is opened. It does this in its `init`, on the daemon's controller thread, before ONNX Runtime or OpenVINO has created a thread, so the process environment is not read while it is written.

- It makes `$XDG_RUNTIME_DIR/omarchy-broadcast/opencl-vendors/` (mode 0700), links in it the `.icd` files that name Intel's compute runtime (`libigdrcl`), and sets `OCL_ICD_VENDORS` to that directory with a trailing slash. The loader that ships with CUDA requires the slash, because it joins directory and file name without a separator. `OCL_ICD_FILENAMES` was not used because both loaders add its drivers instead of replacing them.
- It does nothing when any `OCL_ICD_*` variable is already set (the user's choice) or no Intel GPU driver is installed.
- The NPU and CPU engines load no OpenCL.
- The variable is set once, in every process that opens the module (the daemon and the command-line probes), whether or not `openvino-gpu` is used. The processes that process starts afterwards inherit it.

Code: `src/opencl.rs`. Test: `tests/intel_gpu.rs`.

### The GPU is left as it was found

ONNX Runtime's CUDA and TensorRT engines run through the CUDA runtime, which activates the device's primary CUDA context. When the last session that registered (or tried) one of them goes, at an unload or after a failed attempt of `auto`, the module resets that primary context (`cuDevicePrimaryCtxReset`), with the thread's current context put back. So no CUDA context of the module's outlives its sessions, and the GPU can be parked at idle. The frames' own context (GStreamer's) is another context and is untouched. The probe and the check make no CUDA call.

## Reading the reasons

The module logs one line per engine, in the daemon's log (`journalctl --user -u omarchy-broadcastd`):

- **When it is probed** (files only): the engines whose provider library is missing beside `libonnxruntime.so`, or whose GPU is hidden (`CUDA_VISIBLE_DEVICES` empty).
- **When it is checked before the camera starts** (the runtime opened, no device touched): for each engine, `available in this ONNX Runtime build`, or why not. Whether the device runs it is found when the cut-out loads.
- **When the cut-out loads**: each engine ruled out without being tried (`ONNX cut-out: <engine> unavailable: <reason>`), each engine tried that did not start, with ONNX Runtime's error, then the one it runs on: `ONNX cut-out: <model> runs on <engine> (ONNX Runtime <version>; …)`.

| Reason | Means |
|---|---|
| `not in this ONNX Runtime build (no libonnxruntime_providers_<x>.so beside …)` | The runtime has no such provider, or its library is not beside `libonnxruntime.so`. The engine is not tried. Install the provider package the reason names, or a build that has it |
| `CUDA not available: …` | The CUDA provider is there, the GPU or its libraries are not (the CUDA error follows: `no CUDA-capable device is detected`, a missing `libcudnn.so.9`) |
| `TensorRT not available: TensorRT's libraries are not on the daemon's library path (… not found; add TensorRT's lib directory to LD_LIBRARY_PATH)` | TensorRT's builder library cannot be found by name. TensorRT is not started, `auto` moves on to CUDA, a chosen `tensorrt` falls back with a notice |
| `TensorRT not available: TensorRT <ver> has no builder library for this GPU (sm<NN>)` | The installed TensorRT ships no builder library for this GPU |
| `TensorRT not available: …` (other) | Another TensorRT library cannot be loaded, or no NVIDIA GPU was found through the CUDA driver |
| `OpenVINO NPU: …`, `OpenVINO GPU: …` | OpenVINO could not use the device (an NPU needs the user-space driver) |
| `the provider was registered but ran no part of the model` | The provider started and ran nothing (OpenVINO does this when its device is missing) |
| `partly on the CPU (k of n kernels)` | The engine runs, but ONNX Runtime left some of the model to the CPU |

**Partly on the CPU** is measured, not guessed. The first run of a session is profiled, and the profile tells which provider ran each kernel. Profiling stops after that run. The profile is written in a new directory of the process's own (mode 0700) under `$XDG_RUNTIME_DIR` (else the temporary directory) and removed at once.

If no runtime is found, the stage is listed as unavailable with the reason `ONNX Runtime not installed (libonnxruntime.so not found)`. With no model file, the reason is `No cut-out model installed (searched: …)`.

## What it does with a frame

The module averages the frame down to the model's input (RGB, normalised per model) and runs the model. It reads the output as the person's matte and smooths it lightly over time (forgotten on a new camera).

The composite scales the matte up bilinearly and puts the person over the room:

- **Blurred room.** A quarter-size average weighted by how much of each pixel is room, so the person does not bleed into it. It is blurred and scaled back up. The strength comes from `effect.background.blur_strength`.
- **Replacement.** The replacement image, scaled to cover the frame.

A CUDA frame is copied to page-locked host memory on the frame's stream, processed and copied back. The steps are reported as `convert-in`, `mask`, `composite` and `convert-out`.

**A matte never fails open.** Any failure of a frame fails the whole chain. The effect then follows `[effect] privacy`: with privacy on, black, the away image or the last frame by `privacy_mode`; with privacy off (the default), the camera is shown. A failed load leaves what was running as it was.

## Tests

```sh
cargo test --manifest-path crates/obc-module-onnx/Cargo.toml
```

The unit tests, and the module's manifest, settings and probe through `obc-module-host`, run anywhere. `tests/runtime.rs` runs the module with a real ONNX Runtime and the models. It is skipped (with the reason printed) unless both are given:

```sh
ORT_DYLIB_PATH=/path/to/libonnxruntime.so.1.31.1 OBC_ONNX_MODEL_DIR=/path/to/models \
  cargo test --manifest-path crates/obc-module-onnx/Cargo.toml --test runtime -- --nocapture
```

`OBC_ONNX_MODEL_DIR` may hold the catalogue's file names or the evaluation's (`ours_*.onnx`). With a CUDA build of ONNX Runtime, the test checks that `auto` picks TensorRT or CUDA. With `CUDA_VISIBLE_DEVICES=""` it checks that both are reported unavailable with a reason, that `auto` falls to the CPU and the blur still works, and that an explicit `cuda` fails the load.

- `tests/hardware.rs` (ignored) runs CUDA frames on a real GPU: `-- --ignored`.
- `tests/exit.rs` runs itself again in a child process that runs a frame on each OpenVINO engine that loads, unloads and drops everything. It checks that the child exits with status 0 and not by a signal. It needs `ORT_DYLIB_PATH`, `OBC_ONNX_MODEL_DIR` and a runtime with OpenVINO, and is skipped with the reason printed otherwise.
