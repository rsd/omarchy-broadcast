//! Hardware integration tests, parametrised by environment variables.
//!
//! All are `#[ignore]`: they need a real camera attached and free, a
//! v4l2loopback device nobody else is feeding, and an NVIDIA GPU with
//! GStreamer's `cuda*` elements. Nothing here assumes a camera model:
//!
//! * `OBC_TEST_CAMERA` (required): the camera, in the same three forms as
//!   `obc-bench --camera`: a card-name substring, a USB `vid:pid`
//!   (`hhhh:hhhh`), or a `/dev/videoN` path. Unset, every test prints
//!   `SKIPPED` and returns (passing vacuously; see `--nocapture`).
//! * `OBC_TEST_MODE` (optional): the requested mode, `WxH@FPS` or
//!   `FORMAT:WxH@FPS`; default `1920x1080@60`. It is resolved against the
//!   camera's capabilities like `--mode`, so a mode the camera lacks degrades.
//! * `OBC_TEST_SINK`: the loopback node the pipeline tests write to (it must
//!   be a v4l2loopback device; anything else fails the test). Unset, a
//!   v4l2loopback device labelled "Omarchy Broadcast" is used; with neither,
//!   the pipeline tests skip. A test never feeds "the first loopback found".
//!
//! Check `fuser -v /dev/video*` first. Example:
//! `OBC_TEST_CAMERA=1234:abcd OBC_TEST_SINK=/dev/video11 cargo test -p obc-pipeline --test hardware -- --ignored`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use gst::prelude::*;
use obc_core::{CameraSelector, DEFAULT_SINK_LABEL, ModeRequest, OutputFormat, SinkSelector};
use obc_pipeline::capability::{Capabilities, IngestPath, ModeChoice, choose_mode};
use obc_pipeline::discovery::resolve_node_path;
use obc_pipeline::instrument::Recorder;
use obc_pipeline::pipeline::{DECODE_OUTPUT_COLORIMETRY, names};
use obc_pipeline::{
    DeviceInfo, DiscoveryRoute, GpuStage, LinkCaps, MediaPipeline, PipelineConfig, PipelineError,
    all_cuda_between, list_cameras, scan_sysfs, select_camera, select_sink,
};

/// The camera can only be opened by one test at a time.
static CAMERA: Mutex<()> = Mutex::new(());

fn init() {
    gst::init().expect("gst::init");
}

/// `OBC_TEST_CAMERA`, or `None` (with a printed reason) when unset.
fn test_camera() -> Option<CameraSelector> {
    let Some(v) = std::env::var_os("OBC_TEST_CAMERA") else {
        eprintln!(
            "SKIPPED: set OBC_TEST_CAMERA to a card-name substring, a USB vid:pid (hhhh:hhhh) \
             or a /dev/videoN path to run the hardware tests"
        );
        return None;
    };
    let sel: CameraSelector = v
        .to_string_lossy()
        .parse()
        .unwrap_or_else(|e| panic!("OBC_TEST_CAMERA: {e}"));
    Some(match sel {
        CameraSelector::Path(p) => CameraSelector::Path(resolve_node_path(&p)),
        other => other,
    })
}

/// `OBC_TEST_MODE`, or the default request.
fn test_mode() -> ModeRequest {
    match std::env::var("OBC_TEST_MODE") {
        Ok(v) => v.parse().unwrap_or_else(|e| panic!("OBC_TEST_MODE: {e}")),
        Err(_) => ModeRequest::default(),
    }
}

/// The explicitly named test loopback, or `None` (with a printed reason)
/// when none is named: never "the first loopback found".
fn test_sink() -> Option<DeviceInfo> {
    let devs = scan_sysfs();
    if let Some(path) = std::env::var_os("OBC_TEST_SINK") {
        let sel = SinkSelector::Path(path.into());
        let d = select_sink(&devs, &sel).unwrap_or_else(|e| panic!("OBC_TEST_SINK: {e}"));
        return Some(d.clone());
    }
    match select_sink(&devs, &SinkSelector::Label(DEFAULT_SINK_LABEL.into())) {
        Ok(d) => Some(d.clone()),
        Err(e) => {
            eprintln!(
                "SKIPPED: no explicit test loopback ({e}); set OBC_TEST_SINK=/dev/videoN \
                 or create a v4l2loopback device labelled {DEFAULT_SINK_LABEL:?}"
            );
            None
        }
    }
}

/// The test camera and the mode chosen for the test request, via `route`.
fn select_with(
    sel: &CameraSelector,
    req: &ModeRequest,
    route: DiscoveryRoute,
) -> (DeviceInfo, ModeChoice) {
    let mut cams = list_cameras(route, Some(req));
    if let CameraSelector::Path(p) = sel {
        cams.include_node(p);
    }
    let got = select_camera(&cams.devices, Some(sel), req)
        .unwrap_or_else(|e| panic!("{route:?}: OBC_TEST_CAMERA: {e}"));
    (got.device.clone(), got.choice)
}

#[test]
#[ignore = "needs OBC_TEST_CAMERA attached"]
fn selects_test_camera_by_both_routes() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sel) = test_camera() else {
        return;
    };
    let req = test_mode();
    let mut picks = Vec::new();
    for route in [DiscoveryRoute::Monitor, DiscoveryRoute::Sysfs] {
        let (d, choice) = select_with(&sel, &req, route);
        assert!(d.is_colour_camera(), "{route:?}: {d}");
        assert!(d.capture && !d.loopback, "{d}");
        let caps = d.capabilities.as_ref().expect("enumerated");
        assert!(caps.modes.contains(&choice.mode), "{route:?}: {choice}");
        eprintln!("{route:?}: {d}: requested {req} -> {choice}");
        picks.push((d.path, choice));
    }
    assert_eq!(picks[0], picks[1], "monitor and sysfs routes disagree");
}

/// The bench's control guard puts the camera control back on drop, also
/// when the scope unwinds from a panic. A camera without the control skips.
#[test]
#[ignore = "needs OBC_TEST_CAMERA attached and not streaming"]
fn control_guard_restores_on_drop_and_unwind() {
    use obc_pipeline::controls::{CID_EXPOSURE_AUTO_PRIORITY, ControlGuard, get_ctrl, probe_ctrl};
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sel) = test_camera() else {
        return;
    };
    let (camera, _) = select_with(&sel, &test_mode(), DiscoveryRoute::Auto);
    let path = camera.path.clone();
    let original = match probe_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY) {
        Ok((info, v)) if !info.read_only() => v,
        Ok(_) => {
            eprintln!("SKIPPED: exposure_dynamic_framerate is read-only on {camera}");
            return;
        }
        Err(skip) => {
            eprintln!("SKIPPED: exposure_dynamic_framerate {skip}");
            return;
        }
    };
    let other = i32::from(original == 0);

    let guard = ControlGuard::set(&path, CID_EXPOSURE_AUTO_PRIORITY, original, other).unwrap();
    assert_eq!(get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(), other);
    drop(guard);
    assert_eq!(
        get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(),
        original
    );

    let p = path.clone();
    let res = std::panic::catch_unwind(move || {
        let _guard = ControlGuard::set(&p, CID_EXPOSURE_AUTO_PRIORITY, original, other).unwrap();
        panic!("simulated failure mid-run");
    });
    assert!(res.is_err());
    assert_eq!(
        get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(),
        original
    );

    let guard = ControlGuard::set(&path, CID_EXPOSURE_AUTO_PRIORITY, original, other).unwrap();
    assert_eq!(guard.original(), original);
    guard.restore().unwrap();
    assert_eq!(
        get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(),
        original
    );
}

/// A string field of a caps string (`format=(string)NV12`).
fn caps_field<'a>(caps: &'a str, field: &str) -> Option<&'a str> {
    caps.split(&format!(" {field}=(string)"))
        .nth(1)
        .map(|r| r.split([',', ' ']).next().unwrap_or(r))
}

fn caps_format(caps: &str) -> Option<&str> {
    caps_field(caps, "format")
}

/// Builds the pipeline, plays it until enough buffers reached the loopback
/// (or 15 s pass), checks the caps, and tears it down. Returns the links.
fn stream_a_few_frames(
    camera: &DeviceInfo,
    sink: &DeviceInfo,
    choice: &ModeChoice,
    stage: GpuStage,
    output: OutputFormat,
) -> Vec<LinkCaps> {
    let tag = format!("{} {} {stage} -> {output}", choice.mode, choice.ingest);
    let frames = ((choice.mode.fps() * 1.5).ceil() as u64).clamp(8, 45);
    let mut cfg = PipelineConfig::new(camera.path.clone(), sink.path.clone(), choice.mode);
    cfg.gpu_stage = stage;
    cfg.output = output;
    let p = MediaPipeline::build(&cfg).expect("build");
    let rec = Recorder::new(false);
    rec.attach_main(&p).expect("probes");
    p.play().expect("PLAYING");

    let bus = p.bus();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut error = None;
    while rec.totals().sink_buffers < frames && Instant::now() < deadline {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(50))
            && let gst::MessageView::Error(e) = msg.view()
        {
            error = Some(format!("{} ({:?})", e.error(), e.debug()));
            break;
        }
    }
    let (_, state, _) = p.pipeline().state(gst::ClockTime::ZERO);
    let links = p.links();
    let cuda = all_cuda_between(&links, names::UPLOAD, names::DOWNLOAD);
    let latency = rec.samples();
    p.shutdown().expect("teardown to NULL");

    assert!(error.is_none(), "{tag}: bus error: {error:?}");
    assert_eq!(state, gst::State::Playing, "{tag}");
    let t = latency.totals;
    assert!(
        t.sink_buffers >= frames,
        "{tag}: only {} of {frames} frames reached the sink",
        t.sink_buffers
    );
    assert_eq!(
        t.sink_unmatched, 0,
        "{tag}: sink buffers with no src record"
    );
    // CUDA memory on every link from cudaupload to cudadownload; the CPU
    // ingest adapters before cudaupload are system memory by design.
    assert_eq!(cuda, Some(true), "{tag}: links: {links:#?}");
    let first = &links[0];
    assert!(!first.cuda_memory, "{tag}: {first:?}");
    let last = links.last().unwrap();
    assert!(!last.cuda_memory, "{tag}: {last:?}");
    // The loopback receives the requested output format.
    assert_eq!(
        last.caps.as_deref().and_then(caps_format),
        Some(output.gst_name()),
        "{tag}: {last:?}"
    );
    // The colorimetry label of what was uploaded reaches the loopback
    // unchanged. Both labels must be present: two absent labels are not a
    // match.
    let label = |l: &LinkCaps| {
        l.caps
            .as_deref()
            .and_then(|c| caps_field(c, "colorimetry"))
            .map(str::to_string)
    };
    let into_upload = links
        .iter()
        .find(|l| l.to.starts_with(&format!("{}:", names::UPLOAD)))
        .expect("a link into cudaupload");
    let upload_label = label(into_upload);
    assert!(
        upload_label.is_some(),
        "{tag}: no colorimetry into cudaupload: {into_upload:?}"
    );
    if choice.ingest == IngestPath::CpuDecode {
        // jpegdec's full-range label goes in; cudaconvert rescales to the
        // fixed limited-range label.
        assert_eq!(
            upload_label.as_deref(),
            Some("1:4:0:0"),
            "{tag}: {into_upload:?}"
        );
        assert_eq!(
            label(last).as_deref(),
            Some(DECODE_OUTPUT_COLORIMETRY),
            "{tag}: {last:?}"
        );
    } else {
        assert_eq!(
            label(last),
            upload_label,
            "{tag}: {into_upload:?} vs {last:?}"
        );
    }
    // CPU convert: videoconvert keeps the camera's own label, so the camera
    // src caps and the cudaupload input agree.
    if choice.ingest == IngestPath::CpuConvert {
        let camera_label = label(first);
        assert!(
            camera_label.is_some(),
            "{tag}: no colorimetry on the camera caps: {first:?}"
        );
        assert_eq!(
            camera_label, upload_label,
            "{tag}: camera {first:?} vs cudaupload input {into_upload:?}"
        );
    }
    eprintln!(
        "{tag}: {} frames; camera caps {}; loopback caps {}",
        t.sink_buffers,
        first.caps.as_deref().unwrap_or("-"),
        last.caps.as_deref().unwrap_or("-")
    );
    links
}

fn pipeline_test(stage: GpuStage) {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sel) = test_camera() else {
        return;
    };
    let Some(sink) = test_sink() else {
        return;
    };
    let (camera, choice) = select_with(&sel, &test_mode(), DiscoveryRoute::Auto);
    stream_a_few_frames(&camera, &sink, &choice, stage, OutputFormat::Nv12);
}

#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn passthrough_reaches_playing_with_cuda_links() {
    pipeline_test(GpuStage::Passthrough);
}

#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn roundtrip_reaches_playing_with_cuda_links() {
    pipeline_test(GpuStage::Roundtrip);
}

/// For every ingest path the camera offers (GPU-direct, CPU convert, CPU
/// decode), the mode closest to the request within that path streams, and
/// `cudaconvert` delivers the requested loopback format (NV12 and I420).
#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn every_ingest_path_outputs_requested_format() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sel) = test_camera() else {
        return;
    };
    let Some(sink) = test_sink() else {
        return;
    };
    let req = test_mode();
    let (camera, _) = select_with(&sel, &req, DiscoveryRoute::Auto);
    let caps: &Capabilities = camera.capabilities.as_ref().expect("enumerated");
    let mut ran = Vec::new();
    for path in [
        IngestPath::GpuDirect,
        IngestPath::CpuConvert,
        IngestPath::CpuDecode,
    ] {
        let subset = caps.filtered(|m| IngestPath::of(m.format) == path);
        // Without the requested format: it may belong to another path.
        let any_format = ModeRequest {
            format: None,
            ..req
        };
        let Some(choice) = choose_mode(&subset, &any_format) else {
            eprintln!("{camera}: offers no {path} format; not exercised");
            continue;
        };
        for output in [OutputFormat::Nv12, OutputFormat::I420] {
            let links = stream_a_few_frames(&camera, &sink, &choice, GpuStage::Passthrough, output);
            // What reaches cudaupload is the path's upload format.
            let into_upload = links
                .iter()
                .find(|l| l.to.starts_with(&format!("{}:", names::UPLOAD)))
                .expect("a link into cudaupload");
            assert_eq!(
                into_upload.caps.as_deref().and_then(caps_format),
                Some(path.upload_format(choice.mode.format)),
                "{path}: {into_upload:?}"
            );
        }
        ran.push(path);
    }
    assert!(!ran.is_empty(), "{camera}: no ingest path exercised");
}

/// A consumer that keeps the loopback open after its producer stopped (OBS
/// does) pins the producer's format and rate. A stream at another rate is
/// refused by `play` with [`PipelineError::LoopbackFormatHeld`] naming both,
/// not by a `not-negotiated` streaming error, and streams once the consumer
/// lets go. Same build and play code as `obc-bench` (run mode included).
#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn held_loopback_format_is_reported_before_streaming() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sel) = test_camera() else {
        return;
    };
    let Some(sink) = test_sink() else {
        return;
    };
    let (camera, choice) = select_with(&sel, &test_mode(), DiscoveryRoute::Auto);
    let mode = choice.mode;
    let held_rate = if (mode.fps_num, mode.fps_den) == (15, 1) {
        10
    } else {
        15
    };
    let device = sink.path.to_string_lossy().into_owned();

    // An earlier stream: same size and format, another rate.
    let earlier = gst::parse::launch(&format!(
        "videotestsrc is-live=true ! video/x-raw,format=NV12,width={},height={},framerate={held_rate}/1 \
         ! v4l2sink device={device} sync=false",
        mode.width, mode.height
    ))
    .expect("earlier producer");
    earlier
        .set_state(gst::State::Playing)
        .expect("earlier PLAYING");
    // The loopback offers capture only once the producer streams
    // (exclusive_caps): retry the consumer until it starts and has caps.
    let deadline = Instant::now() + Duration::from_secs(10);
    let has_caps = |c: &MediaPipeline| {
        let src = c.element(names::CONSUMER_SRC).unwrap();
        src.static_pad("src").unwrap().current_caps().is_some()
    };
    let consumer = loop {
        let c =
            MediaPipeline::build_consumer(&sink.path, &mode, OutputFormat::Nv12).expect("consumer");
        if c.play().is_ok() {
            while !has_caps(&c) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if has_caps(&c) {
                break c;
            }
        }
        assert!(Instant::now() < deadline, "the consumer never started");
        drop(c);
        std::thread::sleep(Duration::from_millis(200));
    };
    // The producer leaves; the consumer (errored, not yet NULL) keeps the
    // device open, as OBS does.
    earlier.set_state(gst::State::Null).expect("earlier NULL");
    drop(earlier);

    let mut cfg = PipelineConfig::new(camera.path.clone(), sink.path.clone(), mode);
    cfg.output = OutputFormat::Nv12;
    let p = MediaPipeline::build(&cfg).expect("build");
    let err = p
        .play()
        .expect_err("a held loopback at another rate must be refused");
    p.shutdown().expect("teardown to NULL");
    match &err {
        PipelineError::LoopbackFormatHeld {
            accepted, wanted, ..
        } => {
            assert_eq!(
                accepted,
                &format!("NV12 {}x{}@{held_rate}", mode.width, mode.height)
            );
            assert!(
                wanted.starts_with(&format!("NV12 {}x{}@", mode.width, mode.height)),
                "{err}"
            );
        }
        other => panic!("expected LoopbackFormatHeld, got {other}"),
    }
    eprintln!("{mode} {}: {err}", choice.ingest);

    // Once the consumer closes the device, the same stream goes through.
    consumer.shutdown().expect("consumer NULL");
    stream_a_few_frames(
        &camera,
        &sink,
        &choice,
        GpuStage::Passthrough,
        OutputFormat::Nv12,
    );
}
