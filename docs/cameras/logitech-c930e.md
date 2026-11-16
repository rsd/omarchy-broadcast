# Logitech Webcam C930e (046d:0843) on Linux

Reference for the Logitech Webcam C930e: what the plain Linux kernel gives you, its vendor extension units and what each one holds, its microphone, and how omarchy-broadcast uses it.

Date: 2026-11-16. Firmware `bcdDevice` 0.21, USB 2.0 (480M), kernel 7.2 `uvcvideo` and `snd-usb-audio`. Measurements were made with one camera on a lit desk.

## Evidence levels

| Level | Meaning |
|---|---|
| **Measured** | Observed on the camera on Linux. |
| **Read-verified** | An extension-unit control whose length, current value, minimum, maximum and default were read with read-only queries (`GET_INFO`, `GET_LEN`, `GET_CUR`, `GET_MIN`, `GET_MAX`, `GET_DEF`); no write was made. |
| **Identified** | The meaning of the unit or selector comes from public sources (see [Sources](#sources)), not from a measurement. |

## Device topology

Interfaces: VideoControl, VideoStreaming, and a USB Audio Class microphone. The camera has no HID interface.

The UVC descriptors list a camera terminal, a processing unit and seven extension units (XU). In every control request `wIndex = unit << 8 | interface` (interface 0) and `wValue = selector << 8`. Unit IDs are specific to the camera and firmware: **match extension units by GUID, never by unit ID**.

| Unit | Type | GUID | Selectors present | Identified as |
|---|---|---|---|---|
| 1 | Camera terminal | none | standard | AE mode, AE priority, exposure time, focus, zoom, pan/tilt, autofocus (`bmControls 0x00020a2e`) |
| 3 | Processing unit | none | standard | brightness, contrast, saturation, sharpness, white balance (auto and temperature), backlight compensation, gain, power-line frequency (`bmControls 0x0000175b`) |
| 6 | XU | `23e49ed0-1178-4f31-ae52-d2fb8a8d3b48` | `0x01`–`0x0a` | not in public Logitech data |
| 8 | XU | `69678ee4-410f-40db-a850-7420d7d8240e` | `0x01`, `0x02`, `0x04`–`0x06`, `0x09`, `0x0a` | Logitech Device Information V3 |
| 9 | XU | `1f5d4ca9-de11-4487-840d-50933c8ec8d1` | `0x01`, `0x02`, `0x05`–`0x12` | Logitech Test/Debug V3 |
| 10 | XU | `49e40215-f434-47fe-b158-0e885023e51b` | `0x02`, `0x04`, `0x06`, `0x08`–`0x0c` | Logitech Video Pipe V3 |
| 11 | XU | `ffe52d21-8030-4e2c-82d9-f587d00540bd` | `0x09`, `0x0f`, `0x11` | Logitech Peripheral |
| 12 | XU | `a29e7641-de04-47e3-8b2b-f4341aff003b` | `0x01`–`0x03`, `0x09`–`0x0f` | UVC H.264 extension (UVCX) |
| 13 | XU | `13612d26-5aaa-46c4-b13d-ff4d9a60db86` | `0x02` | not in public Logitech data |

"Selectors present" is what answered `GET_INFO` among selectors 1 to 32 of each unit (measured); the others answer `ENOENT`. It matches the units' `bmControls` bitmaps, except unit 10, whose bitmap marks eight selectors while its `bNumControls` says seven.

## Video formats and modes

Two formats, measured with `v4l2-ctl --list-formats-ext`:

- **MJPEG**: every size from 160x120 to 1920x1080 at 30, 24, 20, 15, 10, 7.5 and 5 fps.
- **YUYV**: 30 fps up to 848x480 (800x600 from 24 fps), 15 fps at 960x540 and 1024x576, 10 fps at 1280x720, 7.5 fps at 1600x896, 5 fps at 1920x1080, and 2560x1472 at 2 fps (the only mode above 1080p, YUYV only).

There is **no 60 fps mode** at any size. 1080p at 30 fps is MJPEG only.

The H.264 encoder of the camera (unit 12) is not a separate format on the video node: it is reached through the UVCX extension unit, as GStreamer's `uvch264src` does. Read-verified, its committed configuration is 1920x1080 at 30 fps (frame interval 333333 x 100 ns) and 3 Mb/s, with bit rates from 64 kb/s to 12 Mb/s and frame intervals up to 200 ms (5 fps); the UVCX version (selector `0x0a`) reads 1.00.

## Standard UVC controls

All of them work through `uvcvideo` with no extra software. V4L2 names as `v4l2-ctl -l` shows them.

| Function | V4L2 control | Range, default | Measured behaviour |
|---|---|---|---|
| Brightness, contrast, saturation, sharpness | `brightness`, `contrast`, `saturation`, `sharpness` | 0 to 255, default 128 | Written values read back exactly |
| Backlight compensation | `backlight_compensation` | 0 or 1, default 0 | Reads back |
| White balance | `white_balance_automatic`, `white_balance_temperature` | auto default on; 2000 to 7500 K, default 4000 | The temperature is flagged inactive while automatic white balance is on; with it off, the written temperature reads back exactly |
| Exposure mode | `auto_exposure` | Manual (1), Aperture Priority (3, the camera's auto); default 3 | Reads back |
| Exposure time | `exposure_time_absolute` | 3 to 2047 in 100 µs units, default 250; active in manual mode only | **Quantised**: the camera keeps only about one value per stop: 3, 9, 38, 77, 156, 312, 624 and 2047 were seen. A written value reads back as one of those steps within a second, for example 250 as 156, 300 and 500 as 312, 700 as 624. The default 250 is not one of the steps. The picture follows the step (mean luma 27, 64, 111 and 167 at steps 38, 77, 156 and 312 with the gain fixed) |
| Gain | `gain` | 0 to 255, default 0 | With **automatic exposure** the camera drives the gain itself: a written value is replaced within seconds. With **manual exposure** the written gain sets the picture (mean luma 46, 108, 145 and 182 at gain 0, 64, 128 and 255, exposure fixed), but `GET_CUR` often returns another number (109 after writing 0, 255 after writing 128) |
| Keep frame rate in low light | `exposure_dynamic_framerate` | 0 or 1, default 0; 1 allows the frame rate to drop | Reads back |
| Anti-flicker | `power_line_frequency` | Disabled, 50 Hz, 60 Hz; default 60 Hz; no Auto item | Reads back |
| Focus | `focus_automatic_continuous`, `focus_absolute` | autofocus default on; 0 to 255 in steps of 5 | The position is inactive while autofocus is on |
| Digital zoom | `zoom_absolute` | 100 to 400 (1x to 4x), default 100 | Works |
| Digital pan and tilt | `pan_absolute`, `tilt_absolute` | ±36000 arc-seconds (±10°) in steps of 3600 (1°), default 0 | **Move the picture only while zoomed in.** At zoom 100 a full-range pan or tilt is accepted and reads back but leaves the picture unchanged (the frame difference stays at the noise level of an unchanged scene); at zoom 200 both move it |

The camera terminal has no UVC privacy control.

## Extension units (read-verified)

All values below are little-endian hex as the camera returns them. `GET_INFO` 1 is get only, 2 set only, 3 get and set. No unknown selector was written.

### Device Information V3 (`69678ee4-…`, unit 8)

All read-only (`GET_INFO` 1), and minimum, maximum and default equal the current value.

| Selector | Name in public data | Size | Value | Reading |
|---|---|---|---|---|
| `0x01` | Firmware version | 4 | `01081f00` | firmware version bytes (the byte order of the fields is not documented) |
| `0x02` | Firmware CRC | 2 | `9653` | |
| `0x04` | Sensor information | 2 | `0426` | |
| `0x05` | Processor information | 2 | `3200` | |
| `0x06` | USB information | 6 | `6d0443082100` | vendor `046d`, product `0843`, `bcdDevice` 0.21: matches the USB descriptor, which confirms the layout |
| `0x09` | Lens field of view | 2 | `0001` | |
| `0x0a` | Sensor dimension | 4 | `00000000` | |

The public data also lists `0x03` (EEPROM version) and `0x0b` (extended firmware version); this camera has neither.

### Video Pipe V3 (`49e40215-…`, unit 10)

| Selector | Name in public data | Size | Info | Current, min, max, default |
|---|---|---|---|---|
| `0x02` | Native mode forced | 1 | 3 | `00`, 0, 1, 0 |
| `0x04` | RightLight mode | 1 | 3 | `00`, 0, 1, 0 |
| `0x06` | Firmware zoom | 2 | 3 | `0100` for all four |
| `0x08` | Sensor cropping dimension | 6 | 3 | `000000000101`, `000000000100`, `e02ee02e02ff`, `000000000100` |
| `0x09` | MJPEG resync marker | 2 | 3 | `010a`, `0000`, `01ff`, `0000` |
| `0x0a` | (not in public data) | 2 | 1 | `0100`, `0000`, `ff00`, `0000` |
| `0x0b` | (not in public data) | 2 | 3 | `0100`, `0100`, `1400`, `0100` |
| `0x0c` | (not in public data) | 1 | 3 | `00`, 0, 1, 0 |

RightLight (`0x04`) is the one selector of this unit with a documented on/off meaning. A write of 1 was accepted and read back, and 0 was written back afterwards (measured). In a lit room it made no measurable difference to the picture's mean luma or to the exposure; its effect in low light is untested. The C930e has none of the MX Brio's settings in this unit: no field of view (`0x05`), Show Mode, exposure compensation, vibrance or white balance offset.

### Test/Debug V3 (`1f5d4ca9-…`, unit 9)

| Selector | Name in public data | Size | Info | Current, min, max, default |
|---|---|---|---|---|
| `0x01` | (not in public data) | 2 | 3 | `0094`, `0000`, `ffff`, `0000` |
| `0x02` | (not in public data) | 1 | 3 | `00`, 0, 255, 0 |
| `0x05`, `0x06` | (not in public data) | 2 | 3 | `0000`, `0000`, `ffff`, `0000` |
| `0x07` | (not in public data) | 5 | 3 | zero, zero, `ffffffffff`, zero |
| `0x08` | (not in public data) | 8 | 3 | zero, zero, `ffffffffff0fffff`, zero |
| `0x09` | (not in public data) | 2 | 3 | `e0e5` for all four |
| `0x0a` | TDE mode | 1 | 3 | `00`, 0, 1, 0 |
| `0x0b` | Gain | 1 | 3 | `00`, 0, 3, 0 |
| `0x0c` | Low-light priority | 1 | 3 | `00`, 0, 1, 0 |
| `0x0d` | Colour processing disable | 1 | 3 | `00`, 0, 1, 0 |
| `0x0e` | Pixel defect correction | 1 | 3 | `03`, 0, 2, 0 (the current value is above the reported maximum) |
| `0x0f` | Lens shading compensation | 1 | 3 | `01`, 0, 63, 0 |
| `0x10` | Gamma | 1 | 3 | `01`, 0, 2, 0 |
| `0x11` | Integration time | 4 | 3 | `00000000`, 10, 1000, 0 |
| `0x12` | Raw data bits per pixel | 1 | 1 | `31`, 0, 255, 0 |

This is a factory and debugging unit. Nothing in it is safe to write without a measurement plan; omarchy-broadcast never reaches it.

### Peripheral (`ffe52d21-…`, unit 11)

| Selector | Name in public data | Size | Result |
|---|---|---|---|
| `0x09` | LED | 5 (public data) | **`EACCES` to every query**, `GET_INFO` included |
| `0x0f` | (not in public data) | 14 | `02000600fcfff4fff4ff00000000` for all four; get and set |
| `0x11` | (not in public data) | 1 | `00`, 0, 1, 0; get and set |

The LED control is the one the Linux kernel now guards as a privacy control: `uvcvideo` refuses user-space queries to it unless the module parameter `allow_privacy_override` is set, which is off by default (it was off here). So the LED mode (Off, On, Blink, Auto in the public data) cannot be read or set without changing that parameter as root, and omarchy-broadcast does not try. The camera has no pan/tilt motor: the peripheral unit's relative pan/tilt (`0x01`) and focus motor selectors are absent.

### Unit 6 (`23e49ed0-…`) and unit 13 (`13612d26-…`)

Neither GUID appears in the public Logitech data. Unit 6 has ten selectors of 8 to 32 bytes, all zero except `0x0a` (16 bytes, read-only), which reads the ASCII text `PYTHON_V2B` followed by zeros. Unit 13 has one boolean selector, `0x02` (`00`, 0, 1, 0). Both are left alone.

### H.264 (`a29e7641-…`, unit 12)

The standard UVCX H.264 extension unit (selectors as in the UVC H.264 payload specification: `0x01` probe, `0x02` commit, `0x03` rate control mode, `0x09` picture type, `0x0a` version, `0x0b` encoder reset, `0x0c` frame rate, `0x0d` advanced configuration, `0x0e` bit rate, `0x0f` QP steps). See [Video formats and modes](#video-formats-and-modes) for the configuration it reads.

## Microphone

A USB Audio Class stereo microphone, 16-bit, at 16, 24, 32 or 48 kHz (measured from the descriptors and `arecord --dump-hw-params`). The feature unit has mute and volume; ALSA shows it as the simple control `Mic`, capture only, raw 0 to 60 for 20 to 50 dB in 0.5 dB steps (default at the maximum, 50 dB). So omarchy-broadcast's hardware gain (`[mic] hardware_gain_db`) applies to it.

The two channels are closely correlated (correlation 0.80 on speech in a quiet room): a stereo pair of capsules, not two independent microphones.

Levels, measured through the Omarchy Broadcast Mic with the C930e as its pinned input, speech at a normal distance, mixer at its default 50 dB:

| Path | RMS | Peak | Noise floor (10th percentile of 50 ms blocks) |
|---|---|---|---|
| The camera's own PipeWire source, left / right | −25.5 / −23.7 dBFS | −1.9 / −1.2 dBFS | −43.6 / −41.4 dBFS |
| Omarchy Broadcast Mic, noise removal off | −27.2 dBFS | −2.9 dBFS | −43.7 dBFS |
| Omarchy Broadcast Mic, noise removal on (NVIDIA Maxine) | −30.7 dBFS | −8.2 dBFS | −76.7 dBFS |

Noise removal lowers the noise floor by about 33 dB and the speech level by about 3.5 dB. The peaks come close to full scale at the default gain; a lower hardware gain leaves more headroom for a loud voice.

## In omarchy-broadcast

### Mode selection

The default capture request (`camera.mode`, 1080p60) resolves to **MJPEG 1920x1080 at 30 fps**: there is no 60 fps mode, and at 1080p the rate is kept over the format (YUYV is 5 fps there), so the camera is decoded on the CPU. Requests for 720p60 give MJPEG 1280x720 at 30 fps. Small sizes offered by both formats at the same rate (640x360 at 30) take YUYV. The capture-mode setting lists the MJPEG sizes from 1920x1080 down to 640x360, at 30 fps and, down to 960x540, at 24 fps.

### Measured with the virtual camera

1080p30 output (NV12), the camera in MJPEG 1080p30, 10 s per row; frame rate from a consumer of the virtual camera, added latency (`latency_p50_ms`, camera buffer to output tick) and load from the daemon's statistics; NVIDIA GPU effects.

| Effect | Consumer fps | Added latency p50 | Daemon CPU | GPU |
|---|---|---|---|---|
| none | 30.2 | 6.0 ms | 13 % | 1 % |
| blur | 30.4 | 5.8 ms | 17 % | 9 % |
| replace (image) | 30.2 | 5.8 ms | 17 % | 16 % |
| noise removal | 30.4 | 5.9 ms | 19 % | 19 % |
| HDR look | 30.2 | 5.7 ms | 16 % | 4 % |
| relighting | 30.5 | 5.6 ms | 48 % | 47 % |
| eye contact | 30.8 | 5.7 ms | 19 % | 10 % |
| auto-framing | 30.2 | 5.6 ms | 14 % | 14 % |

No frame was dropped in any run. "Enhance low-resolution camera" does not apply: the camera captures at the output's full resolution.

### Settings

Every standard control is published. The camera's row of control labels (`CONTROL_LABELS`) keeps the stock labels and gives three settings a description: pan and tilt move the picture only while zoomed in, and the gain is the camera's own while automatic exposure is on. No control is hidden, and no range is rewritten: the exposure time keeps its 3 to 2047 range although the camera keeps only about one value per stop in it.

The recommended settings of this model are the camera's defaults (noise removal is not suggested: it cleans the picture by about 20 %).

## Open questions

- What RightLight (unit 10, `0x04`) does in low light.
- Whether the camera's H.264 stream would be worth a GPU-decode ingest path at 1080p30 compared with MJPEG on the CPU.
- The meaning of the unidentified selectors of units 6, 9, 10, 11 and 13.

## Sources

- libwebcam/uvcdynctrl, `uvcdynctrl/data/046d/logitech.xml` (version 0.2.5 as packaged by Debian): the Logitech GUIDs (Device Information V3, Video Pipe V3, Test/Debug V3, Peripheral, UVCX H.264) and their selector names, and the LED mode values. <https://sources.debian.org/src/libwebcam/0.2.5-7/uvcdynctrl/data/046d/logitech.xml>
- Ricardo Ribalda, "media: uvcvideo: Import standard controls from uvcdynctrl" (Linux kernel patch, 2025-11-17): the Logitech Peripheral GUID and its pan/tilt selectors in the kernel's mapping table. <https://lkml.iu.edu/hypermail/linux/kernel/2511.2/02196.html>
- Ricardo Ribalda, "media: uvcvideo: Map known XU controls" (patch series, 2025-11-19), with "Introduce allow_privacy_override param": user-space access to privacy-relevant extension-unit controls such as the LED is blocked unless the parameter is set. <https://lwn.net/Articles/1047245/>
- USB Implementers Forum, *USB Device Class Definition for Video Devices: H.264 Payload* (UVC 1.1): the UVCX extension unit GUID and selectors; GStreamer's `uvch264src` (gst-plugins-bad, `sys/uvch264/uvc_h264.h`) uses the same definitions.
