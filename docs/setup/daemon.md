# Running omarchy-broadcastd as a user service

This guide covers running and configuring the daemon, `omarchy-broadcastd`, as a systemd user service. You need the daemon on every machine: it is the program that makes the virtual camera work. To install it, follow [INSTALL.md](../../INSTALL.md) first.

The daemon owns the D-Bus name `br.com.dias.OmarchyBroadcast` on the session bus. It starts the camera and the GPU pipeline only while an application uses the virtual camera ([architecture.md](../architecture.md)).

## Before you start

- The v4l2loopback device with the label "Omarchy Broadcast" and `exclusive_caps=1`, set up as in [loopback.md](loopback.md). The daemon finds the device by its label. Without it the daemon runs in the state `error` and retries every 2 s.
- A systemd user session with a D-Bus session bus. This is the normal state of a desktop login.
- Optional, for the GPU path and the Maxine modules: an NVIDIA GPU, the NVIDIA driver (`nvidia-utils`), CUDA (`cuda`, for NVRTC) and GStreamer's `nvcodec` plugin ([README, Requirements](../../README.md#requirements)). See [Without CUDA or without an NVIDIA GPU](#without-cuda-or-without-an-nvidia-gpu).

### Without CUDA or without an NVIDIA GPU

Without an NVIDIA GPU you need nothing extra. The daemon runs the video in system memory ([onnx.md](onnx.md#using-it-on-a-machine-without-an-nvidia-gpu)).

On a machine with an NVIDIA GPU, install `cuda` as well as `nvidia-utils` for the GPU path, because GStreamer's CUDA converters need NVRTC from `cuda`. Without it:

- The default `[video] memory = "auto"` runs the video in system memory. The log and the `video.memory` row say why: "the CUDA toolkit (NVRTC) is not installed; install `cuda` for the GPU path".
- An explicit `memory = "cuda"` fails the camera start with that reason.
- To run without CUDA on purpose, set `[video] memory = "system"`.

## Install

The install steps, the package choices and the install routes are in [INSTALL.md](../../INSTALL.md). Do them first, then return here to run and configure the daemon. The manual route ([Manual install](../../INSTALL.md#manual-install)) needs a rewritten `ExecStart=` in the unit file.

## Start the daemon

`systemctl --user enable --now omarchy-broadcastd` starts the daemon now and at every login. Each install route in [INSTALL.md](../../INSTALL.md) ends with it.

To start the daemon now without enabling it, use D-Bus activation:

```sh
omarchy-broadcast start
```

This starts the service through its D-Bus activation file, if it is not running, and prints the status. It changes no state: the daemon restores what it saved and never turns the camera on by itself. The camera opens when an application uses it. The Arch package and the per-user install ship the activation file; the manual install has it as the optional step 3 ([INSTALL.md](../../INSTALL.md#manual-install)) ([dbus-api.md](../dbus-api.md#service-presence-and-activation)).

Which commands start the daemon:

- Commands that read never start it: `omarchy-broadcast status` and `watch`, among others.
- Commands that change something, such as `omarchy-broadcast on`, start it through the activation file when it is installed.

## Check that it works

```sh
systemctl --user status omarchy-broadcastd
omarchy-broadcast status
```

The service should be `active (running)`, and `status` should print the state, the camera and the output mode. The service is ready (`Type=dbus`) once it owns its bus name. It requests the name after the loopback writer has started, so the virtual camera is already listed as a capture device at that point.

If `status` shows an `Error:` line, see Troubleshooting.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| The daemon is in the state `error` and retries every 2 s | The v4l2loopback device with the label "Omarchy Broadcast" is missing | Set it up as in [loopback.md](loopback.md) |
| The service fails with status 203/EXEC | The unit names `/usr/bin/omarchy-broadcastd`, but the daemon is in `~/.cargo/bin` | Rewrite `ExecStart=` ([Manual install](../../INSTALL.md#manual-install), step 2) |
| An old daemon keeps starting after you installed the package | A copy of the unit in `~/.config/systemd/user` shadows the packaged one | See [Which unit is used](#which-unit-is-used) |
| The unit is `failed` after repeated crashes | systemd's start limit: 5 starts within 60 s | Run `systemctl --user reset-failed omarchy-broadcastd`, then start it ([Restarts and crashes](#restarts-and-crashes)) |
| The daemon exits with status 3 and is not restarted | Another instance already owns the bus name | Stop the other instance, then start the service |
| `omarchy-broadcast status` shows `Error:` after you edited the configuration | The daemon rejected a file as a whole and kept the previous configuration | Read the file, line and key in `Error`, fix it and reload ([Configuration](#configuration)) |
| The daemon runs the video in system memory on an NVIDIA machine | `cuda` (NVRTC) is not installed | Install `cuda`, or set `[video] memory = "system"` on purpose ([Without CUDA or without an NVIDIA GPU](#without-cuda-or-without-an-nvidia-gpu)) |
| A rebuilt module does not take effect | The daemon never reloads a module library | Run `systemctl --user restart omarchy-broadcastd` |
| Applications that list cameras through PipeWire do not show the virtual camera | WirePlumber probed the loopback before the daemon wrote to it | See [Discovery by PipeWire](#discovery-by-pipewire) |
| A second application gets `EBUSY` or no camera | A v4l2loopback device serves one capture stream at a time | See [loopback.md](loopback.md#one-application-at-a-time) |
| A PipeWire consumer does not return after a daemon crash | The PipeWire node lives in the daemon process | Use the loopback device in that application ([Restarts and crashes](#restarts-and-crashes)) |
| OBS Studio crashes when you pick NV12 for the PipeWire source | A bug in OBS | See [Known issue: OBS Studio and NV12](#known-issue-obs-studio-and-nv12) |
| The MX Brio's Show Mode stays unknown | The `hidraw` node belongs to root | Install the udev rule ([The udev rule for the MX Brio](#the-udev-rule-for-the-mx-brio)) |
| A video background shows as a still picture | The CPU plays only some video files | See [Playing a video without an NVIDIA GPU](#playing-a-video-without-an-nvidia-gpu) |

### Which unit is used

systemd searches the user unit directories in this order and uses the first match:

1. `~/.config/systemd/user`
2. `~/.local/share/systemd/user`
3. `/usr/lib/systemd/user`

A copy of the unit in a directory earlier in the list shadows the others. For example, an older manual copy in `~/.config/systemd/user` keeps starting `~/.cargo/bin/omarchy-broadcastd` after you install the package. Remove it and run `systemctl --user daemon-reload` to use the packaged unit. To see which file is in force:

```sh
systemctl --user cat omarchy-broadcastd
```

## Stop, disable, uninstall

Stop and disable the service, on every install route:

```sh
systemctl --user disable --now omarchy-broadcastd
```

Removing the files depends on how you installed: see [Uninstall](../../INSTALL.md#uninstall).

## Optional setup

### The udev rule for the MX Brio

The daemon reads the Logitech MX Brio's Show Mode (desk view or normal) from the camera's HID interface, only while the camera streams ([../cameras/mx-brio.md](../cameras/mx-brio.md#camera-status-in-omarchy-broadcast)). A `hidraw` node belongs to root by default. Without access to it the status stays unknown; nothing else is affected.

The rule `dist/udev/70-omarchy-broadcast-mx-brio.rules` tags the `hidraw` nodes of USB `046d:0944`, and of no other device, with `uaccess`. systemd-logind then gives the user of the active local seat an ACL on the node, read and write. The daemon opens it read-only and never writes to it.

The Arch package installs the rule in `/usr/lib/udev/rules.d/`. The per-user install and the manual install do not, because a rule needs root. To add it from a checkout:

1. Install the rule and reload udev:

   ```sh
   sudo install -Dm644 dist/udev/70-omarchy-broadcast-mx-brio.rules \
       /etc/udev/rules.d/70-omarchy-broadcast-mx-brio.rules
   sudo udevadm control --reload
   ```

2. Unplug and replug the camera. The rule applies to a node when it is created.

Keep the `70-` prefix: the file name must sort before `73-seat-late.rules`, where systemd turns the `uaccess` tag into the ACL.

### Discovery by PipeWire

WirePlumber probes video devices when it starts. A loopback device that has no writer at that moment is recorded as output-only and not probed again. Applications that list cameras through PipeWire (the camera portal, OBS's PipeWire source, browsers using PipeWire) then do not show it. Two remedies:

- Install the drop-in ([Manual install](../../INSTALL.md#manual-install), step 4). It starts WirePlumber after the daemon is ready. Run `systemctl --user daemon-reload` after installing it, and restart WirePlumber once for the current session.
- Without the drop-in, or after you restarted the daemon by hand, restart WirePlumber while the daemon runs:

  ```sh
  systemctl --user restart wireplumber
  ```

Applications that open `/dev/video*` directly are not affected. Meanwhile, `gst-device-monitor-1.0 -i Video/Source` lists the camera through the v4l2 provider. Details are in [testing.md](../testing.md#discovery-by-pipewire).

The daemon can also register its own PipeWire source, which needs none of this: see [The PipeWire output](#the-pipewire-output).

A v4l2loopback device serves one capture stream at a time: a second application that opens the same device gets `EBUSY` at `VIDIOC_S_FMT` or `VIDIOC_REQBUFS`. Applications that need the camera at the same moment should use the PipeWire node, which serves several consumers (with the limitation under [The PipeWire output](#the-pipewire-output)). Or give the loopback more devices. [loopback.md](loopback.md#one-application-at-a-time) explains why the limit exists and which applications share the camera by themselves.

### The PipeWire output

The daemon can also register a native PipeWire video source, "Omarchy Broadcast", next to the loopback device. The loopback stays as it is and remains the default. Applications that use PipeWire for cameras find the node without WirePlumber having to probe the loopback.

It is off in two places: the daemon must be built with the cargo feature, and the configuration must enable it.

**Build.** The Arch package is built with the feature. From source, build or install with it. It needs the libpipewire headers and clang at build time and links `libpipewire-0.3`:

```sh
cargo build --release --features omarchy-broadcastd/pipewire
cargo install --path crates/omarchy-broadcastd --features pipewire
```

`scripts/install-user.sh` builds without it.

**Enable.** Add this to `~/.config/omarchy-broadcast/config.toml`:

```toml
[pipewire]
enabled = true
```

Then reload:

```sh
systemctl --user reload omarchy-broadcastd
```

The node appears at once. Setting `enabled` back to `false` removes it at once if no application is linked to it, otherwise when the last one leaves. The loopback is not touched either way. The node exists only while the loopback writer runs.

**Check.**

```sh
omarchy-broadcast status          # Consumers: 0 (loopback 0, PipeWire 0), PipeWire: ready
pw-cli ls Node | grep -B2 -A8 omarchy-broadcast
gst-launch-1.0 pipewiresrc target-object=omarchy-broadcast ! videoconvert ! autovideosink
```

`PipeWire:` in `status` shows one of these:

| Value | Meaning |
|---|---|
| `off` | The output is not enabled |
| `connecting` | The connection to PipeWire is being made |
| `ready` | The node is registered |
| `streaming` | An application is linked |
| `failed` | PipeWire is not running or the connection was lost. The daemon retries every 2 s, stays usable, and the loopback keeps working |
| `unsupported` | The configuration enables the node in a daemon built without the feature |

A PipeWire consumer starts the camera like a loopback consumer does, and `Consumers` is the total of both kinds.

The node carries exactly the frames the loopback carries: the same contract and timestamps, black placeholders while no camera runs, and the last frame repeated (not black) while the output pauses for an effect change. Nothing is sent while no application streams from the node. No colour range or matrix is announced to consumers.

**Applications.**

| Application | Setting |
|---|---|
| Firefox | Uses the camera portal. The preference `media.webrtc.camera.allow-pipewire` in `about:config` must be `true` (the default in some distributions) |
| Chromium, Chrome, Electron applications | Off by default. Enable the "PipeWire Camera support" flag (`chrome://flags`, feature `WebRtcPipeWireCamera`), or start the browser with `--enable-features=WebRtcPipeWireCamera` |
| OBS Studio 30.1 and later | Add a "Video Capture Device (PipeWire)" source |
| GNOME Snapshot, GStreamer `pipewiresrc` | Work without a setting |
| Zoom, Discord | Do not use the PipeWire camera. Select the virtual camera (the loopback device) |

**Formats.** The node offers YUY2, NV12 and I420, in that order, at the output contract's size and rate. YUY2 is first because OBS Studio's PipeWire camera source mis-sizes NV12 rows ([Known issue](#known-issue-obs-studio-and-nv12)). An application that asks for NV12 or I420 explicitly still gets it. An application that asks for another format or size fails to link, and nothing else is affected.

NV12 is offered by default. To offer YUY2 and I420 only, set `offer_nv12 = false`:

```toml
[pipewire]
enabled = true
offer_nv12 = false
```

A reload that changes it rebuilds the node, like enabling or disabling it: at once if no application is linked to the node, otherwise when the last one leaves. The loopback is not touched.

**Known limitation: consumers that ask for different formats.** The node has one port and one negotiated format. While one consumer streams a format (YUY2, say), a second one that accepts only another (NV12, say) fails to link. The first one can stop receiving frames from the moment the second connects. The stream recovers once every consumer has disconnected. Consumers that ask for the same format run together.

This is one reason the PipeWire output is optional and off by default. Where applications that ask for different formats must run at the same time, let one of them use the loopback device instead.

#### Known issue: OBS Studio and NV12

OBS Studio's PipeWire camera source (`plugins/linux-pipewire`; present in 32.2.2 and on the master branch as of November 2026) computes the row size of an NV12 frame as 2 bytes per pixel. It ignores the stride and the plane offsets the producer announces. For a 1920x1080 frame it reads rows 3840 bytes apart from a buffer that holds 1920 bytes per luma row, so it reads past the end of the buffer and OBS crashes. The chroma plane is not copied either.

This is in OBS, not in the node: the node's buffer is a whole NV12 frame in the layout GStreamer's `GstVideoInfo` gives, and other applications read it correctly.

OBS lists the node's formats in the source's properties, in the order the node offers them, and starts with the first one. With the node's order (YUY2 first), OBS uses YUY2, which works. The crash happens only when you choose NV12 by hand in the source's format selection.

Workarounds, either of them:

- Leave the format of the OBS source on YUY2.
- Set `offer_nv12 = false` in `[pipewire]` ([Formats](#the-pipewire-output)), so that OBS cannot choose NV12.

Upstream:

- issue [obsproject/obs-studio#13823](https://github.com/obsproject/obs-studio/issues/13823), closed without a fix;
- pull request [obsproject/obs-studio#14002](https://github.com/obsproject/obs-studio/pull/14002), closed unmerged;
- the lines concerned, at tag 32.2.2: the `bpp` of the NV12 entry in [`formats.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/formats.c#L79), the line size in [`pipewire.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/pipewire.c#L657), and the plane pointers taken from the buffer in [`pipewire.c`](https://github.com/obsproject/obs-studio/blob/32.2.2/plugins/linux-pipewire/pipewire.c#L689-L695).

#### Hiding the duplicate

Once WirePlumber has probed the loopback while the writer streams, PipeWire lists "Omarchy Broadcast" twice: the native node, and the loopback through v4l2. To hide the loopback from PipeWire only, create `~/.config/wireplumber/wireplumber.conf.d/50-omarchy-broadcast.conf`:

```
monitor.v4l2.rules = [
  { matches = [ { device.product.name = "Omarchy Broadcast" } ]
    actions = { update-props = { device.disabled = true } } }
]
```

Then restart WirePlumber:

```sh
systemctl --user restart wireplumber
```

Applications that open `/dev/video*` directly are unaffected. The package does not install this file. With it, the WirePlumber ordering drop-in is unnecessary for applications that use the native node. It stays useful for applications that reach the camera through v4l2 only. Remove the file and restart WirePlumber to undo it.

## Details

### Restarts and crashes

A clean restart (`systemctl --user restart omarchy-broadcastd`, or an upgrade) is invisible to applications that hold the virtual camera. The old writer sends placeholders until it closes, and the new one starts within about 50 ms.

If the daemon crashes, the unit restarts it after 250 ms (`RestartSec=250ms`). The camera picture returns once the new process has opened the camera and initialised CUDA, which takes seconds. Until then applications see the crash cover's image: black, or the away image when `[output] placeholder = "image"` and `[effect] fallback_image` are set.

Chromium-based applications (Chrome, Edge, Teams, Zoom in the browser, Discord, Slack and other Electron applications) could lose the camera in that time. To prevent it, the daemon prepares the loopback at every writer start. It uploads the cover's image (black, or the away image) and sets the driver's `timeout`, so the driver keeps serving that image after the daemon is gone ([loopback.md](loopback.md#what-the-daemon-sets-on-the-device)). The outcome is in the journal:

```text
crash cover in place, timeout 250 ms (consumers keep receiving its timeout image if the daemon stops writing)
crash cover shows black
crash cover shows the away image <path>
crash cover shows black in place of the away image: <reason>
crash cover not in place: <reason>; a consumer may lose the camera if the daemon crashes
```

The last line is only a warning. The daemon runs; an older v4l2loopback without the controls is one cause. The `crash cover shows` lines are logged once a cover is in place, and again whenever what it shows changes. The line "crash cover shows black in place of the away image" is a warning that the away image cannot be shown, with its reason;  black is shown instead.

A crash loop is stopped. After 5 starts within 60 s (`StartLimitBurst=5`, `StartLimitIntervalSec=60`) systemd stops restarting the unit and it is `failed`. Manual starts and restarts count too. Bring it back with:

```sh
systemctl --user reset-failed omarchy-broadcastd
systemctl --user start omarchy-broadcastd
```

**PipeWire consumers do not reconnect after a crash.** The PipeWire node lives in the daemon process. When the daemon dies, WirePlumber destroys the consumer's stream, and the consumer does not come back by itself. An application that has to survive a crash should use the loopback device.

### Where your backgrounds are

The images you add as backgrounds, and your away images, are kept in a folder called `omarchy-broadcast` in your Pictures folder (`$(xdg-user-dir PICTURES)`, else `~/Pictures`). The away images are in its `away` folder. The videos, and the WebM made from an animated image, are kept in a folder of the same name in your Videos folder (`$(xdg-user-dir VIDEOS)`, else `~/Videos`).

A file you copy there yourself is listed the next time the gallery is shown. The folders have nothing to do with the desktop's wallpapers. A gallery that an earlier version kept in `~/.local/share/omarchy-broadcast/media` is moved there once, when the daemon starts. The posters of videos are in the cache, `~/.cache/omarchy-broadcast/`, and you can delete them at any time.

### Playing a video without an NVIDIA GPU

A video background plays on every machine, but how depends on the GPU. With an NVIDIA GPU the daemon plays any accepted video as it is. Without one (`[video] memory = system`, or no NVIDIA GPU at all) the CPU plays only one kind of file.

**What plays on the CPU.** A **VP9** video, **profile 0** (8-bit 4:2:0), in a **WebM or Matroska** file, of **at most 1920x1080** and **at most 30 frames a second**. The two loops that come with the package are such files. So is every animated GIF, WebP or APNG you import, because the daemon converts those to a WebM of this kind at import.

**What happens otherwise.** An H.264, HEVC or AV1 video, or a VP9 one that is larger or faster, is shown as its **poster, a still picture** (the first frame), behind you. `omarchy-broadcast status` shows why in its warning:

```text
The video background is shown as a still: it can't play without an NVIDIA GPU; run omarchy-broadcast media convert-command '/home/user/Videos/clip.mp4' for the conversion command
```

The CPU plays VP9 only, on purpose: it is the one CPU decoder that ships in a required GStreamer package, so the same file behaves the same on every machine, and it costs two to four times less CPU than H.264, HEVC or AV1 at 1080p. Hardware decoding on an integrated GPU (VA-API) is not used, so a video that the integrated GPU could decode still shows as its poster until you convert it.

The daemon does not convert the video itself. It tells you how, and you decide when.

**Convert it.** Run the command the warning names, with the file as its argument. It prints one line, an `ffmpeg` command, and runs nothing:

```sh
omarchy-broadcast media convert-command ~/Videos/clip.mp4
```

```text
ffmpeg -nostdin -n -protocol_whitelist file -i '/home/user/Videos/clip.mp4' -map 0:v:0 -an -sn -dn -map_metadata -1 -vf 'scale=w=max(2\,trunc(min(1\,min(1920/iw\,1080/ih))*iw/2)*2):h=max(2\,trunc(min(1\,min(1920/iw\,1080/ih))*ih/2)*2),setsar=1' -fpsmax 30 -pix_fmt yuv420p -c:v libvpx-vp9 -profile:v 0 -deadline realtime -cpu-used 8 -row-mt 1 -crf 32 -b:v 8M -g 60 -force_key_frames 'expr:gte(t,n_forced*2)' -f webm '/home/user/Videos/omarchy-broadcast/clip-cpu.webm'
```

Copy that line into a terminal and run it. It needs `ffmpeg` with `libvpx-vp9`. It makes a VP9 WebM that fits inside 1920x1080, at most 30 frames a second, without audio. The command refuses to overwrite a file (`-n`), and your original is not changed. To convert the same file twice, print the command again for a new output name.

**Where the result goes.** The output is written to `omarchy-broadcast` in your **Videos** folder, as `<name>-cpu.webm` (`clip-cpu.webm` above; `clip-cpu-2.webm` when that name is taken). It is listed with your other videos the next time the gallery is shown, and plays on the CPU when you choose it.

**In the panel.** The Backgrounds section of the bar's setup dialog (bar plugin) lists your videos. A video the CPU cannot play has a note under it, the same command, and a **Copy** button that puts the command on the clipboard. Paste it in a terminal. The panel needs a daemon of interface revision 19 or later.

A video chosen from outside the gallery, by a path in `config.toml`, has no row in the panel. But `omarchy-broadcast media convert-command PATH` works for any file ([../cli-json.md](../cli-json.md#media)). The daemon does not make the converted copy itself. The command makes it in a folder you see and keep, and the daemon needs no conversion queue or cache.

### Configuration

The configuration is `~/.config/omarchy-broadcast/config.toml` (`$XDG_CONFIG_HOME/omarchy-broadcast/config.toml`). It is optional: a missing file means the defaults. [config.example.toml](../config.example.toml) lists every key with its default and a comment. The daemon never writes this file.

After it, the daemon reads the files of `~/.config/omarchy-broadcast/config.d/`: every file ending in `.toml`, in order of the file name. Each overrides the earlier ones key by key. Put your own overrides in a file that sorts late, such as `config.d/90-local.toml`.

One file there is the daemon's: `config.d/50-panel.toml`. It writes that file when a setting is changed through it (the bar's setup dialog, `omarchy-broadcast config set`, `modules enable|disable|profile|pin`, `order set`), and the file starts with a comment that says so. Do not edit it by hand. A key that a later file sets, such as `90-local.toml`, wins, and the daemon refuses to change that key until you remove it from that file.

To see where each setting comes from:

```sh
omarchy-broadcast config show     # every key with the file that sets it
omarchy-broadcast config files    # the files in load order
```

See also [config.example.toml](../config.example.toml).

**Reload after editing**, without restarting:

```sh
systemctl --user reload omarchy-broadcastd     # sends SIGHUP
omarchy-broadcast status                       # shows Error: if a file was rejected
```

This `busctl` call does the same and returns the reason on failure:

```sh
busctl --user call br.com.dias.OmarchyBroadcast /br/com/dias/OmarchyBroadcast br.com.dias.OmarchyBroadcast1 ReloadConfig
```

An invalid file, in `config.toml` or in `config.d/`, is rejected as a whole. The previous configuration stays in force, and the `Error` property names the file, line and key. The files are read at start and on a reload, not when they change.

**State file.** Settings changed while the daemon runs are kept in a separate state file, `~/.local/state/omarchy-broadcast/state.toml` (`$XDG_STATE_HOME/omarchy-broadcast/state.toml`), so that `config.toml` keeps its comments and layout. The daemon writes it. It holds:

- `omarchy-broadcast on` / `off`;
- the camera selection;
- the video effect, with its blur strength and background image;
- the stages noise removal, enhancement and relighting, with its light, direction and brightness;
- the settings of any module.

If the daemon cannot read the state file, it starts `off` and says why in `Error`: privacy fails closed.

The choices of modules (which are enabled, the stage order, the backend profile and the pins) are configuration, kept in `config.d/50-panel.toml`. A state file written by the 0.24 daemon, which kept them in its `[modules]` table, is moved there when the daemon first loads. A daemon of 0.24 ignores `config.d`, so after a downgrade it sees only what `config.toml` says of the modules.

**Cache.** What the daemon has learnt about the GPU is kept in `~/.cache/omarchy-broadcast/compute-capability` (`$XDG_CACHE_HOME/omarchy-broadcast/compute-capability`). It is the CUDA compute capability, which picks the models made for it and lets the daemon tell which SDK features are installed without waking the GPU. It is a cache, not a setting: delete it and the daemon learns it again at its next session.

**Crash ledger.** The crash ledger of the modules ([../modules.md](../modules.md#the-crash-ledger)) is a small file, `modules-crash.json`, next to the state file. It records which module's code was running when the daemon last went down. It disables a module that did so twice in a row until you run `omarchy-broadcast modules enable ID`. Deleting the file forgives every module.

**Maxine SDK root.** You can set it in the configuration (`sdk_root` in `[module."br.com.dias.maxine-vfx"]`, or its alias `[effect] maxine_root`) or with `MAXINE_VFX_ROOT`. The Maxine module loads the SDK's libraries from it itself, and `LD_LIBRARY_PATH` is not needed ([maxine.md](maxine.md#4-the-sdk-root-and-the-environment)). The first effect or noise removal the daemon starts loads the SDK first. A different root then takes a daemon restart; a reload that changes the key says so in the log.

**Other paths.** Override the paths with `--config PATH` and `--state PATH` in a copy of the unit file. `config.d` is then the directory next to `PATH`.

### Logs

```sh
journalctl --user -u omarchy-broadcastd -f
```

Add `-v` to `ExecStart` for debug output (consumer counts, teardown times). Exit status 3 means another instance already owns the bus name. systemd does not restart the service on that status.

## Publishing to the AUR

This part is for maintainers. The AUR holds two recipes of the split package ([Prebuilt or built from source](../../INSTALL.md#prebuilt-or-built-from-source)): the source one, pkgbase `omarchy-broadcast-git`, and the prebuilt one, pkgbase `omarchy-broadcast-bin` ([The prebuilt packages](#the-prebuilt-packages-omarchy-broadcast-bin)). This section covers the source one first.

The AUR copy of `packaging/arch/PKGBUILD` differs from it only in where it builds from. It clones `https://github.com/rsd/omarchy-broadcast.git` instead of the local checkout, and its header comment says so. `scripts/aur.sh` makes that copy:

- It replaces exactly the local-checkout header paragraph, the build hint and the `source=` line. It fails when one of them is not in the PKGBUILD exactly once. A change to one of those lines is made in the script too.
- It generates the `.SRCINFO` with `LC_ALL=C makepkg --printsrcinfo` on the copy, alone in an empty directory, in an environment holding only `PATH` and an empty `HOME`.
- It copies the install scriptlet, `packaging/arch/omarchy-broadcast.install`, unchanged. The scriptlet lies beside the copy when the `.SRCINFO` is generated, because makepkg checks that it exists.
- The AUR repository holds exactly `PKGBUILD`, `.SRCINFO`, `omarchy-broadcast.install` and `LICENSE` (0BSD, written by the script too). `--aur` and `--check` refuse any other file in it.

```sh
scripts/aur.sh                  # prints the AUR PKGBUILD
scripts/aur.sh --srcinfo        # prints its .SRCINFO
scripts/aur.sh --check DIR      # fails, with a diff, when DIR's files are stale or DIR holds another file
scripts/aur.sh --aur DIR        # writes PKGBUILD, .SRCINFO, omarchy-broadcast.install and LICENSE into DIR
```

`DIR` must be the top of a git clone of the AUR repository. The script commits nothing. The clone is kept in `aur/omarchy-broadcast-git`, which this repository ignores. To publish, after the release commit is pushed to GitHub:

```sh
git clone ssh://aur@aur.archlinux.org/omarchy-broadcast-git.git aur/omarchy-broadcast-git   # once
scripts/aur.sh --aur aur/omarchy-broadcast-git
cd aur/omarchy-broadcast-git
git diff                        # review
git add PKGBUILD .SRCINFO omarchy-broadcast.install LICENSE
git commit -m '<version>-1'
git push
```

The committed `pkgver=` is the release version. makepkg replaces it with the output of `pkgver()` (`<version>.r<commits>.g<hash>`), which sorts after it. A new AUR commit is needed only when the PKGBUILD changes (a dependency, a file installed, a new version in `pkgver=`), not for every commit of the repository.

After a push, the AUR's web interface shows the new version at once. Its RPC index, which AUR helpers such as `yay` query, can take a few minutes to follow, so `yay` may still offer the previous version for a while. This needs no action.

### The prebuilt packages (`omarchy-broadcast-bin`)

The `-bin` recipe installs the files of a GitHub release, so you make and publish the release first. Each package of the split package has one asset, `<name>-<version>-x86_64.tar.zst`. It is the `usr/` tree of the `-git` package as it is, with its licence directory named `<name>-bin`. The recipe pins every asset by the sha256 in the release's `SHA256SUMS`. It has no `build()` and no build dependencies.

`scripts/release-bin.sh [--nocheck] [--out DIR]` makes the assets:

- It releases the commit `HEAD` names when it starts, and the workspace version of that commit. It refuses when the `pkgver=` of that commit's `packaging/arch/PKGBUILD` is another. It warns that uncommitted changes are not built.
- It builds the five `-git` packages of that commit with `makepkg` in a scratch directory, without installing anything and without `-s`, so the build dependencies must be installed. `--nocheck` skips `check()`, the workspace test suite.
- It writes the assets, `SHA256SUMS` and, in `pkg/`, the `-git` packages they were made of into the directory `--out` names. That directory must not exist or must be empty. The script has a default and prints the path when it is done. It writes into a temporary directory beside it and moves it into place only when the release is complete, so a failure leaves nothing behind.

Before any asset is kept, the script runs a gate that keeps the NVIDIA SDKs and ONNX Runtime out of the release:

- Every file of an asset must be one that the package is expected to hold. There is no other shared library, no model or engine file and no `models/` directory.
- Every ELF file must need only the libraries on its own allowlist (the modules and the LADSPA plugin: only the C runtime) and define only its own dynamic symbols (a module: `obc_module_abi_version` and `obc_module_entry`).
- The libraries must hold none of the strings that only SDK code linked in statically carries.

A Maxine module that fails the gate is left out of the release, and the script says so. If the core or the ONNX module fails it, the script fails.

The script does not publish anything. It prints, and does not run, the commands for the rest: tag the commit and push the tag, then `gh release create` with the assets and `SHA256SUMS`, then the update of the AUR repository. Run them in this order. The assets must be on the GitHub release before the push to the AUR, because the `-bin` sources download from it:

1. Push the release commit to GitHub, then tag and push the tag.
2. Create the release `v<version>` on GitHub with the assets and `SHA256SUMS`.
3. Make the AUR recipe from the release directory, and review it:

   ```sh
   git clone ssh://aur@aur.archlinux.org/omarchy-broadcast-bin.git aur/omarchy-broadcast-bin   # once
   scripts/aur.sh --bin RELEASE_DIR --aur aur/omarchy-broadcast-bin
   scripts/aur.sh --bin RELEASE_DIR --check aur/omarchy-broadcast-bin
   cd aur/omarchy-broadcast-bin
   git diff                        # review
   git add PKGBUILD .SRCINFO omarchy-broadcast.install LICENSE
   git commit -m 'Update to <version>'
   git push origin HEAD:master
   ```

`scripts/aur.sh --bin RELEASE_DIR` has no package data of its own. The description, licences, dependencies, optional dependencies, provides and conflicts come from the `.PKGINFO` of each `-git` package in `RELEASE_DIR/pkg`. Before it writes anything, it checks that every asset matches `SHA256SUMS` and holds the `usr/` tree of its `-git` package unchanged, apart from the licence directory name.

The packages of the recipe are those of the assets `SHA256SUMS` lists, so a Maxine module left out of the release is left out of the recipe. `--srcinfo` prints the `.SRCINFO`, and `--check` and `--aur` behave as for the `-git` recipe. The AUR repository holds the same four files. A new AUR commit is needed for every release, since `pkgver=` is the release version.
