# ADR-0020: Camera image controls: a per-camera allowlist, written whenever the camera opens

- **Status:** Accepted (2026-11-09). Extends [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) (the D-Bus API as revision 6, additive, and the runtime state file) and [ADR-0009](0009-ingest-process-output.md) (the camera and its controls belong to the ingest stage). Amended 2026-11-10 by [ADR-0023](0023-settings-schema.md): see [Amendment](#amendment-2026-11-10).

## Context
- A UVC camera keeps its image controls on the device, and other software changes them. A camera was found with sharpness 255, saturation 179, contrast 77 and backlight compensation 0: the picture had halos around edges and oversaturated colour, and nothing in the daemon could improve it. The user needs to adjust these controls and to put them back, for each camera, without another tool.
- The design must not be specific to one camera model ([../architecture.md](../architecture.md)). Cameras differ in which controls they have, in their ranges and steps, and in which controls the driver marks read-only, disabled or inactive. The device is the source of truth for all of it (`VIDIOC_QUERYCTRL`).
- Users have several cameras, and a camera's id is what the daemon already uses to select and remember it ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md)).
- The daemon opens a camera only while a session runs (`starting`, `active`), and leaves a closed camera untouched: its LED is off and no other software is disturbed ([ADR-0006](0006-on-demand-gpu-lifecycle.md)). Reading a control is a different kind of access from streaming; discovery already opens nodes to query them.
- The daemon already pins one control itself: `exposure_dynamic_framerate` is held at 0 while a camera streams (`[camera] fixed_framerate`) and restored when it stops ([ADR-0009](0009-ingest-process-output.md), [../architecture.md](../architecture.md#2-stage-and-caps-model)).
- Reading a control reads the device, and the device can fail: UVC controls often answer `EIO` or `EPIPE`.
- The JSON lines of the CLI are capped at 4096 bytes, and a client reads one line at a time ([../cli-json.md](../cli-json.md)).

## Decision
**The daemon offers five image controls, the same for every camera, with the range, step and default of each taken from the device. The user's values are stored per camera id and written to the camera whenever the daemon opens it. A closed camera is never written, and the daemon does not restore the camera's previous values at close.**

### Which controls
- An allowlist kept as data in `obc-pipeline::controls` (`IMAGE_CONTROLS`): `brightness`, `contrast`, `saturation`, `sharpness` and `backlight_compensation` (the `v4l2-ctl` names, the V4L2 controls of the same names), in that order. No camera is special-cased.
- A control is exposed for a camera only if the device has it, it is not disabled, read-only or inactive, and it is an integer or a boolean. Range, step and default come from the device. A value is validated against the range and the step (counted from the minimum) before it is written, by the same check on every path.
- A control that cannot be queried or read is left out of the list, never the whole list, and is logged once per camera and control.
- `exposure_dynamic_framerate` is not in the list. It stays what it was: pinned while streaming and restored when the stream stops.

### Storage
- The values are kept in the runtime state file, by camera id, only for what the user set: `[camera_controls."<id>"]`, with `values = { … }` and `reset_pending = true` when a reset is waiting. A value equal to the default is kept if the user set it.
- Ids are written as TOML basic strings, escaped, because the `toml` crate writes some strings as literals or multi-line strings, and a multi-line string is not a valid key.
- The state file stays strict for the settings that hide something ([ADR-0011](0011-daemon-lifecycle-and-dbus-api.md): a damaged file starts the daemon `off`). Image controls hide nothing, so a bad entry (an unknown name, a value that is not a 32-bit integer, a wrong type) is dropped alone, with a warning that names the file, and never makes the file damaged.

### When the device is written
- The daemon writes to a camera only while its session has it open: from the moment the ingest's switch to that camera succeeded (state `starting` or `active`).
- While the camera is open, `SetCameraControl` validates, writes, reads the value back (the driver may round it), stores and saves. `ResetCameraControls` writes every default and forgets the stored values.
- While it is not open, `SetCameraControl` only stores the value, and `ResetCameraControls` forgets the stored values and marks a reset pending.
- At every successful switch to a camera, and when the open camera's id changes to one with stored values, the daemon opens the node once, writes the defaults if a reset is pending (and clears the mark), then writes each stored value, each validated against the device's current range. A stored value the device does not take now is skipped, logged and kept for the camera. A failing write is logged and never fails the session.
- The ingest switch happens on every start, camera selection, fail-over and mode change, so every way a camera opens is covered by one point.

### Reads
- `GetCameraControls` and the `CameraControls` property read the device in any state: a query and a read of each value, with no stream. `value` is the device's value while the camera is open; otherwise the value it will be given at the next open (the stored value, else the default if a reset is pending, else the device's).
- `CameraControls` is the view of `ActiveCamera`. It is refreshed when `ActiveCamera` changes, on a rescan, when the camera opens, once at its first frame (a camera can change controls as it starts streaming) and after a set or reset. It is never polled: a value changed on the device by other software shows at the next of these events.

### Close
- The values the user set stay applied. The daemon does not put back what it found.

### D-Bus (revision 6, additive)
- Property `CameraControls` `(sa(siiiiib))`: the active camera's id, then `(name, min, max, step, default, value, stored)` per control. It emits changes.
- `GetCameraControls(s camera) -> a(siiiiib)`, `SetCameraControl(s camera, s name, i value)`, `ResetCameraControls(s camera)`. The camera is an id of `Cameras`, or `""` for `ActiveCamera`.
- Errors: `Unavailable` (no active camera), `NoSuchCamera`, `InvalidArgument` (a name that is not allowed or not exposed, a value outside the range or off the step) and `Failed`. A refused write stores nothing.
- `InterfaceRevision` is 6. Details are in [../dbus-api.md](../dbus-api.md#camera-image-controls).

### The CLI
`omarchy-broadcast controls [--json]` reads without starting the daemon; `controls set NAME VALUE` and `controls reset` may start it, like the other setters. They act on the active camera and print the controls read back, and say whether a change applied live (state `starting` or `active`) or applies the next time the camera opens. Against a daemon below revision 6 they fail with a message (`unsupported-daemon` with `--json`).
- JSON: a new `controls` line type, a `camera_controls` object in `snapshot` (`null` below revision 6, absent from `state` lines), and a `controls` line from `watch --json` on each change. `v` stays 1. Bounds: a control name is at most 32 bytes and at most 16 controls are listed. A `snapshot` that exceeds the line cap gives up its controls first and then its cameras; the worst-case snapshot of twelve cameras at their string bounds no longer fits with controls at theirs. Details in [../cli-json.md](../cli-json.md#camera-image-controls).
- The bar widget is a client of these lines, and shows sliders for the first four controls (brightness, contrast, saturation, sharpness); `backlight_compensation` is reset and set from the CLI.

## Consequences
- Revision 6 is additive: a client built for revision 5 ignores the new members.
- A user who sets a value changes the camera for every application and every other tool, and it stays so after the daemon stops. That is the intent, and it is the reason `reset` exists.
- Opening a camera applies the stored values to hardware that other software may have changed meanwhile, so the picture after an open is the one the user chose, not the one the last tool left.
- A change made while the camera is closed shows in the property and the CLI as the value it will have, not as the device's value, and is applied at the next open. A client says which to its user from the state.
- Values changed on the device by other software while the camera is open are not noticed until the next refresh event.
- A camera whose controls cannot be read at all shows none, logged once, and its stored values wait for a later open.
- Known limitations:
  - Two controls interact through the driver in some cameras (a manual value that becomes inactive when an automatic mode is on). Such a control is left out while it is inactive.
  - A driver may round a written value; the value read back is what is shown.
  - The state file keeps an entry for a camera that is no longer plugged in, and applies it if the camera returns with the same id.
- Tests: [../testing.md](../testing.md#camera-image-controls).

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| Restore the camera's previous values at close | It would bring back the values other software left, the ones the user came to fix. The camera would look right only while the daemon had it open |
| Write to a closed camera in `idle`, by opening the node | The daemon keeps a closed camera untouched. Reads are allowed because they change nothing; writes wait for the open, where the daemon already holds the camera and applies the stored values |
| Camera-specific code for a known bad camera | The design stays free of per-camera code ([../architecture.md](../architecture.md)): the allowlist is data, the same for every camera, and the device supplies the ranges. Another camera with other defaults needs no change |
| Poll the device to follow changes made by other software | A poll would touch the device at intervals, also while the daemon is otherwise idle, for an event that is rare. The view is refreshed on the daemon's own events, and a client that wants a fresh value asks with `GetCameraControls` |
| List every control the device has | Exposure, focus and white balance have automatic modes that make the manual control inactive or fight the automatic one, and the daemon already manages one exposure setting itself. The five chosen controls are the ones that change the picture without such interplay |
| Store the values by camera name or by selector | The id is what `SelectCamera` takes and `Cameras` reports, and what the selection is stored by. Two cameras of one model have the same name |
| Fail the whole session when a stored value cannot be written | The camera is more important than a setting. A failure is logged and the stream goes on |
| Treat a bad `[camera_controls]` entry like a damaged state file | A damaged file starts the daemon `off` so that a lost effect choice never shows the raw image. Image controls hide nothing; dropping the bad entry and warning is enough |

## What would change this decision
- Evidence that users need other controls (zoom, pan, white balance) or a camera whose auto mode has to be switched off first: the allowlist is data, and a control with an automatic mode would need its own rule, to be decided with its interplay in view.
- A need to see changes made by other software at once: a refresh on a timer, or a V4L2 control event subscription while the camera is open, would be additive.
- Evidence that users want the camera's original values back when the daemon stops: a restore at close could be added as an option, not as the default.

## Amendment (2026-11-10)
The settings channel (D-Bus revision 7): [ADR-0023](0023-settings-schema.md) builds on this decision. The decision above stands as written (per-camera storage by id, writes whenever the camera opens and never to a closed camera, reads in any state, no restore at close); these points are changed or extended:

- **The allowlist covers every standard UVC control, through the schema.** The five names in `IMAGE_CONTROLS` are no longer the only controls: the daemon enumerates every user- and camera-class control of an exposed type (integer, boolean, menu, integer menu, button) and publishes it in a tree with groups, labels, units and the rules between controls, from a catalogue kept as data. A control the catalogue does not know goes to a group `other`. The motion controls (relative, continuous, speed) and `privacy` are never exposed. The reason for leaving out exposure, focus and white balance (their automatic modes) is answered by a rule per control, computed by the daemon, instead of by leaving them out. The five image controls stay what the revision-6 members show.
- **Inactive controls are shown greyed, not left out.** The daemon publishes a control the device marks inactive with `enabled` false and a reason (`Set exposure to manual to change this`), and refuses a write to it with `Disabled`. The "left out while it is inactive" limitation above applies to the revision-6 members only, which keep that behaviour for byte compatibility.
- **The storage keys are extended.** The table keeps `[camera_controls."<id>"]` and `values`, but any control name is a key (`^[a-z][a-z0-9_]{0,31}$`, raw device values, among them `exposure_dynamic_framerate` and the Logitech `field_of_view` byte). `reset_pending` is replaced: `reset_all = true` is a reset of every control of the camera, and `reset_names = [...]` a reset of some (the revision-6 `ResetCameraControls`, which resets only its five controls, and narrower scopes of `ResetSettings`). A state file written by 0.21 with `reset_pending = true` is read as `reset_names` of the five image controls, never as `reset_all`, and is not written back. At an open, a pending reset writes the defaults (dependent settings first) and then the stored values (masters first).
- **V4L2 control events are followed while the camera is open.** This is the subscription the section "What would change this decision" anticipated: one extra read-only descriptor on the node, held only while the session has the camera open, reports changes made by other software and by the camera, and the tree is read again after the first event of a burst. Nothing is polled. A closed camera is still untouched, and a change made to it shows at the next refresh point, as above. The `CameraControls` property is refreshed by the same events.
- **The revision-6 members are adapters.** `CameraControls`, `GetCameraControls`, `SetCameraControl` and `ResetCameraControls` keep their signatures and meanings, over the same model, storage and write coalescing as `SetSetting` (a value set through one shows in the other). `InterfaceRevision` is 7; the details are in [../dbus-api.md](../dbus-api.md#camera-settings). The CLI's `controls` command keeps working beside the new `settings` command ([../cli-json.md](../cli-json.md#camera-settings)).
- **The frame-rate pin.** The statement that the daemon pins `exposure_dynamic_framerate` while streaming is unchanged, with one addition: the control is now the setting "Keep 30 fps in low light", and a camera for which the user stored a value is not pinned; the stored choice wins.
- **Writes are coalesced and the state file is saved write-behind**, because a client may stream values: at most one device write per setting every 50 ms (the last value wins), a global cap of 100 writes a second (`RateLimited`), and a save 300 ms after the last settings change. The statement that `SetCameraControl` stores and saves before it returns is therefore true of the device and of memory, and of the file after at most 300 ms (at once when the state file cannot take a write, with `Failed`, as before).
