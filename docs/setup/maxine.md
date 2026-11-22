# NVIDIA Maxine effects

On an NVIDIA RTX GPU, the Maxine modules give you the GPU video effects `blur` and `replace`, video noise removal, relighting, the enhancement of a low-resolution camera, the HDR look and frame generation. Two more modules add eye contact and auto-framing, and GPU microphone effects. Use this page if you have such a GPU and your own copy of the NVIDIA SDKs. Without an NVIDIA GPU, use the open-model cut-out instead ([onnx.md](onnx.md)).

The effects run on the NVIDIA Maxine Video Effects SDK, through the **Maxine module**. The module is a loadable module of omarchy-broadcast ([../modules.md](../modules.md)) that the daemon loads and that opens the SDK at run time.

omarchy-broadcast does not contain, build or download the SDK. The SDK is a personal download from NVIDIA. The sibling project omarchy-nvidia-sdks (the omarchy-nvidia-sdks repository) packages it for Arch as `maxine-vfx-sdk-local`.

This page covers the omarchy-broadcast side:

- which parts of the SDK you need;
- how the daemon applies the SDK's environment to itself;
- what the daemon reads from its environment;
- how to fix the failures specific to omarchy-broadcast.

Without the SDK the daemon works as usual. The `none` effect and the passthrough need nothing from it, and an effect or noise removal that cannot load reports the reason in the daemon's `Error` property.

Without the module (no manifest for it in a `modules.d` directory) the daemon offers no effect at all. The camera image passes through, and `omarchy-broadcast modules` says `No effect modules installed`. A blur or replacement that is asked for then follows `[effect] privacy` (D-Bus revision 14, and from revision 21 the privacy keys):

- privacy off, the default: the camera is shown without the effect;
- privacy on: black frames, the away image or the last frame, by `privacy_mode`.

The daemon's `Warning` says why.

Two more modules have their own sections below:

- The AR effects, eye contact and auto-framing, run on a second SDK through a second module: [The Maxine AR module](#the-maxine-ar-module).
- The microphone's GPU effects (noise removal, room echo removal and studio voice) run on a third, the Audio Effects SDK, through a third module. The audio worker loads that module, not the daemon: [The Maxine audio effects module](#the-maxine-audio-effects-module).

Background: [../research/maxine-linux.md](../research/maxine-linux.md) (access, licence, API). The SDK is never vendored in this repository, because its licence forbids redistribution. The SDK tooling lives in omarchy-nvidia-sdks.

## Quick start

Before you start, you need:

- an NVIDIA GPU of compute capability 7.5 or newer, and driver 570.26 or newer (section 1);
- the core package installed and working ([INSTALL.md](../../INSTALL.md));
- the SDK downloaded from NVIDIA with your own account (section 2).

Then:

1. Build and install the SDK package `maxine-vfx-sdk-local` from your download, with omarchy-nvidia-sdks (section 2).
2. Install the module package `omarchy-broadcast-maxine-vfx-bin` (or `omarchy-broadcast-maxine-vfx-git`), as [INSTALL.md](../../INSTALL.md#steps) describes in step 4. The module package depends on `maxine-vfx-sdk-local`.
3. If the daemon is already running, restart it. Check the log and the module (section 3). The log shows `SDK environment applied from /usr/share/maxine-vfx/maxine-vfx.env`. `modules` lists `br.com.dias.maxine-vfx` with its stages.

## The Maxine module

The module is the library `libobc_module_maxine.so` and a manifest, `maxine-vfx.toml`, which gives it the id `br.com.dias.maxine-vfx`.

The Arch package `omarchy-broadcast-maxine-vfx-git` (or its prebuilt twin `omarchy-broadcast-maxine-vfx-bin`) is one of the packages built from one `PKGBUILD`. It depends on the module ABI that the core `omarchy-broadcast-git` provides and on `maxine-vfx-sdk-local`. It installs:

- the library as `/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so`;
- the manifest in `/usr/lib/omarchy-broadcast/modules.d/`, where the daemon looks for manifests. The manifest names the library by absolute path.

The module contains no NVIDIA code. It opens the SDK when an effect loads, so a build from a checkout installs and loads on a machine without the SDK or an NVIDIA GPU. Without the package, the daemon does not list the module. An effect that cannot load then fails with the reason (in the daemon's `Error` property). Where an SDK tree is found but lacks a feature, the stage that needs it is unavailable ("Not installed").

It provides these stages ([../modules.md](../modules.md#the-manifest) explains the fields):

| Stage | Slot | What it does | SDK feature (section 1) |
|---|---|---|---|
| `enhance` | `pre-matte`, position 0 | Upscales a low-resolution camera in the ingest, by AI super resolution or by a fast upscale | `nvvfxvideosuperres` (AI), `nvvfxupscale` (fast). Either one makes the stage available |
| `denoise` | `pre-matte` | Video noise removal | `nvvfxdenoising` |
| `background` | `matte` | The cut-out of the person, with the blur or the replacement | `nvvfxgreenscreen`, and `nvvfxbackgroundblur` for `blur` |
| `relight` | `post-matte`, needs the cut-out | Relighting of the person | `nvvfxrelighting`, and `nvvfxgreenscreen` |
| `hdr` | `post-composite` | The HDR look: NVIDIA TrueHDR, tone-mapped back to SDR | `nvvfxtruehdr` |
| `frame-generation` | `post-composite`, last | The picture between two camera frames, while the output rate is twice the camera's | `nvvfxvideoframegeneration` |

Check the module with the CLI:

```bash
omarchy-broadcast modules
omarchy-broadcast modules info br.com.dias.maxine-vfx
```

`modules` lists the module with its status and stages. A stage whose feature is not installed says `unavailable: Not installed`. `modules info` shows its manifest and library paths.

The module's settings are the `effect.*` settings of `omarchy-broadcast settings`.

### From a checkout

`scripts/install-user.sh` builds the module and installs it for the current user, without root. It puts the library in `~/.local/share/omarchy-broadcast/modules/` and a manifest in `~/.local/share/omarchy-broadcast/modules.d/`, whose `library` names that library. The module is installed whether or not the SDK is present. Without the SDK it is listed greyed out, with the reason. Run the script again after a change to the module.

A manifest in `~/.local/share/omarchy-broadcast/modules.d` replaces the packaged one of the same id. A module library that is rebuilt later takes a daemon restart to load (libraries are never unloaded). A changed manifest only needs a reload.

A checkout that was not installed with the script (the cargo route of [building.md](../building.md#manual-install)) can take the module by hand, with the recipe below. The script refuses to replace a manifest written this way, so remove it first when you use the script later (section 8):

```bash
cargo build --release -p obc-module-maxine
lib=~/.local/lib/omarchy-broadcast/modules/libobc_module_maxine.so
install -Dm755 target/release/libobc_module_maxine.so "$lib"
mkdir -p ~/.local/share/omarchy-broadcast/modules.d
sed "s|^library = .*|library = \"$lib\"|" crates/obc-module-maxine/maxine-vfx.toml \
    > ~/.local/share/omarchy-broadcast/modules.d/maxine-vfx.toml
systemctl --user restart omarchy-broadcastd
```

### Configuration

The module takes one key, the SDK root, in its own table of `config.toml`:

```toml
[module."br.com.dias.maxine-vfx"]
sdk_root = "/home/user/.local/opt/maxine/VideoFX"
```

Use an absolute path. `[effect] maxine_root` is still honoured: the daemon passes it to the module as `sdk_root` when this table sets none. `MAXINE_VFX_ROOT` in the daemon's environment still wins over both (section 4).

## 1. Requirements

| Item | Requirement | Check |
|---|---|---|
| GPU | NVIDIA, compute capability 7.5 or newer (Turing and up). Tested on the reference machine: RTX 5080 Laptop, CC 12.0 (sm_120) | `nvidia-smi --query-gpu=name,compute_cap --format=csv` |
| Driver | 570.26 or newer. Tested: 615.78.08. The mask-hardening kernel does not raise this floor or the GPU floor | `nvidia-smi --query-gpu=driver_version --format=csv,noheader` |
| System CUDA toolkit | The SDK does not need it: the SDK bundles CUDA 12.8, cuDNN 9.7.1 and TensorRT 10.9 and only takes `libcuda.so.1` from the driver (`nvidia-utils`). The daemon's CUDA video path does need `cuda` for NVRTC (`libnvrtc.so`), which GStreamer's CUDA converters open, so the packages of the Maxine video module and the AR module depend on `cuda` | |
| SDK features | One SDK package that contains the features below, with the models for your GPU architecture | `MAXINE_VFX_FEATURES` in `/usr/share/maxine-vfx/maxine-vfx.env` |

Which effect needs which feature:

| In omarchy-broadcast | SDK feature |
|---|---|
| `blur` and `replace` (the green-screen mask) | `nvvfxgreenscreen` |
| `blur` (the blur itself) | `nvvfxbackgroundblur` |
| Video noise removal (`denoise`) | `nvvfxdenoising` |
| Relighting (`relight`) | `nvvfxrelighting`, and `nvvfxgreenscreen` for the person's mask |
| Enhancement of a low-resolution camera (`enhance`), AI super resolution | `nvvfxvideosuperres` |
| Enhancement of a low-resolution camera (`enhance`), fast upscale | `nvvfxupscale` |
| The HDR look (`hdr`) | `nvvfxtruehdr` (no model files) |
| Frame generation (`frame-generation`) | `nvvfxvideoframegeneration` (no model files) |

- `replace` needs `nvvfxgreenscreen` only for its mask. The replacement is a composite done by the project.
- Noise removal, relighting, enhancement and the HDR look are optional. The effects work without them.
- The names are those of the SDK's feature directories under `features/`, which is where the daemon looks.
- Relighting also needs its models for your GPU architecture in `lib/models` (`relight_0thframe_*` and `relight_nthframe_*`). Noise removal needs its `Denoise-*` models. Neither of the two enhancement features has model files.
- The enhancement stage is available when either of its features is installed. Section 6 says what runs when the chosen method is the one that is missing.
- The daemon checks what is installed from these files, without waking the GPU. A stage whose feature is missing is shown as "Not installed" by the clients and is never requested.

## 2. The SDK package

The omarchy-nvidia-sdks repository owns everything about the SDK itself: the NVIDIA account and NGC access, the download, the supported GPU architectures, and the Arch package `maxine-vfx-sdk-local`. Follow its [README](https://github.com/rsd/omarchy-nvidia-sdks#readme) and [INSTALL.md](https://github.com/rsd/omarchy-nvidia-sdks/blob/main/INSTALL.md) to build and install the package, and its `docs/download.md` for the download and `docs/gpu-support.md` for the architectures. Install it before the module package, which depends on it.

Build the package with the features omarchy-broadcast uses (section 1). Leave out what you do not want:

- `nvvfxrelighting` is only for relighting;
- `nvvfxvideosuperres` and `nvvfxupscale` are only for enhancement;
- `nvvfxtruehdr` is only for the HDR look;
- frame generation needs `nvvfxvideoframegeneration`, which you can add the same way.

The package installs:

- the SDK under `/opt/nvidia/maxine/videofx`;
- an environment file, `/usr/share/maxine-vfx/maxine-vfx.env`;
- the os-release shim, `/usr/lib/maxine-vfx/os-release-shim.so`.

The daemon finds both files at start (section 3).

Nothing in omarchy-broadcast builds or downloads the SDK. The module package `omarchy-broadcast-maxine-vfx-git` (or `-bin`) depends on `maxine-vfx-sdk-local`, so its install stops until the SDK package is installed. The core does not need it. The SDK is a personal download, which is why the module is a package of its own.

**Licence.** The SDK is under NVIDIA's licence and is for your own machine only. omarchy-broadcast never distributes it. You must not put the SDK, the models or the package in a repository, a package or a release artifact. The licence notes are in the "Licence and personal use" section of the omarchy-nvidia-sdks README.

## 3. The SDK environment

The daemon needs the os-release shim from the moment it starts (see below). You need no systemd drop-in for it. With the SDK package installed, `omarchy-broadcastd` applies the package's environment to itself when it starts, and logs that it did.

Install the packages ([INSTALL.md](../../INSTALL.md#steps), step 4), restart the daemon and check the log and the module:

```bash
systemctl --user restart omarchy-broadcastd.service
journalctl --user -u omarchy-broadcastd | grep 'SDK environment'
omarchy-broadcast modules
```

The line is `SDK environment applied from /usr/share/maxine-vfx/maxine-vfx.env`. With the AR SDK installed as well, it names the two files, separated by a comma. `modules` lists `br.com.dias.maxine-vfx` with its stages. The same holds for a checkout (`scripts/install-user.sh`) when `maxine-vfx-sdk-local` is installed.

### What the daemon does

At start the daemon looks for the SDK's two files: the environment file `/usr/share/maxine-vfx/maxine-vfx.env` (in the `EnvironmentFile=` syntax of systemd) and the shim `/usr/lib/maxine-vfx/os-release-shim.so`. Then it does the same for `maxine-ar-sdk-local` (`/usr/share/maxine-ar/maxine-ar.env`, `/usr/lib/maxine-ar/os-release-shim.so`).

When an SDK has both files, the daemon merges the file into its environment and **executes itself again**, once, with the same arguments and the same process. The PID, the unit and the sockets that systemd handed over are the same, and the process keeps the name of the binary.

The second run is needed because `LD_PRELOAD` takes effect only when a process starts. A shim loaded later cannot replace the `os-release` reads that the C library has already bound. The second run logs the line above and clears its marker (`OBC_SDK_ENV_APPLIED`), so the programs it starts decide for themselves. The audio worker does the same with the Audio Effects SDK (see [The Maxine audio effects module](#the-maxine-audio-effects-module)).

The merge follows these rules:

- **An explicit setting wins.** A variable that the daemon's environment already sets (in the unit, a drop-in of yours or the shell that started it) is not overwritten.
- **`LD_PRELOAD` gets the shim appended** to any value it has, once. A value that already names the shim is left as it is. Only one shim is preloaded, the first complete SDK's (Video Effects, then AR), and `MAXINE_OS_RELEASE` is that SDK's. The other SDK contributes its other variables only.
- **`LD_LIBRARY_PATH` is not applied** by the daemon, although the files set it. The modules load the SDKs' libraries from the root by absolute path (section 4). The SDKs' bundled CUDA and TensorRT libraries on the daemon's library path would shadow the engines that the ONNX module's runtime loads. The audio worker needs the path and applies it: it merges the entries you set first, then the file's.
- **No SDK root is applied.** `MAXINE_VFX_ROOT` and `MAXINE_AR_ROOT` stay unset, so `sdk_root` in `config.toml` keeps working with the package installed (section 4).
- **Other dynamic loader variables of the file are refused:** every `LD_*` but `LD_LIBRARY_PATH`, and `GCONV_PATH`. Every other variable is applied when it is not set already (`MAXINE_OS_RELEASE`, and the feature list that the package records).

When something is missing, the daemon runs without that SDK, and the module reports it as before, "Not installed":

| State | What the log says |
|---|---|
| Neither file is installed | Nothing, at the default log level: the SDK is not installed |
| One file only, or the environment file cannot be read or parsed | One warning that names the file (and the line, for a parse error) and ends `running without its environment` |
| The shim is already loaded (a unit or a drop-in of yours preloads it, section 5) | `SDK environment already in place (the os-release shim is loaded): not applied again` |
| `OBC_NO_SDK_ENV=1` in the environment | Nothing at the default level (a line with `-v`): the check is skipped |
| The second run cannot be started | A warning (`cannot execute again with the SDK environment of ...`) and the daemon runs without it |

`OBC_NO_SDK_ENV=1` is for debugging, for a daemon that must run without the SDKs. For example, open a drop-in for the unit:

```bash
systemctl --user edit omarchy-broadcastd
```

and add:

```ini
[Service]
Environment=OBC_NO_SDK_ENV=1
```

The processes the test suites start set it, so a developer's SDK does not leak into them ([../testing.md](../testing.md)).

### A drop-in from an earlier version

Earlier versions asked you to copy `maxine.conf` into `~/.config/systemd/user/omarchy-broadcastd.service.d/`. It still works, because a shim it preloads counts as in place. You do not need it any more, and it would fail the unit if its `EnvironmentFile=` named a package you remove later. Remove it:

```bash
rm -f ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf
systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd.service
```

### The os-release shim

The SDK's model loader reads `/etc/os-release` and refuses distributions that are not on NVIDIA's list. Omarchy reports `ID=omarchy`, and the SDK then fails with `-1209` ("no kernel image is available for execution on the device"). That message is misleading: no CUDA error occurs.

`os-release-shim.so` is an `LD_PRELOAD` library, part of `maxine-vfx-sdk-local`. It shows the preloading process a replacement `os-release` (Ubuntu 24.04) instead of the real file. Only that process is affected, and no root is needed. It does not modify the SDK or the models. NVIDIA does not support this configuration. The shim works because the SDK reads `/etc/os-release` through an ordinary C library call. A future SDK version that reads it another way, or checks something else, would make the load fail again with `-1209`. The Maxine hardware tests detect this. How it works is in the "The compatibility shim" section of `docs/usage.md` in the omarchy-nvidia-sdks repository.

Set `MAXINE_OS_RELEASE_DEBUG=1` in the daemon's environment to make the shim log each redirect on stderr. The journal then shows whether the shim is active.

`LD_PRELOAD` is set on the daemon only. The daemon sets it for its second run (see above), so the shim loads into `omarchy-broadcastd` itself. The shim removes `LD_PRELOAD` from the environment of the process that loads it, so the programs the daemon starts do not inherit it. The daemon recognises that the shim is loaded by looking at its own memory map, as the variable is gone by then.

The environment file never sets `LD_PRELOAD`, and you must not set it on a wrapper such as `timeout`, `sh -c` or `cargo`. The wrapper would load the shim, drop the variable and start the program without it, which then fails with `-1209`.

The daemon does not run in a namespace with a bind-mounted `os-release`. The daemon finds consumers of the virtual camera by reading `/proc/<pid>/fd/N` of the user's other processes, which the kernel refuses inside a user namespace.

## 4. The SDK root and the environment

### The SDK root

The SDK root is the directory that contains `lib/`, `features/` and `external/`. The Maxine module takes it from the first of these that applies:

1. `MAXINE_VFX_ROOT` in the daemon's environment, when it is set and not empty. The daemon does not take it from the package's environment file (section 3). Set it in a drop-in of your own to override the other sources.
2. `sdk_root` in the module's table of `config.toml`, `[module."br.com.dias.maxine-vfx"]`, or, when that table sets none, `[effect] maxine_root` (an alias, kept for existing configurations). It must be an absolute path ([../config.example.toml](../config.example.toml)). A relative or empty `maxine_root` is rejected when the file is loaded. Whether the directory exists is found out when an effect loads.
3. `/opt/nvidia/maxine/videofx`, when that is a directory: where `maxine-vfx-sdk-local` installs the SDK.
4. Otherwise no root: the SDK is looked up on the library search path only, which then has to include its directories (`LD_LIBRARY_PATH` at process start).

When a root is chosen, the module loads the SDK's own libraries from it, by absolute path, before it opens `libVideoFX.so`. These are the SDK libraries, the libraries of every feature under `features/*/lib`, and the bundled CUDA and TensorRT libraries they depend on. **`LD_LIBRARY_PATH` is therefore not needed.** It still works if it is set. A feature added to the tree later is found without any change to the unit. The `models` directory next to the loaded `libVideoFX.so` (`<root>/lib/models`) is passed to the SDK as `NVVFX_MODEL_DIRECTORY`.

Things to know:

- **The key works with the package installed.** `/usr/share/maxine-vfx/maxine-vfx.env` sets `MAXINE_VFX_ROOT`, but the daemon never applies a root variable from it (section 3), so `sdk_root` and `[effect] maxine_root` are honoured. A `MAXINE_VFX_ROOT` that you set yourself in the daemon's environment still wins over the key, and the daemon logs a warning when both are set.
- **The root is fixed once the SDK is loaded.** The SDK is loaded when an effect or noise removal first loads or is first checked, and a loaded library is never unloaded. After that, a different root (a changed `sdk_root` or `maxine_root` after `systemctl --user reload omarchy-broadcastd`) is refused. The effect fails with a message that asks for a restart:

  ```bash
  systemctl --user restart omarchy-broadcastd
  ```

  A reload that changes the key logs this. A running output keeps the root it was started with.
- **A failure keeps the reason.** A missing or incomplete SDK makes the requested effect or noise removal fail as before, with the reason in the `Error` property. Libraries that could not be loaded are named in it.

### Environment variables

What the daemon reads, and where each value comes from:

| Variable | Used for | Comes from |
|---|---|---|
| `MAXINE_VFX_ROOT` | The SDK root (above) | A drop-in or the shell of your own. Optional; never applied from the package's environment file |
| `LD_PRELOAD` | The os-release shim. It must be set when the process starts | The daemon, which appends the shim of the installed SDK and executes itself again (section 3); or a drop-in of yours (section 5) |
| `MAXINE_OS_RELEASE` | The replacement `os-release` file the shim opens: an absolute path | The package's environment file, applied by the daemon (`/usr/lib/maxine-vfx/fake-os-release` with the package); or a line in a drop-in of yours |
| `MAXINE_OS_RELEASE_DEBUG` | `1` makes the shim log each redirect | Set it by hand, for diagnosis |
| `LD_LIBRARY_PATH` | Not needed. The environment file sets it, and the daemon does not apply it: its own loading from the root uses the same files | |
| `OBC_NO_SDK_ENV` | `1` makes the daemon skip the SDK environment (section 3) | Set it by hand, for diagnosis |

A feature added to the package later, as noise removal was, needs only a rebuild and a reinstall of `maxine-vfx-sdk-local` and a restart of the daemon.

The bundled CUDA 12 and TensorRT 10 libraries are loaded into the daemon only. Never put them on `LD_LIBRARY_PATH` in a login profile or in `~/.bashrc`: they would shadow Arch's CUDA 13 for every program you start. For the same reason the SDK package installs no `ld.so.conf.d` or `profile.d` entry.

Development knob: `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process overrides the noise-removal strength, for comparisons. It is not a supported configuration (see section 6).

## 5. An SDK tree without the package

An SDK tree that was installed some other way, for example under `~/.local/opt/maxine/VideoFX`, works too. The daemon applies an environment only from the SDK packages. For a tree of your own, set up in a drop-in what the package would. Two things are needed:

1. The root, either in `config.toml`:

   ```toml
   [module."br.com.dias.maxine-vfx"]
   sdk_root = "/home/user/.local/opt/maxine/VideoFX"
   ```

   (`[effect] maxine_root = "..."` does the same, as an alias)

   or as `Environment=MAXINE_VFX_ROOT=%h/.local/opt/maxine/VideoFX` in the drop-in.
2. The two shim lines, in a drop-in at `~/.config/systemd/user/omarchy-broadcastd.service.d/maxine-tree.conf`. Any name ending in `.conf` works; the directory name must match the unit. Adjust the paths:

   ```ini
   [Service]
   Environment=LD_PRELOAD=%h/.local/lib/maxine-vfx/os-release-shim.so
   Environment=MAXINE_OS_RELEASE=%h/.local/lib/maxine-vfx/fake-os-release
   ```

   Then reload systemd and restart the daemon:

   ```bash
   systemctl --user daemon-reload
   systemctl --user restart omarchy-broadcastd.service
   ```

The shim of your tree counts as in place. The daemon sees a library named `os-release-shim.so` loaded in its own process, from whatever directory, logs `SDK environment already in place`, and applies nothing from a package. So the two can be installed together. You need no `LD_LIBRARY_PATH` line and no list of feature directories.

The shim library and the replacement `os-release` come from the omarchy-nvidia-sdks repository, which owns both. omarchy-broadcast does not contain them. Its `docs/usage.md` describes how to build the shim and where the replacement `os-release` file is ("The compatibility shim" and "Running a program"), and the unpackaged case. Put both files in `~/.local/lib/maxine-vfx/`, where the two lines above point, or change the paths.

`%h` is the user's home directory. systemd does not expand `$HOME` or other variables inside `Environment=`.

## 6. Using the effects

With the SDK installed and its environment applied to the daemon (section 3), you select the effects in two ways:

- In `config.toml`: the `[effect]` section (`mode`, `blur_strength`, `background_image`, `denoise`, `enhance`, `relight`, `relight_light`, `relight_direction`, `relight_brightness`, `privacy`, `privacy_mode`, `maxine_root`) and the module's own `[module."br.com.dias.maxine-vfx"]` table (`sdk_root`). See [../config.example.toml](../config.example.toml).
- At run time, with the `effect` command, which the daemon remembers across restarts:

  ```bash
  omarchy-broadcast effect none
  omarchy-broadcast effect blur
  omarchy-broadcast effect replace
  ```

  The command takes the options `--strength X` and `--image PATH`.

  For every effect and stage there is also `omarchy-broadcast settings` (`effect.*`), for example:

  ```bash
  omarchy-broadcast settings set effect.lighting.relight on
  ```

  See [../cli-json.md](../cli-json.md) and [../dbus-api.md](../dbus-api.md).

### Video noise removal

Video noise removal is a separate switch that combines with any effect: `[effect] denoise = true` in `config.toml`, or at run time:

```bash
omarchy-broadcast denoise on
omarchy-broadcast denoise off
```

It removes the grain a webcam produces, which is mostly visible in dim rooms. It runs before the blur or replacement, or alone with the effect `none`. It needs the `nvvfxdenoising` feature and its models in the package (section 2).

- **It fails open.** Noise removal hides nothing. If it cannot load or run (the feature or SDK is missing, or the frame size is out of range), the stream continues without it, and the daemon's `Error` property says `noise removal unavailable: <reason>`. The privacy setting (`[effect] privacy`) does not apply to it and the daemon does not enter `error`. With privacy on, the effects themselves fail closed, as below.
- **Frame size.** The feature supports frames from 80x80 up to 1920x1080 (the SDK's range). An output contract above 1080p runs without noise removal and reports the reason.
- **Strength.** The SDK's strength is binary: 0 favours texture, 1 favours noise removal. The daemon uses 1 and has no setting for it. For comparisons during development, `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process overrides it. It is a development knob only and is not a supported configuration.
- **Cost.** It adds one GPU pass per frame and its model's GPU memory. With it off, nothing is added: no pass, copy or allocation. The models load only while it is on. Measure on your machine ([../testing.md](../testing.md)):

  ```bash
  obc-bench --denoise
  ```

### Relighting, enhancement, HDR look and chair

Relighting and enhancement are stages that stack the same way, and they fail open too. If one cannot run, the video continues without it and `Error` says why (`relighting unavailable: <reason>; the video continues without it`, `enhance unavailable: ...`).

- **Relighting** is on with `[effect] relight = true` or, at run time, with:

  ```bash
  omarchy-broadcast settings set effect.lighting.relight on
  ```

  It lights the person with a warm, neutral or cool key light (`relight_light`). The direction is from -90 to 90 degrees (`relight_direction`) and the brightness from 50 to 200 percent (`relight_brightness`). The direction and the brightness change while it runs, with no reload. It runs after the green-screen mask, so it needs `nvvfxgreenscreen` as well. It lights only the person: the camera image (`none`), the blurred image or the replacement behind them is not relit. The daemon makes the lighting environments; the SDK ships none. When the camera is switched, the stage is built again from scratch, so the first frames of the new camera run without it.
- **Enhancement** is on with `[effect] enhance = true` or, at run time, with:

  ```bash
  omarchy-broadcast settings set effect.video.enhance on
  ```

  It upscales a camera that captures well below the output size. It runs before the picture is fitted to the output, by one of two methods (below). It applies only where it helps:
  - The part of the camera frame the output shows must have at most half the pixels of the area it fills. A 960x540 or 640x360 mode into a 1080p output does; a 1080p camera does not.
  - The camera's frames must be NV12 or I420.

  Otherwise the setting is greyed with the reason ("The camera already captures at full resolution", "The camera's format cannot be enhanced"). Turning it on or off while the camera is open reopens the camera, like a camera switch: the virtual camera holds the last frame and keeps its format.

  The **method** is the setting `effect.video.enhance_method` ("Enhancement method"). Write it in the panel or with:

  ```bash
  omarchy-broadcast settings set effect.video.enhance_method ai
  omarchy-broadcast settings set effect.video.enhance_method fast
  ```

  It is greyed while enhancement is off, and a change reopens the camera as turning enhancement on or off does. It has no key in `config.toml`; like the other settings of a module, it is kept in the state file.
  - `ai` ("AI super resolution", the default) is NVIDIA's AI video super resolution, the `nvvfxvideosuperres` feature.
  - `fast` ("Fast upscale (lower power)") is NVIDIA's non-AI upscale, the `nvvfxupscale` feature, at the SDK's default sharpening. It is lighter on the GPU and draws less power, for a picture with less restored detail than the AI method. It has no model files.
  - With neither method chosen, the installed one runs, `ai` first. If `ai` is chosen (it is the default) and its feature is not installed while `nvvfxupscale` is, the fast method runs instead and the daemon's log says so. If `fast` is chosen and `nvvfxupscale` is not installed, the stage is refused with the reason (`enhance unavailable: ...`). The costlier AI method never replaces it. The stage is shown as "Not installed" only when neither feature is.
- **HDR look** is off by default. Turn it on with the "HDR look" switch in the panel or with:

  ```bash
  omarchy-broadcast settings set effect.video.hdr true
  ```

  It gives the picture the look of NVIDIA's TrueHDR. TrueHDR converts the picture to HDR10 (the PQ transfer, BT.2020 primaries, 10 bits). omarchy-broadcast then converts the result back to an ordinary 8-bit SDR picture: a tone map that keeps the mid-tones where the camera had them, rolls the highlights off to SDR white and keeps the colour. So the virtual camera, the PipeWire node and the preview all show it, in any application. The picture has more contrast in the highlights and shadows and richer colour. It is a matter of taste, and with the switch off nothing is loaded. It needs the `nvvfxtruehdr` feature, which has no model files; without it the setting is greyed "Not installed". It runs on the finished picture, after the cut-out, the blur or the replacement, and relighting, and before frame generation.
  - **No HDR output.** The output is 8-bit SDR and no application receives HDR. A real 10-bit HDR output is not built: no application that reads the camera on the reference system would show it as HDR.
  - **Banding.** NVIDIA's debander is on (the SDK default), and it limits banding in smooth gradients of an 8-bit source (a plain wall, a soft shadow). Banding may still show there. If you see it, turn the look off.
  - **High output rates.** The HDR look is tested up to 60 fps. When frames arrive much faster than a camera delivers them, NVIDIA's TrueHDR can give a frame an output that depends on the frames before it. At an output rate above 60 fps (the setting allows up to 240) this is untested, and it would show as faint temporal blending or flicker. There is no restriction.
  - **It fails open.** If the look cannot load or fails on a frame, the picture goes out without it. The cut-out is not affected.
- **Chair.** The setting `effect.cutout.chair` ("Hide the chair with the background", in the "Cut-out" group; off by default) chooses what the green screen does with a chair the person sits on.
  - Off: the chair is cut out with the person (NVIDIA's "chair as foreground" segmentation).
  - On: the chair is treated as part of the background, so `blur` blurs it and `replace` covers it ("chair as background").

  It applies to `blur` and `replace` (and to the relighting that uses the same mask), is greyed while the effect is `none`, and has no key in `config.toml`. Only the SDK's quality modes of the green screen are used. A change loads the green screen again in the other mode. The new one is built before the old one is released, so the previous cut-out keeps running until it is ready, and if the new one cannot be built the previous chain keeps running. The cut-out hides the room, so a failure here follows the privacy setting (`[effect] privacy`) like any other failure of the effect. With privacy on, it never lets the camera image through.
- **The mask edge.** Before the background effects use the person mask, the Maxine module maps it through a fixed curve: a mask value of 128 or less becomes fully background, 224 or more becomes fully person, and values in between rise linearly. The values are fixed and there is no setting for them. The room no longer shows around the subject or faintly through it. The cost is that the outline sits 1 to 2 px further in, and fine structures such as hair and fingers lose a little of their soft edge. Two limits remain. The curve changes the edge, not the classification: if the model counts an object (a pile of blankets, say) as part of the person, it stays visible. The edge can also flicker slightly on a still subject, because the curve turns some small mask changes into larger changes of opacity at the edge.
- **Cost.** Each stage adds a GPU pass per frame and its models' GPU memory while it is on, and nothing while it is off. Measure on your machine ([../testing.md](../testing.md)):

  ```bash
  obc-bench --effect blur --denoise --relight
  ```

  The figures are not published.

### The replacement image and what you see while an effect loads

The background image for `replace` must be a PNG, JPEG or still WebP file. WebP needs the GStreamer WebP decoder (`webpdec`, in gst-plugins-bad), an optional package dependency. Without it, a WebP file is refused with a reason that names it. An animated WebP is refused.

What the virtual camera shows while an effect loads:

- **The camera starts with an effect requested.** With privacy on (`[effect] privacy`), the virtual camera shows black, the away image or the last frame until the effect is ready, rather than the unprocessed camera image. With privacy off, the default, it shows the camera as it is until the effect is ready.
- **You turn an effect on while the camera streams.** The camera picture, which was already on screen, continues until the effect is ready. Then the effect replaces it.
- **The SDK cannot be loaded.** With privacy off (`[effect] privacy`, the default), the virtual camera shows the camera as it is, without the effect; the camera stays open and the daemon's `Warning` property says why. With privacy on, it shows black, the away image or the last frame, by `privacy_mode`, instead of the unprocessed image.

Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence. Reproduce locally with `obc-bench` (see [../testing.md](../testing.md)).

To check the SDK installation itself, use the check commands in the "Check the installation" section of `docs/usage.md` in the omarchy-nvidia-sdks repository, and its own tests. omarchy-broadcast has no probe of its own.

## 7. Troubleshooting

Problems with the SDK itself (download, package, architecture) are in omarchy-nvidia-sdks. Its `docs/usage.md` Troubleshooting section explains the SDK status codes `-1209` ("no kernel image is available"), `-2` (unimplemented, a feature library not found) and `-13` ("file could not be found", the model directory). The rows below are the ones specific to omarchy-broadcast.

| Symptom | Cause | Fix |
|---|---|---|
| The daemon's `Warning` property reads `effect blur unavailable: NvVFX_Load(GreenScreen) returned -1209 (...); the video is black until it can run`, naming the os-release override (this page, section 3) (`Error`, with the state `error`, before interface revision 11) | The daemon runs without the shim: `maxine-vfx-sdk-local` is not installed (or lacks its environment file or its shim), the daemon was started with `OBC_NO_SDK_ENV=1`, or it was started before the package was installed. With privacy on (`[effect] privacy = true`, mode `black`) the virtual camera stays black. With the default, privacy off, it shows the camera without the effect | `journalctl --user -u omarchy-broadcastd \| grep 'SDK environment'` must show `SDK environment applied from ...` (or `already in place`). Install the package (section 2) and restart the daemon. Check `ls /usr/share/maxine-vfx/maxine-vfx.env /usr/lib/maxine-vfx/os-release-shim.so`. Set `MAXINE_OS_RELEASE_DEBUG=1` to see the redirect. A `cannot be preloaded` line in the journal means the shim cannot be loaded (a broken package, or a drop-in of yours whose `LD_PRELOAD` path is wrong) |
| The journal has a warning ending `running without its environment`, naming a file under `/usr/share/maxine-vfx`, `/usr/lib/maxine-vfx` or the AR or audio equivalents | The SDK package is half installed (the environment file without the shim, or the reverse), or its environment file cannot be read or parsed (the warning gives the line) | Rebuild and reinstall the SDK package (section 2). The daemon runs without that SDK until then |
| The journal has `the os-release shim of the SDK environment is not loaded` after `SDK environment applied` | The daemon executed itself again with `LD_PRELOAD`, but the shim did not load into it (a library that cannot be mapped, or a `noexec` mount of `/usr/lib`) | Check the library with `ldd /usr/lib/maxine-vfx/os-release-shim.so` and the mount options. Reinstall the package |
| The unit fails to start, and `journalctl --user -u omarchy-broadcastd` shows `Failed to load environment files: No such file or directory` (systemd does not name the file) | A drop-in copied for an earlier version still has `EnvironmentFile=/usr/share/maxine-vfx/maxine-vfx.env` without a leading `-`, and `maxine-vfx-sdk-local` was removed | Remove the drop-in (section 8). The daemon no longer needs it |
| An effect fails with a message that the SDK is already loaded from another place and asks for a restart | `sdk_root`, `[effect] maxine_root` (or `MAXINE_VFX_ROOT`) changed after the SDK was first loaded in the running daemon | `systemctl --user restart omarchy-broadcastd` (section 4) |
| An effect reports that a feature library is not found (`-2`) or the package lacks a feature | The package was built without that feature | Rebuild `maxine-vfx-sdk-local` with the features of section 1 that you use (section 2), reinstall it and restart the daemon |
| `omarchy-broadcast modules` says `No effect modules installed`, or the settings have no effect group | The Maxine module's manifest is not in a `modules.d` directory: the package is not installed, or the checkout was not installed with `scripts/install-user.sh` (or the module was not built and installed by hand) | Install the package, or run `scripts/install-user.sh` in the checkout (see "From a checkout" under [The Maxine module](#the-maxine-module)). Then `systemctl --user restart omarchy-broadcastd` |
| The module is listed `unavailable`, with a reason such as `The library /... is not installed` or `Built for module ABI N; this daemon supports module ABI 1` | The library named by the manifest's `library` is missing, or was built for another version of the module interface than this daemon's | Check the paths with `omarchy-broadcast modules info br.com.dias.maxine-vfx`. Rebuild and reinstall the module from the same checkout as the daemon |
| The module is `disabled` with `Disabled after crashing the daemon twice in a row` | The module's code was running when the daemon went down twice in a row (see the journal for the crash) | Fix the cause (the SDK installation and the shim of section 3 are the usual ones), then `omarchy-broadcast modules enable br.com.dias.maxine-vfx` |
| A setting such as `effect.lighting.relight`, `effect.video.denoise` or `effect.video.hdr` is greyed "Not installed" | The package has no library, or no models for this GPU's architecture, for that feature (relighting also needs `nvvfxgreenscreen`; the HDR look needs `nvvfxtruehdr`, which has no model files) | `ls /opt/nvidia/maxine/videofx/features` and `ls /opt/nvidia/maxine/videofx/lib/models`. Rebuild the package with the feature and your architecture (section 2). The daemon looks again at its next start, reload and session |
| A stage shows as installed, but the effect fails when it loads, and `Warning` gives the SDK's reason | Until the GPU's compute capability is known (before the first session on a machine, with no cache file), the check counts models for any architecture as installed. A model for another architecture then fails when the stage loads, and the stage fails open with the SDK's reason. The check reports what is on disk, not what loads | Install models for your GPU's architecture (section 2). After the first session the daemon knows the architecture and shows the stage as "Not installed" |
| The daemon's `Error` says `relighting unavailable: ...` or `enhance unavailable: ...`, and the video is shown without it | The feature failed to load or run (the reason says which) | The reason names the SDK's status. Check section 3 (the shim applies to every feature's library) and the models. The video is not affected otherwise |
| The daemon's `Error` says `noise removal unavailable: ... -2` (NVCV status), and the camera image is shown without noise removal | The package does not contain the `nvvfxdenoising` feature, or the daemon found no SDK root (section 4) and so looked for it on the library search path only | Check that `MAXINE_VFX_FEATURES` in `/usr/share/maxine-vfx/maxine-vfx.env` lists `nvvfxdenoising`. If not, rebuild the package with it (section 2). Check the `SDK environment` line (section 3) |
| The same, with `-1209` or `-13` | The shim is not applied to this process (the feature's library needs it too), or the `Denoise-*` models for your architecture are not in the package | Section 3. `ls /opt/nvidia/maxine/videofx/lib/models` must show `Denoise-agg-<SM>.engine.trtpkg` and `Denoise-con-<SM>.engine.trtpkg`. Otherwise rebuild the package for your architecture |
| The same, with a message that the frame size is out of range | The output contract is smaller than 80x80 or larger than 1920x1080 | Use an output mode within the range (`[output]` in `config.toml`). Noise removal does not run outside it |
| The daemon's `Warning` property (`Error` before interface revision 11) or the element's status names the mask-hardening kernel and a CUDA error `222` ("the NVIDIA driver is older than this build supports") | The driver rejects the PTX of the mask-hardening kernel: it is older than 570.26 | Update the NVIDIA driver to 570.26 or newer (`nvidia-smi --query-gpu=driver_version --format=csv,noheader`). With privacy on (`[effect] privacy`), the virtual camera stays black until then; with the default, privacy off, it shows the camera without the effect |
| The same, with CUDA error `221` | The driver cannot JIT-compile the kernel: `libnvidia-ptxjitcompiler.so.1` is missing from the driver installation | Reinstall the driver's userspace package (`nvidia-utils`) so that it provides `libnvidia-ptxjitcompiler.so.1`. Check with `ldconfig -p \| grep ptxjitcompiler` |

## The Maxine AR module

Two more effects run on the NVIDIA Maxine **AR** SDK, through a second module of omarchy-broadcast, `br.com.dias.maxine-ar`:

- **Eye contact** redirects the eyes towards the camera.
- **Auto-framing** is a digital pan and zoom that keeps the face in frame.

The AR SDK is a separate personal download from the Video Effects SDK. omarchy-nvidia-sdks packages it as `maxine-ar-sdk-local`. Everything on this page about not distributing the SDK (section 9) applies to it too.

The module is the library `libobc_module_maxine_ar.so` and the manifest `maxine-ar.toml`. The Arch package `omarchy-broadcast-maxine-ar-git` (or `omarchy-broadcast-maxine-ar-bin`) installs it and depends on `maxine-ar-sdk-local` (and on the module ABI that the core provides). `scripts/install-user.sh` installs it for the current user in the same way as the Maxine module (see "From a checkout" under [The Maxine module](#the-maxine-module)).

It opens the SDK at run time, so it installs and loads without the SDK or an NVIDIA GPU. Without the SDK, or without the models the effect needs for your GPU, the effect's setting is greyed with the reason, for example `Maxine AR SDK not installed` or `No face detection models for this GPU (sm_120)`. Check it with:

```bash
omarchy-broadcast modules
omarchy-broadcast modules info br.com.dias.maxine-ar
```

| Stage | Setting | SDK features and models needed |
|---|---|---|
| `eye-contact` | `effect.eye_contact.enabled` | `nvarfaceboxdetection`, `nvarlandmarkdetection` and `nvargazeredirection`, with the models for your GPU architecture: face detection, 126-point face landmarks and gaze redirection |
| `auto-frame` | `effect.auto_frame.enabled` | `nvarfaceboxdetection`, with its face detection models |

The GPU and driver requirements are those of section 1. The names are those of the SDK's feature directories under `features/`. The daemon checks what is installed from these directories and the files in `lib/models`, without waking the GPU. `MAXINE_AR_FEATURES` in `/usr/share/maxine-ar/maxine-ar.env` lists the features the package was built with.

### The AR SDK root

The module takes the root from the first of these that applies:

1. `MAXINE_AR_ROOT` in the daemon's environment, when it is set and not empty (the daemon does not take it from the package's environment file);
2. `sdk_root` in `[module."br.com.dias.maxine-ar"]` of `config.toml`;
3. `/opt/nvidia/maxine/arsdk`, when that is a directory.

Use an absolute path:

```toml
[module."br.com.dias.maxine-ar"]
sdk_root = "/home/user/.local/opt/maxine/arsdk"
```

As for the Video Effects module:

- The key works with the package installed (the daemon applies no root from the environment file, section 3).
- `LD_LIBRARY_PATH` is not needed. The module loads the SDK's libraries from the root by absolute path.
- The SDK is loaded once per process. A different root takes a restart of the daemon:

  ```bash
  systemctl --user restart omarchy-broadcastd
  ```

The two modules have separate roots, set separately. Any other key in the table is refused.

### The AR os-release shim

The AR SDK's feature libraries read `/etc/os-release` as the Video Effects SDK's do, so the daemon needs the shim described in section 3. It applies the AR package's environment (`/usr/share/maxine-ar/maxine-ar.env` and `/usr/lib/maxine-ar/os-release-shim.so`) in the same way, after the Video Effects SDK's.

The shim is process-wide:

- With both packages installed, the daemon preloads one shim, the Video Effects package's, and applies the AR file's other variables. `MAXINE_OS_RELEASE` is then the Video Effects package's.
- With only `maxine-ar-sdk-local` installed, it preloads the AR package's.

The log line names the files applied. You write nothing by hand. Earlier versions asked for a drop-in `maxine-ar.conf` at `~/.config/systemd/user/omarchy-broadcastd.service.d/` when only the AR package was installed. It is no longer needed.

### Both SDKs in one process

The two SDKs each ship a `libNVCVImage` of the same soname, and bundle the same major versions of CUDA and TensorRT. Using the Video Effects module and the AR module in one daemon is tested, with either SDK loaded first ([../testing.md](../testing.md#maxine-ar-module-tests-ignored)).

### Use the AR effects

Both effects are off by default. They are in the settings tree (the bar plugin's panel and `omarchy-broadcast settings`):

- **Eye contact** (`effect.eye_contact.enabled`). It redirects the eyes of the largest face in view towards the camera. With no face the picture is unchanged. There is no strength control: the SDK has none, and its other parameters are left at the SDK's defaults. **Natural look-away** (`effect.eye_contact.look_away`, applies on reload) lets the eyes leave the camera now and then, as a person's do.
- **Auto-framing** (`effect.auto_frame.enabled`). It crops the frame around the largest face and scales the crop back to the frame's size, so the face stays centred as the person moves. **Framing** (`effect.auto_frame.framing`: Tight, Medium or Wide, live) sets how close the crop is.
  - **Framing** sets how much of the cropped picture the face fills: Tight 65 %, Medium 52 %, Wide 45 % of the crop's height. At a normal desk distance a face is 35 to 45 % of the frame's height, so Tight always zooms, Medium zooms moderately, and Wide stays at the full frame for a face larger than about 41 % of the height: Wide zooms only when you sit farther from the camera. A resting crop moves only when the zoom would exceed about 1.09, so small zooms do not show. The bar's panel shows the current zoom as a read-only line.
  - The zoom is capped at 1.5 times.
  - Small head movements do not move the picture, and the motion is smoothed.
  - When the face is lost, the crop holds for about two seconds and then eases back to the full frame.
  - It zooms the picture after the camera image has been scaled to the output size, so a zoomed picture shows less detail than the camera captured.
- **Order.** They run before noise removal, the cut-out and relighting, eye contact first and auto-framing second by default. `omarchy-broadcast order` reorders them within that part of the pipeline.
- **They fail open.** If a stage cannot load or run, it is dropped with a `Warning` and the video goes on without it.

### Troubleshoot the AR module

If a stage is greyed or does not run, the reason is the one `omarchy-broadcast modules` shows. The usual ones are in this table. The rows of section 7 about the shim and the unit apply to this module too.

| Reason shown | Cause | Fix |
|---|---|---|
| `Maxine AR SDK not installed` | No SDK root was found, or it lacks the core libraries | Install `maxine-ar-sdk-local`, or set `sdk_root` or `MAXINE_AR_ROOT`. Restart the daemon |
| `Maxine AR face detection not installed`, `Maxine AR landmark detection not installed` or `Maxine AR eye contact not installed` | The package was built without that feature | Rebuild `maxine-ar-sdk-local` with the feature (the table above), reinstall it and restart the daemon |
| `No face detection models for this GPU (sm_120)` (or `126-point face landmark`, or `Eye Contact`) | The package has no models of that kind for your GPU architecture | `ls /opt/nvidia/maxine/arsdk/lib/models`, and rebuild the package for your architecture |
| The stage is available but is dropped, and the `Warning` names an SDK status | The shim is not applied to the daemon (the feature libraries read `/etc/os-release`), or the models are not found | The AR shim section above and section 3 (the `SDK environment` line). `MAXINE_OS_RELEASE_DEBUG=1` shows the redirect |

A drop-in `maxine-ar.conf` written for an earlier version is not needed any more. Remove it as in section 8. Remove the AR module package and then the SDK package, only if nothing else uses the SDK (`omarchy-broadcast-maxine-ar-bin` for the prebuilt module):

```bash
sudo pacman -R omarchy-broadcast-maxine-ar-git maxine-ar-sdk-local
```

## The Maxine audio effects module

The microphone's GPU effects run on the NVIDIA Maxine **Audio Effects** SDK, through a third module of omarchy-broadcast, `br.com.dias.maxine-afx` (an `audio-filter` module: [../modules.md](../modules.md#the-audio-modules-in-this-repository)). The Audio Effects SDK is a separate personal download from the Video Effects and AR SDKs. omarchy-nvidia-sdks packages it as `maxine-afx-sdk-local`. Section 9 applies to it too.

It differs from the two video modules in three ways, which is why this page does not describe it with them:

- **The audio worker loads it, not the daemon.** The worker is the unit `omarchy-broadcast-audiod.service`. The worker, not `omarchy-broadcastd`, applies the Audio Effects SDK's environment to itself (`/usr/share/maxine-afx/maxine-afx.env` and `/usr/lib/maxine-afx/os-release-shim.so`). The daemon never opens the audio SDK. The camera does not need the audio SDK, and the microphone does not need the camera's. A machine can have any of the three SDK packages.
- **`LD_LIBRARY_PATH` is required.** The SDK opens its effects' feature libraries by a name that only the library search path resolves, read when the process starts. So, unlike the video modules, which load their libraries by absolute path, this module requires the path to lead to the feature libraries of the root in use. The package's environment file sets it, and the worker, unlike the daemon, applies it: it merges the file's entries after the ones of its own environment (section 3). Without it the stages are unavailable, with a reason that names the SDK package.
- **The root is searched in the other order.** `sdk_root` in `[module."br.com.dias.maxine-afx"]`, then `MAXINE_AFX_ROOT`, then `/opt/nvidia/maxine/afx`. The configured root comes first, whereas for the video modules the environment variable comes first (section 4).

The os-release shim of section 3 applies to the worker in the same way. At start the worker merges the package's environment into its own, preloads the shim and executes itself again, once, with the same rules as the daemon. It logs `SDK environment applied from /usr/share/maxine-afx/maxine-afx.env` ([microphone.md](microphone.md#the-workers-sdk-environment) shows the command that finds the line).

The worker starts on demand and exits when the microphone is idle, so the next start applies the environment. You need no restart of the unit after installing the package.

The Arch package `omarchy-broadcast-maxine-afx-git` (or `omarchy-broadcast-maxine-afx-bin`) installs the module and depends on `maxine-afx-sdk-local`. From a checkout, the script's `--audio` option installs the module for the current user ([microphone.md](microphone.md#with-scriptsinstall-usersh---audio)).

The module installs and loads without the SDK, and without the SDK the microphone uses its CPU module.

[microphone.md](microphone.md#on-an-nvidia-gpu-maxine-audio-effects) covers what the effects do, the settings in the Mic tab, studio voice's exclusion of noise and echo removal and its delay, the choice of provider, and troubleshooting. Check the module with the commands below, which show each stage's reason:

```bash
omarchy-broadcast modules
omarchy-broadcast modules info br.com.dias.maxine-afx
```

## 8. Uninstall

```bash
# the module package first (it depends on the SDK package), then the SDK package,
# only if nothing else uses it
sudo pacman -R omarchy-broadcast-maxine-vfx-git     # or omarchy-broadcast-maxine-vfx-bin, if you installed the prebuilt one
sudo pacman -R maxine-vfx-sdk-local
systemctl --user restart omarchy-broadcastd.service
```

Without the SDK the daemon runs as usual and the effects are unavailable.

**Drop-ins from an earlier version.** You do not need a drop-in copied for an earlier version (`maxine.conf`, `maxine-ar.conf`). Remove it, then reload systemd and restart the daemon:

```bash
rm -f ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine-ar.conf
systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd.service
```

**A checkout.** To remove the Maxine module of a checkout installed with `scripts/install-user.sh`, run the script's `--uninstall` option ([building.md](../building.md#uninstall-a-build-from-source)). It removes the module's library and manifest, and the directories among them once they are empty.

A module installed by hand (the recipe under "From a checkout") has a manifest without the script's mark, and the script refuses to remove it. Delete that manifest and the library it names first, then run the uninstall again:

```bash
rm -f ~/.local/share/omarchy-broadcast/modules.d/maxine-vfx.toml ~/.local/lib/omarchy-broadcast/modules/libobc_module_maxine.so
```

Restart the daemon after either. The packaged module goes with the omarchy-broadcast package. To turn it off without removing it:

```bash
omarchy-broadcast modules disable br.com.dias.maxine-vfx
```

**An older shim.** An os-release shim installed by `scripts/install-user.sh` of an older omarchy-broadcast release, under `~/.local/lib/omarchy-broadcast/`, is removed by `scripts/install-user.sh --uninstall` of the current script, and by reinstalling with it. To remove it by hand:

```bash
rm -f ~/.local/lib/omarchy-broadcast/os-release-shim.so ~/.local/lib/omarchy-broadcast/fake-os-release
```

**The audio module** needs no drop-in either. Once `maxine-afx-sdk-local` is removed (after `omarchy-broadcast-maxine-afx-git`, or `-bin`, which depends on it), the worker starts without the SDK from its next start. Delete a drop-in copied for an earlier version, `~/.config/systemd/user/omarchy-broadcast-audiod.service.d/maxine-afx.conf`:

```bash
rm -f ~/.config/systemd/user/omarchy-broadcast-audiod.service.d/maxine-afx.conf
systemctl --user daemon-reload
```

`scripts/install-user.sh --uninstall` removes the module's library and manifest of a checkout, and the packaged ones go with the omarchy-broadcast package. To turn the module off without removing it:

```bash
omarchy-broadcast modules disable br.com.dias.maxine-afx
```

The NGC CLI, the NGC key and the downloads belong to omarchy-nvidia-sdks. Its documentation covers removing them.

## 9. Licence and support

This is a summary, not legal advice. Read the licence you accept.

- The SDK and its models are NVIDIA's, under NVIDIA's licence. You download them with your own NVIDIA account and accept that licence yourself.
- Neither omarchy-broadcast nor omarchy-nvidia-sdks redistributes NVIDIA files. Do not commit the SDK, the models, an NGC key or `~/.ngc/` to a repository, and do not put them in a package or release artifact.
- Benchmark results for the SDK are not published (SLA 8.9).
- Running the SDK on a distribution it does not list, through the compatibility shim, is not supported by NVIDIA.

The README of omarchy-nvidia-sdks has that repository's own licence notes.
