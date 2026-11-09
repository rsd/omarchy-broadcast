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

## Notes

- `max_buffers=4` is the value all recorded benchmarks used.
- The device number (`10` above) is only a suggestion. The code never relies on it: the loopback is found by label through `/sys/class/video4linux/*/name`, and the camera by name, USB id or path.
- Opening a loopback node to query it counts as a consumer, so the code reads sysfs only.
- Runtime-only alternative (lost at reboot): `v4l2loopback-ctl add -n "Omarchy Broadcast" -x 1 -b 4 /dev/video11`, from the `v4l2loopback-utils` package.
