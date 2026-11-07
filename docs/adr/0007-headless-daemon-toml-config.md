# ADR-0007: Headless daemon with a TOML config file

- **Status:** Accepted

## Context
- The existing nvbroadcast only works while its GUI is open, so a status-bar plugin cannot drive it alone.
- Settings must survive reboots without any GUI.

## Decision
- `oma-broadcastd` is a systemd user service and is the product. Every frontend is an optional D-Bus client.
- Settings live in `~/.config/oma-broadcast/config.toml`.
- D-Bus changes are written back to the file.
- Hand edits are hot-reloaded through inotify; the daemon ignores its own writes by content hash.

## Consequences
- A config schema with versioning and validation is needed (in `obc-core`).
- Closing any client never affects the stream.
- Dotfile-friendly: the file can be versioned and edited in any editor.

## Alternatives considered
- A GUI-owned process (the existing model): rejected because the stream then depends on a window being open.
