# ADR-0033: A system-memory video pipeline for machines without an NVIDIA GPU, chosen at run time

- **Status:** Accepted (2026-11-12), implemented in daemon 0.40.0 (`crates/obc-gpu`, `crates/obc-core`, `crates/obc-backend`, `crates/gst-obc`, `crates/obc-module-host`, `crates/obc-module-onnx`, `crates/obc-pipeline`, `crates/omarchy-broadcastd`). No D-Bus member is added: the interface stays at revision 15. Builds on [ADR-0009](0009-ingest-process-output.md) (ingest, process and output joined by a slot), [ADR-0012](0012-video-effects-and-maxine-backend.md) (the effect element and its privacy rules), [ADR-0025](0025-loadable-modules.md) (frame kinds, providers by rank and pin), [ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md) (the retry without effects, frame generation) and [ADR-0031](0031-onnx-cutout-module.md) (a cut-out that runs on host frames). Lifts the known limitation of ADR-0031, and amends [ADR-0017](0017-cuda-checkpoint-parking.md) (the checks that may run while parked, and no parking in system memory). Measurements: [../benchmarks/system-memory.md](../benchmarks/system-memory.md).

## Context
- Most machines have no NVIDIA RTX GPU. Until now the daemon could not run a camera session on one of them at all: the ingest ended in `cudaupload`, the output began with `cudadownload`, and GStreamer's `nvcodec` plugin does not load without a usable CUDA device (the registry still lists its elements, and making one fails). A session therefore ended in `error`, and the virtual camera showed only the writer's black placeholder.
- The pipeline was already built around two memories. The ingest, the output and the session had a system-memory variant used by the tests, the module host already ran host chains, the ONNX module already processed host NV12 ([ADR-0031](0031-onnx-cutout-module.md)), and the loopback writer, the PipeWire tap and the preview have always received system memory. What was missing was the decision of which memory a session uses, the effect element on host frames, a host variant of the backend seam, stage availability by memory, and the daemon's wiring.
- Choosing CUDA wrongly has a cost on both sides. A daemon that wakes a runtime-suspended GPU defeats [ADR-0006](0006-on-demand-gpu-lifecycle.md); a daemon that makes a CUDA call while parked blocks until the next unpark ([ADR-0017](0017-cuda-checkpoint-parking.md)). The decision therefore has to be made without a CUDA call.

## Decision
**A camera session keeps its frames in GPU memory (CUDA) or in system memory, chosen by `[video] memory`. In system memory the pipeline, the effect element and the module adapters make no CUDA call at all.**

### The setting
```toml
[video]
memory = "auto"   # "auto" | "cuda" | "system"
```
- `cuda`: the pipeline as before. A start that cannot build it fails, and never falls back.
- `system`: system memory, also on an NVIDIA machine (to keep the GPU asleep, or to try the CPU path). The daemon's pipeline makes no CUDA call and the modules are told CUDA is off (below).
- `auto` (default): `cuda` when the probe below says CUDA is usable, else `system`. On a machine with a working NVIDIA driver this is the behaviour of earlier releases.
- `video.memory` is an editable enum row of the config schema in a new group, "Video processing", `apply` `reload`. Its description is computed at run time and says which memory is in force and why ("Now: system memory, no NVIDIA driver is loaded. Applies at the next camera start.").

### The probe: no CUDA context, no GPU wake
`obc_gpu::presence::probe` decides from what the system shows, in order, the first failure giving the reason:

| Step | Check | Reason when it fails |
|---|---|---|
| 1 | `CUDA_VISIBLE_DEVICES` is set and empty, `-1` or `NoDevFiles` | CUDA_VISIBLE_DEVICES hides every GPU |
| 2 | `/proc/driver/nvidia/gpus/` exists and has an entry (the listing only; nothing inside is read) | no NVIDIA driver is loaded / the NVIDIA driver reports no GPU |
| 3 | `libcuda.so.1` opens with `dlopen` | the CUDA driver library is not installed |
| 4 | GStreamer's registry lists `cudaupload`, `cudadownload` and `cudaconvertscale` | GStreamer's CUDA elements (gst-plugins-bad, nvcodec) are not installed |

- **Why it is safe.** Listing procfs touches no device. A `dlopen` of the driver library runs its initialisers and opens no device node (verified: no new descriptor and no `/dev/nvidia` mapping); `cuInit` is what opens the devices and registers with `nvidia-uvm` ([ADR-0017](0017-cuda-checkpoint-parking.md); `obc_gpu::checkpoint` opens the library the same way). The registry lookup (`ElementFactory::find`) reads the cache and loads no plugin. The probe never calls `cuInit`, any other driver function, NVML, or `ElementFactory::make` of a CUDA element (making one loads `nvcodec`, whose initialisation calls `cuInit`). It is therefore allowed at any time, parked or not.
- **What it cannot see** is a driver that is present but whose `cuInit` fails: a library and kernel module of different versions, a GPU in a bad state, `CUDA_VISIBLE_DEVICES` naming a GPU that does not exist. That is learnt when the CUDA output starts (next section).
- **When it runs.** At daemon start, before the modules open; on `SIGHUP` and `ReloadConfig`; and at each camera start, before the plan. A running session keeps the memory it started with. Hotplug of a GPU is not followed.
- **Reporting.** One info line at each change (`video memory: system (auto: no NVIDIA driver is loaded)`, `video memory: cuda (auto)`, `video memory: system (set in the configuration)`), the description of the `video.memory` row, and the stage reasons below. `Warning` is not used for the memory: running on the CPU is a normal state, not a fault.

### A CUDA start that fails for want of CUDA: `auto` retries once in system memory
When the probe passes but the CUDA output cannot start, `auto` (and only `auto`) tries again at once in system memory, once, and the system memory stays in force until the next reload. A warning is logged with the CUDA error, and the row's description says why. The failure counts as "for want of CUDA" when the output build finds a CUDA element missing, a state change fails with an error posted by a `cuda*` or `nv*` element, or the output opened no CUDA context. It does not when the output contract, a module or the effect element is at fault, nor when the GPU could not be **unparked**: an unpark failure keeps the error of [ADR-0017](0017-cuda-checkpoint-parking.md), because the process is then parked and must touch nothing that might call CUDA. `cuda` never falls back, so a user who asked for the GPU is not given a CPU run without knowing. The retry comes before the start without effects ([ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md)), which would want CUDA as much.

**No park after a start that failed for want of CUDA.** The first hardware run of this fallback crashed the daemon: the failed start left the process with no CUDA context, `Parker::starting` parked it all the same, and the driver's checkpoint lock crashed inside `libcuda` (with `CUDA_VISIBLE_DEVICES` naming no GPU). Such a start holds nothing to release, so it makes no driver call.

### Changes apply at the next camera start; no D-Bus member
A change of `video.memory`, or of what the probe finds at a reload, applies at the next camera start, as the module choices do ([ADR-0025](0025-loadable-modules.md)); the running session keeps its memory. Clients learn the effective memory from the log, the computed description of the `video.memory` row, and the reasons on the stages. No D-Bus member or CLI field is added ([../dbus-api.md](../dbus-api.md) is unchanged); the config schema gains a row and a group, which clients that draw the schema generically show with no change.

### Caps and adapters in system memory
The canonical frame is `video/x-raw, format=NV12` at the processing size with the colour label of the contract, as in CUDA memory with `memory:CUDAMemory`; every element after the ingest sees the same caps in either memory. The ingest tail is `[videocrop !] videoconvertscale ! capsfilter(canonical)`, with no upload and no CUDA context.

| Path | Camera format | In system memory |
|---|---|---|
| GPU-direct | NV12, other raw formats | `v4l2src ! [videocrop !] videoconvertscale` (a passthrough when the mode already has the canonical size and label) |
| CPU convert | YUY2, UYVY | the same: one pass converts the format, the size and the label, so the separate `videoconvert` is not used (it would convert twice) |
| CPU decode | MJPEG | `v4l2src ! jpegdec ! I420 ! [videobalance ! capssetter (limited contract)] ! [videocrop !] videoconvertscale` (see the range fix below) |

- The enhance stage (video super resolution) takes CUDA frames: the plan never asks for it in system memory, and the ingest builder refuses it as a guard. `videoconvertscale` keeps one thread: four threads cut the wall time of a scaling pass by about a third but raised its CPU time by 30 to 50 %, and mode selection already prefers a camera mode at the contract size, where the pass is a cheap conversion.
- **The output** has no `cudadownload`. A `videoconvert` to I420 is added only when the contract is I420 (about 0.3 ms per 1080p frame); then the existing capsfilter with the canonical label, the `capssetter` relabel to BT.601 for the loopback (labels only), the contract capsfilter and the `appsink` that forwards to the loopback writer. The writer, the PipeWire tap and the preview are unchanged. The placeholder frame is black made on the CPU.

### The effect element on host frames
`obceffect` accepts both memories on its pads (`video/x-raw(memory:CUDAMemory)` and `video/x-raw`, NV12). What a frame gets is decided by one piece of code for both: the privacy rules of [ADR-0012](0012-video-effects-and-maxine-backend.md), [ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md) and the later amendments (black or the fallback image at a start, on a new source and on a failure, the camera kept while an effect is turned on mid-stream, never the raw picture under an effect, the repeat cache) are written once, and only the pixel work differs: `CudaPixels` on the element's stream, `HostPixels` on the CPU.
- On host frames black is two row-by-row fills of the mapped output (Y to the black of the caps' range, UV to 128), the decoded fallback image and the input are row-by-row copies, and the output buffers come from a system-memory video pool made by the first frame the element writes (passthrough allocates nothing). No CUDA object exists: no context, no stream, and GStreamer's CUDA library is not opened.
- The three pixel operations are our own code, not stock elements, because each is conditional per frame on the element's privacy decision, which no stock element can follow. Stock elements are used wherever one exists (`videoconvertscale`, `videoconvert`, `capssetter`, `videobalance`).
- The worker loads the backend as soon as the caps are known, with no context. A backend instance is for one memory: one created for the other (new caps) is unloaded and dropped before a load.
- **Even sizes only.** The fills and copies refuse an odd width or height, so the element fails the stream rather than send a partly written frame. The canonical sizes of the pipeline are always even, so this never happens in the daemon; it fails closed.

### The seam and the module adapters
- **`obc-backend`.** `BackendFactory::kinds()` lists the memories a factory's instances take (CUDA alone by default), and `InferenceBackend` gains `load_host`, `process_host` and `reset_temporal_host`, whose defaults return `BackendError::Unavailable("<name> takes CUDA frames only")`. Every call on an instance loaded with `load_host` is made with no CUDA context current, and the backend must not create or make current one. The Maxine backend implements none of the three and is untouched. This is internal Rust API, not the module ABI.
- **`obc-module-host`.** `ModuleBackend::load_host` issues the chain request with frames of kind `OBC_FRAME_HOST` and null CUDA handles, and `process_host` passes host frames. `ModuleBackendFactory::kinds` is the `frames` list of the module's `background` declaration. The adapters refuse a call of the other kind on a loaded chain.
- **No change to the module ABI.** `OBC_FRAME_HOST` and null CUDA handles already existed; the ABI version does not change.

### Stage availability by frame memory
The registry knows the memory of the session (`Registry::set_memory`). In system memory a stage whose manifest `frames` does not list `host` is **unavailable, with a reason**, exactly as a stage whose probe said it cannot run, so the choice of provider, the pins, the profile, the requirements, the module statuses, the settings tree and the output mode list all follow with no new path:
- under `auto` with no usable CUDA: "Needs an NVIDIA GPU (CUDA)";
- under `system` set in the configuration: "Needs CUDA; the video pipeline is set to system memory".

Because this is availability and not a filter after the choice, a machine with both Maxine and ONNX resolves `background` to ONNX in system memory, and a pin to Maxine falls back with "Pinned module br.com.dias.maxine-vfx is unavailable: Needs an NVIDIA GPU (CUDA)". A module none of whose stages can run is unavailable as a whole. The Maxine modules stay open and probed in system memory (their probes read files only); only their stages are unavailable. A module that lists both kinds runs with the one the session has. In CUDA memory nothing is filtered.

### How a module learns that CUDA is off
Every module's configuration carries the reserved key `_host.cuda = false` when the video pipeline runs in system memory (absent otherwise, meaning CUDA is allowed), beside `_host.compute_capability`; a change of memory changes the configuration, which re-opens the module as any configuration change does ([../modules.md](../modules.md#module-configuration)). The ONNX module's `auto` then skips TensorRT and CUDA with the reason "CUDA is off: the video pipeline runs in system memory", an explicit `cuda` or `tensorrt` fails the load with it, and the module refuses a CUDA-frame chain while CUDA is off. The chain kind alone cannot carry this: host chains are also used on the CUDA path. Without the key the ONNX module's `auto` picks TensorRT on a machine with a visible GPU and creates the device's primary context, which would wake the GPU that `system` was chosen to keep asleep, or block while parked.

### CUDA-only features in system memory
Each is unavailable with the reason above, greyed in the settings tree, and never requested:

| Feature | In system memory |
|---|---|
| Background blur and replace through Maxine | Unavailable; the ONNX module provides `background` when installed. With no provider that takes host frames the usual rule applies: the effect fails and `on_failure` decides what shows, with a `Warning` saying why |
| Video noise removal, relighting, the HDR look, enhance (video super resolution), eye contact, auto-framing | Unavailable (CUDA frames) |
| Frame generation | Unavailable, so the 2x output modes labelled "(generated)" are not offered. A configured 2x rate stays in force ([ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md) keeps the configured value) and repeats frames, with the existing `Warning` |
| The ONNX cut-out | Available on host frames; engines limited by `_host.cuda` |
| The example module (`frames = ["host"]`) and the audio modules | Available and unaffected |

### Lifecycle and parking
A session in system memory makes no CUDA call, so it is neither unparked nor parked: it starts while the process is parked, leaves the GPU as it found it, never taints the parker (nothing of it could call CUDA), and the daemon never reads the compute capability. A CUDA session that a system session replaces is stopped and parked first, as at any stop. An unclean stop of a system session is warned about and does not disable parking. The checks that may run while parked ([ADR-0017](0017-cuda-checkpoint-parking.md)) gain the presence probe.

### The privacy rules are unchanged
The rules of the effect element are memory-independent code, as above: the raw camera image is never shown while an effect is requested and `on_failure` hides it; black at a start, on a new source and on a failure is exactly Y=16, UV=128 (Y=0 for a full-range label); `image` shows the fallback image; turning blur on mid-stream keeps the picture until the matte is ready and then switches, with no black frame between; a module disabled or crashed mid-session gives black from the next frame; the cut-out never fails open; the writer's placeholder and the crash cover are unchanged. With only the Maxine module installed in system memory, a requested background effect fails and shows black (or the image) with a `Warning`, never the camera, and under `none` it shows the camera as chosen.

### The MJPEG range fix
A system-memory ingest of MJPEG must turn the decoder's full-range picture into the limited range of the contract, or blacks sit at Y=0 and whites at 255 under a label that says 16 and 235, and applications that ignore the range flag clip them. On the CUDA path `cudaconvertscale` does this in the same pass as the scale.

- **What was found.** GStreamer 1.28.8's `videoconvert` and `videoconvertscale` do **not** convert the range when only the range differs. The converter (`video-converter.c`, both the chain construction and the fast-path lookup) compares only the matrix and the primaries. `jpegdec`'s output is full-range BT.601 (`1:4:0:0`) and the canonical label of a limited contract is `2:4:5:1`; both have matrix 4, so no conversion runs and Y=0 stays 0. Checked with full-range black labelled `1:4:0:0`, `1:4:5:1` and `1:4:5:4` through `videoconvertscale`, `videoconvert` and a two-hop path: Y=0 every time. Upstream's development branch adds a `same_ranges` check, so a later GStreamer will convert.
- **The fix**, in the system-memory MJPEG adapter and for a limited contract only: `jpegdec ! I420 ! videobalance contrast=0.858824 brightness=0.053887 saturation=0.878431 ! capssetter caps="video/x-raw,colorimetry=(string)2:4:0:0"`. The constants are 219/255 (the luma gain), 16·219/255² (the luma offset) and 224/255 (the chroma gain, about 128). They map luma 0 to 16 and 255 to 235 and chroma to 16 to 240 with neutral at 128, which a test checks on the decoded levels.
- **Why the relabel.** After the balance the pixels are limited range, so the frames must say so: `capssetter` relabels them `2:4:0:0` (`jpegdec`'s matrix, limited range). On GStreamer 1.28.8 the converter then sees the same matrix and passes them on. On a GStreamer that has the upstream range check it sees the same range as the canonical caps and also passes them on. Without the relabel, such a GStreamer would see full-range frames, convert them a second time, and clip.
- **Cost.** +0.55 ms per frame at 1080p (+0.25 ms at 720p), against +2.1 ms for a hop through RGB.

## Consequences
- A machine with no NVIDIA GPU, or with `CUDA_VISIBLE_DEVICES` empty, runs the camera, the ONNX cut-out (blur, replace) and every host stage. On the reference machine (Intel Core Ultra 9 275HX, ONNX Runtime 1.31.1 CPU build, the MediaPipe model, 4 threads) a blurred 1080p30 session costs the daemon about 42 % of one core with an MJPEG camera and about 29 % with an NV12 camera, and 25 % at 720p30; the effect takes about 6 ms per processed frame; the virtual camera held 30 fps with no dropped frames. Details and method: [../benchmarks/system-memory.md](../benchmarks/system-memory.md). A 4-core low-power machine is expected to be two to three times slower per frame; that has not been measured.
- With CUDA hidden, the daemon's process maps no CUDA library and holds no `/dev/nvidia*` descriptor. With CUDA visible and `memory = "system"`, it holds none during or after a blur session, and the GPU is absent from `/proc/driver/nvidia-uvm/gpus` for it. An actual runtime suspend was not observed ([ADR-0017](0017-cuda-checkpoint-parking.md) explains why that needs a quiet desktop).
- The effect element and the output have two implementations of their pixel work, so a change to either must keep both. The decision logic is shared and tested in both memories; the CUDA suites (`gpu_`) must run after a change to the shared code, because the CUDA path was moved into the shared structure.
- Modules gain one reserved configuration key, which a module that makes CUDA calls of its own must honour.
- A probe that passes for a machine whose CUDA does not work costs one failed start, then runs on the CPU until the next reload.

### Known limitations
- **A raw camera that sends full range but labels it limited is carried as labelled, on both paths.** Measured on a Logitech MX Brio: its YUYV and NV12 modes report limited range, and the frames span 0 to 255 (3 % below 16 and 8 % above 235 in YUYV). Both the CUDA and the system path trust the label, so the output carries full-range values under limited caps. The laptop FHD camera's YUYV is limited as it says, and both cameras' MJPEG is full range as `jpegdec` labels it, and is converted. The remedy is a per-camera data row (see the quirks as data in [../architecture.md](../architecture.md#camera-settings)), not built.
- **A limited-range raw camera into a full-range contract is not converted** in system memory (the same converter limit). The daemon's contract is always limited range, and the loopback refuses a full-range one, so this is reachable only with `obc-bench`.
- **A rare black frame after an output gap of more than two periods.** The writer sends its black placeholder when no frame arrives for two contract periods (66.7 ms at 30 fps). With a camera that delivered about 20 fps in dim light, two black frames in about 5,300 appeared under blur, each after a gap of 70 to 74 ms in the output, and none in 60 s without an effect or with the other camera. The effect's own time at those frames was at most 20 ms, so the stall is elsewhere in the output thread; the cause is not isolated. It is privacy-safe (black, never the room) and visible as a flash.
- **The background settings group is shown when only CUDA modules provide `background` in system memory.** The stage is unavailable with its reason, so selecting blur then shows black or the fallback image, with a `Warning`.
- **The ONNX engines other than the CPU were not run in system memory** (OpenVINO is not CUDA and is expected to work).
- **The CPU figures are those of one high-end laptop CPU.**

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **A flag in the module ABI** (a chain-request bit, ABI 1.3) to say CUDA is off | It would be per chain rather than per module, but needs an ABI bump, a regenerated header and an SDK change for one bit, when the reserved `_host` key already carries host facts and changes nothing in the ABI |
| **Setting `CUDA_VISIBLE_DEVICES=""` in the daemon when the choice is `system`** | It covers every library at once, but `setenv` in a running multi-threaded process is unsound, so it could only be done before any thread starts and the choice could change only with a daemon restart; it also hides CUDA from what does not need hiding (NVML statistics) |
| **A D-Bus property for the effective memory** | An API change (a revision, `obc-ipc`, the CLI, the plugin) for what the log, the computed row description and the stage reasons already say. [ADR-0017](0017-cuda-checkpoint-parking.md) declined the same for the parked state. It can be added if a panel needs it outside the configuration dialog |
| **Restarting a running session at once when `video.memory` changes** | The camera would close and open again, with a short black gap, for a change nobody is waiting on; the module choices already apply at the next camera start |
| **An RGB hop for the range** (`jpegdec` to BGRx and back to NV12, so that the converter converts the range) | Exact, but +2.1 ms per 1080p frame against +0.55 ms for `videobalance`, and it needs a second conversion |
| **Trusting `videoconvertscale` to convert the range** | It does not in GStreamer 1.28.8 (above); a test of the levels fails |
| **`auto` that never falls back after a CUDA start fails** | The probe cannot run `cuInit`, so a mismatched driver would leave the machine with no camera, which is the case the feature exists for |
| **A fallback for `cuda`** | A user who asked for the GPU would run on the CPU without knowing |
| **Replacing the CUDA context argument of every `InferenceBackend` method with a memory enum** | Cleaner, but it touches the Maxine backend, which needs the SDK and a GPU to verify, for no change of behaviour there. The additive host methods leave it alone |
| **Parking after every failed start, as before** | It crashed inside `libcuda` when CUDA had never initialised |
| **`auto` choosing system memory on an NVIDIA machine when the session needs nothing of CUDA** (so that the GPU sleeps) | Not decided here. On an NVIDIA machine the CUDA path stays the default; `memory = "system"` is the explicit way |

## What would change this decision
- A GStreamer whose converter checks the range: the `videobalance` adapter would become redundant, though the relabel keeps it correct meanwhile. A test of the levels says when.
- A per-camera range row that corrects raw cameras mislabelled as limited, on both paths, would end that limitation.
- A panel that needs the effective memory outside the configuration dialog: a D-Bus property.
- A cause found for the rare black frame: the placeholder rule or the output thread would change.
- An engine of the ONNX module that uses an accelerator with its own memory: the seam's `kinds` would grow.
