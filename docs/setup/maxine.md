# Setting up NVIDIA Maxine Video Effects SDK on Arch Linux / Omarchy

Goal: a user-prefix install of the Maxine VFX SDK 1.3.0.0 (Linux) at `$MAXINE_VFX_ROOT`, with the AI green screen (AIGS), background blur and video noise removal (denoising) features, ready for `omarchy-broadcast`. The examples use `~/.local/opt/maxine/VideoFX` as the default value. No `sudo` is needed and nothing is written to `/usr`, `/opt` or `/etc`.

Background: [../research/maxine-linux.md](../research/maxine-linux.md) (access, licence, API). The SDK is never vendored in this repository, see [ADR-0003](../adr/0003-maxine-first-behind-backend-seam.md).

Steps marked **Manual** need your NVIDIA credentials and cannot be scripted. Everything else is copy-paste.

Choose the install root once; the later steps use these variables:

```bash
export MAXINE_VFX_ROOT=~/.local/opt/maxine/VideoFX   # the SDK root (contains lib/, features/, external/)
PREFIX=$(dirname "$MAXINE_VFX_ROOT")                 # holds VideoFX/ and env.sh
```

## Prerequisites

| Item | Requirement | Check |
|---|---|---|
| GPU | NVIDIA, compute capability 7.5 or newer (Turing and up). Tested on the reference machine: RTX 5080 Laptop, CC 12.0 (sm_120) | `nvidia-smi --query-gpu=name,compute_cap --format=csv` |
| Driver | 570.26 or newer. Tested: 615.78.08. The mask-hardening kernel ([ADR-0013](../adr/0013-mask-hardening-cuda-kernel.md)) does not raise this floor or the GPU floor | `nvidia-smi --query-gpu=driver_version --format=csv,noheader` |
| System CUDA toolkit | Not needed. The SDK bundles CUDA 12.8, cuDNN 9.7.1 and TensorRT 10.9 and only takes `libcuda.so.1` from the driver (`nvidia-utils`) | |
| Tools | `curl`, `unzip`, `jq`, `gcc` (also builds the os-release shim in step 7), `make` | `pacman -S --needed curl unzip jq gcc make` |
| Disk | About 4.5 GB for the final install. Peak about 8 GB while the downloads (2.7 GB) and the extracted tree coexist | `df -h ~` |

## 1. NVIDIA account and access (Manual)

A plain NGC account is not enough. An NVIDIA Developer Program account (free membership) is enough; NVIDIA AI Enterprise is not required.

1. Create or log in to an NVIDIA Developer Program account. Use this one account for NGC, the Developer Program and the API key in step 2.
2. Open https://catalog.ngc.nvidia.com/orgs/nvidia/maxine/resources/vfx_sdk_core ("NVIDIA Video Effects SDK Core"), log in.
3. Under "NVIDIA Developer Program" click **Get Access** and accept the terms.

The API key in step 2 must come from the account and org that holds this access, otherwise downloads fail with 403/402 (see troubleshooting).

## 2. Personal API key (Manual)

1. Open https://org.ngc.nvidia.com/setup/personal-keys and click **Generate Personal Key**.
2. Include the **NGC Catalog** service. Choose an expiry you are comfortable with.
3. Copy the key (`nvapi-...`) immediately; it is shown once. Keep it out of the repo and out of shell history; you will paste it into `ngc config set` in step 3.

## 3. NGC CLI

Arch has no package for it. Install the official zip from NGC's public API into the user prefix. Pin the version in `NGC_VER` (4.36.6 was used when this was written; the newest is the first entry of the version list).

```bash
# newest published version (4.36.6 when this was written)
curl -s https://api.ngc.nvidia.com/v2/resources/nvidia/ngc-apps/ngc_cli/versions | jq -r '.recipeVersions[0].versionId'

NGC_VER=4.36.6
D=$(mktemp -d) && cd "$D"
curl -L -o ngccli_linux.zip \
  "https://api.ngc.nvidia.com/v2/resources/nvidia/ngc-apps/ngc_cli/versions/$NGC_VER/files/ngccli_linux.zip"
unzip -q ngccli_linux.zip                      # -> ngc-cli/  and  ngc-cli.md5

# integrity check: the two hashes must be identical
find ngc-cli/ -type f -exec md5sum {} + | LC_ALL=C sort | md5sum
cat ngc-cli.md5

mkdir -p ~/.local/opt ~/.local/bin
mv ngc-cli ~/.local/opt/ngc-cli
ln -sf ~/.local/opt/ngc-cli/ngc ~/.local/bin/ngc
cd ~ && rm -rf "$D"
ngc --version                                  # NGC CLI 4.36.6
```

`~/.local/bin` must be on `PATH` (it is on Omarchy).

### Configure (Manual)

```bash
ngc config set
```

Answer the prompts interactively so the key never ends up in a script, a log or shell history:

- API key: paste the `nvapi-...` key from step 2.
- CLI output format: `ascii`.
- org: the org of the account that has the Developer Program access (the prompt lists the orgs your key can see).
- team and ace: press Enter to accept the defaults.

The key is stored in `~/.ngc/config`. Check access (read-only):

```bash
ngc registry resource list "nvidia/maxine/vfx_sdk_core:*"
```

You should see a table of versions (`1.3.0.0_linux`, `1.3.0.0_woa`, ...). An authorization error means the key or org is wrong, see troubleshooting.

## 4. Download the core SDK

Tags carry a platform suffix. Use `_linux`. `_woa` is Windows on ARM and is not usable (it was the newest-looking entry on the catalog page). Linux 1.3.0.0 is 2.63 GB and holds `VFXSDK_linux_1.3.0.0.tgz` (the one needed) and a Triton gRPC-client variant (not needed).

```bash
ngc registry resource list "nvidia/maxine/vfx_sdk_core:*"      # confirm the tag
mkdir -p ~/Downloads/maxine
ngc registry resource download-version "nvidia/maxine/vfx_sdk_core:1.3.0.0_linux" --dest ~/Downloads/maxine
ls -lh ~/Downloads/maxine/vfx_sdk_core_v1.3.0.0_linux/
# README_VFX_SDK_Triton.md  VFXSDK_linux_1.3.0.0.tgz (2.8 GB)  VFXSDK_triton_1.3.0.0.tgz
```

## 5. Download features and the GPU-architecture models

AIGS needs its feature library plus TensorRT model packages built for your GPU's SM architecture. Background blur is pure CUDA compositing on top of an external mask and has no model package. Video noise removal (the `nvvfxdenoising` feature, the equivalent of NVIDIA Broadcast's "Video Noise Removal") needs its feature library and its own model packages for your architecture; it is optional, and the effects work without it.

Pick the architecture from your GPU:

```bash
SM=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d .)   # 12.0 -> 120
echo "SM=$SM"
ngc registry model list "nvidia/maxine/nvvfxgreenscreen:*"
ngc registry model list "nvidia/maxine/nvvfxdenoising:*"     # only for noise removal
```

In that list, a `1.3.0.0_models_linux_sm<SM>` entry must exist for your value (in each of the two lists you use). For 1.3.0.0 these exist: `sm75, sm80, sm86, sm89, sm90, sm100, sm120` (Linux). If your `SM` is not listed (for example CC 8.7 or 12.1), pick the closest listed one only if you accept an untested combination; the loader may refuse it. Ignore entries named `windows_*` and `woa_*`.

Download into a scratch directory (the NGC CLI creates one subdirectory per version):

```bash
DL=$(mktemp -d) && cd "$DL"
ngc registry model download-version nvidia/maxine/nvvfxgreenscreen:1.3.0.0_lib_linux --dest .
ngc registry model download-version nvidia/maxine/nvvfxbackgroundblur:1.3.0.0_lib_linux --dest .
ngc registry model download-version "nvidia/maxine/nvvfxgreenscreen:1.3.0.0_models_linux_sm$SM" --dest .
# optional, for noise removal:
ngc registry model download-version nvidia/maxine/nvvfxdenoising:1.3.0.0_lib_linux --dest .
ngc registry model download-version "nvidia/maxine/nvvfxdenoising:1.3.0.0_models_linux_sm$SM" --dest .
ls                                             # directory names as created by the CLI, e.g.
# nvvfxgreenscreen_v1.3.0.0_lib_linux  nvvfxbackgroundblur_v1.3.0.0_lib_linux  nvvfxgreenscreen_v1.3.0.0_models_linux_sm120
# and, with noise removal: nvvfxdenoising_v1.3.0.0_lib_linux  nvvfxdenoising_v1.3.0.0_models_linux_sm120
```

Sizes: greenscreen lib 2.6 MB, blur lib 0.4 MB, sm120 models 113 MB (8 `.engine.trtpkg` files). Keep this shell and `$DL` for step 6.

The feature version's MAJOR.MINOR must equal the SDK's (1.3). Mixing versions is rejected by `install_feature.sh` and is unsupported here.

## 6. Lay out the prefix

This replicates `features/install_feature.sh` from the SDK. That script hard-codes `VFXSDK_PATH=/usr/local/VideoFX`, has no prefix option and needs root, so it is not used. What it does, and what the commands below reproduce 1:1: extract the core to the prefix, extract each feature `tar.gz` into `<prefix>/VideoFX/features`, and copy the model files into `<prefix>/VideoFX/lib/models`.

```bash
mkdir -p "$PREFIX"
tar -xzf ~/Downloads/maxine/vfx_sdk_core_v1.3.0.0_linux/VFXSDK_linux_1.3.0.0.tgz -C "$PREFIX"   # -> $PREFIX/VideoFX (4.4 GB)

cd "$DL"
for t in */*.tar.gz; do tar -xzf "$t" -C "$PREFIX/VideoFX/features"; done
cp nvvfxgreenscreen_v1.3.0.0_models_linux_sm$SM/* "$PREFIX/VideoFX/lib/models/"
# with noise removal:
cp nvvfxdenoising_v1.3.0.0_models_linux_sm$SM/* "$PREFIX/VideoFX/lib/models/"
```

Resulting layout (`$MAXINE_VFX_ROOT`, 4.4 GB):

```
VideoFX/
  include/                 nvVideoEffects.h  nvCVImage.h  nvCVStatus.h
  lib/                     libVideoFX.so  libVideoFXLocal.so  libNVCVImage.so  libnvngxruntime.so  libVideoFXTriton.so (unused)
  lib/models/              AIGS_288x512_<SM>_m{0,1,2,3}.engine.trtpkg  and  *_1_4_8.engine.trtpkg   (8 files, 114 MB)
                           Denoise-agg-<SM>.engine.trtpkg  Denoise-con-<SM>.engine.trtpkg   (noise removal)
  features/nvvfxgreenscreen/lib/libnvVFXGreenScreen.so         (+ include/, license/)
  features/nvvfxbackgroundblur/lib/libnvVFXBackgroundBlur.so   (+ include/, license/)
  features/nvvfxdenoising/lib/libnvVFXDenoising.so             (noise removal; + include/, license/)
  features/install_feature.sh  compute_capability  README.md   (not used)
  external/cuda/           bundled CUDA 12.8 (3.6 GB: bin, include, lib)
  external/tensorrt/       bundled TensorRT 10.9 (700 MB: include, lib)
  share/                   docs, licence PDFs, third-party licences
```

Reclaim space and clean up (the tarball is kept only if you want a quick reinstall):

```bash
rm -rf "$DL"
rm -rf ~/Downloads/maxine          # optional, frees 2.7 GB
```

## 7. The os-release override

**Why.** The model-package loader inside `libnvVFXGreenScreen.so` (and the one of the noise-removal feature) reads `/etc/os-release` (with `fopen`, twice per load) and refuses distributions that are not on NVIDIA's list. Omarchy reports `ID=omarchy`, so `NvVFX_Load` fails with `-1209` ("no kernel image is available for execution on the device"). That message is misleading: no CUDA error occurs (verified with compute-sanitizer), the models do contain sm_120 code, and the failure is purely the distro check. Findings on what passes:

| os-release | Result |
|---|---|
| `ID=ubuntu` + `VERSION_ID="24.04"` | Load succeeds |
| `ID=debian` + `VERSION_ID="12"` | Load succeeds |
| `ID=rocky` + `VERSION_ID="9.4"` | Load succeeds |
| `ID=arch` (any `VERSION_ID`), or `ID_LIKE=ubuntu` | Fails (-1209). Only the `ID` / `VERSION_ID` pair is read |
| `ID=ubuntu` without `VERSION_ID` | No output at all in testing, probably a crash. Always give `VERSION_ID` |

**How.** A small `LD_PRELOAD` library, [`tools/os-release-shim`](../../tools/os-release-shim/README.md), makes one process see a replacement `/etc/os-release`: inside a process that preloads it, a read-only open of exactly `/etc/os-release` opens the file named by `OMARCHY_BROADCAST_OS_RELEASE` instead, and every other call passes through unchanged. Only that process is affected, the real file is untouched, and no root is needed.

**Why not a namespace.** Hiding the file with a bind mount needs a user and mount namespace (`unshare -Urm`, or `PrivateUsers=` and `BindReadOnlyPaths=` in the unit). The daemon finds out that an application has opened the virtual camera by reading `/proc/<pid>/fd/N` of the user's other processes, and inside a user namespace the kernel refuses `readlink` on those entries (listing the directory still works). The daemon would then never see a consumer and would stay idle, sending black. `unshare -Urm` also presents the user as uid 0 to the session bus, so authentication fails. The reasoning is in [ADR-0012](../adr/0012-video-effects-and-maxine-backend.md). The shim needs no namespace: the process keeps its uid, its mounts and `/proc`.

### Build and install

Three routes put the same two files in place: the shim library and a replacement `os-release`. None of them activates anything; activation is step 7a, after the SDK is installed.

- **Arch package.** Already installed, inactive: `/usr/lib/omarchy-broadcast/os-release-shim.so` and `/usr/lib/omarchy-broadcast/fake-os-release`. The package's `check()` runs the shim's test. The SDK itself is never packaged.
- **`scripts/install-user.sh`.** Builds and installs the shim by default when `make` and a C compiler are present (otherwise it skips the shim with a note), to `~/.local/lib/omarchy-broadcast/os-release-shim.so` and `~/.local/lib/omarchy-broadcast/fake-os-release`. `--no-shim` skips it; `--no-build` installs an existing build only. The script never installs or enables the systemd drop-in; it prints the activation steps. If a shim was placed by hand earlier at `~/.local/lib/omarchy-broadcast`, the script refuses: remove those files first, or pass `--no-shim`.
- **Manual**, from the repository root:

```bash
make -C tools/os-release-shim                     # -> target/os-release-shim/os-release-shim.so
make -C tools/os-release-shim test                # optional: the shim's own test
install -Dm755 target/os-release-shim/os-release-shim.so ~/.local/lib/omarchy-broadcast/os-release-shim.so
install -Dm644 tools/maxine-probe/fake-os-release ~/.local/lib/omarchy-broadcast/fake-os-release
```

The repository ships a working replacement file at [../../tools/maxine-probe/fake-os-release](../../tools/maxine-probe/fake-os-release); its content is:

```
NAME="Ubuntu"
VERSION_ID="24.04"
ID=ubuntu
ID_LIKE=debian
VERSION_CODENAME=noble
PRETTY_NAME="Ubuntu 24.04 LTS"
```

### 7a. The daemon (systemd user unit)

The optional drop-in sets `LD_PRELOAD`, `OMARCHY_BROADCAST_OS_RELEASE`, `MAXINE_VFX_ROOT` and `LD_LIBRARY_PATH` for the daemon, with the SDK in `~/.local/opt/maxine/VideoFX`. Two versions exist, differing only in where the shim and the replacement file are:

- the package's example, `/usr/share/doc/omarchy-broadcast/examples/systemd/omarchy-broadcastd.service.d/maxine.conf`, points at `/usr/lib/omarchy-broadcast/`;
- the checkout's [`dist/systemd/omarchy-broadcastd.service.d/maxine.conf`](../../dist/systemd/omarchy-broadcastd.service.d/maxine.conf) points at `%h/.local/lib/omarchy-broadcast/` (the `install-user.sh` and manual locations).

Adjust the paths in the file if you installed elsewhere. After installing the SDK, copy the matching file and restart the daemon:

```bash
# package install:
install -Dm644 /usr/share/doc/omarchy-broadcast/examples/systemd/omarchy-broadcastd.service.d/maxine.conf \
    ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf
# checkout (install-user.sh or manual install), from the repository root:
install -Dm644 dist/systemd/omarchy-broadcastd.service.d/maxine.conf \
    ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf

systemctl --user daemon-reload
systemctl --user restart omarchy-broadcastd.service
```

Remove the file (then `daemon-reload` and restart) to go back to the default unit. If the shim file is missing, the dynamic loader prints `ld.so: object '...' from LD_PRELOAD cannot be preloaded ... ignored` in the journal and the daemon starts normally; the first effect then fails with the reason in the `Error` property (see troubleshooting). Without the drop-in the daemon runs as usual and effects fail the same way.

### 7b. A manual run (probe, `obc-bench`, the daemon, tests)

Set the two variables on the program that loads the SDK:

```bash
env LD_PRELOAD=~/.local/lib/omarchy-broadcast/os-release-shim.so \
    OMARCHY_BROADCAST_OS_RELEASE=$HOME/.local/lib/omarchy-broadcast/fake-os-release \
    <command> [args]
```

**Set `LD_PRELOAD` on the final program, never on a wrapper in front of it.** The shim removes `LD_PRELOAD` from the environment of the process that loads it, so that no child inherits it. A wrapper such as `timeout`, `sh -c` or `cargo` that carries `LD_PRELOAD` loads the shim itself, drops the variable, and starts the program without it, which then fails with `-1209`. Write `timeout 60 env LD_PRELOAD=... prog`, not `LD_PRELOAD=... timeout 60 prog`. For `cargo test`, use a runner so that only the test binary is preloaded:

```bash
OMARCHY_BROADCAST_OS_RELEASE=$HOME/.local/lib/omarchy-broadcast/fake-os-release \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$HOME/.local/lib/omarchy-broadcast/os-release-shim.so" \
    cargo test -p obc-backend-maxine --test hardware -- --ignored --test-threads=1
```

`OMARCHY_BROADCAST_OS_RELEASE_DEBUG=1` makes the shim log each redirect on stderr, which shows whether it is active.

Support note: see section 11.

## 8. Environment

The SDK bundles CUDA 12.8, cuDNN 9, TensorRT 10 and NPP 12, while Arch ships CUDA 13. None of the `.12` / `.9` / `.10` sonames exist system-wide, so the bundled directories must be on `LD_LIBRARY_PATH`. The feature libraries are found by plain `dlopen("libnvVFXGreenScreen.so")`, so their directories must be on it too (otherwise `NvVFX_CreateEffect` returns `-2`). That includes `features/nvvfxdenoising/lib` when noise removal is installed.

Create an env file (kept outside the repo):

```bash
cat > "$PREFIX/env.sh" <<'EOF'
export MAXINE_VFX_ROOT="${MAXINE_VFX_ROOT:-$HOME/.local/opt/maxine/VideoFX}"
P="$MAXINE_VFX_ROOT"
export LD_LIBRARY_PATH="$P/lib:$P/features/nvvfxgreenscreen/lib:$P/features/nvvfxbackgroundblur/lib:$P/features/nvvfxdenoising/lib:$P/external/cuda/lib:$P/external/tensorrt/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
unset P
EOF
source "$PREFIX/env.sh"
```

- `MAXINE_VFX_ROOT` is the one variable the project reads: the SDK root (the directory containing `lib/`, `features/`, `external/`). The daemon derives the model directory (`$MAXINE_VFX_ROOT/lib/models`, passed to the SDK as `NVVFX_MODEL_DIRECTORY`) and the library paths from it, and the systemd drop-in of step 7 sets the same variables in `Environment=`.
- Do not `source` this from `~/.bashrc` or `~/.profile`. The bundled CUDA 12 libraries would shadow system libraries for every program you start. Source it per shell, or set it per process/unit.

## 9. Verify with the probe

The probe ([`tools/maxine-probe/probe.c`](../../tools/maxine-probe/), see its README) loads AIGS, runs 200 frames at 1080p, then runs background blur. The CUDA headers it needs are bundled; no system toolkit is required. The model directory is argv[1]; if omitted it defaults to `./models`, which does not exist and gives `-13` ("file could not be found").

```bash
source "$PREFIX/env.sh"                # first, so the linker finds the bundled .12 libs
P=$MAXINE_VFX_ROOT
cd "$(mktemp -d)"
gcc -O2 -o probe <repo root>/tools/maxine-probe/probe.c \
  -I$P/include -I$P/external/cuda/include -L$P/lib -lVideoFX -lNVCVImage -lcuda
env LD_PRELOAD=~/.local/lib/omarchy-broadcast/os-release-shim.so \
    OMARCHY_BROADCAST_OS_RELEASE=$HOME/.local/lib/omarchy-broadcast/fake-os-release \
    ./probe $P/lib/models
```

Expected output on the reference machine (RTX 5080 Laptop, driver 615.78; timings are shown as `<t>`; they vary by GPU and are not published here, per the NVIDIA Maxine SDK licence):

```
NvVFX_GetVersion = 0x01030000  (major 1 minor 3 build 0)
CUDA device: NVIDIA GeForce RTX 5080 Laptop GPU
NvVFX_Load -> 0 (No error)  took <t> ms
first NvVFX_Run(sync) -> 0 (No error)  took <t> ms
AIGS mode 0 1920x1080 BGR u8 GPU -> A u8 GPU: avg <t> ms/frame over 200 (sync Run)
...
Blur NvVFX_Load -> 0 (No error) took <t> ms
BackgroundBlur 1920x1080: avg <t> ms/frame
DONE
```

Pass criteria: version `0x01030000`, `NvVFX_Load -> 0`, `DONE`, no `FAIL` line. Set `VFX_LOG=1` for the SDK's own log on stderr.

Observation, cause unconfirmed: on the very first probe run the first `NvVFX_Run` took much longer than later ones (plausibly a one-time warm-up of the TensorRT context, cuDNN and CUDA graph capture); later runs were faster, plausibly because of a CUDA JIT or file cache. Budget for a cold first run of a few seconds: the daemon is meant to show a black or "starting..." frame while an effect loads.

Without the shim the same probe stops at `NvVFX_Load -> -1209`, which confirms the distro gate.

### Using the effects

With the SDK installed and the daemon running with the drop-in (step 7a), the effects are selected in two ways:

- In `config.toml`, the `[effect]` section (`mode`, `blur_strength`, `background_image`, `on_failure`); see [../config.example.toml](../config.example.toml).
- At run time, with `omarchy-broadcast effect none|blur|replace [--strength X] [--image PATH]`, which the daemon remembers across restarts; see [../cli-json.md](../cli-json.md) and [../dbus-api.md](../dbus-api.md).

Video noise removal is a separate switch that combines with any effect: `[effect] denoise = true` in `config.toml`, or `omarchy-broadcast denoise on|off` at run time. It removes the grain a webcam produces, which is mostly visible in dim rooms, and it runs before the blur or replacement, or alone with the effect `none`. It needs the `nvvfxdenoising` feature and its models (steps 5 and 6). Things to know:

- **It fails open.** Noise removal hides nothing, so if it cannot load or run (the feature or SDK is missing, or the frame size is out of range) the stream continues without it, and the daemon's `Error` property says `noise removal unavailable: <reason>`. `[effect] on_failure` does not apply to it and the daemon does not enter `error`. The effects themselves still fail closed, as above.
- **Frame size.** The feature supports frames from 80x80 up to 1920x1080 (the SDK's range). An output contract above 1080p runs without noise removal and reports the reason.
- **Strength.** The SDK's strength is binary: 0 favours texture, 1 favours noise removal. The daemon uses 1 and has no setting for it. For comparisons during development, `OBC_MAXINE_DENOISE_STRENGTH=0` or `=1` in the environment of the process overrides it. It is a development knob only and is not a supported configuration.
- **Cost.** It adds one GPU pass per frame and its model's GPU memory; with it off, nothing is added, no pass, copy or allocation. The models load only while it is on. Measure on your machine with `obc-bench --denoise` ([../testing.md](../testing.md)).
- The design is in [ADR-0018](../adr/0018-video-noise-removal.md).

The background image for `replace` must be a PNG or JPEG file. Until an effect is ready, the virtual camera shows black rather than the unprocessed camera image; if the SDK cannot be loaded, the daemon enters the `error` state (default) instead of showing the unprocessed image. The design is in [ADR-0012](../adr/0012-video-effects-and-maxine-backend.md). Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce locally with `obc-bench` (see [../testing.md](../testing.md)).

## 10. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `NvVFX_Load` returns `-1209`, "no kernel image is available", log line `Loading TRTModel failed in Aigs` | `/etc/os-release` is not an accepted distro (`ID=omarchy`). Not a real CUDA error | Step 7: the shim is missing or not applied to this process. Check that the library exists at the path in `LD_PRELOAD`, that `OMARCHY_BROADCAST_OS_RELEASE` is an absolute path to a file with both `ID=` and `VERSION_ID=`, that `LD_PRELOAD` is set on the program itself and not on a wrapper (`timeout`, `sh -c`, `cargo`), and run with `OMARCHY_BROADCAST_OS_RELEASE_DEBUG=1` to see the redirect |
| `NvVFX_CreateEffect` returns `-2` (unimplemented) | `libnvVFXGreenScreen.so` / `libnvVFXBackgroundBlur.so` / `libnvVFXDenoising.so` not found by `dlopen` | Put `features/nvvfxgreenscreen/lib`, `features/nvvfxbackgroundblur/lib` and `features/nvvfxdenoising/lib` on `LD_LIBRARY_PATH` (step 8); confirm the feature was extracted in step 6 |
| The daemon's `Error` says `noise removal unavailable: ... -2` (NVCV status), and the camera image is shown without noise removal | The feature library `libnvVFXDenoising.so` is not on `LD_LIBRARY_PATH` of the daemon (the drop-in or `env.sh` predates noise removal, or the feature was not extracted) | Add `features/nvvfxdenoising/lib` to `LD_LIBRARY_PATH` (the shipped drop-in has it; re-copy it, `daemon-reload`, restart) and check step 6 |
| The same, with `-1209` or `-13` | The os-release override is missing for this process (the feature's library needs it too), or the `Denoise-*` models for your SM are not in `lib/models` | Step 7; `ls $MAXINE_VFX_ROOT/lib/models` must show `Denoise-agg-<SM>.engine.trtpkg` and `Denoise-con-<SM>.engine.trtpkg` |
| The same, with a message that the frame size is out of range | The output contract is smaller than 80x80 or larger than 1920x1080 | Use an output mode within the range (`[output]` in `config.toml`); noise removal does not run outside it |
| `NvVFX_Load` returns `-13` ("file could not be found") | Model directory wrong or models missing | Pass `$MAXINE_VFX_ROOT/lib/models` (probe argv[1]; the default `./models` does not exist). Check `ls $MAXINE_VFX_ROOT/lib/models` shows 8 `.trtpkg` files for your SM |
| `error while loading shared libraries: libcudart.so.12` / `libnpp*.so.12` / `libcudnn.so.9` / `libnvinfer.so.10` not found (or linker warnings about `.so.12`) | Bundled `external/cuda/lib` and `external/tensorrt/lib` not on `LD_LIBRARY_PATH` | `source "$PREFIX/env.sh"`, then check `ldd $MAXINE_VFX_ROOT/lib/libVideoFXLocal.so \| grep 'not found'` is empty |
| `Cannot load libVideoFXTriton.so` in the log | The Triton client library needs `libgrpcclient.so`; unused | Harmless, ignore |
| Model list shows only `_woa` / `windows` entries, or the downloaded SDK has no Linux libs | Wrong tag: `_woa` is Windows on ARM | Use `1.3.0.0_linux` (step 4) |
| `ngc registry ...` returns 401/402/403 or an empty list, "not authorized" | API key from another account or org than the Developer Program access, or "NGC Catalog" service not included, or access not yet granted | Redo step 1 with the same account that owns the key; regenerate the key with the NGC Catalog service (step 2); run `ngc config set` again with the right org; `ngc config current` shows what is active |
| No `models_linux_sm<SM>` for your GPU | GPU architecture not in the 1.3.0.0 matrix | List with step 5; Turing/Ampere/Ada/Hopper/Blackwell (sm75 to sm120) are covered |
| `NvVFX_Load` succeeds but a program hangs at start | Cold first run (see step 9 observation) | Wait a few seconds; the probe prints the time |
| The daemon's `Error` property reads `effect blur unavailable: NvVFX_Load(GreenScreen) returned -1209 (...): the SDK rejected this distribution (its model loader reads /etc/os-release). Run the process with the os-release override (docs/setup/maxine.md, section 7)` and the state is `error` | The daemon runs without the shim: the drop-in is not installed, `daemon-reload` and a restart were skipped, or its paths do not match your installation. With the default `on_failure = "black"` the virtual camera stays black | Install the drop-in (step 7a) and check `systemctl --user show omarchy-broadcastd -p Environment`; a `cannot be preloaded` line in `journalctl --user -u omarchy-broadcastd` means the library path is wrong |
| The daemon's `Error` property or the element's status names the mask-hardening kernel and a CUDA error `222` ("the NVIDIA driver is older than this build supports") | The driver rejects the PTX of the mask-hardening kernel: it is older than 570.26 | Update the NVIDIA driver to 570.26 or newer (`nvidia-smi --query-gpu=driver_version --format=csv,noheader`). With the default `on_failure = "black"` the virtual camera stays black until then |
| The same, with CUDA error `221` | The driver cannot JIT-compile the kernel: `libnvidia-ptxjitcompiler.so.1` is missing from the driver installation | Reinstall the driver's userspace package (`nvidia-utils`) so that it provides `libnvidia-ptxjitcompiler.so.1`; check with `ldconfig -p \| grep ptxjitcompiler` |
| `ld.so: object '...os-release-shim.so' from LD_PRELOAD cannot be preloaded` | The shim library is not at the path in `LD_PRELOAD`; the program runs without it | Build and install it, or check the paths in the drop-in (step 7) |

## 11. Licence and support notes

Summary of the licence terms as read from the SDK licence documents. This is not legal advice; read the licence you accept.

- The SDK and models are under the NVIDIA Software License Agreement plus the AI Product-Specific Terms. The Developer Program path is "solely for internal evaluation, development or test non-production purposes ... not for use, distribution or deployment in production" (terms 1.2.8). Personal use on your own machine and development are fine; shipping the SDK inside a product needs an NVIDIA enterprise agreement.
- **No redistribution.** SLA 8.5 forbids copying, distributing or sublicensing any part of the Software, "including in any publicly accessible software repositories". Never commit the SDK, the models (`*.trtpkg`), the NGC key or `~/.ngc/` to this repo, never put them in a package or release artifact (users download from NGC themselves, the daemon only points at the install directory). Keep the install directory outside the working tree.
- No reverse engineering of the binaries (SLA 8.4); no publishing benchmark results without written permission (SLA 8.9). The timings above are for local verification, not for publication.
- The os-release override (the shim of step 7) is a compatibility shim for distributions the SDK does not list. It changes only what the daemon (or another process you start with it) reads as `/etc/os-release`. It does not modify the SDK, the models or their encryption. NVIDIA does not support this configuration.
- NVIDIA documents the Linux SDK as server-side and does not officially support local, client-side use or Arch. That is a support statement, not a technical block. Expect no vendor support.

## 12. Uninstall

```bash
rm -rf "$PREFIX"                         # SDK, models and env.sh
# install-user.sh route: scripts/install-user.sh --uninstall removes the whole script install
#   (binaries, unit, D-Bus file and shim), not only the shim
# manual route:
rm -f ~/.local/lib/omarchy-broadcast/fake-os-release ~/.local/lib/omarchy-broadcast/os-release-shim.so
rm -f ~/.local/share/omarchy-broadcast/fake-os-release   # installs made by earlier instructions
# package install: sudo pacman -R omarchy-broadcast-git removes /usr/lib/omarchy-broadcast/*
# every route: remove the drop-in, then reload
rm -f ~/.config/systemd/user/omarchy-broadcastd.service.d/maxine.conf   # then: systemctl --user daemon-reload
rm -rf ~/Downloads/maxine                # downloads, if kept
# NGC CLI (only if nothing else uses it)
rm -rf ~/.local/opt/ngc-cli ~/.local/bin/ngc
rm -rf ~/.ngc                            # contains your API key; also revoke it at https://org.ngc.nvidia.com/setup/personal-keys
```

Nothing outside your home directory was written, so nothing else needs reverting.
