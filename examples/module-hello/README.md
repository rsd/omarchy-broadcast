# module-hello: an example omarchy-broadcast module

`module-hello` is a minimal video module written in Rust with `obc-module-sdk`. It gives the picture a warm tint. Build it, load it and see it listed in about five minutes. Copy it as the starting point for your own module.

It is built on its own, outside the omarchy-broadcast workspace, the way a third-party module is.

## What the example shows

The module provides one stage, `x-example-hello`. On the CPU, in place on NV12 frames in host memory, it shifts the colour of every pixel towards red and away from blue. Brightness and detail are untouched.

- The stage id has the vendor form `x-<vendor>-<name>`, which a stage of a module's own takes. Unprefixed ids are the well-known stages of the registry.
- The module's settings appear in the daemon's clients (the bar plugin, the CLI) as a "Hello module" group, `x_example_hello`, the vendor form of a group.

| Setting id | Type | Meaning |
|---|---|---|
| `effect.x_example_hello.enabled` | switch | Turns the stage on and off |
| `effect.x_example_hello.strength` | 0 to 100, step 5, default 50 | How strong the tint is; applies from the next frame |

Files:

- `src/lib.rs`: the module (the `Module` and `Instance` implementations and the tint), with its unit tests.
- `hello.toml`: the module's manifest. It holds the id, the library to load and the stage the module provides (`slot = "post-composite"`, `device = "cpu"`, `frames = ["host"]`, `formats = ["nv12"]`).

## Before you start

- A Rust toolchain (the crate declares `rust-version = "1.92"`).
- A checkout of the omarchy-broadcast repository. Run the commands below from its root.
- The daemon installed.

## Build and test

```sh
cargo build --release --manifest-path examples/module-hello/Cargo.toml
cargo test --manifest-path examples/module-hello/Cargo.toml
```

The library is `examples/module-hello/target/release/libobc_module_hello.so`.

## Load the module

The daemon reads module manifests from `modules.d` directories. One of them is `~/.local/share/omarchy-broadcast/modules.d/`, for the current user.

1. Copy the manifest and the library there:

   ```sh
   dir=~/.local/share/omarchy-broadcast/modules.d
   mkdir -p "$dir"
   cp examples/module-hello/hello.toml "$dir/"
   cp examples/module-hello/target/release/libobc_module_hello.so "$dir/"
   ```

2. Set `library` in the copied `hello.toml` to the library's new place. A relative path is relative to the manifest's directory.

   ```sh
   sed -i 's|^library = .*|library = "libobc_module_hello.so"|' "$dir/hello.toml"
   ```

3. Restart the daemon so it discovers the module:

   ```sh
   systemctl --user restart omarchy-broadcastd
   ```

## Check that it works

```sh
omarchy-broadcast modules
```

The list has the module `org.example.hello` with its stage `x-example-hello`. Turn the stage on:

```sh
omarchy-broadcast settings set effect.x_example_hello.enabled on
```

The tint applies when the camera next starts.

## Write your own module

Start from a copy of this directory:

1. Give the module its own reverse-DNS `id` in the manifest. Declare each stage it provides, with the frame kinds and pixel formats it accepts.
2. Depend on `obc-module-sdk` (from a git checkout or a path), and build a `cdylib`.
3. Implement `Module` (configuration, probe, settings) and `Instance` (load, process, unload). Export them with `obc_module_sdk::export_module!(YourModule);`.

To write a module in another language, implement the C ABI of `crates/obc-module-abi/include/omarchy_broadcast_module.h` directly. [`module-hello-c`](../module-hello-c/README.md) is this example in C.

## Licence

MIT OR Apache-2.0, like omarchy-broadcast.
