# Running oma-broadcastd as a user service

`oma-broadcastd` is the headless daemon behind the virtual camera. It runs as a systemd user service, owns the D-Bus name `br.com.dias.OmaBroadcast` on the session bus, and starts the camera and the GPU pipeline only while an application is using the virtual camera ([architecture.md](../architecture.md), [ADR-0011](../adr/0011-daemon-lifecycle-and-dbus-api.md)). Nothing is installed by the build; the steps below install the binaries and the unit files for the current user and need no root.

## Prerequisites

- The v4l2loopback device with the label "Omarchy Broadcast" and `exclusive_caps=1`, as described in [loopback.md](loopback.md). The daemon finds the device by its label. Without it the daemon runs in the state `error` and retries every 2 s.
- The GPU prerequisites of the pipeline (README, "Requirements"): the NVIDIA driver, CUDA and GStreamer with the `nvcodec` plugin.
- A systemd user session with a D-Bus session bus (the normal state of a desktop login).

## Install

1. Install the daemon and the command-line client into `~/.cargo/bin`, from the repository root. The unit starts `%h/.cargo/bin/oma-broadcastd`.

   ```sh
   cargo install --path crates/oma-broadcastd
   cargo install --path crates/oma-broadcast
   ```

2. Install the unit file:

   ```sh
   install -Dm644 dist/systemd/oma-broadcastd.service \
       ~/.config/systemd/user/oma-broadcastd.service
   ```

3. Optional, D-Bus activation: with this file, a command such as `oma-broadcast on` starts the service if it is not running. Reads (`oma-broadcast status`, `watch`) never start it. See [dbus-api.md](../dbus-api.md#service-presence-and-activation).

   ```sh
   install -Dm644 dist/dbus-1/services/br.com.dias.OmaBroadcast.service \
       ~/.local/share/dbus-1/services/br.com.dias.OmaBroadcast.service
   ```

4. Optional, discovery through PipeWire: a drop-in that orders WirePlumber after the daemon (see below).

   ```sh
   install -Dm644 dist/systemd/wireplumber.service.d/oma-broadcast.conf \
       ~/.config/systemd/user/wireplumber.service.d/oma-broadcast.conf
   ```

5. Load the units and start the service now and at every login:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now oma-broadcastd
   ```

   If the service was enabled before from an older copy of the unit, run `systemctl --user reenable oma-broadcastd` so that it is wanted by `default.target`.

Check it:

```sh
systemctl --user status oma-broadcastd
oma-broadcast status
```

The service is ready (`Type=dbus`) once it owns its bus name, which it requests after the loopback writer has started: at that point the virtual camera is already listed as a capture device.

## Discovery by PipeWire

WirePlumber probes video devices when it starts. A loopback device that has no writer at that moment is recorded as output-only and not probed again, so applications that list cameras through PipeWire (the camera portal, OBS's PipeWire source, browsers using PipeWire) would not show it. Two remedies:

- The drop-in of step 4 starts WirePlumber after the daemon is ready. Run `systemctl --user daemon-reload` after installing it, and restart WirePlumber once (below) for the current session.
- Without the drop-in, or after the daemon was restarted by hand: `systemctl --user restart wireplumber` while the daemon runs.

Applications that open `/dev/video*` directly are not affected. Meanwhile, `gst-device-monitor-1.0 -i Video/Source` lists the camera through the v4l2 provider. Details are in [testing.md](../testing.md#discovery-by-pipewire).

## Configuration

The configuration is `~/.config/oma-broadcast/config.toml` (`$XDG_CONFIG_HOME/oma-broadcast/config.toml`). It is optional: a missing file means the defaults. [config.example.toml](../config.example.toml) lists every key with its default and a comment. The daemon never writes this file.

After editing it, reload without restarting:

```sh
systemctl --user reload oma-broadcastd     # sends SIGHUP
oma-broadcast status                       # shows Error: if the file was rejected
```

`busctl --user call br.com.dias.OmaBroadcast /br/com/dias/OmaBroadcast br.com.dias.OmaBroadcast1 ReloadConfig` does the same and returns the reason on failure. An invalid file is rejected as a whole: the previous configuration stays in force and the `Error` property names the file, line and key.

Settings changed while the daemon runs (`oma-broadcast on` / `off`, the camera selection and the video effect with its blur strength and background image) are kept in a separate state file, `~/.local/state/oma-broadcast/state.toml` (`$XDG_STATE_HOME/oma-broadcast/state.toml`), so that `config.toml` keeps its comments and layout. The state file is written by the daemon. If the state file cannot be read, the daemon starts `off` and says why in `Error`: privacy fails closed.

The paths can be overridden with `--config PATH` and `--state PATH` in a copy of the unit file.

## Logs

```sh
journalctl --user -u oma-broadcastd -f
```

Add `-v` to `ExecStart` for debug output (consumer counts, teardown times). Exit status 3 means another instance already owns the bus name; systemd does not restart the service on that status.

## Stop, disable, uninstall

```sh
systemctl --user disable --now oma-broadcastd
rm ~/.config/systemd/user/oma-broadcastd.service
rm -f ~/.local/share/dbus-1/services/br.com.dias.OmaBroadcast.service
rm -f ~/.config/systemd/user/wireplumber.service.d/oma-broadcast.conf
systemctl --user daemon-reload
```
