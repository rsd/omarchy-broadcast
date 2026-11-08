# ADR-0013: Hardening the green-screen mask with a CUDA kernel shipped as PTX

- **Status:** Accepted. Amends [ADR-0012](0012-video-effects-and-maxine-backend.md) (its statement that no custom CUDA kernel is needed). Adds no setting, configuration key or D-Bus member.

## Context
- The mask that the Maxine `GreenScreen` effect writes is a soft confidence matte, not a cut-out: its largest value is 254, and it has wide soft bands around the subject because the model works at 512x288 and the mask is upscaled to the frame size.
- Both effects used that mask unchanged, so the room showed through. Measured on recorded 1080p frames from two cameras, the raw mask held 11,557 and 15,747 pixels above zero on the background side of the subject's outline, which `replace` and `blur` rendered as a 1 to 4 px fringe of the original room. Because the mask never reaches 255, `replace` also let about 0.4 to 2 % of the background through the whole subject.
- Hiding the room is what `replace` and `blur` promise, so this is a defect, not a matter of taste.
- The SDK's image API (`nvCVImage.h`) offers transfer, composite, flip and sharpen operations. It has none that thresholds, subtracts or applies a table to a single-channel mask.
- The CUDA driver is already reached through a hand-written function table opened with `dlopen` in `obc-gpu` (no crate links CUDA, [ADR-0012](0012-video-effects-and-maxine-backend.md)). The repository had no kernels.
- The driver floor is 570.26 and the GPU floor is compute capability 7.5 ([../setup/maxine.md](../setup/maxine.md)). Whatever is added must not raise either.

## Decision

### The curve
- The mask goes through a fixed 256-entry table before either effect reads it. The table is a linear ramp: a raw value of 128 or less becomes 0, 224 or more becomes 255, and values in between rise linearly (`lut[129]` = 3, `lut[176]` = 128, `lut[223]` = 252).
- **128 is the bottom** because it is the model's decision boundary (confidence 0.5), and it removes every background-side pixel in the recorded frames.
- **224 is the top** because the mask is about 246 to 251 on average a few pixels inside the 128 contour, with a 10th percentile of 226 to 247; a top of 224 keeps the subject's interior opaque (a top of 240 would not). It also leaves about 70 % fewer partly mixed edge pixels (alpha between 10 % and 90 %) than the raw mask, costs 1 to 2 px of edge, and is the middle ground on edge flicker.
- The values are fixed. They describe the model's output, not a matter of taste. A setting would let a user reopen the leak by going below 128, and every setting multiplies the hardware test states. The table is built in Rust, so changing it later is a Rust-only change that does not touch the kernel.

### Both effects, a separate buffer
- `replace` composites through the hardened mask and `blur` passes the hardened mask to `BackgroundBlur` as its mask input. The raw mask is never used by an effect.
- The kernel writes into its own buffer instead of the mask in place. It is not known whether the SDK reads its own output again as temporal state, and the raw mask must stay readable (the backend exposes both, for tests).

### The kernel, shipped as PTX
- The kernel is one pass over the mask, `dst = lut[src]` on a pitched 8-bit plane, with no host synchronisation: it is launched on the element's stream right after the green-screen run.
- Its PTX is committed as text (`crates/obc-gpu/src/kernels/lut8.ptx`, with the CUDA C source `lut8.cu` beside it), embedded in `obc-gpu` and loaded at run time with `cuModuleLoadData` through the driver table `obc-gpu` already opens. The build needs no nvcc, no NVRTC and no new crate.
- The driver compiles the PTX to the running GPU's machine code when the module is loaded (JIT). That happens inside the black warm-up of a cold load ([ADR-0012](0012-video-effects-and-maxine-backend.md)), not on the first visible frame.

### The ISA pin
- The PTX is pinned to `.version 7.0` and `.target sm_75`. A newer nvcc writes its own, newer `.version`; a driver older than that version refuses the module, so committing a newer nvcc's output unchanged would silently raise the driver floor. Driver 570.26 accepts PTX up to ISA 8.7. The kernel uses only instructions that ISA 7.0 has, and `sm_75` (Turing) is the oldest GPU the SDK supports, so both floors stay where they were.
- A unit test of `obc-gpu` checks the `.version` and `.target` lines, the single NUL-terminated string, and the seven-parameter entry point. An ignored test runs `ptxas`.

### Failing closed
- A kernel that cannot be set up (the module does not load or the function is missing) fails the load with `BackendError::Load`. A failed launch fails the frame with `BackendError::Run`. An asynchronous fault surfaces at the element's final stream synchronisation, which fails the buffer, so no frame is pushed. All of these take the existing failure path: the output goes black and the effect state goes to Failed, as `effect.on_failure` says (`"black"` by default).
- Nothing falls back to the raw mask. The fallback would show the room the user asked to hide, the reason [ADR-0012](0012-video-effects-and-maxine-backend.md) gives for failing closed.
- Load errors name the kernel. Two CUDA codes get an explanation: 222 (the driver is older than this build supports; it needs 570.26 or newer) and 221 (`libnvidia-ptxjitcompiler.so.1` is missing from the driver installation).

### Regenerating the PTX
The build never runs nvcc. After changing `lut8.cu`, with the CUDA toolkit on the path, in `crates/obc-gpu/src/kernels`:

```sh
nvcc -arch=compute_75 -ptx -o lut8.ptx lut8.cu
sed -i 's/^\.version .*/.version 7.0/' lut8.ptx
ptxas -arch=sm_75  -o /dev/null lut8.ptx
ptxas -arch=sm_120 -o /dev/null lut8.ptx
```

Both `ptxas` runs must succeed (`cargo test -p obc-gpu -- --ignored ptxas` runs them).

## Consequences
- The room no longer shows around the subject: in the recorded frames the background-side pixels above zero drop to 0 in `replace` and `blur`, and the subject's interior is fully opaque.
- The partly mixed edge band is narrower (about 0.3 of its raw width in pixels on the recorded frames), and the outline sits 1 to 2 px further in than with the raw mask. Thin structures near the outline, such as hair and fingers, lose a little of their soft edge.
- Edge flicker is not removed. A hard step at the ramp's ends turns some small mask changes into larger alpha changes at the edge; the measured count of pixels changing by a large step per frame is higher than with the raw mask on a still subject and is dominated by subject movement otherwise.
- `obc-gpu` is the first crate with device code. The PTX is a reviewed text file; changing the kernel needs a toolchain that most contributors do not need.
- Nothing changes for users: no setting, no configuration, no D-Bus change, and the driver and GPU floors are the same.
- A cold load JIT-compiles the kernel, inside the warm-up that already shows black. The JIT needs the driver's `libnvidia-ptxjitcompiler.so.1`, which ships with the driver packages.
- The model still classifies what it classifies: a pile of blankets can be foreground. Hardening changes the edge, not the classification.

## Alternatives considered
- **Chaining SDK operations to build the curve:** relies on undocumented saturation when a transfer goes from float back to 8 bits, needs an inversion the API does not offer for single-channel images, and costs four to six full-frame passes with float intermediates. The best result is the same piecewise-linear curve the kernel computes in one pass.
- **A CPU round trip:** breaks the all-GPU rule of [ADR-0012](0012-video-effects-and-maxine-backend.md) and needs a stream synchronisation in the middle of every frame.
- **Accepting and documenting the fringe:** the effect would not do what it is for.
- **NVRTC:** another library to `dlopen` with a versioned soname search, compiles on every cold load, and the PTX version follows the installed toolkit, which defeats the pin.
- **The `cudarc` crate:** a second loader of the driver library, and its context model keeps the primary context alive, against [ADR-0006](0006-on-demand-gpu-lifecycle.md) and [ADR-0012](0012-video-effects-and-maxine-backend.md).
- **The `cust` crate:** links CUDA at build time.
- **A committed cubin or fatbin:** a binary blob, specific to an architecture, that needs nvcc to produce and cannot be reviewed.
- **A hard threshold at 128:** the edge boils; in recorded frames it flips full 0-to-255 steps on a few thousand pixels per frame on one camera.
- **Smoothstep between 128 and 224:** about 45 % more flicker than the linear ramp on one camera, for little gain.
- **Erosion or dilation:** the 128 contour already sits about 1 px inside the true edge, so remapping alone removes the room. Erosion adds a neighbourhood pass and eats hair and fingers; dilation moves the wrong way for privacy.
- **A user setting:** see the curve section above.

## What would change this decision
- A driver at 570.26 or newer that rejects the pinned PTX (error 222 or 221 on a supported distribution): ship a fatbin built at release time, or use NVRTC.
- Blur looking visibly worse with the hardened mask: harden `replace` only.
- On another camera, the room showing at raw values of 128 or more outside the true edge: raise the bottom (for example to 144) or add a 1 px erosion.
- Visible shimmer on a still subject with a clean camera: widen the top to 240.
- The SDK gaining a remap or threshold operation: use it and drop the kernel.
