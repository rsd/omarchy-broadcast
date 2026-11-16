//! The instances of an `audio-filter` module (ABI 1.2): one chain of the
//! module's audio stages for one microphone stream, in the audio worker.
//!
//! An [`AudioInstance`] is made by [`crate::Module::create_audio_instance`]
//! of a module opened by a host of [`crate::LoadKinds::Audio`]
//! ([`crate::Module::open_for`]). It is loaded with an [`AudioSpec`] (the
//! stages, the block format, the load-time parameters) and then runs one
//! block at a time ([`AudioInstance::process`]), with safe slices: the
//! input's planes and the output's never overlap, as the ABI promises.
//!
//! The rules are those of the video [`crate::Instance`], without matte
//! stages: a failed load on a loaded instance with the same block format
//! (a switch) keeps the previous chain, less the stage of a stage failure;
//! a failed first load or reload leaves it unloaded; a stage that fails on
//! its own while processing is dropped from the chain and the next block
//! runs without it (every audio stage fails open); a panic unloads the
//! instance, which then refuses blocks until it is loaded again. Counting
//! overruns, and failing a stage open after a run of them, is the worker's
//! business, not this type's.

use std::ffi::{CString, c_char};
use std::fmt;
use std::sync::Arc;

use obc_backend::{StageId, StageList, Value};
use obc_module_abi::{
    OBC_AUDIO_F32_PLANAR, OBC_ERR_STAGE, OBC_MAX_AUDIO_CHANNELS, OBC_OK, obc_audio_buffer,
    obc_audio_load_report, obc_audio_request, obc_instance, obc_status, obc_timing,
};

use crate::gating::{self, Constant, HostStruct};
use crate::library::LogLevel;
use crate::manifest::Manifest;
use crate::module::{
    ModuleCore, ModuleError, RawParams, Status, gate, owned_text, reported_entries, reported_name,
};

/// What [`AudioInstance::load`] loads, owned: the ABI's audio request with
/// its strings.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSpec {
    /// The stages, in the order they run (`mic-*` ids of the module's
    /// manifest).
    pub stages: Vec<StageId>,
    /// Samples per second: one of the `rates` of every stage.
    pub sample_rate: u32,
    /// The channels of the input blocks: the `channels` of every stage.
    pub channels_in: u32,
    /// The channels of the output blocks.
    pub channels_out: u32,
    /// The samples per channel per block proposed: one of the
    /// `frame_samples` of every stage. The module may choose another of
    /// them ([`AudioLoadOutcome::frame_samples`]).
    pub frame_samples: u32,
    /// The load-time parameters: `(stage, key, value)`.
    pub params: Vec<(StageId, String, Value)>,
}

/// The block format of an [`AudioSpec`]: everything but its stages and
/// parameters. A load that keeps the format of the loaded chain is a
/// switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioFormat {
    sample_rate: u32,
    channels_in: u32,
    channels_out: u32,
    frame_samples: u32,
}

impl AudioSpec {
    fn format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: self.sample_rate,
            channels_in: self.channels_in,
            channels_out: self.channels_out,
            frame_samples: self.frame_samples,
        }
    }
}

/// The block formats every stage of `stages` takes, as `manifest` declares
/// them: the common sample rates and the common block sizes, each in the
/// order of preference of the first stage, and the channels they share.
/// What a worker proposes a load from. The reason names the stage that is
/// not an audio stage of the manifest, or says what they do not share.
pub fn common_block_formats(
    manifest: &Manifest,
    stages: &[StageId],
) -> Result<(Vec<u32>, Vec<u32>, u32), String> {
    let mut decls = Vec::with_capacity(stages.len());
    for s in stages {
        let decl = manifest
            .stage(s.as_str())
            .and_then(|d| d.audio.as_ref())
            .ok_or_else(|| format!("{s} is not an audio stage of the module {}", manifest.id))?;
        decls.push((s, decl));
    }
    let Some((_, first)) = decls.first() else {
        return Err("no stage is asked for".into());
    };
    let rates: Vec<u32> = first
        .rates
        .iter()
        .copied()
        .filter(|r| decls.iter().all(|(_, d)| d.rates.contains(r)))
        .collect();
    let sizes: Vec<u32> = first
        .frame_samples
        .iter()
        .copied()
        .filter(|n| decls.iter().all(|(_, d)| d.frame_samples.contains(n)))
        .collect();
    let channels = first.channels;
    if let Some((s, _)) = decls.iter().find(|(_, d)| d.channels != channels) {
        return Err(format!(
            "{s} takes another channel count than {}",
            stages[0]
        ));
    }
    if rates.is_empty() {
        return Err("the stages share no sample rate".into());
    }
    if sizes.is_empty() {
        return Err("the stages share no block size".into());
    }
    Ok((rates, sizes, channels))
}

/// What a successful [`AudioInstance::load`] reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioLoadOutcome {
    /// The load time, in milliseconds.
    pub load_ms: f32,
    /// The warm-up time, if there was one.
    pub warmup_ms: Option<f32>,
    /// The samples per channel of every block the chain takes: the
    /// proposed one, or another of the manifest's the module chose.
    pub frame_samples: u32,
    /// The chain's algorithmic delay, in samples, its lookahead included.
    pub latency_samples: u32,
    /// Whether models of the previous load were reused.
    pub reused_model: bool,
}

/// What comes with a block besides its samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioBlock {
    /// The capture time of the block's first sample, in nanoseconds, if
    /// known.
    pub pts_ns: Option<i64>,
    /// `OBC_AUDIO_FLAG_*` bits; those this ABI does not know are cleared.
    pub flags: u32,
}

/// What the loaded chain takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Loaded {
    format: AudioFormat,
    frame_samples: u32,
    latency_samples: u32,
}

/// An instance of an `audio-filter` module: one chain of its audio stages
/// for one stream. See the module documentation.
pub struct AudioInstance {
    core: Arc<ModuleCore>,
    manifest: Arc<Manifest>,
    raw: *mut obc_instance,
    /// The stages of the last load, to read stage failures against.
    chain: Vec<StageId>,
    loaded: Option<Loaded>,
    /// Why the instance refuses blocks: it panicked.
    poisoned: Option<String>,
}

// SAFETY: an instance is used through `&mut self` (or `&self` for reads
// that the type's `!Sync` keeps on one thread), so its calls are
// serialised; the ABI allows them on any thread.
unsafe impl Send for AudioInstance {}

impl fmt::Debug for AudioInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioInstance")
            .field("module", &self.core.id)
            .field("chain", &self.chain)
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl Drop for AudioInstance {
    fn drop(&mut self) {
        let f = self
            .core
            .lib
            .vtable()
            .instance_destroy
            .expect("checked at open");
        // SAFETY: created by `instance_create`, destroyed once.
        unsafe { f(self.raw) };
    }
}

fn invalid(message: impl Into<String>) -> ModuleError {
    ModuleError::host(Status::Invalid, message)
}

impl AudioInstance {
    pub(crate) fn new(
        core: Arc<ModuleCore>,
        manifest: Arc<Manifest>,
        raw: *mut obc_instance,
    ) -> AudioInstance {
        AudioInstance {
            core,
            manifest,
            raw,
            chain: Vec::new(),
            loaded: None,
            poisoned: None,
        }
    }

    /// The module id.
    pub fn module_id(&self) -> &str {
        &self.core.id
    }

    /// Whether a chain is loaded, as far as the host knows.
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// The stages of the last load that succeeded or that keeps the
    /// previous chain, less those dropped since.
    pub fn chain(&self) -> &[StageId] {
        &self.chain
    }

    /// The samples per channel of every block the loaded chain takes.
    pub fn frame_samples(&self) -> Option<u32> {
        self.loaded.map(|l| l.frame_samples)
    }

    /// The loaded chain's algorithmic delay, in samples.
    pub fn latency_samples(&self) -> Option<u32> {
        self.loaded.map(|l| l.latency_samples)
    }

    fn last_error(&self) -> String {
        let f = self
            .core
            .lib
            .vtable()
            .instance_last_error
            .expect("checked at open");
        // SAFETY: a live instance; copied before the next call.
        unsafe { owned_text(f(self.raw)) }
    }

    /// The error of a failed call; a panic unloads the instance, which then
    /// refuses blocks until the next successful load. A stage failure whose
    /// stage cannot be read (the module dropped a stage the host cannot
    /// name) unloads it too. A stage failure is read against `chain`: the
    /// request's stages for a load, the loaded chain otherwise.
    fn fail(&mut self, status: obc_status, what: &str, chain: &[StageId]) -> ModuleError {
        let e = ModuleError::from_call(status, self.last_error(), chain);
        if e.status == Status::Panic {
            self.core.log(
                LogLevel::Error,
                &format!(
                    "the module panicked in {what}; the instance is unloaded: {}",
                    e.message
                ),
            );
            self.unload();
            self.poisoned = Some(e.message.clone());
        }
        e
    }

    /// Loads `spec`. A load on a loaded instance with the block format of
    /// the loaded chain is a switch; one with another format a reload. A
    /// failed switch keeps the previous chain (less the stage of a stage
    /// failure); a failed first load or reload, and a panic, leave the
    /// instance unloaded. A spec with no stage, a channel count of 0 or
    /// more than `OBC_MAX_AUDIO_CHANNELS`, a rate or block size of 0, or
    /// one the manifest does not declare (a stage that is not an audio
    /// stage of the module, a rate, block size or channel count not
    /// declared for every stage) is refused ([`Status::Invalid`]) before
    /// the module is called, the instance left as it was; so is a
    /// module's choice of a block size the manifest does not declare for
    /// every stage ([`Status::Failed`], the instance unloaded).
    pub fn load(&mut self, spec: &AudioSpec) -> Result<AudioLoadOutcome, ModuleError> {
        if spec.stages.is_empty() {
            return Err(invalid("an audio chain needs at least one stage"));
        }
        for (what, n) in [
            ("channels_in", spec.channels_in),
            ("channels_out", spec.channels_out),
        ] {
            if n == 0 || n as usize > OBC_MAX_AUDIO_CHANNELS {
                return Err(invalid(format!(
                    "{what} {n} is not 1 to {OBC_MAX_AUDIO_CHANNELS}"
                )));
            }
        }
        if spec.sample_rate == 0 || spec.frame_samples == 0 {
            return Err(invalid(
                "the sample rate and the block size must be non-zero",
            ));
        }
        self.check_declared(spec)?;
        let names: Vec<CString> = spec
            .stages
            .iter()
            .map(|s| CString::new(s.as_str()).expect("stage ids have no NUL byte"))
            .collect();
        let ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let params = RawParams::new(
            spec.params
                .iter()
                .map(|(s, k, v)| (s.as_str(), k.as_str(), v)),
        )?;
        let minor = self.core.lib.abi_minor();
        gate(minor, params.value_kinds())?;
        let mut req = obc_audio_request::empty();
        req.struct_size = gating::struct_size(HostStruct::AudioRequest, minor);
        req.stages = ptrs.as_ptr();
        req.n_stages = ptrs.len();
        req.sample_rate = spec.sample_rate;
        req.channels_in = spec.channels_in;
        req.channels_out = spec.channels_out;
        req.frame_samples = spec.frame_samples;
        req.params = params.as_ptr();
        req.n_params = params.len();
        let mut report = obc_audio_load_report::empty(spec.frame_samples);
        report.struct_size = gating::struct_size(HostStruct::AudioLoadReport, minor);
        let f = self
            .core
            .lib
            .vtable()
            .audio_instance_load
            .expect("checked at open: an audio module has the audio entries");
        // SAFETY: a live instance; the request, its strings and parameters
        // live across the call; `report` is writable.
        let status = unsafe { f(self.raw, &req, &mut report) };
        let format = spec.format();
        if status != OBC_OK {
            let switch = self.loaded.is_some_and(|l| l.format == format);
            let e = self.fail(status, "an audio load", &spec.stages);
            if e.status == Status::Panic {
                // Unloaded by `fail`.
            } else if !switch || (e.code == OBC_ERR_STAGE && e.stage.is_none()) {
                // Nothing loaded, as the module agrees once told.
                self.unload();
            } else if let Some(stage) = &e.stage {
                self.chain.retain(|s| s != stage);
            }
            return Err(e);
        }
        let chosen = report.frame_samples;
        if chosen != spec.frame_samples && !self.declares_block(&spec.stages, chosen) {
            self.unload();
            return Err(ModuleError::host(
                Status::Failed,
                format!(
                    "the module chose blocks of {chosen} samples, which the manifest does not \
                     declare for every stage"
                ),
            ));
        }
        self.loaded = Some(Loaded {
            format,
            frame_samples: chosen,
            latency_samples: report.latency_samples,
        });
        self.poisoned = None;
        self.chain = spec.stages.clone();
        Ok(AudioLoadOutcome {
            load_ms: report.load_ms,
            warmup_ms: (report.warmup_ms >= 0.0).then_some(report.warmup_ms),
            frame_samples: chosen,
            latency_samples: report.latency_samples,
            reused_model: report.reused_model != 0,
        })
    }

    /// Refuses ([`Status::Invalid`]) a spec the manifest does not declare:
    /// a stage that is not an audio stage of this module, or a sample rate,
    /// block size or channel count (in or out) not declared for every
    /// stage.
    fn check_declared(&self, spec: &AudioSpec) -> Result<(), ModuleError> {
        for s in &spec.stages {
            let Some(decl) = self
                .manifest
                .stage(s.as_str())
                .and_then(|d| d.audio.as_ref())
            else {
                return Err(invalid(format!(
                    "{s} is not an audio stage of the module {}",
                    self.manifest.id
                )));
            };
            if !decl.rates.contains(&spec.sample_rate) {
                return Err(invalid(format!(
                    "{s} does not declare the sample rate {}",
                    spec.sample_rate
                )));
            }
            if !decl.frame_samples.contains(&spec.frame_samples) {
                return Err(invalid(format!(
                    "{s} does not declare blocks of {} samples",
                    spec.frame_samples
                )));
            }
            if spec.channels_in != decl.channels || spec.channels_out != decl.channels {
                return Err(invalid(format!(
                    "{s} takes {} channels, not {} in and {} out",
                    decl.channels, spec.channels_in, spec.channels_out
                )));
            }
        }
        Ok(())
    }

    /// Whether the manifest declares blocks of `n` samples for every stage
    /// of `stages`.
    fn declares_block(&self, stages: &[StageId], n: u32) -> bool {
        stages.iter().all(|s| {
            self.manifest
                .stage(s.as_str())
                .and_then(|d| d.audio.as_ref())
                .is_some_and(|a| a.frame_samples.contains(&n))
        })
    }

    /// Runs one block through the loaded chain, from `input` (one slice per
    /// input channel) into `output` (one per output channel), every slice
    /// of the loaded block size ([`AudioInstance::frame_samples`]), with
    /// the live parameters `live` (`(stage, key, value)`). Returns the
    /// module's voice probability of the block (0.0 to 1.0), when it gave
    /// one.
    ///
    /// Blocks of another shape are refused ([`Status::Invalid`]) before the
    /// module is called. After a stage failure ([`Status::Stage`]) the
    /// module has dropped the stage, so has [`AudioInstance::chain`], and
    /// the output was not written: the caller passes this block on as it
    /// came.
    pub fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        block: AudioBlock,
        live: &[(&str, &str, &Value)],
    ) -> Result<Option<f32>, ModuleError> {
        if let Some(reason) = &self.poisoned {
            return Err(ModuleError::host(
                Status::State,
                format!("the module panicked earlier and must be loaded again: {reason}"),
            ));
        }
        let Some(loaded) = self.loaded else {
            return Err(ModuleError::host(
                Status::State,
                "no audio chain is loaded: a block came before a successful load",
            ));
        };
        let n = loaded.frame_samples as usize;
        let f = loaded.format;
        if input.len() != f.channels_in as usize || output.len() != f.channels_out as usize {
            return Err(invalid(format!(
                "a block of {} input and {} output channels; the chain is loaded for {} and {}",
                input.len(),
                output.len(),
                f.channels_in,
                f.channels_out
            )));
        }
        if input.iter().any(|p| p.len() != n) || output.iter().any(|p| p.len() != n) {
            return Err(invalid(format!(
                "a plane is not of the loaded block size, {n} samples"
            )));
        }
        let params = RawParams::new(live.iter().copied())?;
        let minor = self.core.lib.abi_minor();
        gate(
            minor,
            std::iter::once(Constant::AudioFormat(OBC_AUDIO_F32_PLANAR))
                .chain(params.value_kinds()),
        )?;
        let size = gating::struct_size(HostStruct::AudioBuffer, minor);
        let mut inb = obc_audio_buffer::empty();
        inb.struct_size = size;
        inb.format = OBC_AUDIO_F32_PLANAR;
        inb.sample_rate = f.sample_rate;
        inb.channels = f.channels_in;
        inb.n_samples = loaded.frame_samples;
        for (slot, plane) in inb.planes.iter_mut().zip(input) {
            // Read-only for the module, by the ABI.
            *slot = plane.as_ptr().cast_mut();
        }
        inb.pts_ns = block.pts_ns.filter(|t| *t >= 0).unwrap_or(-1);
        // Only the flags of the module's version (all of 1.2 today).
        inb.flags = block.flags & gating::audio_flags(minor);
        let mut outb = obc_audio_buffer::empty();
        outb.struct_size = size;
        outb.format = OBC_AUDIO_F32_PLANAR;
        outb.sample_rate = f.sample_rate;
        outb.channels = f.channels_out;
        outb.n_samples = loaded.frame_samples;
        for (slot, plane) in outb.planes.iter_mut().zip(output.iter_mut()) {
            *slot = plane.as_mut_ptr();
        }
        outb.pts_ns = inb.pts_ns;
        let call = self
            .core
            .lib
            .vtable()
            .audio_instance_process
            .expect("checked at open: an audio module has the audio entries");
        // SAFETY: a live instance; the planes are borrowed slices of the
        // block size that live across the call, the input's shared and the
        // output's exclusive, so none overlaps another; the parameters live
        // across the call.
        let status = unsafe { call(self.raw, &inb, &mut outb, params.as_ptr(), params.len()) };
        if status != OBC_OK {
            let chain = self.chain.clone();
            let e = self.fail(status, "an audio block", &chain);
            if e.code == OBC_ERR_STAGE && e.stage.is_none() {
                // The module dropped a stage the host cannot name.
                self.unload();
            } else if let Some(stage) = &e.stage {
                self.chain.retain(|s| s != stage);
            }
            return Err(e);
        }
        let v = outb.voice_probability;
        Ok((0.0..=1.0).contains(&v).then_some(v))
    }

    /// Forgets temporal state (a new source); returns the stages the module
    /// dropped from the chain.
    pub fn reset_temporal(&mut self) -> Result<StageList, ModuleError> {
        let f = self
            .core
            .lib
            .vtable()
            .instance_reset_temporal
            .expect("checked at open");
        let mut out: *const c_char = std::ptr::null();
        // SAFETY: a live instance; `out` writable.
        let status = unsafe { f(self.raw, &mut out) };
        if status != OBC_OK {
            let chain = self.chain.clone();
            return Err(self.fail(status, "a temporal reset", &chain));
        }
        // SAFETY: a module-owned string valid until the next call.
        let json = unsafe { owned_text(out) };
        let ids: Vec<String> = if json.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&json).map_err(|e| {
                ModuleError::host(
                    Status::Failed,
                    format!("the temporal reset returned malformed JSON: {e}"),
                )
            })?
        };
        let mut dropped = Vec::new();
        for id in ids {
            match self.chain.iter().find(|s| s.as_str() == id) {
                Some(s) => dropped.push(s.clone()),
                None => self.core.log(
                    LogLevel::Warn,
                    &format!(
                        "the temporal reset dropped {id:?}, which is not in the chain; ignored"
                    ),
                ),
            }
        }
        self.chain.retain(|s| !dropped.contains(s));
        StageList::new(dropped).map_err(|e| ModuleError::host(Status::Failed, e.to_string()))
    }

    /// Releases what is loaded (a GPU context the module made, among the
    /// rest). Idempotent.
    pub fn unload(&mut self) {
        let f = self
            .core
            .lib
            .vtable()
            .instance_unload
            .expect("checked at open");
        // SAFETY: a live instance.
        unsafe { f(self.raw) };
        self.loaded = None;
        self.chain.clear();
    }

    /// The step times of the last processed block: `(step, ms)`; empty when
    /// the module reports none, and when the call fails or the list is
    /// null, misaligned or longer than `MAX_REPORTED_ENTRIES` (step names
    /// are cut to `MAX_REPORTED_NAME` bytes).
    pub fn timings(&self) -> Vec<(String, f32)> {
        let Some(f) = self.core.lib.vtable().instance_timings else {
            return Vec::new();
        };
        let (mut out, mut n): (*const obc_timing, usize) = (std::ptr::null(), 0);
        // SAFETY: a live instance; `AudioInstance` is not `Sync`, so no
        // other call on it runs; the outputs are writable.
        let status = unsafe { f(self.raw, &mut out, &mut n) };
        if status != OBC_OK {
            return Vec::new();
        }
        // SAFETY: `n` entries owned by the instance, valid until the next
        // call; copied now.
        let entries = unsafe { reported_entries(out, n) };
        entries
            .iter()
            // SAFETY: NUL-terminated step names, as above.
            .map(|t| (unsafe { reported_name(t.step) }, t.ms))
            .collect()
    }
}
