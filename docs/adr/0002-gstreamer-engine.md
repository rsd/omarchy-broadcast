# ADR-0002: GStreamer as the media engine

- **Status:** Accepted

## Context
- Stages must compose like shell pipes and extend to more cameras, GPUs, CPU backends, sinks and audio.
- Building a custom engine means rebuilding caps negotiation, buffer pools, threading and device quirks.

## Decision
- Use GStreamer (1.28) through the official `gstreamer-rs` bindings.
- Our stages are GStreamer elements written in Rust (gst-plugins-rs style).
- The daemon builds and controls the pipelines.
- Buffers stay in `memory:CUDAMemory` between upload and the sink.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| Own pipeline core | Reinvents the framework |
| FFmpeg / libavfilter | File and transcode oriented, static graphs |
| MediaPipe | Bazel build, tied to Google's models |
| NVIDIA DeepStream | Heavy and server/Jetson-oriented, though it is itself GStreamer-based |

PipeWire is a complement (sink, audio), not an alternative.

## Consequences
- **Gains:** v4l2src/v4l2sink, PipeWire elements, nvcodec/CUDA memory, audio and the VA-API paths come for free.
- **Low-latency tuning is required:**
  - leaky small queues;
  - `sync=false` on the sink;
  - dmabuf/mmap capture.
- **Dynamic relinking is tricky.** Effects stay linked and are toggled with a passthrough property.
- **Debugging needs GStreamer knowledge:** `GST_DEBUG`, dot graphs, tracers.
- **Bottleneck check:** the [passthrough benchmark](../benchmarks/passthrough.md) shows sustained 1080p60 with at most 8 ms of added latency, so GStreamer is not the bottleneck. Should that change, the elements are thin wrappers over plain Rust and can move.
