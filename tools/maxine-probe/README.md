# maxine-probe

A small standalone C program that checks a Maxine Video Effects SDK installation on a machine, independent of the rest of the workspace. It is a development aid, not part of the build.

## What it checks

1. `NvVFX_GetVersion` reports the SDK version (expected `0x01030000` for 1.3.0.0).
2. A CUDA device and stream can be created (primary context shared with the SDK's runtime).
3. The AI green screen (AIGS) effect loads its model package (`NvVFX_Load`), then runs 200 frames at 1920x1080 BGR u8 on the GPU. Load time, first-run time and average time per frame are printed. Two image-allocation paths are tried: SDK-allocated GPU images, and images wrapping application-owned device memory.
4. The background blur effect loads and runs with an external mask.

A run passes when the version is correct, `NvVFX_Load -> 0` is printed, the output ends with `DONE` and there is no `FAIL` line.

## Files

| File | Purpose |
|---|---|
| `probe.c` | The probe |
| `env.sh` | Sets `LD_LIBRARY_PATH` for the SDK's bundled libraries; honours `$MAXINE_VFX_ROOT` (default `~/.local/opt/maxine/VideoFX`) |
| `fake-os-release` | An `os-release` file naming a supported distribution, used for the distribution-check workaround described in [docs/setup/maxine.md](../../docs/setup/maxine.md) |

## Build and run

The SDK must be installed first ([docs/setup/maxine.md](../../docs/setup/maxine.md)). The CUDA headers are bundled with the SDK, so no system CUDA toolkit is needed.

```sh
export MAXINE_VFX_ROOT=~/.local/opt/maxine/VideoFX
source tools/maxine-probe/env.sh
P=$MAXINE_VFX_ROOT
gcc -O2 -o /tmp/probe tools/maxine-probe/probe.c \
  -I$P/include -I$P/external/cuda/include -L$P/lib -lVideoFX -lNVCVImage -lcuda
# On a distribution the SDK does not recognise, run with the os-release shim
# (make -C tools/os-release-shim); set LD_PRELOAD on the probe itself:
env LD_PRELOAD=$PWD/target/os-release-shim/os-release-shim.so \
    OMA_BROADCAST_OS_RELEASE=$PWD/tools/maxine-probe/fake-os-release \
    /tmp/probe $P/lib/models
```

Arguments: `probe [model_dir] [aigs_mode]`. `model_dir` defaults to `./models`, which usually does not exist; pass `$MAXINE_VFX_ROOT/lib/models`. `aigs_mode` defaults to 0.

## Environment variables

| Variable | Effect |
|---|---|
| `MAXINE_VFX_ROOT` | SDK root; used by `env.sh` |
| `VFX_LOG` | If set, enables the SDK's own logging on stderr |
