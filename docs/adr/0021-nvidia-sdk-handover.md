# ADR-0021: The NVIDIA SDK tooling moves to omarchy-nvidia-sdks; omarchy-broadcast keeps a drop-in and a default root

- **Status:** Accepted (2026-11-09). Amends [ADR-0012](0012-video-effects-and-maxine-backend.md) for the os-release shim: its location, its variable name, the drop-in that activates it and the way it is packaged. [ADR-0003](0003-maxine-first-behind-backend-seam.md) (the SDK is never vendored) is unchanged. Amended on 2026-11-09 (see [Amendment](#amendment-2026-11-09-the-sdk-root-from-the-configuration-and-the-sdks-libraries-loaded-by-the-daemon)): the SDK root can come from the configuration, and the daemon loads the SDK's libraries itself, so `LD_LIBRARY_PATH` is no longer needed.

## Context
- [ADR-0012](0012-video-effects-and-maxine-backend.md) put the os-release shim in this repository (`tools/os-release-shim`), with a probe and a replacement `os-release` in `tools/maxine-probe`. The Arch package installed the shim inactive under `/usr/lib/omarchy-broadcast/`, and `scripts/install-user.sh` built it into `~/.local/lib/omarchy-broadcast/`. The note on 2026-11-08 already said it was expected to move to a separate project.
- That project now exists: the omarchy-nvidia-sdks repository packages the SDK a user downloads from NVIDIA as the Arch package `maxine-vfx-sdk-local`. It installs the SDK under `/opt/nvidia/maxine/videofx`, its own build of the shim and the replacement `os-release` under `/usr/lib/maxine-vfx/`, and an environment file, `/usr/share/maxine-vfx/maxine-vfx.env`, that lists the library path of every packaged feature. It also owns the download, the GPU architecture support and the SDK troubleshooting.
- Two copies of the shim would drift. The package's shim already reads `MAXINE_OS_RELEASE`, where this repository's read `OMARCHY_BROADCAST_OS_RELEASE`, so the copies had diverged.
- The SDK is licensed for personal use and may not be redistributed, so this project cannot ship it, and the SDK is a download that belongs to one user. The licence summary is in [Licence and support](../setup/maxine.md#9-licence-and-support).
- The bundled CUDA 12 and TensorRT 10 libraries must reach only the daemon: Arch ships CUDA 13, and a global library path would shadow it for every program.

## Decision
**omarchy-broadcast contains no NVIDIA tooling. It uses `maxine-vfx-sdk-local` through one user drop-in and a default SDK root, and lists the package as an optional dependency.**

### What was removed
- `tools/os-release-shim` and `tools/maxine-probe` are deleted. The shim, its test, the replacement `os-release` and the SDK checks live in the omarchy-nvidia-sdks repository.
- The Arch package no longer builds or installs the shim and the replacement file; nothing is installed under `/usr/lib/omarchy-broadcast`.
- `scripts/install-user.sh` no longer builds or installs the shim, and its `--no-shim` option is gone. `--uninstall` and a reinstall still remove a shim that an older version of the script installed.
- The shim's own test is no longer in `docs/testing.md`; it is run in omarchy-nvidia-sdks.

### What the project keeps
- **The drop-in.** `dist/systemd/omarchy-broadcastd.service.d/maxine.conf` (the Arch package installs it unchanged as an example under `/usr/share/doc/omarchy-broadcast/examples/systemd/`) has two active lines: `EnvironmentFile=/usr/share/maxine-vfx/maxine-vfx.env` and `Environment=LD_PRELOAD=/usr/lib/maxine-vfx/os-release-shim.so`. The environment file supplies `MAXINE_VFX_ROOT`, `LD_LIBRARY_PATH` and `MAXINE_OS_RELEASE`; it never sets `LD_PRELOAD`, so the shim is preloaded on the daemon only. A commented variant describes an SDK tree outside the package.
- **The shim variable** is `MAXINE_OS_RELEASE` (debug output with `MAXINE_OS_RELEASE_DEBUG=1`), replacing `OMARCHY_BROADCAST_OS_RELEASE`. Nothing in this project reads or sets the old name.
- **A default SDK root.** When `MAXINE_VFX_ROOT` is unset or empty, the daemon uses `/opt/nvidia/maxine/videofx` if that directory exists; otherwise effects are unavailable as before, with the library search path only. A set variable still wins. The change is additive.
- **An optional dependency.** The Arch package has `optdepends=('maxine-vfx-sdk-local: NVIDIA Maxine video effects (build it from omarchy-nvidia-sdks)')`.
- **The documentation** in [../setup/maxine.md](../setup/maxine.md), reduced to the part that is specific to omarchy-broadcast. Its section numbers are cited by error messages of the daemon.

## Consequences
- This repository has no C code and no NVIDIA-specific build step. The package no longer has a `check()` for the shim.
- Enabling the effects is three steps: build and install `maxine-vfx-sdk-local` from omarchy-nvidia-sdks, copy the drop-in, restart the daemon. A feature added to the package later (noise removal was added after the first effects) needs only a rebuild of the package: its environment file is generated from the packaged features, and the drop-in does not change.
- The drop-in's `EnvironmentFile=` has no `-`. A user who installs the drop-in without the package gets a unit that fails to start, with `Failed to load environment files: No such file or directory` in the journal, not a daemon that runs silently without the SDK. The cost is that the drop-in cannot be installed ahead of the package.
- A user with an SDK tree that is not the package must set `MAXINE_VFX_ROOT`, `LD_LIBRARY_PATH`, `LD_PRELOAD` and `MAXINE_OS_RELEASE` by hand (the commented variant), with a shim built from omarchy-nvidia-sdks. The hardware tests in [../testing.md](../testing.md) do the same.
- The hermetic default test suite is unaffected. With the package installed the default root applies to a test process that builds the daemon's factory, but no default-suite test probes or loads the SDK through it: those that do construct the factory with an explicit root or none, or inject a test factory. The suite's `env -u` check removes the SDK variables but cannot disable the directory-based default.
- A user who followed the earlier instructions can have a shim in `~/.local/lib/omarchy-broadcast/` and a drop-in that points at it and sets `OMARCHY_BROADCAST_OS_RELEASE`. That drop-in no longer works as written; the user installs the package and copies the new drop-in. The old files are removed by `scripts/install-user.sh --uninstall` or a reinstall.
- The shim is still a compatibility measure for a distribution the SDK does not list, and NVIDIA does not support that configuration.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| Keep the shim in both repositories | Two copies drift. The variable rename had already made them differ, and a fix in one would not reach the other |
| Vendor or redistribute the SDK in a package or in this repository | NVIDIA's licence forbids redistribution (see [Licence and support](../setup/maxine.md#9-licence-and-support)). Each user downloads the SDK and builds the package locally |
| A global `ld.so.conf.d` or `profile.d` install of the SDK libraries | The bundled CUDA 12.8 and TensorRT 10.9 would shadow Arch's CUDA 13 for every program on the system. The libraries are put on the path of the daemon's unit only |
| A drop-in per feature, or one drop-in with a glob for the feature libraries | systemd has no glob in `Environment=`. The package's generated environment file already lists every packaged feature, so one `EnvironmentFile=` line covers all of them and a new feature needs no drop-in change |
| A hard `depends` on `maxine-vfx-sdk-local` | The SDK is a personal download that a user may not have, and the package is built locally. Most of the project works without it, so the dependency is optional |
| Leave the daemon without a default root and require `MAXINE_VFX_ROOT` always | The package puts the SDK in one fixed place, so the variable would only repeat it. The default needs no variable for the common case and still yields to an explicit one |

## What would change this decision
- A change in the package layout of `maxine-vfx-sdk-local` (its paths, its environment file or the variable names): the drop-in and the default root follow it.
- An SDK release that reads the distribution without an interposable libc call, or one that lists Arch: the shim would stop being needed, as noted in [ADR-0012](0012-video-effects-and-maxine-backend.md).
- A redistribution licence for the SDK: a package that contains it could then be a real dependency.

## Amendment (2026-11-09): the SDK root from the configuration, and the SDK's libraries loaded by the daemon

### Context
- The decision above left `LD_LIBRARY_PATH` as a requirement: the SDK's own libraries, the bundled CUDA 12 and TensorRT 10 and one directory per feature had to be on it when the process started, and systemd cannot expand a glob in `Environment=`. A tree outside the package needed a hand-written list of feature directories that went stale when a feature was added.
- The SDK root could be set only through the environment (`MAXINE_VFX_ROOT`), which a user with a tree elsewhere had to put in the unit.
- glibc reads `LD_LIBRARY_PATH` only at process start, which is why a running daemon cannot set it for itself.

### Decision
**The daemon loads the SDK's libraries from the SDK root by absolute path, and the root can be set in `config.toml`.**

- **Root precedence.** `MAXINE_VFX_ROOT` (set and non-empty), then `[effect] maxine_root` (an absolute path, validated when the configuration is loaded; whether the directory exists is not checked then), then `/opt/nvidia/maxine/videofx` when it is a directory (where `maxine-vfx-sdk-local` installs the SDK), then none: the library search path only, as before.
- **Self-loading.** When a root is chosen, before it opens `libNVCVImage` and `libVideoFX`, the daemon opens with `RTLD_GLOBAL`, by absolute path and dependencies first: `lib/libNVCVImage`, `lib/libVideoFXLocal`, `lib/libVideoFX`, every library in `features/*/lib`, and `external/cuda/lib/libnvJitLink.so.12` when it is present (TensorRT opens it by name), together with their `DT_NEEDED` closure, resolved in the SDK's directories (`lib`, `features/*/lib`, `external/cuda/lib`, `external/tensorrt/lib`). The SDK's own later `dlopen` calls by name then find the objects already loaded. The cuDNN and cuBLAS libraries find their sub-libraries through their own `RUNPATH $ORIGIN`. Only the libraries the SDK needs are loaded, not every bundled library.
- **The root is pinned.** It is taken when the SDK is first loaded in the process (the first probe or load of an effect or of noise removal) and recorded as the canonical path of that directory. A library is never unloaded, so a different root later, after a configuration reload, is refused: the effect fails closed with a message that asks for a daemon restart. A reload that changes the key logs this; an output that is already running keeps its root.
- **The variable wins over the key.** With `MAXINE_VFX_ROOT` set, `[effect] maxine_root` is ignored, with a warning in the log. The package's environment file (`/usr/share/maxine-vfx/maxine-vfx.env`) sets the variable, so a unit that loads it ignores the key.
- **`LD_LIBRARY_PATH` still works** when it is set: with no root the search path is used as before, and with a root the absolute-path loads are the same files.
- **Unchanged:** a missing or broken SDK makes the requested effect or noise removal fail with the reason in the `Error` property, and a preload failure is part of that reason. The os-release shim is still a requirement from process start: `LD_PRELOAD` with the shim and `MAXINE_OS_RELEASE` with the replacement file, which the daemon cannot set for itself either.
- **The drop-in** needs, with the package at the default root, only the two shim lines (`Environment=LD_PRELOAD=/usr/lib/maxine-vfx/os-release-shim.so` and `Environment=MAXINE_OS_RELEASE=/usr/lib/maxine-vfx/fake-os-release`). Its `EnvironmentFile=` line still works and is kept. For a tree elsewhere, set `[effect] maxine_root` (or `MAXINE_VFX_ROOT`) and keep the two shim lines; no `LD_LIBRARY_PATH` line and no per-feature list.

### Consequences
- A feature added to a tree needs no edit anywhere: its `features/*/lib` directory is found by the daemon.
- The "hand-written variables" consequence above shrinks to the two shim variables and, for a tree outside the default root, the root.
- One SDK per process, for the life of the process. Moving the root takes a daemon restart.
- The daemon depends on the layout of the SDK tree (`lib`, `features/*/lib`, `external/cuda/lib`, `external/tensorrt/lib`) and on the TensorRT library opening `libnvJitLink.so.12` by name. A layout change in a new SDK release is handled in the daemon, not in the unit.

### Alternatives considered

| Alternative | Why rejected |
|---|---|
| Keep requiring `LD_LIBRARY_PATH` | The list is long, systemd cannot glob, and a feature added to the tree silently fails with an unimplemented-status error until the list is edited |
| Load every bundled library | Much more memory for libraries nothing uses |
| Re-exec the daemon with `LD_LIBRARY_PATH` set | It works, but it doubles the process start and complicates the unit and D-Bus activation (the process that owns the bus name and the one systemd tracks must stay the same) |
| An environment variable as the only way to set the root | A user with a tree elsewhere would have to edit the unit; the key keeps the setting with the rest of the configuration. The variable still wins, because the package's environment file sets it |

