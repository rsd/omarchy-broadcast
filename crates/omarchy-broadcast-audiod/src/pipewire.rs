//! The worker's PipeWire side, behind [`PipeWireControl`]: the sources a
//! user may pick, the default-source metadata (with capture pinning), the
//! consumers of the microphone node and its `ProcessLatency`. All PipeWire
//! access for the mic lives in the worker.
//!
//! * [`graph`]: what is known of the graph, and the decisions on it (pure);
//! * [`live`]: [`Live`], the implementation over pipewire-rs, on a thread
//!   of its own that connects at the first request and goes at exit;
//! * [`Unavailable`]: no PipeWire (tests, or a worker told not to use it).

pub mod graph;
pub mod live;

use std::fmt;
use std::time::{Duration, Instant};

pub use live::Live;

/// A hardware `Audio/Source` node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDevice {
    /// Its `node.name`.
    pub name: String,
    /// Its `node.description`.
    pub description: String,
}

/// The links on the microphone node, by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Consumers {
    /// Links of applications that record (not monitors or level meters).
    pub real: u32,
    /// Monitor and peak-meter links.
    pub monitors: u32,
}

/// Why a PipeWire request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeWireError {
    /// This worker cannot reach PipeWire for the mic (or was built
    /// without it).
    Unavailable(String),
    /// The request is not valid (an unknown device, among others).
    Invalid(String),
    /// PipeWire refused or failed.
    Failed(String),
}

impl fmt::Display for PipeWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(m) | Self::Invalid(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for PipeWireError {}

/// What the worker asks of PipeWire.
pub trait PipeWireControl: Send + Sync {
    /// The hardware `Audio/Source` nodes, our own nodes excluded, at most
    /// 16.
    fn devices(&self) -> Result<Vec<SourceDevice>, PipeWireError>;

    /// Makes the microphone the default source and pins its capture side to
    /// `target` (a device's `node.name`; empty: the previous default), so
    /// that it never loops onto itself nor picks between equal-priority
    /// mics. Returns the previous default's `node.name` (empty when not
    /// known), for [`PipeWireControl::restore_default`]; called again, it
    /// never returns the microphone itself (see
    /// [`graph::Graph::make_default`]).
    fn make_default(&self, target: &str) -> Result<String, PipeWireError>;

    /// Makes `previous` the default source again, if the microphone is
    /// still the default (WirePlumber's choice when `previous` is empty),
    /// and unpins the capture side.
    fn restore_default(&self, previous: &str) -> Result<(), PipeWireError>;

    /// The consumers of the microphone node, debounced
    /// ([`CONSUMERS_DEBOUNCE`]); `None` when unknown.
    fn consumers(&self) -> Option<Consumers>;

    /// Whether the consumers are unknown only for now: PipeWire is
    /// connected and its graph not synced yet (the GPU gate stays closed
    /// meanwhile). `false` when they are known, or PipeWire is not there.
    fn consumers_pending(&self) -> bool {
        false
    }

    /// The delay the microphone adds now, in samples, for its
    /// `ProcessLatency` (`None`: leave it as it is). Never blocks.
    fn set_latency(&self, _samples: Option<u32>) {}

    /// Lets PipeWire go (the worker exits).
    fn shutdown(&self) {}
}

/// How long a new consumer count must hold before it is reported.
pub const CONSUMERS_DEBOUNCE: Duration = Duration::from_millis(300);

/// A value that changes only once a new one has held for a while; the
/// first value known is taken at once.
#[derive(Debug, Clone)]
pub struct Debounce<T> {
    stable: Option<T>,
    pending: Option<(T, Instant)>,
    after: Duration,
}

impl<T: Clone + PartialEq> Debounce<T> {
    /// A debounce of `after`, nothing known yet.
    pub fn new(after: Duration) -> Self {
        Debounce {
            stable: None,
            pending: None,
            after,
        }
    }

    /// Observes `value` at `now`; returns the debounced value. `None`
    /// (unknown) is taken at once.
    pub fn update(&mut self, value: Option<T>, now: Instant) -> Option<T> {
        let Some(v) = value else {
            self.stable = None;
            self.pending = None;
            return None;
        };
        match &self.stable {
            None => {
                self.stable = Some(v);
                self.pending = None;
            }
            Some(s) if *s == v => self.pending = None,
            Some(_) => match &self.pending {
                Some((p, since)) if *p == v => {
                    if now.duration_since(*since) >= self.after {
                        self.stable = Some(v);
                        self.pending = None;
                    }
                }
                _ => self.pending = Some((v, now)),
            },
        }
        self.stable.clone()
    }
}

/// The reason of [`Unavailable`].
pub const REASON_UNAVAILABLE: &str = "PipeWire control is not available in this audio worker";

/// A [`PipeWireControl`] that has no PipeWire: every request fails with
/// [`PipeWireError::Unavailable`], and the consumers are unknown.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unavailable;

impl PipeWireControl for Unavailable {
    fn devices(&self) -> Result<Vec<SourceDevice>, PipeWireError> {
        Err(PipeWireError::Unavailable(REASON_UNAVAILABLE.into()))
    }

    fn make_default(&self, _target: &str) -> Result<String, PipeWireError> {
        Err(PipeWireError::Unavailable(REASON_UNAVAILABLE.into()))
    }

    fn restore_default(&self, _previous: &str) -> Result<(), PipeWireError> {
        Err(PipeWireError::Unavailable(REASON_UNAVAILABLE.into()))
    }

    fn consumers(&self) -> Option<Consumers> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_takes_the_first_value_then_waits_for_a_change_to_hold() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut d = Debounce::new(CONSUMERS_DEBOUNCE);
        assert_eq!(d.update(Some(0), ms(0)), Some(0));
        assert_eq!(d.update(Some(1), ms(10)), Some(0));
        assert_eq!(d.update(Some(1), ms(200)), Some(0));
        assert_eq!(d.update(Some(1), ms(310)), Some(1));
        // A blip shorter than the debounce is never seen.
        assert_eq!(d.update(Some(0), ms(400)), Some(1));
        assert_eq!(d.update(Some(1), ms(500)), Some(1));
        assert_eq!(d.update(Some(0), ms(900)), Some(1));
        // Another value restarts the wait.
        assert_eq!(d.update(Some(2), ms(1000)), Some(1));
        assert_eq!(d.update(Some(0), ms(1250)), Some(1));
        assert_eq!(d.update(Some(0), ms(1500)), Some(1));
        assert_eq!(d.update(Some(0), ms(1560)), Some(0));
        // Unknown at once, and the next value known at once.
        assert_eq!(d.update(None, ms(1600)), None);
        assert_eq!(d.update(Some(3), ms(1601)), Some(3));
    }
}
