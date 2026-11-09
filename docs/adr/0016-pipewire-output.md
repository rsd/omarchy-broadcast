# ADR-0016: An optional native PipeWire video source next to the loopback

- **Status:** Accepted (2026-11-09). Amends [ADR-0004](0004-v4l2loopback-first.md) (the PipeWire node that "comes later" is now an optional second output) and [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) (the definition of a consumer and the property list: D-Bus interface revision 3).

## Context
- [ADR-0004](0004-v4l2loopback-first.md) made v4l2loopback the first sink and left a PipeWire video node for later. [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) records that applications which discover cameras through PipeWire see the loopback only if WirePlumber probed it while a writer was streaming. A node that the daemon registers in PipeWire itself needs no probe and no kernel module.
- PipeWire camera support differs by application (as of late 2026):

| Application | Native PipeWire camera |
|---|---|
| Firefox | through the camera portal; the preference `media.webrtc.camera.allow-pipewire` (on by default in some distributions) |
| Chromium, Chrome | only with the `WebRtcPipeWireCamera` feature (`chrome://flags`, "PipeWire Camera support"), off by default; Electron applications likewise |
| OBS Studio | 30.1 and later, "Video Capture Device (PipeWire)" |
| GNOME Snapshot, GStreamer `pipewiresrc` | yes |
| Zoom, Discord | no; v4l2loopback only |

- The camera portal lists only nodes with `media.class` `Video/Source` and `media.role` `Camera`.
- A node that offers a single fixed format fails a consumer that asks for another one. Applications request their own format (RGBA, YUY2, I420, a smaller size), so the node must offer the formats a consumer may choose from, at the output contract's size and rate.
- The output already has one clock, one timeline and one persistent writer ([ADR-0010](0010-phase-locked-output-clock.md), [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md)). A second output should not have a second stream to keep consistent with it.

## Decision
**An optional second output, a PipeWire `Video/Source` node, fed from the loopback writer. The loopback is unchanged and remains the default.**

### Off by default, in two places
- The cargo feature `pipewire` of `obc-pipeline` (forwarded by `oma-broadcastd/pipewire`) compiles it. It needs the libpipewire headers and clang at build time and links `libpipewire-0.3`. The Arch package is built with it.
- The configuration key `[pipewire] enabled` (default `false`) turns it on. A build without the feature and `enabled = true` logs a warning and reports `PipeWireOutput` = `unsupported`; the loopback is unaffected.

### The node
- A client-owned node created in the daemon process through the `pipewire` crate (pipewire-rs 0.10.1): `node.name` `oma-broadcast`, `node.description` `Omarchy Broadcast`, `media.class` `Video/Source`, `media.role` `Camera`, `media.category` `Capture`.
- One thread, `obc-pipewire`, with its own PipeWire main loop. It owns the output stream (the driver of the node) and a registry listener.
- **Formats.** Three `EnumFormat` entries, all at the contract's exact size and frame rate: in the order YUY2, NV12, I420, whatever the contract's format (see the note on format order below). A consumer that requests another format or size fails its own link only; the node, its id and the other consumers are unaffected. This was verified live: RGBA and 640x480 requests failed with "no more input formats" on the consumer side only. No colour range or matrix is announced.

### Frames: a tap on the loopback writer
- Every buffer the writer pushes to the loopback, a placeholder or a camera frame, after it is restamped, is also handed to the PipeWire output. The two outputs therefore carry the same stream: the same contract, the same phase-locked pacing ([ADR-0010](0010-phase-locked-output-clock.md)), the same timestamps (the writer's timeline; `spa_meta_header` `pts` and `seq` are the writer's PTS and offset) and the same Idle behaviour (black placeholders while no camera runs, [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md)).
- Nothing is sent while no PipeWire consumer streams.
- **The conversion runs on the CPU, inside the one copy into the PipeWire buffer:** one data block with the planes contiguous (the GstVideoInfo default layout), 8 buffers requested (minimum 2, maximum 16).

### Consumers and the lifecycle
- **Detection.** The registry listener tracks the `Link` objects whose output node is this one. Their number is the PipeWire consumer count; each such link is followed, and a link in the error state is not counted. The node's `running` state is not used: it means "linked", not "producing".
- **Lifecycle.** The daemon's lifecycle keys off the total of the loopback consumers (processes holding `/dev/videoN`, as before) and the PipeWire links: the same 300 ms start debounce, the same idle grace and the same states of [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md). A PipeWire consumer starts the camera exactly as a loopback consumer does. Measured live: a link is visible about 10 ms after the consumer starts, and the state is `idle` about the grace period plus 0.65 s after it leaves.
- **Failure never reaches the lifecycle.** With no PipeWire daemon, a lost connection or a stream error the output is `failed` and retried every 2 s. It never puts the daemon in `error` and never sets `Error`; the loopback keeps working. Events from a replaced output are discarded (each output has a generation). A panic in a PipeWire callback is contained: the output fails and reconnects.
- **Node lifetime.**
  - It starts with the loopback writer when enabled, and exists only while the writer runs: a missing loopback device means no node. A PipeWire-only mode without v4l2loopback is not offered.
  - A reload that enables `[pipewire]` starts it at once. A reload that disables it stops it at once when no PipeWire consumer is linked, otherwise when the last one leaves.
  - The loopback writer is never rebuilt for a PipeWire setting.
  - An output-mode change rebuilds both, since the formats follow the contract, when no consumer of either kind remains.

### D-Bus and CLI (additive; `InterfaceRevision` 3)
- `LoopbackConsumers` (`u`): processes holding the loopback, the former meaning of `Consumers`. `PipeWireConsumers` (`u`): links to the node, 0 when the output is off. `PipeWireOutput` (`s`): `off`, `unsupported`, `connecting`, `ready` (registered, no stream), `streaming` or `failed`. All emit `PropertiesChanged`.
- `Consumers` is now the total of both. With the PipeWire output off it equals its former value.
- The CLI shows the split and the state, and the JSON output (schema version `v` stays 1) gains `loopback_consumers`, `pipewire_consumers` and `pipewire_output`. See [../dbus-api.md](../dbus-api.md) and [../cli-json.md](../cli-json.md).

### Duplicates in the camera list
Once WirePlumber has probed the loopback while the writer streams, PipeWire also lists the loopback as a camera ("Omarchy Broadcast" through v4l2), next to the native node. A user can hide the loopback from PipeWire only, with a WirePlumber rule; applications that open `/dev/video*` directly are unaffected. The rule is documented in [../setup/daemon.md](../setup/daemon.md) and not installed. With it, the WirePlumber ordering drop-in of ADR-0011 is unnecessary for PipeWire-based applications that use the native node; it stays useful where applications reach the camera through v4l2 only.

## Consequences
- Firefox, OBS, GNOME Snapshot and GStreamer reach the camera without WirePlumber probing the loopback. Chromium needs its flag and Zoom and Discord need the loopback, so the node is additive and does not replace the loopback.
- The PipeWire output costs one CPU conversion and copy per frame while a consumer streams, and nothing otherwise. The camera and GPU lifecycle are unchanged.
- `Consumers` changes meaning for clients that read it from a revision 3 daemon with the output on: it counts links as well. A client that needs the loopback alone reads `LoopbackConsumers`.
- Both outputs share one timeline, so a PipeWire consumer sees what a loopback consumer sees, including black while idle.
- The build gains an optional dependency (libpipewire, clang). The default build and the default CI job are unchanged; a second CI job builds and tests with the feature, and the Arch package is built with it.
- Tests: [../testing.md](../testing.md#the-pipewire-output).
- Not supported: a PipeWire-only mode without v4l2loopback; a colour range or matrix announced to consumers.

## Notes
- **Format order (2026-11-09).** The node offers YUY2 first, then NV12 and I420, whatever the contract's format. OBS Studio 32.2's PipeWire camera source computes NV12 row size as 2 bytes per pixel and reads past the buffer; offering YUY2 first avoids this. A consumer that asks for NV12 or I420 explicitly still negotiates it, and the buffer layout and stride the node announces are unchanged.
- **NV12 offer is a setting (2026-11-09).** `[pipewire] offer_nv12` (default `true`) keeps NV12 on the node's offer; `false` offers YUY2 and I420 only, so OBS Studio's NV12 bug cannot be reached by choosing the format by hand. Known issue and upstream links: [setup/daemon.md](../setup/daemon.md#known-issue-obs-studio-and-nv12).

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| GStreamer `pipewiresink mode=provide` with fixed caps | a consumer asking for another format errors the producer pipeline and removes the node. A list of caps does not help: upstream fixates to one format |
| Replacing v4l2loopback with the node | Zoom, Discord, Electron applications and Chromium by default would lose the camera |
| A WirePlumber Lua script or an SPA plugin | more moving parts outside the daemon, installed per user or system-wide, for what a client node does in-process |
| `pw-dump --monitor` as a subprocess to detect consumers | a process to supervise and a JSON stream to parse; the in-process registry listener sees the links directly, with a state per link |
| GPU conversion before the download | the format is the consumer's choice at run time; idle placeholders never touch the GPU ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md): idle means no CUDA); it would need a second `cudadownload` next to the loopback's |
| An independent placeholder in the PipeWire output | the tap makes both outputs carry the same stream and timeline; a second source of frames would need its own timestamps and its own hand-over |
| Rebuilding the loopback writer on a PipeWire toggle | a consumer holding the loopback would lose its stream for a setting that does not concern it |
| Using the node's `running` state as the consumer signal | it means linked, not producing |
