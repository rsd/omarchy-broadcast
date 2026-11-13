//! A whole session in system memory, through the public API a daemon
//! uses: an output built with `Output::build_in(.., VideoMemory::System)`
//! forwarding to a loopback writer, the effect element with the fake
//! backend of gst-obc's tests (on host frames), and a test-pattern source
//! behind the CPU ingest adapter. No GPU, no camera: the pipelines must
//! hold no element of the `nvcodec` plugin, and the backend must be called
//! on host frames only.
//!
//! The assertions count frames and read their luma; the only times are
//! upper bounds on waiting for something to happen (`docs/testing.md`).

#[path = "../../gst-obc/tests/support/fake_backend.rs"]
mod fake_backend;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_backend::{
    FAKE_BLUR_Y, FAKE_COVERAGE, FAKE_DENOISE_LEVEL, FakeConfig, FakeFactory, FakeLog,
};
use gst::prelude::*;
use obc_core::config::VideoMemory;
use obc_core::{AspectPolicy, OutputContract, PixelFormat};
use obc_pipeline::effect::EffectKind;
use obc_pipeline::pipeline::{ingest_adapter, names};
use obc_pipeline::{
    EffectBackend, EffectSettings, EffectStatus, Ingest, LoopbackWriter, MediaPipeline, Output,
    OutputConfig, OutputSink, PlaceholderState, Session, WriterSink,
};

/// Every element these tests build.
const ELEMENTS: &[&str] = &[
    "videotestsrc",
    "capsfilter",
    "videocrop",
    "videoconvertscale",
    "appsink",
    "appsrc",
    "capssetter",
    "fakesink",
];

fn require_elements(factories: &[&str]) {
    gst::init().expect("gst::init");
    let missing: Vec<&str> = factories
        .iter()
        .copied()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "GStreamer element(s) not installed: {} (GStreamer core, gst-plugins-base and \
         gst-plugins-good, which the tests require)",
        missing.join(", ")
    );
}

/// Fails unless no element of `p` (bins included) comes from the
/// `nvcodec` plugin or is a CUDA element.
fn assert_no_cuda_element(p: &MediaPipeline) {
    for e in p.pipeline().iterate_recurse().into_iter().flatten() {
        // appsrc and appsink are made by their builders, without a factory.
        if let Some(factory) = e.factory() {
            let plugin = factory.plugin_name().map(|n| n.to_string());
            assert_ne!(plugin.as_deref(), Some("nvcodec"), "{}", e.name());
            assert!(!factory.name().starts_with("cuda"), "{}", e.name());
        }
        assert!(!e.type_().name().starts_with("GstCuda"), "{}", e.name());
    }
}

/// A live YUY2 640x480@30 test pattern (white, limited range) behind the
/// CPU ingest adapter of a YUY2 camera, writing into `output`.
fn yuy2_pattern(output: &Output) -> Ingest {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .property_from_str("pattern", "white")
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .name(names::CAMERA_CAPS)
        .property(
            "caps",
            "video/x-raw,format=YUY2,width=640,height=480,framerate=30/1,colorimetry=2:4:5:1"
                .parse::<gst::Caps>()
                .unwrap(),
        )
        .build()
        .unwrap();
    let (adapter, format) = ingest_adapter(
        &caps,
        PixelFormat::Yuy2,
        VideoMemory::System,
        obc_core::ColourRange::Limited,
    )
    .unwrap();
    assert!(adapter.is_empty(), "no separate videoconvert");
    assert_eq!(format, "YUY2");
    let mut head = vec![src, caps];
    head.extend(adapter);
    Ingest::from_head(
        head,
        format,
        (640, 480),
        AspectPolicy::Crop,
        output.frame_writer(),
    )
    .expect("test pattern ingest")
}

/// The centre luma of every buffer reaching the output's sink.
fn centre_luma(output: &Output, width: usize, height: usize) -> Arc<Mutex<Vec<u8>>> {
    let lumas: Arc<Mutex<Vec<u8>>> = Arc::default();
    let l = Arc::clone(&lumas);
    output
        .pipeline()
        .pipeline()
        .by_name(names::LOOPBACK)
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(b)) = &info.data
                && let Ok(map) = b.map_readable()
            {
                // NV12, Y plane first, stride = width at this size.
                l.lock()
                    .unwrap()
                    .push(map[(height / 2) * width + width / 2]);
            }
            gst::PadProbeReturn::Ok
        });
    lumas
}

fn wait_for(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The daemon's session in system memory: output into a loopback writer,
/// with the effect element and a backend that takes host frames. The
/// camera image reaches the writer, blur is processed on host frames, and
/// no pipeline holds a CUDA element or a CUDA context.
#[test]
fn a_system_memory_session_streams_through_the_effect_without_cuda() {
    require_elements(ELEMENTS);
    let contract: OutputContract = "320x240@30".parse().unwrap();
    let mut writer = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
    writer.start().unwrap();
    let fake = FakeFactory::new(FakeConfig::default());
    let log: Arc<FakeLog> = Arc::clone(&fake.log);
    let mut cfg = OutputConfig::new(contract, OutputSink::Writer(writer.handle()));
    cfg.effect = Some(EffectSettings::default());
    cfg.effect_backend = Some(EffectBackend::new(fake.clone()));
    let output = Output::build_in(cfg, VideoMemory::System).expect("build output");
    assert_eq!(output.memory(), VideoMemory::System);
    assert!(output.pipeline().element(names::EFFECT).is_some());
    assert!(output.pipeline().element(names::DOWNLOAD).is_none());
    assert_no_cuda_element(output.pipeline());
    let lumas = centre_luma(&output, 320, 240);

    let mut session = Session::new(output).expect("start output");
    assert!(session.output().cuda_context().is_none());
    let ingest = yuy2_pattern(session.output());
    assert_no_cuda_element(ingest.pipeline());
    session.switch_to(ingest).expect("start source");
    wait_for(Duration::from_secs(10), "the camera image", || {
        lumas.lock().unwrap().iter().filter(|&&y| y == 235).count() >= 10
    });

    session
        .output()
        .set_effect(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    wait_for(Duration::from_secs(10), "the effect to load", || {
        session.output().effect_status() == EffectStatus::Ready
    });
    let from = lumas.lock().unwrap().len();
    wait_for(Duration::from_secs(10), "blurred frames", || {
        lumas.lock().unwrap()[from..]
            .iter()
            .filter(|&&y| y == FAKE_BLUR_Y)
            .count()
            >= 10
    });
    assert_eq!(
        session
            .output()
            .stage_values()
            .iter()
            .map(|v| (v.stage.as_str(), v.name.as_str(), v.value))
            .collect::<Vec<_>>(),
        [("background", "coverage", FAKE_COVERAGE)],
        "the effect alone reports its own values only"
    );
    // A stage that reports a status value: its value after a frame.
    session
        .output()
        .set_effect(&EffectSettings::of(EffectKind::Blur).with_stage(
            obc_pipeline::effect::StageRequest::on(obc_pipeline::effect::StageId::DENOISE),
        ))
        .unwrap();
    wait_for(Duration::from_secs(10), "the stage's status value", || {
        session
            .output()
            .stage_values()
            .iter()
            .any(|v| v.stage.as_str() == "denoise" && v.value == FAKE_DENOISE_LEVEL)
    });
    assert!(writer.counts().forwarded > 0, "{:?}", writer.counts());
    session.stop().unwrap();
    writer.stop().unwrap();

    assert!(FakeLog::get(&log.host_loads) >= 1);
    assert!(FakeLog::get(&log.host_processes) >= 10);
    assert_eq!(
        FakeLog::get(&log.host_processes),
        FakeLog::get(&log.processes)
    );
    assert_eq!(FakeLog::get(&log.wrong_memory), 0);
}

/// A backend that takes CUDA frames only, on a system-memory output: the
/// effect fails with that reason (the default `on_failure`, black, shows no
/// camera image), and the backend is never asked to process. CPU only.
#[test]
fn a_cuda_only_backend_fails_the_effect_on_a_system_memory_session() {
    require_elements(ELEMENTS);
    let contract: OutputContract = "320x240@30".parse().unwrap();
    let fake = FakeFactory::cuda_only(FakeConfig::default());
    let log: Arc<FakeLog> = Arc::clone(&fake.log);
    let mut cfg = OutputConfig::new(contract, OutputSink::Fake);
    cfg.effect = Some(EffectSettings::of(EffectKind::Blur));
    cfg.effect_backend = Some(EffectBackend::new(fake.clone()));
    let output = Output::build_in(cfg, VideoMemory::System).expect("build output");
    let lumas = centre_luma(&output, 320, 240);
    let mut session = Session::new(output).expect("start output");
    session
        .switch_to(yuy2_pattern(session.output()))
        .expect("start source");
    wait_for(Duration::from_secs(10), "the effect to fail", || {
        matches!(session.output().effect_status(), EffectStatus::Failed(_))
    });
    let EffectStatus::Failed(reason) = session.output().effect_status() else {
        unreachable!()
    };
    assert!(reason.contains("CUDA frames only"), "{reason}");
    wait_for(Duration::from_secs(10), "frames at the sink", || {
        lumas.lock().unwrap().len() >= 10
    });
    session.stop().unwrap();
    assert!(
        lumas.lock().unwrap().iter().all(|&y| y == 16),
        "black, never the camera: {:?}",
        lumas.lock().unwrap()
    );
    assert_eq!(FakeLog::get(&log.processes), 0);
}

/// A white PNG of `w x h` in `dir`.
fn white_png(dir: &std::path::Path, w: u32, h: u32) -> std::path::PathBuf {
    let path = dir.join("away.png");
    let p = gst::parse::launch(&format!(
        "videotestsrc num-buffers=1 pattern=white ! video/x-raw,format=RGB,width={w},\
         height={h} ! pngenc snapshot=true ! filesink location={}",
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
        .unwrap();
    assert_eq!(m.type_(), gst::MessageType::Eos, "{m:?}");
    p.set_state(gst::State::Null).unwrap();
    path
}

/// The output clock's placeholder image reaches the sink as decoded
/// through an effect in force: never blurred or blacked, and the backend
/// never runs on it. The black placeholder before it is the effect's
/// (black under blur at a start), as before. No camera: every tick is a
/// placeholder. CPU only.
#[test]
fn the_placeholder_image_passes_the_effect_untouched() {
    require_elements(ELEMENTS);
    require_elements(&["pngenc", "pngdec", "filesink", "videoconvert", "videoscale"]);
    let dir = tempfile::tempdir().unwrap();
    let png = white_png(dir.path(), 160, 120);
    let contract: OutputContract = "320x240@30".parse().unwrap();
    let fake = FakeFactory::new(FakeConfig::default());
    let log: Arc<FakeLog> = Arc::clone(&fake.log);
    let mut cfg = OutputConfig::new(contract, OutputSink::Fake);
    cfg.effect = Some(EffectSettings::of(EffectKind::Blur));
    cfg.effect_backend = Some(EffectBackend::new(fake.clone()));
    let output = Output::build_in(cfg, VideoMemory::System).expect("build output");
    let lumas = centre_luma(&output, 320, 240);
    let session = Session::new(output).expect("start output");
    wait_for(Duration::from_secs(10), "the effect to load", || {
        session.output().effect_status() == EffectStatus::Ready
    });
    wait_for(Duration::from_secs(10), "black placeholders", || {
        lumas.lock().unwrap().len() >= 5
    });
    session.output().set_placeholder_image(Some(&png));
    wait_for(Duration::from_secs(10), "the image decoded", || {
        session.output().placeholder_state() == PlaceholderState::Image
    });
    // The tick in flight may still be a black one.
    let from = lumas.lock().unwrap().len() + 1;
    let processes = FakeLog::get(&log.processes);
    wait_for(Duration::from_secs(10), "image placeholders", || {
        lumas.lock().unwrap().len() >= from + 10
    });
    let seen = lumas.lock().unwrap()[from..].to_vec();
    assert!(
        seen.iter().all(|&y| y == 235),
        "the image, untouched: {seen:?}"
    );
    assert!(
        FakeLog::get(&log.processes) <= processes + 1,
        "the backend ran on the image"
    );
    assert!(
        processes > 0,
        "the black placeholder ran through the effect"
    );
    session.stop().unwrap();
}

/// A session forwarding to a started writer, blur loaded and showing, on
/// the fake backend; CPU only.
fn blurred_session() -> (LoopbackWriter, Arc<FakeFactory>, Session) {
    require_elements(ELEMENTS);
    let contract: OutputContract = "320x240@30".parse().unwrap();
    let mut writer = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
    writer.start().unwrap();
    let fake = FakeFactory::new(FakeConfig::default());
    let mut cfg = OutputConfig::new(contract, OutputSink::Writer(writer.handle()));
    cfg.effect = Some(EffectSettings::default());
    cfg.effect_backend = Some(EffectBackend::new(fake.clone()));
    let output = Output::build_in(cfg, VideoMemory::System).expect("build output");
    let lumas = centre_luma(&output, 320, 240);
    let mut session = Session::new(output).expect("start output");
    let ingest = yuy2_pattern(session.output());
    session.switch_to(ingest).expect("start source");
    wait_for(Duration::from_secs(10), "the camera image", || {
        lumas.lock().unwrap().iter().filter(|&&y| y == 235).count() >= 10
    });
    session
        .output()
        .set_effect(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    wait_for(Duration::from_secs(10), "the effect to load", || {
        session.output().effect_status() == EffectStatus::Ready
    });
    let from = lumas.lock().unwrap().len();
    wait_for(Duration::from_secs(10), "blurred frames", || {
        lumas.lock().unwrap()[from..]
            .iter()
            .filter(|&&y| y == FAKE_BLUR_Y)
            .count()
            >= 10
    });
    (writer, fake, session)
}

/// Turns the denoise stage on under blur, a switch the fake backend takes
/// 300 ms over: about 9 periods with no frame from the output.
fn switch_on_denoise_slowly(fake: &FakeFactory, session: &Session) {
    use obc_pipeline::effect::{StageId, StageRequest};
    fake.set(|c| c.switch_delay = Duration::from_millis(300));
    session
        .output()
        .set_effect(
            &EffectSettings::of(EffectKind::Blur).with_stage(StageRequest::on(StageId::DENOISE)),
        )
        .unwrap();
}

/// A deliberate switch on the loaded effect (a stage turned on under blur)
/// stalls the output's streaming thread for as long as the backend takes
/// to switch: the writer holds the output's last frame meanwhile, and
/// sends no placeholder. Once the session is stopped (the camera off), the
/// placeholder resumes. CPU only.
#[test]
fn a_stage_switch_under_blur_holds_the_last_frame_not_the_placeholder() {
    let (mut writer, fake, session) = blurred_session();
    let before = writer.counts();
    switch_on_denoise_slowly(&fake, &session);
    wait_for(Duration::from_secs(10), "frames after the switch", || {
        session
            .output()
            .stage_values()
            .iter()
            .any(|v| v.stage.as_str() == "denoise" && v.value == FAKE_DENOISE_LEVEL)
            && writer.counts().forwarded >= before.forwarded + 10
    });
    let after = writer.counts();
    assert_eq!(
        after.placeholder, before.placeholder,
        "placeholders during a stage switch: {before:?} -> {after:?}"
    );
    assert!(
        after.held > before.held,
        "the last frame was not held: {before:?} -> {after:?}"
    );

    // The camera off: the writer's placeholder follows, not a frozen frame.
    session.stop().unwrap();
    let stopped = writer.counts();
    wait_for(
        Duration::from_secs(1),
        "the placeholder after the stop",
        || writer.counts().placeholder >= stopped.placeholder + 5,
    );
    let end = writer.counts();
    writer.stop().unwrap();
    assert_eq!(
        end.held, stopped.held,
        "held after the stop: {stopped:?} -> {end:?}"
    );
}

/// The camera turned off in the middle of a 300 ms switch stall: the
/// session's stop detaches the output from the writer, so the placeholder
/// takes over and nothing is held once the stop has returned. CPU only.
#[test]
fn a_stop_during_a_switch_stall_hands_over_to_the_placeholder() {
    let (mut writer, fake, session) = blurred_session();
    let before = writer.counts();
    switch_on_denoise_slowly(&fake, &session);
    // The stall has begun: the writer holds the last frame.
    wait_for(Duration::from_secs(10), "held frames", || {
        writer.counts().held >= before.held + 2
    });
    session.stop().unwrap();
    let stopped = writer.counts();
    wait_for(
        Duration::from_secs(10),
        "the placeholder after the stop",
        || writer.counts().placeholder >= stopped.placeholder + 5,
    );
    std::thread::sleep(Duration::from_millis(100));
    let end = writer.counts();
    writer.stop().unwrap();
    assert_eq!(
        end.held, stopped.held,
        "held after the stop: {stopped:?} -> {end:?}"
    );
}
