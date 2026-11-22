# Set up the v4l2loopback virtual camera

The daemon writes the processed video into a [v4l2loopback](https://github.com/v4l2loopback/v4l2loopback) device, the virtual camera. Applications such as OBS, Chrome and Zoom select that device as their camera. Creating it needs root, once. Do this before you start the daemon ([daemon.md](daemon.md)).

The daemon finds the device by its **label**, `Omarchy Broadcast`, not by its `/dev/videoN` number, so the device number can change.

## Before you start

- A v4l2loopback kernel module. The Arch packages depend on one. On another distribution you supply it ([building.md](../building.md#before-you-start)).
- No other `options v4l2loopback` line in `/etc/modprobe.d`. Only one such line takes effect. If you already have one, merge the two as in [Several devices](#several-devices).

## Single device

[Step 5 of INSTALL.md](../../INSTALL.md#steps) creates the device: it installs the line below as `/etc/modprobe.d/omarchy-broadcast.conf`, loads the module at boot with `/etc/modules-load.d/v4l2loopback.conf` and reloads the module. The Arch package ships the line as an example, `/usr/share/doc/omarchy-broadcast/examples/modprobe.d/omarchy-broadcast.conf`, and a checkout has it in `dist/modprobe.d/omarchy-broadcast.conf`:

```text
options v4l2loopback devices=1 video_nr=10 card_label="Omarchy Broadcast" exclusive_caps=1 max_buffers=4
```

The reload fails with "Module v4l2loopback is in use" while any application holds a loopback node. Check first with `fuser -v /dev/video10` and close those applications.

## Several devices

You can have only one `options v4l2loopback` line, so a second device means listing both devices in that line. Write `video_nr`, `card_label` and `exclusive_caps` as comma-separated arrays, one value per device.

Put the labels **inside one pair of quotes**, separated by a comma. If you quote each label separately, the quote characters become part of the names.

```text
options v4l2loopback devices=2 video_nr=10,11 card_label="First Device,Omarchy Broadcast" exclusive_caps=1,1 max_buffers=4
```

`max_buffers` is one global value, not an array.

## Check that it works

Step 5 of [INSTALL.md](../../INSTALL.md#steps) ends with the check, which needs no root: the name of the device is `Omarchy Broadcast`. With the daemon running, `omarchy-broadcast status` shows the camera state ([daemon.md](daemon.md#check-that-it-works)).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Module v4l2loopback is in use` on `modprobe -r` | An application holds a loopback node | Find it with `fuser -v /dev/video10`, close it, reload the module |
| The daemon is in the state `error` and retries every 2 s | No device carries the label `Omarchy Broadcast` | Check the name in `/sys/class/video4linux/*/name`, then fix the `card_label` and reload the module |
| Two applications want the camera and the second gets "device busy" or no camera | v4l2loopback 0.14 and later lets one process stream at a time | See [One application at a time](#one-application-at-a-time) and [LIMITATIONS.md](../../LIMITATIONS.md#one-application-streams-from-the-virtual-camera-at-a-time) |
| The camera does not show up in OBS or a browser | With `exclusive_caps=1` the device appears only while the daemon runs | Start the daemon; see [Why `exclusive_caps=1`](#why-exclusive_caps1) |
| Device names show literal quote characters | Each label was quoted separately in the `modprobe.d` line | Put all labels inside one pair of quotes ([Several devices](#several-devices)) |
| The daemon logs "crash cover not in place" | The v4l2loopback version lacks the `timeout` and `timeout_image_io` controls | Use a newer v4l2loopback (0.15.4 has them); the daemon still runs ([below](#what-the-daemon-sets-on-the-device)) |

## One application at a time

A v4l2loopback device from version 0.14 on lets only one process stream from its capture side at a time ([LIMITATIONS.md](../../LIMITATIONS.md#one-application-streams-from-the-virtual-camera-at-a-time) has the effect and the workarounds). A second application that opens the same device can read its format and list its controls. Its attempt to start streaming (`VIDIOC_S_FMT` or `VIDIOC_REQBUFS`, or a plain `read()`) fails with `EBUSY`, even when it asks for exactly the format the device already has.

The driver gives the right to stream to the first process and keeps it until that process lets go. No module option changes this:

- `max_openers` limits how many processes may open the device, not how many may stream.
- `keep_format` only locks the format.
- The daemon's writer has no part in it.

Version 0.13.x let several processes read at once, but it has bugs that 0.14 fixed, so downgrading is not a remedy.

In practice:

- **One application can share the camera internally.** Chrome and Chromium open the device once and share the capture between their tabs, so several tabs of the same browser can show the camera at the same time.
- **A second application gets "device busy"** (or no camera, or an error about the format) while the first one streams. It works once the first one releases the camera. `omarchy-broadcast status` shows `Consumers` for the applications that hold the device.
- **For several applications at once, use the PipeWire camera node.** The daemon can register the camera as a native PipeWire video source next to the loopback, and PipeWire serves any number of consumers from it. Enable it with `[pipewire] enabled = true` in the configuration ([daemon.md](daemon.md#the-pipewire-output)), which also lists the setting each application needs. Zoom and Discord cannot use a PipeWire camera. They keep using the loopback device, which serves one of them at a time.
- **The PipeWire node has a known limitation** ([LIMITATIONS.md](../../LIMITATIONS.md#the-pipewire-node-and-mixed-formats)). It offers one format at a time to all its consumers. If one application stops receiving frames after another one starts, close both and start them again, or make the second one use the loopback.

## Why `exclusive_caps=1`

With `exclusive_caps=1` the device advertises only the CAPTURE capability while a producer is writing to it. It does not show up as a dead camera otherwise. The daemon's loopback writer is that producer. It opens the device when the daemon starts and streams into it in every state, idle and off included: black frames, or the away image when `[output] placeholder = "image"` and `[effect] fallback_image` are set. It stops only while its writer has failed, shown as the `error` state naming the device, which the daemon retries every 2 s. The device therefore appears as a camera in OBS or a browser whenever the daemon runs. An application that opens it is counted as a consumer, and that starts the camera unless you turned it off.

Chrome requires this: it lists only devices that expose capture capability and not output capability. Keep it enabled.

## What the daemon sets on the device

At every start of its loopback writer, the daemon sets two things on the device so that a daemon crash does not make applications lose the camera. No root is needed:

- a **timeout image**, uploaded through the driver's `timeout_image_io` control: black, or the away image when `[output] placeholder = "image"` and `[effect] fallback_image` are set, and
- the **`timeout`** control, in milliseconds: 250 at the default 30 fps contract.

When no frame arrives within that time, the driver serves the timeout image, also after the daemon's process has died. Chromium-based applications restart their capture after about a second without a frame. v4l2loopback refuses that restart while no writer streams, so without the timeout image they lose the camera.

Both settings are device-wide and persist until the module is reloaded. The daemon does not reset them when it stops. The driver's timeout flag is device-wide and cleared by the first reader that dequeues. With several readers on the node the timeout frames are shared, so one reader can go longer than Chromium's limit of about one second without a frame. The crash cover reliably protects a single consumer. While the daemon uploads the timeout image, a program that opens the device in the instant between the daemon's control write and its own `open()` gets the upload descriptor, which receives no frames. The window is as short as it can be made, and `v4l2loopback-ctl set-timeout-image` has the same race. It can only occur at a writer start. An application that still holds the node after a clean stop gets the timeout image (black, or the away image) at a few frames per second instead of a frozen image.

To inspect the timeout, run this. The query opens the node, so it counts as a consumer while it runs:

```sh
v4l2-ctl -d /dev/video10 -C timeout
```

The daemon logs the outcome at each writer start ([daemon.md](daemon.md#restarts-and-crashes)). The controls need a v4l2loopback that has them (0.15.4 does). Without them the daemon only logs a warning.

Do not set `keep_format` or `sustain_framerate` for this daemon. The daemon never changes them.

- `sustain_framerate=1` repeats the last frame, which would leave the camera image on screen while the daemon is down. It also inserted duplicate frames in steady state.
- `keep_format=1` refuses a new writer until it is cleared.

## Changing the output size and rate

The size and frame rate of the virtual camera are `[output] mode` ([config.example.toml](../config.example.toml)). From daemon 0.27.0 you can change them in the bar's setup dialog, or with:

```sh
omarchy-broadcast config set output.mode 1280x720@30
```

The daemon computes the list of choices for the camera in use: its 16:9 sizes from 3840x2160 to 640x360 at the rates it offers. With a frame-generation module, rates at twice the camera's are marked "(generated)". A "(generated)" rate shows the picture one output frame later than the same rate without frame generation, because each new camera frame is held back to be blended with the one before it. An effect still runs once per camera frame, and its result is repeated for the in-between frame, so the cost of an expensive effect is paid at the camera's rate, not the output's. Frame generation is planned when a session starts, from the cameras the daemon knows then. A camera plugged in during a session whose rate is half the output rate repeats frames until the next session, and gets frame generation from that session on. The change is written to `config.d/50-panel.toml`.

The format of a loopback device that is open is fixed ([`exclusive_caps=1`](#why-exclusive_caps1)), so a format change cannot reach an application that already holds the device:

- **It waits for the applications.** While any application holds the virtual camera, the camera keeps its current format and the daemon's `Warning` says `Output 1280x720@30 applies when <applications> release the virtual camera`, naming up to three by process name. The new format starts once the last one lets go. Applications that open the camera afterwards get it.
- **A device held in another format is not a failure.** Suppose the device is held in a format the daemon did not expect, by a holder it could not count, such as another user's process. The camera keeps streaming the format it holds, if the daemon can feed that one (NV12 or I420). It tries the new format again after 2 s, then 4, 8 and 16, then every 32 s while the hold lasts. **Known limit:** each try stops and restarts the writer, so the native PipeWire node is recreated at each try. A holder the daemon can see (the loopback, PipeWire and the live preview are counted) stops the tries.
- **A format that cannot start reverts.** If the loopback or the output cannot start at the format, the daemon streams another and says so in `Warning`, in this order: the format the device is held in, the last format that worked in this run, the last one that streamed a frame in an earlier run (kept in the state file as `output_good`), and then 1920x1080@30. It stays on that until another format is chosen.
- **Two limits cannot be removed here.**
  - A device held in YUY2 by another producer gets no fallback: the daemon streams only NV12 and I420, and `Error` names the held format.
  - A format whose session output cannot start even without effects leaves the first application on the writer's placeholder (black, or the away image) until it releases the device, because the format cannot change under it. Applications that open the camera later get the format that works.

The capture mode of the camera is separate and changes at once, without touching the virtual camera. It is the setting "Resolution and frame rate" of the Capture group ([../dbus-api.md](../dbus-api.md#capture-and-output-modes-revision-12)).

## Notes

- `max_buffers=4` is the value all recorded benchmarks used.
- The device number (`10` above) is only a suggestion. The code never relies on it: it finds the loopback by label through `/sys/class/video4linux/*/name`, and the camera by name, USB id or path.
- Opening a loopback node to query it counts as a consumer, so the code reads sysfs only.
- Runtime-only alternative, lost at reboot, from the `v4l2loopback-utils` package:

  ```sh
  v4l2loopback-ctl add -n "Omarchy Broadcast" -x 1 -b 4 /dev/video11
  ```
