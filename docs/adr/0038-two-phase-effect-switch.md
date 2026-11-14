# ADR-0038: A two-phase effect switch: the new effects are built beside the running ones and take over between two frames

- **Status:** Accepted (2026-11-14), implemented in daemon 0.53.0 (`crates/obc-pipeline`, `crates/omarchy-broadcastd`). No D-Bus member is added: the interface stays at revision 27; `Warning` gains two clauses. Amends [ADR-0034](0034-away-image.md) (the restart hold of a module toggle is now the fallback path, not the usual one) and [ADR-0009](0009-ingest-process-output.md) (the output pipeline no longer lives for the whole session: it can be replaced by a second one while the first streams). Builds on [ADR-0010](0010-phase-locked-output-clock.md) (the output clock and its slot), [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) (the persistent writer and its hold), [ADR-0012](0012-video-effects-and-maxine-backend.md) (the effect element and its privacy rules), [ADR-0025](0025-loadable-modules.md) (the plan of modules is fixed for a session), [ADR-0033](0033-system-memory-pipeline.md) (the system-memory path) and [ADR-0037](0037-privacy-as-an-option.md) (privacy as an option). Reference: [../architecture.md](../architecture.md#the-two-phase-effect-switch), [../dbus-api.md](../dbus-api.md), [../testing.md](../testing.md#the-two-phase-effect-switch).

## Context
- A session runs the modules of its **plan**, fixed when it starts ([ADR-0025](0025-loadable-modules.md)): the effect element's backend and the module chains are built into the output pipeline and cannot take another module later. When the user enables or disables a module so that a new session would run a selected stage that the running one cannot (a module enabled that the session started without, or one that now runs a stage the session's own module cannot), the daemon stopped the session and started a new one: the camera was closed and opened again.
- That restart was covered, since the amendment of [ADR-0034](0034-away-image.md), by a restart hold: the loopback writer repeats the last frame, for at most 3 seconds, until the new session's first frame. The hold removes black and the away image from the transition, but the consumer sees a **frozen picture** for as long as the camera takes to reopen and the effects to load. For a module toggle in the middle of a call that is a visible stall, and the camera's LED goes off and on.
- What the user changed is the effects, not the camera. The ingest (the camera, its mode, its conversion to the canonical frame) is the same before and after. Only the output pipeline, the part after the slot ([ADR-0009](0009-ingest-process-output.md)), differs.
- The output reads canonical frames from a latest-frame slot and forwards to the loopback writer, which is persistent and keeps the consumer's device open ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md)). Both are designed so that what feeds them can change under a consumer; the writer's hold already survives a stall of the output.
- A background effect or a stage can need a long time to load, and can fail to load. A switch must not show the consumer an effect that is not ready, and with privacy on ([ADR-0037](0037-privacy-as-an-option.md)) must never show the room.
- Two output pipelines hold two chains in GPU memory while they overlap, and frames of one ingest are valid in a second pipeline only if both use the same CUDA context ([ADR-0009](0009-ingest-process-output.md), "One CUDA context").

## Decision
**On a live session in CUDA memory, a module change that needs new effects builds a second output beside the running one, on the same ingest, and hands the writer over to it between two frames once it is ready. The camera is not reopened and no restart hold is taken. Everything the switch cannot do falls back to the restart of [ADR-0034](0034-away-image.md), unchanged.**

### Preparing
- **The decision is the controller's** (`restart_for_modules`). When a new session would run a selected stage that the running one cannot, the controller checks whether the session can switch alongside; if it can, it prepares the new output, else it restarts as before.
- **A second output on the same ingest.** The daemon builds the output a start would build now (the effect element with its backend, the module chains, the stages in force) with the ingest's own fixed frame rate and enhancement, and gives it to the session (`Session::prepare_output`). The ingest keeps running. It writes each canonical frame into the slots of both outputs **by reference**: the same buffer, never copied, with its one source id and a sequence number and arrival time of each slot (`FrameWriter` fan-out). The new output takes the old one's CUDA context (`OutputConfig::cuda_context`) instead of opening its own, so the frames of the one ingest are valid in both.
- **A standby feed on the writer.** The new output forwards to the loopback writer, but on a **standby feed** (`WriterHandle::standby`). The writer accepts the forwarded frames of one feed only, the current one; the standby feed's frames are refused before anything of the writer's state is touched (its hold, its kept frame, its placeholder timing) and counted in `WriterCounts::standby_refused`. The new output runs, attaches and forwards like any output, and the consumer sees none of it.
- **The running session keeps running what it ran.** While the new output prepares, the old one keeps the effect and the stages of the module being switched away from, also after the user disabled that module, and follows the live settings it can run (strength, image, a stage's parameters). A stage that only the new output will run reads `<stage> unavailable: it runs once the new effects are ready` in `Warning` meanwhile.

### Ready
The new output is ready when all of these hold:
- it has emitted its first fresh frame of the current camera (a camera frame is through its whole chain);
- its background effect is `Ready`, when its plan runs one and one is selected;
- every stage that is enabled and that its plan runs is `Ready`. A stage that is absent from the output's report was not built (a chain whose module could not make it, which a start would also run without) and is not awaited.

### Commit
`promote` makes the new output's feed the writer's only accepted feed, under the push lock that serialises pushes, so the consumer's stream turns from one output to the other **between two frames**: every frame before is the old output's, every frame after the new one's, with no placeholder and nothing held. The ingest is then pointed at the new output's slot only, the new output becomes the session's output and only then is the old one stopped. Promotion is not a restart: it neither starts nor ends a restart hold. The controller then adopts the new plan and gives the effect settings that changed meanwhile to the new effects. No restart hold is taken, the camera is not reopened, and a commit that fails counts as a failure of the switch.

### Privacy is unchanged
With a policy that hides the camera ([ADR-0037](0037-privacy-as-an-option.md)) the consumer sees only the old output's frames until the new one is ready under the same policy, and the old output's frames are what that policy already allows (the effect, or what the policy shows in its place). A change of the privacy policy while a switch is prepared discards the prepared output, and the switch is decided again under the new policy. The new output is never forwarded before it is ready, so the room is never shown as an effect loads.

### When the session switches alongside, and when it restarts
It switches alongside only on a **live** session (a camera frame reached its output), streaming with its effects, in CUDA memory, when a session started now would run on the same camera with the same output contract and the same ingest, and the GPU has room. Otherwise it restarts as before, with the restart hold. The cases:
- **The session is not live yet**, or is streaming without its effects.
- **System memory**, including the battery saver ([ADR-0033](0033-system-memory-pipeline.md)). The switch needs two outputs sharing a CUDA context.
- **A change that needs a new ingest**: the enhance stage (enhancement is the ingest's, [ADR-0024](0024-stackable-stages-and-effects-in-the-settings-schema.md)), another camera, another output contract.
- **Not enough GPU memory.** The daemon reads the GPU's free memory from NVML afresh at the decision (not a sample of the statistics, which can be some seconds old and can still count an output discarded since). It switches alongside only when the free memory is at least the larger of what this process uses now and 1 GiB: the new chain is about as large as the old one. When NVML does not report the process's memory, the daemon restarts.
- **Not ready within 30 s** of the first preparation: the load was slow, which is not a failure of the effects, and a restart waits for it as any start does. The prepared output is discarded and the session restarts once, nothing remembered.
- **A failed preparation when the old chain can no longer run everything it kept**, for example because its module was disabled: there is nothing to keep running, so the session restarts at once.

### When the new chain fails
When the new chain fails (its pipeline, its background effect or one of its stages) while the old one can still run everything it kept, the prepared output is **discarded** and the old output keeps streaming, untouched. `Warning` carries the clause

`<what the new session would run>: could not switch: <reason>; the current effects keep running`

where `<what>` is the label of the stage or the background effect that only a new session runs and the reason is made safe to show and bounded as the other clauses. The clause stays while the old chain runs everything it kept. **That target is not prepared again until it changes** (the modules, the stage order, a stage turned on or off, the background effect): the failure is remembered by what a session started now would run. A change of target forgets it and prepares anew.

### What aborts a preparation
A preparation in progress is discarded, and the switch decided again, when what it was built for no longer stands: the session stopped or was dropped, the privacy policy or the background effect changed, the memory a start would choose changed (the battery saver), or the plan of modules a start would run changed again. A **camera switch or reopen** discards it too but suspends the switch: it is prepared again once the camera streams on the new ingest, within the same 30 s counted from the first preparation, so that a camera that does not come back does not hold the switch for ever; past the bound the session restarts for the new effects.

### Resources
Two output chains are resident in GPU memory for the length of the overlap, which is the load time of the new effects. This is the cost of the design, and the reason for the memory check above. The overlap does not wake the GPU (it runs in a session that holds it already) and adds no CUDA context.

## Consequences
- A module toggle on a live session no longer freezes the picture or reopens the camera: the consumer sees the old effects until the new ones are ready and then the new ones, with no frame held and none dropped.
- The restart hold of [ADR-0034](0034-away-image.md) stays, for the fallback cases: system memory, a change of ingest, a tight GPU, a slow load and a session whose old chain cannot go on. They behave as they did.
- The old effects run for as long as the new ones load, including a module that the user has disabled: its code runs for up to 30 s after the disable, bounded by the preparation, and the crash ledger ([ADR-0025](0025-loadable-modules.md)) marks the new modules as running from the start of the preparation.
- Two chains are in GPU memory during the overlap; a machine without room restarts instead.
- The writer gains a notion of a current feed and a counter for refused frames; nothing else of its behaviour changes while there is one feed.
- The session's output is no longer fixed for its life: code that held an output across a module change must follow the session's current one.
- A failed switch is visible and does not retry by itself; the user changes the target to retry.

### Known limits
- **A slow load shows no progress.** The old effects keep streaming and `Warning` says the stage runs once the new effects are ready; a load that takes more than 30 s ends in a restart, and the freeze that this ADR avoids then happens once.
- **The memory check is a threshold, not a guarantee.** The free memory can be taken by another process during the overlap; a failed build is then a failed switch, handled as above.
- **System memory restarts.** The switch is built and tested for CUDA memory only; a session in system memory, the battery saver included, restarts with the hold.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **Keep the restart hold** (the camera reopens, the writer repeats the last frame) | The hold removed black, not the stall: the consumer saw a frozen picture for the length of the restart, and the camera's LED went off and on. It remains as the fallback, where the switch cannot run |
| **Switch inside one effect element** | A module change replaces the backend, and the effect element cannot take another backend in a running pipeline: the element is built with its module's backend and its chains, and the module chains sit around it in the pipeline ([ADR-0025](0025-loadable-modules.md)). Making the element swap backends mid-session would put module loading, failure and privacy rules on the streaming thread, and would still not give a module chain a way in |
| **A second ingest** (open the camera twice, one for each output) | The camera cannot be opened twice: a V4L2 device streams to one reader, and a second open fails or steals the first stream. Closing and reopening it to feed the new output is the restart this ADR avoids. One ingest feeding two outputs by reference costs no copy and no second camera stream |
| **Fall back on a disable** (switch alongside only on an enable; restart when the user disables a module) | A disable is the common case: the old chain's module is going away, which is exactly when the user wants to move to the module that can run the effect. Keeping the old chain running, also after its module was disabled, until the new one is ready is what makes the switch seamless for it; restarting there would lose the benefit where it matters. The price is that disabled module code runs for the length of the preparation, bounded by 30 s |
| **Retry a failed preparation** (try again on a timer or at each step) | A failed chain is expensive to build: each attempt loads the effects and the models and holds a second chain in GPU memory. A failure is usually deterministic (a missing model, a refused engine), so retrying would reload the same thing and make `Warning` flap. The failure is remembered by target and the user's next change retries it |
| **Promote the new output as soon as it forwards a frame** | A frame through the chain is not a frame the effect has processed: an effect that is loading shows its fallback, and under privacy that is black or the away image. Waiting for the effect and the stages to be `Ready` means the consumer never sees the in-between |
| **Switch also in system memory** | The switch is built for CUDA memory, where the second output takes the first one's CUDA context and the free GPU memory can be measured. In system memory it would run two chains on the CPU for the whole load, which has not been measured on the machines that use that path. It restarts with the hold, as before |

## What would change this decision
- A way to give the output element a new backend in a running pipeline (an ABI that lets a module hand its instance over): the switch could then be one element's, with no second chain in memory.
- A switch in system memory, if a measurement shows two chains on the CPU affordable on the machines that use that path.
- A request to switch alongside across a change of enhancement: the ingest would need the same two-phase treatment.
- A load that regularly passes 30 s in practice: the bound would become a setting, or the restart would be replaced by waiting longer.
