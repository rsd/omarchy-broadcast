# Install omarchy-broadcast

Install the core package and one module, create the virtual camera device once, and start the daemon. On Arch Linux and Omarchy the packages come from the AUR. On another Linux distribution you install from a checkout of the repository ([Other distributions](#other-distributions)). The packages and the device need root. The rest runs as your user.

Not sure whether omarchy-broadcast is for you? Read the [README](README.md) first.

## Before you start

- A system to install on: Omarchy or Arch Linux with an AUR helper such as `yay` (the main route), or another Linux distribution (the [route for other distributions](#other-distributions) works from a checkout and is untested outside Arch and Omarchy).
- `sudo`, for the packages and the virtual camera device.
- A webcam. Any standard webcam works, USB or built-in. Cameras that are not standard webcams may not work: see [LIMITATIONS.md](LIMITATIONS.md#cameras-that-are-not-standard-webcams).
- The requirements below. Your GPU decides what you add.

## Requirements

What you need depends on the features you use.

**Always:**

- GStreamer 1.x with the base, good and bad plugin sets (`videoconvert`, `jpegdec`). On Arch these are `gst-plugins-good` and `gst-plugins-bad-libs`, which the core package depends on.
- A v4l2loopback kernel module, for example `v4l2loopback-dkms`. The core package depends on one. Configure it as in [docs/setup/loopback.md](docs/setup/loopback.md).
- A systemd user session with a D-Bus session bus. A desktop login provides both.

**By GPU:**

- **Without an NVIDIA GPU:** nothing more for the core. The daemon runs its system-memory pipeline: camera passthrough, the open-model cut-out on the CPU, an Intel GPU or an NPU through ONNX Runtime, and the CPU microphone module need no CUDA. For the open-model effects, add `onnxruntime-cpu` on Arch.
- **With an NVIDIA GPU:** the proprietary driver (`nvidia-utils` comes with it), the CUDA toolkit (`cuda`, for `nvrtc`) and GStreamer's `nvcodec` plugin (`cudaupload`, `cudaconvert`, `cudadownload`). Without the CUDA toolkit, the default `[video] memory = "auto"` runs the system-memory pipeline and says why in the log and in `omarchy-broadcast settings`. Install `cuda` for the GPU path. The daemon opens `nvidia-utils` and `cuda` at run time ([Without CUDA or without an NVIDIA GPU](docs/setup/daemon.md#without-cuda-or-without-an-nvidia-gpu)).

**By feature:**

- **Open-model cut-out:** the package `omarchy-broadcast-onnx-git` (or `-bin`, or `scripts/install-user.sh`), a model package (`omarchy-onnx-model-mediapipe-landscape` by default) and ONNX Runtime 1.29 or newer from an Arch `onnxruntime-*` package. Related repositories: [omarchy-onnxruntime](https://github.com/rsd/omarchy-onnxruntime) and [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models). See [docs/setup/onnx.md](docs/setup/onnx.md).
- **Maxine video effects:** an NVIDIA RTX GPU, the package `omarchy-broadcast-maxine-vfx-git` and the SDK package `maxine-vfx-sdk-local`. The module finds the SDK in `/opt/nvidia/maxine/videofx` or at `sdk_root` in `[module."br.com.dias.maxine-vfx"]` (`[effect] maxine_root` is an alias). See [docs/setup/maxine.md](docs/setup/maxine.md). The module package depends on `cuda`, and the SDK package on `nvidia-utils`.
- **Eye contact and auto-framing:** an NVIDIA RTX GPU, the package `omarchy-broadcast-maxine-ar-git` and the SDK package `maxine-ar-sdk-local`. The module finds the SDK in `/opt/nvidia/maxine/arsdk` or at `sdk_root` in `[module."br.com.dias.maxine-ar"]`. See [docs/setup/maxine.md](docs/setup/maxine.md#the-maxine-ar-module).
- **The microphone's noise removal on the CPU:** nothing more. The Arch package installs the CPU module with the mic host and the audio worker ([docs/setup/microphone.md](docs/setup/microphone.md)).
- **The microphone on an NVIDIA RTX GPU:** the package `omarchy-broadcast-maxine-afx-git` and the SDK package `maxine-afx-sdk-local`, whose environment the worker applies itself ([docs/setup/microphone.md](docs/setup/microphone.md#on-an-nvidia-gpu-maxine-audio-effects)). Only the older filter-chain units need `deepfilternet-ladspa` or `noise-suppression-for-voice`.
- **Video backgrounds:** `ffmpeg` is optional. The daemon runs it, at low priority, to import animated GIF, WebP and APNG files and to make the posters of imported videos. On a machine without an NVIDIA GPU it is also the program of the command that converts a video the CPU cannot play; the daemon never runs that command. VP9 profile 0 in WebM, at most 1920x1080 and 30 fps, plays as it is. Any other video shows its poster as a still until you convert it. With an NVIDIA GPU a video plays as it is. The two bundled loops need none of this. See [docs/setup/daemon.md](docs/setup/daemon.md#playing-a-video-without-an-nvidia-gpu) and [docs/dbus-api.md](docs/dbus-api.md#video-backgrounds-revision-18).
- **The native PipeWire camera output:** a build with the cargo feature `omarchy-broadcastd/pipewire` (libpipewire headers and clang at build time; the Arch package has it) and `[pipewire] enabled = true` in the configuration.
- **Building from source:** the Rust toolchain. The Arch package is built with it and installed without it ([Build from source](#build-from-source)).

## Choose your packages

This section and the steps that follow are for Arch Linux and Omarchy. For another distribution, go to [Other distributions](#other-distributions).

### Prebuilt or built from source

The AUR has the split package of `packaging/arch/PKGBUILD` in two forms. Install one of them. They conflict with each other, and the modules of one form work with the core of the other.

| Form | Package names | Use it when |
|---|---|---|
| Prebuilt, `-bin` | `omarchy-broadcast-bin`, `omarchy-broadcast-onnx-bin`, `omarchy-broadcast-maxine-vfx-bin`, `omarchy-broadcast-maxine-ar-bin`, `omarchy-broadcast-maxine-afx-bin` | You want a quick install with no build tools. The files come from the GitHub release of the same version and are checked against a sha256 in the recipe |
| Source, `-git` | The same names with `-git` in place of `-bin` | You want the latest source. It builds from this repository on GitHub and needs the build tools of `packaging/arch/PKGBUILD` |

Each `-bin` package installs exactly the files of its `-git` twin. It provides the plain name (`omarchy-broadcast`, `omarchy-broadcast-onnx`, and so on) and conflicts with the plain name and with the twin.

### Choose your modules

The core installs the daemon, the CLI, the units, the CPU voice module and the microphone's parts. Each module is a package of its own, so you install only what you use. Add `-bin` or `-git` to the name, the same kind as the core.

| Package | What it adds | Depends on |
|---|---|---|
| `omarchy-broadcast` | The core: the daemon and the CLI, the audio worker, the CPU voice module, the microphone's host and plug-in, the units, the D-Bus files, the udev rule, the media and the documentation. The CPU video path is part of the daemon | optional: `nvidia-utils` and `cuda`, for the CUDA video path on NVIDIA GPUs |
| `omarchy-broadcast-onnx` | The open-model cut-out module (blur and replace, no NVIDIA SDK). Models are separate packages | `onnxruntime>=1.29`, `omarchy-onnx-model-mediapipe-landscape`; optional: the other `omarchy-onnx-model-*` packages and an `onnxruntime-provider-cuda`, `-tensorrt` or `-openvino` engine |
| `omarchy-broadcast-maxine-vfx` | The Maxine video effects module | `cuda`, `maxine-vfx-sdk-local` |
| `omarchy-broadcast-maxine-ar` | The Maxine AR module (eye contact, auto-framing) | `cuda`, `maxine-ar-sdk-local` |
| `omarchy-broadcast-maxine-afx` | The Maxine audio effects module for the microphone | `maxine-afx-sdk-local` |

- Each module package depends on the module ABI range it was built for (`omarchy-broadcast-module-abi>=1.6` and `<2`), which the core provides. The daemon refuses a module built for a newer ABI minor than its own.
- The NVIDIA SDK is never packaged here. Build `maxine-vfx-sdk-local`, `maxine-ar-sdk-local` or `maxine-afx-sdk-local` yourself from the [omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) repository ([docs/setup/maxine.md](docs/setup/maxine.md)). The daemon and the audio worker find the SDK's environment and apply it to themselves, so no systemd drop-in and no `LD_LIBRARY_PATH` are needed.
- The ONNX models are packages of their own from [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models): `omarchy-onnx-model-<id>`, with `<id>` one of `mediapipe-landscape` (the default), `pphumanseg-v2-mobile`, `pphumanseg-v2-lite` or `modnet`. They install in `/usr/share/omarchy-onnx-models` with each model's own licence ([docs/setup/onnx.md](docs/setup/onnx.md)).
- The prebuilt packages do not contain the NVIDIA SDKs or ONNX Runtime, which are licensed separately. The Maxine and ONNX modules hold no code of either: they load the SDK or the runtime when the daemon runs. So the `-bin` modules need the SDK packages and an `onnxruntime` package exactly as the `-git` ones do.

## Install on Arch or Omarchy

Do the steps in order. Steps 1 to 5 give you a working camera. Steps 6 and 7 are optional.

1. **Install the core and one module.**

   ```sh
   yay -S omarchy-broadcast-bin omarchy-broadcast-onnx-bin     # prebuilt
   yay -S omarchy-broadcast-git omarchy-broadcast-onnx-git     # built from source
   ```

   Right after a release, an AUR helper can show the previous version for a few minutes. The AUR's web interface updates at once, but its RPC index, which AUR helpers query, can take a few minutes to follow. Wait and try again.

2. **Add what your GPU needs.**

   With an NVIDIA GPU, add `cuda`. The CUDA video path needs it for NVRTC, which GStreamer's CUDA converters use.

   ```sh
   yay -S cuda
   ```

   Without an NVIDIA GPU, install the CPU build of ONNX Runtime. You can also pick `onnxruntime-cpu` when `yay` asks which provider of `onnxruntime` to install.

   ```sh
   yay -S onnxruntime-cpu
   ```

   The other providers build a much larger runtime: the AUR's `onnxruntime-core` builds with CUDA, cuDNN, OpenVINO and TensorRT. [docs/setup/onnx.md](docs/setup/onnx.md#2-install-onnx-runtime) lists them.

3. **Add the Maxine modules (optional, NVIDIA RTX GPU only).** Build and install the SDK package of each module first, as in [docs/setup/maxine.md](docs/setup/maxine.md). Then install the module packages:

   ```sh
   yay -S omarchy-broadcast-maxine-vfx-bin omarchy-broadcast-maxine-ar-bin omarchy-broadcast-maxine-afx-bin
   ```

   Use `-git` in place of `-bin` for the source build.

4. **Create the virtual camera device.** This needs root and runs once. The package ships the `modprobe.d` line as an example.

   ```sh
   sudo install -Dm644 /usr/share/doc/omarchy-broadcast/examples/modprobe.d/omarchy-broadcast.conf /etc/modprobe.d/omarchy-broadcast.conf
   echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
   sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback
   ```

   Only one `options v4l2loopback` line takes effect. If `/etc/modprobe.d` already has one, merge the two lines as [docs/setup/loopback.md](docs/setup/loopback.md#several-devices) describes. The reload fails with "Module v4l2loopback is in use" while an application holds a loopback node. Close it and run the reload again.

5. **Start the daemon at every login.**

   ```sh
   systemctl --user enable --now omarchy-broadcastd
   ```

   The package enables nothing by itself. To start the daemon once, without enabling it, run:

   ```sh
   omarchy-broadcast start
   ```

6. **Turn on the virtual microphone (optional).**

   ```sh
   omarchy-broadcast mic on
   ```

   The microphone is off until you turn it on. [docs/setup/microphone.md](docs/setup/microphone.md) covers choosing it as the default, the noise removal engines and the NVIDIA modules.

7. **Add the bar plugin (optional).** The Omarchy bar plugin is a separate project, [omarchy-broadcast-plugin](https://github.com/rsd/omarchy-broadcast-plugin). The package does not install it. Install the daemon first, then add the plugin:

   ```sh
   omarchy plugin add https://github.com/rsd/omarchy-broadcast-plugin
   ```

   The plugin's README covers its requirements and settings.

On a first install the package prints steps 4 to 6. On an upgrade it prints one line, which restarts the daemon to run the new version:

```sh
systemctl --user try-restart omarchy-broadcastd
```

## Other distributions

The AUR packages are for Arch Linux and Omarchy. On another distribution, install from a checkout of the repository. This route has not been tested outside Arch and Omarchy. The daemon probably works on any modern Linux distribution.

### What you need

Your package manager supplies these. Package names differ between distributions, so this guide names none beyond Arch's. `packaging/arch/PKGBUILD` lists the libraries the Arch package depends on.

- The Rust toolchain, to build the daemon and the modules.
- GStreamer 1.x with the base, good and bad plugin sets (`videoconvert`, `jpegdec`), and the libraries the daemon links: GLib, ALSA (`libasound`, for the microphone's hardware gain) and, for the PipeWire camera output, libpipewire.
- A v4l2loopback kernel module. Version 0.15.4 has the controls that the daemon's crash cover uses. With an older module the daemon runs and logs a warning ([docs/setup/loopback.md](docs/setup/loopback.md#what-the-daemon-sets-on-the-device)).
- A systemd user session with a D-Bus session bus. The daemon is a systemd user service and owns a name on the session bus.
- PipeWire, for the virtual microphone and the optional PipeWire camera node. The loopback works without it.
- With an NVIDIA GPU, the driver, the CUDA toolkit and GStreamer's `nvcodec` plugin, as under [Requirements](#requirements). Without one, the system-memory pipeline needs none of them.
- For the open-model effects, ONNX Runtime 1.29 or newer: the module opens `libonnxruntime.so` when the daemon runs. It looks at `$ORT_DYLIB_PATH`, then at `ort_library` in the module's table of `config.toml`, then in the system's library path ([docs/setup/onnx.md](docs/setup/onnx.md#where-the-module-looks-for-the-library)). It also needs the four model files ([docs/setup/onnx.md](docs/setup/onnx.md#from-a-checkout)).

### Steps

1. **Install the daemon and the CLI for your user.** From the repository root, run the script. It builds the release binaries and the modules, installs them under `~/.local`, needs no root and refuses to run as root. [Per-user install from a checkout](#per-user-install-from-a-checkout) lists what it installs and its options.

   ```sh
   scripts/install-user.sh
   ```

   The script builds without the PipeWire camera output. To get it, use the [manual install](#manual-install) with the cargo feature `pipewire` ([the PipeWire output](docs/setup/daemon.md#the-pipewire-output)).

2. **Make sure `${XDG_BIN_HOME:-~/.local/bin}` is on your `PATH`.** The script installs `omarchy-broadcast` and `omarchy-broadcastd` there.

3. **Create the virtual camera device.** This needs root and runs once. The checkout holds the `modprobe.d` line that the Arch package ships as an example.

   ```sh
   sudo install -Dm644 dist/modprobe.d/omarchy-broadcast.conf /etc/modprobe.d/omarchy-broadcast.conf
   echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
   sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback
   ```

   Only one `options v4l2loopback` line takes effect. If `/etc/modprobe.d` already has one, merge the two lines as [docs/setup/loopback.md](docs/setup/loopback.md#several-devices) describes. The reload fails with "Module v4l2loopback is in use" while an application holds a loopback node. Close it and run the reload again.

4. **Start the daemon at every login.**

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now omarchy-broadcastd
   ```

5. **Add the optional parts.** The open-model effects need ONNX Runtime and the model files ([docs/setup/onnx.md](docs/setup/onnx.md#from-a-checkout)). The virtual microphone needs the script's `--mic` or `--audio` option ([docs/setup/microphone.md](docs/setup/microphone.md)). The NVIDIA Maxine effects need an SDK you obtain yourself ([docs/setup/maxine.md](docs/setup/maxine.md)). The Omarchy bar plugin is for Omarchy only; elsewhere the CLI is the control.

Then [check that it works](#check-that-it-works).

## Check that it works

```sh
omarchy-broadcast start
```

`start` starts the daemon through D-Bus activation if it is not running, and prints its status. It changes no setting and does not turn the camera on. Expect a block of lines that includes your camera:

```text
State:      idle
Camera:     <your camera>
Consumers:  0
```

`State` is `idle` while no application uses the camera and `active` while one does. It is `off` if you turned the camera off. Turn it on again:

```sh
omarchy-broadcast on
```

For the service itself, check that it is `active (running)`:

```sh
systemctl --user status omarchy-broadcastd
```

Now select **Omarchy Broadcast** as the camera in OBS or Chrome and choose an effect:

```sh
omarchy-broadcast effect blur
omarchy-broadcast effect replace      # with no background set, uses the first bundled background
```

`effect replace --image FILE` or the bar panel chooses another background. A few more commands follow. All of them are in [docs/cli-json.md](docs/cli-json.md).

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

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| The reload fails with "Module v4l2loopback is in use" | An application holds a loopback node | Close it, check with `fuser -v /dev/video10`, and run the reload again |
| `omarchy-broadcast status` shows the state `error` | The v4l2loopback device labelled "Omarchy Broadcast" is missing. The daemon retries every 2 s | Create the virtual camera device (step 4, or step 3 of [Other distributions](#other-distributions)). [docs/setup/loopback.md](docs/setup/loopback.md#troubleshooting) has more |
| `omarchy-broadcast` exits with status 3 | The daemon is not running, or there is no D-Bus session bus | Run `omarchy-broadcast start` or step 5 (step 4 of Other distributions), then `systemctl --user status omarchy-broadcastd` |
| The AUR helper offers the previous version | The AUR's RPC index lags a few minutes after a release | Wait and try again |
| The service fails with status 203/EXEC | The unit names `/usr/bin/omarchy-broadcastd`, but the daemon is in `~/.cargo/bin` | Rewrite `ExecStart=` ([Manual install](#manual-install), step 2) |
| An old daemon keeps starting after you installed the package | A copy of the unit in `~/.config/systemd/user` shadows the packaged one | [docs/setup/daemon.md](docs/setup/daemon.md#which-unit-is-used) |
| A second application gets "device busy" | A v4l2loopback device serves one capture stream at a time | [LIMITATIONS.md](LIMITATIONS.md#one-application-streams-from-the-virtual-camera-at-a-time) and [docs/setup/loopback.md](docs/setup/loopback.md#one-application-at-a-time) |

More in the troubleshooting sections of the guides: [daemon](docs/setup/daemon.md#troubleshooting), [loopback](docs/setup/loopback.md#troubleshooting), [ONNX](docs/setup/onnx.md#8-troubleshooting), [Maxine](docs/setup/maxine.md#7-troubleshooting) and [microphone](docs/setup/microphone.md#troubleshooting).

## Next steps

| To set up | Guide |
|---|---|
| The daemon, the unit files and the optional PipeWire camera | [docs/setup/daemon.md](docs/setup/daemon.md) |
| The v4l2loopback device and the virtual camera's size and rate | [docs/setup/loopback.md](docs/setup/loopback.md) |
| Open-model background effects, without an NVIDIA SDK | [docs/setup/onnx.md](docs/setup/onnx.md) |
| NVIDIA Maxine video effects, eye contact and auto-framing | [docs/setup/maxine.md](docs/setup/maxine.md) |
| The virtual microphone | [docs/setup/microphone.md](docs/setup/microphone.md) |

## Build from source

```sh
cargo build --release
```

The PipeWire output is a cargo feature that is off by default. It needs the libpipewire headers and clang at build time ([the PipeWire output](docs/setup/daemon.md#the-pipewire-output)):

```sh
cargo build --release --features omarchy-broadcastd/pipewire
```

None of the routes below configures the v4l2loopback device (step 4 of the Arch route, step 3 of [Other distributions](#other-distributions)) or installs the Maxine SDK. Choose one route.

| Route | Root needed | Use it when |
|---|---|---|
| [Arch package from a checkout](#arch-package-from-a-checkout) | yes (pacman) | You build the package from a local checkout |
| [Per-user install from a checkout](#per-user-install-from-a-checkout) | no | You want an install in your home directory |
| [Manual install](#manual-install) | no | You want only the two binaries and the unit file |

### Arch package from a checkout

`packaging/arch/PKGBUILD` builds one split package of five from the committed HEAD of your local checkout. Uncommitted changes are not included. The five are the core `omarchy-broadcast-git` and a package per loadable module, with the base names of the [table above](#choose-your-modules) and the `-git` suffix.

1. Build from `packaging/arch`, then install the core and the module packages you want from the same build:

   ```sh
   cd packaging/arch
   makepkg -s
   sudo pacman -U omarchy-broadcast-git-*.pkg.tar.zst
   ```

   To install a module too, name its package in the same command, for example:

   ```sh
   sudo pacman -U omarchy-broadcast-git-*.pkg.tar.zst omarchy-broadcast-onnx-git-*.pkg.tar.zst
   ```

2. Create the virtual camera device, as in step 4 above. The package does not do it for you.

3. Start the daemon as each user. Nothing is enabled by the install. Pacman prints the next steps from the install scriptlet `packaging/arch/omarchy-broadcast.install`, which changes nothing.

   ```sh
   systemctl --user enable --now omarchy-broadcastd
   ```

What the core package installs and needs is under [Details](#details).

The AUR recipe of the same split package, pkgbase `omarchy-broadcast-git`, builds from this repository on GitHub. `scripts/aur.sh` generates its PKGBUILD and `.SRCINFO` from `packaging/arch/PKGBUILD`. The prebuilt recipe, pkgbase `omarchy-broadcast-bin`, is made by `scripts/release-bin.sh`, which builds and checks the release assets, and `scripts/aur.sh --bin`. Both are for maintainers ([docs/setup/daemon.md](docs/setup/daemon.md#publishing-to-the-aur)).

### Per-user install from a checkout

1. From the repository root, run the script. It needs no root and refuses to run as root.

   ```sh
   scripts/install-user.sh
   ```

2. Load the unit and start the service:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now omarchy-broadcastd
   ```

The script builds the release binaries and the NVIDIA Maxine module and installs:

- `omarchy-broadcastd` and `omarchy-broadcast` in `${XDG_BIN_HOME:-~/.local/bin}`;
- the unit in `${XDG_DATA_HOME:-~/.local/share}/systemd/user/`, with `ExecStart=` pointing at the installed daemon;
- the D-Bus activation file in `${XDG_DATA_HOME:-~/.local/share}/dbus-1/services/`;
- the modules, each as a library in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules/` and a manifest in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/`, whose `library` line names the installed library:
  - the NVIDIA Maxine video module (`libobc_module_maxine.so`, `maxine-vfx.toml`);
  - the NVIDIA Maxine AR module (`libobc_module_maxine_ar.so`, `maxine-ar.toml`);
  - the open-model cut-out module (`libobc_module_onnx.so`, `onnx.toml`);
- a manifest, `install-user.manifest`, in `${XDG_STATE_HOME:-~/.local/state}/omarchy-broadcast/`. It lists every path written (the modules' libraries and manifests included) and the sha256 of each binary and of each module library.

The Maxine modules are built and installed whether or not the Maxine SDK is present. Without the SDK the daemon lists the module greyed out, with the reason. The open-model cut-out module is installed without ONNX Runtime and without models: the daemon lists it as "model not installed" until the four model files are in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/models/onnx`. Set `OBC_ONNX_MODELS_DIR` to a directory that holds them, and the script installs them once their sha256 match the module's catalogue ([crates/obc-module-onnx/README.md](crates/obc-module-onnx/README.md)). Or install the model package `omarchy-onnx-model-mediapipe-landscape`.

The script does not install the SDKs, the os-release shim or any drop-in; [docs/setup/maxine.md](docs/setup/maxine.md) describes them ("The Maxine module" and section 3 say how the daemon applies the SDK's environment). An older version of the script installed a shim in `~/.local/lib/omarchy-broadcast/`. A reinstall or `--uninstall` removes it.

**Behaviour.**

- It marks every text file it writes. It overwrites or removes a file only if it carries that mark. For a binary, a module library included, it does so only if the manifest lists it with the checksum on disk. It stops before changing anything if a target path holds another file. Running it again updates its own install.
- A Maxine manifest at `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/maxine-vfx.toml` that it did not write has no mark, so the script refuses it. An example is the hand-made copy of the manual recipe in [docs/setup/maxine.md](docs/setup/maxine.md). Remove that file and the library its `library` line names, then run the script again. This applies to `--uninstall` as well.
- For an install, the data directory must be an absolute path made only of letters, digits and `._/@+-`. Its path is written into the Maxine manifest and, with `--mic`, into the microphone's unit. Otherwise the script exits with status 2.
- It runs no `systemctl` command and enables nothing. It prints the next steps.
- A running daemon never reloads a module library. After a run that rebuilt a module, restart the daemon:

  ```sh
  systemctl --user restart omarchy-broadcastd
  ```

- It does not install the udev rule for the MX Brio, which needs root ([the udev rule](docs/setup/daemon.md#the-udev-rule-for-the-mx-brio)), nor the WirePlumber drop-in.
- It warns when a unit in `~/.config/systemd/user` exists, because that location takes precedence ([Which unit is used](docs/setup/daemon.md#which-unit-is-used)).

**Options.**

| Option | Effect |
|---|---|
| `--mic` | Also installs the optional noise-removed microphone ([docs/setup/microphone.md](docs/setup/microphone.md)) |
| `--audio` | Also installs the microphone with audio modules, the alternative to `--mic`; the script refuses both in one run ([docs/setup/microphone.md](docs/setup/microphone.md)) |
| `--dry-run` | Prints every action and changes nothing |
| `--no-build` | Installs the binaries and the module libraries already in `${CARGO_TARGET_DIR:-target}/release` |
| `--uninstall` | Removes what the script installed, the modules' libraries and manifests included |
| `--help` | Shows the usage |

**Exit status.** 0 done. 1 refused (a target path holds a file the script did not install, or the unit is still enabled on `--uninstall`) or the build failed. 2 usage error or unusable environment.

### Manual install

1. Install the daemon and the CLI into `~/.cargo/bin`, from the repository root:

   ```sh
   cargo install --path crates/omarchy-broadcastd
   cargo install --path crates/omarchy-broadcast
   ```

2. Install the unit file with `ExecStart=` rewritten. The file in `dist/` names `/usr/bin/omarchy-broadcastd`, the path of the package. Copied unchanged after `cargo install`, the service fails to start (status 203/EXEC).

   ```sh
   mkdir -p ~/.config/systemd/user
   sed 's|^ExecStart=/usr/bin/|ExecStart=%h/.cargo/bin/|' \
       dist/systemd/omarchy-broadcastd.service \
       > ~/.config/systemd/user/omarchy-broadcastd.service
   ```

3. Optional, D-Bus activation: install the activation file. A command such as `omarchy-broadcast on` then starts the service if it is not running ([Start the daemon](docs/setup/daemon.md#start-the-daemon)).

   ```sh
   install -Dm644 dist/dbus-1/services/br.com.dias.OmarchyBroadcast.service \
       ~/.local/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service
   ```

4. Optional, discovery through PipeWire: install a drop-in that orders WirePlumber after the daemon ([Discovery by PipeWire](docs/setup/daemon.md#discovery-by-pipewire)). In a checkout it is `dist/systemd/wireplumber.service.d/omarchy-broadcast.conf`. The package ships the same file as an example at `/usr/share/doc/omarchy-broadcast/examples/systemd/wireplumber.service.d/omarchy-broadcast.conf`. The same drop-in applies to the other routes; the package does not install it.

   ```sh
   install -Dm644 dist/systemd/wireplumber.service.d/omarchy-broadcast.conf \
       ~/.config/systemd/user/wireplumber.service.d/omarchy-broadcast.conf
   ```

5. Load the units and start the service now and at every login:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now omarchy-broadcastd
   ```

   If you enabled the service earlier from an older copy of the unit, run this so that `default.target` wants it:

   ```sh
   systemctl --user reenable omarchy-broadcastd
   ```

### Run the benchmark binary

`obc-bench` streams from the camera to the "Omarchy Broadcast" loopback device until Ctrl-C:

```sh
cargo run --release -p obc-pipeline --bin obc-bench -- --duration 0
```

Then select "Omarchy Broadcast" as the camera in OBS or Chrome. Optional arguments:

- `--camera <name|vid:pid|/dev/videoN>` chooses the camera. The default is automatic. A name or ID matching two colour cameras is an error, and greyscale IR nodes are never picked.
- `--mode WxH@FPS` or `FORMAT:WxH@FPS` chooses the capture mode, for example `--mode 1920x1080@30` or `--mode NV12:1920x1080@60`. The default is `1920x1080@60`, any format. The closest mode the camera offers is used.

### Check the repository

Run `scripts/check-all.sh` to run every check of the repository. It stops at the first failure and needs neither a GPU nor a camera:

```sh
scripts/check-all.sh
```

It runs:

- the format check, clippy and the workspace tests, with `--locked`;
- the same three for the video and the audio example module, which are built outside the workspace as third-party modules would be;
- clippy and the tests of `obc-pipeline` and `omarchy-broadcastd` with the PipeWire feature;
- a check that the `pkgver=` of both PKGBUILDs matches the workspace version;
- shellcheck over the shell scripts.

Besides the Rust toolchain and the packages listed in [docs/testing.md](docs/testing.md), it needs libpipewire, clang and pipewire for the PipeWire build, and either `shellcheck` or `uvx`, which fetches it. Tests, including the manual check with OBS or Chrome, are in [docs/testing.md](docs/testing.md).

## Uninstall

1. Stop and disable the service, on every route:

   ```sh
   systemctl --user disable --now omarchy-broadcastd
   ```

2. Remove the files, depending on how you installed:

   - **Arch package:** remove any module package first, because it depends on the core. Use `omarchy-broadcast-bin` in place of `omarchy-broadcast-git` for the prebuilt package.

     ```sh
     sudo pacman -R omarchy-broadcast-git
     ```

     This removes the udev rule too. Remove any drop-in you copied from the examples (the WirePlumber one). A Maxine drop-in copied for an earlier version is not needed any more ([docs/setup/maxine.md](docs/setup/maxine.md#3-the-sdk-environment)).
   - **Per-user install:** run the script from the checkout. It removes only the files it installed, the modules' libraries and manifests among them. It refuses while the unit is still enabled, so run the command above first. A hand-made Maxine manifest stops it too; remove the files named in [docs/setup/maxine.md](docs/setup/maxine.md#8-uninstall) first.

     ```sh
     scripts/install-user.sh --uninstall
     ```
   - **Manual install:**

     ```sh
     rm ~/.config/systemd/user/omarchy-broadcastd.service
     rm -f ~/.local/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service
     rm -f ~/.config/systemd/user/wireplumber.service.d/omarchy-broadcast.conf
     cargo uninstall omarchy-broadcastd omarchy-broadcast
     ```

3. Reload the units:

   ```sh
   systemctl --user daemon-reload
   ```

4. On the per-user and manual routes, also remove the udev rule if you copied it:

   ```sh
   sudo rm /etc/udev/rules.d/70-omarchy-broadcast-mx-brio.rules
   sudo udevadm control --reload
   ```

5. The bar plugin is not part of the package. See the [omarchy-broadcast-plugin](https://github.com/rsd/omarchy-broadcast-plugin) repository for removing it. Removing the plugin does not touch the daemon.

## Details

### What the core package installs

The core package, `omarchy-broadcast-git` (or `-bin`), installs:

- `/usr/bin/omarchy-broadcastd` and `/usr/bin/omarchy-broadcast`;
- the microphone's parts (the audio worker, the mic host's unit and configuration, the forwarding plugin and the CPU voice module), all inactive until `omarchy-broadcast mic on`;
- the unit `/usr/lib/systemd/user/omarchy-broadcastd.service`;
- the D-Bus activation file `/usr/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service`;
- the directories modules are found in, `/usr/lib/omarchy-broadcast/modules` (libraries) and `/usr/lib/omarchy-broadcast/modules.d` (manifests), with the CPU voice module in them;
- the C header of the module interface, `/usr/include/omarchy-broadcast/omarchy_broadcast_module.h`;
- the udev rule for the MX Brio, `/usr/lib/udev/rules.d/70-omarchy-broadcast-mx-brio.rules`, which lets the daemon read the camera's Show Mode ([the udev rule](docs/setup/daemon.md#the-udev-rule-for-the-mx-brio));
- the licences and the documentation, in `/usr/share/doc/omarchy-broadcast/`;
- the optional WirePlumber drop-in, as an inactive example in `/usr/share/doc/omarchy-broadcast/examples/systemd/wireplumber.service.d/omarchy-broadcast.conf` ([Discovery by PipeWire](docs/setup/daemon.md#discovery-by-pipewire));
- the loopback's `modprobe.d` line of [docs/setup/loopback.md](docs/setup/loopback.md), as an inactive example, `/usr/share/doc/omarchy-broadcast/examples/modprobe.d/omarchy-broadcast.conf`, to copy into `/etc/modprobe.d`. It is not installed in `/usr/lib/modprobe.d`: only one `options v4l2loopback` line takes effect, and it would collide with one of your own.

The other modules are packages of their own, built from the same `PKGBUILD` ([docs/setup/maxine.md](docs/setup/maxine.md#the-maxine-module), [docs/setup/onnx.md](docs/setup/onnx.md), [docs/modules.md](docs/modules.md)). Each depends on the module ABI that the core provides (`omarchy-broadcast-module-abi`, a range: [docs/modules.md](docs/modules.md#packaging-a-module)). The Maxine SDKs need no drop-in: the daemon applies the SDK package's environment to itself at start ([docs/setup/maxine.md](docs/setup/maxine.md#3-the-sdk-environment)).
