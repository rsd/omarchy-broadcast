# ADR-0028: The Maxine AR module: eye contact and auto-framing as one loadable module

- **Status:** Accepted (2026-11-11), implemented as the crate `crates/obc-module-maxine-ar` (module id `br.com.dias.maxine-ar`, versioned with the workspace). Builds on [ADR-0025](0025-loadable-modules.md) (a module is a manifest and a library behind a versioned C ABI; the stage slots and the order rules; a module owns its SDK root) and [ADR-0023](0023-settings-schema.md) (the module describes its own settings). Keeps [ADR-0003](0003-maxine-first-behind-backend-seam.md): the SDK is never vendored. Author guide: [../modules.md](../modules.md). Setup: [../setup/maxine.md](../setup/maxine.md#the-maxine-ar-module).

## Context
- The NVIDIA Maxine AR SDK is a separate download from the Video Effects SDK that the Maxine module ([ADR-0025](0025-loadable-modules.md)) uses. It has its own root, its own package (`maxine-ar-sdk-local`, built by the user with the omarchy-nvidia-sdks repository), its own version and its own model files. Its features that matter here are face box detection, 126-point landmark detection and gaze redirection.
- Two effects were wanted from it: **eye contact** (the eyes redirected towards the camera) and **auto-framing** (a digital pan and zoom that keeps the face in frame).
- Both need the face. Gaze redirection finds and tracks the largest face inside its own feature; auto-framing needs the same face to move its crop.
- The module interface at this point has no stage that changes the size or the region of the frame, and the host's ingest scales the camera picture to the output size before the stages run. A framing stage therefore works on a frame that is already at the output size.
- A stage that cannot run must never take the camera down. Eye contact and framing hide nothing the user wants hidden, unlike the cut-out ([../modules.md](../modules.md#failure-semantics)).
- The AR SDK's libraries read `/etc/os-release` as the Video Effects SDK's do, so the same os-release preload shim applies ([../setup/maxine.md](../setup/maxine.md#3-enable-the-drop-in)).
- Both SDKs may be installed on one machine, and the daemon may load both in one process (the Maxine module's stages and these). Both ship a `libNVCVImage` of the same soname.

## Decision
**One module, `br.com.dias.maxine-ar`, with two stages, `eye-contact` and `auto-frame`, both in the `pre-matte` slot, that fail open. The module has its own SDK loading and its own root, and shares nothing with the Maxine module but the process.**

### One module for both stages
- The two stages share one SDK root, one set of loaded libraries and the face the SDK tracked on the frame: when `eye-contact` runs first on a frame, `auto-frame` takes its face and runs no detector of its own. They remain independent for availability and failure: each has its own probe (eye contact needs gaze redirection, face detection and 126-point landmark models for the GPU; auto-framing needs face box detection only), its own switch and its own failure.
- A manifest `maxine-ar.toml` with the module id, the library `libobc_module_maxine_ar.so`, and two `[[stage]]` tables (`pre-matte`, positions 10 and 20, CUDA frames, NV12), installed like the Maxine module's: by the Arch package, with `maxine-ar-sdk-local` as an optional dependency, and by `scripts/install-user.sh`.
- The library has no NVIDIA code and opens the SDK at run time, so it builds, installs and loads on a machine without the SDK or the GPU. The probe reads the SDK tree only (no `dlopen`, no CUDA), so it never wakes a suspended GPU; a stage the SDK or the models for this GPU cannot serve is greyed with a reason in plain words.

### Its own SDK loading, not the Maxine module's
- The module carries its own FFI to the AR SDK and its own preload of the SDK's bundled libraries by absolute path (the approach of the Video Effects backend, copied rather than shared).
- The AR SDK has its own root and version, set by `MAXINE_AR_ROOT`, then `sdk_root` in `[module."br.com.dias.maxine-ar"]`, then `/opt/nvidia/maxine/arsdk`. The Video Effects backend pins its root once per process ([ADR-0025](0025-loadable-modules.md)); that pin is the VFX SDK's and says nothing about this one, and the two roots can be changed independently.
- A module that depends on the other would have to be installed with it, and could not be installed alone.

### Placement and order: before the cut-out, eye contact first
- Both stages are `pre-matte` ([ADR-0025](0025-loadable-modules.md), where module stages run), each in the module's own element before the cut-out module's element. They therefore run before noise removal, the cut-out and relighting.
- Eye contact runs on the full-resolution face, before any other stage has changed it. Auto-framing runs before the cut-out so that the cut-out, and the blur or replacement behind it, see the framed picture rather than a picture that is reframed afterwards, with a background of the wrong scale.
- The default positions put eye contact first (10) and auto-framing second (20): the redirected eyes are part of the picture that is then cropped. The module runs its stages in the order the host gives, so the user may reorder them within `pre-matte`; when auto-framing runs first, eye contact runs on the framed picture. This is the host's contract for every module, and the module does not impose an order of its own.

### Settings
- `effect.eye_contact.enabled` ("Eye contact", off by default) and `effect.eye_contact.look_away` ("Natural look-away", off by default, applies on reload); `effect.auto_frame.enabled` ("Auto-framing", off by default) and `effect.auto_frame.framing` (Tight, Medium or Wide; applies live).
- **No strength control for eye contact.** The SDK has none, and a control the SDK does not honour would only be a number that does nothing.
- **The SDK's sensitivity thresholds stay at the SDK's defaults** (the gaze and head-pose limits, the eye-size sensitivity, the closure detection and the look-away timing). They have no meaning a person can reason about without reading the SDK's documentation. The one exception is the randomised look-away, which has a plain-words effect (the eyes sometimes leave the camera, as a person's do) and which is applied by a new load.
- Eye contact works on the largest face only (the SDK tracks one) and leaves the picture unchanged when there is no face.
- Auto-framing keeps the face centred, with a crop that always has the frame's aspect and is scaled back to the frame's size; the zoom is capped at 1.5. A dead zone and hysteresis keep a head that sways from moving the picture, the crop eases towards its goal by the time between frames (the path does not depend on the frame rate), it holds for about two seconds when the face is lost and then eases back to the full frame, and it snaps to the full frame when the camera changes.
- *Amendment (daemon 0.36.0): the three framings are set by the share of the crop's height that the face's box fills: Tight 0.65, Medium 0.52 and Wide 0.45 (they were 0.42, 0.30 and 0.22). The zoom is the share times the frame's height over the face's height, limited to between 1 and 1.5, and the dead zone means a resting crop moves only above a zoom of about 1.087. At desk distance a face box is 35 to 45 % of the frame's height, and with the earlier shares Medium and Wide never reached the dead zone there, so those two settings left the picture unchanged. The settled zoom (Tight, Medium, Wide) is now 1.5, 1.49 and 1.29 at a face of 35 %; 1.5, 1.3 and 1.125 at 40 %; and 1.44, 1.16 and 1.0 at 45 %. Wide stays at the full frame for a face above about 41 % of the height, so Wide zooms only when the person sits farther away. Measured with the real SDK on a rendered head still, Tight at a face of 32 % of the height went from a zoom of 1.30 to 1.49, and at a face of 52 % from 1.0 (no change) to 1.24. The cap of 1.5, the dead zone and the smoothing are unchanged.*

### Auto-framing is a digital zoom inside the module, now
- The stage crops the frame it is given and scales the crop back to the same size with NPP, on the host's stream. It needs no change to the module interface.
- **Trade-off, stated plainly:** the zoom is applied after the ingest has scaled the camera picture to the output size, so a zoomed picture has less detail than the camera captured. A crop at capture resolution, or the camera's own pan and zoom, would keep the detail; both need a change in the host (the ingest would have to take the crop from the module) and are future work.
- A repeated frame (the output clock's repeat of the last picture) does not move the crop: it repeats the result of the last fresh frame.
- *Amendment (daemon 0.41.0, ABI 1.3, D-Bus revision 16): the stage reports its zoom through the module interface's status values ([ADR-0025](0025-loadable-modules.md)): `("auto-frame", "zoom", z)`, the frame's height divided by the crop's, so `1.00` is the full frame and the value is at most the zoom cap. It is read from the crop as it is now, not from the goal it eases towards, so a panel's read-out follows the picture, and it is reported while the stage is loaded, so it returns to `1.00` as the crop eases back to the full frame when the face is gone. A repeated frame does not move it, for the reason above: the crop moves on fresh frames only. The reason for the read-out is that the setting (Tight, Medium or Wide) is a goal, and the zoom a person gets depends on how far the face is; the user can now see what the stage is doing. The daemon publishes it as `StageValues` and the bar shows it as a read-only line in the Auto-frame group. The module needs no other change: it still reports nothing when the stage is not loaded.*

### Both stages fail open
- An error at load or on a frame drops that stage alone, with a `Warning`, and the feed keeps streaming; the host runs the frame again without the stage. The model is the noise-removal and relighting stages of [ADR-0018](0018-video-noise-removal.md) and [ADR-0024](0024-stackable-stages-and-effects-in-the-settings-schema.md). With the stage off, or the picture unchanged (no face, or the crop at the full frame), the frame is left as it is.

### Two SDKs in one process
- Both SDKs ship a `libNVCVImage` with the same soname, and both bundle the same major versions of CUDA and TensorRT. Whichever is opened first answers a later request for that name.
- The module opens each library of its own SDK by its absolute path, with the libraries it depends on first, so the later lookups by name find the objects already loaded. Loading both modules in one process was tested in both orders (the Video Effects module first, the AR module first) with effects of both running, on an NVIDIA GPU; the tests are in [../testing.md](../testing.md#maxine-ar-module-tests-ignored).
- This is a tested configuration, not a guarantee of the SDKs: a later release of either SDK that changes the shared library's interface could break it, and the hardware tests are what finds that.

## Consequences
- A user with the AR SDK gets two new switches in the settings tree, in the bar and in `omarchy-broadcast settings`, with no change to the clients. Without the SDK or the models for the GPU they are greyed, with the reason.
- The AR SDK's `libNVCVImage` exports the GCC 8 `libstdc++` it links statically. The module opens the system `libstdc++.so.6` globally before the SDK's libraries, so that one runtime serves the process. Without it, `NvAR_Load` of the face-box model could abort with `free(): invalid size` when the system runtime was loaded first, for example by GStreamer's CUDA plugins.
- The shim of the Video Effects drop-in is process-wide, so it serves both SDKs. A machine with only the AR SDK needs the AR package's own shim and environment, and the shipped drop-in names the Video Effects package's files ([../setup/maxine.md](../setup/maxine.md#the-maxine-ar-module)).
- Auto-framing costs detail at high zoom (above). Eye contact changes the eyes only, and only for one face.
- The NVIDIA licence applies as for the other module: the SDK is the user's own download, is never distributed here, and no figures are published.
- Two copies of the loading code exist (the Video Effects backend's and this module's). They are small and describe two SDK trees; a change to one is not automatically a change to the other.
- Known limitations:
  - One face: with several people in view, the SDK follows the largest.
  - Moving the AR root needs a daemon restart, as for the Video Effects root (the SDK is loaded once per process, from the first root used).

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| A separate module for eye contact and one for auto-framing | Both need the face, and the SDK tracks it per feature: the face tracking would be duplicated, once in each. One module shares the face and the one SDK root, and the two stages are still switched on and failed separately |
| Reusing `obc-backend-maxine` for the AR SDK | A different SDK with its own root, version and models. The Video Effects root pin is process-wide for that SDK and would have to be generalised for a second one; the module would also depend on a crate that is built for the other SDK's features. A copy of the small loading code keeps the two independent, at the price of two copies |
| Deferring auto-framing until the module interface can crop at capture resolution | That change is in the host, and it would hold eye contact's release, with no use for it. An in-module zoom works with the interface as it is, and the detail it costs is stated; a better crop replaces it later without a change for the user |
| Cropping in the ingest now | It needs the host to ask the module for the crop before the camera picture is scaled: a new part of the module interface, with the ordering and failure rules that come with it. Sequenced after this module |
| Using the camera's own pan and zoom (PTZ) | Specific to the cameras that have it, and each has its own controls and limits ([ADR-0020](0020-camera-image-controls.md)). A digital zoom works for every camera |
| Exposing every SDK threshold (gaze limits, head-pose limits, eye-size sensitivity, closure detection) as settings | None of them has a meaning in plain words, and a wrong value gives worse results with no clue why. The SDK's defaults are the tested ones; the look-away timing is left alone for the same reason |
| A strength control for eye contact | The SDK has none, and a control it does not honour would only be a number that does nothing |
| Failing closed, as the cut-out does | Neither effect hides anything. A feed that stops because an optional effect cannot run is worse than the same feed without it |

## What would change this decision
- A module interface that lets a stage ask the ingest for a crop at capture resolution: auto-framing would move to it and keep its smoothing and its settings.
- A release of either SDK that changes the shared `libNVCVImage`, or of the AR SDK that adds a strength control for gaze redirection (a setting would then be possible) or a different feature set.
- A module that has to share more than the face between stages: a shared service for the face could then be a part of the module interface.
