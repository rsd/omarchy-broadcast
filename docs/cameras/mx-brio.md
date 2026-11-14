# Logitech MX Brio (046d:0944) on Linux

Reference for the Logitech MX Brio 4K (and the MX Brio 705 for Business, the same camera with a business bundle): what the plain Linux kernel gives you, the vendor extension units and what is known about them, what omarchy-broadcast adds, and what is missing and why.

Date: 2026-11-14. Firmware 1.1.394, USB 3 (5000M). Logitech's own statement for Linux is "basic UVC webcam support only, with no Logitech software"; everything beyond UVC below is either measured on Linux, decoded from USB captures of Logitech's software, or marked as unverified.

## Evidence levels

Each claim in this page is one of three kinds. The tables repeat the level where it matters.

| Level | Meaning |
|---|---|
| **Measured** | Observed on the camera on Linux (kernel `uvcvideo`, `snd-usb-audio`, `hidraw`). |
| **Decoded** | Read from a USB capture (USBPcap and Wireshark) of Logi Tune 3.15.96 changing one setting at a time. The request and the values are known; the effect of a write on Linux has **not yet been verified**. Hardware verification is in progress. |
| **Read-verified** | A decoded control whose unit GUID, length, current value, minimum, maximum, step and default were read on Linux with read-only queries (no `SET_CUR`). The ranges are real; what a write does on Linux is still unverified. |
| **Documented** | Stated by Logitech (spec sheets, firmware notes) and not checked here. |

## Device topology

Interfaces: 0 VideoControl, 1 VideoStreaming, audio (UAC input), 2 HID (`hid-generic`, no input device), 3 vendor-specific with no kernel driver (probably firmware and management traffic).

The UVC descriptors list two standard units and eight Logitech extension units (XU). In every control request, `wIndex = unit << 8 | interface` (interface 0) and `wValue = selector << 8`. Unit IDs are specific to this camera and firmware; **match extension units by GUID, never by unit ID**. All eight GUIDs and their control counts below were confirmed on Linux from the USB descriptors that the kernel exposes in sysfs (`/sys/bus/usb/devices/<bus>-<port>/descriptors`, world-readable, no root needed).

| Unit | Type | GUID | Controls | Used by Logi Tune for |
|---|---|---|---|---|
| 1 | Camera terminal | none | standard | AE priority, pan/tilt |
| 3 | Processing unit | none | standard | backlight compensation, brightness, contrast, saturation, sharpness |
| 2 | XU | `2c49d16a-32b8-4485-3ea8-643a152362f2` | 6 | not seen |
| 6 | XU | `23e49ed0-1178-4f31-ae52-d2fb8a8d3b48` | 5 | not seen (the GUID other tools call "Kiyo Pro EU1") |
| 8 | XU | `69678ee4-410f-40db-a850-7420d7d8240e` | 13 | not seen |
| 9 | XU | `1f5d4ca9-de11-4487-840d-50933c8ec8d1` | 22 | not seen |
| 10 | XU | `49e40215-f434-47fe-b158-0e885023e51b` | 14 | field of view, Show Mode, exposure compensation, vibrance, white balance offset, framing zoom |
| 11 | XU | `ffe52d21-8030-4e2c-82d9-f587d00540bd` | 7 | RightSight framing pan/tilt start |
| 21 | XU | `5a6d654c-7e35-4d4e-810d-069d15e0f79b` | 2 | not seen |
| 22 | XU | `29144c1f-658d-4f47-a40f-0ed339428bf0` | 1 | RightSight block |

The camera terminal's `bmControls` is `0x00020a2e` (AE mode, AE priority, exposure absolute, focus absolute, zoom absolute, pan/tilt absolute, focus auto). There is **no UVC privacy control** on this unit (measured).

## Standard UVC controls

All work through `uvcvideo` with no extra software (measured). V4L2 names as shown by `v4l2-ctl -l`.

| Function | V4L2 control | Range, default |
|---|---|---|
| Auto exposure | `auto_exposure` | Manual, Aperture Priority (the camera's auto); default auto |
| Shutter | `exposure_time_absolute` | 3 to 2047 in 100 us units (0.3 to 204.7 ms), default 156; active in manual mode only |
| Gain ("ISO") | `gain` | 0 to 255, default 0; measured to change nothing visible |
| Keep frame rate in low light | `exposure_dynamic_framerate` | 0 or 1, default 0. This is UVC AE priority; **1 allows the frame rate to drop**. Logi Tune calls it "Low light compensation" (decoded: unit 1, selector `0x03`, 1 byte). No effect on the frame rate in a lit room (measured); untested in the dark |
| HDR | `backlight_compensation` | 0 or 1, **default 1**. This is UVC backlight compensation; Logi Tune's "HDR" switch is this control (decoded: unit 3, selector `0x01`, 2 bytes, `0000` off, `0100` on). **With it on, 1080p60 MJPEG is capped at 30 fps** (measured, see [Frame rate and HDR](#frame-rate-and-hdr)) |
| White balance | `white_balance_automatic`, `white_balance_temperature` | auto default on; 2800 to 7500 K, default 5000 |
| Brightness, contrast, saturation | `brightness`, `contrast`, `saturation` | 0 to 255, default 128 |
| Sharpness | `sharpness` | 0 to 255, default 128; measured to change nothing visible in a dim scene (re-check in good light) |
| Anti-flicker | `power_line_frequency` | Disabled, 50 Hz, 60 Hz; default 60 Hz; no Auto item |
| Focus | `focus_automatic_continuous`, `focus_absolute` | auto default on; 0 to 255 |
| Digital zoom | `zoom_absolute` | 100 to 400 (1x to 4x) |
| Digital pan and tilt | `pan_absolute`, `tilt_absolute` | +/-72000 arc-seconds (+/-20 degrees) in 1 degree steps; moves only while zoomed in |

Two points that are easy to get wrong:

- "HDR" and "Low light compensation" in Logitech's software are two **separate** standard controls (backlight compensation and AE priority), not extension-unit settings.
- Logitech's other image filters (Bright, Blossom, Forest, Film, Glaze, Mono), mirror, grid and preview quality are host-side presets or display options of the software. They produce no camera traffic beyond the standard controls above, so there is nothing to reach on Linux.

## Extension-unit controls (decoded, read-verified on Linux; writes pending verification)

Little-endian throughout. "Seen" is what Logi Tune was observed writing. "Min, max, default" are the camera's own answers to `GET_MIN`, `GET_MAX` and `GET_DEF` on Linux (step 1 for every single-byte control). All selectors answer `GET_INFO` with `0x03` (readable and writable).

| Setting | GUID (unit) | Selector | Size | Values | Min, max, default (Linux) | Level |
|---|---|---|---|---|---|---|
| Field of view (90, 78, 65 degrees) | `49e40215-...` (10) | `0x05` | 1 | `01` written for 78 degrees; also set by third-party tools ("BRIO FoV") | 0, 255, 0 | Measured: works, stored in the camera |
| Show Mode on/off | `49e40215-...` (10) | `0x0d` | 1 | `00` off, `01` on | 0, 1, 0 (off) | Read-verified |
| Exposure compensation | `49e40215-...` (10) | `0x13` | 1 | `0x80` = 0; seen 125 to 199; step per EV unknown | 0, 255, 128 | Read-verified |
| Vibrance | `49e40215-...` (10) | `0x14` | 1 | seen up to 204 | 0, 255, 128 | Read-verified |
| White balance offset ("temperature compensation") | `49e40215-...` (10) | `0x15` | 1 | about 40 K per step; `0x32` is 0 K; `0x39` = +280 K; seen 36 to 58 | 0, 100, 50 (`0x32`) | Read-verified: the default confirms the zero point |
| Framing zoom (RightSight companion) | `49e40215-...` (10) | `0x06` | 2 | mirrors V4L2 `zoom_absolute` (same value, range 100 to 400, `100` = 1x); seen `0x64` to `0x89` | 100, 400, 100 | Read-verified: **not a separate control** |
| Unknown | `49e40215-...` (10) | `0x12` | 1 | reads `0x32`; no Logi Tune control touches it; same shape as the white balance offset, possibly a tint offset (unverified) | 0, 100, 50 | **Do not write** |
| RightSight on/off | `29144c1f-...` (22) | `0x01` | 30 | byte 1: `00` off, `01` on | none (see below) | Decoded, read on Linux |
| RightSight framing speed | `29144c1f-...` (22) | `0x01` | 30 | byte 2: `00` slower, `01` default, `02` faster | none | Decoded, read on Linux |
| RightSight framing distance | `29144c1f-...` (22) | `0x01` | 30 | byte 3: `00` closer, `01` default, `02` farther | none | Decoded, read on Linux |
| Framing pan/tilt (RightSight companion) | `ffe52d21-...` (11) | `0x0c` | 8 | two int32 arc-seconds (pan, tilt), `0, 0` is centre; mirrors V4L2 `pan_absolute` and `tilt_absolute` | +/-72000 each, step 3600 (1 degree), default `0, 0` | Read-verified: **not a separate control** |

The RightSight block (unit 22, selector `0x01`) answers `GET_MIN`, `GET_MAX`, `GET_RES` and `GET_DEF` with the current bytes, so it reports no range of its own; the value sets above come from Logi Tune's captures. Its current content was `01 00 01 01 5a 01` followed by zeros, with RightSight off.

### The RightSight block

Unit 22, selector `0x01`, 30 bytes. Logi Tune always writes the whole block. Byte 0 is constant `01`; bytes 1 to 3 are the settings above; byte 4 read back as `5a` and once as `50`, byte 5 as `01`, bytes 6 to 29 are zero. The meaning of bytes 0, 4 and 5 is unknown, so a writer must **read the block first and change only the documented byte**, never write a fixed block.

When RightSight is switched on, and after each distance change (not after a speed change), Logi Tune also writes two companion controls (unit 10 selector `0x06`, unit 11 selector `0x0c`). Read on Linux, these mirror the standard V4L2 `zoom_absolute` and `pan_absolute`/`tilt_absolute` controls exactly (same value, range and step), so the companion writes are the standard zoom, pan and tilt and need no extension-unit access. During 20 s of movement Logi Tune sent only three framing writes, so tracking appears to run inside the camera and the companion writes look like a starting frame. Whether the camera tracks without that starting frame is an open question.

### Safety rule

Other selectors on these units may include save-to-flash or firmware commands, and related Logitech cameras are known to hold a save-to-memory command in the same units. Therefore:

1. Write only selectors and values listed above and observed in captures.
2. Read (`GET_CUR`, and `GET_LEN`) before writing, and keep every byte you do not own.
3. Resolve units by GUID from the descriptors.
4. Read-only queries (`GET_LEN`, `GET_INFO`, `GET_CUR`, `GET_MIN`, `GET_MAX`, `GET_RES`, `GET_DEF`) are safe; `SET_CUR` is not, outside the list above.

## Querying the controls on Linux

Standard controls:

```sh
v4l2-ctl --list-devices
v4l2-ctl -d /dev/videoN -l                       # all controls, ranges, defaults
v4l2-ctl -d /dev/videoN -C backlight_compensation,exposure_dynamic_framerate
```

Extension-unit GUIDs and unit IDs (confirm they match the topology table before sending anything):

```sh
sudo lsusb -v -d 046d:0944 | grep -B2 -A2 guidExtensionCode
```

Extension-unit values go through the `uvcvideo` ioctl `UVCIOC_CTRL_QUERY`, which takes `struct uvc_xu_control_query` (`linux/uvcvideo.h`): unit ID, selector, query code, size, data pointer. The ioctl number is `_IOWR('u', 0x21, struct uvc_xu_control_query)`. Query codes: `SET_CUR 0x01`, `GET_CUR 0x81`, `GET_MIN 0x82`, `GET_MAX 0x83`, `GET_RES 0x84`, `GET_LEN 0x85`, `GET_INFO 0x86`, `GET_DEF 0x87`. The caller needs read/write access to `/dev/videoN` (the `video` group). Stop any program that rewrites stored camera values (including omarchy-broadcast's daemon) while testing.

A read-only query in Python (reads the length, then the current value; never writes):

```python
import ctypes, fcntl, os, sys

class Query(ctypes.Structure):  # struct uvc_xu_control_query
    _fields_ = [("unit", ctypes.c_uint8), ("selector", ctypes.c_uint8),
                ("query", ctypes.c_uint8), ("size", ctypes.c_uint16),
                ("data", ctypes.c_void_p)]

UVCIOC_CTRL_QUERY = (3 << 30) | (ctypes.sizeof(Query) << 16) | (ord("u") << 8) | 0x21
GET_CUR, GET_MIN, GET_MAX, GET_RES, GET_LEN, GET_DEF = 0x81, 0x82, 0x83, 0x84, 0x85, 0x87

def query(fd, unit, sel, code, size):
    buf = ctypes.create_string_buffer(size)
    fcntl.ioctl(fd, UVCIOC_CTRL_QUERY, Query(unit, sel, code, size, ctypes.addressof(buf)))
    return bytes(buf.raw)

fd = os.open(sys.argv[1], os.O_RDONLY)               # e.g. /dev/video0
unit, sel = 10, 0x14                                  # vibrance (unit ID from your descriptors)
size = int.from_bytes(query(fd, unit, sel, GET_LEN, 2), "little")
for name, code in (("cur", GET_CUR), ("min", GET_MIN), ("max", GET_MAX),
                   ("res", GET_RES), ("def", GET_DEF)):
    try:
        print(name, query(fd, unit, sel, code, size).hex())
    except OSError as e:
        print(name, "error", e)
```

An alternative is `uvcdynctrl`-style mapping (`UVCIOC_CTRL_MAP`), which registers an XU selector as a V4L2 control so `v4l2-ctl` can read it; the direct query above avoids changing the driver's control list.

Logi Tune never read `GET_MIN` or `GET_MAX`; the ranges, steps and defaults in the extension-unit table come from exactly these read-only queries (the sysfs `descriptors` file gives the GUIDs without root, so only the queries need access to `/dev/videoN`).

## What works on plain Linux

Measured unless noted.

| Area | Result |
|---|---|
| Video modes | MJPEG up to 3840x2160 at 30 fps (7.5 to 30 fps steps); raw YUYV 4K only to 15 fps; 2560x1440 MJPEG and NV12 at 30, YUYV at 24; 1920x1080 and 1280x720 at 60 fps in MJPEG, YUYV and NV12 (with HDR off; HDR on caps 1080p60 MJPEG at 30 fps). Square and small modes (440x440, 480x480, down to 160x120) are advertised. No H.264. 4K needs a USB 3 port. |
| JPEG | The MJPEG stream is 4:2:2; some hardware JPEG decoders reject it. |
| Colour range | YUYV and NV12 are **limited range, as labelled** (black at 16, highlights stop at 235), re-measured in two different scenes; MJPEG decodes to full range. Software that relabels this camera's raw modes as full range washes the picture out. |
| Field of view, zoom, pan, tilt | Work as above. Pan and tilt are a digital crop, not a motor. |
| Show Mode (auto-flip) | The image flips in the camera when it is tilted down to a desk, with no host software. The orientation is reported on the HID interface, report `0x55` (vendor page `0xFFA1`, 7 bytes including the report ID): byte 5 is `01` flipped and `00` upright, and it can bounce `1/0/1` during the motion. Report `0x0a` (31 bytes, vendor page `0xFF01`) follows each change and is not decoded. During the flip only the inactive `exposure_time_absolute` read-back changed (625 to the default 156 and back), so Show Mode appears to switch the camera's auto-exposure state. |
| Privacy shutter | Mechanical. Closing it sends no HID report and changes no UVC control; the only sign is a **perfectly flat black frame, luma exactly 16** (limited-range black, min = mean = 16.0), which software can detect. |
| Microphone | `snd-usb-audio`, card "Brio", `S16_LE` stereo at 48, 32 and 16 kHz, UAC terminal type `0x0403` (speakerphone, no echo reduction). Left and right are bit-identical, so the camera sends one mixed channel twice. `Headset Capture Volume` is a hardware gain of +18 to +54 dB in 1 dB steps. Whether beamforming runs is not confirmed. |
| HID interface | A telephony headset interface (Phone Mute, Hook Switch, Mute LED) plus Logitech vendor reports. No sensor page, so no standard orientation sensor. `hid-input` does not map it, so nothing reaches the desktop. |
| Settings stored in the camera | Values set by other software persist across power cycles (the field of view read 65 degrees against a default of 90). A consumer that cares should write its own values at every open. |

### Frame rate and HDR

**HDR (`backlight_compensation=1`) caps 1080p60 MJPEG at 30 fps, even in a well-lit room.** Measured at 1920x1080 MJPEG, 60 fps requested, 300 frames, auto exposure (15.6 ms), gain 0, normally lit room (frame mean luma about 130 of 255):

| `backlight_compensation` | `exposure_dynamic_framerate` | Steady rate |
|---|---|---|
| 0 | 0 | about 60 fps |
| 0 | 1 | about 60 fps |
| 1 | 0 | 30 fps |
| 1 | 1 | 30 fps |

Wall-clock averages including about one second of stream start-up read about 49.5 fps for the 60 fps rows and 25.7 fps for the 30 fps rows; the steady rates are from the per-second read-out. Two consequences:

- Turn HDR off to get 60 fps. The V4L2 default is 1 (on), but values set earlier persist in the camera, and this unit was already stored at 0.
- `exposure_dynamic_framerate` made no difference in a lit room. Its effect in the dark, where the camera must lengthen exposure, was not tested.

An earlier session in a dim room, with auto exposure, delivered **20 fps** at every size, including 4K MJPEG and 1080p60 NV12, with `exposure_dynamic_framerate` at 0 (17 fps with it at 1); with manual exposure it delivered 30 fps and never 60. `power_line_frequency` at 50 Hz gave 25 fps; disabled gave 30 fps. The HDR cap explains 30 fps but not 20: the camera's low-light compensation probably lowers the rate further as exposure lengthens, but this has not been tested with HDR off in the dark. To check:

```sh
v4l2-ctl -d /dev/videoN -c backlight_compensation=0 -c exposure_dynamic_framerate=0
v4l2-ctl -d /dev/videoN --set-fmt-video=width=1920,height=1080,pixelformat=MJPG \
  --set-parm=60 --stream-mmap --stream-count=300
```

Run it in a dim room with `exposure_dynamic_framerate` at 0 and at 1 and compare the delivered rate.

## What omarchy-broadcast adds

omarchy-broadcast reads the camera through `uvcvideo` and publishes a virtual camera with effects applied.

- **Settings tree** for every standard control above, stored per camera and written at every open, plus extension-unit rows for this camera (field of view, and the settings in [Support in omarchy-broadcast](#support-in-omarchy-broadcast); `crates/obc-pipeline/src/xu.rs`).
- **Camera health:** the delivered frame rate against the nominal one, damaged frames, shared-hub warnings.
- **Colour range:** the quirk that treated this camera's raw modes as full range was removed in 0.49.1 after the measurement above.
- **Anti-flicker default** from the time zone when none is chosen.
- **Replacements for Logitech features that cannot run on Linux:** RightSight is replaced by a face-following digital pan and zoom stage (needs an NVIDIA GPU and the Maxine AR SDK); the camera's AI microphone suppression by a virtual microphone with Maxine, DeepFilterNet or RNNoise processing; Logitech's apps by one settings tree used by the CLI and the bar panel.
- **Beyond the camera:** background blur and replacement, relighting, eye contact, video noise removal, an HDR look applied to the finished image (it cannot recover detail the sensor clipped).

## Support in omarchy-broadcast

For this camera only (`046d:0944`), omarchy-broadcast exposes the decoded extension-unit controls as settings. Writes have **not yet been verified on hardware**; the ranges and defaults below are the ones read on Linux.

| Setting | Type | Range or values, default | Control |
|---|---|---|---|
| `camera.focus.show_mode` | on/off | off | unit 10, `0x0d` |
| `camera.exposure.exposure_compensation` | integer | 0 to 255, 128 | unit 10, `0x13` |
| `camera.image.vibrance` | integer | 0 to 255, 128 | unit 10, `0x14` |
| `camera.white_balance.temperature_compensation` | integer | 0 to 100, 50 (0 K); about 40 K per step | unit 10, `0x15` |
| `camera.focus.rightsight` | on/off | off | unit 22, `0x01`, byte 1 |
| `camera.focus.rightsight_speed` | slower, default, faster | default | unit 22, `0x01`, byte 2 |
| `camera.focus.rightsight_distance` | closer, default, farther | default | unit 22, `0x01`, byte 3 |

How the settings are guarded:

- **Matched by GUID**, never by unit ID, and only the listed selectors are ever written.
- **Read before write.** The RightSight rows read the 30-byte block first and change only their own byte, so the bytes whose meaning is unknown are written back unchanged.
- **Unknown values are greyed.** A value the camera reports outside the documented set is shown as unknown and disabled rather than guessed.
- **Never written:** unit 10 selector `0x12` (unknown), and the framing zoom and pan/tilt selectors, which are the standard zoom, pan and tilt controls and are set through those.

The face-following pan and zoom stage described above remains the replacement for RightSight where the camera's own framing is not wanted or not available.

## Feature status

| Feature | Plain Linux | Status |
|---|---|---|
| 4K30 (MJPEG), 1440p, 1080p60, 720p60 | Yes | Works |
| Exposure, shutter, gain, white balance, brightness, contrast, saturation, sharpness, focus, anti-flicker | Yes, standard controls | Works |
| HDR switch | `backlight_compensation` | Works (relabel only); on caps 1080p60 at 30 fps |
| Low light compensation | `exposure_dynamic_framerate` | Works (relabel only); effect on frame rate in the dark untested |
| Field of view | Extension unit, selector `0x05` | Works |
| Digital zoom, pan, tilt | Yes | Works |
| Show Mode auto-flip | Runs in the camera | Works |
| Show Mode on/off | XU `0x0d` | Read-verified, in omarchy-broadcast; writes pending |
| Exposure compensation | XU `0x13` | Read-verified, in omarchy-broadcast; writes pending |
| Vibrance | XU `0x14` | Read-verified, in omarchy-broadcast; writes pending |
| White balance offset | XU `0x15` | Read-verified, in omarchy-broadcast; writes pending |
| RightSight on/off, speed, distance | XU block, unit 22 | Decoded, read on Linux, in omarchy-broadcast; writes pending; tracking appears to run in the camera |
| RightSight starting frame (zoom, pan, tilt) | Standard `zoom_absolute`, `pan_absolute`, `tilt_absolute` | Works |
| RightSight auto-framing without the camera | Replaced by a host-side stage | Replaced |
| AI microphone noise suppression | Not reachable | Replaced by a virtual microphone |
| Microphone, hardware gain | Yes | Works |
| Beamforming, AI face-based enhancement | Not confirmed | Unknown |
| Shutter state to the OS | Flat-frame detection only | Missing as a signal; detectable in software |
| Tint | Not offered by Logitech's software for this camera, despite firmware notes; unit 10 selector `0x12` may be it (unverified) | Missing |
| Presets | Software side only; Logitech's filters are host-side presets of standard controls | Partial |
| LED mode control | See below | Missing, on purpose |
| Firmware updates | See below | Missing |
| Logitech Sync management | Logitech's cloud service | Missing |

## What is missing and why

- **LED mode.** The LED sits in a Logitech unit that `uvcvideo` treats as a privacy control and answers `EACCES` unless root loads the module with `allow_privacy_override=1`. omarchy-broadcast deliberately leaves it alone: the LED is the user's guarantee that the camera is on.
- **Firmware updates.** Only Logi Options+, Logi Tune and G HUB update the camera, on Windows and macOS. No fwupd plugin or LVFS package exists for `046d:0944` (fwupd's Logitech webcam plugin covers the Scribe, `046d:08e2`). The protocol is undocumented and a failed flash can brick the camera, so update from a Windows or macOS machine.
- **Logitech Sync.** IT remote management of the 705 for Business needs Logi Tune and Logitech's cloud.
- **Tint.** Present in the firmware notes ("Fine Controls"), but Logi Tune does not offer it for this camera, so no request was captured. Unit 10 selector `0x12` has the same shape as the white balance offset (0 to 100, default 50) and is a candidate, but without a capture or a safe test it is not written. A software colour stage is the alternative.
- **Unknown selectors.** Units 2, 6, 8, 9 and 21, and most selectors of units 10 and 11, were never exercised by Logi Tune. Without a documented purpose and the safety rule above they should be left alone.

## Open questions

1. Exposure compensation: how much exposure one step is worth (EV per step). The range is settled (0 to 255, default 128, step 1).
2. RightSight: does the camera start tracking without the starting frame (zoom, pan and tilt set to the values Logi Tune writes)? What are block bytes 0, 4 and 5?
3. Unit 10 selector `0x12` (0 to 100, default 50): meaning unknown, possibly a tint offset; never written.
4. Frame rate in the dark: with HDR off, does low light compensation (`exposure_dynamic_framerate`) or the camera's auto exposure still lower the rate, and is that the cause of the 20 fps seen in a dim room?
5. Whether writes to the decoded extension-unit controls behave on Linux as they do under Logi Tune. This page will be updated as each is verified.

Settled by read-only queries on Linux (2026-11-14): all eight extension-unit GUIDs match; the white balance offset zero point is `0x32` (50); the ranges and defaults of the single-byte controls (table above); the companion writes are the standard zoom, pan and tilt; HDR, not low light compensation, caps 1080p60 MJPEG at 30 fps in a lit room.

## Sources

Logitech's product, specification and support pages for the MX Brio and MX Brio 705 for Business, and its firmware release notes; USB captures of Logi Tune 3.15.96 on firmware 1.1.394 (USBPcap 1.5.4, Wireshark 4.6.9); measurements on a connected unit on Linux (V4L2 control sweeps, frame statistics across formats and exposure modes, a read-only extension-unit query sweep and an HDR frame-rate test, HID report logging, ALSA mixer and capture levels). The kernel side is documented in the `uvcvideo` driver documentation.
