//! A loaded microphone chain: one [`AudioInstance`] per group of the
//! [`Plan`](crate::host::Plan), run one 480-sample block at a time.
//!
//! Every failure fails open: a group whose load fails is left out (a stage
//! failure at load is retried without that stage); a stage that fails on a
//! block is dropped by its module, and that block passes through the
//! group unchanged; a group that panics, loses its last stage, or runs
//! over the deadline (one block, 10 ms) [`OVERRUNS_TO_FAIL`] blocks in a
//! row is failed open, its blocks passed through from then on. Each such
//! event is returned to the caller, who reports it.
//!
//! A chain is built on the loader thread ([`load_group`]), run on the
//! processing thread ([`Chain::process`], [`Chain::reset`]) and unloaded on
//! the retire thread ([`Chain::unload`]): one instance's load, process and
//! unload may run on different threads, one after the other.

use std::time::{Duration, Instant};

use obc_audio::{BLOCK_SAMPLES, SAMPLE_RATE};
use obc_backend::{StageId, Value};
use obc_module_abi::{OBC_AUDIO_FLAG_DISCONT, OBC_AUDIO_FLAG_NEW_SOURCE};
use obc_module_host::{AudioBlock, AudioInstance, AudioSpec, Module, Status, common_block_formats};

use crate::host::GroupPlan;

/// Consecutive blocks over the deadline that fail a group open.
pub const OVERRUNS_TO_FAIL: u32 = 50;

/// The deadline of one block: its duration.
pub const DEADLINE: Duration =
    Duration::from_micros(BLOCK_SAMPLES as u64 * 1_000_000 / SAMPLE_RATE as u64);

/// Something that happened to the chain, for the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `stage` of `module` failed and was dropped.
    StageFailed {
        module: String,
        stage: String,
        reason: String,
    },
    /// The group of `module` (its `stages`) was failed open.
    GroupFailed {
        module: String,
        stages: Vec<String>,
        reason: String,
    },
}

/// One group, loaded.
struct Group {
    module: String,
    instance: AudioInstance,
    latency: u32,
    live: Vec<(String, String, Value)>,
    in_row: u32,
    failed: bool,
}

/// A loaded chain. See the module documentation.
pub struct Chain {
    groups: Vec<Group>,
    /// Output blocks still flagged raw: the warm-up after a load or a new
    /// stream.
    warm: u32,
    /// The voice probability of the last block, if a module gave one.
    voice: Option<f32>,
    /// Blocks that ran over the deadline.
    overruns: u64,
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("modules", &self.module_ids())
            .field("latency", &self.latency())
            .finish_non_exhaustive()
    }
}

/// What loading a group gave.
pub struct GroupLoad {
    /// The group's instance and latency, when it loaded.
    loaded: Option<(AudioInstance, u32)>,
    /// The stages left out, with why.
    pub failures: Vec<(String, String)>,
}

impl GroupLoad {
    /// Whether the group loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }
}

/// Loads `plan`'s group with `module`: proposes 48 kHz mono blocks of
/// 480 samples; a stage failure is retried without the stage.
pub fn load_group(plan: &GroupPlan, module: &Module) -> GroupLoad {
    let mut failures = Vec::new();
    let fail_all = |stages: &[StageId], why: &str, failures: &mut Vec<(String, String)>| {
        for s in stages {
            failures.push((s.to_string(), why.to_string()));
        }
    };
    let mut stages = plan.stages.clone();
    match common_block_formats(module.manifest(), &stages) {
        Ok((rates, sizes, channels)) => {
            if !rates.contains(&SAMPLE_RATE)
                || !sizes.contains(&(BLOCK_SAMPLES as u32))
                || channels != 1
            {
                fail_all(
                    &stages,
                    "The module does not take 48 kHz mono blocks of 480 samples",
                    &mut failures,
                );
                return GroupLoad {
                    loaded: None,
                    failures,
                };
            }
        }
        Err(e) => {
            fail_all(&stages, &e, &mut failures);
            return GroupLoad {
                loaded: None,
                failures,
            };
        }
    }
    let mut instance = match module.create_audio_instance() {
        Ok(i) => i,
        Err(e) => {
            fail_all(&stages, &e.message, &mut failures);
            return GroupLoad {
                loaded: None,
                failures,
            };
        }
    };
    while !stages.is_empty() {
        let spec = AudioSpec {
            stages: stages.clone(),
            sample_rate: SAMPLE_RATE,
            channels_in: 1,
            channels_out: 1,
            frame_samples: BLOCK_SAMPLES as u32,
            params: plan
                .params
                .iter()
                .filter(|(s, _, _)| stages.contains(s))
                .cloned()
                .collect(),
        };
        match instance.load(&spec) {
            Ok(out) if out.frame_samples == BLOCK_SAMPLES as u32 => {
                return GroupLoad {
                    loaded: Some((instance, out.latency_samples)),
                    failures,
                };
            }
            Ok(out) => {
                fail_all(
                    &stages,
                    &format!(
                        "The module chose blocks of {} samples; the worker runs blocks of \
                         {BLOCK_SAMPLES}",
                        out.frame_samples
                    ),
                    &mut failures,
                );
                return GroupLoad {
                    loaded: None,
                    failures,
                };
            }
            Err(e) if e.status == Status::Stage && e.stage.is_some() => {
                let s = e.stage.clone().expect("checked");
                failures.push((s.to_string(), e.message.clone()));
                stages.retain(|x| *x != s);
            }
            Err(e) => {
                fail_all(&stages, &e.message, &mut failures);
                return GroupLoad {
                    loaded: None,
                    failures,
                };
            }
        }
    }
    GroupLoad {
        loaded: None,
        failures,
    }
}

/// The warm-up of a chain of `latency` samples, in blocks.
fn warm_blocks(latency: u32) -> u32 {
    latency.div_ceil(BLOCK_SAMPLES as u32)
}

impl Chain {
    /// A chain of the groups loaded, each with its plan's live parameters.
    pub fn new(groups: Vec<(GroupPlan, GroupLoad)>) -> Chain {
        let groups: Vec<Group> = groups
            .into_iter()
            .filter_map(|(plan, load)| {
                load.loaded.map(|(instance, latency)| Group {
                    module: plan.module,
                    instance,
                    latency,
                    live: plan.live,
                    in_row: 0,
                    failed: false,
                })
            })
            .collect();
        let mut c = Chain {
            groups,
            warm: 0,
            voice: None,
            overruns: 0,
        };
        c.warm = warm_blocks(c.latency());
        c
    }

    /// Whether any group processes audio.
    pub fn is_live(&self) -> bool {
        self.groups.iter().any(|g| !g.failed)
    }

    /// The algorithmic delay of the groups that process audio, in samples.
    pub fn latency(&self) -> u32 {
        self.groups
            .iter()
            .filter(|g| !g.failed)
            .map(|g| g.latency)
            .sum()
    }

    /// Whether the next output block is still warm-up.
    pub fn warming(&self) -> bool {
        self.warm > 0
    }

    /// The modules of the chain, with a group each (in order).
    pub fn module_ids(&self) -> Vec<String> {
        self.groups.iter().map(|g| g.module.clone()).collect()
    }

    /// The stages that process audio, by module.
    pub fn running(&self) -> Vec<(String, String)> {
        self.groups
            .iter()
            .filter(|g| !g.failed)
            .flat_map(|g| {
                g.instance
                    .chain()
                    .iter()
                    .map(|s| (s.to_string(), g.module.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The voice probability of the last block.
    pub fn voice(&self) -> Option<f32> {
        self.voice
    }

    /// Blocks that ran over the deadline.
    pub fn overruns(&self) -> u64 {
        self.overruns
    }

    /// Sets the live parameters of `plan`'s groups (by module, in order).
    pub fn set_live(&mut self, plan: &[GroupPlan]) {
        for (g, p) in self.groups.iter_mut().zip(plan) {
            if g.module == p.module {
                g.live = p.live.clone();
            }
        }
    }

    /// A new stream: the modules forget their temporal state, and the
    /// warm-up starts again.
    pub fn reset(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for g in self.groups.iter_mut().filter(|g| !g.failed) {
            match g.instance.reset_temporal() {
                Ok(dropped) => {
                    for s in dropped.iter() {
                        events.push(Event::StageFailed {
                            module: g.module.clone(),
                            stage: s.to_string(),
                            reason: "Dropped by its module at a new stream".into(),
                        });
                    }
                }
                Err(e) => {
                    g.failed = true;
                    events.push(Event::GroupFailed {
                        module: g.module.clone(),
                        stages: g.instance.chain().iter().map(|s| s.to_string()).collect(),
                        reason: e.message,
                    });
                }
            }
            if g.instance.chain().is_empty() && !g.failed {
                g.failed = true;
            }
        }
        self.warm = warm_blocks(self.latency());
        events
    }

    /// Runs `block` through the chain in place, with `flags` (the input
    /// slot's flags: new stream, discontinuity). Returns whether the block
    /// carries processed audio (`false`: warm-up, or every group failed),
    /// and what happened.
    pub fn process(&mut self, block: &mut [f32; BLOCK_SAMPLES], flags: u32) -> (bool, Vec<Event>) {
        let mut events = Vec::new();
        let mut module_flags = 0;
        if flags & obc_audio::shm::SLOT_FLAG_NEW_STREAM != 0 {
            module_flags |= OBC_AUDIO_FLAG_NEW_SOURCE;
        }
        if flags & obc_audio::shm::SLOT_FLAG_DISCONT != 0 {
            module_flags |= OBC_AUDIO_FLAG_DISCONT;
        }
        let mut out = [0.0f32; BLOCK_SAMPLES];
        let mut over = false;
        let mut voice = None;
        for g in self.groups.iter_mut().filter(|g| !g.failed) {
            let live: Vec<(&str, &str, &Value)> = g
                .live
                .iter()
                .map(|(s, k, v)| (s.as_str(), k.as_str(), v))
                .collect();
            let started = Instant::now();
            let res = {
                let input: [&[f32]; 1] = [&block[..]];
                let mut output: [&mut [f32]; 1] = [&mut out[..]];
                g.instance.process(
                    &input,
                    &mut output,
                    AudioBlock {
                        pts_ns: None,
                        flags: module_flags,
                    },
                    &live,
                )
            };
            let took = started.elapsed();
            match res {
                Ok(v) => {
                    block.copy_from_slice(&out);
                    if v.is_some() {
                        voice = v;
                    }
                }
                Err(e) => {
                    // The block passes through this group as it came.
                    let gone = e.status == Status::Panic || g.instance.chain().is_empty();
                    if let Some(s) = &e.stage
                        && !gone
                    {
                        events.push(Event::StageFailed {
                            module: g.module.clone(),
                            stage: s.to_string(),
                            reason: e.message.clone(),
                        });
                    } else {
                        g.failed = true;
                        let mut stages: Vec<String> =
                            g.instance.chain().iter().map(|s| s.to_string()).collect();
                        if let Some(s) = &e.stage
                            && !stages.contains(&s.to_string())
                        {
                            stages.push(s.to_string());
                        }
                        events.push(Event::GroupFailed {
                            module: g.module.clone(),
                            stages,
                            reason: e.message,
                        });
                        continue;
                    }
                }
            }
            if took > DEADLINE {
                over = true;
                g.in_row += 1;
                if g.in_row >= OVERRUNS_TO_FAIL {
                    g.failed = true;
                    events.push(Event::GroupFailed {
                        module: g.module.clone(),
                        stages: g.instance.chain().iter().map(|s| s.to_string()).collect(),
                        reason: format!(
                            "Failed open after {OVERRUNS_TO_FAIL} blocks in a row over its \
                             deadline of {} ms",
                            DEADLINE.as_millis()
                        ),
                    });
                }
            } else {
                g.in_row = 0;
            }
        }
        if over {
            self.overruns += 1;
        }
        self.voice = voice;
        let processed = self.is_live() && self.warm == 0;
        if self.warm > 0 {
            self.warm -= 1;
        }
        (processed, events)
    }

    /// Unloads every group; returns the modules whose code ran, one per
    /// group, for the crash ledger.
    pub fn unload(mut self) -> Vec<String> {
        for g in &mut self.groups {
            g.instance.unload();
        }
        self.module_ids()
    }
}

/// Mixes `from` into `to` over the block, linearly: `to` holds the result.
pub fn crossfade(from: &[f32; BLOCK_SAMPLES], to: &mut [f32; BLOCK_SAMPLES]) {
    let n = BLOCK_SAMPLES as f32;
    for (i, (t, f)) in to.iter_mut().zip(from).enumerate() {
        let w = (i as f32 + 0.5) / n;
        *t = *f * (1.0 - w) + *t * w;
    }
}
