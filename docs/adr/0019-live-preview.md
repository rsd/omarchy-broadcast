# ADR-0019: A live preview for clients: a lease, small JPEG frames in a private file, a signal

- **Status:** Accepted (2026-11-09). Extends [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) (the definition of a consumer and the D-Bus API, as revision 5, additive) and [ADR-0016](0016-pipewire-output.md) (a second consumer of the loopback writer's tap).

## Context
- A status-bar widget that controls the virtual camera has no way to show what the camera carries: whether the framing is right, whether an effect is on, whether the camera is black. The widget is a QML plugin that runs inside the shell and reads one JSON line at a time from `omarchy-broadcast watch --json` ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md)).
- The preview must show what applications see (the loopback's stream), including black at idle and while an effect loads at a start ([ADR-0012](0012-video-effects-and-maxine-backend.md)), not an image taken some other way.
- A client must not be able to disturb the virtual camera by looking at it. v4l2loopback in this configuration serves one format at a time and an application that opens the device holds it ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md), [ADR-0004](0004-v4l2loopback-first.md)).
- The daemon's persistent loopback writer already produces one stream with one timeline, and its tap serves the PipeWire output ([ADR-0016](0016-pipewire-output.md)). The tap passes system-memory buffers, so no GPU work is needed to read them.
- A preview is a use of the camera: a user who switches it on expects the picture, so it has to start the camera when nothing else does, and a user who merely opens a panel must not start it.
- Idle means no CUDA context, and the process is parked ([ADR-0006](0006-on-demand-gpu-lifecycle.md), [ADR-0017](0017-cuda-checkpoint-parking.md)); a preview may not change that.
- The JSON lines of the CLI are capped at 4096 bytes, and the plugin's parser handles one line at a time ([../cli-json.md](../cli-json.md)).

## Decision
**A client takes a preview lease over D-Bus; while it holds one, the daemon encodes small JPEG frames of the loopback writer's stream into a private file and announces each with a signal. The lease is a consumer. The CLI wraps the whole thing in one command, `omarchy-broadcast preview`.**

### The lease (D-Bus revision 5, additive)
- `AcquirePreview(u fps) -> s path`: `fps` is 15 or 30 (else `InvalidArgument`). It returns the absolute path of the frame file. It is granted in every state, `off` and `error` included.
- `ReleasePreview()`: ends the caller's lease; it is not an error when there is none.
- Signal `PreviewFrame(u seq)`: emitted after each replacement of the frame file; `seq` increases strictly within a daemon run, from 1.
- Property `PreviewConsumers` (`u`): the leases held. `Consumers` becomes the total of the loopback holders, the PipeWire links and the leases. `InterfaceRevision` is 5.
- **Keyed by the caller's unique bus name**, so there is one lease per connection and a second `AcquirePreview` renews it at the new rate. A call without a sender (a peer-to-peer connection, which has no bus to name its holder) fails with `Failed`.
- **A lease ends** on `ReleasePreview`, when its holder leaves the bus (`NameOwnerChanged` with no new owner, followed for every unique name), and at daemon exit. A holder that crashes therefore loses its lease without any action of its own. A caller that left before its lease was recorded would leak one, because its departure can be handled first; once the lease is recorded the daemon asks the bus whether the name still has an owner and ends the lease if not.
- **The rate** the frames are produced at is the highest asked by a lease held.

### Frames
- **Where they come from.** The encoder is a second consumer of the writer's tap ([ADR-0016](0016-pipewire-output.md)). It sees exactly the buffers the loopback sink and the PipeWire node get: black placeholders at `off` and `idle`, black while an effect loads at a start (an effect turned on mid-stream keeps the camera picture until it is ready, [ADR-0012](0012-video-effects-and-maxine-backend.md)), the processed camera image when `active`. It never opens the loopback device, so it cannot block an application or hold the device in a format, and it makes no CUDA call.
- **The tap never blocks the writer.** It decimates by the writer's timestamps to the rate asked (every second frame for 15 fps from a 30 fps contract; a frame counts when it is due within half a contract period, so jitter does not skip one), keeps only the latest frame as a reference, without a copy, and wakes the encoder's thread. A frame the thread has not taken is replaced.
- **Encoding.** On its own thread: a bilinear scale to 480 pixels wide with the writer's aspect (480x270 for 16:9), conversion to RGB, JPEG. The conversion happens before the encoder because JPEG is full range and the writer's frames are limited range; encoding them as they are would lift black to grey. One frame is in flight at a time.
- **The file.** Each frame is written to `$XDG_RUNTIME_DIR/omarchy-broadcast/preview.jpg.tmp` (mode 0600, never through a link) and renamed over `preview.jpg`, then `PreviewFrame` is emitted, so a reader never sees a partial frame. A symbolic link, hard link, FIFO or other non-regular file in the way is removed and the file created anew; the open never blocks, so a FIFO planted there cannot hold the encoder.
- **The directory** is created mode 0700. If it exists it must be a real directory owned by the daemon's user, and its mode is forced to 0700; otherwise `AcquirePreview` fails with `Failed`. The frames are the user's own image, and the runtime directory is already private to the user; the checks keep another process from placing a link or a file there to redirect or read it.
- **Both files are removed** when the last lease ends, when the writer stops, at shutdown and at daemon start (a crashed daemon may have left them).
- **When it runs.** Only while a lease is held and a writer runs. A writer rebuild (a new output contract) gets a new encoder. The writer has one tap slot, so the backend installs one composed tap that calls the PipeWire output's part and the preview's part, whichever run; stopping one leaves the other's frames flowing, and each outlives the other.
- The frame notification reaches the D-Bus side directly from the encoder, through the publisher's channel, and does not wake the controller for each frame.

### The lease is a consumer
- A lease is the third kind of consumer. The lifecycle (start debounce, idle grace, the deferred writer rebuild, the CUDA parking at `idle`) follows the total, unchanged.
- A lease alone therefore starts the camera and keeps it running, and its end lets the grace run out. This is how a preview switch works: switching it on takes a lease, which starts the camera; switching it off releases the lease, and the camera stops after the grace unless something else uses it. A client that wants to show the preview only while another application uses the camera counts the other consumers itself (`loopback_consumers` and `pipewire_consumers`), because its own lease is part of `consumers`.
- Opening a panel takes no lease, so it does not start the camera.

### The CLI
`omarchy-broadcast preview [--json] [--fps 15|30]` (default 15) holds a lease on its own bus connection and prints one line per frame, until it is interrupted. Details are in [../cli-json.md](../cli-json.md#preview).
- It never starts the daemon (every call carries `NO_AUTO_START`) and subscribes to `PreviewFrame` before it acquires, so no frame is missed.
- Lines: `preview` (`active`, `fps`, `path`) when the lease is held and when it is lost, `frame` (`seq`, `path`) per frame, and `error` lines with the existing outage codes, once per outage, with the daemon re-checked like `watch` does and the lease taken again when it is back. New error codes: `unsupported-daemon` (revision below 5; it keeps waiting for an upgraded daemon) and `preview-path-too-long`.
- **Paths are never clipped.** A client opens the file the line names, and a clipped path names none, unlike the other strings ([../cli-json.md](../cli-json.md#general-rules)). A path whose encoding exceeds 3968 bytes would not fit a line: the command releases the lease and waits for a new daemon instance, with `preview-path-too-long`.
- It exits 0 on `SIGTERM`, `SIGINT`, `SIGHUP`, `SIGPIPE` or a closed stdout, releasing the lease first. After `kill -9` the lease ends through the bus, because it belongs to the command's own connection.
- The frames are files and the lines are tiny: the plugin's parser never sees image data.
- `preview_consumers` is a new field of the `snapshot` and `state` lines (`v` stays 1, [../cli-json.md](../cli-json.md#versioning)).

### CUDA and the lifecycle
The encoder uses no CUDA and the frames are in system memory, so nothing new calls CUDA at Idle ([ADR-0017](0017-cuda-checkpoint-parking.md)). A lease at `idle` or `off` starts nothing but the CPU encoder on the persistent writer's placeholders (at `off` no camera opens); at `idle` the debounce then starts the camera, which is the consumer rule above.

## Consequences
- Revision 5 is additive: a client built for revision 4 ignores the new members. `Consumers` has a different meaning only while a lease is held; a client that needs the processes holding the loopback reads `LoopbackConsumers`.
- While a preview runs, the daemon spends CPU on scaling, conversion and JPEG encoding at most 15 or 30 times per second, and nothing otherwise. The camera, the GPU and the effect pipeline are unchanged.
- The preview is what applications get, including black placeholders and the black that precedes an effect: a user sees what a video call would see. It has no frames of its own to keep consistent with the stream.
- Frames are not a stream with delivery guarantees: they are the latest image. A slow reader or encoder skips frames; the file always holds a complete one.
- Known limitations:
  - A `PreviewFrame` may arrive after the reply of `ReleasePreview`.
  - A client that does not wait for each reply may have `AcquirePreview` and `ReleasePreview` handled out of order. The CLI waits.
  - Every client that leaves the bus wakes the controller once, whether or not it held a lease, because the daemon watches all departures.
- Tests: [../testing.md](../testing.md#the-live-preview).

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| QtMultimedia `Camera` in the plugin, on the loopback device | It opens the device, which blocks applications that want it, and loads libav into the shell process |
| QtMultimedia over PipeWire | Qt has no backend for it |
| A `gst-launch` pipeline with `pipewiresrc`, spawned by the plugin | It needs `[pipewire] enabled`, a GStreamer runtime dependency in a QML plugin and full-rate conversion on the CPU, and the preview would appear as a camera application. |
| Inline base64 frames in the NDJSON | About 25 KB per line through the shell's parser, against the 4096-byte line cap. A file and its path keep the lines small |
| Downscaling on the GPU in the CUDA pipeline | It couples the preview to the park and teardown rules ([ADR-0017](0017-cuda-checkpoint-parking.md)) and saves little, because the writer's frames are already in system memory |
| A lease that is not a consumer | The preview could not start the camera, so the switch that turns the preview on would show black until another application used the camera |
| Starting the camera when a panel opens | A panel opened to read the state would turn the camera and its LED on. The camera starts only when a preview is asked for |
| Opening the loopback device for the preview | It would be a consumer that also holds the device, and an application that wants another format would find it held |
| Encoding while no lease is held, to have a frame ready | It would spend CPU all the time for a view that usually nobody has |
| Per-client frame files or rates | One file and the highest rate are enough: each reader reads the same image, and the CPU cost does not grow with the number of leases |

## What would change this decision
- Evidence that the rate of 15 or 30 frames per second is not enough, or that a different size is needed: expose them (a new value of `fps`, or a size argument, would be additive).
- A client that cannot read files under `$XDG_RUNTIME_DIR` (a sandbox): frames over a file descriptor passed by D-Bus would be a new method.
- A second encoder consumer or a shared tap-composition need: the composed tap becomes a list.
