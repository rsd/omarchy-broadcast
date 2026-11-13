# Setting up the v4l2loopback virtual camera

`omarchy-broadcast` writes the processed video into a [v4l2loopback](https://github.com/v4l2loopback/v4l2loopback) device. Applications (OBS, Chrome, Zoom, ...) then select that device as their camera. The pipeline finds the device by its **label**, `Omarchy Broadcast`, not by its `/dev/videoN` number (see [ADR-0004](../adr/0004-v4l2loopback-first.md)).

Creating the device needs root. This is a one-time setup.

## Single device

Create `/etc/modprobe.d/omarchy-broadcast.conf`:

```text
options v4l2loopback devices=1 video_nr=10 card_label="Omarchy Broadcast" exclusive_caps=1 max_buffers=4
```

Make sure the module is loaded at boot (for example `/etc/modules-load.d/v4l2loopback.conf` containing the line `v4l2loopback`), then reload it:

```sh
# Fails with "Module v4l2loopback is in use" while any application holds a
# loopback node. Check first and close those applications:
fuser -v /dev/video10
sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback
```

Verify (no root needed):

```sh
cat /sys/class/video4linux/video10/name      # -> Omarchy Broadcast
```

## Several devices

There can be only one `options v4l2loopback` line per module, so a second device means listing both devices in that line. `video_nr`, `card_label` and `exclusive_caps` are per-device, comma-separated arrays. The labels go **inside one pair of quotes**, separated by a comma. Quoting each label separately keeps the quote characters inside the names.

```text
options v4l2loopback devices=2 video_nr=10,11 card_label="First Device,Omarchy Broadcast" exclusive_caps=1,1 max_buffers=4
```

`max_buffers` is a single global value, not a per-device array.

## Why `exclusive_caps=1`

With `exclusive_caps=1` the device advertises only the CAPTURE capability while a producer is writing to it. It therefore appears as a camera in OBS or a browser only while `omarchy-broadcast` is streaming, and does not show up as a dead, black camera otherwise. Chrome requires this: it lists only devices that expose capture capability and not output capability. Keep it enabled.

## What the daemon sets on the device

At every start of its loopback writer the daemon sets two things on the device so that a daemon crash does not make applications lose the camera ([ADR-0022](../adr/0022-crash-recovery-for-consumers.md)). No root is needed:

- a **timeout image**, uploaded through the driver's `timeout_image_io` control: black, or the away image when `[output] placeholder = "image"` and `[effect] fallback_image` are set ([ADR-0034](../adr/0034-away-image.md)), and
- the **`timeout`** control, in milliseconds: 250 at the default 30 fps contract. When no frame arrives within that time, the driver serves the timeout image, also after the daemon's process has died. Chromium-based applications restart their capture after about a second without a frame, which v4l2loopback refuses while no writer streams, so without this they lose the camera.

Both are device-wide and persist until the module is reloaded: the daemon does not reset them when it stops, so an application that still holds the node after a clean stop gets the timeout image (black, or the away image) at a few frames per second instead of a frozen image. To inspect (no root; the query opens the node, so it counts as a consumer while it runs):

```sh
v4l2-ctl -d /dev/video10 -C timeout
```

The daemon logs the outcome at each writer start ([daemon.md](daemon.md#restarts-and-crashes)). The controls need a v4l2loopback that has them (0.15.4 does); without them the daemon only logs a warning.

Do not set `keep_format` or `sustain_framerate` for this daemon. `sustain_framerate=1` repeats the last frame, which would leave the camera image on screen while the daemon is down, and it inserted duplicate frames in steady state; `keep_format=1` refuses a new writer until it is cleared. The daemon never changes them.

## Changing the output size and rate

The size and frame rate of the virtual camera are `[output] mode` ([config.example.toml](../config.example.toml)). From daemon 0.27.0 they can be changed in the bar's setup dialog, or with `omarchy-broadcast config set output.mode 1280x720@30`, from a list the daemon computes for the camera in use (its 16:9 sizes from 3840x2160 to 640x360 at the rates it offers, and, with a frame-generation module, rates at twice the camera's marked "(generated)"). The change is written to `config.d/50-panel.toml`. The format of a loopback device that is open is fixed (`exclusive_caps=1`, above), so a format change cannot reach an application that already holds the device:

- **It waits for the applications.** While any application holds the virtual camera, the camera keeps its current format and the daemon's `Warning` says `Output 1280x720@30 applies when <applications> release the virtual camera`, naming up to three by process name. The new format starts once the last one lets go; the applications that open the camera afterwards get it.
- **A device held in another format is not a failure.** If the device turns out to be held in a format the daemon did not expect (a holder it could not count, such as another user's process), the camera keeps streaming the format it holds, if the daemon can feed that one (NV12 or I420), and tries the new format again after 2 s, then 4, 8 and 16, then every 32 s while the hold lasts. **Known limit:** each try stops and restarts the writer, so the native PipeWire node is recreated at each try; a holder the daemon can see (the loopback, PipeWire and the live preview are counted) stops the tries.
- **A format that cannot start reverts.** If the loopback or the output cannot start at the format, the daemon streams another and says so in `Warning`, in this order: the format the device is held in, the last format that worked in this run, the last one that streamed a frame in an earlier run (kept in the state file as `output_good`) and then 1920x1080@30. It stays on that until another format is chosen.
- **Two limits that cannot be removed here.** A device held in YUY2 by another producer gets no fallback: the daemon streams only NV12 and I420, and `Error` names the held format. A format whose session output cannot start even without effects leaves the first application on the writer's placeholder (black, or the away image) until it releases the device, because the format cannot change under it; applications that open the camera later get the format that works.

The capture mode of the camera is separate and changes at once, without touching the virtual camera: it is the setting "Resolution and frame rate" of the Capture group ([../dbus-api.md](../dbus-api.md#capture-and-output-modes-revision-12)).

## Notes

- `max_buffers=4` is the value all recorded benchmarks used.
- The device number (`10` above) is only a suggestion. The code never relies on it: the loopback is found by label through `/sys/class/video4linux/*/name`, and the camera by name, USB id or path.
- Opening a loopback node to query it counts as a consumer, so the code reads sysfs only.
- Runtime-only alternative (lost at reboot): `v4l2loopback-ctl add -n "Omarchy Broadcast" -x 1 -b 4 /dev/video11`, from the `v4l2loopback-utils` package.
