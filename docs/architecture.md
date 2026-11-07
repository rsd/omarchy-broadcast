# oma-broadcast: architecture

The rationale behind each design decision is recorded in [adr/](adr/). The planned D-Bus namespace is `br.com.dias` (bus name `br.com.dias.OmaBroadcast`, interface `br.com.dias.OmaBroadcast1`); see [ADR-0005](adr/0005-dbus-zbus-control-plane.md).

The source is any UVC webcam. The pipeline queries the camera's advertised formats and picks a mode from its capabilities (section 2); nothing in the design is specific to one camera model. Where measured figures are quoted, they come from the reference test cameras (a Logitech MX Brio and a laptop-integrated FHD camera).

## 1. Component overview

```mermaid
flowchart LR
    subgraph Clients["Clients (optional, replaceable)"]
        CLI["oma-broadcast CLI"]
        BAR["Omarchy bar plugin (QML)"]
        GUI["optional GUI (Qt/QML)"]
        SH["scripts via busctl"]
    end

    subgraph Daemon["oma-broadcastd (systemd user service)"]
        IPC["D-Bus interface<br/>zbus · br.com.dias.OmaBroadcast1"]
        LC["Lifecycle state machine"]
        CFG["Config<br/>~/.config/oma-broadcast/config.toml<br/>inotify hot-reload + write-back"]
        WATCH["Consumer watcher<br/>(v4l2loopback device)"]
        PB["Pipeline builder<br/>(obc-pipeline)"]
        subgraph GST["GStreamer pipeline (exists only while Streaming)"]
            SRC["v4l2src<br/>any UVC webcam"] --> UP["obcupload<br/>→ CUDA"] --> FX["obc effects<br/>blur / replace"] --> DOWN["obcconvert<br/>→ NV12/YUYV"] --> SINK["v4l2sink<br/>v4l2loopback"]
        end
        BE["InferenceBackend trait"]
        MX["obc-backend-maxine<br/>(dlopen, user-installed SDK)"]
    end

    CLI & BAR & GUI & SH -- "methods / properties" --> IPC
    IPC -- "signals" --> BAR & GUI
    IPC --> LC
    IPC <--> CFG
    WATCH --> LC
    LC --> PB --> GST
    FX --> BE --> MX
    SINK -. "virtual camera" .-> APPS["Browser / Zoom / OBS"]
    APPS -. "open/close" .-> WATCH
```

## 2. Stage and caps model

Every stage declares its input and output caps: format, size, and memory type. GStreamer negotiates them, and the pipeline builder inserts adapters only where adjacent caps differ.

**Capability-driven mode selection.** The source is not hard-coded to a format. The builder enumerates the camera's modes (format, size, frame rate) from V4L2 (`VIDIOC_ENUM_FMT`, `VIDIOC_ENUM_FRAMESIZES`, `VIDIOC_ENUM_FRAMEINTERVALS`), with GStreamer device caps as the fallback. It then selects one mode for the requested `WxH@FPS` (or `FORMAT:WxH@FPS`; the benchmark default is `1920x1080@60`, any format). The rule is strictly size, then frame rate, then format:
- **Size:** the requested size if offered. Otherwise the largest size that fits within both requested dimensions, preferring the same aspect ratio (within 1 %). Only if nothing fits, the smallest size above the request.
- **Frame rate:** the exact rate, otherwise the highest rate not above the request (0.2 % tolerance, so 30 serves 30000/1001). Only if every rate is above the request, the lowest.
- **Format:** the requested format, then NV12 > YUY2 > other raw formats (GPU-direct before CPU-convert) > MJPEG.

For example, a camera offering only MJPEG 1080p30 and YUYV up to 960x540@15, asked for 1080p60, gets MJPEG 1080p30 (a degraded match) with CPU decode.

**Ingest adapters.** The builder inserts one of three adapters in front of the CUDA stages. The guarantee that every link carries `memory:CUDAMemory` starts at `cudaupload`.

| Path | Camera format | Chain |
|---|---|---|
| GPU-direct | NV12, and any raw format `cudaconvert` accepts | `v4l2src` → `cudaupload` → CUDA |
| CPU convert | YUYV, UYVY | `v4l2src` → `videoconvert` → NV12 → `cudaupload` |
| CPU decode | MJPEG | `v4l2src` → `jpegdec` → `cudaupload` |

`cudaconvert`, `cudaconvertscale` and `cudascale` accept no YUY2/UYVY input, hence the CPU convert step. The colour label (colorimetry) is preserved on the convert path; decoded MJPEG is full-range BT.601 (`1:4:0:0`), and `cudaconvert` rescales it to limited-range BT.601 (`2:4:5:1`) before the loopback, because many consumer apps ignore the range flag and would otherwise crush blacks and clip whites. After the adapter, every source looks the same to the rest of the pipeline. Effects only ever see `video/x-raw(memory:CUDAMemory), format=BGR`. They do not know which source fed them or which sink they feed. Measured cost per path is in [benchmarks/ingest-paths.md](benchmarks/ingest-paths.md); the rationale is in [ADR-0008](adr/0008-cpu-ingest-adapters.md).

**UVC controls.** `exposure_auto_priority` (`exposure_dynamic_framerate`) is probed and used only if the camera exposes it. `--camera-fixed-framerate` pins it to 0 for the run and restores it on exit, including Ctrl-C and SIGTERM. No per-camera quirks are needed.

```mermaid
flowchart LR
    subgraph Sources
        S1["v4l2src (raw-capable camera)<br/>video/x-raw, e.g. NV12 or YUYV<br/>system memory<br/>(YUYV/UYVY: videoconvert → NV12 first)"]
        S2["v4l2src (MJPEG camera or mode)<br/>image/jpeg"]
    end

    subgraph Adapters["Adapters (auto-inserted by caps)"]
        DEC["MJPEG decode adapter<br/>jpegdec (CPU) → cudaupload"]
        UP["obcupload<br/>→ video/x-raw(memory:CUDAMemory), NV12"]
        CONV1["obcconvert NV12 → BGR<br/>(GPU)"]
    end

    subgraph Effects["Effects — CUDAMemory BGR in/out, one CUDA stream"]
        E0["passthrough"]
        E1["obcblur<br/>(Maxine Background Blur)"]
        E2["obcreplace<br/>(Maxine AIGS matte + composite)"]
    end

    subgraph Out["Output adapters"]
        CONV2["obcconvert BGR → NV12/YUYV<br/>(GPU)"]
        DL["download to system memory<br/>(the only GPU→CPU copy)"]
    end

    subgraph Sinks
        K1["v4l2sink → v4l2loopback"]
        K2["pipewiresink (planned)"]
    end

    S1 --> UP --> CONV1
    S2 --> DEC --> CONV1
    CONV1 --> E0 --> E1 --> E2 --> CONV2 --> DL
    DL --> K1
    DL -.-> K2
```

These are the extension points, and each is a trait or element boundary:
- **New source:** another camera or a screen capture. It is a new source element, and adapters cover format gaps (for example the MJPEG decode adapter).
- **New effect:** a new element that accepts CUDAMemory BGR, backed by any `InferenceBackend`.
- **New backend:** TensorRT, CPU, or another vendor. It is a new `InferenceBackend` impl, and the effect elements are unchanged.
- **New sink:** PipeWire or recording. It is a new sink element after the output adapters.

## 3. On-demand lifecycle

The idle state holds **zero CUDA contexts**, so the dGPU can runtime-suspend.

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle: Idle<br/>no CUDA context · camera closed<br/>D-Bus + watcher only
    Starting: Starting<br/>open camera · create CUDA ctx<br/>load backend · build pipeline
    Streaming: Streaming<br/>frames flowing · effects live-toggleable
    Draining: Draining<br/>no consumers · grace timer (≈5 s)
    Failed: Failed<br/>error reported via D-Bus signal

    Idle --> Starting: consumer opens virtual camera<br/>or ForceOn
    Starting --> Streaming: pipeline PLAYING
    Starting --> Failed: camera/backend error
    Streaming --> Draining: last consumer closed
    Draining --> Streaming: consumer reopens
    Draining --> Idle: grace expired → teardown,<br/>release CUDA ctx, close camera
    Streaming --> Starting: camera / source change (rebuild)
    Failed --> Idle: reset / retry
```

## 4. D-Bus sequence: bar-plugin toggle

```mermaid
sequenceDiagram
    autonumber
    participant Bar as Omarchy bar plugin
    participant Bus as Session bus
    participant D as oma-broadcastd (zbus)
    participant C as config.toml
    participant P as GStreamer pipeline

    Bar->>Bus: subscribe PropertiesChanged / StateChanged
    Bar->>Bus: call OmaBroadcast1.SetEffect("blur")
    Bus->>D: SetEffect("blur")
    D->>C: write effect = "blur"
    alt pipeline Streaming
        D->>P: set obcblur passthrough=false (no rebuild)
    else Idle
        D-->>D: store only; applied on next Starting
    end
    D-->>Bus: return OK
    D-->>Bus: signal PropertiesChanged(Effect="blur")
    Bus-->>Bar: PropertiesChanged → update icon
    Note over D,C: inotify sees our own write, which is ignored by content hash
```

## Reference camera formats

Measured on the reference test camera (Logitech MX Brio, USB 3, `uvcvideo`): maximum frame rate per format for modes at least 1280 pixels wide.

| Format | 1280x720 | 1600x896 | 1920x1080 | 2560x1440 | 3840x2160 |
|---|---|---|---|---|---|
| NV12 | 60 | — | 60 | 30 | — |
| YUYV | 60 | 60 | 60 | 24 | 15 |
| MJPEG | 60 | 60 | 60 | 30 | 30 |

Raw 4K tops out at 15 fps; 4K at 30 fps is only available as MJPEG, which is what the decode adapter is for.

## Known limitations / future work

- **MJPEG is decoded on the CPU.** GStreamer's `nvjpegdec` (NVDEC) is not used: on the reference machine it rejects the 4:2:2 MJPEG that typical UVC webcams send (`unhandled chroma format 2`). The cost is CPU time that scales with resolution and frame rate (see [benchmarks/ingest-paths.md](benchmarks/ingest-paths.md)). GPU JPEG decode, for example through an element built on the nvJPEG library, is future work. See [ADR-0008](adr/0008-cpu-ingest-adapters.md).
- **YUYV/UYVY are converted on the CPU.** A custom CUDA kernel for these formats is deferred.
