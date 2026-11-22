# Install omarchy-broadcast

Install the daemon, create its virtual camera, start it and check it. The pieces you may add around it (ONNX Runtime, effect models, the NVIDIA SDKs, the bar plugin) are installed from the repository that owns each one, and this guide says when.

## Before you start

Find your case, then follow the steps for it.

| Your case | What to do |
|---|---|
| Arch Linux or Omarchy | [Steps](#steps) 1 to 6. The packages come from the AUR, prebuilt (`-bin`) or built on your machine (`-git`) |
| Another Linux distribution | [docs/building.md](docs/building.md) compiles and installs from a checkout. It sends you back here for steps 5 and 6 |
| An NVIDIA RTX GPU, and you want the NVIDIA effects | Add step 4 (optional) |
| Omarchy, and you want the bar plugin | Add [the bar plugin](#add-the-bar-plugin-optional-omarchy), after the check |

Steps 3 and 4 are optional, and you can do them after [the check](#check-that-it-works). You also need a webcam. Any standard webcam works, USB or built-in. Cameras that are not standard webcams may not work: see [LIMITATIONS.md](LIMITATIONS.md#cameras-that-are-not-standard-webcams).

## Steps

1. **Install ONNX Runtime.** The open-model effects (background blur and replacement without an NVIDIA SDK) need ONNX Runtime 1.29 or later. Which package you install decides the engine the effects run on: the CPU, an NVIDIA GPU or an Intel CPU, GPU or NPU. [omarchy-onnxruntime](https://github.com/rsd/omarchy-onnxruntime) provides ONNX Runtime 1.29.0 as pacman packages for Arch and Omarchy, one per engine, and makes that choice: [step 1 of its INSTALL.md](https://github.com/rsd/omarchy-onnxruntime/blob/main/INSTALL.md#1-choose-your-packages) is the table of hardware and packages. Install ONNX Runtime from there first. If none is installed when you run step 2, your AUR helper picks a provider of `onnxruntime` or asks you to choose one, and a provider can be CPU-only. Then check the version:

   ```sh
   pacman -T 'onnxruntime>=1.29'
   ```

   The command prints nothing when it is satisfied. That means only that some ONNX Runtime 1.29 or later is installed, a CPU-only one included: it does not say which engine the effects will use. The [check below](#check-that-it-works) shows that. If the command prints the requirement, install ONNX Runtime first.

2. **Install the daemon and the open-model module.** Take one of the two kinds. The `-bin` packages are prebuilt. The `-git` packages are built on your machine. They conflict, so install one kind.

   ```sh
   yay -S omarchy-broadcast-bin omarchy-broadcast-onnx-bin     # prebuilt
   yay -S omarchy-broadcast-git omarchy-broadcast-onnx-git     # built from source
   ```

   The command installs the core and the module. The module's package also brings in its default model, so the open-model effects work without a further download. On a first install the package prints the next steps, which are steps 5 and 6 below.

   The Maxine module packages of step 4 depend on the core, not on the open-model module or on ONNX Runtime, and [docs/setup/maxine.md](docs/setup/maxine.md#quick-start) installs them with the core alone. Without the open-model module you get no background effect while the video runs in system memory, because only the CPU path can run it there: with no usable CUDA, or while the battery saver is active ([docs/setup/onnx.md](docs/setup/onnx.md#using-it-on-a-machine-without-an-nvidia-gpu)).

3. **Add more models (optional).** [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models) holds four open person-segmentation models as ONNX files, with checksums and licences, and one Arch package for each. You can do this after the check, because the default model is already installed. Install more of them there, then choose one in [docs/setup/onnx.md](docs/setup/onnx.md#choose-the-model-and-the-engine).

4. **Add the NVIDIA effects (optional, NVIDIA RTX GPU only).** The NVIDIA Maxine effects need an NVIDIA RTX GPU and its driver ([requirements](docs/setup/maxine.md#1-requirements)), and an SDK that you download from NVIDIA yourself. [omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) turns each SDK into a local pacman package: `maxine-vfx-sdk-local` for the video effects, `maxine-ar-sdk-local` for eye contact and auto-framing, `maxine-afx-sdk-local` for the microphone's effects. Build and install the SDK packages you want there first. [docs/setup/maxine.md](docs/setup/maxine.md#2-the-sdk-package) lists the SDK features each effect needs, and that repository covers the microphone's effects and their sample rates.

   **Install only the module packages whose SDK package you built.** A module package depends on its SDK package, so its install stops without it. The video and AR module packages also pull in `cuda` as a dependency, so you do not install it yourself. Each line below is one module:

   ```sh
   yay -S omarchy-broadcast-maxine-vfx-bin    # video effects; needs maxine-vfx-sdk-local
   yay -S omarchy-broadcast-maxine-ar-bin     # eye contact and auto-framing; needs maxine-ar-sdk-local
   yay -S omarchy-broadcast-maxine-afx-bin    # microphone effects; needs maxine-afx-sdk-local
   ```

   Use `-git` in place of `-bin` if you chose the source kind in step 2. The daemon lists the modules once it runs (see [Check that it works](#check-that-it-works)). You can do this step after the check: install the packages, then restart the daemon as [Update](#update) shows. [docs/setup/maxine.md](docs/setup/maxine.md) covers their setup and checks.

5. **Create the virtual camera.** This needs root and runs once. The core package depends on a v4l2loopback kernel module (`v4l2loopback-dkms` provides it), so step 2 installed one if the system had none. The package ships the `modprobe.d` line as an example.

   ```sh
   sudo install -Dm644 /usr/share/doc/omarchy-broadcast/examples/modprobe.d/omarchy-broadcast.conf /etc/modprobe.d/omarchy-broadcast.conf
   echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
   sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback
   ```

   The second command prints `v4l2loopback`. The other two print nothing. Only one `options v4l2loopback` line takes effect. If `/etc/modprobe.d` already has one, merge the two lines as [docs/setup/loopback.md](docs/setup/loopback.md#several-devices) describes. Then check that the device exists:

   ```sh
   cat /sys/class/video4linux/video10/name
   ```

   Expect `Omarchy Broadcast`. If `modprobe` fails with `Module v4l2loopback is in use`, an application holds the camera: see [Troubleshooting](#troubleshooting). If it says the module is not found, the running kernel has no v4l2loopback module built for it ([the requirement](docs/setup/loopback.md#before-you-start)).

6. **Start the daemon at every login.**

   ```sh
   systemctl --user enable --now omarchy-broadcastd
   ```

   systemd reports the link it creates, and the daemon runs from now on. The package enables nothing by itself.

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

After step 6 on a first install, `State` is `idle`: no application uses the camera yet, and you need no `omarchy-broadcast on`. It is `active` while an application uses the camera. It is `off` only if you turned the camera off earlier, because that state persists, and `omarchy-broadcast on` turns the camera back on. For the service itself, check that it is `active (running)`:

```sh
systemctl --user status omarchy-broadcastd
```

Select **Omarchy Broadcast** as the camera in an application, such as the browser Omarchy ships (Chromium) or OBS. With `exclusive_caps=1`, the device is listed as a camera only while the daemon runs ([why](docs/setup/loopback.md#why-exclusive_caps1)), so step 6 comes first. Then choose an effect:

```sh
omarchy-broadcast effect blur
```

The background in the application's picture is blurred. To see which module and which engine did it, list the modules:

```sh
omarchy-broadcast modules
```

Each module is a line with its stages under it, and `selected` marks the module that provides a stage:

```text
Profile: quality
br.com.dias.onnx-cutout  Open-model cut-out 0.57.7  available
    background (matte, cpu, rank 200) selected
```

A stage that cannot run says `unavailable: <reason>` in place of `selected`. The `cpu` in the stage line is the module's device class for the profiles, not the engine. With the Maxine video module installed too, it lists `background` as well, under `br.com.dias.maxine-vfx`, and the module marked `selected` is the one that provides it. The `quality` profile picks Maxine, the `battery` profile picks the open-model module, and you can pin one ([Choose the module](docs/setup/onnx.md#choose-the-module)).

The engine is in the daemon's log. The line appears when the effect loads, so run this after the `effect blur` above, while an application holds the camera:

```sh
journalctl --user -u omarchy-broadcastd | grep 'ONNX cut-out'
```

Expect a line that names the engine and the ONNX Runtime version:

```text
ONNX cut-out: mediapipe-landscape runs on CPU (ONNX Runtime 1.29.0; session ... ms, first run ... ms)
```

The engine is `CPU` here. It can also be `NVIDIA TensorRT`, `NVIDIA CUDA`, `OpenVINO NPU`, `OpenVINO GPU` or `OpenVINO CPU`. If it names an engine you did not expect, the other `ONNX cut-out:` lines give the reason ([Reading the reasons](docs/setup/onnx.md#5-reading-the-reasons)), and the package you installed in step 1 sets what is available.

Every command of the CLI is in [docs/cli-json.md](docs/cli-json.md). The guides cover the rest: [the daemon](docs/setup/daemon.md) (the unit, the optional PipeWire camera, the configuration), [the virtual camera device](docs/setup/loopback.md), [the open-model effects](docs/setup/onnx.md) and [the NVIDIA effects](docs/setup/maxine.md).

## Add the virtual microphone (optional)

The virtual microphone is optional and off until you turn it on. [docs/setup/microphone.md](docs/setup/microphone.md) covers turning it on, choosing it as the default microphone and its noise removal, on the CPU or on an NVIDIA GPU.

## Add the bar plugin (optional, Omarchy)

The Omarchy bar plugin is a separate project, [omarchy-broadcast-plugin](https://github.com/rsd/omarchy-broadcast-plugin). It adds a bar icon with the camera's state and a panel with a live preview, the effects, the camera's settings and the microphone. It needs the daemon you have just checked. Its own INSTALL.md adds and enables it.

## Update

Update the packages the way you installed them, with `yay`. A running daemon keeps its old version until it restarts. On an upgrade the package prints one line, which restarts the daemon only if it is running:

```sh
systemctl --user try-restart omarchy-broadcastd
```

The daemon runs the new version afterwards. A module library that is rebuilt takes a restart too.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| The reload fails with "Module v4l2loopback is in use" | An application holds a loopback node | Close it, check with `fuser -v /dev/video10`, and run the reload again |
| `omarchy-broadcast status` shows the state `error` | The v4l2loopback device labelled "Omarchy Broadcast" is missing. The daemon retries every 2 s | Create the virtual camera (step 5). [docs/setup/loopback.md](docs/setup/loopback.md#troubleshooting) has more |
| `omarchy-broadcast` exits with status 3 | The daemon is not running, or there is no D-Bus session bus | Run `omarchy-broadcast start` or step 6, then `systemctl --user status omarchy-broadcastd` |
| The AUR helper offers the previous version | The AUR's RPC index lags a few minutes after a release | Wait and try again |
| The install of a Maxine module package stops on `maxine-vfx-sdk-local` (or `-ar-`, `-afx-`) | The SDK package is built locally from your own download and is not installed | Build and install it with [omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) first (step 4) |
| An old daemon keeps starting after you installed the package | A copy of the unit in `~/.config/systemd/user` shadows the packaged one | [docs/setup/daemon.md](docs/setup/daemon.md#which-unit-is-used) |

More in the troubleshooting sections of the guides: [daemon](docs/setup/daemon.md#troubleshooting), [loopback](docs/setup/loopback.md#troubleshooting), [ONNX](docs/setup/onnx.md#8-troubleshooting), [Maxine](docs/setup/maxine.md#7-troubleshooting) and [microphone](docs/setup/microphone.md#troubleshooting).

## Uninstall

1. Stop and disable the service:

   ```sh
   systemctl --user disable --now omarchy-broadcastd
   ```

2. Remove the packages. Name every module package you installed together with the core, because a module depends on it. This example is the prebuilt kind with the ONNX module. Use `-git` for the source kind. The second command reloads the units:

   ```sh
   sudo pacman -R omarchy-broadcast-onnx-bin omarchy-broadcast-bin
   systemctl --user daemon-reload
   ```

   This removes the udev rule too. Remove any drop-in you copied from the examples (the WirePlumber one). A Maxine drop-in copied for an earlier version is not needed any more ([docs/setup/maxine.md](docs/setup/maxine.md#3-the-sdk-environment)). The Maxine module packages and the SDK packages are removed as [docs/setup/maxine.md](docs/setup/maxine.md#8-uninstall) describes. For a build from a checkout, see [docs/building.md](docs/building.md#uninstall-a-build-from-source).

3. The bar plugin is not part of the package. See the [omarchy-broadcast-plugin](https://github.com/rsd/omarchy-broadcast-plugin) repository for removing it. Removing the plugin does not touch the daemon.

ONNX Runtime, the model packages and the SDK packages belong to their own repositories, and removing them is described there.
