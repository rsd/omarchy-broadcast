# module-hello-c: an example omarchy-broadcast module in C

`module-hello-c` is a minimal video module written in C against the module header alone, `omarchy_broadcast_module.h`. It needs no other library and no Rust. It is the C counterpart of [`module-hello`](../module-hello/README.md), and the model for a module in any language that can export C symbols.

## What the example shows

The module provides one stage, `x-example-hello-c`. On the CPU, on NV12 frames in host memory, it shifts the colour of every pixel towards red and away from blue. Brightness and detail are untouched.

- The stage id has the vendor form `x-<vendor>-<name>`, which a stage of a module's own takes. Unprefixed ids are the well-known stages of the registry.
- The settings group, `x_example_hello_c`, is the vendor form of a group.

| Setting id | Type | Meaning |
|---|---|---|
| `effect.x_example_hello_c.enabled` | switch | Turns the stage on and off |
| `effect.x_example_hello_c.strength` | 0 to 100, step 5, default 50 | How strong the tint is; applies from the next frame |

How it uses the ABI:

- `obc_module_abi_version` returns `OBC_MODULE_ABI_VERSION`. `obc_module_entry` refuses a host of another major version, or with an `obc_host` smaller than `OBC_HOST_SIZE_1_0`.
- Every structure with a `struct_size` is checked against its base size (`OBC_CHAIN_REQUEST_SIZE_1_0`, `OBC_FRAME_SIZE_1_0`, `OBC_LOAD_REPORT_SIZE_1_0`), never against `sizeof`. Only the fields of ABI 1.0 are written into the load report.
- A constant the module does not handle (a frame kind, a format) is answered with `OBC_ERR_UNSUPPORTED`. A parameter it does not know is ignored.
- Strings handed to the host stay valid until the next call on the same object. The reason of a failed `init` is kept per thread.
- The entries it does not need are null: `check`, the audio entries, the status values, the per-frame background, and the reserved `instance_park` and `instance_unpark`.

Files:

- `hello.c`: the module. It has the two exported symbols, the vtable and the tint.
- `hello-c.toml`: the module's manifest. It holds the id, the library to load, the lowest module ABI minor version the module needs (`abi_minor = 6`) and the stage the module provides (`slot = "post-composite"`, `device = "cpu"`, `frames = ["host"]`, `formats = ["nv12"]`).

## Before you start

- A C11 compiler (`cc`).
- A checkout of the omarchy-broadcast repository. Run the commands below from its root. With the installed header, pass its directory after `-I` instead.
- The daemon installed.

## Build

```sh
cc -std=c11 -Wall -Wextra -Werror -shared -fPIC -O2 \
   -I crates/obc-module-abi/include \
   -o libobc_module_hello_c.so examples/module-hello-c/hello.c
```

## Load the module

Copy the manifest and the library into a `modules.d` directory. For the current user, that is `~/.local/share/omarchy-broadcast/modules.d/`:

```sh
dir=~/.local/share/omarchy-broadcast/modules.d
mkdir -p "$dir"
cp examples/module-hello-c/hello-c.toml libobc_module_hello_c.so "$dir/"
```

The manifest's `library` is relative to its directory, so it finds the library there. Restart the daemon so it discovers the module:

```sh
systemctl --user restart omarchy-broadcastd
```

## Check that it works

```sh
omarchy-broadcast modules
```

The list has the module `org.example.hello-c` with its stage `x-example-hello-c`. Turn the stage on:

```sh
omarchy-broadcast settings set effect.x_example_hello_c.enabled on
```

The tint applies when the camera next starts.

## Licence

MIT OR Apache-2.0, like omarchy-broadcast.
