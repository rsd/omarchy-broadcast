# CLI reference and JSON output (`omarchy-broadcast`)

`omarchy-broadcast` is the command-line remote control of `omarchy-broadcastd`. It talks to the daemon over the D-Bus session bus, through the interface `br.com.dias.OmarchyBroadcast1` described in [dbus-api.md](dbus-api.md). This document is the reference for its commands and, above all, for its JSON output: the interface for clients that cannot speak D-Bus, such as a QML status-bar plugin, which run `omarchy-broadcast watch --json` and read one JSON object per line. It is the contract the Omarchy bar widget of the omarchy-broadcast-plugin repository depends on: the widget consumes `watch --json` with schema `v` 1, and changes to the output follow the rules in [Versioning](#versioning). The design is recorded in [ADR-0011](adr/0011-daemon-lifecycle-and-dbus-api.md).

## Commands

| Command | Does | `--json` |
|---|---|---|
| `omarchy-broadcast status` | Prints the state, whether the camera is enabled, the active camera, the number of consumers, the output mode, the effect (with noise removal, from interface revision 4), and the error text if there is one. While the PipeWire output is configured, the consumers are shown with their split (`Consumers:  2 (loopback 1, PipeWire 1)`) and a `PipeWire:` line gives the output's state. | One `snapshot` object. |
| `omarchy-broadcast on` | Enables the camera (`SetEnabled(true)`), then prints the resulting state. | No. |
| `omarchy-broadcast off` | Disables the camera, entering the privacy state `off` (`SetEnabled(false)`), then prints the resulting state. | No. |
| `omarchy-broadcast cameras` | Lists the cameras in the daemon's order, the active one marked with `*`: name, USB `vid:pid` (`-` if none) and id. | One `cameras` object. |
| `omarchy-broadcast camera next` | Selects the next camera (`NextCamera()`), then prints the resulting camera. | No. |
| `omarchy-broadcast camera <selector>` | Selects a camera (`SelectCamera(selector)`): a camera id, a case-insensitive name substring, a USB `vid:pid` or a `/dev/videoN` path. Prints the resulting camera. | No. |
| `omarchy-broadcast effect` | Prints the video effect, the blur strength, the background image and, from interface revision 4, noise removal (`Effect`, `BlurStrength`, `BackgroundImage`, `Denoise`). Never starts the daemon. | One `snapshot` object. |
| `omarchy-broadcast effect none\|blur\|replace [--strength X] [--image PATH]` | Selects the video effect (`SetEffect`), then prints the resulting settings. See [Effect](#effect). | No. |
| `omarchy-broadcast denoise on\|off` | Turns video noise removal on or off (`SetDenoise`), then prints the resulting setting. See [Noise removal](#noise-removal). | No. |
| `omarchy-broadcast controls [--json]` | Prints the image controls of the active camera (brightness, contrast, saturation, sharpness, backlight compensation, those the camera has) with each value, range and default. Never starts the daemon. See [Camera image controls](#camera-image-controls). | One `controls` object. |
| `omarchy-broadcast controls set NAME VALUE` | Sets an image control of the active camera (`SetCameraControl`), then prints the controls read back. | No. |
| `omarchy-broadcast controls reset` | Forgets the values stored for the active camera and puts every control back to the camera's default (`ResetCameraControls`), then prints the controls. | No. |
| `omarchy-broadcast settings [--json] [--camera ID]` | Prints every setting of the active camera (or of `--camera`), by group: image, white balance, exposure, focus and zoom, other; those the camera has, each with its value, what it takes, `(stored)` and, for one that cannot be changed now, why. Never starts the daemon. See [Camera settings](#camera-settings). | The full set of `settings_group` lines, or one `settings_clear` line. |
| `omarchy-broadcast settings set [--camera ID] ID VALUE` | Sets one setting (`SetSetting`), then prints it as read back. | No. |
| `omarchy-broadcast settings reset [--camera ID] [--group GROUP \| --setting ID]` | Forgets the stored values of the camera, of a group or of one setting and puts them back to the camera's defaults (`ResetSettings`), then prints them. | No. |
| `omarchy-broadcast preview [--json] [--fps 15\|30]` | Holds a live preview lease on the daemon and prints a line for each frame it writes, until interrupted. See [Preview](#preview). | One object per line (NDJSON). |
| `omarchy-broadcast watch` | Follows the daemon: a summary when it connects, then a line for each change and each `Stats` signal (about once per second while `active`), until interrupted. | One object per line (NDJSON). |

The word `next` is always the `NextCamera` command; select a camera whose name contains "next" by its id or `vid:pid`. The selectors and the errors they can raise are those of `SelectCamera` in [dbus-api.md](dbus-api.md#selectcameras-selector).

The human-readable output is meant for people and may change between versions. Scripts and other programs use `--json`.

## Effect

`omarchy-broadcast effect` shows or selects the video effect described in [dbus-api.md](dbus-api.md#video-effects).

```text
omarchy-broadcast effect [none|blur|replace] [--strength X] [--image PATH]
```

- Without an effect it prints the current settings:

  ```text
  Effect:     blur
  Strength:   0.50
  Image:      (none)
  Denoise:    off
  ```

  The `Denoise:` line is printed from interface revision 4; `status` shows it after the effect lines in the same way.

- `none`, `blur` and `replace` select the effect. `--strength X` (0.0 to 1.0) is accepted with `blur` only and `--image PATH` with `replace` only; giving one with another effect is a usage error (status `2`), as is a strength outside 0 to 1 or an image that does not exist or is not a regular file.
- `PATH` is made absolute against the current directory, so the daemon, which has its own working directory, is always given an absolute path. The daemon requires a PNG or JPEG file; other files are rejected by the daemon (status `1`) or fail when the effect starts (the daemon's `Error` property says why).
- The options are applied before the effect is selected (`SetBlurStrength` and `SetBackgroundImage`, then `SetEffect`), so the effect starts with them. If an option is rejected, the effect is not selected. `omarchy-broadcast effect replace` without `--image` uses the image the daemon already has (the earlier choice or the configuration's); with none, the daemon rejects it with the message `the replace effect needs a background image: set one with SetBackgroundImage first`.
- The settings persist across restarts of the daemon and take precedence over `config.toml` ([dbus-api.md](dbus-api.md#video-effects)).
- Status `1` also covers the daemon's `InvalidArgument` error (an unacceptable value, with its message on stderr).
- Against a daemon of interface revision 1, which has no effects, the command (with or without an effect) prints `omarchy-broadcast: the daemon does not support effects (its interface revision is 1, effects need 2); update omarchy-broadcastd` and exits with status `1`. `effect --json` does not check: it prints the `snapshot`, whose `interface_revision` tells.
- Like `on`, `off` and `camera`, selecting an effect may start the daemon through D-Bus activation and gives up after 30 s; `effect` without an effect, like `status`, never starts it and gives up after 5 s.
- Strings that come from the daemon (the image path, camera names, `Error`) are printed in the human-readable output with control characters and bidirectional-text marks escaped (`\xNN`, `\u{NNNN}`), so that a hostile file name cannot move the cursor or reorder the line. The JSON output is escaped by JSON itself.

## Noise removal

`omarchy-broadcast denoise on|off` turns video noise removal on or off ([dbus-api.md](dbus-api.md#video-effects), [ADR-0018](adr/0018-video-noise-removal.md)). It is independent of the effect: it combines with `none`, `blur` and `replace`.

```text
omarchy-broadcast denoise on
Denoise:    on
```

- The setting persists across restarts of the daemon and takes precedence over `config.toml`. Setting the value already in force writes nothing.
- Like the other setters it may start the daemon through D-Bus activation and gives up after 30 s.
- Against a daemon of interface revision 3 or lower it prints `omarchy-broadcast: the daemon does not support noise removal (its interface revision is N, noise removal needs 4); update omarchy-broadcastd` and exits with status `1`.
- Noise removal fails open: if the stage cannot run, the stream continues without it and the daemon's `Error` text (shown by `status`) says why. The command itself succeeds, because the setting was applied.
- In `watch` without `--json`, the summary line gains ` | denoise on` and a change prints a `denoise: on` (or `off`) line, both from revision 4.

## Camera image controls

`omarchy-broadcast controls` shows and changes the image controls of the active camera ([dbus-api.md](dbus-api.md#camera-image-controls), [ADR-0020](adr/0020-camera-image-controls.md)): `brightness`, `contrast`, `saturation`, `sharpness` and `backlight_compensation`, those the camera has. Some cameras keep controls set by other software far from their defaults (an oversharpened, oversaturated picture); this is how a user adjusts them or puts them back, per camera. The daemon stores what the user sets and gives it to the camera whenever the camera opens.

```text
omarchy-broadcast controls [--json]
omarchy-broadcast controls set NAME VALUE
omarchy-broadcast controls reset
```

Without a subcommand it prints one line per control, with its value, range and default, `(stored)` marking a value the user set:

```text
Camera:     Logitech MX Brio (046d:0944)
  brightness  128  0..255  default 128
  contrast     77  0..255  default 128  (stored)
  sharpness   255  0..255  default 128
Applied live.
```

- **Where a change applies.** The last line says it: `Applied live.` while the daemon's state is `starting` or `active`, when the daemon has the camera open and writes the value to it at once; otherwise `The camera is not in use: changes apply the next time it opens.`, because the daemon leaves a closed camera untouched and only stores the value. A step other than 1 is shown (`0..100 step 2`). With no camera, or a camera that exposes none of the controls, the output is `No image controls.`
- `set NAME VALUE` takes one of the five names (any other is a usage error, status `2`) and a whole number, negative ones included (`controls set brightness -10`). The daemon checks the value against the range and the step the camera reports; one outside them is rejected with status `1` and the daemon's message on stderr, as is a control the camera does not have. On success the command prints the controls as read back, so a value the driver rounded shows as it is.
- `reset` forgets the stored values of the camera and writes every control its default: at once while the camera is open, else when it next opens.
- `--json` is accepted without a subcommand only; with `set` or `reset` it is a usage error (status `2`).
- The values persist across restarts of the daemon, for each camera; they are kept in the daemon's state file.
- Reading never starts the daemon and gives up after 5 s, like `status`. `set` and `reset` may start it through D-Bus activation, like the other setters, and give up after 30 s.
- Against a daemon of interface revision 5 or lower the command prints `omarchy-broadcast: the daemon does not support camera controls (its interface revision is N, camera controls need 6); update omarchy-broadcastd` and exits with status `1`; `controls --json` prints an `error` line with the code `unsupported-daemon` instead.
- If `set` or `reset` was accepted but reading the controls back failed, the command says so on stderr (`the change was made, but reading the controls back failed: ...`) and exits with the status of that failure.
- In `watch` without `--json`, a change of the controls prints a `camera controls: brightness 128, contrast 77 (stored)` line.
- Strings that come from the daemon are escaped in the human-readable output, as for `effect`.

## Camera settings

`omarchy-broadcast settings` shows and changes every setting the active camera offers: the five image controls of [`controls`](#camera-image-controls) and the rest of the standard controls (white balance, exposure, focus and zoom, anything else the camera has), and, on cameras that have it, the Logitech field of view ([dbus-api.md](dbus-api.md#camera-settings), [ADR-0023](adr/0023-settings-schema.md)). The daemon stores what the user sets, for each camera, and gives it to the camera whenever it opens.

```text
omarchy-broadcast settings [--json] [--camera=ID]
omarchy-broadcast settings set [--camera=ID] [--] ID VALUE
omarchy-broadcast settings reset [--camera=ID] [--group=GROUP | --setting=ID]
```

Without a subcommand it prints one line per setting under its group's label: the label, the value (with its unit), the setting's id, what `settings set` takes, `(stored)` for a value the user set and, in parentheses, the daemon's reason for a setting that cannot be changed now:

```text
Camera:     Logitech MX Brio (046d:0944)
Image
  Sharpness                 255          camera.image.sharpness                  0..255                    (stored)
Exposure
  Exposure mode             Auto         camera.exposure.auto_exposure           manual|aperture_priority
  Exposure time             156 ×100 µs  camera.exposure.exposure_time_absolute  3..2047                   (Set exposure to manual to change this)
  Keep 30 fps in low light  on           camera.exposure.keep_framerate          on|off
Applied live.
```

- **Where a change applies.** The last line says it, as for `controls`: `Applied live.` while the daemon's state is `starting` or `active` and the camera is the one in use; otherwise `The camera is not in use: changes apply the next time it opens.`
- **`--camera=ID`** names a camera by its id, as `cameras` lists it; the active camera when omitted. It goes before or after `set` and `reset`. With no camera, or a camera that exposes none of the settings the CLI can show, the output is `No camera settings.`
- **`set ID VALUE`** reads the tree to type the value, then sends `SetSetting`. `VALUE` is `on`, `off`, `true` or `false` for a switch; a whole number for a number, negative ones included (put `--` before the id, or write the value right after it: `settings set -- camera.focus.pan_absolute -3600`); an option's value id, as the listing shows them after the id (`manual`, `60hz`), for a choice; nothing, or `true`, for an action (`settings set camera.white_balance.do_white_balance`). A value that does not fit the setting's type is a usage error (status `2`) and nothing is sent. The daemon then checks the range, the step and whether the setting can be changed now; its rejection (`Disabled`, with its reason, `InvalidArgument`, `RateLimited`, `Failed`) exits with status `1` and the daemon's message on stderr. An id that is not in the camera's tree is refused the same way (`UnknownSetting`, status `1`) before anything is sent. On success the command prints the setting as read back, so a value the driver rounded shows as it is.
- **`reset`** puts back to the camera's defaults, and forgets the stored values of, every setting (no option), the settings of one group (`--group=exposure`) or one setting (`--setting=camera.image.sharpness`); the two options exclude each other. At once while the camera is open, else when it next opens. It prints the settings it reset, read back.
- `--json` is accepted without a subcommand only; with `set` or `reset` it is a usage error (status `2`), as for `controls`.
- Reading never starts the daemon and gives up after 5 s, like `status`. `set` and `reset` may start it through D-Bus activation and give up after 30 s.
- Against a daemon of interface revision 6 or lower the command prints `omarchy-broadcast: the daemon does not support camera settings (its interface revision is N, camera settings need 7); update omarchy-broadcastd` and exits with status `1`; `settings --json` prints an `error` line with the code `unsupported-daemon` instead.
- If `set` or `reset` was accepted but reading the settings back failed, the command says so on stderr (`the change was made, but reading the settings back failed: ...`) and exits with the status of that failure.
- `watch` without `--json` prints nothing about the settings.
- Strings that come from the daemon are escaped in the human-readable output, as for `effect`.

`controls` keeps working, with the five image controls only, and shares the storage of `settings`: a value set by one shows in the other. `controls reset` resets those five only; `settings reset` resets everything.

## Preview

`omarchy-broadcast preview` holds a live preview lease ([dbus-api.md](dbus-api.md#live-preview), [ADR-0019](adr/0019-live-preview.md)) and prints one line per frame the daemon writes, until it is interrupted. It is how a client that cannot speak D-Bus, such as the bar plugin, gets a preview of the virtual camera: the frames are JPEG files, and the lines say when to read them.

```text
omarchy-broadcast preview [--json] [--fps 15|30]
```

- `--fps` is the frame rate asked for, 15 (the default) or 30. Any other value is a usage error (status `2`). The daemon may write at a higher rate while another client holds a faster lease.
- The preview counts as an application using the camera: while the command runs, the camera starts if nothing else keeps it running, and it stops after the idle grace once the command ends and no other consumer remains. It works in every state of the daemon (the frames are black while the camera is not streaming).
- The command never starts the daemon: every call carries `NO_AUTO_START`. It subscribes to the frame signal before it takes the lease, so no frame is missed.
- Without `--json`, the lines are for people and may change: `preview: <path> at 15 fps` when the lease is held, `frame <seq>` for each frame, `preview: stopped` when the lease is lost, and the outage message followed by `; waiting for it`.
- Frames and lease are reported as described in [`preview` and `frame`](#preview-and-frame) below. The path is of the daemon's frame file, `$XDG_RUNTIME_DIR/omarchy-broadcast/preview.jpg`. A client opens it when a `frame` line arrives; each frame replaces the file by a rename, so a read never sees a partial frame.
- **Outages** are handled like `watch` handles them: one `error` line per outage with the usual codes, the daemon's return noticed at once from `NameOwnerChanged` and re-checked every 2 s, and the lease taken again when the daemon is back (a new `preview` line with `"active":true`). A lease that ended because the daemon or the bus went away is announced with `"active":false` first, then the error line.
- A daemon below interface revision 5 has no preview: one `error` line with the code `unsupported-daemon` is printed, and the command keeps waiting for another daemon instance to take the name.
- A daemon whose frame path does not fit a JSON line (see [Line length](#general-rules)) is refused: the command releases the lease and prints one `error` line with the code `preview-path-too-long`, then waits for another daemon instance. Shorten the `XDG_RUNTIME_DIR` of the daemon. The path is never clipped, because a clipped path names no file.
- **Ending.** The command exits with status `0`, after it releases the lease (best effort, within 1 s), on `SIGTERM`, `SIGINT`, `SIGHUP`, `SIGPIPE`, or stdout reporting an error or hang-up. After `kill -9`, or any other end of the process, the lease ends with its bus connection and the daemon releases it. A failed write to stdout other than a closed pipe exits with status `4`.
- Once started, `preview` keeps waiting through outages: it exits only for the reasons above, never because the daemon is absent.

## Starting the daemon

- `status`, `cameras`, `watch`, `preview`, `effect` without an effect, and `controls` and `settings` without a subcommand never start the daemon. They read the properties with `org.freedesktop.DBus.Properties.GetAll` carrying the `NO_AUTO_START` flag, so looking at the daemon never launches it (`controls` also calls `GetCameraControls`, and `settings` calls `GetSettings`, with that flag). This is the read that the `obc-ipc` crate provides to Rust clients (`Snapshot::fetch_no_autostart`); [dbus-api.md](dbus-api.md#service-presence-and-activation) gives the `busctl` and `gdbus` equivalents.
- `on`, `off`, `camera`, `effect <effect>`, `denoise`, `controls set`, `controls reset`, `settings set` and `settings reset` call methods without that flag: when the D-Bus activation file `br.com.dias.OmarchyBroadcast.service` is installed, the call starts the daemon, as `systemctl --user start omarchy-broadcastd` would. `off` on a stopped daemon therefore starts it and records the privacy state, which persists across restarts. Without an activation file, or if the start fails, the command reports that the daemon is not running.

When the daemon is not running, the human-readable commands print one line to stderr, for example:

```text
omarchy-broadcast: omarchy-broadcastd is not running (no owner for br.com.dias.OmarchyBroadcast on the session bus); start it with: systemctl --user start omarchy-broadcastd
```

## Exit status

| Status | Meaning |
|---|---|
| `0` | Success. Also the status of `watch` when it ends on `SIGTERM`, `SIGINT`, `SIGPIPE` or a closed stdout, and of `preview` when it ends on `SIGTERM`, `SIGINT`, `SIGHUP`, `SIGPIPE` or a closed stdout. |
| `1` | The daemon rejected the request: one of the `br.com.dias.OmarchyBroadcast1.Error` errors (`NoSuchCamera`, `InvalidConfig`, `Unavailable`, `InvalidArgument`, `Failed`, and from revision 7 `UnknownSetting`, `Disabled`, `RateLimited`), or the daemon does not support the request (`effect` against interface revision 1, `denoise` against a revision below 4, `controls` against a revision below 6, `settings` against a revision below 7). Its message is printed to stderr. |
| `2` | Invalid command line. |
| `3` | The daemon is not running (its name has no owner, or D-Bus could not start it), or there is no D-Bus session bus. |
| `4` | Any other D-Bus failure (an unexpected reply, a lost connection), a timeout, or a failed write to stdout. |

`status`, `cameras`, `controls` and `settings` give up after 5 s, and `on`, `off`, `camera`, `effect <effect>`, `denoise`, `controls set`, `controls reset`, `settings set` and `settings reset` after 30 s (time for the daemon to be started), with status `4`. `watch` gives each read of the properties 5 s as well (see [Daemon absent or gone](#daemon-absent-or-gone)). `omarchy-broadcast --help` lists the exit statuses.

## JSON output

### General rules

- Every object is printed on one line, terminated by `\n`, in UTF-8. `watch --json` flushes stdout after every line.
- Every object has the field **`v`**, the schema version (an integer, `1` in this version), and the field **`type`**, a string naming the line type: `snapshot`, `state`, `cameras`, `controls`, `settings_group`, `settings_delta`, `settings_clear`, `stats`, `preview`, `frame` or `error`.
- The order of the fields within an object is not significant.
- A client must ignore fields it does not know, and lines whose `type` it does not know.
- **Line length.** No line is longer than **4096 bytes**, newline excluded. To guarantee it, strings are bounded in bytes of their JSON encoding (escapes included). A longer string is cut at a character boundary and ends with the ellipsis character `…` (U+2026); the result is always valid JSON. The bounds:

  | Field | Bound (bytes) |
  |---|---|
  | `error` | 512 |
  | `active_camera`, camera `id` | 96 |
  | `active_camera_name`, camera `name` | 64 |
  | camera `usb` | 16 |
  | `output_mode` | 64 |
  | `effect` | 32 |
  | `background_image` | 256 |
| image control `name` | 32 |
| setting `id`, a condition's `id` | 48 |
| settings group `id` | 32 |
| setting and group `label` | 48 |
| setting `disabled_reason` | 96 |
| setting `unit` | 16 |
| setting `hint` | 16 |
| an enum's `value`, `default`, an option `value`, a string operand of a condition | 32 |
| option `label` | 32 |
| condition `op` | 8 |

- **Clipping of a path.** `error` and the other strings keep their beginning and lose their end. `background_image` is the exception: the end of a path, the file name, is what identifies it, so a path over the bound is clipped at the **start** and begins with `…` (for example `…/Pictures/office/background-2026.jpg`). The bound is 256 bytes, not 512 as for `error`, because a line that carries many cameras and a long path must stay within the 4096-byte cap. Do not use a clipped path as an argument: read it back from the daemon.
- **Camera list.** At most **12** cameras are listed; `cameras_truncated` is `true` when the daemon reported more. The list keeps the daemon's order, so the first 12 are listed. A clipped camera id can no longer be passed to `omarchy-broadcast camera`; real ids are far shorter than the bound.
- **Control list.** At most **16** image controls are listed; `controls_truncated` is `true` when the daemon reported more (a daemon reports at most five). The names are bounded as in the table above.
- **Settings lists.** An enum setting lists at most **16** options, a setting at most **4** `enabled_when` conditions, an `in` condition at most **8** operands (a list inside a list is printed empty), a `settings_group` line at most **8** settings and a `settings_delta` line at most **16** changes. A longer list is cut, in order. The lines are also packed by their encoded length, so no line passes the cap (see [`settings_group`](#settings_group)).
- **A `snapshot` over the cap.** Twelve cameras with every string at its bound, and image controls with theirs, do not fit 4096 bytes together. Strings at those bounds do not occur in practice, but the guarantee holds: an over-cap `snapshot` gives up its controls first, one at a time (`controls` ends empty and `controls_truncated` is `true`), and only then its cameras (`cameras_truncated` is `true`). With realistic ids and names, twelve cameras and the controls fit. The two settings fields of a `snapshot` take the room of one camera in that worst case: with every string at its bound and no controls, a `snapshot` lists 11 of the 12 cameras (`cameras_truncated` is `true`), and a `cameras` line all 12.

### State strings

`state` is one of `off`, `idle`, `starting`, `active` and `error`, the lifecycle states of [dbus-api.md](dbus-api.md#lifecycle-states). The set is fixed for schema version 1.

### `snapshot`

Every property of the daemon at one moment. Printed by `status --json`, and by `watch --json` first and after every reconnection.

| Field | JSON type | Meaning |
|---|---|---|
| `v` | number | `1`. |
| `type` | string | `"snapshot"`. |
| `state` | string | Lifecycle state, see State strings. |
| `enabled` | boolean | `false` in the privacy state `off`. |
| `error` | string | Text of the latest error, from the lifecycle or a configuration reload; `""` if none. Can be non-empty in any state: a rejected configuration reload sets it while the state stays as it was. |
| `active_camera` | string | Id of the camera that streams, or would stream if an application opened the virtual camera now; `""` if none. |
| `active_camera_name` | string | Name of that camera; `""` if there is none or it is not in the camera list. |
| `consumers` | number | Number of consumers of the virtual camera: the processes holding the loopback plus the links to the PipeWire node plus the preview leases (integer, 0 or more). With the PipeWire output off and no preview lease, only the first. |
| `loopback_consumers` | number | Number of processes, other than the daemon, holding the loopback device open (integer, 0 or more). From a daemon of interface revision 2 or lower, the value of `consumers`. |
| `pipewire_consumers` | number | Number of links to the PipeWire output's node (integer, 0 or more); `0` when the output is off or the daemon is older than revision 3. |
| `preview_consumers` | number | Number of preview leases held (integer, 0 or more); `0` from a daemon older than revision 5. |
| `pipewire_output` | string | State of the optional PipeWire output: `"off"`, `"unsupported"`, `"connecting"`, `"ready"`, `"streaming"` or `"failed"` (see [dbus-api.md](dbus-api.md#properties)); `"off"` from a daemon older than revision 3. Bounded to 32 bytes and clipped at its end, like the other bounded strings (see above). |
| `output_mode` | string | What the virtual camera delivers, e.g. `"NV12 1920x1080@30"`. |
| `effect` | string | The video effect in force: `"none"`, `"blur"` or `"replace"`. The effective value; it does not say whether the effect is ready or has failed (see `state` and `error`). |
| `blur_strength` | number | Strength of the blur effect, 0.0 to 1.0, also when `effect` is not `"blur"`. A value outside the range is clamped to it, and a value that is not a finite number is printed as `0`. `0` also when the daemon is too old to have the property (`interface_revision` 1). |
| `background_image` | string | Absolute path of the image used by `"replace"`; `""` if none. Bounded to 256 bytes and clipped at its start (see above). |
| `denoise` | boolean or null | Whether video noise removal is on (a stage that combines with any effect). `null`, with the key present, from a daemon below interface revision 4, which has no noise removal; the older fields take a default value instead (see Versioning). |
| `interface_revision` | number | Revision of `br.com.dias.OmarchyBroadcast1` the daemon implements. |
| `cameras` | array | The cameras present, in the daemon's order, as camera objects (below). |
| `cameras_truncated` | boolean | `true` if more cameras were present than listed. |
| `camera_controls` | object or null | The image controls of the active camera, as in the [`controls`](#controls) line without its `v` and `type`: `camera`, `controls` and `controls_truncated`. `null`, with the key present, from a daemon below interface revision 6. |
| `settings_revision` | number or null | The daemon's `SettingsRevision`, the counter of its camera settings tree (integer, from 1). `null`, with the key present, from a daemon below interface revision 7. |
| `settings_schema` | number or null | The settings schema version the settings lines follow (`1` in this version; see [Settings schema version](#settings-schema-version)). `null`, with the key present, from a daemon below interface revision 7, which has no settings. A client treats a missing key as `null`: an older CLI does not print it. |

A **camera object**:

| Field | JSON type | Meaning |
|---|---|---|
| `id` | string | Opaque, stable identifier, accepted by `omarchy-broadcast camera`. |
| `name` | string | Human-readable device name. |
| `usb` | string | USB `vid:pid` in lower-case hex (`"046d:0944"`), `""` for a non-USB camera. |
| `active` | boolean | `true` for the camera named by `active_camera`. |

### `state`

Printed by `watch --json` when one or more of the scalar fields change. It carries **the current value of every scalar field**, so a client can replace its whole state with it, and names the fields that changed.

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"state"`. |
| `changed` | array of strings | The fields whose value changed, from: `state`, `enabled`, `error`, `active_camera`, `active_camera_name`, `consumers`, `loopback_consumers`, `pipewire_consumers`, `preview_consumers`, `pipewire_output`, `output_mode`, `effect`, `blur_strength`, `background_image`, `denoise`, `interface_revision`. Never empty. |
| `state`, `enabled`, `error`, `active_camera`, `active_camera_name`, `consumers`, `loopback_consumers`, `pipewire_consumers`, `preview_consumers`, `pipewire_output`, `output_mode`, `effect`, `blur_strength`, `background_image`, `denoise`, `interface_revision` | | As in `snapshot`, current values. |

A `state` line does not carry the camera list, `camera_controls`, `settings_revision` or `settings_schema`. `active_camera_name` can change without `active_camera` (the active camera was renamed, or appeared in or left the list); it is then reported in `changed` on its own.

### `cameras`

The camera list. Printed by `cameras --json`, and by `watch --json` when the list changes.

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"cameras"`. |
| `active_camera` | string | As in `snapshot`. |
| `cameras` | array | Camera objects, as in `snapshot`. |
| `cameras_truncated` | boolean | As in `snapshot`. |

When the daemon announces changes to scalar fields and to the camera list in one notification (for example when the active camera is unplugged), `watch --json` prints the `state` line first, then the `cameras` line, then the `controls` line if the controls changed too.

### `controls`

The image controls of one camera. Printed by `controls --json`, and by `watch --json` when the controls of the active camera change.

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"controls"`. |
| `camera` | string | Id of the camera the controls belong to: the active camera. `""` when there is none. Bounded like a camera `id`. |
| `controls` | array | The controls the camera has, in the order brightness, contrast, saturation, sharpness, backlight_compensation, as control objects (below). Empty with no camera, or when the camera exposes none. |
| `controls_truncated` | boolean | `true` if more controls were present than listed. |

A **control object**:

| Field | JSON type | Meaning |
|---|---|---|
| `name` | string | The control's name as `v4l2-ctl` calls it: `brightness`, `contrast`, `saturation`, `sharpness` or `backlight_compensation`. |
| `min`, `max` | number | The range the camera reports (integers, negative values possible). |
| `step` | number | The distance between two valid values, counted from `min`. A value must be on it to be accepted. |
| `default` | number | The camera's own default. |
| `value` | number | For the camera that is open (state `starting` or `active`), the value on the device. Otherwise the value it will be given when it opens: the user's stored value, else the default if a reset is pending, else the value on the device. |
| `stored` | boolean | `true` if the user set a value for this control; the daemon gives it to the camera whenever it opens. |

`watch --json` prints a `controls` line when the property that carries them changes: the camera, or a control's range, default, value or `stored`. The daemon refreshes the controls when the active camera changes, when the cameras are rescanned, when the camera opens, once at its first frame, and after a set or reset; it does not poll the device, so a value changed by other software shows at the next of those moments.

### `settings_group`

The settings of one camera, one group at a time. Printed by `settings --json` (the full set) and by `watch --json` (after the `snapshot`, and again whenever the structure changes). The tree is a **set** of lines, because the settings of a camera do not fit one line of 4096 bytes.

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"settings_group"`. |
| `schema` | number | The settings schema version, `1`. |
| `rev` | number | The daemon's `SettingsRevision` when the set was read. |
| `serial` | number | The daemon's `SettingsSchemaSerial` when the set was read. |
| `camera` | string | Id of the camera the settings belong to: the active camera (or `--camera`). Bounded like a camera `id`. |
| `index` | number | Position of this line in the set, from 0. |
| `count` | number | The number of lines in the set (at least 1). |
| `part`, `parts` | number | A group that does not fit one line is cut over several, repeating `group.id` and `group.label`: this line is part `part` of `parts`, from 1. `1` and `1` when the group is whole. Always present. |
| `group` | object | `id`, `label` and `settings`, an array of setting objects (below). |

The lines of one set are consecutive, share `rev`, `serial` and `camera`, carry `index` from 0 to `count - 1` and `count` the same, and follow the daemon's group order, the parts of a group together and in order. A **packing rule** keeps each line within the cap: settings go into a line one after the other, in order, until it holds 8 or the next would pass 4096 bytes; the next line takes up from there. A single setting at every one of its bounds fits a line alone.

A **setting object** has the keys of its type only:

| Field | JSON type | Present for | Meaning |
|---|---|---|---|
| `id` | string | all | The setting's id, `camera.<group>.<name>`. |
| `label` | string | all | The name to show. |
| `type` | string | all | `"bool"`, `"int"`, `"enum"` or `"action"`. |
| `value` | boolean, number or string | `bool`, `int`, `enum` | The current value: for an `enum`, an option's `value`. For a camera that is not open, the value it will have when it opens. |
| `default` | as `value` | `bool`, `int`, `enum` | The camera's own default. |
| `stored` | boolean | all | The user set a value for this setting on this camera. |
| `enabled` | boolean | all | The setting can be changed now. Computed by the daemon; draw it as given. |
| `disabled_reason` | string | all | Why not, when `enabled` is `false`; `""` otherwise. |
| `enabled_when` | array | all | The daemon's rule, for display, as objects `{"id", "op", "value"}`: the setting `id` it depends on, `op` (`"eq"`, `"ne"` or `"in"`) and the operand (a boolean, number or string, or for `"in"` an array of them). `[]` when there is none. |
| `min`, `max`, `step` | number | `int` | The range and the distance between two valid values, counted from `min`. |
| `unit` | string | `int`, optional | Shown after the value (`"K"`, `"×100 µs"`). |
| `options` | array | `enum` | Objects `{"value", "label"}`, in display order. |
| `hint` | string | optional | A widget suggestion: `"slider"`, `"switch"`, `"segmented"`. |

An `action` has no `value` or `default`. A setting whose `type` the CLI does not know is left out of the line, and a group with nothing left prints no line; a camera for which nothing is left gives a `settings_clear` line.

A client keeps the order, ignores keys it does not know, skips a setting whose `type` it does not know, and trusts `enabled` and `disabled_reason` without evaluating `enabled_when`.

### `settings_delta`

Changes to a set already assembled. Printed by `watch --json` for each `SettingsChanged` signal.

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"settings_delta"`. |
| `schema` | number | `1`. |
| `rev`, `serial` | number | The daemon's `SettingsRevision` and `SettingsSchemaSerial` after the change. |
| `camera` | string | Id of the camera the set belongs to. |
| `changes` | array | One object per setting that changed: `id`, `value` (absent for an action), `stored`, `enabled` and `disabled_reason`, as in the setting object. |

At most 16 changes go into a line, and they are packed by length like the settings; a larger change (a reset of a whole camera) is several lines with the same `rev`. A delta changes a value, `stored`, `enabled` or a reason, never a structure: when the structure changes, `watch --json` prints a full set instead of a delta.

### `settings_clear`

`{"v":1,"type":"settings_clear","rev":R,"serial":S}`: there is nothing to show, because there is no camera, or no setting the CLI can show. A client drops its settings.

### Client assembly rules

- **Assembling a set.** Collect `settings_group` lines of one `rev`, `serial` and `camera`, and **commit** the set when all `count` lines have arrived (their `index` values 0 to `count - 1`, each once). Show nothing of a half-assembled set. A line with another `rev`, `serial` or `camera` while a set is incomplete drops the incomplete one and starts a new one with that line.
- **A committed set replaces everything.** The previous tree, whatever its camera, is gone. This is also the case after a new `snapshot`: a new set follows it.
- **Applying a delta.** A `settings_delta` applies to the committed set when its `serial` is the committed one and its `rev` is the committed `rev` (a further line of the same change) or the next. Deltas carry absolute values, so applying one twice is harmless. A delta of another `serial` is ignored: a new set follows it. A delta whose `rev` is older than the committed one is stale and ignored. A delta that skips a revision cannot occur from `watch --json` (it reads the tree again and prints a full set first); a client that sees one waits for the next set.
- **`settings_clear`** means nothing to show: drop the set and show no settings. It is a state, like a set: a later set replaces it.
- **Nothing below revision 7.** A daemon below interface revision 7 prints no settings lines, and `settings_schema` is `null`.

`watch --json` prints the set after the `snapshot` when the daemon has settings, then a `settings_delta` for each change. When the active camera changes the `serial` moves and a new set is printed. When a signal was missed, or the revision wrapped past its largest value, the CLI reads the tree again and prints a new set. After an outage, the new `snapshot` is followed by a new set.

### Settings schema version

The `schema` field is the version of the **settings schema**: the meaning of the setting objects. It is not `v` (the schema of every JSON line) and not the daemon's `InterfaceRevision`.

- **Additive changes keep `schema` at 1**: a new key in a setting object, a new setting `type`, a new `hint`, a new group. A client ignores keys it does not know and skips a setting whose `type` it does not know; it shows nothing for it and never fails. A rule that depends on a skipped setting is treated as holding: the client has `enabled` anyway.
- **A change of meaning raises `schema`**: for example a new `enabled_when` operator that an old client would read as always true, or a change of a unit. A client that sees a `schema` higher than it knows shows no settings and says that it needs an update (it does not render the tree); the rest of its output is unaffected.
- `enabled_when` operators are a closed set (`eq`, `ne`, `in`). An unknown operator is the reason `enabled` is the daemon's, not the client's.

### `stats`

One `Stats` signal of the daemon, printed by `watch --json`. The daemon sends it about once per second while `state` is `active`, and never in other states.

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"stats"`. |
| `fps` | number | Frames delivered to the virtual camera per second over the last interval, camera and black placeholder frames alike (the `fps` of the `Stats` signal, [dbus-api.md](dbus-api.md#statsd-fps-d-latency_p50_ms-d-cpu_percent-d-gpu_percent)). |
| `latency_p50_ms` | number | Median latency from camera capture to the virtual camera, in milliseconds. |
| `cpu_percent` | number | CPU time used by the daemon, in percent of one core (can exceed 100). |
| `gpu_percent` | number | Utilisation of the GPU the pipeline runs on, in percent. |

A value that cannot be measured is **`-1`**: show it as unknown, not as zero. Every number is either `-1` or a finite value of 0 or more; they are JSON numbers with a fractional part, e.g. `30.0`.

### `preview` and `frame`

Printed by `preview --json` only.

A **`preview`** line says whether the lease is held:

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"preview"`. |
| `active` | boolean | `true` once the lease is held; `false` when it is lost (the daemon or the session bus went away). |
| `fps` | number | The frame rate asked for, 15 or 30. Only with `"active":true`. |
| `path` | string | Absolute path of the frame file. Only with `"active":true`. |

A **`frame`** line says the file was replaced:

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"frame"`. |
| `seq` | number | The frame's sequence number (integer), strictly increasing within a daemon run, from 1. |
| `path` | string | Absolute path of the frame file, as in `preview`. |

The `path` of these two lines is never clipped, unlike the other strings (see [General rules](#general-rules)): a client opens that file, and a clipped path names none. Its bound is 3968 bytes of JSON encoding, so that the lines stay within the 4096-byte cap; a daemon that reports a longer path is refused with the code `preview-path-too-long` (see [`error`](#error)). Real paths, under `$XDG_RUNTIME_DIR`, are far shorter.

### `error`

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"error"`. |
| `code` | string | `"daemon-not-running"`: the daemon is not running, or there is no D-Bus session bus. `"dbus-error"`: any other failure to talk to the daemon (an unexpected reply, a lost connection, a timeout). `"unsupported-daemon"`: the daemon is too old for the request (`preview` needs interface revision 5, `controls` needs 6, `settings` needs 7); printed by `preview --json`, `controls --json` and `settings --json` only. `"preview-path-too-long"`: the daemon's frame path does not fit a line; printed by `preview --json` only. New codes may be added; treat an unknown code like `"dbus-error"`. |
| `error` | string | A one-line explanation for people. Match on `code`, never on this text. |

`status --json`, `cameras --json`, `controls --json` and `settings --json` print an `error` object instead of their usual one when they fail to read the daemon, and exit with the status of the failure (`3` or `4`).

## `watch --json`

The output is a stream of the line types above:

1. A `snapshot` line, once the daemon has been read. The subscriptions to its signals are made before the read, so no change is lost in between.
   From a daemon of interface revision 7, the snapshot is followed by the full set of settings of the active camera (the `settings_group` lines, or one `settings_clear`; see [Client assembly rules](#client-assembly-rules)).
2. Then, as events arrive (no polling): a `state` line per change of the scalar fields, a `cameras` line per change of the camera list, a `controls` line per change of the image controls of the active camera (from a daemon of interface revision 6), a `settings_delta` line per change of a camera setting (from revision 7), a new set of `settings_group` lines (or a `settings_clear`) when the structure of the settings changes, and a `stats` line per `Stats` signal. A change notification that changes no value prints nothing.

### Daemon absent or gone

- If the daemon is not running when `watch` starts (or there is no session bus), the first line is an `error` line with `"code":"daemon-not-running"` instead of a snapshot.
- If the daemon exits while `watch` runs (its bus name loses its owner), `watch` prints one `error` line with `"code":"daemon-not-running"`.
- **One error line per outage.** While the daemon stays absent, nothing more is printed. `watch` notices the daemon's return at once from `org.freedesktop.DBus.NameOwnerChanged`, and also re-checks every 2 s; a lost session bus is reconnected every 2 s.
- When the daemon is back, `watch` prints a new `snapshot` line, which replaces everything the client knew. If the daemon is replaced by a new instance without a gap, the new snapshot is printed without an error line before it.
- If the daemon is present but its properties cannot be read, an `error` line with `"code":"dbus-error"` is printed (once) and the read is retried every 2 s. This includes a daemon that holds its bus name but does not answer: each read is given up after 5 s. When a read succeeds again, a new `snapshot` line follows.
- `watch` never exits because of the daemon's absence, and never starts the daemon.

A client should treat the state after a `daemon-not-running` line as "daemon not running" (show it as unavailable, not as `off`) until the next `snapshot`.

### Ending

`watch` exits with status `0`, without printing anything, on:

- `SIGTERM` or `SIGINT`;
- `SIGPIPE`, or `EPIPE` on a write to stdout;
- stdout reporting an error or hang-up: when stdout is a pipe whose reader has gone, `watch` exits at once, even if it has nothing to print. `omarchy-broadcast watch --json | head -1` ends as soon as `head` does.

## `preview --json`

The output is a stream of `preview`, `frame` and `error` lines:

1. Nothing until the lease is held (or the attempt fails). Then a `preview` line with `"active":true`, the rate and the path.
2. A `frame` line per frame the daemon writes, as the signals arrive.
3. When the lease is lost, a `preview` line with `"active":false`. When the daemon is back, a new lease and a new `"active":true` line.
4. An `error` line for each outage, once (see [Preview](#preview)).

A client treats `"active":false`, and any `error` line, as "no frames now", and keeps the last frame only if it wants to. It does not need to reconnect: the command does.

## Versioning

- `v` is the schema version of every line. It is `1` in this version.
- **`v` stays 1 for the effect fields.** `blur_strength` and `background_image` are new fields in existing line types, `effect` now takes the values `blur` and `replace` in addition to `none`, and `changed` may name the two new fields. None of this removes or renames a field or changes the type or meaning of one, so it is not breaking under the rule below. The set of `state` strings is unchanged. A client written for the earlier version keeps working: it ignores the new fields and shows an `effect` string it does not know as text.
- **`v` stays 1 for the PipeWire fields.** `loopback_consumers`, `pipewire_consumers` and `pipewire_output` are new fields in existing line types, and `changed` may name them. `consumers` keeps its type; with the PipeWire output off it has the value it had before. Against a daemon of interface revision 2 or lower the fields are still printed, with the values of a daemon that has no PipeWire output (`loopback_consumers` equal to `consumers`, `pipewire_consumers` 0, `pipewire_output` `"off"`).
- **`v` stays 1 for `denoise`.** It is a new field in the existing `snapshot` and `state` lines, and `changed` may name it. It differs from the earlier additions in one respect: against a daemon of interface revision 3 or lower the key is still printed, but its value is `null`, not a default, because "off" would claim a setting the daemon does not have. A client that treats a missing or `null` value as "not available" is correct for both. `true` and `false` mean the daemon reports the setting.
- **`v` stays 1 for the preview.** `preview_consumers` is a new field in the existing `snapshot` and `state` lines, and `changed` may name it; `consumers` keeps its type, and with no preview lease held it has the value it had before. Against a daemon of interface revision 4 or lower the key is still printed, with the value `0`, as for the PipeWire fields. `preview` and `frame` are new line types printed by a new command, and `unsupported-daemon` and `preview-path-too-long` are new `error` codes; a client written for the earlier version never sees them, as it never runs `preview`.
- **`v` stays 1 for the camera controls.** `camera_controls` is a new field in the existing `snapshot` line and `controls` is a new line type, printed by a new command and by `watch --json`; the new `unsupported-daemon` use (`controls` needs revision 6) is an existing code. `state` lines do not carry the controls, so `changed` never names them. Against a daemon of interface revision 5 or lower the `snapshot` still prints the key, with the value `null`, as for `denoise`: "no controls" would claim a camera without them. `watch --json` prints no `controls` line then. A client written for the earlier version ignores both.
- **`v` stays 1 for the camera settings.** `settings_revision` and `settings_schema` are new fields in the existing `snapshot` line (not in `state`, so `changed` never names them); `settings_group`, `settings_delta` and `settings_clear` are new line types, printed by a new command and by `watch --json`; `unsupported-daemon` for `settings` is an existing code. Against a daemon of interface revision 6 or lower the `snapshot` still prints both keys, with the value `null`, as for `camera_controls`, and `watch --json` prints no settings line. A client written for the earlier version ignores all of it. The meaning of the setting objects has a version of its own, `schema` (see [Settings schema version](#settings-schema-version)).
- A **breaking change** raises `v`: a removed or renamed field or line type, a changed JSON type or meaning, a new state string, or a change to the rules above (line cap, outage behaviour).
- **Not breaking**, made without raising `v`: a new field in an existing line type, a new line type, a new `error` code. Clients ignore what they do not know.
- The schema is independent of the D-Bus `InterfaceRevision`; a new D-Bus property may appear as a new field.
- **Capability rules for clients.** A client that shows the split of the consumers does so only while `pipewire_output` is not `"off"`. A client offers the effect controls only if both hold: the CLI is new enough to print `blur_strength` (the field is present as a number in `snapshot`; an older CLI has no `effect` command, and `omarchy-broadcast effect` fails with status `2`), and the daemon's `interface_revision` is 2 or more. Offer the noise-removal switch only if `denoise` is a boolean and `interface_revision` is 4 or more. Offer the preview only if the CLI has the `preview` command (an older CLI exits with status `2`) and the daemon's `interface_revision` is 5 or more; `preview --json` reports the second condition itself, with `unsupported-daemon`. Check the CLI first, then the daemon; the two conditions have different remedies (update `omarchy-broadcast`, update `omarchy-broadcastd`). Offer the camera controls only if `camera_controls` is an object and `interface_revision` is 6 or more; with `camera_controls` an empty list, offer none. Edits apply live while `state` is `starting` or `active`, and otherwise are stored for the next time the camera opens, so a client may tell its user which. To change a control run `omarchy-broadcast controls set NAME VALUE` (an older CLI exits with status `2`) and read the result from the `controls` line of `watch`. Offer the generic camera settings only if `interface_revision` is 7 or more and `settings_schema` is a number the client knows (1 in this version); with `settings_schema` `null` the daemon is too old (update `omarchy-broadcastd`), with the key missing the CLI is too old (update `omarchy-broadcast`), and with a number above the client's own the client is too old (update it); in all three, show the camera and no settings, with that hint. To change a setting run `omarchy-broadcast settings set --camera=ID -- SETTING VALUE` and `omarchy-broadcast settings reset --camera=ID` (an older CLI exits with status `2`), naming the camera the settings were drawn for so that a click after a change of camera fails instead of writing to another camera, and read the result from the `settings_delta` lines of `watch`. When the field is absent or the revision is lower, show the effect, if at all, as read-only text.

## Examples

Values are illustrative; camera ids are opaque and differ between machines.

```sh
omarchy-broadcast status --json
```

```json
{"v":1,"type":"snapshot","state":"idle","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":0,"loopback_consumers":0,"pipewire_consumers":0,"pipewire_output":"off","output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","denoise":false,"interface_revision":4,"cameras":[{"id":"usb-046d_0944-video-index0","name":"Logitech MX Brio","usb":"046d:0944","active":true},{"id":"pci-0000:00:14.0-usb-0:5:1.0-video-index0","name":"FHD Camera","usb":"5986:1193","active":false}],"cameras_truncated":false}
```

```sh
omarchy-broadcast cameras --json
```

```json
{"v":1,"type":"cameras","active_camera":"usb-046d_0944-video-index0","cameras":[{"id":"usb-046d_0944-video-index0","name":"Logitech MX Brio","usb":"046d:0944","active":true},{"id":"pci-0000:00:14.0-usb-0:5:1.0-video-index0","name":"FHD Camera","usb":"5986:1193","active":false}],"cameras_truncated":false}
```

A `watch --json` session: an application opens the virtual camera, the stream starts, the daemon is restarted.

```sh
omarchy-broadcast watch --json
```

```json
{"v":1,"type":"snapshot","state":"idle","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":0,"loopback_consumers":0,"pipewire_consumers":0,"pipewire_output":"off","output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","denoise":false,"interface_revision":4,"cameras":[{"id":"usb-046d_0944-video-index0","name":"Logitech MX Brio","usb":"046d:0944","active":true}],"cameras_truncated":false}
{"v":1,"type":"state","changed":["consumers","loopback_consumers"],"state":"idle","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":1,"loopback_consumers":1,"pipewire_consumers":0,"pipewire_output":"off","output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","denoise":false,"interface_revision":4}
{"v":1,"type":"state","changed":["state"],"state":"starting","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":1,"loopback_consumers":1,"pipewire_consumers":0,"pipewire_output":"off","output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","denoise":false,"interface_revision":4}
{"v":1,"type":"state","changed":["state"],"state":"active","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":1,"loopback_consumers":1,"pipewire_consumers":0,"pipewire_output":"off","output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","denoise":false,"interface_revision":4}
{"v":1,"type":"stats","fps":30.0,"latency_p50_ms":41.2,"cpu_percent":6.5,"gpu_percent":18.0}
{"v":1,"type":"stats","fps":30.0,"latency_p50_ms":40.8,"cpu_percent":6.4,"gpu_percent":-1.0}
{"v":1,"type":"error","code":"daemon-not-running","error":"omarchy-broadcastd is not running (no owner for br.com.dias.OmarchyBroadcast on the session bus); start it with: systemctl --user start omarchy-broadcastd"}
{"v":1,"type":"snapshot","state":"idle","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":0,"loopback_consumers":0,"pipewire_consumers":0,"pipewire_output":"off","output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","denoise":false,"interface_revision":4,"cameras":[{"id":"usb-046d_0944-video-index0","name":"Logitech MX Brio","usb":"046d:0944","active":true}],"cameras_truncated":false}
```

The image controls of the active camera, as the daemon reads them from the device, then one set (the daemon is `active`, so the change applies at once):

```sh
omarchy-broadcast controls --json
omarchy-broadcast controls set sharpness 128
```

```json
{"v":1,"type":"controls","camera":"usb-046d_0944-video-index0","controls":[{"name":"brightness","min":0,"max":255,"step":1,"default":128,"value":128,"stored":false},{"name":"contrast","min":0,"max":255,"step":1,"default":128,"value":77,"stored":false},{"name":"saturation","min":0,"max":255,"step":1,"default":128,"value":179,"stored":false},{"name":"sharpness","min":0,"max":255,"step":1,"default":128,"value":255,"stored":false},{"name":"backlight_compensation","min":0,"max":1,"step":1,"default":1,"value":0,"stored":false}],"controls_truncated":false}
```

```text
Camera:     Logitech MX Brio (046d:0944)
  brightness              128  0..255  default 128
  contrast                 77  0..255  default 128
  saturation              179  0..255  default 128
  sharpness               128  0..255  default 128  (stored)
  backlight_compensation    0  0..1    default 1
Applied live.
```

The settings of the active camera, as the daemon reads them from the device: the full set is one line per group (here shortened to two settings), then one setting is changed (the daemon is `active`, so it applies at once) and `watch --json` reports the change, with the setting it enabled, in one delta line:

```sh
omarchy-broadcast settings --json
omarchy-broadcast settings set camera.exposure.auto_exposure manual
```

```json
{"v":1,"type":"settings_group","schema":1,"rev":4,"serial":2,"camera":"usb-046d_0944-video-index0","index":0,"count":2,"part":1,"parts":1,"group":{"id":"image","label":"Image","settings":[{"id":"camera.image.sharpness","label":"Sharpness","type":"int","value":255,"default":128,"stored":false,"enabled":true,"disabled_reason":"","enabled_when":[],"min":0,"max":255,"step":1,"hint":"slider"}]}}
{"v":1,"type":"settings_group","schema":1,"rev":4,"serial":2,"camera":"usb-046d_0944-video-index0","index":1,"count":2,"part":1,"parts":1,"group":{"id":"exposure","label":"Exposure","settings":[{"id":"camera.exposure.auto_exposure","label":"Exposure mode","type":"enum","value":"aperture_priority","default":"aperture_priority","stored":false,"enabled":true,"disabled_reason":"","enabled_when":[],"options":[{"value":"manual","label":"Manual"},{"value":"aperture_priority","label":"Auto"}],"hint":"segmented"},{"id":"camera.exposure.exposure_time_absolute","label":"Exposure time","type":"int","value":156,"default":156,"stored":false,"enabled":false,"disabled_reason":"Set exposure to manual to change this","enabled_when":[{"id":"camera.exposure.auto_exposure","op":"in","value":["manual","shutter_priority"]}],"min":3,"max":2047,"step":1,"unit":"×100 µs","hint":"slider"}]}}
```

```json
{"v":1,"type":"settings_delta","schema":1,"rev":5,"serial":2,"camera":"usb-046d_0944-video-index0","changes":[{"id":"camera.exposure.auto_exposure","value":"manual","stored":true,"enabled":true,"disabled_reason":""},{"id":"camera.exposure.exposure_time_absolute","value":1250,"stored":false,"enabled":true,"disabled_reason":""}]}
```

With no camera the output is one line:

```json
{"v":1,"type":"settings_clear","rev":1,"serial":1}
```

Read the current state once from a shell script:

```sh
state=$(omarchy-broadcast status --json | jq -r 'if .type == "snapshot" then .state else "unavailable" end')
```

A `preview --json` session at 15 fps: the lease is taken, three frames follow, the daemon exits (the lease is lost with it), and the next daemon instance is previewed.

```sh
omarchy-broadcast preview --json
```

```json
{"v":1,"type":"preview","active":true,"fps":15,"path":"/run/user/1000/omarchy-broadcast/preview.jpg"}
{"v":1,"type":"frame","seq":1,"path":"/run/user/1000/omarchy-broadcast/preview.jpg"}
{"v":1,"type":"frame","seq":2,"path":"/run/user/1000/omarchy-broadcast/preview.jpg"}
{"v":1,"type":"frame","seq":3,"path":"/run/user/1000/omarchy-broadcast/preview.jpg"}
{"v":1,"type":"preview","active":false}
{"v":1,"type":"error","code":"daemon-not-running","error":"omarchy-broadcastd is not running (no owner for br.com.dias.OmarchyBroadcast on the session bus); start it with: systemctl --user start omarchy-broadcastd"}
{"v":1,"type":"preview","active":true,"fps":15,"path":"/run/user/1000/omarchy-broadcast/preview.jpg"}
{"v":1,"type":"frame","seq":1,"path":"/run/user/1000/omarchy-broadcast/preview.jpg"}
```

## Writing a client

- Run `omarchy-broadcast watch --json` as a long-lived child process and read its stdout line by line. Do not poll `status --json`.
- Keep one record of the daemon. On `snapshot`, replace it. On `state`, replace its scalar fields. On `cameras`, replace its camera list and active camera. On `stats`, update the figures; on an `error` with `daemon-not-running`, mark the daemon unavailable until the next `snapshot`.
- Clear the stats display when `state` leaves `active`: no `stats` line arrives in other states.
- On `controls`, replace the record's image controls (they are also in `snapshot`, as `camera_controls`).
- For the camera settings, keep one tree: on a complete set of `settings_group` lines (or `settings_clear`) replace it, on a `settings_delta` of the same `serial` update the listed settings' `value`, `stored`, `enabled` and `disabled_reason`, and on a `snapshot` drop it and wait for the set that follows (see [Client assembly rules](#client-assembly-rules)). Draw the tree generically, by `type`, and show `disabled_reason` for a setting that is not `enabled`.
- Show `error` whenever it is non-empty, whatever the state.
- To stop, send `SIGTERM` or close the pipe.
- To show the live preview, run `omarchy-broadcast preview --json` as a second long-lived child process next to `watch`, and only while the user wants the preview: it keeps the camera running. Open the `path` of each `frame` line (or reload the image from it) and keep the lease for as long as the preview is shown; stop the process to end it. The frame file is replaced by a rename, so a reader never sees a partial image.
- Control the daemon by running `omarchy-broadcast on`, `off`, `camera next`, `camera <id>`, `effect <effect>` `denoise on|off`, `controls set NAME VALUE`, `controls reset`, `settings set --camera=ID -- ID VALUE` or `settings reset --camera=ID`, and check the exit status; the resulting change arrives on the `watch` stream.
