//! The element without a GPU: registration, caps, properties, settings
//! and status messages; and the whole effect on system-memory frames
//! (`host_*`), with the fake backend of `support/fake_backend.rs` taking
//! host frames, through `appsrc ! obceffect ! appsink`. Nothing here
//! touches CUDA.

#[path = "support/fake_backend.rs"]
mod fake_backend;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_backend::{
    FAKE_BLUR_Y, FAKE_COVERAGE, FAKE_DENOISE_LEVEL, FAKE_DENOISE_MS, FAKE_DENOISE_UV,
    FAKE_DENOISE_Y, FAKE_REPLACE_Y, FakeConfig, FakeFactory, FakeLog,
};
use gst::glib;
use gst::prelude::*;
use gst_obc::{
    EffectFallback, EffectKind, EffectSettings, EffectStatus, ObcEffect, STATUS_MESSAGE, StageId,
    StageRequest, Value,
};
use gst_video::prelude::*;
use obc_backend::BackendFactory;
use obc_backend_maxine::MaxineFactory;

fn element() -> (gst::Element, ObcEffect) {
    gst_obc::register().expect("register obceffect");
    let e = gst::ElementFactory::make(gst_obc::ELEMENT_NAME)
        .build()
        .expect("obceffect is registered");
    let fx = gst_obc::from_element(&e)
        .expect("an obceffect element downcasts")
        .clone();
    (e, fx)
}

/// Applies the element's settings changed by `f`, as a client does.
fn change(fx: &ObcEffect, f: impl FnOnce(&mut EffectSettings)) {
    let mut s = fx.settings();
    f(&mut s);
    fx.apply(&s).unwrap();
}

/// The relight stage on, with `light`, a direction and a brightness.
fn relight(light: &str, direction: i64, brightness: i64) -> StageRequest {
    StageRequest::on(StageId::RELIGHT)
        .with_param("light", light)
        .with_live("direction", direction)
        .with_live("brightness", brightness)
}

#[test]
fn registers_once_and_has_nv12_pads_in_cuda_and_system_memory_on_both_sides() {
    gst_obc::register().unwrap();
    gst_obc::register().expect("registering twice is fine");
    let (e, _) = element();
    let f = e.factory().unwrap();
    assert_eq!(f.name(), "obceffect");
    for name in ["sink", "src"] {
        let caps = e.static_pad(name).unwrap().pad_template_caps();
        assert_eq!(
            caps.to_string(),
            "video/x-raw(memory:CUDAMemory), format=(string)NV12; \
             video/x-raw, format=(string)NV12",
            "{name}"
        );
    }
}

#[test]
fn properties_have_their_defaults() {
    let (e, fx) = element();
    assert_eq!(e.property::<String>("effect"), "none");
    assert_eq!(e.property::<f64>("blur-strength"), 0.5);
    assert_eq!(e.property::<String>("background"), "");
    assert_eq!(e.property::<String>("on-failure"), "black");
    assert_eq!(e.property::<String>("fallback-image"), "");
    for stage_property in ["denoise", "relight", "relight-light"] {
        assert!(
            e.find_property(stage_property).is_none(),
            "the stages have no property: {stage_property}"
        );
    }
    assert_eq!(e.property::<String>("status"), "off");
    assert_eq!(fx.settings(), EffectSettings::default());
    assert_eq!(fx.status(), EffectStatus::Off);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Off);
    let s = fx.stats();
    assert_eq!(s.effect, EffectKind::None);
    assert!(s.stages.is_empty());
    let d = s.stage(&StageId::DENOISE);
    assert_eq!(d.status, EffectStatus::Off);
    assert_eq!(d.frames, 0);
    assert_eq!((d.ms_p50, d.ms_p95), (None, None));
    assert!(!s.cuda_active);
    assert_eq!(
        s.frames_processed + s.frames_black + s.frames_passthrough,
        0
    );
    assert!(fx.backend_factory().is_none());
}

#[test]
fn properties_parse_and_invalid_values_are_ignored() {
    let (e, fx) = element();
    e.set_property("effect", "BLUR");
    assert_eq!(e.property::<String>("effect"), "blur");
    assert_eq!(fx.status(), EffectStatus::Loading);
    e.set_property("effect", "sepia");
    assert_eq!(
        e.property::<String>("effect"),
        "blur",
        "an unknown effect is ignored"
    );
    e.set_property("blur-strength", 0.8f64);
    assert!((fx.settings().blur_strength - 0.8).abs() < 1e-6);
    e.set_property("background", "relative.png");
    assert_eq!(
        e.property::<String>("background"),
        "",
        "a relative path is ignored"
    );
    e.set_property("background", "/srv/bg.png");
    assert_eq!(e.property::<String>("background"), "/srv/bg.png");
    e.set_property("on-failure", "none");
    assert_eq!(fx.settings().fallback, EffectFallback::None);
    e.set_property("on-failure", "raw");
    assert_eq!(e.property::<String>("on-failure"), "none");
    e.set_property("on-failure", "image");
    assert_eq!(fx.settings().fallback, EffectFallback::Image);
    e.set_property("fallback-image", "away.png");
    assert_eq!(
        e.property::<String>("fallback-image"),
        "",
        "a relative path is ignored"
    );
    e.set_property("fallback-image", "/srv/away.png");
    assert_eq!(fx.settings().fallback_image, Some("/srv/away.png".into()));
    // No caps yet: nothing is decoded, nothing is reported.
    assert_eq!(fx.stats().fallback_error, None);
    e.set_property("fallback-image", "");
    assert_eq!(fx.settings().fallback_image, None);
    e.set_property("on-failure", "none");
    e.set_property("background", "");
    assert_eq!(fx.settings().background, None);
    e.set_property("effect", "none");
    assert_eq!(fx.status(), EffectStatus::Off);
}

#[test]
fn blur_strength_outside_the_range_is_refused_by_the_param_spec() {
    let (e, _) = element();
    let pspec = e.find_property("blur-strength").unwrap();
    let p = pspec.downcast_ref::<glib::ParamSpecDouble>().unwrap();
    assert_eq!((p.minimum(), p.maximum()), (0.0, 1.0));
    assert!(
        pspec.flags().contains(gst::PARAM_FLAG_MUTABLE_PLAYING),
        "changeable while playing"
    );
}

#[test]
fn apply_validates_and_sets_everything_at_once() {
    let (_, fx) = element();
    let err = fx
        .apply(&EffectSettings::of(EffectKind::Replace))
        .unwrap_err();
    assert!(err.contains("background"), "{err}");
    assert_eq!(fx.settings(), EffectSettings::default(), "nothing applied");
    let bad = EffectSettings {
        blur_strength: 2.0,
        ..EffectSettings::of(EffectKind::Blur)
    };
    assert!(fx.apply(&bad).unwrap_err().contains("0 to 1"));
    let s = EffectSettings {
        kind: EffectKind::Replace,
        blur_strength: 0.25,
        background: Some("/srv/bg.png".into()),
        fallback: EffectFallback::None,
        fallback_image: Some("/srv/away.png".into()),
        stages: vec![
            StageRequest::on(StageId::DENOISE),
            relight("cool", -45, 125),
        ],
        background_params: [("chair".to_string(), Value::Bool(true))].into(),
    };
    fx.apply(&s).unwrap();
    assert_eq!(fx.settings(), s);
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Loading);
    assert_eq!(
        fx.stats().stage(&StageId::DENOISE).status,
        EffectStatus::Loading
    );
    assert_eq!(fx.stage_status("relight"), EffectStatus::Loading);
    assert_eq!(
        fx.stats().stage(&StageId::RELIGHT).status,
        EffectStatus::Loading
    );
    let mut twice = s.clone();
    twice.stages.push(StageRequest::on(StageId::DENOISE));
    assert!(fx.apply(&twice).unwrap_err().contains("listed twice"));
    // The stages' parameters are the backend's to check.
    fx.set_backend_factory(Some(Arc::new(MaxineFactory::with_root(None))));
    let mut bad = s.clone();
    bad.stages[1] = relight("neutral", 0, 300);
    assert!(fx.apply(&bad).unwrap_err().contains("relight gain"));
    assert_eq!(fx.settings(), s, "nothing applied");
    // The background stage's too.
    let mut bad = s.clone();
    bad.background_params
        .insert("chair".into(), Value::from("yes"));
    let e = fx.apply(&bad).unwrap_err();
    assert!(e.contains("chair must be a boolean"), "{e}");
    assert_eq!(fx.settings(), s, "nothing applied");
}

#[test]
fn relight_has_a_status_of_its_own() {
    let (_, fx) = element();
    assert_eq!(fx.stage_status("relight"), EffectStatus::Off);
    let st = fx.stats().stage(&StageId::RELIGHT);
    assert_eq!(st.status, EffectStatus::Off);
    assert_eq!(st.frames, 0);
    assert_eq!((st.ms_p50, st.ms_p95), (None, None));
    // none with relight: the effect stays off; the stage loads.
    change(&fx, |s| s.stage_mut(&StageId::RELIGHT).enabled = true);
    assert!(fx.settings().is_enabled("relight"));
    assert_eq!(fx.status(), EffectStatus::Off);
    assert_eq!(fx.stage_status("relight"), EffectStatus::Loading);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Off);
    change(&fx, |s| {
        s.stage_mut(&StageId::RELIGHT)
            .params
            .insert("light".into(), "warm".into());
    });
    change(&fx, |s| {
        let r = s.stage_mut(&StageId::RELIGHT);
        r.live.insert("direction".into(), Value::Int(90));
        r.live.insert("brightness".into(), Value::Int(150));
    });
    assert_eq!(
        fx.settings().stage("relight"),
        Some(&relight("warm", 90, 150))
    );
    change(&fx, |s| s.stage_mut(&StageId::RELIGHT).enabled = false);
    assert_eq!(fx.stage_status("relight"), EffectStatus::Off);
    assert_eq!(
        fx.settings().stage("relight").unwrap().params.get("light"),
        Some(&Value::from("warm")),
        "kept while the stage is off"
    );
}

#[test]
fn denoise_has_a_status_of_its_own() {
    let (e, fx) = element();
    // none with denoise: the effect stays off; the stage loads.
    change(&fx, |s| s.stage_mut(&StageId::DENOISE).enabled = true);
    assert!(fx.settings().is_enabled("denoise"));
    assert_eq!(fx.status(), EffectStatus::Off);
    assert_eq!(e.property::<String>("status"), "off");
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Loading);
    // An effect with it: both load.
    e.set_property("effect", "blur");
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Loading);
    // Turning it off is not a change of the effect.
    change(&fx, |s| s.stage_mut(&StageId::DENOISE).enabled = false);
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Off);
    assert_eq!(
        fx.settings(),
        EffectSettings::of(EffectKind::Blur).with_stage(StageRequest::new(StageId::DENOISE)),
        "the denoise request is part of the settings"
    );
}

#[test]
fn the_new_source_flag_is_a_free_video_buffer_flag() {
    let f = gst_obc::NEW_SOURCE_FLAG;
    assert_eq!(f.bits().count_ones(), 1);
    assert!(!f.intersects(gst::BufferFlags::all()), "a core buffer flag");
    // GST_BUFFER_FLAG_LAST is GST_VIDEO_BUFFER_FLAG_INTERLACED on video
    // buffers: the flag must be past every video flag.
    assert!(f.bits() > gst_video::ffi::GST_VIDEO_BUFFER_FLAG_INTERLACED);
    assert_eq!(f.bits(), gst_video::ffi::GST_VIDEO_BUFFER_FLAG_LAST);
    // It survives a shallow copy, as the output clock pushes frames, and
    // is read with the helper (`flags()` drops bits it has no name for).
    gst::init().unwrap();
    let mut b = gst::Buffer::new();
    assert!(!gst_obc::has_new_source_flag(&b));
    b.get_mut().unwrap().set_flags(f);
    let mut c = b.copy();
    assert!(gst_obc::has_new_source_flag(&c));
    assert!(!c.flags().contains(gst::BufferFlags::DISCONT));
    c.make_mut().unset_flags(f);
    assert!(!gst_obc::has_new_source_flag(&c));
    assert!(gst_obc::has_new_source_flag(&b));
}

#[test]
fn status_changes_are_posted_on_the_bus() {
    let (e, fx) = element();
    let pipeline = gst::Pipeline::new();
    pipeline.add(&e).unwrap();
    let bus = pipeline.bus().unwrap();
    fx.apply(&EffectSettings::of(EffectKind::Blur)).unwrap();
    // Strength alone changes nothing that is loaded: no message.
    e.set_property("blur-strength", 0.9f64);
    e.set_property("effect", "none");
    // A change of the denoise status alone is posted too.
    change(&fx, |s| s.stage_mut(&StageId::DENOISE).enabled = true);
    change(&fx, |s| s.stage_mut(&StageId::DENOISE).enabled = false);
    // So is one of the relight status; direction and gain post nothing, a
    // light that is off neither.
    change(&fx, |s| {
        s.stage_mut(&StageId::RELIGHT)
            .params
            .insert("light".into(), "cool".into());
    });
    change(&fx, |s| s.stage_mut(&StageId::RELIGHT).enabled = true);
    change(&fx, |s| {
        s.stage_mut(&StageId::RELIGHT)
            .live
            .insert("direction".into(), Value::Int(30));
    });
    change(&fx, |s| {
        s.stage_mut(&StageId::RELIGHT)
            .live
            .insert("brightness".into(), Value::Int(75));
    });
    change(&fx, |s| s.stage_mut(&StageId::RELIGHT).enabled = false);
    let mut seen = Vec::new();
    while let Some(m) = bus.pop_filtered(&[gst::MessageType::Element]) {
        let s = m.structure().unwrap();
        if s.name() == STATUS_MESSAGE {
            // A stage the element was never given has no field: it is off.
            let field = |f: &str| {
                s.get::<String>(f).unwrap_or_else(|_| {
                    assert!(
                        f.starts_with("denoise") || f.starts_with("relight"),
                        "{f} is always there"
                    );
                    if f.ends_with("-reason") { "" } else { "off" }.to_string()
                })
            };
            seen.push(
                [
                    "effect",
                    "status",
                    "reason",
                    "denoise",
                    "denoise-reason",
                    "relight",
                    "relight-reason",
                ]
                .map(field)
                .join(" "),
            );
        }
    }
    assert_eq!(
        seen,
        [
            "blur loading  off  off ",
            "none off  off  off ",
            "none off  loading  off ",
            "none off  off  off ",
            "none off  off  loading ",
            "none off  off  off ",
        ]
    );
}

// The effect on system-memory frames, without a GPU: the rules of the
// `gpu_*` tests of `tests/hardware.rs`, one by one, on host frames.

const W: u32 = 64;
const H: u32 = 36;
/// Canonical caps in system memory, limited range (black Y = 16).
const SYSTEM: &str = "video/x-raw,format=NV12,width=64,height=36,pixel-aspect-ratio=1/1,\
                      colorimetry=2:4:5:1,framerate=30/1";
/// The same in full range (black Y = 0).
const SYSTEM_FULL: &str = "video/x-raw,format=NV12,width=64,height=36,pixel-aspect-ratio=1/1,\
                           colorimetry=1:4:5:1,framerate=30/1";

/// What an output frame shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Input,
    /// Limited-range black: Y 16, UV 128.
    Black,
    /// Full-range black: Y 0, UV 128.
    FullBlack,
    /// The white fallback image: Y 235, UV 128.
    Image,
    FakeBlur,
    FakeReplace,
    FakeDenoise,
    FakeBlurDenoised,
    Other,
}

/// The packed NV12 of the input: Y rows 30 + row, UV 128.
fn host_input_bytes() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![128u8; w * h * 3 / 2];
    for row in 0..h {
        data[row * w..(row + 1) * w].fill(30 + row as u8);
    }
    data
}

/// What `frame` shows, for the rig's `input`. A fake effect frame is told
/// by its constant Y below the first row, and must carry the fake's mark of
/// its input in the first row (the input's Y plus one), or it is `Other`.
fn classify(frame: &[u8], input: &[u8]) -> Seen {
    let (y, uv) = frame.split_at((W * H) as usize);
    let all = |s: &[u8], v: u8| s.iter().all(|b| *b == v);
    let w = W as usize;
    let from_input = y[..w]
        .iter()
        .zip(&input[..w])
        .all(|(o, i)| *o == i.saturating_add(1));
    let y = &y[w..];
    if frame == input {
        Seen::Input
    } else if all(&frame[..w], 16) && all(y, 16) && all(uv, 128) {
        Seen::Black
    } else if all(&frame[..w], 0) && all(y, 0) && all(uv, 128) {
        Seen::FullBlack
    } else if all(&frame[..w], 235) && all(y, 235) && all(uv, 128) {
        Seen::Image
    } else if !from_input {
        Seen::Other
    } else if all(y, FAKE_BLUR_Y) && all(uv, 128) {
        Seen::FakeBlur
    } else if all(y, FAKE_REPLACE_Y) && all(uv, 128) {
        Seen::FakeReplace
    } else if all(y, FAKE_DENOISE_Y) && all(uv, FAKE_DENOISE_UV) {
        Seen::FakeDenoise
    } else if all(y, FAKE_BLUR_Y) && all(uv, FAKE_DENOISE_UV) {
        Seen::FakeBlurDenoised
    } else {
        Seen::Other
    }
}

/// The visible bytes of a system-memory NV12 buffer of the rig's caps,
/// packed.
fn packed(buffer: &gst::BufferRef, caps: &gst::Caps) -> Vec<u8> {
    let info = gst_video::VideoInfo::from_caps(caps).unwrap();
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).unwrap();
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

/// What leaves the effect for each frame: its first memory, its PTS and
/// whether it carries the repeat flag.
type Sent = Arc<Mutex<Vec<(usize, Option<gst::ClockTime>, bool)>>>;

/// `appsrc ! obceffect ! appsink` on system-memory caps; one input buffer
/// pushed again and again as shallow copies, as the output clock repeats a
/// frame.
struct HostRig {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    effect: ObcEffect,
    caps: gst::Caps,
    input: gst::Buffer,
    input_bytes: Vec<u8>,
    in_memory: usize,
    sent: Sent,
    pts: u64,
}

impl HostRig {
    fn new(factory: Option<Arc<FakeFactory>>, settings: &EffectSettings) -> HostRig {
        HostRig::with_caps(
            factory.map(|f| f as Arc<dyn BackendFactory>),
            settings,
            SYSTEM,
        )
    }

    fn with_caps(
        factory: Option<Arc<dyn BackendFactory>>,
        settings: &EffectSettings,
        caps: &str,
    ) -> HostRig {
        gst_obc::register().unwrap();
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=src format=time is-live=true caps=\"{caps}\" ! obceffect name=fx ! \
             appsink name=sink sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
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
        let sink = pipeline
            .by_name("sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        let sent: Sent = Arc::default();
        let s = Arc::clone(&sent);
        effect
            .static_pad("src")
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
        pipeline.set_state(gst::State::Playing).unwrap();
        let input = gst::Buffer::from_mut_slice(host_input_bytes());
        let in_memory = input.peek_memory(0).as_ptr() as usize;
        HostRig {
            pipeline,
            src,
            sink,
            effect,
            caps: caps.parse().unwrap(),
            input,
            input_bytes: host_input_bytes(),
            in_memory,
            sent,
            pts: 0,
        }
    }

    fn frame(&mut self) -> Seen {
        self.frame_with(gst::BufferFlags::empty())
    }

    fn frame_with(&mut self, flags: gst::BufferFlags) -> Seen {
        let bytes = self.bytes_with(flags);
        classify(&bytes, &self.input_bytes)
    }

    /// Pushes the input once more with `flags` and returns the output's
    /// packed bytes.
    fn bytes_with(&mut self, flags: gst::BufferFlags) -> Vec<u8> {
        let mut b = self.input.copy();
        {
            let b = b.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(self.pts));
            b.set_duration(gst::ClockTime::from_nseconds(33_333_333));
            b.set_flags(flags);
        }
        self.pts += 33_333_333;
        self.src.push_buffer(b).expect("push");
        let sample = self
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .unwrap_or_else(|| panic!("no output frame; bus: {:?}", self.bus_errors()));
        packed(sample.buffer().unwrap(), &self.caps)
    }

    /// Frames every 20 ms until `done` or `timeout`; returns them.
    fn frames_until(&mut self, timeout: Duration, done: impl Fn(&HostRig) -> bool) -> Vec<Seen> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out; status {:?}, frames {seen:?}",
                self.effect.status()
            );
            seen.push(self.frame());
            std::thread::sleep(Duration::from_millis(20));
        }
        seen
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

    /// Pushes `bytes` (packed NV12 of the rig's caps) from now on.
    fn set_input(&mut self, bytes: Vec<u8>) {
        self.input = gst::Buffer::from_mut_slice(bytes.clone());
        self.in_memory = self.input.peek_memory(0).as_ptr() as usize;
        self.input_bytes = bytes;
    }

    /// The input buffer's bytes now.
    fn input_now(&self) -> Vec<u8> {
        self.input.map_readable().unwrap().as_slice().to_vec()
    }

    /// The first memory of the last buffer that left the effect, its PTS
    /// and repeat flag.
    fn last_sent(&self) -> (usize, Option<gst::ClockTime>, bool) {
        *self
            .sent
            .lock()
            .unwrap()
            .last()
            .expect("a frame left the effect")
    }

    fn stop(&self) {
        self.pipeline.set_state(gst::State::Null).unwrap();
        let (_, cur, _) = self.pipeline.state(gst::ClockTime::from_seconds(5));
        assert_eq!(cur, gst::State::Null);
    }
}

impl Drop for HostRig {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn status_is(want: EffectStatus) -> impl Fn(&HostRig) -> bool {
    move |r| r.effect.status() == want
}

fn failed(r: &HostRig) -> bool {
    matches!(r.effect.status(), EffectStatus::Failed(_))
}

fn get(a: &AtomicU64) -> u64 {
    FakeLog::get(a)
}

/// `seen` is `hold` frames, then at most one `then` frame (the frame
/// decided just after the status turned may already be processed).
fn held_then(seen: &[Seen], hold: Seen, then: Seen) -> bool {
    let n = seen.iter().take_while(|s| **s == hold).count();
    n + 1 >= seen.len() && seen[n..].iter().all(|s| *s == then)
}

/// Waits until `cond`, failing after five seconds with `what`.
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the host path must never do: open a CUDA context or make a CUDA
/// call (the fake's CUDA methods check the context; its host methods make
/// no CUDA call, and record a call of the wrong memory's methods).
fn assert_no_cuda(rig: &HostRig, fake: &FakeFactory) {
    assert!(
        !rig.effect.stats().cuda_active,
        "CUDA resources were created"
    );
    assert_eq!(get(&fake.log.wrong_context), 0, "a CUDA call was made");
    assert_eq!(get(&fake.log.wrong_memory), 0, "a call of the CUDA methods");
    assert_eq!(
        get(&fake.log.host_loads),
        get(&fake.log.loads),
        "every load is a host load"
    );
    assert_eq!(
        get(&fake.log.host_processes),
        get(&fake.log.processes),
        "every frame is a host frame"
    );
}

/// Neither GStreamer's CUDA library nor the CUDA driver is loaded in this
/// process (no test of this file loads them).
fn assert_no_cuda_library() {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    for lib in ["libgstcuda-1.0", "libcuda.so"] {
        assert!(!maps.contains(lib), "{lib} is loaded");
    }
}

/// An image file of `width x height` made by `pattern` (a `videotestsrc`
/// pattern): `white` is the fallback image (its NV12 in limited range: Y
/// 235, UV 128).
fn png(dir: &Path, name: &str, pattern: &str, width: u32, height: u32) -> PathBuf {
    gst::init().unwrap();
    let path = dir.join(name);
    let p = gst::parse::launch(&format!(
        "videotestsrc num-buffers=1 pattern={pattern} ! video/x-raw,format=RGB,width={width},\
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

#[test]
fn host_none_passes_the_input_buffer_through_with_no_copy() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    for _ in 0..5 {
        assert_eq!(rig.frame(), Seen::Input);
    }
    let s = rig.effect.stats();
    assert_eq!(s.frames_passthrough, 5);
    assert_eq!(s.frames_processed + s.frames_black, 0);
    assert_eq!(get(&fake.log.creates), 0);
    let sent = rig.sent.lock().unwrap().clone();
    assert!(
        sent.iter().all(|(m, _, _)| *m == rig.in_memory),
        "passthrough forwards the input memory itself"
    );
    assert_no_cuda(&rig, &fake);
    rig.stop();
    assert_no_cuda_library();
}

#[test]
fn host_a_toggle_mid_stream_keeps_the_input_until_ready_and_switches_never_show_it() {
    let dir = tempfile::tempdir().unwrap();
    let bg = png(dir.path(), "bg.png", "smpte", 80, 60);
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(300),
        switch_delay: Duration::from_millis(100),
        ..FakeConfig::default()
    });
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);

    // none -> blur mid-stream: the input until the load is done, never
    // black, then processed.
    let blur = EffectSettings {
        blur_strength: 0.7,
        ..EffectSettings::of(EffectKind::Blur)
    };
    rig.effect.apply(&blur).unwrap();
    let warm = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(warm.len() >= 5, "{warm:?}");
    assert!(held_then(&warm, Seen::Input, Seen::FakeBlur), "{warm:?}");
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::FakeBlur);
    }
    assert_eq!(get(&fake.log.last_strength_milli), 700);
    // Only the effect ran: its values are reported, not those of a stage
    // outside the loaded chain.
    assert_eq!(
        rig.effect.stats().stage_values,
        [obc_backend::StageValue {
            stage: StageId::BACKGROUND,
            name: "coverage".into(),
            value: FAKE_COVERAGE,
        }],
        "the values of the effect's chain"
    );

    // blur -> replace: blur until replace is ready, never black or input.
    rig.effect
        .apply(&EffectSettings {
            background: Some(bg),
            ..EffectSettings::of(EffectKind::Replace)
        })
        .unwrap();
    let switch = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    let blur_frames = switch.iter().take_while(|s| **s == Seen::FakeBlur).count();
    assert!(
        switch[blur_frames..]
            .iter()
            .all(|s| *s == Seen::FakeReplace),
        "every frame of the switch is processed: {switch:?}"
    );
    assert_eq!(rig.frame(), Seen::FakeReplace);
    assert_eq!(*fake.log.backgrounds.lock().unwrap(), [(W, H)]);

    // -> none: the input at once; the worker unloads.
    rig.effect.apply(&EffectSettings::default()).unwrap();
    assert_eq!(rig.frame(), Seen::Input);
    wait_for("no unload", || get(&fake.log.unloads) == 1);
    assert_eq!(rig.frame(), Seen::Input);

    // none -> blur again: a cold load, the input (on screen) until done.
    rig.effect.apply(&blur).unwrap();
    let again = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(held_then(&again, Seen::Input, Seen::FakeBlur), "{again:?}");
    assert_eq!(rig.frame(), Seen::FakeBlur);

    let stats = rig.effect.stats();
    assert!(stats.frames_processed > 0);
    assert_eq!(stats.frames_black, 0, "a toggle mid-stream is never black");
    assert_eq!(stats.gpu_ms_p50, None, "no GPU time on the CPU path");
    assert!(stats.last_load_ms.unwrap() >= 250.0);
    assert_no_cuda(&rig, &fake);

    // Stopping unloads and drops the instance, with no context.
    rig.stop();
    assert_eq!(get(&fake.log.unloads), 2);
    assert_eq!(get(&fake.log.drops), 1);
    assert_eq!(get(&fake.log.creates), 1);
    assert_no_cuda(&rig, &fake);
    assert_no_cuda_library();
}

/// The hold where the room was never shown: a start with the effect
/// requested, and a new source while the effect loads, are black (black of
/// the caps' range, checked in the bytes).
#[test]
fn host_a_start_or_a_new_source_while_the_effect_loads_is_black() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(300),
        ..FakeConfig::default()
    });
    let blur = EffectSettings::of(EffectKind::Blur);
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &blur);
    let warm = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(warm.len() >= 3, "{warm:?}");
    assert!(held_then(&warm, Seen::Black, Seen::FakeBlur), "{warm:?}");
    assert_eq!(rig.frame(), Seen::FakeBlur);
    rig.stop();

    // none, then blur mid-stream: the input; a new source while it loads:
    // black from that frame on, then the blur.
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    rig.effect.apply(&blur).unwrap();
    assert_eq!(rig.frame(), Seen::Input, "the toggle keeps the picture");
    assert_eq!(rig.effect.status(), EffectStatus::Loading);
    assert_eq!(rig.frame_with(gst_obc::NEW_SOURCE_FLAG), Seen::Black);
    let rest = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(held_then(&rest, Seen::Black, Seen::FakeBlur), "{rest:?}");
    assert_eq!(rig.frame(), Seen::FakeBlur);
    assert!(rig.effect.stats().frames_black > 0);
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

#[test]
fn host_black_is_the_black_of_the_caps_range() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(200),
        ..FakeConfig::default()
    });
    let mut rig = HostRig::with_caps(
        Some(Arc::clone(&fake) as Arc<dyn BackendFactory>),
        &EffectSettings::of(EffectKind::Blur),
        SYSTEM_FULL,
    );
    let warm = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(
        held_then(&warm, Seen::FullBlack, Seen::FakeBlur),
        "{warm:?}"
    );
    rig.stop();
}

#[test]
fn host_on_failure_image_shows_the_fallback_image_where_black_would_be() {
    let dir = tempfile::tempdir().unwrap();
    // Another aspect ratio than the frames': cropped to fill them.
    let image = png(dir.path(), "away.png", "white", 40, 40);
    let fake = FakeFactory::new(FakeConfig {
        fail_load: Some("fake load failure".into()),
        ..FakeConfig::default()
    });
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    let settings = EffectSettings {
        fallback: EffectFallback::Image,
        fallback_image: Some(image),
        ..EffectSettings::of(EffectKind::Blur)
    };
    rig.effect.apply(&settings).unwrap();
    // The input (on screen) while the load runs; once it has failed,
    // black until the image is decoded, then the image: never the input
    // again.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut hidden = false;
    loop {
        let seen = rig.frame();
        if seen == Seen::Image && failed(&rig) {
            break;
        }
        assert!(
            matches!(seen, Seen::Black | Seen::Image) || (seen == Seen::Input && !hidden),
            "only black or the image once hidden: {seen:?}"
        );
        hidden |= seen != Seen::Input;
        assert!(Instant::now() < deadline, "the image never showed");
        std::thread::sleep(Duration::from_millis(20));
    }
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Image);
    }
    let stats = rig.effect.stats();
    assert_eq!(stats.fallback_error, None);
    assert!(stats.frames_black > 0, "the image counts as a hidden frame");
    // A frame failure after a good load: the failing frame is the image.
    fake.set(|c| c.fail_load = None);
    rig.effect.apply(&settings).unwrap();
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    fake.set(|c| c.fail_process = Some("fake frame failure".into()));
    assert_eq!(rig.frame(), Seen::Image);
    assert!(failed(&rig));
    // on-failure=black again: black, the image left unused.
    rig.effect
        .apply(&EffectSettings {
            fallback: EffectFallback::Black,
            ..settings
        })
        .unwrap();
    rig.frames_until(Duration::from_secs(5), failed);
    assert_eq!(rig.frame(), Seen::Black);
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

#[test]
fn host_an_unreadable_fallback_image_shows_black_and_says_why() {
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
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &settings);
    let seen = rig.frames_until(Duration::from_secs(10), |r| {
        failed(r) && r.effect.stats().fallback_error.is_some()
    });
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    assert_eq!(rig.frame(), Seen::Black);
    let why = rig.effect.stats().fallback_error.unwrap();
    assert!(
        why.starts_with(&format!(
            "Fallback image {} could not be read (",
            missing.display()
        )) && why.ends_with("): showing black"),
        "{why}"
    );
    rig.stop();
}

#[test]
fn host_failures_show_black_or_the_input_as_configured() {
    let fake = FakeFactory::new(FakeConfig {
        fail_load: Some("fake load failure".into()),
        ..FakeConfig::default()
    });
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    rig.effect
        .apply(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), failed);
    assert!(held_then(&seen, Seen::Input, Seen::Black), "{seen:?}");
    assert_eq!(
        rig.effect.status(),
        EffectStatus::Failed("fake load failure".into())
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Black, "on-failure=black keeps black");
    }

    // on-failure=none: the input (the user opted in).
    let open = EffectSettings {
        fallback: EffectFallback::None,
        ..EffectSettings::of(EffectKind::Blur)
    };
    rig.effect.apply(&open).unwrap();
    let seen = rig.frames_until(Duration::from_secs(5), failed);
    assert!(
        seen.iter().all(|s| matches!(s, Seen::Black | Seen::Input)),
        "{seen:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::Input);
    }

    // A frame failure after a good load: black with on-failure=black, the
    // input copied with on-failure=none.
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
    fake.set(|c| c.fail_process = None);
    rig.effect.apply(&open).unwrap();
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    fake.set(|c| c.fail_process = Some("fake frame failure".into()));
    let passed = rig.effect.stats().frames_passthrough;
    assert_eq!(rig.frame(), Seen::Input, "the failing frame is the input");
    assert!(
        rig.sent.lock().unwrap().last().unwrap().0 != rig.in_memory,
        "copied into a new buffer"
    );
    assert_eq!(rig.effect.stats().frames_passthrough, passed + 1);
    assert_eq!(rig.input_now(), rig.input_bytes);
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

/// A module that can no longer run the effect, mid-session: black from the
/// next frame, the instance dropped; a factory set after it loads again,
/// with a new instance, by itself.
#[test]
fn host_a_revoked_backend_is_black_at_once_and_a_new_factory_loads_again() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = HostRig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);

    rig.effect.revoke_backend();
    assert!(failed(&rig), "{:?}", rig.effect.status());
    assert_eq!(rig.frame(), Seen::Black, "black from the next frame");
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(&fake.log.drops) == 0 && Instant::now() < deadline {
        assert_eq!(rig.frame(), Seen::Black);
    }
    assert_eq!(get(&fake.log.drops), 1, "the instance is dropped");
    assert!(get(&fake.log.unloads) >= 1, "unloaded before the drop");
    assert_eq!(rig.frame(), Seen::Black);

    rig.effect
        .set_backend_factory(Some(Arc::clone(&fake) as Arc<dyn BackendFactory>));
    let seen = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(
        held_then(&seen, Seen::Black, Seen::FakeBlur),
        "black until ready again: {seen:?}"
    );
    assert_eq!(rig.frame(), Seen::FakeBlur);
    assert_eq!(get(&fake.log.creates), 2, "a new instance");

    // Revoked and set again at once: never the revoked instance.
    rig.effect.revoke_backend();
    rig.effect
        .set_backend_factory(Some(Arc::clone(&fake) as Arc<dyn BackendFactory>));
    let seen = rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert!(held_then(&seen, Seen::Black, Seen::FakeBlur), "{seen:?}");
    assert_eq!(get(&fake.log.creates), 3);
    assert_eq!(get(&fake.log.drops), 2);
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

#[test]
fn host_a_backend_that_lost_its_chain_is_loaded_again_on_a_retry() {
    let fake = FakeFactory::new(FakeConfig::default());
    let blur = EffectSettings::of(EffectKind::Blur);
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &blur);
    rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    let loads = get(&fake.log.loads);
    fake.set(|c| c.lose_chain_on_process = Some("the module panicked".into()));
    assert_eq!(rig.frame(), Seen::Black);
    assert_eq!(get(&fake.log.lost_chains), 1);
    assert!(failed(&rig));
    let processes = get(&fake.log.processes);
    assert_eq!(rig.frame(), Seen::Black);
    assert_eq!(get(&fake.log.processes), processes, "the backend not asked");
    rig.effect.apply(&blur).unwrap();
    rig.frames_until(Duration::from_secs(10), status_is(EffectStatus::Ready));
    assert_eq!(get(&fake.log.loads), loads + 1, "loaded again");
    assert_eq!(rig.frame(), Seen::FakeBlur);
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

#[test]
fn host_an_unavailable_backend_or_image_is_a_failure_with_its_reason() {
    let fake = FakeFactory::new(FakeConfig {
        fail_create: Some("fake SDK missing".into()),
        ..FakeConfig::default()
    });
    let mut rig = HostRig::new(
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
    rig.stop();

    // No factory at all.
    let mut bare = HostRig::new(None, &EffectSettings::of(EffectKind::Blur));
    bare.frames_until(Duration::from_secs(5), failed);
    assert!(
        bare.effect
            .status()
            .reason()
            .unwrap()
            .contains("no inference backend")
    );
    assert_eq!(bare.frame(), Seen::Black);
    bare.stop();
}

/// A backend whose factory takes CUDA frames only cannot run the effect on
/// system memory: the effect fails with the reason, before any load, and
/// the frames follow `on-failure`; the stages alone fail and pass the
/// input through.
#[test]
fn host_a_cuda_only_backend_fails_the_effect_with_its_reason() {
    let fake = FakeFactory::cuda_only(FakeConfig::default());
    let mut rig = HostRig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    let seen = rig.frames_until(Duration::from_secs(5), failed);
    assert!(seen.iter().all(|s| *s == Seen::Black), "{seen:?}");
    assert_eq!(
        rig.effect.status(),
        EffectStatus::Failed(
            "the fake backend takes CUDA frames only, and these frames are in system memory".into()
        )
    );
    assert_eq!(rig.frame(), Seen::Black);
    rig.effect
        .apply(&EffectSettings {
            fallback: EffectFallback::None,
            ..EffectSettings::of(EffectKind::Blur)
        })
        .unwrap();
    rig.frames_until(Duration::from_secs(5), failed);
    assert_eq!(rig.frame(), Seen::Input);
    rig.effect
        .apply(&EffectSettings::default().with_stage(StageRequest::on(StageId::DENOISE)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(rig.effect.stage_status("denoise"), EffectStatus::Failed(_)) {
        assert_eq!(rig.frame(), Seen::Input);
        assert!(Instant::now() < deadline, "the stage never failed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rig.frame(), Seen::Input);
    assert_eq!(get(&fake.log.creates), 0, "nothing was created");
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

#[test]
fn host_repeated_pushes_leave_the_input_unmodified() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = HostRig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    for _ in 0..10 {
        assert_eq!(rig.frame(), Seen::FakeBlur);
    }
    assert_eq!(rig.input_now(), rig.input_bytes, "the input was written");
    let sent = rig.sent.lock().unwrap().clone();
    assert!(
        sent.iter().all(|(m, _, _)| *m != rig.in_memory),
        "an effect frame is a new buffer"
    );
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

/// The effect's output is made from the frame that came in, not from the
/// output buffer or an older frame: the fake marks its output with the
/// input's first Y row, which changes here from frame to frame.
#[test]
fn host_the_effect_output_is_made_from_the_input() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = HostRig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    let w = W as usize;
    for start in [0u8, 77, 254] {
        let mut bytes = host_input_bytes();
        for (x, b) in bytes[..w].iter_mut().enumerate() {
            *b = start.wrapping_add(x as u8);
        }
        rig.set_input(bytes.clone());
        let out = rig.bytes_with(gst::BufferFlags::empty());
        let want: Vec<u8> = bytes[..w].iter().map(|b| b.saturating_add(1)).collect();
        assert_eq!(out[..w], want[..], "the first row follows the input");
        assert!(
            out[w..(W * H) as usize].iter().all(|y| *y == FAKE_BLUR_Y),
            "the rest is the fake blur"
        );
        assert_eq!(classify(&out, &bytes), Seen::FakeBlur);
        assert_eq!(rig.input_now(), bytes, "the input was written");
    }
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

/// A repeat of a processed frame gets the cached output: the same memory,
/// the repeat's timestamp and flag, and no call into the backend.
#[test]
fn host_a_repeat_re_sends_the_last_processed_frame_and_runs_no_effect() {
    let fake = FakeFactory::new(FakeConfig::default());
    let mut rig = HostRig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
    assert_eq!(rig.frame(), Seen::FakeBlur);
    let (processed_mem, _, repeat) = rig.last_sent();
    assert!(!repeat);
    let processes = get(&fake.log.processes);
    let stats = rig.effect.stats();
    for _ in 0..3 {
        let pts = gst::ClockTime::from_nseconds(rig.pts);
        assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
        let (mem, out_pts, repeat) = rig.last_sent();
        assert_eq!(mem, processed_mem, "the cached memory is sent again");
        assert_eq!(out_pts, Some(pts), "with the repeat's timestamp");
        assert!(repeat, "and its flag");
    }
    assert_eq!(get(&fake.log.processes), processes, "no effect ran");
    assert_eq!(rig.effect.stats().frames_processed, stats.frames_processed);
    // A fresh frame is processed, and is the next one cached.
    assert_eq!(rig.frame(), Seen::FakeBlur);
    assert_eq!(get(&fake.log.processes), processes + 1);
    let fresh = rig.last_sent().0;
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
    assert_eq!(rig.last_sent().0, fresh);
    // A settings change: the repeat is processed, never the cached frame.
    rig.effect
        .apply(&EffectSettings {
            blur_strength: 0.9,
            ..EffectSettings::of(EffectKind::Blur)
        })
        .unwrap();
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
    assert_eq!(get(&fake.log.processes), processes + 2);
    assert_eq!(get(&fake.log.last_strength_milli), 900);
    assert_eq!(rig.input_now(), rig.input_bytes, "the input was written");
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

/// A repeat is black wherever a fresh frame is: while the effect loads at
/// a start, and after a failure, even with a processed frame cached; in a
/// toggle mid-stream it is the input, as the fresh frames are.
#[test]
fn host_a_repeat_while_the_effect_loads_is_black_under_black() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(300),
        ..FakeConfig::default()
    });
    let mut rig = HostRig::new(
        Some(Arc::clone(&fake)),
        &EffectSettings::of(EffectKind::Blur),
    );
    assert_eq!(rig.frame(), Seen::Black, "loading");
    let mut repeats = Vec::new();
    while rig.effect.status() != EffectStatus::Ready {
        repeats.push(rig.frame_with(gst_obc::REPEAT_FLAG));
        assert!(repeats.len() < 300, "the effect never loaded");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        held_then(&repeats, Seen::Black, Seen::FakeBlur),
        "{repeats:?}"
    );
    assert_eq!(rig.frame(), Seen::FakeBlur);
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::FakeBlur);
    // To `none` and back: the input, and its repeats stay the input until
    // the effect is ready.
    rig.effect.apply(&EffectSettings::default()).unwrap();
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Input);
    rig.effect
        .apply(&EffectSettings::of(EffectKind::Blur))
        .unwrap();
    let mut held = Vec::new();
    while rig.effect.status() != EffectStatus::Ready {
        held.push(rig.frame_with(gst_obc::REPEAT_FLAG));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(held_then(&held, Seen::Input, Seen::FakeBlur), "{held:?}");
    // A processed frame cached, then a failure: the repeat is black.
    assert_eq!(rig.frame(), Seen::FakeBlur);
    fake.set(|c| c.fail_process = Some("fake frame failure".into()));
    assert_eq!(rig.frame(), Seen::Black);
    assert!(failed(&rig));
    assert_eq!(rig.frame_with(gst_obc::REPEAT_FLAG), Seen::Black);
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

/// The stages alone on host frames: the input until they are loaded, then
/// processed; a new source resets them once; off again, the input.
#[test]
fn host_none_with_denoise_passes_through_until_loaded_then_denoises() {
    let fake = FakeFactory::new(FakeConfig {
        load_delay: Duration::from_millis(300),
        ..FakeConfig::default()
    });
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &EffectSettings::default());
    assert_eq!(rig.frame(), Seen::Input);
    let denoise = EffectSettings::default().with_stage(StageRequest::on(StageId::DENOISE));
    rig.effect.apply(&denoise).unwrap();
    assert_eq!(rig.effect.stage_status("denoise"), EffectStatus::Loading);
    let warm = rig.frames_until(Duration::from_secs(10), |r| {
        r.effect.stage_status("denoise") == EffectStatus::Ready
    });
    assert!(
        warm.iter().all(|s| *s == Seen::Input),
        "the input while the stage loads, never black: {warm:?}"
    );
    for _ in 0..3 {
        assert_eq!(rig.frame(), Seen::FakeDenoise);
    }
    let s = rig.effect.stats();
    assert_eq!(s.frames_black, 0);
    assert_eq!(
        s.stage(&StageId::DENOISE).ms_p50,
        Some(f64::from(FAKE_DENOISE_MS)),
        "the stage times of host frames"
    );
    assert_eq!(
        s.stage_values,
        [obc_backend::StageValue {
            stage: StageId::DENOISE,
            name: "level".into(),
            value: FAKE_DENOISE_LEVEL,
        }],
        "the status values of the last frame, none of a stage not loaded"
    );
    assert_eq!(get(&fake.log.resets), 0);
    assert_eq!(rig.frame_with(gst_obc::NEW_SOURCE_FLAG), Seen::FakeDenoise);
    assert_eq!(get(&fake.log.resets), 1);
    assert_eq!(rig.frame(), Seen::FakeDenoise);
    assert_eq!(get(&fake.log.resets), 1, "one reset per new source");
    // Off again: the input at once; the worker unloads.
    rig.effect.apply(&EffectSettings::default()).unwrap();
    assert_eq!(rig.frame(), Seen::Input);
    assert!(
        rig.effect.stats().stage_values.is_empty(),
        "a stage off reports nothing"
    );
    wait_for("no unload", || get(&fake.log.unloads) == 1);
    assert_no_cuda(&rig, &fake);
    rig.stop();
}

#[test]
fn host_each_stop_unloads_the_backend_and_a_restart_loads_again() {
    let fake = FakeFactory::new(FakeConfig::default());
    let rig_settings = EffectSettings::of(EffectKind::Blur);
    let mut rig = HostRig::new(Some(Arc::clone(&fake)), &rig_settings);
    for cycle in 0..3u64 {
        if cycle > 0 {
            rig.pipeline.set_state(gst::State::Playing).unwrap();
        }
        let warm = rig.frames_until(Duration::from_secs(5), status_is(EffectStatus::Ready));
        assert!(
            warm.iter()
                .all(|s| matches!(s, Seen::Black | Seen::FakeBlur)),
            "a start is black until ready: {warm:?}"
        );
        assert_eq!(rig.frame(), Seen::FakeBlur);
        rig.stop();
        assert_eq!(get(&fake.log.unloads), cycle + 1);
        assert_eq!(get(&fake.log.drops), cycle + 1);
    }
    assert_no_cuda(&rig, &fake);
    assert_no_cuda_library();
}
