# ADR-0030: The HDR look: NVIDIA TrueHDR as a stage, tone-mapped back to the 8-bit output

- **Status:** Accepted (2026-11-11), implemented in daemon 0.34.0. Amends [ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md) ("TrueHDR stays not implemented") and [ADR-0024](0024-stackable-stages-and-effects-in-the-settings-schema.md) (the rejection of TrueHDR under "Not implemented"): TrueHDR is implemented, as a stage that gives every 8-bit output a look, and not as an HDR output. Builds on [ADR-0025](0025-loadable-modules.md) (a stage of a module), [ADR-0012](0012-video-effects-and-maxine-backend.md) (the effect element runs the cut-out provider's stages) and [ADR-0016](0016-pipewire-output.md) (the PipeWire node). A real 10-bit HDR output (phase b below) is not built and is a preliminary no-go. Reference: [../modules.md](../modules.md#the-maxine-module-stages), [../dbus-api.md](../dbus-api.md#global-settings-the-video-effects), [../setup/maxine.md](../setup/maxine.md#6-using-the-effects).

## Context
- [ADR-0024](0024-stackable-stages-and-effects-in-the-settings-schema.md) rejected NVIDIA's TrueHDR because the output contract is 8-bit SDR NV12 and no call application consumes HDR10. [ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md) tried a round trip back to SDR offline and judged the change of look too subtle to justify a stage, compared with a simple tone curve, and left a 10-bit or HDR output for applications such as OBS as the condition for looking again.
- TrueHDR converts an SDR picture to HDR10: the PQ transfer (SMPTE ST 2084), BT.2020 primaries, 10 bits per channel. It expands highlights and shadows and the colour; it is a look, and the look is real. The question that remained was whether the look can reach the people who use the virtual camera at all.
- Two routes were open. (a) Tone-map the HDR10 image back to SDR inside the stage, so that every existing 8-bit output (the v4l2loopback camera, the PipeWire node, the preview) carries the look. (b) Publish a real 10-bit HDR output, which needs a consumer that reads one.
- The NVIDIA effect has a debander option, on by default in the SDK. An earlier measurement on the reference system blamed it for a one-frame lag and a black first frame. The cause was a race in our code: TrueHDR's work was not ordered with the frame stream. With the race fixed, the output matches the current frame with the debander on or off, and the first frame is not black.
- The module system already carries stages in slots, with a switch each, that fail open and alone ([ADR-0025](0025-loadable-modules.md), [ADR-0024](0024-stackable-stages-and-effects-in-the-settings-schema.md)). Frame generation ([ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md)) already runs after every other stage.

## Decision
**(a) is implemented: the Maxine module gains a stage `hdr`, "the HDR look", that runs TrueHDR and tone-maps the result back to SDR on the GPU, so every 8-bit output has the look. (b), a 10-bit HDR output, is not built.**

### The stage
- Stage id `hdr`, provided by the Maxine module (`br.com.dias.maxine-vfx`), slot `post-composite` and the last position before `frame-generation`, frames and formats CUDA NV12 in and out. One setting, `effect.video.hdr` ("HDR look", a switch, off by default), in the module's `video` group. It is available when NVIDIA's `nvvfxtruehdr` feature is installed (the feature has no model files) and otherwise greyed "Not installed", as every Maxine stage is.
- It runs on the finished picture, so the cut-out, the blur or the replacement, relighting and noise removal have already happened and the look covers the composite. The daemon runs it inside the effect element with the other stages of the cut-out provider ([../architecture.md](../architecture.md#modules)); frame generation, when the output rate asks for it, runs after it.
- Per frame, all on the GPU and on the stream's own CUDA context: NV12 to RGBA; TrueHDR (SDR to HDR10, 10 bits, PQ, BT.2020); a tone map of ours back to SDR; RGBA to NV12.
- **The tone map** (`obc-gpu`, a CUDA kernel with a Rust reference that the kernel is tested against): PQ decode to nits; a scaling that returns the source's SDR white to near SDR white, so that what remains of the HDR pass is its highlights, shadows and colour and not an exposure shift; BT.2020 to BT.709 (a colour outside BT.709 is clipped at zero); the ITU-R BT.2390 EETF on the luminance, from the source peak down to a 100-nit SDR target, the channels scaled by the ratio so that the chromaticity is kept; a clip that scales a pixel down as a whole, so that no channel clips alone; the re-encode.
- It runs on every frame it receives. It holds no output for a repeat: the effect element already sends its last processed output on a repeat ([ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md)), so the stage sees a repeat only when that cache is not valid, and then it runs.
- **It fails open, and alone.** A failure of `hdr` drops only that stage; the matted composite goes out. That the cut-out never fails open ([ADR-0012](0012-video-effects-and-maxine-backend.md)) is unchanged, and the stage cannot touch it: the look is applied after the cut-out and hides nothing.

### The debander stays at its default
The effect's debander is left at the SDK default, on; the stage does not set `DebandingOff`. The earlier lag and black first frame were not the debander's doing. They came from a stream-ordering race in our code: TrueHDR's work was not ordered with the frame stream. The fix runs TrueHDR on its own blocking stream and waits for it before the tone map. With the race fixed, the output matches the current frame with the debander on or off, and the first frame is not black (the GPU tests `gpu_truehdr*` and `gpu_the_hdr_look*` check this). The debander stays on because it limits banding on smooth gradients from 8-bit sources (a wall, a sky behind the person), where an 8-bit source has few steps and the expansion to a wider range can make them visible. The look is documented for users ([../setup/maxine.md](../setup/maxine.md#6-using-the-effects)).

### One TrueHDR stage for both outputs
The stage is built so that a later 10-bit output would not run TrueHDR a second time. It keeps the 10-bit image TrueHDR wrote on the GPU, in memory it owns, after the tone map has read it. A later side output would hand that image on:
- the **main output** stays 8-bit and carries the look (this decision);
- a **side output**, an optional addition of module ABI 1.3 that a stage declares and the host routes to an output that can carry 10 bits, would carry the 10-bit image. It is not built, not in the ABI, and not reserved in it: a later minor version is where it would be added, as an optional entry, so that an older module is unaffected. *(Amendment, daemon 0.41.0: ABI 1.3 went to the status values of [ADR-0025](0025-loadable-modules.md), so the side output would now be ABI 1.4 or later.)*

One TrueHDR pass per frame would thus feed both.

### (b) A real 10-bit HDR PipeWire node: a preliminary no-go
Phase (b) is a PipeWire `Video/Source` in P010 with BT.2020 and PQ colour keys, existing only while the look is on. A prototype on the reference system found:
- **Our process can publish it.** A native PipeWire producer (the pipewire-rs path of [ADR-0016](0016-pipewire-output.md)) publishes a P010 node with the colour keys. GStreamer's `pipewiresink` drops the colour keys, so it is not a way to do it.
- **No consumer on the reference system shows HDR.** OBS Studio 32.2.2's PipeWire camera source has no P010 support and ignores the transfer function. Browsers and call applications take 8-bit only. A node that no consumer reads as HDR would be a second camera in the picker that shows a wrong picture.
- The finding is **preliminary**: it is not yet confirmed in OBS Studio. Nothing of (b) is built.

**Conditions to revisit:** OBS (or another consumer) gains P010 or PQ support in its PipeWire source; or this project owns a recording path that consumes the 10-bit image itself.

## Consequences
- The look reaches every consumer today: the v4l2loopback camera, the SDR PipeWire node and the preview. Frame generation runs after it, so the interpolated frames are of the looked picture.
- The look is a taste, not a correction. It is off by default, and with it off nothing is loaded and nothing is paid. Where it is on it adds GPU work per processed frame and the feature's memory; figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence.
- A tone map back to SDR cannot show an HDR display's range. It gives an 8-bit picture whose highlights, shadows and colour were shaped by an HDR pass. [ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md) found the change subtle in an offline test. The decision here is that it is worth a stage a user may turn on, not that it is large.
- The setting is global, in the `video` group, with an `effect.video.hdr` id that clients that draw the tree show without change. No D-Bus member is added; the interface revision is not raised.
- The stage is in the effect element's order after `relight`. A module that provides `hdr` as a second provider would take the same slot; the pin row appears with no change, as for any stage.
- Known limit: banding on smooth gradients from 8-bit sources is mitigated by the debander, which is on, and not ruled out (see "The debander stays at its default").

## Known limitation: frames run much faster than a camera delivers them
Run back to back, much faster than a camera delivers them, NVIDIA's TrueHDR sometimes gives a frame an output that depends on the frames before it. Synchronising the CUDA context does not remove it, so the behaviour is inside the library and not in the stage's stream ordering. At a camera's cadence, up to 60 fps, every frame's output was exact in testing. The output can be configured above 60 fps (up to 240 fps); whether the HDR stage then receives frames close enough together to show the effect is untested, and it would appear as faint temporal blending or flicker. The daemon's own GPU test paces its frames at one 60 fps frame for this reason (`HDR_FRAME_INTERVAL` in `crates/obc-module-maxine/tests/hardware.rs`).

**Decision:** no restriction for now. The HDR look is not disabled or rate-limited at any output rate; this is a documented known limitation, tested up to 60 fps. It is revisited if the effect is seen at a higher output rate.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **An SDR look made by a simple tone curve** (what [ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md) compared against) | A curve on RGB has no model of the picture. TrueHDR's expansion depends on the content, and its result is what the user asked for by name. The curve is not the NVIDIA effect and gives a different, less controllable look |
| **Tone mapping in the output lane** (the writer or the PipeWire tap converts the 10-bit image) | The 10-bit image would have to travel to the output, and the output contract is 8-bit. It would put colour science in the writer, which is vendor neutral, and the preview, the loopback and the node would each need the same conversion or differ. Doing it in the stage gives one place and one picture |
| **Two TrueHDR stages**, one for the 8-bit look and one for a 10-bit output | The effect would run twice per frame on the same picture. One stage that keeps its 10-bit image feeds both (see above) |
| **A patched v4l2loopback** that carries 10-bit formats | Stock v4l2loopback cannot enumerate 10-bit formats, and the browsers and call applications that read the camera take 8-bit only. A patched kernel module is a packaging and maintenance burden for no consumer, and it is not stock |
| **The debander off** | It was only a workaround for a lag and a black first frame that a stream-ordering race in our code caused, and the race is fixed. With it off, banding on smooth gradients from 8-bit sources is left unmitigated. The debander on is the decision |
| **Publishing the 10-bit PipeWire node now** | See (b): no consumer on the reference system reads it as HDR. A second camera that shows a wrong picture is worse than none |
| **TrueHDR as a daemon feature outside the module system** | The same reasons as for frame generation ([ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md)): it would add a second path for a GPU feature that the ABI, the crash ledger, the failure rules and the pin and profile choice already handle |

## Future work
- **A later ABI minor (1.4 or later, since 1.3 carries the status values), an optional side output.** A stage declares a second output (the 10-bit image, a format and a colour description); the host routes it to an output that takes it and ignores it otherwise. Additive, as ABI 1.1 to 1.3 were.
- **Phase (b), a 10-bit HDR PipeWire node** (P010, BT.2020, PQ colour keys, present only while the look is on), if the conditions above are met.
- A test of a P010 node in OBS Studio, to turn the preliminary no-go into a decision.

## What would change this decision
- OBS or another consumer reading P010 and PQ from a PipeWire camera: phase (b) would be built, on the native producer, with the side output above.
- A recording path of our own that consumes the 10-bit image.
- A banding that users report on common cameras, with the debander on: a dither in the tone map's output would be looked at.
