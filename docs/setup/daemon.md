# Running omarchy-broadcastd as a user service

`omarchy-broadcastd` is the headless daemon behind the virtual camera. It runs as a systemd user service, owns the D-Bus name `br.com.dias.OmarchyBroadcast` on the session bus, and starts the camera and the GPU pipeline only while an application is using the virtual camera ([architecture.md](../architecture.md), [ADR-0011](../adr/0011-daemon-lifecycle-and-dbus-api.md)). Nothing is installed by the build. There are three ways to install it: the Arch package, a per-user install with `scripts/install-user.sh`, and a manual install. The last two need no root.

## Prerequisites

- The v4l2loopback device with the label "Omarchy Broadcast" and `exclusive_caps=1`, as described in [loopback.md](loopback.md). The daemon finds the device by its label. Without it the daemon runs in the state `error` and retries every 2 s.
- The GPU prerequisites of the pipeline (README, "Requirements"): the NVIDIA driver, CUDA and GStreamer with the `nvcodec` plugin.
- A systemd user session with a D-Bus session bus (the normal state of a desktop login).

## Install

### Arch package

`packaging/arch/PKGBUILD` builds `omarchy-broadcast-git` from the committed HEAD of the local checkout (uncommitted changes are not included). Build from `packaging/arch`, then install the result:

```sh
cd packaging/arch
makepkg -s
sudo pacman -U omarchy-broadcast-git-*.pkg.tar.zst
```

The package installs:

- `/usr/bin/omarchy-broadcastd` and `/usr/bin/omarchy-broadcast`;
- the unit `/usr/lib/systemd/user/omarchy-broadcastd.service`;
- the D-Bus activation file `/usr/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service`;
- the Maxine module, `/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so`, with its manifest in `/usr/lib/omarchy-broadcast/modules.d/` ([maxine.md](maxine.md#the-maxine-module), [../modules.md](../modules.md)), and the C header of the module interface, `/usr/include/omarchy-broadcast/omarchy_broadcast_module.h`;
- the licences and this documentation, in `/usr/share/doc/omarchy-broadcast/`;
- the optional drop-ins as inactive examples in `/usr/share/doc/omarchy-broadcast/examples/systemd/` (`wireplumber.service.d/omarchy-broadcast.conf`, see "Discovery by PipeWire", and `omarchy-broadcastd.service.d/maxine.conf`, see [maxine.md](maxine.md#3-enable-the-drop-in)).

Nothing is enabled. Each user runs:

```sh
systemctl --user enable --now omarchy-broadcastd
```

The runtime dependencies include the NVIDIA driver userspace (`nvidia-utils`), `cuda` (NVRTC, used by GStreamer's CUDA converters), `gst-plugins-good`, `gst-plugins-bad-libs` and a v4l2loopback kernel module (for example `v4l2loopback-dkms`). The loopback device must still be configured as in [loopback.md](loopback.md). The NVIDIA Maxine VFX SDK is never part of the package; it is an optional dependency (`optdepends`) on `maxine-vfx-sdk-local`, which you build yourself from the omarchy-nvidia-sdks repository and install as in [maxine.md](maxine.md).

### Per-user install from a checkout

From the repository root:

```sh
scripts/install-user.sh
```

The script builds the release binaries and installs, without root:

- `omarchy-broadcastd` and `omarchy-broadcast` in `${XDG_BIN_HOME:-~/.local/bin}`;
- the unit in `${XDG_DATA_HOME:-~/.local/share}/systemd/user/`, with `ExecStart=` pointing at the installed daemon;
- the D-Bus activation file in `${XDG_DATA_HOME:-~/.local/share}/dbus-1/services/`;
- the NVIDIA Maxine module: the library `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules/libobc_module_maxine.so` and its manifest `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/maxine-vfx.toml`, whose `library` line names the installed library. The module is built and installed whether or not the Maxine SDK is present; without the SDK the daemon lists it greyed out, with the reason. The SDK, the os-release shim and the drop-in are not installed by the script; they are described in [maxine.md](maxine.md). An older version of the script installed a shim in `~/.local/lib/omarchy-broadcast/`; `--uninstall` and a reinstall remove it;
- a manifest, `install-user.manifest`, in `${XDG_STATE_HOME:-~/.local/state}/omarchy-broadcast/`, which lists every path written (the module's library and manifest included) and the sha256 of each binary and of the module library.

Behaviour:

- It refuses to run as root.
- It marks every text file it writes. It overwrites or removes a file only if it carries that mark (a binary, the module's library included: only if the manifest lists it with the checksum on disk), and stops before changing anything if a target path holds another file. Running it again updates its own install.
- A Maxine manifest at `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/maxine-vfx.toml` that it did not write, such as the hand-made copy of the manual recipe in [maxine.md](maxine.md), has no mark, so the script refuses it. Remove that file and the library its `library` line names, then run the script again. This applies to `--uninstall` as well.
- The data directory must be an absolute path made only of letters, digits and `._/@+-` for an install: its path is written into the Maxine manifest, and with `--mic` into the microphone's unit. Otherwise the script exits with status 2.
- It runs no `systemctl` command and enables nothing; it prints the next steps. A running daemon never reloads a module library: after a run that rebuilt the module, restart the daemon (`systemctl --user restart omarchy-broadcastd`).
- It does not install the WirePlumber drop-in. The Omarchy bar plugin is a separate project (the omarchy-broadcast-plugin repository).
- It warns when a unit in `~/.config/systemd/user` exists, because that location takes precedence (see below).

Options: `--mic` also installs the optional noise-removed microphone ([microphone.md](microphone.md)), `--dry-run` prints every action and changes nothing, `--no-build` installs the binaries and the module library already in `${CARGO_TARGET_DIR:-target}/release`, `--uninstall` removes what the script installed (the Maxine module's library and manifest included), `--help` shows the usage. Exit status: 0 done; 1 refused (a target path holds a file the script did not install, or the unit is still enabled on `--uninstall`) or the build failed; 2 usage error or unusable environment.

Then load and start the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now omarchy-broadcastd
```

### Manual install

1. Install the daemon and the command-line client into `~/.cargo/bin`, from the repository root.

   ```sh
   cargo install --path crates/omarchy-broadcastd
   cargo install --path crates/omarchy-broadcast
   ```

2. Install the unit file with `ExecStart=` rewritten. The file in `dist/` names `/usr/bin/omarchy-broadcastd`, the path of the package; copied unchanged after `cargo install`, the service fails to start (status 203/EXEC).

   ```sh
   mkdir -p ~/.config/systemd/user
   sed 's|^ExecStart=/usr/bin/|ExecStart=%h/.cargo/bin/|' \
       dist/systemd/omarchy-broadcastd.service \
       > ~/.config/systemd/user/omarchy-broadcastd.service
   ```

3. Optional, D-Bus activation: with this file, a command such as `omarchy-broadcast on` starts the service if it is not running. Reads (`omarchy-broadcast status`, `watch`) never start it. See [dbus-api.md](../dbus-api.md#service-presence-and-activation).

   ```sh
   install -Dm644 dist/dbus-1/services/br.com.dias.OmarchyBroadcast.service \
       ~/.local/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service
   ```

4. Optional, discovery through PipeWire: a drop-in that orders WirePlumber after the daemon (see below). In a checkout it is `dist/systemd/wireplumber.service.d/omarchy-broadcast.conf`; the package ships the same file as an example at `/usr/share/doc/omarchy-broadcast/examples/systemd/wireplumber.service.d/omarchy-broadcast.conf`.

   ```sh
   install -Dm644 dist/systemd/wireplumber.service.d/omarchy-broadcast.conf \
       ~/.config/systemd/user/wireplumber.service.d/omarchy-broadcast.conf
   ```

5. Load the units and start the service now and at every login:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now omarchy-broadcastd
   ```

   If the service was enabled before from an older copy of the unit, run `systemctl --user reenable omarchy-broadcastd` so that it is wanted by `default.target`.

The same drop-in applies to the other two routes (step 4); the package does not install it.

### Which unit is used

systemd searches the user unit directories in this order and uses the first match: `~/.config/systemd/user`, then `~/.local/share/systemd/user`, then `/usr/lib/systemd/user`. A copy of the unit in a directory earlier in the list shadows the others. In particular, an older manual copy in `~/.config/systemd/user` keeps starting `~/.cargo/bin/omarchy-broadcastd` after the package is installed; remove it (and run `systemctl --user daemon-reload`) to use the packaged unit. `systemctl --user cat omarchy-broadcastd` shows which file is in force.

Check it:

```sh
systemctl --user status omarchy-broadcastd
omarchy-broadcast status
```

The service is ready (`Type=dbus`) once it owns its bus name, which it requests after the loopback writer has started: at that point the virtual camera is already listed as a capture device.

### Restarts and crashes

A clean restart (`systemctl --user restart omarchy-broadcastd`, an upgrade) is invisible to applications that hold the virtual camera: the old writer sends placeholders until it closes and the new one starts within about 50 ms.

If the daemon crashes, the unit restarts it after 250 ms (`RestartSec=250ms`). The camera picture returns once the new process has opened the camera and initialised CUDA, which takes seconds; until then applications see black. To keep Chromium-based applications (Chrome, Edge, Teams, Zoom in the browser, Discord, Slack and other Electron applications) from losing the camera in that time, the daemon prepares the loopback at every writer start: it uploads a black image and sets the driver's `timeout`, so the driver keeps serving black frames after the daemon is gone ([loopback.md](loopback.md#what-the-daemon-sets-on-the-device), [ADR-0022](../adr/0022-crash-recovery-for-consumers.md)). The outcome is in the journal:

```text
crash cover: black timeout image, timeout 250 ms (consumers keep receiving black frames if the daemon stops writing)
crash cover not in place: <reason>; a consumer may lose the camera if the daemon crashes
```

The warning is only that: the daemon runs, an older v4l2loopback without the controls included.

A crash loop is stopped: after 5 starts within 60 s (`StartLimitBurst=5`, `StartLimitIntervalSec=60`) systemd stops restarting the unit and it is `failed`. Manual starts and restarts count too. Bring it back with:

```sh
systemctl --user reset-failed omarchy-broadcastd
systemctl --user start omarchy-broadcastd
```

**PipeWire consumers do not reconnect after a crash.** The PipeWire node lives in the daemon process: when it dies, WirePlumber destroys the consumer's stream and the consumer does not come back by itself. An application that has to survive a crash should use the loopback device.

## Discovery by PipeWire

WirePlumber probes video devices when it starts. A loopback device that has no writer at that moment is recorded as output-only and not probed again, so applications that list cameras through PipeWire (the camera portal, OBS's PipeWire source, browsers using PipeWire) would not show it. Two remedies:

- The drop-in (manual install, step 4) starts WirePlumber after the daemon is ready. Run `systemctl --user daemon-reload` after installing it, and restart WirePlumber once (below) for the current session.
- Without the drop-in, or after the daemon was restarted by hand: `systemctl --user restart wireplumber` while the daemon runs.

Applications that open `/dev/video*` directly are not affected. Meanwhile, `gst-device-monitor-1.0 -i Video/Source` lists the camera through the v4l2 provider. Details are in [testing.md](../testing.md#discovery-by-pipewire). The daemon can also register its own PipeWire source, which needs none of this: see the next section.

A v4l2loopback device serves one capture stream at a time: a second application that opens the same device gets `EBUSY` at `VIDIOC_S_FMT` or `VIDIOC_REQBUFS`. Applications that need the camera at the same moment should use the PipeWire node, which serves several consumers, or the loopback should have more devices ([loopback.md](loopback.md)).

## The PipeWire output

Optionally the daemon also registers a native PipeWire video source, "Omarchy Broadcast", next to the loopback device ([ADR-0016](../adr/0016-pipewire-output.md)). The loopback stays as it is and remains the default. Applications that use PipeWire for cameras find the node without WirePlumber having to probe the loopback. It is off in two places: the daemon must be built with the cargo feature, and the configuration must enable it.

**Build.** The Arch package is built with the feature. From source, build or install with it; it needs the libpipewire headers and clang at build time and links `libpipewire-0.3`:

```sh
cargo build --release --features omarchy-broadcastd/pipewire
cargo install --path crates/omarchy-broadcastd --features pipewire
```

`scripts/install-user.sh` builds without it.

**Enable.** In `~/.config/omarchy-broadcast/config.toml`:

```toml
[pipewire]
enabled = true
```

Then reload (`systemctl --user reload omarchy-broadcastd`). The node appears at once. Setting it back to `false` removes it at once if no application is linked to it, otherwise when the last one leaves. The loopback is not touched either way. The node exists only while the loopback writer runs.

**Check.**

```sh
omarchy-broadcast status          # Consumers: 0 (loopback 0, PipeWire 0), PipeWire: ready
pw-cli ls Node | grep -B2 -A8 omarchy-broadcast
gst-launch-1.0 pipewiresrc target-object=omarchy-broadcast ! videoconvert ! autovideosink
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

The node offers YUY2, NV12 and I420, in that order, at the output contract's size and rate. YUY2 is first because OBS Studio's PipeWire camera source mis-sizes NV12 rows (see the known issue below); an application that asks for NV12 or I420 explicitly still gets it. An application that asks for another format or size fails to link, and nothing else is affected.

**Offered formats.** NV12 is offered by default. `offer_nv12 = false` in `[pipewire]` offers YUY2 and I420 only:

```toml
[pipewire]
enabled = true
offer_nv12 = false
```

A reload that changes it rebuilds the node, like enabling or disabling it: at once if no application is linked to the node, otherwise when the last one leaves. The loopback is not touched.

### Known issue: OBS Studio and NV12

OBS Studio's PipeWire camera source (`plugins/linux-pipewire`; present in 32.2.2 and on the master branch as of November 2026) computes the row size of an NV12 frame as 2 bytes per pixel, and ignores the stride and the plane offsets the producer announces. For a 1920x1080 frame it reads rows 3840 bytes apart from a buffer that holds 1920 bytes per luma row, so it reads past the end of the buffer and OBS crashes. The chroma plane is not copied either. This is in OBS, not in the node: the node's buffer is a whole NV12 frame in the layout GStreamer's `GstVideoInfo` gives, and other applications read it correctly.

OBS lists the node's formats in the source's properties, in the order the node offers them, and starts with the first one. With the node's order (YUY2 first), OBS uses YUY2, which works. The crash happens only when NV12 is chosen by hand in the source's format selection.

Workarounds, either of them:

- leave the format of the OBS source on YUY2;
- set `offer_nv12 = false` in `[pipewire]` (above), so that OBS cannot choose NV12.

Upstream:

- issue [obsproject/obs-studio#13823](https://github.com/obsproject/obs-studio/issues/13823), closed without a fix;
- pull request [obsproject/obs-studio#14002](https://github.com/obsproject/obs-studio/pull/14002), closed unmerged;
- the lines concerned, at tag 32.2.2: the `bpp` of the NV12 entry in [`formats.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/formats.c#L79), the line size in [`pipewire.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/pipewire.c#L657), and the plane pointers taken from the buffer in [`pipewire.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/pipewire.c#L689-L695).

### Hiding the duplicate

Once WirePlumber has probed the loopback while the writer streams, PipeWire lists "Omarchy Broadcast" twice: the native node, and the loopback through v4l2. To hide the loopback from PipeWire only, create `~/.config/wireplumber/wireplumber.conf.d/50-omarchy-broadcast.conf`:

```
monitor.v4l2.rules = [
  { matches = [ { device.product.name = "Omarchy Broadcast" } ]
    actions = { update-props = { device.disabled = true } } }
]
```

and run `systemctl --user restart wireplumber`. Applications that open `/dev/video*` directly are unaffected. The package does not install this file. With it, the WirePlumber ordering drop-in is unnecessary for applications that use the native node; it stays useful for applications that reach the camera through v4l2 only. Remove the file and restart WirePlumber to undo it.

## Configuration

The configuration is `~/.config/omarchy-broadcast/config.toml` (`$XDG_CONFIG_HOME/omarchy-broadcast/config.toml`). It is optional: a missing file means the defaults. [config.example.toml](../config.example.toml) lists every key with its default and a comment. The daemon never writes this file.

After it the daemon reads the files of `~/.config/omarchy-broadcast/config.d/` (every file ending in `.toml`, in order of the file name), each overriding the earlier ones key by key. Put your own overrides in a file that sorts late, such as `config.d/90-local.toml`. One file there is the daemon's: `config.d/50-panel.toml`, which it writes when a setting is changed through it (the bar's setup dialog, `omarchy-broadcast config set`, `modules enable|disable|profile|pin`, `order set`) and which starts with a comment that says so. Do not edit it by hand. A key that a file after it sets, such as `90-local.toml`, wins, and the daemon refuses to change that key until it is removed from that file. `omarchy-broadcast config show` lists every key with the file that sets it, and `omarchy-broadcast config files` the files in load order. See [config.example.toml](../config.example.toml) and [ADR-0026](../adr/0026-config-d-and-managed-config.md).

After editing it, reload without restarting:

```sh
systemctl --user reload omarchy-broadcastd     # sends SIGHUP
omarchy-broadcast status                       # shows Error: if a file was rejected
```

`busctl --user call br.com.dias.OmarchyBroadcast /br/com/dias/OmarchyBroadcast br.com.dias.OmarchyBroadcast1 ReloadConfig` does the same and returns the reason on failure. An invalid file, in `config.toml` or in `config.d/`, is rejected as a whole: the previous configuration stays in force and the `Error` property names the file, line and key. The files are read at start and on a reload, not when they change.

Settings changed while the daemon runs (`omarchy-broadcast on` / `off`, the camera selection, the video effect with its blur strength and background image, and the stages noise removal, enhancement and relighting with its light, direction and brightness, and the settings of any module) are kept in a separate state file, `~/.local/state/omarchy-broadcast/state.toml` (`$XDG_STATE_HOME/omarchy-broadcast/state.toml`), so that `config.toml` keeps its comments and layout. The state file is written by the daemon. The choices of modules (which are enabled, the stage order, the backend profile and the pins) are configuration and are kept in `config.d/50-panel.toml`; a state file written by the 0.24 daemon, which kept them in its `[modules]` table, is moved there when the daemon first loads. A daemon of 0.24 ignores `config.d`, so after a downgrade it sees only what `config.toml` says of the modules. If the state file cannot be read, the daemon starts `off` and says why in `Error`: privacy fails closed.

What the daemon has learnt about the GPU (its CUDA compute capability, which picks the models made for it, so that it can tell which SDK features are installed without waking the GPU) is kept in `~/.cache/omarchy-broadcast/compute-capability` (`$XDG_CACHE_HOME/omarchy-broadcast/compute-capability`). It is a cache, not a setting: delete it and the daemon learns it again at its next session.

The crash ledger of the modules ([../modules.md](../modules.md#the-crash-ledger)) is a small file, `modules-crash.json`, next to the state file. It records which module's code was running when the daemon last went down, and disables a module that did so twice in a row until `omarchy-broadcast modules enable ID`. Deleting it forgives every module.

The Maxine SDK root can be set in the configuration (`sdk_root` in `[module."br.com.dias.maxine-vfx"]`, or its alias `[effect] maxine_root`) or with `MAXINE_VFX_ROOT`; the Maxine module loads the SDK's libraries from it itself, and `LD_LIBRARY_PATH` is not needed ([maxine.md](maxine.md#4-the-sdk-root-and-the-environment)). The SDK is first loaded by the first effect or noise removal the daemon starts, and a different root then takes a daemon restart; a reload that changes the key says so in the log.

The paths can be overridden with `--config PATH` and `--state PATH` in a copy of the unit file; `config.d` is then the directory next to `PATH`.

## Logs

```sh
journalctl --user -u omarchy-broadcastd -f
```

Add `-v` to `ExecStart` for debug output (consumer counts, teardown times). Exit status 3 means another instance already owns the bus name; systemd does not restart the service on that status.

## Stop, disable, uninstall

Stop and disable the service first, on every route:

```sh
systemctl --user disable --now omarchy-broadcastd
```

Then, depending on how it was installed:

- Arch package: `sudo pacman -R omarchy-broadcast-git`. Remove any drop-in you copied from the examples. The bar plugin is not part of the package; see the omarchy-broadcast-plugin repository for removing it.
- Per-user install: `scripts/install-user.sh --uninstall` from the checkout. It removes only the files it installed, the Maxine module's library and manifest among them, and refuses while the unit is still enabled, hence the command above comes first. A hand-made Maxine manifest stops it too; remove the files named in [maxine.md](maxine.md#8-uninstall) first.
- Manual install:

  ```sh
  rm ~/.config/systemd/user/omarchy-broadcastd.service
  rm -f ~/.local/share/dbus-1/services/br.com.dias.OmarchyBroadcast.service
  rm -f ~/.config/systemd/user/wireplumber.service.d/omarchy-broadcast.conf
  cargo uninstall omarchy-broadcastd omarchy-broadcast
  ```

Finish with `systemctl --user daemon-reload`.
