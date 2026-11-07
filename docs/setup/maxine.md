# Setting up NVIDIA Maxine Video Effects SDK on Arch Linux / Omarchy

Goal: a user-prefix install of the Maxine VFX SDK 1.3.0.0 (Linux) at `$MAXINE_VFX_ROOT`, with the AI green screen (AIGS) and background blur features, ready for `oma-broadcast`. The examples use `~/.local/opt/maxine/VideoFX` as the default value. No `sudo` is needed and nothing is written to `/usr`, `/opt` or `/etc`.

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
| Driver | 570.26 or newer. Tested: 615.78.08 | `nvidia-smi --query-gpu=driver_version --format=csv,noheader` |
| System CUDA toolkit | Not needed. The SDK bundles CUDA 12.8, cuDNN 9.7.1 and TensorRT 10.9 and only takes `libcuda.so.1` from the driver (`nvidia-utils`) | |
| Tools | `curl`, `unzip`, `jq`, `gcc`, `unshare` (util-linux) | `pacman -S --needed curl unzip jq gcc util-linux` |
| Disk | About 4.5 GB for the final install. Peak about 8 GB while the downloads (2.7 GB) and the extracted tree coexist | `df -h ~` |
| Kernel | Unprivileged user namespaces enabled (default on Arch) for the os-release override in step 7 | `unshare -Urm true && echo ok` |

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

AIGS needs its feature library plus TensorRT model packages built for your GPU's SM architecture. Background blur is pure CUDA compositing on top of an external mask and has no model package.

Pick the architecture from your GPU:

```bash
SM=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d .)   # 12.0 -> 120
echo "SM=$SM"
ngc registry model list "nvidia/maxine/nvvfxgreenscreen:*"
```

In that list, a `1.3.0.0_models_linux_sm<SM>` entry must exist for your value. For 1.3.0.0 these exist: `sm75, sm80, sm86, sm89, sm90, sm100, sm120` (Linux). If your `SM` is not listed (for example CC 8.7 or 12.1), pick the closest listed one only if you accept an untested combination; the loader may refuse it. Ignore entries named `windows_*` and `woa_*`.

Download into a scratch directory (the NGC CLI creates one subdirectory per version):

```bash
DL=$(mktemp -d) && cd "$DL"
ngc registry model download-version nvidia/maxine/nvvfxgreenscreen:1.3.0.0_lib_linux --dest .
ngc registry model download-version nvidia/maxine/nvvfxbackgroundblur:1.3.0.0_lib_linux --dest .
ngc registry model download-version "nvidia/maxine/nvvfxgreenscreen:1.3.0.0_models_linux_sm$SM" --dest .
ls                                             # directory names as created by the CLI, e.g.
# nvvfxgreenscreen_v1.3.0.0_lib_linux  nvvfxbackgroundblur_v1.3.0.0_lib_linux  nvvfxgreenscreen_v1.3.0.0_models_linux_sm120
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
```

Resulting layout (`$MAXINE_VFX_ROOT`, 4.4 GB):

```
VideoFX/
  include/                 nvVideoEffects.h  nvCVImage.h  nvCVStatus.h
  lib/                     libVideoFX.so  libVideoFXLocal.so  libNVCVImage.so  libnvngxruntime.so  libVideoFXTriton.so (unused)
  lib/models/              AIGS_288x512_<SM>_m{0,1,2,3}.engine.trtpkg  and  *_1_4_8.engine.trtpkg   (8 files, 114 MB)
  features/nvvfxgreenscreen/lib/libnvVFXGreenScreen.so         (+ include/, license/)
  features/nvvfxbackgroundblur/lib/libnvVFXBackgroundBlur.so   (+ include/, license/)
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

**Why.** The model-package loader inside `libnvVFXGreenScreen.so` reads `/etc/os-release` and refuses distributions that are not on NVIDIA's list. Omarchy reports `ID=omarchy`, so `NvVFX_Load` fails with `-1209` ("no kernel image is available for execution on the device"). That message is misleading: no CUDA error occurs (verified with compute-sanitizer), the models do contain sm_120 code, and the failure is purely the distro check. Findings on what passes:

| os-release | Result |
|---|---|
| `ID=ubuntu` + `VERSION_ID="24.04"` | Load succeeds |
| `ID=debian` + `VERSION_ID="12"` | Load succeeds |
| `ID=rocky` + `VERSION_ID="9.4"` | Load succeeds |
| `ID=arch` (any `VERSION_ID`), or `ID_LIKE=ubuntu` | Fails (-1209). Only the `ID` / `VERSION_ID` pair is read |
| `ID=ubuntu` without `VERSION_ID` | No output at all in testing, probably a crash. Always give `VERSION_ID` |

**How.** Bind-mount a fake file over `/etc/os-release` inside a private user + mount namespace. Only the process you start sees it; the real file is untouched and no root is needed. The repository ships a working fake file at [../../tools/maxine-probe/fake-os-release](../../tools/maxine-probe/fake-os-release); install a copy where the daemon can find it:

```bash
mkdir -p ~/.local/share/oma-broadcast
cat > ~/.local/share/oma-broadcast/fake-os-release <<'EOF'
NAME="Ubuntu"
VERSION_ID="24.04"
ID=ubuntu
ID_LIKE=debian
VERSION_CODENAME=noble
PRETTY_NAME="Ubuntu 24.04 LTS"
EOF
```

### 7a. Per-process (tested)

```bash
unshare -Urm sh -c 'mount --bind ~/.local/share/oma-broadcast/fake-os-release /etc/os-release && exec <command> [args]'
```

Inside, the process runs as uid 0 *in the namespace* (still your real user on the host) and sees the fake distro. Use this form for the probe in step 9 and for manual runs of the daemon.

### 7b. systemd user unit (not yet validated)

The equivalent for `oma-broadcastd.service`, run as a user unit:

```ini
[Service]
BindReadOnlyPaths=%h/.local/share/oma-broadcast/fake-os-release:/etc/os-release
```

Unprivileged user units can only use mount-namespace options together with a user namespace (`PrivateUsers=` / `PrivateMounts=`); whether this directive works unmodified in a `systemd --user` unit, and how it interacts with the D-Bus session bus and `/dev/video*` access (uid mapping), is untested. If it does not work, the fallback is `ExecStart=unshare -Urm sh -c '...'` (form 7a), or an `LD_PRELOAD` shim intercepting `open`/`fopen` of `/etc/os-release` (also untested).

Licence note on this workaround: see section 11.

## 8. Environment

The SDK bundles CUDA 12.8, cuDNN 9, TensorRT 10 and NPP 12, while Arch ships CUDA 13. None of the `.12` / `.9` / `.10` sonames exist system-wide, so the bundled directories must be on `LD_LIBRARY_PATH`. The feature libraries are found by plain `dlopen("libnvVFXGreenScreen.so")`, so their directories must be on it too (otherwise `NvVFX_CreateEffect` returns `-2`).

Create an env file (kept outside the repo):

```bash
cat > "$PREFIX/env.sh" <<'EOF'
export MAXINE_VFX_ROOT="${MAXINE_VFX_ROOT:-$HOME/.local/opt/maxine/VideoFX}"
P="$MAXINE_VFX_ROOT"
export LD_LIBRARY_PATH="$P/lib:$P/features/nvvfxgreenscreen/lib:$P/features/nvvfxbackgroundblur/lib:$P/external/cuda/lib:$P/external/tensorrt/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
unset P
EOF
source "$PREFIX/env.sh"
```

- `MAXINE_VFX_ROOT` is the one variable the project reads: the SDK root (the directory containing `lib/`, `features/`, `external/`). The daemon derives the model directory (`$MAXINE_VFX_ROOT/lib/models`, passed to the SDK as `NVVFX_MODEL_DIRECTORY`) and the library paths from it, and the systemd unit will set the same variables in `Environment=`.
- Do not `source` this from `~/.bashrc` or `~/.profile`. The bundled CUDA 12 libraries would shadow system libraries for every program you start. Source it per shell, or set it per process/unit.

## 9. Verify with the probe

The probe ([`tools/maxine-probe/probe.c`](../../tools/maxine-probe/), see its README) loads AIGS, runs 200 frames at 1080p, then runs background blur. The CUDA headers it needs are bundled; no system toolkit is required. The model directory is argv[1]; if omitted it defaults to `./models`, which does not exist and gives `-13` ("file could not be found").

```bash
source "$PREFIX/env.sh"                # first, so the linker finds the bundled .12 libs
P=$MAXINE_VFX_ROOT
cd "$(mktemp -d)"
gcc -O2 -o probe <repo root>/tools/maxine-probe/probe.c \
  -I$P/include -I$P/external/cuda/include -L$P/lib -lVideoFX -lNVCVImage -lcuda
unshare -Urm sh -c "mount --bind ~/.local/share/oma-broadcast/fake-os-release /etc/os-release && ./probe $P/lib/models"
```

Expected output on the reference machine (RTX 5080 Laptop, driver 615.78; the numbers vary by GPU, the shape does not):

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

Observation, cause unconfirmed: on the very first probe run the first `NvVFX_Run` took seconds (plausibly a one-time warm-up of the TensorRT context, cuDNN and CUDA graph capture); later runs were much shorter, plausibly because of a CUDA JIT or file cache. Budget for a cold first run of a few seconds: the daemon is meant to show a black or "starting..." frame while an effect loads.

Without the override the same probe stops at `NvVFX_Load -> -1209`, which confirms the distro gate.

## 10. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `NvVFX_Load` returns `-1209`, "no kernel image is available", log line `Loading TRTModel failed in Aigs` | `/etc/os-release` is not an accepted distro (`ID=omarchy`). Not a real CUDA error | Step 7: run under `unshare -Urm` with the fake os-release. Check that the file has both `ID=` and `VERSION_ID=` |
| `NvVFX_CreateEffect` returns `-2` (unimplemented) | `libnvVFXGreenScreen.so` / `libnvVFXBackgroundBlur.so` not found by `dlopen` | Put `features/nvvfxgreenscreen/lib` and `features/nvvfxbackgroundblur/lib` on `LD_LIBRARY_PATH` (step 8); confirm the feature was extracted in step 6 |
| `NvVFX_Load` returns `-13` ("file could not be found") | Model directory wrong or models missing | Pass `$MAXINE_VFX_ROOT/lib/models` (probe argv[1]; the default `./models` does not exist). Check `ls $MAXINE_VFX_ROOT/lib/models` shows 8 `.trtpkg` files for your SM |
| `error while loading shared libraries: libcudart.so.12` / `libnpp*.so.12` / `libcudnn.so.9` / `libnvinfer.so.10` not found (or linker warnings about `.so.12`) | Bundled `external/cuda/lib` and `external/tensorrt/lib` not on `LD_LIBRARY_PATH` | `source "$PREFIX/env.sh"`, then check `ldd $MAXINE_VFX_ROOT/lib/libVideoFXLocal.so \| grep 'not found'` is empty |
| `Cannot load libVideoFXTriton.so` in the log | The Triton client library needs `libgrpcclient.so`; unused | Harmless, ignore |
| Model list shows only `_woa` / `windows` entries, or the downloaded SDK has no Linux libs | Wrong tag: `_woa` is Windows on ARM | Use `1.3.0.0_linux` (step 4) |
| `ngc registry ...` returns 401/402/403 or an empty list, "not authorized" | API key from another account or org than the Developer Program access, or "NGC Catalog" service not included, or access not yet granted | Redo step 1 with the same account that owns the key; regenerate the key with the NGC Catalog service (step 2); run `ngc config set` again with the right org; `ngc config current` shows what is active |
| No `models_linux_sm<SM>` for your GPU | GPU architecture not in the 1.3.0.0 matrix | List with step 5; Turing/Ampere/Ada/Hopper/Blackwell (sm75 to sm120) are covered |
| `NvVFX_Load` succeeds but a program hangs at start | Cold first run (see step 9 observation) | Wait a few seconds; the probe prints the time |
| `unshare: unshare failed: Operation not permitted` | Unprivileged user namespaces disabled | `sysctl kernel.unprivileged_userns_clone` (hardened kernels); enable it or use the `LD_PRELOAD` alternative |

## 11. Licence notes

Summary of the licence terms as read from the SDK licence documents. This is not legal advice; read the licence you accept.

- The SDK and models are under the NVIDIA Software License Agreement plus the AI Product-Specific Terms. The Developer Program path is "solely for internal evaluation, development or test non-production purposes ... not for use, distribution or deployment in production" (terms 1.2.8). Personal use on your own machine and development are fine; shipping the SDK inside a product needs an NVIDIA enterprise agreement.
- **No redistribution.** SLA 8.5 forbids copying, distributing or sublicensing any part of the Software, "including in any publicly accessible software repositories". Never commit the SDK, the models (`*.trtpkg`), the NGC key or `~/.ngc/` to this repo, never put them in a package or release artifact (users download from NGC themselves, the daemon only points at the install directory). Keep the install directory outside the working tree.
- No reverse engineering of the binaries (SLA 8.4); no publishing benchmark results without written permission (SLA 8.9). The timings above are for local verification, not for publication.
- The os-release override changes only what one process of yours reads on your own machine. It does not decrypt or bypass the model encryption, but SLA 8.7 forbids bypassing "encryption ... or authentication mechanism"; treat this as a grey area, keep it per-process, and do not ship it as a feature of a distributed package.
- NVIDIA documents the Linux SDK as server-side and does not officially support local, client-side use or Arch. That is a support statement, not a technical block. Expect no vendor support.

## 12. Uninstall

```bash
rm -rf "$PREFIX"                         # SDK, models and env.sh
rm -rf ~/.local/share/oma-broadcast/fake-os-release
rm -rf ~/Downloads/maxine                # downloads, if kept
# NGC CLI (only if nothing else uses it)
rm -rf ~/.local/opt/ngc-cli ~/.local/bin/ngc
rm -rf ~/.ngc                            # contains your API key; also revoke it at https://org.ngc.nvidia.com/setup/personal-keys
```

Nothing outside your home directory was written, so nothing else needs reverting.
