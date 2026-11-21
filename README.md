# omarchy-broadcast

**A virtual camera and a virtual microphone with live effects for Omarchy and Arch Linux.** Blur or replace your background, remove noise, fix the light and look into the lens, in any application that reads a camera or a microphone.

The daemon, `omarchy-broadcastd`, reads your webcam, applies the effects you choose and writes the result to a virtual camera. Chrome, OBS, Zoom and Discord select it like any other camera. Any UVC webcam works. UVC is the standard USB camera class.

**Install:** `yay -S omarchy-broadcast-bin omarchy-broadcast-onnx-bin`, then do the [other install steps](#install). An NVIDIA GPU is optional.

## What you get

**On any machine** (CPU, or an Intel GPU or NPU through ONNX Runtime):

- Background blur and background replacement with open Apache-2.0 models. A replacement can be an image, an animated image or a video; two loops and a set of backgrounds are bundled.
- Camera passthrough with the camera's own image settings: brightness, contrast, saturation, exposure, white balance, focus and zoom, set per camera.
- A choice of the camera's resolution and frame rate and of the virtual camera's size and rate.
- A virtual microphone, **Omarchy Broadcast Mic**, with noise removal on the CPU (DeepFilterNet by default, or RNNoise).

**With an NVIDIA RTX GPU** (the optional NVIDIA Maxine modules, with an SDK you obtain yourself):

- Background blur and replacement on the GPU, with a hardened mask edge.
- Video noise removal, which clears the grain a webcam shows in dim rooms.
- Relighting of the person with a warm, neutral or cool key light. You adjust its direction and brightness. The background is never relit.
- Enhancement, which upscales a camera that captures well below the output size.
- Eye contact and auto-framing. Eye contact redirects your eyes towards the camera. Auto-framing pans and zooms digitally to keep your face in frame. Both are off by default.
- Frame generation: an output at twice the camera's rate, for example 60 fps from a 30 fps camera.
- Room echo removal and studio voice for the microphone.

Each effect is a **module**: a manifest and a shared library that the daemon discovers at run time. With no module installed, the daemon passes the camera image through. You can [write your own module](docs/modules.md).

## Why use it

- **It runs on your machine.** The daemon processes video and audio locally. The open-model path needs no NVIDIA SDK.
- **It works in any application.** The virtual camera is a [v4l2loopback](https://github.com/v4l2loopback/v4l2loopback) device. An optional PipeWire camera node, "Omarchy Broadcast", serves several applications at once.
- **It leaves the GPU alone when you do not use it.** The camera and GPU start when an application opens the virtual camera. Idle means no CUDA context. On a battery, the daemon can keep the frames in system memory and not wake the NVIDIA GPU.
- **You control it from the Omarchy bar.** The [bar plugin](#the-bar-plugin) shows the camera's state and opens a panel with a live preview, the effects, the camera's settings and the microphone.
- **It fails safe.** If the daemon stops, applications keep receiving your microphone, unprocessed. If an effect cannot run, the virtual camera shows the camera as it is. Turn privacy on to show black frames, an away image or the last frame instead.

## Install

Install the core and one module. The virtual camera needs a v4l2loopback device, which the package does not set up. Do the steps in order.

1. **Install the packages from the AUR.** The `-bin` packages are prebuilt. The `-git` packages build from the latest source. They conflict with each other, so install one kind.

   ```sh
   yay -S omarchy-broadcast-bin omarchy-broadcast-onnx-bin     # prebuilt
   yay -S omarchy-broadcast-git omarchy-broadcast-onnx-git     # built from source
   ```

   Without an NVIDIA GPU, add `onnxruntime-cpu`. With one, add `cuda`. The packages do not include the NVIDIA SDKs or ONNX Runtime, which are licensed separately. Right after a release, an AUR helper can show the previous version for a few minutes. Details: [docs/setup/daemon.md](docs/setup/daemon.md#from-the-aur).

2. **Create the virtual camera device.** This needs root and runs once. If `/etc/modprobe.d` already has an `options v4l2loopback` line, merge the two lines as [docs/setup/loopback.md](docs/setup/loopback.md) describes.

   ```sh
   sudo install -Dm644 /usr/share/doc/omarchy-broadcast/examples/modprobe.d/omarchy-broadcast.conf /etc/modprobe.d/omarchy-broadcast.conf
   echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
   sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback
   ```

   The reload fails with "Module v4l2loopback is in use" while an application holds a loopback node. Close it and run the reload again.

3. **Start the daemon at every login.**

   ```sh
   systemctl --user enable --now omarchy-broadcastd
   ```

4. **Turn on the microphone (optional).**

   ```sh
   omarchy-broadcast mic on
   ```

On a first install the package prints these steps. On an upgrade it prints one line: `systemctl --user try-restart omarchy-broadcastd`.

### Check that it works

```sh
omarchy-broadcast start
```

`start` starts the daemon through D-Bus activation if it is not running, and prints its status. It changes no setting and does not turn the camera on. Expect a block of lines that includes your camera:

```text
State:      idle
Camera:     <your camera>
Consumers:  0
```

`State` is `idle` while no application uses the camera and `active` while one does. It is `off` if you turned the camera off: run `omarchy-broadcast on`. Now select **Omarchy Broadcast** as the camera in OBS or Chrome and choose an effect:

```sh
omarchy-broadcast effect blur
omarchy-broadcast effect replace      # with no background set, uses the first bundled background
```

`effect replace --image FILE` or the bar panel chooses another background. The effect list and a few more commands are below. All of them are in [docs/cli-json.md](docs/cli-json.md).

```sh
omarchy-broadcast modules                      # the modules, their status and stages
omarchy-broadcast modules profile battery      # prefer an NPU or the CPU over a GPU, where a module offers one
omarchy-broadcast denoise on                   # video noise removal
omarchy-broadcast settings                     # every setting of the camera and of the effects
omarchy-broadcast settings set camera.exposure.auto_exposure manual
omarchy-broadcast settings reset --group=exposure   # or --setting=ID, or nothing for the whole camera
omarchy-broadcast settings set effect.lighting.relight on
omarchy-broadcast mic default                  # make Omarchy Broadcast Mic the default microphone
```

### Choose your modules

The core installs the daemon, the CLI, the units, the CPU voice module and the microphone's parts. Each module is a package of its own, so you install only what you use. Add `-bin` or `-git` to the name, the same kind as the core.

| Package | What it adds | Depends on |
|---|---|---|
| `omarchy-broadcast` | The core: the daemon and the CLI, the audio worker, the CPU voice module, the microphone's host and plug-in, the units, the D-Bus files, the udev rule, the media and the documentation. The CPU video path is part of the daemon | optional: `nvidia-utils` and `cuda`, for the CUDA video path on NVIDIA GPUs |
| `omarchy-broadcast-onnx` | The open-model cut-out module (blur and replace, no NVIDIA SDK). Models are separate packages | `onnxruntime>=1.29`, `omarchy-onnx-model-mediapipe-landscape`; optional: the other `omarchy-onnx-model-*` packages and an `onnxruntime-provider-cuda`, `-tensorrt` or `-openvino` engine |
| `omarchy-broadcast-maxine-vfx` | The Maxine video effects module | `cuda`, `maxine-vfx-sdk-local` |
| `omarchy-broadcast-maxine-ar` | The Maxine AR module (eye contact, auto-framing) | `cuda`, `maxine-ar-sdk-local` |
| `omarchy-broadcast-maxine-afx` | The Maxine audio effects module for the microphone | `maxine-afx-sdk-local` |

- Each `-bin` package installs the files of its `-git` twin and provides the plain name, so a module package of either kind works with the core of the other.
- Each module package depends on the module ABI range it was built for (`omarchy-broadcast-module-abi>=1.6` and `<2`), which the core provides. The daemon refuses a module built for a newer ABI minor than its own.
- The NVIDIA SDK is never packaged here. Build `maxine-vfx-sdk-local`, `maxine-ar-sdk-local` or `maxine-afx-sdk-local` yourself from the [omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) repository. The daemon and the audio worker find the SDK's environment and apply it to themselves, so no systemd drop-in and no `LD_LIBRARY_PATH` are needed.
- The ONNX models are packages of their own from [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models): `omarchy-onnx-model-<id>`, with `<id>` one of `mediapipe-landscape` (the default), `pphumanseg-v2-mobile`, `pphumanseg-v2-lite` or `modnet`. They install in `/usr/share/omarchy-onnx-models` with each model's own licence.

### Setup guides

| To set up | Guide |
|---|---|
| The daemon, the packages, the unit files and the optional PipeWire camera | [docs/setup/daemon.md](docs/setup/daemon.md) |
| The v4l2loopback device and the virtual camera's size and rate | [docs/setup/loopback.md](docs/setup/loopback.md) |
| Open-model background effects, without an NVIDIA SDK | [docs/setup/onnx.md](docs/setup/onnx.md) |
| NVIDIA Maxine video effects, eye contact and auto-framing | [docs/setup/maxine.md](docs/setup/maxine.md) |
| The virtual microphone | [docs/setup/microphone.md](docs/setup/microphone.md) |

## Requirements

- GStreamer 1.x with the base, good and bad plugin sets (`videoconvert`, `jpegdec`).
- v4l2loopback, configured as in [docs/setup/loopback.md](docs/setup/loopback.md).
- A systemd user session with a D-Bus session bus.
- **Without an NVIDIA GPU:** nothing more. The daemon runs its system-memory pipeline ([ADR-0033](docs/adr/0033-system-memory-pipeline.md)): camera passthrough, the open-model cut-out on the CPU, an Intel GPU or an NPU through ONNX Runtime, and the CPU microphone module need no CUDA.
- **With an NVIDIA GPU:** the proprietary driver, the CUDA toolkit (for `nvrtc`) and GStreamer's `nvcodec` plugin (`cudaupload`, `cudaconvert`, `cudadownload`). Without the CUDA toolkit, the default `[video] memory = "auto"` runs the system-memory pipeline and says why in the log and in `omarchy-broadcast settings`. Install `cuda` for the GPU path.
- **For the open-model cut-out:** the package `omarchy-broadcast-onnx-git` (or `-bin`, or `scripts/install-user.sh`), a model package (`omarchy-onnx-model-mediapipe-landscape` by default) and ONNX Runtime 1.29 or newer from an Arch `onnxruntime-*` package. Related repositories: [omarchy-onnxruntime](https://github.com/rsd/omarchy-onnxruntime) and [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models). See [docs/setup/onnx.md](docs/setup/onnx.md) and [ADR-0031](docs/adr/0031-onnx-cutout-module.md).
- **For the Maxine video effects:** an NVIDIA RTX GPU, the package `omarchy-broadcast-maxine-vfx-git` and the SDK package `maxine-vfx-sdk-local`. The module finds the SDK in `/opt/nvidia/maxine/videofx` or at `sdk_root` in `[module."br.com.dias.maxine-vfx"]` (`[effect] maxine_root` is an alias). See [docs/setup/maxine.md](docs/setup/maxine.md).
- **For eye contact and auto-framing:** an NVIDIA RTX GPU, the package `omarchy-broadcast-maxine-ar-git` and the SDK package `maxine-ar-sdk-local`. The module finds the SDK in `/opt/nvidia/maxine/arsdk` or at `sdk_root` in `[module."br.com.dias.maxine-ar"]`. See [docs/setup/maxine.md](docs/setup/maxine.md#the-maxine-ar-module).
- **For the microphone's noise removal on the CPU:** nothing more. The Arch package installs the CPU module with the mic host and the audio worker ([docs/setup/microphone.md](docs/setup/microphone.md)).
- **For the microphone on an NVIDIA RTX GPU:** the package `omarchy-broadcast-maxine-afx-git` and the SDK package `maxine-afx-sdk-local`, whose environment the worker applies itself ([docs/setup/microphone.md](docs/setup/microphone.md#on-an-nvidia-gpu-maxine-audio-effects)). Only the older filter-chain units need `deepfilternet-ladspa` or `noise-suppression-for-voice`.
- **For video backgrounds:** `ffmpeg` is optional. It imports animated GIF, WebP and APNG files, makes the posters of imported videos and, on a machine without an NVIDIA GPU, runs the command that converts a video the CPU cannot play. VP9 profile 0 in WebM, at most 1920x1080 and 30 fps, plays as it is. Any other video shows its poster as a still until you convert it. The two bundled loops need none of this. See [docs/setup/daemon.md](docs/setup/daemon.md#playing-a-video-without-an-nvidia-gpu), [docs/dbus-api.md](docs/dbus-api.md#video-backgrounds-revision-18) and [docs/adr/0035-video-backgrounds.md](docs/adr/0035-video-backgrounds.md).
- **For the native PipeWire camera output:** a build with the cargo feature `omarchy-broadcastd/pipewire` (libpipewire headers and clang at build time; the Arch package has it) and `[pipewire] enabled = true` in the configuration.
- **To build from source:** the Rust toolchain. The Arch package is built with it and installed without it.

## Limitations

- **One application streams from the virtual camera at a time.** From v4l2loopback 0.14, a loopback device lets one process stream from its capture side. A second application that tries to stream gets "device busy" (`EBUSY`), even for the same format. No module option changes that. One application can still share the camera inside itself: Chrome and Chromium share one capture among their tabs. To run several applications at once, enable the PipeWire camera node with `[pipewire] enabled = true` ([docs/setup/daemon.md](docs/setup/daemon.md#the-pipewire-output)). Chrome and Chromium then need the `WebRtcPipeWireCamera` flag (`chrome://flags`), and OBS Studio has a "Video Capture Device (PipeWire)" source. Applications without PipeWire camera support, such as Zoom and Discord, keep using the loopback device ([docs/setup/loopback.md](docs/setup/loopback.md#one-application-at-a-time)).
- **The PipeWire node and mixed formats.** Consumers of the PipeWire node that ask for different formats (one for YUY2, another that accepts only NV12) can stall it until every consumer has disconnected. The node is optional for that reason.
- **A daemon crash.** Applications that hold the virtual camera keep it. The daemon leaves a black timeout image and a `timeout` on the loopback, so Chromium-based applications get black frames until the restarted daemon writes again. The unit restarts the daemon after 250 ms. Consumers of the PipeWire node do not reconnect: use the loopback device where that matters ([docs/adr/0022-crash-recovery-for-consumers.md](docs/adr/0022-crash-recovery-for-consumers.md), [docs/setup/daemon.md](docs/setup/daemon.md#restarts-and-crashes)).
- **TensorRT of the ONNX module after a Maxine effect.** The Maxine SDKs load their own TensorRT into the daemon. Once one has run, ONNX Runtime's TensorRT engine would call the wrong library, so the module refuses it and uses CUDA until the daemon restarts ([docs/setup/onnx.md](docs/setup/onnx.md#tensorrts-library-path)).
- **OpenVINO needs the device to be there.** The Intel GPU engine needs an Intel GPU present in the running system (on a laptop with a GPU switch, not in discrete-only mode). The NPU engine needs access to `/dev/accel/accel0`, normally through the `render` group ([docs/setup/onnx.md](docs/setup/onnx.md#8-troubleshooting)).
- **Eye contact runs without the SDK's CUDA graphs,** which are the SDK's default. With them on, a capture on one thread could make another thread's CUDA call fail and abort the daemon ([docs/adr/0028-maxine-ar-module.md](docs/adr/0028-maxine-ar-module.md)).
- **Maxine performance figures are not published,** per the NVIDIA Maxine SDK licence. Reproduce them locally with `obc-bench` ([docs/testing.md](docs/testing.md)). Other measurements are in [docs/benchmarks/](docs/benchmarks/passthrough.md).
- **Status: early development.** Passthrough from camera to virtual camera sustains 1080p60 on the reference machine.

## How it works

The daemon runs on demand as a systemd user service, controlled over D-Bus by the CLI, `omarchy-broadcast`, and by the bar plugin. Frames flow from the camera through GStreamer, in CUDA or system memory, into the virtual camera. MJPEG cameras get a CPU decode step and YUYV or UYVY cameras a CPU convert step. The pipeline picks the capture mode from what the camera advertises.

- **Modules.** The effects are modules behind a versioned C ABI (D-Bus revision 9 and later; the interface is at revision 28). `omarchy-broadcast modules` lists them with their status, the reason when one is unavailable and the stages each provides. `modules enable|disable ID`, `modules profile quality|battery` and `modules pin STAGE MODULE` choose which module provides a stage, from the next camera start. The `quality` profile prefers Maxine and the `battery` profile prefers the ONNX module. A disabled module's stages leave the running stream at once. `omarchy-broadcast order` shows and sets the order the stages run in. A module's settings appear in `omarchy-broadcast settings` and in the bar panel with no change to them. A module can come from the Rust SDK or from the C header, installed in `~/.local/share/omarchy-broadcast/modules.d/`: [docs/modules.md](docs/modules.md) is the author guide and `examples/module-hello` a complete example ([ADR-0025](docs/adr/0025-loadable-modules.md)).
- **Settings.** `omarchy-broadcast settings` lists every standard setting the camera offers by group (image, white balance, exposure, focus and zoom, anything else) with its value, what it takes and, for a setting that cannot change now, why. The effects are settings of the same tree (`effect.*`, the same for every camera). A change applies at once while the camera is in use and otherwise at the next open. The daemon stores the values per camera and writes them whenever the camera opens. `controls` is the short form for brightness, contrast, saturation, sharpness and backlight compensation ([ADR-0020](docs/adr/0020-camera-image-controls.md), [ADR-0023](docs/adr/0023-settings-schema.md), [ADR-0024](docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md), [docs/dbus-api.md](docs/dbus-api.md#camera-settings), [docs/cli-json.md](docs/cli-json.md#camera-settings)).
- **Configuration.** The daemon writes the choices you make in the CLI and the bar plugin's setup dialog to `~/.config/omarchy-broadcast/config.d/50-panel.toml`. `config.toml` stays yours. A later file such as `config.d/90-local.toml` wins over the managed one, and the daemon refuses to change a key that such a file sets ([ADR-0026](docs/adr/0026-config-d-and-managed-config.md), [docs/config.example.toml](docs/config.example.toml)). The Maxine SDK root is the module's `sdk_root` setting.
- **Capture and output modes.** `camera.capture.mode` is `auto` or one of the sizes and rates the camera reports. The virtual camera's size and rate are `output.mode`, which `omarchy-broadcast config set output.mode 1280x720@30` or the setup dialog changes. While an application holds the virtual camera, the change waits for it. A format that cannot start reverts to one that works ([docs/setup/loopback.md](docs/setup/loopback.md#changing-the-output-size-and-rate)). Frame generation needs the Maxine module ([ADR-0027](docs/adr/0027-output-modes-frame-generation-and-privacy-fallback.md)).
- **Privacy.** With `[effect] privacy` off, the default from daemon 0.48.0, the virtual camera shows the camera as it is when an effect cannot run. With it on, `[effect] privacy_mode` chooses what shows instead: `black` (the default), `away` (your own image, the `fallback_image` set in the configuration or the dialog) or `freeze` (the last frame the effect produced). The older key `on_failure` is still read ([ADR-0037](docs/adr/0037-privacy-as-an-option.md)).
- **Fail-open stages.** Noise removal, relighting and enhancement stack with the effect. Noise removal adds one GPU pass per frame while it is on. If one cannot run, the stream continues without it, and a stage whose SDK feature is not installed shows as not installed ([ADR-0018](docs/adr/0018-video-noise-removal.md), [ADR-0024](docs/adr/0024-stackable-stages-and-effects-in-the-settings-schema.md)). Blur and replace harden the mask edge, so the room does not show through around you ([ADR-0013](docs/adr/0013-mask-hardening-cuda-kernel.md)).
- **Camera image controls.** Some cameras keep image settings from other software far from their defaults, oversharpened or oversaturated. `omarchy-broadcast controls reset` puts the five controls back to the camera's defaults. On a Logitech Brio or MX Brio the field of view (90, 78 or 65 degrees) is among the settings.
- **The microphone.** It runs apart from the video daemon, as a stable PipeWire node and an audio worker, so applications keep receiving your microphone whatever stops. It is off until you turn it on. It becomes the default microphone only when you ask (`mic default`), and `mic restore-default` or `mic off` restores the previous default. The CPU module uses DeepFilterNet by default, or RNNoise. The Maxine module adds room echo removal and studio voice. The older single-engine filter-chain units remain as a lightweight alternative ([docs/setup/microphone.md](docs/setup/microphone.md), [ADR-0029](docs/adr/0029-audio-modules-mic-host-and-worker.md)).
- **PipeWire camera node.** The daemon can also offer the camera as a native PipeWire video source, "Omarchy Broadcast", next to the loopback device ([docs/setup/daemon.md](docs/setup/daemon.md#the-pipewire-output)).

## The bar plugin

The Omarchy bar plugin is a separate project, the [omarchy-broadcast-plugin](https://github.com/rsd/omarchy-broadcast-plugin) repository, and the package does not install it. It shows the virtual camera's state in the bar and opens a panel with a live preview, the effects, the camera's settings, the microphone and a Details view, and a setup dialog for the modules and the configuration.

- It is a client of the daemon through `omarchy-broadcast watch --json` and the commands of the CLI. It runs no `systemctl`: when the daemon is not running, its Start button runs `omarchy-broadcast start`.
- Its camera settings need omarchy-broadcast 0.22.0 or newer, the first release with the settings channel ([ADR-0023](docs/adr/0023-settings-schema.md)). Its effect settings need 0.23.0 or newer. Against an older daemon it keeps what the daemon supports and says what needs an update.
- For its live preview, it runs `omarchy-broadcast preview --json`, which holds a preview lease and prints a line per frame the daemon writes to a small JPEG file under `$XDG_RUNTIME_DIR` ([docs/cli-json.md](docs/cli-json.md#preview)). A preview counts as a user of the camera, so it starts it.

## Build from source

```sh
cargo build --release
```

The PipeWire output is a cargo feature that is off by default: `cargo build --release --features omarchy-broadcastd/pipewire`. None of the routes below configures the v4l2loopback device or installs the Maxine SDK. The daemon, the unit files and the other steps are in [docs/setup/daemon.md](docs/setup/daemon.md).

### Arch package from a checkout

`packaging/arch/PKGBUILD` is one split package. A single build, from the committed HEAD of the local checkout, makes the five packages of the table above, with the `-git` suffix. Build from `packaging/arch`, install the core, then add the modules you want:

```sh
cd packaging/arch
makepkg -s
sudo pacman -U omarchy-broadcast-git-*.pkg.tar.zst
systemctl --user enable --now omarchy-broadcastd
```

To install a module too, name its package in the same command, for example `sudo pacman -U omarchy-broadcast-git-*.pkg.tar.zst omarchy-broadcast-onnx-git-*.pkg.tar.zst`.

The core installs:

- the daemon and the CLI in `/usr/bin`;
- the microphone's parts (the audio worker, the mic host's unit and configuration, the forwarding plugin and the CPU voice module), all inactive until `omarchy-broadcast mic on`;
- the directories where modules are found: a module package puts its library in `/usr/lib/omarchy-broadcast/modules` and its manifest in `/usr/lib/omarchy-broadcast/modules.d`;
- the C header of the module interface in `/usr/include/omarchy-broadcast`;
- a udev rule that lets the daemon read the MX Brio's Show Mode (`/usr/lib/udev/rules.d/70-omarchy-broadcast-mx-brio.rules`);
- the systemd user unit, the D-Bus activation file and the documentation.

The package enables nothing: each user runs the `systemctl --user enable --now` command. See [docs/setup/maxine.md](docs/setup/maxine.md) (section 3) and [ADR-0039](docs/adr/0039-self-applied-sdk-environment-and-module-packages.md) for how the SDK environment is applied.

The AUR recipe of the same split package, pkgbase `omarchy-broadcast-git`, builds from this repository on GitHub. `scripts/aur.sh` generates its PKGBUILD and `.SRCINFO` from `packaging/arch/PKGBUILD`. The prebuilt recipe, pkgbase `omarchy-broadcast-bin`, is made by `scripts/release-bin.sh`, which builds and checks the release assets, and `scripts/aur.sh --bin`. Both are for maintainers ([docs/setup/daemon.md](docs/setup/daemon.md#publishing-to-the-aur)).

### Developer install

`scripts/install-user.sh` builds the release binaries and the NVIDIA Maxine module and installs them for the current user, without root:

- the binaries in `~/.local/bin`;
- the unit and the D-Bus file under `~/.local/share`;
- the Maxine module's library and manifest under `~/.local/share/omarchy-broadcast`.

It does not install the udev rule for the MX Brio, which needs root ([docs/setup/daemon.md](docs/setup/daemon.md#the-udev-rule-for-the-mx-brio)). It does not install the Maxine SDK either; the daemon applies the SDK's environment when it finds the SDK package ([docs/setup/maxine.md](docs/setup/maxine.md), "The Maxine module" and section 3). It never runs `systemctl`; it prints the next steps. `--dry-run` shows the actions and `--uninstall` removes only what the script installed.

```sh
scripts/install-user.sh
systemctl --user daemon-reload
systemctl --user enable --now omarchy-broadcastd
```

### Manual install

Run `cargo install` for the two crates and copy the unit file with `ExecStart=` rewritten: see [docs/setup/daemon.md](docs/setup/daemon.md#install).

### Run the benchmark binary

`obc-bench` streams from the camera to the "Omarchy Broadcast" loopback device until Ctrl-C:

```sh
cargo run --release -p obc-pipeline --bin obc-bench -- --duration 0
```

Then select "Omarchy Broadcast" as the camera in OBS or Chrome. Optional arguments:

- `--camera <name|vid:pid|/dev/videoN>` chooses the camera. The default is automatic. A name or ID matching two colour cameras is an error, and greyscale IR nodes are never picked.
- `--mode WxH@FPS` or `FORMAT:WxH@FPS` chooses the capture mode, for example `--mode 1920x1080@30` or `--mode NV12:1920x1080@60`. The default is `1920x1080@60`, any format. The closest mode the camera offers is used.

### Check the repository

`scripts/check-all.sh` runs every check of the repository and stops at the first failure. It needs neither a GPU nor a camera. It runs:

- the format check, clippy and the workspace tests, with `--locked`;
- the same three for the video and the audio example module, which are built outside the workspace as third-party modules would be;
- clippy and the tests of `obc-pipeline` and `omarchy-broadcastd` with the PipeWire feature;
- a check that the `pkgver=` of both PKGBUILDs matches the workspace version;
- shellcheck over the shell scripts.

Besides the Rust toolchain and the packages listed in [docs/testing.md](docs/testing.md), it needs libpipewire, clang and pipewire for the PipeWire build, and either `shellcheck` or `uvx`, which fetches it. Tests, including the manual check with OBS or Chrome, are in [docs/testing.md](docs/testing.md).

## Documentation

[docs/README.md](docs/README.md) routes you to the right page by task. It also indexes the architecture, the decision records, the setup guides and the benchmarks. [docs/modules.md](docs/modules.md) covers writing and installing modules.

## Affiliation

omarchy-broadcast is not affiliated with or endorsed by NVIDIA. NVIDIA, Maxine and TensorRT are trademarks of NVIDIA Corporation.

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
