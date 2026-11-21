# module-hello-audio: an example omarchy-broadcast audio module

`module-hello-audio` is a minimal audio module written in Rust with `obc-module-sdk`. It changes the level of the microphone and can mix in a test tone. Copy it as the starting point for your own audio module.

It is built on its own, outside the omarchy-broadcast workspace, the way a third-party module is.

## What the example shows

The module provides one audio stage, `mic-enhance`. It changes the level of the microphone by a gain in decibels and can mix in a 1 kHz test tone. It runs on the CPU, on blocks of 480 samples of mono audio at 48 kHz.

An audio module is loaded by the audio worker, a process of its own, never by the video daemon. The daemon lists the module and never opens its library.

The settings appear with the microphone's settings (the group's `surface` is `mic`) as a "Hello voice" group, `x_example_hello_audio`. That is the vendor form `x_<vendor>_<name>` of a group of a module's own. The stage id is a well-known one of the registry.

| Setting id | Type | Meaning |
|---|---|---|
| `mic.x_example_hello_audio.enabled` | switch | Turns the stage on and off |
| `mic.x_example_hello_audio.gain` | -12 to 12 dB, step 1, default 0 | The gain; applies from the next block |
| `mic.x_example_hello_audio.tone` | switch | Mixes a 1 kHz sine into the voice; applies at the next load |
| `mic.x_example_hello_audio.tone_level` | 0 to 100 %, step 5, default 10 | The tone's peak level, in percent of full scale; applies at the next load |

Files:

- `src/lib.rs`: the module (the `Module`, `Instance` and `AudioInstance` implementations, the gain and the tone), with its unit tests.
- `hello-audio.toml`: the module's manifest. It holds the id, `kind = "audio-filter"`, the library to load and the stage the module provides (`slot = "shape"`, `device = "cpu"`, `frames = ["host"]`, `rates = [48000]`, `frame_samples = [480]`, `channels = 1`).

## Before you start

- A Rust toolchain (the crate declares `rust-version = "1.92"`).
- A checkout of the omarchy-broadcast repository. Run the commands below from its root.
- The daemon installed. To hear the stage, also turn on the Omarchy Broadcast Mic ([microphone.md](../../docs/setup/microphone.md)).

## Build and test

```sh
cargo build --release --manifest-path examples/module-hello-audio/Cargo.toml
cargo test --manifest-path examples/module-hello-audio/Cargo.toml
```

The library is `examples/module-hello-audio/target/release/libobc_module_hello_audio.so`.

## Load the module

The manifest directories are the same `modules.d` directories as for video modules. One of them is `~/.local/share/omarchy-broadcast/modules.d/`, for the current user.

1. Copy the manifest and the library there:

   ```sh
   dir=~/.local/share/omarchy-broadcast/modules.d
   mkdir -p "$dir"
   cp examples/module-hello-audio/hello-audio.toml "$dir/"
   cp examples/module-hello-audio/target/release/libobc_module_hello_audio.so "$dir/"
   ```

2. Set `library` in the copied `hello-audio.toml` to the library's new place. A relative path is relative to the manifest's directory.

   ```sh
   sed -i 's|^library = .*|library = "libobc_module_hello_audio.so"|' "$dir/hello-audio.toml"
   ```

3. Restart the daemon so it discovers the module:

   ```sh
   systemctl --user restart omarchy-broadcastd
   ```

## Check that it works

```sh
omarchy-broadcast modules
```

The list has the module `org.example.hello-audio` with its stage `mic-enhance`. Turn the stage on:

```sh
omarchy-broadcast settings set mic.x_example_hello_audio.enabled on
```

## Write your own audio module

Start from a copy of this directory:

1. Give the module its own reverse-DNS `id` in the manifest, and set `kind = "audio-filter"`. Declare each stage it provides:
   - The stage id starts with `mic-` (`mic-denoise`, `mic-dereverb`, `mic-enhance`).
   - The slot is `clean` (removing noise or room echo) or `shape` (working on the voice that is left).
   - It takes `frames = ["host"]` and lists the sample rates and block sizes it accepts.
2. Depend on `obc-module-sdk` (from a git checkout or a path), and build a `cdylib`.
3. Implement three traits, and export them with `obc_module_sdk::export_module!(YourModule, audio);`:
   - `Module` (configuration, probe, settings with `mic.<group>.<name>` ids);
   - `Instance` (`unload`, with `obc_module_sdk::no_video_stages!();` for the video entry points);
   - `AudioInstance` (`load_audio`, `process_audio`).

`process_audio` runs on the worker's processing thread with a deadline of one block. A module may create a GPU context of its own, but only in `load_audio` (released in `unload`), never in `init`, `probe` or `check`. A stage that fails is dropped, and the audio passes on without it.

To write a module in another language, implement the C ABI of `crates/obc-module-abi/include/omarchy_broadcast_module.h` directly (module ABI 1.2 or later, with the `audio_instance_load` and `audio_instance_process` entries set).

## Licence

MIT OR Apache-2.0, like omarchy-broadcast.
