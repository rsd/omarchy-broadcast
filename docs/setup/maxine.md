# Using the NVIDIA Maxine Video Effects SDK with omarchy-broadcast

The video effects `blur` and `replace`, and video noise removal, run on the NVIDIA Maxine Video Effects SDK. omarchy-broadcast does not contain, build or download the SDK. The SDK is a personal download from NVIDIA, and the sibling project omarchy-nvidia-sdks (the omarchy-nvidia-sdks repository) packages it for Arch as `maxine-vfx-sdk-local`. This page covers the omarchy-broadcast side: which parts of the SDK are needed, how to enable the daemon's drop-in, what the daemon reads from its environment and how to fix the failures that are specific to omarchy-broadcast.

Without the SDK the daemon works as usual: the `none` effect and the passthrough need nothing from it, and an effect or noise removal that cannot load reports the reason in the daemon's `Error` property.

Background: [../research/maxine-linux.md](../research/maxine-linux.md) (access, licence, API). The SDK is never vendored in this repository, see [ADR-0003](../adr/0003-maxine-first-behind-backend-seam.md). The decision to leave the SDK tooling to omarchy-nvidia-sdks is in [ADR-0021](../adr/0021-nvidia-sdk-handover.md).

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

`replace` needs `nvvfxgreenscreen` only for its mask; the replacement is a composite done by the project. Noise removal is optional and the effects work without it.

## 2. Build and install maxine-vfx-sdk-local

The omarchy-nvidia-sdks repository owns everything about the SDK itself: the NVIDIA account and NGC access, the download, the supported GPU architectures, and the Arch package `maxine-vfx-sdk-local`. Follow its README and its `docs/download.md` for the download and `docs/gpu-support.md` for the architectures. The package installs the SDK under `/opt/nvidia/maxine/videofx` and an environment file, `/usr/share/maxine-vfx/maxine-vfx.env`.

Build the package with the three features omarchy-broadcast needs. From a checkout of the omarchy-nvidia-sdks repository, with the download in `~/Downloads/maxine` and `sm_120` standing for your architecture:

```bash
maxine-vfx/configure.sh --src ~/Downloads/maxine --archs sm_120 \
    --features nvvfxgreenscreen,nvvfxbackgroundblur,nvvfxdenoising --write
(cd maxine-vfx && makepkg -f)
```

Then install the package yourself:

```bash
cd maxine-vfx && sudo pacman -U maxine-vfx-sdk-local-*.pkg.tar*
```

Nothing in omarchy-broadcast installs the package. The omarchy-broadcast Arch package lists it as an optional dependency (`optdepends`), because the SDK is a personal download and cannot be a hard requirement.

Licence: the SDK is under NVIDIA's licence and is for your own machine only. omarchy-broadcast never distributes it, and you must not put the SDK, the models or the package in a repository, a package or a release artifact. The licence notes are in the "Licence and personal use" section of the omarchy-nvidia-sdks README.

## 3. Enable the drop-in

The daemon uses the SDK only when its systemd unit has the SDK's environment. That is a user drop-in with two active lines:

```ini
[Service]
EnvironmentFile=/usr/share/maxine-vfx/maxine-vfx.env
Environment=LD_PRELOAD=/usr/lib/maxine-vfx/os-release-shim.so
```

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

## 4. Environment

What the daemon reads, and where each value comes from with the drop-in of section 3:

| Variable | Used for | Comes from |
|---|---|---|
| `MAXINE_VFX_ROOT` | The SDK root: the directory that contains `lib/`, `features/` and `external/`. The daemon loads `libVideoFX.so` from its `lib/` and passes the `models` directory next to the loaded `libVideoFX.so` (`<root>/lib/models`) to the SDK as `NVVFX_MODEL_DIRECTORY` | The environment file. When unset or empty, the daemon uses `/opt/nvidia/maxine/videofx` if that directory exists, and otherwise searches the library path only |
| `LD_LIBRARY_PATH` | Lets the dynamic loader find the SDK, the bundled CUDA 12 and TensorRT 10 libraries, and the feature libraries (loaded by a plain `dlopen("libnvVFXGreenScreen.so")`) | The environment file: `lib`, one `features/<feature>/lib` entry for every packaged feature, `external/cuda/lib` and `external/tensorrt/lib` |
| `LD_PRELOAD` | The os-release shim | The drop-in |
| `MAXINE_OS_RELEASE` | The replacement `os-release` file the shim opens: an absolute path | The environment file |
| `MAXINE_OS_RELEASE_DEBUG` | `1` makes the shim log each redirect | Set it by hand, for diagnosis |

Because the environment file is generated for the packaged features, a feature added later, as noise removal was, needs only a rebuild and a reinstall of `maxine-vfx-sdk-local`. The drop-in does not change.

The bundled CUDA 12 and TensorRT 10 libraries are on the library path of the daemon only. Never set them in a login profile or in `~/.bashrc`: they would shadow Arch's CUDA 13 for every program you start. For the same reason the SDK package installs no `ld.so.conf.d` or `profile.d` entry.

Development knob: `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process overrides the noise-removal strength, for comparisons. It is not a supported configuration (see section 6).

## 5. An SDK tree without the package

An SDK tree that was installed some other way, for example under `~/.local/opt/maxine/VideoFX`, works too. The shipped drop-in has a commented variant for it. Replace the two active lines with the variant, keep the paths that match your tree, and run `daemon-reload` and a restart:

```ini
[Service]
Environment=MAXINE_VFX_ROOT=%h/.local/opt/maxine/VideoFX
Environment=LD_LIBRARY_PATH=%h/.local/opt/maxine/VideoFX/lib:%h/.local/opt/maxine/VideoFX/features/nvvfxgreenscreen/lib:%h/.local/opt/maxine/VideoFX/features/nvvfxbackgroundblur/lib:%h/.local/opt/maxine/VideoFX/features/nvvfxdenoising/lib:%h/.local/opt/maxine/VideoFX/external/cuda/lib:%h/.local/opt/maxine/VideoFX/external/tensorrt/lib
Environment=LD_PRELOAD=%h/.local/lib/maxine-vfx/os-release-shim.so
Environment=MAXINE_OS_RELEASE=%h/.local/lib/maxine-vfx/fake-os-release
```

Notes:

- `LD_LIBRARY_PATH` needs one `features/<feature>/lib` entry for every feature that is installed in the tree. systemd does not expand globs in `Environment=`, so a feature added later needs a new entry.
- The shim library and the replacement `os-release` come from the omarchy-nvidia-sdks repository, which owns both; omarchy-broadcast does not contain them. In a checkout of that repository, `make -C tools/os-release-shim` writes `target/os-release-shim/os-release-shim.so`, and the replacement `os-release` is that repository's `tools/maxine-probe/fake-os-release`. Copy both to `~/.local/lib/maxine-vfx/`, where the two lines above point, or change the paths. The "The compatibility shim" and "Running a program" sections of its `docs/usage.md` describe them and the unpackaged case.

## 6. Using the effects

With the SDK installed and the daemon running with the drop-in (section 3), the effects are selected in two ways:

- In `config.toml`, the `[effect]` section (`mode`, `blur_strength`, `background_image`, `on_failure`); see [../config.example.toml](../config.example.toml).
- At run time, with `omarchy-broadcast effect none|blur|replace [--strength X] [--image PATH]`, which the daemon remembers across restarts; see [../cli-json.md](../cli-json.md) and [../dbus-api.md](../dbus-api.md).

Video noise removal is a separate switch that combines with any effect: `[effect] denoise = true` in `config.toml`, or `omarchy-broadcast denoise on|off` at run time. It removes the grain a webcam produces, which is mostly visible in dim rooms, and it runs before the blur or replacement, or alone with the effect `none`. It needs the `nvvfxdenoising` feature and its models in the package (section 2). Things to know:

- **It fails open.** Noise removal hides nothing, so if it cannot load or run (the feature or SDK is missing, or the frame size is out of range) the stream continues without it, and the daemon's `Error` property says `noise removal unavailable: <reason>`. `[effect] on_failure` does not apply to it and the daemon does not enter `error`. The effects themselves still fail closed, as below.
- **Frame size.** The feature supports frames from 80x80 up to 1920x1080 (the SDK's range). An output contract above 1080p runs without noise removal and reports the reason.
- **Strength.** The SDK's strength is binary: 0 favours texture, 1 favours noise removal. The daemon uses 1 and has no setting for it. For comparisons during development, `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process overrides it. It is a development knob only and is not a supported configuration.
- **Cost.** It adds one GPU pass per frame and its model's GPU memory; with it off, nothing is added, no pass, copy or allocation. The models load only while it is on. Measure on your machine with `obc-bench --denoise` ([../testing.md](../testing.md)).
- The design is in [ADR-0018](../adr/0018-video-noise-removal.md).

The background image for `replace` must be a PNG or JPEG file. Until an effect is ready, the virtual camera shows black rather than the unprocessed camera image; if the SDK cannot be loaded, the daemon enters the `error` state (default) instead of showing the unprocessed image. The design is in [ADR-0012](../adr/0012-video-effects-and-maxine-backend.md). Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce locally with `obc-bench` (see [../testing.md](../testing.md)).

To check the SDK installation itself, use the check commands in the "Check the installation" section of `docs/usage.md` in the omarchy-nvidia-sdks repository, and its own tests. omarchy-broadcast has no probe of its own.

## 7. Troubleshooting

Problems with the SDK itself (download, package, architecture) are in omarchy-nvidia-sdks. The SDK status codes `-1209` ("no kernel image is available"), `-2` (unimplemented, a feature library not found) and `-13` ("file could not be found", the model directory) are explained in the Troubleshooting section of its `docs/usage.md`. The rows below are the ones that are specific to omarchy-broadcast.

| Symptom | Cause | Fix |
|---|---|---|
| The daemon's `Error` property reads `effect blur unavailable: NvVFX_Load(GreenScreen) returned -1209 (...)`, naming the os-release override (this page, section 3), and the state is `error` | The daemon runs without the shim: the drop-in is not installed, `daemon-reload` and a restart were skipped, or its paths do not match your installation. With the default `on_failure = "black"` the virtual camera stays black | Install the drop-in (section 3) and check `systemctl --user show omarchy-broadcastd -p Environment`; set `MAXINE_OS_RELEASE_DEBUG=1` to see the redirect. A `cannot be preloaded` line in `journalctl --user -u omarchy-broadcastd` means the library path in `LD_PRELOAD` is wrong or the package is not installed |
| The unit fails to start after the drop-in was installed, and `journalctl --user -u omarchy-broadcastd` shows `Failed to load environment files: No such file or directory` (systemd does not name the file) | `EnvironmentFile=` has no `-`, so a missing file is an error. `maxine-vfx-sdk-local` is not installed | Check that `/usr/share/maxine-vfx/maxine-vfx.env` exists, which it does when the package is installed. Install the package (section 2), or remove the drop-in (section 8) |
| The drop-in has no effect: the daemon's environment lacks `LD_PRELOAD` | The file is not at `~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf` (the directory name must match the unit), or `daemon-reload` was skipped | Check the path, run `systemctl --user daemon-reload`, restart, and check with `systemctl --user cat omarchy-broadcastd` |
| An effect reports that a feature library is not found (`-2`) or the package lacks a feature | The package was built without that feature | Rebuild `maxine-vfx-sdk-local` with all three features of section 1 (section 2), reinstall it and restart the daemon. The drop-in needs no edit |
| The daemon's `Error` says `noise removal unavailable: ... -2` (NVCV status), and the camera image is shown without noise removal | The package does not contain the `nvvfxdenoising` feature, or the daemon runs without the environment file | Check that `MAXINE_VFX_FEATURES` in `/usr/share/maxine-vfx/maxine-vfx.env` lists `nvvfxdenoising`; rebuild the package with it if not (section 2); check the drop-in |
| The same, with `-1209` or `-13` | The shim is not applied to this process (the feature's library needs it too), or the `Denoise-*` models for your architecture are not in the package | Section 3; `ls /opt/nvidia/maxine/videofx/lib/models` must show `Denoise-agg-<SM>.engine.trtpkg` and `Denoise-con-<SM>.engine.trtpkg`; rebuild the package for your architecture otherwise |
| The same, with a message that the frame size is out of range | The output contract is smaller than 80x80 or larger than 1920x1080 | Use an output mode within the range (`[output]` in `config.toml`); noise removal does not run outside it |
| The daemon's `Error` property or the element's status names the mask-hardening kernel and a CUDA error `222` ("the NVIDIA driver is older than this build supports") | The driver rejects the PTX of the mask-hardening kernel: it is older than 570.26 | Update the NVIDIA driver to 570.26 or newer (`nvidia-smi --query-gpu=driver_version --format=csv,noheader`). With the default `on_failure = "black"` the virtual camera stays black until then |
| The same, with CUDA error `221` | The driver cannot JIT-compile the kernel: `libnvidia-ptxjitcompiler.so.1` is missing from the driver installation | Reinstall the driver's userspace package (`nvidia-utils`) so that it provides `libnvidia-ptxjitcompiler.so.1`; check with `ldconfig -p \| grep ptxjitcompiler` |

## 8. Uninstall

```bash
# remove the drop-in, then reload and restart
rm -f ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf
systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd.service
# the SDK package, only if nothing else uses it
sudo pacman -R maxine-vfx-sdk-local
```

Without the drop-in the daemon runs as usual and the effects are unavailable.

An os-release shim installed by `scripts/install-user.sh` of an older omarchy-broadcast release, under `~/.local/lib/omarchy-broadcast/`, is removed by `scripts/install-user.sh --uninstall` of the current script, and by reinstalling with it. To remove it by hand: `rm -f ~/.local/lib/omarchy-broadcast/os-release-shim.so ~/.local/lib/omarchy-broadcast/fake-os-release`.

The NGC CLI, the NGC key and the downloads belong to omarchy-nvidia-sdks; its documentation covers removing them.

## 9. Licence and support

This is a summary, not legal advice. Read the licence you accept.

- The SDK and its models are NVIDIA's, under NVIDIA's licence. You download them with your own NVIDIA account and accept that licence yourself.
- Neither omarchy-broadcast nor omarchy-nvidia-sdks redistributes NVIDIA files. Do not commit the SDK, the models, an NGC key or `~/.ngc/` to a repository, and do not put them in a package or release artifact.
- Benchmark results for the SDK are not published (SLA 8.9).
- Running the SDK on a distribution it does not list, through the compatibility shim, is not supported by NVIDIA.

The README of omarchy-nvidia-sdks has that repository's own licence notes.
