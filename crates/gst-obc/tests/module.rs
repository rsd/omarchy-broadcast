//! The `obcmodule` element with the fake chain of `support/fake_chain.rs`.
//!
//! The tests without a prefix run on system memory, on the CPU: a frame of
//! known bytes is pushed again and again as shallow copies (as the output
//! clock repeats a frame) through `appsrc ! obcmodule ! appsink`, and
//! every output frame is read back.
//!
//! The `gpu_*` tests are `#[ignore]` and need an NVIDIA GPU with
//! GStreamer's CUDA elements (`cudaupload`, `cudadownload`): the same rig
//! on CUDA frames, `appsrc ! obcmodule ! cudadownload ! appsink`. Run them
//! with `cargo test -p gst-obc --test module -- --ignored gpu_
//! --test-threads=1`.

#[path = "support/fake_chain.rs"]
mod fake_chain;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_chain::{
    FAKE_BUMP_Y, FAKE_LIFT_Y, FAKE_TINT_UV, FakeChainConfig, FakeChainFactory, FakeChainLog,
};
use gst::prelude::*;
use gst_obc::module::{self, STATUS_MESSAGE};
use gst_obc::{EffectStatus, ObcModule, StageId, StageRequest};
use gst_video::prelude::*;
use obc_backend::{FrameKind, StageChainFactory};

const W: u32 = 64;
const H: u32 = 48;
const SYSTEM: &str = "video/x-raw,format=NV12,width=64,height=48,pixel-aspect-ratio=1/1,\
                      colorimetry=2:4:5:1,framerate=30/1";
const CUDA: &str = "video/x-raw(memory:CUDAMemory),format=NV12,width=64,height=48,\
                    pixel-aspect-ratio=1/1,colorimetry=2:4:5:1,framerate=30/1";

fn id(s: &str) -> StageId {
    StageId::new(s).unwrap()
}

fn tint() -> StageRequest {
    StageRequest::on(id("tint"))
}

fn lift() -> StageRequest {
    StageRequest::on(id("lift"))
}

fn bump() -> StageRequest {
    StageRequest::on(id("bump"))
}

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
         present.",
        missing.join(", ")
    );
}

/// The packed NV12 input: Y rows 30 + row, UV 128.
fn input_bytes() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![128u8; w * h * 3 / 2];
    for row in 0..h {
        data[row * w..(row + 1) * w].fill(30 + row as u8);
    }
    data
}

/// The input with every luma byte plus `n` (wrapping) and, with `uv`, its
/// chroma all `uv`: what `bump` (run `n / FAKE_BUMP_Y` times) and `tint`
/// make of it.
fn bumped(n: u8, uv: Option<u8>) -> Vec<u8> {
    let n_y = (W * H) as usize;
    let mut data = input_bytes();
    for b in &mut data[..n_y] {
        *b = b.wrapping_add(n);
    }
    if let Some(v) = uv {
        data[n_y..].fill(v);
    }
    data
}

/// Asserts two packed frames are equal, naming the first differing byte
/// rather than printing both.
#[track_caller]
fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    if got == want {
        return;
    }
    let n_y = (W * H) as usize;
    let at = got
        .iter()
        .zip(want)
        .position(|(a, b)| a != b)
        .unwrap_or(got.len().min(want.len()));
    let plane = if at < n_y { "Y" } else { "UV" };
    let wrong = got.iter().zip(want).filter(|(a, b)| a != b).count();
    panic!(
        "{what}: {wrong} bytes differ, the first at {at} ({plane}): {:?} instead of {:?}",
        got.get(at),
        want.get(at)
    );
}

/// What an output frame shows: its Y plane is the input's or all one
/// value, and likewise its UV plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    /// `None`: the input's luma.
    y: Option<u8>,
    /// `None`: the input's chroma.
    uv: Option<u8>,
    other: bool,
}

const INPUT: Seen = Seen {
    y: None,
    uv: None,
    other: false,
};

fn tinted(u: u8) -> Seen {
    Seen {
        uv: Some(u),
        ..INPUT
    }
}

fn lifted(y: u8, u: Option<u8>) -> Seen {
    Seen {
        y: Some(y),
        uv: u,
        other: false,
    }
}

fn classify(frame: &[u8], input: &[u8]) -> Seen {
    let n = (W * H) as usize;
    let (y, uv) = frame.split_at(n);
    let (iy, iuv) = input.split_at(n);
    let plane = |p: &[u8], i: &[u8]| -> Result<Option<u8>, ()> {
        if p == i {
            Ok(None)
        } else if p.iter().all(|b| *b == p[0]) {
            Ok(Some(p[0]))
        } else {
            Err(())
        }
    };
    match (plane(y, iy), plane(uv, iuv)) {
        (Ok(y), Ok(uv)) => Seen {
            y,
            uv,
            other: false,
        },
        _ => Seen {
            y: None,
            uv: None,
            other: true,
        },
    }
}

/// A system-memory NV12 buffer read back as packed bytes.
fn packed_buffer(buffer: &gst::BufferRef, caps: &gst::CapsRef) -> Vec<u8> {
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

/// One CUDA frame of `input_bytes()`, and the context it lives in.
fn upload() -> (gst::Buffer, gst::Context) {
    let p = gst::parse::launch(&format!(
        "appsrc name=src format=time caps={SYSTEM} ! cudaupload ! capsfilter caps=\"{CUDA}\" ! \
         appsink name=sink sync=false"
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

/// The fields of one status message: the module, and `(id, status,
/// reason)` of every stage in it.
type Message = (String, Vec<(String, String, String)>);

struct Rig {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    module: ObcModule,
    input: gst::Buffer,
    input_bytes: Vec<u8>,
    cuda: bool,
    pts: u64,
    /// The first memory of every buffer entering and leaving the element.
    memories: Arc<Mutex<Vec<(usize, usize)>>>,
    messages: Vec<Message>,
}

impl Rig {
    fn new(factory: Option<Arc<FakeChainFactory>>, stages: &[StageRequest]) -> Rig {
        Rig::build(
            factory.map(|f| f as Arc<dyn StageChainFactory>),
            stages,
            false,
        )
    }

    fn gpu(factory: Arc<FakeChainFactory>, stages: &[StageRequest]) -> Rig {
        require_elements(&["cudaupload", "cudadownload", "appsrc", "appsink"]);
        Rig::build(Some(factory), stages, true)
    }

    fn build(
        factory: Option<Arc<dyn StageChainFactory>>,
        stages: &[StageRequest],
        cuda: bool,
    ) -> Rig {
        require_elements(&["appsrc", "appsink"]);
        module::register().unwrap();
        let (input, context, launch) = if cuda {
            let (input, ctx) = upload();
            (
                input,
                Some(ctx),
                format!(
                    "appsrc name=src format=time is-live=true caps=\"{CUDA}\" ! obcmodule name=m \
                     ! cudadownload ! video/x-raw,format=NV12 ! appsink name=sink sync=false"
                ),
            )
        } else {
            let mut b = gst::Buffer::from_mut_slice(input_bytes());
            b.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
            (
                b,
                None,
                format!(
                    "appsrc name=src format=time is-live=true caps={SYSTEM} ! obcmodule name=m ! \
                     appsink name=sink sync=false"
                ),
            )
        };
        let pipeline = gst::parse::launch(&launch)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        if let Some(ctx) = &context {
            pipeline.set_context(ctx);
        }
        let module = pipeline
            .by_name("m")
            .unwrap()
            .downcast::<ObcModule>()
            .unwrap();
        module.set_factory(factory);
        module.apply(stages).unwrap();
        let memories: Arc<Mutex<Vec<(usize, usize)>>> = Arc::default();
        let last_in: Arc<Mutex<usize>> = Arc::default();
        let l = Arc::clone(&last_in);
        module
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(b) = info.buffer() {
                    *l.lock().unwrap() = b.peek_memory(0).as_ptr() as usize;
                }
                gst::PadProbeReturn::Ok
            });
        let m = Arc::clone(&memories);
        module
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(b) = info.buffer() {
                    let inp = *last_in.lock().unwrap();
                    m.lock()
                        .unwrap()
                        .push((inp, b.peek_memory(0).as_ptr() as usize));
                }
                gst::PadProbeReturn::Ok
            });
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
        pipeline.set_state(gst::State::Playing).unwrap();
        Rig {
            pipeline,
            src,
            sink,
            module,
            input,
            input_bytes: input_bytes(),
            cuda,
            pts: 0,
            memories,
            messages: Vec::new(),
        }
    }

    fn frame(&mut self) -> Seen {
        self.frame_with(gst::BufferFlags::empty())
    }

    /// Pushes the input once more (a shallow copy) with `flags`, and
    /// classifies what came out.
    fn frame_with(&mut self, flags: gst::BufferFlags) -> Seen {
        let bytes = self.frame_bytes_with(flags);
        classify(&bytes, &self.input_bytes)
    }

    /// Pushes the input once more and returns the output's packed bytes.
    fn frame_bytes(&mut self) -> Vec<u8> {
        self.frame_bytes_with(gst::BufferFlags::empty())
    }

    fn frame_bytes_with(&mut self, flags: gst::BufferFlags) -> Vec<u8> {
        let mut b = self.input.copy();
        {
            let b = b.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(self.pts));
            b.set_duration(gst::ClockTime::from_nseconds(33_333_333));
            b.unset_flags(gst_obc::NEW_SOURCE_FLAG | gst_obc::REPEAT_FLAG);
            b.set_flags(flags);
        }
        self.pts += 33_333_333;
        self.src.push_buffer(b).expect("push");
        let sample = self
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .unwrap_or_else(|| panic!("no output frame; bus: {:?}", self.bus_errors()));
        self.drain_messages();
        packed_buffer(sample.buffer().unwrap(), sample.caps().unwrap())
    }

    /// Frames until `done` or `timeout`; returns them.
    fn frames_until(&mut self, timeout: Duration, done: impl Fn(&Rig) -> bool) -> Vec<Seen> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out; stats {:?}, frames {seen:?}",
                self.module.stats()
            );
            seen.push(self.frame());
            std::thread::sleep(Duration::from_millis(10));
        }
        seen
    }

    fn until_status(&mut self, stage: &str, want: EffectStatus) -> Vec<Seen> {
        self.frames_until(Duration::from_secs(10), |r| {
            r.module.stage_status(stage) == want
        })
    }

    fn until_failed(&mut self, stage: &str) -> Vec<Seen> {
        self.frames_until(Duration::from_secs(10), |r| {
            matches!(r.module.stage_status(stage), EffectStatus::Failed(_))
        })
    }

    fn drain_messages(&mut self) {
        let bus = self.pipeline.bus().unwrap();
        while let Some(m) = bus.pop_filtered(&[gst::MessageType::Element]) {
            if let Some(s) = m.structure()
                && s.name() == STATUS_MESSAGE
            {
                let module = s.get::<String>("module").unwrap();
                let stages = s
                    .fields()
                    .filter(|f| *f != "module" && !f.ends_with("-reason"))
                    .map(|f| {
                        (
                            f.to_string(),
                            s.get::<String>(f).unwrap(),
                            s.get::<String>(format!("{f}-reason").as_str()).unwrap(),
                        )
                    })
                    .collect();
                self.messages.push((module, stages));
            }
        }
    }

    /// The last status message's `(status, reason)` of `stage`.
    fn last_message(&self, stage: &str) -> Option<(String, String)> {
        self.messages.iter().rev().find_map(|(_, stages)| {
            stages
                .iter()
                .find(|(s, _, _)| s == stage)
                .map(|(_, st, r)| (st.clone(), r.clone()))
        })
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

    /// The input's bytes now (read back from its memory).
    fn input_now(&self) -> Vec<u8> {
        let caps: gst::Caps = if self.cuda { CUDA } else { SYSTEM }.parse().unwrap();
        packed_buffer(&self.input, &caps)
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

/// Frames seen until a status was reached: the input, except that the
/// last may already be `processed` (the load can end between the check and
/// the frame).
fn assert_warm(warm: &[Seen], processed: Seen) {
    let (last, before) = warm.split_last().expect("at least one frame");
    assert!(
        before.iter().all(|s| *s == INPUT) && (*last == INPUT || *last == processed),
        "the input until loaded: {warm:?}"
    );
}

fn get(a: &std::sync::atomic::AtomicU64) -> u64 {
    FakeChainLog::get(a)
}

#[test]
fn registers_once_with_nv12_pads_in_cuda_and_system_memory() {
    module::register().unwrap();
    module::register().expect("registering twice is fine");
    let e = gst::ElementFactory::make(module::ELEMENT_NAME)
        .build()
        .unwrap();
    let m = module::from_element(&e).expect("an obcmodule element downcasts");
    assert!(m.factory().is_none());
    assert!(m.stages().is_empty());
    assert_eq!(m.stage_status("tint"), EffectStatus::Off);
    let s = m.stats();
    assert_eq!(s.module, "");
    assert_eq!(s.kind, None);
    assert!(s.stages.is_empty() && !s.cuda_active && s.last_error.is_none());
    for pad in ["sink", "src"] {
        let caps = e.static_pad(pad).unwrap().pad_template_caps();
        assert_eq!(
            caps.to_string(),
            "video/x-raw(memory:CUDAMemory), format=(string)NV12; video/x-raw, \
             format=(string)NV12",
            "{pad}"
        );
    }
}

#[test]
fn apply_refuses_a_stage_listed_twice() {
    module::register().unwrap();
    let m = ObcModule::new(Some("m"));
    let e = m.apply(&[tint(), lift(), tint()]).unwrap_err();
    assert!(e.contains("listed twice"), "{e}");
    assert!(m.stages().is_empty(), "nothing applied");
    m.apply(&[tint(), StageRequest::new(id("lift"))]).unwrap();
    assert_eq!(m.stage_status("tint"), EffectStatus::Loading);
    assert_eq!(m.stage_status("lift"), EffectStatus::Off);
    assert_eq!(m.stats().stage(&id("lift")).status, EffectStatus::Off);
}

#[test]
fn a_test_pattern_stream_is_tinted_once_loaded() {
    require_elements(&["videotestsrc", "appsink"]);
    module::register().unwrap();
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let p = gst::parse::launch(&format!(
        "videotestsrc is-live=true pattern=smpte ! {SYSTEM} ! obcmodule name=m ! \
         appsink name=sink sync=false max-buffers=2 drop=true"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let m = p.by_name("m").unwrap().downcast::<ObcModule>().unwrap();
    m.set_factory(Some(fake.clone()));
    m.apply(&[tint()]).unwrap();
    let sink = p
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    p.set_state(gst::State::Playing).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut tinted_frames = 0;
    while tinted_frames < 5 {
        assert!(Instant::now() < deadline, "{:?}", m.stats());
        let s = sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("a frame");
        let bytes = packed_buffer(s.buffer().unwrap(), s.caps().unwrap());
        let uv = &bytes[(W * H) as usize..];
        if uv.iter().all(|b| *b == FAKE_TINT_UV) {
            tinted_frames += 1;
            // The pattern's luma is not touched.
            assert!(bytes[..(W * H) as usize].iter().any(|b| *b != bytes[0]));
        }
    }
    p.set_state(gst::State::Null).unwrap();
    assert_eq!(
        m.stage_status("tint"),
        EffectStatus::Loading,
        "restarts load"
    );
    let s = m.stats();
    assert_eq!(s.module, "fake");
    assert!(
        s.frames_processed >= 5 && s.stage(&id("tint")).frames >= 5,
        "{s:?}"
    );
    assert_eq!(get(&fake.log.loads), 1);
    assert_eq!(get(&fake.log.unloads), 1, "stopping unloads");
}

#[test]
fn frames_pass_through_until_loaded_then_every_stage_runs() {
    let fake = FakeChainFactory::host(FakeChainConfig {
        load_delay: Duration::from_millis(300),
        ..FakeChainConfig::default()
    });
    let mut r = Rig::new(Some(fake.clone()), &[lift(), tint()]);
    let warm = r.until_status("tint", EffectStatus::Ready);
    assert!(warm.len() > 1, "the load takes a while");
    assert_warm(&warm, lifted(FAKE_LIFT_Y, Some(FAKE_TINT_UV)));
    assert_eq!(r.module.stage_status("lift"), EffectStatus::Ready);
    for _ in 0..5 {
        assert_eq!(r.frame(), lifted(FAKE_LIFT_Y, Some(FAKE_TINT_UV)));
    }
    assert_eq!(
        r.last_message("tint"),
        Some(("ready".into(), String::new()))
    );
    assert_eq!(r.messages.last().unwrap().0, "fake");
    let s = r.module.stats();
    assert_eq!(s.kind, Some(FrameKind::Host));
    assert!(s.frames_processed >= 5 && s.frames_passthrough + 1 >= warm.len() as u64);
    assert_eq!((s.loads, s.frames_converted, s.cuda_active), (1, 0, false));
    assert!(s.last_load_ms.unwrap() >= 300.0);
    let (stages, _) = fake.log.last_request.lock().unwrap().clone().unwrap();
    assert_eq!(stages, ["lift", "tint"], "in the requested order");
    assert_eq!(r.input_now(), r.input_bytes, "the input is never written");
    r.stop();
    assert_eq!(get(&fake.log.unloads), 1);
    assert_eq!(get(&fake.log.misuse), 0);
}

#[test]
fn live_parameters_apply_on_the_next_frame_without_a_load() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[tint()]);
    r.until_status("tint", EffectStatus::Ready);
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    r.module.apply(&[tint().with_live("u", 40i64)]).unwrap();
    assert_eq!(r.frame(), tinted(40), "the next frame");
    r.module.apply(&[tint().with_live("u", 41i64)]).unwrap();
    assert_eq!(r.frame(), tinted(41));
    assert_eq!(get(&fake.log.loads), 1, "no load for a live parameter");
    let live = fake.log.last_live.lock().unwrap().clone();
    assert_eq!(live[&id("tint")]["u"], 41i64.into());
    assert_eq!(r.module.stage_status("tint"), EffectStatus::Ready);
}

#[test]
fn a_load_time_parameter_reloads_the_chain() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[lift()]);
    r.until_status("lift", EffectStatus::Ready);
    assert_eq!(r.frame(), lifted(FAKE_LIFT_Y, None));
    r.module.apply(&[lift().with_param("y", 77i64)]).unwrap();
    let seen = r.frames_until(Duration::from_secs(10), |_| get(&fake.log.loads) == 2);
    // The last frame may already be the new chain's: the switch can end
    // between the check and the frame.
    assert!(
        seen.iter()
            .all(|s| [lifted(FAKE_LIFT_Y, None), INPUT, lifted(77, None)].contains(s)),
        "the previous chain or the input while switching: {seen:?}"
    );
    r.until_status("lift", EffectStatus::Ready);
    assert_eq!(r.frame(), lifted(77, None));
    let (_, params) = fake.log.last_request.lock().unwrap().clone().unwrap();
    assert_eq!(params[&id("lift")]["y"], 77i64.into());
    assert_eq!(get(&fake.log.creates), 1, "a switch on the same chain");
}

#[test]
fn every_stage_off_is_passthrough_without_a_copy_or_a_chain_call() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[StageRequest::new(id("tint"))]);
    for _ in 0..5 {
        assert_eq!(r.frame(), INPUT);
    }
    let mem = r.memories.lock().unwrap().clone();
    assert!(
        mem.iter().all(|(i, o)| i == o),
        "the input memory itself: {mem:?}"
    );
    assert_eq!(get(&fake.log.creates), 0, "no chain is ever made");
    assert_eq!(r.module.stage_status("tint"), EffectStatus::Off);
    // On, then off again: the input at once, and the chain unloaded.
    r.module.apply(&[tint()]).unwrap();
    r.until_status("tint", EffectStatus::Ready);
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    r.memories.lock().unwrap().clear();
    let frames_before = get(&fake.log.frames);
    r.module.apply(&[StageRequest::new(id("tint"))]).unwrap();
    for _ in 0..5 {
        assert_eq!(r.frame(), INPUT, "the next frame is the input");
    }
    assert_eq!(get(&fake.log.frames), frames_before);
    let mem = r.memories.lock().unwrap().clone();
    assert!(mem.iter().all(|(i, o)| i == o), "{mem:?}");
    r.frames_until(Duration::from_secs(5), |_| get(&fake.log.unloads) == 1);
    assert_eq!(r.module.stage_status("tint"), EffectStatus::Off);
    assert_eq!(r.last_message("tint"), Some(("off".into(), String::new())));
}

#[test]
fn a_load_failure_fails_open_with_the_reason() {
    let fake = FakeChainFactory::host(FakeChainConfig {
        fail_load: Some("the model is missing".into()),
        ..FakeChainConfig::default()
    });
    let mut r = Rig::new(Some(fake.clone()), &[tint(), lift()]);
    let seen = r.until_failed("tint");
    assert!(seen.iter().all(|s| *s == INPUT), "{seen:?}");
    for _ in 0..3 {
        assert_eq!(r.frame(), INPUT);
    }
    for stage in ["tint", "lift"] {
        let st = r.module.stage_status(stage);
        assert_eq!(st.reason(), Some("the model is missing"), "{stage}");
        assert_eq!(
            r.last_message(stage),
            Some(("failed".into(), "the model is missing".into()))
        );
    }
    let s = r.module.stats();
    assert_eq!(s.last_error.as_deref(), Some("the model is missing"));
    assert_eq!((s.frames_processed, s.kind), (0, None));
    // A retry once the cause is gone.
    fake.set(|c| c.fail_load = None);
    r.module.apply(&[tint(), lift()]).unwrap();
    r.until_status("tint", EffectStatus::Ready);
    assert_eq!(r.frame(), lifted(FAKE_LIFT_Y, Some(FAKE_TINT_UV)));
}

#[test]
fn a_stage_failing_to_load_is_dropped_and_the_rest_runs() {
    let fake = FakeChainFactory::host(FakeChainConfig {
        fail_load_stage: Some("lift".into()),
        ..FakeChainConfig::default()
    });
    let mut r = Rig::new(Some(fake.clone()), &[lift(), tint()]);
    r.until_status("tint", EffectStatus::Ready);
    let lift_status = r.module.stage_status("lift");
    assert!(
        lift_status
            .reason()
            .is_some_and(|m| m.contains("lift cannot load")),
        "{lift_status:?}"
    );
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    let (stages, _) = fake.log.last_request.lock().unwrap().clone().unwrap();
    assert_eq!(stages, ["tint"]);
    let (st, reason) = r.last_message("lift").unwrap();
    assert_eq!(st, "failed");
    assert!(reason.contains("lift cannot load"), "{reason}");
}

#[test]
fn a_stage_failing_on_a_frame_is_dropped_and_the_frame_runs_without_it() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[lift(), tint()]);
    r.until_status("tint", EffectStatus::Ready);
    assert_eq!(r.frame(), lifted(FAKE_LIFT_Y, Some(FAKE_TINT_UV)));
    fake.set(|c| c.fail_frame_stage = Some("lift".into()));
    assert_eq!(
        r.frame(),
        tinted(FAKE_TINT_UV),
        "the failing frame itself runs again without the stage"
    );
    let st = r.module.stage_status("lift");
    assert!(
        st.reason()
            .is_some_and(|m| m.contains("the lift stage failed on a frame")),
        "{st:?}"
    );
    for _ in 0..5 {
        assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    }
    assert_eq!(r.module.stage_status("tint"), EffectStatus::Ready);
    assert_eq!(get(&fake.log.loads), 1, "the rest needs no load");
    // A change of the stage's request retries it.
    fake.set(|c| c.fail_frame_stage = None);
    r.module
        .apply(&[lift().with_param("y", 50i64), tint()])
        .unwrap();
    r.until_status("lift", EffectStatus::Ready);
    assert_eq!(r.frame(), lifted(50, Some(FAKE_TINT_UV)));
}

#[test]
fn a_chain_failing_on_a_frame_fails_open_and_unloads() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[tint()]);
    r.until_status("tint", EffectStatus::Ready);
    fake.set(|c| c.fail_frame = true);
    assert_eq!(r.frame(), INPUT, "the failed frame is the input");
    let st = r.module.stage_status("tint");
    assert!(
        st.reason()
            .is_some_and(|m| m.contains("processing a frame failed")),
        "{st:?}"
    );
    r.frames_until(Duration::from_secs(5), |_| get(&fake.log.unloads) == 1);
    let calls = get(&fake.log.frames);
    for _ in 0..3 {
        assert_eq!(r.frame(), INPUT);
    }
    assert_eq!(get(&fake.log.frames), calls, "no chain call any more");
    assert!(
        r.module
            .stats()
            .last_error
            .is_some_and(|e| e.contains("fake: the frame failed"))
    );
}

/// A stage failing alone on a frame after the chain wrote it: the frame
/// runs again from the input, so `bump` (not idempotent) shows once and
/// what the failing call wrote is gone.
fn assert_a_stage_failing_midway_reruns_from_the_input(fake: &FakeChainFactory, r: &mut Rig) {
    r.until_status("tint", EffectStatus::Ready);
    assert_bytes(
        &r.frame_bytes(),
        &bumped(FAKE_BUMP_Y, Some(FAKE_TINT_UV)),
        "before the failure",
    );
    fake.set(|c| {
        c.fail_frame_stage = Some("tint".into());
        c.scribble_on_fail = true;
    });
    assert_bytes(
        &r.frame_bytes(),
        &bumped(FAKE_BUMP_Y, None),
        "the failing frame runs again from the input, without the stage",
    );
    assert!(matches!(
        r.module.stage_status("tint"),
        EffectStatus::Failed(_)
    ));
    for _ in 0..3 {
        assert_bytes(
            &r.frame_bytes(),
            &bumped(FAKE_BUMP_Y, None),
            "after the failure",
        );
    }
    assert_eq!(r.input_now(), r.input_bytes, "the input is never written");
}

/// The chain failing as a whole after it wrote the frame: the frame is
/// the input, untouched.
fn assert_a_chain_failing_midway_outputs_the_input(fake: &FakeChainFactory, r: &mut Rig) {
    r.until_status("tint", EffectStatus::Ready);
    assert_bytes(
        &r.frame_bytes(),
        &bumped(FAKE_BUMP_Y, Some(FAKE_TINT_UV)),
        "before the failure",
    );
    fake.set(|c| {
        c.fail_frame = true;
        c.scribble_on_fail = true;
    });
    assert_bytes(
        &r.frame_bytes(),
        &r.input_bytes,
        "the failed frame is the input",
    );
    assert!(matches!(
        r.module.stage_status("bump"),
        EffectStatus::Failed(_)
    ));
    for _ in 0..3 {
        assert_bytes(&r.frame_bytes(), &r.input_bytes, "after the failure");
    }
    assert_eq!(r.input_now(), r.input_bytes, "the input is never written");
}

#[test]
fn a_stage_failing_midway_on_a_frame_reruns_from_the_input() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[bump(), tint()]);
    assert_a_stage_failing_midway_reruns_from_the_input(&fake, &mut r);
}

#[test]
fn a_chain_failing_midway_on_a_frame_outputs_the_untouched_input() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[bump(), tint()]);
    assert_a_chain_failing_midway_outputs_the_input(&fake, &mut r);
}

#[test]
fn the_only_stage_failing_midway_on_a_frame_outputs_the_untouched_input() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[bump()]);
    r.until_status("bump", EffectStatus::Ready);
    assert_bytes(
        &r.frame_bytes(),
        &bumped(FAKE_BUMP_Y, None),
        "before the failure",
    );
    fake.set(|c| {
        c.fail_frame_stage = Some("bump".into());
        c.scribble_on_fail = true;
    });
    assert_bytes(&r.frame_bytes(), &r.input_bytes, "no stage left to run");
    assert!(matches!(
        r.module.stage_status("bump"),
        EffectStatus::Failed(_)
    ));
}

#[test]
fn a_cuda_only_chain_on_system_memory_is_refused_with_the_reason() {
    let fake = FakeChainFactory::cuda(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[tint()]);
    let seen = r.until_failed("tint");
    assert!(seen.iter().all(|s| *s == INPUT));
    let reason = r.module.stage_status("tint").reason().unwrap().to_owned();
    assert!(
        reason.contains("take cuda frames only") && reason.contains("system memory"),
        "{reason}"
    );
    assert_eq!(get(&fake.log.creates), 0);
    assert_eq!(r.frame(), INPUT);
}

#[test]
fn without_a_factory_every_stage_fails_with_the_reason() {
    let mut r = Rig::new(None, &[tint()]);
    r.until_failed("tint");
    assert_eq!(
        r.module.stage_status("tint").reason(),
        Some("no module is configured for these stages")
    );
    assert_eq!(r.frame(), INPUT);
}

#[test]
fn the_new_source_flag_resets_the_temporal_state_and_reloads_a_dropped_stage() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let still = StageRequest::on(id("still"));
    let mut r = Rig::new(Some(fake.clone()), &[tint(), still]);
    r.until_status("still", EffectStatus::Ready);
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    assert_eq!(get(&fake.log.resets), 0);
    assert_eq!(
        r.frame_with(gst_obc::NEW_SOURCE_FLAG),
        tinted(FAKE_TINT_UV),
        "the rest runs on the new source's first frame"
    );
    assert_eq!(get(&fake.log.resets), 1, "reset before that frame");
    r.until_status("still", EffectStatus::Ready);
    assert_eq!(get(&fake.log.loads), 2, "the dropped stage loaded again");
    let (stages, _) = fake.log.last_request.lock().unwrap().clone().unwrap();
    assert_eq!(stages, ["tint", "still"]);
    for _ in 0..3 {
        r.frame();
    }
    assert_eq!(get(&fake.log.resets), 1, "once per new source");
}

#[test]
fn a_repeat_goes_to_the_chain_as_a_repeat() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[tint()]);
    r.until_status("tint", EffectStatus::Ready);
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    assert_eq!(get(&fake.log.repeats), 0, "a fresh frame");
    let frames = get(&fake.log.frames);
    assert_eq!(
        r.frame_with(gst_obc::REPEAT_FLAG),
        tinted(FAKE_TINT_UV),
        "the chain ran on the repeat"
    );
    assert_eq!(get(&fake.log.repeats), 1);
    assert_eq!(get(&fake.log.frames), frames + 1);
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    assert_eq!(get(&fake.log.repeats), 1, "the next fresh frame is not one");
}

/// The repeat flag as the session's output carries it: set by the output
/// clock (here appsrc), through an element that writes every frame into a
/// new buffer and copies the input's metadata with BaseTransform's
/// default, as `obceffect` does (`prepare_output_buffer`; here a pair of
/// `videoconvert`s, `obceffect` being CUDA only), then an `obcmodule` that
/// processes in place, then a second one (as frame generation's chain runs
/// after the others). Both chains get the repeat as a repeat.
#[test]
fn a_repeat_reaches_a_second_module_through_a_copying_element() {
    require_elements(&["appsrc", "appsink", "videoconvert"]);
    module::register().unwrap();
    let launch = format!(
        "appsrc name=src format=time is-live=true caps={SYSTEM} ! videoconvert ! \
         video/x-raw,format=I420 ! videoconvert ! video/x-raw,format=NV12 ! obcmodule name=a ! \
         obcmodule name=b ! appsink name=sink sync=false"
    );
    let pipeline = gst::parse::launch(&launch)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let first = FakeChainFactory::host(FakeChainConfig::default());
    let second = FakeChainFactory::host(FakeChainConfig::default());
    for (name, fake, stage) in [("a", &first, tint()), ("b", &second, lift())] {
        let m = pipeline
            .by_name(name)
            .unwrap()
            .downcast::<ObcModule>()
            .unwrap();
        m.set_factory(Some(Arc::clone(fake) as Arc<dyn StageChainFactory>));
        m.apply(&[stage]).unwrap();
    }
    // What leaves the copying element: a new buffer, the flags of the input.
    let copied: Arc<Mutex<Vec<bool>>> = Arc::default();
    let c = Arc::clone(&copied);
    pipeline
        .by_name("a")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                c.lock().unwrap().push(gst_obc::has_repeat_flag(b));
            }
            gst::PadProbeReturn::Ok
        });
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
    pipeline.set_state(gst::State::Playing).unwrap();
    let input = input_bytes();
    let mut pts = 0u64;
    let mut push = |flags: gst::BufferFlags| -> Vec<u8> {
        let mut b = gst::Buffer::from_mut_slice(input.clone());
        {
            let b = b.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(pts));
            b.set_duration(gst::ClockTime::from_nseconds(33_333_333));
            b.set_flags(flags);
        }
        pts += 33_333_333;
        src.push_buffer(b).expect("push");
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("an output frame");
        packed_buffer(sample.buffer().unwrap(), sample.caps().unwrap())
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while get(&first.log.frames) == 0 || get(&second.log.frames) == 0 {
        assert!(Instant::now() < deadline, "the chains never ran");
        push(gst::BufferFlags::empty());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(get(&first.log.repeats), 0);
    assert_eq!(get(&second.log.repeats), 0);
    let both = Seen {
        y: Some(FAKE_LIFT_Y),
        ..tinted(FAKE_TINT_UV)
    };
    let (f1, f2) = (get(&first.log.frames), get(&second.log.frames));
    let out = push(gst_obc::REPEAT_FLAG);
    assert_eq!(
        classify(&out, &input),
        both,
        "both chains ran on the repeat"
    );
    assert_eq!(get(&first.log.repeats), 1);
    assert_eq!(
        get(&second.log.repeats),
        1,
        "the second element saw the flag"
    );
    assert_eq!(get(&first.log.frames), f1 + 1);
    assert_eq!(get(&second.log.frames), f2 + 1);
    assert_eq!(copied.lock().unwrap().last(), Some(&true));
    // A fresh frame is not a repeat.
    push(gst::BufferFlags::empty());
    assert_eq!(get(&first.log.repeats), 1);
    assert_eq!(get(&second.log.repeats), 1);
    assert_eq!(copied.lock().unwrap().last(), Some(&false));
    pipeline.set_state(gst::State::Null).unwrap();
}

/// The only stage switched off while the element runs (frame generation
/// off in its own chain at a capture mode change): the chain is unloaded
/// and dropped at once, so nothing of it stays loaded, and the frames pass
/// through.
#[test]
fn the_only_stage_switched_off_unloads_the_chain() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::new(Some(fake.clone()), &[tint()]);
    r.until_status("tint", EffectStatus::Ready);
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    let mut off = tint();
    off.enabled = false;
    r.module.apply(&[off]).unwrap();
    r.frames_until(Duration::from_secs(5), |_| get(&fake.log.unloads) == 1);
    assert_eq!(r.frame(), INPUT);
    let frames = get(&fake.log.frames);
    assert_eq!(r.frame(), INPUT);
    assert_eq!(get(&fake.log.frames), frames, "no chain call");
    assert_eq!(get(&fake.log.unloads), 1);
    r.stop();
    assert_eq!(get(&fake.log.unloads), 1, "nothing left to unload");
}

#[test]
fn stopping_unloads_and_a_restart_loads_again() {
    let fake = FakeChainFactory::host(FakeChainConfig {
        fail_load: Some("broken".into()),
        ..FakeChainConfig::default()
    });
    let mut r = Rig::new(Some(fake.clone()), &[tint()]);
    r.until_failed("tint");
    r.stop();
    assert_eq!(
        r.module.stage_status("tint"),
        EffectStatus::Loading,
        "a restart retries"
    );
    fake.set(|c| c.fail_load = None);
    r.pipeline.set_state(gst::State::Playing).unwrap();
    r.until_status("tint", EffectStatus::Ready);
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    r.stop();
    assert_eq!(get(&fake.log.unloads), 1, "the chain that loaded");
    assert_eq!(
        get(&fake.log.creates),
        2,
        "the failed chain went with its failure"
    );
}

#[test]
fn a_restart_loads_only_at_the_caps_it_is_given_again() {
    // The element alone, fed by a pad of the test's: a restart sends no
    // caps until the test does (a source would resend its own at once).
    module::register().unwrap();
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let m = gst::ElementFactory::make("obcmodule")
        .build()
        .unwrap()
        .downcast::<ObcModule>()
        .unwrap();
    m.set_factory(Some(fake.clone() as Arc<dyn StageChainFactory>));
    m.apply(&[tint()]).unwrap();
    let src = gst::Pad::builder(gst::PadDirection::Src).build();
    src.set_active(true).unwrap();
    src.link(&m.static_pad("sink").unwrap()).unwrap();
    let start = |caps: &str| {
        m.set_state(gst::State::Playing).unwrap();
        assert!(src.push_event(gst::event::StreamStart::new("restart")));
        assert!(src.push_event(gst::event::Caps::new(&caps.parse::<gst::Caps>().unwrap())));
        assert!(
            src.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::ClockTime,
            >::new()))
        );
    };
    let until_ready = || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while m.stage_status("tint") != EffectStatus::Ready {
            assert!(Instant::now() < deadline, "{:?}", m.stats());
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    start(SYSTEM);
    until_ready();
    m.set_state(gst::State::Null).unwrap();
    m.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        *fake.log.load_sizes.lock().unwrap(),
        [(W, H)],
        "nothing loads at the last run's size before the caps"
    );
    assert_eq!(m.stage_status("tint"), EffectStatus::Loading);
    start(&SYSTEM.replace("width=64,height=48", "width=32,height=24"));
    until_ready();
    assert_eq!(*fake.log.load_sizes.lock().unwrap(), [(W, H), (32, 24)]);
    m.set_state(gst::State::Null).unwrap();
}

// ---------------------------------------------------------------- GPU

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_host_chain_runs_on_cuda_frames_through_a_host_copy() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::gpu(fake.clone(), &[lift(), tint()]);
    let warm = r.until_status("tint", EffectStatus::Ready);
    assert_warm(&warm, lifted(FAKE_LIFT_Y, Some(FAKE_TINT_UV)));
    for _ in 0..5 {
        assert_eq!(r.frame(), lifted(FAKE_LIFT_Y, Some(FAKE_TINT_UV)));
    }
    r.module
        .apply(&[lift(), tint().with_live("u", 33i64)])
        .unwrap();
    assert_eq!(r.frame(), lifted(FAKE_LIFT_Y, Some(33)));
    let s = r.module.stats();
    assert_eq!(s.kind, Some(FrameKind::Host));
    assert!(s.cuda_active);
    assert!(
        s.frames_converted >= 6 && s.frames_converted == s.frames_processed,
        "{s:?}"
    );
    assert_eq!(r.input_now(), r.input_bytes, "the input is never written");
    // A failing frame through the copy is the input, untouched.
    fake.set(|c| c.fail_frame = true);
    assert_eq!(r.frame(), INPUT);
    r.stop();
    assert!(!r.module.stats().cuda_active);
    assert_eq!(get(&fake.log.unloads), 1);
    assert_eq!(get(&fake.log.misuse), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_cuda_chain_runs_on_cuda_frames_in_place() {
    let fake = FakeChainFactory::cuda(FakeChainConfig {
        load_delay: Duration::from_millis(200),
        ..FakeChainConfig::default()
    });
    let mut r = Rig::gpu(fake.clone(), &[tint(), lift()]);
    let warm = r.until_status("tint", EffectStatus::Ready);
    assert_warm(&warm, lifted(FAKE_LIFT_Y, Some(FAKE_TINT_UV)));
    for _ in 0..5 {
        assert_eq!(r.frame(), lifted(FAKE_LIFT_Y, Some(FAKE_TINT_UV)));
    }
    // A stage failing on a frame: dropped, the frame runs without it.
    fake.set(|c| c.fail_frame_stage = Some("lift".into()));
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    assert!(matches!(
        r.module.stage_status("lift"),
        EffectStatus::Failed(_)
    ));
    assert_eq!(r.frame(), tinted(FAKE_TINT_UV));
    let s = r.module.stats();
    assert_eq!(s.kind, Some(FrameKind::Cuda));
    assert_eq!(s.frames_converted, 0);
    assert!(s.cuda_active);
    assert_eq!(r.input_now(), r.input_bytes, "the input is never written");
    r.stop();
    assert_eq!(get(&fake.log.unloads), 1);
    assert_eq!(get(&fake.log.misuse), 0, "context and stream rules kept");
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_every_stage_off_passes_cuda_frames_through_without_cuda() {
    let fake = FakeChainFactory::cuda(FakeChainConfig::default());
    let mut r = Rig::gpu(fake.clone(), &[StageRequest::new(id("tint"))]);
    for _ in 0..5 {
        assert_eq!(r.frame(), INPUT);
    }
    let mem = r.memories.lock().unwrap().clone();
    assert!(mem.iter().all(|(i, o)| i == o), "{mem:?}");
    assert!(!r.module.stats().cuda_active);
    assert_eq!(get(&fake.log.creates), 0);
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_cuda_chain_failing_midway_restores_the_frame() {
    let fake = FakeChainFactory::cuda(FakeChainConfig::default());
    let mut r = Rig::gpu(fake.clone(), &[bump(), tint()]);
    assert_a_stage_failing_midway_reruns_from_the_input(&fake, &mut r);
    let fake = FakeChainFactory::cuda(FakeChainConfig::default());
    let mut r = Rig::gpu(fake.clone(), &[bump(), tint()]);
    assert_a_chain_failing_midway_outputs_the_input(&fake, &mut r);
    r.stop();
    assert_eq!(get(&fake.log.misuse), 0, "context and stream rules kept");
}

#[test]
#[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
fn gpu_a_host_chain_failing_midway_through_the_host_copy_restores_the_frame() {
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::gpu(fake.clone(), &[bump(), tint()]);
    assert_a_stage_failing_midway_reruns_from_the_input(&fake, &mut r);
    let fake = FakeChainFactory::host(FakeChainConfig::default());
    let mut r = Rig::gpu(fake.clone(), &[bump(), tint()]);
    assert_a_chain_failing_midway_outputs_the_input(&fake, &mut r);
}
