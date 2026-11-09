# ADR-0018: Video noise removal as a stage that stacks with the effects

- **Status:** Accepted (2026-11-09). Amends [ADR-0012](0012-video-effects-and-maxine-backend.md): effects can now be combined with a stage, and the fail-closed rule of that ADR does not apply to the stage. Extends the D-Bus API of [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) within interface version 1, as revision 4.

## Context
- A webcam in a dim room produces visible grain. NVIDIA Broadcast offers "Video Noise Removal" for it; the Maxine Video Effects SDK 1.3 provides the same model as the feature `nvvfxdenoising` (effect selector `Denoising`).
- The Maxine chain of [ADR-0012](0012-video-effects-and-maxine-backend.md) already converts each NV12 frame to a BGR image that the green screen, the blur and the composite read, and converts the result back.
- The SDK's strength for this feature is binary: 0 favours texture, 1 favours noise removal, and the value selects one of two models. It accepts an 8-bit BGR chunky image only if the image is tightly pitched, and frames from 80x80 up to 1920x1080. It keeps a temporal state in a GPU buffer that the SDK sizes.
- Noise removal does not hide anything. Unlike the effects, a frame that skipped it is still the image the user is entitled to see, only grainier.
- The output clock marks a skip with `DISCONT`, so that flag cannot also announce a new source ([ADR-0010](0010-phase-locked-output-clock.md), [ADR-0015](0015-cadence-emission-for-sub-rate-inputs.md)).
- The daemon parks its CUDA state at Idle ([ADR-0017](0017-cuda-checkpoint-parking.md)), so anything the stage allocates must be released before the context is.
- Performance figures for the Maxine backend are not published, per the NVIDIA Maxine SDK licence; reproduce locally with `obc-bench` (see [../testing.md](../testing.md)).

## Decision
**Noise removal is a boolean stage of the existing Maxine chain that runs before the effect, or alone with `none`. It is not a fourth effect.**

### A stage, not an effect
- `Effect` stays `none`, `blur` or `replace`, mutually exclusive. A new setting, `denoise` (boolean, default off), adds the stage under any of them.
- Off, the path is the one of [ADR-0012](0012-video-effects-and-maxine-backend.md), unchanged: the conversion writes straight into the frame image, and no pass, copy or allocation is added.
- On, the conversion writes into a separate input image, the denoising feature writes the cleaned frame into the frame image, and the green screen, blur and composite run as before on that image. With `none` and noise removal, there is no mask: the frame image is converted back to NV12.
- The stage runs on the element's CUDA stream in GStreamer's shared context, as the rest of the chain ([ADR-0012](0012-video-effects-and-maxine-backend.md)).
- The mask-hardening kernel of [ADR-0013](0013-mask-hardening-cuda-kernel.md) is unchanged. It hardens the mask computed on the denoised frame.
- The frame image is now tightly pitched for every effect, because the SDK accepts the image for this feature only that way. No effect measurably changed.

### Strength, size and temporal state
- **Strength** is fixed at 1 (noise removal favoured). There is no setting in this release. A development-only environment variable, `OBC_MAXINE_DENOISE_STRENGTH=0|1`, overrides it in the backend to compare the two models; it is not a supported configuration.
- **Size.** The stage supports 80x80 up to 1920x1080, the SDK's range. Outside it, the stage fails with a clear reason and degrades as below, so an output contract above 1080p runs without noise removal.
- **Temporal state** is zeroed when the stage loads and whenever the input switches to another source. The output clock marks the first fresh frame of a new source with a buffer flag, the `GST_VIDEO_BUFFER_FLAG_LAST` bit (the first bit free for custom use on video buffers). When the element sees the flag, it calls the backend's `reset_temporal`, which also resets the green screen's temporal state.
- **Limitation:** if the output queue drops the flagged buffer itself ([ADR-0010](0010-phase-locked-output-clock.md) keeps at most 3 frames), the reset is skipped and the state blends the old and the new source for a few frames.

### Switching
- `none` to `none` with noise removal: the raw image is shown until the stage is ready, because noise removal hides nothing.
- `none`, with or without noise removal, to `blur` or `replace`: black until the green screen is ready, the privacy rule of [ADR-0012](0012-video-effects-and-maxine-backend.md), unchanged.
- Turning noise removal on or off under a running `blur` or `replace`, and `blur` or `replace` to `none` with noise removal, is a switch on the loaded instance, of the same kind as `blur` to `replace`: the streaming thread waits for the switch, including the load of the stage's model, the last processed frame stays on screen, and no raw frame is shown.

### Failure policy: fail open
- Noise removal hides nothing, so its failure is not a privacy failure. If the stage cannot load or run (the SDK or the feature library is missing, the size is out of range, a run fails), the stream continues without it: with `none`, the camera image; with `blur` or `replace`, the effect without noise removal.
- `[effect] on_failure` does not apply to the stage and the daemon does not enter `error`. `Error` says `noise removal unavailable: <reason>; the camera image is shown without it`.
- A frame whose noise-removal run fails under `blur` or `replace` is processed again without the stage, so no raw frame appears.
- If the whole chain fails (the green screen, for example), that is the effect's failure and follows `on_failure` as before (black by default).
- The failure is cleared when the stage is ready again, on an effect method (`SetDenoise` included), on a configuration reload, or at the end of the session.
- `Error` precedence, first applicable: writer missing, fault, effect failure, noise-removal failure, damaged state file.
- A noise-removal failure seen while the effect is loading or ready is confirmed one poll later before it is reported, so that an effect failure that was read a moment apart is not blamed on the stage.
- There is no capability check before the session starts: a `none` session with noise removal starts even without the SDK, and the failure arrives while streaming.

### CUDA parking
The stage lives in the same backend instance as the effect. Its model handle and state buffer are released at unload, before the context is released, so the park at Idle of [ADR-0017](0017-cuda-checkpoint-parking.md) covers it, and nothing new calls CUDA at Idle. This is covered by backend hardware tests (park and unpark) and by a daemon test that streams `none` with noise removal, adds `blur`, toggles, stops and parks ([../testing.md](../testing.md#video-noise-removal)).

### Configuration, state and interfaces
- `[effect] denoise = false` is the default. The `denoise` key of `state.toml` overrides it, like the other effect keys. A value that is not a boolean is a damaged state file, which fails closed ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md), [ADR-0012](0012-video-effects-and-maxine-backend.md)).
- **D-Bus**, additive in `br.com.dias.OmarchyBroadcast1`, `InterfaceRevision` 4: the read-only property `Denoise` (`b`, announced with `PropertiesChanged`) and the method `SetDenoise(b)`. The method persists the value, applies it to the running session, writes nothing when the value is already in the state file, and clears a standing fault or effect-failure `Error` like the other effect setters ([../dbus-api.md](../dbus-api.md)).
- **CLI:** `omarchy-broadcast denoise on|off`. Against a daemon below revision 4 it exits 1 with a message that names both revisions. The human `status` and `effect` output add a `Denoise:` line from revision 4, and `watch` adds ` | denoise on` and a `denoise: on` change line. In JSON, `denoise` is `true` or `false`, and `null` (key present) from a daemon below revision 4. Earlier added fields take a default value from an old daemon; `null` is new here, because a default of "off" would claim a setting the daemon does not have ([../cli-json.md](../cli-json.md)).
- **Bar plugin:** a "Noise removal" switch in the panel's effect section, shown only when the CLI reports a boolean `denoise` and the daemon's revision is at least 4 ([../../clients/omarchy-bar/README.md](../../clients/omarchy-bar/README.md)).
- **SDK setup:** the feature and its models are downloaded like the green-screen and blur features, and `features/nvvfxdenoising/lib` must be on `LD_LIBRARY_PATH` (the shipped drop-in and the probe's `env.sh` have it). Without it the stage fails with NVCV status -2 and the stream continues without it. The feature's library needs the os-release override as well ([../setup/maxine.md](../setup/maxine.md)).
- **`obc-bench`:** `--denoise`, with `--effect none|blur|replace`, and the JSON effect section reports the request, the status, the reason, the frames processed and the stage's GPU time.

## Consequences
- The stage costs one GPU pass per frame and its model's GPU memory while it is on. Off, it costs nothing: no pass, copy or allocation, by construction.
- Every combination of `none`, `blur`, `replace` with and without noise removal was run live at 1080p30 on the reference machine, and each held the output rate with no skipped ticks. The figures are not published; see Context.
- A client built for revision 3 keeps working: it ignores the new property and field.
- A new, opt-in dependency on the SDK feature. The effects work without it, and so does the daemon.
- The new-source flag is a contract between the output clock and the element, with the one limitation above.
- A noise-removal failure is easy to miss, because the stream keeps running. `Error` and the `status` command say so.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| A standalone effect, `effect = denoise` | It would exclude combining with `blur` and `replace`, which is the main use in a dim room. Turning it into a boolean later would not be additive on D-Bus |
| Running noise removal after the effect | The green-screen mask would be computed on the grainy frame, and the stage would run over composited content instead of camera frames, which is what it is meant for. Running first also lets the stage write the frame image that the effects already read, so nothing else is rebound |
| A fail-closed noise removal (black on failure, like the effects) | Noise removal hides nothing. Showing the camera image without it is what the user would see with it off, so black would punish a missing optional library |
| `DISCONT` to mark the new source | The output pacer sets it on every skip, so every skip would reset the temporal state |
| `GST_BUFFER_FLAG_LAST` | It has the same value as the interlaced flag on video buffers; the video-specific `GST_VIDEO_BUFFER_FLAG_LAST` is the first bit free for custom use |
| A two-phase load off the streaming lock for the toggle under an effect | Disproportionate for a switch of the same kind as `blur` to `replace`, which already waits for the switch |
| Exposing the strength as a setting | Deferred: the strength is binary, and a setting would be a new public name to support. The development override covers comparisons |

## What would change this decision
- A second stage that stacks the same way (for example a relighting stage) would turn the boolean into a general list of stages, and the D-Bus shape would then be revisited.
- Evidence that the strength matters to users: expose it as a setting.
- A user report of the reset being skipped often enough to be visible: carry the new-source mark outside the buffer queue.
- A denoising model for frames above 1080p.
