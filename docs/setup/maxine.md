# Using the NVIDIA Maxine Video Effects SDK with omarchy-broadcast

The video effects `blur` and `replace`, video noise removal, relighting and the enhancement of a low-resolution camera run on the NVIDIA Maxine Video Effects SDK, through the **Maxine module**: a loadable module of omarchy-broadcast ([../modules.md](../modules.md), [ADR-0025](../adr/0025-loadable-modules.md)) that the daemon loads and that opens the SDK at run time. omarchy-broadcast does not contain, build or download the SDK. The SDK is a personal download from NVIDIA, and the sibling project omarchy-nvidia-sdks (the omarchy-nvidia-sdks repository) packages it for Arch as `maxine-vfx-sdk-local`. This page covers the omarchy-broadcast side: which parts of the SDK are needed, how to enable the daemon's drop-in, what the daemon reads from its environment and how to fix the failures that are specific to omarchy-broadcast.

Without the SDK the daemon works as usual: the `none` effect and the passthrough need nothing from it, and an effect or noise removal that cannot load reports the reason in the daemon's `Error` property. Without the module (no manifest for it in a `modules.d` directory) the daemon offers no effect at all: the camera image passes through and `omarchy-broadcast modules` says `No effect modules installed`.

The AR effects, eye contact and auto-framing, run on a second SDK through a second module, and are described in [The Maxine AR module](#the-maxine-ar-module). The microphone's GPU effects (noise removal, room echo removal and studio voice) run on a third, the Audio Effects SDK, through a third module that the audio worker loads, not the daemon, and are described in [The Maxine audio effects module](#the-maxine-audio-effects-module).

Background: [../research/maxine-linux.md](../research/maxine-linux.md) (access, licence, API). The SDK is never vendored in this repository, see [ADR-0003](../adr/0003-maxine-first-behind-backend-seam.md). The decision to leave the SDK tooling to omarchy-nvidia-sdks is in [ADR-0021](../adr/0021-nvidia-sdk-handover.md).

## The Maxine module

The module is the library `libobc_module_maxine.so` and a manifest, `maxine-vfx.toml`, which gives it the id `br.com.dias.maxine-vfx`. The Arch package installs the library as `/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so` and the manifest in `/usr/lib/omarchy-broadcast/modules.d/`, where the daemon looks for manifests; the manifest names the library by absolute path. The module contains no NVIDIA code: it opens the SDK when an effect loads, so it installs and loads on a machine without the SDK or an NVIDIA GPU. An effect that cannot load then fails with the reason (in the daemon's `Error` property), and where an SDK tree is found but lacks a feature, the stage that needs it is unavailable ("Not installed").

It provides these stages ([../modules.md](../modules.md#the-manifest) explains the fields):

| Stage | Slot | What it does | SDK feature (section 1) |
|---|---|---|---|
| `enhance` | `pre-matte`, position 0 | Upscales a low-resolution camera in the ingest, by AI super resolution or by a fast upscale | `nvvfxvideosuperres` (AI), `nvvfxupscale` (fast); either one makes the stage available |
| `denoise` | `pre-matte` | Video noise removal | `nvvfxdenoising` |
| `background` | `matte` | The cut-out of the person, with the blur or the replacement | `nvvfxgreenscreen`, and `nvvfxbackgroundblur` for `blur` |
| `relight` | `post-matte`, needs the cut-out | Relighting of the person | `nvvfxrelighting`, and `nvvfxgreenscreen` |
| `hdr` | `post-composite` | The HDR look: NVIDIA TrueHDR, tone-mapped back to SDR | `nvvfxtruehdr` |
| `frame-generation` | `post-composite`, last | The picture between two camera frames, while the output rate is twice the camera's | `nvvfxvideoframegeneration` |

Check it with the CLI: `omarchy-broadcast modules` lists the module with its status and stages (a stage whose feature is not installed says `unavailable: Not installed`), and `omarchy-broadcast modules info br.com.dias.maxine-vfx` shows its manifest and library paths. Its settings are the `effect.*` settings of `omarchy-broadcast settings`.

**From a checkout**: `scripts/install-user.sh` builds the module and installs it for the current user, without root: the library in `~/.local/share/omarchy-broadcast/modules/` and a manifest in `~/.local/share/omarchy-broadcast/modules.d/`, whose `library` names that library. The module is installed whether or not the SDK is present; without the SDK it is listed greyed out, with the reason. Run the script again after a change to the module.

A manifest in `~/.local/share/omarchy-broadcast/modules.d` replaces the packaged one of the same id. A module library that is rebuilt later takes a daemon restart to load (libraries are never unloaded); a changed manifest only needs a reload.

A checkout that was not installed with the script (the cargo route of [daemon.md](daemon.md#manual-install)) can take the module by hand, with the recipe below. The script refuses to replace a manifest written this way, so remove it first when the script is used later (section 8):

```bash
cargo build --release -p obc-module-maxine
lib=~/.local/lib/omarchy-broadcast/modules/libobc_module_maxine.so
install -Dm755 target/release/libobc_module_maxine.so "$lib"
mkdir -p ~/.local/share/omarchy-broadcast/modules.d
sed "s|^library = .*|library = \"$lib\"|" crates/obc-module-maxine/maxine-vfx.toml \
    > ~/.local/share/omarchy-broadcast/modules.d/maxine-vfx.toml
systemctl --user restart omarchy-broadcastd
```

**Configuration.** The module takes one key, the SDK root, in its own table of `config.toml`:

```toml
[module."br.com.dias.maxine-vfx"]
sdk_root = "/home/user/.local/opt/maxine/VideoFX"
```

`[effect] maxine_root` is still honoured: the daemon passes it to the module as `sdk_root` when this table sets none, and `MAXINE_VFX_ROOT` in the daemon's environment still wins over both (section 4). Use an absolute path.

## 1. Requirements

| Item | Requirement | Check |
|---|---|---|
| GPU | NVIDIA, compute capability 7.5 or newer (Turing and up). Tested on the reference machine: RTX 5080 Laptop, CC 12.0 (sm_120) | `nvidia-smi --query-gpu=name,compute_cap --format=csv` |
| Driver | 570.26 or newer. Tested: 615.78.08. The mask-hardening kernel ([ADR-0013](../adr/0013-mask-hardening-cuda-kernel.md)) does not raise this floor or the GPU floor | `nvidia-smi --query-gpu=driver_version --format=csv,noheader` |
| System CUDA toolkit | Not needed for the SDK: it bundles CUDA 12.8, cuDNN 9.7.1 and TensorRT 10.9 and only takes `libcuda.so.1` from the driver (`nvidia-utils`) | |
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

`replace` needs `nvvfxgreenscreen` only for its mask; the replacement is a composite done by the project. Noise removal, relighting, enhancement and the HDR look are optional and the effects work without them. The names are those of the SDK's feature directories under `features/`, which is where the daemon looks. Relighting also needs its models for your GPU architecture in `lib/models` (`relight_0thframe_*` and `relight_nthframe_*`), and noise removal its `Denoise-*` models; neither of the two enhancement features has model files. The enhancement stage is available when either of them is installed, and section 6 says what runs when the method chosen is the one that is missing. The daemon checks what is installed from these files, without waking the GPU: a stage whose feature is missing is shown as "Not installed" by the clients and is never requested.

## 2. Build and install maxine-vfx-sdk-local

The omarchy-nvidia-sdks repository owns everything about the SDK itself: the NVIDIA account and NGC access, the download, the supported GPU architectures, and the Arch package `maxine-vfx-sdk-local`. Follow its README and its `docs/download.md` for the download and `docs/gpu-support.md` for the architectures. The package installs the SDK under `/opt/nvidia/maxine/videofx` and an environment file, `/usr/share/maxine-vfx/maxine-vfx.env`.

Build the package with the features omarchy-broadcast uses (`nvvfxrelighting` is only for relighting, `nvvfxvideosuperres` and `nvvfxupscale` only for enhancement, `nvvfxtruehdr` only for the HDR look, and frame generation needs `nvvfxvideoframegeneration`, which you can add the same way; leave out what you do not want). From a checkout of the omarchy-nvidia-sdks repository, with the download in `~/Downloads/maxine` and `sm_120` standing for your architecture:

```bash
maxine-vfx/configure.sh --src ~/Downloads/maxine --archs sm_120 \
    --features nvvfxgreenscreen,nvvfxbackgroundblur,nvvfxdenoising,nvvfxrelighting,nvvfxvideosuperres,nvvfxupscale,nvvfxtruehdr --write
(cd maxine-vfx && makepkg -f)
```

Then install the package yourself:

```bash
cd maxine-vfx && sudo pacman -U maxine-vfx-sdk-local-*.pkg.tar*
```

Nothing in omarchy-broadcast installs the package. The omarchy-broadcast Arch package lists it as an optional dependency (`optdepends`), because the SDK is a personal download and cannot be a hard requirement.

Licence: the SDK is under NVIDIA's licence and is for your own machine only. omarchy-broadcast never distributes it, and you must not put the SDK, the models or the package in a repository, a package or a release artifact. The licence notes are in the "Licence and personal use" section of the omarchy-nvidia-sdks README.

## 3. Enable the drop-in

The daemon needs the os-release shim from the moment it starts (see below), and that takes a systemd user drop-in. The shipped drop-in has two active lines:

```ini
[Service]
EnvironmentFile=/usr/share/maxine-vfx/maxine-vfx.env
Environment=LD_PRELOAD=/usr/lib/maxine-vfx/os-release-shim.so
```

With the package installed at its default root, the environment file is optional: the daemon finds `/opt/nvidia/maxine/videofx` itself (section 4) and loads the SDK's libraries from it. The two lines that are strictly needed are `Environment=LD_PRELOAD=/usr/lib/maxine-vfx/os-release-shim.so` and `Environment=MAXINE_OS_RELEASE=/usr/lib/maxine-vfx/fake-os-release`. Keeping `EnvironmentFile=` is fine; it also sets `MAXINE_VFX_ROOT` (to the same tree) and `MAXINE_OS_RELEASE`, and `[effect] maxine_root` is then ignored (section 4).

Copy the example that matches your install of omarchy-broadcast, then reload and restart:

```bash
# package install:
install -Dm644 /usr/share/doc/omarchy-broadcast/examples/systemd/omarchy-broadcastd.service.d/maxine.conf \
    ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf
# checkout (install-user.sh or cargo), from the repository root:
install -Dm644 dist/systemd/omarchy-broadcastd.service.d/maxine.conf \
    ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf

systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd.service
```

Both files have the same content: [`dist/systemd/omarchy-broadcastd.service.d/maxine.conf`](../../dist/systemd/omarchy-broadcastd.service.d/maxine.conf). The `EnvironmentFile=` line has no leading `-`, so the unit fails to start when the package is not installed; that is intended, see section 7.

**The os-release shim.** The SDK's model loader reads `/etc/os-release` and refuses distributions that are not on NVIDIA's list. Omarchy reports `ID=omarchy`, and the SDK then fails with `-1209` ("no kernel image is available for execution on the device"), which is misleading: no CUDA error occurs. `os-release-shim.so` is an `LD_PRELOAD` library, part of `maxine-vfx-sdk-local`, that shows the preloading process a replacement `os-release` (Ubuntu 24.04) instead of the real file. Only that process is affected and no root is needed. It does not modify the SDK, the models or their encryption, and NVIDIA does not support this configuration. How it works is in the "The compatibility shim" section of `docs/usage.md` in the omarchy-nvidia-sdks repository. Set `MAXINE_OS_RELEASE_DEBUG=1` in the daemon's environment to make the shim log each redirect on stderr, which shows in the journal whether it is active.

`LD_PRELOAD` is set on the daemon only, and the drop-in does it with `Environment=` on the unit, so the shim loads into `omarchy-broadcastd` itself. The shim removes `LD_PRELOAD` from the environment of the process that loads it, so the programs the daemon starts do not inherit it. The environment file never sets `LD_PRELOAD`, and it must not be set on a wrapper such as `timeout`, `sh -c` or `cargo`: the wrapper would load the shim, drop the variable and start the program without it, which then fails with `-1209`.

The reason the daemon is not run in a namespace with a bind-mounted `os-release` is that the daemon finds consumers of the virtual camera by reading `/proc/<pid>/fd/N` of the user's other processes, which the kernel refuses inside a user namespace; see [ADR-0012](../adr/0012-video-effects-and-maxine-backend.md).

## 4. The SDK root and the environment

### The SDK root

The SDK root is the directory that contains `lib/`, `features/` and `external/`. The Maxine module takes it from the first of these that applies:

1. `MAXINE_VFX_ROOT` in the daemon's environment, when it is set and not empty. The package's environment file sets it.
2. `sdk_root` in the module's table of `config.toml`, `[module."br.com.dias.maxine-vfx"]`, or, when that table sets none, `[effect] maxine_root` (an alias, kept for existing configurations): an absolute path ([../config.example.toml](../config.example.toml)). A relative or empty `maxine_root` is rejected when the file is loaded; whether the directory exists is found out when an effect loads.
3. `/opt/nvidia/maxine/videofx`, when that is a directory: where `maxine-vfx-sdk-local` installs the SDK.
4. Otherwise no root: the SDK is looked up on the library search path only, which then has to include its directories (`LD_LIBRARY_PATH` at process start).

When a root is chosen, the module loads the SDK's own libraries from it, by absolute path, before it opens `libVideoFX.so`: the SDK libraries, the libraries of every feature under `features/*/lib`, and the bundled CUDA and TensorRT libraries they depend on. **`LD_LIBRARY_PATH` is therefore not needed.** It still works if it is set. A feature added to the tree later is found without any change to the unit. The `models` directory next to the loaded `libVideoFX.so` (`<root>/lib/models`) is passed to the SDK as `NVVFX_MODEL_DIRECTORY`.

Things to know:

- **The environment file overrides the key.** `/usr/share/maxine-vfx/maxine-vfx.env` sets `MAXINE_VFX_ROOT`, so a unit that loads it ignores `sdk_root` and `[effect] maxine_root`; the daemon logs a warning when both are set. To use the key, leave the environment file out of the drop-in.
- **The root is fixed once the SDK is loaded.** The SDK is loaded when an effect or noise removal first loads or is first checked, and a loaded library is never unloaded. After that, a different root (a changed `sdk_root` or `maxine_root` after `systemctl --user reload omarchy-broadcastd`) is refused: the effect fails with a message that asks for a restart (`systemctl --user restart omarchy-broadcastd`). A reload that changes the key logs this. A running output keeps the root it was started with.
- **A failure keeps the reason.** A missing or incomplete SDK makes the requested effect or noise removal fail as before, with the reason in the `Error` property; libraries that could not be loaded are named in it.

### Environment variables

What the daemon reads, and where each value comes from:

| Variable | Used for | Comes from |
|---|---|---|
| `MAXINE_VFX_ROOT` | The SDK root (above) | The environment file, if the drop-in loads it. Optional |
| `LD_PRELOAD` | The os-release shim. It must be set when the process starts | The drop-in (required) |
| `MAXINE_OS_RELEASE` | The replacement `os-release` file the shim opens: an absolute path | The environment file, or a line in the drop-in (`/usr/lib/maxine-vfx/fake-os-release` with the package) |
| `MAXINE_OS_RELEASE_DEBUG` | `1` makes the shim log each redirect | Set it by hand, for diagnosis |
| `LD_LIBRARY_PATH` | Not needed. The environment file still sets it; the daemon's own loading from the root uses the same files | |

A feature added to the package later, as noise removal was, needs only a rebuild and a reinstall of `maxine-vfx-sdk-local`. The drop-in does not change.

The bundled CUDA 12 and TensorRT 10 libraries are loaded into the daemon only. Never put them on `LD_LIBRARY_PATH` in a login profile or in `~/.bashrc`: they would shadow Arch's CUDA 13 for every program you start. For the same reason the SDK package installs no `ld.so.conf.d` or `profile.d` entry.

Development knob: `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process overrides the noise-removal strength, for comparisons. It is not a supported configuration (see section 6).

## 5. An SDK tree without the package

An SDK tree that was installed some other way, for example under `~/.local/opt/maxine/VideoFX`, works too. Two things are needed:

1. The root, either in `config.toml` (then do not load the package's environment file, which would override it):

   ```toml
   [module."br.com.dias.maxine-vfx"]
   sdk_root = "/home/user/.local/opt/maxine/VideoFX"
   ```

   (`[effect] maxine_root = "..."` does the same, as an alias)

   or as `Environment=MAXINE_VFX_ROOT=%h/.local/opt/maxine/VideoFX` in the drop-in.
2. The two shim lines in the drop-in. Replace the active lines of the shipped drop-in with them, adjust the paths, and run `daemon-reload` and a restart:

   ```ini
   [Service]
   Environment=LD_PRELOAD=%h/.local/lib/maxine-vfx/os-release-shim.so
   Environment=MAXINE_OS_RELEASE=%h/.local/lib/maxine-vfx/fake-os-release
   ```

No `LD_LIBRARY_PATH` line and no list of feature directories are needed. The shipped drop-in has the same variant as a comment.

The shim library and the replacement `os-release` come from the omarchy-nvidia-sdks repository, which owns both; omarchy-broadcast does not contain them. In a checkout of that repository, `make -C tools/os-release-shim` writes `target/os-release-shim/os-release-shim.so`, and the replacement `os-release` is that repository's `tools/maxine-probe/fake-os-release`. Copy both to `~/.local/lib/maxine-vfx/`, where the two lines above point, or change the paths. The "The compatibility shim" and "Running a program" sections of its `docs/usage.md` describe them and the unpackaged case. `%h` is the user's home directory: systemd does not expand `$HOME` or other variables inside `Environment=`.

## 6. Using the effects

With the SDK installed and the daemon running with the drop-in (section 3), the effects are selected in two ways:

- In `config.toml`, the `[effect]` section (`mode`, `blur_strength`, `background_image`, `denoise`, `enhance`, `relight`, `relight_light`, `relight_direction`, `relight_brightness`, `on_failure`, `maxine_root`) and the module's own `[module."br.com.dias.maxine-vfx"]` table (`sdk_root`); see [../config.example.toml](../config.example.toml).
- At run time, with `omarchy-broadcast effect none|blur|replace [--strength X] [--image PATH]`, which the daemon remembers across restarts, and, for every effect and stage, `omarchy-broadcast settings` (`effect.*`, for example `settings set effect.lighting.relight on`); see [../cli-json.md](../cli-json.md) and [../dbus-api.md](../dbus-api.md).

Video noise removal is a separate switch that combines with any effect: `[effect] denoise = true` in `config.toml`, or `omarchy-broadcast denoise on|off` at run time. It removes the grain a webcam produces, which is mostly visible in dim rooms, and it runs before the blur or replacement, or alone with the effect `none`. It needs the `nvvfxdenoising` feature and its models in the package (section 2). Things to know:

- **It fails open.** Noise removal hides nothing, so if it cannot load or run (the feature or SDK is missing, or the frame size is out of range) the stream continues without it, and the daemon's `Error` property says `noise removal unavailable: <reason>`. `[effect] on_failure` does not apply to it and the daemon does not enter `error`. The effects themselves still fail closed, as below.
- **Frame size.** The feature supports frames from 80x80 up to 1920x1080 (the SDK's range). An output contract above 1080p runs without noise removal and reports the reason.
- **Strength.** The SDK's strength is binary: 0 favours texture, 1 favours noise removal. The daemon uses 1 and has no setting for it. For comparisons during development, `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process overrides it. It is a development knob only and is not a supported configuration.
- **Cost.** It adds one GPU pass per frame and its model's GPU memory; with it off, nothing is added, no pass, copy or allocation. The models load only while it is on. Measure on your machine with `obc-bench --denoise` ([../testing.md](../testing.md)).
- The design is in [ADR-0018](../adr/0018-video-noise-removal.md).

Relighting and enhancement are stages that stack the same way ([ADR-0024](../adr/0024-stackable-stages-and-effects-in-the-settings-schema.md)), and fail open too: if one cannot run, the video continues without it and `Error` says why (`relighting unavailable: <reason>; the video continues without it`, `enhance unavailable: ...`).

- **Relighting** (`[effect] relight = true`, or `omarchy-broadcast settings set effect.lighting.relight on`) lights the person with a warm, neutral or cool key light (`relight_light`), from a direction of -90 to 90 degrees (`relight_direction`) and at a brightness of 50 to 200 percent (`relight_brightness`); the direction and the brightness change while it runs with no reload. It runs after the green-screen mask, so it needs `nvvfxgreenscreen` as well, and it lights only the person: the camera image (`none`), the blurred image or the replacement behind them is not relit. The lighting environments are made by the daemon; the SDK ships none. When the camera is switched, the stage is built again from scratch, so the first frames of the new camera run without it.
- **Enhancement** (`[effect] enhance = true`, or `settings set effect.video.enhance on`) upscales a camera that captures well below the output size, before the picture is fitted to the output, by one of two methods (below). It applies only where it helps: the part of the camera frame the output shows must have at most half the pixels of the area it fills (a 960x540 or 640x360 mode into a 1080p output does; a 1080p camera does not), and the camera's frames must be NV12 or I420. Otherwise the setting is greyed with the reason ("The camera already captures at full resolution", "The camera's format cannot be enhanced"). Turning it on or off while the camera is open reopens the camera, like a camera switch: the virtual camera holds the last frame and keeps its format.
  The **method** is the setting `effect.video.enhance_method` ("Enhancement method"), written with `omarchy-broadcast settings set effect.video.enhance_method ai|fast` or in the panel; it is greyed while enhancement is off, and a change reopens the camera as turning enhancement on or off does. It has no key in `config.toml`; like the other settings of a module it is kept in the state file.
  - `ai` ("AI super resolution", the default) is NVIDIA's AI video super resolution, the `nvvfxvideosuperres` feature.
  - `fast` ("Fast upscale (lower power)") is NVIDIA's non-AI upscale, the `nvvfxupscale` feature, at the SDK's default sharpening. It is lighter on the GPU and draws less power, for a picture with less restored detail than the AI method. It has no model files.
  - With neither method chosen the installed one runs, `ai` first. If `ai` is chosen (it is the default) and its feature is not installed while `nvvfxupscale` is, the fast method runs instead and the daemon's log says so. If `fast` is chosen and `nvvfxupscale` is not installed, the stage is refused with the reason (`enhance unavailable: ...`) and is never replaced by the costlier AI method. The stage is shown as "Not installed" only when neither feature is.
- **HDR look** (`omarchy-broadcast settings set effect.video.hdr true`, or the "HDR look" switch in the panel; off by default) gives the picture the look of NVIDIA's TrueHDR. TrueHDR converts the picture to HDR10 (the PQ transfer, BT.2020 primaries, 10 bits); omarchy-broadcast then converts the result back to an ordinary 8-bit SDR picture (a tone map that keeps the mid-tones where the camera had them, rolls the highlights off to SDR white and keeps the colour), so the virtual camera, the PipeWire node and the preview all show it, in any application. It gives a picture with more contrast in the highlights and shadows and richer colour; it is a matter of taste, and with the switch off nothing is loaded. It needs the `nvvfxtruehdr` feature, which has no model files; without it the setting is greyed "Not installed". It runs on the finished picture, after the cut-out, the blur or the replacement, and relighting, and before frame generation.
  - **No HDR output.** The output is 8-bit SDR and no application receives HDR. A real 10-bit HDR output is not built: no application that reads the camera on the reference system would show it as HDR ([ADR-0030](../adr/0030-hdr-look-and-truehdr.md)).
  - **Banding.** NVIDIA's debander is on (the SDK default), and it limits banding in smooth gradients of an 8-bit source (a plain wall, a soft shadow). Banding may still show there. If you see it, turn the look off.
  - **It fails open.** If the look cannot load or fails on a frame, the picture goes out without it; the cut-out is not affected.
- **Chair.** The setting `effect.cutout.chair` ("Hide the chair with the background", in the "Cut-out" group; off by default) chooses what the green screen does with a chair the person sits on. Off, the chair is cut out with the person (NVIDIA's "chair as foreground" segmentation); on, it is treated as part of the background, so `blur` blurs it and `replace` covers it ("chair as background"). It applies to `blur` and `replace` (and to the relighting that uses the same mask), is greyed while the effect is `none`, and has no key in `config.toml`. Only the SDK's quality modes of the green screen are used. A change loads the green screen again in the other mode: the new one is built before the old one is released, so the previous cut-out keeps running until it is ready, and if the new one cannot be built the previous chain keeps running. The cut-out hides the room, so a failure here follows `[effect] on_failure` like any other failure of the effect and never lets the camera image through.
- **Cost.** Each stage adds a GPU pass per frame and its models' GPU memory while it is on, and nothing while it is off. Measure on your machine with `obc-bench --effect blur --denoise --relight` ([../testing.md](../testing.md)); the figures are not published.

The background image for `replace` must be a PNG, JPEG or still WebP file. WebP needs the GStreamer WebP decoder (`webpdec`, in gst-plugins-bad), an optional package dependency: without it a WebP file is refused with a reason that names it. An animated WebP is refused. Until an effect is ready, the virtual camera shows black rather than the unprocessed camera image; if the SDK cannot be loaded, the virtual camera keeps showing black (the default `on_failure = "black"`; the camera stays open and the daemon's `Warning` property says why) instead of the unprocessed image. The design is in [ADR-0012](../adr/0012-video-effects-and-maxine-backend.md). Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce locally with `obc-bench` (see [../testing.md](../testing.md)).

To check the SDK installation itself, use the check commands in the "Check the installation" section of `docs/usage.md` in the omarchy-nvidia-sdks repository, and its own tests. omarchy-broadcast has no probe of its own.

## 7. Troubleshooting

Problems with the SDK itself (download, package, architecture) are in omarchy-nvidia-sdks. The SDK status codes `-1209` ("no kernel image is available"), `-2` (unimplemented, a feature library not found) and `-13` ("file could not be found", the model directory) are explained in the Troubleshooting section of its `docs/usage.md`. The rows below are the ones that are specific to omarchy-broadcast.

| Symptom | Cause | Fix |
|---|---|---|
| The daemon's `Warning` property reads `effect blur unavailable: NvVFX_Load(GreenScreen) returned -1209 (...); the video is black until it can run`, naming the os-release override (this page, section 3) (`Error`, with the state `error`, before interface revision 11) | The daemon runs without the shim: the drop-in is not installed, `daemon-reload` and a restart were skipped, or its paths do not match your installation. With the default `on_failure = "black"` the virtual camera stays black | Install the drop-in (section 3) and check `systemctl --user show omarchy-broadcastd -p Environment`; set `MAXINE_OS_RELEASE_DEBUG=1` to see the redirect. A `cannot be preloaded` line in `journalctl --user -u omarchy-broadcastd` means the library path in `LD_PRELOAD` is wrong or the package is not installed |
| The unit fails to start after the drop-in was installed, and `journalctl --user -u omarchy-broadcastd` shows `Failed to load environment files: No such file or directory` (systemd does not name the file) | `EnvironmentFile=` has no `-`, so a missing file is an error. `maxine-vfx-sdk-local` is not installed | Check that `/usr/share/maxine-vfx/maxine-vfx.env` exists, which it does when the package is installed. Install the package (section 2), or remove the drop-in (section 8) |
| The drop-in has no effect: the daemon's environment lacks `LD_PRELOAD` | The file is not at `~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf` (the directory name must match the unit), or `daemon-reload` was skipped | Check the path, run `systemctl --user daemon-reload`, restart, and check with `systemctl --user cat omarchy-broadcastd` |
| An effect fails with a message that the SDK is already loaded from another place and asks for a restart | `sdk_root`, `[effect] maxine_root` (or `MAXINE_VFX_ROOT`) changed after the SDK was first loaded in the running daemon | `systemctl --user restart omarchy-broadcastd` (section 4) |
| An effect reports that a feature library is not found (`-2`) or the package lacks a feature | The package was built without that feature | Rebuild `maxine-vfx-sdk-local` with the features of section 1 that you use (section 2), reinstall it and restart the daemon. The drop-in needs no edit |
| `omarchy-broadcast modules` says `No effect modules installed`, or the settings have no effect group | The Maxine module's manifest is not in a `modules.d` directory: the package is not installed, or the checkout was not installed with `scripts/install-user.sh` (or the module was not built and installed by hand) | Install the package, or run `scripts/install-user.sh` in the checkout (see "From a checkout" under [The Maxine module](#the-maxine-module)); then `systemctl --user restart omarchy-broadcastd` |
| The module is listed `unavailable`, with a reason such as `The library /... is not installed` or `Built for module ABI N; this daemon supports module ABI 1` | The library named by the manifest's `library` is missing, or was built for another version of the module interface than this daemon's | Check the paths with `omarchy-broadcast modules info br.com.dias.maxine-vfx`; rebuild and reinstall the module from the same checkout as the daemon |
| The module is `disabled` with `Disabled after crashing the daemon twice in a row` | The module's code was running when the daemon went down twice in a row (see the journal for the crash) | Fix the cause (the SDK installation and the shim of section 3 are the usual ones), then `omarchy-broadcast modules enable br.com.dias.maxine-vfx` |
| A setting such as `effect.lighting.relight`, `effect.video.denoise` or `effect.video.hdr` is greyed "Not installed" | The package has no library, or no models for this GPU's architecture, for that feature (relighting also needs `nvvfxgreenscreen`; the HDR look needs `nvvfxtruehdr`, which has no model files) | `ls /opt/nvidia/maxine/videofx/features` and `ls /opt/nvidia/maxine/videofx/lib/models`; rebuild the package with the feature and your architecture (section 2). The daemon looks again at its next start, reload and session |
| The daemon's `Error` says `relighting unavailable: ...` or `enhance unavailable: ...`, and the video is shown without it | The feature failed to load or run (the reason says which) | The reason names the SDK's status; check section 3 (the shim applies to every feature's library) and the models. The video is not affected otherwise |
| The daemon's `Error` says `noise removal unavailable: ... -2` (NVCV status), and the camera image is shown without noise removal | The package does not contain the `nvvfxdenoising` feature, or the daemon found no SDK root (section 4) and so looked for it on the library search path only | Check that `MAXINE_VFX_FEATURES` in `/usr/share/maxine-vfx/maxine-vfx.env` lists `nvvfxdenoising`; rebuild the package with it if not (section 2); check the drop-in |
| The same, with `-1209` or `-13` | The shim is not applied to this process (the feature's library needs it too), or the `Denoise-*` models for your architecture are not in the package | Section 3; `ls /opt/nvidia/maxine/videofx/lib/models` must show `Denoise-agg-<SM>.engine.trtpkg` and `Denoise-con-<SM>.engine.trtpkg`; rebuild the package for your architecture otherwise |
| The same, with a message that the frame size is out of range | The output contract is smaller than 80x80 or larger than 1920x1080 | Use an output mode within the range (`[output]` in `config.toml`); noise removal does not run outside it |
| The daemon's `Warning` property (`Error` before interface revision 11) or the element's status names the mask-hardening kernel and a CUDA error `222` ("the NVIDIA driver is older than this build supports") | The driver rejects the PTX of the mask-hardening kernel: it is older than 570.26 | Update the NVIDIA driver to 570.26 or newer (`nvidia-smi --query-gpu=driver_version --format=csv,noheader`). With the default `on_failure = "black"` the virtual camera stays black until then |
| The same, with CUDA error `221` | The driver cannot JIT-compile the kernel: `libnvidia-ptxjitcompiler.so.1` is missing from the driver installation | Reinstall the driver's userspace package (`nvidia-utils`) so that it provides `libnvidia-ptxjitcompiler.so.1`; check with `ldconfig -p \| grep ptxjitcompiler` |

## The Maxine AR module

Two more effects run on the NVIDIA Maxine **AR** SDK, through a second module of omarchy-broadcast, `br.com.dias.maxine-ar` ([ADR-0028](../adr/0028-maxine-ar-module.md)): **eye contact**, which redirects the eyes towards the camera, and **auto-framing**, a digital pan and zoom that keeps the face in frame. The AR SDK is a separate personal download from the Video Effects SDK; omarchy-nvidia-sdks packages it as `maxine-ar-sdk-local`. Everything on this page about not distributing the SDK (section 9) applies to it too.

The module is the library `libobc_module_maxine_ar.so` and the manifest `maxine-ar.toml`. The Arch package installs it next to the Maxine module and lists `maxine-ar-sdk-local` as an optional dependency; `scripts/install-user.sh` installs it for the current user in the same way as the Maxine module (see "From a checkout" under [The Maxine module](#the-maxine-module)). It opens the SDK at run time, so it installs and loads without the SDK or an NVIDIA GPU. Without the SDK, or without the models the effect needs for your GPU, the effect's setting is greyed with the reason, for example `Maxine AR SDK not installed` or `No face detection models for this GPU (sm_120)`. Check it with `omarchy-broadcast modules` and `omarchy-broadcast modules info br.com.dias.maxine-ar`.

| Stage | Setting | SDK features and models needed |
|---|---|---|
| `eye-contact` | `effect.eye_contact.enabled` | `nvarfaceboxdetection`, `nvarlandmarkdetection` and `nvargazeredirection`, with the models for your GPU architecture: face detection, 126-point face landmarks and gaze redirection |
| `auto-frame` | `effect.auto_frame.enabled` | `nvarfaceboxdetection`, with its face detection models |

The GPU and driver requirements are those of section 1. The names are those of the SDK's feature directories under `features/`; the daemon checks what is installed from these directories and the files in `lib/models`, without waking the GPU. `MAXINE_AR_FEATURES` in `/usr/share/maxine-ar/maxine-ar.env` lists the features the package was built with.

**The SDK root.** The module takes it from the first of these that applies: `MAXINE_AR_ROOT` in the daemon's environment, when it is set and not empty (the package's environment file sets it); `sdk_root` in `[module."br.com.dias.maxine-ar"]` of `config.toml`; `/opt/nvidia/maxine/arsdk`, when that is a directory. Use an absolute path:

```toml
[module."br.com.dias.maxine-ar"]
sdk_root = "/home/user/.local/opt/maxine/arsdk"
```

As for the Video Effects module, the environment file overrides the key, `LD_LIBRARY_PATH` is not needed (the module loads the SDK's libraries from the root by absolute path), and the SDK is loaded once per process: a different root takes `systemctl --user restart omarchy-broadcastd`. The two modules have separate roots, set separately. Any other key in the table is refused.

**The os-release shim.** The AR SDK's feature libraries read `/etc/os-release` as the Video Effects SDK's do, so the daemon needs the shim described in section 3. The shim is process-wide: a daemon that already runs with the shipped drop-in (from `maxine-vfx-sdk-local`) needs nothing more for the AR module. The shipped drop-in names the Video Effects package's files, and the environment file it loads is that package's. If only `maxine-ar-sdk-local` is installed, write a drop-in of your own at `~/.config/systemd/user/omarchy-broadcastd.service.d/maxine-ar.conf` with the AR package's files, then `daemon-reload` and restart as in section 3:

```ini
[Service]
EnvironmentFile=/usr/share/maxine-ar/maxine-ar.env
Environment=LD_PRELOAD=/usr/lib/maxine-ar/os-release-shim.so
```

The environment file sets `MAXINE_AR_ROOT` and `MAXINE_OS_RELEASE` (so, as above, `sdk_root` is then ignored) and never `LD_PRELOAD`, which the drop-in sets. Without the package's environment file, the two lines that matter are `Environment=LD_PRELOAD=/usr/lib/maxine-ar/os-release-shim.so` and `Environment=MAXINE_OS_RELEASE=/usr/lib/maxine-ar/fake-os-release`.

**Both SDKs in one process.** The two SDKs each ship a `libNVCVImage` of the same soname, and bundle the same major versions of CUDA and TensorRT. Using the Video Effects module and the AR module in one daemon is tested, with either SDK loaded first ([../testing.md](../testing.md#maxine-ar-module-tests-ignored)).

**Using the effects.** Both are off by default, and are in the settings tree (the bar's panel and `omarchy-broadcast settings`):

- **Eye contact** (`effect.eye_contact.enabled`). It redirects the eyes of the largest face in view towards the camera. With no face the picture is unchanged. There is no strength control: the SDK has none, and its other parameters are left at the SDK's defaults. **Natural look-away** (`effect.eye_contact.look_away`, applies on reload) lets the eyes leave the camera now and then, as a person's do.
- **Auto-framing** (`effect.auto_frame.enabled`). It crops the frame around the largest face and scales the crop back to the frame's size, so the face stays centred as the person moves. **Framing** (`effect.auto_frame.framing`: Tight, Medium or Wide, live) sets how close the crop is. The zoom is capped at 1.5 times. Small head movements do not move the picture, the motion is smoothed, and when the face is lost the crop holds for about two seconds and then eases back to the full frame. It zooms the picture after the camera image has been scaled to the output size, so a zoomed picture shows less detail than the camera captured.
- **Order.** They run before noise removal, the cut-out and relighting, eye contact first and auto-framing second by default; `omarchy-broadcast order` reorders them within that part of the pipeline.
- **They fail open.** If a stage cannot load or run, it is dropped with a `Warning` and the video goes on without it.

If the stage is greyed or does not run, the reason is the one `omarchy-broadcast modules` shows. The usual ones are in this table; the rows of section 7 about the shim and the unit apply to this module too.

| Reason shown | Cause | Fix |
|---|---|---|
| `Maxine AR SDK not installed` | No SDK root was found, or it lacks the core libraries | Install `maxine-ar-sdk-local`, or set `sdk_root` or `MAXINE_AR_ROOT`; restart the daemon |
| `Maxine AR face detection not installed`, `Maxine AR landmark detection not installed` or `Maxine AR eye contact not installed` | The package was built without that feature | Rebuild `maxine-ar-sdk-local` with the feature (the table above), reinstall it and restart the daemon |
| `No face detection models for this GPU (sm_120)` (or `126-point face landmark`, or `Eye Contact`) | The package has no models of that kind for your GPU architecture | `ls /opt/nvidia/maxine/arsdk/lib/models`, and rebuild the package for your architecture |
| The stage is available but is dropped, and the `Warning` names an SDK status | The shim is not applied to the daemon (the feature libraries read `/etc/os-release`), or the models are not found | The shim paragraph above and section 3; `MAXINE_OS_RELEASE_DEBUG=1` shows the redirect |

To remove the module's drop-in, delete `maxine-ar.conf` as in section 8. The AR package is removed with `sudo pacman -R maxine-ar-sdk-local`, only if nothing else uses it.

## The Maxine audio effects module

The microphone's GPU effects run on the NVIDIA Maxine **Audio Effects** SDK, through a third module of omarchy-broadcast, `br.com.dias.maxine-afx` (an `audio-filter` module: [../modules.md](../modules.md#the-audio-modules-in-this-repository), [ADR-0029](../adr/0029-audio-modules-mic-host-and-worker.md)). The Audio Effects SDK is a separate personal download from the Video Effects and AR SDKs; omarchy-nvidia-sdks packages it as `maxine-afx-sdk-local`. Section 9 applies to it too.

It differs from the two video modules in three ways, which is why it is not described with them:

- **The audio worker loads it, not the daemon.** The worker is the unit `omarchy-broadcast-audiod.service`, so its drop-in is `omarchy-broadcast-audiod.service.d/maxine-afx.conf`, not the daemon's. The daemon never opens the audio SDK, and the audio drop-in is not needed for the camera, nor the camera's for the microphone. A machine can have any of the three drop-ins.
- **`LD_LIBRARY_PATH` is required.** The SDK opens its effects' feature libraries by a name that only the library search path resolves, read when the process starts, so (unlike the video modules, which load their libraries by absolute path) the module requires the path to lead to the feature libraries of the root in use. The package's environment file sets it. Without it the stages are unavailable, with a reason that names the drop-in.
- **The root is searched in the other order.** `sdk_root` in `[module."br.com.dias.maxine-afx"]`, then `MAXINE_AFX_ROOT`, then `/opt/nvidia/maxine/afx`: the configured root comes first, whereas for the video modules the environment variable comes first (section 4).

The os-release shim of section 3 applies to the worker in the same way, set by the drop-in on the worker only. The Arch package installs the module and ships the drop-in as an inactive example (`/usr/share/doc/omarchy-broadcast/examples/systemd/omarchy-broadcast-audiod.service.d/maxine-afx.conf`); `scripts/install-user.sh --audio` installs the module and prints the command that copies the drop-in. The module installs and loads without the SDK, and without the SDK or the drop-in the microphone uses its CPU module.

What the effects do, the settings in the Mic tab, studio voice's exclusion of noise and echo removal and its delay, the choice of provider, enabling the drop-in and troubleshooting are in [microphone.md](microphone.md#on-an-nvidia-gpu-maxine-audio-effects). Check the module with `omarchy-broadcast modules` and `omarchy-broadcast modules info br.com.dias.maxine-afx`, which show each stage's reason.

## 8. Uninstall

```bash
# remove the drop-in, then reload and restart
rm -f ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf
systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd.service
# the SDK package, only if nothing else uses it
sudo pacman -R maxine-vfx-sdk-local
```

Without the drop-in the daemon runs as usual and the effects are unavailable. To remove the Maxine module of a checkout installed with `scripts/install-user.sh`, run `scripts/install-user.sh --uninstall`: it removes the module's library and manifest, and the directories among them once they are empty. A module installed by hand (the recipe under "From a checkout") has a manifest without the script's mark, and the script refuses to remove it. Delete that manifest and the library it names first, then run the uninstall again:

```bash
rm -f ~/.local/share/omarchy-broadcast/modules.d/maxine-vfx.toml ~/.local/lib/omarchy-broadcast/modules/libobc_module_maxine.so
```

Restart the daemon after either. The packaged module goes with the omarchy-broadcast package, and `omarchy-broadcast modules disable br.com.dias.maxine-vfx` turns it off without removing it.

An os-release shim installed by `scripts/install-user.sh` of an older omarchy-broadcast release, under `~/.local/lib/omarchy-broadcast/`, is removed by `scripts/install-user.sh --uninstall` of the current script, and by reinstalling with it. To remove it by hand: `rm -f ~/.local/lib/omarchy-broadcast/os-release-shim.so ~/.local/lib/omarchy-broadcast/fake-os-release`.

The audio module's drop-in is removed the same way: delete `~/.config/systemd/user/omarchy-broadcast-audiod.service.d/maxine-afx.conf` and run `systemctl --user daemon-reload`; the worker starts without the SDK from its next start. The package is `maxine-afx-sdk-local`. The module's library and manifest of a checkout are removed by `scripts/install-user.sh --uninstall`, and the packaged ones go with the omarchy-broadcast package; `omarchy-broadcast modules disable br.com.dias.maxine-afx` turns the module off without removing it.

The NGC CLI, the NGC key and the downloads belong to omarchy-nvidia-sdks; its documentation covers removing them.

## 9. Licence and support

This is a summary, not legal advice. Read the licence you accept.

- The SDK and its models are NVIDIA's, under NVIDIA's licence. You download them with your own NVIDIA account and accept that licence yourself.
- Neither omarchy-broadcast nor omarchy-nvidia-sdks redistributes NVIDIA files. Do not commit the SDK, the models, an NGC key or `~/.ngc/` to a repository, and do not put them in a package or release artifact.
- Benchmark results for the SDK are not published (SLA 8.9).
- Running the SDK on a distribution it does not list, through the compatibility shim, is not supported by NVIDIA.

The README of omarchy-nvidia-sdks has that repository's own licence notes.
