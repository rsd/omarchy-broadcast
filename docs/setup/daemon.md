# Running omarchy-broadcastd as a user service

This guide installs the daemon, `omarchy-broadcastd`, and runs it as a systemd user service. You need it on every machine: the daemon is the program that makes the virtual camera work.

The daemon owns the D-Bus name `br.com.dias.OmarchyBroadcast` on the session bus. It starts the camera and the GPU pipeline only while an application uses the virtual camera ([architecture.md](../architecture.md), [ADR-0011](../adr/0011-daemon-lifecycle-and-dbus-api.md)). The build installs nothing; you pick one of four routes below.

## Before you start

- The v4l2loopback device with the label "Omarchy Broadcast" and `exclusive_caps=1`, set up as in [loopback.md](loopback.md). The daemon finds the device by its label. Without it the daemon runs in the state `error` and retries every 2 s.
- A systemd user session with a D-Bus session bus. This is the normal state of a desktop login.
- Optional, for the GPU path and the Maxine modules: an NVIDIA GPU, the NVIDIA driver (`nvidia-utils`), CUDA (`cuda`, for NVRTC) and GStreamer's `nvcodec` plugin ([README, Requirements](../../README.md#requirements)). See [Without CUDA or without an NVIDIA GPU](#without-cuda-or-without-an-nvidia-gpu).

### Without CUDA or without an NVIDIA GPU

Without an NVIDIA GPU you need nothing extra. The daemon runs the video in system memory ([onnx.md](onnx.md#using-it-on-a-machine-without-an-nvidia-gpu)).

On a machine with an NVIDIA GPU, install `cuda` as well as `nvidia-utils` for the GPU path, because GStreamer's CUDA converters need NVRTC from `cuda`. Without it:

- The default `[video] memory = "auto"` runs the video in system memory. The log and the `video.memory` row say why: "the CUDA toolkit (NVRTC) is not installed; install `cuda` for the GPU path".
- An explicit `memory = "cuda"` fails the camera start with that reason.
- To run without CUDA on purpose, set `[video] memory = "system"`.

## Install

Choose one route.

| Route | Root needed | Use it when |
|---|---|---|
| [From the AUR](#from-the-aur) | yes (pacman) | You want a package, prebuilt or built from the latest source |
| [Arch package from a checkout](#arch-package-from-a-checkout) | yes (pacman) | You build the package from a local checkout |
| [Per-user install from a checkout](#per-user-install-from-a-checkout) | no | You want an install in your home directory |
| [Manual install](#manual-install) | no | You want only the two binaries and the unit file |

### From the AUR

The AUR has the split package of `packaging/arch/PKGBUILD` in two forms. Install one of them. They conflict with each other, and the modules of one form work with the core of the other.

- `omarchy-broadcast-bin` (pkgbase) is prebuilt and quick to install. Its files come from the GitHub release of the same version, are checked against a sha256 in the recipe, and need no build tools. Its packages are `omarchy-broadcast-bin`, `omarchy-broadcast-onnx-bin`, `omarchy-broadcast-maxine-vfx-bin`, `omarchy-broadcast-maxine-ar-bin` and `omarchy-broadcast-maxine-afx-bin`.
- `omarchy-broadcast-git` (pkgbase) builds from the latest source of this repository on GitHub and needs the build tools of `packaging/arch/PKGBUILD`. Its packages have the same names with `-git` in place of `-bin`.

Each `-bin` package installs exactly the files of its `-git` twin. It provides the plain name (`omarchy-broadcast`, `omarchy-broadcast-onnx`, and so on) and conflicts with the plain name and with the twin.

1. Install the core and the open-model cut-out module with an AUR helper such as `yay`:

   ```sh
   yay -S omarchy-broadcast-bin omarchy-broadcast-onnx-bin   # prebuilt
   yay -S omarchy-broadcast-git omarchy-broadcast-onnx-git   # built from source
   ```

2. Add what your GPU needs.

   On a machine with an NVIDIA GPU, add `cuda` (NVRTC, needed by the CUDA video path). `nvidia-utils` comes with the NVIDIA driver.

   ```sh
   yay -S cuda omarchy-broadcast-bin omarchy-broadcast-onnx-bin
   ```

   On a machine without an NVIDIA GPU, install the CPU build of ONNX Runtime with the module. Or pick `onnxruntime-cpu` when yay asks which provider of `onnxruntime` to install.

   ```sh
   yay -S onnxruntime-cpu omarchy-broadcast-onnx-bin
   ```

   The other providers build a much larger runtime: the AUR's `onnxruntime-core` builds with CUDA, cuDNN, OpenVINO and TensorRT. [onnx.md](onnx.md#2-install-onnx-runtime) lists them.

3. Optional, on an NVIDIA RTX GPU: install the Maxine modules `omarchy-broadcast-maxine-vfx-bin`, `omarchy-broadcast-maxine-ar-bin` and `omarchy-broadcast-maxine-afx-bin` (`-git` in place of `-bin` for the source build). Each depends on an SDK package that you build yourself from your own download of the NVIDIA SDK, as in [maxine.md](maxine.md). Install that SDK package first.

4. Configure the v4l2loopback device as in [loopback.md](loopback.md). This needs root, once. The package ships the `modprobe.d` line as `/usr/share/doc/omarchy-broadcast/examples/modprobe.d/omarchy-broadcast.conf`.

5. Start the daemon, now and at every login:

   ```sh
   systemctl --user enable --now omarchy-broadcastd
   ```

The package prints these steps after it is installed.

The prebuilt packages do not contain the NVIDIA SDKs or ONNX Runtime, which are licensed separately. The Maxine and ONNX modules hold no code of either: they load the SDK or the runtime when the daemon runs. So the `-bin` modules need the SDK packages and an `onnxruntime` package exactly as the `-git` ones do.

### Arch package from a checkout

`packaging/arch/PKGBUILD` builds one split package of five from the committed HEAD of your local checkout. Uncommitted changes are not included. The five are the core `omarchy-broadcast-git` and a package per loadable module. The [README table](../../README.md#choose-your-modules) lists them by their base names (`omarchy-broadcast`, `omarchy-broadcast-onnx`, and so on); add `-git` to each name.

1. Build from `packaging/arch`, then install the core and the module packages you want from the same build:

   ```sh
   cd packaging/arch
   makepkg -s
   sudo pacman -U omarchy-broadcast-git-*.pkg.tar.zst
   ```

2. Configure the v4l2loopback device as in [loopback.md](loopback.md). The package does not do this for you.

3. Start the daemon as each user. Nothing is enabled by the install. Pacman prints the next steps from the install scriptlet `packaging/arch/omarchy-broadcast.install`, which changes nothing.

   ```sh
   systemctl --user enable --now omarchy-broadcastd
   ```

The core package, `omarchy-broadcast-git`, installs:

- `/usr/bin/omarchy-broadcastd` and `/usr/bin/omarchy-broadcast`;
- the unit `/usr/lib/systemd/user/omarchy-broadcastd.service`;
- the D-Bus activation file `/usr/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service`;
- the directories modules are found in, `/usr/lib/omarchy-broadcast/modules` (libraries) and `/usr/lib/omarchy-broadcast/modules.d` (manifests), with the CPU voice module in them;
- the C header of the module interface, `/usr/include/omarchy-broadcast/omarchy_broadcast_module.h`;
- the udev rule for the MX Brio, `/usr/lib/udev/rules.d/70-omarchy-broadcast-mx-brio.rules`, which lets the daemon read the camera's Show Mode ([The udev rule for the MX Brio](#the-udev-rule-for-the-mx-brio));
- the licences and this documentation, in `/usr/share/doc/omarchy-broadcast/`;
- the optional WirePlumber drop-in, as an inactive example in `/usr/share/doc/omarchy-broadcast/examples/systemd/wireplumber.service.d/omarchy-broadcast.conf` ([Discovery by PipeWire](#discovery-by-pipewire));
- the loopback's `modprobe.d` line of [loopback.md](loopback.md), as an inactive example, `/usr/share/doc/omarchy-broadcast/examples/modprobe.d/omarchy-broadcast.conf`, to copy into `/etc/modprobe.d`. It is not installed in `/usr/lib/modprobe.d`: only one `options v4l2loopback` line takes effect, and it would collide with one of your own.

The other modules are packages of their own, built from the same `PKGBUILD`: `omarchy-broadcast-maxine-vfx-git`, `-maxine-ar-git`, `-maxine-afx-git` and `-onnx-git` ([maxine.md](maxine.md#the-maxine-module), [onnx.md](onnx.md), [../modules.md](../modules.md)). Each depends on the module ABI that the core provides (`omarchy-broadcast-module-abi`, a range: [../modules.md](../modules.md#packaging-a-module)). The Maxine SDKs need no drop-in: the daemon applies the SDK package's environment to itself at start ([maxine.md](maxine.md#3-the-sdk-environment)).

**Dependencies.**

- Required: `gst-plugins-good`, `gst-plugins-bad-libs` and a v4l2loopback kernel module (for example `v4l2loopback-dkms`).
- Optional, for the GPU path and the Maxine modules: `nvidia-utils` and `cuda` (NVRTC, used by GStreamer's CUDA converters). The daemon opens them at run time. See [Without CUDA or without an NVIDIA GPU](#without-cuda-or-without-an-nvidia-gpu). The Maxine video and AR module packages depend on `cuda`, and the Maxine SDK packages on `nvidia-utils`.
- Optional: `ffmpeg`. The daemon runs it, at low priority, to convert an animated GIF, WebP or APNG at import and to make the poster of an imported video. On a machine without an NVIDIA GPU it is also the program of the command that converts a video the CPU cannot play ([Playing a video without an NVIDIA GPU](#playing-a-video-without-an-nvidia-gpu)); the daemon never runs that command. The bundled video loops need none of it, and with an NVIDIA GPU a video plays as it is ([../adr/0035-video-backgrounds.md](../adr/0035-video-backgrounds.md)).
- The NVIDIA Maxine SDKs are never part of the packages. Each Maxine module package depends on the SDK package of its own (`maxine-vfx-sdk-local`, `maxine-ar-sdk-local` or `maxine-afx-sdk-local`). You build it yourself from the omarchy-nvidia-sdks repository and install it as in [maxine.md](maxine.md).

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

The script builds the release binaries and installs:

- `omarchy-broadcastd` and `omarchy-broadcast` in `${XDG_BIN_HOME:-~/.local/bin}`;
- the unit in `${XDG_DATA_HOME:-~/.local/share}/systemd/user/`, with `ExecStart=` pointing at the installed daemon;
- the D-Bus activation file in `${XDG_DATA_HOME:-~/.local/share}/dbus-1/services/`;
- the modules, each as a library in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules/` and a manifest in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/`, whose `library` line names the installed library:
  - the NVIDIA Maxine video module (`libobc_module_maxine.so`, `maxine-vfx.toml`);
  - the NVIDIA Maxine AR module (`libobc_module_maxine_ar.so`, `maxine-ar.toml`);
  - the open-model cut-out module (`libobc_module_onnx.so`, `onnx.toml`);
- a manifest, `install-user.manifest`, in `${XDG_STATE_HOME:-~/.local/state}/omarchy-broadcast/`. It lists every path written (the modules' libraries and manifests included) and the sha256 of each binary and of each module library.

The Maxine modules are built and installed whether or not the Maxine SDK is present. Without the SDK the daemon lists the module greyed out, with the reason. The open-model cut-out module is installed without ONNX Runtime and without models: the daemon lists it as "model not installed" until the four model files are in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/models/onnx`. Set `OBC_ONNX_MODELS_DIR` to a directory that holds them, and the script installs them once their sha256 match the module's catalogue ([../../crates/obc-module-onnx/README.md](../../crates/obc-module-onnx/README.md)).

The script does not install the SDKs, the os-release shim or any drop-in; [maxine.md](maxine.md) describes them. An older version of the script installed a shim in `~/.local/lib/omarchy-broadcast/`. A reinstall or `--uninstall` removes it.

**Behaviour.**

- It marks every text file it writes. It overwrites or removes a file only if it carries that mark. For a binary, a module library included, it does so only if the manifest lists it with the checksum on disk. It stops before changing anything if a target path holds another file. Running it again updates its own install.
- A Maxine manifest at `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/maxine-vfx.toml` that it did not write has no mark, so the script refuses it. An example is the hand-made copy of the manual recipe in [maxine.md](maxine.md). Remove that file and the library its `library` line names, then run the script again. This applies to `--uninstall` as well.
- For an install, the data directory must be an absolute path made only of letters, digits and `._/@+-`. Its path is written into the Maxine manifest and, with `--mic`, into the microphone's unit. Otherwise the script exits with status 2.
- It runs no `systemctl` command and enables nothing. It prints the next steps.
- A running daemon never reloads a module library. After a run that rebuilt a module, restart the daemon:

  ```sh
  systemctl --user restart omarchy-broadcastd
  ```

- It does not install the udev rule for the MX Brio, which needs root ([The udev rule for the MX Brio](#the-udev-rule-for-the-mx-brio)), nor the WirePlumber drop-in. The Omarchy bar plugin is a separate project (the omarchy-broadcast-plugin repository).
- It warns when a unit in `~/.config/systemd/user` exists, because that location takes precedence ([Which unit is used](#which-unit-is-used)).

**Options.**

| Option | Effect |
|---|---|
| `--mic` | Also installs the optional noise-removed microphone ([microphone.md](microphone.md)) |
| `--audio` | Also installs the microphone with audio modules, the alternative to `--mic`; the script refuses both in one run ([microphone.md](microphone.md)) |
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

3. Optional, D-Bus activation: install the activation file. A command such as `omarchy-broadcast on` then starts the service if it is not running ([Start the daemon](#start-the-daemon)).

   ```sh
   install -Dm644 dist/dbus-1/services/br.com.dias.OmarchyBroadcast.service \
       ~/.local/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service
   ```

4. Optional, discovery through PipeWire: install a drop-in that orders WirePlumber after the daemon ([Discovery by PipeWire](#discovery-by-pipewire)). In a checkout it is `dist/systemd/wireplumber.service.d/omarchy-broadcast.conf`. The package ships the same file as an example at `/usr/share/doc/omarchy-broadcast/examples/systemd/wireplumber.service.d/omarchy-broadcast.conf`. The same drop-in applies to the other routes; the package does not install it.

   ```sh
   install -Dm644 dist/systemd/wireplumber.service.d/omarchy-broadcast.conf \
       ~/.config/systemd/user/wireplumber.service.d/omarchy-broadcast.conf
   ```

5. Load the units and start the service now and at every login:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now omarchy-broadcastd
   ```

   If you enabled the service earlier from an older copy of the unit, run `systemctl --user reenable omarchy-broadcastd` so that `default.target` wants it.

## Start the daemon

`systemctl --user enable --now omarchy-broadcastd` starts the daemon now and at every login. Each install route above ends with it.

To start the daemon now without enabling it, use D-Bus activation:

```sh
omarchy-broadcast start
```

This starts the service through its D-Bus activation file, if it is not running, and prints the status. It changes no state: the daemon restores what it saved and never turns the camera on by itself. The camera opens when an application uses it. The Arch package and the per-user install ship the activation file; the manual install has it as the optional step 3 ([dbus-api.md](../dbus-api.md#service-presence-and-activation)).

Which commands start the daemon:

- Commands that read never start it: `omarchy-broadcast status` and `watch`, among others.
- Commands that change something, such as `omarchy-broadcast on`, start it through the activation file when it is installed.

## Check that it works

```sh
systemctl --user status omarchy-broadcastd
omarchy-broadcast status
```

The service should be `active (running)`, and `status` should print the state, the camera and the output mode. The service is ready (`Type=dbus`) once it owns its bus name. It requests the name after the loopback writer has started, so the virtual camera is already listed as a capture device at that point.

If `status` shows an `Error:` line, see Troubleshooting.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| The daemon is in the state `error` and retries every 2 s | The v4l2loopback device with the label "Omarchy Broadcast" is missing | Set it up as in [loopback.md](loopback.md) |
| The service fails with status 203/EXEC | The unit names `/usr/bin/omarchy-broadcastd`, but the daemon is in `~/.cargo/bin` | Rewrite `ExecStart=` ([Manual install](#manual-install), step 2) |
| An old daemon keeps starting after you installed the package | A copy of the unit in `~/.config/systemd/user` shadows the packaged one | See [Which unit is used](#which-unit-is-used) |
| The unit is `failed` after repeated crashes | systemd's start limit: 5 starts within 60 s | Run `systemctl --user reset-failed omarchy-broadcastd`, then start it ([Restarts and crashes](#restarts-and-crashes)) |
| The daemon exits with status 3 and is not restarted | Another instance already owns the bus name | Stop the other instance, then start the service |
| `omarchy-broadcast status` shows `Error:` after you edited the configuration | The daemon rejected a file as a whole and kept the previous configuration | Read the file, line and key in `Error`, fix it and reload ([Configuration](#configuration)) |
| The daemon runs the video in system memory on an NVIDIA machine | `cuda` (NVRTC) is not installed | Install `cuda`, or set `[video] memory = "system"` on purpose ([Without CUDA or without an NVIDIA GPU](#without-cuda-or-without-an-nvidia-gpu)) |
| A rebuilt module does not take effect | The daemon never reloads a module library | Run `systemctl --user restart omarchy-broadcastd` |
| Applications that list cameras through PipeWire do not show the virtual camera | WirePlumber probed the loopback before the daemon wrote to it | See [Discovery by PipeWire](#discovery-by-pipewire) |
| A second application gets `EBUSY` or no camera | A v4l2loopback device serves one capture stream at a time | See [loopback.md](loopback.md#one-application-at-a-time) |
| A PipeWire consumer does not return after a daemon crash | The PipeWire node lives in the daemon process | Use the loopback device in that application ([Restarts and crashes](#restarts-and-crashes)) |
| OBS Studio crashes when you pick NV12 for the PipeWire source | A bug in OBS | See [Known issue: OBS Studio and NV12](#known-issue-obs-studio-and-nv12) |
| The MX Brio's Show Mode stays unknown | The `hidraw` node belongs to root | Install the udev rule ([The udev rule for the MX Brio](#the-udev-rule-for-the-mx-brio)) |
| A video background shows as a still picture | The CPU plays only some video files | See [Playing a video without an NVIDIA GPU](#playing-a-video-without-an-nvidia-gpu) |

### Which unit is used

systemd searches the user unit directories in this order and uses the first match:

1. `~/.config/systemd/user`
2. `~/.local/share/systemd/user`
3. `/usr/lib/systemd/user`

A copy of the unit in a directory earlier in the list shadows the others. For example, an older manual copy in `~/.config/systemd/user` keeps starting `~/.cargo/bin/omarchy-broadcastd` after you install the package. Remove it and run `systemctl --user daemon-reload` to use the packaged unit. To see which file is in force:

```sh
systemctl --user cat omarchy-broadcastd
```

## Stop, disable, uninstall

1. Stop and disable the service, on every route:

   ```sh
   systemctl --user disable --now omarchy-broadcastd
   ```

2. Remove the files, depending on how you installed:

   - **Arch package:** `sudo pacman -R omarchy-broadcast-git`, or `sudo pacman -R omarchy-broadcast-bin` for the prebuilt package. Remove any module package first, because it depends on the core. This removes the udev rule too. Remove any drop-in you copied from the examples (the WirePlumber one). A Maxine drop-in copied for an earlier version is not needed any more ([maxine.md](maxine.md#3-the-sdk-environment)). The bar plugin is not part of the package; see the omarchy-broadcast-plugin repository for removing it.
   - **Per-user install:** run `scripts/install-user.sh --uninstall` from the checkout. It removes only the files it installed, the modules' libraries and manifests among them. It refuses while the unit is still enabled, so run the command above first. A hand-made Maxine manifest stops it too; remove the files named in [maxine.md](maxine.md#8-uninstall) first.
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

## Optional setup

### The udev rule for the MX Brio

The daemon reads the Logitech MX Brio's Show Mode (desk view or normal) from the camera's HID interface, only while the camera streams ([../cameras/mx-brio.md](../cameras/mx-brio.md#camera-status-in-omarchy-broadcast)). A `hidraw` node belongs to root by default. Without access to it the status stays unknown; nothing else is affected.

The rule `dist/udev/70-omarchy-broadcast-mx-brio.rules` tags the `hidraw` nodes of USB `046d:0944`, and of no other device, with `uaccess`. systemd-logind then gives the user of the active local seat an ACL on the node, read and write. The daemon opens it read-only and never writes to it.

The Arch package installs the rule in `/usr/lib/udev/rules.d/`. The per-user install and the manual install do not, because a rule needs root. To add it from a checkout:

1. Install the rule and reload udev:

   ```sh
   sudo install -Dm644 dist/udev/70-omarchy-broadcast-mx-brio.rules \
       /etc/udev/rules.d/70-omarchy-broadcast-mx-brio.rules
   sudo udevadm control --reload
   ```

2. Unplug and replug the camera. The rule applies to a node when it is created.

Keep the `70-` prefix: the file name must sort before `73-seat-late.rules`, where systemd turns the `uaccess` tag into the ACL.

### Discovery by PipeWire

WirePlumber probes video devices when it starts. A loopback device that has no writer at that moment is recorded as output-only and not probed again. Applications that list cameras through PipeWire (the camera portal, OBS's PipeWire source, browsers using PipeWire) then do not show it. Two remedies:

- Install the drop-in ([Manual install](#manual-install), step 4). It starts WirePlumber after the daemon is ready. Run `systemctl --user daemon-reload` after installing it, and restart WirePlumber once for the current session.
- Without the drop-in, or after you restarted the daemon by hand, restart WirePlumber while the daemon runs:

  ```sh
  systemctl --user restart wireplumber
  ```

Applications that open `/dev/video*` directly are not affected. Meanwhile, `gst-device-monitor-1.0 -i Video/Source` lists the camera through the v4l2 provider. Details are in [testing.md](../testing.md#discovery-by-pipewire).

The daemon can also register its own PipeWire source, which needs none of this: see [The PipeWire output](#the-pipewire-output).

A v4l2loopback device serves one capture stream at a time: a second application that opens the same device gets `EBUSY` at `VIDIOC_S_FMT` or `VIDIOC_REQBUFS`. Applications that need the camera at the same moment should use the PipeWire node, which serves several consumers (with the limitation under [The PipeWire output](#the-pipewire-output)). Or give the loopback more devices. [loopback.md](loopback.md#one-application-at-a-time) explains why the limit exists and which applications share the camera by themselves.

### The PipeWire output

The daemon can also register a native PipeWire video source, "Omarchy Broadcast", next to the loopback device ([ADR-0016](../adr/0016-pipewire-output.md)). The loopback stays as it is and remains the default. Applications that use PipeWire for cameras find the node without WirePlumber having to probe the loopback.

It is off in two places: the daemon must be built with the cargo feature, and the configuration must enable it.

**Build.** The Arch package is built with the feature. From source, build or install with it. It needs the libpipewire headers and clang at build time and links `libpipewire-0.3`:

```sh
cargo build --release --features omarchy-broadcastd/pipewire
cargo install --path crates/omarchy-broadcastd --features pipewire
```

`scripts/install-user.sh` builds without it.

**Enable.** Add this to `~/.config/omarchy-broadcast/config.toml`:

```toml
[pipewire]
enabled = true
```

Then reload:

```sh
systemctl --user reload omarchy-broadcastd
```

The node appears at once. Setting `enabled` back to `false` removes it at once if no application is linked to it, otherwise when the last one leaves. The loopback is not touched either way. The node exists only while the loopback writer runs.

**Check.**

```sh
omarchy-broadcast status          # Consumers: 0 (loopback 0, PipeWire 0), PipeWire: ready
pw-cli ls Node | grep -B2 -A8 omarchy-broadcast
gst-launch-1.0 pipewiresrc target-object=omarchy-broadcast ! videoconvert ! autovideosink
```

`PipeWire:` in `status` shows one of these:

| Value | Meaning |
|---|---|
| `ready` | The node is registered |
| `streaming` | An application is linked |
| `failed` | PipeWire is not running or the connection was lost. The daemon retries every 2 s, stays usable, and the loopback keeps working |
| `unsupported` | The configuration enables the node in a daemon built without the feature |

A PipeWire consumer starts the camera like a loopback consumer does, and `Consumers` is the total of both kinds.

**Applications.**

| Application | Setting |
|---|---|
| Firefox | Uses the camera portal. The preference `media.webrtc.camera.allow-pipewire` in `about:config` must be `true` (the default in some distributions) |
| Chromium, Chrome, Electron applications | Off by default. Enable the "PipeWire Camera support" flag (`chrome://flags`, feature `WebRtcPipeWireCamera`), or start the browser with `--enable-features=WebRtcPipeWireCamera` |
| OBS Studio 30.1 and later | Add a "Video Capture Device (PipeWire)" source |
| GNOME Snapshot, GStreamer `pipewiresrc` | Work without a setting |
| Zoom, Discord | Do not use the PipeWire camera. Select the virtual camera (the loopback device) |

**Formats.** The node offers YUY2, NV12 and I420, in that order, at the output contract's size and rate. YUY2 is first because OBS Studio's PipeWire camera source mis-sizes NV12 rows ([Known issue](#known-issue-obs-studio-and-nv12)). An application that asks for NV12 or I420 explicitly still gets it. An application that asks for another format or size fails to link, and nothing else is affected.

NV12 is offered by default. To offer YUY2 and I420 only, set `offer_nv12 = false`:

```toml
[pipewire]
enabled = true
offer_nv12 = false
```

A reload that changes it rebuilds the node, like enabling or disabling it: at once if no application is linked to the node, otherwise when the last one leaves. The loopback is not touched.

**Known limitation: consumers that ask for different formats.** The node has one port and one negotiated format. While one consumer streams a format (YUY2, say), a second one that accepts only another (NV12, say) fails to link. The first one can stop receiving frames from the moment the second connects. The stream recovers once every consumer has disconnected. Consumers that ask for the same format run together.

This is one reason the PipeWire output is optional and off by default. Where applications that ask for different formats must run at the same time, let one of them use the loopback device instead.

#### Known issue: OBS Studio and NV12

OBS Studio's PipeWire camera source (`plugins/linux-pipewire`; present in 32.2.2 and on the master branch as of November 2026) computes the row size of an NV12 frame as 2 bytes per pixel. It ignores the stride and the plane offsets the producer announces. For a 1920x1080 frame it reads rows 3840 bytes apart from a buffer that holds 1920 bytes per luma row, so it reads past the end of the buffer and OBS crashes. The chroma plane is not copied either.

This is in OBS, not in the node: the node's buffer is a whole NV12 frame in the layout GStreamer's `GstVideoInfo` gives, and other applications read it correctly.

OBS lists the node's formats in the source's properties, in the order the node offers them, and starts with the first one. With the node's order (YUY2 first), OBS uses YUY2, which works. The crash happens only when you choose NV12 by hand in the source's format selection.

Workarounds, either of them:

- Leave the format of the OBS source on YUY2.
- Set `offer_nv12 = false` in `[pipewire]` ([Formats](#the-pipewire-output)), so that OBS cannot choose NV12.

Upstream:

- issue [obsproject/obs-studio#13823](https://github.com/obsproject/obs-studio/issues/13823), closed without a fix;
- pull request [obsproject/obs-studio#14002](https://github.com/obsproject/obs-studio/pull/14002), closed unmerged;
- the lines concerned, at tag 32.2.2: the `bpp` of the NV12 entry in [`formats.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/formats.c#L79), the line size in [`pipewire.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/pipewire.c#L657), and the plane pointers taken from the buffer in [`pipewire.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/pipewire.c#L689-L695).

#### Hiding the duplicate

Once WirePlumber has probed the loopback while the writer streams, PipeWire lists "Omarchy Broadcast" twice: the native node, and the loopback through v4l2. To hide the loopback from PipeWire only, create `~/.config/wireplumber/wireplumber.conf.d/50-omarchy-broadcast.conf`:

```
monitor.v4l2.rules = [
  { matches = [ { device.product.name = "Omarchy Broadcast" } ]
    actions = { update-props = { device.disabled = true } } }
]
```

Then restart WirePlumber:

```sh
systemctl --user restart wireplumber
```

Applications that open `/dev/video*` directly are unaffected. The package does not install this file. With it, the WirePlumber ordering drop-in is unnecessary for applications that use the native node. It stays useful for applications that reach the camera through v4l2 only. Remove the file and restart WirePlumber to undo it.

## Details

### Restarts and crashes

A clean restart (`systemctl --user restart omarchy-broadcastd`, or an upgrade) is invisible to applications that hold the virtual camera. The old writer sends placeholders until it closes, and the new one starts within about 50 ms.

If the daemon crashes, the unit restarts it after 250 ms (`RestartSec=250ms`). The camera picture returns once the new process has opened the camera and initialised CUDA, which takes seconds. Until then applications see the crash cover's image: black, or the away image when `[output] placeholder = "image"` and `[effect] fallback_image` are set ([ADR-0034](../adr/0034-away-image.md)).

Chromium-based applications (Chrome, Edge, Teams, Zoom in the browser, Discord, Slack and other Electron applications) could lose the camera in that time. To prevent it, the daemon prepares the loopback at every writer start. It uploads the cover's image (black, or the away image) and sets the driver's `timeout`, so the driver keeps serving that image after the daemon is gone ([loopback.md](loopback.md#what-the-daemon-sets-on-the-device), [ADR-0022](../adr/0022-crash-recovery-for-consumers.md)). The outcome is in the journal:

```text
crash cover in place, timeout 250 ms (consumers keep receiving its timeout image if the daemon stops writing)
crash cover shows black
crash cover shows the away image <path>
crash cover shows black in place of the away image: <reason>
crash cover not in place: <reason>; a consumer may lose the camera if the daemon crashes
```

The last line is only a warning. The daemon runs; an older v4l2loopback without the controls is one cause. The `crash cover shows` lines are logged once a cover is in place, and again whenever what it shows changes. The line "crash cover shows black in place of the away image" is a warning that the away image cannot be shown, with its reason; black is shown instead ([ADR-0034](../adr/0034-away-image.md)).

A crash loop is stopped. After 5 starts within 60 s (`StartLimitBurst=5`, `StartLimitIntervalSec=60`) systemd stops restarting the unit and it is `failed`. Manual starts and restarts count too. Bring it back with:

```sh
systemctl --user reset-failed omarchy-broadcastd
systemctl --user start omarchy-broadcastd
```

**PipeWire consumers do not reconnect after a crash.** The PipeWire node lives in the daemon process. When the daemon dies, WirePlumber destroys the consumer's stream, and the consumer does not come back by itself. An application that has to survive a crash should use the loopback device.

### Where your backgrounds are

The images you add as backgrounds, and your away images, are kept in a folder called `omarchy-broadcast` in your Pictures folder (`$(xdg-user-dir PICTURES)`, else `~/Pictures`). The away images are in its `away` folder. The videos, and the WebM made from an animated image, are kept in a folder of the same name in your Videos folder (`$(xdg-user-dir VIDEOS)`, else `~/Videos`).

A file you copy there yourself is listed the next time the gallery is shown. The folders have nothing to do with the desktop's wallpapers. A gallery that an earlier version kept in `~/.local/share/omarchy-broadcast/media` is moved there once, when the daemon starts ([../adr/0032-background-gallery.md](../adr/0032-background-gallery.md)). The posters of videos are in the cache, `~/.cache/omarchy-broadcast/`, and you can delete them at any time.

### Playing a video without an NVIDIA GPU

A video background plays on every machine, but how depends on the GPU. With an NVIDIA GPU the daemon plays any accepted video as it is. Without one (`[video] memory = system`, or no NVIDIA GPU at all) the CPU plays only one kind of file.

**What plays on the CPU.** A **VP9** video, **profile 0** (8-bit 4:2:0), in a **WebM or Matroska** file, of **at most 1920x1080** and **at most 30 frames a second**. The two loops that come with the package are such files. So is every animated GIF, WebP or APNG you import, because the daemon converts those to a WebM of this kind at import.

**What happens otherwise.** An H.264, HEVC or AV1 video, or a VP9 one that is larger or faster, is shown as its **poster, a still picture** (the first frame), behind you. `omarchy-broadcast status` shows why in its warning:

```text
The video background is shown as a still: it can't play without an NVIDIA GPU; run omarchy-broadcast media convert-command '/home/user/Videos/clip.mp4' for the conversion command
```

The daemon does not convert the video itself. It tells you how, and you decide when.

**Convert it.** Run the command the warning names, with the file as its argument. It prints one line, an `ffmpeg` command, and runs nothing:

```sh
omarchy-broadcast media convert-command ~/Videos/clip.mp4
```

```text
ffmpeg -nostdin -n -protocol_whitelist file -i '/home/user/Videos/clip.mp4' -map 0:v:0 -an -sn -dn -map_metadata -1 -vf 'scale=w=max(2\,trunc(min(1\,min(1920/iw\,1080/ih))*iw/2)*2):h=max(2\,trunc(min(1\,min(1920/iw\,1080/ih))*ih/2)*2),setsar=1' -fpsmax 30 -pix_fmt yuv420p -c:v libvpx-vp9 -profile:v 0 -deadline realtime -cpu-used 8 -row-mt 1 -crf 32 -b:v 8M -g 60 -force_key_frames 'expr:gte(t,n_forced*2)' -f webm '/home/user/Videos/omarchy-broadcast/clip-cpu.webm'
```

Copy that line into a terminal and run it. It needs `ffmpeg` with `libvpx-vp9`. It makes a VP9 WebM that fits inside 1920x1080, at most 30 frames a second, without audio. The command refuses to overwrite a file (`-n`), and your original is not changed. To convert the same file twice, print the command again for a new output name.

**Where the result goes.** The output is written to `omarchy-broadcast` in your **Videos** folder, as `<name>-cpu.webm` (`clip-cpu.webm` above; `clip-cpu-2.webm` when that name is taken). It is listed with your other videos the next time the gallery is shown, and plays on the CPU when you choose it.

**In the panel.** The Backgrounds section of the bar's setup dialog (bar plugin) lists your videos. A video the CPU cannot play has a note under it, the same command, and a **Copy** button that puts the command on the clipboard. Paste it in a terminal. The panel needs a daemon of interface revision 19 or later.

A video chosen from outside the gallery, by a path in `config.toml`, has no row in the panel. But `omarchy-broadcast media convert-command PATH` works for any file ([../cli-json.md](../cli-json.md#media)). The rules, and why the daemon does not make the copy itself, are in [../adr/0035-video-backgrounds.md](../adr/0035-video-backgrounds.md).

### Configuration

The configuration is `~/.config/omarchy-broadcast/config.toml` (`$XDG_CONFIG_HOME/omarchy-broadcast/config.toml`). It is optional: a missing file means the defaults. [config.example.toml](../config.example.toml) lists every key with its default and a comment. The daemon never writes this file.

After it, the daemon reads the files of `~/.config/omarchy-broadcast/config.d/`: every file ending in `.toml`, in order of the file name. Each overrides the earlier ones key by key. Put your own overrides in a file that sorts late, such as `config.d/90-local.toml`.

One file there is the daemon's: `config.d/50-panel.toml`. It writes that file when a setting is changed through it (the bar's setup dialog, `omarchy-broadcast config set`, `modules enable|disable|profile|pin`, `order set`), and the file starts with a comment that says so. Do not edit it by hand. A key that a later file sets, such as `90-local.toml`, wins, and the daemon refuses to change that key until you remove it from that file.

To see where each setting comes from:

```sh
omarchy-broadcast config show     # every key with the file that sets it
omarchy-broadcast config files    # the files in load order
```

See also [config.example.toml](../config.example.toml) and [ADR-0026](../adr/0026-config-d-and-managed-config.md).

**Reload after editing**, without restarting:

```sh
systemctl --user reload omarchy-broadcastd     # sends SIGHUP
omarchy-broadcast status                       # shows Error: if a file was rejected
```

This `busctl` call does the same and returns the reason on failure:

```sh
busctl --user call br.com.dias.OmarchyBroadcast /br/com/dias/OmarchyBroadcast br.com.dias.OmarchyBroadcast1 ReloadConfig
```

An invalid file, in `config.toml` or in `config.d/`, is rejected as a whole. The previous configuration stays in force, and the `Error` property names the file, line and key. The files are read at start and on a reload, not when they change.

**State file.** Settings changed while the daemon runs are kept in a separate state file, `~/.local/state/omarchy-broadcast/state.toml` (`$XDG_STATE_HOME/omarchy-broadcast/state.toml`), so that `config.toml` keeps its comments and layout. The daemon writes it. It holds:

- `omarchy-broadcast on` / `off`;
- the camera selection;
- the video effect, with its blur strength and background image;
- the stages noise removal, enhancement and relighting, with its light, direction and brightness;
- the settings of any module.

If the daemon cannot read the state file, it starts `off` and says why in `Error`: privacy fails closed.

The choices of modules (which are enabled, the stage order, the backend profile and the pins) are configuration, kept in `config.d/50-panel.toml`. A state file written by the 0.24 daemon, which kept them in its `[modules]` table, is moved there when the daemon first loads. A daemon of 0.24 ignores `config.d`, so after a downgrade it sees only what `config.toml` says of the modules.

**Cache.** What the daemon has learnt about the GPU is kept in `~/.cache/omarchy-broadcast/compute-capability` (`$XDG_CACHE_HOME/omarchy-broadcast/compute-capability`). It is the CUDA compute capability, which picks the models made for it and lets the daemon tell which SDK features are installed without waking the GPU. It is a cache, not a setting: delete it and the daemon learns it again at its next session.

**Crash ledger.** The crash ledger of the modules ([../modules.md](../modules.md#the-crash-ledger)) is a small file, `modules-crash.json`, next to the state file. It records which module's code was running when the daemon last went down. It disables a module that did so twice in a row until you run `omarchy-broadcast modules enable ID`. Deleting the file forgives every module.

**Maxine SDK root.** You can set it in the configuration (`sdk_root` in `[module."br.com.dias.maxine-vfx"]`, or its alias `[effect] maxine_root`) or with `MAXINE_VFX_ROOT`. The Maxine module loads the SDK's libraries from it itself, and `LD_LIBRARY_PATH` is not needed ([maxine.md](maxine.md#4-the-sdk-root-and-the-environment)). The first effect or noise removal the daemon starts loads the SDK first. A different root then takes a daemon restart; a reload that changes the key says so in the log.

**Other paths.** Override the paths with `--config PATH` and `--state PATH` in a copy of the unit file. `config.d` is then the directory next to `PATH`.

### Logs

```sh
journalctl --user -u omarchy-broadcastd -f
```

Add `-v` to `ExecStart` for debug output (consumer counts, teardown times). Exit status 3 means another instance already owns the bus name. systemd does not restart the service on that status.

## Publishing to the AUR

This part is for maintainers. The AUR holds two recipes of the split package ([From the AUR](#from-the-aur)): the source one, pkgbase `omarchy-broadcast-git`, and the prebuilt one, pkgbase `omarchy-broadcast-bin` ([The prebuilt packages](#the-prebuilt-packages-omarchy-broadcast-bin)). This section covers the source one first.

The AUR copy of `packaging/arch/PKGBUILD` differs from it only in where it builds from. It clones `https://github.com/rsd/omarchy-broadcast.git` instead of the local checkout, and its header comment says so. `scripts/aur.sh` makes that copy:

- It replaces exactly the local-checkout header paragraph, the build hint and the `source=` line. It fails when one of them is not in the PKGBUILD exactly once. A change to one of those lines is made in the script too.
- It generates the `.SRCINFO` with `LC_ALL=C makepkg --printsrcinfo` on the copy, alone in an empty directory, in an environment holding only `PATH` and an empty `HOME`.
- It copies the install scriptlet, `packaging/arch/omarchy-broadcast.install`, unchanged. The scriptlet lies beside the copy when the `.SRCINFO` is generated, because makepkg checks that it exists.
- The AUR repository holds exactly `PKGBUILD`, `.SRCINFO`, `omarchy-broadcast.install` and `LICENSE` (0BSD, written by the script too). `--aur` and `--check` refuse any other file in it.

```sh
scripts/aur.sh                  # prints the AUR PKGBUILD
scripts/aur.sh --srcinfo        # prints its .SRCINFO
scripts/aur.sh --check DIR      # fails, with a diff, when DIR's files are stale or DIR holds another file
scripts/aur.sh --aur DIR        # writes PKGBUILD, .SRCINFO, omarchy-broadcast.install and LICENSE into DIR
```

`DIR` must be the top of a git clone of the AUR repository. The script commits nothing. The clone is kept in `aur/omarchy-broadcast-git`, which this repository ignores. To publish, after the release commit is pushed to GitHub:

```sh
git clone ssh://aur@aur.archlinux.org/omarchy-broadcast-git.git aur/omarchy-broadcast-git   # once
scripts/aur.sh --aur aur/omarchy-broadcast-git
cd aur/omarchy-broadcast-git
git diff                        # review
git add PKGBUILD .SRCINFO omarchy-broadcast.install LICENSE
git commit -m '<version>-1'
git push
```

The committed `pkgver=` is the release version. makepkg replaces it with the output of `pkgver()` (`<version>.r<commits>.g<hash>`), which sorts after it. A new AUR commit is needed only when the PKGBUILD changes (a dependency, a file installed, a new version in `pkgver=`), not for every commit of the repository.

After a push, the AUR's web interface shows the new version at once. Its RPC index, which AUR helpers such as `yay` query, can take a few minutes to follow, so `yay` may still offer the previous version for a while. This needs no action.

### The prebuilt packages (`omarchy-broadcast-bin`)

The `-bin` recipe installs the files of a GitHub release, so you make and publish the release first. Each package of the split package has one asset, `<name>-<version>-x86_64.tar.zst`. It is the `usr/` tree of the `-git` package as it is, with its licence directory named `<name>-bin`. The recipe pins every asset by the sha256 in the release's `SHA256SUMS`. It has no `build()` and no build dependencies.

`scripts/release-bin.sh [--nocheck] [--out DIR]` makes the assets:

- It releases the commit `HEAD` names when it starts, and the workspace version of that commit. It refuses when the `pkgver=` of that commit's `packaging/arch/PKGBUILD` is another. It warns that uncommitted changes are not built.
- It builds the five `-git` packages of that commit with `makepkg` in a scratch directory, without installing anything and without `-s`, so the build dependencies must be installed. `--nocheck` skips `check()`, the workspace test suite.
- It writes the assets, `SHA256SUMS` and, in `pkg/`, the `-git` packages they were made of into the directory `--out` names. That directory must not exist or must be empty. The script has a default and prints the path when it is done. It writes into a temporary directory beside it and moves it into place only when the release is complete, so a failure leaves nothing behind.

Before any asset is kept, the script runs a gate that keeps the NVIDIA SDKs and ONNX Runtime out of the release:

- Every file of an asset must be one that the package is expected to hold. There is no other shared library, no model or engine file and no `models/` directory.
- Every ELF file must need only the libraries on its own allowlist (the modules and the LADSPA plugin: only the C runtime) and define only its own dynamic symbols (a module: `obc_module_abi_version` and `obc_module_entry`).
- The libraries must hold none of the strings that only SDK code linked in statically carries.

A Maxine module that fails the gate is left out of the release, and the script says so. If the core or the ONNX module fails it, the script fails.

The script does not publish anything. It prints, and does not run, the commands for the rest: tag the commit and push the tag, then `gh release create` with the assets and `SHA256SUMS`, then the update of the AUR repository. Run them in this order. The assets must be on the GitHub release before the push to the AUR, because the `-bin` sources download from it:

1. Push the release commit to GitHub, then tag and push the tag.
2. Create the release `v<version>` on GitHub with the assets and `SHA256SUMS`.
3. Make the AUR recipe from the release directory, and review it:

   ```sh
   git clone ssh://aur@aur.archlinux.org/omarchy-broadcast-bin.git aur/omarchy-broadcast-bin   # once
   scripts/aur.sh --bin RELEASE_DIR --aur aur/omarchy-broadcast-bin
   scripts/aur.sh --bin RELEASE_DIR --check aur/omarchy-broadcast-bin
   cd aur/omarchy-broadcast-bin
   git diff                        # review
   git add PKGBUILD .SRCINFO omarchy-broadcast.install LICENSE
   git commit -m 'Update to <version>'
   git push origin HEAD:master
   ```

`scripts/aur.sh --bin RELEASE_DIR` has no package data of its own. The description, licences, dependencies, optional dependencies, provides and conflicts come from the `.PKGINFO` of each `-git` package in `RELEASE_DIR/pkg`. Before it writes anything, it checks that every asset matches `SHA256SUMS` and holds the `usr/` tree of its `-git` package unchanged, apart from the licence directory name.

The packages of the recipe are those of the assets `SHA256SUMS` lists, so a Maxine module left out of the release is left out of the recipe. `--srcinfo` prints the `.SRCINFO`, and `--check` and `--aur` behave as for the `-git` recipe. The AUR repository holds the same four files. A new AUR commit is needed for every release, since `pkgver=` is the release version.
