# NVIDIA Maxine on Linux: technical notes

Background notes on the NVIDIA Maxine SDKs relevant to this project: distribution and licence, versions, Blackwell support, features and API shape. They were compiled from NVIDIA's public documentation and catalog pages (checked in November 2026) and later checked by installing the VFX SDK; the install procedure is in [../setup/maxine.md](../setup/maxine.md). Statements marked **unverified** have not been confirmed first-hand. NVIDIA pages change; re-check before relying on version numbers.

## 1. Distribution and licence

### Where the SDKs live now
- **NGC catalog is the only binary source for Linux.** Core packages are `.tgz` (`VFXSDK_linux_<ver>.tgz`, `ARSDK_linux_<ver>.tgz`). They are extracted to `/usr/local`.
  - The core contains headers, libs and runtime deps, but NOT the per-feature libs and models.
  - Features are fetched afterwards with `install_feature.sh`, which needs an NGC personal API key in `NGC_CLI_API_KEY`.
  - Sources: https://docs.nvidia.com/maxine/vfx/latest/LinuxVFXSDK/InstalltheVFXSDK.html (docs footer 2021-2026); https://docs.nvidia.com/maxine/ar/latest/LinuxARSDK/InstalltheARSDK.html
- **VFX SDK Core** is `vfx_sdk_core`, latest `1.3.0.0_woa`, updated 2026-09-30, 637 MB. Platform builds are labelled `<ver>_linux` / `<ver>_windows`.
  - https://catalog.ngc.nvidia.com/orgs/nvidia/maxine/resources/vfx_sdk_core/-
  - The VFX collection page (updated 2026-09-10) says Windows on Arm is planned for October 2026.
  - https://catalog.ngc.nvidia.com/orgs/nvidia/maxine/collections/maxine_vfx_sdk/-?_lr=1
- **Older Linux-specific NGC listings are stale.** `maxine_linux_vfx_sdk_ga` shows 0.7.7.0_GA, updated 2025-06-26, behind an NVIDIA AI Enterprise (NVAIE) gate. https://catalog.ngc.nvidia.com/orgs/nvidia/teams/maxine/resources/maxine_linux_vfx_sdk_ga
- **Audio (AFX) Linux collection** was updated 2026-10-05. https://catalog.ngc.nvidia.com/orgs/nvidia/maxine/collections/maxine_linux_audio_effects_sdk_collection/-
- **AR Linux collection**, "Maxine Linux Augmented Reality SDK" (updated 2025-06-26, NVAIE-tagged). https://catalog.ngc.nvidia.com/orgs/nvidia/maxine/collections/maxine_linux_ar_sdk_collection_ga/-
- **GitHub** `github.com/NVIDIA-Maxine` (page fetched 2026-11): repos `Maxine-VFX-SDK`, `Maxine-AR-SDK`, `Maxine-AFX-SDK`, `VFX-SDK-Samples`, `AR-SDK-Samples`, `AFX-SDK-Samples`, `nvidia-vfx-python-samples`. These were last updated 2026-09/10.
  - The `Maxine-*-SDK` repos are MIT-licensed headers, proxy-loader source and Windows samples. Their READMEs are Windows-only and ship no binaries.
  - The old `github.com/NVIDIA/MAXINE-*-SDK` URLs return 404 (verified for AR and VFX). A forum thread of 2025-02-20 to 2025-05-17 reports the same, with AUR users having made mirrors (Codeberg / Software Heritage). https://forums.developer.nvidia.com/t/obs-streamfx-maxine-ar-afx-vfx-sdks-for-linux/324484
  - The Linux samples are in `VFX-SDK-Samples` and `AR-SDK-Samples`. They install deps and build with `build_samples.sh`, and the SDK itself still comes from NGC. https://github.com/NVIDIA-Maxine/VFX-SDK-Samples

### Account and access
- **NGC pages show two options:** "NVIDIA AI Enterprise" (sales form) or "NVIDIA Developer Program" ("Get Access", described as "free tools, support, and tech resources"). A plain free NGC account is explicitly not enough.
  - Seen on `vfx_sdk_core` (2026-09-30), the VFX collection (2026-09-10) and the AFX collection (2026-10-05). This is new compared with the 2025 pages, which only offered NVAIE.
  - The Developer Program path unlocks the Linux download (verified by installing; see [../setup/maxine.md](../setup/maxine.md)). The terms that apply are summarised below.
- **History of the gating:**
  - Forum users in Jan 2025 reported NGC returning 402 "Payment Required" and a redirect to a 90-day AI Enterprise evaluation request. https://forums.developer.nvidia.com/t/maxine-r14-release-notes-and-highlights/318615
  - The Maxine R14 notes (2025-01-02) say "Linux SDK features are included under an NVAIE license".
  - A separate "Early Access" Linux collection is under the "NVIDIA Software Evaluation License Agreement" (2024-12-06 PDF), "available by request only". https://catalog.ngc.nvidia.com/orgs/nvidia/maxine/collections/maxine_linux_vfx_sdk_collection_ea/-

### Licence terms
- **Current NGC licence:** the NVIDIA Software License Agreement plus the Product-Specific Terms for NVIDIA AI Products. Associated AI models fall under the NVIDIA Community Model License or the NVIDIA Open Model License, per model. (`vfx_sdk_core` page, 2026-09-30.)
- **The AI Product-Specific Terms** (last modified 2026-04-15) do not name Maxine. https://nvidia.com/en-us/agreements/enterprise-software/product-specific-terms-for-ai-products/
  - §1.1.2.2 allows sublicensing and distributing the Software as part of a "Customer Product", under §1.7 conditions that include use reporting.
  - §F (Developer Programs) says developer-program software is for internal evaluation, development or testing, is not for production, and may not be included in a Customer Product.
  - **Unverified:** whether the Developer Program route inherits §F for Maxine. Treat the SDK as non-production, evaluation-and-development software.
- **The Maxine-specific SDK licence** is v. 2021-04-01. https://developer.download.nvidia.com/licenses/Maxine_SDK_License_1Apr2021_updated.pdf (read via pdftotext). It may no longer be what the Linux NGC package ships; the current licence text was not read.
  - It grants install and use, and allows distributing the SDK "as incorporated in object code format into a software application".
  - The Maxine supplement says "any portion of the SDK other than the audio/video data samples" is distributable.
  - Conditions on distribution: the app must have "material additional functionality"; the SDK portions may only be accessed by that app; distribution terms must be consistent with the licence.
  - §2.5 says the SDK may not be used so that it becomes subject to an open-source licence (copyleft or "redistributable at no charge").
  - Branding: desktop apps that use the SDK must attribute NVIDIA per https://www.nvidia.com/maxine-sdk-guidelines.
  - No non-commercial / personal-use carve-out appears in it.
- **Conclusion on redistribution:** do NOT vendor the runtime or models in the open-source repo. Have each user obtain them from NGC themselves and point the daemon at the install dir (the pattern used by `nvafx_rs`, https://github.com/loganintech/nvafx_rs, and by Blucast). The MIT header repos can be referenced for bindgen, but an MIT daemon that ships NVIDIA binaries conflicts with the 2021 licence's §2.5 and the NVSLA terms.

## 2. Versions, distros, dependencies

- **VFX core is 1.3.0.0** (2026-09-30). The PyPI wrapper `nvidia-vfx` 0.2.0.0 (2026-09-30) says it bundles "VFX SDK 1.3.0.0"; 0.1.0.1 (2026-03-09) bundled 1.2.0.0. https://pypi.org/project/nvidia-vfx/
  - Docs are versioned 1.0.0 / 1.1.0 / 1.3.0 and the Linux page was updated 2026-09-22. Older release lines were 0.7.5 (Dec 2024, R14), 0.7.7 (2025).
  - AR SDK was v0.8.6 and AFX v1.6.0 in R14 (Dec 2024). **Unverified** are the current AR and AFX version numbers; NGC pages hide them.
- **Officially supported distros (VFX Linux docs)** (https://docs.nvidia.com/maxine/vfx/latest/LinuxVFXSDK/GetStartedonLinux.html): Ubuntu 20.04 / 22.04 / 24.04, Debian 11 / 12, Rocky 8 / 9, RHEL 8 / 9. **Arch is not supported.**
  - Linux driver minimum is 570.26.
  - The PyPI page lists the stricter 570.190+ / 580.82+ / 590.44+ branch minimums.
  - Driver 615.78 is newer than all of these.
- **Official stance:** "The Linux SDK is designed and optimized for server-side (datacenter or cloud) deployments", and NVIDIA "do[es] not officially support" client-side / local deployment (same page, copyright 2021-2026). Windows is the client-side SDK.
- **CUDA / TensorRT for VFX:**
  - The current Linux docs list none. The Oct 2024 / Dec 2024 R14 notes give TensorRT 10.4.0.26, CUDA 12.1.1, cuDNN 8.9.7 for Linux SDKs. https://forums.developer.nvidia.com/t/maxine-r14-release-notes-and-highlights/318615
  - The old 0.7.2 system guide says CUDA, TensorRT and cuDNN are NOT bundled and must be installed (CUDA 11.8, TRT 8.5.1.7, cuDNN 8.6.0.163). https://docs.nvidia.com/deeplearning/maxine/vfx-sdk-system-guide/
  - **VFX 1.3.0.0 bundles its runtime**, verified by installing it: CUDA 12.8, cuDNN 9.7.1 and TensorRT 10.9 under `external/`, with only `libcuda.so.1` taken from the driver.
  - **AFX is explicit:** it bundles its libs (CUDA 12.8.1, TensorRT 10.9.0.34, cuDNN 9.7.1, "All libraries that are required to use the SDK are in the package"). Driver 570.26+. https://docs.nvidia.com/maxine/afx/latest/LinuxAFXSDK/GetStartedOnLinux.html
- **No system CUDA toolkit is required for the SDK:** `NvVFX_CudaStreamCreate` is documented as a wrapper for `cudaStreamCreate()` "so you can avoid linking the CUDA Toolkit libraries", and the bundled runtime works without a system CUDA install (verified). https://docs.nvidia.com/maxine/vfx/latest/API/Reference/VideoEffectsFunctions.html
- **Arch Linux reports:** none found for VFX / AR / AFX (searches for Arch / Manjaro / NixOS / Fedora returned nothing relevant). In practice the SDK loads on Arch with one workaround for its distribution check ([../setup/maxine.md](../setup/maxine.md), step 7).
  - The only Arch-adjacent items are the AUR `obs-streamfx-unstable` mirrors of the old GitHub source (forum thread above) and `nvbroadcast-bin` (AUR page blocked by Anubis; see section 6).
  - Blucast avoids the distro problem by running the SDK in an Ubuntu 20.04 container with the NVIDIA Container Toolkit. NVIDIA's own docs have a "Run SDK in a Container" page that builds on Ubuntu 20.04. https://docs.nvidia.com/maxine/vfx/latest/LinuxVFXSDK/RunSDKinaContainer.html
  - A container is the safe fallback where the SDK does not load.

## 3. Blackwell

- **Docs claim it.** VFX Linux Get-Started lists "Turing, Volta, Ampere, Ada, Hopper, and Blackwell" (docs 1.3.0, updated 2026-09-22). AFX lists Turing / Ampere / Ada / Hopper / Blackwell. The Maxine-VFX-SDK and AR-SDK READMEs say "Turing, Ampere, Ada or Blackwell". `nvidia-vfx` 0.2.0.0 says Blackwell works for VSR and TrueHDR, and Frame Gen needs Ada or Blackwell.
- **First version with Blackwell: unverified.** R14 (Dec 2024, VFX 0.7.5) lists Volta through Hopper only on Linux and no Blackwell. A TouchDesigner release note (June 2025) says its Maxine upgrade added Blackwell (RTX 50) support, so the Windows VFX SDK had it by about mid-2025. https://derivative.ca/release/experimental-202530280/72310 The Linux Blackwell docs line appears in the 1.x line; NGC version histories and release notes were not visible.
- **Consumer-Blackwell risk:**
  - The AR Linux install page lists GPU arches for the model downloader as a40, l40, l4, a30, b200, a2, h100, a10, t4, b100, a16, a100, b40. These are server parts, with no GeForce / sm_120.
  - The `install_feature.sh -g` option takes a GPU architecture and defaults to auto-detect, and R14 says models are downloaded "per feature and per GPU".
  - Consumer Blackwell (sm_120) model packages do exist for VFX 1.3.0.0 on Linux (`sm75` to `sm120` are published for AIGS) and were verified on an RTX 5080 Laptop. Check availability for other GPUs with `ngc registry model list` ([../setup/maxine.md](../setup/maxine.md), step 5).
  - The VFX Linux performance page only benchmarks T4, A40, L40 and B40. https://docs.nvidia.com/maxine/vfx/latest/LinuxVFXSDK/PerformanceReference.html
  - No public report of a Linux RTX 50-series run was found before the verification above.

## 4. Features

### VFX (Linux docs, all filters listed in the common nav)
Docs list AI Green Screen (AIGS), Background Blur, Upscale, Webcam Denoise, Video Relighting, Video Super Resolution (VSR), Video Frame Generation, TrueHDR (https://docs.nvidia.com/maxine/vfx/latest/index.html). The Linux perf table covers all of them.
- **AIGS:** 4 modes (quality / performance, chairs fg or bg). Input BGR u8 interleaved GPU; output u8 alpha mask, same size. Minimum 512x288; 16:9 best. Uses a state variable for temporal consistency.
- **Background Blur:** composites the input with a blurred copy through a mask. BGR u8 in and out; mask u8; strength 0 to 1.
- **Webcam Denoise:** available. Upscale: RGBA u8 interleaved in and out, scales 4/3, 1.5, 2, 3, 4x, strength 0 to 1.
- **VSR:** streaming modes (mode 21 medium / 23 ultra) exist; also denoise / deblur quality levels.
- **Artifact Reduction:** in the older (0.7.x) NGC description, not in the 1.x nav.
- **Video Relighting:** Early Access only on Linux in Dec 2024 (R14). Current status **unverified**. It is documented as computationally heavy.
- **Frame Generation:** Ada / Blackwell only.
- **No eye contact, no auto-frame** in VFX.

### AR SDK on Linux
- Features (https://docs.nvidia.com/maxine/ar/latest/index.html): face detection and tracking, facial landmarks (68 / 126), 3D body pose, **Eye Contact (gaze redirection; feature `nvargazeredirection`)**, expression estimation, LipSync, Active Speaker Detection.
- The Linux install page names gaze redirection explicitly, so it is Linux-available.
- No dedicated auto-frame effect; build it from face tracking.
- The AR Linux install page lists only server GPUs (see section 3).

### AFX
- Noise removal / background noise suppression, room echo removal, noise plus echo removal, Audio super-resolution, Studio Voice, Speaker Focus. AEC is listed in the GitHub README.
- Linux docs: https://docs.nvidia.com/maxine/afx/latest/LinuxAFXSDK/GetStartedOnLinux.html
- Its "Operating system" line there still reads Ubuntu 18.04 / RHEL8 / CentOS8 / Debian 10+ (older wording).
- R14: Voice Font and Speaker Focus were Early Access.

### Windows-only vs Linux
- No effect was identified as Windows-only in the docs. Windows on Arm lacks LipSync and Active Speaker Detection.
- Linux has extra batch sample apps.
- Because the Linux SDK is server-oriented, the per-feature GPU model catalog may be narrower (see Blackwell risk).

## 5. API shape

All items below are confirmed against https://docs.nvidia.com/maxine/vfx/latest/API/Reference/VideoEffectsFunctions.html (function reference, header `nvVideoEffects.h`) unless noted.
- **C API confirmed.**
  - `NvVFX_CreateEffect(code, &handle)`
  - `NvVFX_SetImage(handle, NVVFX_INPUT_IMAGE | NVVFX_OUTPUT_IMAGE, NvCVImage*)`
  - `NvVFX_SetCudaStream(handle, NVVFX_CUDA_STREAM, CUstream)`, `NvVFX_CudaStreamCreate(&stream)`
  - `NvVFX_SetString(NVVFX_MODEL_DIRECTORY, ...)`, `NvVFX_SetU32` / `SetF32` for mode and strength
  - `NvVFX_Load`, `NvVFX_Run(handle, int async)`
  - `NvVFX_AllocateState` / `ResetState` / `DeallocateState` / `SetStateObjectHandleArray` (`NVVFX_STATE`)
  - Destroy.
- **GPU buffers and streams.** Docs: "the effect filters require GPU buffers" for performance. https://docs.nvidia.com/maxine/vfx/latest/API/Architecture/WorkwithImageFramesonGPUorCPUBuffers.html
  - `NvCVImage_Init(&img, w, h, pitch, ptr, fmt, type, layout, NVCV_GPU)` wraps an existing device pointer without owning it.
  - So a daemon can wrap its own CUDA allocations, including NV12 from NVDEC or its own kernels, with zero copies.
  - `NvCVImage_Transfer(src, dst, scale, stream, &stage)` does CPU<->GPU copies plus format and colour conversion on the given stream.
- **Formats.**
  - Documented examples: BGR / BGRA / RGB; u8 and f32; interleaved and planar.
  - NV12 is accepted by `NvCVImage` as `NVCV_YUV420` + `NVCV_NV12` and converted to BGRA by `NvCVImage_Transfer`. The docs example sets `colorSpace = NVCV_709 | NVCV_VIDEO_RANGE | NVCV_CHROMA_COSITED`.
  - The effects themselves take BGR u8 interleaved (AIGS, Blur) or RGBA u8 interleaved (Upscale). Denoise and VSR formats were not found: the 404 on the Webcam Denoise page left them **unverified**, though the NVIDIA samples historically use BGR f32 planar.
  - A webcam NV12 or MJPEG pipeline therefore needs NV12 -> BGR u8 on GPU, effect, then BGR -> NV12 for v4l2loopback. This is NvCVImage_Transfer or your own kernel.
- **Async and batch.**
  - `NvVFX_Run(h, 1)` is asynchronous on the effect's CUDA stream. Docs state "1: The filter runs asynchronously."
  - Batch size is set by `NVVFX_BATCH_SIZE` / `NVVFX_MODEL_BATCH`. The AIGS state variables are per stream.
  - Multi-GPU and batch docs sections exist; their contents were not read. Thread safety is not documented.
- **Python alternative.** `nvidia-vfx` Python bindings use DLPack GPU tensors (float32, `(3,H,W)` RGB in [0,1] for VideoSuperRes). They expose only VideoSuperRes, VideoFrameGeneration and TrueHDR (no AIGS / blur). The output buffer is owned by the SDK, so clone before the next `run()`. https://docs.nvidia.com/maxine/vfx-python/latest
  - The PyPI sdist for 0.2.0.0 is 3.7 kB, a stub; the actual wheel host and the licence/auth gating for it are **unverified**.

## 6. Existing bindings and projects

- **Rust:** no maintained Rust crate for the VFX or AR SDK was found (**unverified** that none exists).
  - `loganintech/nvafx_rs` is bindgen-based and covers only Audio Effects, with 4 commits, 2 stars and no licence file. https://github.com/loganintech/nvafx_rs
  - The Maxine API is plain C with a proxy loader, so bindgen over `nvVideoEffects.h`, `nvCVImage.h` and `nvCVStatus.h` plus `dlopen` is the natural route.
- **StreamFX (OBS)** compiled against the Maxine VFX / AR / AFX source and enables NVIDIA features only on Windows, per the forum thread of 2025-05-17.
- **Blucast** (https://github.com/Andrei9383/Blucast, MIT, Python/PySide6, HN Show post 2026-02-23).
  - Uses the Maxine VideoFX SDK v0.7.2.0 inside a container; README build deps are CUDA 11.8, cuDNN 8.6, TensorRT 8.5.1.7.
  - Says it needs an AI Enterprise subscription and the end user downloads the SDK from the NVIDIA catalog; the SDK is not redistributed.
  - No performance numbers; RTX 50 support not mentioned. Podman / Docker, v4l2loopback and the NVIDIA Container Toolkit are required.
- **nvbroadcast** (`Hkshoonya/nvidia-broadcast-linux`, GPL-3.0, AUR `nvbroadcast-bin`) does NOT use Maxine.
  - It uses ONNX Runtime / TensorRT with RVM, IS-Net and BiRefNet, MediaPipe, a fused CUDA compositor and DeepFilterNet3. "NVIDIA Maxine SDK integration" is only an unchecked wish.
  - Its self-reported 1080p numbers: about 20 to 44 ms per frame depending on mode; RVM about 29 ms at 720p on an RTX 5060; a TRT engine build "can take several minutes" on first use. https://github.com/Hkshoonya/nvidia-broadcast-linux
  - It is evidence that non-Maxine paths run on RTX 50 under Linux. Its AUR page was blocked by Anubis, so the AUR details were not read.

## NVIDIA's performance data

NVIDIA publishes per-effect timings for the VFX SDK on its Performance Reference page: https://docs.nvidia.com/maxine/vfx/latest/LinuxVFXSDK/PerformanceReference.html. The figures are not reproduced here.
