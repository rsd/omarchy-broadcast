# ADR-0004: v4l2loopback is the first output sink

- **Status:** Accepted

## Context
- Applications need to see the processed camera.
- v4l2loopback is widely packaged and supported by Chrome, Zoom, Discord and OBS.
- PipeWire camera support through the portal is still uneven across applications.

## Decision
- The first sink is `v4l2sink` into a v4l2loopback device labelled "Omarchy Broadcast", located by its label through sysfs.
- A PipeWire video node comes later, as a second sink behind the same output adapters.

## Consequences
- A v4l2loopback device must exist. Creating and configuring it (`modprobe` options, `exclusive_caps`) is a root-level setup step: see [../setup/loopback.md](../setup/loopback.md).
- The output format defaults to NV12, with I420 optional. YUYV is not offered because `cudaconvert` cannot produce it and the CPU alternative would add a GPU-to-CPU round trip; see [../benchmarks/passthrough.md](../benchmarks/passthrough.md).
- The one GPU-to-CPU copy happens at this sink.

## Alternatives considered
- PipeWire video node only: consumer support is not yet reliable enough to be the primary path.
