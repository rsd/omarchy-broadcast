# Adding a camera: measuring it with camera-tune

`scripts/camera-tune.py` measures one camera and prints proposals for the camera rows of `crates/obc-pipeline/src/quirks.rs`. Use it when you add support for a camera model. It is a developer tool, not part of the installed daemon.

The proposals are a `RangeQuirk`, a `ControlQuirk` (with `reset_at_open`) and a `CameraProfile`. The rows are data keyed by USB vendor and product id ([Quirks as data](architecture.md#camera-settings))), so adding a camera is adding rows and no code. The tool does not edit the table. It prints Rust text and the evidence next to it, and a person decides what goes in.

The daemon never runs the tool, and the tool never starts or stops the daemon.

## Before you start

### Requirements

- `python3` and `numpy`. Without numpy, or without `v4l2-ctl`, the tool exits with status 2 before it touches anything.
- `v4l2-ctl` (the `v4l-utils` package), for the formats, the controls and the capture.
- `ffmpeg`, only to decode MJPEG. Without it, an MJPEG mode is measured for timing only (no luma, noise or sharpness), and no range row is proposed from it.
- Read access to the camera's sysfs entries (`/sys/class/video4linux`), for the USB id, the product name and the hub the camera sits behind. A camera without a USB id (not a USB camera) is measured, but no row is proposed, because the rows are keyed by USB id.

### The scene

Point the camera at a static, evenly lit scene that has both highlights and shadows. A desk under a lamp works. A grey wall or a window does not.

- **Colour range.** The check can only fire when the picture reaches the ends of the scale. On a dim or flat scene, a full-range camera labelled limited looks like a correct one, and the tool reports nothing. That is a miss, not a clean result.
- **Noise.** Temporal noise is the spread of each pixel over about two dozen frames, and it is meaningful only when nothing moves. A moving subject, a flickering light or a window with passing traffic is read as noise and can turn `noisy` on wrongly.
- **Control sweeps.** Each sweep point restarts the stream, and the camera's automatic exposure converges again. A steady scene keeps that difference small. The tool also widens its tolerances by the measured difference between two baseline captures ([How each verdict is reached](#how-each-verdict-is-reached)).

## Run the tool

Stop whatever holds the camera first (the daemon, a browser tab), then run:

```sh
python3 scripts/camera-tune.py /dev/video0 --json /tmp/camera.json
```

| Option | Meaning |
|---|---|
| `device` | The video node to measure, such as `/dev/video0`. One camera per run. |
| `--quick` | Measure the modes only and skip the control sweep. Fast, writes no control, and proposes only a range row and the profile's `noisy` (each only when measured). |
| `--json OUT` | Write every measurement to `OUT` as JSON: identity, each mode's timing and analysis, the original controls, the sweep points and which controls could not be restored. |
| `--seconds N` | Seconds of stream per mode (default 4). |
| `--sizes N` | Largest sizes to measure per format, up to 1080p, each at its top frame rate (default 2). |
| `--settle N` | Seconds to wait after a control write before capturing (default 1). Raise it for a camera that adjusts slowly. |
| `--recommend NAME=VALUE` | A control value for the `CameraProfile` row. Repeatable, or comma separated (`--recommend sharpness=5,hue=0`). The name must be one the tool knows: brightness, contrast, saturation, hue, white_balance_automatic, gamma, gain, white_balance_temperature, sharpness, backlight_compensation. |
| `--force` | Measure even if another process holds the node (see [Safety](#safety)). |
| `--allow-shared-hub` | Stream raw modes of at least 12 MB/s even behind a hub shared with HID devices (see [Safety](#safety)). |
| `--tmpdir DIR` | Where the temporary raw captures go (default: the system temporary directory). |

The exit status is:

| Status | Meaning |
|---|---|
| 0 | The run finished and every control the tool wrote is back as it was. |
| 1 | A control the tool wrote could not be restored. |
| 2 | A refusal or a missing requirement: numpy, `v4l2-ctl`, a busy node, a bad option. |
| 130 | Interrupted. The controls are restored first. |

## Read the output

The output has a per-mode summary, one line per swept control, and then the proposed rows. This abbreviated, generic example shows a camera that fails a manual white balance write and is grainy:

```text
camera-tune: Example Webcam (driver uvcvideo)
  USB 045e:0766 Microsoft LifeCam VX-800, path 1-1.4.2 <- usb1, 480 Mb/s
camera-tune: 6 mode(s) listed, measuring 4
  YUYV 640x480: 29.97 fps (nominal 30), jitter std 0.9 ms max 6.1 ms, first frame 0.9 s, errors 0, dropped 0
      Y 7-250 (<16 4.1 %, >235 3.0 %), noise mid-tone temporal 1.4, spatial 1.1, sharpness 82.0
      RANGE: luma outside 16-235 under a limited label (full range suspected)
camera-tune: sweeping controls on YUYV 640x480@30 (settle 1 s) ...
  brightness                   effect   mean Y moves 38.2
  white_balance_automatic      broken   value 0 moves the chroma by 41 code values
  white_balance_temperature    broken   write of 2800 failed: VIDIOC_S_CTRL: Protocol error
camera-tune: restoring controls ...

== proposed rows (crates/obc-pipeline/src/quirks.rs) ==
// RANGE_QUIRKS
    // YUYV: luma 7-250, 7.1 % outside 16-235 (margin 2) under a limited label.
    RangeQuirk {
        usb: usb(0x045e, 0x0766),
        formats: &[PixelFormat::Yuy2],
        model: "Microsoft LifeCam VX-800",
    },

// CONTROL_QUIRKS
    // white_balance_automatic: value 0 moves the chroma by 41 code values
    // white_balance_temperature: write of 2800 failed: VIDIOC_S_CTRL: Protocol error
    ControlQuirk {
        usb: usb(0x045e, 0x0766),
        hidden: &[CID_AUTO_WHITE_BALANCE, CID_WHITE_BALANCE_TEMPERATURE],
        because: HiddenBecause::Broken,
        reset_at_open: &[CID_AUTO_WHITE_BALANCE],
        model: "Microsoft LifeCam VX-800",
    },

// CAMERA_PROFILES
    // mid-tone temporal noise 1.40.
    CameraProfile {
        usb: usb(0x045e, 0x0766),
        values: &[],
        noisy: true,
        model: "Microsoft LifeCam VX-800",
    },
```

The numbers are illustrative. A control the tool knows has its constant in the table, such as `crate::controls::CID_BRIGHTNESS`. Any other control is printed as a hex id with its name in a comment. Turn that into a named constant.

## Turn the proposal into a row

1. **Check the evidence beside each row**, on the scene described above. A row is a claim about a camera model, and the tool measured one unit of it in one light.
2. **Look at the pictures** for every `Broken` verdict that came from the chroma jump alone (no write failed). White balance and other colour controls legitimately move the chroma a lot at their extremes, so a jump of 25 or more is a reason to look, not proof. Set the control to the offending value in a viewer and judge the picture by eye. Only a failed write, or a picture that is plainly broken, justifies hiding the control.
3. **Keep one `ControlQuirk` per camera.** The tool may print two rows for the same USB id (`Broken` and `NoEffect`). The table holds one row per camera, so merge them by hand: one row, one `because`. Keep the `Broken` row and note the `NoEffect` controls in a comment beside it, or split the controls by the reason that matters most.
4. **List `reset_at_open` only for controls whose default write is known to be safe.** Each must also be in `hidden` (a test checks that). Drop from the proposal any control whose write failed in the sweep. The VX-800's row resets the automatic switch and never the temperature, because the temperature write is what fails.
5. **Fill `CameraProfile.values` only after looking at pictures.** Run again with `--recommend NAME=VALUE` for the values you judged better than the camera's default. Never use it for a hidden control or the power-line frequency (it follows the region, not the camera). An empty list means the defaults are best.
6. **Add the rows** to `RANGE_QUIRKS`, `CONTROL_QUIRKS` and `CAMERA_PROFILES` in `crates/obc-pipeline/src/quirks.rs`, with a comment that says what was measured, as the existing rows do.
7. **Update the tests in the same file** and run them:

   ```sh
   cargo test -p obc-pipeline --lib quirks
   ```

   The tests of that module look the rows up by USB id and check the invariants that a new row must keep:

   - no table lists a camera twice (which is why the merge in step 3 is required);
   - MJPEG is never in a range row;
   - every `reset_at_open` control is also hidden;
   - a profile names no hidden control and never the power-line frequency.

   Add an assertion for the new camera next to those of the cameras already there. [Quirks as data](architecture.md#camera-settings) lists the cameras that have rows. Update it when you add a row.

## Safety

A run drives a real device, so the tool is conservative:

- **A busy node is refused** (`fuser`, else a scan of `/proc`) unless you pass `--force`. Stop the daemon and any other user of the camera first. With `--force` the measurements are unreliable, because another reader changes what the stream does.
- **One stream at a time**, on one node. The tool never opens two streams, of the same camera or of two.
- **Every control the tool wrote is restored, and only those.** The tool reads all controls first, remembers each one it writes, and puts those back at the end. It also restores on an error and on `SIGINT` or `SIGTERM` (a second signal during the restore is ignored, so the restore finishes).
  - A control it never wrote is left alone. Autofocus, auto exposure and the gain that follows it are therefore never moved or compared. A `--quick` run writes nothing and restores nothing.
  - The order is: the automatic masters that started on manual, then the other controls, then the masters that started on automatic.
  - A written control that still differs is retried. An inactive one is retried with its automatic master switched to manual for a moment.
  - A control that was inactive at the start is not written back once its master is back at its starting value, since the camera drives it again.
  - A written control that still differs afterwards is named, and the exit status is 1.
- **Controls it never writes** (nor restores): privacy, LED, and motion controls (pan, tilt, zoom, roll, iris, focus), the power-line frequency and the other controls that do not show in the picture, read-only controls, and inactive controls. There is one exception: a dependent of an automatic master that the sweep covers (white balance temperature, gain) is written while the tool holds the master on manual, and the master is put back afterwards. A master is switched only when it has such a dependent, never for a lens motor.
- **A raw mode of at least 12 MB/s (12 000 000 bytes per second) is skipped behind a shared hub** unless you pass `--allow-shared-hub`. The skip applies when a hub above the camera, other than the root hub, has a HID device anywhere below it. The nearest such hub counts. The camera itself and any device with a video interface do not. Such a stream can fill the hub's link and reset the keyboard and mouse on it: a 640x480 YUYV stream at 30 fps (18.4 MB/s) did so in practice, and it is the same condition under which the daemon warns. MJPEG has no fixed rate and is never skipped. Use the flag only when you can afford to lose the keyboard and mouse, or move the camera to a port on the root hub.

## Limits

- One camera, one light, one unit: a row is as good as that measurement. A second unit or a second scene that disagrees is a reason to look again, not to average.
- The tool sees what the picture shows. A control that acts only under conditions the scene does not create (a backlight compensation with no backlight) can read as `NoEffect`. Check before hiding it.
- A moving scene or an unsteady light shows as noise and can break the baseline tolerances. The tolerances widen to absorb it, and so can hide a real small effect.
- The tool proposes rows for the quirk table only. Vendor settings in an Extension Unit ([Quirks as data](architecture.md#camera-settings)) are outside what it measures.

## What it measures

**Identity.** The card and driver (`v4l2-ctl --info`). From sysfs: the USB vendor:product, the product name, the link speed and the USB path. The tool also reports whether the camera shares a hub, other than the root hub, with HID devices (a keyboard, a mouse). The walk goes up every hub above the camera, and the nearest one with a HID device anywhere below it is the shared hub. The camera itself, and any device with a video interface (a webcam with a button has a HID interface), are not HID devices. This is the daemon's rule (`crates/obc-pipeline/src/usb_topology.rs`).

**Modes.** For each pixel format, the largest `--sizes` sizes up to 1920x1080 at their top frame rate, streamed for `--seconds` each. Per mode:

- **Delivered frame rate**, against the nominal one. It comes from the kernel's buffer timestamps (`v4l2-ctl --verbose`), not from wall-clock time, after the first 15 frames (warm-up). The tool also reports the interval jitter (standard deviation, minimum, maximum, 99th percentile), the frames later than 1.5 times the nominal interval, and dropped sequence numbers.
- **First-frame time**, from starting the stream to the first buffer.
- **Error frames**, the buffers the driver flagged `V4L2_BUF_FLAG_ERROR`.
- **Luma range**: the minimum and maximum of Y and the share of samples outside 16-235, over the last 24 usable frames of the capture (not the whole capture). A frame is usable when it is full size and not flagged as an error. The daemon drops the others, so the analysis does too. The tool matches each frame in the capture file to its `v4l2-ctl --verbose` record:
  - by the running sum of the recorded sizes, when they add up to the file;
  - otherwise, when the file is a whole number of full frames, by aligning the records to the frames from the end (v4l2-ctl can log one dequeue more than it writes, such as a first buffer flagged as an error on a UVC camera).

  A frame whose matched record is flagged or short is left out. A file that is neither is not analysed, and the mode line says why.
- **Temporal noise**, the per-pixel standard deviation over those frames. The figure used is the median over the mid-tones (pixels whose mean lies between 60 and 190, where clipping hides nothing), in code values. A scene with no mid-tone pixel at all falls back to the 25th percentile of the per-pixel spread over the whole picture, which is not comparable with the mid-tone figure. The output still prints one number then, so a flat dark or blown-out scene is a reason to retake it.
- **Spatial noise**, Immerkaer's single-frame estimate on the last frame.
- **Sharpness**, the variance of the Laplacian after an area-average downscale to 640x360, so modes of different sizes compare on one scale.

A raw mode needs a format the tool decodes (YUYV, UYVY, NV12, NV21, I420, YV12, GREY). MJPEG needs ffmpeg. The tool deletes raw captures after each mode, whether it finished or was interrupted. A large mode is cut short once the file would hold about 300 MB of frames, plus the 15 warm-up frames, and never to fewer than the 24 analysed frames. A very large frame can therefore take the file above 300 MB.

**Control sweeps** (not with `--quick`). The tool picks the mode with the smallest area among the raw modes at least 640 wide that it measured (else a compressed one). On that mode, it sets each integer, menu and boolean control to its minimum, default and maximum, one control at a time. A boolean goes to 0 and 1. A menu goes to its first, default and last item.

At each point the tool waits `--settle` seconds, captures a short run and records the mean Y, U and V, the contrast (standard deviation of the mean image), the sharpness and the noise. A control that an automatic master keeps inactive (white balance temperature under automatic white balance, exposure and gain under automatic exposure) is swept with the master switched to manual for the duration.

## How each verdict is reached

**Colour range (`RangeQuirk`).** The rule is the daemon's run-time check (`crates/obc-pipeline/src/colour_range.rs`, [the colour range of a raw camera](architecture.md#2-stage-and-caps-model)). The tool applies it to the luma histogram of the last 24 usable frames of the capture. A row is proposed when at least 1000 samples were taken, at least 1 % of them lie below 14 or above 237, and the format is labelled limited. The thresholds are 16 and 235 with a margin of 2, since a correct camera overshoots by a level or two.

- MJPEG is never proposed, because decoded JPEG is full range and labelled so.
- A mode whose label already says full range is never proposed either.
- A row lists the raw formats that tripped it, with the minimum, maximum and share as a comment.
- A format with no `PixelFormat` in the table (GREY, for one) gets a note in the output and no row, since `formats: &[]` would be an empty rule.

**Hidden controls (`ControlQuirk`).** The tool classifies a control from its sweep points:

- **Broken**: a write failed with `EPROTO` or `EIO`, or the mean U or V of some point moved by 25 code values or more from the baseline.
- **NoEffect**: at least two distinct values were written without error, and nothing measurable changed between them. The tolerances are floors, widened by twice the difference between two baseline captures taken before the sweep: mean Y within 1 code value, mean U and V within 1, contrast within 5 %, sharpness within 20 %. A change beyond any of them makes the verdict `effect`, and no row is proposed for the control.
- **unknown**: a write failed with another error, or fewer than two values were measured. No row is proposed. Read the reason.

A `Broken` row also lists, in `reset_at_open`, the broken controls whose default value was written without error during the sweep. A `NoEffect` row has an empty `reset_at_open`.

**Profile (`CameraProfile`).** `noisy` is true when the mid-tone temporal noise of the sweep's mode is above 1.0 code value (the 25th percentile figure stands in when the picture has no mid-tones, see above). `values` is empty unless `--recommend` names some: the tool measures what is wrong and does not choose settings.

Some absent rows are not verdicts:

- When no mode was analysed, the noise is not measured. The tool prints `// noisy: not measured` and proposes no `CameraProfile` row, rather than a `noisy: false` it never established.
- A raw mode without a frame analysis gets a note that its colour range was not measured.
- A run without a control sweep (`--quick`, or a failed baseline) gets a note that no control was judged.

## The tool's own tests

The pure parts are tested without a camera: the `v4l2-ctl` parsers, the timing and noise statistics, the range rule, the sweep classification, the Rust text and the sysfs walk.

```sh
python3 -m unittest discover -s scripts/tests -v
```

They need `numpy` and nothing else. A change to a threshold in the tool (the range margin, the 25 code value chroma jump, the noise limit) belongs in the same commit as the matching change to the daemon's rule or the table's documentation, because the tool and the daemon must keep one rule.
