# Writing and installing modules

The effects of omarchy-broadcast are provided by **modules**: a TOML manifest and a shared library that exports a versioned C ABI. The daemon finds the manifests, loads the libraries of the modules that are enabled, and shows the settings each module describes in its clients, with no change to them. This page is the reference for module authors and for people who install modules. Modules keep vendor code (the Maxine SDK, which the project cannot distribute) out of the daemon, and let a third party add an effect or a backend without changing it. The design is in [architecture.md](architecture.md#modules), and for the audio kind (the microphone) in [The microphone](architecture.md#the-microphone-host-and-worker); the D-Bus members are in [dbus-api.md](dbus-api.md#modules) and the CLI in [cli-json.md](cli-json.md#modules-and-order).

The module interface is **ABI 1.x (1.6 at present), and ABI 1 is stable from 1.6**: a module library built for ABI 1.N loads and behaves as documented on every host of ABI 1.M with M at least N, so a module built outside this repository does not need rebuilding after an update of omarchy-broadcast. What is covered, what is not, how a minor version may change the interface and what a break would be are in [Stability promise](#stability-promise-and-versioning). The version rules the host enforces are in [The ABI](#the-abi-and-the-sdk).

Where to look:

- the C header, [`crates/obc-module-abi/include/omarchy_broadcast_module.h`](../crates/obc-module-abi/include/omarchy_broadcast_module.h) (installed by the Arch package as `/usr/include/omarchy-broadcast/omarchy_broadcast_module.h`): the ABI, with a comment on every structure, function and constant;
- the Rust SDK, `crates/obc-module-sdk`: a safe API over the ABI;
- the example, [`examples/module-hello`](../examples/module-hello/README.md): a complete module, built on its own outside the workspace as a third-party module would be;
- the C example, [`examples/module-hello-c`](../examples/module-hello-c/README.md): a complete module written in C against the header alone, with no Rust and no other library (see [Writing a module in C](#writing-a-module-in-c));
- the audio example, [`examples/module-hello-audio`](../examples/module-hello-audio/README.md): a complete audio module (a gain and an optional test tone for the microphone), built the same way (see [Audio modules](#audio-modules));
- the Maxine module, `crates/obc-module-maxine`: a module with several stages, a probe of the filesystem, settings and CUDA frames;
- the Maxine AR module, `crates/obc-module-maxine-ar`: a second example, a module with two stages that share one SDK, its own configuration table and a probe that reports each stage's reason separately;
- the open-model cut-out module, `crates/obc-module-onnx`: a module with one stage over a runtime library it opens at run time (ONNX Runtime), several engines probed generically, model files it checks by checksum, and the model of the primary-context rule (see [Threading and CUDA rules](#threading-and-cuda-rules));
- the CPU voice module, `crates/obc-module-voice-cpu`: an audio module with one stage and two engines, which needs no model files;
- the Maxine audio effects module, `crates/obc-module-afx`: an audio module with three stages over an SDK it opens at run time, live strengths, a combined effect with a fallback, and a delay it measures at load (see [The audio modules in this repository](#the-audio-modules-in-this-repository)).

## What a module provides

A module provides **stages**. A stage is one step of the video pipeline (or, for an audio module, of the microphone's: see [Audio modules](#audio-modules)): noise removal, relighting, the cut-out of the person, a colour tint. A stage is named by an id (`denoise`), sits in a **slot** (where it runs relative to the cut-out), has a **rank** and a **device** (which the daemon uses to choose between modules that provide the same stage), and declares the **frame kinds** and **pixel formats** it takes. A module also describes the **settings** a user can change (a switch for each stage, parameters), in the same self-describing model as the camera's settings, so that the CLI and the bar plugin draw them with no code of their own.

Three stage ids have a meaning to the daemon:

- `background`: the cut-out of the person, with the modes `none`, `blur` and `replace` and the blur strength. **The provider composites**: it makes the matte and returns the finished picture, from the parameters the host sends it (see [The background stage](#the-background-stage)); the daemon chooses the mode and the strength and does not blend. If the privacy of the effect depends on it (the room behind the person must not show), it never fails open. The module's settings bound to this stage as load-time parameters (the Maxine module's `chair`, which chooses what the cut-out does with a chair) reach the provider with the effect, and a new value loads the chain again as a switch, the previous chain running until the new one is in force (and after it, if the new one is refused). In system memory the new chain loads on a second instance of the module while the first runs; on CUDA frames it loads on the running instance, which holds the last frame meanwhile.
- `enhance`: the upscaler of the ingest, which the daemon applies to a camera that captures well below the output size. It loads in the ingest, a chain of its own, and receives the load-time parameters of the module's settings bound to it like any other stage (the Maxine module's `method`, `ai` or `fast`); a new value reopens the camera's ingest, as turning the stage on or off does.
- `frame-generation`: a stage that makes the picture between two camera frames, which the daemon runs only while the output rate is twice the camera's (see [The frame-generation stage](#the-frame-generation-stage)). Unlike every other stage it has no switch, and it is not in the stage order.

Every other id is opaque to the daemon, and the ids are a registry: see [Stage ids and settings groups](#stage-ids-and-settings-groups-the-registry). With no module installed the camera image passes through, and the settings tree has no effect group.

## A module in five steps

From the root of this repository (the example needs only the SDK crate):

```sh
cargo build --release --manifest-path examples/module-hello/Cargo.toml
dir=~/.local/share/omarchy-broadcast/modules.d
mkdir -p "$dir"
cp examples/module-hello/hello.toml examples/module-hello/target/release/libobc_module_hello.so "$dir/"
sed -i 's|^library = .*|library = "libobc_module_hello.so"|' "$dir/hello.toml"
systemctl --user restart omarchy-broadcastd     # `systemctl --user reload omarchy-broadcastd` also finds a new manifest
omarchy-broadcast modules
omarchy-broadcast settings set effect.x_example_hello.enabled on
```

The module is listed, with its stage `x-example-hello` (`selected`), and its settings (`effect.x_example_hello.enabled`, `effect.x_example_hello.strength`) appear in `omarchy-broadcast settings` and in the bar panel. It applies when the camera next starts. To try a module without installing it, put its manifest in a directory and point `OBC_MODULE_PATH` at it for the daemon's process.

## The manifest

A manifest is a TOML file with any name ending in `.toml`, in a `modules.d` directory. It is parsed strictly: an unknown key, a missing required key or an invalid value makes the module **unavailable**, with the parse error as the reason shown by `omarchy-broadcast modules` and `GetModules`; nothing else is affected.

```toml
abi = 1
abi_minor = 6
id = "org.example.hello"
name = "Hello tint"
version = "0.1.0"
licence = "MIT OR Apache-2.0"
kind = "video-stage"
library = "libobc_module_hello.so"
description = "Example module: tints the picture warm, on the CPU"
conflicts = []

[requires]
hardware = ["any"]
modules = []

[[stage]]
id = "x-example-hello"
slot = "post-composite"
rank = 100
device = "cpu"
position = 100
requires = []
conflicts = []
frames = ["host"]
formats = ["nv12"]
```

Top-level keys:

| Key | Required | Meaning |
|---|---|---|
| `abi` | yes | The ABI **major** version the module is built for (an integer; `1`). A manifest of another major version is listed with the reason `Built for module ABI N; this daemon supports module ABI 1`, and its library is never opened |
| `abi_minor` | no | The lowest ABI **minor** version the module needs (an integer, 0 when absent; since ABI 1.6). A host of an older minor lists the module with the reason `Needs module ABI 1.N; this daemon has module ABI 1.M` and never opens its library, and the key is read before the strict parse, so a manifest that uses keys of a later minor is listed with that reason and not with a parse error. A library built for a lower minor than its manifest's `abi_minor` is refused (`The library is built for module ABI 1.K, older than the module ABI 1.N its manifest needs`). Write the minor your SDK or header defines (the SDK's `obc_module_abi_version` returns it) |
| `id` | yes | The module's id: reverse-DNS, lower-case letters, digits and hyphens in at least two dot-separated segments (`^[a-z0-9]+(\.[a-z0-9-]+)+$`), at most 64 bytes. Unique: a module with the same id in a later directory replaces it |
| `name`, `version`, `licence` | yes | Shown in the module list; non-empty text. The `licence` is the module's own (a module can be proprietary or under a licence of its own) |
| `kind` | yes | `video-stage` (loaded by the daemon) or `audio-filter` (ABI 1.2: loaded by the audio worker, see [Audio modules](#audio-modules); the daemon lists it and never opens its library). `analyser` is reserved: such a module is listed as unavailable ("not supported yet") and never loaded |
| `library` | yes | The shared library: an absolute path, or a path relative to the manifest's directory |
| `description` | no | One line of text |
| `conflicts` | no | A list of module ids this module cannot be loaded with. Enforced from D-Bus revision 14: an enabled module that declares a conflict with another enabled, installed module is refused (see [Conflicts, requirements and forcing](#conflicts-requirements-and-forcing)). Each id must be a valid module id, not the module's own, and a repeat is ignored |
| `[requires] hardware` | no | A list of text, informational only: the daemon does not use it, and the probe says what can run |
| `[requires] modules` | no | A list of module ids this module cannot be loaded without (the same rules as `conflicts`). Enforced from revision 14: the module is refused with `Needs the module <id>` unless every module named is enabled and installed |
| `[[stage]]` | at least one for `video-stage` and for `audio-filter` | One table per stage provided; a stage id twice is an error. The keys of a stage depend on the kind (below) |

Keys of a `[[stage]]`:

| Key | Required | Meaning |
|---|---|---|
| `id` | yes | The stage id. A lower-case letter followed by up to 31 lower-case letters, digits or `-` (`^[a-z][a-z0-9-]{0,31}$`). Not `status`, `effect`, `reason`, `module`, nor any name ending in `-reason` (those are field names of the daemon's status messages). Stage ids are one namespace across modules: two modules that declare the same id are two providers of that stage. The ids of an `audio-filter` module must start with `mic-` and those of a `video-stage` module must not. **An id without the private prefix is the registry's** (see [Stage ids and settings groups](#stage-ids-and-settings-groups-the-registry)); a stage of a module's own has the vendor form `x-<vendor>-<name>` (`mic-x-<vendor>-<name>` for an audio stage). An id of neither form is loaded, with a warning in the module's log |
| `slot` | yes | For a video stage, where the stage runs relative to the person's cut-out, in pipeline order: `pre-matte` (on the camera frame, before the cut-out), `matte` (the cut-out itself), `post-matte` (on the person, after the cut-out), `post-composite` (on the finished picture: the Maxine module's HDR look is one). A stage never comes after a stage of a later slot. For an audio stage the slots are `clean` (removing what is not the voice: noise, room echo) and then `shape` (working on the voice that is left: enhancement, equalisation); a slot of the other kind is an error |
| `rank` | yes | A non-negative integer. When several modules provide the same stage, the `quality` profile takes the highest rank |
| `device` | yes | `nvidia-gpu`, `gpu`, `npu` or `cpu`: what the stage runs on. The `battery` profile prefers an NPU, then the CPU, then a GPU, then an NVIDIA GPU, and then the rank |
| `position` | no | An integer (default 100). Where the stage prefers to sit within its slot in the default order: lower first, then by stage id |
| `requires` | no | A list of stage ids and slot names that must come earlier in the order. A slot name (`matte`) is met by any earlier stage of that slot. A requirement that no enabled module offers is not an error of the order. It is enforced when the provider is chosen (revision 14): a stage that `requires` a stage that has no provider, or a slot in which no stage has a provider, cannot run, and is listed unavailable with the reason `Needs <stage>` or `Needs a <slot> stage` |
| `conflicts` | no | A list of stage ids that cannot be active with this one. Enforced from revision 14: of two stages that are both switched on and conflict (either one names the other), one is dropped from the session. The stage that declares the conflict wins; when both declare it, the one earlier in the stage order loses; `background` never loses, because the cut-out hides what is behind the person. The loser is dropped with a `Warning` (`<label> unavailable: Cannot run together with <label>`) and listed `available` false with the reason `Cannot run together with <label>`; the feed goes on without it. This is the rule the audio worker applies to the microphone's stages. Not enforced while the module of either stage is forced |
| `frames` | yes | The frame kinds the stage takes, in order of preference: `cuda` (device memory) or `host` (system memory). `dmabuf` is reserved in the ABI and accepted in the manifest, and no host offers it. An audio stage takes `["host"]` only. **A stage without `host` is unavailable when the video pipeline runs in system memory** (a machine with no usable CUDA, or `[video] memory = "system"`): it is listed `available` false with the reason "Needs an NVIDIA GPU (CUDA)" or "Needs CUDA; the video pipeline is set to system memory", never selected, and another provider that lists `host` is chosen in its place. A module none of whose stages lists `host` is unavailable as a whole in that memory, and is still opened and probed. A stage that lists both kinds runs with the one the session has. In CUDA memory nothing is filtered: a stage with only `host` runs on a host copy of the CUDA frames. List `host` only if the stage really works on host NV12 frames without a CUDA call |
| `formats` | video stages | The pixel formats the stage takes: `nv12`, `i420`, `bgra`, `rgba` or `rgb-f32-planar`. An audio stage has none |
| `rates`, `frame_samples` | audio stages | The sample rates (`[48000]`) and the block sizes in samples per channel (`[480]`) the stage takes, in order of preference. Not keys of a video stage |
| `channels` | no (audio stages) | The channels the stage takes, 1 to 8; 1 when not given |

Each kind's keys and vocabularies are checked: a video stage with `rates`, an audio stage with `formats`, a slot or a slot requirement of the other kind, or a stage id that breaks the `mic-` rule makes the manifest invalid, and the module is listed unavailable with the parse error as the reason.

**Provider, order and placement.** The provider of a stage is the module the user pinned, if it can run it, else the best by the profile; the choice is made when the camera starts and never changes while it runs. The stages of all enabled modules have one order, which the user may change within the slots and the `requires` entries (the default is by slot, `position` and id). The provider of `background` runs the whole cut-out family (its stages in `pre-matte`, `matte` and `post-matte`) inside the effect element, so a stage in the `matte` or `post-matte` slot, or one that `requires` the `matte` slot or the `background` stage, is run only by that module (the `background` stage itself excepted): a pin to another module cannot be honoured for it, and if only another module provides it, it is unavailable with the reason "Needs the cut-out of the module that provides it". The stages of any other module run in their own element, those in `pre-matte` before the effect element and the others after it. The `enhance` stage runs in the ingest, in a chain of its own.

### Conflicts, requirements and forcing

The rules of this section are enforced from D-Bus revision 14. Those on modules are decided from the manifests of the modules that are enabled and that this host can load, before any library is opened, so that two modules that conflict are never loaded into one process.

- **Module conflicts.** An enabled module whose manifest declares `conflicts = ["<module id>"]` with another enabled, installed module is refused: its status is `unavailable`, its reason `Conflicts with <name> (<id>)` (the other module's name, made one line and at most 64 bytes), and its library is never opened. A conflict holds whichever module declares it. The declarer is the one refused; when both declare it, the one with the higher module id is refused. The module that provides `background` (by pin, then by profile, whatever its probe says) is never the one refused: the other module is refused instead, whether or not it declares the conflict. A module that is disabled, whose manifest is broken or of another ABI, or that is an audio module (the video daemon does not load it) takes no part.
- **Module requirements.** A module whose manifest lists `[requires] modules = ["<id>"]` is refused with `Needs the module <id>` unless every module named is enabled and installed. The check is not transitive: a module that needs one refused by a conflict, or by its own requirement, is not refused for it, so one refusal never spreads.
- **A refusal never takes the background effect from a running session.** The module that runs the session's background effect stays open, and able to run its stages, until the session ends, even if a conflict refuses it meanwhile (a module enabled during the session that conflicts with it is not opened until then, so two conflicting libraries never run in one process). The refusal applies at the next start, as a pin does; `GetModules` lists the module as refused and `Warning` carries `<name> unavailable from the next start: <reason>`.
- **What the lists show.** A refused module is `unavailable` with the reason, and so are its stages. A stage that a module could run but the resolution refuses (`Needs <stage>`, `Needs a <slot> stage`, the cut-out placement rule, a stage conflict) is listed `available` false with the reason, never `selected`.

**Forcing a module (for debugging and testing).** The configuration table `[modules.forced]` takes a module id and `true` (the key is `modules.forced.<id>`, [config.example.toml](config.example.toml)): the conflicts that involve that module are not enforced, whichever side declares them, for modules and for stages, so that both load and both stages run. It lifts conflicts only: `[requires] modules`, a stage's `requires` and the cut-out placement rule hold. It applies to video modules; an audio module's conflicts are lifted by `[mic] force_conflicts` instead, and a force for an audio module has no effect and no row in the schema.

A force is written like any configuration choice, with `config set` or `SetConfig`, or with `modules enable --force`, which writes the force first and then enables the module:

```sh
omarchy-broadcast config set modules.forced.<id> true
omarchy-broadcast modules enable <id> --force
```

A plain `modules enable` clears a force. A forced module is marked wherever it is listed: `forced` in `GetModules`, in `modules list` and in its JSON, a `Forced:` line in `modules info`, and the bar plugin's setup dialog; the dialog shows the mark and does not offer the switch. While a force actually lets two modules or two stages run together, `Warning` carries `forced: <A> runs together with <B> despite a declared conflict (modules.forced)`; a force that overrides nothing says nothing.

### The frame-generation stage

The stage id `frame-generation` is vendor neutral: any module may provide it, as the Maxine module does with NVIDIA Video Frame Generation (a feature of the SDK's own library, with no model files to install). It makes the picture midway between two camera frames, so that an output at **twice** the camera's rate shows a new picture on every tick and not the same picture twice. It differs from every other stage in what the daemon does with it:

- **It is never a user's choice.** It has no enable setting and no row in the settings tree. It is not part of the stage order: `modules.order` never lists it, a stored order that names it is kept without it, and a `SetStageOrder` that names it is refused (`The order cannot name frame-generation: it always runs after the other stages`). Its pin row (`modules.pin.frame-generation`) is shown only when two modules provide it. A stage with no enable setting runs whenever it is provided; this one runs while the output rate is twice the camera's, within 0.2 %, and is otherwise off and loads nothing.
- **It runs last.** The daemon plans it as a chain of its own, named `<module id>-gen`, after the effect element and every other stage, in an `obcmodule` element, whatever slot and position the manifest gives it (the Maxine module gives it the last position of `post-composite`). It sees every tick, fresh frames and repeats, and the frames are NV12 in CUDA memory of one size.
- **What it does with each tick.** A module author follows this contract: on a **fresh** frame, write the frame midway between the previous frame and this one (so the output is one output tick later than the input); on a **repeat** (`OBC_FRAME_FLAG_REPEAT`), write the held current frame; on the first frame after a load or `reset_temporal`, which has no previous frame, write the frame as it is.
- **It fails open, and alone.** If it cannot load or fails on a frame, only the stage is dropped (`OBC_ERR_STAGE`): the frame goes out as it came, the output repeats frames as before and the daemon's `Warning` says so (`60 fps output repeats frames: no frame generation available`). Nothing else changes.
- **Choosing it.** The user chooses a rate with `(generated)` in its label for `output.mode` ([dbus-api.md](dbus-api.md#capture-and-output-modes-revision-12)). Those options are offered only while some enabled module provides the stage.

### The Maxine module stages

The Maxine module (`br.com.dias.maxine-vfx`) provides these video stages, in the order the daemon runs them. A stage whose SDK feature is not installed is listed as `unavailable: Not installed` and its setting is greyed. The features are in [setup/maxine.md](setup/maxine.md#1-requirements).

| Stage | Slot | What it does | Setting |
|---|---|---|---|
| `enhance` | `pre-matte`, position 0 | Upscales a low-resolution camera, in the ingest | `effect.video.enhance`, `effect.video.enhance_method` |
| `denoise` | `pre-matte` | Video noise removal | `effect.video.denoise` |
| `background` | `matte` | The cut-out of the person | `effect.background.*`, `effect.cutout.chair` |
| `relight` | `post-matte`, needs the cut-out | Relighting of the person | `effect.lighting.*` |
| `hdr` | `post-composite` | The HDR look: NVIDIA TrueHDR, tone-mapped back to SDR | `effect.video.hdr` |
| `frame-generation` | `post-composite`, last | The picture between two camera frames, while the output rate is twice the camera's | none (see above) |

**The `post-composite` slot** is the finished picture: the cut-out, the blur or the replacement, and every stage before it are done. The Maxine module has two stages there. `hdr` is the HDR look: it converts the NV12 frame to RGBA, runs NVIDIA's TrueHDR (SDR to HDR10: the PQ transfer, BT.2020 primaries, 10 bits), tone-maps the result back to SDR (PQ decode, a scaling that keeps the mid-tones at their SDR level, the BT.2390 EETF on the luminance to a 100-nit target, BT.2020 to BT.709, a clip and the re-encode) and converts it to NV12, all on the GPU, so every 8-bit output carries the look (the output is 8-bit SDR and no call application consumes HDR10). It has a switch (off by default) and no other setting, it runs on every frame it receives, and it fails open and alone: a failure drops the stage and the composite goes out. The daemon runs it inside the effect element with the module's other stages, before `frame-generation`.

### The ONNX module stages

The open-model cut-out (`br.com.dias.onnx-cutout`, "Open-model cut-out") provides one video stage. Its user's guide is [setup/onnx.md](setup/onnx.md), it exists so that there is a cut-out without NVIDIA's SDK, which the project cannot distribute, and it runs open-licensed models.

| Stage | Slot | Rank, device | What it does | Setting |
|---|---|---|---|---|
| `background` | `matte` | 200, `cpu` | The cut-out of the person, with the blur or the replacement, from an open model run by ONNX Runtime | `effect.onnx.model`, `effect.onnx.engine` |

- **Choosing it.** It competes with the Maxine module's `background` (rank 300) like any two providers of a stage: with both installed the `quality` profile picks Maxine, the `battery` profile picks this module (its device is `cpu`), and a pin picks either whatever the profile. The blur strength and the replacement image are the daemon's own `effect.background.*` settings.
- **Settings.** Both are parameters of the stage with `apply = reload`, so a change is a switch: the previous chain keeps running until the new one is loaded, and keeps running if the new one cannot be (a refused switch, [setup/onnx.md](setup/onnx.md#4-choose-the-module-the-model-and-the-engine)). `effect.onnx.model` is `mediapipe-landscape` (default), `pphumanseg-v2-mobile`, `pphumanseg-v2-lite` or `modnet`; `effect.onnx.engine` is `auto` (default), `tensorrt`, `cuda`, `openvino-npu`, `openvino-gpu`, `openvino-cpu` or `cpu`. Both are greyed while the background effect is off.
- **Status.** The module loads and probes with no ONNX Runtime and no model file, and then lists the stage as `unavailable` with the reason (`ONNX Runtime not installed ...`, `No cut-out model installed ...`). Its probe reads files only.
- **The cut-out never fails open**, as for the Maxine module: a frame that fails fails the whole chain, and a load that fails leaves the running chain as it was.
- **Frames.** The manifest declares `frames = ["cuda", "host"]` and NV12. For a CUDA frame the module copies to page-locked host memory on the frame's stream, processes on the CPU or on the ONNX Runtime engine, and copies back. In CUDA memory the daemon hands it CUDA frames (the order of `frames` is its preference). **In system memory it gets host frames and `_host.cuda = false`**: the module then runs on the CPU, or on OpenVINO where the installed ONNX Runtime has it, and a machine with no NVIDIA GPU runs the daemon's effect path with it (blur and replace; [setup/onnx.md](setup/onnx.md#using-it-on-a-machine-without-an-nvidia-gpu)). The Maxine module's `background` takes CUDA frames only, so it is unavailable there.
- **Engines and caches.** Under `auto` the module tries TensorRT, CUDA, OpenVINO on the NPU, the GPU and the CPU, then ONNX Runtime's CPU provider, and takes the first that starts; an engine chosen explicitly that cannot run falls back to the engines after it in that order only (a chosen OpenVINO engine or CPU never falls to TensorRT or CUDA), and the load reports a notice naming the engine chosen, why it cannot run and the engine used (ABI 1.5), which the daemon shows as a `Warning` clause; the setting is not changed. Only when none of those engines starts does the load fail, with an error that leads with the chosen engine's reason, then lists why each later engine could not run (`<Chosen> was chosen and cannot run: <reason>; no later engine could run it (<engine>: <reason>; ...)`). TensorRT engines are cached under `$XDG_CACHE_HOME/omarchy-broadcast/trt/`.
- **Configuration.** The module's table in `config.toml` takes `ort_library` (the library), `model_dir` (the first model directory searched) and `threads` (CPU threads, 1 to 64, default 4), all optional.

## Discovery and installation

A module is native code that runs inside the daemon's process, with no signing and no sandbox: install only modules you trust (see [Failure semantics](#failure-semantics)).

The daemon reads manifests from these directories, in order; a later directory overrides an earlier one **by module id**:

1. `/usr/lib/omarchy-broadcast/modules.d`: packaged modules.
2. `$XDG_DATA_HOME/omarchy-broadcast/modules.d`, by default `~/.local/share/omarchy-broadcast/modules.d`: the user's own.
3. The directories of `[modules] dirs` in `config.toml` (absolute paths).
4. The colon-separated directories of the `OBC_MODULE_PATH` environment variable, for development and tests.

Discovery reads manifests and nothing else: no library is opened, so a broken library cannot stop the daemon from starting. The daemon looks again at its start and on `SIGHUP` or `ReloadConfig`. A library is opened for a module that is **enabled** (all are, except the ones the configuration's `[modules] disabled`, a `modules.enabled.<id> = false`, the user, or the crash ledger disabled), and that no conflict or requirement refuses ([Conflicts, requirements and forcing](#conflicts-requirements-and-forcing)). The user enables or disables a module with:

```sh
omarchy-broadcast modules enable ID
omarchy-broadcast modules disable ID
```

The Arch build makes a package for each module of this repository, so that a module is never installed without the SDK or runtime it needs and a user who wants no NVIDIA software installs none. `omarchy-broadcast-maxine-vfx-git` installs the Maxine module as `/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so` with its manifest, `maxine-vfx.toml`, in `/usr/lib/omarchy-broadcast/modules.d` (the manifest names the library by absolute path); `omarchy-broadcast-maxine-ar-git` installs the Maxine AR module likewise (`libobc_module_maxine_ar.so`, `maxine-ar.toml`), and `omarchy-broadcast-onnx-git` the ONNX module. Each depends on the module ABI that the core provides, as a range ([Packaging a module](#packaging-a-module)), and on its SDK or runtime. The Maxine packages need no drop-in: the daemon applies the SDK package's environment to itself. `scripts/install-user.sh` in a checkout builds both Maxine modules and installs them for the current user, in `~/.local/share/omarchy-broadcast/modules/` with a manifest in `~/.local/share/omarchy-broadcast/modules.d/`, which replaces the packaged manifest of the same id. The two audio modules ([The audio modules in this repository](#the-audio-modules-in-this-repository)) are installed likewise: the CPU voice module by the core package and the Maxine audio module by `omarchy-broadcast-maxine-afx-git`, and both by the `--audio` option of `install-user.sh`, run from a checkout:

```sh
scripts/install-user.sh --audio
```

A module you build yourself goes in your own `modules.d`, with `library` relative to the manifest or absolute.

**A rebuilt library needs a daemon restart.** Libraries are opened with `RTLD_NODELETE` and are never unloaded: a module may hold process-wide state that must outlive a reload, and code it started must never run unmapped. A library rebuilt at the same path is therefore not loaded again while the daemon runs; the code first loaded stays. A new or changed manifest, or a library at another path, is picked up on a reload; for a rebuilt library, restart the daemon:

```sh
systemctl --user restart omarchy-broadcastd
```

## Module configuration

A user configures a module in `config.toml`, in a table named after its id:

```toml
[module."br.com.dias.maxine-vfx"]
sdk_root = "/home/user/.local/opt/maxine/VideoFX"
```

The table is free-form apart from the keys that start with `_`, which are the host's: the daemon checks only the id, and passes the table to the module's `init` as a JSON object (`{}` when there is none). The module checks its own keys and may refuse the table, which makes it unavailable with the reason. The daemon adds one key, **`_host`**, to every module's configuration, with these reserved members: `{"_host": {"compute_capability": [12, 0]}}` (the GPU's compute capability as `[major, minor]`, or `null` when it is not known without waking the GPU; in system memory it is never read from the GPU, so it is the cached value or `null`) and, **only when the video pipeline runs in system memory, `"cuda": false`** (`{"_host": {"compute_capability": null, "cuda": false}}`, with the cached capability in place of `null` when there is one). The key is absent when CUDA is allowed, and a module that finds none treats CUDA as allowed. **A module that sees `cuda = false` must make no CUDA call**: no context, no CUDA runtime, no driver call, not even to learn the device. It may still run on the CPU or on a non-CUDA accelerator, and an engine or a path of its own that needs CUDA is to be reported unavailable with a reason that says so (the ONNX module skips TensorRT and CUDA under `auto` with "CUDA is off: the video pipeline runs in system memory", fails an explicit `cuda` or `tensorrt` with it, and refuses a chain of CUDA frames). A change of memory changes the module's configuration, so the daemon opens the module again at the next reload or camera start, as for any configuration change. Both members are read, never written, by a module; the daemon owns the `_host` key, and the same name in a user's `[module."<id>"]` table is replaced. **Every key that starts with `_` is reserved**, not only `_host`: a key of that form that the daemon does not define is not passed to the module, with a warning (for a module of ABI 1.6 or later; a module of an older minor still gets it, with a warning, as a host of 1.5 did). A module that writes its own configuration keys names them without a leading `_`. A module that refuses unknown keys must accept `_host` and may ignore it. The Maxine module also takes `[effect] maxine_root` as its `sdk_root` when its own table sets none.

The enabled modules, the stage order, the backend profile and the pins are `[modules]` keys of the configuration (`enabled`, `disabled`, `forced`, `order`, `profile`, `pin`, `dirs`). The user's choices made through the daemon (`omarchy-broadcast modules enable|disable|profile|pin`, `order set`, the bar's setup dialog, the D-Bus setters) are written by the daemon to `config.d/50-panel.toml`, which is read after `config.toml`; a file that sorts after it, such as `config.d/90-local.toml`, wins and the daemon then refuses to change that choice. A module's own table (`[module."<id>"]`) is not written by the daemon. A daemon of 0.24 kept these choices in the state file; the daemon moves them to `config.d/50-panel.toml` when it first loads. See [config.example.toml](config.example.toml).

## The ABI and the SDK

A module library exports two C symbols:

| Symbol | Does |
|---|---|
| `uint32_t obc_module_abi_version(void)` | Returns the ABI version the library was built against, packed as `major << 16 \| minor`. No side effects: the host calls it before anything else |
| `const obc_module_vtable *obc_module_entry(const obc_host *host)` | Takes the host's description (its ABI version and a log callback) and returns the module's vtable, or null to refuse the host |

**Version rules.** The host refuses a library whose major version is not its own, and one built for a **newer minor** version than its own, each with a reason naming both versions; the entry point is not called then. A library built for an older minor version of the same major version loads. A structure that can grow starts with `struct_size`, and a reader reads a field only when it fits in `struct_size`; vtable entries and fields are only ever appended; a vtable at least as large as that of ABI 1.0 is accepted, and an entry past its `struct_size` counts as not set. A boolean crossing the boundary is a `uint8_t`. The symbol `obc_module_abi_version`, its name and its signature, are the same in every major version of the ABI, so that a host can tell a library of another major version before it calls anything else. From ABI 1.6 the host applies the version rule to everything it gives a module, not to the flags alone: see [Version gating](#version-gating).

**ABI 1.1** adds one thing, `OBC_FRAME_FLAG_REPEAT` (below). The change is additive: a library built for ABI 1.0 loads and runs unchanged, because the host hands a module only the `obc_frame::flags` bits that its ABI version defines, and clears the others on every frame. A module of ABI 1.0 therefore never sees the new flag and processes a repeat as any other frame. 
**ABI 1.2** adds the audio kind ([Audio modules](#audio-modules)): the structures `obc_audio_request`, `obc_audio_load_report` and `obc_audio_buffer`, the constants `OBC_AUDIO_F32_PLANAR`, `OBC_AUDIO_FLAG_NEW_SOURCE`, `OBC_AUDIO_FLAG_DISCONT` and `OBC_MAX_AUDIO_CHANNELS`, and two vtable entries at the end, `audio_instance_load` and `audio_instance_process`. Nothing existing changes: a library built for ABI 1.0 or 1.1 has a vtable that ends before the new entries, which the host counts as not set, and loads and runs unchanged. The `obc-module-abi` and `obc-module-sdk` crates were 1.2.0 with it.

**ABI 1.3** adds named status values ([Status values](#status-values-abi-13)): the structure `obc_status_value` (32 bytes) and one vtable entry at the end, `instance_status_values`, which is nullable. Nothing existing changes: a library built for ABI 1.0, 1.1 or 1.2 has a vtable that ends before the new entry, which the host counts as not set, and loads and runs unchanged, reporting no values. The converse does not hold: a module built for 1.3 on a host of 1.2 is refused by the minor-version check above, since the host cannot know the entry. The `obc-module-abi` and `obc-module-sdk` crates were 1.3.0 with it.

**ABI 1.4** adds the per-frame background ([The per-frame background](#the-per-frame-background-abi-14)): one vtable entry at the end, `instance_set_background`, which is nullable, with a contract for the frame it takes. Nothing existing changes: a library built for ABI 1.0 to 1.3 has a vtable that ends before the new entry, which the host counts as not set, and loads and runs unchanged; the host also never calls the entry of a library that declares an older minor version, whatever the size of its vtable. Such a module shows a video background as the still image of its load. The converse does not hold: a module built for 1.4 on a host of 1.3 is refused by the minor-version check above. The `obc-module-abi` and `obc-module-sdk` crates were 1.4.0 with it.

**ABI 1.5** adds the load's notice: `obc_load_report::notice`, a nullable NUL-terminated string appended after `reserved0`, the first field appended to a structure since ABI 1.0. A module sets it on a load that succeeded when the user should know something of it: a setting it could not honour and what runs in its place (the open-model cut-out's engine chosen explicitly that cannot run, and the engine it falls back to; the text reads `<Requested> was chosen and cannot run: <reason>; using <engine>`). The SDK's field is `LoadReport::notice: Option<String>`. The string belongs to the instance and stays valid until the next call on it. The daemon shows it as a `Warning` clause, `<module name>: <notice>`, while that chain runs; a later load without one ends it. The field is gated by `struct_size`: a host of ABI 1.4 or older passes the report's 1.0 size (`OBC_LOAD_REPORT_SIZE_1_0`, 16 bytes), so a module never writes the field for it, and the host reads the field only from a module of ABI 1.5 or later whose report has room for it, so a module of 1.4 or older never has a notice. Such a module loads and runs unchanged. The converse does not hold: a module built for 1.5 on a host of 1.4 is refused by the minor-version check above. The `obc-module-abi` crate was 1.5.0 with it.

**ABI 1.6**, the release from which ABI 1 is stable ([Stability promise and versioning](#stability-promise-and-versioning)), adds no capability: no field, entry or constant that gives a module something new. It states the rules, reserves the names and fixes what a module author may rely on: the host presents itself as a host of the module's declared minor version ([Version gating](#version-gating)); the base sizes of every growable structure are in the header (`OBC_*_SIZE_1_*`, such as `OBC_HOST_SIZE_1_0`, `OBC_FRAME_SIZE_1_0`, `OBC_CHAIN_REQUEST_SIZE_1_0`, `OBC_LOAD_REPORT_SIZE_1_0` and `OBC_LOAD_REPORT_SIZE_1_5`, `OBC_AUDIO_*_SIZE_1_2` and `OBC_MODULE_VTABLE_SIZE_1_0` to `_1_4`), and the header refuses to compile for a target that is not LP64 little-endian; the contract of the `background` stage is in the header by name ([The background stage](#the-background-stage)); the stage-id and settings-group registry, the vendor forms and the reserved names ([Stage ids and settings groups](#stage-ids-and-settings-groups-the-registry)); the rules of threads, GPU contexts and the process ([Threading and CUDA rules](#threading-and-cuda-rules)); the entries a module must set, per kind (below); `instance_park` and `instance_unpark` are reserved; and the manifest's optional `abi_minor` ([The manifest](#the-manifest)). `action` stops being a module setting type ([Settings](#settings)). Nothing existing changes: a library built for ABI 1.0 to 1.5 loads and runs unchanged. The converse does not hold: a module built for 1.6 on a host of 1.5 or older is refused by the minor-version check above. The `obc-module-abi` crate is 1.6.0 with it, and the `obc-module-sdk` crate 2.0.0 ([Crates](#crates-and-their-versions)).

**Stability.** ABI 1 is stable from 1.6: the version numbers are a promise as well as the host's check. The promise, and what a minor version may and may not do, are in [Stability promise and versioning](#stability-promise-and-versioning).

**The vtable** (every entry is a nullable function pointer; the header says which are required):

| Entry | Does |
|---|---|
| `init(config_json, &module)` | Creates the module object from its configuration. Cheap: no GPU work, no model loading |
| `probe(module, &json)` | Says which stages can run here: `{"stages":[{"id":"denoise","available":true},{"id":"relight","available":false,"reason":"Not installed"}]}`. **Filesystem checks only**: it must not create a CUDA context, load a model or otherwise wake a runtime-suspended GPU. The reason is shown to the user |
| `check(module, stages, n)` | Optional. A deeper check, before a stream that needs the module starts, that the stages can run: it may open the module's own runtime libraries, never touch the GPU. A non-zero status (`OBC_ERR_UNAVAILABLE` for a runtime that is not there) with the reason in `last_error` tells the user before the camera opens |
| `describe_settings(module, &json)` | The module's settings, below |
| `instance_create`, `instance_destroy` | An **instance** runs one chain of this module's stages for one stream; all instances are destroyed before the module |
| `instance_load(instance, request, &report)` | Required for a `video-stage` module; an `audio-filter` module may leave it null (since ABI 1.6; a host of ABI 1.5 or older requires it, so a module that supports those sets it to a stub that fails with `OBC_ERR_UNSUPPORTED`). Loads the chain of stages the request names, in order, for one frame geometry (below); the report carries the load time, the warm-up, whether models were reused and, from ABI 1.5, an optional notice (the SDK's `LoadReport::notice`) |
| `instance_process(instance, in, out, live, n_live)` | Required for a `video-stage` module and optional for an `audio-filter` module, likewise. Runs one frame through the loaded chain. `out` equals `in` only for an in-place chain. The per-frame parameters apply from this frame |
| `instance_reset_temporal(instance, &dropped_json)` | Forget temporal state (a new camera); returns, as a JSON array, the stage ids that could not keep running and were dropped |
| `instance_unload` | Release what the last load loaded; the instance can be loaded again. Must cope with any failed load |
| `instance_timings` | Optional: how long each step of the last frame took, for statistics. The host reads it within the bounds of [Status values](#status-values-abi-13) |
| `instance_status_values(instance, &values, &n)` | ABI 1.3, optional (null: the module reports no values). The named numeric values of the last processed frame, for the daemon's status, such as auto-frame's zoom. See [Status values](#status-values-abi-13) |
| `instance_set_background(instance, background)` | ABI 1.4, optional (null: the module shows a video background as the load-time still). Replaces the background of a loaded chain that replaces it, for the following `instance_process` calls, until the next call. See [The per-frame background](#the-per-frame-background-abi-14) |
| `audio_instance_load(instance, request, &report)` | ABI 1.2, required for an `audio-filter` module, null for the others. Loads a chain of audio stages for one block format; the report carries the load time, the warm-up, the block size chosen and the chain's algorithmic delay in samples |
| `audio_instance_process(instance, in, out, live, n_live)` | ABI 1.2, required for an `audio-filter` module. Runs one block of audio through the loaded chain; `out` never overlaps `in`; the module may set the output's `voice_probability` |
| `instance_park`, `instance_unpark` | **Reserved**: a module leaves them null. No host of ABI 1.x calls them unless a later minor version defines them, and then only for a module that declares that version (the daemon unloads a session's instances when it stops) |
| `last_error`, `instance_last_error` | The reason of the last failed call; never null |
| `destroy` | Releases the module object |

A failing call returns a non-zero status: `OBC_ERR_INVALID` (1), `OBC_ERR_UNSUPPORTED` (2), `OBC_ERR_UNAVAILABLE` (3), `OBC_ERR_FAILED` (4), `OBC_ERR_STATE` (5), `OBC_ERR_PANIC` (6) or `OBC_ERR_STAGE` (7, one stage failed on its own; `instance_last_error` is `<stage id>: <reason>`). A host treats an unknown code as `OBC_ERR_FAILED`.

**The chain request.** `instance_load` receives the stage ids of the chain, in the order they run, with the input frame's kind, format and size, the output format and size (an upscaler's output is larger), the colour description (matrix `bt601`, `bt709` or `bt2020`; full or limited range; the chroma siting), the CUDA context and stream (null for host frames: in system memory the daemon loads the `background` chain with frames of kind `OBC_FRAME_HOST` and both CUDA handles null, and calls every method of that instance with no context current), the load-time parameters of every stage, `enhance` and `background` included, as `(stage, key, value)` triples, and, for a replacement, the background image as a host BGR frame (valid only during the call). A load on a loaded instance is a **switch** when the request keeps the frames of the loaded chain (same kinds, formats, sizes, colour and CUDA handles: only the stages, the parameters or the background change), and may reuse the models of the previous load; with other frames it is a **reload**.

**A frame** is a description: kind (`OBC_FRAME_CUDA`, `OBC_FRAME_HOST`; `OBC_FRAME_DMABUF` reserved), format (`NV12`, `I420`, `BGRA`, `RGBA`, `RGB_F32_PLANAR`, `BGR`), size, up to four planes (`ptr_or_fd`, `pitch`, `offset`; a plane starts at `ptr_or_fd + offset`), the CUDA handles for device memory, a presentation time and flags (bit 0, `OBC_FRAME_FLAG_NEW_SOURCE`: the first frame of a new source; from ABI 1.1, bit 1, `OBC_FRAME_FLAG_REPEAT`: this frame repeats the camera frame of the previous call, see below). The pixels belong to the host: a module may access them only during the call.

**Repeats** (ABI 1.1). The output clock emits a frame on every tick of the output rate. When no new camera frame has arrived since the last tick it emits the last one again: a **repeat**. The host marks it with `OBC_FRAME_FLAG_REPEAT` for a module built for ABI 1.1 or later (the Rust SDK: `Frame::is_repeat()`); the frame has the same picture as the previous call's, which that call has processed. A module may use the flag to skip work on a repeat, to answer it from what it keeps, or to tell a repeat from a fresh frame (a frame-generation stage makes a picture from two fresh frames); a module that ignores it processes the repeat as any frame, as a module of ABI 1.0 does. It still writes `output` on a repeat (an in-place chain's frame may be left as it came). In the daemon the flag reaches a module through an `obcmodule` element. The effect element, which runs the stages of the `background` provider, sends its last processed output on a repeat instead of running its chain, and processes the repeat only when it has no valid output to send.

**The Rust SDK.** `obc-module-sdk` wraps all of this. A module is a `cdylib` crate that depends on the SDK, implements `Module` (`init`, `probe`, `settings`, optionally `check`, `create_instance`) and `Instance` (`unload`; `load` and `process` for video stages, which default to failing with `Unsupported`, so that an audio-only instance writes `no_video_stages!()` instead; optionally `reset_temporal`, `timings`, `status_values` (ABI 1.3), `LIVE_BACKGROUND` with `set_background` (ABI 1.4), `park` and `unpark`, which are reserved and never called; `Frame::is_new_source()` and `Frame::is_repeat()` tell the flags of a frame), and exports it with `obc_module_sdk::export_module!(MyModule);`. The macro defines the two symbols and the vtable. For every call it checks the pointers and the `struct_size` of every structure, converts the C structures to safe views (`Frame`, `ChainRequest`, `Param`), **catches a panic** (it becomes an error that carries the message, and never unwinds into the daemon: do not build a module with `panic = "abort"`) and keeps every string it hands back alive until the next call on the same object. Log lines go to the daemon's log with the `log` module of the SDK. An audio module also implements `AudioInstance` and is exported with `obc_module_sdk::export_module!(MyModule, audio);` ([Audio modules](#audio-modules)). A module outside this repository depends on the SDK through a path or a git dependency, as `examples/module-hello/Cargo.toml` shows for the path form. The SDK has its own semver and its public structures are `#[non_exhaustive]` ([Crates and their versions](#crates-and-their-versions)). The example's `Cargo.lock` is committed and `scripts/check-all.sh` builds it with `--locked`.

A module in another language implements the header directly: export the two symbols, fill in a vtable of `struct_size` bytes with the entries it supports, and follow the rules above and in the header's comments ([Writing a module in C](#writing-a-module-in-c)).

### Stability promise and versioning

**The promise.** ABI 1 is stable from 1.6: a module library built for ABI 1.N loads and behaves as documented on every host of ABI 1.M with M at least N. A module therefore needs rebuilding for a new release of omarchy-broadcast only when it wants something that a later minor version adds.

- **Covered:** the layouts, offsets and constant values of the header; the two exported symbols; the entries a module must set; the rules of ownership, lifetime and call order; the manifest keys and vocabularies of 1.N; the settings, probe and `_host` JSON of 1.N; the discovery directories; and the ids of the registry ([Stage ids and settings groups](#stage-ids-and-settings-groups-the-registry)).
- **Not covered:** which thread makes a call (see [Threading and CUDA rules](#threading-and-cuda-rules)); timing; the D-Bus interface, which has its own revisions ([dbus-api.md](dbus-api.md)); the Rust crates `obc-module-host` and `obc-backend`, which are the daemon's internals; and anything that this page and the header do not document.
- **Platforms.** ABI 1 is defined for LP64 little-endian targets (x86_64 and aarch64). The header stops the compilation with `#error` on another pointer size or byte order.
- **Fixed structures.** `obc_value`, `obc_param`, `obc_plane`, `obc_cuda`, `obc_timing` and `obc_status_value` have no `struct_size` and sit in arrays or inside other structures. Their size and layout are frozen for the life of major version 1; growth is by a new structure. (`obc_value` keeps room: a later kind may use its 8-byte payload as a pointer and `reserved0` as a length.)
- **Base sizes.** A structure that can grow is read by its `struct_size`. The header gives each one's size at the version that defined it (`OBC_HOST_SIZE_1_0`, `OBC_FRAME_SIZE_1_0`, `OBC_CHAIN_REQUEST_SIZE_1_0`, `OBC_LOAD_REPORT_SIZE_1_0`, `OBC_LOAD_REPORT_SIZE_1_5`, `OBC_AUDIO_BUFFER_SIZE_1_2`, `OBC_AUDIO_REQUEST_SIZE_1_2`, `OBC_AUDIO_LOAD_REPORT_SIZE_1_2`, `OBC_MODULE_VTABLE_SIZE_1_0`, `_1_2`, `_1_3` and `_1_4`). A module compares a `struct_size` it is given with these constants, never with `sizeof` of the structure it was built with, which grows when a field is appended.

**What a minor version may do.** A minor version only appends: fields at the end of a structure, entries at the end of the vtable, constants, manifest keys, and keys and types in the JSON documents. It never changes the meaning, layout or value of anything that exists.

**Deprecation.** Nothing is removed in major version 1. An entry may be marked deprecated; a host may stop calling it only where that equals a case the documentation already defines (a null entry). `instance_park` and `instance_unpark` are reserved in this sense: no host of ABI 1.x calls them unless a later minor version defines them, and then only for modules that declare that version.

**A break is ABI 2.** An incompatible change needs a new major version, with `abi = 2` in the manifest. A host may then load libraries of both majors for a transition; `obc_module_abi_version` is the same symbol in every major, so the major is read before anything else.

**A module newer than the host is refused.** A host refuses a library built for a newer minor version than its own, and a manifest whose `abi_minor` is newer, each with a reason that names both versions (`Needs module ABI 1.N; this daemon has module ABI 1.M`), and the module's library is not entered. The host does not load it with less than it expects: that would make every module cope with older and smaller host structures, a combination nobody can test. A module that wants to run on older hosts is built against the older SDK or header, which declares the older minor.

**Required entries, per kind.** The set of entries a module must set depends on its kind. A `video-stage` module sets the entries every module sets (`init`, `probe`, `describe_settings`, `instance_create`, `instance_destroy`, `instance_reset_temporal`, `instance_unload`, `last_error`, `instance_last_error` and `destroy`; the header marks each as required) and `instance_load` and `instance_process`. An `audio-filter` module sets the common entries and `audio_instance_load` and `audio_instance_process`, and may leave `instance_load` and `instance_process` null; a module of a kind added later may leave the entries of the other kinds null. A host of ABI 1.5 or older still requires the video entries of an audio module, so a module that supports those hosts sets them to stubs that fail with `OBC_ERR_UNSUPPORTED`.

#### Version gating

The host presents itself to a module as a **host of the module's declared minor version**, the one `obc_module_abi_version` returns. It sets no flag bit, passes no constant (a frame kind, pixel format, value kind, colour matrix, chroma siting or audio format), fills no field, sets no `struct_size` larger than that version's, accepts no setting type or status value and calls no vtable entry that the declared version does not define, whatever the size of the module's vtable says. Each addition of a minor version is one line in a table of the host, and the host's tests check, for a module of the previous minor, that it is handed nothing newer. (ABI 1.1 to 1.5 each hold under this rule: a module of 1.0 never sees `OBC_FRAME_FLAG_REPEAT`, a module of 1.3 is never called through `instance_set_background`, a module of 1.4 never has a load notice.)

The module's side of the rule:

- it ignores a parameter whose key it does not know, and the members of `_host` that it does not know;
- it accepts a `struct_size` larger than its own;
- it answers a constant that it does not know with `OBC_ERR_UNSUPPORTED`.

#### Crates and their versions

- **`obc-module-abi`** follows the ABI: its release 1.N.x describes ABI 1.N, and a patch release changes comments only. Its semver holds by construction, because a minor version of the ABI only appends.
- **`obc-module-sdk`** has its own semver, independent of the ABI and of omarchy-broadcast. A minor or patch release keeps the source of a module compiling; a major release may break it. Each release emits one ABI minor version, which a module built with it declares: release 2.0.0 emits ABI 1.6, and a module built with it needs a host of 1.6 or later. A method added to `Module`, `Instance` or `AudioInstance` in a minor release has a default.
- The public structures a module builds (`LoadReport`, `AudioLoadReport`, `StatusValue`, and the probe and settings documents with their parts) are `#[non_exhaustive]`, so that a field added later is not a source break; make them with their constructors and methods (`LoadReport::new(load_ms).with_notice(text)`), not as a literal. The enumerations a later release may extend (`ErrorKind`, `Value`, `FrameKind`, `PixelFormat`, `SettingKind` and others) are `#[non_exhaustive]` too, so a `match` on one needs a catch-all arm. `Param` and `CudaHandles` mirror fixed structures of the ABI and stay plain structures.

#### Packaging a module

A module that is packaged apart from omarchy-broadcast depends on the **module ABI**, not on one version of the daemon. In the Arch packages the core provides `omarchy-broadcast-module-abi=1.6` (the version of the header it installs), and a module package declares

```
depends=('omarchy-broadcast-module-abi>=1.N' 'omarchy-broadcast-module-abi<2')
```

where N is the minor version the module was built for: the one its SDK or header defines, which `obc_module_abi_version()` returns, and which belongs in the manifest's `abi_minor`. The packages of this repository's modules carry that range. A newer core then keeps an installed module, and a module that needs a newer minor than the installed core is not installable (and, if it is put in a `modules.d` directory by hand, is refused by the host with its reason). The virtual names the interface's version and not a build, so any package of the core provides it, the `-git` one included. Install the header with the package: `/usr/include/omarchy-broadcast/omarchy_broadcast_module.h`.

### Stage ids and settings groups (the registry)

Stage ids are **one namespace across modules**, and the same id means "another provider of the same function": two modules that declare `denoise` compete to provide it, and the settings of the stage are shared. Settings-group ids are one namespace too, since the group is the unit that clients draw. To keep a module's private id from colliding with an id that the core or another module adds later, ids are divided in two:

- **Every id without the private prefix belongs to the registry**, the ones listed below and any added later. A module that declares one takes the meaning that the registry gives it.
- **A stage or a group of a module's own has the vendor form**: a stage `x-<vendor>-<name>` (an audio stage `mic-x-<vendor>-<name>`) and a settings group `x_<vendor>_<name>`. `<vendor>` and `<name>` are non-empty, and the whole id keeps the grammar of its kind (a stage id is at most 32 bytes). The example uses the stage `x-example-hello`, the group `x_example_hello` and the settings `effect.x_example_hello.*`.

The host loads a module that declares an id of neither form, and warns in the module's log; the id may take another meaning in a later release.

| Well-known stage id | Kind | Meaning |
|---|---|---|
| `background` | video, `matte` | The cut-out of the person with blur or replacement; see [The background stage](#the-background-stage) |
| `enhance` | video, `pre-matte` | The ingest's upscaler |
| `frame-generation` | video, `post-composite` | The picture between two camera frames ([The frame-generation stage](#the-frame-generation-stage)) |
| `denoise` | video, `pre-matte` | Video noise removal |
| `relight` | video, `post-matte` | Relighting of the person |
| `hdr` | video, `post-composite` | The HDR look |
| `eye-contact` | video, `pre-matte` | Gaze redirection |
| `auto-frame` | video, `pre-matte` | Framing that follows the face |
| `mic-denoise`, `mic-dereverb`, `mic-enhance`, `mic-focus`, `mic-bandwidth` | audio | Noise removal, room echo removal, voice enhancement, speaker focus, bandwidth restoration ([The audio modules in this repository](#the-audio-modules-in-this-repository)) |

The well-known settings groups: for a video module `background` (the daemon's own, which a module cannot declare), `cutout`, `video`, `lighting`, `eye_contact`, `auto_frame` and `onnx`; for an audio module `input` (the daemon's own), `noise`, `echo`, `focus`, `bandwidth`, `voice` and `cpu`. Two providers of a well-known stage that describe the same group share its switch and its strength, as the two providers of `mic-denoise` share the microphone's `noise` group; a stage of a module's own has a group of its own.

**Reserved names.** Two more namespaces belong to the host:

- **Configuration keys.** Every key that starts with `_` in `[module."<id>"]` is the host's. The host defines `_host` ([Module configuration](#module-configuration)); a key of that form that it does not define is not passed to the module, with a warning.
- **Status value names.** A name that starts with `obc.` is the host's, for values the host may define later; a value of that name that a module reports is dropped ([Status values](#status-values-abi-13)).

The header has the prefixes as constants (`OBC_PRIVATE_STAGE_PREFIX`, `OBC_PRIVATE_GROUP_PREFIX`, `OBC_RESERVED_CONFIG_PREFIX`, `OBC_HOST_CONFIG_KEY`, `OBC_RESERVED_VALUE_PREFIX`). The two reservations of names are enforced for a module that declares ABI 1.6 or later; for a module of an older minor, which could not know them, the host still passes the key and keeps the value, as before, and warns.

### The background stage

The provider of the well-known `background` stage **makes the matte and composites the picture itself**: the host gives it the camera frame and takes the finished picture back. The daemon chooses the mode and the strength; the provider does the blending. The header names what the host sends it:

| Name (header constant) | Kind | Meaning |
|---|---|---|
| `mode` (`OBC_BACKGROUND_PARAM_MODE`) | load-time string | `blur` (`OBC_BACKGROUND_MODE_BLUR`) or `replace` (`OBC_BACKGROUND_MODE_REPLACE`). A chain without the effect has no `background` stage |
| `blur_strength` (`OBC_BACKGROUND_PARAM_BLUR_STRENGTH`) | live float, 0.0 to 1.0 | Sent with every frame while the chain has the stage, under either mode; under replace a module ignores it |

Under replace, the background is the chain request's `background` image (a host BGR frame of the output size) or, from ABI 1.4, the per-frame background ([The per-frame background](#the-per-frame-background-abi-14)). The stage's own load-time parameters (the Maxine module's `chair`) arrive beside these, as for any stage; a matte stage never fails open ([Failure semantics](#failure-semantics)).

This is the contract of the stage **for the life of major version 1**. A later minor version may add, beside it, an optional contract in which a module returns the matte only and the host composites; a module that does not declare it is not affected, and a module that composites itself keeps working unchanged. Only module-side compositing is defined in major version 1: a matte-only contract has no module to use it yet and would be frozen without a user, and one that replaced module-side compositing would be ABI 2.

### Writing a module in C

A module in any language that exports C symbols implements the header, [`omarchy_broadcast_module.h`](../crates/obc-module-abi/include/omarchy_broadcast_module.h), and needs nothing else. The example [`examples/module-hello-c`](../examples/module-hello-c/README.md) is a complete video module in C (one stage, a warm tint on NV12 host frames); the host's tests build it with `cc` from the header alone and load it. It shows the rules in a few lines each:

- export `obc_module_abi_version` (returning `OBC_MODULE_ABI_VERSION`) and `obc_module_entry`, which refuses a host of another major version or with an `obc_host` smaller than `OBC_HOST_SIZE_1_0`;
- fill a vtable of `struct_size` bytes (`OBC_MODULE_VTABLE_SIZE_1_4` covers every entry of ABI 1.x so far) and leave the entries it does not need null;
- check every `struct_size` it is given against a base size (`OBC_CHAIN_REQUEST_SIZE_1_0`, `OBC_FRAME_SIZE_1_0`), and write into the load report only the fields that fit in the `struct_size` the host set (`OBC_LOAD_REPORT_SIZE_1_0`);
- answer a constant it does not handle with `OBC_ERR_UNSUPPORTED`, and ignore a parameter it does not know;
- keep every string it hands back alive until the next call on the same object.

Build it from the root of this repository:

```sh
cc -std=c11 -shared -fPIC -I crates/obc-module-abi/include -o libobc_module_hello_c.so examples/module-hello-c/hello.c
```

Its README has the full command. The manifest of a C module declares `abi_minor` for the minor version of the header it was built against.

### Status values (ABI 1.3)

A stage can report **named numeric values** about the frame it has just processed, for the daemon to show: auto-frame's current zoom, for one. They are read-outs, not settings: the host never sends one back, and a client shows them next to the setting they relate to. The daemon publishes them as the `StageValues` property ([dbus-api.md](dbus-api.md#stage-values-revision-16)) and the CLI prints them ([cli-json.md](cli-json.md#stage-values)); the daemon's status could otherwise show only what the host measures itself, while a value such as the crop an auto-framing stage chose exists only inside the module.

```c
typedef struct obc_status_value {
    const char *stage;   /* the stage id the value belongs to, NUL-terminated */
    const char *name;    /* the value's name, unique within the stage, NUL-terminated */
    double      value;
    uint32_t    reserved0;   /* zero */
    uint32_t    reserved1;   /* zero */
} obc_status_value;          /* 32 bytes, 8-byte aligned */
```

`obc_status instance_status_values(obc_instance *instance, const obc_status_value **out, size_t *n)` returns, through `out` and `n`, the values after the last processed frame. The contract:

- **When.** The host calls it after a frame, and never while a call on the same instance runs (calls are serialised and have no thread affinity, [Threading and CUDA rules](#threading-and-cuda-rules)), so it must be cheap: it reads what `instance_process` left, and does no work of its own.
- **Ownership.** The array and the strings it points to belong to the instance and stay valid until the next call on that instance. The host copies them at once. With no values, set `*out` to null and `*n` to 0.
- **What it reports.** The values of the stages that are loaded and have processed a frame. A stage that is not loaded reports nothing. A repeated frame (a repeat, ABI 1.1, see above) should leave a value as it was, if the stage did no work on it.
- **Return.** `OBC_OK`. Any other status is read as "no values" for that call.

**What the host accepts.** The host does not trust the list, because a bad one must not harm the daemon or its status:

| Input | Result |
|---|---|
| A non-`OBC_OK` status, a null array or an array not aligned for `obc_status_value` | No values |
| More than 64 entries, or none (`*n` of 0) | No values: a long list is ignored whole, not cut to its first 64 |
| A name or stage longer than 64 bytes | Cut to 64 bytes at a character boundary |
| An empty or null name, or a value that is `NaN` or infinite | That entry is left out |
| A stage that is not a valid stage id, or null | That entry is left out |
| A name that starts with `obc.` (a value of a module that declares ABI 1.6 or later) | That entry is left out: the prefix is the host's. A module of an older minor keeps such a value, with a warning |

The same bounds (64 entries, names cut to 64 bytes, a null or misaligned array) now apply to what `instance_timings` returns. A value reaches a client only if the stage is also one that is ready: the daemon's elements keep the values of the stages that ran on the last frame and drop those of a stage that has been switched off or has failed, and of a stage the chain never loaded.

**The Rust SDK.** Implement `Instance::status_values(&self) -> Vec<StatusValue>`, with `StatusValue::new(stage, name, value)`; it defaults to none, and the macro keeps the strings alive until the next call. The Maxine AR module is the model: while auto-frame is loaded it reports `("auto-frame", "zoom", frame height / crop height)`, where `1.00` is the full frame.

### The per-frame background (ABI 1.4)

A chain that replaces the background (its `background` stage under the replace mode) is given its background image once, at load time, as a host BGR frame in the chain request. A **video background** needs a new background for every frame, and ABI 1.4 gives the module one through the nullable vtable entry `instance_set_background`. A module that does not set the entry shows a video as its poster ([architecture.md](architecture.md#video-backgrounds)).

```c
obc_status (*instance_set_background)(struct obc_instance *instance,
                                      const struct obc_frame *background);
```

The contract:

- **When.** The host calls it between two `instance_process` calls, never while another call on the same instance runs, only on a loaded instance whose chain includes `background` under replace.
- **The frame.** `background` is an NV12 frame (`OBC_FORMAT_NV12`) of the chain's **output size and colour description** (matrix, chroma siting and range) and of the chain's **frame kind**: CUDA device memory on the chain's context and stream for a CUDA chain, host memory for a host chain (in system memory the chain is a host chain, with both CUDA handles null and no context current). The host crops the video to fill the frame, as it does a still, so the module never scales. The module copies or converts what it keeps.
- **Validity.** A host frame is valid only during the call. A CUDA frame is valid **in stream order**: the module may enqueue reads of it on the chain's stream during the call, and the host keeps the memory alive and unwritten until the work so enqueued has completed. The module must not read it after the call on any other stream, and must not keep the pointer. A module that needs the picture later (to composite a frame enqueued after the call) therefore enqueues a copy or a conversion into memory of its own during the call, which is what both modules in this repository do.
- **What it replaces.** The background of the following `instance_process` calls, until the next call. It is the per-frame counterpart of the load-time image, not a change of the chain: nothing is reloaded and no model is touched.
- **A load resets it.** A null `background` returns to the load-time background image, and so does **every load of the chain**, a switch (a load that keeps the frames) as well as a full reload: a frame given before the load is no longer in force after it, the load-time image of that load is, and the host gives a frame again when it wants one. A module that kept its per-frame background across a load would show the previous video behind a new still.
- **Errors.** `OBC_ERR_INVALID` for a frame of the wrong size, format or kind (the host checks this too and does not call the module with such a frame, but a module that is called by another host must). Any failure leaves the previous background in force: a module writes the new picture where the composite does not read, and makes it the one read only when the work is enqueued whole.
- **The version rule.** The host never calls the entry of a library built for an ABI older than 1.4, nor one that leaves it null. It then shows the video as a still, the poster or the first frame given as the load-time image, and reports that: `<module> shows the video background as a still` ([dbus-api.md](dbus-api.md#video-backgrounds-revision-18)).

**The Rust SDK.** Set `Instance::LIVE_BACKGROUND` to `true` and implement `Instance::set_background(&mut self, frame: Option<&Frame>) -> Result<(), Error>`; only then does `export_module!` fill the vtable entry (with the default, `false`, it stays null, so a module that does not implement the method is never called and the host knows to show the still). The SDK checks that the frame is NV12 of the loaded chain's kind and output size and refuses any other with `ErrorKind::Invalid` before the module runs. The default of `set_background` fails with `ErrorKind::Unsupported`.

**The modules in this repository.** Both take it. The Maxine module converts each frame, on the chain's stream, from NV12 into one of two BGR images of its own with the conversion it uses for the camera frames, so the load-time image stays for a return to it, a failed conversion leaves the picture in force, and stream order alone keeps a composite that is already queued from reading a half-written image. The ONNX module copies each frame into a buffer it keeps for the chain's life (a host frame directly; a CUDA frame through the frame buffer to the host, on the chain's stream, because its composite runs on the CPU either way).

## Settings

`describe_settings` returns a JSON document in the model of the camera settings ([dbus-api.md](dbus-api.md#camera-settings)):

```json
{"groups":[{"id":"x_example_hello","label":"Hello module","settings":[
  {"id":"effect.x_example_hello.enabled","label":"Warm tint","type":"bool","default":false,
   "stage":"x-example-hello","role":"enable","hint":"switch"},
  {"id":"effect.x_example_hello.strength","label":"Strength","type":"int","min":0,"max":100,"step":5,
   "default":50,"unit":"%","stage":"x-example-hello","key":"strength","apply":"live",
   "enabled_when":[["effect.x_example_hello.enabled","eq",true]],
   "disabled_reason":"Turn on the warm tint to change this","hint":"slider"}]}]}
```

- A **group** has an `id` (one setting-id segment: lower-case letters, digits and `_`; not `background`, which is the daemon's, and for an audio module not `input`, which is the daemon's microphone group; a group of a module's own has the vendor form `x_<vendor>_<name>`, and another id outside the registry keeps its group with a warning in the module's log), a `label` and, optionally, a `surface`: where clients show the group. `mic` is the microphone's page (an audio module's groups should carry it); a client that does not know a surface shows the group with the effects, and an invalid one is left out with a log line.
- A **setting** has an `id` of the form `effect.<group id>.<name>` (`mic.<group id>.<name>` for an audio module, and a setting of the other prefix is dropped; the grammar `^[a-z0-9]+(\.[a-z0-9_]+){1,3}$`, at most 48 bytes, unique), a `label` (at most 48 bytes), and a `type`: `bool` (`default`), `int` (`min`, `max`, `step`, `default`, optional `unit`) or `enum` (`options` as `[value id, label]` pairs, at most 16, and `default`, one of them). `hint` (`switch`, `slider`, `segmented`) is a suggestion.
- A setting binds to a stage with `stage`, which must be a stage of the manifest. `"role": "enable"` (a `bool`) is the stage's on/off switch: a stage with no such setting runs whenever it is provided, with one exception: `frame-generation` has no switch and is not given one (a setting with `"role": "enable"` bound to it is left out of the tree), because the daemon decides whether it runs, from the output rate (see [The frame-generation stage](#the-frame-generation-stage)). `"key"` makes the setting a **parameter** of the stage, passed to the module under that key: at load time, in `obc_chain_request`, or per frame, in `instance_process`, as `apply` says. `apply` is `live` (per frame, no reload), `reload` (the default for a bound key; the chain is loaded again, a switch that may reuse models) or `restart` (kept, applied when the camera next starts). A parameter's value arrives as an `obc_value`: a boolean, an integer, or a string (an `enum`'s option id).
- `enabled_when` is a list of `[setting id, op, value]` conditions (`eq`, `ne`, `in`), all of which must hold for the setting to be enabled, and `disabled_reason` is what to say while one does not. The daemon applies them; a client evaluates nothing.
- The daemon greys a stage's switch and its parameters, with the stage's reason, while the probe says the stage cannot run ("Not installed").
- **`action` is not a module setting type.** An action has no value to deliver and reaches no module, so a module's setting of type `action` is dropped, with the reason in the log, like any invalid setting; the module loads. The daemon's own camera and microphone actions are unaffected. A later minor version may add a type with a delivery entry; a host drops a setting whose type the module's declared minor does not define.
- At most 64 settings in all. A setting or a group that is malformed or breaks a rule is dropped with a line in the log, and the module and the rest of its description stay.

The values the user sets are kept by setting id in the state file, and reach the module as parameters (those of an audio module reach the worker in the audio settings snapshot). The daemon publishes the settings as `scope` `global` settings of the tree, after the camera's, so that `omarchy-broadcast settings`, `SetSetting` and the bar plugin show and write them.

## Threading and CUDA rules

- **Calls are serialised, and have no thread affinity.** The host serialises the calls on one module object, and the calls on each instance: two calls on one instance never run at the same time. They may come from different threads, one after the other, so a module assumes no thread affinity and keeps no per-instance state in thread-local storage. A chain loads on a worker thread (a load may block for seconds, and never runs during a frame) and a **second instance may load beside the running one**; the frames run on the streaming thread. Calls on the module object and on its instances, and on different instances, may run at the same time on different threads, so state they share needs its own locking (an `Arc` in the SDK's terms: `Module` is `Send + Sync`, `Instance` is `Send`). The log callback may be called from any thread.
- **A module may run threads of its own** (a worker, a timer, a runtime's pool). The module stops (joins) the threads of an instance before `instance_destroy` returns, and those of the module object before `destroy` returns; no code of the module runs after that, except what a later call starts. From such a thread the only host callback it may call is `log`, unless the header says otherwise of a callback.
- **The process is not the module's.** A module assumes nothing of the process that hosts it (the daemon, the audio worker, or a host of its own in a later release): it sets no signal handler, changes no environment variable, working directory or locale, does not fork, and does not exit the process.
- A string a module returns is owned by the module and stays valid until the next call on the same object.
- For **CUDA frames** the host makes the context of `obc_cuda.context` current on the calling thread around every instance call, and the module enqueues its work on `obc_cuda.stream` (a null stream with a context is the legacy default stream). **A module never creates a CUDA context for those frames, never makes one current beyond the call, and never touches the frames' context but to enqueue work on the chain's stream.** The context is the one GStreamer's CUDA elements share; releasing it at teardown leaves no CUDA context in the process, which is what lets the GPU be parked at idle ([Lifecycle](architecture.md#3-lifecycle)). Do not initialise CUDA in `init`, `probe` or `check`.
- **The primary-context rule.** ONNX Runtime's CUDA and TensorRT engines (and other runtimes) run through the CUDA runtime, which activates the device's *primary* context for as long as a session of theirs is alive, so a module that offers such an engine cannot avoid that context. A module may let its runtime hold the primary context while a GPU session of its own lives, **on these terms**, which apply to every module and not to one: it counts its GPU sessions and, when the last ends, resets the device's primary context (`cuDevicePrimaryCtxReset`); it restores the context that was current on the calling thread before it returns; it never touches the frames' context (if the primary context is the caller's current context, or holds a live chain's frames, the reset is skipped with a warning); and it makes no GPU call in `init`, `probe` or `check`, nor at all under `_host.cuda = false`. No CUDA context of the module's then remains at idle, so parking ([Lifecycle](architecture.md#3-lifecycle)) is unaffected after an unload. The open-model cut-out module (`br.com.dias.onnx-cutout`) is the model.
- A `host` chain gets CUDA frames through a page-locked host copy that the host makes: you declare the memory your stage takes, and the host converts only at the boundary of your chain.
- **In system memory there is no CUDA at all.** The video pipeline has no context, no stream and no CUDA frame, the chain request has null CUDA handles, and the host calls the instance with no context current. The module is told so by `_host.cuda = false` ([Module configuration](#module-configuration)) and **must not create or use a CUDA context, call the CUDA runtime or the driver, or start an engine that does**, because a CUDA call would wake a GPU that the user chose to keep asleep, and in a process that was parked earlier it would block until an unpark that never comes ([Lifecycle](architecture.md#3-lifecycle)). The primary-context rule does not arise there: a module makes no GPU session. A stage that needs CUDA frames declares `frames = ["cuda"]` and is unavailable in that memory, with the reason above.
- The rules of the audio kind differ in three places (the processing thread is not real time, a module may create its own GPU context in its load and unload, and a load, a process and an unload of one instance may run on different threads): see [Audio modules](#audio-modules).
- A frame is processed in place for an in-place chain; the output clock may push the same frame twice, so the host copies a frame before a stage writes it (and, when a chain fails halfway, restores the input before running again).

## Failure semantics

- **A module that cannot load is not an error of the daemon.** A broken manifest, a missing or unloadable library, another ABI major version, a library built for a newer minor version, a failing `init`, probe or `check`, a conflict with another enabled module, a module it needs that is not enabled, each makes the module `unavailable` with the reason; the daemon runs, and the module list says why.
- **A stage that fails alone fails open**, unless it is a matte stage: return `OBC_ERR_STAGE` (the SDK's `Error::stage`) naming the stage. From `instance_load` nothing of the request is in force (a first load or a reload leaves the instance unloaded; a switch keeps the previous chain, less the failed stage when it could not be left as it was); from `instance_process` the module has dropped the stage, the output of that frame is not written, and the host runs the frame again through the chain without it. The picture is never blocked by a stage that hides nothing.
- **A matte never fails open.** A failure of the `background` stage, or of any stage in the `matte` slot, is taken as a failure of the whole chain, and the instance is unloaded, so that no frame runs without its cut-out; the effect then follows the privacy setting (`[effect] privacy`, off by default from daemon 0.48.0, and its `privacy_mode`): with privacy on, which is what the user chose when they asked to hide the room behind them, the feed keeps streaming, its frames black (or the away image or the last frame, by `privacy_mode`) until the effect runs again; with it off the camera image shows; the daemon's `Warning` property says why either way; the camera is not closed. This governs a module that provides the stage and fails to load or run, and, from revision 14, every other case in which no module can run a requested background effect (`blur` or `replace`): none installed or enabled, the module disabled, refused by a conflict, a probe or an open that failed, the crash ledger, a pin that falls back to nothing. The output always holds the effect element, and with no backend behind it the element fails the effect: with privacy on, `black` gives black frames, `away` the away image and `freeze` the effect's last frame, with privacy off the camera image, and the `Warning` reads `effect blur unavailable: <module>: <reason>; the video is black until it can run` (privacy on, `black`) or `...; the camera image is shown without the effect (effect.privacy is off)`. The camera stays open and the stream stays active. In a running session, when the module behind the effect can no longer run it (it is disabled, its probe fails again, or it cannot be opened again at a reload), the element drops that module's instance and shows black or the image from the next frame; when a module can run the effect again it returns with no restart, reading as loading, not as an error, while it loads. Under `none` the effect is dropped and the camera image shows. Before revision 14 the background effect with no provider was dropped whatever `on_failure` said. Any other selected stage that no module provides is still dropped: the host does not run it, the feed keeps streaming with the stages that still have a provider, and the daemon's `Warning` property names the stage and the module's reason ([dbus-api.md](dbus-api.md#video-effects)). For a stage of your own module that holds once the daemon has read your settings (the `role = "enable"` switch) since it started.
- **A failed switch keeps the previous chain** when the frames of the request are those of the loaded chain: it keeps running as it was. In the daemon this is a **refused switch** when the effect is kept: the chain keeps processing every frame, the effect stays ready (never black or the away image), and `Warning` says `<module>: <reason>; the previous setting keeps running`. The stored setting is not rewritten, and the refused value is tried again only when the user changes it or applies the settings again; a refused load-time parameter still stored is loaded again at the next camera start. A failed first load, a failed load at another geometry (a reload) and a load that panicked leave the instance unloaded; the host then calls `instance_unload`, which must be safe after any failure.
- **A panic** in a module written with the SDK is caught at the boundary and returned as `OBC_ERR_PANIC`; the host unloads the instance.
- **A crash of native code** (a segmentation fault, an `abort`) takes the daemon down with it: modules run in the daemon's process. (An audio module runs in the audio worker, which a crash takes down instead: the microphone then passes the raw audio until the worker is back.) The unit's restart and the loopback's timeout frames keep applications that hold the camera alive, and the crash ledger below keeps a module from doing it again and again.
- **A module is trusted code.** A module is native code that runs inside the daemon's process. It can read the process's memory and files, and there is no signing and no sandbox; install only modules you trust. The crash ledger only bounds the damage of a module that crashes on every start.

## The crash ledger

The daemon keeps a small JSON file, `modules-crash.json`, in its state directory (`$XDG_STATE_HOME/omarchy-broadcast/`). While a module's code runs a pipeline or a probe, the file holds a marker for the module; a clean stop clears it. At each start the daemon counts, per module, the consecutive starts that found its marker (a start that finds none resets the count). A module that was running when the daemon went down **twice in a row** is disabled, with the reason `Disabled after crashing the daemon twice in a row`, until the user enables it again, which forgives it:

```sh
omarchy-broadcast modules enable ID
```

A session marks only the modules that run in it: the effect module while the background effect is on or one of its stages is requested, the enhancer while enhancement is on, and another module's chain while one of its stages is. A damaged ledger file counts as empty.

The audio worker keeps the same kind of ledger for audio modules in its own file, `audio-modules-crash.json`, in the same directory, so that a strike of an audio module never counts against the camera and the other way round. A call into an audio module that does not return within two seconds aborts the worker, and leaves the marker: it counts as a crash. The same `modules enable` command forgives an audio module too.

## Audio modules

ABI 1.2 adds the **`audio-filter` kind**: modules that process the microphone, such as noise removal, room echo removal (dereverberation) and voice enhancement. The design is in [The microphone](architecture.md#the-microphone-host-and-worker); for the user's side, see [setup/microphone.md](setup/microphone.md).

**Where an audio module runs.** Not in the daemon. The microphone has a worker process, `omarchy-broadcast-audiod`, which discovers modules in the same `modules.d` directories, opens only the `audio-filter` ones and runs them in-process through the C ABI. The daemon (and `omarchy-broadcast modules`) lists an audio module from its manifest and the worker's report, and never opens its library; the worker never opens a video module. Install an audio module as a video one: its manifest and its library in a `modules.d` directory.

```toml
abi = 1
abi_minor = 6
id = "org.example.hello-audio"
name = "Hello voice"
version = "0.1.0"
licence = "MIT OR Apache-2.0"
kind = "audio-filter"
library = "libobc_module_hello_audio.so"

[[stage]]
id = "mic-enhance"
slot = "shape"
rank = 100
device = "cpu"
frames = ["host"]
rates = [48000]
frame_samples = [480]
channels = 1
```

**Stages.** An audio stage id starts with `mic-` in both directions: an audio module's ids must, a video module's must not, so the one namespace of stage ids needs no other rule. The ids in use are `mic-bandwidth`, `mic-denoise`, `mic-dereverb`, `mic-enhance` and `mic-focus`. The slots are `clean` (taking away what is not the voice) and then `shape` (working on the voice that is left); a stage that needs another to come first lists it in `requires` as for video. `rank`, `device`, the `quality` and `battery` profiles and pins choose the provider of a stage exactly as for video; the order of the audio stages is fixed by slot, `position` and id (there is no `SetStageOrder` for audio). Consecutive stages of one module reach it as one request, so a module that has one model for two stages (noise and room echo together) can run them as one. A stage that declares a `conflicts` entry with another stage that is on turns that one off, unless `[mic] force_conflicts` is set.

**Block format.** `rates`, `frame_samples` and `channels` declare what a stage accepts. The microphone host's format is fixed: mono, 48 kHz, blocks of 480 samples (10 ms). The worker runs a chain only if the stages of its request all list 48000 among their `rates`, 480 among their `frame_samples` and one channel; otherwise the stages fail to load with the reason "The module does not take 48 kHz mono blocks of 480 samples". Declaring other rates or sizes is allowed, but they do not reach the module in this version. Samples are planar 32-bit floats in [-1, 1]; the buffers belong to the host and are valid only during the call.

**Settings.** Ids are `mic.<group>.<name>` and the group carries `surface: "mic"`; the daemon's own group, `mic.input` (the processing switch, the input device, a USB microphone's hardware gain and the default-microphone actions), cannot be redeclared. The daemon shows one group per id, the active provider's, so two providers of `mic-denoise` that describe the group `noise` with `mic.noise.enabled` and `mic.noise.strength` share one switch and one strength, and the strength is whatever each engine makes of 0 to 100 %. A setting whose `apply` is `live` reaches the module with each block, one that reloads is a switch of the chain.

**The rules of the kind** (also in the header):
- `audio_instance_process` runs on the worker's **processing thread, which is not a real-time thread**, with a **deadline of one block** (480 samples are 10 ms). The worker counts the blocks that miss it; after **50 consecutive overruns** it fails the group of the module **open** (its blocks pass through unchanged from then on) and reports it.
- A module **may create its own GPU context**, but only inside `audio_instance_load` (and release it in `instance_unload`), never in `init`, `probe` or `check`, which must stay cheap and must not wake a suspended GPU. The worker is a process of its own that exits after the microphone has been idle, so the process exit releases what the module held. The video rule (a module never creates a context) does not bind it.
- **A stage that fails fails open.** Return `OBC_ERR_STAGE` naming the stage (the SDK's `Error::stage`): the module has dropped the stage, the output of that block is not written, and the worker passes the block through the rest of the chain. The audio is never silence because of a stage. There is no matte stage in audio.
- **Load, process and unload of one instance may run on different threads**, one after the other and never at the same time: the worker loads a chain on a loader thread, runs it on the processing thread and unloads it on a retire thread, so that a switch never holds a block up. State a module keeps per instance must not be thread-local, and instances of one module are loaded, run and unloaded concurrently.
- A load on a loaded instance that keeps the block format is a **switch** and may reuse its models; the worker warms the new chain up, with the old one still processing, and crossfades one block. A failed switch keeps the previous chain (less the failed stage), a failed first load or reload leaves the instance unloaded.
- `latency_samples` in the load report is the chain's algorithmic delay in samples, lookahead included: the worker adds it to the transport delay and publishes the total as the microphone's latency. A delay that varies with the settings or the input makes the crossfades between the processed and the raw audio audible, so keep it constant.
- A block with `OBC_AUDIO_FLAG_NEW_SOURCE` is the first of a new stream (the previous one's state should not carry over); `OBC_AUDIO_FLAG_DISCONT` follows a gap. `voice_probability` in the output buffer (0 to 1, or -1 when the module has none; the SDK's `AudioBufferMut::set_voice_probability`) is optional.
- A call into a module that does not return within two seconds aborts the worker (the [crash ledger](#the-crash-ledger) counts it as a crash), and an audio module that crashes the worker twice in a row is disabled with the reason.

**The SDK.** An audio module implements `Module`, `Instance` (only `unload`, with the macro `no_video_stages!()` standing for the video entries) and `AudioInstance`:

```rust
impl obc_module_sdk::Instance for MyInstance {
    obc_module_sdk::no_video_stages!();
    fn unload(&mut self) { /* release what the load made */ }
}

impl obc_module_sdk::AudioInstance for MyInstance {
    fn load_audio(&mut self, req: &AudioRequest<'_>) -> Result<AudioLoadReport, Error> { /* ... */ }
    fn process_audio(&mut self, input: &AudioBuffer<'_>, output: &mut AudioBufferMut<'_>,
                     live: &[Param<'_>]) -> Result<(), Error> { /* write every sample of the output */ }
}

obc_module_sdk::export_module!(MyModule, audio);
```

The `audio` form of the macro fills the vtable's audio entries and leaves a video module's alone; a panic is caught at the boundary as for video. [`examples/module-hello-audio`](../examples/module-hello-audio/README.md) is a complete module: one stage, `mic-enhance` in `shape`, a gain and an optional test tone, with its settings in the `mic` group `x_example_hello_audio`. It is built outside the workspace (`scripts/check-all.sh` builds it with `--locked`), installed in a `modules.d` directory like any module, and its README lists the steps. A module in another language implements the header directly: set `audio_instance_load` and `audio_instance_process`, report ABI 1.2 or later, and leave the video entries null (a module that also supports hosts of ABI 1.5 or older sets them to stubs that fail with `OBC_ERR_UNSUPPORTED`, because those hosts require them: see [The vtable](#the-abi-and-the-sdk)).

## The audio modules in this repository

Two `audio-filter` modules are built here and installed by the Arch packages (the CPU voice module in the core, the Maxine audio module in `omarchy-broadcast-maxine-afx-git`) and by the `--audio` option of `scripts/install-user.sh`, each a library with a manifest in a `modules.d` directory. Both provide stages of the microphone and take 48 kHz mono blocks of 480 samples. The user's side is [setup/microphone.md](setup/microphone.md).

| Module | Id | Stages (slot) | Device | Rank | Needs |
|---|---|---|---|---|---|
| CPU voice, `crates/obc-module-voice-cpu` | `br.com.dias.voice-cpu` | `mic-denoise` (clean) | `cpu` | 200 | Nothing: its engines, DeepFilterNet and RNNoise, are built in |
| NVIDIA Maxine audio effects, `crates/obc-module-afx` | `br.com.dias.maxine-afx` | `mic-focus` (clean, position 25), `mic-dereverb` (clean, position 50), `mic-denoise` (clean), `mic-bandwidth` (clean, position 150), `mic-enhance` (shape) | `nvidia-gpu` | 300, above the CPU module | An NVIDIA GPU and the Maxine Audio Effects SDK, which is NVIDIA's, under its licence, and is not part of omarchy-broadcast; `mic-focus` also needs NVIDIA's speaker focus feature, and `mic-bandwidth` NVIDIA's superres feature; neither is part of the Audio Effects SDK's standard set, and speaker focus is an NVIDIA Early Access feature |

Settings ids, all on the `mic` surface:

| Module | Settings |
|---|---|
| CPU voice | `mic.noise.enabled`, `mic.noise.strength` (shared with the GPU module), `mic.cpu.engine` (`deepfilternet` or `rnnoise`, reloads) |
| Maxine audio effects | `mic.noise.enabled`, `mic.noise.strength` (live), `mic.echo.enabled`, `mic.echo.strength` (live), `mic.voice.enabled`, `mic.voice.profile` (reloads), `mic.focus.enabled` (no strength), `mic.bandwidth.enabled` (no strength) |

Two providers of `mic-denoise` describe the same `noise` group, so the panel shows one switch and one strength, and each engine maps the strength its own way. The Maxine module is the only provider of `mic-dereverb`, `mic-enhance`, `mic-focus` and `mic-bandwidth`; its `mic-enhance` (studio voice) declares `conflicts = ["mic-denoise", "mic-dereverb"]`, so turning it on turns those two off unless `[mic] force_conflicts` is set. `mic-focus` (speaker focus: keeps the main speaker and removes other voices and background talk) removes neither noise nor echo and declares no conflict, so it runs together with every other stage, and with studio voice too. It runs first in the chain (position 25, before `mic-dereverb` at 50 and `mic-denoise`), as its own effect and not chained with noise removal; it has a switch and no strength. `mic-bandwidth` (headset bandwidth restoration: rebuilds the high band of a narrowband microphone, such as a Bluetooth headset in its 16 kHz mode) also declares no conflict, and it runs after noise and echo removal (position 150, after `mic-dereverb` at 50 and `mic-denoise` at 100), the order NVIDIA's own chains use, so that it extends a signal that has already been cleaned; it has a switch (group `bandwidth`) and no strength.

How the Maxine module behaves, for anyone writing a module over a vendor SDK:

- **The SDK is opened at run time**, never linked, so the library loads on a machine without it. `probe` reads only the SDK tree, `/proc/driver/nvidia` and the process's library search path; `check` opens the SDK's core library, and neither creates a CUDA context. The GPU is first used in `audio_instance_load`, and every effect is destroyed at unload.
- **The search path is a requirement.** The SDK opens an effect's feature library by a name that only the dynamic linker's search path resolves, and that path is read once, when the process starts, so a module cannot repair it. The module therefore requires `LD_LIBRARY_PATH` to lead to the feature libraries of the root in use, and without it the stages are unavailable with a reason that names the SDK package (the worker applies that package's environment to itself when it starts). The module never changes the worker's working directory.
- **A combined effect, with a fallback.** When one request holds both `mic-dereverb` and `mic-denoise`, the module runs them as one combined model where the SDK has it, and as two effects one after the other if it is not installed or fails to load. The combined model's one strength is noise removal's.
- **Speaker focus and headset bandwidth are optional features.** Neither is part of the Audio Effects SDK's standard set: speaker focus is an NVIDIA Early Access feature. Until a feature's library and models are installed under `<root>/features/speaker_focus/{lib,models/sm_<NN>}` or `<root>/features/superres/{lib,models/sm_<NN>}` (`<NN>` the GPU's compute capability), its stage (`mic-focus`, `mic-bandwidth`) is listed unavailable and its switch is greyed with the reason "Models not installed", whichever part is missing. The selector, library and model names the module uses follow the layout of NVIDIA's feature download and are unverified until a real installation has been tried; for superres the model looked for is the 16 kHz to 48 kHz one.
- **Bandwidth restoration runs at 16 kHz.** The chain works at 48 kHz, so for `mic-bandwidth` the module decimates the audio to 16 kHz with a linear-phase low-pass filter and the superres effect rebuilds 48 kHz from it. The filter's delay is counted in the chain's reported delay, like each effect's own.
- **Strengths are live.** Each strength arrives with every block and mixes the effect's output with its input delayed by the effect's own delay, so a change affects the mix only and the chain's delay stays what it reported. Studio voice has no strength; its profile reloads the chain.
- **The delay is measured when the chain loads**, on the loaded effects, and reported as `latency_samples`; a stage whose delay cannot be measured fails to load.
- **A failed effect becomes a delay line.** An effect that fails on a block is replaced by its input delayed by the same amount, so the chain's delay does not change and the other stages keep running; it is destroyed with the chain at unload, not on the processing thread.
- **Some effects keep GPU memory until the process ends.** This is why the worker exits after the microphone has been idle, which releases it.

**Where the SDK is found (the difference from the video modules).** The module's root is, in this order: `sdk_root` in `[module."br.com.dias.maxine-afx"]`, then the `MAXINE_AFX_ROOT` environment variable, then `/opt/nvidia/maxine/afx`. The Maxine video modules take the environment variable first and then the table (`MAXINE_VFX_ROOT`, `MAXINE_AR_ROOT`). The audio module reverses it so that a `MAXINE_AFX_ROOT` in the worker's environment does not hide a root the user configured (the worker never applies the root of the package's environment file in any case). The table takes `sdk_root` and the daemon's `_host` key, and no other. No systemd drop-in is needed: the worker finds the Audio Effects SDK's environment file and shim at start and applies them to itself, `LD_LIBRARY_PATH` included ([setup/microphone.md](setup/microphone.md#the-workers-sdk-environment)).

## Reserved names, in one place

| Kind | Rule |
|---|---|
| Module id | `^[a-z0-9]+(\.[a-z0-9-]+)+$`, at most 64 bytes |
| Stage id | `^[a-z][a-z0-9-]{0,31}$`; not `status`, `effect`, `reason`, `module`, or any name ending in `-reason`. `background`, `enhance` and `frame-generation` have a meaning to the daemon. An audio stage id starts with `mic-`, a video stage id does not. An id without the private prefix is the registry's ([Stage ids and settings groups](#stage-ids-and-settings-groups-the-registry)); a stage of a module's own is `x-<vendor>-<name>` (`mic-x-<vendor>-<name>` for audio) |
| Module lists | `conflicts` and `[requires] modules` hold module ids (the grammar above), each once, never the module's own id; a manifest that breaks this is invalid |
| Slot | video: `pre-matte`, `matte`, `post-matte`, `post-composite`; audio: `clean`, `shape` |
| Device | `nvidia-gpu`, `gpu`, `npu`, `cpu` |
| Frame kind | `cuda`, `host`; `dmabuf` reserved; an audio stage takes `host` |
| Audio block | `OBC_AUDIO_F32_PLANAR` (planar 32-bit float), at most 8 channels; the stages in use take 48 kHz mono in blocks of 480 samples |
| Group `surface` | `mic` (the microphone's settings) |
| Group id | One setting-id segment (`[a-z0-9_]`). The well-known groups are the registry's; a group of a module's own is `x_<vendor>_<name>` |
| Format | `nv12`, `i420`, `bgra`, `rgba`, `rgb-f32-planar` |
| Setting id | `effect.<group>.<name>` (video) or `mic.<group>.<name>` (audio), at most 48 bytes; the groups `background` (video) and `input` (audio) are the daemon's |
| Configuration | `[module."<id>"]` for the module's table; every key starting with `_` in it is the daemon's (`_host` is the one defined). `[modules.forced]` takes module ids (video modules) with `true` |
| Status value name | A name that starts with `obc.` is the host's; a module's value of that name is dropped |
| Manifest `abi_minor` | Optional, the lowest ABI minor version the module needs (0 when absent) |
| Background parameters | `mode` (`blur` or `replace`) and `blur_strength` (0 to 1) of the `background` stage ([The background stage](#the-background-stage)) |
| Environment | `OBC_MODULE_PATH`: extra manifest directories, colon-separated |

## Testing a module

- Test the logic with the SDK's types, as `examples/module-hello/src/lib.rs` does (its unit tests build frames and call the instance directly), and the exported library through the C ABI: `crates/obc-module-sdk/tests/vtable.rs` calls the vtable as a host does.
- A module in C is tested through the same host: `crates/obc-module-host/tests/modules.rs` builds `examples/module-hello-c` with `cc` from the header alone, loads it and drives a host frame through it.
- To run a module in the real host without a camera, `crates/obc-module-host/tests/modules.rs` builds the example and a fixture module, loads them with `dlopen` as the daemon does and drives the registry and the adapters; the fixture's features export a library of another major version, of a newer minor version, and one that refuses the host, and the features `abi-1-0` to `abi-1-5` export a module that declares that older minor version, which the host must hand nothing newer than it defines.
- The commands, and the tests that need a GPU or the Maxine SDK, are in [testing.md](testing.md#modules).
- For an audio module, `examples/module-hello-audio` has unit tests of its gain and tone, `crates/obc-module-sdk/tests/audio_vtable.rs` drives an exported audio module through the vtable with raw ABI structures, and `crates/obc-module-host/tests/modules.rs` loads the audio example and an audio fixture through the worker's host.
- `crates/obc-module-afx` shows a module over a vendor SDK tested without the SDK: a mock SDK fixture stands in for it, and its hardware tests are ignored ([testing.md](testing.md#the-maxine-audio-effects-module)).
- `crates/obc-module-onnx` shows a module over a runtime library, tested with and without it: its unit tests, and its manifest, settings and probe through `obc-module-host`, run anywhere; `tests/runtime.rs` runs it with a real ONNX Runtime and the models and is skipped, with the reason printed, unless `ORT_DYLIB_PATH` and `OBC_ONNX_MODEL_DIR` are given; `tests/hardware.rs` (ignored) runs CUDA frames on a real GPU.
- To try a module in the daemon: put its manifest in a directory and run the daemon with `OBC_MODULE_PATH` set to it. The first command below lists what the daemon found, with the reasons; the second lists the settings, to turn its stage on:

  ```sh
  omarchy-broadcast modules
  omarchy-broadcast settings
  ```
