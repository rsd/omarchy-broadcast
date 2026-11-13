# omarchy-broadcast

A webcam-effects pipeline for Linux, with GPU acceleration on NVIDIA hardware. Frames flow from a camera through GStreamer, in CUDA or system memory, into a [v4l2loopback](https://github.com/v4l2loopback/v4l2loopback) virtual camera that any application can select. The effects (background blur and background replacement), video noise removal, relighting of the person and enhancement of a low-resolution camera are provided by **modules**: a manifest and a shared library behind a versioned C ABI, which the daemon discovers at run time and which can be built and distributed separately ([docs/modules.md](docs/modules.md)). The Maxine module, which uses the NVIDIA Maxine Video Effects SDK, is the first; with no module installed the daemon passes the camera image through.

Optionally, the daemon also offers the camera as a native PipeWire video source, "Omarchy Broadcast", next to the loopback device, for applications that use PipeWire for cameras ([docs/setup/daemon.md](docs/setup/daemon.md#the-pipewire-output)).

Any UVC webcam works: the pipeline picks a capture mode from what the camera advertises, and adds a CPU decode step for MJPEG cameras and a CPU convert step for YUYV/UYVY.

## Status

Early development. The camera -> CUDA -> virtual camera passthrough works, with sustained 1080p60 on the reference machine. The `omarchy-broadcastd` daemon (a systemd user service controlled over D-Bus) and the `omarchy-broadcast` command-line client run it on demand: the camera and GPU start when an application opens the virtual camera ([docs/setup/daemon.md](docs/setup/daemon.md)). Background blur and background replacement run on the GPU through the Maxine module and the Maxine SDK, which each user installs ([docs/setup/maxine.md](docs/setup/maxine.md)); they are selected with `omarchy-broadcast effect` and add a small GPU cost per frame. Video noise removal, which removes the grain a webcam shows in dim rooms, is a separate switch that combines with either effect or runs alone: `omarchy-broadcast denoise on|off` ([docs/adr/0018-video-noise-removal.md](docs/adr/0018-video-noise-removal.md)). It adds one GPU pass per frame while it is on, and it fails open: if it cannot run, the stream continues without it. Two more stages stack the same way and fail open too ([docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md](docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md)): relighting lights the person with a warm, neutral or cool key light whose direction and brightness can be adjusted, and the background behind them is never relit; and enhancement upscales a camera that captures well below the output size (for example a 960x540 or 640x360 mode into 1080p) before it is fitted to the output, so it is sharper than scaling alone. All of them, and the effect itself, are settings of the same tree as the camera's (`omarchy-broadcast settings`, `effect.*`), and a stage whose SDK feature is not installed is shown as not installed. Blur and replace harden the segmentation mask edge, so the room does not show through around the subject ([docs/adr/0013-mask-hardening-cuda-kernel.md](docs/adr/0013-mask-hardening-cuda-kernel.md)). Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce locally with `obc-bench` (see [docs/testing.md](docs/testing.md)). Other measurements are in [docs/benchmarks/](docs/benchmarks/passthrough.md).

If the daemon crashes, applications that hold the virtual camera keep it: the daemon leaves a black timeout image and a `timeout` on the loopback, so Chromium-based applications get black frames until the restarted daemon writes again, and the unit restarts it after 250 ms. Known limitation: consumers of the native PipeWire node do not reconnect after a crash; use the loopback device where that matters ([docs/adr/0022-crash-recovery-for-consumers.md](docs/adr/0022-crash-recovery-for-consumers.md), [docs/setup/daemon.md](docs/setup/daemon.md#restarts-and-crashes)).

The effects are modules ([docs/adr/0025-loadable-modules.md](docs/adr/0025-loadable-modules.md), D-Bus revision 9 and later; the interface is at revision 19). The Arch package installs the Maxine module as `/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so`, with its manifest in `/usr/lib/omarchy-broadcast/modules.d/`; it loads the SDK at run time, so it installs without it. `omarchy-broadcast modules` lists the modules found, with their status, the reason when one is unavailable, and the stages each provides; `modules enable|disable ID`, `modules profile quality|battery` and `modules pin STAGE MODULE` choose which module provides a stage (applied when the camera next starts; a disabled module's stages leave the running stream at once, a selected background effect that no module can run shows black frames, or the fallback image with `on_failure = "image"`, not the camera image, which only `on_failure = "none"` shows, and any other stage that no module provides is left out with a warning), and `omarchy-broadcast order` shows and sets the order the stages run in, within the limits each module declares. The daemon writes these choices, and the settings the bar's setup dialog changes (`omarchy-broadcast config`), to `~/.config/omarchy-broadcast/config.d/50-panel.toml`, which it manages; `config.toml` stays yours, a later file such as `config.d/90-local.toml` wins over the managed one, and the daemon refuses to change a key such as that ([docs/adr/0026-config-d-and-managed-config.md](docs/adr/0026-config-d-and-managed-config.md)). The Arch package also installs the Maxine AR module (`br.com.dias.maxine-ar`, which needs the NVIDIA Maxine AR SDK as `maxine-ar-sdk-local`), which adds two effects, off by default: **Eye contact**, which redirects the eyes towards the camera, and **Auto-framing**, which pans and zooms digitally to keep the face in frame ([docs/setup/maxine.md](docs/setup/maxine.md#the-maxine-ar-module), [docs/adr/0028-maxine-ar-module.md](docs/adr/0028-maxine-ar-module.md)). The Arch package `omarchy-broadcast-onnx-git` adds the open-model cut-out (`br.com.dias.onnx-cutout`): the same `background` stage with open Apache-2.0 models run by ONNX Runtime, so the background effect needs no NVIDIA SDK; it is preferred by the `battery` profile, Maxine by `quality`, and it also runs in the system-memory pipeline without an NVIDIA GPU ([docs/setup/onnx.md](docs/setup/onnx.md), [docs/adr/0031-onnx-cutout-module.md](docs/adr/0031-onnx-cutout-module.md)). A module's settings appear in `omarchy-broadcast settings` and in the bar panel with no change to them. Someone can write a module with the Rust SDK or against the C header and install it in `~/.local/share/omarchy-broadcast/modules.d/`: [docs/modules.md](docs/modules.md) is the author guide and `examples/module-hello` a complete example. The Maxine SDK root is the module's setting, `sdk_root` in `[module."br.com.dias.maxine-vfx"]` (`[effect] maxine_root` still works).

```sh
omarchy-broadcast modules                      # the modules, their status and stages
omarchy-broadcast modules profile battery      # prefer an NPU or the CPU over a GPU, where a module offers one
omarchy-broadcast order                        # the order the stages run in
```

The microphone is a feature of its own: **Omarchy Broadcast Mic**, a virtual microphone with noise removal that any application can select, produced by audio modules (a CPU module with DeepFilterNet, the default, and RNNoise ships, and a module for NVIDIA GPUs with room echo removal and studio voice besides noise removal runs on the NVIDIA Maxine Audio Effects SDK, which you obtain yourself; other engines can be added as modules). It runs apart from the video daemon, as a stable PipeWire node and an audio worker, so that whatever stops, applications keep receiving your microphone, unprocessed, and are never left silent. It is off until you turn it on, makes itself the default microphone only when you ask, and restores the previous default when you ask or turn it off: `omarchy-broadcast mic on`, `mic default`, `mic restore-default`, `mic off` ([docs/setup/microphone.md](docs/setup/microphone.md), [docs/adr/0029-audio-modules-mic-host-and-worker.md](docs/adr/0029-audio-modules-mic-host-and-worker.md), D-Bus revision 13). The older single-engine filter-chain units remain as a lightweight alternative.

Some cameras keep image settings from other software far from their defaults (oversharpened, oversaturated). `omarchy-broadcast controls` shows a camera's brightness, contrast, saturation, sharpness and backlight compensation, `omarchy-broadcast controls set NAME VALUE` changes one and `omarchy-broadcast controls reset` puts them back to the camera's defaults. The daemon keeps the values per camera and writes them whenever that camera opens ([docs/adr/0020-camera-image-controls.md](docs/adr/0020-camera-image-controls.md)).

Beyond those five, `omarchy-broadcast settings` lists every standard setting the camera offers, by group (image, white balance, exposure, focus and zoom, anything else), with its value, what it takes and, for one that cannot be changed now, why: exposure time is greyed with "Set exposure to manual to change this" while exposure is automatic. The daemon works the rules out from the camera's own flags, so no client carries a list of controls. On a Logitech Brio or MX Brio the field of view (90, 78 or 65 degrees) is among them. The values are stored per camera and written whenever the camera opens, as above.

```sh
omarchy-broadcast settings                                        # show them all
omarchy-broadcast settings set camera.exposure.auto_exposure manual
omarchy-broadcast settings set camera.exposure.exposure_time_absolute 400
omarchy-broadcast settings set camera.exposure.keep_framerate on  # keep 30 fps in low light
omarchy-broadcast settings set -- camera.focus.pan_absolute -3600
omarchy-broadcast settings reset --group=exposure                 # or --setting=ID, or nothing for the whole camera
omarchy-broadcast settings set effect.lighting.relight on          # the video effects, for every camera
omarchy-broadcast settings set effect.lighting.light warm
omarchy-broadcast settings set -- effect.lighting.direction -30
```

A change applies at once while the camera is in use, and otherwise at the next time it opens. While the daemon has the camera open it also follows changes other software makes to it, with no polling. From D-Bus revision 8 the tree also holds the video effects (`effect.*`, scope `global`: the same for every camera, present with no camera, never reset by a camera reset). The tree, the D-Bus revision 7 and 8 members and the `watch --json` lines that carry it are described in [docs/adr/0023-settings-schema.md](docs/adr/0023-settings-schema.md), [docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md](docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md), [docs/dbus-api.md](docs/dbus-api.md#camera-settings) and [docs/cli-json.md](docs/cli-json.md#camera-settings).

The camera's resolution and frame rate are the first settings of the list (`camera.capture.mode`, from D-Bus revision 12): `auto` or one of the sizes and rates the camera reports, kept per camera and applied at once. The size and rate of the virtual camera are `output.mode`, a choice computed from the camera that `omarchy-broadcast config set output.mode 1280x720@30` or the setup dialog changes; while an application holds the virtual camera the change waits for it, and `Warning` says for whom, and a format that cannot start reverts to one that works ([docs/setup/loopback.md](docs/setup/loopback.md#changing-the-output-size-and-rate)). With the Maxine module and a camera at half the output rate (a 30 fps camera into a 60 fps output), the output can also be made at twice the camera's rate with video frame generation, one output tick later ([docs/adr/0027-output-modes-frame-generation-and-privacy-fallback.md](docs/adr/0027-output-modes-frame-generation-and-privacy-fallback.md)). When an effect cannot run, `[effect] on_failure` chooses what the virtual camera shows: black (the default), your own still image (`image`, with `fallback_image` set in the configuration file) or the camera (`none`).

## Requirements

- GStreamer 1.x with the base/good/bad plugin sets (`videoconvert`, `jpegdec`)
- Optional, for the GPU pipeline and the NVIDIA effects: an NVIDIA GPU with the proprietary driver, the CUDA toolkit (for `nvrtc`) and GStreamer's `nvcodec` plugin (`cudaupload`, `cudaconvert`, `cudadownload`). Without an NVIDIA GPU the daemon runs its system-memory pipeline ([docs/adr/0033-system-memory-pipeline.md](docs/adr/0033-system-memory-pipeline.md)): the camera passthrough, the open-model cut-out on the CPU, an Intel GPU or an NPU through ONNX Runtime, and the CPU microphone module need no CUDA. The Maxine video effects, the Maxine AR module (eye contact, auto-framing), the Maxine audio module and the CUDA frame path need an NVIDIA GPU
- v4l2loopback, configured as described in [docs/setup/loopback.md](docs/setup/loopback.md)
- Rust toolchain (only to build from source; the Arch package is built with it, and installed without it)
- Optional, for the native PipeWire camera output: a build with the cargo feature `omarchy-broadcastd/pipewire` (libpipewire headers and clang at build time; the Arch package has it) and `[pipewire] enabled = true` in the configuration
- Optional, for effects, noise removal, relighting and enhancement: the Maxine module (in the Arch package) and the NVIDIA Maxine VFX SDK, as the package `maxine-vfx-sdk-local` that you build yourself from the [omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) repository ([docs/setup/maxine.md](docs/setup/maxine.md)); the module finds the SDK in `/opt/nvidia/maxine/videofx` or at `sdk_root` (`[effect] maxine_root` is an alias), and `LD_LIBRARY_PATH` is not needed
- Optional, for the background effect without the NVIDIA SDK: the open-model cut-out (the package `omarchy-broadcast-onnx-git` built from `packaging/arch-onnx/`, or `scripts/install-user.sh` with `OBC_ONNX_MODELS_DIR`) and ONNX Runtime 1.29 or newer, from an Arch `onnxruntime-*` package (related repositories: [omarchy-onnxruntime](https://github.com/rsd/omarchy-onnxruntime) and [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models); [docs/setup/onnx.md](docs/setup/onnx.md))
- Optional, for video backgrounds: `ffmpeg`, for importing animated GIF, WebP and APNG files, for the posters of imported videos and, on a machine without an NVIDIA GPU, to run the command that converts a video the CPU cannot play (VP9 profile 0 in WebM, at most 1920x1080 and 30 fps, plays as it is; any other video shows its poster as a still until converted, see [docs/setup/daemon.md](docs/setup/daemon.md#playing-a-video-without-an-nvidia-gpu); the two bundled loops need none of it); a video background itself needs nothing more ([docs/dbus-api.md](docs/dbus-api.md#video-backgrounds-revision-18), [docs/adr/0035-video-backgrounds.md](docs/adr/0035-video-backgrounds.md))
- Optional, for eye contact and auto-framing: the Maxine AR module (in the Arch package) and the NVIDIA Maxine AR SDK, as the package `maxine-ar-sdk-local` built from the [omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) repository ([docs/setup/maxine.md](docs/setup/maxine.md#the-maxine-ar-module)); the module finds the SDK in `/opt/nvidia/maxine/arsdk` or at `sdk_root` in `[module."br.com.dias.maxine-ar"]`
- Optional, for the microphone with noise removal: nothing more for the CPU module in the Arch package, which installs it with the mic host and the audio worker ([docs/setup/microphone.md](docs/setup/microphone.md)); for the GPU module, an NVIDIA GPU and `maxine-afx-sdk-local` with the worker's drop-in ([docs/setup/microphone.md](docs/setup/microphone.md#on-an-nvidia-gpu-maxine-audio-effects)); only the older filter-chain units need `deepfilternet-ladspa` or `noise-suppression-for-voice`

## Build

```sh
cargo build --release
```

The PipeWire output is a cargo feature that is off by default: `cargo build --release --features omarchy-broadcastd/pipewire`.

## Install

Three routes; the daemon, the unit files and the other steps are described in [docs/setup/daemon.md](docs/setup/daemon.md). None of them configures the v4l2loopback device or installs the Maxine SDK.

### Arch package

`packaging/arch/PKGBUILD` builds `omarchy-broadcast-git` from the committed HEAD of the local checkout. Build from `packaging/arch`:

```sh
cd packaging/arch
makepkg -s
sudo pacman -U omarchy-broadcast-git-*.pkg.tar.zst
systemctl --user enable --now omarchy-broadcastd
```

The package installs the daemon and the client in `/usr/bin`, the Maxine module and its manifest under `/usr/lib/omarchy-broadcast`, the microphone's parts (the audio worker, the mic host's unit and configuration, the forwarding plugin, the CPU voice module and the Maxine audio effects module, all inactive until `omarchy-broadcast mic on`), the C header of the module interface in `/usr/include/omarchy-broadcast`, the systemd user unit, the D-Bus activation file and the documentation. It enables nothing: each user runs the last command. The loopback device is still configured as in [docs/setup/loopback.md](docs/setup/loopback.md). The NVIDIA Maxine SDK is never packaged here; the package lists `maxine-vfx-sdk-local`, `maxine-ar-sdk-local` and `maxine-afx-sdk-local` (each built by the user from the [omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) repository) as optional dependencies, and [docs/setup/maxine.md](docs/setup/maxine.md) covers installing it and enabling the drop-in (section 3).

The open-model cut-out is a second package with its own `PKGBUILD`, `packaging/arch-onnx/`, built after the core and depending on it and on `onnxruntime>=1.29`. Its four model files are sources of that build, taken from a local directory until a release host is published: `cd packaging/arch-onnx && OBC_ONNX_MODELS_DIR=/path/to/models makepkg -s`, then `sudo pacman -U` of both packages ([docs/setup/onnx.md](docs/setup/onnx.md)). The core builds without them.

### Developer install

`scripts/install-user.sh` builds the release binaries and the NVIDIA Maxine module, and installs them for the current user, without root: the binaries in `~/.local/bin`, the unit and the D-Bus file under `~/.local/share`, and the Maxine module's library and manifest under `~/.local/share/omarchy-broadcast`. It does not install the Maxine SDK or the systemd drop-in that enables the effects; see [docs/setup/maxine.md](docs/setup/maxine.md) ("The Maxine module", and section 3). It never runs `systemctl`; it prints the next steps. `--dry-run` shows the actions and `--uninstall` removes only what the script installed.

```sh
scripts/install-user.sh
systemctl --user daemon-reload
systemctl --user enable --now omarchy-broadcastd
```

### Manual install

`cargo install` of the two crates and a copy of the unit file with `ExecStart=` rewritten: see [docs/setup/daemon.md](docs/setup/daemon.md#install).

The continuous integration (`.github/workflows/ci.yml`) runs the format check, clippy, the workspace tests, the example modules' own format check, clippy and tests (the video and the audio example are built outside the workspace, as third-party modules would be) and shellcheck in an Arch Linux container without a GPU, and a second job that builds, lints and tests `obc-pipeline` and `omarchy-broadcastd` with the PipeWire feature.

## Run

```sh
cargo run --release -p obc-pipeline --bin obc-bench -- --duration 0
```

This streams to the "Omarchy Broadcast" loopback device until Ctrl-C. Then select "Omarchy Broadcast" as the camera in OBS or Chrome.

Optional arguments:

- `--camera <name|vid:pid|/dev/videoN>` chooses the camera (default: automatic; a name or ID matching two colour cameras is an error, and greyscale IR nodes are never picked).
- `--mode WxH@FPS` or `FORMAT:WxH@FPS`, for example `--mode 1920x1080@30` or `--mode NV12:1920x1080@60`, chooses the capture mode (default: `1920x1080@60`, any format; the closest mode the camera offers is used).

Tests, including the manual check with OBS or Chrome, are described in [docs/testing.md](docs/testing.md).

## Documentation

See [docs/README.md](docs/README.md) for the architecture, decision records, setup guides and benchmarks, and [docs/modules.md](docs/modules.md) for writing and installing modules. The Omarchy bar widget (status icon, on/off, camera, effect and noise-removal selection) is a separate project, the [omarchy-broadcast-plugin](https://github.com/rsd/omarchy-broadcast-plugin) repository (being published shortly); it is a client of the daemon through `omarchy-broadcast watch --json` and is not installed by the package. Its camera settings panel (plugin 0.19.0) needs omarchy-broadcast 0.22.0 or newer, the first release with the settings channel ([docs/adr/0023-settings-schema.md](docs/adr/0023-settings-schema.md)); against an older daemon the plugin shows the cameras and asks for an update. The Effect tab draws the daemon's video-effect settings (blur strength, noise removal, enhancement, lighting) from the same tree, which needs the revision 8 daemon (omarchy-broadcast 0.23.0 or newer, plugin 0.20.0); against an older daemon it keeps the effect choice and noise removal as before. For a live preview of the virtual camera, the widget runs `omarchy-broadcast preview --json`, which holds a preview lease and prints a line per frame the daemon writes to a small JPEG file under `$XDG_RUNTIME_DIR` ([docs/cli-json.md](docs/cli-json.md#preview)); a preview counts as a user of the camera, so it starts it.

## Affiliation

omarchy-broadcast is not affiliated with or endorsed by NVIDIA. NVIDIA, Maxine and TensorRT are trademarks of NVIDIA Corporation.

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
