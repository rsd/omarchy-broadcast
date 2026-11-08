# os-release-shim

A small `LD_PRELOAD` library that shows one process a replacement `/etc/os-release`. It exists for one reason: the model loader of the NVIDIA Maxine Video Effects SDK reads `/etc/os-release` and refuses distributions it does not list, so on an unlisted distribution `NvVFX_Load` fails with `-1209` ([docs/setup/maxine.md](../../docs/setup/maxine.md), section 7). The daemon preloads this library through the optional systemd drop-in [`dist/systemd/oma-broadcastd.service.d/maxine.conf`](../../dist/systemd/oma-broadcastd.service.d/maxine.conf); the reasons for this mechanism are in [ADR-0012](../../docs/adr/0012-video-effects-and-maxine-backend.md).

It is not part of the Rust build, and nothing installs it: users who want the Maxine effects build and install it themselves.

## What it does

- It interposes `open`, `open64`, `openat`, `openat64`, `__open_2`, `__open64_2`, `__openat_2`, `__openat64_2`, `fopen` and `fopen64`. The SDK reads the file with `fopen`.
- A read-only open of exactly the absolute path `/etc/os-release` opens the file named by `$OMA_BROADCAST_OS_RELEASE` instead. Nothing else is redirected:
  - not a relative path, with or without a directory descriptor (`openat(fd_of_etc, "os-release")`), nor another spelling of the path (`/etc//os-release`, `/usr/lib/os-release`);
  - not an open for writing (`O_WRONLY`, `O_RDWR`, `O_CREAT`, `O_TRUNC`, `O_TMPFILE`, or an `fopen` mode other than `r...` without `+`): those go to the real path, so the replacement file is never written;
  - not `stat`, `access` or `readlink`: the SDK only reads the contents.
- An `O_PATH` open of the exact path counts as read-only and is redirected too, so `open("/etc/os-release", O_PATH | O_NOFOLLOW)` returns a descriptor for the replacement file, not for the symbolic link that `/etc/os-release` usually is.
- Every other call passes through unchanged to the next definition (`dlsym(RTLD_NEXT, ...)`), with the mode argument passed on when `O_CREAT` or `O_TMPFILE` is set. The fortified `__open_2` variants take no mode argument; where `O_CREAT` or `O_TMPFILE` is set they pass mode 0 on, which glibc's own fortified callers never do (they abort on those flags).
- `$OMA_BROADCAST_OS_RELEASE` must be an absolute path. If it is unset, empty, relative or longer than `PATH_MAX`, nothing is redirected; a set but unusable value prints a warning on stderr. If the replacement file cannot be opened, the real file is opened instead, so the process behaves as it would without the shim.
- When it is loaded, it removes `LD_PRELOAD` from the process environment, so no child process inherits it (the whole variable, including any other library listed in it). `$OMA_BROADCAST_OS_RELEASE` stays. The daemon starts no child process that needs the shim; GStreamer's plugin scanner, which it may start, does not load the SDK.
- `OMA_BROADCAST_OS_RELEASE_DEBUG=1` logs each redirect on stderr.

No namespace is involved: the process keeps the user's uid and the user's own user and mount namespaces, so it can read `/proc/<pid>/fd` of the user's other processes (consumer detection, [ADR-0011](../../docs/adr/0011-daemon-lifecycle-and-dbus-api.md)) and authenticates to the session bus as usual.

## Build, test and install

```sh
make -C tools/os-release-shim            # -> target/os-release-shim/os-release-shim.so
make -C tools/os-release-shim test       # builds and runs test.sh
make -C tools/os-release-shim OUT=/dir   # -> /dir/os-release-shim.so
```

The build needs only a C compiler and glibc. Install the library and a replacement `os-release` where the systemd drop-in expects them:

```sh
install -Dm644 target/os-release-shim/os-release-shim.so ~/.local/lib/oma-broadcast/os-release-shim.so
install -Dm644 tools/maxine-probe/fake-os-release ~/.local/share/oma-broadcast/fake-os-release
```

## Running a program with it

```sh
env LD_PRELOAD=~/.local/lib/oma-broadcast/os-release-shim.so \
    OMA_BROADCAST_OS_RELEASE=$HOME/.local/share/oma-broadcast/fake-os-release \
    <program> [args]
```

Put `LD_PRELOAD` on the program that loads the SDK, not on a wrapper in front of it: a wrapper such as `timeout`, `sh -c` or `cargo` would load the shim itself, remove `LD_PRELOAD`, and start the program without it. Write `timeout 60 env LD_PRELOAD=... program`, not `LD_PRELOAD=... timeout 60 program`. For `cargo test`, set it through a runner so that only the test binary is preloaded:

```sh
OMA_BROADCAST_OS_RELEASE=$HOME/.local/share/oma-broadcast/fake-os-release \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$HOME/.local/lib/oma-broadcast/os-release-shim.so" \
    cargo test -p obc-backend-maxine --test hardware -- --ignored --test-threads=1
```

If the library file is missing, the dynamic loader prints `ERROR: ld.so: object '...' from LD_PRELOAD cannot be preloaded (cannot open shared object file): ignored.` and the program runs without it.

## Files

| File | Purpose |
|---|---|
| `os-release-shim.c` | The library |
| `Makefile` | Builds the library into `target/os-release-shim/`, and runs the test |
| `shim-test.c` | Calls every interposed entry point and checks what it reads or creates |
| `test.sh` | Runs `shim-test` with and without the shim, and checks ordinary programs (`cat`, `grep`, `python3`), child processes and namespaces |

## Licence note

The shim only changes what one process of the user's reads, on the user's own machine. It does not modify `/etc/os-release`, the SDK or the models, and it does not touch the model encryption. It is per-user configuration, not a default: oma-broadcast does not install it, and it must not be shipped as a feature of a distributed package. See [docs/setup/maxine.md](../../docs/setup/maxine.md), section 11, for the licence terms this follows.
