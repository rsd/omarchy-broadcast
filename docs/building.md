# Build and install from source

Use this page to install omarchy-broadcast from a checkout of the repository, without the AUR packages: on a Linux distribution other than Arch Linux and Omarchy, or when you want an install in your home directory. On Arch Linux and Omarchy the AUR packages are the easier route: [INSTALL.md](../INSTALL.md).

This route has not been tested outside Arch Linux and Omarchy. The daemon probably works on any modern Linux distribution. You supply the libraries from your package manager, and you obtain the other pieces yourself. The open-model effects need ONNX Runtime 1.29 or newer and the model files (the [omarchy-onnxruntime](https://github.com/rsd/omarchy-onnxruntime) and [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models) repositories package them for Arch and describe the other routes). The NVIDIA Maxine effects need the NVIDIA SDKs from NVIDIA ([omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) packages them for Arch only). The bar plugin is for Omarchy only; elsewhere the CLI is the control.

## Before you start

Your package manager supplies these. Package names differ between distributions, so this page names none beyond Arch's. `packaging/arch/PKGBUILD` lists the libraries the Arch package depends on.

- The Rust toolchain, to build the daemon and the modules.
- GStreamer 1.x with the base, good and bad plugin sets (`videoconvert`, `jpegdec`), and the libraries the daemon links: GLib, ALSA (`libasound`, for the microphone's hardware gain) and, for the PipeWire camera output, libpipewire.
- A v4l2loopback kernel module. Version 0.15.4 has the controls that the daemon's crash cover uses. With an older module the daemon runs and logs a warning ([setup/loopback.md](setup/loopback.md#what-the-daemon-sets-on-the-device)).
- A systemd user session with a D-Bus session bus. The daemon is a systemd user service and owns a name on the session bus.
- PipeWire with WirePlumber 0.5, for the virtual microphone and the optional PipeWire camera node. The loopback works without them.
- With an NVIDIA GPU, the driver, the CUDA toolkit (for NVRTC) and GStreamer's `nvcodec` plugin (`cudaupload`, `cudaconvert`, `cudadownload`). Without one, the system-memory pipeline needs none of them ([Without CUDA or without an NVIDIA GPU](setup/daemon.md#without-cuda-or-without-an-nvidia-gpu)).
- For the open-model effects, ONNX Runtime 1.29 or newer: the module opens `libonnxruntime.so` when the daemon runs. It looks at `$ORT_DYLIB_PATH`, then at `ort_library` in the module's table of `config.toml`, then in the system's library path ([setup/onnx.md](setup/onnx.md#where-the-module-looks-for-the-library)). It also needs the four model files ([setup/onnx.md](setup/onnx.md#from-a-checkout)).
- Optionally, `ffmpeg`, for video backgrounds ([setup/daemon.md](setup/daemon.md#playing-a-video-without-an-nvidia-gpu)).

## Per-user install

1. From the repository root, run the script. It builds the release binaries and the modules, installs them under `~/.local`, needs no root and refuses to run as root:

   ```sh
   scripts/install-user.sh
   ```

   The script builds without the PipeWire camera output. To get it, use the [manual install](#manual-install) with the cargo feature `pipewire` ([the PipeWire output](setup/daemon.md#the-pipewire-output)). Expect the script to print the next steps when it ends.

2. Make sure `${XDG_BIN_HOME:-~/.local/bin}` is on your `PATH`. The script installs `omarchy-broadcast` and `omarchy-broadcastd` there.

3. Create the virtual camera device. The checkout holds the `modprobe.d` line that the Arch package ships as an example. Install it, then run the other two commands of [INSTALL.md](../INSTALL.md#steps), step 4, which load the module at boot and reload it:

   ```sh
   sudo install -Dm644 dist/modprobe.d/omarchy-broadcast.conf /etc/modprobe.d/omarchy-broadcast.conf
   ```

4. Load the unit:

   ```sh
   systemctl --user daemon-reload
   ```

   Then start the daemon at every login and [check that it works](../INSTALL.md#check-that-it-works), as in INSTALL.md, step 5.

5. Add the optional parts. The open-model effects need ONNX Runtime and the model files ([setup/onnx.md](setup/onnx.md#from-a-checkout)). The virtual microphone needs the script's `--mic` or `--audio` option ([setup/microphone.md](setup/microphone.md)). The NVIDIA Maxine effects need an SDK you obtain yourself ([setup/maxine.md](setup/maxine.md)).

The script builds the release binaries and the NVIDIA Maxine module and installs:

- `omarchy-broadcastd` and `omarchy-broadcast` in `${XDG_BIN_HOME:-~/.local/bin}`;
- the unit in `${XDG_DATA_HOME:-~/.local/share}/systemd/user/`, with `ExecStart=` pointing at the installed daemon;
- the D-Bus activation file in `${XDG_DATA_HOME:-~/.local/share}/dbus-1/services/`;
- the modules, each as a library in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules/` and a manifest in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/`, whose `library` line names the installed library:
  - the NVIDIA Maxine video module (`libobc_module_maxine.so`, `maxine-vfx.toml`);
  - the NVIDIA Maxine AR module (`libobc_module_maxine_ar.so`, `maxine-ar.toml`);
  - the open-model cut-out module (`libobc_module_onnx.so`, `onnx.toml`);
- a manifest, `install-user.manifest`, in `${XDG_STATE_HOME:-~/.local/state}/omarchy-broadcast/`. It lists every path written (the modules' libraries and manifests included) and the sha256 of each binary and of each module library.

The Maxine modules are built and installed whether or not the Maxine SDK is present. Without the SDK the daemon lists the module greyed out, with the reason. The open-model cut-out module is installed without ONNX Runtime and without models: the daemon lists it as "model not installed" until the four model files are in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/models/onnx`. Set `OBC_ONNX_MODELS_DIR` to a directory that holds them, and the script installs them once their sha256 match the module's catalogue ([crates/obc-module-onnx/README.md](../crates/obc-module-onnx/README.md)). Or install the model package `omarchy-onnx-model-mediapipe-landscape`.

The script does not install the SDKs, the os-release shim or any drop-in; [setup/maxine.md](setup/maxine.md) describes them ("The Maxine module" and section 3 say how the daemon applies the SDK's environment). An older version of the script installed a shim in `~/.local/lib/omarchy-broadcast/`. A reinstall or `--uninstall` removes it.

**Behaviour.**

- It marks every text file it writes. It overwrites or removes a file only if it carries that mark. For a binary, a module library included, it does so only if the manifest lists it with the checksum on disk. It stops before changing anything if a target path holds another file. Running it again updates its own install.
- A Maxine manifest at `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/maxine-vfx.toml` that it did not write has no mark, so the script refuses it. An example is the hand-made copy of the manual recipe in [setup/maxine.md](setup/maxine.md). Remove that file and the library its `library` line names, then run the script again. This applies to `--uninstall` as well.
- For an install, the data directory must be an absolute path made only of letters, digits and `._/@+-`. Its path is written into the Maxine manifest and, with `--mic`, into the microphone's unit. Otherwise the script exits with status 2.
- It runs no `systemctl` command and enables nothing. It prints the next steps.
- A running daemon never reloads a module library. After a run that rebuilt a module, restart the daemon:

  ```sh
  systemctl --user restart omarchy-broadcastd
  ```

- It does not install the udev rule for the MX Brio, which needs root ([the udev rule](setup/daemon.md#the-udev-rule-for-the-mx-brio)), nor the WirePlumber drop-in.
- It warns when a unit in `~/.config/systemd/user` exists, because that location takes precedence ([Which unit is used](setup/daemon.md#which-unit-is-used)).

**Options.**

| Option | Effect |
|---|---|
| `--mic` | Also installs the optional noise-removed microphone ([setup/microphone.md](setup/microphone.md)) |
| `--audio` | Also installs the microphone with audio modules, the alternative to `--mic`; the script refuses both in one run ([setup/microphone.md](setup/microphone.md)) |
| `--dry-run` | Prints every action and changes nothing |
| `--no-build` | Installs the binaries and the module libraries already in `${CARGO_TARGET_DIR:-target}/release` |
| `--uninstall` | Removes what the script installed, the modules' libraries and manifests included |
| `--help` | Shows the usage |

**Exit status.** 0 done. 1 refused (a target path holds a file the script did not install, or the unit is still enabled on `--uninstall`) or the build failed. 2 usage error or unusable environment.

## Manual install

The two binaries and the unit file, with no script. None of the routes on this page installs the Maxine SDK. The routes that build a package are in [development.md](development.md#arch-package-from-a-checkout).

1. Build, from the repository root:

   ```sh
   cargo build --release
   ```

   The PipeWire output is a cargo feature that is off by default. It needs the libpipewire headers and clang at build time. To get it, build and install with the feature, as [the PipeWire output](setup/daemon.md#the-pipewire-output) describes.

2. Install the daemon and the CLI into `~/.cargo/bin`:

   ```sh
   cargo install --path crates/omarchy-broadcastd
   cargo install --path crates/omarchy-broadcast
   ```

3. Install the unit file with `ExecStart=` rewritten. The file in `dist/` names `/usr/bin/omarchy-broadcastd`, the path of the package. Copied unchanged after `cargo install`, the service fails to start (status 203/EXEC).

   ```sh
   mkdir -p ~/.config/systemd/user
   sed 's|^ExecStart=/usr/bin/|ExecStart=%h/.cargo/bin/|' \
       dist/systemd/omarchy-broadcastd.service \
       > ~/.config/systemd/user/omarchy-broadcastd.service
   ```

4. Optional, D-Bus activation: install the activation file. A command such as `omarchy-broadcast on` then starts the service if it is not running ([Start the daemon](setup/daemon.md#start-the-daemon)).

   ```sh
   install -Dm644 dist/dbus-1/services/br.com.dias.OmarchyBroadcast.service \
       ~/.local/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service
   ```

5. Optional, discovery through PipeWire: install a drop-in that orders WirePlumber after the daemon ([Discovery by PipeWire](setup/daemon.md#discovery-by-pipewire) has the command).

6. Create the virtual camera device as in [Per-user install](#per-user-install), step 3. Then load the units:

   ```sh
   systemctl --user daemon-reload
   ```

   Start the daemon as in [INSTALL.md](../INSTALL.md#steps), step 5. If you enabled the service earlier from an older copy of the unit, run this so that `default.target` wants it:

   ```sh
   systemctl --user reenable omarchy-broadcastd
   ```

## Uninstall a build from source

1. Stop and disable the service, as in [INSTALL.md](../INSTALL.md#uninstall), step 1.

2. Remove the files, depending on how you installed:

   - **Per-user install:** run the script from the checkout. It removes only the files it installed, the modules' libraries and manifests among them. It refuses while the unit is still enabled, so do step 1 first. A hand-made Maxine manifest stops it too; remove the files named in [setup/maxine.md](setup/maxine.md#8-uninstall) first.

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

4. Also remove the udev rule if you copied it:

   ```sh
   sudo rm /etc/udev/rules.d/70-omarchy-broadcast-mx-brio.rules
   sudo udevadm control --reload
   ```
