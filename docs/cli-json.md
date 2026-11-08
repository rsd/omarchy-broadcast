# CLI reference and JSON output (`oma-broadcast`)

`oma-broadcast` is the command-line remote control of `oma-broadcastd`. It talks to the daemon over the D-Bus session bus, through the interface `br.com.dias.OmaBroadcast1` described in [dbus-api.md](dbus-api.md). This document is the reference for its commands and, above all, for its JSON output: the interface for clients that cannot speak D-Bus, such as a QML status-bar plugin, which run `oma-broadcast watch --json` and read one JSON object per line. The design is recorded in [ADR-0011](adr/0011-daemon-lifecycle-and-dbus-api.md).

## Commands

| Command | Does | `--json` |
|---|---|---|
| `oma-broadcast status` | Prints the state, whether the camera is enabled, the active camera, the number of consumers, the output mode, the effect, and the error text if there is one. | One `snapshot` object. |
| `oma-broadcast on` | Enables the camera (`SetEnabled(true)`), then prints the resulting state. | No. |
| `oma-broadcast off` | Disables the camera, entering the privacy state `off` (`SetEnabled(false)`), then prints the resulting state. | No. |
| `oma-broadcast cameras` | Lists the cameras in the daemon's order, the active one marked with `*`: name, USB `vid:pid` (`-` if none) and id. | One `cameras` object. |
| `oma-broadcast camera next` | Selects the next camera (`NextCamera()`), then prints the resulting camera. | No. |
| `oma-broadcast camera <selector>` | Selects a camera (`SelectCamera(selector)`): a camera id, a case-insensitive name substring, a USB `vid:pid` or a `/dev/videoN` path. Prints the resulting camera. | No. |
| `oma-broadcast effect` | Prints the video effect, the blur strength and the background image (`Effect`, `BlurStrength`, `BackgroundImage`). Never starts the daemon. | One `snapshot` object. |
| `oma-broadcast effect none\|blur\|replace [--strength X] [--image PATH]` | Selects the video effect (`SetEffect`), then prints the resulting settings. See [Effect](#effect). | No. |
| `oma-broadcast watch` | Follows the daemon: a summary when it connects, then a line for each change and each `Stats` signal (about once per second while `active`), until interrupted. | One object per line (NDJSON). |

The word `next` is always the `NextCamera` command; select a camera whose name contains "next" by its id or `vid:pid`. The selectors and the errors they can raise are those of `SelectCamera` in [dbus-api.md](dbus-api.md#selectcameras-selector).

The human-readable output is meant for people and may change between versions. Scripts and other programs use `--json`.

## Effect

`oma-broadcast effect` shows or selects the video effect described in [dbus-api.md](dbus-api.md#video-effects).

```text
oma-broadcast effect [none|blur|replace] [--strength X] [--image PATH]
```

- Without an effect it prints the current settings:

  ```text
  Effect:     blur
  Strength:   0.50
  Image:      (none)
  ```

- `none`, `blur` and `replace` select the effect. `--strength X` (0.0 to 1.0) is accepted with `blur` only and `--image PATH` with `replace` only; giving one with another effect is a usage error (status `2`), as is a strength outside 0 to 1 or an image that does not exist or is not a regular file.
- `PATH` is made absolute against the current directory, so the daemon, which has its own working directory, is always given an absolute path. The daemon requires a PNG or JPEG file; other files are rejected by the daemon (status `1`) or fail when the effect starts (the daemon's `Error` property says why).
- The options are applied before the effect is selected (`SetBlurStrength` and `SetBackgroundImage`, then `SetEffect`), so the effect starts with them. If an option is rejected, the effect is not selected. `oma-broadcast effect replace` without `--image` uses the image the daemon already has (the earlier choice or the configuration's); with none, the daemon rejects it with the message `the replace effect needs a background image: set one with SetBackgroundImage first`.
- The settings persist across restarts of the daemon and take precedence over `config.toml` ([dbus-api.md](dbus-api.md#video-effects)).
- Status `1` also covers the daemon's `InvalidArgument` error (an unacceptable value, with its message on stderr).
- Against a daemon of interface revision 1, which has no effects, the command (with or without an effect) prints `oma-broadcast: the daemon does not support effects (its interface revision is 1, effects need 2); update oma-broadcastd` and exits with status `1`. `effect --json` does not check: it prints the `snapshot`, whose `interface_revision` tells.
- Like `on`, `off` and `camera`, selecting an effect may start the daemon through D-Bus activation and gives up after 30 s; `effect` without an effect, like `status`, never starts it and gives up after 5 s.
- Strings that come from the daemon (the image path, camera names, `Error`) are printed in the human-readable output with control characters and bidirectional-text marks escaped (`\xNN`, `\u{NNNN}`), so that a hostile file name cannot move the cursor or reorder the line. The JSON output is escaped by JSON itself.

## Starting the daemon

- `status`, `cameras`, `watch` and `effect` without an effect never start the daemon. They read the properties with `org.freedesktop.DBus.Properties.GetAll` carrying the `NO_AUTO_START` flag, so looking at the daemon never launches it. This is the read that the `obc-ipc` crate provides to Rust clients (`Snapshot::fetch_no_autostart`); [dbus-api.md](dbus-api.md#service-presence-and-activation) gives the `busctl` and `gdbus` equivalents.
- `on`, `off`, `camera` and `effect <effect>` call methods without that flag: when the D-Bus activation file `br.com.dias.OmaBroadcast.service` is installed, the call starts the daemon, as `systemctl --user start oma-broadcastd` would. `off` on a stopped daemon therefore starts it and records the privacy state, which persists across restarts. Without an activation file, or if the start fails, the command reports that the daemon is not running.

When the daemon is not running, the human-readable commands print one line to stderr, for example:

```text
oma-broadcast: oma-broadcastd is not running (no owner for br.com.dias.OmaBroadcast on the session bus); start it with: systemctl --user start oma-broadcastd
```

## Exit status

| Status | Meaning |
|---|---|
| `0` | Success. Also the status of `watch` when it ends on `SIGTERM`, `SIGINT`, `SIGPIPE` or a closed stdout. |
| `1` | The daemon rejected the request: one of the `br.com.dias.OmaBroadcast1.Error` errors (`NoSuchCamera`, `InvalidConfig`, `Unavailable`, `InvalidArgument`, `Failed`), or the daemon does not support the request (`effect` against interface revision 1). Its message is printed to stderr. |
| `2` | Invalid command line. |
| `3` | The daemon is not running (its name has no owner, or D-Bus could not start it), or there is no D-Bus session bus. |
| `4` | Any other D-Bus failure (an unexpected reply, a lost connection), a timeout, or a failed write to stdout. |

`status` and `cameras` give up after 5 s, and `on`, `off`, `camera` and `effect <effect>` after 30 s (time for the daemon to be started), with status `4`. `watch` gives each read of the properties 5 s as well (see [Daemon absent or gone](#daemon-absent-or-gone)). `oma-broadcast --help` lists the exit statuses.

## JSON output

### General rules

- Every object is printed on one line, terminated by `\n`, in UTF-8. `watch --json` flushes stdout after every line.
- Every object has the field **`v`**, the schema version (an integer, `1` in this version), and the field **`type`**, a string naming the line type: `snapshot`, `state`, `cameras`, `stats` or `error`.
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

- **Clipping of a path.** `error` and the other strings keep their beginning and lose their end. `background_image` is the exception: the end of a path, the file name, is what identifies it, so a path over the bound is clipped at the **start** and begins with `…` (for example `…/Pictures/office/background-2026.jpg`). The bound is 256 bytes, not 512 as for `error`, because a line that carries twelve cameras and a long path must stay within the 4096-byte cap. Do not use a clipped path as an argument: read it back from the daemon.
- **Camera list.** At most **12** cameras are listed; `cameras_truncated` is `true` when the daemon reported more. The list keeps the daemon's order, so the first 12 are listed. A clipped camera id can no longer be passed to `oma-broadcast camera`; real ids are far shorter than the bound.

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
| `consumers` | number | Number of processes, other than the daemon, holding the virtual camera open (integer, 0 or more). |
| `output_mode` | string | What the virtual camera delivers, e.g. `"NV12 1920x1080@30"`. |
| `effect` | string | The video effect in force: `"none"`, `"blur"` or `"replace"`. The effective value; it does not say whether the effect is ready or has failed (see `state` and `error`). |
| `blur_strength` | number | Strength of the blur effect, 0.0 to 1.0, also when `effect` is not `"blur"`. A value outside the range is clamped to it, and a value that is not a finite number is printed as `0`. `0` also when the daemon is too old to have the property (`interface_revision` 1). |
| `background_image` | string | Absolute path of the image used by `"replace"`; `""` if none. Bounded to 256 bytes and clipped at its start (see above). |
| `interface_revision` | number | Revision of `br.com.dias.OmaBroadcast1` the daemon implements. |
| `cameras` | array | The cameras present, in the daemon's order, as camera objects (below). |
| `cameras_truncated` | boolean | `true` if more cameras were present than listed. |

A **camera object**:

| Field | JSON type | Meaning |
|---|---|---|
| `id` | string | Opaque, stable identifier, accepted by `oma-broadcast camera`. |
| `name` | string | Human-readable device name. |
| `usb` | string | USB `vid:pid` in lower-case hex (`"046d:0944"`), `""` for a non-USB camera. |
| `active` | boolean | `true` for the camera named by `active_camera`. |

### `state`

Printed by `watch --json` when one or more of the scalar fields change. It carries **the current value of every scalar field**, so a client can replace its whole state with it, and names the fields that changed.

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"state"`. |
| `changed` | array of strings | The fields whose value changed, from: `state`, `enabled`, `error`, `active_camera`, `active_camera_name`, `consumers`, `output_mode`, `effect`, `blur_strength`, `background_image`, `interface_revision`. Never empty. |
| `state`, `enabled`, `error`, `active_camera`, `active_camera_name`, `consumers`, `output_mode`, `effect`, `blur_strength`, `background_image`, `interface_revision` | | As in `snapshot`, current values. |

A `state` line does not carry the camera list. `active_camera_name` can change without `active_camera` (the active camera was renamed, or appeared in or left the list); it is then reported in `changed` on its own.

### `cameras`

The camera list. Printed by `cameras --json`, and by `watch --json` when the list changes.

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"cameras"`. |
| `active_camera` | string | As in `snapshot`. |
| `cameras` | array | Camera objects, as in `snapshot`. |
| `cameras_truncated` | boolean | As in `snapshot`. |

When the daemon announces changes to scalar fields and to the camera list in one notification (for example when the active camera is unplugged), `watch --json` prints the `state` line first, then the `cameras` line.

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

### `error`

| Field | JSON type | Meaning |
|---|---|---|
| `v`, `type` | | `1`, `"error"`. |
| `code` | string | `"daemon-not-running"`: the daemon is not running, or there is no D-Bus session bus. `"dbus-error"`: any other failure to talk to the daemon (an unexpected reply, a lost connection, a timeout). New codes may be added; treat an unknown code like `"dbus-error"`. |
| `error` | string | A one-line explanation for people. Match on `code`, never on this text. |

`status --json` and `cameras --json` print an `error` object instead of their usual one when they fail to read the daemon, and exit with the status of the failure (`3` or `4`).

## `watch --json`

The output is a stream of the line types above:

1. A `snapshot` line, once the daemon has been read. The subscriptions to its signals are made before the read, so no change is lost in between.
2. Then, as events arrive (no polling): a `state` line per change of the scalar fields, a `cameras` line per change of the camera list, and a `stats` line per `Stats` signal. A change notification that changes no value prints nothing.

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
- stdout reporting an error or hang-up: when stdout is a pipe whose reader has gone, `watch` exits at once, even if it has nothing to print. `oma-broadcast watch --json | head -1` ends as soon as `head` does.

## Versioning

- `v` is the schema version of every line. It is `1` in this version.
- **`v` stays 1 for the effect fields.** `blur_strength` and `background_image` are new fields in existing line types, `effect` now takes the values `blur` and `replace` in addition to `none`, and `changed` may name the two new fields. None of this removes or renames a field or changes the type or meaning of one, so it is not breaking under the rule below. The set of `state` strings is unchanged. A client written for the earlier version keeps working: it ignores the new fields and shows an `effect` string it does not know as text.
- A **breaking change** raises `v`: a removed or renamed field or line type, a changed JSON type or meaning, a new state string, or a change to the rules above (line cap, outage behaviour).
- **Not breaking**, made without raising `v`: a new field in an existing line type, a new line type, a new `error` code. Clients ignore what they do not know.
- The schema is independent of the D-Bus `InterfaceRevision`; a new D-Bus property may appear as a new field.
- **Capability rules for clients.** A client offers the effect controls only if both hold: the CLI is new enough to print `blur_strength` (the field is present as a number in `snapshot`; an older CLI has no `effect` command, and `oma-broadcast effect` fails with status `2`), and the daemon's `interface_revision` is 2 or more. Check the CLI first, then the daemon; the two conditions have different remedies (update `oma-broadcast`, update `oma-broadcastd`). When the field is absent or the revision is lower, show the effect, if at all, as read-only text.

## Examples

Values are illustrative; camera ids are opaque and differ between machines.

```sh
oma-broadcast status --json
```

```json
{"v":1,"type":"snapshot","state":"idle","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":0,"output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","interface_revision":2,"cameras":[{"id":"usb-046d_0944-video-index0","name":"Logitech MX Brio","usb":"046d:0944","active":true},{"id":"pci-0000:00:14.0-usb-0:5:1.0-video-index0","name":"FHD Camera","usb":"5986:1193","active":false}],"cameras_truncated":false}
```

```sh
oma-broadcast cameras --json
```

```json
{"v":1,"type":"cameras","active_camera":"usb-046d_0944-video-index0","cameras":[{"id":"usb-046d_0944-video-index0","name":"Logitech MX Brio","usb":"046d:0944","active":true},{"id":"pci-0000:00:14.0-usb-0:5:1.0-video-index0","name":"FHD Camera","usb":"5986:1193","active":false}],"cameras_truncated":false}
```

A `watch --json` session: an application opens the virtual camera, the stream starts, the daemon is restarted.

```sh
oma-broadcast watch --json
```

```json
{"v":1,"type":"snapshot","state":"idle","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":0,"output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","interface_revision":2,"cameras":[{"id":"usb-046d_0944-video-index0","name":"Logitech MX Brio","usb":"046d:0944","active":true}],"cameras_truncated":false}
{"v":1,"type":"state","changed":["consumers"],"state":"idle","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":1,"output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","interface_revision":2}
{"v":1,"type":"state","changed":["state"],"state":"starting","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":1,"output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","interface_revision":2}
{"v":1,"type":"state","changed":["state"],"state":"active","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":1,"output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","interface_revision":2}
{"v":1,"type":"stats","fps":30.0,"latency_p50_ms":41.2,"cpu_percent":6.5,"gpu_percent":18.0}
{"v":1,"type":"stats","fps":30.0,"latency_p50_ms":40.8,"cpu_percent":6.4,"gpu_percent":-1.0}
{"v":1,"type":"error","code":"daemon-not-running","error":"oma-broadcastd is not running (no owner for br.com.dias.OmaBroadcast on the session bus); start it with: systemctl --user start oma-broadcastd"}
{"v":1,"type":"snapshot","state":"idle","enabled":true,"error":"","active_camera":"usb-046d_0944-video-index0","active_camera_name":"Logitech MX Brio","consumers":0,"output_mode":"NV12 1920x1080@30","effect":"none","blur_strength":0.5,"background_image":"","interface_revision":2,"cameras":[{"id":"usb-046d_0944-video-index0","name":"Logitech MX Brio","usb":"046d:0944","active":true}],"cameras_truncated":false}
```

Read the current state once from a shell script:

```sh
state=$(oma-broadcast status --json | jq -r 'if .type == "snapshot" then .state else "unavailable" end')
```

## Writing a client

- Run `oma-broadcast watch --json` as a long-lived child process and read its stdout line by line. Do not poll `status --json`.
- Keep one record of the daemon. On `snapshot`, replace it. On `state`, replace its scalar fields. On `cameras`, replace its camera list and active camera. On `stats`, update the figures; on an `error` with `daemon-not-running`, mark the daemon unavailable until the next `snapshot`.
- Clear the stats display when `state` leaves `active`: no `stats` line arrives in other states.
- Show `error` whenever it is non-empty, whatever the state.
- To stop, send `SIGTERM` or close the pipe.
- Control the daemon by running `oma-broadcast on`, `off`, `camera next`, `camera <id>` or `effect <effect>`, and check the exit status; the resulting change arrives on the `watch` stream.
