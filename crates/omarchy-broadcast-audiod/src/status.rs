//! The worker's status, as `GetStatus` and `StatusChanged` carry it (JSON).

use serde::Serialize;

/// The state of the microphone processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Processing is off: raw passthrough.
    Off,
    /// On, but nothing is processed: no plugin is connected (nothing
    /// records), or no stage is on or able to run: raw.
    Passthrough,
    /// A chain is loading or warming up: raw until it is ready.
    Starting,
    /// A chain processes the microphone.
    Active,
    /// Stages are on but none of them could run: raw, with the reason.
    Error,
}

/// One stage, as the status lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageStatus {
    /// The stage id.
    pub id: String,
    /// The module that runs it, or would.
    pub provider: Option<String>,
    /// `running`, `loading`, `off` or `failed`.
    pub state: String,
    /// Why it does not run; empty when it does.
    pub reason: String,
}

/// The status. See the module documentation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub state: State,
    /// Applications recording from the mic (0 when unknown).
    pub consumers: u32,
    /// Whether the consumers are known (PipeWire control available).
    pub consumers_known: bool,
    /// Whether a mic plugin is connected (or connecting again, after the
    /// connection was dropped for a deeper pipeline).
    pub connected: bool,
    /// The module that runs each running stage (empty while no plugin is
    /// connected).
    pub provider: std::collections::BTreeMap<String, String>,
    /// The delay processing adds at a 480-sample quantum, in milliseconds:
    /// the pipeline depth plus the chain's algorithmic delay (0 while no
    /// plugin is connected).
    pub latency_ms: f64,
    /// The chain's algorithmic delay, in samples (0 while no plugin is
    /// connected).
    pub latency_samples: u32,
    /// The pipeline depth in blocks (of the connection, else of the plan).
    pub pipeline_depth: u32,
    /// The first error, empty when none.
    pub error: String,
    /// Blocks over the deadline, since the worker started.
    pub overruns: u64,
    /// Blocks the plugin played raw because the processed one was late
    /// (its counter, for the current connection).
    pub misses: u64,
    /// The voice probability of the last block, -1 when unknown.
    pub voice: f64,
    /// Every audio stage offered.
    pub stages: Vec<StageStatus>,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            state: State::Passthrough,
            consumers: 0,
            consumers_known: false,
            connected: false,
            provider: Default::default(),
            latency_ms: 0.0,
            latency_samples: 0,
            pipeline_depth: 1,
            error: String::new(),
            overruns: 0,
            misses: 0,
            voice: -1.0,
            stages: Vec::new(),
        }
    }
}

impl Status {
    /// The status without its counters: what a `StatusChanged` is sent
    /// for (the counters go out with `Stats`).
    pub fn without_counters(&self) -> Status {
        Status {
            overruns: 0,
            misses: 0,
            voice: -1.0,
            ..self.clone()
        }
    }

    /// The JSON text.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("the status serialises")
    }
}
