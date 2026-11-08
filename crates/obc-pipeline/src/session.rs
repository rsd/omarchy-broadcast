//! A running stream: one output and the ingest currently feeding it, with
//! source switching (`docs/adr/0009-ingest-process-output.md`).
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

use std::time::{Duration, Instant};

use crate::ingest::{Ingest, IngestConfig};
use crate::output::Output;
use crate::pipeline::PipelineError;

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
}

impl SwitchTiming {
    /// From the request to the new source's first emitted frame.
    pub fn gap(&self) -> Option<Duration> {
        self.first_fresh
            .map(|t| t.saturating_duration_since(self.requested))
    }
}

/// The output plus its current ingest. Dropping it stops the ingest first,
/// then the output.
#[derive(Debug)]
pub struct Session {
    // Field order is drop order: ingest before output.
    ingest: Option<Ingest>,
    output: Output,
    last_switch: Option<SwitchTiming>,
}

impl Session {
    /// Takes over `output`, starting it if it is not running.
    pub fn new(mut output: Output) -> Result<Self, PipelineError> {
        output.start()?;
        Ok(Session {
            ingest: None,
            output,
            last_switch: None,
        })
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    pub fn ingest(&self) -> Option<&Ingest> {
        self.ingest.as_ref()
    }

    /// Builds (but does not start) an ingest for `cfg` writing into this
    /// session's output, so the caller can inspect or probe it before
    /// [`Self::switch_to`].
    pub fn build_ingest(&self, cfg: &IngestConfig) -> Result<Ingest, PipelineError> {
        Ingest::build(cfg, self.output.frame_writer())
    }

    /// Makes `next` the current source: arms the output's hold for `next`
    /// (when a source is streaming), stops the current ingest, then starts
    /// `next` with the output's CUDA context. If stopping the current
    /// ingest fails, `next` is not started (it is dropped); if starting
    /// `next` fails, the session has no source. Either way the hold armed
    /// for `next` runs to its timeout
    /// ([`OutputConfig::hold_timeout`](crate::output::OutputConfig::hold_timeout),
    /// 3 s by default): the output repeats the last frame that long, then
    /// sends placeholders.
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
        let mut timing = SwitchTiming {
            source_id: next.source_id(),
            requested,
            previous_stopped: None,
            started: None,
            first_fresh: None,
            error: None,
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
            old.stop()?;
            timing.previous_stopped = Some(Instant::now());
        }
        let cuda = self.output.cuda_context().ok_or_else(|| {
            PipelineError::Cuda("the output is not running: it has no CUDA context to share".into())
        })?;
        next.start(cuda)?;
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

    /// Stops the current ingest, if any; the output keeps running.
    pub fn stop_ingest(&mut self) -> Result<(), PipelineError> {
        match self.ingest.take() {
            Some(i) => i.stop(),
            None => Ok(()),
        }
    }

    /// Stops the ingest, then the output, reporting the first failure.
    pub fn stop(mut self) -> Result<(), PipelineError> {
        let ingest = self.stop_ingest();
        let output = self.output.stop();
        ingest.and(output)
    }
}
