# Limitations

Check this page when something does not work as you expect. The main limits: the virtual camera serves one application at a time, a camera that is not a standard webcam may not appear, and the NVIDIA effects need an RTX GPU and an SDK you obtain yourself. Each section says what happens, why, and what to do.

| Limitation | Affects | Short answer |
|---|---|---|
| [One application at a time](#one-application-streams-from-the-virtual-camera-at-a-time) | The virtual camera | Enable the PipeWire camera node for several applications |
| [Cameras that are not standard webcams](#cameras-that-are-not-standard-webcams) | Camera detection | The daemon may not see them |
| [The PipeWire node and mixed formats](#the-pipewire-node-and-mixed-formats) | The PipeWire camera node | Let one application use the virtual camera |
| [A daemon crash](#a-daemon-crash) | Applications using the camera | Use the virtual camera where a reconnect matters |
| [TensorRT after a Maxine effect](#tensorrt-of-the-onnx-module-after-a-maxine-effect) | The open-model module | It uses CUDA until the daemon restarts |
| [OpenVINO needs the device](#openvino-needs-the-device-to-be-there) | The open-model module | Present the Intel GPU, or join the `render` group for the NPU |
| [Eye contact and CUDA graphs](#eye-contact-runs-without-the-sdks-cuda-graphs) | Maxine eye contact | None needed |
| [Maxine performance figures](#maxine-performance-figures-are-not-published) | Benchmarks | Measure locally |
| [Status](#status-early-development) | The project | Expect change |

Limits of the design, for developers, are in [docs/architecture.md](docs/architecture.md#known-limitations--future-work).

## One application streams from the virtual camera at a time

**What happens.** A second application that tries to stream from the virtual camera gets "device busy" (`EBUSY`), even when it asks for the same format. It works once the first application releases the camera.

**Why.** From v4l2loopback 0.14, a loopback device lets one process stream from its capture side. No module option changes that.

**Workaround.**

- One application can still share the camera inside itself: Chrome and Chromium share one capture among their tabs.
- To run several applications at once, enable the PipeWire camera node by adding this to `~/.config/omarchy-broadcast/config.toml`, then reload the daemon:

  ```toml
  [pipewire]
  enabled = true
  ```

  ```sh
  systemctl --user reload omarchy-broadcastd
  ```

- Chrome and Chromium then need the `WebRtcPipeWireCamera` flag (`chrome://flags`). OBS Studio has a "Video Capture Device (PipeWire)" source.
- Applications without PipeWire camera support, such as Zoom and Discord, keep using the virtual camera.

**More.** [The PipeWire output](docs/setup/daemon.md#the-pipewire-output) and [One application at a time](docs/setup/loopback.md#one-application-at-a-time).

## Cameras that are not standard webcams

**What happens.** The daemon may not see the camera, so no effect or virtual camera is available for it. This concerns cameras that are not standard USB Video Class devices. The notable case is the MIPI cameras (Intel IPU6 and IPU7) in many recent laptops.

**Why.** These cameras usually do not appear as a normal V4L2 camera without extra software such as libcamera. The daemon reads cameras through V4L2.

**Workaround.** Use a standard USB webcam, or set up the software your camera needs so that it appears as a V4L2 camera. This is general Linux knowledge and has not been tested with this project, so a camera set up that way may not work.

**More.** [Install](INSTALL.md#before-you-start).

## The PipeWire node and mixed formats

**What happens.** Consumers of the PipeWire node that ask for different formats (one for YUY2, another that accepts only NV12) can stall it until every consumer has disconnected.

**Why.** The node has one port and one negotiated format. A second consumer that accepts only another format fails to link, and the first can stop receiving frames. Consumers that ask for the same format run together.

**Workaround.** The node is optional for this reason. Let one of the applications that need a different format use the virtual camera instead.

**More.** [The PipeWire output](docs/setup/daemon.md#the-pipewire-output).

## A daemon crash

**What happens.** Applications that hold the virtual camera keep it. Chromium-based applications get black frames until the restarted daemon writes again. The unit restarts the daemon after 250 ms. Consumers of the PipeWire node do not reconnect.

**Why.** The daemon leaves a black timeout image and a `timeout` on the virtual camera, so that applications keep the camera while the daemon is gone. They show the black image until the restarted daemon writes again. The PipeWire node lives in the daemon process, so a crash destroys the consumer's stream.

**Workaround.** Use the virtual camera in an application that must survive a crash.

**More.** [Restarts and crashes](docs/setup/daemon.md#restarts-and-crashes).

## TensorRT of the ONNX module after a Maxine effect

**What happens.** Once a Maxine effect has run, the open-model module refuses ONNX Runtime's TensorRT engine and uses CUDA until the daemon restarts.

**Why.** The Maxine SDKs load their own TensorRT into the daemon. After one has run, ONNX Runtime's TensorRT engine would call the wrong library.

**Workaround.** Restart the daemon to get the TensorRT engine back.

```sh
systemctl --user restart omarchy-broadcastd
```

**More.** [TensorRT's library path](docs/setup/onnx.md#tensorrts-library-path).

## OpenVINO needs the device to be there

**What happens.** The OpenVINO engines are not used when their device is missing.

**Why.** The Intel GPU engine needs an Intel GPU present in the running system. On a laptop with a GPU switch, that excludes discrete-only mode. The NPU engine needs access to `/dev/accel/accel0`, normally through the `render` group.

**Workaround.** Switch the laptop out of discrete-only mode, or give your user access to `/dev/accel/accel0`.

**More.** [Troubleshooting the ONNX module](docs/setup/onnx.md#8-troubleshooting).

## Eye contact runs without the SDK's CUDA graphs

**What happens.** Eye contact runs with the SDK's CUDA graphs off, although they are the SDK's default.

**Why.** With them on, a capture on one thread could make another thread's CUDA call fail and abort the daemon.

**Workaround.** None needed.

## Maxine performance figures are not published

**What happens.** This project publishes no performance figures for the NVIDIA Maxine effects.

**Why.** The NVIDIA Maxine SDK licence does not allow it.

**Workaround.** Reproduce the figures locally with `obc-bench` ([docs/testing.md](docs/testing.md)). Other measurements are in [docs/benchmarks/](docs/benchmarks/passthrough.md).

## Status: early development

omarchy-broadcast is in early development. Passthrough from camera to virtual camera sustains 1080p60 on the reference machine ([benchmark](docs/benchmarks/passthrough.md)).
