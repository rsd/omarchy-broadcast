# oma-broadcast documentation

| Doc | What it is |
|---|---|
| [architecture.md](architecture.md) | Mermaid diagrams: component overview, stage/caps model with capability-driven source selection, on-demand lifecycle, D-Bus toggle sequence |
| [setup/loopback.md](setup/loopback.md) | Creating the "Omarchy Broadcast" v4l2loopback virtual camera: `modprobe` options, reload, and why `exclusive_caps=1` |
| [setup/maxine.md](setup/maxine.md) | Step-by-step NVIDIA Maxine VFX SDK setup on Arch/Omarchy: NGC access and key, user-prefix install under `$MAXINE_VFX_ROOT`, os-release override, verification with the probe, troubleshooting, licence notes, uninstall |
| [research/maxine-linux.md](research/maxine-linux.md) | Technical notes on Maxine on Linux: distribution, licence, versions, Blackwell support, features, API shape, existing projects |
| [benchmarks/passthrough.md](benchmarks/passthrough.md) | Passthrough and roundtrip results: pipeline, latency method, output format, camera frame-rate caveat, teardown finding; raw results are the JSON files alongside |
| [benchmarks/ingest-paths.md](benchmarks/ingest-paths.md) | Camera and ingest-path results: GPU-direct, CPU convert and CPU decode cost on two cameras, with variance under load |
| [testing.md](testing.md) | Unit tests, hardware tests (environment variables) and the manual OBS/Chrome test |
| [benchmarks/dgpu-idle.md](benchmarks/dgpu-idle.md) | dGPU idle check after teardown: findings and the quiet-desktop procedure |
| `../scripts/dgpu-idle-check.sh` | Reads sysfs and `/proc` only (never NVML) to report whether the dGPU runtime-suspends and who holds it awake |
| `../tools/maxine-probe/` | Standalone C probe that checks a Maxine SDK installation; see its README |
| `obc-bench` (`crates/obc-pipeline`) | Development binary: camera, then CUDA memory, then v4l2loopback, with latency, fps and GPU stats as JSON; not shipped |

## Architecture decision records

| ADR | Decision |
|---|---|
| [0001](adr/0001-rust.md) | Rust for daemon, elements, clients |
| [0002](adr/0002-gstreamer-engine.md) | GStreamer (gstreamer-rs) as the media engine |
| [0003](adr/0003-maxine-first-behind-backend-seam.md) | Maxine first, behind an `InferenceBackend` seam |
| [0004](adr/0004-v4l2loopback-first.md) | v4l2loopback is the first sink |
| [0005](adr/0005-dbus-zbus-control-plane.md) | D-Bus via zbus as the control plane |
| [0006](adr/0006-on-demand-gpu-lifecycle.md) | On-demand lifecycle; idle means zero CUDA contexts |
| [0007](adr/0007-headless-daemon-toml-config.md) | Headless daemon with a TOML config file |
| [0008](adr/0008-cpu-ingest-adapters.md) | CPU ingest adapters for MJPEG and YUYV/UYVY |

Conventions: diagrams are mermaid. New decisions get the next ADR number; superseded ADRs keep their file and change their status line.
