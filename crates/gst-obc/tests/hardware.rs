//! The element on a GPU, with the fake backend of `support/fake_backend.rs`
//! (no Maxine SDK needed), and once with the real Maxine backend.
//!
//! All tests are `#[ignore]`. The ones named `gpu_*` need an NVIDIA GPU with
//! GStreamer's CUDA elements (`cudaupload`, `cudadownload`, from the
//! nvcodec plugin). A test without them fails, naming what is missing. Run
//! with `cargo test -p gst-obc --test hardware -- --ignored gpu_ --test-threads=1`.
//!
//! The ones named `maxine_*` also need the Maxine SDK, set up as for
//! `crates/obc-backend-maxine/tests/hardware.rs` (the environment file of
//! the `maxine-vfx-sdk-local` package loaded, the os-release shim preloaded
//! through the cargo runner); run them with `-- --ignored maxine_
//! --test-threads=1`.
//!
//! The rig is the output pipeline's head in miniature: one canonical frame
//! (NV12 in CUDA memory, a luma gradient that matches no constant the fake
//! writes) is uploaded once and then pushed again and again as shallow
//! copies, as the output clock repeats a frame, through
//! `appsrc ! obceffect ! cudadownload ! appsink`; every output frame is read
//! back and classified.

#[path = "support/fake_backend.rs"]
mod fake_backend;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_backend::{
    FAKE_BLUR_Y, FAKE_DENOISE_MS, FAKE_DENOISE_UV, FAKE_DENOISE_Y, FAKE_REPLACE_Y, FakeConfig,
    FakeFactory, FakeLog, Hook,
};
use gst::prelude::*;
use gst_obc::{EffectFallback, EffectKind, EffectSettings, EffectStatus, ObcEffect};
use gst_video::prelude::*;
use obc_backend::BackendFactory;
use obc_backend_maxine::{MaskFault, MaskFaultSwitch, MaxineFactory};

const W: u32 = 640;
const H: u32 = 360;
const CANONICAL: &str = "video/x-raw(memory:CUDAMemory),format=NV12,width=640,height=360,\
                         pixel-aspect-ratio=1/1,colorimetry=2:4:5:1,framerate=30/1";

fn require_elements(factories: &[&str]) {
    gst::init().unwrap();
    let missing: Vec<&str> = factories
        .iter()
        .copied()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "GStreamer element(s) not installed: {}. The cuda* elements come from the nvcodec \
         plugin (gst-plugins-bad) and register only where an NVIDIA GPU and its driver are \
         present. Check with `gst-inspect-1.0 <element>`.",
        missing.join(", ")
    );
}

/// What an output frame shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Input,
    Black,
    FakeBlur,
    FakeReplace,
    /// The fake's denoise stage alone, or under an effect.
    FakeDenoise,
    FakeBlurDenoised,
    FakeReplaceDenoised,
    Other,
}

/// The packed host NV12 of the input: Y rows 30 + (row % 150), UV 128.
fn input_bytes() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![128u8; w * h * 3 / 2];
    for row in 0..h {
        data[row * w..(row + 1) * w].fill(30 + (row % 150) as u8);
    }
    data
}

fn classify(frame: &[u8], input: &[u8]) -> Seen {
    let y_len = (W * H) as usize;
    let (y, uv) = frame.split_at(y_len);
    let all = |s: &[u8], v: u8| s.iter().all(|b| *b == v);
    if frame == input {
        Seen::Input
    } else if all(y, 16) && all(uv, 128) {
        Seen::Black
    } else if all(y, FAKE_BLUR_Y) && all(uv, 128) {
        Seen::FakeBlur
    } else if all(y, FAKE_REPLACE_Y) && all(uv, 128) {
        Seen::FakeReplace
    } else if all(y, FAKE_DENOISE_Y) && all(uv, FAKE_DENOISE_UV) {
        Seen::FakeDenoise
    } else if all(y, FAKE_BLUR_Y) && all(uv, FAKE_DENOISE_UV) {
        Seen::FakeBlurDenoised
    } else if all(y, FAKE_REPLACE_Y) && all(uv, FAKE_DENOISE_UV) {
        Seen::FakeReplaceDenoised
    } else {
        Seen::Other
    }
}

/// Reads a system-memory NV12 sample back as packed bytes.
fn packed(sample: &gst::Sample) -> Vec<u8> {
    let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer().unwrap(), &info)
        .unwrap();
    let mut out = Vec::with_capacity((W * H * 3 / 2) as usize);
    for (plane, rows) in [(0, H), (1, H / 2)] {
        let stride = frame.plane_stride()[plane] as usize;
        let data = frame.plane_data(plane as u32).unwrap();
        for r in 0..rows as usize {
            out.extend_from_slice(&data[r * stride..r * stride + W as usize]);
        }
    }
    out
}

/// One CUDA frame of `input_bytes()`, and the context it lives in.
fn upload() -> (gst::Buffer, gst::Context) {
    let p = gst::parse::launch(&format!(
        "appsrc name=src format=time caps=video/x-raw,format=NV12,width={W},height={H},\
         pixel-aspect-ratio=1/1,colorimetry=2:4:5:1,framerate=30/1 ! cudaupload ! \
         capsfilter caps=\"{CANONICAL}\" ! appsink name=sink sync=false"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let src = p
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    let sink = p
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    p.set_state(gst::State::Playing).unwrap();
    let mut b = gst::Buffer::from_mut_slice(input_bytes());
    b.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    src.push_buffer(b).unwrap();
    let sample = sink
        .try_pull_sample(gst::ClockTime::from_seconds(10))
        .expect("an uploaded frame");
    let ctx = p
        .context(gst_obc::cuda::CONTEXT_TYPE)
        .expect("cudaupload opened a CUDA context");
    let buffer = sample.buffer_owned().unwrap();
    p.set_state(gst::State::Null).unwrap();
    (buffer, ctx)
}

struct Rig {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    effect: ObcEffect,
    /// The `GstCudaContext` the input lives in.
    context: gst::Context,
    input: gst::Buffer,
    input_bytes: Vec<u8>,
    caps_events: Arc<AtomicU64>,
    /// The first memory of every buffer leaving the effect.
    out_memory: Arc<Mutex<Vec<usize>>>,
    in_memory: usize,
    pts: u64,
    messages: Vec<(String, String, String)>,
    /// The `denoise` and `denoise-reason` fields of the same messages.
    denoise_messages: Vec<(String, String)>,
}

impl Rig {
    fn new(factory: Option<Arc<FakeFactory>>, settings: &EffectSettings) -> Rig {
        Rig::with_factory(factory.map(|f| f as Arc<dyn BackendFactory>), settings)
    }

    fn with_factory(factory: Option<Arc<dyn BackendFactory>>, settings: &EffectSettings) -> Rig {
        require_elements(&["cudaupload", "cudadownload", "appsrc", "appsink"]);
        gst_obc::register().unwrap();
        let (input, ctx) = upload();
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=src format=time is-live=true caps=\"{CANONICAL}\" ! \
             obceffect name=fx ! cudadownload ! video/x-raw,format=NV12 ! \
             appsink name=sink sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        pipeline.set_context(&ctx);
        let context = ctx;
        let effect = pipeline
            .by_name("fx")
            .unwrap()
            .downcast::<ObcEffect>()
            .unwrap();
        if let Some(f) = factory {
            effect.set_backend_factory(Some(f));
        }
        effect.apply(settings).unwrap();
        let src = pipeline
            .by_name("src")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        let sink_el = pipeline.by_name("sink").unwrap();
        let caps_events = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&caps_events);
        sink_el.static_pad("sink").unwrap().add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| {
                if let Some(gst::PadProbeData::Event(e)) = &info.data
                    && e.type_() == gst::EventType::Caps
                {
                    c.fetch_add(1, Ordering::SeqCst);
                }
                gst::PadProbeReturn::Ok
            },
        );
        let out_memory: Arc<Mutex<Vec<usize>>> = Arc::default();
        let m = Arc::clone(&out_memory);
        effect
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                    m.lock().unwrap().push(b.peek_memory(0).as_ptr() as usize);
                }
                gst::PadProbeReturn::Ok
            });
        let sink = sink_el.downcast::<gst_app::AppSink>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let in_memory = input.peek_memory(0).as_ptr() as usize;
        Rig {
            pipeline,
            src,
            sink,
            effect,
            context,
            input,
            input_bytes: input_bytes(),
            caps_events,
            out_memory,
            in_memory,
            pts: 0,
            messages: Vec::new(),
            denoise_messages: Vec::new(),
        }
    }

    /// Pushes the input once more (a shallow copy, as a clock repeat) and
    /// returns what came out.
    fn frame(&mut self) -> Seen {
        self.frame_with(gst::BufferFlags::empty())
    }

    /// [`Self::frame`] with `flags` set on the pushed buffer.
    fn frame_with(&mut self, flags: gst::BufferFlags) -> Seen {
        let mut b = self.input.copy();
        {
            let b = b.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(self.pts));
            b.set_duration(gst::ClockTime::from_nseconds(33_333_333));
            b.unset_flags(gst_obc::NEW_SOURCE_FLAG);
            b.set_flags(flags);
        }
        self.pts += 33_333_333;
        self.src.push_buffer(b).expect("push");
        let sample = self
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .unwrap_or_else(|| panic!("no output frame; bus: {:?}", self.bus_errors()));
        self.drain_messages();
        classify(&packed(&sample), &self.input_bytes)
    }

    /// Frames at about 30 fps until `done` or `timeout`; returns them.
    fn frames_until(&mut self, timeout: Duration, done: impl Fn(&Rig) -> bool) -> Vec<Seen> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out; status {:?}, frames {seen:?}",
                self.effect.status()
            );
            seen.push(self.frame());
            std::thread::sleep(Duration::from_millis(30));
        }
        seen
    }

    fn drain_messages(&mut self) {
        let bus = self.pipeline.bus().unwrap();
        while let Some(m) = bus.pop_filtered(&[gst::MessageType::Element]) {
            if let Some(s) = m.structure()
                && s.name() == gst_obc::STATUS_MESSAGE
            {
                self.messages.push((
                    s.get::<String>("effect").unwrap(),
                    s.get::<String>("status").unwrap(),
                    s.get::<String>("reason").unwrap(),
                ));
                self.denoise_messages.push((
                    s.get::<String>("denoise").unwrap(),
                    s.get::<String>("denoise-reason").unwrap(),
                ));
            }
        }
    }

    fn bus_errors(&self) -> Vec<String> {
        let bus = self.pipeline.bus().unwrap();
        let mut v = Vec::new();
        while let Some(m) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = m.view() {
                v.push(format!("{} ({:?})", e.error(), e.debug()));
            }
        }
        v
    }

    /// The input's bytes, read back from its CUDA memory.
    fn input_now(&self) -> Vec<u8> {
        let caps: gst::Caps = CANONICAL.parse().unwrap();
        let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(&self.input, &info)
            .expect("map the input on the host");
        let mut out = Vec::new();
        for (plane, rows) in [(0, H), (1, H / 2)] {
            let stride = frame.plane_stride()[plane] as usize;
            let data = frame.plane_data(plane as u32).unwrap();
            for r in 0..rows as usize {
                out.extend_from_slice(&data[r * stride..r * stride + W as usize]);
            }
        }
        out
    }

    /// The reference count of the input's `GstCudaContext`: the context is
    /// destroyed when it reaches zero.
    fn context_refs(&self) -> u32 {
        self.context
            .structure()
            .get::<gst::glib::Object>(gst_obc::cuda::CONTEXT_TYPE)
            .expect("the CUDA context in the GstContext")
            .ref_count()
            // The `get` above holds one more.
            - 1
    }

    fn stop(&self) {
        self.pipeline.set_state(gst::State::Null).unwrap();
        let (_, cur, _) = self.pipeline.state(gst::ClockTime::from_seconds(5));
        assert_eq!(cur, gst::State::Null);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn status_is(want: EffectStatus) -> impl Fn(&Rig) -> bool {
    move |r| r.effect.status() == want
}

fn failed(r: &Rig) -> bool {
    matches!(r.effect.status(), EffectStatus::Failed(_))
}

fn denoise_is(want: EffectStatus) -> impl Fn(&Rig) -> bool {
    move |r| r.effect.denoise_status() == want
}

fn denoise_failed(r: &Rig) -> bool {
    matches!(r.effect.denoise_status(), EffectStatus::Failed(_))
}

/// `kind` with the denoise stage.
fn denoised(kind: EffectKind) -> EffectSettings {
    EffectSettings {
        denoise: true,
        ..EffectSettings::of(kind)
    }
}

fn get(a: &AtomicU64) -> u64 {
    FakeLog::get(a)
}

/// A background PNG for replace.
fn background(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("bg.png");
    let p = gst::parse::launch(&format!(
        "videotestsrc num-buffers=1 ! video/x-raw,width=800,height=600 ! pngenc snapshot=true ! \
         filesink location={}",
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

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_none_passes_the_input_buffer_through_without_cuda() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    for _ in 0..5 {
        assert_eq!(rig.frame(), Seen::Input);
    }
    let s = rig.effect.stats();
    assert_eq!(s.frames_passthrough, 5);
    assert_eq!(s.frames_processed + s.frames_black, 0);
    assert!(!s.cuda_active, "passthrough must not create CUDA resources");
    assert_eq!(get(&fake.log.creates), 0);
    let mems = rig.out_memory.lock().unwrap().clone();
    assert!(
        mems.iter().all(|m| *m == rig.in_memory),
        "passthrough forwards the input memory itself"
    );
    assert_eq!(rig.caps_events.load(Ordering::SeqCst), 1);
    rig.stop();
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_switches_never_show_the_input_while_an_effect_is_requested() {
    let dir = tempfile::tempdir().unwrap();
    let bg = background(dir.path());
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(800),
        switch_delay: Duration::from_millis(300),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);

    // none -> blur: black until the (slow) load is done, then processed.
    let blur = EffectSettings {
        blur_strength: 0.7,
        ..EffectSettings::of(EffectKind::Blur)
    };
    rig.effect.apply(&blur).unwrap();
    let t0 = Instant::now();
    let warm = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    let warm_ms = t0.elapsed().as_millis();
    assert!(warm.len() >= 10, "{warm:?}");
    assert!(warm.iter().all(|s| *s == Seen::Black), "{warm:?}");
    assert!(warm_ms >= 750, "the load took {warm_ms} ms");
    for _ in 0..5 {
        assert_eq!(rig.frame(), Seen::FakeBlur);
    }
    assert_eq!(get(&fake.log.last_strength_milli), 700);

    // A strength change applies on the next frame, without a load.
    let loads = get(&fake.log.loads);
    rig.effect.set_property("blur-strength", 0.2f64);
    assert_eq!(rig.frame(), Seen::FakeBlur);
    assert_eq!(get(&fake.log.last_strength_milli), 200);
    assert_eq!(get(&fake.log.loads), loads);

    // blur -> replace: blur until replace is ready, never black or input.
    let replace = EffectSettings {
        background: Some(bg.clone()),
        ..EffectSettings::of(EffectKind::Replace)
    };
    rig.effect.apply(&replace).unwrap();
    let switch = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    // The previous effect until the switch is done, never black or input.
    // (The streaming thread waits for the switch itself, 300 ms here and
    // milliseconds with Maxine, so the frame during it comes late but
    // processed; the last frame before the status turns may already be the
    // new effect.)
    let blur_frames = switch.iter().take_while(|s| **s == Seen::FakeBlur).count();
    assert!(blur_frames >= 1, "{switch:?}");
    assert!(
        switch[blur_frames..]
            .iter()
            .all(|s| *s == Seen::FakeReplace),
        "every frame of the switch is processed: {switch:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::FakeReplace);
    }
    assert_eq!(*fake.log.backgrounds.lock().unwrap(), [(W, H)]);

    // -> none: the input at once; the worker unloads.
    rig.effect.apply(&EffectSettings::default()).unwrap();
    assert_eq!(rig.frame(), Seen::Input);
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(&fake.log.unloads) == 0 {
        assert!(Instant::now() < deadline, "no unload");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rig.frame(), Seen::Input);

    // none -> blur again: the model was unloaded, so black first.
    rig.effect.apply(&blur).unwrap();
    let again = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(again.iter().all(|s| *s == Seen::Black), "{again:?}");
    assert_eq!(rig.frame(), Seen::FakeBlur);

    assert_eq!(
        get(&fake.log.wrong_context),
        0,
        "a call without the context"
    );
    assert_eq!(get(&fake.log.wrong_stream), 0, "process on another stream");
    assert_eq!(
        rig.caps_events.load(Ordering::SeqCst),
        1,
        "the effect changes never renegotiate"
    );
    let stats = rig.effect.stats();
    assert!(stats.cuda_active);
    assert!(stats.frames_processed > 0 && stats.frames_black > 0);
    assert!(stats.gpu_ms_p50.is_some() && stats.gpu_ms_p95 >= stats.gpu_ms_p50);
    assert!(stats.last_load_ms.unwrap() >= 750.0);
    rig.drain_messages();
    let statuses: Vec<&str> = rig.messages.iter().map(|m| m.1.as_str()).collect();
    assert_eq!(
        statuses,
        [
            "loading", "ready", "loading", "ready", "off", "loading", "ready"
        ],
        "{:?}",
        rig.messages
    );

    // Stopping unloads with the context current, then releases CUDA.
    rig.stop();
    assert!(!rig.effect.stats().cuda_active);
    assert_eq!(get(&fake.log.drops), 1);
    assert_eq!(
        get(&fake.log.wrong_context),
        0,
        "unload/drop without the context"
    );
    assert_eq!(get(&fake.log.creates), 1);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_failures_show_black_or_the_input_as_configured() {
    let fake = FakeFactory::new(FakeConfig {
        fail_load: Some("fake load failure".into()),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    rig.effect
        .apply(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), failed);
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    assert_eq!(
        rig.effect.status(),
        EffectStatus::Failed("fake load failure".into())
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Black, "on-failure=black keeps black");
    }
    rig.drain_messages();
    assert!(
        rig.messages
            .contains(&("blur".into(), "failed".into(), "fake load failure".into())),
        "{:?}",
        rig.messages
    );

    // on-failure=none: the input (the user opted in). Applying retries,
    // fails again, and then passes the input through.
    let open = EffectSettings {
        fallback: EffectFallback::None,
        ..EffectSettings::of(EffectKind::Blur)
    };
    rig.effect.apply(&open).unwrap();
    // Black while the retry loads; the input once it has failed (the
    // failure may win the race with the first frame).
    let seen = rig.frames_until(Duration::from_secs(5), failed);
    assert!(
        seen.iter().all(|s| matches!(s, Seen::Black | Seen::Input)),
        "{seen:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Input);
    }

    // A process failure after a good load: black again with on-failure=black.
    fake.set(|c| c.fail_load = None);
    rig.effect
        .apply(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    fake.set(|c| c.fail_process = Some("fake frame failure".into()));
    assert_eq!(rig.frame(), Seen::Black, "the failing frame is black");
    assert!(
        rig.effect
            .status()
            .reason()
            .is_some_and(|r| r.contains("fake frame failure")),
        "{:?}",
        rig.effect.status()
    );
    assert_eq!(rig.frame(), Seen::Black);
    assert_eq!(rig.caps_events.load(Ordering::SeqCst), 1);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_an_unavailable_backend_or_image_is_a_failure_with_its_reason() {
    let fake = FakeFactory::new(FakeConfig {
        fail_create: Some("fake SDK missing".into()),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    let seen = rig.frames_until(Duration::from_secs(5), failed);
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    assert_eq!(
        rig.effect.status(),
        EffectStatus::Failed("fake SDK missing".into())
    );

    // A replace image that does not decode.
    fake.set(|c| c.fail_create = None);
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.png");
    std::fs::write(&junk, b"not an image").unwrap();
    rig.effect
        .apply(&EffectSettings {
            background: Some(junk),
            ..EffectSettings::of(EffectKind::Replace)
        })
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(10), failed);
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    let reason = rig.effect.status().reason().unwrap().to_string();
    assert!(reason.contains("junk.png"), "{reason}");

    // No factory at all.
    let mut bare = Rig::new(None, &EffectSettings::of(EffectKind::Blur));
    bare.frames_until(Duration::from_secs(5), failed);
    assert!(
        bare.effect
            .status()
            .reason()
            .unwrap()
            .contains("no inference backend")
    );
    assert_eq!(bare.frame(), Seen::Black);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_repeated_pushes_leave_the_input_unmodified() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    for _ in 0..20 {
        assert_eq!(rig.frame(), Seen::FakeBlur);
    }
    assert_eq!(rig.input_now(), rig.input_bytes, "the input was written");
    let mems = rig.out_memory.lock().unwrap().clone();
    assert!(
        mems.iter().all(|m| *m != rig.in_memory),
        "an effect frame is a new buffer"
    );
    rig.stop();
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_each_stop_releases_the_cuda_context_exactly_once() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    // The element's context reference, stream and events are released by
    // dropping them (GpuResources::drop); stop drops them once. Across
    // cycles the context's reference count must come back to the same
    // value: a leak would raise it, a second release would lower it.
    let mut after_stop = Vec::new();
    for cycle in 0..3 {
        if cycle > 0 {
            rig.pipeline.set_state(gst::State::Playing).unwrap();
        }
        rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
        assert_eq!(rig.frame(), Seen::FakeBlur);
        assert!(rig.effect.stats().cuda_active);
        rig.stop();
        assert!(!rig.effect.stats().cuda_active);
        after_stop.push(rig.context_refs());
    }
    assert!(
        after_stop.iter().all(|n| *n == after_stop[0]),
        "context references after each stop: {after_stop:?}"
    );
    assert_eq!(
        get(&fake.log.wrong_context),
        0,
        "unload/drop without the context"
    );
    assert_eq!(get(&fake.log.unloads), 3);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_none_with_denoise_passes_through_until_loaded_then_denoises() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(500),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    // none without denoise: passthrough, no CUDA at all.
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Input);
    }
    assert!(!rig.effect.stats().cuda_active);
    assert_eq!(get(&fake.log.creates), 0);

    rig.effect.apply(&denoised(EffectKind::None)).unwrap();
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    assert_eq!(rig.effect.denoise_status(), EffectStatus::Loading);
    let warm = rig.frames_until(Duration::from_secs(10), denoise_is(EffectStatus::Ready));
    assert!(warm.len() >= 5, "{warm:?}");
    assert!(
        warm.iter().all(|s| *s == Seen::Input),
        "the input while the stage loads, never black: {warm:?}"
    );
    for _ in 0..5 {
        assert_eq!(rig.frame(), Seen::FakeDenoise);
    }
    assert_eq!(rig.effect.status(), EffectStatus::Off, "no effect");
    let s = rig.effect.stats();
    assert_eq!(s.frames_black, 0);
    assert!(s.frames_denoised >= 5 && s.frames_denoised == s.frames_processed);
    assert_eq!(s.denoise_ms_p50, Some(f64::from(FAKE_DENOISE_MS)));
    assert_eq!(
        *fake.log.requests.lock().unwrap(),
        [(EffectKind::None, true)]
    );

    // Off again: the input at once; the worker unloads.
    rig.effect.set_property("denoise", false);
    assert_eq!(rig.frame(), Seen::Input);
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(&fake.log.unloads) == 0 {
        assert!(Instant::now() < deadline, "no unload");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rig.frame(), Seen::Input);
    assert_eq!(rig.effect.denoise_status(), EffectStatus::Off);
    rig.drain_messages();
    let denoise: Vec<&str> = rig.denoise_messages.iter().map(|m| m.0.as_str()).collect();
    assert_eq!(denoise, ["loading", "ready", "off"], "{:?}", rig.messages);
    assert!(
        rig.messages.iter().all(|m| m.1 == "off"),
        "{:?}",
        rig.messages
    );
    rig.stop();
    assert!(!rig.effect.stats().cuda_active);
    assert_eq!(get(&fake.log.wrong_context), 0);
    assert_eq!(get(&fake.log.wrong_stream), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_denoise_load_failure_alone_passes_the_input_through_with_the_reason() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(200),
        fail_denoise_load: Some("fake denoise unavailable".into()),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &denoised(EffectKind::None));
    let seen = rig.frames_until(Duration::from_secs(5), denoise_failed);
    assert!(seen.iter().all(|s| *s == Seen::Input), "{seen:?}");
    assert_eq!(
        rig.effect.denoise_status(),
        EffectStatus::Failed("fake denoise unavailable".into())
    );
    assert_eq!(
        rig.effect.status(),
        EffectStatus::Off,
        "the effect never fails"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Input);
    }
    assert_eq!(rig.effect.stats().frames_black, 0, "never black");
    rig.drain_messages();
    assert!(
        rig.denoise_messages
            .contains(&("failed".into(), "fake denoise unavailable".into())),
        "{:?}",
        rig.denoise_messages
    );

    // Off clears the failure; on again retries (and now loads).
    fake.set(|c| c.fail_denoise_load = None);
    rig.effect.set_property("denoise", false);
    assert_eq!(rig.effect.denoise_status(), EffectStatus::Off);
    rig.effect.set_property("denoise", true);
    assert_eq!(rig.effect.denoise_status(), EffectStatus::Loading);
    let seen = rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert!(seen.iter().all(|s| *s == Seen::Input), "{seen:?}");
    assert_eq!(rig.frame(), Seen::FakeDenoise);

    // A retry (apply, unchanged settings) clears a recorded failure too.
    // (Off, and once unloaded on again, so the stage loads anew: turned on
    // before the unload it would still be loaded.)
    fake.set(|c| c.fail_denoise_load = Some("fake denoise unavailable".into()));
    let unloads = get(&fake.log.unloads);
    rig.effect.set_property("denoise", false);
    rig.frames_until(Duration::from_secs(5), |_| get(&fake.log.unloads) > unloads);
    rig.effect.set_property("denoise", true);
    rig.frames_until(Duration::from_secs(5), denoise_failed);
    assert_eq!(rig.frame(), Seen::Input);
    fake.set(|c| c.fail_denoise_load = None);
    rig.effect.apply(&denoised(EffectKind::None)).unwrap();
    assert_eq!(rig.effect.denoise_status(), EffectStatus::Loading);
    rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeDenoise);
    assert_eq!(rig.effect.stats().frames_black, 0, "never black");
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);

    // Any failure of the stage alone is a denoise failure: here the
    // backend is unavailable.
    let missing = FakeFactory::new(FakeConfig {
        fail_create: Some("fake SDK missing".into()),
        ..FakeConfig::default()
    });
    let mut bare = Rig::new(Some(Arc::clone(&missing)), &denoised(EffectKind::None));
    let seen = bare.frames_until(Duration::from_secs(5), denoise_failed);
    assert!(seen.iter().all(|s| *s == Seen::Input), "{seen:?}");
    assert_eq!(
        bare.effect.denoise_status(),
        EffectStatus::Failed("fake SDK missing".into())
    );
    assert_eq!(bare.effect.status(), EffectStatus::Off);
    assert_eq!(bare.frame(), Seen::Input);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_blur_with_a_failed_denoise_load_runs_the_blur_alone() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(200),
        fail_denoise_load: Some("fake denoise unavailable".into()),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    rig.effect.apply(&denoised(EffectKind::Blur)).unwrap();
    let warm = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(warm.iter().all(|s| *s == Seen::Black), "{warm:?}");
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::FakeBlur, "the blur, without the stage");
    }
    assert_eq!(
        rig.effect.denoise_status(),
        EffectStatus::Failed("fake denoise unavailable".into())
    );
    assert_eq!(rig.effect.status(), EffectStatus::Ready);
    // The failed attempt with the stage loaded nothing; the blur alone did.
    assert_eq!(
        *fake.log.requests.lock().unwrap(),
        [(EffectKind::Blur, false)]
    );
    assert!(rig.effect.stats().frames_processed >= 3);
    assert_eq!(rig.effect.stats().frames_denoised, 0);

    // The stage becomes available; a retry adds it as a switch: the blur
    // runs throughout, never black.
    fake.set(|c| c.fail_denoise_load = None);
    rig.effect.apply(&denoised(EffectKind::Blur)).unwrap();
    assert_eq!(
        rig.effect.status(),
        EffectStatus::Ready,
        "the effect is unchanged"
    );
    let seen = rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeBlur | Seen::FakeBlurDenoised)),
        "{seen:?}"
    );
    assert_eq!(rig.frame(), Seen::FakeBlurDenoised);
    rig.drain_messages();
    assert!(
        rig.messages.iter().all(|m| m.1 != "failed"),
        "the effect never failed: {:?}",
        rig.messages
    );
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_denoise_frame_failure_drops_the_stage_and_keeps_the_effect() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &denoised(EffectKind::Blur));
    rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert_eq!(rig.effect.status(), EffectStatus::Ready);
    assert_eq!(rig.frame(), Seen::FakeBlurDenoised);

    // For each fallback: the failing frame runs again without the stage,
    // so it is the effect's (never black, never the input), and so is
    // every later one.
    for fallback in [EffectFallback::Black, EffectFallback::None] {
        let settings = EffectSettings {
            fallback,
            ..denoised(EffectKind::Blur)
        };
        rig.effect.apply(&settings).unwrap();
        rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
        assert_eq!(rig.frame(), Seen::FakeBlurDenoised, "{fallback:?}");
        let before = rig.effect.stats();
        fake.set(|c| c.fail_denoise_process = Some("fake denoise frame failure".into()));
        assert_eq!(
            rig.frame(),
            Seen::FakeBlur,
            "the failing frame, {fallback:?}"
        );
        for _ in 0..3 {
            assert_eq!(rig.frame(), Seen::FakeBlur, "the chain without the stage");
        }
        assert_eq!(
            rig.effect.status(),
            EffectStatus::Ready,
            "the effect never fails"
        );
        let reason = rig.effect.denoise_status().reason().unwrap().to_string();
        assert!(reason.contains("fake denoise frame failure"), "{reason}");
        let after = rig.effect.stats();
        assert_eq!(after.frames_processed, before.frames_processed + 4);
        assert_eq!(after.frames_black, before.frames_black, "{fallback:?}");
        assert_eq!(
            after.frames_passthrough, before.frames_passthrough,
            "never the input, {fallback:?}"
        );
        assert_eq!(after.frames_denoised, before.frames_denoised);
        let processed = get(&fake.log.processes);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(rig.frame(), Seen::FakeBlur);
        assert_eq!(get(&fake.log.processes), processed + 1);
        fake.set(|c| c.fail_denoise_process = None);
    }

    // The stage alone (none): the failing frame is the input, then the
    // input passes through (the worker unloads).
    rig.effect.apply(&denoised(EffectKind::Blur)).unwrap();
    rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    fake.set(|c| c.fail_denoise_process = Some("fake denoise frame failure".into()));
    rig.effect.apply(&denoised(EffectKind::None)).unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), denoise_failed);
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeBlur | Seen::Input)),
        "the previous chain, then the input: {seen:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Input);
    }
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    rig.drain_messages();
    assert!(
        rig.messages.iter().all(|m| m.1 != "failed"),
        "the effect never failed: {:?}",
        rig.messages
    );
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_an_effect_failing_again_without_the_stage_takes_the_failure_path() {
    for (fallback, want) in [
        (EffectFallback::Black, Seen::Black),
        (EffectFallback::None, Seen::Input),
    ] {
        let fake = FakeFactory::new(FakeConfig::default());
        let settings = EffectSettings {
            fallback,
            ..denoised(EffectKind::Blur)
        };
        let mut rig = Rig::new(Some(Arc::clone(&fake)), &settings);
        rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
        assert_eq!(rig.frame(), Seen::FakeBlurDenoised);
        fake.set(|c| {
            c.fail_denoise_process = Some("fake denoise frame failure".into());
            c.fail_process = Some("fake frame failure".into());
        });
        assert_eq!(rig.frame(), want, "{fallback:?}");
        let status = rig.effect.status();
        assert!(
            matches!(&status, EffectStatus::Failed(r) if r.contains("fake frame failure")),
            "{status:?}"
        );
        let reason = rig.effect.denoise_status().reason().unwrap().to_string();
        assert!(reason.contains("fake denoise frame failure"), "{reason}");
        // The failure stays: no request starts behind it.
        std::thread::sleep(Duration::from_millis(100));
        for _ in 0..3 {
            assert_eq!(rig.frame(), want, "{fallback:?}");
        }
        assert!(matches!(rig.effect.status(), EffectStatus::Failed(_)));
        rig.stop();
        assert_eq!(get(&fake.log.wrong_context), 0);
    }
}

/// A frame decided for an earlier request that fails in the denoise stage
/// still records the failure, while the current request asks for the stage
/// (as the first frame after turning the stage on: it waited for the switch
/// under the previous request). Here the request changes while the frame is
/// in the backend, from the fake's hook.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_denoise_frame_failure_of_an_earlier_request_is_recorded() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &denoised(EffectKind::Blur));
    rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlurDenoised);
    let toggle = |fx: &ObcEffect| {
        let fx = fx.clone();
        Hook::new(move || {
            fx.set_property("denoise", false);
            fx.set_property("denoise", true);
        })
    };

    // Blur: the frame is the effect's; the failure is recorded at once.
    fake.set(|c| {
        c.fail_denoise_process = Some("fake denoise frame failure".into());
        c.on_process = Some(toggle(&rig.effect));
    });
    assert_eq!(rig.frame(), Seen::FakeBlur);
    let status = rig.effect.denoise_status();
    assert!(
        matches!(&status, EffectStatus::Failed(r) if r.contains("fake denoise frame failure")),
        "{status:?}"
    );
    // The worker settles on the effective request (the blur alone); then
    // the stage would work again, but it is no longer loaded.
    for _ in 0..10 {
        assert_eq!(rig.frame(), Seen::FakeBlur);
        std::thread::sleep(Duration::from_millis(30));
    }
    fake.set(|c| c.fail_denoise_process = None);
    let denoised_frames = rig.effect.stats().frames_denoised;
    for _ in 0..5 {
        assert_eq!(rig.frame(), Seen::FakeBlur, "processed, without the stage");
    }
    assert_eq!(rig.effect.stats().frames_denoised, denoised_frames);
    assert_eq!(rig.effect.status(), EffectStatus::Ready);
    assert_eq!(rig.effect.denoise_status(), status, "still failed");

    // The stage alone: the frame is the input; the failure is recorded at
    // once, and the worker unloads: the input passes through, untouched.
    fake.set(|c| c.fail_denoise_process = None);
    rig.effect.apply(&denoised(EffectKind::None)).unwrap();
    rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeDenoise);
    let unloads = get(&fake.log.unloads);
    fake.set(|c| {
        c.fail_denoise_process = Some("fake denoise frame failure".into());
        c.on_process = Some(toggle(&rig.effect));
    });
    assert_eq!(rig.frame(), Seen::Input);
    assert!(denoise_failed(&rig), "{:?}", rig.effect.denoise_status());
    rig.frames_until(Duration::from_secs(5), |_| get(&fake.log.unloads) > unloads);
    std::thread::sleep(Duration::from_millis(100));
    rig.out_memory.lock().unwrap().clear();
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Input);
    }
    assert!(
        rig.out_memory
            .lock()
            .unwrap()
            .iter()
            .all(|m| *m == rig.in_memory),
        "passed through, not copied with a chain loaded"
    );
    assert!(denoise_failed(&rig));
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
    assert_eq!(get(&fake.log.wrong_stream), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_the_green_screen_from_none_with_denoise_loads_black_without_blocking() {
    let load = Duration::from_millis(800);
    let fake = FakeFactory::new(FakeConfig {
        load_delay: load,
        switch_delay: Duration::from_millis(50),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &denoised(EffectKind::None));
    rig.frames_until(Duration::from_secs(10), denoise_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeDenoise);

    // none + denoise -> blur + denoise: the green screen loads cold. Black,
    // never the (denoised) input, and no frame waits for the load.
    rig.effect.apply(&denoised(EffectKind::Blur)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    let mut slowest = Duration::ZERO;
    while rig.effect.status() != EffectStatus::Ready {
        assert!(Instant::now() < deadline, "timed out: {seen:?}");
        let t0 = Instant::now();
        seen.push(rig.frame());
        slowest = slowest.max(t0.elapsed());
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(seen.len() >= 10, "{seen:?}");
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    assert!(
        slowest < load / 2,
        "a frame waited {slowest:?} for the {load:?} load"
    );
    assert_eq!(rig.frame(), Seen::FakeBlurDenoised);

    // Switches on the loaded chain: never black, never the input.
    let switches = [
        (EffectSettings::of(EffectKind::Blur), Seen::FakeBlur),
        (denoised(EffectKind::Blur), Seen::FakeBlurDenoised),
        (denoised(EffectKind::None), Seen::FakeDenoise),
    ];
    for (settings, want) in switches {
        let loads = fake.log.requests.lock().unwrap().len();
        rig.effect.apply(&settings).unwrap();
        let seen = rig.frames_until(Duration::from_secs(5), |_| {
            fake.log.requests.lock().unwrap().len() > loads
        });
        assert!(
            seen.iter()
                .all(|s| !matches!(s, Seen::Black | Seen::Input | Seen::Other)),
            "{settings:?}: {seen:?}"
        );
        assert_eq!(rig.frame(), want, "{settings:?}");
    }
    // blur + denoise -> none: the input at once.
    rig.effect.apply(&denoised(EffectKind::Blur)).unwrap();
    rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlurDenoised);
    rig.effect.apply(&EffectSettings::default()).unwrap();
    assert_eq!(rig.frame(), Seen::Input);
    assert_eq!(rig.caps_events.load(Ordering::SeqCst), 1);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
    assert_eq!(get(&fake.log.wrong_stream), 0);
    assert_eq!(get(&fake.log.creates), 1);
    assert_eq!(get(&fake.log.drops), 1);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_the_new_source_flag_resets_the_temporal_state_once() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &denoised(EffectKind::Blur));
    rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    for _ in 0..5 {
        assert_eq!(rig.frame(), Seen::FakeBlurDenoised);
    }
    assert_eq!(get(&fake.log.resets), 0, "no new source yet");
    assert_eq!(
        rig.frame_with(gst_obc::NEW_SOURCE_FLAG),
        Seen::FakeBlurDenoised
    );
    assert_eq!(get(&fake.log.resets), 1);
    for _ in 0..5 {
        assert_eq!(rig.frame(), Seen::FakeBlurDenoised);
    }
    assert_eq!(get(&fake.log.resets), 1, "one reset per new source");
    // DISCONT (the pacer's skips) is not a new source.
    rig.frame_with(gst::BufferFlags::DISCONT);
    assert_eq!(get(&fake.log.resets), 1);

    // The stage alone resets on a new source too.
    rig.effect.apply(&denoised(EffectKind::None)).unwrap();
    rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeDenoise);
    let before = get(&fake.log.resets);
    assert_eq!(rig.frame_with(gst_obc::NEW_SOURCE_FLAG), Seen::FakeDenoise);
    assert_eq!(get(&fake.log.resets), before + 1);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// What the `maxine_*` tests need before the factory can find the SDK.
const MAXINE_SETUP: &str = "this test needs the Maxine SDK: install the maxine-vfx-sdk-local \
     package and load its environment (set -a; . /usr/share/maxine-vfx/maxine-vfx.env; set +a), \
     or set MAXINE_VFX_ROOT, LD_LIBRARY_PATH and MAXINE_OS_RELEASE as omarchy-nvidia-sdks \
     docs/usage.md describes (crates/obc-backend-maxine/tests/hardware.rs has the setup)";

/// The Maxine mask-hardening kernel fails closed through the element: a
/// kernel that does not load fails the effect, and a kernel launch that
/// fails on a frame blacks that frame, both with the reason in the status.
/// The faults are real driver errors (garbage PTX, an oversized thread
/// block), forced through the backend's test-only switch.
#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK (MAXINE_VFX_ROOT and its libraries on \
            LD_LIBRARY_PATH), the os-release override, an NVIDIA GPU and the GStreamer CUDA \
            elements"]
fn maxine_mask_kernel_failures_show_black_with_the_reason() {
    let switch = MaskFaultSwitch::default();
    let maxine = MaxineFactory::from_env().with_mask_faults(switch.clone());
    assert!(maxine.root().is_some(), "{MAXINE_SETUP}");
    maxine
        .probe()
        .unwrap_or_else(|e| panic!("Maxine probe failed: {e}"));
    let mut rig = Rig::with_factory(Some(Arc::new(maxine)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);

    // The kernel does not load: the load fails, black, status failed.
    switch.set(Some(MaskFault::ModuleLoad));
    rig.effect
        .apply(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(60), failed);
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    let reason = rig.effect.status().reason().unwrap().to_string();
    println!("forced module-load failure: {reason}");
    assert!(reason.contains("cuModuleLoadData"), "{reason}");
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Black, "a failed load stays black");
    }
    rig.drain_messages();
    assert!(
        rig.messages
            .iter()
            .any(|(e, s, r)| e == "blur" && s == "failed" && r.contains("cuModuleLoadData")),
        "{:?}",
        rig.messages
    );

    // A good load runs the effect; then frame launches fail: black.
    switch.set(None);
    rig.effect
        .apply(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(60), status_is(EffectStatus::Ready));
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    let effect_frame = rig.frame();
    assert!(
        !matches!(effect_frame, Seen::Input | Seen::Black),
        "the blur did not run: {effect_frame:?}"
    );
    switch.set(Some(MaskFault::Launch));
    assert_eq!(rig.frame(), Seen::Black, "the failing frame is black");
    let status = rig.effect.status();
    println!("effect frame before the launch fault: {effect_frame:?}; after: {status:?}");
    assert!(
        matches!(&status, EffectStatus::Failed(r) if r.contains("cuLaunchKernel")),
        "{status:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Black, "a failed frame path stays black");
    }
    rig.stop();
}

/// The real denoise stage through the element: alone (the input until it
/// is loaded, then denoised frames), under blur (the green screen loads
/// black after the stage alone), and turned off and on while playing (a
/// switch: every frame processed), and the stage's GPU time measured once
/// frames are denoised. Needs the Maxine denoise feature
/// (`features/nvvfxdenoising` of the SDK) installed.
#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK with its denoising feature (MAXINE_VFX_ROOT and \
            its library directories on LD_LIBRARY_PATH), the os-release override, an NVIDIA \
            GPU and the GStreamer CUDA elements"]
fn maxine_denoise_alone_under_blur_and_toggled_while_playing() {
    let maxine = MaxineFactory::from_env();
    assert!(maxine.root().is_some(), "{MAXINE_SETUP}");
    maxine
        .probe()
        .unwrap_or_else(|e| panic!("Maxine probe failed: {e}"));
    let mut rig = Rig::with_factory(Some(Arc::new(maxine)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    assert!(!rig.effect.stats().cuda_active);

    // The stage alone: the input while it loads, then denoised frames.
    rig.effect.apply(&denoised(EffectKind::None)).unwrap();
    let warm = rig.frames_until(Duration::from_secs(60), |r| {
        r.effect.denoise_status() != EffectStatus::Loading
    });
    assert_eq!(
        rig.effect.denoise_status(),
        EffectStatus::Ready,
        "the denoise stage did not load"
    );
    assert!(warm.iter().all(|s| *s == Seen::Input), "{warm:?}");
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    let before = rig.effect.stats().frames_denoised;
    for _ in 0..5 {
        let s = rig.frame();
        assert!(!matches!(s, Seen::Black), "{s:?}");
    }
    assert_eq!(rig.effect.stats().frames_denoised, before + 5);
    // Read after each frame's synchronisation: the backend has the times
    // of a completed frame only.
    let p50 = rig.effect.stats().denoise_ms_p50;
    assert!(
        p50.is_some_and(|ms| ms > 0.0),
        "no denoise stage time after denoised frames"
    );

    // Blur on top: the green screen loads cold, black until ready.
    rig.effect.apply(&denoised(EffectKind::Blur)).unwrap();
    let warm = rig.frames_until(Duration::from_secs(60), |r| {
        r.effect.status() != EffectStatus::Loading
    });
    assert_eq!(rig.effect.status(), EffectStatus::Ready);
    assert_eq!(rig.effect.denoise_status(), EffectStatus::Ready);
    assert!(warm.iter().all(|s| *s == Seen::Black), "{warm:?}");
    let processed = |s: &Seen| !matches!(s, Seen::Input | Seen::Black);
    for _ in 0..5 {
        let s = rig.frame();
        assert!(processed(&s), "{s:?}");
    }

    // Off and on while playing: switches, every frame processed.
    for on in [false, true] {
        rig.effect.set_property("denoise", on);
        let want = if on {
            EffectStatus::Ready
        } else {
            EffectStatus::Off
        };
        let seen = rig.frames_until(Duration::from_secs(60), denoise_is(want));
        for _ in 0..5 {
            let s = rig.frame();
            assert!(processed(&s), "denoise {on}: {s:?}");
        }
        assert!(seen.iter().all(processed), "denoise {on}: {seen:?}");
        assert_eq!(rig.effect.status(), EffectStatus::Ready);
    }
    // A new source resets the temporal state; the frame is processed.
    let s = rig.frame_with(gst_obc::NEW_SOURCE_FLAG);
    assert!(processed(&s), "{s:?}");

    // none: the input at once.
    rig.effect.apply(&EffectSettings::default()).unwrap();
    assert_eq!(rig.frame(), Seen::Input);
    assert_eq!(rig.caps_events.load(Ordering::SeqCst), 1);
    rig.drain_messages();
    assert!(
        rig.messages.iter().all(|m| m.1 != "failed")
            && rig.denoise_messages.iter().all(|m| m.0 != "failed"),
        "{:?} {:?}",
        rig.messages,
        rig.denoise_messages
    );
    rig.stop();
    assert!(!rig.effect.stats().cuda_active);
}
