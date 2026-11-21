//! A running stream: one output and the ingest currently feeding it, with
//! source switching.
//!
//! A switch stops the current ingest (camera closed, its controls restored)
//! and only then starts the next one, so the same camera can be reopened in
//! another mode. The output pipeline is never touched. Before the current
//! ingest stops, the output is told to hold the last frame until the next
//! source's first frame ([`Output::hold_for`], at most
//! [`crate::output::OutputConfig::hold_timeout`]), so the gap shows the last
//! frame instead of black placeholders. If the next source does not start,
//! the hold runs to its timeout and placeholders follow. Stopping the ingest
//! without a switch ([`Session::stop_ingest`]) or an ingest that fails arms
//! no hold: the output repeats the last frame and sends placeholders after
//! the gap threshold.
//!
//! **Memory.** A session runs in the memory its output was built for
//! ([`Output::build_in`]: CUDA by default, or system memory): the ingests it
//! builds ([`Session::build_ingest`]) write canonical frames in that
//! memory, and in system memory no pipeline of the session holds a CUDA
//! context.
//!
//! **Output handover.** A change of the output itself (another effect
//! element or module chain) need not stop the camera. The next output is
//! built and prepared while the current one streams
//! ([`Session::prepare_output`]): it is started, given the current
//! output's CUDA context ([`crate::output::OutputConfig::cuda_context`];
//! none in system memory), and the current ingest writes every frame into
//! its slot too, the same buffer by reference ([`crate::frame`],
//! "Fan-out"). An output forwarding to a loopback writer is prepared on a
//! standby feed ([`crate::loopback::WriterHandle::standby`]): it forwards,
//! and the writer refuses its frames, so the consumer still sees the
//! current output's only. Once the prepared output has processed a frame
//! of the ingest ([`Session::prepared_first_fresh`]), the commit
//! ([`Session::commit_prepared`]) promotes its feed (one push lock, so the
//! consumer's stream turns from one output to the other between two
//! frames, nothing held, no placeholder), points the ingest at its slot
//! only, makes it the session's output and only then stops the previous
//! one. [`Session::discard_prepared`] stops the prepared output instead;
//! the current one never noticed. The ingest is never stopped or rebuilt.
//!
//! **Source ids across a handover.** Source ids come from each output's
//! own counter ([`Output::frame_writer`]). The current ingest keeps its id
//! in both outputs: its frames carry it into the prepared output's slot,
//! so [`Output::first_fresh`] of the prepared output, and of the session's
//! output after the commit, takes the current ingest's
//! [`Ingest::source_id`]. A prepared output's counter continues after the
//! current output's, so a source built after the commit
//! ([`Session::build_ingest`]) never shares an id with one built before.
//!
//! **A prepared output and the ingest.** A source switch
//! ([`Session::switch_to`]) and [`Session::stop_ingest`] discard the
//! prepared output first: the next ingest writes into the session's output
//! only (as [`Session::build_ingest`] builds it), so the prepared one would
//! be left without frames. Dropping or stopping the session stops the
//! ingest, then the prepared output, then the output.

use std::time::{Duration, Instant};

use crate::ingest::{Ingest, IngestConfig, cuda_context_object};
use crate::output::Output;
use crate::pipeline::{PipelineError, StopError, both_stopped};
use obc_core::config::VideoMemory;

/// When a source switch happened, or how it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchTiming {
    /// The new source's id.
    pub source_id: u64,
    /// When the switch was asked for.
    pub requested: Instant,
    /// When the previous ingest was stopped (`None`: there was none, or
    /// stopping it failed).
    pub previous_stopped: Option<Instant>,
    /// When the new ingest reached PLAYING (`None`: the switch failed).
    pub started: Option<Instant>,
    /// When the output emitted the new source's first frame (`None`: not
    /// yet, as far as [`Session::last_switch`] could tell).
    pub first_fresh: Option<Instant>,
    /// Why the switch failed: stopping the previous ingest or starting the
    /// new one (the error [`Session::switch_to`] returned).
    pub error: Option<String>,
    /// Why a pipeline the failed switch took down did not reach NULL (or
    /// its state change to NULL failed): the previous ingest's, whose stop
    /// failed, or the new one's, stopped after it failed to start. Some of
    /// its elements may still run. `None` when every pipeline it took down
    /// reached NULL ([`StopError::reached_null`]).
    pub unclean_stop: Option<String>,
}

impl SwitchTiming {
    /// From the request to the new source's first emitted frame.
    pub fn gap(&self) -> Option<Duration> {
        self.first_fresh
            .map(|t| t.saturating_duration_since(self.requested))
    }
}

/// What [`Session::commit_prepared`] did.
#[derive(Debug)]
pub struct Handover {
    /// The previous output, stopped (its counts stay readable).
    pub previous: Output,
    /// How its stop went. An error does not undo the handover: the prepared
    /// output is the session's either way.
    pub previous_stop: Result<(), StopError>,
    /// When the prepared output's writer feed was promoted: from then on
    /// the consumer sees its frames only. When the output forwards to no
    /// writer, when the ingest was pointed at its slot only.
    pub promoted_at: Instant,
}

/// Why [`Session::prepare_output`] prepared nothing: the refusal or the
/// failed start, and how the output it was given stopped after it (its
/// [`Output::stop_checked`]: a start that failed may leave a pipeline
/// outside NULL, which the caller must not take for a clean stop).
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct PrepareError {
    pub error: PipelineError,
    /// How the output stopped: an output refused before its start was
    /// never started, and stopped in NULL.
    pub cleanup: Result<(), StopError>,
}

/// The output plus its current ingest, and the next output while one is
/// prepared (see the module docs, "Output handover"). Dropping it stops the
/// ingest first, then the prepared output, then the output.
#[derive(Debug)]
pub struct Session {
    // Field order is drop order: ingest, prepared output, output.
    ingest: Option<Ingest>,
    prepared: Option<Output>,
    output: Output,
    last_switch: Option<SwitchTiming>,
}

impl Session {
    /// Takes over `output`, starting it if it is not running.
    pub fn new(mut output: Output) -> Result<Self, PipelineError> {
        output.start()?;
        Ok(Session {
            ingest: None,
            prepared: None,
            output,
            last_switch: None,
        })
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    /// The output prepared to take over ([`Self::prepare_output`]), until
    /// it is committed or discarded.
    pub fn prepared(&self) -> Option<&Output> {
        self.prepared.as_ref()
    }

    /// Prepares `next` to take over from the session's output, while that
    /// one keeps streaming (see the module docs, "Output handover"): `next`
    /// is given the output's CUDA context (CUDA memory: set as
    /// [`OutputConfig::cuda_context`](crate::output::OutputConfig::cuda_context)
    /// before it starts; already running, it must have been started with
    /// that context), started if it is not running, and from then on the
    /// current ingest writes each frame into its slot too. Its source ids
    /// continue after the output's (module docs, "Source ids across a
    /// handover"). An output already prepared is discarded first.
    ///
    /// `next` must take the output's canonical frames (same contract size
    /// and colour, same [`Output::memory`]), and forward to the same
    /// loopback writer as the output when that one does, on a standby feed
    /// ([`crate::loopback::WriterHandle::standby`]), or to no writer when
    /// it does not: [`PipelineError::Caps`] or [`PipelineError::State`]
    /// otherwise. On a refusal or a start that fails, `next` is stopped
    /// ([`Output::stop_checked`], its result in [`PrepareError::cleanup`])
    /// and dropped, and nothing is prepared; the output never noticed.
    pub fn prepare_output(&mut self, mut next: Output) -> Result<(), Box<PrepareError>> {
        if let Err(e) = self.discard_prepared() {
            eprintln!("the output prepared before did not stop cleanly: {e}");
        }
        if let Err(error) = self.start_prepared(&mut next) {
            let cleanup = next.stop_checked();
            return Err(Box::new(PrepareError { error, cleanup }));
        }
        if let Some(ingest) = &self.ingest {
            ingest.frame_writer().fan_out(Some(next.slot_target()));
        }
        self.prepared = Some(next);
        Ok(())
    }

    /// Checks `next` against the session's output and starts it with its
    /// CUDA context ([`Self::prepare_output`]).
    fn start_prepared(&self, next: &mut Output) -> Result<(), PipelineError> {
        if next.memory() != self.output.memory() || next.canonical() != self.output.canonical() {
            return Err(PipelineError::Caps(format!(
                "a prepared output must take the session's canonical frames ({:?} in {:?} \
                 memory), not {:?} in {:?} memory",
                self.output.canonical(),
                self.output.memory(),
                next.canonical(),
                next.memory()
            )));
        }
        match (self.output.writer(), next.writer()) {
            (None, None) => {}
            (Some(current), Some(w)) if current == w && !w.is_current_feed() => {}
            (Some(current), Some(w)) if current == w => {
                return Err(PipelineError::State(
                    "a prepared output must forward on a standby feed of the writer \
                     (WriterHandle::standby), not on its current one"
                        .into(),
                ));
            }
            _ => {
                return Err(PipelineError::State(
                    "a prepared output must forward to the session output's loopback writer, \
                     or to none when the session's output does not"
                        .into(),
                ));
            }
        }
        if self.output.memory() == VideoMemory::Cuda {
            let cuda = self.output.cuda_context().cloned().ok_or_else(|| {
                PipelineError::Cuda(
                    "the output is not running: it has no CUDA context to share".into(),
                )
            })?;
            if next.is_running() {
                let object = |c: &gst::Context| cuda_context_object(c);
                if next.cuda_context().and_then(object).is_none()
                    || next.cuda_context().and_then(object) != object(&cuda)
                {
                    return Err(PipelineError::Cuda(
                        "a running prepared output must use the session output's CUDA context \
                         (OutputConfig::cuda_context)"
                            .into(),
                    ));
                }
            } else {
                next.share_cuda_context(Some(cuda));
            }
        }
        next.continue_sources_from(&self.output);
        next.start()
    }

    /// When the prepared output emitted its first fresh frame of the
    /// current ingest ([`Output::first_fresh`] of the ingest's
    /// [`Ingest::source_id`]): it has a camera frame through its whole
    /// chain, and a commit hands over without a gap. `None` without a
    /// prepared output or an ingest, and until then. A frame through its
    /// chain is not one its effect has processed: an effect still loading
    /// shows its fallback ([`Output::effect_status`],
    /// [`Output::stage_status`] and [`Output::module_stage_status`] say when
    /// what it runs is ready; under the `none` effect with a stage on, the
    /// effect's own status is [`crate::effect::EffectStatus::Off`]).
    pub fn prepared_first_fresh(&self) -> Option<Instant> {
        let id = self.ingest.as_ref()?.source_id();
        self.prepared.as_ref()?.first_fresh(id)
    }

    /// Hands the session over to the prepared output (see the module docs,
    /// "Output handover"): promotes its writer feed, so the consumer sees
    /// its frames from the next push on and the previous output's no more
    /// ([`crate::loopback::WriterHandle::promote`]), points the current
    /// ingest at its slot only, makes it the session's output, and only
    /// then stops the previous output, returned in the [`Handover`] with
    /// how its stop went. The ingest is untouched. Fails with
    /// [`PipelineError::State`], changing nothing, when no output is
    /// prepared.
    pub fn commit_prepared(&mut self) -> Result<Handover, PipelineError> {
        let next = self
            .prepared
            .take()
            .ok_or_else(|| PipelineError::State("no output is prepared".into()))?;
        // The consumer's stream turns here, between two pushes.
        if let Some(w) = next.writer() {
            w.promote();
        }
        let promoted_at = Instant::now();
        if let Some(ingest) = &self.ingest {
            ingest.frame_writer().retarget(next.slot_target());
        }
        let mut previous = std::mem::replace(&mut self.output, next);
        // After the promotion: its last frames are refused, and its detach
        // is not the writer's last (the new output is attached).
        let previous_stop = previous.stop_checked();
        Ok(Handover {
            previous,
            previous_stop,
            promoted_at,
        })
    }

    /// Stops the prepared output, if any, after the ingest has stopped
    /// writing into its slot; the session's output never noticed (its
    /// writer feed still the current one). Its stop's error, if it failed;
    /// nothing is prepared afterwards either way.
    pub fn discard_prepared(&mut self) -> Result<(), StopError> {
        let Some(mut next) = self.prepared.take() else {
            return Ok(());
        };
        if let Some(ingest) = &self.ingest {
            ingest.frame_writer().fan_out(None);
        }
        next.stop_checked()
    }

    /// Discards the prepared output before the ingest changes (see the
    /// module docs, "A prepared output and the ingest"); a stop that fails
    /// is logged.
    fn discard_for_ingest_change(&mut self) {
        if let Err(e) = self.discard_prepared() {
            eprintln!("the prepared output did not stop cleanly: {e}");
        }
    }

    pub fn ingest(&self) -> Option<&Ingest> {
        self.ingest.as_ref()
    }

    /// Why the current ingest has failed, if it has
    /// ([`Ingest::poll_failure`]); `None` without an ingest. Non-blocking:
    /// the owner polls it and switches to another source.
    pub fn ingest_failure(&self) -> Option<String> {
        self.ingest.as_ref().and_then(Ingest::poll_failure)
    }

    /// The corrupt camera frames the current ingest dropped at its source
    /// ([`Ingest::corrupt_frames`]); `None` without an ingest.
    pub fn corrupt_frames(&self) -> Option<crate::frame_guard::CorruptFrames> {
        self.ingest.as_ref().map(Ingest::corrupt_frames)
    }

    /// The rate the current ingest's camera delivers good frames at
    /// ([`Ingest::delivered_fps`]); `None` without an ingest, and while the
    /// ingest has no rate yet.
    pub fn delivered_fps(&self) -> Option<f64> {
        self.ingest.as_ref().and_then(Ingest::delivered_fps)
    }

    /// Whether the current ingest's camera has its privacy shutter open or
    /// closed ([`Ingest::shutter`]); `None` without an ingest, and while
    /// its frames say nothing of it.
    pub fn shutter(&self) -> Option<crate::shutter::ShutterState> {
        self.ingest.as_ref().and_then(Ingest::shutter)
    }

    /// Builds (but does not start) an ingest for `cfg` writing into this
    /// session's output, so the caller can inspect or probe it before
    /// [`Self::switch_to`].
    pub fn build_ingest(&self, cfg: &IngestConfig) -> Result<Ingest, PipelineError> {
        Ingest::build(cfg, self.output.frame_writer())
    }

    /// Makes `next` the current source: arms the output's hold for `next`
    /// (when a source is streaming), stops the current ingest, then starts
    /// `next` with the output's CUDA context (none for an output in system
    /// memory, [`Output::memory`]). If stopping the current
    /// ingest fails, `next` is not started (it is dropped), unless the old
    /// pipeline reached NULL and only a pinned control could not be put
    /// back: that is logged and the switch goes on; if starting
    /// `next` fails, the session has no source. Either way the hold armed
    /// for `next` runs to its timeout
    /// ([`OutputConfig::hold_timeout`](crate::output::OutputConfig::hold_timeout),
    /// 3 s by default): the output repeats the last frame that long, then
    /// sends placeholders. An output prepared is discarded first (see the
    /// module docs, "A prepared output and the ingest").
    pub fn switch_to(&mut self, next: Ingest) -> Result<SwitchTiming, PipelineError> {
        self.switch_at(next, Instant::now())
    }

    /// Builds an ingest for `cfg` and switches to it; the switch is timed
    /// from before the build.
    pub fn switch_to_camera(&mut self, cfg: &IngestConfig) -> Result<SwitchTiming, PipelineError> {
        let requested = Instant::now();
        let next = self.build_ingest(cfg)?;
        self.switch_at(next, requested)
    }

    /// Every attempt is recorded in [`Self::last_switch`], a failed one
    /// with its error.
    fn switch_at(
        &mut self,
        next: Ingest,
        requested: Instant,
    ) -> Result<SwitchTiming, PipelineError> {
        self.discard_for_ingest_change();
        let mut timing = SwitchTiming {
            source_id: next.source_id(),
            requested,
            previous_stopped: None,
            started: None,
            first_fresh: None,
            error: None,
            unclean_stop: None,
        };
        let res = self.replace_ingest(next, &mut timing);
        if let Err(e) = &res {
            timing.error = Some(e.to_string());
        }
        self.last_switch = Some(timing.clone());
        res.map(|()| timing)
    }

    fn replace_ingest(
        &mut self,
        mut next: Ingest,
        timing: &mut SwitchTiming,
    ) -> Result<(), PipelineError> {
        if let Some(old) = self.ingest.take() {
            // Before the old camera stops: no tick may see the gap first.
            self.output.hold_for(next.source_id());
            // A failed stop drops `next`: the hold runs to its timeout.
            if let Err(e) = old.stop_checked() {
                if !e.reached_null {
                    timing.unclean_stop = Some(format!("stopping the previous ingest: {e}"));
                    return Err(e.error);
                }
                // The camera is closed and the pipeline is in NULL; only a
                // control could not be put back (an unplugged camera, say).
                // That is no reason to keep the switch from a camera that
                // still works, and nothing is left running: not unclean.
                eprintln!("the previous ingest stopped, but not cleanly: {e}");
            }
            timing.previous_stopped = Some(Instant::now());
        }
        // System-memory frames need no CUDA context.
        let cuda = match self.output.memory() {
            VideoMemory::Cuda => Some(self.output.cuda_context().ok_or_else(|| {
                PipelineError::Cuda(
                    "the output is not running: it has no CUDA context to share".into(),
                )
            })?),
            VideoMemory::System => None,
        };
        if let Err(e) = next.start_with(cuda) {
            // Back to NULL and dropped here, checked, rather than by its
            // drop, which does not say whether it reached NULL.
            if let Err(stop) = next.stop_checked() {
                if stop.reached_null {
                    // Nothing is left running: logged, not unclean.
                    eprintln!(
                        "the new ingest stopped, but not cleanly, after it failed to start: {stop}"
                    );
                } else {
                    timing.unclean_stop = Some(format!(
                        "stopping the new ingest after it failed to start: {stop}"
                    ));
                }
            }
            return Err(e);
        }
        timing.started = Some(Instant::now());
        self.ingest = Some(next);
        Ok(())
    }

    /// The last switch attempt (failed ones included, with their error),
    /// with the time of the new source's first emitted frame once the
    /// output has emitted it.
    pub fn last_switch(&mut self) -> Option<SwitchTiming> {
        let t = self.last_switch.as_mut()?;
        if t.first_fresh.is_none() && t.started.is_some() {
            t.first_fresh = self.output.first_fresh(t.source_id);
        }
        Some(t.clone())
    }

    /// Stops the current ingest, if any; the output keeps running. An
    /// output prepared is discarded first (see the module docs, "A
    /// prepared output and the ingest").
    pub fn stop_ingest(&mut self) -> Result<(), PipelineError> {
        self.discard_for_ingest_change();
        match self.ingest.take() {
            Some(i) => i.stop(),
            None => Ok(()),
        }
    }

    /// Stops the ingest, then the prepared output (if any), then the
    /// output, reporting the first failure (one that left a pipeline
    /// outside NULL first).
    pub fn stop(self) -> Result<(), PipelineError> {
        self.stop_checked().map_err(Into::into)
    }

    /// [`Self::stop`], its error saying whether both pipelines reached
    /// NULL. Of this stop only: an earlier switch's is in its
    /// [`SwitchTiming::unclean_stop`].
    pub fn stop_checked(mut self) -> Result<(), StopError> {
        let ingest = match self.ingest.take() {
            Some(i) => i.stop_checked(),
            None => Ok(()),
        };
        let prepared = match self.prepared.take() {
            Some(mut p) => p.stop_checked(),
            None => Ok(()),
        };
        let output = self.output.stop_checked();
        both_stopped(both_stopped(ingest, prepared), output)
    }
}

/// Source switching on the CPU: the session, ingest and output code of the
/// stream, with canonical frames in system memory (`VideoMemory::System`),
/// test-pattern sources and a `fakesink` output. The assertions count and
/// order ticks; the only times compared are lower bounds a loaded machine
/// can only lengthen.
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use gst::prelude::*;
    use obc_core::{AspectPolicy, OutputContract, OutputFormat};

    use super::*;
    use crate::clock::TickKind;
    use crate::output::{EmitInfo, OutputConfig, OutputSink};
    use crate::pipeline::names;
    use crate::test_support::require_elements;

    /// Every element these tests build.
    const ELEMENTS: &[&str] = &[
        "videotestsrc",
        "filesrc",
        "capsfilter",
        "queue",
        "videocrop",
        "videoconvertscale",
        "appsink",
        "appsrc",
        "capssetter",
        "fakesink",
    ];

    /// The output's gap threshold: input older than this is replaced by
    /// placeholders unless a hold is armed.
    const GAP: Duration = Duration::from_millis(100);

    /// The name of a test pattern's `videotestsrc`.
    const PATTERN: &str = "pattern";

    type Ticks = Arc<Mutex<Vec<EmitInfo>>>;

    /// A running session on a system-memory output (NV12 320x240@30 into
    /// a `fakesink`) with the gap threshold [`GAP`], `hold` as its hold
    /// timeout, and every tick recorded.
    fn cpu_session(hold: Duration) -> (Session, Ticks) {
        let (output, ticks) = cpu_output("320x240@30".parse().unwrap(), hold);
        (Session::new(output).expect("start output"), ticks)
    }

    /// The output of [`cpu_session`] for `contract`, built, not started.
    fn cpu_output(contract: OutputContract, hold: Duration) -> (Output, Ticks) {
        require_elements(ELEMENTS);
        let mut cfg = OutputConfig::new(contract, OutputSink::Fake);
        cfg.gap_threshold = GAP;
        cfg.hold_timeout = hold;
        let mut output = Output::build_in(cfg, VideoMemory::System).expect("build output");
        let ticks: Ticks = Arc::default();
        let t = Arc::clone(&ticks);
        output.set_observer(Some(Arc::new(move |e: &EmitInfo| {
            t.lock().unwrap().push(*e);
        })));
        (output, ticks)
    }

    /// A live 30 fps test pattern (NV12 160x120) for `session`, whose first
    /// frame reaches the slot `delay` after it starts (a `queue` holding
    /// back that much), as a camera that is slow to start.
    fn pattern(session: &Session, delay: Option<Duration>) -> Ingest {
        let src = gst::ElementFactory::make("videotestsrc")
            .name(PATTERN)
            .property("is-live", true)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                "video/x-raw,format=NV12,width=160,height=120,framerate=30/1,colorimetry=2:4:5:1"
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
            (160, 120),
            AspectPolicy::Crop,
            session.output().frame_writer(),
        )
        .expect("test pattern ingest")
    }

    /// A source that cannot start: `filesrc` on a file that does not exist
    /// fails its READY -> PAUSED change.
    fn broken(session: &Session) -> Ingest {
        let src = gst::ElementFactory::make("filesrc")
            .property("location", "/nonexistent/obc-test-source")
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                "video/x-raw,format=NV12,width=160,height=120,framerate=30/1"
                    .parse::<gst::Caps>()
                    .unwrap(),
            )
            .build()
            .unwrap();
        Ingest::from_head(
            vec![src, caps],
            "NV12",
            (160, 120),
            AspectPolicy::Crop,
            session.output().frame_writer(),
        )
        .expect("build the broken source")
    }

    /// Polls `done` every 10 ms; fails the test after `limit`.
    fn wait_for(
        session: &mut Session,
        limit: Duration,
        what: &str,
        mut done: impl FnMut(&mut Session) -> bool,
    ) {
        let deadline = Instant::now() + limit;
        while !done(session) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Starts `first` and waits until the output has emitted some of its
    /// frames.
    fn stream(session: &mut Session, first: Ingest) -> u64 {
        let id = first.source_id();
        let t = session.switch_to(first).expect("first source");
        assert_eq!((t.source_id, t.error.as_deref()), (id, None));
        assert!(t.previous_stopped.is_none(), "there was no previous source");
        assert!(t.started.is_some());
        wait_for(session, Duration::from_secs(10), "first frames", |s| {
            s.output().counts().fresh > 10
        });
        id
    }

    /// Records whether `old` was already in NULL when `next` made its first
    /// state change (on `next`'s bus, synchronously, on the thread changing
    /// its state).
    fn watch_start_order(old: &Ingest, next: &Ingest) -> Arc<Mutex<Option<bool>>> {
        let old = old.pipeline().pipeline().clone();
        let seen: Arc<Mutex<Option<bool>>> = Arc::default();
        let s = Arc::clone(&seen);
        next.pipeline().bus().set_sync_handler(move |_, msg| {
            if let gst::MessageView::StateChanged(_) = msg.view() {
                s.lock()
                    .unwrap()
                    .get_or_insert_with(|| old.current_state() == gst::State::Null);
            }
            gst::BusSyncReply::Pass
        });
        seen
    }

    /// Makes stopping `ingest` take `pause` longer after its frames have
    /// stopped: the stopping thread sleeps when the test pattern leaves
    /// PLAYING (the sinks are already paused then).
    fn slow_stop(ingest: &Ingest, pause: Duration) {
        let slept = AtomicBool::new(false);
        ingest.pipeline().bus().set_sync_handler(move |_, msg| {
            if let gst::MessageView::StateChanged(c) = msg.view()
                && msg.src().is_some_and(|s| s.name() == PATTERN)
                && c.old() == gst::State::Playing
                && !slept.swap(true, Ordering::Relaxed)
            {
                std::thread::sleep(pause);
            }
            gst::BusSyncReply::Pass
        });
    }

    /// Makes stopping `ingest` fail: when its pipeline reaches NULL, the
    /// stopping thread (on the bus, synchronously) sets it back to READY,
    /// so the stop ends outside NULL. Once: dropping the pipeline
    /// afterwards takes it to NULL. (A pipeline flushes its bus on the way
    /// to NULL unless told not to; the NULL message would not be seen.)
    fn failing_stop(ingest: &Ingest) {
        failing_stops(ingest, 1);
    }

    /// [`failing_stop`] for the first `times` times it reaches NULL.
    fn failing_stops(ingest: &Ingest, times: u32) {
        ingest.pipeline().pipeline().set_auto_flush_bus(false);
        let pipeline = ingest.pipeline().pipeline().downgrade();
        let raised = AtomicU32::new(0);
        ingest.pipeline().bus().set_sync_handler(move |_, msg| {
            if let gst::MessageView::StateChanged(c) = msg.view()
                && c.current() == gst::State::Null
                && let Some(p) = pipeline.upgrade()
                && msg.src() == Some(p.upcast_ref::<gst::Object>())
                && raised.fetch_add(1, Ordering::Relaxed) < times
            {
                let _ = p.set_state(gst::State::Ready);
            }
            gst::BusSyncReply::Pass
        });
    }

    /// The index of the last fresh frame of source `old` before the first
    /// frame of source `next`, and the index of that first frame (`None`:
    /// `next` emitted nothing, or none is given).
    fn gap_ticks(ticks: &[EmitInfo], old: u64, next: Option<u64>) -> (usize, Option<usize>) {
        let first_next = next.and_then(|n| {
            ticks
                .iter()
                .position(|e| e.origin.is_some_and(|o| o.source_id == n))
        });
        let last_old = ticks[..first_next.unwrap_or(ticks.len())]
            .iter()
            .rposition(|e| {
                e.kind == TickKind::Fresh && e.origin.is_some_and(|o| o.source_id == old)
            })
            .expect("fresh frames of the old source");
        (last_old, first_next)
    }

    /// Whether `e` repeats the frame `frame` emitted (same source, same
    /// slot sequence number).
    fn repeats(e: &EmitInfo, frame: &EmitInfo) -> bool {
        let key = |e: &EmitInfo| e.origin.map(|o| (o.source_id, o.seq));
        e.kind == TickKind::Repeat && key(e).is_some() && key(e) == key(frame)
    }

    /// (a) A switch arms the hold before the old source stops, stops it
    /// before starting the new one, and records its timings: requested,
    /// previous stopped, started, then the new source's first emitted frame.
    /// The old source's stop is slowed down to four times the gap threshold:
    /// the output repeats its last frame throughout, no placeholder.
    #[test]
    fn a_switch_holds_stops_then_starts_and_is_timed() {
        let (mut session, ticks) = cpu_session(OutputConfig::DEFAULT_HOLD_TIMEOUT);
        let first = pattern(&session, None);
        let first_id = stream(&mut session, first);
        let pause = 4 * GAP;
        slow_stop(session.ingest().unwrap(), pause);
        let next = pattern(&session, None);
        let next_id = next.source_id();
        let order = watch_start_order(session.ingest().unwrap(), &next);

        let timing = session.switch_to(next).expect("switch");
        assert_eq!((timing.source_id, timing.error.as_deref()), (next_id, None));
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the new source's first frame",
            |s| s.last_switch().unwrap().first_fresh.is_some(),
        );
        // The clock publishes the first fresh frame before its hold count.
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the hold release",
            |s| s.output().counts().holds_released == 1,
        );
        let last = session.last_switch().unwrap();
        let counts = session.output().counts();
        assert_eq!(session.ingest().map(Ingest::source_id), Some(next_id));
        session.stop().expect("teardown");

        assert_eq!(*order.lock().unwrap(), Some(true), "old stopped first");
        let stopped = last.previous_stopped.expect("the old source was stopped");
        let started = last.started.expect("started");
        let fresh = last.first_fresh.expect("first fresh frame");
        assert!(last.requested <= stopped && stopped <= started && started <= fresh);
        assert!(
            stopped - last.requested >= pause,
            "the slowed stop: {last:?}"
        );
        // The recorded attempt is the one returned, plus the first frame.
        assert_eq!(
            SwitchTiming {
                first_fresh: None,
                ..last.clone()
            },
            timing
        );
        assert_eq!(
            (counts.holds, counts.holds_released, counts.holds_expired),
            (1, 1, 0),
            "{counts:?}"
        );

        let t = ticks.lock().unwrap();
        let (last_old, first_new) = gap_ticks(&t, first_id, Some(next_id));
        let first_new = first_new.unwrap();
        assert_eq!(t[first_new].kind, TickKind::Fresh);
        // first_fresh is that tick's emission (stored in nanoseconds).
        let diff = t[first_new].emitted_at.max(fresh) - t[first_new].emitted_at.min(fresh);
        assert!(diff < Duration::from_micros(1), "{diff:?}");
        let gap = &t[last_old + 1..first_new];
        assert!(!gap.is_empty(), "no tick while the old source stopped");
        assert!(gap.iter().all(|e| repeats(e, &t[last_old])), "{gap:?}");
    }

    /// (b) A switch whose new source fails to start is recorded with its
    /// error, after the old source was stopped (and before the new one was
    /// started). The session is left without a source, the output keeps
    /// running (repeats, then placeholders once the hold times out), and a
    /// later switch succeeds.
    #[test]
    fn a_failed_switch_is_recorded_and_the_next_one_succeeds() {
        let hold = 3 * GAP;
        let (mut session, ticks) = cpu_session(hold);
        let first = pattern(&session, None);
        let first_id = stream(&mut session, first);
        let bad = broken(&session);
        let bad_id = bad.source_id();
        let order = watch_start_order(session.ingest().unwrap(), &bad);

        let err = session
            .switch_to(bad)
            .expect_err("a source that cannot open must fail to start");
        let failed = session.last_switch().expect("the attempt is recorded");
        assert_eq!(failed.source_id, bad_id);
        assert_eq!(failed.error.as_deref(), Some(err.to_string().as_str()));
        assert!(failed.started.is_none() && failed.first_fresh.is_none());
        assert!(
            failed
                .previous_stopped
                .is_some_and(|t| t >= failed.requested)
        );
        assert!(session.ingest().is_none(), "left without a source");
        assert_eq!(*order.lock().unwrap(), Some(true), "old stopped first");
        assert_eq!(failed.unclean_stop, None, "both ingests reached NULL");

        // The hold armed for the broken source times out; the output keeps
        // emitting.
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the hold timeout",
            |s| s.output().counts().holds_expired == 1,
        );
        let emitted = session.output().counts().emitted;
        wait_for(&mut session, Duration::from_secs(10), "more ticks", |s| {
            s.output().counts().emitted >= emitted + 10
        });
        assert!(session.output().is_running());
        session.output().health().expect("the output is healthy");

        let next = pattern(&session, None);
        let next_id = next.source_id();
        let t = session.switch_to(next).expect("a later switch succeeds");
        assert!(t.previous_stopped.is_none(), "there was no source to stop");
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the new source's first frame",
            |s| s.last_switch().unwrap().first_fresh.is_some(),
        );
        let last = session.last_switch().unwrap();
        let counts = session.output().counts();
        session.stop().expect("teardown");
        assert_eq!((last.source_id, last.error), (next_id, None));
        assert_eq!(
            (counts.holds, counts.holds_released, counts.holds_expired),
            (1, 0, 1),
            "{counts:?}"
        );
        // Between the sources: the old frame repeated, then placeholders.
        let t = ticks.lock().unwrap();
        let (last_old, first_new) = gap_ticks(&t, first_id, Some(next_id));
        let gap = &t[last_old + 1..first_new.unwrap()];
        let held = gap.iter().take_while(|e| repeats(e, &t[last_old])).count();
        assert!(held > 0, "{:?}", gap.first());
        assert!(gap[held..].iter().all(|e| e.kind == TickKind::Placeholder));
        assert!(gap.len() - held >= 10, "{} placeholders", gap.len() - held);
    }

    /// (c) A switch to a source whose first frame comes four times the gap
    /// threshold after it starts: every tick in between repeats the old
    /// source's frame, none is a placeholder, and the new source's first
    /// frame ends the hold.
    #[test]
    fn a_switch_repeats_the_old_frame_until_the_new_source_delivers() {
        let (mut session, ticks) = cpu_session(OutputConfig::DEFAULT_HOLD_TIMEOUT);
        let first = pattern(&session, None);
        let first_id = stream(&mut session, first);
        let slow = pattern(&session, Some(4 * GAP));
        let slow_id = slow.source_id();
        session.switch_to(slow).expect("switch");
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the new source's first frame",
            |s| s.output().first_fresh(slow_id).is_some(),
        );
        // The clock publishes the first fresh frame before its hold count.
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the hold release",
            |s| s.output().counts().holds_released == 1,
        );
        let last = session.last_switch().unwrap();
        let counts = session.output().counts();
        session.stop().expect("teardown");
        assert!(
            last.first_fresh.is_some() && last.error.is_none(),
            "{last:?}"
        );
        assert_eq!(
            (counts.holds, counts.holds_released, counts.holds_expired),
            (1, 1, 0),
            "{counts:?}"
        );

        let t = ticks.lock().unwrap();
        let (last_old, first_new) = gap_ticks(&t, first_id, Some(slow_id));
        let first_new = first_new.unwrap();
        assert_eq!(t[first_new].kind, TickKind::Fresh);
        let gap = &t[last_old + 1..first_new];
        assert!(gap.iter().all(|e| repeats(e, &t[last_old])), "{gap:?}");
        // The new source started after the old one stopped and its queue
        // released the first frame once it held 4 x GAP of frames: one
        // frame period less of capture time, at the live rate. The hold
        // outlasted the gap threshold several times over, which a source
        // without the delay (stop and start alone) does not reach.
        let held = t[first_new].emitted_at - t[last_old].emitted_at;
        assert!(held >= 3 * GAP, "held only {held:?}");
    }

    /// (d) A switch to a source that never delivers: the old frame is
    /// repeated until the hold times out, then placeholders follow, and
    /// not before the timeout.
    #[test]
    fn a_hold_that_times_out_falls_back_to_placeholders() {
        let hold = 4 * GAP;
        let (mut session, ticks) = cpu_session(hold);
        let first = pattern(&session, None);
        let first_id = stream(&mut session, first);
        let never = pattern(&session, Some(Duration::from_secs(60)));
        let timing = session.switch_to(never).expect("switch");
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the hold timeout",
            |s| s.output().counts().holds_expired == 1,
        );
        let placeholders = session.output().counts().placeholder;
        wait_for(&mut session, Duration::from_secs(10), "placeholders", |s| {
            s.output().counts().placeholder >= placeholders + 10
        });
        let counts = session.output().counts();
        session.stop().expect("teardown");
        assert_eq!(
            (counts.holds, counts.holds_released, counts.holds_expired),
            (1, 0, 1),
            "{counts:?}"
        );

        let t = ticks.lock().unwrap();
        let (last_old, _) = gap_ticks(&t, first_id, None);
        let after = &t[last_old + 1..];
        let held = after
            .iter()
            .take_while(|e| repeats(e, &t[last_old]))
            .count();
        assert!(held > 0, "{:?}", after.first());
        let rest = &after[held..];
        assert!(rest.len() >= 10, "{} placeholders", rest.len());
        assert!(rest.iter().all(|e| e.kind == TickKind::Placeholder));
        // The deadline is set when the switch arms the hold, after it was
        // requested: no placeholder before the timeout.
        assert!(
            rest[0].emitted_at >= timing.requested + hold,
            "placeholder {:?} after the request",
            rest[0].emitted_at - timing.requested
        );
    }

    /// (e) A switch whose old source fails to stop: the new source is not
    /// started (it is dropped), the attempt is recorded with the stop's
    /// error and no stop time, the session is left without a source, and
    /// the hold armed for the new source runs to its timeout before
    /// placeholders follow.
    #[test]
    fn a_failed_stop_drops_the_next_source_and_the_hold_times_out() {
        let hold = 4 * GAP;
        let (mut session, ticks) = cpu_session(hold);
        let first = pattern(&session, None);
        let first_id = stream(&mut session, first);
        failing_stop(session.ingest().unwrap());
        let next = pattern(&session, None);
        let next_id = next.source_id();
        let next_state = watch_start_order(session.ingest().unwrap(), &next);

        let err = session
            .switch_to(next)
            .expect_err("a source that does not stop must fail the switch");
        assert!(matches!(err, PipelineError::State(_)), "{err:?}");
        let failed = session.last_switch().expect("the attempt is recorded");
        assert_eq!(failed.source_id, next_id);
        assert_eq!(failed.error.as_deref(), Some(err.to_string().as_str()));
        assert_eq!(
            failed.unclean_stop.as_deref(),
            Some(format!("stopping the previous ingest: {err}").as_str()),
            "it ended outside NULL"
        );
        assert_eq!(
            (failed.previous_stopped, failed.started, failed.first_fresh),
            (None, None, None),
            "{failed:?}"
        );
        assert!(session.ingest().is_none(), "left without a source");
        assert_eq!(
            *next_state.lock().unwrap(),
            None,
            "the next source never started"
        );

        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the hold timeout",
            |s| s.output().counts().holds_expired == 1,
        );
        let placeholders = session.output().counts().placeholder;
        wait_for(&mut session, Duration::from_secs(10), "placeholders", |s| {
            s.output().counts().placeholder >= placeholders + 10
        });
        let counts = session.output().counts();
        session.stop().expect("teardown");
        assert_eq!(
            (counts.holds, counts.holds_released, counts.holds_expired),
            (1, 0, 1),
            "{counts:?}"
        );

        let t = ticks.lock().unwrap();
        assert!(
            t.iter()
                .all(|e| e.origin.is_none_or(|o| o.source_id != next_id)),
            "a frame of the dropped source"
        );
        let (last_old, _) = gap_ticks(&t, first_id, None);
        let after = &t[last_old + 1..];
        let held = after
            .iter()
            .take_while(|e| repeats(e, &t[last_old]))
            .count();
        assert!(held > 0, "{:?}", after.first());
        let rest = &after[held..];
        assert!(rest.len() >= 10, "{} placeholders", rest.len());
        assert!(rest.iter().all(|e| e.kind == TickKind::Placeholder));
        assert!(
            rest[0].emitted_at >= failed.requested + hold,
            "placeholder {:?} after the request",
            rest[0].emitted_at - failed.requested
        );
    }

    /// (e2) A switch whose new source fails to start, and does not reach
    /// NULL when it is stopped again: the attempt records it as an unclean
    /// stop (the failed start's own change to NULL is raised back too). A
    /// session stop afterwards is clean: only its own pipelines count.
    #[test]
    fn a_new_source_left_outside_null_after_its_failed_start_is_recorded() {
        let (mut session, _) = cpu_session(OutputConfig::DEFAULT_HOLD_TIMEOUT);
        let bad = broken(&session);
        failing_stops(&bad, 2);
        let err = session
            .switch_to(bad)
            .expect_err("a source that cannot open must fail to start");
        let failed = session.last_switch().expect("the attempt is recorded");
        assert_eq!(failed.error.as_deref(), Some(err.to_string().as_str()));
        let why = failed.unclean_stop.expect("recorded as unclean");
        assert!(
            why.starts_with("stopping the new ingest after it failed to start: ")
                && why.contains("not Null"),
            "{why}"
        );
        assert!(session.ingest().is_none(), "left without a source");
        session
            .stop_checked()
            .expect("the session's own stop is clean");
    }

    /// A test source whose `head` is `videotestsrc` (live, `props`) and
    /// then the capsfilters `caps`, in order, named `test_caps<i>`.
    fn source_with(session: &Session, props: &[(&str, i32)], caps: &[&str]) -> Ingest {
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        for (name, value) in props {
            src.set_property_from_str(name, &value.to_string());
        }
        let mut head = vec![src];
        for (i, c) in caps.iter().enumerate() {
            head.push(
                gst::ElementFactory::make("capsfilter")
                    .name(format!("test_caps{i}"))
                    .property("caps", c.parse::<gst::Caps>().unwrap())
                    .build()
                    .unwrap(),
            );
        }
        Ingest::from_head(
            head,
            "NV12",
            (160, 120),
            AspectPolicy::Crop,
            session.output().frame_writer(),
        )
        .expect("test source")
    }

    /// (g) An ingest that fails while streaming is reported by
    /// [`Session::ingest_failure`], without blocking and on every later
    /// call: a streaming error (two capsfilters linked with the same caps,
    /// then the second one changed to another format, so the source fails
    /// to negotiate once it runs) and an end of stream (a
    /// source that stops after three frames). A healthy source that has
    /// streamed for a while is not reported, and neither is a session
    /// without a source. The output keeps running throughout.
    #[test]
    fn a_failing_ingest_is_reported_and_a_healthy_one_is_not() {
        let (mut session, _) = cpu_session(OutputConfig::DEFAULT_HOLD_TIMEOUT);
        assert_eq!(session.ingest_failure(), None, "no source");
        let healthy = pattern(&session, None);
        stream(&mut session, healthy);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(session.ingest_failure(), None, "a healthy source");

        let nv12 = "video/x-raw,format=NV12,width=160,height=120,framerate=30/1";
        let i420 = "video/x-raw,format=I420,width=160,height=120,framerate=30/1";
        let mismatched = source_with(&session, &[], &[nv12, nv12]);
        mismatched
            .pipeline()
            .element("test_caps1")
            .unwrap()
            .set_property("caps", i420.parse::<gst::Caps>().unwrap());
        let cases = [
            ("a streaming error", mismatched),
            (
                "an end of stream",
                source_with(&session, &[("num-buffers", 3)], &[nv12]),
            ),
        ];
        for (what, ingest) in cases {
            session.switch_to(ingest).expect(what);
            let t0 = Instant::now();
            let failure = loop {
                if let Some(f) = session.ingest_failure() {
                    break f;
                }
                assert!(
                    t0.elapsed() < Duration::from_secs(10),
                    "{what} was not reported"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            eprintln!("{what}: {failure}");
            match what {
                "an end of stream" => assert!(failure.ends_with("end of stream"), "{failure}"),
                _ => assert!(failure.contains("not-negotiated"), "{failure}"),
            }
            assert_eq!(session.ingest_failure().as_deref(), Some(failure.as_str()));
            assert!(session.output().is_running());
        }
        session.stop_ingest().unwrap();
        assert_eq!(session.ingest_failure(), None);
        session.stop().expect("teardown");
    }

    /// (f) A system-memory output with an I420 contract converts the
    /// canonical NV12 frames on the CPU (`videoconvert`): the sink receives
    /// I420 buffers of the contract size, the source's test pattern among
    /// them.
    #[test]
    fn a_system_memory_output_streams_an_i420_contract() {
        require_elements(&["videoconvert"]);
        let contract = OutputContract {
            format: OutputFormat::I420,
            .."320x240@30".parse().unwrap()
        };
        let (output, _) = cpu_output(contract, OutputConfig::DEFAULT_HOLD_TIMEOUT);
        let sink = output.pipeline().element(names::LOOPBACK).unwrap();
        assert_eq!(sink.factory().unwrap().name(), "fakesink");
        // Per buffer reaching the sink: its caps' video info, its size, and
        // the brightest luma sample.
        type Seen = Vec<(Option<gst_video::VideoInfo>, usize, u8)>;
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let s = Arc::clone(&seen);
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                if let Some(buffer) = info.buffer() {
                    let video = pad
                        .current_caps()
                        .and_then(|c| gst_video::VideoInfo::from_caps(&c).ok());
                    let map = buffer.map_readable().unwrap();
                    let luma = video.as_ref().map_or(0, |v| {
                        let (offset, stride) = (v.offset()[0], v.stride()[0] as usize);
                        (0..v.height() as usize)
                            .flat_map(|row| {
                                let start = offset + row * stride;
                                map[start..start + v.width() as usize].iter().copied()
                            })
                            .max()
                            .unwrap_or(0)
                    });
                    s.lock().unwrap().push((video, map.size(), luma));
                }
                gst::PadProbeReturn::Ok
            });
        let mut session = Session::new(output).expect("start output");
        let first = pattern(&session, None);
        stream(&mut session, first);
        session.stop().expect("teardown");

        let seen = seen.lock().unwrap();
        assert!(seen.len() > 10, "{} buffers", seen.len());
        for (video, size, _) in seen.iter() {
            let v = video.as_ref().expect("negotiated video caps");
            assert_eq!(
                (v.format(), v.width(), v.height()),
                (gst_video::VideoFormat::I420, 320, 240)
            );
            assert_eq!(*size, v.size(), "a whole I420 frame");
        }
        // Black is luma 16; the test pattern has white bars.
        assert!(
            seen.iter().any(|(_, _, luma)| *luma > 128),
            "no pattern frame reached the sink"
        );
    }

    /// How many ingest pipelines are open at once: above NULL (a camera's
    /// device is open from NULL -> READY to READY -> NULL) and in PAUSED or
    /// PLAYING, with the most of each seen, and every state change in
    /// order, for the failure message.
    #[derive(Debug, Default)]
    struct Concurrency {
        open: i32,
        max_open: i32,
        active: i32,
        max_active: i32,
        log: Vec<(u64, gst::State, gst::State)>,
    }

    /// Counts `ingest`'s pipeline in `seen` ([`Concurrency`]), from its
    /// own state changes on its bus, synchronously, on the thread making
    /// them. The bus is not flushed on the way to NULL, so the change to
    /// NULL is seen ([`failing_stops`]).
    fn count_open(ingest: &Ingest, seen: &Arc<Mutex<Concurrency>>) {
        let id = ingest.source_id();
        let pipeline = ingest.pipeline().pipeline();
        pipeline.set_auto_flush_bus(false);
        let pipeline = pipeline.downgrade();
        let seen = Arc::clone(seen);
        ingest.pipeline().bus().set_sync_handler(move |_, msg| {
            if let gst::MessageView::StateChanged(c) = msg.view()
                && let Some(p) = pipeline.upgrade()
                && msg.src() == Some(p.upcast_ref::<gst::Object>())
            {
                use gst::State::{Null, Paused, Ready};
                let mut s = seen.lock().unwrap();
                s.log.push((id, c.old(), c.current()));
                match (c.old(), c.current()) {
                    (Null, Ready) => s.open += 1,
                    (Ready, Null) => s.open -= 1,
                    (Ready, Paused) => s.active += 1,
                    (Paused, Ready) => s.active -= 1,
                    _ => {}
                }
                s.max_open = s.max_open.max(s.open);
                s.max_active = s.max_active.max(s.active);
            }
            gst::BusSyncReply::Pass
        });
    }

    /// (f) One stream at a time: across a run of switches, never more than
    /// one ingest pipeline is open (above NULL, so never two in PAUSED or
    /// PLAYING either). The run has a switch requested while the previous
    /// one still holds (its source's first frame is held back 400 ms by a
    /// queue), a switch to a source that fails to start, and three
    /// switches back to back, each old source stopped while it streams.
    #[test]
    fn a_switch_never_has_two_ingests_open() {
        let (mut session, _ticks) = cpu_session(OutputConfig::DEFAULT_HOLD_TIMEOUT);
        let seen: Arc<Mutex<Concurrency>> = Arc::default();
        let first = pattern(&session, None);
        count_open(&first, &seen);
        stream(&mut session, first);
        // A test pattern is no camera: it has no delivered rate.
        assert_eq!(session.delivered_fps(), None);

        let slow = pattern(&session, Some(4 * GAP));
        let slow_id = slow.source_id();
        count_open(&slow, &seen);
        session.switch_to(slow).expect("switch to the slow source");
        let next = pattern(&session, None);
        count_open(&next, &seen);
        assert_eq!(
            session.output().first_fresh(slow_id),
            None,
            "the switch to the slow source still holds"
        );
        assert_eq!(session.output().counts().holds_released, 0);
        session.switch_to(next).expect("switch while holding");

        let bad = broken(&session);
        count_open(&bad, &seen);
        assert!(session.switch_to(bad).is_err(), "the broken source");
        let mut last = 0;
        for _ in 0..3 {
            let next = pattern(&session, None);
            last = next.source_id();
            count_open(&next, &seen);
            session.switch_to(next).expect("back-to-back switch");
        }
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the last source's first frame",
            |s| s.output().first_fresh(last).is_some(),
        );
        session.stop().expect("teardown");

        let s = seen.lock().unwrap();
        assert_eq!((s.max_open, s.max_active), (1, 1), "{:?}", s.log);
        // Seven ingests, each opened once (the broken one too: it fails
        // READY -> PAUSED) and closed.
        let opened = s.log.iter().filter(|(_, o, _)| *o == gst::State::Null);
        assert_eq!(opened.count(), 7, "{:?}", s.log);
        assert_eq!((s.open, s.active), (0, 0), "{:?}", s.log);
    }

    /// (h) An output prepared and committed: the ingest writes into both
    /// slots meanwhile (the prepared output emits the source's frames,
    /// under the same source id, while the output keeps emitting them), and
    /// after the commit the session's output is the prepared one, fed by
    /// the same ingest pipeline, never restarted; the previous output is
    /// stopped and returned, and its ticks end. Source ids handed out after
    /// the commit continue after every earlier one.
    #[test]
    fn a_committed_output_takes_over_the_same_ingest() {
        let (mut session, old_ticks) = cpu_session(OutputConfig::DEFAULT_HOLD_TIMEOUT);
        let first = pattern(&session, None);
        let id = stream(&mut session, first);
        let ingest = session.ingest().unwrap().pipeline().pipeline().clone();
        let (next, new_ticks) = cpu_output("320x240@30".parse().unwrap(), GAP);
        assert_eq!(session.prepared_first_fresh(), None, "nothing prepared");
        session.prepare_output(next).expect("prepare");
        assert!(session.prepared().unwrap().is_running());
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the prepared output's first frame",
            |s| s.prepared_first_fresh().is_some(),
        );
        let old_fresh = session.output().counts().fresh;
        wait_for(&mut session, Duration::from_secs(10), "both fed", |s| {
            s.prepared().unwrap().counts().fresh >= 10
                && s.output().counts().fresh >= old_fresh + 10
        });
        assert!(
            session.prepared().unwrap().frame_writer().source_id() > id,
            "a prepared output's ids continue after the output's"
        );

        let handover = session.commit_prepared().expect("commit");
        handover.previous_stop.expect("the previous output stopped");
        assert!(!handover.previous.is_running());
        assert!(session.prepared().is_none());
        assert!(session.output().first_fresh(id).is_some(), "{id}");
        let fresh = session.output().counts().fresh;
        wait_for(&mut session, Duration::from_secs(10), "frames after", |s| {
            s.output().counts().fresh >= fresh + 10
        });
        assert_eq!(session.ingest().map(Ingest::source_id), Some(id));
        assert_eq!(session.ingest().unwrap().pipeline().pipeline(), &ingest);
        assert_eq!(ingest.current_state(), gst::State::Playing);
        let later = session.output().frame_writer().source_id();
        assert!(later > id, "{later} after {id}");
        let ended = old_ticks.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            old_ticks.lock().unwrap().len(),
            ended,
            "the old clock ticks"
        );
        assert!(matches!(
            session.commit_prepared(),
            Err(PipelineError::State(_))
        ));
        session.stop().expect("teardown");
        let t = new_ticks.lock().unwrap();
        assert!(
            t.iter().filter_map(|e| e.origin).all(|o| o.source_id == id),
            "a frame of another source"
        );
        assert!(
            t.iter().filter(|e| e.kind == TickKind::Fresh).count() >= 20,
            "{} ticks",
            t.len()
        );
    }

    /// (i) A prepared output is discarded by a source switch and by an
    /// ingest stop (its ticks end), and by [`Session::discard_prepared`];
    /// the session's output keeps streaming throughout. An output of
    /// another canonical size is refused.
    #[test]
    fn a_switch_or_an_ingest_stop_discards_the_prepared_output() {
        let (mut session, _) = cpu_session(OutputConfig::DEFAULT_HOLD_TIMEOUT);
        let first = pattern(&session, None);
        stream(&mut session, first);
        let prepare = |session: &mut Session| {
            let (next, ticks) = cpu_output("320x240@30".parse().unwrap(), GAP);
            session.prepare_output(next).expect("prepare");
            wait_for(session, Duration::from_secs(10), "its first frame", |s| {
                s.prepared_first_fresh().is_some()
            });
            ticks
        };
        let assert_stopped = |ticks: &Ticks| {
            let n = ticks.lock().unwrap().len();
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(ticks.lock().unwrap().len(), n, "a discarded output ticks");
        };

        let ticks = prepare(&mut session);
        let next = pattern(&session, None);
        let next_id = next.source_id();
        session.switch_to(next).expect("switch");
        assert!(session.prepared().is_none(), "kept across a switch");
        assert_stopped(&ticks);
        wait_for(
            &mut session,
            Duration::from_secs(10),
            "the new source",
            |s| s.output().first_fresh(next_id).is_some(),
        );

        let ticks = prepare(&mut session);
        session.stop_ingest().unwrap();
        assert!(session.prepared().is_none(), "kept across an ingest stop");
        assert_stopped(&ticks);

        let again = pattern(&session, None);
        stream(&mut session, again);
        let ticks = prepare(&mut session);
        session.discard_prepared().expect("discard");
        assert_stopped(&ticks);
        let fresh = session.output().counts().fresh;
        wait_for(&mut session, Duration::from_secs(10), "the output", |s| {
            s.output().counts().fresh >= fresh + 10
        });
        session.discard_prepared().expect("nothing to discard");

        let (other, _) = cpu_output("640x480@30".parse().unwrap(), GAP);
        let refused = session.prepare_output(other).expect_err("another size");
        assert!(matches!(
            *refused,
            PrepareError {
                error: PipelineError::Caps(_),
                cleanup: Ok(())
            }
        ));
        assert!(session.prepared().is_none());
        session.stop().expect("teardown");
    }
}
