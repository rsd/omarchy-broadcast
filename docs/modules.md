# Writing and installing modules

The effects of omarchy-broadcast are provided by **modules**: a TOML manifest and a shared library that exports a versioned C ABI. The daemon finds the manifests, loads the libraries of the modules that are enabled, and shows the settings each module describes in its clients, with no change to them. This page is the reference for module authors and for people who install modules. The design and the reasons for it are in [ADR-0025](adr/0025-loadable-modules.md); the daemon's side is in [architecture.md](architecture.md#modules), the D-Bus members in [dbus-api.md](dbus-api.md#modules) and the CLI in [cli-json.md](cli-json.md#modules-and-order).

The module interface is **ABI 1.x (1.1 at present), and it is not yet declared stable**: until it is, it may change in breaking ways, so a module built outside this repository may need rebuilding against the matching SDK or header after an update of omarchy-broadcast. No module outside this repository exists yet, so a promise of stability (when a major version changes, and what a minor version may add) has not been made. The version rules the host enforces are in [The ABI](#the-abi-and-the-sdk), and they are unchanged by this.

Where to look:

- the C header, [`crates/obc-module-abi/include/omarchy_broadcast_module.h`](../crates/obc-module-abi/include/omarchy_broadcast_module.h) (installed by the Arch package as `/usr/include/omarchy-broadcast/omarchy_broadcast_module.h`): the ABI, with a comment on every structure, function and constant;
- the Rust SDK, `crates/obc-module-sdk`: a safe API over the ABI;
- the example, [`examples/module-hello`](../examples/module-hello/README.md): a complete module, built on its own outside the workspace as a third-party module would be;
- the Maxine module, `crates/obc-module-maxine`: a module with several stages, a probe of the filesystem, settings and CUDA frames.

## What a module provides

A module provides **stages**. A stage is one step of the video pipeline: noise removal, relighting, the cut-out of the person, a colour tint. A stage is named by an id (`denoise`), sits in a **slot** (where it runs relative to the cut-out), has a **rank** and a **device** (which the daemon uses to choose between modules that provide the same stage), and declares the **frame kinds** and **pixel formats** it takes. A module also describes the **settings** a user can change (a switch for each stage, parameters), in the same self-describing model as the camera's settings, so that the CLI and the bar plugin draw them with no code of their own.

Three stage ids have a meaning to the daemon:

- `background`: the cut-out of the person, with the compositor's modes (`none`, `blur`, `replace`) and the blur strength. The modes and the compositing are the daemon's; a module provides the matte. If the privacy of the effect depends on it (the room behind the person must not show), it never fails open.
- `enhance`: the upscaler of the ingest, which the daemon applies to a camera that captures well below the output size.
- `frame-generation`: a stage that makes the picture between two camera frames, which the daemon runs only while the output rate is twice the camera's (see [The frame-generation stage](#the-frame-generation-stage)). Unlike every other stage it has no switch, and it is not in the stage order.

Every other id is opaque to the daemon. With no module installed the camera image passes through, and the settings tree has no effect group.

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
omarchy-broadcast settings set effect.hello.enabled on
```

The module is listed, with its stage `hello` (`selected`), and its settings (`effect.hello.enabled`, `effect.hello.strength`) appear in `omarchy-broadcast settings` and in the bar panel. It applies when the camera next starts. To try a module without installing it, put its manifest in a directory and point `OBC_MODULE_PATH` at it for the daemon's process.

## The manifest

A manifest is a TOML file with any name ending in `.toml`, in a `modules.d` directory. It is parsed strictly: an unknown key, a missing required key or an invalid value makes the module **unavailable**, with the parse error as the reason shown by `omarchy-broadcast modules` and `GetModules`; nothing else is affected.

```toml
abi = 1
id = "org.example.hello"
name = "Hello tint"
version = "0.1.0"
licence = "MIT OR Apache-2.0"
kind = "video-stage"
library = "libobc_module_hello.so"
description = "Example module: tints the picture warm, on the CPU"

[requires]
hardware = ["any"]

[[stage]]
id = "hello"
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
| `id` | yes | The module's id: reverse-DNS, lower-case letters, digits and hyphens in at least two dot-separated segments (`^[a-z0-9]+(\.[a-z0-9-]+)+$`), at most 64 bytes. Unique: a module with the same id in a later directory replaces it |
| `name`, `version`, `licence` | yes | Shown in the module list; non-empty text. The `licence` is the module's own (a module can be proprietary or under a licence of its own) |
| `kind` | yes | `video-stage`. `audio-filter` and `analyser` are reserved: such a module is listed as unavailable ("not supported yet") and never loaded |
| `library` | yes | The shared library: an absolute path, or a path relative to the manifest's directory |
| `description` | no | One line of text |
| `[requires] hardware` | no | A list of text, informational only: the daemon does not use it, and the probe says what can run |
| `[[stage]]` | at least one for `video-stage` | One table per stage provided; a stage id twice is an error |

Keys of a `[[stage]]`:

| Key | Required | Meaning |
|---|---|---|
| `id` | yes | The stage id. A lower-case letter followed by up to 31 lower-case letters, digits or `-` (`^[a-z][a-z0-9-]{0,31}$`). Not `status`, `effect`, `reason`, `module`, nor any name ending in `-reason` (those are field names of the daemon's status messages). Stage ids are one namespace across modules: two modules that declare the same id are two providers of that stage |
| `slot` | yes | Where the stage runs relative to the person's cut-out, in pipeline order: `pre-matte` (on the camera frame, before the cut-out), `matte` (the cut-out itself), `post-matte` (on the person, after the cut-out), `post-composite` (on the finished picture). A stage never comes after a stage of a later slot |
| `rank` | yes | A non-negative integer. When several modules provide the same stage, the `quality` profile takes the highest rank |
| `device` | yes | `nvidia-gpu`, `gpu`, `npu` or `cpu`: what the stage runs on. The `battery` profile prefers an NPU, then the CPU, then a GPU, then an NVIDIA GPU, and then the rank |
| `position` | no | An integer (default 100). Where the stage prefers to sit within its slot in the default order: lower first, then by stage id |
| `requires` | no | A list of stage ids and slot names that must come earlier in the order. A slot name (`matte`) is met by any earlier stage of that slot. A requirement that no enabled module offers is not an error of the order; the stage's provider decides whether it can run |
| `conflicts` | no | A list of stage ids that cannot be active with this one. In this version the field is validated and kept, and the daemon does not act on it |
| `frames` | yes | The frame kinds the stage takes, in order of preference: `cuda` (device memory) or `host` (system memory). `dmabuf` is reserved in the ABI and accepted in the manifest, and no host offers it |
| `formats` | yes | The pixel formats the stage takes: `nv12`, `i420`, `bgra`, `rgba` or `rgb-f32-planar` |

**Provider, order and placement.** The provider of a stage is the module the user pinned, if it can run it, else the best by the profile; the choice is made when the camera starts and never changes while it runs. The stages of all enabled modules have one order, which the user may change within the slots and the `requires` entries (the default is by slot, `position` and id). The provider of `background` runs the whole cut-out family (its stages in `pre-matte`, `matte` and `post-matte`) inside the effect element, so a stage that `requires` the `matte` slot or the `background` stage is run only by that module: if another module provides it, it is unavailable with the reason "Needs the cut-out of the module that provides it". The stages of any other module run in their own element, those in `pre-matte` before the effect element and the others after it. The `enhance` stage runs in the ingest, in a chain of its own.

### The frame-generation stage

The stage id `frame-generation` is vendor neutral: any module may provide it, as the Maxine module does with NVIDIA Video Frame Generation (a feature of the SDK's own library, with no model files to install). It makes the picture midway between two camera frames, so that an output at **twice** the camera's rate shows a new picture on every tick and not the same picture twice. It differs from every other stage in what the daemon does with it:

- **It is never a user's choice.** It has no enable setting and no row in the settings tree. It is not part of the stage order: `modules.order` never lists it, a stored order that names it is kept without it, and a `SetStageOrder` that names it is refused (`The order cannot name frame-generation: it always runs after the other stages`). Its pin row (`modules.pin.frame-generation`) is shown only when two modules provide it. A stage with no enable setting runs whenever it is provided; this one runs while the output rate is twice the camera's, within 0.2 %, and is otherwise off and loads nothing.
- **It runs last.** The daemon plans it as a chain of its own, named `<module id>-gen`, after the effect element and every other stage, in an `obcmodule` element, whatever slot and position the manifest gives it (the Maxine module gives it the last position of `post-composite`). It sees every tick, fresh frames and repeats, and the frames are NV12 in CUDA memory of one size.
- **What it does with each tick.** A module author follows this contract: on a **fresh** frame, write the frame midway between the previous frame and this one (so the output is one output tick later than the input); on a **repeat** (`OBC_FRAME_FLAG_REPEAT`), write the held current frame; on the first frame after a load or `reset_temporal`, which has no previous frame, write the frame as it is.
- **It fails open, and alone.** If it cannot load or fails on a frame, only the stage is dropped (`OBC_ERR_STAGE`): the frame goes out as it came, the output repeats frames as before and the daemon's `Warning` says so (`60 fps output repeats frames: no frame generation available`). Nothing else changes.
- **Choosing it.** The user chooses a rate with `(generated)` in its label for `output.mode` ([dbus-api.md](dbus-api.md#capture-and-output-modes-revision-12)). Those options are offered only while some enabled module provides the stage.

## Discovery and installation

The daemon reads manifests from these directories, in order; a later directory overrides an earlier one **by module id**:

1. `/usr/lib/omarchy-broadcast/modules.d`: packaged modules.
2. `$XDG_DATA_HOME/omarchy-broadcast/modules.d`, by default `~/.local/share/omarchy-broadcast/modules.d`: the user's own.
3. The directories of `[modules] dirs` in `config.toml` (absolute paths).
4. The colon-separated directories of the `OBC_MODULE_PATH` environment variable, for development and tests.

Discovery reads manifests and nothing else: no library is opened, so a broken library cannot stop the daemon from starting. The daemon looks again at its start and on `SIGHUP` or `ReloadConfig`. A library is opened for a module that is **enabled** (all are, except the ones the configuration's `[modules] disabled`, a `modules.enabled.<id> = false`, the user, or the crash ledger disabled): `omarchy-broadcast modules enable|disable ID`.

The Arch package installs the Maxine module as `/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so` with its manifest, `maxine-vfx.toml`, in `/usr/lib/omarchy-broadcast/modules.d` (the manifest names the library by absolute path). `scripts/install-user.sh` in a checkout builds the Maxine module and installs it for the current user, in `~/.local/share/omarchy-broadcast/modules/` with a manifest in `~/.local/share/omarchy-broadcast/modules.d/`, which replaces the packaged manifest of the same id. A module you build yourself goes in your own `modules.d`, with `library` relative to the manifest or absolute.

**A rebuilt library needs a daemon restart.** Libraries are opened with `RTLD_NODELETE` and are never unloaded: a module may hold process-wide state that must outlive a reload, and code it started must never run unmapped. A library rebuilt at the same path is therefore not loaded again while the daemon runs; the code first loaded stays. A new or changed manifest, or a library at another path, is picked up on a reload; for a rebuilt library, run `systemctl --user restart omarchy-broadcastd`.

## Module configuration

A user configures a module in `config.toml`, in a table named after its id:

```toml
[module."br.com.dias.maxine-vfx"]
sdk_root = "/home/user/.local/opt/maxine/VideoFX"
```

The table is free-form: the daemon checks only the id, and passes the table to the module's `init` as a JSON object (`{}` when there is none). The module checks its own keys and may refuse the table, which makes it unavailable with the reason. The daemon adds one key, **`_host`**, to every module's configuration: `{"_host": {"compute_capability": [12, 0]}}` (the GPU's compute capability as `[major, minor]`, or `null` when it is not known without waking the GPU). A module that refuses unknown keys must accept `_host` and may ignore it. The Maxine module also takes `[effect] maxine_root` as its `sdk_root` when its own table sets none.

The enabled modules, the stage order, the backend profile and the pins are `[modules]` keys of the configuration (`enabled`, `disabled`, `order`, `profile`, `pin`, `dirs`). The user's choices made through the daemon (`omarchy-broadcast modules enable|disable|profile|pin`, `order set`, the bar's setup dialog, the D-Bus setters) are written by the daemon to `config.d/50-panel.toml`, which is read after `config.toml`; a file that sorts after it, such as `config.d/90-local.toml`, wins and the daemon then refuses to change that choice. A module's own table (`[module."<id>"]`) is not written by the daemon. A daemon of 0.24 kept these choices in the state file; the daemon moves them to `config.d/50-panel.toml` when it first loads. See [config.example.toml](config.example.toml) and [ADR-0026](adr/0026-config-d-and-managed-config.md).

## The ABI and the SDK

A module library exports two C symbols:

| Symbol | Does |
|---|---|
| `uint32_t obc_module_abi_version(void)` | Returns the ABI version the library was built against, packed as `major << 16 \| minor`. No side effects: the host calls it before anything else |
| `const obc_module_vtable *obc_module_entry(const obc_host *host)` | Takes the host's description (its ABI version and a log callback) and returns the module's vtable, or null to refuse the host |

**Version rules.** The host refuses a library whose major version is not its own, and one built for a **newer minor** version than its own, each with a reason naming both versions; the entry point is not called then. A library built for an older minor version of the same major version loads. A structure that can grow starts with `struct_size`, and a reader reads a field only when it fits in `struct_size`; vtable entries and fields are only ever appended; a vtable at least as large as that of ABI 1.0 is accepted, and an entry past its `struct_size` counts as not set. A boolean crossing the boundary is a `uint8_t`.

**ABI 1.1** adds one thing, `OBC_FRAME_FLAG_REPEAT` (below). The change is additive: a library built for ABI 1.0 loads and runs unchanged, because the host hands a module only the `obc_frame::flags` bits that its ABI version defines, and clears the others on every frame. A module of ABI 1.0 therefore never sees the new flag and processes a repeat as any other frame. The `obc-module-abi` and `obc-module-sdk` crates are 1.1.0.

**Stability.** The version numbers are the host's check, not yet a promise. ABI 1.x may still change in breaking ways, even without a change of the major version, until it is declared stable and its stability promise is stated (see [ADR-0025](adr/0025-loadable-modules.md)). A module built for one release of omarchy-broadcast may therefore need rebuilding, against the `obc-module-sdk` or the header of the matching release, after an update. The checks above are unchanged: a library of another major version, or of a newer minor version, is still refused with its reason.

**The vtable** (every entry is a nullable function pointer; the header says which are required):

| Entry | Does |
|---|---|
| `init(config_json, &module)` | Creates the module object from its configuration. Cheap: no GPU work, no model loading |
| `probe(module, &json)` | Says which stages can run here: `{"stages":[{"id":"denoise","available":true},{"id":"relight","available":false,"reason":"Not installed"}]}`. **Filesystem checks only**: it must not create a CUDA context, load a model or otherwise wake a runtime-suspended GPU ([ADR-0006](adr/0006-on-demand-gpu-lifecycle.md), [ADR-0017](adr/0017-cuda-checkpoint-parking.md)). The reason is shown to the user |
| `check(module, stages, n)` | Optional. A deeper check, before a stream that needs the module starts, that the stages can run: it may open the module's own runtime libraries, never touch the GPU. A non-zero status (`OBC_ERR_UNAVAILABLE` for a runtime that is not there) with the reason in `last_error` tells the user before the camera opens |
| `describe_settings(module, &json)` | The module's settings, below |
| `instance_create`, `instance_destroy` | An **instance** runs one chain of this module's stages for one stream; all instances are destroyed before the module |
| `instance_load(instance, request, &report)` | Loads the chain of stages the request names, in order, for one frame geometry (below) |
| `instance_process(instance, in, out, live, n_live)` | Runs one frame through the loaded chain. `out` equals `in` only for an in-place chain. The per-frame parameters apply from this frame |
| `instance_reset_temporal(instance, &dropped_json)` | Forget temporal state (a new camera); returns, as a JSON array, the stage ids that could not keep running and were dropped |
| `instance_unload` | Release what the last load loaded; the instance can be loaded again. Must cope with any failed load |
| `instance_timings` | Optional: how long each step of the last frame took, for statistics |
| `instance_park`, `instance_unpark` | Optional hooks for a stream that stops for a while. This version of the daemon does not call them (a session's instances are unloaded when it stops) |
| `last_error`, `instance_last_error` | The reason of the last failed call; never null |
| `destroy` | Releases the module object |

A failing call returns a non-zero status: `OBC_ERR_INVALID` (1), `OBC_ERR_UNSUPPORTED` (2), `OBC_ERR_UNAVAILABLE` (3), `OBC_ERR_FAILED` (4), `OBC_ERR_STATE` (5), `OBC_ERR_PANIC` (6) or `OBC_ERR_STAGE` (7, one stage failed on its own; `instance_last_error` is `<stage id>: <reason>`). A host treats an unknown code as `OBC_ERR_FAILED`.

**The chain request.** `instance_load` receives the stage ids of the chain, in the order they run, with the input frame's kind, format and size, the output format and size (an upscaler's output is larger), the colour description (matrix `bt601`, `bt709` or `bt2020`; full or limited range; the chroma siting), the CUDA context and stream (null for host frames), the load-time parameters of every stage as `(stage, key, value)` triples, and, for a replacement, the background image as a host BGR frame (valid only during the call). A load on a loaded instance is a **switch** when the request keeps the frames of the loaded chain (same kinds, formats, sizes, colour and CUDA handles: only the stages, the parameters or the background change), and may reuse the models of the previous load; with other frames it is a **reload**.

**A frame** is a description: kind (`OBC_FRAME_CUDA`, `OBC_FRAME_HOST`; `OBC_FRAME_DMABUF` reserved), format (`NV12`, `I420`, `BGRA`, `RGBA`, `RGB_F32_PLANAR`, `BGR`), size, up to four planes (`ptr_or_fd`, `pitch`, `offset`; a plane starts at `ptr_or_fd + offset`), the CUDA handles for device memory, a presentation time and flags (bit 0, `OBC_FRAME_FLAG_NEW_SOURCE`: the first frame of a new source; from ABI 1.1, bit 1, `OBC_FRAME_FLAG_REPEAT`: this frame repeats the camera frame of the previous call, see below). The pixels belong to the host: a module may access them only during the call.

**Repeats** (ABI 1.1). The output clock emits a frame on every tick of the output rate. When no new camera frame has arrived since the last tick it emits the last one again: a **repeat**. The host marks it with `OBC_FRAME_FLAG_REPEAT` for a module built for ABI 1.1 or later (the Rust SDK: `Frame::is_repeat()`); the frame has the same picture as the previous call's, which that call has processed. A module may use the flag to skip work on a repeat, to answer it from what it keeps, or to tell a repeat from a fresh frame (a frame-generation stage makes a picture from two fresh frames); a module that ignores it processes the repeat as any frame, as a module of ABI 1.0 does. It still writes `output` on a repeat (an in-place chain's frame may be left as it came). In the daemon the flag reaches a module through an `obcmodule` element. The effect element, which runs the stages of the `background` provider, sends its last processed output on a repeat instead of running its chain, and processes the repeat only when it has no valid output to send ([ADR-0027](adr/0027-output-modes-frame-generation-and-privacy-fallback.md)).

**The Rust SDK.** `obc-module-sdk` wraps all of this. A module is a `cdylib` crate that depends on the SDK, implements `Module` (`init`, `probe`, `settings`, optionally `check`, `create_instance`) and `Instance` (`load`, `process`, optionally `reset_temporal`, `unload`, `timings`, `park`, `unpark`; `Frame::is_new_source()` and `Frame::is_repeat()` tell the flags of a frame), and exports it with `obc_module_sdk::export_module!(MyModule);`. The macro defines the two symbols and the vtable. For every call it checks the pointers and the `struct_size` of every structure, converts the C structures to safe views (`Frame`, `ChainRequest`, `Param`), **catches a panic** (it becomes an error that carries the message, and never unwinds into the daemon: do not build a module with `panic = "abort"`) and keeps every string it hands back alive until the next call on the same object. Log lines go to the daemon's log with the `log` module of the SDK. A module outside this repository depends on the SDK through a path or a git dependency, as `examples/module-hello/Cargo.toml` shows for the path form. The example's `Cargo.lock` is committed and CI builds it with `--locked`.

A module in another language implements the header directly: export the two symbols, fill in a vtable of `struct_size` bytes with the entries it supports, and follow the rules above and in the header's comments.

## Settings

`describe_settings` returns a JSON document in the model of the camera settings ([dbus-api.md](dbus-api.md#camera-settings)):

```json
{"groups":[{"id":"hello","label":"Hello module","settings":[
  {"id":"effect.hello.enabled","label":"Warm tint","type":"bool","default":false,
   "stage":"hello","role":"enable","hint":"switch"},
  {"id":"effect.hello.strength","label":"Strength","type":"int","min":0,"max":100,"step":5,
   "default":50,"unit":"%","stage":"hello","key":"strength","apply":"live",
   "enabled_when":[["effect.hello.enabled","eq",true]],
   "disabled_reason":"Turn on the warm tint to change this","hint":"slider"}]}]}
```

- A **group** has an `id` (one setting-id segment: lower-case letters, digits and `_`; not `background`, which is the daemon's) and a `label`.
- A **setting** has an `id` of the form `effect.<group id>.<name>` (the grammar `^[a-z0-9]+(\.[a-z0-9_]+){1,3}$`, at most 48 bytes, unique), a `label` (at most 48 bytes), and a `type`: `bool` (`default`), `int` (`min`, `max`, `step`, `default`, optional `unit`), `enum` (`options` as `[value id, label]` pairs, at most 16, and `default`, one of them) or `action` (no value). `hint` (`switch`, `slider`, `segmented`) is a suggestion.
- A setting binds to a stage with `stage`, which must be a stage of the manifest. `"role": "enable"` (a `bool`) is the stage's on/off switch: a stage with no such setting runs whenever it is provided, with one exception: `frame-generation` has no switch and is not given one (a setting with `"role": "enable"` bound to it is left out of the tree), because the daemon decides whether it runs, from the output rate (see [The frame-generation stage](#the-frame-generation-stage)). `"key"` makes the setting a **parameter** of the stage, passed to the module under that key: at load time, in `obc_chain_request`, or per frame, in `instance_process`, as `apply` says. `apply` is `live` (per frame, no reload), `reload` (the default for a bound key; the chain is loaded again, a switch that may reuse models) or `restart` (kept, applied when the camera next starts). A parameter's value arrives as an `obc_value`: a boolean, an integer, or a string (an `enum`'s option id).
- `enabled_when` is a list of `[setting id, op, value]` conditions (`eq`, `ne`, `in`), all of which must hold for the setting to be enabled, and `disabled_reason` is what to say while one does not. The daemon applies them; a client evaluates nothing.
- The daemon greys a stage's switch and its parameters, with the stage's reason, while the probe says the stage cannot run ("Not installed").
- At most 64 settings in all. A setting or a group that is malformed or breaks a rule is dropped with a line in the log, and the module and the rest of its description stay.

The values the user sets are kept by setting id in the state file, and reach the module as parameters. The daemon publishes the settings as `scope` `global` settings of the tree, after the camera's, so that `omarchy-broadcast settings`, `SetSetting` and the bar plugin show and write them.

## Threading and CUDA rules

- The host serialises the calls on one module object, and the calls on each instance. Calls on one instance come from one thread, the stream's; a load may block for seconds and runs on a worker thread, never during a frame. The module object and its instances may be called at the same time from different threads, so state they share needs its own locking (an `Arc` in the SDK's terms: `Module` is `Send + Sync`, `Instance` is `Send`). The log callback may be called from any thread.
- A string a module returns is owned by the module and stays valid until the next call on the same object.
- For **CUDA frames** the host makes the context of `obc_cuda.context` current on the calling thread around every instance call, and the module enqueues its work on `obc_cuda.stream` (a null stream with a context is the legacy default stream). **A module never creates a CUDA context of its own, and never makes one current.** The context is the one GStreamer's CUDA elements share ([ADR-0012](adr/0012-video-effects-and-maxine-backend.md)); releasing it at teardown leaves no CUDA context in the process, which is what lets the GPU be parked at idle ([ADR-0017](adr/0017-cuda-checkpoint-parking.md)). Do not initialise CUDA in `init`, `probe` or `check`.
- A `host` chain gets CUDA frames through a page-locked host copy that the host makes: you declare the memory your stage takes, and the host converts only at the boundary of your chain.
- A frame is processed in place for an in-place chain; the output clock may push the same frame twice, so the host copies a frame before a stage writes it (and, when a chain fails halfway, restores the input before running again).

## Failure semantics

- **A module that cannot load is not an error of the daemon.** A broken manifest, a missing or unloadable library, another ABI major version, a library built for a newer minor version, a failing `init`, probe or `check`, each makes the module `unavailable` with the reason; the daemon runs, and the module list says why.
- **A stage that fails alone fails open**, unless it is a matte stage: return `OBC_ERR_STAGE` (the SDK's `Error::stage`) naming the stage. From `instance_load` nothing of the request is in force (a first load or a reload leaves the instance unloaded; a switch keeps the previous chain, less the failed stage when it could not be left as it was); from `instance_process` the module has dropped the stage, the output of that frame is not written, and the host runs the frame again through the chain without it. The picture is never blocked by a stage that hides nothing.
- **A matte never fails open.** A failure of the `background` stage, or of any stage in the `matte` slot, is taken as a failure of the whole chain, and the instance is unloaded, so that no frame runs without its cut-out; the effect then follows `[effect] on_failure` (black by default, which is what the user chose when they asked to hide the room behind them): the feed keeps streaming, its frames black until the effect runs again, and the daemon's `Warning` property says why; the camera is not closed. This governs a module that provides the stage and fails to load or run. When no module provides a selected stage (the module is disabled, disabled by the crash ledger, `unavailable`, or the stage not installed), the host does not run it at all: the stage is dropped, the feed keeps streaming with the stages that still have a provider, and the daemon's `Warning` property names the stage and the module's reason ([dbus-api.md](dbus-api.md#video-effects)). For a stage of your own module that holds once the daemon has read your settings (the `role = "enable"` switch) since it started.
- **A failed switch keeps the previous chain** when the frames of the request are those of the loaded chain: it keeps running as it was. A failed first load, a failed load at another geometry (a reload) and a load that panicked leave the instance unloaded; the host then calls `instance_unload`, which must be safe after any failure.
- **A panic** in a module written with the SDK is caught at the boundary and returned as `OBC_ERR_PANIC`; the host unloads the instance.
- **A crash of native code** (a segmentation fault, an `abort`) takes the daemon down with it: modules run in the daemon's process. The unit's restart and the loopback's timeout frames keep applications that hold the camera alive ([ADR-0022](adr/0022-crash-recovery-for-consumers.md)), and the crash ledger below keeps a module from doing it again and again.

## The crash ledger

The daemon keeps a small JSON file, `modules-crash.json`, in its state directory (`$XDG_STATE_HOME/omarchy-broadcast/`). While a module's code runs a pipeline or a probe, the file holds a marker for the module; a clean stop clears it. At each start the daemon counts, per module, the consecutive starts that found its marker (a start that finds none resets the count). A module that was running when the daemon went down **twice in a row** is disabled, with the reason `Disabled after crashing the daemon twice in a row`, until the user enables it again (`omarchy-broadcast modules enable ID`), which forgives it. A session marks only the modules that run in it: the effect module while the background effect is on or one of its stages is requested, the enhancer while enhancement is on, and another module's chain while one of its stages is. A damaged ledger file counts as empty.

## Reserved names, in one place

| Kind | Rule |
|---|---|
| Module id | `^[a-z0-9]+(\.[a-z0-9-]+)+$`, at most 64 bytes |
| Stage id | `^[a-z][a-z0-9-]{0,31}$`; not `status`, `effect`, `reason`, `module`, or any name ending in `-reason`. `background`, `enhance` and `frame-generation` have a meaning to the daemon |
| Slot | `pre-matte`, `matte`, `post-matte`, `post-composite` |
| Device | `nvidia-gpu`, `gpu`, `npu`, `cpu` |
| Frame kind | `cuda`, `host`; `dmabuf` reserved |
| Format | `nv12`, `i420`, `bgra`, `rgba`, `rgb-f32-planar` |
| Setting id | `effect.<group>.<name>`, at most 48 bytes; the group `background` is the daemon's |
| Configuration | `[module."<id>"]` for the module's table; `_host` is the daemon's key inside it |
| Environment | `OBC_MODULE_PATH`: extra manifest directories, colon-separated |

## Testing a module

- Test the logic with the SDK's types, as `examples/module-hello/src/lib.rs` does (its unit tests build frames and call the instance directly), and the exported library through the C ABI: `crates/obc-module-sdk/tests/vtable.rs` calls the vtable as a host does.
- To run a module in the real host without a camera, `crates/obc-module-host/tests/modules.rs` builds the example and a fixture module, loads them with `dlopen` as the daemon does and drives the registry and the adapters; the fixture's features export a library of another major version, of a newer minor version, and one that refuses the host.
- The commands, and the tests that need a GPU or the Maxine SDK, are in [testing.md](testing.md#modules).
- To try a module in the daemon: put its manifest in a directory, run the daemon with `OBC_MODULE_PATH` set to it (`omarchy-broadcast modules` lists what it found, with the reasons), and use `omarchy-broadcast settings` to turn its stage on.
