//! Hardware integration tests, parametrised by environment variables.
//!
//! All are `#[ignore]`, with the reason naming what they need, and run with
//! `--ignored`. The camera tests need a real camera attached and free, a
//! v4l2loopback device nobody else is feeding, and an NVIDIA GPU with
//! GStreamer's `cuda*` elements; the GPU-only tests (`gpu_*`, selected with
//! the `gpu_` filter) need only the GPU. A test that runs without its
//! prerequisite fails, with a message naming what is missing and how to
//! provide it: no test passes without running. Nothing here assumes a camera
//! model:
//!
//! * `OBC_TEST_CAMERA` (camera tests): the camera, in the same three forms as
//!   `obc-bench --camera`: a card-name substring, a USB `vid:pid`
//!   (`hhhh:hhhh`), or a `/dev/videoN` path. The camera tests fail when it is
//!   unset. The control tests need a camera whose
//!   `exposure_dynamic_framerate` control is writable.
//! * `OBC_TEST_CAMERA_2` (optional): a second camera for the switching test,
//!   same forms. Unset, that test switches between two openings of
//!   `OBC_TEST_CAMERA`.
//! * `OBC_TEST_MODE` (optional): the requested camera mode, `WxH@FPS` or
//!   `FORMAT:WxH@FPS`; default `1920x1080@60`. It is resolved against the
//!   camera's capabilities like `--mode`, so a mode the camera lacks degrades.
//!   The output contract is the default (NV12 1920x1080@30, limited range)
//!   whatever the camera mode.
//! * `OBC_TEST_SINK`: the loopback node the pipeline tests write to (it must
//!   be a v4l2loopback device; anything else fails the test). Unset, a
//!   v4l2loopback device labelled "Omarchy Broadcast" is used; with neither,
//!   the pipeline tests fail. A test never feeds "the first loopback found".
//!   `output_restarts_while_a_consumer_holds_the_loopback` needs no camera
//!   and requires `OBC_TEST_SINK`, never the labelled default.
//!
//! * The effect tests: `gpu_an_output_switches_effects_while_streaming`
//!   uses the fake backend of `gst-obc/tests/support/fake_backend.rs` (GPU
//!   only); `maxine_*` needs `OBC_TEST_CAMERA` and the NVIDIA Maxine SDK
//!   (`MAXINE_VFX_ROOT`, `source tools/maxine-probe/env.sh`) run under the
//!   os-release override (`docs/setup/maxine.md`, section 7).
//!
//! Check `fuser -v /dev/video*` first. Examples:
//! `cargo test -p obc-pipeline --test hardware -- --ignored gpu_ --test-threads=1` (GPU only);
//! `OBC_TEST_CAMERA=1234:abcd OBC_TEST_SINK=/dev/video11 cargo test -p obc-pipeline --test hardware -- --ignored --test-threads=1` (all).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gst_video::prelude::*;
use obc_core::{
    AspectPolicy, CameraSelector, ColourRange, DEFAULT_SINK_LABEL, ModeRequest, OutputContract,
    OutputFormat, SinkSelector,
};
use obc_pipeline::capability::{Capabilities, IngestPath, ModeChoice, choose_mode};
use obc_pipeline::discovery::resolve_node_path;
use obc_pipeline::ingest::{CUDA_CONTEXT_TYPE, canonical_tail, cuda_context_object};
use obc_pipeline::output::SINK_COLORIMETRY;
use obc_pipeline::pipeline::names;
use obc_pipeline::{
    Canonical, DeviceInfo, DiscoveryRoute, EffectBackend, EffectKind, EffectSettings, EffectStatus,
    EmitInfo, GpuStage, Ingest, IngestConfig, LinkCaps, MediaPipeline, Output, OutputConfig,
    OutputCounts, OutputSink, PipelineError, Session, TickKind, all_cuda_between, crop_for,
    list_cameras, scan_sysfs, select_camera, select_sink,
};

/// The fake effect backend shared with gst-obc's GPU tests.
#[path = "../../gst-obc/tests/support/fake_backend.rs"]
mod fake_backend;

/// The camera can only be opened by one test at a time.
static CAMERA: Mutex<()> = Mutex::new(());

fn init() {
    gst::init().expect("gst::init");
}

fn camera_from_env(var: &str) -> Option<CameraSelector> {
    let v = std::env::var_os(var)?;
    let sel: CameraSelector = v
        .to_string_lossy()
        .parse()
        .unwrap_or_else(|e| panic!("{var}: {e}"));
    Some(match sel {
        CameraSelector::Path(p) => CameraSelector::Path(resolve_node_path(&p)),
        other => other,
    })
}

/// `OBC_TEST_CAMERA`; the test fails when it is unset.
fn test_camera() -> CameraSelector {
    camera_from_env("OBC_TEST_CAMERA").unwrap_or_else(|| {
        panic!(
            "this test needs a camera: set OBC_TEST_CAMERA to a card-name substring, a USB \
             vid:pid (hhhh:hhhh) or a /dev/videoN path"
        )
    })
}

/// `OBC_TEST_MODE`, or the default request.
fn test_mode() -> ModeRequest {
    match std::env::var("OBC_TEST_MODE") {
        Ok(v) => v.parse().unwrap_or_else(|e| panic!("OBC_TEST_MODE: {e}")),
        Err(_) => ModeRequest::default(),
    }
}

/// The explicitly named test loopback: `OBC_TEST_SINK`, or else the
/// v4l2loopback device labelled [`DEFAULT_SINK_LABEL`], never "the first
/// loopback found". The test fails when there is neither.
fn test_sink() -> DeviceInfo {
    let devs = scan_sysfs();
    if let Some(path) = std::env::var_os("OBC_TEST_SINK") {
        let sel = SinkSelector::Path(path.into());
        let d = select_sink(&devs, &sel).unwrap_or_else(|e| panic!("OBC_TEST_SINK: {e}"));
        return d.clone();
    }
    match select_sink(&devs, &SinkSelector::Label(DEFAULT_SINK_LABEL.into())) {
        Ok(d) => d.clone(),
        Err(e) => panic!(
            "this test needs a free v4l2loopback device ({e}): set OBC_TEST_SINK=/dev/videoN \
             or create a v4l2loopback device labelled {DEFAULT_SINK_LABEL:?}"
        ),
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
        .unwrap_or_else(|e| panic!("{route:?}: camera {sel}: {e}"));
    (got.device.clone(), got.choice)
}

/// Fails the test unless every GStreamer element in `factories` is
/// installed, naming the missing ones and where they come from.
fn require_elements(factories: &[&str]) {
    let missing: Vec<&str> = factories
        .iter()
        .copied()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "GStreamer element(s) not installed: {}. The cuda* elements come from the nvcodec \
         plugin (gst-plugins-bad) and register only where an NVIDIA GPU and its driver are \
         present; the others from GStreamer core, gst-plugins-base and gst-plugins-good. \
         Check with \
         `gst-inspect-1.0 <element>`.",
        missing.join(", ")
    );
}

/// The value of the camera's `exposure_dynamic_framerate` control; the test
/// fails when the camera does not have it writable (both reference cameras
/// do).
fn writable_exposure_dynamic_framerate(camera: &DeviceInfo) -> i32 {
    use obc_pipeline::controls::{CID_EXPOSURE_AUTO_PRIORITY, probe_ctrl};
    match probe_ctrl(&camera.path, CID_EXPOSURE_AUTO_PRIORITY) {
        Ok((info, v)) if !info.read_only() => v,
        Ok(_) => panic!(
            "this test needs a camera whose exposure_dynamic_framerate control is writable; it \
             is read-only on {camera}: choose another OBC_TEST_CAMERA"
        ),
        Err(skip) => panic!(
            "this test needs a camera with a writable exposure_dynamic_framerate control \
             ({skip} on {camera}): choose another OBC_TEST_CAMERA"
        ),
    }
}

#[test]
#[ignore = "needs OBC_TEST_CAMERA attached"]
fn selects_test_camera_by_both_routes() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let sel = test_camera();
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

/// The control guard puts the camera control back on drop, also when the
/// scope unwinds from a panic. The camera must have the control writable.
#[test]
#[ignore = "needs OBC_TEST_CAMERA attached and not streaming"]
fn control_guard_restores_on_drop_and_unwind() {
    use obc_pipeline::controls::{CID_EXPOSURE_AUTO_PRIORITY, ControlGuard, get_ctrl};
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let sel = test_camera();
    let (camera, _) = select_with(&sel, &test_mode(), DiscoveryRoute::Auto);
    let path = camera.path.clone();
    let original = writable_exposure_dynamic_framerate(&camera);
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

/// Puts a camera control back to the value it had when the test found it,
/// when dropped: also when an assertion fails partway through the test.
struct RestoreControl {
    path: std::path::PathBuf,
    id: u32,
    value: i32,
}

impl Drop for RestoreControl {
    fn drop(&mut self) {
        if let Err(e) = obc_pipeline::controls::set_ctrl(&self.path, self.id, self.value) {
            eprintln!(
                "could not restore control {:#x} of {} to {}: {e}",
                self.id,
                self.path.display(),
                self.value
            );
        }
    }
}

/// An ingest that pins `exposure_dynamic_framerate` off restores it when it
/// is stopped (explicitly, by a switch, or by dropping the session), before
/// the next ingest of the same camera starts. The camera must have the
/// control writable. The camera's original value is put back at the end,
/// also when the test fails.
#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn ingest_restores_the_camera_control_on_stop() {
    use obc_pipeline::controls::{CID_EXPOSURE_AUTO_PRIORITY, get_ctrl, set_ctrl};
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let sel = test_camera();
    let (camera, choice) = select_with(&sel, &test_mode(), DiscoveryRoute::Auto);
    let path = camera.path.clone();
    let original = writable_exposure_dynamic_framerate(&camera);
    // Declared before the session, so dropped after it: the session's own
    // restore runs first, then the original value is put back.
    let _restore = RestoreControl {
        path: path.clone(),
        id: CID_EXPOSURE_AUTO_PRIORITY,
        value: original,
    };
    // Start from "on", so the ingest has something to pin.
    set_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY, 1).unwrap();
    let mut session = Session::new(
        Output::build(OutputConfig::new(
            OutputContract::default(),
            OutputSink::Fake,
        ))
        .unwrap(),
    )
    .expect("output");
    let mut cfg = IngestConfig::new(path.clone(), choice.mode);
    cfg.fixed_framerate = true;
    session.switch_to_camera(&cfg).expect("ingest");
    let report = session.ingest().unwrap().controls().clone();
    assert!(report.pinned, "{report:?}");
    assert_eq!((report.before, report.during), (Some(1), Some(0)));
    assert_eq!(get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(), 0);
    // A switch to a new ingest of the same camera: the old ingest restores
    // the control before the new one reads it, so the new one finds it on.
    session.switch_to_camera(&cfg).expect("switch");
    let report = session.ingest().unwrap().controls().clone();
    assert!(report.pinned, "after the switch: {report:?}");
    assert_eq!(
        (report.before, report.during),
        (Some(1), Some(0)),
        "after the switch"
    );
    assert_eq!(get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(), 0);
    session.stop_ingest().expect("stop ingest");
    assert_eq!(get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(), 1);
    // Dropping the session (no explicit stop) also restores.
    session.switch_to_camera(&cfg).expect("ingest");
    assert_eq!(get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(), 0);
    drop(session);
    assert_eq!(get_ctrl(&path, CID_EXPOSURE_AUTO_PRIORITY).unwrap(), 1);
}

/// What a pipeline's bus said.
#[derive(Debug, Default)]
struct BusLog {
    errors: Vec<String>,
    eos: bool,
    /// `have-context` messages for a CUDA context: an element created one.
    cuda_contexts_created: usize,
}

impl BusLog {
    fn drain(&mut self, p: &MediaPipeline) {
        let bus = p.bus();
        while let Some(msg) = bus.pop() {
            match msg.view() {
                gst::MessageView::Error(e) => self.errors.push(format!(
                    "{}: {} ({:?})",
                    msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    e.error(),
                    e.debug()
                )),
                gst::MessageView::Eos(_) => self.eos = true,
                gst::MessageView::HaveContext(c)
                    if c.context().context_type() == CUDA_CONTEXT_TYPE =>
                {
                    self.cuda_contexts_created += 1
                }
                _ => {}
            }
        }
    }
}

/// Counts buffers through `element`'s `pad`.
fn count_buffers(p: &MediaPipeline, element: &str, pad: &str) -> Arc<AtomicU64> {
    let n = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&n);
    p.element(element)
        .and_then(|e| e.static_pad(pad))
        .unwrap_or_else(|| panic!("{element}:{pad}"))
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    n
}

/// `(pts, arrival)` of every buffer reaching `element`'s sink pad.
type Arrivals = Arc<Mutex<Vec<(u64, Instant)>>>;

fn record_arrivals(p: &MediaPipeline, element: &str) -> Arrivals {
    let log: Arrivals = Arc::default();
    let l = Arc::clone(&log);
    p.element(element)
        .and_then(|e| e.static_pad("sink"))
        .expect("sink pad")
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let t = Instant::now();
            if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                l.lock().unwrap().push((pts.nseconds(), t));
            }
            gst::PadProbeReturn::Ok
        });
    log
}

/// Buffers through `element`'s src pad whose memories are all CUDA memory,
/// and those that are not.
fn memory_kinds(p: &MediaPipeline, element: &str) -> Arc<(AtomicU64, AtomicU64)> {
    let n: Arc<(AtomicU64, AtomicU64)> = Arc::default();
    let c = Arc::clone(&n);
    p.element(element)
        .and_then(|e| e.static_pad("src"))
        .expect("src pad")
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                let cuda = b.n_memory() > 0
                    && (0..b.n_memory()).all(|i| b.peek_memory(i).is_type("gst.cuda.memory"));
                if cuda { &c.0 } else { &c.1 }.fetch_add(1, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    n
}

/// The CUDA context objects the session's pipelines use: output pipeline,
/// output `cudadownload`, ingest pipeline, ingest `cudaupload` and
/// `cudaconvertscale`.
fn cuda_contexts(session: &Session) -> Vec<(&'static str, Option<gst::Object>)> {
    let of = |e: Option<gst::Element>| {
        e.and_then(|e| e.context(CUDA_CONTEXT_TYPE))
            .as_ref()
            .and_then(cuda_context_object)
    };
    let out = session.output().pipeline();
    let ingest = session.ingest().expect("an ingest").pipeline();
    vec![
        (
            "output",
            session
                .output()
                .cuda_context()
                .and_then(cuda_context_object),
        ),
        ("output download", of(out.element(names::DOWNLOAD))),
        (
            "ingest",
            ingest
                .pipeline()
                .context(CUDA_CONTEXT_TYPE)
                .as_ref()
                .and_then(cuda_context_object),
        ),
        ("ingest upload", of(ingest.element(names::UPLOAD))),
        ("ingest scale", of(ingest.element(names::SCALE))),
    ]
}

fn assert_one_cuda_context(contexts: &[(&str, Option<gst::Object>)], tag: &str) {
    let first = contexts[0].1.as_ref().expect("output CUDA context");
    for (what, ctx) in contexts {
        assert_eq!(ctx.as_ref(), Some(first), "{tag}: {what}: {contexts:?}");
    }
}

fn link_caps(l: &LinkCaps) -> gst::Caps {
    l.caps
        .as_deref()
        .unwrap_or_else(|| panic!("no caps on {l:?}"))
        .parse()
        .unwrap()
}

fn caps_str(l: &LinkCaps, field: &str) -> Option<String> {
    link_caps(l).structure(0)?.get::<String>(field).ok()
}

fn link_into<'a>(links: &'a [LinkCaps], element: &str) -> &'a LinkCaps {
    links
        .iter()
        .find(|l| l.to.starts_with(&format!("{element}:")))
        .unwrap_or_else(|| panic!("a link into {element}: {links:#?}"))
}

/// Polls the session's buses until `done` or `timeout`, or an error.
fn run_until(
    session: &Session,
    out_log: &mut BusLog,
    in_log: &mut BusLog,
    timeout: Duration,
    done: impl Fn(&Session) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        out_log.drain(session.output().pipeline());
        if let Some(i) = session.ingest() {
            in_log.drain(i.pipeline());
        }
        if done(session) {
            return true;
        }
        if Instant::now() >= deadline || !out_log.errors.is_empty() || !in_log.errors.is_empty() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts an output on the loopback and a camera ingest, streams until 30
/// fresh frames went out (or 15 s pass), checks memory, contexts and caps,
/// and tears both down.
fn stream_a_few_frames(
    camera: &DeviceInfo,
    sink: &DeviceInfo,
    choice: &ModeChoice,
    stage: GpuStage,
    format: OutputFormat,
) -> (Vec<LinkCaps>, Vec<LinkCaps>) {
    let tag = format!("{} {} {stage} -> {format}", choice.mode, choice.ingest);
    let contract = OutputContract {
        format,
        ..OutputContract::default()
    };
    let mut cfg = OutputConfig::new(contract, OutputSink::Loopback(sink.path.clone()));
    cfg.process = stage;
    let output = Output::build(cfg).expect("build output");
    let at_sink = count_buffers(output.pipeline(), names::LOOPBACK, "sink");
    let into_output = memory_kinds(output.pipeline(), names::FRAMES_SRC);
    let mut session = Session::new(output).expect("start output");
    let ingest = session
        .build_ingest(&IngestConfig::new(camera.path.clone(), choice.mode))
        .expect("build ingest");
    session.switch_to(ingest).expect("start ingest");

    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(15),
        |s| s.output().counts().fresh >= 30,
    );
    let state = |p: &MediaPipeline| p.pipeline().state(gst::ClockTime::ZERO).1;
    let out_state = state(session.output().pipeline());
    let in_state = state(session.ingest().unwrap().pipeline());
    let ingest_links = session.ingest().unwrap().pipeline().links();
    let output_links = session.output().pipeline().links();
    let contexts = cuda_contexts(&session);
    let counts = session.output().counts();
    session.stop().expect("teardown to NULL");

    assert!(
        out_log.errors.is_empty(),
        "{tag}: output: {:?}",
        out_log.errors
    );
    assert!(
        in_log.errors.is_empty(),
        "{tag}: ingest: {:?}",
        in_log.errors
    );
    assert!(!out_log.eos && !in_log.eos, "{tag}: EOS");
    assert_eq!(
        (out_state, in_state),
        (gst::State::Playing, gst::State::Playing),
        "{tag}"
    );
    assert!(counts.fresh >= 30, "{tag}: {counts:?}");
    assert!(at_sink.load(Ordering::Relaxed) >= 30, "{tag}");
    // One CUDA context: none created by the ingest, the same object
    // everywhere, and CUDA memory on every link from cudaupload to the slot
    // and from the slot to cudadownload.
    assert_eq!(in_log.cuda_contexts_created, 0, "{tag}");
    assert_one_cuda_context(&contexts, &tag);
    assert_eq!(
        all_cuda_between(&ingest_links, names::UPLOAD, names::FRAMES_SINK),
        Some(true),
        "{tag}: {ingest_links:#?}"
    );
    assert_eq!(
        all_cuda_between(&output_links, names::FRAMES_SRC, names::DOWNLOAD),
        Some(true),
        "{tag}: {output_links:#?}"
    );
    let (cuda, other) = (&into_output.0, &into_output.1);
    assert!(cuda.load(Ordering::Relaxed) > 0, "{tag}");
    assert_eq!(
        other.load(Ordering::Relaxed),
        0,
        "{tag}: system memory into the output"
    );
    assert!(!ingest_links[0].cuda_memory, "{tag}");
    let last = output_links.last().unwrap();
    assert!(!last.cuda_memory, "{tag}: {last:?}");

    // The loopback gets the contract, whatever the camera mode.
    let caps = link_caps(last);
    let s = caps.structure(0).unwrap();
    assert_eq!(s.get::<&str>("format").unwrap(), format.gst_name(), "{tag}");
    assert_eq!(
        (
            s.get::<i32>("width").unwrap(),
            s.get::<i32>("height").unwrap()
        ),
        (1920, 1080),
        "{tag}"
    );
    assert_eq!(
        s.get::<gst::Fraction>("framerate").unwrap(),
        gst::Fraction::new(30, 1),
        "{tag}"
    );
    // ... labelled as v4l2loopback reports it back.
    assert_eq!(
        s.get::<&str>("colorimetry").unwrap(),
        SINK_COLORIMETRY,
        "{tag}: {last:?}"
    );
    // The canonical frame carries the contract label, up to the download.
    assert_eq!(
        caps_str(link_into(&output_links, names::RELABEL), "colorimetry").as_deref(),
        Some(contract.colorimetry()),
        "{tag}: {output_links:?}"
    );
    let canonical = link_into(&ingest_links, names::FRAMES_SINK);
    assert_eq!(
        caps_str(canonical, "colorimetry").as_deref(),
        Some(contract.colorimetry()),
        "{tag}: {canonical:?}"
    );
    // What reaches cudaupload: jpegdec's full-range label for MJPEG, the
    // camera's own label (kept through videoconvert) for YUY2/UYVY.
    let into_upload = link_into(&ingest_links, names::UPLOAD);
    let upload_label = caps_str(into_upload, "colorimetry");
    assert!(upload_label.is_some(), "{tag}: {into_upload:?}");
    match choice.ingest {
        IngestPath::CpuDecode => {
            assert_eq!(upload_label.as_deref(), Some("1:4:0:0"), "{tag}")
        }
        IngestPath::CpuConvert => assert_eq!(
            caps_str(&ingest_links[0], "colorimetry"),
            upload_label,
            "{tag}: camera {:?} vs cudaupload input {into_upload:?}",
            ingest_links[0]
        ),
        IngestPath::GpuDirect => {}
    }
    eprintln!(
        "{tag}: {counts:?}; camera caps {}; loopback caps {}",
        ingest_links[0].caps.as_deref().unwrap_or("-"),
        last.caps.as_deref().unwrap_or("-")
    );
    (ingest_links, output_links)
}

fn pipeline_test(stage: GpuStage) {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let sel = test_camera();
    let sink = test_sink();
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
/// the output delivers the contract in both output formats (NV12, I420).
#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn every_ingest_path_outputs_requested_format() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let sel = test_camera();
    let sink = test_sink();
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
            let (ingest_links, _) =
                stream_a_few_frames(&camera, &sink, &choice, GpuStage::Passthrough, output);
            // What reaches cudaupload is the path's upload format.
            let into_upload = link_into(&ingest_links, names::UPLOAD);
            assert_eq!(
                caps_str(into_upload, "format").as_deref(),
                Some(path.upload_format(choice.mode.format)),
                "{path}: {into_upload:?}"
            );
        }
        ran.push(path);
    }
    assert!(!ran.is_empty(), "{camera}: no ingest path exercised");
}

/// (a) The loopback receives exactly the contract frame rate: every 1 s
/// window of ~6 s holds 30 frames, by timestamp and by arrival time,
/// whatever rate the camera delivers.
#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn output_holds_the_contract_frame_rate() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let sel = test_camera();
    let sink = test_sink();
    let (camera, choice) = select_with(&sel, &test_mode(), DiscoveryRoute::Auto);
    let contract = OutputContract::default();
    let output = Output::build(OutputConfig::new(
        contract,
        OutputSink::Loopback(sink.path.clone()),
    ))
    .expect("build output");
    let arrivals = record_arrivals(output.pipeline(), names::LOOPBACK);
    let mut session = Session::new(output).expect("start output");
    session
        .switch_to_camera(&IngestConfig::new(camera.path.clone(), choice.mode))
        .expect("start ingest");
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    assert!(
        run_until(
            &session,
            &mut out_log,
            &mut in_log,
            Duration::from_secs(15),
            |s| { s.output().counts().fresh > 0 }
        ),
        "no camera frame reached the output: {out_log:?} {in_log:?}"
    );
    // Warm-up, then the measured span.
    run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(1),
        |_| false,
    );
    let c0 = session.output().counts();
    let t0 = Instant::now();
    run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_millis(6_500),
        |_| false,
    );
    let c1 = session.output().counts();
    let elapsed = t0.elapsed().as_secs_f64();
    session.stop().expect("teardown");
    assert!(
        out_log.errors.is_empty() && in_log.errors.is_empty(),
        "{out_log:?} {in_log:?}"
    );

    let log = arrivals.lock().unwrap().clone();
    let period = contract.frame_duration_ns();
    let windows = 6u64;
    // Start the windows on a tick about 1 s into the stream.
    let start_pts = log[log.len().min(30)].0;
    let in_span = |pts: u64| (start_pts..start_pts + windows * 1_000_000_000).contains(&pts);
    let span: Vec<(u64, Instant)> = log.iter().copied().filter(|(p, _)| in_span(*p)).collect();
    let by_pts: Vec<usize> = (0..windows)
        .map(|k| {
            let w = start_pts + k * 1_000_000_000;
            span.iter()
                .filter(|(p, _)| (w..w + 1_000_000_000).contains(p))
                .count()
        })
        .collect();
    // Arrival times relative to the grid: offset = arrival - pts.
    let origin = span[0].1;
    let mut offsets: Vec<i64> = span
        .iter()
        .map(|(p, t)| t.duration_since(origin).as_nanos() as i64 - (*p - start_pts) as i64)
        .collect();
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    for o in &mut offsets {
        *o -= median;
    }
    let jitter_max = offsets.iter().map(|o| o.unsigned_abs()).max().unwrap_or(0);
    // Windows on arrival time, shifted by half a period so that jitter below
    // that never moves a frame across a boundary.
    let half = (period / 2) as i64;
    let by_arrival: Vec<usize> = (0..windows as i64)
        .map(|k| {
            let lo = k * 1_000_000_000 - half;
            span.iter()
                .zip(&offsets)
                .filter(|((p, _), o)| {
                    let t = (*p - start_pts) as i64 + **o;
                    (lo..lo + 1_000_000_000).contains(&t)
                })
                .count()
        })
        .collect();
    let d = |a: u64, b: u64| a - b;
    let input_fps = d(c1.input_frames, c0.input_frames) as f64 / elapsed;
    eprintln!(
        "{} {}: frames per 1 s window by pts {by_pts:?}, by arrival {by_arrival:?}; \
         arrival jitter max {:.2} ms; camera input {input_fps:.1} fps; over {elapsed:.1} s: \
         fresh {} repeat {} placeholder {} superseded {} late {} skipped {}",
        choice.mode,
        choice.ingest,
        jitter_max as f64 / 1e6,
        d(c1.fresh, c0.fresh),
        d(c1.repeat, c0.repeat),
        d(c1.placeholder, c0.placeholder),
        d(c1.superseded, c0.superseded),
        d(c1.late, c0.late),
        d(c1.skipped, c0.skipped),
    );
    assert!(by_pts.iter().all(|n| *n == 30), "by pts: {by_pts:?}");
    assert!(
        by_arrival.iter().all(|n| *n == 30),
        "by arrival: {by_arrival:?}"
    );
    assert_eq!(d(c1.skipped, c0.skipped), 0);
    assert_eq!(d(c1.placeholder, c0.placeholder), 0, "the camera stalled");
}

/// Arrival time of every buffer reaching `element`'s sink pad.
fn record_arrival_times(p: &MediaPipeline, element: &str) -> Arc<Mutex<Vec<Instant>>> {
    let log: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let l = Arc::clone(&log);
    p.element(element)
        .and_then(|e| e.static_pad("sink"))
        .expect("sink pad")
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            l.lock().unwrap().push(Instant::now());
            gst::PadProbeReturn::Ok
        });
    log
}

/// Counts the CAPS events reaching `element`'s sink pad (each one is a
/// caps negotiation).
fn count_caps_events(p: &MediaPipeline, element: &str) -> Arc<AtomicU64> {
    let n = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&n);
    p.element(element)
        .and_then(|e| e.static_pad("sink"))
        .expect("sink pad")
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(e)) = &info.data
                && e.type_() == gst::EventType::Caps
            {
                c.fetch_add(1, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    n
}

/// The longest interval without a buffer over `[from, to]`, the edges
/// included: from `from` to the first arrival, between arrivals, and from
/// the last arrival to `to`.
fn longest_silence(arrivals: &[Instant], from: Instant, to: Instant) -> Duration {
    let mut points: Vec<Instant> = arrivals
        .iter()
        .copied()
        .filter(|t| *t >= from && *t <= to)
        .collect();
    points.sort_unstable();
    let mut edges = vec![from];
    edges.extend(points);
    edges.push(to);
    edges
        .windows(2)
        .map(|w| w[1].saturating_duration_since(w[0]))
        .max()
        .unwrap_or_default()
}

/// Starts a loopback consumer once the output streams (with
/// `exclusive_caps` the capture side appears only then), retrying until it
/// receives caps.
fn start_consumer(sink: &DeviceInfo, size: (u32, u32)) -> MediaPipeline {
    let deadline = Instant::now() + Duration::from_secs(10);
    let has_caps = |c: &MediaPipeline| {
        let src = c.element(names::CONSUMER_SRC).unwrap();
        src.static_pad("src").unwrap().current_caps().is_some()
    };
    loop {
        let c =
            MediaPipeline::build_consumer(&sink.path, size, OutputFormat::Nv12).expect("consumer");
        if c.play().is_ok() {
            while !has_caps(&c) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if has_caps(&c) {
                return c;
            }
        }
        assert!(Instant::now() < deadline, "the consumer never started");
        drop(c);
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// (b) Switching sources while an application reads the loopback: the
/// output never stops or renegotiates, and the consumer keeps receiving.
/// Switches between `OBC_TEST_CAMERA` and `OBC_TEST_CAMERA_2` (or two
/// openings of the first camera). Over the four switches neither the
/// loopback sink nor the consumer goes longer than two output periods
/// (plus 20 ms of scheduling margin) without a frame, and each sees exactly
/// one caps negotiation for the whole test (none at a switch, not even
/// back to the same caps).
///
/// `OBC_TEST_MODE` applies to the first camera only; the second camera
/// uses the default mode request.
#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn switching_sources_keeps_the_output_and_its_consumer() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let sel_a = test_camera();
    let sink = test_sink();
    let (cam_a, choice_a) = select_with(&sel_a, &test_mode(), DiscoveryRoute::Auto);
    let (cam_b, choice_b) = match camera_from_env("OBC_TEST_CAMERA_2") {
        Some(sel_b) => select_with(&sel_b, &ModeRequest::default(), DiscoveryRoute::Auto),
        None => {
            eprintln!("OBC_TEST_CAMERA_2 unset: switching between two openings of {cam_a}");
            (cam_a.clone(), choice_a.clone())
        }
    };
    let cfg_a = IngestConfig::new(cam_a.path.clone(), choice_a.mode);
    let cfg_b = IngestConfig::new(cam_b.path.clone(), choice_b.mode);
    let contract = OutputContract::default();

    let output = Output::build(OutputConfig::new(
        contract,
        OutputSink::Loopback(sink.path.clone()),
    ))
    .expect("build output");
    let sink_arrivals = record_arrival_times(output.pipeline(), names::LOOPBACK);
    let sink_caps_events = count_caps_events(output.pipeline(), names::LOOPBACK);
    let mut session = Session::new(output).expect("start output");
    session.switch_to_camera(&cfg_a).expect("first source");
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    assert!(run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(15),
        |s| { s.output().counts().fresh > 0 }
    ));
    let consumer = start_consumer(&sink, (contract.width, contract.height));
    let consumed = count_buffers(&consumer, names::CONSUMER_SINK, "sink");
    let consumer_arrivals = record_arrival_times(&consumer, names::CONSUMER_SINK);
    // The consumer negotiated once before this probe (start_consumer waits
    // for its caps): any CAPS event from here on is a renegotiation.
    let consumer_caps_events = count_caps_events(&consumer, names::CONSUMER_SINK);
    let mut consumer_log = BusLog::default();
    let loopback_pad = session
        .output()
        .pipeline()
        .element(names::LOOPBACK)
        .and_then(|e| e.static_pad("sink"))
        .unwrap();
    let caps_before = loopback_pad.current_caps().expect("loopback caps");

    let mut report = Vec::new();
    let switching_from = Instant::now();
    for i in 0..4 {
        let (cfg, label) = if i % 2 == 0 {
            (
                &cfg_b,
                format!("{} {}", cam_b.path.display(), choice_b.mode),
            )
        } else {
            (
                &cfg_a,
                format!("{} {}", cam_a.path.display(), choice_a.mode),
            )
        };
        let c0: OutputCounts = session.output().counts();
        let seen0 = consumed.load(Ordering::Relaxed);
        session
            .switch_to_camera(cfg)
            .unwrap_or_else(|e| panic!("switch {i} to {label}: {e}"));
        let source = session.ingest().unwrap().source_id();
        let fresh_again = run_until(
            &session,
            &mut out_log,
            &mut in_log,
            Duration::from_secs(10),
            |s| s.output().first_fresh(source).is_some(),
        );
        // Stream from the new source for a while.
        run_until(
            &session,
            &mut out_log,
            &mut in_log,
            Duration::from_millis(1_500),
            |_| false,
        );
        consumer_log.drain(&consumer);
        let timing = session.last_switch().unwrap();
        let c1 = session.output().counts();
        let seen = consumed.load(Ordering::Relaxed) - seen0;
        report.push(format!(
            "switch {i} -> {label}: first frame after {:?} (old stopped after {:?}, new playing after {:?}); \
             consumer +{seen}; repeat +{} placeholder +{} fresh +{}",
            timing.gap(),
            timing.previous_stopped.map(|t| t - timing.requested),
            timing.started.map(|t| t - timing.requested),
            c1.repeat - c0.repeat,
            c1.placeholder - c0.placeholder,
            c1.fresh - c0.fresh,
        ));
        assert!(
            fresh_again,
            "switch {i}: no frame from {label}: {out_log:?} {in_log:?}"
        );
        assert!(
            out_log.errors.is_empty() && !out_log.eos,
            "switch {i}: output {out_log:?}"
        );
        assert!(
            in_log.errors.is_empty() && !in_log.eos,
            "switch {i}: ingest {in_log:?}"
        );
        assert!(
            consumer_log.errors.is_empty() && !consumer_log.eos,
            "switch {i}: consumer {consumer_log:?}"
        );
        // ~2 s of output at 30 fps reached the consumer.
        assert!(
            seen >= 30,
            "switch {i}: the consumer received only {seen} frames"
        );
        assert_eq!(in_log.cuda_contexts_created, 0, "switch {i}");
        assert_one_cuda_context(&cuda_contexts(&session), &format!("switch {i}"));
    }
    let switching_to = Instant::now();
    let caps_after = loopback_pad.current_caps().expect("loopback caps");
    let consumer_caps = consumer
        .element(names::CONSUMER_SINK)
        .and_then(|e| e.static_pad("sink"))
        .and_then(|p| p.current_caps());
    consumer.shutdown().expect("consumer NULL");
    session.stop().expect("teardown");
    for r in &report {
        eprintln!("{r}");
    }
    let limit = Duration::from_nanos(2 * contract.frame_duration_ns()) + Duration::from_millis(20);
    let sink_silence =
        longest_silence(&sink_arrivals.lock().unwrap(), switching_from, switching_to);
    let consumer_silence = longest_silence(
        &consumer_arrivals.lock().unwrap(),
        switching_from,
        switching_to,
    );
    eprintln!(
        "longest interval without a frame over the switches: loopback sink {sink_silence:?}, \
         consumer {consumer_silence:?} (limit {limit:?})"
    );
    assert!(
        sink_silence <= limit,
        "the output stalled for {sink_silence:?} (limit {limit:?})"
    );
    assert!(
        consumer_silence <= limit,
        "the consumer received nothing for {consumer_silence:?} (limit {limit:?})"
    );
    assert_eq!(caps_before, caps_after, "the output renegotiated");
    assert_eq!(
        sink_caps_events.load(Ordering::Relaxed),
        1,
        "the loopback sink saw more than one caps negotiation"
    );
    assert!(consumer_caps.is_some(), "the consumer has no caps");
    assert_eq!(
        consumer_caps_events.load(Ordering::Relaxed),
        0,
        "the consumer renegotiated during the switches"
    );
}

/// A system-memory NV12 frame: `rows` of `(count, luma)` bands, top to
/// bottom, neutral chroma.
fn banded_nv12(width: usize, bands: &[(usize, u8)]) -> gst::Buffer {
    let height: usize = bands.iter().map(|b| b.0).sum();
    let mut data = vec![128u8; width * height * 3 / 2];
    let mut row = 0;
    for &(count, y) in bands {
        data[row * width..(row + count) * width].fill(y);
        row += count;
    }
    let mut b = gst::Buffer::from_mut_slice(data);
    b.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    b
}

/// A system-memory NV12 frame: `columns` of `(count, luma)` bands, left
/// to right, neutral chroma. `width` (the sum of the counts) must be a
/// multiple of 4, so that rows are not padded.
fn columned_nv12(height: usize, columns: &[(usize, u8)]) -> gst::Buffer {
    let width: usize = columns.iter().map(|c| c.0).sum();
    assert_eq!(width % 4, 0, "{width}: rows would be padded");
    let mut row = Vec::with_capacity(width);
    for &(count, y) in columns {
        row.extend(std::iter::repeat_n(y, count));
    }
    let mut data = vec![128u8; width * height * 3 / 2];
    for r in 0..height {
        data[r * width..(r + 1) * width].copy_from_slice(&row);
    }
    let mut b = gst::Buffer::from_mut_slice(data);
    b.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    b
}

/// Runs one frame through the path a canonical frame takes: `appsrc !
/// <canonical tail> ! appsink` (the ingest pipeline, whose `appsink` feeds
/// the slot), then `appsrc ! cudadownload ! appsink` in a second pipeline
/// sharing the CUDA context (the output pipeline). Returns the downloaded
/// frame, and whether the canonical frame still carried a crop region
/// (`GstVideoCropMeta`), which nothing after the ingest applies.
fn through_canonical_tail(
    input: gst::Buffer,
    size: (u32, u32),
    out: (u32, u32),
    aspect: AspectPolicy,
) -> (gst_video::VideoInfo, gst::Buffer, bool) {
    let canonical = Canonical::of(&OutputContract {
        width: out.0,
        height: out.1,
        ..OutputContract::default()
    });
    let pipeline = gst::Pipeline::with_name("canonical-tail-test");
    let caps: gst::Caps = format!(
        "video/x-raw,format=NV12,width={},height={},framerate=30/1,pixel-aspect-ratio=1/1,\
         colorimetry=2:4:5:1",
        size.0, size.1
    )
    .parse()
    .unwrap();
    let src = gst_app::AppSrc::builder()
        .caps(&caps)
        .format(gst::Format::Time)
        .build();
    let sink = gst_app::AppSink::builder().sync(false).build();
    let (tail, _) = canonical_tail("NV12", size.0, size.1, &canonical, aspect).unwrap();
    let mut chain: Vec<gst::Element> = vec![src.clone().upcast()];
    chain.extend(tail);
    chain.push(sink.clone().upcast());
    pipeline.add_many(&chain).unwrap();
    gst::Element::link_many(&chain).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    src.push_buffer(input).unwrap();
    src.end_of_stream().unwrap();
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(10));
    let error = pipeline
        .bus()
        .unwrap()
        .pop_filtered(&[gst::MessageType::Error]);
    let cuda = pipeline.context(CUDA_CONTEXT_TYPE);
    pipeline.set_state(gst::State::Null).unwrap();
    let sample = sample.unwrap_or_else(|| panic!("no canonical frame: {error:?}"));
    let canonical_frame = sample.buffer_owned().unwrap();
    let cropped = canonical_frame.meta::<gst_video::VideoCropMeta>().is_some();

    let download = gst::Pipeline::with_name("download-test");
    if let Some(ctx) = &cuda {
        download.set_context(ctx);
    }
    let src = gst_app::AppSrc::builder()
        .caps(&sample.caps_owned().unwrap())
        .format(gst::Format::Time)
        .build();
    let sink = gst_app::AppSink::builder().sync(false).build();
    let chain: Vec<gst::Element> = vec![
        src.clone().upcast(),
        gst::ElementFactory::make("cudadownload")
            .name(names::DOWNLOAD)
            .build()
            .unwrap(),
        sink.clone().upcast(),
    ];
    download.add_many(&chain).unwrap();
    gst::Element::link_many(&chain).unwrap();
    download.set_state(gst::State::Playing).unwrap();
    src.push_buffer(canonical_frame).unwrap();
    src.end_of_stream().unwrap();
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(10));
    let error = download
        .bus()
        .unwrap()
        .pop_filtered(&[gst::MessageType::Error]);
    download.set_state(gst::State::Null).unwrap();
    let sample = sample.unwrap_or_else(|| panic!("no downloaded frame: {error:?}"));
    let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
    (info, sample.buffer_owned().unwrap(), cropped)
}

fn luma(info: &gst_video::VideoInfo, buffer: &gst::Buffer, x: usize, y: usize) -> u8 {
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer.as_ref(), info).unwrap();
    let stride = frame.plane_stride()[0] as usize;
    frame.plane_data(0).unwrap()[y * stride + x]
}

/// (c) GPU only: a source taller than the output (4:3 and square into
/// 16:9) with a white band on top, a grey band at the bottom and mid-grey
/// between; and a source wider than the output (16:9 into square and 9:16)
/// with the bands on the left and right. Crop-to-fill removes both bands
/// and fills the output, also when the kept region already has the output
/// size (no scaling left to do); letterbox keeps them and adds black bars
/// on the other axis (left and right for the taller source, top and
/// bottom for the wider one).
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_crop_fills_and_letterbox_pads() {
    init();
    require_elements(&[
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
    ]);
    // (source size, output size, band rows): a 4:3 source whose top and
    // bottom bands are exactly what crop-to-fill removes.
    for (src, out, band) in [
        ((640u32, 480u32), (1920u32, 1080u32), 60usize),
        ((1920, 1440), (1920, 1080), 180),
        ((1280, 960), (1280, 720), 120),
        ((1080, 1080), (1920, 1080), 236),
    ] {
        let crop = crop_for(AspectPolicy::Crop, src.0, src.1, out.0, out.1);
        assert_eq!(
            (crop.top, crop.bottom, crop.left, crop.right),
            (band as u32, band as u32, 0, 0),
            "test bands must match the crop"
        );
        let tag = format!("{}x{} -> {}x{}", src.0, src.1, out.0, out.1);
        let bands = [(band, 235u8), (src.1 as usize - 2 * band, 100), (band, 180)];
        let (info, frame, cropped) = through_canonical_tail(
            banded_nv12(src.0 as usize, &bands),
            src,
            out,
            AspectPolicy::Crop,
        );
        assert_eq!((info.width(), info.height()), out, "{tag}");
        assert!(
            !cropped,
            "{tag}: the canonical frame still carries a crop region"
        );
        let (w, h) = (out.0 as usize, out.1 as usize);
        let cols = [0, 1, w / 2, w - 2, w - 1];
        // Inside the frame the bands are gone: mid-grey everywhere.
        for y in [1, 2, 3, 100, h / 2, h - 100, h - 4, h - 3, h - 2] {
            for x in cols {
                let v = luma(&info, &frame, x, y);
                assert!(
                    v.abs_diff(100) <= 2,
                    "{tag}: crop left a band at ({x}, {y}): {v}"
                );
            }
        }
        // The outermost rows: when scaling, the bilinear filter reaches up
        // to a third of a source row beyond the crop rectangle, so they
        // blend slightly with the row just outside it (here a hard band
        // edge; on camera content, the continuing image). They must stay
        // nearer the kept content.
        for (y, band) in [(0, 235u8), (h - 1, 180)] {
            for x in cols {
                let v = luma(&info, &frame, x, y);
                assert!(
                    v.abs_diff(100) < v.abs_diff(band),
                    "{tag}: crop edge row {y} at {x}: {v} (band {band})"
                );
            }
            eprintln!(
                "crop {tag}: edge row {y}: luma {}",
                luma(&info, &frame, w / 2, y)
            );
        }
    }

    // Side crop: a source wider than the output loses its left and right
    // bands, whose widths are the crop the geometry computes.
    for (src, out) in [
        ((1920u32, 1080u32), (1080u32, 1080u32)),
        ((1920, 1080), (720, 1280)),
    ] {
        let tag = format!("{}x{} -> {}x{}", src.0, src.1, out.0, out.1);
        let crop = crop_for(AspectPolicy::Crop, src.0, src.1, out.0, out.1);
        assert!(
            crop.left > 0 && crop.right > 0 && crop.top == 0,
            "{tag}: {crop:?}"
        );
        let (left, right) = (crop.left as usize, crop.right as usize);
        let columns = [
            (left, 235u8),
            (src.0 as usize - left - right, 100),
            (right, 180),
        ];
        let (info, frame, cropped) = through_canonical_tail(
            columned_nv12(src.1 as usize, &columns),
            src,
            out,
            AspectPolicy::Crop,
        );
        assert_eq!((info.width(), info.height()), out, "{tag}");
        assert!(
            !cropped,
            "{tag}: the canonical frame still carries a crop region"
        );
        let (w, h) = (out.0 as usize, out.1 as usize);
        let rows = [0, 1, h / 2, h - 2, h - 1];
        for x in [1, 2, 3, 100, w / 2, w - 100, w - 4, w - 3, w - 2] {
            for y in rows {
                let v = luma(&info, &frame, x, y);
                assert!(
                    v.abs_diff(100) <= 2,
                    "{tag}: crop left a band at ({x}, {y}): {v}"
                );
            }
        }
        // The outermost columns may blend slightly with the column just
        // outside the crop (see the rows above).
        for (x, band) in [(0, 235u8), (w - 1, 180)] {
            for y in rows {
                let v = luma(&info, &frame, x, y);
                assert!(
                    v.abs_diff(100) < v.abs_diff(band),
                    "{tag}: crop edge column {x} at {y}: {v} (band {band})"
                );
            }
            eprintln!(
                "crop {tag}: edge column {x}: luma {}",
                luma(&info, &frame, x, h / 2)
            );
        }
    }

    // Letterbox of a wider source: 1920x1080 scales to 1080x608 in a
    // 1080x1080 frame, centred: bars of about 236 rows above and below;
    // the 420-column side bands become about 236 columns.
    let columns = [(420, 235u8), (1080, 100), (420, 180)];
    let (info, frame, _) = through_canonical_tail(
        columned_nv12(1080, &columns),
        (1920, 1080),
        (1080, 1080),
        AspectPolicy::Letterbox,
    );
    assert_eq!((info.width(), info.height()), (1080, 1080));
    for ((x, y), want, what) in [
        ((540, 540), 100u8, "middle"),
        ((100, 540), 235, "left band"),
        ((980, 540), 180, "right band"),
        ((100, 300), 235, "left band, near the top bar"),
        ((540, 100), 16, "top bar"),
        ((540, 225), 16, "top bar edge"),
        ((540, 855), 16, "bottom bar edge"),
        ((540, 980), 16, "bottom bar"),
    ] {
        let v = luma(&info, &frame, x, y);
        eprintln!("letterbox (wider source) {what} ({x}, {y}): {v}");
        assert!(
            v.abs_diff(want) <= 4,
            "letterbox (wider source) {what} at ({x}, {y}): {v}, want {want}"
        );
    }

    let bands = [(60, 235u8), (360, 100), (60, 180)];
    let (info, frame, _) = through_canonical_tail(
        banded_nv12(640, &bands),
        (640, 480),
        (1920, 1080),
        AspectPolicy::Letterbox,
    );
    assert_eq!((info.width(), info.height()), (1920, 1080));
    // 640x480 scales to 1440x1080, centred: bars of 240 columns each side;
    // the bands become 135 rows each.
    let probes = [
        ((960, 60), 235u8, "top band"),
        ((960, 540), 100, "middle"),
        ((960, 1020), 180, "bottom band"),
        ((300, 60), 235, "top band, left of centre"),
        ((100, 540), 16, "left bar"),
        ((230, 540), 16, "left bar edge"),
        ((1690, 540), 16, "right bar edge"),
        ((1819, 540), 16, "right bar"),
    ];
    for ((x, y), want, what) in probes {
        let v = luma(&info, &frame, x, y);
        eprintln!("letterbox {what} ({x}, {y}): {v}");
        assert!(
            v.abs_diff(want) <= 4,
            "letterbox {what} at ({x}, {y}): {v}, want {want}"
        );
    }
}

/// A test-pattern source for the GPU-only output tests: `videotestsrc`
/// (live, solid white) at `fps`, 640x480 NV12, stopping after `frames`.
fn test_pattern_ingest(output: &Output, fps: i32, frames: i32) -> Ingest {
    pattern_ingest(output, fps, frames, "white", None)
}

/// A test-pattern source of `pattern`, whose first frame reaches the slot
/// `delay` after it starts (a `queue` holding back that much), as a camera
/// that is slow to start.
fn pattern_ingest(
    output: &Output,
    fps: i32,
    frames: i32,
    pattern: &str,
    delay: Option<Duration>,
) -> Ingest {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .property("num-buffers", frames)
        .property_from_str("pattern", pattern)
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            format!(
                "video/x-raw,format=NV12,width=640,height=480,framerate={fps}/1,\
                 colorimetry=2:4:5:1"
            )
            .parse::<gst::Caps>()
            .unwrap(),
        )
        .build()
        .unwrap();
    let mut head = vec![src, caps];
    if let Some(d) = delay {
        let ns = u64::try_from(d.as_nanos()).unwrap();
        head.push(
            gst::ElementFactory::make("queue")
                .property("min-threshold-time", ns)
                .property("max-size-time", 2 * ns + 1_000_000_000)
                .property("max-size-buffers", 0u32)
                .property("max-size-bytes", 0u32)
                .build()
                .unwrap(),
        );
    }
    Ingest::from_head(
        head,
        "NV12",
        (640, 480),
        AspectPolicy::Crop,
        output.frame_writer(),
    )
    .expect("test pattern ingest")
}

/// Per output buffer: PTS, the luma at the frame centre (at the sink), and
/// the address of the first memory pushed into the output (at `appsrc`).
#[derive(Default)]
struct OutputTrace {
    emitted: Vec<EmitInfo>,
    centre_luma: Vec<(u64, u8)>,
    memory: Vec<(u64, usize)>,
}

fn trace_output(output: &mut Output) -> Arc<Mutex<OutputTrace>> {
    let trace: Arc<Mutex<OutputTrace>> = Arc::default();
    let t = Arc::clone(&trace);
    output.set_observer(Some(Arc::new(move |e: &EmitInfo| {
        t.lock().unwrap().emitted.push(*e);
    })));
    let t = Arc::clone(&trace);
    let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, 1920, 1080)
        .build()
        .unwrap();
    output
        .pipeline()
        .element(names::LOOPBACK)
        .and_then(|e| e.static_pad("sink"))
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, probe| {
            if let Some(b) = probe.buffer()
                && let Some(pts) = b.pts()
                && let Ok(f) = gst_video::VideoFrameRef::from_buffer_ref_readable(b, &info)
            {
                let stride = f.plane_stride()[0] as usize;
                let v = f.plane_data(0).unwrap()[540 * stride + 960];
                t.lock().unwrap().centre_luma.push((pts.nseconds(), v));
            }
            gst::PadProbeReturn::Ok
        });
    let t = Arc::clone(&trace);
    output
        .pipeline()
        .element(names::FRAMES_SRC)
        .and_then(|e| e.static_pad("src"))
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, probe| {
            if let Some(b) = probe.buffer()
                && let Some(pts) = b.pts()
                && b.n_memory() > 0
            {
                let m = b.peek_memory(0).as_ptr() as usize;
                t.lock().unwrap().memory.push((pts.nseconds(), m));
            }
            gst::PadProbeReturn::Ok
        });
    trace
}

/// (d) GPU only: a source that stops is repeated until the gap threshold
/// (500 ms), then replaced by the black placeholder (luma 16). Repeats
/// re-push the same CUDA memory.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_stalled_source_repeats_then_sends_the_placeholder() {
    init();
    require_elements(&[
        "videotestsrc",
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
    ]);
    let mut output = Output::build(OutputConfig::new(
        OutputContract::default(),
        OutputSink::Fake,
    ))
    .expect("build output");
    let trace = trace_output(&mut output);
    let mut session = Session::new(output).expect("start output");
    // One second of frames, then the source ends.
    let ingest = test_pattern_ingest(session.output(), 30, 30);
    session.switch_to(ingest).expect("start source");
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_millis(3_000),
        |_| false,
    );
    let contexts = cuda_contexts(&session);
    let ingest_links = session.ingest().unwrap().pipeline().links();
    let output_links = session.output().pipeline().links();
    let counts = session.output().counts();
    session.stop().expect("teardown");

    assert!(out_log.errors.is_empty() && !out_log.eos, "{out_log:?}");
    assert!(in_log.errors.is_empty(), "{in_log:?}");
    assert!(in_log.eos, "the test source should have ended");
    assert_eq!(in_log.cuda_contexts_created, 0);
    assert_one_cuda_context(&contexts, "test pattern");
    assert_eq!(
        all_cuda_between(&ingest_links, names::UPLOAD, names::FRAMES_SINK),
        Some(true)
    );
    assert_eq!(
        all_cuda_between(&output_links, names::FRAMES_SRC, names::DOWNLOAD),
        Some(true)
    );

    let t = trace.lock().unwrap();
    let kinds: Vec<TickKind> = t.emitted.iter().map(|e| e.kind).collect();
    let first_fresh = kinds
        .iter()
        .position(|k| *k == TickKind::Fresh)
        .expect("a fresh frame");
    let last_fresh = kinds.iter().rposition(|k| *k == TickKind::Fresh).unwrap();
    let after: Vec<TickKind> = kinds[last_fresh + 1..].to_vec();
    let repeats = after.iter().take_while(|k| **k == TickKind::Repeat).count();
    eprintln!(
        "{counts:?}; ticks: {} placeholder before the source, fresh {}..={}, then {repeats} \
         repeats and {} placeholders",
        first_fresh,
        first_fresh,
        last_fresh,
        after.len() - repeats
    );
    assert!(
        kinds[..first_fresh]
            .iter()
            .all(|k| *k == TickKind::Placeholder)
    );
    // 500 ms at 30 fps is 15 ticks; the tick phase makes it 14 or 15.
    assert!((14..=16).contains(&repeats), "{repeats} repeats");
    assert!(after.len() - repeats >= 30, "too few placeholders");
    assert!(
        after[repeats..].iter().all(|k| *k == TickKind::Placeholder),
        "{after:?}"
    );

    // At the sink: white source frames, black placeholders.
    let luma_at = |pts: u64| {
        t.centre_luma
            .iter()
            .find(|(p, _)| *p == pts)
            .map(|(_, v)| *v)
    };
    for e in &t.emitted {
        let Some(v) = luma_at(e.pts_ns) else { continue };
        match e.kind {
            TickKind::Placeholder => assert_eq!(v, 16, "placeholder at {}", e.pts_ns),
            TickKind::Fresh | TickKind::Repeat => {
                assert!(
                    v.abs_diff(235) <= 3,
                    "{:?} luma {v} at {}",
                    e.kind,
                    e.pts_ns
                )
            }
        }
    }
    assert!(
        t.centre_luma.len() >= 80,
        "{} frames at the sink",
        t.centre_luma.len()
    );
    // Repeats push the memory of the last fresh frame, not a copy.
    let memory_at = |pts: u64| t.memory.iter().find(|(p, _)| *p == pts).map(|(_, m)| *m);
    let fresh_mem = memory_at(t.emitted[last_fresh].pts_ns).expect("traced");
    for e in &t.emitted[last_fresh + 1..=last_fresh + repeats] {
        assert_eq!(
            memory_at(e.pts_ns),
            Some(fresh_mem),
            "repeat at {} copied",
            e.pts_ns
        );
        assert_eq!(
            e.origin.map(|o| o.seq),
            t.emitted[last_fresh].origin.map(|o| o.seq)
        );
    }
    let placeholder_mem: Vec<usize> = t.emitted[last_fresh + repeats + 1..]
        .iter()
        .filter_map(|e| memory_at(e.pts_ns))
        .collect();
    assert!(
        placeholder_mem.windows(2).all(|w| w[0] == w[1]),
        "the placeholder is one prepared frame"
    );
}

/// (d) GPU only: a 15 fps source is padded to the 30 fps contract with
/// about one repeat per fresh frame, and no placeholder.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_slow_source_is_padded_to_the_contract_rate() {
    init();
    require_elements(&[
        "videotestsrc",
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
    ]);
    let mut output = Output::build(OutputConfig::new(
        OutputContract::default(),
        OutputSink::Fake,
    ))
    .expect("build output");
    let trace = trace_output(&mut output);
    let mut session = Session::new(output).expect("start output");
    let ingest = test_pattern_ingest(session.output(), 15, -1);
    session.switch_to(ingest).expect("start source");
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    assert!(run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(10),
        |s| { s.output().counts().fresh > 0 }
    ));
    run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_millis(500),
        |_| false,
    );
    let c0 = session.output().counts();
    let t0 = Instant::now();
    run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_millis(3_000),
        |_| false,
    );
    let c1 = session.output().counts();
    let elapsed = t0.elapsed().as_secs_f64();
    session.stop().expect("teardown");
    assert!(
        out_log.errors.is_empty() && in_log.errors.is_empty(),
        "{out_log:?} {in_log:?}"
    );

    let (fresh, repeat, placeholder) = (
        c1.fresh - c0.fresh,
        c1.repeat - c0.repeat,
        c1.placeholder - c0.placeholder,
    );
    let ticks = fresh + repeat + placeholder;
    let rate = ticks as f64 / elapsed;
    let input = (c1.input_frames - c0.input_frames) as f64 / elapsed;
    eprintln!(
        "15 fps source: {ticks} ticks in {elapsed:.2} s ({rate:.2}/s), input {input:.2} fps, \
         fresh {fresh} repeat {repeat} placeholder {placeholder}"
    );
    assert_eq!(placeholder, 0);
    assert!((29.0..=31.0).contains(&rate), "{rate}");
    let share = repeat as f64 / ticks as f64;
    assert!((0.4..=0.6).contains(&share), "repeat share {share:.2}");
    // Every pushed buffer reached the sink, one per tick.
    let t = trace.lock().unwrap();
    assert!(t.emitted.iter().all(|e| e.pushed));
}

/// (e) A consumer that keeps the loopback open after its producer stopped
/// (OBS does) pins the producer's format and rate. An output whose contract
/// differs is refused at start with [`PipelineError::LoopbackFormatHeld`]
/// naming both, and streams once the consumer lets go.
#[test]
#[ignore = "needs OBC_TEST_CAMERA, a free v4l2loopback device and an NVIDIA GPU"]
fn held_loopback_format_is_reported_before_streaming() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    let sel = test_camera();
    let sink = test_sink();
    let (camera, choice) = select_with(&sel, &test_mode(), DiscoveryRoute::Auto);
    let contract = OutputContract::default();
    let held_rate = 15;
    let device = sink.path.to_string_lossy().into_owned();

    // An earlier stream: the contract's size and format, another rate.
    let earlier = gst::parse::launch(&format!(
        "videotestsrc is-live=true ! video/x-raw,format=NV12,width={},height={},framerate={held_rate}/1 \
         ! v4l2sink device={device} sync=false",
        contract.width, contract.height
    ))
    .expect("earlier producer");
    earlier
        .set_state(gst::State::Playing)
        .expect("earlier PLAYING");
    let consumer = start_consumer(&sink, (contract.width, contract.height));
    // The producer leaves; the consumer (errored, not yet NULL) keeps the
    // device open, as OBS does.
    earlier.set_state(gst::State::Null).expect("earlier NULL");
    drop(earlier);

    let mut output = Output::build(OutputConfig::new(
        contract,
        OutputSink::Loopback(sink.path.clone()),
    ))
    .expect("build output");
    let err = output
        .start()
        .expect_err("a held loopback at another rate must be refused");
    assert!(!output.is_running());
    let (_, state, _) = output.pipeline().pipeline().state(gst::ClockTime::ZERO);
    assert_eq!(state, gst::State::Null, "a refused output is back in NULL");
    drop(output);
    match &err {
        PipelineError::LoopbackFormatHeld {
            accepted, wanted, ..
        } => {
            assert_eq!(
                accepted,
                &format!("NV12 {}x{}@{held_rate}", contract.width, contract.height)
            );
            assert_eq!(
                wanted,
                &format!("NV12 {}x{}@30", contract.width, contract.height)
            );
        }
        other => panic!("expected LoopbackFormatHeld, got {other}"),
    }
    eprintln!("{err}");

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

/// (f) A consumer that holds the loopback while the output restarts (stop,
/// then start again): the restart is accepted and streams, because the
/// output labels its frames at the loopback the way v4l2loopback reports
/// them back. A full-range contract is refused for the loopback before
/// anything streams (the device carries no colour range). The output's
/// counters carry on across the restart: none goes back, and frames written
/// before it are not counted as superseded after it.
///
/// Writes to the loopback named by `OBC_TEST_SINK` only (no default), and
/// fails without it.
#[test]
#[ignore = "needs OBC_TEST_SINK (a free v4l2loopback device) and an NVIDIA GPU"]
fn output_restarts_while_a_consumer_holds_the_loopback() {
    init();
    require_elements(&[
        "videotestsrc",
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
        "capssetter",
        "v4l2sink",
    ]);
    assert!(
        std::env::var_os("OBC_TEST_SINK").is_some(),
        "this test needs OBC_TEST_SINK=/dev/videoN, a free v4l2loopback device (it never \
         writes to a device found by its label alone)"
    );
    let sink = test_sink();

    // Full range cannot reach a loopback: refused at build, before streaming.
    let full = OutputContract {
        range: ColourRange::Full,
        ..OutputContract::default()
    };
    match Output::build(OutputConfig::new(
        full,
        OutputSink::Loopback(sink.path.clone()),
    )) {
        Err(PipelineError::LoopbackRefused { wanted, .. }) => {
            assert_eq!(wanted, full.to_string())
        }
        Err(other) => panic!("expected LoopbackRefused for a full-range contract, got {other}"),
        Ok(_) => panic!("a full-range contract was accepted for a loopback"),
    }

    let contract = OutputContract::default();
    let mut consumer: Option<MediaPipeline> = None;
    let mut output = Output::build(OutputConfig::new(
        contract,
        OutputSink::Loopback(sink.path.clone()),
    ))
    .expect("build output");
    let at_sink = count_buffers(output.pipeline(), names::LOOPBACK, "sink");
    let mut before: Option<OutputCounts> = None;
    for round in 0..2 {
        let tag = format!("start {}", round + 1);
        output
            .start()
            .unwrap_or_else(|e| panic!("{tag}: the output did not start: {e}"));
        let mut ingest = test_pattern_ingest(&output, 30, 100_000);
        ingest
            .start(
                output
                    .cuda_context()
                    .expect("a started output has a CUDA context"),
            )
            .unwrap_or_else(|e| panic!("{tag}: source: {e}"));
        if consumer.is_none() {
            consumer = Some(start_consumer(&sink, (contract.width, contract.height)));
        }
        let seen0 = at_sink.load(Ordering::Relaxed);
        let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
        let deadline = Instant::now() + Duration::from_millis(1_500);
        while Instant::now() < deadline && out_log.errors.is_empty() {
            out_log.drain(output.pipeline());
            in_log.drain(ingest.pipeline());
            std::thread::sleep(Duration::from_millis(20));
        }
        let counts = output.counts();
        let seen = at_sink.load(Ordering::Relaxed) - seen0;
        ingest.stop().expect("source stop");
        output.stop().expect("output stop");
        assert!(
            out_log.errors.is_empty() && !out_log.eos,
            "{tag}: {out_log:?}"
        );
        assert!(in_log.errors.is_empty(), "{tag}: {in_log:?}");
        assert!(seen >= 30, "{tag}: only {seen} frames reached the loopback");
        eprintln!("{tag}: {seen} frames to the loopback; {counts:?}");
        if let Some(b) = before {
            for (what, now, then) in [
                ("fresh", counts.fresh, b.fresh),
                ("repeat", counts.repeat, b.repeat),
                ("placeholder", counts.placeholder, b.placeholder),
                ("superseded", counts.superseded, b.superseded),
                ("late", counts.late, b.late),
                ("skipped", counts.skipped, b.skipped),
                ("emitted", counts.emitted, b.emitted),
                ("input_frames", counts.input_frames, b.input_frames),
            ] {
                assert!(now >= then, "{tag}: {what} went back: {then} -> {now}");
            }
            assert!(
                counts.fresh > b.fresh,
                "{tag}: no fresh frame after the restart"
            );
            // 30 fps into 30 fps: a frame or two superseded at most, not
            // every frame written before the restart.
            assert!(
                counts.superseded - b.superseded <= 5,
                "{tag}: superseded jumped {} -> {}",
                b.superseded,
                counts.superseded
            );
        }
        before = Some(counts);
    }
    if let Some(c) = consumer {
        c.shutdown().expect("consumer NULL");
    }
}

/// GPU only: a clock thread that dies (here: a panicking emit observer)
/// stops emitting, so the output reports it: `is_running` turns false,
/// `health` returns the panic message, and so does `stop`.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_dead_output_clock_is_reported() {
    init();
    require_elements(&["cudaupload", "cudadownload", "capssetter", "fakesink"]);
    let mut output = Output::build(OutputConfig::new(
        OutputContract::default(),
        OutputSink::Fake,
    ))
    .expect("build output");
    let ticks = Arc::new(AtomicU64::new(0));
    let t = Arc::clone(&ticks);
    output.set_observer(Some(Arc::new(move |e: &EmitInfo| {
        if t.fetch_add(1, Ordering::Relaxed) == 5 {
            panic!("observer failure at tick {}", e.tick);
        }
    })));
    output.start().expect("start output");
    let deadline = Instant::now() + Duration::from_secs(5);
    while output.is_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!output.is_running(), "the dead clock went unnoticed");
    let health = output
        .health()
        .expect_err("health must report the dead clock");
    assert!(
        health.to_string().contains("observer failure at tick 5"),
        "{health}"
    );
    let stopped = output.stop().expect_err("stop must report the dead clock");
    assert!(
        stopped.to_string().contains("observer failure at tick 5"),
        "{stopped}"
    );
    output.health().expect("a stopped output is healthy");
    assert_eq!(ticks.load(Ordering::Relaxed), 6, "no tick after the panic");
}

/// GPU only: a switch whose new source fails to start leaves the session
/// without a source, and `last_switch` records the attempt with its error.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_failed_switch_is_recorded_with_its_error() {
    init();
    require_elements(&[
        "videotestsrc",
        "filesrc",
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
    ]);
    let mut cfg = OutputConfig::new(OutputContract::default(), OutputSink::Fake);
    cfg.hold_timeout = Duration::from_secs(1);
    let mut output = Output::build(cfg).expect("build output");
    let trace = trace_output(&mut output);
    let mut session = Session::new(output).expect("start output");
    let first = test_pattern_ingest(session.output(), 30, -1);
    let first_id = first.source_id();
    session.switch_to(first).expect("first source");
    let ok = session.last_switch().unwrap();
    assert_eq!((ok.source_id, ok.error.as_deref()), (first_id, None));
    assert!(ok.started.is_some());
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    assert!(run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(10),
        |s| s.output().counts().fresh > 10
    ));

    // A source that cannot open: a file that does not exist.
    let src = gst::ElementFactory::make("filesrc")
        .property("location", "/nonexistent/obc-test-source")
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            "video/x-raw,format=NV12,width=640,height=480,framerate=30/1"
                .parse::<gst::Caps>()
                .unwrap(),
        )
        .build()
        .unwrap();
    let broken = Ingest::from_head(
        vec![src, caps],
        "NV12",
        (640, 480),
        AspectPolicy::Crop,
        session.output().frame_writer(),
    )
    .expect("build the broken source");
    let broken_id = broken.source_id();
    let err = session
        .switch_to(broken)
        .expect_err("a source that cannot open must fail to start");
    let failed = session.last_switch().expect("the attempt is recorded");
    let no_source = session.ingest().is_none();
    // The hold armed for the broken source runs to its timeout (1 s), then
    // placeholders follow.
    std::thread::sleep(Duration::from_millis(2_000));
    let counts = session.output().counts();
    session.stop().expect("teardown");
    assert_eq!(
        (counts.holds, counts.holds_released, counts.holds_expired),
        (1, 0, 1),
        "{counts:?}"
    );
    let t = trace.lock().unwrap();
    let last_fresh = t
        .emitted
        .iter()
        .rposition(|e| e.kind == TickKind::Fresh)
        .expect("frames of the first source");
    let after: Vec<&EmitInfo> = t.emitted[last_fresh + 1..].iter().collect();
    let held = after
        .iter()
        .take_while(|e| e.kind == TickKind::Repeat)
        .count();
    assert!(
        held > 0,
        "no repeat after the last fresh frame: {:?}",
        after.first()
    );
    let held_for = after[held - 1].emitted_at - t.emitted[last_fresh].emitted_at;
    eprintln!("failed switch: {held} repeats over {held_for:?}, then placeholders");
    // Past the 500 ms gap threshold, up to the 1 s hold.
    assert!(
        held_for > Duration::from_millis(700) && held_for < Duration::from_millis(1_100),
        "{held_for:?}"
    );
    assert!(
        after[held..]
            .iter()
            .all(|e| e.kind == TickKind::Placeholder)
    );
    assert!(after.len() - held > 15);

    assert!(no_source, "the session should be left without a source");

    assert_eq!(failed.source_id, broken_id);
    assert_eq!(failed.error.as_deref(), Some(err.to_string().as_str()));
    assert!(failed.started.is_none() && failed.first_fresh.is_none());
    assert!(
        failed.previous_stopped.is_some(),
        "the first source was stopped"
    );
}

/// GPU only: a switch to a source that takes 1 s to deliver its first
/// frame (twice the 500 ms gap threshold) holds the previous source's last
/// frame throughout: no placeholder, white repeats until the new source's
/// first frame.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_switch_holds_the_last_frame_until_the_new_source() {
    init();
    require_elements(&[
        "videotestsrc",
        "queue",
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
    ]);
    let mut output = Output::build(OutputConfig::new(
        OutputContract::default(),
        OutputSink::Fake,
    ))
    .expect("build output");
    let trace = trace_output(&mut output);
    let mut session = Session::new(output).expect("start output");
    let first = test_pattern_ingest(session.output(), 30, -1);
    session.switch_to(first).expect("first source");
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    assert!(run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(10),
        |s| s.output().counts().fresh > 30
    ));
    let slow = pattern_ingest(
        session.output(),
        30,
        -1,
        "black",
        Some(Duration::from_millis(1_000)),
    );
    let slow_id = slow.source_id();
    session.switch_to(slow).expect("switch");
    assert!(run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(5),
        |s| s.output().first_fresh(slow_id).is_some()
    ));
    run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_millis(500),
        |_| false,
    );
    let timing = session.last_switch().unwrap();
    let counts = session.output().counts();
    session.stop().expect("teardown");
    assert!(
        out_log.errors.is_empty() && in_log.errors.is_empty(),
        "{out_log:?} {in_log:?}"
    );

    let t = trace.lock().unwrap();
    let first_new = t
        .emitted
        .iter()
        .position(|e| e.origin.is_some_and(|o| o.source_id == slow_id))
        .unwrap();
    let last_old = t.emitted[..first_new]
        .iter()
        .rposition(|e| e.kind == TickKind::Fresh)
        .unwrap();
    let held = t.emitted[first_new].emitted_at - t.emitted[last_old].emitted_at;
    eprintln!(
        "{counts:?}; switch gap {:?}, last frame held {held:?} over {} ticks",
        timing.gap(),
        first_new - last_old - 1
    );
    assert_eq!(
        (counts.holds, counts.holds_released, counts.holds_expired),
        (1, 1, 0)
    );
    assert!(held > Duration::from_millis(900), "held only {held:?}");
    let gap = &t.emitted[last_old + 1..first_new];
    assert!(gap.iter().all(|e| e.kind == TickKind::Repeat), "{gap:?}");
    assert_eq!(t.emitted[first_new].kind, TickKind::Fresh);
    // At the sink: the old (white) frame during the hold, never black.
    let luma_at = |pts: u64| {
        t.centre_luma
            .iter()
            .find(|(p, _)| *p == pts)
            .map(|(_, v)| *v)
    };
    let mut checked = 0;
    for e in gap {
        if let Some(v) = luma_at(e.pts_ns) {
            assert!(v.abs_diff(235) <= 3, "luma {v} during the hold");
            checked += 1;
        }
    }
    // The sink saw the held frames (not only the clock's decisions).
    assert!(
        checked > 0 && checked * 2 >= gap.len(),
        "luma recorded for {checked} of {} held ticks",
        gap.len()
    );
}

/// GPU only: the real output behind a sink that needs 100 ms per frame (a
/// third of the contract rate): the `appsrc` queue never exceeds its bound
/// (3), the frames it drops are counted, the clock neither skips nor falls
/// behind, and no frame reaches the sink later than about the bound times
/// the sink's service time after its emission.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_slow_sink_never_queues_more_than_the_bound() {
    init();
    require_elements(&[
        "videotestsrc",
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
        "fakesink",
    ]);
    const SERVICE: Duration = Duration::from_millis(100);
    let cfg = OutputConfig::new(OutputContract::default(), OutputSink::Fake);
    let bound = u64::from(cfg.queue_buffers);
    let mut output = Output::build(cfg).expect("build output");
    let emitted: Arc<Mutex<Vec<(u64, Instant)>>> = Arc::default();
    let e = Arc::clone(&emitted);
    output.set_observer(Some(Arc::new(move |i: &EmitInfo| {
        e.lock().unwrap().push((i.pts_ns, i.emitted_at));
    })));
    let ages: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let sink = output.pipeline().element(names::LOOPBACK).unwrap();
    sink.set_property("signal-handoffs", true);
    let (e, a) = (Arc::clone(&emitted), Arc::clone(&ages));
    sink.connect("handoff", false, move |args| {
        let buffer = args[1].get::<gst::Buffer>().unwrap();
        let now = Instant::now();
        if let Some(pts) = buffer.pts().map(|p| p.nseconds())
            && let Some((_, at)) = e.lock().unwrap().iter().rev().find(|(p, _)| *p == pts)
        {
            a.lock().unwrap().push(now - *at);
        }
        std::thread::sleep(SERVICE);
        None
    });
    let mut session = Session::new(output).expect("start output");
    session
        .switch_to(test_pattern_ingest(session.output(), 30, -1))
        .expect("source");
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(4),
        |_| false,
    );
    let c = session.output().counts();
    session.stop().expect("teardown");
    let ages = ages.lock().unwrap().clone();
    let oldest = ages.iter().max().copied().unwrap_or_default();
    eprintln!(
        "slow sink: emitted {} queue dropped {} queue level max {} skipped {} late {}; {} \
         frames at the sink, oldest {oldest:?} after its emission",
        c.emitted,
        c.queue_dropped,
        c.queue_level_max,
        c.skipped,
        c.late,
        ages.len()
    );
    assert!(out_log.errors.is_empty(), "{out_log:?}");
    assert!(c.queue_level_max <= bound, "{c:?}");
    assert!(c.queue_dropped > c.emitted / 2, "{c:?}");
    assert_eq!((c.skipped, c.push_failed), (0, 0), "{c:?}");
    assert!(ages.len() >= 30, "{} frames reached the sink", ages.len());
    let limit = SERVICE * u32::try_from(bound).unwrap() + Duration::from_millis(60);
    assert!(oldest <= limit, "{oldest:?} (limit {limit:?})");
}

/// One line of the consumer process (`obc-consumer`, see its docs).
#[derive(Debug, Clone, PartialEq)]
enum ConsumerLine {
    Caps(String),
    /// Luma mean, min and max (`None` for a format without a luma plane),
    /// and the frame's stamp ([`obc_pipeline::stamp`]), if it has one.
    Frame(Option<(f64, u8, u8)>, Option<u64>),
    Error(String),
    Eos,
    Other(String),
}

impl ConsumerLine {
    fn parse(line: &str) -> Self {
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "caps" => ConsumerLine::Caps(rest.to_string()),
            "frame" => {
                let f: Vec<&str> = rest.split(' ').collect();
                let (luma, stamp) = match f.as_slice() {
                    [_, mean, min, max, stamp] => (
                        (|| Some((mean.parse().ok()?, min.parse().ok()?, max.parse().ok()?)))(),
                        stamp.parse().ok(),
                    ),
                    _ => (None, None),
                };
                ConsumerLine::Frame(luma, stamp)
            }
            "error" => ConsumerLine::Error(rest.to_string()),
            "eos" => ConsumerLine::Eos,
            _ => ConsumerLine::Other(line.to_string()),
        }
    }
}

/// A frame the consumer received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    at: Instant,
    /// Every sampled luma value is 16: black (limited range).
    black: bool,
    /// It carries a stamp: the test marked it on its way out of the output
    /// (a forwarded frame, not a writer placeholder).
    stamped: bool,
}

/// A loopback consumer in a separate process: the `obc-consumer` binary of
/// this crate, `v4l2src device=<loopback> ! appsink`, reporting every caps
/// negotiation and every frame on stdout, each line stamped here on
/// arrival. Killed and reaped when dropped, also when the test fails.
struct ConsumerProcess {
    child: std::process::Child,
    lines: Arc<Mutex<Vec<(Instant, ConsumerLine)>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl ConsumerProcess {
    fn spawn(device: &std::path::Path) -> Self {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let mut child = Command::new(env!("CARGO_BIN_EXE_obc-consumer"))
            .arg(device)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the obc-consumer process");
        let stdout = child.stdout.take().unwrap();
        let lines: Arc<Mutex<Vec<(Instant, ConsumerLine)>>> = Arc::default();
        let l = Arc::clone(&lines);
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                l.lock()
                    .unwrap()
                    .push((Instant::now(), ConsumerLine::parse(&line)));
            }
        });
        ConsumerProcess {
            child,
            lines,
            reader: Some(reader),
        }
    }

    /// A consumer that has negotiated caps and received a frame: respawned
    /// (with `exclusive_caps` the capture side can lag the writer's start)
    /// until then, for at most `limit`.
    fn start(device: &std::path::Path, limit: Duration) -> Self {
        let deadline = Instant::now() + limit;
        loop {
            let mut c = Self::spawn(device);
            let ok = loop {
                let lines = c.lines();
                if lines.iter().any(|l| matches!(l.1, ConsumerLine::Frame(..))) {
                    break true;
                }
                let failed = lines.iter().any(|l| matches!(l.1, ConsumerLine::Error(_)))
                    || c.child.try_wait().ok().flatten().is_some();
                if failed || Instant::now() > deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            if ok {
                return c;
            }
            let lines = c.lines();
            drop(c);
            eprintln!("consumer attempt failed, retrying: {lines:?}");
            assert!(
                Instant::now() < deadline,
                "the consumer process never received a frame from {}: {lines:?}",
                device.display()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn lines(&self) -> Vec<(Instant, ConsumerLine)> {
        self.lines.lock().unwrap().clone()
    }

    /// Every frame received.
    fn frames(&self) -> Vec<Seen> {
        self.lines()
            .into_iter()
            .filter_map(|(at, l)| match l {
                ConsumerLine::Frame(luma, stamp) => Some(Seen {
                    at,
                    black: matches!(luma, Some((_, 16, 16))),
                    stamped: stamp.is_some(),
                }),
                _ => None,
            })
            .collect()
    }

    fn caps(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter_map(|(_, l)| match l {
                ConsumerLine::Caps(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// Lines other than caps and frames (errors, end of stream, other).
    fn problems(&self) -> Vec<ConsumerLine> {
        self.lines()
            .into_iter()
            .map(|(_, l)| l)
            .filter(|l| !matches!(l, ConsumerLine::Caps(_) | ConsumerLine::Frame(..)))
            .collect()
    }

    /// Waits until `done` holds of the frames received, for at most
    /// `limit`; returns whether it did.
    fn wait_frames(&self, limit: Duration, mut done: impl FnMut(&[Seen]) -> bool) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if done(&self.frames()) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ConsumerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

/// The runs of unstamped (`false`) and stamped (`true`) frames, in order:
/// `[(false, _), (true, _), (false, _)]` for writer placeholders, then
/// forwarded frames, then placeholders.
fn runs(frames: &[Seen]) -> Vec<(bool, usize)> {
    let mut out: Vec<(bool, usize)> = Vec::new();
    for s in frames {
        match out.last_mut() {
            Some((last, n)) if *last == s.stamped => *n += 1,
            _ => out.push((s.stamped, 1)),
        }
    }
    out
}

/// Stamps every buffer reaching `element`'s sink pad in `p` with its PTS
/// ([`obc_pipeline::stamp`], the first 16 luma bytes), so a consumer can
/// tell it from a writer placeholder whatever the picture.
fn stamp_buffers(p: &MediaPipeline, element: &str) {
    p.element(element)
        .and_then(|e| e.static_pad("sink"))
        .expect("sink pad")
        .add_probe(gst::PadProbeType::BUFFER, |_, info| {
            if let Some(buffer) = info.buffer_mut() {
                let pts = buffer.pts().map_or(0, |p| p.nseconds());
                if let Ok(mut map) = buffer.make_mut().map_writable() {
                    obc_pipeline::stamp::write_stamp(map.as_mut_slice(), pts);
                }
            }
            gst::PadProbeReturn::Ok
        });
}

/// Whether the `nvcodec` GStreamer plugin (the CUDA elements) is loaded in
/// this process.
fn nvcodec_loaded() -> bool {
    gst::Registry::get()
        .find_plugin("nvcodec")
        .is_some_and(|p| p.is_loaded())
}

/// The pids `nvidia-smi` lists with a compute context.
fn cuda_pids() -> Vec<u32> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-compute-apps=pid", "--format=csv,noheader"])
        .output()
        .expect("this test needs nvidia-smi (it comes with the NVIDIA driver)");
    assert!(out.status.success(), "nvidia-smi failed: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Whether this process holds `node` open.
fn holds_open(node: &std::path::Path) -> bool {
    let node = std::fs::canonicalize(node).unwrap_or_else(|_| node.to_path_buf());
    std::fs::read_dir("/proc/self/fd")
        .into_iter()
        .flatten()
        .flatten()
        .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|t| t == node))
}

/// `OBC_TEST_SINK` is required (never the labelled default): these tests
/// write to the device for a while with no camera to show.
fn explicit_test_sink() -> DeviceInfo {
    assert!(
        std::env::var_os("OBC_TEST_SINK").is_some(),
        "this test needs OBC_TEST_SINK=/dev/videoN, a free v4l2loopback device created with \
         exclusive_caps=1 (it never writes to a device found by its label alone)"
    );
    test_sink()
}

/// The loopback writer alone (the daemon idle): while it streams
/// placeholders, the loopback, which is not a capture device before (with
/// `exclusive_caps=1`, without a writer), is one, a consumer in another
/// process negotiates once and receives only black frames of the contract,
/// and the writer counts placeholders only. After it stops (and the
/// consumer has closed), the device is no capture device again. CPU only:
/// no camera, no GPU. If no earlier test in this process loaded the CUDA
/// plugin, the writer must not have loaded it either.
#[test]
#[ignore = "needs OBC_TEST_SINK (a free v4l2loopback device with exclusive_caps=1)"]
fn the_writer_alone_is_a_capture_device_with_black_frames() {
    use obc_pipeline::device::CAP_VIDEO_CAPTURE;
    use obc_pipeline::discovery::query_cap;
    use obc_pipeline::{LoopbackWriter, WriterSink};
    init();
    require_elements(&["appsrc", "v4l2sink", "v4l2src", "appsink"]);
    let sink = explicit_test_sink();
    let capture = || {
        query_cap(&sink.path)
            .unwrap_or_else(|| panic!("VIDIOC_QUERYCAP failed on {}", sink.path.display()))
            .caps
            & CAP_VIDEO_CAPTURE
            != 0
    };
    assert!(
        !capture(),
        "{} is already a capture device before the writer streams: it needs exclusive_caps=1 \
         and no other writer (check `fuser -v {}`)",
        sink.path.display(),
        sink.path.display()
    );
    let nvcodec_before = nvcodec_loaded();
    let contract = OutputContract::default();
    let mut writer = LoopbackWriter::build(contract, WriterSink::Loopback(sink.path.clone()))
        .expect("build the writer");
    let sink_caps_events = count_caps_events(writer.pipeline(), names::LOOPBACK);
    writer.start().expect("start the writer");
    // The device turns capture once the first frame reaches v4l2sink.
    let started = Instant::now();
    let visible_after = loop {
        if capture() {
            break Some(started.elapsed());
        }
        if started.elapsed() > Duration::from_secs(5) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let consumer = ConsumerProcess::start(&sink.path, Duration::from_secs(10));
    assert!(
        consumer.wait_frames(Duration::from_secs(10), |f| f.len() >= 60),
        "the consumer received {} frames",
        consumer.frames().len()
    );
    writer.health().expect("writer health");
    let counts = writer.counts();
    let (frames, caps, problems) = (consumer.frames(), consumer.caps(), consumer.problems());
    drop(consumer);
    writer.stop().expect("stop the writer");
    let hidden = !capture();
    eprintln!(
        "writer alone: a capture device {visible_after:?} after the start; {} frames at the \
         consumer, caps {caps:?}; {counts:?}",
        frames.len()
    );
    assert!(
        visible_after.is_some(),
        "not a capture device within 5 s of the writer's start"
    );
    assert!(problems.is_empty(), "consumer: {problems:?}");
    assert_eq!(caps.len(), 1, "consumer caps negotiations: {caps:?}");
    for want in ["NV12", "1920", "1080", "30/1"] {
        assert!(caps[0].contains(want), "{caps:?}");
    }
    assert_eq!(sink_caps_events.load(Ordering::Relaxed), 1);
    assert!(
        frames.iter().all(|f| f.black && !f.stamped),
        "not every frame was an unstamped black frame: {:?}",
        frames.iter().find(|f| !f.black || f.stamped)
    );
    assert!(counts.placeholder >= 60, "{counts:?}");
    assert_eq!((counts.forwarded, counts.push_failed), (0, 0), "{counts:?}");
    assert!(hidden, "still a capture device after the writer stopped");
    if nvcodec_before {
        eprintln!(
            "the CUDA plugin was loaded by an earlier test: its check is not meaningful here"
        );
    } else {
        assert!(!nvcodec_loaded(), "the writer loaded the CUDA plugin");
    }
}

/// The core invariant of the daemon (ADR-0011): a consumer in another
/// process that holds the loopback across Idle -> Active -> Idle sees one
/// format and an uninterrupted stream: black placeholders from the writer,
/// then the frames a CUDA session's output forwards from the camera, then
/// black placeholders again once the session has stopped (its camera
/// closed, its CUDA context released: this process is no longer listed by
/// `nvidia-smi`, as it is while the session runs).
///
/// The output's frames are stamped on their way into its forwarding
/// `appsink` ([`obc_pipeline::stamp`], 16 luma bytes), so the consumer
/// tells them from the writer's placeholders whatever the camera shows (a
/// camera with its privacy shutter closed, or in the dark, delivers black
/// too). Asserted: the consumer and the writer's sink each negotiate caps
/// exactly once; the consumer's frames are one run of placeholders (all
/// black), one of forwarded frames, one of placeholders (all black);
/// neither the sink nor the consumer goes longer than three output periods
/// plus 30 ms without a frame from the consumer's first frame to the end;
/// while the session runs, the writer pushes no placeholder; the output
/// emitted fresh camera frames. Whether the forwarded frames show a
/// picture (not black) is printed, not asserted: a camera with its shutter
/// closed or in the dark delivers black. No camera control is changed.
#[test]
#[ignore = "needs OBC_TEST_CAMERA, OBC_TEST_SINK (a free v4l2loopback device) and an NVIDIA GPU"]
fn a_consumer_keeps_one_stream_across_idle_active_idle() {
    use obc_pipeline::{LoopbackWriter, WriterSink};
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    require_elements(&[
        "appsrc",
        "v4l2sink",
        "v4l2src",
        "appsink",
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
        "capssetter",
    ]);
    let sink = explicit_test_sink();
    let (camera, choice) = select_with(&test_camera(), &test_mode(), DiscoveryRoute::Auto);
    let contract = OutputContract::default();
    let period = Duration::from_nanos(contract.frame_duration_ns());
    let pid = std::process::id();

    let mut writer = LoopbackWriter::build(contract, WriterSink::Loopback(sink.path.clone()))
        .expect("build the writer");
    let sink_caps_events = count_caps_events(writer.pipeline(), names::LOOPBACK);
    let sink_arrivals = record_arrival_times(writer.pipeline(), names::LOOPBACK);
    writer.start().expect("start the writer");
    let consumer = ConsumerProcess::start(&sink.path, Duration::from_secs(10));
    assert!(
        consumer.wait_frames(Duration::from_secs(10), |f| f.len() >= 30),
        "idle: the consumer received {} frames",
        consumer.frames().len()
    );
    assert!(
        !cuda_pids().contains(&pid),
        "a CUDA context before the session"
    );

    // Active: a CUDA session whose output forwards to the writer.
    let output = Output::build(OutputConfig::new(
        contract,
        OutputSink::Writer(writer.handle()),
    ))
    .expect("build the output");
    stamp_buffers(output.pipeline(), names::LOOPBACK);
    let mut session = Session::new(output).expect("start the output");
    // The default: no camera control is changed.
    let cfg = IngestConfig::new(camera.path.clone(), choice.mode);
    session.switch_to_camera(&cfg).expect("start the camera");
    let source = session.ingest().unwrap().source_id();
    let forwarded = |f: &[Seen]| f.iter().filter(|x| x.stamped).count();
    let streaming = consumer.wait_frames(Duration::from_secs(20), |f| {
        forwarded(f) >= 60 && session.output().first_fresh(source).is_some()
    });
    assert!(
        streaming,
        "active: the consumer received {} forwarded frames (last lines {:?}); writer {:?}, \
         output {:?}, ingest failure {:?}",
        forwarded(&consumer.frames()),
        consumer.lines().iter().rev().take(3).collect::<Vec<_>>(),
        writer.counts(),
        session.output().counts(),
        session.ingest_failure()
    );
    let active = writer.counts();
    let fresh0 = session.output().counts().fresh;
    std::thread::sleep(Duration::from_secs(1));
    let active_after = writer.counts();
    let fresh1 = session.output().counts().fresh;
    let cuda_while_active = cuda_pids().contains(&pid);
    let failure = session.ingest_failure();
    let stopped_at = Instant::now();
    session.stop().expect("stop the session");
    let camera_closed = !holds_open(&camera.path);
    let deadline = Instant::now() + Duration::from_secs(5);
    let cuda_released = loop {
        if !cuda_pids().contains(&pid) {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let black_again = consumer.wait_frames(Duration::from_secs(10), |f| {
        f.iter().filter(|x| x.at > stopped_at && !x.stamped).count() >= 30
    });
    let end = Instant::now();
    writer.health().expect("writer health");
    let counts = writer.counts();
    let (frames, caps, problems) = (consumer.frames(), consumer.caps(), consumer.problems());
    drop(consumer);
    writer.stop().expect("stop the writer");

    let limit = 3 * period + Duration::from_millis(30);
    let from = frames.first().map(|f| f.at).expect("frames");
    let arrivals: Vec<Instant> = frames.iter().map(|f| f.at).collect();
    let consumer_silence = longest_silence(&arrivals, from, end);
    let sink_silence = longest_silence(&sink_arrivals.lock().unwrap(), from, end);
    let pictures = frames.iter().filter(|f| f.stamped && !f.black).count();
    eprintln!(
        "idle -> active -> idle: consumer runs (stamped, count) {:?}, {pictures} forwarded \
         frames with a picture, caps {caps:?}; longest interval without a frame: consumer \
         {consumer_silence:?}, writer sink {sink_silence:?} (limit {limit:?}); writer {counts:?}",
        runs(&frames)
    );
    if pictures == 0 {
        eprintln!(
            "the camera delivered only black frames (privacy shutter closed, or a dark room): \
             the forwarded frames were told from the placeholders by their stamp alone"
        );
    }
    assert!(problems.is_empty(), "consumer: {problems:?}");
    assert_eq!(failure, None, "the camera failed while active");
    assert!(black_again, "no placeholders after the session stopped");
    assert!(
        cuda_while_active,
        "no CUDA context listed while active (control)"
    );
    assert!(
        camera_closed,
        "the camera is still open after the session stopped"
    );
    assert!(cuda_released, "the CUDA context outlived the session");
    assert_eq!(caps.len(), 1, "consumer caps negotiations: {caps:?}");
    assert_eq!(
        sink_caps_events.load(Ordering::Relaxed),
        1,
        "the writer's sink negotiated more than once"
    );
    let r = runs(&frames);
    assert_eq!(
        r.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![false, true, false],
        "placeholders, forwarded, placeholders: {r:?}"
    );
    assert!(
        frames.iter().filter(|f| !f.stamped).all(|f| f.black),
        "a placeholder that is not black"
    );
    assert!(fresh1 > fresh0, "no fresh camera frame while active");
    assert_eq!(
        active_after.placeholder, active.placeholder,
        "placeholders while the session forwarded"
    );
    assert!(
        active_after.forwarded >= active.forwarded + 25,
        "{active_after:?}"
    );
    assert_eq!(counts.push_failed, 0, "{counts:?}");
    assert!(
        consumer_silence <= limit,
        "consumer silent {consumer_silence:?}"
    );
    assert!(sink_silence <= limit, "writer sink silent {sink_silence:?}");
}

/// A PNG background of `width` x `height` (a test pattern) in `dir`.
fn background_png(dir: &std::path::Path, width: u32, height: u32) -> std::path::PathBuf {
    let path = dir.join("background.png");
    let p = gst::parse::launch(&format!(
        "videotestsrc num-buffers=1 pattern=smpte ! video/x-raw,width={width},height={height} \
         ! pngenc snapshot=true ! filesink location={}",
        path.display()
    ))
    .unwrap();
    p.set_state(gst::State::Playing).unwrap();
    let m = p
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("the background was written");
    assert_eq!(m.type_(), gst::MessageType::Eos, "{m:?}");
    p.set_state(gst::State::Null).unwrap();
    path
}

/// Waits (draining both buses) until the output's effect is ready; panics
/// with the status otherwise.
fn wait_effect_ready(
    session: &Session,
    out_log: &mut BusLog,
    in_log: &mut BusLog,
    timeout: Duration,
) {
    let ready = run_until(session, out_log, in_log, timeout, |s| {
        matches!(
            s.output().effect_status(),
            EffectStatus::Ready | EffectStatus::Failed(_)
        )
    });
    assert!(
        ready && session.output().effect_status() == EffectStatus::Ready,
        "effect {:?}; {out_log:?} {in_log:?}",
        session.output().effect_status()
    );
}

/// GPU only, fake effect backend: an output built with the effect element
/// switches none -> blur -> replace -> none while a source streams. The
/// sink sees the source (luma 235), black (16) only while the first effect
/// loads, the fake blur (200) and replace (100), never the source while an
/// effect is requested, and one caps event in all.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_an_output_switches_effects_while_streaming() {
    use fake_backend::{FAKE_BLUR_Y, FAKE_REPLACE_Y, FakeConfig, FakeFactory, FakeLog};
    init();
    require_elements(&[
        "videotestsrc",
        "cudaupload",
        "cudaconvertscale",
        "videocrop",
        "cudadownload",
        "pngenc",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let bg = background_png(dir.path(), 640, 480);
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(800),
        switch_delay: Duration::from_millis(20),
        ..FakeConfig::default()
    });
    let mut cfg = OutputConfig::new(OutputContract::default(), OutputSink::Fake);
    cfg.effect = Some(EffectSettings::default());
    cfg.effect_backend = EffectBackend::new(fake.clone());
    let mut output = Output::build(cfg).expect("build output");
    assert!(output.pipeline().element(names::EFFECT).is_some());
    let trace = trace_output(&mut output);
    let caps_events = count_caps_events(output.pipeline(), names::LOOPBACK);
    let mut session = Session::new(output).expect("start output");
    session
        .switch_to(test_pattern_ingest(session.output(), 30, 30 * 30))
        .expect("start source");
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    assert!(run_until(
        &session,
        &mut out_log,
        &mut in_log,
        Duration::from_secs(10),
        |s| s.output().counts().fresh > 10
    ));
    let len = || trace.lock().unwrap().centre_luma.len();
    let lumas = |from: usize, to: usize| -> Vec<u8> {
        trace.lock().unwrap().centre_luma[from..to]
            .iter()
            .map(|(_, v)| *v)
            .collect()
    };
    let idle = |session: &Session, out_log: &mut BusLog, in_log: &mut BusLog, ms: u64| {
        run_until(session, out_log, in_log, Duration::from_millis(ms), |_| {
            false
        });
    };
    idle(&session, &mut out_log, &mut in_log, 500);
    let none_end = len();
    assert!(
        lumas(none_end - 10, none_end).iter().all(|v| *v == 235),
        "{:?}",
        lumas(none_end - 10, none_end)
    );

    // none -> blur.
    session
        .output()
        .set_effect(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    let a = len();
    wait_effect_ready(&session, &mut out_log, &mut in_log, Duration::from_secs(10));
    let b = len();
    idle(&session, &mut out_log, &mut in_log, 500);
    let c = len();
    let warm = lumas(a, b);
    let first_black = warm.iter().position(|v| *v == 16).expect("black warm-up");
    assert!(first_black <= 1, "at most the frame in flight: {warm:?}");
    assert!(
        warm[first_black..]
            .iter()
            .all(|v| *v == 16 || *v == FAKE_BLUR_Y),
        "{warm:?}"
    );
    assert!(
        warm.len() >= 15,
        "the 800 ms load is about 24 frames: {warm:?}"
    );
    assert!(lumas(b, c).iter().all(|v| *v == FAKE_BLUR_Y));

    // blur -> replace.
    session
        .output()
        .set_effect(&EffectSettings {
            background: Some(bg.clone()),
            ..EffectSettings::of(EffectKind::Replace)
        })
        .unwrap();
    let d = len();
    wait_effect_ready(&session, &mut out_log, &mut in_log, Duration::from_secs(10));
    idle(&session, &mut out_log, &mut in_log, 500);
    let e = len();
    let switch = lumas(d, e);
    let blur_run = switch.iter().take_while(|v| **v == FAKE_BLUR_Y).count();
    assert!(
        switch[blur_run..].iter().all(|v| *v == FAKE_REPLACE_Y) && blur_run < switch.len(),
        "{switch:?}"
    );

    // replace -> none: the source at once (after the frame in flight).
    session
        .output()
        .set_effect(&EffectSettings::default())
        .unwrap();
    let f = len();
    idle(&session, &mut out_log, &mut in_log, 500);
    let g = len();
    let back = lumas(f, g);
    let first_input = back.iter().position(|v| *v == 235).expect("the source");
    assert!(first_input <= 1, "{back:?}");
    assert!(back[first_input..].iter().all(|v| *v == 235), "{back:?}");
    assert_eq!(session.output().effect_status(), EffectStatus::Off);

    let stats = session.output().effect_stats().unwrap();
    let counts = session.output().counts();
    session.stop().expect("teardown");
    assert!(
        out_log.errors.is_empty() && in_log.errors.is_empty(),
        "{out_log:?} {in_log:?}"
    );
    eprintln!("{stats:?}; {counts:?}");
    assert_eq!(caps_events.load(Ordering::Relaxed), 1, "one negotiation");
    assert!(stats.frames_processed > 20 && stats.frames_black >= 15);
    assert!(stats.frames_passthrough > 20);
    assert_eq!(counts.queue_dropped, 0);
    assert_eq!(FakeLog::get(&fake.log.wrong_context), 0);
    assert_eq!(FakeLog::get(&fake.log.wrong_stream), 0);
    assert_eq!(
        FakeLog::get(&fake.log.drops),
        1,
        "the instance went with the output"
    );
}

/// `nvidia-smi`'s compute processes with their used memory in MiB.
fn cuda_apps() -> Vec<(u32, u64)> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .expect("this test needs nvidia-smi (it comes with the NVIDIA driver)");
    assert!(out.status.success(), "nvidia-smi failed: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split(',').map(str::trim);
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect()
}

/// End to end with the Maxine backend: camera -> session -> output with the
/// effect blur from the start, then replace, then none. Every phase keeps
/// the contract rate at the sink; the effect becomes ready, its statistics
/// are sane, and after the output stops this process holds no CUDA context
/// (so its GPU memory is back).
///
/// Run with the SDK set up and the os-release shim (`make -C
/// tools/os-release-shim`), e.g. `source tools/maxine-probe/env.sh;
/// OMA_BROADCAST_OS_RELEASE=$PWD/tools/maxine-probe/fake-os-release
/// CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env
/// LD_PRELOAD=$PWD/target/os-release-shim/os-release-shim.so" cargo test -p
/// obc-pipeline --test hardware -- --ignored maxine_ --test-threads=1`.
#[test]
#[ignore = "needs OBC_TEST_CAMERA, an NVIDIA GPU and the NVIDIA Maxine VFX SDK with the os-release override"]
fn maxine_effects_end_to_end_keep_the_rate_and_release_the_gpu() {
    init();
    let _g = CAMERA.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(reason) = obc_pipeline::effect_backend_available() {
        panic!(
            "this test needs the Maxine SDK: {reason} (set MAXINE_VFX_ROOT and source \
             tools/maxine-probe/env.sh)"
        );
    }
    let sel = test_camera();
    let (camera, choice) = select_with(&sel, &test_mode(), DiscoveryRoute::Auto);
    let dir = tempfile::tempdir().unwrap();
    let bg = background_png(dir.path(), 2560, 1440);
    let pid = std::process::id();
    assert!(
        !cuda_apps().iter().any(|(p, _)| *p == pid),
        "this process already holds a CUDA context"
    );
    let contract = OutputContract::default();
    let mut cfg = OutputConfig::new(contract, OutputSink::Fake);
    cfg.effect = Some(EffectSettings::of(EffectKind::Blur));
    let output = Output::build(cfg).expect("build output");
    let arrivals = record_arrival_times(output.pipeline(), names::LOOPBACK);
    let caps_events = count_caps_events(output.pipeline(), names::LOOPBACK);
    let mut session = Session::new(output).expect("start output");
    session
        .switch_to_camera(&IngestConfig::new(camera.path.clone(), choice.mode))
        .expect("start ingest");
    let (mut out_log, mut in_log) = (BusLog::default(), BusLog::default());
    let t_start = Instant::now();
    wait_effect_ready(&session, &mut out_log, &mut in_log, Duration::from_secs(30));
    let ready_after = t_start.elapsed();
    // The effect may be ready before the camera delivers.
    assert!(
        run_until(
            &session,
            &mut out_log,
            &mut in_log,
            Duration::from_secs(15),
            |s| s.output().counts().fresh > 30
        ),
        "no camera frames: {out_log:?} {in_log:?}"
    );

    // Frames at the sink over `secs` seconds, after a settling second.
    let phase = |session: &Session, out_log: &mut BusLog, in_log: &mut BusLog, secs: u64| {
        run_until(session, out_log, in_log, Duration::from_secs(1), |_| false);
        let c0 = session.output().counts();
        let from = Instant::now();
        run_until(session, out_log, in_log, Duration::from_secs(secs), |_| {
            false
        });
        let to = Instant::now();
        let c1 = session.output().counts();
        let n = arrivals
            .lock()
            .unwrap()
            .iter()
            .filter(|t| **t >= from && **t < to)
            .count();
        (
            n,
            c1.placeholder - c0.placeholder,
            c1.queue_dropped - c0.queue_dropped,
        )
    };
    let mut report = Vec::new();
    let (n, ph, dropped) = phase(&session, &mut out_log, &mut in_log, 5);
    let blur_stats = session.output().effect_stats().unwrap();
    let during = cuda_apps();
    report.push(("blur", n, ph, dropped));

    session
        .output()
        .set_effect(&EffectSettings {
            background: Some(bg),
            ..EffectSettings::of(EffectKind::Replace)
        })
        .unwrap();
    wait_effect_ready(&session, &mut out_log, &mut in_log, Duration::from_secs(15));
    let (n, ph, dropped) = phase(&session, &mut out_log, &mut in_log, 5);
    let replace_stats = session.output().effect_stats().unwrap();
    report.push(("replace", n, ph, dropped));

    session
        .output()
        .set_effect(&EffectSettings::default())
        .unwrap();
    let (n, ph, dropped) = phase(&session, &mut out_log, &mut in_log, 5);
    let none_stats = session.output().effect_stats().unwrap();
    report.push(("none", n, ph, dropped));
    session.stop().expect("teardown");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut after = cuda_apps();
    while after.iter().any(|(p, _)| *p == pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        after = cuda_apps();
    }
    eprintln!(
        "ready after {ready_after:?}; phases (effect, sink frames in 5 s, placeholders, \
         queue drops): {report:?}"
    );
    eprintln!("blur: {blur_stats:?}");
    eprintln!("replace: {replace_stats:?}");
    eprintln!("none: {none_stats:?}");
    eprintln!("CUDA apps while streaming {during:?}; after teardown {after:?}");
    assert!(
        out_log.errors.is_empty() && in_log.errors.is_empty(),
        "{out_log:?} {in_log:?}"
    );
    for (name, n, ph, dropped) in &report {
        assert!(
            (147..=153).contains(n),
            "{name}: {n} frames in 5 s at 30 fps"
        );
        assert_eq!(*ph, 0, "{name}: placeholders");
        assert_eq!(*dropped, 0, "{name}: queue drops");
    }
    assert_eq!(caps_events.load(Ordering::Relaxed), 1, "one negotiation");
    for s in [&blur_stats, &replace_stats] {
        assert!(s.frames_processed > 100, "{s:?}");
        let (p50, p95) = (s.gpu_ms_p50.unwrap(), s.gpu_ms_p95.unwrap());
        assert!(p50 > 0.0 && p95 >= p50 && p95 < 20.0, "{s:?}");
        assert!(s.last_load_ms.is_some() && s.cuda_active, "{s:?}");
    }
    assert!(
        replace_stats.last_load_reused,
        "the switch reused the model"
    );
    assert!(none_stats.frames_passthrough > 100);
    assert!(
        during.iter().any(|(p, _)| *p == pid),
        "streaming holds one CUDA context: {during:?}"
    );
    assert!(
        !after.iter().any(|(p, _)| *p == pid),
        "a CUDA context outlived the output: {after:?}"
    );
}
