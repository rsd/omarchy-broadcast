# ADR-0022: Crash recovery for consumers: a black timeout image and `timeout` on the loopback, monotonic timestamps, a faster restart

- **Status:** Accepted (2026-11-09). Extends [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) (the persistent loopback writer) and amends [ADR-0010](0010-phase-locked-output-clock.md) in one detail: the writer's timestamps are no longer counted from the writer's creation (the phase lock itself is unchanged). [ADR-0016](0016-pipewire-output.md) (the PipeWire output) is unchanged; its limitation under a crash is recorded here.

## Context
- The daemon keeps one loopback writer for its whole life ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md)): while it runs, an application that holds the virtual camera sees one uninterrupted stream. When the process dies without closing the writer (`kill -9`, a crash, the out-of-memory killer), the loopback has no writer until systemd starts a new process and that process attaches a new one.
- What a consumer sees in that gap was measured on v4l2loopback 0.15.4 with `exclusive_caps=1`, before this change:

| Consumer | Daemon crash (`kill -9`) | Clean restart (`SIGTERM`, an upgrade) |
|---|---|---|
| Plain blocking V4L2 reader (GStreamer `v4l2src`, `ffmpeg`, `ffplay`) | Survives any outage and resumes by itself | Invisible: the old writer sends placeholders until it closes and the new one starts within about 50 ms |
| Chromium-based application (Chrome and Edge with Meet, Teams on Linux, Zoom web, Discord, Slack, Electron applications) | **Loses the camera**: the track ends and does not come back unless the application opens the camera again | Invisible |
| PipeWire consumer of the daemon's node ([ADR-0016](0016-pipewire-output.md)) | Loses the stream: see [the PipeWire limitation](#the-pipewire-limitation) | Not measured |

- Why Chromium loses it: after about 1 s without a frame Chromium restarts its capture stream. v4l2loopback refuses a `VIDIOC_STREAMON` on the capture side with `EIO` while no writer streams (`keep_format=0`, the default), so the restart fails and the track ends.
- v4l2loopback has runtime controls that cover the gap without root and without a kernel change. `timeout` (milliseconds) makes the driver serve a timeout image when no frame arrives in time, and its timers keep firing after the writer's process is gone, until the last opener closes the node. With `timeout=250`, Chromium survived `kill -9`. Without an uploaded image the driver serves a zero-filled buffer, which is green in NV12.
- The writer's timestamps counted from the writer's creation. `v4l2sink` copies them into the V4L2 buffer timestamp (`TIMESTAMP_COPY`), so every daemon restart made the stream jump backwards: `ffmpeg` saw 7.9 s followed by -65.1 s and warned on every frame, and a strict muxer such as mp4 refuses non-monotonic DTS.
- The packaged unit restarted the daemon after 2 s (`RestartSec=2`). Counted from the crash, the frameless gap was about 0.7 s for the kernel to tear down the dying process, then the 2 s delay, then about 0.05 s for the new writer: the delay was about 70% of it.

## Decision
**At every loopback writer start the daemon uploads a black timeout image to the device and sets its `timeout`, so a crash is a stretch of black frames at a few frames per second instead of a camera that disappears. The writer stamps with `CLOCK_MONOTONIC`. The packaged unit restarts after 250 ms and has an explicit start limit.**

### The crash cover (`obc_pipeline::loopback_cover`)
- **When.** After the writer's `v4l2sink` has set the device's format, so the image can be sized to it. A probe on the sink's pad fires on the second buffer (the first has set the format and allocated the buffers) and hands the work to a helper thread. The writer never waits for it and never fails because of it.
- **The image is black.** Luma 16 and chroma 128, the contract's limited-range black, at the format the driver reports (`VIDIOC_G_FMT`). It is never the last frame: a camera image left on screen by a dead daemon would stay visible for the whole outage, which may never end if the daemon does not return, to a user who believes the camera is off.
- **The upload** uses the driver's `timeout_image_io` protocol: pressing that button control makes the next `open()` of the node a timeout-image opener; on that descriptor `VIDIOC_REQBUFS` (output, one buffer, no `VIDIOC_S_FMT`, which would reallocate the device's buffers), `VIDIOC_QUERYBUF`, `mmap` of the offset it returns, the fill, and `VIDIOC_REQBUFS` with zero buffers to give it back.
- **The `timeout`** is `max(250 ms, placeholder_after + one contract period + 50 ms)`: 250 ms at the default 30 fps contract. It is capped at the control's maximum. The formula keeps it above the writer's worst steady-state gap (placeholders resume two periods after the last forwarded frame), so the driver's timer never fires while the writer is alive. The writer's hold of the last forwarded frame during a stall of the output ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md), "The hold") does not change the formula: a held frame is pushed in a slot the placeholder thread would have filled, on the placeholder thread's grid, so the worst gap between two frames is the same with or without it.
- **Nothing is reset when the writer stops.** The cover must outlive the process. After a clean stop a consumer that still holds the node gets black frames at 1000/`timeout` fps instead of a frozen last frame; that is intended. The controls are device-wide and persist until the module is reloaded.
- **The outcome is logged** by the daemon at every writer start: `crash cover: black timeout image, timeout 250 ms ...`, or a warning with the reason. A driver without the controls gets the warning only; the writer runs as before.
- `keep_format` and `sustain_framerate` are never touched.

### Timestamps
The writer's PTS is the `CLOCK_MONOTONIC` time of the push (read once when the writer is built and advanced with `Instant`, which is the same clock), still strictly increasing. A new process therefore continues forward from the old one, and the timestamps are on the clock a UVC camera stamps with. The driver's timeout frames keep the timestamp of the last frame written. The phase lock of [ADR-0010](0010-phase-locked-output-clock.md) is unaffected: it runs on the output's own timeline, which the writer restamps. The PipeWire tap's `spa_meta_header.pts` and the preview carry the same monotonic values.

### The unit
- `RestartSec=250ms`.
- `StartLimitIntervalSec=60` and `StartLimitBurst=5`, so a crash loop stops whatever the manager's defaults are, and a loop of slow crashes too (systemd's default window is 10 s). Manual starts and restarts count. `systemctl --user reset-failed omarchy-broadcastd` clears it.
- `RestartPreventExitStatus=3` (another instance owns the bus name) is kept.

After a crash the camera picture returns once the new process has opened the camera and initialised CUDA, which takes seconds; until then the consumer sees black.

## Consequences
- A Chromium-based application keeps its camera across a daemon crash. A plain V4L2 reader already did and still does. A clean restart is still invisible.
- **Limitation: several readers share the ticks.** The driver's timeout flag is one device-wide flag, cleared by the first dequeue. With several readers the ticks are shared and one reader can go longer than Chromium's limit of about 1 s without a frame. The cover keeps one consumer alive across a crash; with several it may not. (A v4l2loopback device serves one capture stream at a time anyway, see [setup/daemon.md](../setup/daemon.md).)
- **Limitation: a race at the upload.** A program that opens the node in the instant between the daemon's button press and its own `open()` gets a timeout-image descriptor, which receives no frames. The cover makes the interval as short as it can (nothing runs between the press, the `open()` and the request) but cannot close it; upstream's `v4l2loopback-ctl set-timeout-image` has the same race. When the upload fails after the press, the daemon clears the pending flag with one more descriptor so that it does not catch the next program.
- <a id="the-pipewire-limitation"></a>**Limitation: PipeWire consumers do not survive a crash.** The PipeWire node lives in the daemon process. When the process dies, WirePlumber destroys the consumer's stream (`target not found`) and the consumer does not reconnect. Nothing in v4l2loopback can help; an application that must survive a crash should use the loopback device.
- The controls are left set on the device after the daemon stops (see above).
- Timestamps in a recording that spans a restart continue forward. They are monotonic-clock values, as from a UVC camera, not times since the writer started.
- A crash loop ends after five starts in 60 s and the unit is `failed` until `reset-failed`; before, it depended on the manager's defaults.
- The hardware tests that start the daemon or the writer on a loopback leave `timeout` set on that device. The tests that exercise the cover restore the controls they change ([../testing.md](../testing.md)).

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| Leave it: a crash loses the camera for Chromium-based applications | They are the browsers and conferencing applications this project exists for; the loopback offers a fix that needs no root |
| A timeout of 500 ms | Not tested with Chromium, and it halves the frame rate during the outage. 250 ms leaves a margin of four frames per second under Chromium's limit of about 1 s |
| `sustain_framerate=1` | It repeats the last frame, so the camera image stays on screen for the whole outage (a privacy problem). In steady state it also inserted duplicate frames, about 33 fps instead of 30, whenever the writer was more than 1.5 periods late |
| The timeout image as the last frame | The same privacy problem; black shows nothing |
| `keep_format=1` | It would keep the node listed as a capture device during the gap, but a new writer is refused until it is cleared. Not tested |
| A system-level udev rule or `v4l2loopback-ctl` at boot | Needs root, and has to be repeated after a module reload. The daemon doing it at each writer start is simpler and survives module reloads |
| Timestamps counted from the writer's creation (as before) | Every restart jumps backwards; strict muxers refuse the stream |
| Leaving `RestartSec=2` | It was most of the frameless gap after a crash |

### Future work: a separate writer process
A small process that owns the loopback descriptor and the PipeWire node and receives frames from the CUDA and effects process would make a crash of the effects process invisible to every consumer, including PipeWire ones. It costs a frame hand-over between processes (shared memory or dmabuf) and a second process to supervise. With the timeout cover in place its benefit for V4L2 consumers is small, so it would pay mainly for PipeWire. It is not built.

## What would change this decision
- A kernel module or driver that keeps the capture side alive without a writer (`keep_format` that does not block a new writer), or removes the need for the timeout image.
- A driver without the `timeout` controls on the systems this project targets: the cover would only warn, and the separate writer process would be the remedy.
- Many users with PipeWire consumers that must survive a crash: the separate writer process above.
- Evidence that a Chromium release restarts its capture sooner than about 1 s.
