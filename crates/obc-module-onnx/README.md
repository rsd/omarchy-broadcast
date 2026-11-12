# Open-model cut-out (`br.com.dias.onnx-cutout`)

An omarchy-broadcast module that provides the `background` stage (the person cut out, the room behind blurred or replaced) with open ONNX models run by ONNX Runtime, so the background effect works without NVIDIA Maxine. It runs on the CPU, or on whatever accelerator the installed ONNX Runtime offers (TensorRT, CUDA, OpenVINO).

| | |
|---|---|
| Stage | `background`, slot `matte`, rank 200 (below the Maxine module's 300), device `cpu` |
| Frames | NV12, CUDA or host memory, any even size |
| Models | MediaPipe selfie segmentation (default), PP-HumanSeg v2 mobile and lite, MODNet; see [model-sources/LICENSES.md](model-sources/LICENSES.md) |
| Runtime | `libonnxruntime.so` 1.29 or later, opened at run time |
| Licence | MIT OR Apache-2.0 (the module); the models Apache-2.0 |

With both modules installed, the `quality` profile picks Maxine (the higher rank) and the `battery` profile picks this one (a CPU device). `omarchy-broadcast modules pin background br.com.dias.onnx-cutout` picks it whatever the profile.

## Building

The crate is a workspace of its own, with its own `Cargo.lock`, so that ONNX Runtime's bindings stay out of the repository's lock file:

```sh
cargo build --release --locked --manifest-path crates/obc-module-onnx/Cargo.toml
```

The library is `crates/obc-module-onnx/target/release/libobc_module_onnx.so`. Nothing links against ONNX Runtime or CUDA: the module builds anywhere.

## Installing

1. **ONNX Runtime**, 1.29 or later: on Arch, `onnxruntime-cpu`, or `onnxruntime-opt-cuda` for NVIDIA's CUDA engine (any package that provides `onnxruntime`).
2. **The module**: the library and the manifest in a `modules.d` directory, with `library` pointing at the library:

   ```sh
   dir=~/.local/share/omarchy-broadcast/modules.d
   mkdir -p "$dir"
   cp crates/obc-module-onnx/onnx.toml crates/obc-module-onnx/target/release/libobc_module_onnx.so "$dir/"
   sed -i 's|^library = .*|library = "libobc_module_onnx.so"|' "$dir/onnx.toml"
   ```

3. **The models**<a id="models"></a>, under the file names of the catalogue, in a model directory (the first that has the file is used):

   1. the module configuration's `model_dir`;
   2. `$OBC_ONNX_MODEL_DIR`;
   3. `$XDG_DATA_HOME/omarchy-broadcast/models/onnx/` (by default `~/.local/share/omarchy-broadcast/models/onnx/`);
   4. `/usr/share/omarchy-broadcast/models/onnx/`.

   | Model setting | File | SHA-256 |
   |---|---|---|
   | `mediapipe-landscape` | `selfie_segmentation_landscape.onnx` | `fa02222275c455638fa1190493b659d25ad31e86f1b6459f17bd501f02cbe43d` |
   | `pphumanseg-v2-mobile` | `pp_humansegv2_mobile_192x192.onnx` | `2bca11ec1f2a662146147d97523d613f5ef4ebf2a89803a95a0cbabfe2bd70a9` |
   | `pphumanseg-v2-lite` | `pp_humansegv2_lite_192x192.onnx` | `ca96b4436f1ce6a76059e8902e20a9617f6829fb11f6c6c9e876563e6352e6b4` |
   | `modnet` | `modnet_photographic.onnx` | `07c308cf0fc7e6e8b2065a12ed7fc07e1de8febb7dc7839d7b7f15dd66584df9` |

   Each file is checked against its SHA-256 when it loads; a missing or different file fails the load, with a reason that names the file and the directories searched. Install [model-sources/LICENSE-APACHE-2.0](model-sources/LICENSE-APACHE-2.0) and [model-sources/LICENSES.md](model-sources/LICENSES.md) with the files. How the files are made is in [model-sources/README.md](model-sources/README.md).

4. `systemctl --user restart omarchy-broadcastd`, then `omarchy-broadcast modules`: the module is listed with its stage, or with the reason it cannot run.

## Settings

The group "Open-model cut-out" of the effect settings, greyed while the background effect is off. Both settings are parameters of the `background` stage that load the chain again: a switch, the previous chain running until the new one is in force (with a GPU engine, a first use of TensorRT takes a while; see below).

| Setting | Values | Default |
|---|---|---|
| `effect.onnx.model` | `mediapipe-landscape` (MediaPipe, fast), `pphumanseg-v2-mobile` (PP-HumanSeg mobile, quality), `pphumanseg-v2-lite` (PP-HumanSeg lite), `modnet` (MODNet, sharp hair, for a GPU) | `mediapipe-landscape` |
| `effect.onnx.engine` | `auto`, `tensorrt`, `cuda`, `openvino-npu`, `openvino-gpu`, `openvino-cpu`, `cpu` | `auto` |

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
| `XDG_DATA_HOME`, `XDG_CACHE_HOME` | The user's model directory and the engine caches (`$XDG_CACHE_HOME/omarchy-broadcast/trt/`, `.../openvino/`) |

ONNX Runtime allows one environment per process: the first library the daemon opens stays until it restarts.

## Engines

`auto` tries the engines in this order and takes the first that starts:

1. **TensorRT** (`TensorrtExecutionProvider`), fp16, with an engine cache in `$XDG_CACHE_HOME/omarchy-broadcast/trt/<model checksum prefix>-ort<ONNX Runtime version>-trt<TensorRT version>-<GPU name>-sm<compute capability>[-<input size>]/`. The directory's name covers the model file, the runtime, TensorRT (its version read from `getInferLibVersion` of the `libnvinfer` the loader finds; `trtunknown` when it cannot be read) and the GPU (its name and compute capability, from the CUDA driver), and the input size of a model of dynamic size, so an engine built on another GPU or by another TensorRT is never taken for a cached one. CUDA runs what TensorRT cannot.
2. **CUDA** (`CUDAExecutionProvider`).
3. **OpenVINO** on the NPU, then on the GPU, then on the CPU (`OpenVINOExecutionProvider`).
4. **The CPU** (ONNX Runtime's own provider).

The module asks the loaded runtime which providers it was built with, then tries to start the model on each. A build of ONNX Runtime with more providers lights the other engines up with no change to the module. An engine chosen explicitly (not `auto`) that cannot run fails the load with its reason: there is no silent fallback.

**The first use of TensorRT** builds an engine for the model and the GPU: from about 6 to 45 seconds. The log says `preparing the TensorRT engine for <model> (first use: this takes a while)…`, then `the TensorRT engine for <model> is ready`, and the load's time (`load_ms`) includes it; later loads read the cache in under a second. A load runs on the effect element's worker thread when the effect is turned on or the camera starts: the picture is then black (at a start) or the camera as it was (turning the effect on mid-stream) until it is ready. A **change of a setting** while the effect runs is a switch, which the streaming thread waits for. So under `auto`, a switch to a model whose TensorRT engine is not in the cache does not build it: it runs on CUDA for now, and the log says `TensorRT engine not built yet; using CUDA for now. It builds at the next camera start.` An explicit `tensorrt` never falls back: its switch builds the engine, and the log says the feed pauses until it is ready.

**The GPU is left as it was found.** ONNX Runtime's CUDA and TensorRT engines run through the CUDA runtime, which activates the device's primary CUDA context. When the last session that registered (or tried) one of them goes, at an unload or after a failed attempt of `auto`, the module resets that primary context (`cuDevicePrimaryCtxReset`), with the thread's current context put back, so no CUDA context of the module's outlives its sessions and the GPU can be parked at idle. The frames' own context (GStreamer's) is another context and is untouched. The probe and the check make no CUDA call.

### Reading the reasons

The module logs one line per engine, in the daemon's log (`journalctl --user -u omarchy-broadcastd`):

- when it is probed (files only): the engines whose provider library is missing beside `libonnxruntime.so`, or whose GPU is hidden (`CUDA_VISIBLE_DEVICES` empty);
- when it is checked before the camera starts (the runtime opened, no device touched): for each engine, `available in this ONNX Runtime build`, or why not (whether the device runs it is found when the cut-out loads);
- when the cut-out loads: each engine tried that did not start, with ONNX Runtime's error, then the one it runs on: `ONNX cut-out: <model> runs on <engine> (ONNX Runtime <version>; …)`.

| Reason | Means |
|---|---|
| `not in this ONNX Runtime build` | The runtime has no such provider: install a build that has it |
| `CUDA not available: …` | The CUDA provider is there, the GPU or its libraries are not (the CUDA error follows: `no CUDA-capable device is detected`, a missing `libcudnn.so.9`) |
| `TensorRT not available: …` | Likewise for TensorRT (its provider needs TensorRT 10 libraries) |
| `OpenVINO NPU: …`, `OpenVINO GPU: …` | OpenVINO could not use the device (an NPU needs the user-space driver) |
| `the provider was registered but ran no part of the model` | The provider started and ran nothing (OpenVINO does this when its device is missing) |
| `partly on the CPU (k of n kernels)` | The engine runs, but ONNX Runtime left some of the model to the CPU |

**Partly on the CPU** is measured, not guessed: the first run of a session is profiled, and the profile tells which provider ran each kernel; profiling stops after that run. The profile is written in a new directory of the process's own (mode 0700) under `$XDG_RUNTIME_DIR` (else the temporary directory) and removed at once.

If no runtime is found, the stage is listed as unavailable with the reason `ONNX Runtime not installed (libonnxruntime.so not found)`; with no model file, `No cut-out model installed (searched: …)`.

## What it does with a frame

The frame is averaged down to the model's input (RGB, normalised per model), the model runs, and its output is read as the person's matte, smoothed lightly over time (forgotten on a new camera). The composite scales the matte up bilinearly and puts the person over the room, blurred (a quarter-size average weighted by how much of each pixel is room, so the person does not bleed into it, blurred and scaled back up; the strength from `effect.background.blur_strength`), or over the replacement image scaled to cover the frame. A CUDA frame is copied to page-locked host memory on the frame's stream, processed and copied back. The steps are reported as `convert-in`, `mask`, `composite` and `convert-out`.

**A matte never fails open**: any failure of a frame fails the whole chain (the effect then follows `[effect] on_failure`, black by default), and a failed load leaves what was running as it was.

## Tests

```sh
cargo test --manifest-path crates/obc-module-onnx/Cargo.toml
```

The unit tests, and the module's manifest, settings and probe through `obc-module-host`, run anywhere. `tests/runtime.rs` runs the module with a real ONNX Runtime and the models, and is skipped (with the reason printed) unless both are given:

```sh
ORT_DYLIB_PATH=/path/to/libonnxruntime.so.1.31.1 OBC_ONNX_MODEL_DIR=/path/to/models \
  cargo test --manifest-path crates/obc-module-onnx/Cargo.toml --test runtime -- --nocapture
```

`OBC_ONNX_MODEL_DIR` may hold the catalogue's file names or the evaluation's (`ours_*.onnx`). With a CUDA build of ONNX Runtime it checks that `auto` picks TensorRT or CUDA; with `CUDA_VISIBLE_DEVICES=""` that both are reported unavailable with a reason, that `auto` falls to the CPU and the blur still works, and that an explicit `cuda` fails the load. `tests/hardware.rs` (ignored) runs CUDA frames on a real GPU: `-- --ignored`.
