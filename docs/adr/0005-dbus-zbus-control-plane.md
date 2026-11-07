# ADR-0005: D-Bus via zbus as the control plane

- **Status:** Accepted

## Context
- Several clients (CLI, bar plugin, optional GUI, scripts) must control the headless daemon and see state changes without polling.

## Decision
- The daemon exposes `br.com.dias.OmaBroadcast1` on the session bus using `zbus`, which is pure Rust and async, with `#[interface]` structs.
- It offers methods, properties and signals.
- The interface definition lives in a shared crate (`obc-ipc`) that both the daemon and the clients use.
- The trailing major version follows the D-Bus convention. Breaking changes add `Broadcast2` alongside it.
- Namespace: bus name `br.com.dias.OmaBroadcast`, interface `br.com.dias.OmaBroadcast1`, object path `/br/com/dias/OmaBroadcast`. A reverse-DNS name under a domain the project controls avoids claiming to be an official component of any distribution.

## Consequences
- Scripts can use it via `busctl`/`gdbus`, and systemd D-Bus activation becomes possible.
- The bar plugin subscribes to signals and does not poll.

## Alternatives considered
- A custom JSON-over-Unix-socket protocol: reinvents discovery, introspection and signals.
- An `org.omarchy.*` namespace: rejected because it implies an official Omarchy project.
