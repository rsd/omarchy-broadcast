# Sourced, not run: source tools/maxine-probe/env.sh
# shellcheck shell=bash
P=${MAXINE_VFX_ROOT:-$HOME/.local/opt/maxine/VideoFX}
export LD_LIBRARY_PATH=$P/lib:$P/features/nvvfxgreenscreen/lib:$P/features/nvvfxbackgroundblur/lib:$P/external/cuda/lib:$P/external/tensorrt/lib
