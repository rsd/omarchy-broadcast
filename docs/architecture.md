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
        subgraph GST["GStreamer pipelines (exist only while Streaming)"]
            subgraph ING["Ingest pipeline (per source, rebuilt on switch)"]
                SRC["v4l2src<br/>any UVC webcam"] --> UP["adapter + cudaupload<br/>→ CUDA"] --> CS["crop + scale<br/>→ canonical NV12"]
            end
            SLOT[("latest-frame slot")]
            subgraph OUTP["Output pipeline (fixed contract, own clock)"]
                FX["Process: obc effects<br/>blur / replace"] --> DOWN["convert → contract format<br/>download + relabel"] --> SINK["v4l2sink<br/>v4l2loopback"]
            end
            CS --> SLOT --> FX
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

`cudaconvert`, `cudaconvertscale` and `cudascale` accept no YUY2/UYVY input, hence the CPU convert step. The colour label (colorimetry) is preserved on the convert path. Decoded MJPEG is full-range BT.601 (`1:4:0:0`); the canonical caps fix the label of the contract, and `cudaconvertscale` (the crop and scale step of the ingest pipeline) converts to it in the same pass as the scale. For the default limited-range contract that is `2:4:5:1`, which matters because many consumer apps ignore the range flag and would otherwise crush blacks and clip whites. After the ingest pipeline, every source looks the same to the rest of the stream. Measured cost per path is in [benchmarks/ingest-paths.md](benchmarks/ingest-paths.md) and [benchmarks/io-split.md](benchmarks/io-split.md); the rationale is in [ADR-0008](adr/0008-cpu-ingest-adapters.md).

**Ingest, Process, Output.** The stream is split into three stages ([ADR-0009](adr/0009-ingest-process-output.md)):
- **Ingest** (per source) crops and scales every camera to the **canonical frame**: `video/x-raw(memory:CUDAMemory), format=NV12` at the processing size (the output size for now), with the colour label of the contract's range (`2:4:5:1` limited, `1:4:5:1` full). The default aspect policy is a centre crop to fill the frame; letterbox is an option.
- **Process** applies the effect chain to canonical frames. Effects do not know which source fed them or which sink they feed. An effect that needs another format gets converters inserted around it in CUDA memory.
- **Output** (per sink) holds a fixed contract of format (NV12 or I420), size, frame rate and colour range (default NV12 1920x1080@30, limited range) and runs on its own clock: it repeats the latest frame when the source is slower, drops frames when it is faster, and sends a black placeholder after a gap without input (default 500 ms). The clock's timestamps sit on an exact grid of the contract rate, while the instant each tick is emitted is locked to the camera's frames, a small margin after each expected arrival and never later than the timestamp ([ADR-0010](adr/0010-phase-locked-output-clock.md)), so a frame waits in the slot only that margin plus the camera's arrival jitter. The source can be switched while the output keeps running: the output holds the previous camera's last frame until the new camera's first frame (at most 3 s), so a switch shows no black frame ([benchmarks/io-split.md](benchmarks/io-split.md)). The output's `appsrc` queues at most 3 frames, so a slow consumer gets recent frames, not a backlog.
- **v4l2loopback constraints.** The device carries no colour description: it reports every stream as BT.601 (`2:4:5:4`) and, while a consumer holds it, refuses any other label. The output therefore relabels frames `2:4:5:4` after download (labels only, no pixel change), and a **full-range contract is refused** for a loopback sink, because consumers would read it as limited. At start the output compares the device's accepted caps with the contract: if the device is held by a consumer in another format, size or rate (its caps are fixed), the error says so (`LoopbackFormatHeld`); a colour label is compared by meaning. Any other mismatch is a generic refusal.

**UVC controls.** `exposure_auto_priority` (`exposure_dynamic_framerate`) is probed and used only if the camera exposes it. `--camera-fixed-framerate` pins it to 0 while a camera streams and restores it when that camera's ingest stops (on a source switch and on exit, including Ctrl-C and SIGTERM, but not SIGKILL). No per-camera quirks are needed.

```mermaid
flowchart LR
    subgraph Ingest["Ingest pipeline — per source, rebuilt on source switch"]
        S1["v4l2src (raw mode)<br/>NV12, or YUYV/UYVY<br/>(YUYV/UYVY: videoconvert → NV12 on the CPU)"]
        S2["v4l2src (MJPEG mode)<br/>jpegdec on the CPU"]
        UP["cudaupload<br/>→ CUDAMemory"]
        CROP["videocrop<br/>(crop region only, no copy;<br/>skipped for letterbox)"]
        SCALE["cudaconvertscale<br/>→ canonical frame:<br/>CUDAMemory NV12, processing size,<br/>contract colour label"]
        AS["appsink<br/>(newest frame only)"]
        S1 --> UP
        S2 --> UP
        UP --> CROP --> SCALE --> AS
    end

    SLOT[("latest-frame slot<br/>buffer reference + sequence")]

    subgraph Output["Output pipeline — fixed contract, lives for the whole stream"]
        CLK["output clock<br/>one tick per contract period,<br/>phase locked to the camera:<br/>fresh / repeat / placeholder;<br/>holds the last frame during a switch"]
        SRC["appsrc<br/>canonical caps, at most 3 queued<br/>(oldest dropped)"]
        subgraph Process["Process — effect chain (empty in this step)"]
            E1["obcblur / obcreplace<br/>(converters around effects<br/>that need another format)"]
        end
        CONV["cudaconvert<br/>(only if the contract format is not NV12)"]
        DL["cudadownload<br/>(the only GPU→CPU copy)"]
        RL["capssetter<br/>relabel 2:4:5:4 (loopback)"]
        K1["v4l2sink → v4l2loopback<br/>contract caps, sync=false"]
        CLK --> SRC --> E1 --> CONV --> DL --> RL --> K1
    end

    AS --> SLOT --> CLK
    K2["pipewiresink (planned)"]
    DL -.-> K2
```

A source switch, as the output sees it ([ADR-0010](adr/0010-phase-locked-output-clock.md)):

```mermaid
sequenceDiagram
    participant S as Session
    participant O as Output clock
    participant A as Old ingest
    participant B as New ingest
    S->>O: hold for B (at most 3 s)
    S->>A: stop (camera closed, controls restored)
    S->>B: start
    loop every tick until B's first frame
        O->>O: repeat the last frame<br/>(no black after 500 ms)
    end
    B-->>O: first frame (fresh): hold ends
    O->>O: phase lock re-acquires on B<br/>within a few frames
    Note over O: no frame from B within 3 s:<br/>black placeholders, as after any input loss
```

These are the extension points, and each is a trait or element boundary:
- **New source:** another camera or a screen capture. It is a new source element, and adapters cover format gaps (for example the MJPEG decode adapter).
- **New effect:** a new element in the Process stage that accepts canonical CUDAMemory NV12 (or gets converters around it), backed by any `InferenceBackend`.
- **New backend:** TensorRT, CPU, or another vendor. It is a new `InferenceBackend` impl, and the effect elements are unchanged.
- **New sink:** PipeWire or recording. It is a new output with its own contract, fed after the Process stage.

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
    Streaming --> Streaming: camera / source change<br/>(rebuild the ingest pipeline only;<br/>output keeps its contract and clock<br/>and holds the last frame meanwhile)
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
