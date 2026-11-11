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
#[path = "support/fake_chain.rs"]
mod fake_chain;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_backend::{
    FAKE_BLUR_Y, FAKE_DENOISE_MS, FAKE_DENOISE_UV, FAKE_DENOISE_Y, FAKE_RELIGHT_DY,
    FAKE_RELIGHT_MS, FAKE_RELIGHT_Y, FAKE_REPLACE_Y, FakeConfig, FakeFactory, FakeLog, Hook,
};
use gst::prelude::*;
use gst_obc::{
    EffectFallback, EffectKind, EffectSettings, EffectStatus, ObcEffect, StageId, StageRequest,
    Value,
};
use gst_video::prelude::*;
use obc_backend::BackendFactory;
use obc_backend_maxine::{MaskFault, MaskFaultSwitch, MaxineFactory, RelightLight};

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
    /// The fake's relight stage without an effect (with or without
    /// denoise), and under an effect.
    FakeRelight,
    FakeRelightDenoised,
    FakeBlurRelit,
    FakeReplaceRelit,
    FakeBlurRelitDenoised,
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
    } else if all(y, FAKE_RELIGHT_Y) && all(uv, 128) {
        Seen::FakeRelight
    } else if all(y, FAKE_RELIGHT_Y) && all(uv, FAKE_DENOISE_UV) {
        Seen::FakeRelightDenoised
    } else if all(y, FAKE_BLUR_Y + FAKE_RELIGHT_DY) && all(uv, 128) {
        Seen::FakeBlurRelit
    } else if all(y, FAKE_REPLACE_Y + FAKE_RELIGHT_DY) && all(uv, 128) {
        Seen::FakeReplaceRelit
    } else if all(y, FAKE_BLUR_Y + FAKE_RELIGHT_DY) && all(uv, FAKE_DENOISE_UV) {
        Seen::FakeBlurRelitDenoised
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
    /// The `relight` and `relight-reason` fields of the same messages.
    relight_messages: Vec<(String, String)>,
    /// The `fallback-image-error` field of the same messages.
    fallback_messages: Vec<String>,
}

impl Rig {
    fn new(factory: Option<Arc<FakeFactory>>, settings: &EffectSettings) -> Rig {
        Rig::with_factory(factory.map(|f| f as Arc<dyn BackendFactory>), settings)
    }

    fn with_factory(factory: Option<Arc<dyn BackendFactory>>, settings: &EffectSettings) -> Rig {
        Rig::with_downstream(factory, settings, "")
    }

    /// The rig with `downstream` (elements, each followed by `!`) between
    /// the effect and `cudadownload`.
    fn with_downstream(
        factory: Option<Arc<dyn BackendFactory>>,
        settings: &EffectSettings,
        downstream: &str,
    ) -> Rig {
        require_elements(&["cudaupload", "cudadownload", "appsrc", "appsink"]);
        gst_obc::register().unwrap();
        let (input, ctx) = upload();
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=src format=time is-live=true caps=\"{CANONICAL}\" ! \
             obceffect name=fx ! {downstream}cudadownload ! video/x-raw,format=NV12 ! \
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
            relight_messages: Vec::new(),
            fallback_messages: Vec::new(),
        }
    }

    /// Pushes the input once more (a shallow copy, as a clock repeat) and
    /// returns what came out.
    fn frame(&mut self) -> Seen {
        self.frame_with(gst::BufferFlags::empty())
    }

    /// [`Self::frame`] with `flags` set on the pushed buffer.
    fn frame_with(&mut self, flags: gst::BufferFlags) -> Seen {
        let bytes = self.bytes_with(flags);
        classify(&bytes, &self.input_bytes)
    }

    /// [`Self::frame_with`], the output's packed bytes.
    fn bytes_with(&mut self, flags: gst::BufferFlags) -> Vec<u8> {
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
        packed(&sample)
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
                // A stage the element was never given has no field: off.
                let stage = |id: &str| {
                    (
                        s.get::<String>(id).unwrap_or_else(|_| "off".into()),
                        s.get::<String>(format!("{id}-reason").as_str())
                            .unwrap_or_default(),
                    )
                };
                self.denoise_messages.push(stage("denoise"));
                self.relight_messages.push(stage("relight"));
                self.fallback_messages
                    .push(s.get::<String>("fallback-image-error").unwrap());
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
    move |r| r.effect.stage_status("denoise") == want
}

fn denoise_failed(r: &Rig) -> bool {
    matches!(r.effect.stage_status("denoise"), EffectStatus::Failed(_))
}

/// `kind` with the denoise stage.
fn denoised(kind: EffectKind) -> EffectSettings {
    with_denoise(EffectSettings::of(kind))
}

/// `s` with the denoise stage on, first in the list (before relight, as the
/// daemon lists them).
fn with_denoise(mut s: EffectSettings) -> EffectSettings {
    if s.stage("denoise").is_none() {
        s.stages.insert(0, StageRequest::new(StageId::DENOISE));
    }
    s.stage_mut(&StageId::DENOISE).enabled = true;
    s
}

/// Applies the element's settings with stage `id` on or off (denoise listed
/// first if it was not listed), as a client does.
fn set_stage(fx: &ObcEffect, id: &StageId, on: bool) {
    let mut s = fx.settings();
    if *id == StageId::DENOISE && s.stage("denoise").is_none() {
        s.stages.insert(0, StageRequest::new(StageId::DENOISE));
    }
    s.stage_mut(id).enabled = on;
    fx.apply(&s).unwrap();
}

/// Applies the element's settings with the relight stage's light `light`.
fn set_light(fx: &ObcEffect, light: &str) {
    let mut s = fx.settings();
    s.stage_mut(&StageId::RELIGHT)
        .params
        .insert("light".into(), light.into());
    fx.apply(&s).unwrap();
}

/// Applies the element's settings with the relight stage's live parameter
/// `key` at `value`.
fn set_live(fx: &ObcEffect, key: &str, value: Value) {
    let mut s = fx.settings();
    s.stage_mut(&StageId::RELIGHT)
        .live
        .insert(key.into(), value);
    fx.apply(&s).unwrap();
}

fn relight_is(want: EffectStatus) -> impl Fn(&Rig) -> bool {
    move |r| r.effect.stage_status("relight") == want
}

fn relight_failed(r: &Rig) -> bool {
    matches!(r.effect.stage_status("relight"), EffectStatus::Failed(_))
}

/// `kind` with the relight stage, with `light`.
fn relit(kind: EffectKind, light: RelightLight) -> EffectSettings {
    EffectSettings::of(kind)
        .with_stage(StageRequest::on(StageId::RELIGHT).with_param("light", light.as_str()))
}

fn get(a: &AtomicU64) -> u64 {
    FakeLog::get(a)
}

/// A background PNG for replace.
fn background(dir: &Path) -> std::path::PathBuf {
    // Tests call this before the rig exists, which is what inits otherwise.
    gst::init().unwrap();
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
    // The frame decided just after the status turned ready may already be
    // processed: the status is read before the frame is taken.
    let black = warm.iter().take_while(|s| **s == Seen::Black).count();
    assert!(
        black + 1 >= warm.len() && warm[black..].iter().all(|s| *s == Seen::FakeBlur),
        "{warm:?}"
    );
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
    // The frame decided just after the status turned ready may already be
    // processed: the status is read before the frame is taken.
    let black = again.iter().take_while(|s| **s == Seen::Black).count();
    assert!(
        black + 1 >= again.len() && again[black..].iter().all(|s| *s == Seen::FakeBlur),
        "{again:?}"
    );
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

/// A white PNG of `width x height` for the fallback image (its NV12 in the
/// rig's limited range: Y 235, U = V = 128).
fn white_png(dir: &Path, width: u32, height: u32) -> std::path::PathBuf {
    gst::init().unwrap();
    let path = dir.join("away.png");
    let p = gst::parse::launch(&format!(
        "videotestsrc num-buffers=1 pattern=white ! video/x-raw,format=RGB,width={width},\
         height={height} ! pngenc snapshot=true ! filesink location={}",
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

/// Every Y byte 235 and every chroma byte 128: the white fallback image.
fn is_white(frame: &[u8]) -> bool {
    let (y, uv) = frame.split_at((W * H) as usize);
    y.iter().all(|b| *b == 235) && uv.iter().all(|b| *b == 128)
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_on_failure_image_shows_the_fallback_image_where_black_would_be() {
    let dir = tempfile::tempdir().unwrap();
    // Another aspect ratio than the frames': cropped to fill them.
    let image = white_png(dir.path(), 400, 400);
    let fake = FakeFactory::new(FakeConfig {
        fail_load: Some("fake load failure".into()),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    let settings = EffectSettings {
        fallback: EffectFallback::Image,
        fallback_image: Some(image.clone()),
        ..EffectSettings::of(EffectKind::Blur)
    };
    rig.effect.apply(&settings).unwrap();
    // Black until the image is decoded, then the image: never the input.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let bytes = rig.bytes_with(gst::BufferFlags::empty());
        if is_white(&bytes) && failed(&rig) {
            break;
        }
        assert!(
            classify(&bytes, &rig.input_bytes) == Seen::Black || is_white(&bytes),
            "only black or the image: {:?}",
            classify(&bytes, &rig.input_bytes)
        );
        assert!(Instant::now() < deadline, "the image never showed");
        std::thread::sleep(Duration::from_millis(30));
    }
    for _ in 0..3 {
        assert!(is_white(&rig.bytes_with(gst::BufferFlags::empty())));
    }
    let stats = rig.effect.stats();
    assert_eq!(stats.fallback_error, None);
    assert!(stats.frames_black > 0, "the image counts as a hidden frame");
    // A process failure after a good load: the failing frame is the image.
    fake.set(|c| c.fail_load = None);
    rig.effect.apply(&settings).unwrap();
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    fake.set(|c| c.fail_process = Some("fake frame failure".into()));
    assert!(is_white(&rig.bytes_with(gst::BufferFlags::empty())));
    assert!(failed(&rig));
    // on-failure=black again: black, the image left unused.
    rig.effect
        .apply(&EffectSettings {
            fallback: EffectFallback::Black,
            ..settings.clone()
        })
        .unwrap();
    rig.frames_until(Duration::from_secs(5), failed);
    assert_eq!(rig.frame(), Seen::Black);
    assert_eq!(rig.caps_events.load(Ordering::SeqCst), 1);
    rig.stop();
    assert!(!rig.effect.stats().cuda_active);
    assert_eq!(get(&fake.log.wrong_context), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_an_unreadable_fallback_image_shows_black_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("gone.png");
    let fake = FakeFactory::new(FakeConfig {
        fail_load: Some("fake load failure".into()),
        ..FakeConfig::default()
    });
    let settings = EffectSettings {
        fallback: EffectFallback::Image,
        fallback_image: Some(missing.clone()),
        ..EffectSettings::of(EffectKind::Blur)
    };
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &settings);
    let seen = rig.frames_until(Duration::from_secs(10), |r| {
        failed(r) && r.effect.stats().fallback_error.is_some()
    });
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Black);
    }
    let why = rig.effect.stats().fallback_error.unwrap();
    assert!(
        why.starts_with(&format!(
            "Fallback image {} could not be read (",
            missing.display()
        )) && why.ends_with("): showing black")
            && why.contains("unreadable"),
        "{why}"
    );
    // The status message carries it.
    rig.drain_messages();
    assert_eq!(
        rig.fallback_messages.last(),
        Some(&why),
        "{:?}",
        rig.messages
    );
    // Another fallback: no report any more.
    rig.effect
        .apply(&EffectSettings {
            fallback: EffectFallback::Black,
            ..settings
        })
        .unwrap();
    assert_eq!(rig.effect.stats().fallback_error, None);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_fallback_image_put_at_its_path_is_read_when_the_settings_are_applied_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("away.png");
    let fake = FakeFactory::new(FakeConfig {
        fail_load: Some("fake load failure".into()),
        ..FakeConfig::default()
    });
    let settings = EffectSettings {
        fallback: EffectFallback::Image,
        fallback_image: Some(path.clone()),
        ..EffectSettings::of(EffectKind::Blur)
    };
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &settings);
    rig.frames_until(Duration::from_secs(10), |r| {
        failed(r) && r.effect.stats().fallback_error.is_some()
    });
    assert_eq!(rig.frame(), Seen::Black);
    // The image put at the same path: the same settings applied again
    // read it.
    assert_eq!(white_png(dir.path(), 400, 400), path);
    rig.effect.apply(&settings).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !is_white(&rig.bytes_with(gst::BufferFlags::empty())) {
        assert!(Instant::now() < deadline, "the image never showed");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(rig.effect.stats().fallback_error, None);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
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
fn gpu_a_backend_that_lost_its_chain_is_loaded_again_on_a_retry() {
    let fake = FakeFactory::new(FakeConfig::default());
    let blur = EffectSettings::of(EffectKind::Blur);
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &blur);
    rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    let loads = get(&fake.log.loads);

    // The backend loses its chain on its own (a module instance unloaded
    // after a panic): the frame fails, the effect is failed.
    fake.set(|c| c.lose_chain_on_process = Some("the module panicked".into()));
    assert_eq!(rig.frame(), Seen::Black);
    assert_eq!(get(&fake.log.lost_chains), 1);
    assert!(matches!(rig.effect.status(), EffectStatus::Failed(_)));
    // Nothing runs on the lost chain: black, the backend not asked.
    let processes = get(&fake.log.processes);
    assert_eq!(rig.frame(), Seen::Black);
    assert_eq!(get(&fake.log.processes), processes);

    // A retry of the same effect loads it again, rather than taking what
    // was loaded last as still loaded and saying ready.
    rig.effect.apply(&blur).unwrap();
    rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert_eq!(get(&fake.log.loads), loads + 1, "loaded again");
    assert_eq!(get(&fake.log.lost_chains), 1, "no further chain was lost");
    assert_eq!(
        fake.log.loaded.lock().unwrap().as_slice(),
        [EffectKind::Blur, EffectKind::Blur],
        "the chain was loaded, lost and loaded again"
    );
    assert_eq!(rig.frame(), Seen::FakeBlur);
    rig.stop();
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
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Loading);
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
    assert!(
        s.stage(&StageId::DENOISE).frames >= 5
            && s.stage(&StageId::DENOISE).frames == s.frames_processed
    );
    assert_eq!(
        s.stage(&StageId::DENOISE).ms_p50,
        Some(f64::from(FAKE_DENOISE_MS))
    );
    assert_eq!(
        *fake.log.requests.lock().unwrap(),
        [(EffectKind::None, true)]
    );

    // Off again: the input at once; the worker unloads.
    set_stage(&rig.effect, &StageId::DENOISE, false);
    assert_eq!(rig.frame(), Seen::Input);
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(&fake.log.unloads) == 0 {
        assert!(Instant::now() < deadline, "no unload");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rig.frame(), Seen::Input);
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Off);
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
        rig.effect.stage_status("denoise"),
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
    set_stage(&rig.effect, &StageId::DENOISE, false);
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Off);
    set_stage(&rig.effect, &StageId::DENOISE, true);
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Loading);
    let seen = rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert!(seen.iter().all(|s| *s == Seen::Input), "{seen:?}");
    assert_eq!(rig.frame(), Seen::FakeDenoise);

    // A retry (apply, unchanged settings) clears a recorded failure too.
    // (Off, and once unloaded on again, so the stage loads anew: turned on
    // before the unload it would still be loaded.)
    fake.set(|c| c.fail_denoise_load = Some("fake denoise unavailable".into()));
    let unloads = get(&fake.log.unloads);
    set_stage(&rig.effect, &StageId::DENOISE, false);
    rig.frames_until(Duration::from_secs(5), |_| get(&fake.log.unloads) > unloads);
    set_stage(&rig.effect, &StageId::DENOISE, true);
    rig.frames_until(Duration::from_secs(5), denoise_failed);
    assert_eq!(rig.frame(), Seen::Input);
    fake.set(|c| c.fail_denoise_load = None);
    rig.effect.apply(&denoised(EffectKind::None)).unwrap();
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Loading);
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
        bare.effect.stage_status("denoise"),
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
        rig.effect.stage_status("denoise"),
        EffectStatus::Failed("fake denoise unavailable".into())
    );
    assert_eq!(rig.effect.status(), EffectStatus::Ready);
    // The failed attempt with the stage loaded nothing; the blur alone did.
    assert_eq!(
        *fake.log.requests.lock().unwrap(),
        [(EffectKind::Blur, false)]
    );
    assert!(rig.effect.stats().frames_processed >= 3);
    assert_eq!(rig.effect.stats().stage(&StageId::DENOISE).frames, 0);

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
        let reason = rig
            .effect
            .stage_status("denoise")
            .reason()
            .unwrap()
            .to_string();
        assert!(reason.contains("fake denoise frame failure"), "{reason}");
        let after = rig.effect.stats();
        assert_eq!(after.frames_processed, before.frames_processed + 4);
        assert_eq!(after.frames_black, before.frames_black, "{fallback:?}");
        assert_eq!(
            after.frames_passthrough, before.frames_passthrough,
            "never the input, {fallback:?}"
        );
        assert_eq!(
            after.stage(&StageId::DENOISE).frames,
            before.stage(&StageId::DENOISE).frames
        );
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
        let reason = rig
            .effect
            .stage_status("denoise")
            .reason()
            .unwrap()
            .to_string();
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
            set_stage(&fx, &StageId::DENOISE, false);
            set_stage(&fx, &StageId::DENOISE, true);
        })
    };

    // Blur: the frame is the effect's; the failure is recorded at once.
    fake.set(|c| {
        c.fail_denoise_process = Some("fake denoise frame failure".into());
        c.on_process = Some(toggle(&rig.effect));
    });
    assert_eq!(rig.frame(), Seen::FakeBlur);
    let status = rig.effect.stage_status("denoise");
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
    let denoised_frames = rig.effect.stats().stage(&StageId::DENOISE).frames;
    for _ in 0..5 {
        assert_eq!(rig.frame(), Seen::FakeBlur, "processed, without the stage");
    }
    assert_eq!(
        rig.effect.stats().stage(&StageId::DENOISE).frames,
        denoised_frames
    );
    assert_eq!(rig.effect.status(), EffectStatus::Ready);
    assert_eq!(rig.effect.stage_status("denoise"), status, "still failed");

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
    assert!(
        denoise_failed(&rig),
        "{:?}",
        rig.effect.stage_status("denoise")
    );
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

    // The stage alone resets on a new source too. Blur to none is a
    // switch that keeps the ready stage ready, so its status does not mark
    // the switch's end: the backend's record of the request does.
    let loads = fake.log.requests.lock().unwrap().len();
    rig.effect.apply(&denoised(EffectKind::None)).unwrap();
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Ready);
    let seen = rig.frames_until(Duration::from_secs(5), |_| {
        fake.log.requests.lock().unwrap().len() > loads
    });
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeBlurDenoised | Seen::FakeDenoise)),
        "{seen:?}"
    );
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Ready);
    assert_eq!(rig.frame(), Seen::FakeDenoise);
    let before = get(&fake.log.resets);
    assert_eq!(rig.frame_with(gst_obc::NEW_SOURCE_FLAG), Seen::FakeDenoise);
    assert_eq!(get(&fake.log.resets), before + 1);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_none_with_relight_passes_through_until_loaded_then_relights() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(500),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    rig.effect
        .apply(&relit(EffectKind::None, RelightLight::Warm))
        .unwrap();
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    assert_eq!(rig.effect.stage_status("relight"), EffectStatus::Loading);
    let warm = rig.frames_until(Duration::from_secs(10), relight_is(EffectStatus::Ready));
    assert!(
        warm.iter().all(|s| *s == Seen::Input),
        "the input while the stage loads, never black: {warm:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::FakeRelight);
    }
    // Direction and gain reach the backend on the next frame, without a
    // load.
    let loads = get(&fake.log.loads);
    set_live(&rig.effect, "direction", Value::Float(-120.0));
    set_live(&rig.effect, "brightness", Value::Float(175.0));
    assert_eq!(rig.frame(), Seen::FakeRelight);
    assert_eq!(
        *fake.log.last_relight_milli.lock().unwrap(),
        Some((-120_000, 1_750))
    );
    assert_eq!(get(&fake.log.loads), loads, "no load for direction or gain");
    assert_eq!(rig.effect.stage_status("relight"), EffectStatus::Ready);
    let s = rig.effect.stats();
    assert_eq!(s.frames_black, 0);
    assert!(
        s.stage(&StageId::RELIGHT).frames >= 4
            && s.stage(&StageId::RELIGHT).frames == s.frames_processed
    );
    assert_eq!(s.stage(&StageId::DENOISE).frames, 0);
    assert_eq!(
        s.stage(&StageId::RELIGHT).ms_p50,
        Some(f64::from(FAKE_RELIGHT_MS))
    );
    assert_eq!(s.stage(&StageId::DENOISE).ms_p50, None);
    assert_eq!(
        *fake.log.stage_requests.lock().unwrap(),
        [(EffectKind::None, vec![StageId::RELIGHT], RelightLight::Warm)]
    );

    // With denoise too: both stages, in order, in one switch.
    rig.effect
        .apply(&with_denoise(relit(EffectKind::None, RelightLight::Warm)))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), denoise_is(EffectStatus::Ready));
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeRelight | Seen::FakeRelightDenoised)),
        "{seen:?}"
    );
    assert_eq!(rig.frame(), Seen::FakeRelightDenoised);
    assert_eq!(
        fake.log.stage_requests.lock().unwrap().last().unwrap(),
        &(
            EffectKind::None,
            vec![StageId::DENOISE, StageId::RELIGHT],
            RelightLight::Warm
        )
    );

    // Off again: the input at once; the worker unloads.
    rig.effect.apply(&EffectSettings::default()).unwrap();
    assert_eq!(rig.frame(), Seen::Input);
    assert_eq!(rig.effect.stage_status("relight"), EffectStatus::Off);
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(&fake.log.unloads) == 0 {
        assert!(Instant::now() < deadline, "no unload");
        std::thread::sleep(Duration::from_millis(10));
    }
    rig.drain_messages();
    let relight: Vec<&str> = rig.relight_messages.iter().map(|m| m.0.as_str()).collect();
    assert_eq!(relight.first(), Some(&"loading"), "{relight:?}");
    assert_eq!(relight.last(), Some(&"off"), "{relight:?}");
    assert!(!relight.contains(&"failed"), "{relight:?}");
    assert!(
        rig.messages.iter().all(|m| m.1 == "off"),
        "{:?}",
        rig.messages
    );
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
    assert_eq!(get(&fake.log.wrong_stream), 0);
}

/// Turning the stage on and off, and changing its light, under an effect
/// are switches on the loaded chain: the effect runs throughout, never
/// black and never the input, and no switch unloads anything.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_relight_toggles_and_new_lights_are_switches_on_the_loaded_chain() {
    let fake = FakeFactory::new(FakeConfig {
        switch_delay: Duration::from_millis(50),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    let start = rig.effect.stats();
    let effect_ok = |s: &Seen| matches!(s, Seen::FakeBlur | Seen::FakeBlurRelit);
    for light in [
        RelightLight::Warm,
        RelightLight::Cool,
        RelightLight::Neutral,
    ] {
        rig.effect.apply(&relit(EffectKind::Blur, light)).unwrap();
        assert_eq!(
            rig.effect.status(),
            EffectStatus::Ready,
            "the effect is unchanged"
        );
        let seen = rig.frames_until(Duration::from_secs(5), relight_is(EffectStatus::Ready));
        assert!(seen.iter().all(effect_ok), "{light}: {seen:?}");
        assert_eq!(rig.frame(), Seen::FakeBlurRelit, "{light}");
        assert_eq!(
            fake.log.stage_requests.lock().unwrap().last().unwrap(),
            &(EffectKind::Blur, vec![StageId::RELIGHT], light)
        );
    }
    // The property does the same.
    set_light(&rig.effect, "warm");
    rig.frames_until(Duration::from_secs(5), relight_is(EffectStatus::Ready));
    assert_eq!(
        fake.log.stage_requests.lock().unwrap().last().unwrap().2,
        RelightLight::Warm
    );
    set_stage(&rig.effect, &StageId::RELIGHT, false);
    assert_eq!(rig.effect.stage_status("relight"), EffectStatus::Off);
    let blur_alone = |_: &Rig| {
        fake.log.stage_requests.lock().unwrap().last().unwrap()
            == &(EffectKind::Blur, vec![], RelightLight::Neutral)
    };
    let seen = rig.frames_until(Duration::from_secs(5), blur_alone);
    assert!(seen.iter().all(effect_ok), "{seen:?}");
    assert_eq!(rig.frame(), Seen::FakeBlur);
    // Blur to replace with the stage keeps it.
    let dir = tempfile::tempdir().unwrap();
    let replace = EffectSettings {
        background: Some(background(dir.path())),
        ..relit(EffectKind::Replace, RelightLight::Neutral)
    };
    rig.effect.apply(&replace).unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeBlur | Seen::FakeReplaceRelit)),
        "{seen:?}"
    );
    rig.frames_until(Duration::from_secs(5), relight_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeReplaceRelit);
    assert_eq!(get(&fake.log.unloads), 0, "no switch unloaded anything");
    let s = rig.effect.stats();
    assert_eq!(s.frames_black, start.frames_black, "never black");
    assert_eq!(
        s.frames_passthrough, start.frames_passthrough,
        "never the input"
    );
    rig.drain_messages();
    assert!(
        rig.messages.iter().all(|m| m.1 != "failed"),
        "{:?}",
        rig.messages
    );
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// `kind` with the stages `denoise` and `relight` (with `light`).
fn staged(kind: EffectKind, denoise: bool, relight: bool, light: RelightLight) -> EffectSettings {
    EffectSettings {
        stages: vec![
            StageRequest {
                enabled: denoise,
                ..StageRequest::new(StageId::DENOISE)
            },
            StageRequest {
                enabled: relight,
                ..StageRequest::new(StageId::RELIGHT).with_param("light", light.as_str())
            },
        ],
        ..EffectSettings::of(kind)
    }
}

/// A change of one stage (its light, or the stage on or off) loads or
/// releases that stage alone, under `none` and under blur: a new light is a
/// rebind of the loaded stage (no model load), turning a stage on builds
/// that stage only, turning one off builds nothing, and the other stage
/// keeps its model and its ready status throughout (the daemon follows the
/// statuses: a stage shown loading again reads as a reload).
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_stage_change_loads_or_releases_that_stage_alone() {
    for kind in [EffectKind::None, EffectKind::Blur] {
        let fake = FakeFactory::new(FakeConfig {
            switch_delay: Duration::from_millis(20),
            ..FakeConfig::default()
        });
        let mut rig = Rig::new(
            Some(Arc::clone(&fake)),
            &staged(kind, true, true, RelightLight::Neutral),
        );
        let ready = |r: &Rig| {
            r.effect.stage_status("denoise") == EffectStatus::Ready
                && r.effect.stage_status("relight") == EffectStatus::Ready
        };
        rig.frames_until(Duration::from_secs(5), ready);
        let start = rig.effect.stats();
        let counts = || {
            (
                get(&fake.log.denoise_builds),
                get(&fake.log.relight_builds),
                get(&fake.log.relight_rebinds),
            )
        };
        assert_eq!(counts(), (1, 1, 0), "{kind}: the first load");
        let last = || {
            fake.log
                .stage_requests
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap()
        };
        // Each new light: one rebind, no model.
        let mut rebinds = 0;
        for light in [
            RelightLight::Warm,
            RelightLight::Cool,
            RelightLight::Neutral,
        ] {
            rig.effect.apply(&staged(kind, true, true, light)).unwrap();
            assert_eq!(
                rig.effect.stage_status("denoise"),
                EffectStatus::Ready,
                "{kind}, {light}: denoise is untouched"
            );
            rig.frames_until(Duration::from_secs(5), |r| last().2 == light && ready(r));
            rebinds += 1;
            assert_eq!(counts(), (1, 1, rebinds), "{kind}, {light}");
        }
        // Denoise off, then on: relight keeps its model and its status;
        // relight off, then on: denoise does.
        let steps = [
            (StageId::DENOISE, false, (1, 1)),
            (StageId::DENOISE, true, (2, 1)),
            (StageId::RELIGHT, false, (2, 1)),
            (StageId::RELIGHT, true, (2, 2)),
        ];
        for (toggled, on, (d, r)) in steps {
            let label = format!("{kind}, {toggled} {}", if on { "on" } else { "off" });
            let (denoise, relight) = if toggled == StageId::DENOISE {
                (on, true)
            } else {
                (true, on)
            };
            rig.effect
                .apply(&staged(kind, denoise, relight, RelightLight::Neutral))
                .unwrap();
            let other = if toggled == StageId::DENOISE {
                rig.effect.stage_status("relight")
            } else {
                rig.effect.stage_status("denoise")
            };
            assert_eq!(other, EffectStatus::Ready, "{label}: the other stage");
            let stages: Vec<StageId> = [StageId::DENOISE, StageId::RELIGHT]
                .into_iter()
                .filter(|s| *s != toggled || on)
                .collect();
            rig.frames_until(Duration::from_secs(5), |rg| {
                last().1 == stages
                    && (!denoise || rg.effect.stage_status("denoise") == EffectStatus::Ready)
                    && (!relight || rg.effect.stage_status("relight") == EffectStatus::Ready)
            });
            assert_eq!(counts(), (d, r, rebinds), "{label}");
        }
        assert_eq!(
            get(&fake.log.unloads),
            0,
            "{kind}: no switch unloaded anything"
        );
        let s = rig.effect.stats();
        assert_eq!(s.frames_black, start.frames_black, "{kind}: never black");
        rig.stop();
        assert_eq!(get(&fake.log.wrong_context), 0);
    }
}

/// A relight load failure drops the stage, never the effect: blur and
/// replace run without it, `none` passes the input through, each with the
/// reason in the relight status; changing the light retries.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_relight_load_failure_runs_the_chain_without_it() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(100),
        fail_relight_load: Some("fake relight unavailable".into()),
        ..FakeConfig::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let bg = background(dir.path());
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    for (kind, want) in [
        (EffectKind::Blur, Seen::FakeBlur),
        (EffectKind::Replace, Seen::FakeReplace),
    ] {
        let settings = EffectSettings {
            background: Some(bg.clone()),
            ..relit(kind, RelightLight::Cool)
        };
        rig.effect.apply(&settings).unwrap();
        rig.frames_until(Duration::from_secs(10), |r| {
            relight_failed(r) && r.effect.status() == EffectStatus::Ready
        });
        for _ in 0..3 {
            assert_eq!(rig.frame(), want, "{kind}: the effect, without the stage");
        }
        assert_eq!(
            rig.effect.stage_status("relight"),
            EffectStatus::Failed("fake relight unavailable".into())
        );
        assert_eq!(rig.effect.status(), EffectStatus::Ready, "{kind}");
    }
    assert_eq!(
        rig.effect.stats().frames_passthrough,
        1,
        "only the first frame"
    );
    // The stage alone: the input, with the reason; the effect never fails.
    let black = rig.effect.stats().frames_black;
    rig.effect
        .apply(&relit(EffectKind::None, RelightLight::Cool))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), |r| {
        relight_failed(r) && get(&fake.log.unloads) > 0
    });
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeReplace | Seen::Input)),
        "{seen:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Input);
    }
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    // A new light retries; the stage is available now.
    fake.set(|c| c.fail_relight_load = None);
    set_light(&rig.effect, "warm");
    assert_eq!(rig.effect.stage_status("relight"), EffectStatus::Loading);
    let seen = rig.frames_until(Duration::from_secs(5), relight_is(EffectStatus::Ready));
    assert!(seen.iter().all(|s| *s == Seen::Input), "{seen:?}");
    assert_eq!(rig.frame(), Seen::FakeRelight);
    assert_eq!(rig.effect.stats().frames_black, black, "never black");
    rig.drain_messages();
    assert!(
        rig.relight_messages
            .contains(&("failed".into(), "fake relight unavailable".into())),
        "{:?}",
        rig.relight_messages
    );
    assert!(
        rig.messages.iter().all(|m| m.1 != "failed"),
        "{:?}",
        rig.messages
    );
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// A relight frame failure drops the stage: under blur and replace the
/// failing frame runs again without it (never black, never the input);
/// under `none` it is the input, and so is every later frame unless
/// another stage is left; a denoise stage keeps running.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_relight_frame_failure_drops_the_stage_and_keeps_the_rest() {
    let fake = FakeFactory::new(FakeConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let bg = background(dir.path());
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    for (kind, relit_frame, plain) in [
        (EffectKind::Blur, Seen::FakeBlurRelit, Seen::FakeBlur),
        (
            EffectKind::Replace,
            Seen::FakeReplaceRelit,
            Seen::FakeReplace,
        ),
    ] {
        for fallback in [EffectFallback::Black, EffectFallback::None] {
            let label = format!("{kind} {fallback:?}");
            let settings = EffectSettings {
                fallback,
                background: Some(bg.clone()),
                ..relit(kind, RelightLight::Neutral)
            };
            rig.effect.apply(&settings).unwrap();
            rig.frames_until(Duration::from_secs(5), |r| {
                r.effect.stage_status("relight") == EffectStatus::Ready
                    && r.effect.status() == EffectStatus::Ready
            });
            assert_eq!(rig.frame(), relit_frame, "{label}");
            let before = rig.effect.stats();
            fake.set(|c| c.fail_relight_process = Some("fake relight frame failure".into()));
            assert_eq!(rig.frame(), plain, "the failing frame, {label}");
            for _ in 0..3 {
                assert_eq!(rig.frame(), plain, "the chain without the stage, {label}");
            }
            fake.set(|c| c.fail_relight_process = None);
            assert_eq!(rig.effect.status(), EffectStatus::Ready, "{label}");
            let reason = rig
                .effect
                .stage_status("relight")
                .reason()
                .unwrap()
                .to_string();
            assert!(reason.contains("fake relight frame failure"), "{reason}");
            let after = rig.effect.stats();
            assert_eq!(
                after.frames_processed,
                before.frames_processed + 4,
                "{label}"
            );
            assert_eq!(after.frames_black, before.frames_black, "{label}");
            assert_eq!(
                after.frames_passthrough, before.frames_passthrough,
                "{label}"
            );
            assert_eq!(
                after.stage(&StageId::RELIGHT).frames,
                before.stage(&StageId::RELIGHT).frames,
                "{label}"
            );
        }
    }
    // Alone under none: the failing frame and the later ones are the input.
    rig.effect
        .apply(&relit(EffectKind::None, RelightLight::Neutral))
        .unwrap();
    rig.frames_until(Duration::from_secs(5), relight_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeRelight);
    fake.set(|c| c.fail_relight_process = Some("fake relight frame failure".into()));
    assert_eq!(rig.frame(), Seen::Input, "the failing frame");
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Input);
    }
    assert!(relight_failed(&rig));
    fake.set(|c| c.fail_relight_process = None);
    // With denoise under none: the failing frame is the input, then the
    // denoise stage goes on alone.
    rig.effect
        .apply(&with_denoise(relit(EffectKind::None, RelightLight::Warm)))
        .unwrap();
    rig.frames_until(Duration::from_secs(5), |r| {
        r.effect.stage_status("relight") == EffectStatus::Ready
            && r.effect.stage_status("denoise") == EffectStatus::Ready
    });
    assert_eq!(rig.frame(), Seen::FakeRelightDenoised);
    fake.set(|c| c.fail_relight_process = Some("fake relight frame failure".into()));
    assert_eq!(rig.frame(), Seen::Input, "the failing frame");
    fake.set(|c| c.fail_relight_process = None);
    let seen = rig.frames_until(Duration::from_secs(5), |r| {
        r.effect.stage_status("denoise") == EffectStatus::Ready && relight_failed(r)
    });
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeDenoise | Seen::Input)),
        "{seen:?}"
    );
    assert_eq!(rig.frame(), Seen::FakeDenoise);
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    rig.drain_messages();
    assert!(
        rig.messages.iter().all(|m| m.1 != "failed"),
        "{:?}",
        rig.messages
    );
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// A new light the loaded stage fails to take drops the stage (the seam's
/// rule for that switch), and the element knows: the relight status says
/// failed, the effect runs on without it, and a request that comes back to
/// the old light while the failed switch is handled loads the stage again
/// instead of finding it "already loaded".
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_failed_new_light_drops_the_stage_and_the_status_says_so() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(
        Some(Arc::clone(&fake)),
        &relit(EffectKind::Blur, RelightLight::Warm),
    );
    rig.frames_until(Duration::from_secs(5), |r| {
        r.effect.stage_status("relight") == EffectStatus::Ready
            && r.effect.status() == EffectStatus::Ready
    });
    assert_eq!(rig.frame(), Seen::FakeBlurRelit);
    fake.set(|c| c.fail_relight_rebind = Some("fake rebind failure".into()));
    rig.effect
        .apply(&relit(EffectKind::Blur, RelightLight::Cool))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), relight_failed);
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeBlur | Seen::FakeBlurRelit)),
        "{seen:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::FakeBlur, "the effect without the stage");
    }
    assert_eq!(
        rig.effect.stage_status("relight"),
        EffectStatus::Failed("fake rebind failure".into())
    );
    assert_eq!(rig.effect.status(), EffectStatus::Ready);

    // Back to warm while the failed switch is being handled: the chain in
    // force is blur alone, so warm loads the stage again (a fresh build,
    // no rebind) and it runs; never ready without running.
    rig.effect
        .apply(&relit(EffectKind::Blur, RelightLight::Warm))
        .unwrap();
    rig.frames_until(Duration::from_secs(5), relight_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlurRelit);
    let builds = get(&fake.log.relight_builds);
    let effect = rig.effect.clone();
    fake.set(|c| {
        c.on_rebind_failure = Some(Hook::new(move || {
            set_light(&effect, "warm");
        }))
    });
    rig.effect
        .apply(&relit(EffectKind::Blur, RelightLight::Cool))
        .unwrap();
    rig.frames_until(Duration::from_secs(5), |r| {
        get(&fake.log.relight_builds) > builds
            && r.effect.stage_status("relight") == EffectStatus::Ready
    });
    for _ in 0..3 {
        assert_eq!(
            rig.frame(),
            Seen::FakeBlurRelit,
            "relight ready means relight running"
        );
    }
    assert_eq!(
        fake.log.stage_requests.lock().unwrap().last().unwrap(),
        &(EffectKind::Blur, vec![StageId::RELIGHT], RelightLight::Warm)
    );

    // The stage alone: the input after the failure, then an unload. Blur
    // to none keeps the ready stage ready, so its status does not mark the
    // switch's end: the backend's record of the request does.
    let loads = fake.log.requests.lock().unwrap().len();
    rig.effect
        .apply(&relit(EffectKind::None, RelightLight::Warm))
        .unwrap();
    assert_eq!(rig.effect.stage_status("relight"), EffectStatus::Ready);
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    let seen = rig.frames_until(Duration::from_secs(5), |_| {
        fake.log.requests.lock().unwrap().len() > loads
    });
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeBlurRelit | Seen::FakeRelight)),
        "{seen:?}"
    );
    assert_eq!(rig.effect.stage_status("relight"), EffectStatus::Ready);
    assert_eq!(rig.frame(), Seen::FakeRelight);
    let unloads = get(&fake.log.unloads);
    let black = rig.effect.stats().frames_black;
    rig.effect
        .apply(&relit(EffectKind::None, RelightLight::Cool))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), |r| {
        relight_failed(r) && get(&fake.log.unloads) > unloads
    });
    assert!(
        seen.iter()
            .all(|s| matches!(s, Seen::FakeRelight | Seen::Input)),
        "{seen:?}"
    );
    assert_eq!(rig.frame(), Seen::Input);
    assert_eq!(rig.effect.stats().frames_black, black, "never black");
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// A new source rebuilds the relight stage, whose state the backend cannot
/// reset: from the flagged frame on no frame is relit by the stage that saw
/// the previous source; until the new one is ready the effect runs alone
/// (never the input, never black), and the stage alone passes the input.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_new_source_rebuilds_the_relight_stage_before_it_relights_again() {
    let fake = FakeFactory::new(FakeConfig {
        switch_delay: Duration::from_millis(50),
        ..FakeConfig::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let bg = background(dir.path());
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    for (kind, relit_frame, plain) in [
        (EffectKind::Blur, Seen::FakeBlurRelit, Seen::FakeBlur),
        (
            EffectKind::Replace,
            Seen::FakeReplaceRelit,
            Seen::FakeReplace,
        ),
        (EffectKind::None, Seen::FakeRelight, Seen::Input),
    ] {
        let settings = EffectSettings {
            background: Some(bg.clone()),
            ..relit(kind, RelightLight::Neutral)
        };
        // Replace to none keeps the ready stage ready, so the statuses do
        // not mark the switch's end: the backend's record of it does.
        let loads = fake.log.requests.lock().unwrap().len();
        rig.effect.apply(&settings).unwrap();
        rig.frames_until(Duration::from_secs(5), |r| {
            fake.log.requests.lock().unwrap().len() > loads
                && r.effect.stage_status("relight") == EffectStatus::Ready
                && (kind == EffectKind::None || r.effect.status() == EffectStatus::Ready)
        });
        assert_eq!(rig.frame(), relit_frame, "{kind}");
        let builds = get(&fake.log.relight_builds);
        let relit_before = fake.log.relit_by.lock().unwrap().len();
        let black = rig.effect.stats().frames_black;
        assert_eq!(
            rig.frame_with(gst_obc::NEW_SOURCE_FLAG),
            plain,
            "{kind}: the flagged frame runs without the stage"
        );
        assert_eq!(get(&fake.log.relight_resets_dropped) as usize, {
            let k = [EffectKind::Blur, EffectKind::Replace, EffectKind::None];
            k.iter().position(|&x| x == kind).unwrap() + 1
        });
        let seen = rig.frames_until(Duration::from_secs(5), |r| {
            get(&fake.log.relight_builds) > builds
                && r.effect.stage_status("relight") == EffectStatus::Ready
        });
        assert!(
            seen.iter().all(|s| *s == plain || *s == relit_frame),
            "{kind}: {seen:?}"
        );
        assert_eq!(rig.frame(), relit_frame, "{kind}: relit again");
        assert_eq!(get(&fake.log.relight_builds), builds + 1, "{kind}");
        let by = fake.log.relit_by.lock().unwrap()[relit_before..].to_vec();
        assert!(
            !by.is_empty() && by.iter().all(|&b| b == builds + 1),
            "{kind}: relit after the new source by builds {by:?}, the old one was {builds}"
        );
        assert_eq!(
            rig.effect.stats().frames_black,
            black,
            "{kind}: never black"
        );
    }
    rig.drain_messages();
    assert!(
        rig.relight_messages.iter().all(|m| m.0 != "failed"),
        "a rebuild is no failure: {:?}",
        rig.relight_messages
    );
    // A backend that can reset the stage in place keeps it: no rebuild.
    fake.set(|c| c.relight_resets_in_place = true);
    let builds = get(&fake.log.relight_builds);
    assert_eq!(rig.frame_with(gst_obc::NEW_SOURCE_FLAG), Seen::FakeRelight);
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::FakeRelight);
    }
    assert_eq!(get(&fake.log.relight_builds), builds);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
    assert_eq!(get(&fake.log.wrong_stream), 0);
}

/// The per-frame parameters never retry what failed: after a relight load
/// failure, direction and gain changes reach the frames and load nothing;
/// the same settings again (a retry) or a new light do.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_frame_parameters_after_a_failure_load_nothing() {
    let fake = FakeFactory::new(FakeConfig {
        fail_relight_load: Some("fake relight unavailable".into()),
        ..FakeConfig::default()
    });
    let settings = relit(EffectKind::Blur, RelightLight::Cool);
    let mut rig = Rig::new(Some(Arc::clone(&fake)), &settings);
    rig.frames_until(Duration::from_secs(5), |r| {
        relight_failed(r) && r.effect.status() == EffectStatus::Ready
    });
    let loads = get(&fake.log.loads);
    let attempts = |f: &FakeFactory| f.log.stage_requests.lock().unwrap().len();
    let tries = attempts(&fake);
    for (i, pan) in [-90.0f32, -45.0, 0.0, 45.0, 90.0].into_iter().enumerate() {
        let mut s = EffectSettings {
            blur_strength: 0.1 * i as f32,
            ..settings.clone()
        };
        let r = s.stage_mut(&StageId::RELIGHT);
        r.live.insert("direction".into(), Value::Float(pan.into()));
        r.live
            .insert("brightness".into(), Value::Float(100.0 + 10.0 * i as f64));
        rig.effect.apply(&s).unwrap();
        assert_eq!(rig.frame(), Seen::FakeBlur);
    }
    assert_eq!(
        get(&fake.log.last_strength_milli),
        400,
        "the strength arrived"
    );
    // A load the fake would now accept: a retry would show it.
    fake.set(|c| c.fail_relight_load = None);
    std::thread::sleep(Duration::from_millis(200));
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::FakeBlur);
    }
    assert_eq!(get(&fake.log.loads), loads, "no load for a frame parameter");
    assert_eq!(attempts(&fake), tries);
    assert!(relight_failed(&rig), "the failure stands");
    // The same settings again are a retry.
    let current = rig.effect.settings();
    rig.effect.apply(&current).unwrap();
    rig.frames_until(Duration::from_secs(5), relight_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlurRelit);
    assert!(get(&fake.log.loads) > loads);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// What leaves the effect for each frame: its first memory, its PTS and
/// whether it carries the repeat flag.
type Sent = Arc<Mutex<Vec<(usize, Option<gst::ClockTime>, bool)>>>;

fn record_sent(fx: &ObcEffect) -> Sent {
    let sent: Sent = Arc::default();
    let s = Arc::clone(&sent);
    fx.static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                s.lock().unwrap().push((
                    b.peek_memory(0).as_ptr() as usize,
                    b.pts(),
                    gst_obc::has_repeat_flag(b),
                ));
            }
            gst::PadProbeReturn::Ok
        });
    sent
}

fn last(sent: &Sent) -> (usize, Option<gst::ClockTime>, bool) {
    *sent
        .lock()
        .unwrap()
        .last()
        .expect("a frame left the effect")
}

/// A repeat of a processed frame gets the cached output: the same memory,
/// the repeat's timestamp and flag, and no call into the backend; the next
/// fresh frame is processed again.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_repeat_re_sends_the_last_processed_frame_and_runs_no_effect() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    let sent = record_sent(&rig.effect);
    assert_eq!(rig.frame(), Seen::FakeBlur);
    let (processed_mem, _, repeat) = last(&sent);
    assert!(!repeat);
    let processes = get(&fake.log.processes);
    let stats = rig.effect.stats();
    for _ in 0..3 {
        let pts = gst::ClockTime::from_nseconds(rig.pts);
        assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
        let (mem, out_pts, repeat) = last(&sent);
        assert_eq!(mem, processed_mem, "the cached memory is sent again");
        assert_eq!(out_pts, Some(pts), "with the repeat's timestamp");
        assert!(repeat, "and its flag");
    }
    assert_eq!(get(&fake.log.processes), processes, "no effect ran");
    let after = rig.effect.stats();
    assert_eq!(after.frames_processed, stats.frames_processed);
    assert_eq!(after.frames_black, stats.frames_black);
    assert_eq!(after.frames_passthrough, stats.frames_passthrough);
    // A fresh frame is processed, and is the next one cached.
    assert_eq!(rig.frame(), Seen::FakeBlur);
    assert_eq!(get(&fake.log.processes), processes + 1);
    let (fresh_mem, _, _) = last(&sent);
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
    assert_eq!(last(&sent).0, fresh_mem);
    assert_eq!(get(&fake.log.processes), processes + 1);
    assert_eq!(rig.input_now(), rig.input_bytes, "the input was written");
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// A repeat after any change of the settings (a per-frame parameter, a
/// stage switched on) or after a new source runs the effect: the cached
/// frame was made under what came before.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_repeat_after_a_settings_change_is_processed_never_the_cached_frame() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    let sent = record_sent(&rig.effect);
    assert_eq!(rig.frame(), Seen::FakeBlur);
    // The blur strength alone: no new request, but a new frame parameter.
    let processes = get(&fake.log.processes);
    rig.effect
        .apply(&EffectSettings {
            blur_strength: 0.9,
            ..EffectSettings::of(EffectKind::Blur)
        })
        .unwrap();
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
    assert_eq!(get(&fake.log.processes), processes + 1, "processed");
    assert_eq!(
        get(&fake.log.last_strength_milli),
        900,
        "with the new strength"
    );
    // The repeat processed under the new settings is the new cache.
    let cached = last(&sent).0;
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
    assert_eq!(last(&sent).0, cached);
    assert_eq!(get(&fake.log.processes), processes + 1);
    // A stage switched on: every repeat is processed until it is ready,
    // and the first one after shows the stage.
    set_stage(&rig.effect, &StageId::DENOISE, true);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let before = get(&fake.log.processes);
        let seen = rig.frame_with(gst_obc::REPEAT_FLAG);
        assert!(
            matches!(seen, Seen::FakeBlur | Seen::FakeBlurDenoised),
            "{seen:?}"
        );
        assert_eq!(
            get(&fake.log.processes),
            before + 1,
            "a repeat ran the effect"
        );
        if seen == Seen::FakeBlurDenoised {
            break;
        }
        assert!(Instant::now() < deadline, "the stage never ran");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Ready);
    // A new source: the repeat after it resets and processes.
    assert_eq!(rig.frame(), Seen::FakeBlurDenoised);
    let processes = get(&fake.log.processes);
    let resets = get(&fake.log.resets);
    assert_eq!(
        rig.frame_with(gst_obc::NEW_SOURCE_FLAG | gst_obc::REPEAT_FLAG),
        Seen::FakeBlurDenoised
    );
    assert_eq!(get(&fake.log.processes), processes + 1);
    assert_eq!(get(&fake.log.resets), resets + 1);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// A repeat is black wherever a fresh frame is: while the effect loads,
/// cold after `none`, and after a failure with `on-failure=black`, even
/// with a processed frame cached before. Under `none` it is the input,
/// which hides nothing.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_repeat_while_the_effect_loads_is_black_under_black() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(400),
        ..FakeConfig::default()
    });
    let mut rig = Rig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    assert_eq!(rig.frame(), Seen::Black, "loading");
    let mut repeats = 0;
    while rig.effect.status() != EffectStatus::Ready {
        assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Black);
        repeats += 1;
        assert!(repeats < 200, "the effect never loaded");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(repeats > 0);
    assert_eq!(rig.frame(), Seen::FakeBlur);
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
    // To `none` and back: the input, then a cold load in black.
    rig.effect.apply(&EffectSettings::default()).unwrap();
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Input);
    rig.effect
        .apply(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), |r| {
        r.effect.status() == EffectStatus::Loading
    });
    assert!(seen.iter().all(|s| *s == Seen::Input), "{seen:?}");
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Black);
    while rig.effect.status() != EffectStatus::Ready {
        assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Black);
        std::thread::sleep(Duration::from_millis(10));
    }
    // A processed frame cached, then a failure: the repeat is black.
    assert_eq!(rig.frame(), Seen::FakeBlur);
    fake.set(|c| c.fail_process = Some("fake frame failure".into()));
    assert_eq!(rig.frame(), Seen::Black);
    assert!(failed(&rig));
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Black);
    // The same with the failure only seen on the repeat itself.
    fake.set(|c| c.fail_process = None);
    rig.effect
        .apply(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    rig.effect
        .apply(&EffectSettings {
            blur_strength: 0.2,
            ..EffectSettings::of(EffectKind::Blur)
        })
        .unwrap();
    fake.set(|c| c.fail_process = Some("fake frame failure".into()));
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Black);
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Black);
    rig.stop();
    assert_eq!(get(&fake.log.wrong_context), 0);
}

/// An `obcmodule` after the effect processes in place, with a stage that
/// is not idempotent (`bump` adds to every luma byte). The cached frame is
/// shared, so the module's write lands in a copy: the repeat, bumped again
/// by the module, shows the cached blur bumped once, not twice.
#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_downstream_in_place_write_does_not_change_the_cached_frame() {
    use fake_chain::{FAKE_BUMP_Y, FakeChainConfig, FakeChainFactory};
    gst_obc::module::register().unwrap();
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = Rig::with_downstream(
        Some(Arc::clone(&fake) as Arc<dyn BackendFactory>),
        &EffectSettings::of(EffectKind::Blur),
        "obcmodule name=m ! ",
    );
    let module = rig
        .pipeline
        .by_name("m")
        .unwrap()
        .downcast::<gst_obc::ObcModule>()
        .unwrap();
    let chain = FakeChainFactory::cuda(FakeChainConfig::default());
    module.set_factory(Some(
        Arc::clone(&chain) as Arc<dyn obc_backend::StageChainFactory>
    ));
    module
        .apply(&[StageRequest::on(StageId::new("bump").unwrap())])
        .unwrap();
    let module_out: Arc<Mutex<Vec<usize>>> = Arc::default();
    let m = Arc::clone(&module_out);
    module
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                m.lock().unwrap().push(b.peek_memory(0).as_ptr() as usize);
            }
            gst::PadProbeReturn::Ok
        });
    let deadline = Instant::now() + Duration::from_secs(5);
    while rig.effect.status() != EffectStatus::Ready
        || module.stage_status("bump") != EffectStatus::Ready
    {
        assert!(Instant::now() < deadline, "not ready");
        rig.frame();
        std::thread::sleep(Duration::from_millis(30));
    }
    let sent = record_sent(&rig.effect);
    let y_len = (W * H) as usize;
    let once = FAKE_BLUR_Y.wrapping_add(FAKE_BUMP_Y);
    let bumped_once = |bytes: &[u8]| bytes[..y_len].iter().all(|b| *b == once);
    let fresh = rig.bytes_with(gst::BufferFlags::empty());
    assert!(
        bumped_once(&fresh),
        "the module bumped the blur: {}",
        fresh[0]
    );
    let cached = last(&sent).0;
    assert_ne!(
        module_out.lock().unwrap().last().copied(),
        Some(cached),
        "the module wrote into a copy of the effect's memory"
    );
    let processes = get(&fake.log.processes);
    for _ in 0..3 {
        let out = rig.bytes_with(gst_obc::REPEAT_FLAG);
        assert_eq!(last(&sent).0, cached, "the cached memory was re-sent");
        assert!(
            bumped_once(&out),
            "the cached frame changed under the module's write: Y {}",
            out[0]
        );
    }
    assert_eq!(get(&fake.log.processes), processes, "no effect ran");
    assert_eq!(
        fake_chain::FakeChainLog::get(&chain.log.repeats),
        3,
        "the module ran on every repeat"
    );
    rig.stop();
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
        r.effect.stage_status("denoise") != EffectStatus::Loading
    });
    assert_eq!(
        rig.effect.stage_status("denoise"),
        EffectStatus::Ready,
        "the denoise stage did not load"
    );
    assert!(warm.iter().all(|s| *s == Seen::Input), "{warm:?}");
    assert_eq!(rig.effect.status(), EffectStatus::Off);
    let before = rig.effect.stats().stage(&StageId::DENOISE).frames;
    for _ in 0..5 {
        let s = rig.frame();
        assert!(!matches!(s, Seen::Black), "{s:?}");
    }
    assert_eq!(
        rig.effect.stats().stage(&StageId::DENOISE).frames,
        before + 5
    );
    // Read after each frame's synchronisation: the backend has the times
    // of a completed frame only.
    let p50 = rig.effect.stats().stage(&StageId::DENOISE).ms_p50;
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
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Ready);
    assert!(warm.iter().all(|s| *s == Seen::Black), "{warm:?}");
    let processed = |s: &Seen| !matches!(s, Seen::Input | Seen::Black);
    for _ in 0..5 {
        let s = rig.frame();
        assert!(processed(&s), "{s:?}");
    }

    // Off and on while playing: switches, every frame processed.
    for on in [false, true] {
        set_stage(&rig.effect, &StageId::DENOISE, on);
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

/// A new source through the element with Maxine's relighting, whose state
/// cannot be reset: the flagged frame and the frames until the stage is
/// built again are the blur alone (processed, never the input or black),
/// the stage shows loading, never failed, and then relights again with a
/// fresh build. Needs the relighting and green-screen features installed.
#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK with its relighting feature (MAXINE_VFX_ROOT), the \
            os-release override, an NVIDIA GPU and the GStreamer CUDA elements"]
fn maxine_a_new_source_rebuilds_the_relighting_before_it_relights() {
    let maxine = MaxineFactory::from_env();
    assert!(maxine.root().is_some(), "{MAXINE_SETUP}");
    maxine
        .probe()
        .unwrap_or_else(|e| panic!("Maxine probe failed: {e}"));
    let mut rig = Rig::with_factory(
        Some(Arc::new(maxine)),
        &relit(EffectKind::Blur, RelightLight::Warm),
    );
    rig.frames_until(Duration::from_secs(60), |r| {
        r.effect.status() != EffectStatus::Loading
            && r.effect.stage_status("relight") != EffectStatus::Loading
    });
    assert_eq!(rig.effect.status(), EffectStatus::Ready);
    assert_eq!(rig.effect.stage_status("relight"), EffectStatus::Ready);
    let processed = |s: &Seen| !matches!(s, Seen::Input | Seen::Black);
    for _ in 0..3 {
        let s = rig.frame();
        assert!(processed(&s), "{s:?}");
    }
    let before = rig.effect.stats();
    let s = rig.frame_with(gst_obc::NEW_SOURCE_FLAG);
    assert!(processed(&s), "the flagged frame: {s:?}");
    let after = rig.effect.stats();
    assert_eq!(
        after.stage(&StageId::RELIGHT).frames,
        before.stage(&StageId::RELIGHT).frames,
        "relit by the old stage"
    );
    assert_eq!(after.frames_processed, before.frames_processed + 1);
    let seen = rig.frames_until(Duration::from_secs(60), relight_is(EffectStatus::Ready));
    assert!(seen.iter().all(processed), "{seen:?}");
    let ready = rig.effect.stats();
    for _ in 0..3 {
        let s = rig.frame();
        assert!(processed(&s), "{s:?}");
    }
    let end = rig.effect.stats();
    assert_eq!(
        end.stage(&StageId::RELIGHT).frames,
        ready.stage(&StageId::RELIGHT).frames + 3,
        "relit again"
    );
    assert_eq!(end.loads, after.loads + 1, "one load rebuilt the stage");
    assert_eq!(end.frames_black, before.frames_black);
    assert_eq!(end.frames_passthrough, before.frames_passthrough);
    rig.drain_messages();
    assert!(
        rig.relight_messages.iter().all(|m| m.0 != "failed"),
        "{:?}",
        rig.relight_messages
    );
    assert!(
        rig.relight_messages.iter().any(|m| m.0 == "loading"),
        "{:?}",
        rig.relight_messages
    );
    rig.stop();
}
