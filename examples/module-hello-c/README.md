# module-hello-c: an example omarchy-broadcast module in C

A minimal video module for omarchy-broadcast, written in C against the
module header alone, `omarchy_broadcast_module.h`, with no other library
and no Rust. It is the C counterpart of
[`module-hello`](../module-hello/README.md), and the model for a module
written in any language that can export C symbols.

It provides one stage, `x-example-hello-c`, which gives the picture a warm
tint: on the CPU, on NV12 frames in host memory, it shifts the colour of
every pixel towards red and away from blue. Brightness and detail are
untouched. The stage id has the vendor form `x-<vendor>-<name>` that a
stage of a module's own takes (unprefixed ids are the well-known stages of
the registry), and its settings group, `x_example_hello_c`, the vendor form
of a group:

| Setting id | Type | Meaning |
|---|---|---|
| `effect.x_example_hello_c.enabled` | switch | Turns the stage on and off |
| `effect.x_example_hello_c.strength` | 0 to 100, step 5, default 50 | How strong the tint is; applies from the next frame |

## Files

- `hello.c`: the module: the two exported symbols, the vtable and the tint.
- `hello-c.toml`: the module's manifest: its id, the library to load, the
  lowest module ABI minor version it needs (`abi_minor = 6`) and the stage
  it provides (`slot = "post-composite"`, `device = "cpu"`,
  `frames = ["host"]`, `formats = ["nv12"]`).

## Build

From the root of the omarchy-broadcast repository (or with the installed
header's directory after `-I`):

```sh
cc -std=c11 -Wall -Wextra -Werror -shared -fPIC -O2 \
   -I crates/obc-module-abi/include \
   -o libobc_module_hello_c.so examples/module-hello-c/hello.c
```

## Install

Copy the manifest and the library into a `modules.d` directory, for the
current user `~/.local/share/omarchy-broadcast/modules.d/`:

```sh
dir=~/.local/share/omarchy-broadcast/modules.d
mkdir -p "$dir"
cp examples/module-hello-c/hello-c.toml libobc_module_hello_c.so "$dir/"
```

The manifest's `library` is relative to its directory, so it finds the
library there. Restart the daemon so it discovers the module.

## What the example shows

- `obc_module_abi_version` returns `OBC_MODULE_ABI_VERSION`, and
  `obc_module_entry` refuses a host of another major version or with an
  `obc_host` smaller than `OBC_HOST_SIZE_1_0`.
- Every structure with a `struct_size` is checked against its base size
  (`OBC_CHAIN_REQUEST_SIZE_1_0`, `OBC_FRAME_SIZE_1_0`,
  `OBC_LOAD_REPORT_SIZE_1_0`), never against `sizeof`, and only the fields
  of ABI 1.0 are written into the load report.
- A constant the module does not handle (a frame kind, a format) is
  answered with `OBC_ERR_UNSUPPORTED`; a parameter it does not know is
  ignored.
- Strings handed to the host stay valid until the next call on the same
  object; the reason of a failed `init` is kept per thread.
- The entries it does not need are null: `check`, the audio entries, the
  status values, the per-frame background, and the reserved
  `instance_park` and `instance_unpark`.

## Licence

MIT OR Apache-2.0, like omarchy-broadcast.
