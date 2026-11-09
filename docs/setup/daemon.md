# Running oma-broadcastd as a user service

`oma-broadcastd` is the headless daemon behind the virtual camera. It runs as a systemd user service, owns the D-Bus name `br.com.dias.OmaBroadcast` on the session bus, and starts the camera and the GPU pipeline only while an application is using the virtual camera ([architecture.md](../architecture.md), [ADR-0011](../adr/0011-daemon-lifecycle-and-dbus-api.md)). Nothing is installed by the build. There are three ways to install it: the Arch package, a per-user install with `scripts/install-user.sh`, and a manual install. The last two need no root.

## Prerequisites

- The v4l2loopback device with the label "Omarchy Broadcast" and `exclusive_caps=1`, as described in [loopback.md](loopback.md). The daemon finds the device by its label. Without it the daemon runs in the state `error` and retries every 2 s.
- The GPU prerequisites of the pipeline (README, "Requirements"): the NVIDIA driver, CUDA and GStreamer with the `nvcodec` plugin.
- A systemd user session with a D-Bus session bus (the normal state of a desktop login).

## Install

### Arch package

`packaging/arch/PKGBUILD` builds `oma-broadcast-git` from the committed HEAD of the local checkout (uncommitted changes are not included). Build from `packaging/arch`, then install the result:

```sh
cd packaging/arch
makepkg -s
sudo pacman -U oma-broadcast-git-*.pkg.tar.zst
```

The package installs:

- `/usr/bin/oma-broadcastd` and `/usr/bin/oma-broadcast`;
- the unit `/usr/lib/systemd/user/oma-broadcastd.service`;
- the D-Bus activation file `/usr/share/dbus-1/services/br.com.dias.OmaBroadcast.service`;
- the Omarchy bar plugin in `/usr/share/oma-broadcast/omarchy-bar/br.com.dias.oma-broadcast/` ([link it](../../clients/omarchy-bar/README.md#install));
- the licences and this documentation, in `/usr/share/doc/oma-broadcast/`;
- the optional drop-ins as inactive examples in `/usr/share/doc/oma-broadcast/examples/systemd/` (`wireplumber.service.d/oma-broadcast.conf`, see "Discovery by PipeWire", and `oma-broadcastd.service.d/maxine.conf`, see [maxine.md](maxine.md#7a-the-daemon-systemd-user-unit)).

Nothing is enabled. Each user runs:

```sh
systemctl --user enable --now oma-broadcastd
```

The runtime dependencies include the NVIDIA driver userspace (`nvidia-utils`), `cuda` (NVRTC, used by GStreamer's CUDA converters), `gst-plugins-good`, `gst-plugins-bad-libs` and a v4l2loopback kernel module (for example `v4l2loopback-dkms`). The loopback device must still be configured as in [loopback.md](loopback.md). The NVIDIA Maxine VFX SDK is never part of the package; install it as in [maxine.md](maxine.md) (the os-release override described there still needs a source checkout).

### Per-user install from a checkout

From the repository root:

```sh
scripts/install-user.sh
```

The script builds the release binaries and installs, without root:

- `oma-broadcastd` and `oma-broadcast` in `${XDG_BIN_HOME:-~/.local/bin}`;
- the unit in `${XDG_DATA_HOME:-~/.local/share}/systemd/user/`, with `ExecStart=` pointing at the installed daemon;
- the D-Bus activation file in `${XDG_DATA_HOME:-~/.local/share}/dbus-1/services/`;
- optionally, the os-release shim for the Maxine backend, `os-release-shim.so` and `fake-os-release`, in `~/.local/lib/oma-broadcast/`: built when `make` and a C compiler are present, skipped with `--no-shim` (see [the os-release override](maxine.md#7-the-os-release-override));
- a manifest, `install-user.manifest`, in `${XDG_STATE_HOME:-~/.local/state}/oma-broadcast/`, which lists every path written and the sha256 of each binary.

Behaviour:

- It refuses to run as root.
- It marks every text file it writes. It overwrites or removes a file only if it carries that mark (a binary: only if the manifest lists it with the checksum on disk), and stops before changing anything if a target path holds another file. Running it again updates its own install.
- It runs no `systemctl` command and enables nothing; it prints the next steps.
- It does not install the WirePlumber drop-in or the bar plugin.
- It warns when a unit in `~/.config/systemd/user` exists, because that location takes precedence (see below).

Options: `--dry-run` prints every action and changes nothing, `--no-build` installs the binaries already in `${CARGO_TARGET_DIR:-target}/release`, `--uninstall` removes what the script installed, `--help` shows the usage. Exit status: 0 done; 1 refused (a target path holds a file the script did not install, or the unit is still enabled on `--uninstall`) or the build failed; 2 usage error or unusable environment.

Then load and start the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now oma-broadcastd
```

### Manual install

1. Install the daemon and the command-line client into `~/.cargo/bin`, from the repository root.

   ```sh
   cargo install --path crates/oma-broadcastd
   cargo install --path crates/oma-broadcast
   ```

2. Install the unit file with `ExecStart=` rewritten. The file in `dist/` names `/usr/bin/oma-broadcastd`, the path of the package; copied unchanged after `cargo install`, the service fails to start (status 203/EXEC).

   ```sh
   mkdir -p ~/.config/systemd/user
   sed 's|^ExecStart=/usr/bin/|ExecStart=%h/.cargo/bin/|' \
       dist/systemd/oma-broadcastd.service \
       > ~/.config/systemd/user/oma-broadcastd.service
   ```

3. Optional, D-Bus activation: with this file, a command such as `oma-broadcast on` starts the service if it is not running. Reads (`oma-broadcast status`, `watch`) never start it. See [dbus-api.md](../dbus-api.md#service-presence-and-activation).

   ```sh
   install -Dm644 dist/dbus-1/services/br.com.dias.OmaBroadcast.service \
       ~/.local/share/dbus-1/services/br.com.dias.OmaBroadcast.service
   ```

4. Optional, discovery through PipeWire: a drop-in that orders WirePlumber after the daemon (see below). In a checkout it is `dist/systemd/wireplumber.service.d/oma-broadcast.conf`; the package ships the same file as an example at `/usr/share/doc/oma-broadcast/examples/systemd/wireplumber.service.d/oma-broadcast.conf`.

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

The same drop-in applies to the other two routes (step 4); the package does not install it.

### Which unit is used

systemd searches the user unit directories in this order and uses the first match: `~/.config/systemd/user`, then `~/.local/share/systemd/user`, then `/usr/lib/systemd/user`. A copy of the unit in a directory earlier in the list shadows the others. In particular, an older manual copy in `~/.config/systemd/user` keeps starting `~/.cargo/bin/oma-broadcastd` after the package is installed; remove it (and run `systemctl --user daemon-reload`) to use the packaged unit. `systemctl --user cat oma-broadcastd` shows which file is in force.

Check it:

```sh
systemctl --user status oma-broadcastd
oma-broadcast status
```

The service is ready (`Type=dbus`) once it owns its bus name, which it requests after the loopback writer has started: at that point the virtual camera is already listed as a capture device.

## Discovery by PipeWire

WirePlumber probes video devices when it starts. A loopback device that has no writer at that moment is recorded as output-only and not probed again, so applications that list cameras through PipeWire (the camera portal, OBS's PipeWire source, browsers using PipeWire) would not show it. Two remedies:

- The drop-in (manual install, step 4) starts WirePlumber after the daemon is ready. Run `systemctl --user daemon-reload` after installing it, and restart WirePlumber once (below) for the current session.
- Without the drop-in, or after the daemon was restarted by hand: `systemctl --user restart wireplumber` while the daemon runs.

Applications that open `/dev/video*` directly are not affected. Meanwhile, `gst-device-monitor-1.0 -i Video/Source` lists the camera through the v4l2 provider. Details are in [testing.md](../testing.md#discovery-by-pipewire). The daemon can also register its own PipeWire source, which needs none of this: see the next section.

## The PipeWire output

Optionally the daemon also registers a native PipeWire video source, "Omarchy Broadcast", next to the loopback device ([ADR-0016](../adr/0016-pipewire-output.md)). The loopback stays as it is and remains the default. Applications that use PipeWire for cameras find the node without WirePlumber having to probe the loopback. It is off in two places: the daemon must be built with the cargo feature, and the configuration must enable it.

**Build.** The Arch package is built with the feature. From source, build or install with it; it needs the libpipewire headers and clang at build time and links `libpipewire-0.3`:

```sh
cargo build --release --features oma-broadcastd/pipewire
cargo install --path crates/oma-broadcastd --features pipewire
```

`scripts/install-user.sh` builds without it.

**Enable.** In `~/.config/oma-broadcast/config.toml`:

```toml
[pipewire]
enabled = true
```

Then reload (`systemctl --user reload oma-broadcastd`). The node appears at once. Setting it back to `false` removes it at once if no application is linked to it, otherwise when the last one leaves. The loopback is not touched either way. The node exists only while the loopback writer runs.

**Check.**

```sh
oma-broadcast status          # Consumers: 0 (loopback 0, PipeWire 0), PipeWire: ready
pw-cli ls Node | grep -B2 -A8 oma-broadcast
gst-launch-1.0 pipewiresrc target-object=oma-broadcast ! videoconvert ! autovideosink
```

`PipeWire:` in `status` is `ready` when the node is registered, `streaming` while an application is linked, `failed` when PipeWire is not running or the connection was lost (retried every 2 s; the daemon stays usable and the loopback keeps working), and `unsupported` when the configuration enables it in a daemon built without the feature. A PipeWire consumer starts the camera like a loopback consumer does, and `Consumers` is the total of both kinds.

**Applications.**

| Application | Setting |
|---|---|
| Firefox | uses the camera portal; the preference `media.webrtc.camera.allow-pipewire` in `about:config` must be `true` (the default in some distributions) |
| Chromium, Chrome, Electron applications | off by default: enable the "PipeWire Camera support" flag (`chrome://flags`, feature `WebRtcPipeWireCamera`), or start the browser with `--enable-features=WebRtcPipeWireCamera` |
| OBS Studio 30.1 and later | add a "Video Capture Device (PipeWire)" source |
| GNOME Snapshot, GStreamer `pipewiresrc` | work without a setting |
| Zoom, Discord | do not use the PipeWire camera; select the loopback camera |

The node offers NV12, YUY2 and I420 at the output contract's size and rate. An application that asks for another format or size fails to link, and nothing else is affected.

### Hiding the duplicate

Once WirePlumber has probed the loopback while the writer streams, PipeWire lists "Omarchy Broadcast" twice: the native node, and the loopback through v4l2. To hide the loopback from PipeWire only, create `~/.config/wireplumber/wireplumber.conf.d/50-oma-broadcast.conf`:

```
monitor.v4l2.rules = [
  { matches = [ { device.product.name = "Omarchy Broadcast" } ]
    actions = { update-props = { device.disabled = true } } }
]
```

and run `systemctl --user restart wireplumber`. Applications that open `/dev/video*` directly are unaffected. The package does not install this file. With it, the WirePlumber ordering drop-in is unnecessary for applications that use the native node; it stays useful for applications that reach the camera through v4l2 only. Remove the file and restart WirePlumber to undo it.

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

Stop and disable the service first, on every route:

```sh
systemctl --user disable --now oma-broadcastd
```

Then, depending on how it was installed:

- Arch package: `sudo pacman -R oma-broadcast-git`. Remove a bar plugin link you made (`rm ~/.config/omarchy/plugins/br.com.dias.oma-broadcast`) and any drop-in you copied from the examples.
- Per-user install: `scripts/install-user.sh --uninstall` from the checkout. It removes only the files it installed and refuses while the unit is still enabled, hence the command above comes first.
- Manual install:

  ```sh
  rm ~/.config/systemd/user/oma-broadcastd.service
  rm -f ~/.local/share/dbus-1/services/br.com.dias.OmaBroadcast.service
  rm -f ~/.config/systemd/user/wireplumber.service.d/oma-broadcast.conf
  cargo uninstall oma-broadcastd oma-broadcast
  ```

Finish with `systemctl --user daemon-reload`.
