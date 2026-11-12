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

use fake_backend::{FAKE_BLUR_Y, FakeConfig, FakeFactory, FakeLog};
use gst::prelude::*;
use obc_core::config::VideoMemory;
use obc_core::{AspectPolicy, OutputContract, PixelFormat};
use obc_pipeline::effect::EffectKind;
use obc_pipeline::pipeline::{ingest_adapter, names};
use obc_pipeline::{
    EffectBackend, EffectSettings, EffectStatus, Ingest, LoopbackWriter, MediaPipeline, Output,
    OutputConfig, OutputSink, Session, WriterSink,
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
