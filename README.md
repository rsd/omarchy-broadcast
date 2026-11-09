# oma-broadcast

A GPU webcam-effects pipeline for Linux. Frames flow from a camera through GStreamer and CUDA memory into a [v4l2loopback](https://github.com/v4l2loopback/v4l2loopback) virtual camera that any application can select. The effects (background blur and background replacement) use the NVIDIA Maxine Video Effects SDK behind a replaceable inference-backend interface.

Optionally, the daemon also offers the camera as a native PipeWire video source, "Omarchy Broadcast", next to the loopback device, for applications that use PipeWire for cameras ([docs/setup/daemon.md](docs/setup/daemon.md#the-pipewire-output)).

Any UVC webcam works: the pipeline picks a capture mode from what the camera advertises, and adds a CPU decode step for MJPEG cameras and a CPU convert step for YUYV/UYVY.

## Status

Early development. The camera -> CUDA -> virtual camera passthrough works, with sustained 1080p60 on the reference machine. The `oma-broadcastd` daemon (a systemd user service controlled over D-Bus) and the `oma-broadcast` command-line client run it on demand: the camera and GPU start when an application opens the virtual camera ([docs/setup/daemon.md](docs/setup/daemon.md)). Background blur and background replacement run on the GPU through the Maxine SDK, which each user installs ([docs/setup/maxine.md](docs/setup/maxine.md)); they are selected with `oma-broadcast effect` and add a small GPU cost per frame. Both harden the segmentation mask edge, so the room does not show through around the subject ([docs/adr/0013-mask-hardening-cuda-kernel.md](docs/adr/0013-mask-hardening-cuda-kernel.md)). Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce locally with `obc-bench` (see [docs/testing.md](docs/testing.md)). Other measurements are in [docs/benchmarks/](docs/benchmarks/passthrough.md).

## Requirements

- NVIDIA GPU with the proprietary driver
- CUDA toolkit (for `nvrtc`)
- GStreamer 1.x with the `nvcodec` plugin (`cudaupload`, `cudaconvert`, `cudadownload`) and the base/good/bad plugin sets (`videoconvert`, `jpegdec`)
- v4l2loopback, configured as described in [docs/setup/loopback.md](docs/setup/loopback.md)
- Rust toolchain (only to build from source; the Arch package is built with it, and installed without it)
- Optional, for the native PipeWire camera output: a build with the cargo feature `oma-broadcastd/pipewire` (libpipewire headers and clang at build time; the Arch package has it) and `[pipewire] enabled = true` in the configuration
- Optional, for effects: the NVIDIA Maxine VFX SDK ([docs/setup/maxine.md](docs/setup/maxine.md))
- Optional, for the noise-removed microphone: `deepfilternet-ladspa` or `noise-suppression-for-voice` ([docs/setup/microphone.md](docs/setup/microphone.md))

## Build

```sh
cargo build --release
```

The PipeWire output is a cargo feature that is off by default: `cargo build --release --features oma-broadcastd/pipewire`.

## Install

Three routes; the daemon, the unit files and the other steps are described in [docs/setup/daemon.md](docs/setup/daemon.md). None of them configures the v4l2loopback device or installs the Maxine SDK.

### Arch package

`packaging/arch/PKGBUILD` builds `oma-broadcast-git` from the committed HEAD of the local checkout. Build from `packaging/arch`:

```sh
cd packaging/arch
makepkg -s
sudo pacman -U oma-broadcast-git-*.pkg.tar.zst
systemctl --user enable --now oma-broadcastd
```

The package installs the daemon and the client in `/usr/bin`, the systemd user unit, the D-Bus activation file, the Omarchy bar plugin and the documentation. It enables nothing: each user runs the last command. The loopback device is still configured as in [docs/setup/loopback.md](docs/setup/loopback.md). The package also installs the os-release shim that the Maxine effects need on Arch-based systems, inactive, in `/usr/lib/oma-broadcast/`. The NVIDIA Maxine SDK is never packaged; install it as in [docs/setup/maxine.md](docs/setup/maxine.md), which also covers activating the shim (section 7).

### Developer install

`scripts/install-user.sh` builds the release binaries and installs them for the current user, without root: the binaries in `~/.local/bin`, the unit and the D-Bus file under `~/.local/share`. When `make` and a C compiler are present it also builds and installs the os-release shim for the Maxine effects (`--no-shim` skips it; activation is described in [docs/setup/maxine.md](docs/setup/maxine.md), section 7). It never runs `systemctl`; it prints the next steps. `--dry-run` shows the actions and `--uninstall` removes only what the script installed.

```sh
scripts/install-user.sh
systemctl --user daemon-reload
systemctl --user enable --now oma-broadcastd
```

### Manual install

`cargo install` of the two crates and a copy of the unit file with `ExecStart=` rewritten: see [docs/setup/daemon.md](docs/setup/daemon.md#install).

The continuous integration (`.github/workflows/ci.yml`) runs the format check, clippy, the workspace tests and shellcheck in an Arch Linux container without a GPU, and a second job that builds, lints and tests `obc-pipeline` and `oma-broadcastd` with the PipeWire feature.

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

See [docs/README.md](docs/README.md) for the architecture, decision records, setup guides and benchmarks. The Omarchy bar widget (status icon, on/off, camera and effect selection) is described in [clients/omarchy-bar/README.md](clients/omarchy-bar/README.md).

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
