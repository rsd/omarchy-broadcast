//! The PipeWire camera node against the user's running PipeWire daemon
//! (feature `pipewire`). `#[ignore]`: it needs the user PipeWire daemon
//! (with WirePlumber) and GStreamer's `pipewiresrc` (gst-plugin-pipewire).
//!
//! It creates exactly one node, named `omarchy-broadcast-test-<pid>`, fed by a
//! loopback writer into a `fakesink` (no device), and consumes it with
//! in-process `pipewiresrc` pipelines that target it by name with
//! `node.dont-fallback` and `node.dont-reconnect`, so a consumer can never
//! fall back to a real camera; a consumer whose link does not show up on
//! our node within 2 s is stopped at once and the test fails. A refused
//! consumer is held in its failed state for 1 s, and must not count as a
//! consumer beyond a 300 ms negotiating window. It changes no PipeWire or
//! WirePlumber setting. Everything is stopped by guards,
//! also on failure.
//!
//! `cargo test -p obc-pipeline --features pipewire --test pipewire_live -- --ignored --nocapture --test-threads=1`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;
use obc_core::OutputContract;
use obc_pipeline::loopback::{LoopbackWriter, WriterHandle, WriterSink, writer_caps_string};
use obc_pipeline::pipewire::format::{PwFormat, enum_format};
use obc_pipeline::pipewire::{PipeWireOutput, PwEvent, PwOutputConfig, PwState};
use pipewire as pw;
use pw::spa;

/// A flat colour (limited range) the frames are filled with.
const COLOUR: (u8, u8, u8) = (81, 90, 240);

/// A `pipewiresrc` consumer of our node; set to NULL when dropped.
struct Consumer {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    what: String,
    /// The PLAYING state change failed at once.
    start_error: Option<String>,
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Consumer {
    fn start(target: &str, caps: &str) -> Consumer {
        let src = gst::ElementFactory::make("pipewiresrc")
            .property("target-object", target)
            .property(
                "stream-properties",
                gst::Structure::builder("props")
                    .field("node.dont-fallback", "true")
                    .field("node.dont-reconnect", "true")
                    .build(),
            )
            .build()
            .expect("pipewiresrc (gst-plugin-pipewire) is needed");
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", caps.parse::<gst::Caps>().unwrap())
            .build()
            .unwrap();
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .max_buffers(2)
            .leaky_type(gst_app::AppLeakyType::Downstream)
            .build();
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([&src, &filter, sink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([&src, &filter, sink.upcast_ref()]).unwrap();
        let mut c = Consumer {
            pipeline,
            sink,
            what: caps.to_string(),
            start_error: None,
        };
        c.start_error = c
            .pipeline
            .set_state(gst::State::Playing)
            .err()
            .map(|e| e.to_string());
        c
    }

    /// The first ERROR on the consumer's bus, if any so far.
    fn error(&self) -> Option<String> {
        let bus = self.pipeline.bus().unwrap();
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(e) = msg.view() {
                return Some(format!("{} ({:?})", e.error(), e.debug()));
            }
        }
        None
    }

    fn pull(&self, timeout: Duration) -> Option<gst::Sample> {
        self.sink
            .try_pull_sample(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64))
    }
}

/// The node, its events and the writer feeding it; stopped when dropped.
struct Rig {
    output: Option<PipeWireOutput>,
    writer: LoopbackWriter,
    events: Arc<Mutex<Vec<PwEvent>>>,
    name: String,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.writer.set_tap(None);
        if let Some(o) = self.output.take() {
            o.stop();
        }
        let _ = self.writer.stop();
    }
}

impl Rig {
    fn output(&self) -> &PipeWireOutput {
        self.output.as_ref().unwrap()
    }

    fn failed(&self) -> Vec<PwEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, PwEvent::State(PwState::Failed(_))))
            .cloned()
            .collect()
    }
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Starts a consumer that must link to OUR node within 2 s; otherwise it
/// is stopped at once and the test fails.
fn linked_consumer(rig: &Rig, caps: &str) -> Consumer {
    let c = Consumer::start(&rig.name, caps);
    assert_eq!(c.start_error, None, "{caps}");
    if !wait_until(Duration::from_secs(2), || rig.output().consumers() == 1) {
        let err = c.error();
        drop(c);
        panic!("the {caps} consumer did not link to our node within 2 s (error: {err:?})");
    }
    c
}

fn video_info(sample: &gst::Sample) -> gst_video::VideoInfo {
    gst_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap()
}

fn consumer_gone(rig: &Rig, c: Consumer) {
    drop(c);
    assert!(
        wait_until(Duration::from_secs(2), || rig.output().consumers() == 0),
        "the consumer's link did not go away"
    );
    assert!(
        wait_until(Duration::from_secs(2), || rig.output().state()
            == PwState::Ready),
        "not Ready after the consumer left: {:?}",
        rig.output().state()
    );
}

/// A system-memory frame of the writer's caps, flat [`COLOUR`].
fn colour_frame(contract: &OutputContract) -> gst::Buffer {
    let caps: gst::Caps = writer_caps_string(contract).parse().unwrap();
    let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
    let (y, u, v) = COLOUR;
    let mut data = vec![0u8; info.size()];
    let luma_end = info.offset()[1];
    data[..luma_end].fill(y);
    for uv in data[luma_end..].as_chunks_mut::<2>().0 {
        uv[0] = u;
        uv[1] = v;
    }
    gst::Buffer::from_mut_slice(data)
}

/// Pushes colour frames at the contract rate until dropped.
struct Pusher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Pusher {
    fn start(handle: WriterHandle) -> Pusher {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let frame = colour_frame(handle.contract());
        let period = Duration::from_nanos(handle.contract().frame_duration_ns());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                handle.push(frame.copy());
                std::thread::sleep(period);
            }
        });
        Pusher {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Pusher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Our node, as `pw-dump` sees it (read only).
fn pw_dump(id: u32) -> String {
    let out = std::process::Command::new("pw-dump")
        .arg(id.to_string())
        .output()
        .expect("pw-dump");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The links from `node`, as `pw-dump` sees them (read only): (link id,
/// state, error).
fn links_from(node: u32) -> Vec<(u64, String, Option<String>)> {
    let out = std::process::Command::new("pw-dump")
        .output()
        .expect("pw-dump");
    let all: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    all.as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["type"] == "PipeWire:Interface:Link")
        .filter(|o| o["info"]["output-node-id"] == node)
        .map(|o| {
            (
                o["id"].as_u64().unwrap_or_default(),
                o["info"]["state"].as_str().unwrap_or("?").to_string(),
                o["info"]["error"].as_str().map(str::to_string),
            )
        })
        .collect()
}

/// How long a refused consumer's link may count while it is negotiated
/// (the daemon's start debounce absorbs it).
const NEGOTIATING: Duration = Duration::from_millis(300);

/// A consumer asking for something not offered fails or gets no caps,
/// receives nothing, and leaves our node as it was. Held in its failed
/// state (not set to NULL) for 1 s after its error: its link, in error,
/// must not count as a consumer beyond a short negotiating window.
fn refused_consumer(rig: &Rig, caps: &str, node: u32) {
    let c = Consumer::start(&rig.name, caps);
    let t0 = Instant::now();
    let mut samples: Vec<(Duration, u32)> = Vec::new();
    let mut sample = || samples.push((t0.elapsed(), rig.output().consumers()));
    let deadline = t0 + Duration::from_secs(3);
    let mut error = c.start_error.clone();
    while Instant::now() < deadline && error.is_none() {
        if let Some(s) = c.pull(Duration::from_millis(10)) {
            drop(c);
            panic!("the {caps} consumer received a frame: {:?}", s.caps());
        }
        sample();
        error = c.error();
    }
    let errored_at = t0.elapsed();
    // Held in its failed state, alive.
    let mut links = None;
    while t0.elapsed() < errored_at + Duration::from_secs(1) {
        if let Some(s) = c.pull(Duration::from_millis(10)) {
            drop(c);
            panic!("the {caps} consumer received a frame: {:?}", s.caps());
        }
        sample();
        if links.is_none() && t0.elapsed() >= errored_at + Duration::from_millis(500) {
            links = Some(links_from(node));
        }
    }
    let pad_caps = c
        .sink
        .static_pad("sink")
        .and_then(|p| p.current_caps())
        .map(|c| c.to_string());
    let (state, _, _) = c.pipeline.state(gst::ClockTime::ZERO);
    let counted: Vec<Duration> = samples
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(t, _)| *t)
        .collect();
    eprintln!(
        "{}: error {error:?} after {errored_at:?}, sink caps {pad_caps:?}, pipeline {state:?}; \
         links from our node 500 ms after the error {links:?}; counted as a consumer \
         {} of {} samples, first {:?}, last {:?}; counts {:?}",
        c.what,
        counted.len(),
        samples.len(),
        counted.first(),
        counted.last(),
        rig.output().counts()
    );
    assert!(error.is_some(), "{caps}: no error within 3 s");
    assert!(pad_caps.is_none(), "{caps}: negotiated {pad_caps:?}");
    if let Some(&first) = counted.first() {
        let late: Vec<_> = samples
            .iter()
            .filter(|(t, n)| *t > first + NEGOTIATING && *n > 0)
            .collect();
        assert!(
            late.is_empty(),
            "{caps}: the refused consumer counted beyond {NEGOTIATING:?} from {first:?}: {late:?}"
        );
    }
    drop(c);
    assert!(
        wait_until(Duration::from_secs(2), || rig.output().consumers() == 0),
        "a link of the refused consumer stayed"
    );
    assert_eq!(
        rig.output().node_id(),
        Some(node),
        "{caps}: our node id changed; events {:?}",
        rig.events.lock().unwrap()
    );
    assert!(rig.failed().is_empty(), "{caps}: {:?}", rig.failed());
    assert!(
        matches!(rig.output().state(), PwState::Ready | PwState::Streaming),
        "{caps}: {:?}",
        rig.output().state()
    );
}

/// A native PipeWire consumer of our node (its own thread, main loop and
/// connection) asking only for `pod`. Unlike `pipewiresrc` it keeps its
/// stream connected whatever happens, until dropped: a refused
/// negotiation leaves its link alive, in error.
struct NativeConsumer {
    quit: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The consumer stream's states, in order.
    states: Arc<Mutex<Vec<String>>>,
}

impl NativeConsumer {
    fn start(target: &str, pod: Vec<u8>) -> NativeConsumer {
        let (quit, quit_rx) = pw::channel::channel::<()>();
        let states: Arc<Mutex<Vec<String>>> = Arc::default();
        let (st, target) = (Arc::clone(&states), target.to_string());
        let thread = std::thread::spawn(move || {
            pw::init();
            let ml = pw::main_loop::MainLoopRc::new(None).unwrap();
            let ctx = pw::context::ContextRc::new(&ml, None).unwrap();
            let core = ctx.connect_rc(None).unwrap();
            let props = pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Camera",
                "target.object" => target.as_str(),
                "node.dont-fallback" => "true",
                "node.dont-reconnect" => "true",
            };
            let stream = pw::stream::StreamRc::new(core, "obc-test-refused", props).unwrap();
            let _listener = stream
                .add_local_listener_with_user_data(())
                .state_changed(move |_, _, _, new| st.lock().unwrap().push(format!("{new:?}")))
                .register()
                .unwrap();
            let mut params = [pw::spa::pod::Pod::from_bytes(&pod).unwrap()];
            stream
                .connect(
                    spa::utils::Direction::Input,
                    None,
                    pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut params,
                )
                .unwrap();
            let _quit = quit_rx.attach(ml.loop_(), {
                let ml = ml.clone();
                move |()| ml.quit()
            });
            ml.run();
        });
        NativeConsumer {
            quit,
            thread: Some(thread),
            states,
        }
    }
}

impl Drop for NativeConsumer {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A serialised EnumFormat pod: `format` at `width`x`height` and the
/// contract's rate.
fn native_pod(
    contract: &OutputContract,
    format: spa::param::video::VideoFormat,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let sized = OutputContract {
        width,
        height,
        ..*contract
    };
    let mut obj = enum_format(&sized, PwFormat::Nv12);
    for p in &mut obj.properties {
        if p.key == spa::param::format::FormatProperties::VideoFormat.as_raw() {
            p.value = spa::pod::Value::Id(spa::utils::Id(format.as_raw()));
        }
    }
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .unwrap()
    .0
    .into_inner()
}

/// A native consumer asking for something not offered, held connected
/// for 1.5 s: its link (in error, see `pw-dump`) must not count as a
/// consumer beyond the negotiating window, and our node stays as it was.
fn refused_native_consumer(rig: &Rig, what: &str, pod: Vec<u8>, node: u32) {
    let c = NativeConsumer::start(&rig.name, pod);
    let t0 = Instant::now();
    let mut samples: Vec<(Duration, u32)> = Vec::new();
    let mut links = None;
    while t0.elapsed() < Duration::from_millis(1500) {
        samples.push((t0.elapsed(), rig.output().consumers()));
        if links.is_none() && t0.elapsed() >= Duration::from_millis(700) {
            links = Some(links_from(node));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let counted: Vec<Duration> = samples
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(t, _)| *t)
        .collect();
    eprintln!(
        "native {what}: stream states {:?}; links from our node at 700 ms {links:?}; \
         counted as a consumer {} of {} samples, first {:?}, last {:?}; counts {:?}",
        c.states.lock().unwrap(),
        counted.len(),
        samples.len(),
        counted.first(),
        counted.last(),
        rig.output().counts()
    );
    if let Some(&first) = counted.first() {
        let late: Vec<_> = samples
            .iter()
            .filter(|(t, n)| *t > first + NEGOTIATING && *n > 0)
            .collect();
        assert!(
            late.is_empty(),
            "native {what}: counted beyond {NEGOTIATING:?} from {first:?}: {late:?}"
        );
    }
    drop(c);
    assert!(
        wait_until(Duration::from_secs(2), || rig.output().consumers() == 0),
        "native {what}: a link stayed counted"
    );
    assert_eq!(rig.output().node_id(), Some(node), "native {what}");
    assert!(rig.failed().is_empty(), "native {what}: {:?}", rig.failed());
}

#[test]
#[ignore = "needs the user PipeWire daemon (with WirePlumber) and gst-plugin-pipewire (pipewiresrc)"]
fn the_node_serves_nv12_and_yuy2_and_survives_refused_consumers() {
    gst::init().unwrap();
    let contract = OutputContract::default();
    let name = format!("omarchy-broadcast-test-{}", std::process::id());
    let description = format!("Omarchy Broadcast test {}", std::process::id());
    let mut writer = LoopbackWriter::build(contract, WriterSink::Fake).unwrap();
    writer.start().unwrap();
    let events: Arc<Mutex<Vec<PwEvent>>> = Arc::default();
    let e = Arc::clone(&events);
    let output = PipeWireOutput::start(
        PwOutputConfig {
            contract,
            node_name: name.clone(),
            description: description.clone(),
            offer_nv12: true,
        },
        move |ev| e.lock().unwrap().push(ev),
    )
    .unwrap();
    writer.set_tap(Some(output.tap()));
    let rig = Rig {
        output: Some(output),
        writer,
        events,
        name,
    };

    assert!(
        wait_until(Duration::from_secs(5), || rig.output().state()
            == PwState::Ready
            && rig.output().node_id().is_some()),
        "not Ready: {:?}, events {:?}",
        rig.output().state(),
        rig.events.lock().unwrap()
    );
    let node = rig.output().node_id().unwrap();
    {
        let ev = rig.events.lock().unwrap();
        assert_eq!(
            ev[..2],
            [PwEvent::State(PwState::Connecting), PwEvent::Consumers(0)]
        );
    }
    let dump = pw_dump(node);
    for want in [
        "\"media.class\": \"Video/Source\"",
        "\"media.role\": \"Camera\"",
        &format!("\"node.name\": \"{}\"", rig.name),
        &format!("\"node.description\": \"{description}\""),
    ] {
        assert!(dump.contains(want), "pw-dump {node} lacks {want}:\n{dump}");
    }
    // Nobody watches: nothing crosses to the loop.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.output().counts().delivered, 0);
    assert!(rig.output().counts().idle_dropped > 0);

    // (a) NV12, the contract format: placeholders arrive.
    let c = linked_consumer(&rig, "video/x-raw,format=NV12");
    let s = c.pull(Duration::from_secs(5)).expect("no NV12 frame");
    let info = video_info(&s);
    assert_eq!(
        (info.format(), info.width(), info.height()),
        (gst_video::VideoFormat::Nv12, 1920, 1080)
    );
    let b = s.buffer().unwrap();
    assert_eq!(b.size(), 1920 * 1080 * 3 / 2);
    {
        let map = b.map_readable().unwrap();
        let (luma, chroma) = map.split_at(1920 * 1080);
        assert!(luma.iter().all(|&y| y == 16) && chroma.iter().all(|&c| c == 128));
    }
    assert_eq!(rig.output().state(), PwState::Streaming);
    // Frames keep coming at the writer's pace, in PTS order.
    let mut pts = vec![b.pts().unwrap()];
    while pts.len() < 15 {
        let s = c
            .pull(Duration::from_secs(2))
            .expect("the NV12 frames stopped");
        pts.push(s.buffer().unwrap().pts().unwrap());
    }
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "{pts:?}");
    let span = (pts[14] - pts[0]).nseconds() as f64 / 1e9;
    eprintln!(
        "NV12: {} at {:?}, 15 frames over {span:.3} s of PTS, counts {:?}",
        s.caps().unwrap(),
        b.pts(),
        rig.output().counts()
    );
    consumer_gone(&rig, c);

    // (b) YUY2, converted, with a known colour.
    let c = linked_consumer(&rig, "video/x-raw,format=YUY2");
    let pusher = Pusher::start(rig.writer.handle());
    let (y, u, v) = COLOUR;
    let want = [y, u, y, v];
    let mut seen = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Some(s) = c.pull(Duration::from_millis(500)) else {
            continue;
        };
        let info = video_info(&s);
        assert_eq!(
            (info.format(), info.width(), info.height()),
            (gst_video::VideoFormat::Yuy2, 1920, 1080)
        );
        let b = s.buffer().unwrap();
        assert_eq!(b.size(), 1920 * 1080 * 2);
        let map = b.map_readable().unwrap();
        if map[..4] == want {
            assert!(
                map.as_chunks::<4>().0.iter().all(|px| *px == want),
                "a YUY2 frame not flat {want:?}"
            );
            seen = Some(s.clone());
            break;
        }
    }
    drop(pusher);
    let s = seen.expect("no YUY2 frame of the pushed colour");
    eprintln!(
        "YUY2: {} pts {:?}, counts {:?}",
        s.caps().unwrap(),
        s.buffer().unwrap().pts(),
        rig.output().counts()
    );
    consumer_gone(&rig, c);

    // (c) Refused requests: another format, another size. Each fails only
    // its own link; a following NV12 consumer works.
    use spa::param::video::VideoFormat;
    for (caps, native, pod) in [
        (
            "video/x-raw,format=RGBA",
            "RGBA",
            native_pod(
                &contract,
                VideoFormat::RGBA,
                contract.width,
                contract.height,
            ),
        ),
        (
            "video/x-raw,format=NV12,width=640,height=480",
            "NV12 640x480",
            native_pod(&contract, VideoFormat::NV12, 640, 480),
        ),
    ] {
        refused_consumer(&rig, caps, node);
        refused_native_consumer(&rig, native, pod, node);
        let c = linked_consumer(&rig, "video/x-raw,format=NV12");
        let s = c
            .pull(Duration::from_secs(5))
            .expect("no NV12 frame after a refusal");
        assert_eq!(video_info(&s).width(), 1920);
        consumer_gone(&rig, c);
    }
    assert_eq!(rig.output().node_id(), Some(node));
    assert!(rig.failed().is_empty(), "{:?}", rig.failed());
    eprintln!(
        "events {:?}, counts {:?}",
        rig.events.lock().unwrap(),
        rig.output().counts()
    );

    // The node is gone once stopped.
    drop(rig);
    let dump = pw_dump(node);
    assert!(
        !dump.contains("omarchy-broadcast-test-"),
        "node left: {dump}"
    );
}
