# Setting up the noise-removed microphone

"Omarchy Broadcast Mic" is an optional virtual microphone: the default microphone with background noise removed. It is a PipeWire `filter-chain` that hosts a LADSPA noise-suppression plugin and exposes the result as a new audio source that any application can select. It runs on the CPU only; it needs no GPU and no NVIDIA SDK. It is independent of `oma-broadcastd` and of the virtual camera: neither needs the other. The reasoning is in [ADR-0014](../adr/0014-mic-noise-removal-filter-chain.md).

Two engines are available, each with its own configuration file: DeepFilterNet and RNNoise. Each runs as its own process, started by one instance of the systemd user unit `oma-broadcast-mic@.service`. Both create a source with the same name, so run one at a time.

The configuration files and the unit are in the repository's `dist/` directory (`dist/pipewire/` and `dist/systemd/`). The Arch package does not install them yet; copy them from a source checkout, as below. Run the commands from the repository root.

## Requirements

- PipeWire with WirePlumber 0.5 (the default on Omarchy).
- A systemd user session.
- At least one LADSPA plugin package:

```sh
sudo pacman -S deepfilternet-ladspa          # DeepFilterNet engine
sudo pacman -S noise-suppression-for-voice   # RNNoise engine
```

`deepfilternet-ladspa` installs `/usr/lib/ladspa/libdeep_filter_ladspa.so`; `noise-suppression-for-voice` installs `/usr/lib/ladspa/librnnoise_ladspa.so`. The configurations were written against `deepfilternet-ladspa` 0.5.6 and `noise-suppression-for-voice` 1.21.

## Install

Keep the file names: the unit looks the configuration up by name. PipeWire searches `~/.config/pipewire`, `/etc/pipewire` and `/usr/share/pipewire`.

```sh
# the engine or engines you want:
install -Dm644 dist/pipewire/oma-broadcast-mic-deepfilter.conf ~/.config/pipewire/oma-broadcast-mic-deepfilter.conf
install -Dm644 dist/pipewire/oma-broadcast-mic-rnnoise.conf    ~/.config/pipewire/oma-broadcast-mic-rnnoise.conf

# the unit (once):
install -Dm644 dist/systemd/oma-broadcast-mic@.service ~/.config/systemd/user/oma-broadcast-mic@.service
systemctl --user daemon-reload
```

Installing both configurations side by side is fine; only the running instance matters.

## Enable

```sh
systemctl --user enable --now oma-broadcast-mic@deepfilter.service
```

Use `oma-broadcast-mic@rnnoise.service` for the RNNoise engine. The unit is bound to `pipewire.service`: it stops with PipeWire and is started again whenever PipeWire starts, so it survives `systemctl --user restart pipewire`.

## Using it

"Omarchy Broadcast Mic" appears as an ordinary microphone. Either:

- select it in the application (the input device setting of the meeting, recording or chat program); or
- make it the default input. Find its id, then set it:

```sh
wpctl status                 # under Audio > Filters, find "oma-broadcast-mic  [Audio/Source]" and its id
wpctl set-default <id>
```

Making it the default is safe. The filter-chain puts its capture side and its output side in one `node.link-group`, and WirePlumber never links two nodes of the same link group. The capture side therefore does not loop back onto its own output; it follows the best hardware microphone instead. Because the capture side follows the default source, it also moves with the microphone chosen in the desktop's audio settings.

The source is mono at 48 kHz. A stereo microphone is downmixed.

## Choosing the engine

| | DeepFilterNet | RNNoise |
|---|---|---|
| Strength | Non-stationary noise such as keyboards and voices in the background; less damage to speech | Steady noise such as fans and hiss |
| Weakness | More CPU | Weaker on keyboards; can clip the onset of words |
| Latency | Algorithmic latency of at least 20 ms, per the upstream documentation | 10 ms frames |
| CPU while recording | About 20 % of one core | About 2.5 % of one core |
| Memory | About 155 MB once a recorder has connected (the model loads then) | About 14 MB |
| Package | `deepfilternet-ladspa` | `noise-suppression-for-voice` |

The CPU and memory figures were measured on an Intel Core Ultra 9 275HX with PipeWire 1.6.9, from the process's own CPU time over 30 s of continuous recording. With nothing recording from the source, both use effectively no CPU. The latency values are from the upstream documentation and were not measured. With the default settings, RNNoise mutes everything it does not classify as voice, so a room with no one speaking comes out as silence; DeepFilterNet lowers the noise floor instead. Try DeepFilterNet first and compare by ear with your own microphone and room.

`wpctl status` lists the source under **Filters**, not under Sources, because WirePlumber groups filter-chain nodes there. Applications list it as an ordinary input device named "Omarchy Broadcast Mic".

## Switching engines

```sh
systemctl --user disable --now oma-broadcast-mic@deepfilter.service
systemctl --user enable --now oma-broadcast-mic@rnnoise.service
```

Both engines use the node name `oma-broadcast-mic`, so applications that remember their input by name keep it across the switch.

## Tuning

Edit the copy in `~/.config/pipewire`, then restart the instance:

```sh
systemctl --user restart oma-broadcast-mic@deepfilter.service   # or @rnnoise
```

### DeepFilterNet

`"Attenuation Limit (dB)"` is the maximum noise attenuation, 0 to 100 dB. The shipped value, 100, means no limit (the plugin's default). Lower values leave some of the noise and sound more natural: about 18 to 24 dB for medium reduction, 6 to 12 dB for light reduction. The other plugin controls are listed, commented out, in the file with their ranges and defaults.

### RNNoise

- `"VAD Threshold (%)"` (0 to 99, shipped 50): frames whose voice probability is below it are muted. Raise it to remove more noise; lower it if speech is cut.
- `"VAD Grace Period (ms)"` (0 to 1000, shipped 200): how long the output stays open after voice was last detected. Raise it if word endings are cut.
- `"Retroactive VAD Grace (ms)"` (0 to 200, shipped 0): opens the output this far back in time so the start of a word gets through, at the cost of that much added latency.

The shipped values are those recommended by the plugin's README.

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

```sh
systemctl --user disable --now oma-broadcast-mic@deepfilter.service oma-broadcast-mic@rnnoise.service
rm ~/.config/systemd/user/oma-broadcast-mic@.service ~/.config/pipewire/oma-broadcast-mic-*.conf
systemctl --user daemon-reload
```

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| The unit fails to start | Often a missing plugin; see the log | `journalctl --user -u 'oma-broadcast-mic@*'` |
| The log says a plugin cannot be loaded | The LADSPA package is not installed. The unit fails on purpose: the filter module has no `nofail` flag, so a missing plugin is visible instead of leaving a silent or missing source | Install `deepfilternet-ladspa` or `noise-suppression-for-voice`, then start the unit again |
| "Omarchy Broadcast Mic" is not in the application's list | The unit is not running, or the application read its device list before the source existed | `systemctl --user status 'oma-broadcast-mic@*'`; reopen the application's input settings or restart it |
| The source exists but is silent | No hardware source is available, or the capture side is not linked to one | `wpctl status` to check that a microphone exists and is the default; `pw-link -l` to see whether `oma-broadcast-mic.capture` has an input link. If you pinned `target.object`, check the name |
| Two "Omarchy Broadcast Mic" entries | Two instances are running (both engines, or a stale process) | Disable one: `systemctl --user disable --now oma-broadcast-mic@<engine>.service` |
| The voice sounds clipped or pumping | The reduction is too strong | DeepFilterNet: lower the attenuation limit. RNNoise: lower the VAD threshold or raise the grace period |
| CPU use while idle | The capture side is passive, so the microphone is not opened while nothing records from the source | Nothing to fix. If the microphone is open with no recorder, check which application is reading "Omarchy Broadcast Mic" or the hardware source |
