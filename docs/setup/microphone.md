# Setting up the noise-removed microphone

"Omarchy Broadcast Mic" is an optional virtual microphone: the default microphone with background noise removed. It is a PipeWire `filter-chain` that hosts a LADSPA noise-suppression plugin and exposes the result as a new audio source that any application can select. It runs on the CPU only; it needs no GPU and no NVIDIA SDK. It is independent of `omarchy-broadcastd` and of the virtual camera: neither needs the other. The reasoning is in [ADR-0014](../adr/0014-mic-noise-removal-filter-chain.md).

Two engines are available, each with its own configuration file: RNNoise (the default, light on CPU) and DeepFilterNet (higher quality, more CPU); see [Choosing the engine](#choosing-the-engine). Each runs as its own process, started by one instance of the systemd user unit `omarchy-broadcast-mic@.service`. Both create a source with the same name, so run one at a time.

The configuration files and the unit are in the repository's `dist/` directory (`dist/pipewire/` and `dist/systemd/`). There are three ways to install them; each installs both engines' configurations and the unit, and none enables anything.

## Requirements

- PipeWire with WirePlumber 0.5 (the default on Omarchy).
- A systemd user session.
- At least one LADSPA plugin package. The Arch package lists both as optional dependencies.

```sh
sudo pacman -S noise-suppression-for-voice   # RNNoise engine (default)
sudo pacman -S deepfilternet-ladspa          # DeepFilterNet engine
```

`noise-suppression-for-voice` installs `/usr/lib/ladspa/librnnoise_ladspa.so`; `deepfilternet-ladspa` installs `/usr/lib/ladspa/libdeep_filter_ladspa.so`. The configurations were written against `noise-suppression-for-voice` 1.21 and `deepfilternet-ladspa` 0.5.6.

## Install

Choose one of the three routes.

### From the Arch package

The package installs the configurations as `/usr/share/omarchy-broadcast/pipewire/omarchy-broadcast-mic-rnnoise.conf` and `omarchy-broadcast-mic-deepfilter.conf`, and the unit as `/usr/lib/systemd/user/omarchy-broadcast-mic@.service`. Those directories are not on PipeWire's configuration search path, so the packaged unit names the configuration by absolute path:

```
ExecStart=/usr/bin/pipewire -c /usr/share/omarchy-broadcast/pipewire/omarchy-broadcast-mic-%i.conf
```

The package enables nothing. Install the plugin package (above), then follow [Enable](#enable).

### With `scripts/install-user.sh --mic`

From the repository root, `scripts/install-user.sh --mic` (see [daemon.md](daemon.md#per-user-install-from-a-checkout)) also installs, without root:

- the configurations in `${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/pipewire/`;
- the unit as `${XDG_DATA_HOME:-~/.local/share}/systemd/user/omarchy-broadcast-mic@.service`, with `ExecStart=` pointing at that absolute configuration path.

It enables nothing. `scripts/install-user.sh --uninstall` removes these files; it refuses while an instance is enabled, so disable it first (see [Undo](#undo)).

### Manually, from a checkout

Keep the file names: the unit looks the configuration up by name. PipeWire searches `~/.config/pipewire`, `/etc/pipewire` and `/usr/share/pipewire`. Run the commands from the repository root.

```sh
# the engine or engines you want:
install -Dm644 dist/pipewire/omarchy-broadcast-mic-rnnoise.conf    ~/.config/pipewire/omarchy-broadcast-mic-rnnoise.conf
install -Dm644 dist/pipewire/omarchy-broadcast-mic-deepfilter.conf ~/.config/pipewire/omarchy-broadcast-mic-deepfilter.conf

# the unit (once):
install -Dm644 dist/systemd/omarchy-broadcast-mic@.service ~/.config/systemd/user/omarchy-broadcast-mic@.service
systemctl --user daemon-reload
```

Installing both configurations side by side is fine; only the running instance matters.

### Which unit wins

systemd uses the first unit it finds: one in `~/.config/systemd/user` shadows one in `~/.local/share/systemd/user`, which shadows one in `/usr/lib/systemd/user`. To tune a packaged configuration, copy it to `~/.config/pipewire` and override `ExecStart` with a drop-in, which makes PipeWire look the file up by name:

```sh
install -Dm644 /usr/share/omarchy-broadcast/pipewire/omarchy-broadcast-mic-rnnoise.conf ~/.config/pipewire/
systemctl --user edit omarchy-broadcast-mic@.service
```

with this content in the editor:

```
[Service]
ExecStart=
ExecStart=/usr/bin/pipewire -c omarchy-broadcast-mic-%i.conf
```

## Enable

```sh
systemctl --user enable --now omarchy-broadcast-mic@rnnoise.service
```

Use `omarchy-broadcast-mic@deepfilter.service` for the DeepFilterNet engine. The unit is bound to `pipewire.service`: it stops with PipeWire and is started again whenever PipeWire starts, so it survives `systemctl --user restart pipewire`.

## Using it

"Omarchy Broadcast Mic" appears as an ordinary microphone. Either:

- select it in the application (the input device setting of the meeting, recording or chat program); or
- make it the default input. Find its id, then set it:

```sh
wpctl status                 # under Audio > Filters, find "omarchy-broadcast-mic  [Audio/Source]" and its id
wpctl set-default <id>
```

Making it the default is safe. The filter-chain puts its capture side and its output side in one `node.link-group`, and WirePlumber never links two nodes of the same link group. The capture side therefore does not loop back onto its own output; it follows the best hardware microphone instead. Because the capture side follows the default source, it also moves with the microphone chosen in the desktop's audio settings.

The source is mono at 48 kHz. A stereo microphone is downmixed.

## Choosing the engine

RNNoise is the default: it costs little CPU, which matters on a laptop battery. Its voice-activity gate mutes everything it does not classify as voice (ambience, music, keyboard between words), so a room with no one speaking comes out as silence. Choose DeepFilterNet when you want higher quality, noise removed from under speech, or non-speech sound kept (for example an instrument), and have CPU to spare: it is a neural model that removes noise from inside speech and lowers the noise floor instead of gating. Listening tests found both acceptable.

| | RNNoise | DeepFilterNet |
|---|---|---|
| Strength | Low CPU and memory; steady noise such as fans and hiss | Non-stationary noise such as keyboards and voices in the background; less damage to speech; non-speech sound is kept |
| Weakness | Mutes non-speech sound (VAD gate); weaker on keyboards; can clip the onset of words | More CPU and memory |
| Latency | 10 ms frames | Algorithmic latency of at least 20 ms, per the upstream documentation |
| CPU while recording | About 2.4 % of one core | About 21.5 % of one core |
| Memory | About 14 MB | About 155 MB once a recorder has connected (the model loads then) |
| Package | `noise-suppression-for-voice` | `deepfilternet-ladspa` |

The CPU and memory figures were measured on an Intel Core Ultra 9 275HX with PipeWire 1.6.9, from the process's own CPU time over 30 s of continuous recording. With nothing recording from the source, both use effectively no CPU. The latency values are from the upstream documentation and were not measured. Compare by ear with your own microphone and room.

`wpctl status` lists the source under **Filters**, not under Sources, because WirePlumber groups filter-chain nodes there. Applications list it as an ordinary input device named "Omarchy Broadcast Mic".

## Switching engines

```sh
systemctl --user disable --now omarchy-broadcast-mic@rnnoise.service
systemctl --user enable --now omarchy-broadcast-mic@deepfilter.service
```

Both engines use the node name `omarchy-broadcast-mic`, so applications that remember their input by name keep it across the switch.

## Tuning

Edit the copy in `~/.config/pipewire` (for a packaged or `--mic` install, see [Which unit wins](#which-unit-wins)), then restart the instance:

```sh
systemctl --user restart omarchy-broadcast-mic@rnnoise.service   # or @deepfilter
```

### RNNoise

- `"VAD Threshold (%)"` (0 to 99, shipped 50): frames whose voice probability is below it are muted. Raise it to remove more noise; lower it if speech is cut.
- `"VAD Grace Period (ms)"` (0 to 1000, shipped 200): how long the output stays open after voice was last detected. Raise it if word endings are cut.
- `"Retroactive VAD Grace (ms)"` (0 to 200, shipped 0): opens the output this far back in time so the start of a word gets through, at the cost of that much added latency.

The shipped values are those recommended by the plugin's README.

### DeepFilterNet

`"Attenuation Limit (dB)"` is the maximum noise attenuation, 0 to 100 dB. The shipped value, 100, means no limit (the plugin's default). Lower values leave some of the noise and sound more natural: about 18 to 24 dB for medium reduction, 6 to 12 dB for light reduction. The other plugin controls are listed, commented out, in the file with their ranges and defaults.

## Pinning one microphone

By default the capture side follows the default source. To use one particular microphone regardless of the default, find its `node.name`:

```sh
wpctl inspect <id>           # look for node.name
```

Then, in the `capture.props` block of the configuration, uncomment `target.object` and set it:

```
target.object = "alsa_input.usb-..."
```

Restart the instance.

## Undo

Disable the instances first (both engines):

```sh
systemctl --user disable --now omarchy-broadcast-mic@rnnoise.service omarchy-broadcast-mic@deepfilter.service
```

Then remove the files, according to the route:

- Arch package: `sudo pacman -R omarchy-broadcast-git` removes the daemon too; the microphone files alone are not removable separately. Leaving the instances disabled is enough.
- `install-user.sh --mic`: `scripts/install-user.sh --uninstall`.
- Manual install:

```sh
rm ~/.config/systemd/user/omarchy-broadcast-mic@.service ~/.config/pipewire/omarchy-broadcast-mic-*.conf
systemctl --user daemon-reload
```

Remove a drop-in made with `systemctl --user edit` with `systemctl --user revert omarchy-broadcast-mic@.service`.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| The unit fails to start | Often a missing plugin; see the log | `journalctl --user -u 'omarchy-broadcast-mic@*'` |
| The log says a plugin cannot be loaded | The LADSPA package is not installed. The unit fails on purpose: the filter module has no `nofail` flag, so a missing plugin is visible instead of leaving a silent or missing source | Install `noise-suppression-for-voice` or `deepfilternet-ladspa`, then start the unit again |
| "Omarchy Broadcast Mic" is not in the application's list | The unit is not running, or the application read its device list before the source existed | `systemctl --user status 'omarchy-broadcast-mic@*'`; reopen the application's input settings or restart it |
| The source exists but is silent | No hardware source is available, or the capture side is not linked to one | `wpctl status` to check that a microphone exists and is the default; `pw-link -l` to see whether `omarchy-broadcast-mic.capture` has an input link. If you pinned `target.object`, check the name |
| Two "Omarchy Broadcast Mic" entries | Two instances are running (both engines, or a stale process) | Disable one: `systemctl --user disable --now omarchy-broadcast-mic@<engine>.service` |
| The voice sounds clipped or pumping | The reduction is too strong | DeepFilterNet: lower the attenuation limit. RNNoise: lower the VAD threshold or raise the grace period |
| CPU use while idle | The capture side is passive, so the microphone is not opened while nothing records from the source | Nothing to fix. If the microphone is open with no recorder, check which application is reading "Omarchy Broadcast Mic" or the hardware source |
