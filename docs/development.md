# Development and packaging

This page is for people who work on omarchy-broadcast's code or packages: how to check the repository, build the Arch package from a checkout, run the benchmark binary, what each package holds and how the AUR packages are published. To install omarchy-broadcast, read [INSTALL.md](../INSTALL.md). To build and install it from a checkout without the AUR, read [building.md](building.md).

## Check the repository

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

Besides the Rust toolchain and the packages listed in [testing.md](testing.md), it needs libpipewire, clang and pipewire for the PipeWire build, and either `shellcheck` or `uvx`, which fetches it. Tests, including the manual check with OBS or Chrome, are in [testing.md](testing.md).

## Run the benchmark binary

`obc-bench` streams from the camera to the "Omarchy Broadcast" loopback device until Ctrl-C:

```sh
cargo run --release -p obc-pipeline --bin obc-bench -- --duration 0
```

Then select "Omarchy Broadcast" as the camera in OBS or Chrome. Optional arguments:

- `--camera <name|vid:pid|/dev/videoN>` chooses the camera. The default is automatic. A name or ID matching two colour cameras is an error, and greyscale IR nodes are never picked.
- `--mode WxH@FPS` or `FORMAT:WxH@FPS` chooses the capture mode, for example `--mode 1920x1080@30` or `--mode NV12:1920x1080@60`. The default is `1920x1080@60`, any format. The closest mode the camera offers is used.

## Arch package from a checkout

`packaging/arch/PKGBUILD` builds one split package of five from the committed HEAD of your local checkout. Uncommitted changes are not included. The five are the core `omarchy-broadcast-git` and a package per loadable module, with the base names of the [table below](#the-packages) and the `-git` suffix.

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

2. Create the virtual camera device and start the daemon as [INSTALL.md](../INSTALL.md) describes, from its step "Create the virtual camera" on. The package does not do either for you, and enables nothing. Pacman prints the next steps from the install scriptlet `packaging/arch/omarchy-broadcast.install`, which changes nothing.

What the core package installs and needs is under [What the core package installs](#what-the-core-package-installs).

The AUR recipes of the same split package are built from this repository on GitHub. `scripts/aur.sh` generates the source recipe's PKGBUILD and `.SRCINFO` from `packaging/arch/PKGBUILD`, and `scripts/release-bin.sh` with `scripts/aur.sh --bin` makes the prebuilt one ([Publishing to the AUR](#publishing-to-the-aur)).

## The packages

The AUR has the split package of `packaging/arch/PKGBUILD` in two forms. There is no package without a suffix. Install one form. A `-bin` package and its `-git` twin conflict with each other, and the modules of one form work with the core of the other.

| Form | Package names | Use it when |
|---|---|---|
| Prebuilt, `-bin` | `omarchy-broadcast-bin`, `omarchy-broadcast-onnx-bin`, `omarchy-broadcast-maxine-vfx-bin`, `omarchy-broadcast-maxine-ar-bin`, `omarchy-broadcast-maxine-afx-bin` | You want a quick install with no build. The files come from the GitHub release of the same version and are checked against a sha256 in the recipe |
| Source, `-git` | The same names with `-git` in place of `-bin` | You want the latest source. It builds from this repository on GitHub and needs the build tools of `packaging/arch/PKGBUILD` |

Each `-bin` package installs exactly the files of its `-git` twin. It provides the plain name (`omarchy-broadcast`, `omarchy-broadcast-onnx`, and so on) and conflicts with the plain name and with the twin.

The core installs the daemon, the CLI, the units, the CPU voice module and the microphone's parts. Each module is a package of its own, so you install only what you use. The names below take `-bin` or `-git`, the same kind as the core.

| Package | What it adds | Depends on |
|---|---|---|
| `omarchy-broadcast` | The core: the daemon and the CLI, the audio worker, the CPU voice module, the microphone's host and plug-in, the units, the D-Bus files, the udev rule, the media and the documentation. The CPU video path is part of the daemon | optional: `nvidia-utils` and `cuda`, for the CUDA video path on NVIDIA GPUs |
| `omarchy-broadcast-onnx` | The open-model cut-out module (blur and replace, no NVIDIA SDK). Models are separate packages | `onnxruntime>=1.29`, `omarchy-onnx-model-mediapipe-landscape`; optional: the other `omarchy-onnx-model-*` packages and an `onnxruntime-provider-cuda`, `-tensorrt` or `-openvino` engine |
| `omarchy-broadcast-maxine-vfx` | The Maxine video effects module | `cuda`, `maxine-vfx-sdk-local` |
| `omarchy-broadcast-maxine-ar` | The Maxine AR module (eye contact, auto-framing) | `cuda`, `maxine-ar-sdk-local` |
| `omarchy-broadcast-maxine-afx` | The Maxine audio effects module for the microphone | `maxine-afx-sdk-local` |

- Each module package depends on the module ABI range it was built for (`omarchy-broadcast-module-abi>=1.6` and `<2`), which the core provides. The daemon refuses a module built for a newer ABI minor than its own.
- The NVIDIA SDK is never packaged here. `maxine-vfx-sdk-local`, `maxine-ar-sdk-local` and `maxine-afx-sdk-local` are built from your own download with the [omarchy-nvidia-sdks](https://github.com/rsd/omarchy-nvidia-sdks) repository ([setup/maxine.md](setup/maxine.md)). The daemon and the audio worker find the SDK's environment and apply it to themselves, so no systemd drop-in and no `LD_LIBRARY_PATH` are needed.
- The ONNX models are packages of their own from [omarchy-onnx-models](https://github.com/rsd/omarchy-onnx-models): `omarchy-onnx-model-<id>`, with `<id>` one of `mediapipe-landscape` (the default), `pphumanseg-v2-mobile`, `pphumanseg-v2-lite` or `modnet`. They install in `/usr/share/omarchy-onnx-models` with each model's own licence ([setup/onnx.md](setup/onnx.md)).
- The prebuilt packages do not contain the NVIDIA SDKs or ONNX Runtime, which are licensed separately. The Maxine and ONNX modules hold no code of either: they load the SDK or the runtime when the daemon runs. So the `-bin` modules need the SDK packages and an `onnxruntime` package exactly as the `-git` ones do.

## What the core package installs

The core package, `omarchy-broadcast-git` (or `-bin`), installs:

- `/usr/bin/omarchy-broadcastd` and `/usr/bin/omarchy-broadcast`;
- the microphone's parts (the audio worker, the mic host's unit and configuration, the forwarding plugin and the CPU voice module), all inactive until `omarchy-broadcast mic on`;
- the unit `/usr/lib/systemd/user/omarchy-broadcastd.service`;
- the D-Bus activation file `/usr/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service`;
- the directories modules are found in, `/usr/lib/omarchy-broadcast/modules` (libraries) and `/usr/lib/omarchy-broadcast/modules.d` (manifests), with the CPU voice module in them;
- the C header of the module interface, `/usr/include/omarchy-broadcast/omarchy_broadcast_module.h`;
- the udev rule for the MX Brio, `/usr/lib/udev/rules.d/70-omarchy-broadcast-mx-brio.rules`, which lets the daemon read the camera's Show Mode ([the udev rule](setup/daemon.md#the-udev-rule-for-the-mx-brio));
- the licences and the documentation, in `/usr/share/doc/omarchy-broadcast/`;
- the optional WirePlumber drop-in, as an inactive example in `/usr/share/doc/omarchy-broadcast/examples/systemd/wireplumber.service.d/omarchy-broadcast.conf` ([Discovery by PipeWire](setup/daemon.md#discovery-by-pipewire));
- the loopback's `modprobe.d` line of [setup/loopback.md](setup/loopback.md), as an inactive example, `/usr/share/doc/omarchy-broadcast/examples/modprobe.d/omarchy-broadcast.conf`, to copy into `/etc/modprobe.d`. It is not installed in `/usr/lib/modprobe.d`: only one `options v4l2loopback` line takes effect, and it would collide with one of your own.

The other modules are packages of their own, built from the same `PKGBUILD` ([setup/maxine.md](setup/maxine.md#the-maxine-module), [setup/onnx.md](setup/onnx.md), [modules.md](modules.md)). Each depends on the module ABI that the core provides (`omarchy-broadcast-module-abi`, a range: [modules.md](modules.md#packaging-a-module)). The Maxine SDKs need no drop-in: the daemon applies the SDK package's environment to itself at start ([setup/maxine.md](setup/maxine.md#3-the-sdk-environment)).

## Publishing to the AUR

The AUR holds two recipes of the split package ([The packages](#the-packages)): the source one, pkgbase `omarchy-broadcast-git`, and the prebuilt one, pkgbase `omarchy-broadcast-bin` ([The prebuilt packages](#the-prebuilt-packages-omarchy-broadcast-bin)). This section covers the source one first.

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
