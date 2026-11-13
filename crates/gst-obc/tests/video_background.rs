//! The replace effect's video background (`background-video`), end to end
//! through `appsrc ! obceffect ! appsink`: on system-memory frames with a
//! fake backend of host frames (`host_*`, no GPU), and on CUDA frames with
//! NVDEC (`gpu_*`, ignored: run them with the dev daemon stopped).
//!
//! The clips are made by `ffmpeg` in each test (a few hundred bytes): grey
//! frames whose luma climbs by 20 from 60, frame by frame, so the luma of a
//! background frame tells which frame of the clip it is. Without `ffmpeg`
//! a test says so and passes.
//!
//! The fake writes every output frame with one luma, the "mark" of the
//! background in force: [`STILL`] for the load-time image, [`BLACK_STILL`]
//! for the black still of a video without an image, [`BLURRED`] under blur,
//! else the luma of the last video frame it was given. So the output shows
//! which background was behind the person, frame by frame.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gst_obc::{EffectKind, EffectSettings, EffectStatus, ObcEffect, STATUS_MESSAGE};
use obc_backend::{
    BackendError, BackendFactory, BackgroundFrame, DeviceNv12, FrameKind, FrameParams, GpuContext,
    HostNv12, InferenceBackend, LoadReport, LoadRequest, StageTimings,
};

/// The mark of the load-time image.
const STILL: u8 = 1;
/// The mark of the black load-time still (a video without an image).
const BLACK_STILL: u8 = 2;
/// The mark of blur (no background at all).
const BLURRED: u8 = 4;

#[derive(Default)]
struct Log {
    loads: AtomicU64,
    /// The mark of each video frame given, in order.
    frames: Mutex<Vec<u8>>,
    /// `set_background_frame(None)` calls.
    nones: AtomicU64,
    /// Calls with a frame of the wrong size, memory or context.
    wrong: AtomicU64,
}

impl Log {
    fn frames(&self) -> Vec<u8> {
        self.frames.lock().unwrap().clone()
    }
}

struct Factory {
    live: bool,
    log: Arc<Log>,
}

impl BackendFactory for Factory {
    fn name(&self) -> &'static str {
        "live-fake"
    }

    fn kinds(&self) -> &[FrameKind] {
        &[FrameKind::Cuda, FrameKind::Host]
    }

    fn probe(&self) -> Result<(), BackendError> {
        Ok(())
    }

    fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
        Ok(Box::new(Fake {
            live: self.live,
            log: Arc::clone(&self.log),
            size: (0, 0),
            still: STILL,
            mark: STILL,
            context: 0,
        }))
    }
}

struct Fake {
    live: bool,
    log: Arc<Log>,
    size: (u32, u32),
    still: u8,
    mark: u8,
    /// The CUDA context of the load (0: host frames).
    context: usize,
}

impl Fake {
    fn load_req(&mut self, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        self.log.loads.fetch_add(1, Ordering::SeqCst);
        self.size = (req.width, req.height);
        let black = req
            .background
            .as_ref()
            .is_some_and(|b| b.data.iter().all(|v| *v == 0));
        self.still = if req.effect == EffectKind::Blur {
            BLURRED
        } else if black {
            BLACK_STILL
        } else {
            STILL
        };
        self.mark = self.still;
        Ok(LoadReport::default())
    }
}

impl InferenceBackend for Fake {
    fn name(&self) -> &'static str {
        "live-fake"
    }

    fn load(&mut self, gpu: &GpuContext, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        self.context = gpu.context as usize;
        self.load_req(req)
    }

    fn load_host(&mut self, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        self.context = 0;
        self.load_req(req)
    }

    fn process(
        &mut self,
        gpu: &GpuContext,
        _params: &FrameParams,
        _src: &DeviceNv12,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        let (w, h) = (dst.width as usize, dst.height as usize);
        // SAFETY: the caller's mapped output frame, on its stream.
        unsafe {
            obc_gpu::memset_2d_async(dst.y, dst.y_pitch, self.mark, w, h, gpu.stream)?;
            obc_gpu::memset_2d_async(dst.uv, dst.uv_pitch, 128, w, h / 2, gpu.stream)?;
        }
        Ok(())
    }

    fn process_host(
        &mut self,
        _params: &FrameParams,
        _src: &HostNv12,
        dst: &mut HostNv12,
    ) -> Result<(), BackendError> {
        let (w, h) = (dst.width as usize, dst.height as usize);
        let (yp, uvp) = (dst.y_pitch, dst.uv_pitch);
        // SAFETY: the caller's frame, as `HostNv12` documents it.
        let (y, uv) = unsafe { dst.planes_mut() }?;
        for r in 0..h {
            y[r * yp..r * yp + w].fill(self.mark);
        }
        for r in 0..h / 2 {
            uv[r * uvp..r * uvp + w].fill(128);
        }
        Ok(())
    }

    fn unload(&mut self) {}

    fn timings(&self) -> Option<StageTimings> {
        None
    }

    fn supports_live_background(&self) -> bool {
        self.live
    }

    fn set_background_frame(
        &mut self,
        frame: Option<BackgroundFrame<'_>>,
    ) -> Result<(), BackendError> {
        let Some(frame) = frame else {
            self.log.nones.fetch_add(1, Ordering::SeqCst);
            self.mark = self.still;
            return Ok(());
        };
        if frame.validate().is_err() || frame.size() != self.size {
            self.log.wrong.fetch_add(1, Ordering::SeqCst);
            return Err(BackendError::InvalidInput("wrong frame".into()));
        }
        let y = match frame {
            BackgroundFrame::Host(v) => {
                if self.context != 0 {
                    self.log.wrong.fetch_add(1, Ordering::SeqCst);
                }
                v.y[0]
            }
            BackgroundFrame::Cuda(d) => {
                let current = obc_gpu::current_context().unwrap_or(std::ptr::null_mut());
                if self.context == 0 || current as usize != self.context {
                    self.log.wrong.fetch_add(1, Ordering::SeqCst);
                }
                let mut px = [0u8; 1];
                // SAFETY: the frame is device memory of the current
                // context, valid for the call, at least 1x1.
                unsafe { obc_gpu::download_2d(d.y, d.y_pitch, &mut px, 1, 1, 1) }?;
                px[0]
            }
        };
        self.mark = y.max(3);
        self.log.frames.lock().unwrap().push(self.mark);
        Ok(())
    }
}

/// Makes `name` in `dir` with ffmpeg: `frames` grey frames of `w`x`h` at
/// 30 fps whose luma is 60, 80, 100, ...; `codec` is an ffmpeg encoder.
/// `None` (with a note) when ffmpeg or the encoder is missing.
fn clip(dir: &Path, name: &str, codec: &str, frames: u32, w: u32, h: u32) -> Option<PathBuf> {
    let path = dir.join(name);
    let mut args: Vec<String> = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!(
        "color=c=black:s={w}x{h}:r=30,format=yuv420p,geq=lum='60+N*20':cb=128:cr=128"
    ));
    args.extend(["-frames:v".into(), frames.to_string(), "-an".into()]);
    args.extend(["-c:v".into(), codec.into()]);
    if codec == "libvpx-vp9" {
        args.extend(["-lossless".into(), "1".into()]);
    }
    args.push(path.to_string_lossy().into_owned());
    match std::process::Command::new("ffmpeg").args(&args).status() {
        Ok(s) if s.success() => Some(path),
        Ok(s) => {
            eprintln!("note: ffmpeg could not make {name} ({s}); test skipped");
            None
        }
        Err(e) => {
            eprintln!("note: no ffmpeg ({e}); test skipped");
            None
        }
    }
}

/// A PNG still of `w`x`h`.
fn png(dir: &Path, w: u32, h: u32) -> PathBuf {
    gst::init().unwrap();
    let path = dir.join("still.png");
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

const W: u32 = 64;
const H: u32 = 36;
const SYSTEM: &str = "video/x-raw,format=NV12,width=64,height=36,pixel-aspect-ratio=1/1,\
                      colorimetry=2:4:5:1,framerate=30/1";

/// What an output frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    /// Processed by the fake, with the background of this mark.
    Mark(u8),
    /// Anything else (black, the input).
    Other,
}

/// `appsrc ! obceffect ! appsink` on system memory, one input frame pushed
/// again and again.
struct Rig {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    fx: ObcEffect,
    pts: u64,
}

impl Rig {
    fn new(factory: Arc<Factory>, settings: &EffectSettings, video: Option<&Path>) -> Rig {
        gst_obc::register().unwrap();
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=src format=time is-live=true caps=\"{SYSTEM}\" ! obceffect name=fx ! \
             appsink name=sink sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let fx = pipeline
            .by_name("fx")
            .unwrap()
            .downcast::<ObcEffect>()
            .unwrap();
        fx.set_backend_factory(Some(factory));
        fx.set_background_video(video).unwrap();
        fx.apply(settings).unwrap();
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
            fx,
            pts: 0,
        }
    }

    /// Pushes one camera frame (with `flags`) and classifies the output.
    fn frame_with(&mut self, flags: gst::BufferFlags) -> Out {
        let mut b = gst::Buffer::from_mut_slice(vec![50u8; (W * H * 3 / 2) as usize]);
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
            .expect("an output frame");
        let map = sample.buffer().unwrap().map_readable().unwrap();
        let y = map.as_slice()[0];
        let uniform = map.as_slice()[..(W * H) as usize].iter().all(|v| *v == y);
        // Black (16) and the input (50) are never a mark.
        if uniform && y != 16 && y != 50 {
            Out::Mark(y)
        } else {
            Out::Other
        }
    }

    fn frame(&mut self) -> Out {
        self.frame_with(gst::BufferFlags::empty())
    }

    /// Frames every 20 ms until `done` or `timeout` (then panics).
    fn frames_until(&mut self, timeout: Duration, done: impl Fn(&Rig) -> bool) -> Vec<Out> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out; status {:?}, stats {:?}, frames {seen:?}",
                self.fx.status(),
                self.fx.stats()
            );
            seen.push(self.frame());
            std::thread::sleep(Duration::from_millis(20));
        }
        seen
    }

    /// Frames every 20 ms for `d`.
    fn frames_for(&mut self, d: Duration) -> Vec<Out> {
        let end = Instant::now() + d;
        self.frames_until(d + Duration::from_secs(5), |_| Instant::now() >= end)
    }

    /// The `background-error` fields of the status messages so far.
    fn posted_background_errors(&self) -> Vec<String> {
        let bus = self.pipeline.bus().unwrap();
        let mut v = Vec::new();
        while let Some(m) = bus.pop_filtered(&[gst::MessageType::Element]) {
            if let Some(s) = m.structure()
                && s.name() == STATUS_MESSAGE
            {
                v.push(s.get::<String>("background-error").unwrap());
            }
        }
        v
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn factory(live: bool) -> Arc<Factory> {
    Arc::new(Factory {
        live,
        log: Arc::default(),
    })
}

fn replace(still: Option<PathBuf>) -> EffectSettings {
    EffectSettings {
        background: still,
        ..EffectSettings::of(EffectKind::Replace)
    }
}

fn ready(r: &Rig) -> bool {
    r.fx.status() == EffectStatus::Ready
}

/// The marks of `seen` (frames processed by the fake), in order.
fn marks(seen: &[Out]) -> Vec<u8> {
    seen.iter()
        .filter_map(|o| match o {
            Out::Mark(m) => Some(*m),
            Out::Other => None,
        })
        .collect()
}

#[test]
fn host_video_frames_reach_the_backend_and_loop_past_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "loop.webm", "libvpx-vp9", 6, 96, 54) else {
        return;
    };
    let f = factory(true);
    let mut rig = Rig::new(
        Arc::clone(&f),
        &replace(Some(png(dir.path(), 96, 54))),
        Some(&video),
    );
    rig.frames_until(Duration::from_secs(10), ready);
    // Six frames at 30 fps: 0.2 s a loop. Twenty frames given is three
    // loops at least.
    let log = Arc::clone(&f.log);
    let seen = rig.frames_until(Duration::from_secs(10), |_| log.frames().len() >= 20);
    let given = f.log.frames();
    let wraps = given.windows(2).filter(|w| w[1] < w[0]).count();
    assert!(wraps >= 2, "the clip loops: {given:?}");
    // Every frame is the clip's: a luma of 60 + 20n (a few levels off
    // after the conversion to the caps' colour).
    assert!(
        given.iter().all(|y| (50..=170).contains(y)),
        "clip frames: {given:?}"
    );
    assert!(
        seen.iter().all(|o| matches!(o, Out::Mark(_))),
        "never black nor the input: {seen:?}"
    );
    let s = rig.fx.stats();
    assert!(s.background_video, "the source runs");
    assert_eq!(s.background_error, None);
    assert_eq!(f.log.wrong.load(Ordering::SeqCst), 0);
    assert_eq!(f.log.loads.load(Ordering::SeqCst), 1, "one load, no reload");
}

#[test]
fn host_a_repeat_never_advances_the_video() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "loop.webm", "libvpx-vp9", 6, 96, 54) else {
        return;
    };
    let f = factory(true);
    let mut rig = Rig::new(
        Arc::clone(&f),
        &replace(Some(png(dir.path(), 96, 54))),
        Some(&video),
    );
    rig.frames_until(Duration::from_secs(10), ready);
    let log = Arc::clone(&f.log);
    rig.frames_until(Duration::from_secs(10), |_| !log.frames().is_empty());
    rig.frame();
    let before = f.log.frames().len();
    let mut repeats = Vec::new();
    for _ in 0..15 {
        std::thread::sleep(Duration::from_millis(20));
        repeats.push(rig.frame_with(gst_obc::REPEAT_FLAG));
    }
    assert_eq!(f.log.frames().len(), before, "no frame given on a repeat");
    assert!(repeats.windows(2).all(|w| w[0] == w[1]), "{repeats:?}");
    // The placeholder passes untouched, and gives nothing either.
    rig.frame_with(gst_obc::PLACEHOLDER_FLAG);
    assert_eq!(f.log.frames().len(), before);
    // The next fresh frame takes the newest.
    rig.frame();
    assert_eq!(f.log.frames().len(), before + 1);
}

#[test]
fn host_a_switch_from_a_still_keeps_it_until_the_first_video_frame() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "loop.webm", "libvpx-vp9", 6, 96, 54) else {
        return;
    };
    let f = factory(true);
    let still = png(dir.path(), 96, 54);
    let mut rig = Rig::new(Arc::clone(&f), &replace(Some(still)), None);
    rig.frames_until(Duration::from_secs(10), ready);
    assert_eq!(rig.frame(), Out::Mark(STILL));
    assert!(
        !rig.fx.stats().background_video,
        "no source without a video"
    );
    rig.fx.set_background_video(Some(&video)).unwrap();
    let log = Arc::clone(&f.log);
    let seen = rig.frames_until(Duration::from_secs(10), |_| log.frames().len() >= 3);
    let m = marks(&seen);
    assert_eq!(m.len(), seen.len(), "never black nor the input: {seen:?}");
    let n = m.iter().take_while(|v| **v == STILL).count();
    assert!(
        m[n..].iter().all(|v| *v >= 50),
        "the still, then the video only: {m:?}"
    );
    assert_eq!(f.log.loads.load(Ordering::SeqCst), 1, "no reload");
    // And back: the still again, with no reload either.
    rig.fx.set_background_video(None).unwrap();
    let seen = rig.frames_for(Duration::from_millis(200));
    assert_eq!(*seen.last().unwrap(), Out::Mark(STILL), "{seen:?}");
    assert!(seen.iter().all(|o| matches!(o, Out::Mark(_))), "{seen:?}");
    assert_eq!(f.log.nones.load(Ordering::SeqCst), 1);
    assert!(!rig.fx.stats().background_video, "the source stopped");
    assert_eq!(f.log.loads.load(Ordering::SeqCst), 1);
}

#[test]
fn host_a_switch_between_videos_keeps_the_last_frame_until_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(a), Some(b)) = (
        clip(dir.path(), "a.webm", "libvpx-vp9", 6, 96, 54),
        clip(dir.path(), "b.webm", "libvpx-vp9", 3, 64, 36),
    ) else {
        return;
    };
    let f = factory(true);
    let mut rig = Rig::new(Arc::clone(&f), &replace(None), Some(&a));
    let seen = rig.frames_until(Duration::from_secs(10), ready);
    let log = Arc::clone(&f.log);
    let seen2 = rig.frames_until(Duration::from_secs(10), |_| log.frames().len() >= 3);
    // A video without an image: black behind the person (the black still)
    // until its first frame, never black frames.
    let first: Vec<u8> = marks(&seen2);
    assert!(
        first.iter().take_while(|v| **v == BLACK_STILL).count() < first.len(),
        "{first:?}"
    );
    assert!(
        seen.iter()
            .all(|o| *o == Out::Other || *o == Out::Mark(BLACK_STILL))
    );
    let before = f.log.frames().len();
    rig.fx.set_background_video(Some(&b)).unwrap();
    let after = rig.frames_for(Duration::from_millis(600));
    let m = marks(&after);
    assert_eq!(m.len(), after.len(), "{after:?}");
    assert!(
        !m.contains(&BLACK_STILL),
        "the black still never comes back: {m:?}"
    );
    // B has three frames: 60, 80, 100.
    let new = &f.log.frames()[before..];
    assert!(
        new.iter().all(|y| *y <= 110),
        "only B's frames now: {new:?}"
    );
    assert!(!new.is_empty());
    assert_eq!(f.log.loads.load(Ordering::SeqCst), 1, "no reload");
}

#[test]
fn host_h264_without_the_gpu_says_why_and_keeps_the_still() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "clip.mp4", "libx264", 6, 96, 54) else {
        return;
    };
    let f = factory(true);
    let mut rig = Rig::new(
        Arc::clone(&f),
        &replace(Some(png(dir.path(), 96, 54))),
        Some(&video),
    );
    rig.frames_until(Duration::from_secs(10), ready);
    let seen = rig.frames_until(Duration::from_secs(10), |r| {
        r.fx.stats().background_error.is_some()
    });
    let why = rig.fx.stats().background_error.unwrap();
    assert!(why.contains("only VP9"), "{why}");
    assert!(why.contains(video.to_str().unwrap()), "{why}");
    assert!(
        seen.iter()
            .chain(&rig.frames_for(Duration::from_millis(100)))
            .all(|o| *o == Out::Mark(STILL)),
        "{seen:?}"
    );
    assert!(f.log.frames().is_empty());
    assert!(
        rig.posted_background_errors().iter().any(|e| e == &why),
        "posted in the status message"
    );
    assert_eq!(
        rig.fx.status(),
        EffectStatus::Ready,
        "not an effect failure"
    );
}

#[test]
fn host_a_file_that_is_no_video_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let still = png(dir.path(), 96, 54);
    let fake_video = dir.path().join("not-a-video.webm");
    std::fs::copy(&still, &fake_video).unwrap();
    let f = factory(true);
    let mut rig = Rig::new(Arc::clone(&f), &replace(Some(still)), Some(&fake_video));
    rig.frames_until(Duration::from_secs(10), ready);
    rig.frames_until(Duration::from_secs(5), |r| {
        r.fx.stats().background_error.is_some()
    });
    let why = rig.fx.stats().background_error.unwrap();
    assert!(why.contains("not an MP4"), "{why}");
    assert!(!rig.fx.stats().background_video);
    assert_eq!(rig.frame(), Out::Mark(STILL));
}

#[test]
fn host_no_decode_when_the_backend_takes_no_live_background() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "loop.webm", "libvpx-vp9", 6, 96, 54) else {
        return;
    };
    let f = factory(false);
    let mut rig = Rig::new(
        Arc::clone(&f),
        &replace(Some(png(dir.path(), 96, 54))),
        Some(&video),
    );
    rig.frames_until(Duration::from_secs(10), ready);
    rig.frames_until(Duration::from_secs(5), |r| {
        r.fx.stats().background_error.is_some()
    });
    let s = rig.fx.stats();
    assert_eq!(
        s.background_error.as_deref(),
        Some("live-fake shows the video background as a still")
    );
    assert!(!s.background_video, "nothing decodes");
    let seen = rig.frames_for(Duration::from_millis(300));
    assert!(seen.iter().all(|o| *o == Out::Mark(STILL)), "{seen:?}");
    assert!(f.log.frames().is_empty());
}

#[test]
fn host_the_source_lives_only_while_replace_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "loop.webm", "libvpx-vp9", 6, 96, 54) else {
        return;
    };
    let f = factory(true);
    // Set before anything is loaded: nothing runs at idle.
    let mut rig = Rig::new(
        Arc::clone(&f),
        &EffectSettings::of(EffectKind::None),
        Some(&video),
    );
    rig.frames_for(Duration::from_millis(200));
    assert!(!rig.fx.stats().background_video, "not at idle");
    rig.fx
        .apply(&replace(Some(png(dir.path(), 96, 54))))
        .unwrap();
    rig.frames_until(Duration::from_secs(10), |r| {
        r.fx.stats().background_video && ready(r)
    });
    // Blur: the source stops once blur is loaded (it plays on until
    // then: the chain in force shows it).
    rig.fx.apply(&EffectSettings::of(EffectKind::Blur)).unwrap();
    rig.frames_until(Duration::from_secs(10), |r| {
        !r.fx.stats().background_video && ready(r)
    });
    rig.fx
        .apply(&replace(Some(png(dir.path(), 96, 54))))
        .unwrap();
    rig.frames_until(Duration::from_secs(10), |r| r.fx.stats().background_video);
    // A stop.
    rig.pipeline.set_state(gst::State::Null).unwrap();
    assert!(!rig.fx.stats().background_video, "stopped with the element");
}

/// Plays `video` behind replace without an image until the fake has had
/// three of its frames.
fn playing(f: &Arc<Factory>, video: &Path) -> Rig {
    let mut rig = Rig::new(Arc::clone(f), &replace(None), Some(video));
    rig.frames_until(Duration::from_secs(10), ready);
    let log = Arc::clone(&f.log);
    rig.frames_until(Duration::from_secs(10), |_| log.frames().len() >= 3);
    rig
}

#[test]
fn host_a_repeat_right_after_a_reload_carries_the_video_frame() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "loop.webm", "libvpx-vp9", 6, 96, 54) else {
        return;
    };
    let f = factory(true);
    let mut rig = playing(&f, &video);
    assert!(matches!(rig.frame(), Out::Mark(m) if m >= 50));
    // A new still with the video still on: a reload, which puts the
    // load-time still back in force in the backend.
    rig.fx
        .apply(&replace(Some(png(dir.path(), 96, 54))))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.log.loads.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "no reload");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The tick right after it is a repeat: the video's frame, not the
    // still, behind the person.
    let repeats: Vec<Out> = (0..3)
        .map(|_| rig.frame_with(gst_obc::REPEAT_FLAG))
        .collect();
    assert!(
        repeats
            .iter()
            .all(|o| matches!(o, Out::Mark(m) if *m >= 50)),
        "{repeats:?}"
    );
    let seen = rig.frames_for(Duration::from_millis(200));
    assert!(
        seen.iter().all(|o| matches!(o, Out::Mark(m) if *m >= 50)),
        "{seen:?}"
    );
}

#[test]
fn host_video_to_a_still_with_a_reload_never_shows_the_black_still() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "loop.webm", "libvpx-vp9", 6, 96, 54) else {
        return;
    };
    // Large, so that its decode keeps the load away for a while.
    let still = png(dir.path(), 3840, 2160);
    let f = factory(true);
    let mut rig = playing(&f, &video);
    // What `Output::set_effect_video` does: the settings with the still,
    // then no video.
    rig.fx.apply(&replace(Some(still))).unwrap();
    rig.fx.set_background_video(None).unwrap();
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.log.loads.load(Ordering::SeqCst) < 2 || rig.fx.stats().background_video {
        assert!(Instant::now() < deadline, "{seen:?}");
        seen.push(rig.frame());
    }
    seen.extend(rig.frames_for(Duration::from_millis(100)));
    let m = marks(&seen);
    assert_eq!(m.len(), seen.len(), "never black nor the input: {seen:?}");
    assert!(
        !m.contains(&BLACK_STILL),
        "never the still of the chain in force: {m:?}"
    );
    let n = m.iter().take_while(|v| **v >= 50).count();
    assert!(
        m[n..].iter().all(|v| *v == STILL) && n < m.len(),
        "the video until the new still is loaded, then the still: {m:?}"
    );
}

#[test]
fn host_video_to_blur_never_shows_the_black_still() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "loop.webm", "libvpx-vp9", 6, 96, 54) else {
        return;
    };
    let f = factory(true);
    let mut rig = playing(&f, &video);
    rig.fx.apply(&EffectSettings::of(EffectKind::Blur)).unwrap();
    rig.fx.set_background_video(None).unwrap();
    let mut seen = vec![rig.frame()];
    seen.extend(rig.frames_until(Duration::from_secs(10), |r| {
        ready(r) && !r.fx.stats().background_video
    }));
    seen.extend(rig.frames_for(Duration::from_millis(100)));
    let m = marks(&seen);
    assert_eq!(m.len(), seen.len(), "never black nor the input: {seen:?}");
    let n = m.iter().take_while(|v| **v >= 50).count();
    assert!(
        m[n..].iter().all(|v| *v == BLURRED) && n < m.len(),
        "the video until blur is loaded, then blur: {m:?}"
    );
    assert_eq!(f.log.nones.load(Ordering::SeqCst), 0, "{m:?}");
}

#[test]
fn host_a_clip_with_no_frames_fails_instead_of_looping() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "empty.webm", "libvpx-vp9", 0, 96, 54) else {
        return;
    };
    let f = factory(true);
    let mut rig = Rig::new(
        Arc::clone(&f),
        &replace(Some(png(dir.path(), 96, 54))),
        Some(&video),
    );
    rig.frames_until(Duration::from_secs(10), ready);
    rig.frames_until(Duration::from_secs(10), |r| {
        r.fx.stats().background_error.is_some()
    });
    let why = rig.fx.stats().background_error.unwrap();
    assert!(why.contains("it has no frames to play"), "{why}");
    assert_eq!(rig.frame(), Out::Mark(STILL));
}

#[test]
fn host_a_video_larger_than_4k_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), "big.webm", "libvpx-vp9", 1, 4096, 2160) else {
        return;
    };
    let f = factory(true);
    let mut rig = Rig::new(
        Arc::clone(&f),
        &replace(Some(png(dir.path(), 96, 54))),
        Some(&video),
    );
    rig.frames_until(Duration::from_secs(10), ready);
    rig.frames_until(Duration::from_secs(20), |r| {
        r.fx.stats().background_error.is_some()
    });
    let why = rig.fx.stats().background_error.unwrap();
    assert!(why.contains("4096x2160, larger than 3840x2160"), "{why}");
    assert!(f.log.frames().is_empty());
}

#[test]
fn background_video_is_a_property_and_needs_an_absolute_path() {
    gst_obc::register().unwrap();
    let e = gst::ElementFactory::make(gst_obc::ELEMENT_NAME)
        .build()
        .unwrap();
    let fx = gst_obc::from_element(&e).unwrap();
    assert_eq!(e.property::<String>("background-video"), "");
    e.set_property("background-video", "clip.webm");
    assert_eq!(e.property::<String>("background-video"), "", "relative");
    assert!(
        fx.set_background_video(Some(Path::new("clip.webm")))
            .is_err()
    );
    e.set_property("background-video", "/srv/clip.webm");
    assert_eq!(
        fx.background_video().as_deref(),
        Some(Path::new("/srv/clip.webm"))
    );
    // Replace with a video needs no image (black is its still); without
    // one it does.
    assert!(fx.apply(&EffectSettings::of(EffectKind::Replace)).is_ok());
    fx.set_background_video(None).unwrap();
    let e = fx
        .apply(&EffectSettings::of(EffectKind::Replace))
        .unwrap_err();
    assert!(e.contains("needs a background image"), "{e}");
}

// On CUDA frames, with NVDEC: stop any running daemon so the GPU is free.

/// `videotestsrc ! cudaupload ! obceffect ! fakesink`, live at 30 fps.
fn gpu_rig(f: Arc<Factory>, video: &Path, w: u32, h: u32) -> (gst::Pipeline, ObcEffect) {
    gst_obc::register().unwrap();
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc is-live=true pattern=black ! video/x-raw,format=NV12,width={w},\
         height={h},framerate=30/1,colorimetry=bt709,pixel-aspect-ratio=1/1 ! cudaupload ! \
         video/x-raw(memory:CUDAMemory) ! obceffect name=fx ! fakesink sync=false"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let fx = pipeline
        .by_name("fx")
        .unwrap()
        .downcast::<ObcEffect>()
        .unwrap();
    fx.set_backend_factory(Some(f));
    fx.set_background_video(Some(video)).unwrap();
    fx.apply(&replace(None)).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    (pipeline, fx)
}

fn gpu_plays(codec: &str, name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let Some(video) = clip(dir.path(), name, codec, 6, 320, 180) else {
        return;
    };
    let f = factory(true);
    let (pipeline, fx) = gpu_rig(Arc::clone(&f), &video, 256, 144);
    let deadline = Instant::now() + Duration::from_secs(20);
    while f.log.frames().len() < 20 {
        assert!(
            Instant::now() < deadline,
            "no frames; status {:?}, stats {:?}",
            fx.status(),
            fx.stats()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let given = f.log.frames();
    assert!(given.windows(2).any(|w| w[1] < w[0]), "loops: {given:?}");
    assert!(given.iter().all(|y| (50..=170).contains(y)), "{given:?}");
    let s = fx.stats();
    assert!(s.cuda_active && s.background_video);
    assert_eq!(s.background_error, None);
    assert_eq!(
        f.log.wrong.load(Ordering::SeqCst),
        0,
        "the effect's context"
    );
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(!fx.stats().background_video);
}

#[test]
#[ignore = "needs an NVIDIA GPU with NVDEC; stop the dev daemon first"]
fn gpu_nvdec_h264_plays_into_the_effects_cuda_context() {
    gpu_plays("libx264", "clip.mp4");
}

#[test]
#[ignore = "needs an NVIDIA GPU with NVDEC; stop the dev daemon first"]
fn gpu_nvdec_vp9_plays_into_the_effects_cuda_context() {
    gpu_plays("libvpx-vp9", "clip.webm");
}
