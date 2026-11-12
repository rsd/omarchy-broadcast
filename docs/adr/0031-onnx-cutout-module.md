# ADR-0031: The open-model cut-out: a second `background` provider on ONNX Runtime

- **Status:** Accepted (2026-11-12), implemented in module version 0.36.0 (`crates/obc-module-onnx`). Builds on [ADR-0025](0025-loadable-modules.md) (a second provider of a stage, by rank, pin and profile), [ADR-0012](0012-video-effects-and-maxine-backend.md) (the cut-out never fails open). Amends the CUDA context rule of [modules.md](../modules.md#threading-and-cuda-rules) with one bounded exception, and leaves [ADR-0017](0017-cuda-checkpoint-parking.md) (parking) in force.

## Context
- Until now the only provider of the `background` stage was the Maxine module, which needs NVIDIA's SDK, a personal download that the project cannot distribute ([ADR-0003](0003-maxine-first-behind-backend-seam.md), [ADR-0021](0021-nvidia-sdk-handover.md)). Without the SDK there was no cut-out at all.
- The module system already supports several providers of one stage, chosen by rank, by profile (a device preference) and by pin ([ADR-0025](0025-loadable-modules.md)). The manifest's `device` field exists so that a `battery` profile can prefer a CPU or NPU provider over a GPU one.
- Person segmentation models under open licences exist, and ONNX is the common file format they can be brought to. ONNX Runtime runs an ONNX file on the CPU, on NVIDIA GPUs (CUDA, TensorRT) and on Intel hardware (OpenVINO), through *execution providers* (engines), each present or not in a given build of the library.
- Arch Linux packages ONNX Runtime in several variants (`onnxruntime-cpu`, `-cuda`, `-opt-cuda`, `-rocm`, `-opt-rocm`) that each provide `onnxruntime` and conflict with one another. The Arch builds include the CPU, CUDA and ROCm providers; TensorRT and OpenVINO need a build that includes them.
- ONNX Runtime's CUDA and TensorRT engines run through the CUDA runtime, which activates the device's primary CUDA context. That collides with the module rule that no module creates a CUDA context, and with parking, which requires that no context remains at idle.
- The daemon's effect element runs on CUDA frames only.

## Decision
**A new module, `br.com.dias.onnx-cutout` ("Open-model cut-out"), provides `background` with open Apache-2.0 models run by ONNX Runtime, which the module opens at run time. It is a separate Arch package. It ranks below Maxine and declares the device `cpu`.**

### The stage
- Stage `background`, slot `matte`, rank 200 (Maxine: 300), device `cpu`, NV12, `frames = ["cuda", "host"]`. With both modules installed the `quality` profile picks Maxine, the `battery` profile picks this module, and a pin picks either. It uses the daemon's own `effect.background.*` settings (blur strength, replacement image).
- Settings, both bound to the stage with `apply = reload` (a switch: the previous chain runs until the new one is loaded): `effect.onnx.model` and `effect.onnx.engine`, in the group "Open-model cut-out", greyed while the background effect is off.
- It loads and probes with no ONNX Runtime and no model file, and lists the stage `unavailable` with the reason. Its probe reads files only; its check opens the library and reads which providers it has; neither makes a CUDA call.
- **The matte never fails open**, as for Maxine: a failed frame fails the chain and the effect follows `on_failure`; a failed load leaves the running chain as it was.

### The models: default by provenance, a quality option, the rest opt-in
| Setting value | Model | Role |
|---|---|---|
| `mediapipe-landscape` | Google MediaPipe selfie segmentation, landscape | **Default.** The model whose weights carry an explicit Apache-2.0 statement in the publisher's own model card, from one organisation, with training data described in that card |
| `pphumanseg-v2-mobile` | Baidu PaddleSeg PP-HumanSeg v2, mobile | The quality option |
| `pphumanseg-v2-lite` | PP-HumanSeg v2, lite | A lighter variant of the same family |
| `modnet` | MODNet, photographic | Sharp hair; for a GPU |

- All four are Apache-2.0, recorded with their notices in `crates/obc-module-onnx/model-sources/LICENSES.md`, and the package ships the licence text with the files. The **training-data terms of PP-HumanSeg and MODNet are unconfirmed** (PP-HumanSeg's data set is available from its publisher on request with no public licence; MODNet's README does not name its data), and the file says so. That is why the default is MediaPipe and the others are a choice the user makes in the settings, not what a fresh install runs.
- The files are the project's own conversions to ONNX of the upstream releases, with unchanged weights, reproducible with the scripts in `model-sources/`. The MediaPipe conversion is ours and not the community's ONNX file: both give the same matte (the largest difference is 4e-5), and ours is one step from Google's release. A modified file carries the notice that Apache-2.0 requires. The MODNet file is the upstream ONNX export unmodified.
- Each file is checked against its SHA-256 when it loads, so a wrong or truncated file fails the load with a reason that names it.

### ONNX Runtime opened at run time, every build's engines probed generically
The module opens `libonnxruntime.so` (1.29 or newer, from `ORT_DYLIB_PATH`, the module's `ort_library`, or the system library path) and asks it which execution providers it was built with, then tries each in turn. It links against nothing. Whatever variant the user installs, and any build with more providers later, is used with no change to the module: TensorRT and OpenVINO light up when the library has them.

### The engine order
Under `auto`: **TensorRT, CUDA, OpenVINO on the NPU, OpenVINO on the GPU, OpenVINO on the CPU, then ONNX Runtime's own CPU provider.** The first that starts is used.
- The order goes from the engine most specific to the hardware to the most general. TensorRT comes first because it compiles the model for the GPU, with CUDA beneath it, and CUDA runs what TensorRT cannot. The NPU precedes the GPU and the CPU of OpenVINO because it is the lowest-power device on the machine that has one, which is what an unattended camera wants.
- "Starts" is checked, not assumed: the module creates the session with the provider registered and runs the model once, and profiles that run, because ONNX Runtime places a node its provider cannot run on the CPU silently, and OpenVINO falls back like this when its device is missing. An engine that ran no kernel did not start; an engine that left some kernels to the CPU is reported as "partly on the CPU (k of n kernels)".
- **Each engine that is not used has a reason**, in the daemon's log at three moments (the probe, the check and the load) and, for an explicit engine, in the error the load returns.
- **An engine chosen explicitly that cannot run fails the load and does not fall back.** A user who chose `cuda` and got a CPU run would have no way to know.
- TensorRT engines are built for the model and the GPU, fp16, and cached under `$XDG_CACHE_HOME/omarchy-broadcast/trt/` in a directory keyed by the model file's checksum, the ONNX Runtime version, the TensorRT version, and the GPU's name and compute capability, so that an engine built for something else is never taken for a cached one. The first build takes tens of seconds.

### The TensorRT build on a switch is skipped under `auto`
A load at a camera start or when the effect is turned on runs on a worker thread, with a black picture (at a start) or the camera as it was (turning the effect on mid-stream) until it is ready. A **switch**, a change of a setting while the chain runs, is different: the streaming thread waits for it, so a first TensorRT build would hold the feed for tens of seconds. Therefore, **under `auto`, a switch to a model with no cached TensorRT engine skips TensorRT**: it runs on CUDA for now, logs why, and the engine builds at the next camera start, when a pause costs nothing the user can see. An **explicit `tensorrt`** switch does build, and the log says the feed pauses: the user asked for that engine.

### The primary-context reset, and an exception to the CUDA context rule
The rule in [modules.md](../modules.md#threading-and-cuda-rules) says a video module never creates a CUDA context. ONNX Runtime's CUDA and TensorRT engines call `cudaSetDevice`, which activates the device's primary context in the process, and no setting of theirs avoids it.
- **The exception, with its bounds.** While a GPU session of the module is alive the primary context exists, which is the engine's own doing. The module counts its GPU sessions (counted before one is created, under one lock), and when the last ends it resets the device's primary context with the driver call `cuDevicePrimaryCtxReset`, putting back the thread's current context afterwards. So **no CUDA context of the module's remains at idle, and parking ([ADR-0017](0017-cuda-checkpoint-parking.md)) is unaffected after an unload.** It never touches the context of the frames (GStreamer's, made with `cuCtxCreate`, the one Maxine also runs in). If the primary context is the caller's current context, or holds the frames of a live chain, the reset is skipped with a warning. Its probe and check make no CUDA call, and it makes no CUDA call before a session is wanted. It saves and restores the caller's current context around session creation, runs and teardown.
- **Why not `cudaDeviceReset`.** It resets the *current* device, and with the host's context current (the host makes the frames' context current around every call) it affects that context's runtime state and not the primary context. Using it would need the primary context made current first, which is the module making a context current; and which `libcudart` it would reach (the system's, or TensorRT's own) is not the module's to know. `cuDevicePrimaryCtxReset` names the primary context of the device explicitly and leaves contexts made with `cuCtxCreate` alone. It needs no `cuInit` of its own: with no CUDA call made yet there is nothing to reset, and it returns early.
- Verified on hardware with the engines `auto`, `cpu`, `cuda` and `tensorrt`: after each unload the primary context is inactive, the next load works, and with two chains alive unloading one keeps the primary context for the other.

### Packaging: weights fetched with a checksum, in a package of its own
- The packaging is split into two `PKGBUILD`s. `packaging/arch/` builds `omarchy-broadcast-git`, the core, which never depends on the model files; **`packaging/arch-onnx/`** builds `omarchy-broadcast-onnx-git`, after the core, carrying the module, the manifest, the models (`/usr/share/omarchy-broadcast/models/onnx/`) and the licence files. It depends on the core of the same commit and on `onnxruntime>=1.29`.
- **A package of its own**, because the ONNX Runtime variants conflict with one another and the user chooses one (CPU, CUDA, ROCm): the core cannot depend on one, and a hard dependency would force a choice on everyone who wants no cut-out. `onnxruntime` is the name every variant provides, so any satisfies `onnxruntime>=1.29`.
- **The model files are sources of the package build**, checked by SHA-256, taken from a release asset. The download address in `packaging/arch-onnx/PKGBUILD` is a placeholder until the release host is published; until then `makepkg` reads the files from a local directory (`OBC_ONNX_MODELS_DIR`). `scripts/install-user.sh` installs them to `~/.local/share/omarchy-broadcast/models/onnx/` from a directory given in the same variable, after checking their checksums.

## Consequences
- A user without an NVIDIA SDK has a cut-out. The `battery` profile can prefer it over Maxine on a machine that has both. The cost is a second `background` provider to maintain, with its own model catalogue and its own licence notices.
- ONNX Runtime stays optional and the user's to choose. Whatever engine the user's build offers is used, and the reasons for those that are not are in the log.
- **Known limitation: the effect element is CUDA-only.** The daemon runs the effect on CUDA frames only, so a machine with no NVIDIA GPU cannot yet run the daemon's effect path, whichever provider it uses. The module is written for host frames too, and has been tested with them at the module level, so a daemon pipeline that runs the effect on system memory needs no change to it. That pipeline is future work, with no date.
- The CUDA context rule has one exception, documented with its bounds in modules.md. The next module that wants to hold a primary context has to meet the same bounds: count, reset when the last ends, restore the caller's context, never touch the frames' context.
- TensorRT's first build is a one-off wait per model, GPU and library version, and an upgrade of ONNX Runtime or TensorRT rebuilds it. Old cache directories are not removed by the module.
- The model files are about 70 MB together and are not in the git repository.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **Link ONNX Runtime, or bundle a build** | A link ties the module to one variant, and the variants conflict; a bundled build is a large binary to maintain, patch for security fixes and build with every provider's libraries. Opening the user's own library keeps the choice, and the distribution's updates, with the user |
| **TensorRT only** | It leaves everyone without an NVIDIA GPU, and everyone with a distribution build that lacks TensorRT, with nothing. Probing every provider the library has, generically, costs the same code and covers CUDA, OpenVINO and the CPU as well |
| **`cudaDeviceReset`** to release the context | See above: it does not reset the primary context while the host's is current, and would need the module to make a context current |
| **Leaving the primary context alive** | It breaks parking ([ADR-0017](0017-cuda-checkpoint-parking.md)): the process would hold `/dev/nvidia*` at idle for as long as it runs |
| **`auto` that skips CUDA and TensorRT** | It would give up the GPU engines on the machines that have them, for a problem the reset solves |
| **Building TensorRT engines on every switch** | The feed would pause for tens of seconds in the middle of a call every time the model changed |
| **No fallback for `auto` on a switch to a model without a cached engine** (wait for the build) | Same pause. Only an explicit `tensorrt` earns it |
| **A fallback for an explicit engine that fails** | The user would not know the engine they chose is not the one running |
| **The community MediaPipe ONNX file** | The same matte, but it is not reproducible from Google's release by scripts in this repository. Ours is |
| **A default of PP-HumanSeg or MODNet** | Their training-data terms are unconfirmed. They are choices, not the default |
| **Other open models**: BiRefNet (trained on data whose terms forbid commercial use), U2-Net human segmentation and SINet (weights terms not stated), RMBG-2.0 (non-commercial), RVM (GPL-3.0) | Their terms do not fit a package that ships Apache-2.0 weights with unchanged provenance. None is shipped or downloaded |
| **The model files in git** | About 70 MB of binary files, in a repository whose history is permanent and whose clones everyone pays for. A release asset with a checksum in the `PKGBUILD` gives the same integrity |
| **Download the models on first use** | Kept as an option for a model whose licence is unclear (the user accepts it, the project does not distribute it). Not used for these four: a package that works offline, with its files verified at build time, is better for the models that are clear |
| **The module in the core package** | The core would depend on an ONNX Runtime variant (see above), or on none and fail to load the module |
| **A CPU-only effect path in the daemon, as part of this change** | A larger change to the effect element and the output, separate from adding the module. Deferred, and recorded as the limitation above |

## Future work
- **A daemon pipeline that runs the effect on host memory**, so that the module runs on a machine with no NVIDIA GPU. No date.
- **A published release host** for the model files, replacing the placeholder in `packaging/arch-onnx/PKGBUILD`.
- **A download on first use** for any model of unclear licence added to the catalogue.
- Confirmation of the training-data terms of PP-HumanSeg and MODNet.

## What would change this decision
- Training-data terms for PP-HumanSeg or MODNet that rule them out, or that clear them: they would leave the catalogue or become candidates for the default.
- An ONNX Runtime that no longer needs the primary context, or a CUDA runtime that releases it with the last session: the reset would become unnecessary and the exception would go.
- An ONNX Runtime package on Arch that includes every provider: the engines of TensorRT and OpenVINO would light up with no change to the module, and the install instructions would shrink; the package split would stay.
