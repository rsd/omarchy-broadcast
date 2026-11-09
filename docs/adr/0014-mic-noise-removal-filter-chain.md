# ADR-0014: Microphone noise removal through a PipeWire filter-chain; Maxine Audio Effects deferred

- **Status:** Accepted (2026-11-09). Adds no setting, configuration key or D-Bus member, and no Rust code.

## Context
- The project offers camera effects ([ADR-0012](0012-video-effects-and-maxine-backend.md)); users also want a cleaner microphone.
- The engines available on Arch Linux:
  - DeepFilterNet 3 (`deepfilternet-ladspa`; MIT or Apache-2.0; full-band 48 kHz).
  - RNNoise through `noise-suppression-for-voice` (the plugin is GPL-3.0; it is used, not shipped).
  - The NVIDIA Maxine Audio Effects (AFX) SDK.
- PipeWire ships `libpipewire-module-filter-chain` and an example `source-rnnoise.conf`. DeepFilterNet ships a filter-chain example.

## Decision
- A virtual `Audio/Source`, "Omarchy Broadcast Mic" (`node.name` `omarchy-broadcast-mic`), built by a filter-chain that hosts a CPU LADSPA plugin.
- Two engines, one configuration file each (`dist/pipewire/omarchy-broadcast-mic-deepfilter.conf` and `-rnnoise.conf`), with the same `node.name`, so applications that remember their input by name keep it across an engine switch. Only one runs at a time.
- Each runs as a dedicated `pipewire -c omarchy-broadcast-mic-<engine>.conf` process, started by the template user unit `omarchy-broadcast-mic@<engine>.service` (`dist/systemd/`).
- The capture side is passive and follows the default source; it can be pinned with `target.object`. The source is mono at 48 kHz.
- Independent of `omarchy-broadcastd`. No Rust code.
- Packaging: the Arch package ships both configurations and the template unit, and lists each plugin package as an optional dependency. Nothing is enabled; the user enables an instance. `scripts/install-user.sh --mic` installs the same files per user.
- User documentation: [../setup/microphone.md](../setup/microphone.md).

### Default engine
- RNNoise is the documented default; DeepFilterNet remains a supported choice.
- Both engines passed a listening test and were found acceptable.
- The reason is cost, which matters on a laptop battery: RNNoise about 2.4 % of one core and 14 MB resident, DeepFilterNet about 21.5 % of one core and 155 MB (Intel Core Ultra 9 275HX).
- The trade-off: RNNoise's voice-activity gate mutes sound it does not classify as voice (ambience, music, keyboard between words) to silence, while DeepFilterNet is a neural model that removes noise from inside speech and lowers the noise floor instead. DeepFilterNet is the choice for higher quality, for noise under speech, or to keep non-speech sound such as an instrument, when the CPU is available.

### How it runs, and the rejected run modes
- **A `pipewire.conf.d` drop-in:** rejected. The plugin would run inside the PipeWire server, so a plugin crash or overrun would hit every stream, and enabling, disabling or changing the filter would need a PipeWire restart that drops every stream.
- **A `filter-chain.conf.d` fragment with PipeWire's stock `filter-chain.service`:** rejected. Every fragment in that directory loads together, so the two engines could not be installed side by side, and the unit is shared with whatever other filters the user runs.

A dedicated process confines a plugin fault to that process, and starting, stopping and reconfiguring it touches nothing else.

## Alternatives considered
- **Maxine AFX now:** deferred, see below.
- **A native node inside `omarchy-broadcastd`** (GStreamer `pipewiresrc` and `pipewiresink`, or `pipewire-rs` streams): new code and a real-time ring buffer for an effect that filter-chain already hosts. It would be needed only for a GPU engine.
- **EasyEffects embedded or required:** a separate, always-on application that owns its own source. Fine to use alongside this; not something to build on.
- **NoiseTorch:** a separate application that does the same RNNoise virtual source.
- **A LADSPA wrapper around AFX inside filter-chain:** would load CUDA into a PipeWire process and run GPU calls in the real-time thread.

## Maxine Audio Effects deferred
- NVIDIA describes the Linux AFX SDK as designed for server-side (datacenter or cloud) deployments and says other use is not officially supported.
- Its supported GPU list names datacenter and RTX PRO parts, no GeForce.
- The download is gated through NGC (NVIDIA AI Enterprise or the NVIDIA Developer Program).
- Using the discrete GPU for audio would wake it for every call, even with camera effects off ([ADR-0006](0006-on-demand-gpu-lifecycle.md)), for work a CPU core does.
- The SDK bundles its own CUDA and TensorRT runtimes. They would need a separate worker process to avoid clashing with the video SDK's ([../setup/maxine.md](../setup/maxine.md), section 8).
- Its licence terms forbid publishing benchmark results, which limits what this project could document.

Revisit only after a local trial shows that AFX loads on the target GPU and is audibly better than DeepFilterNet. No AFX performance or latency figures are given here.

## Consequences
- An optional runtime dependency on one plugin package per engine (`deepfilternet-ladspa` or `noise-suppression-for-voice`). Without the plugin the unit fails visibly: the filter module has no `nofail` flag.
- Each engine adds latency. The upstream figures are 10 ms frames for RNNoise and at least 20 ms of algorithmic latency for DeepFilterNet; the whole chain has not been measured.
- CPU, measured on an Intel Core Ultra 9 275HX with PipeWire 1.6.9 over 30 s of continuous recording: DeepFilterNet about 21.5 % of one core (about 155 MB resident once the model has loaded), RNNoise about 2.4 % (about 14 MB). With no recorder connected, both are effectively idle and the hardware microphone stays suspended. Both run under the unit's hardening (including `MemoryDenyWriteExecute=yes`) without errors.
- The Arch package installs the configurations to `/usr/share/omarchy-broadcast/pipewire/` and the unit to `/usr/lib/systemd/user/`, with `noise-suppression-for-voice` and `deepfilternet-ladspa` as optional dependencies. Those directories are not on PipeWire's configuration search path, so the packaged unit names the configuration by absolute path; the repository's unit looks it up by name. Nothing is enabled by the package.
- The source can be made the default input safely: filter-chain puts its capture and playback nodes in one `node.link-group` and WirePlumber 0.5 never links nodes of one group, so the capture side does not loop back onto its own output.

### Later integration with the daemon
- The daemon could start and stop the unit through systemd's D-Bus API, driven by a configuration key.
- A consumer-driven lifecycle like the camera's ([ADR-0006](0006-on-demand-gpu-lifecycle.md), [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md)) is not needed for CPU engines: the capture side is passive, so the filter does not open the microphone while nothing records from the source.
- If one is needed later, for example for a GPU engine, the consumer signal is the PipeWire link state of the `omarchy-broadcast-mic` node, filtering out level-meter monitor streams.

## What would change this decision
- A later finding that an engine is not good enough or costs too much CPU: reconsider the engines, not the run mode.
- A local AFX trial that loads on the target GPU and is audibly better than DeepFilterNet: reopen the deferral.
