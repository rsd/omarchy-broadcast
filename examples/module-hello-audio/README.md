# module-hello-audio: an example omarchy-broadcast audio module

A minimal audio module for omarchy-broadcast, written in Rust with
`obc-module-sdk`. It is built on its own, outside the omarchy-broadcast
workspace, the way a third-party module is.

It provides one audio stage, `mic-enhance`, which changes the level of the
microphone by a gain in decibels and can mix in a 1 kHz test tone. It runs
on the CPU, on blocks of 480 samples of mono audio at 48 kHz. Its settings
appear with the microphone's settings (the group's `surface` is `mic`) as a
"Hello voice" group:

| Setting id | Type | Meaning |
|---|---|---|
| `mic.hello.enabled` | switch | Turns the stage on and off |
| `mic.hello.gain` | -12 to 12 dB, step 1, default 0 | The gain; applies from the next block |
| `mic.hello.tone` | switch | Mixes a 1 kHz sine into the voice; applies at the next load |
| `mic.hello.tone_level` | 0 to 100 %, step 5, default 10 | The tone's peak level, in percent of full scale; applies at the next load |

An audio module is loaded by the audio worker, a process of its own, never
by the video daemon, which lists the module and never opens its library.

## Files

- `src/lib.rs`: the module (the `Module`, `Instance` and `AudioInstance`
  implementations, the gain and the tone), with its unit tests.
- `hello-audio.toml`: the module's manifest: its id, `kind =
  "audio-filter"`, the library to load and the stage it provides
  (`slot = "shape"`, `device = "cpu"`, `frames = ["host"]`,
  `rates = [48000]`, `frame_samples = [480]`, `channels = 1`).

## Build and test

From the root of the omarchy-broadcast repository:

```sh
cargo build --release --manifest-path examples/module-hello-audio/Cargo.toml
cargo test --manifest-path examples/module-hello-audio/Cargo.toml
```

The library is
`examples/module-hello-audio/target/release/libobc_module_hello_audio.so`.

## Install

The manifest directories are the same `modules.d` directories as for video
modules, among them `~/.local/share/omarchy-broadcast/modules.d/` for the
current user. Copy the manifest and the library there:

```sh
dir=~/.local/share/omarchy-broadcast/modules.d
mkdir -p "$dir"
cp examples/module-hello-audio/hello-audio.toml "$dir/"
cp examples/module-hello-audio/target/release/libobc_module_hello_audio.so "$dir/"
```

Then edit `library` in the copied `hello-audio.toml` to name the library's
new place, `library = "libobc_module_hello_audio.so"` (a relative path is
relative to the manifest's directory).

## Writing your own audio module

Start from a copy of this directory:

1. Give the module its own reverse-DNS `id` in the manifest, set
   `kind = "audio-filter"`, and declare each stage it provides. An audio
   stage id starts with `mic-` (`mic-denoise`, `mic-dereverb`,
   `mic-enhance`); its slot is `clean` (removing noise or room echo) or
   `shape` (working on the voice that is left); it takes `frames =
   ["host"]` and lists the sample rates and block sizes it accepts.
2. Depend on `obc-module-sdk` (from a git checkout or a path), and build a
   `cdylib`.
3. Implement `Module` (configuration, probe, settings with `mic.<group>.<name>`
   ids), `Instance` (`unload`, with `obc_module_sdk::no_video_stages!();`
   for the video entry points) and `AudioInstance`
   (`load_audio`, `process_audio`), and export them with
   `obc_module_sdk::export_module!(YourModule, audio);`.

`process_audio` runs on the worker's processing thread with a deadline of
one block. A module may create a GPU context of its own, but only in
`load_audio` (released in `unload`), never in `init`, `probe` or `check`. A
stage that fails is dropped and the audio passes on without it.

A module written in another language implements the C ABI of
`crates/obc-module-abi/include/omarchy_broadcast_module.h` directly (module
ABI 1.2 or later, with the `audio_instance_load` and
`audio_instance_process` entries set).

## Licence

MIT OR Apache-2.0, like omarchy-broadcast.
