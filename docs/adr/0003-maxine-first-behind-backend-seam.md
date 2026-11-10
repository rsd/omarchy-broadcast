# ADR-0003: NVIDIA Maxine first, behind an InferenceBackend seam

- **Status:** Accepted

## Context
- Maxine (Video Effects SDK: AI green screen, background blur) was preferred over TensorRT with open models for the first effects backend.
- Maxine's Linux SDK is server-oriented, and Arch is not a supported distribution.
- Downloading needs an NGC API key through the NVIDIA Developer Program.
- The feasibility checks (download, models for consumer Blackwell GPUs, library loading on an unsupported distribution) are recorded in [../setup/maxine.md](../setup/maxine.md) and [../research/maxine-linux.md](../research/maxine-linux.md).

## Decision
- Effect elements call an `InferenceBackend` trait.
- The first implementation is `obc-backend-maxine`:
  - bindgen over the MIT-licensed headers;
  - `dlopen` of the user-installed SDK;
  - GPU-resident `NvCVImage` wrapping our CUDA buffers;
  - `NvVFX_Run(async=1)` on our CUDA stream.
- The Maxine runtime and models are never vendored: the SDK licence forbids redistribution.

## Consequences
- A TensorRT/ONNX backend can be added later without changing the pipeline or the elements.
- Users must install Maxine themselves, which makes packaging harder. That is acceptable while no release is planned.
- On an unsupported distribution the SDK's distribution check must be worked around per process ([../setup/maxine.md](../setup/maxine.md), section 3). The fallback is an Ubuntu container.

## Alternatives considered
- TensorRT with open models (RVM, BiRefNet and similar): no vendor licence constraint, but more integration work and no vendor-tuned models. Kept as the second backend behind the same seam.
