# ADR-0007: Headless daemon with a TOML config file

- **Status:** Accepted. Amended by [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md): runtime changes go to a separate state file instead of being written back to `config.toml`, and the configuration is reloaded by `SIGHUP` or `ReloadConfig()` instead of by inotify. Amended further by [ADR-0026](0026-config-d-and-managed-config.md): the configuration is `config.toml` plus the files of `config.d/`, one of which the daemon manages

## Context
- The existing nvbroadcast only works while its GUI is open, so a status-bar plugin cannot drive it alone.
- Settings must survive reboots without any GUI.

## Decision
- `omarchy-broadcastd` is a systemd user service and is the product. Every frontend is an optional D-Bus client.
- Settings live in `~/.config/omarchy-broadcast/config.toml`.
- D-Bus changes are written back to the file.
- Hand edits are hot-reloaded through inotify; the daemon ignores its own writes by content hash.

## Consequences
- A config schema with versioning and validation is needed (in `obc-core`).
- Closing any client never affects the stream.
- Dotfile-friendly: the file can be versioned and edited in any editor.

## Alternatives considered
- A GUI-owned process (the existing model): rejected because the stream then depends on a window being open.
