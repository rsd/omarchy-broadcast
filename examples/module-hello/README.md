# module-hello: an example omarchy-broadcast module

A minimal video module for omarchy-broadcast, written in Rust with
`obc-module-sdk`. It is built on its own, outside the omarchy-broadcast
workspace, the way a third-party module is.

It provides one stage, `x-example-hello`, which gives the picture a warm
tint: on the CPU, in place on NV12 frames in host memory, it shifts the
colour of every pixel towards red and away from blue. Brightness and detail
are untouched. The stage id has the vendor form `x-<vendor>-<name>` that a
stage of a module's own takes (unprefixed ids are the well-known stages of
the registry). Its settings appear in the daemon's clients (the bar panel,
the CLI) as a "Hello module" group, `x_example_hello`, the vendor form of
a group:

| Setting id | Type | Meaning |
|---|---|---|
| `effect.x_example_hello.enabled` | switch | Turns the stage on and off |
| `effect.x_example_hello.strength` | 0 to 100, step 5, default 50 | How strong the tint is; applies from the next frame |

## Files

- `src/lib.rs`: the module (the `Module` and `Instance` implementations and
  the tint), with its unit tests.
- `hello.toml`: the module's manifest: its id, the library to load and the
  stage it provides (`slot = "post-composite"`, `device = "cpu"`,
  `frames = ["host"]`, `formats = ["nv12"]`).

## Build and test

From the root of the omarchy-broadcast repository:

```sh
cargo build --release --manifest-path examples/module-hello/Cargo.toml
cargo test --manifest-path examples/module-hello/Cargo.toml
```

The library is `examples/module-hello/target/release/libobc_module_hello.so`.

## Install

The daemon reads module manifests from `modules.d` directories, among them
`~/.local/share/omarchy-broadcast/modules.d/` for the current user. Copy
the manifest and the library there:

```sh
dir=~/.local/share/omarchy-broadcast/modules.d
mkdir -p "$dir"
cp examples/module-hello/hello.toml "$dir/"
cp examples/module-hello/target/release/libobc_module_hello.so "$dir/"
```

Then edit `library` in the copied `hello.toml` to name the library's new
place, `library = "libobc_module_hello.so"` (a relative path is relative
to the manifest's directory). Restart the daemon so it discovers the
module.

## Writing your own module

Start from a copy of this directory:

1. Give the module its own reverse-DNS `id` in the manifest, and declare
   each stage it provides, with the frame kinds and pixel formats it
   accepts.
2. Depend on `obc-module-sdk` (from a git checkout or a path), and build a
   `cdylib`.
3. Implement `Module` (configuration, probe, settings) and `Instance`
   (load, process, unload), and export them with
   `obc_module_sdk::export_module!(YourModule);`.

A module written in another language implements the C ABI of
`crates/obc-module-abi/include/omarchy_broadcast_module.h` directly; [`module-hello-c`](../module-hello-c/README.md) is this example in C.

## Licence

MIT OR Apache-2.0, like omarchy-broadcast.
