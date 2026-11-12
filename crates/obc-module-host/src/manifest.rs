//! The module manifest: a TOML file in a `modules.d` directory that names a
//! module, its library and the stages it provides.
//!
//! ```toml
//! abi = 1
//! id = "org.example.hello"
//! name = "Hello tint"
//! version = "0.1.0"
//! licence = "MIT OR Apache-2.0"
//! kind = "video-stage"
//! library = "libobc_module_hello.so"   # absolute, or relative to this file
//! description = "Tints the picture warm"  # optional
//! conflicts = ["org.example.other"]     # optional: module ids
//!
//! [requires]                            # optional
//! hardware = ["nvidia-turing+"]         # informational
//! modules = ["org.example.base"]        # module ids, enforced
//!
//! [[stage]]
//! id = "hello"
//! slot = "post-composite"               # pre-matte | matte | post-matte | post-composite
//! rank = 100
//! device = "cpu"                        # nvidia-gpu | gpu | npu | cpu
//! position = 100                        # optional, lower first within the slot
//! requires = []                         # optional: stage ids or slot names
//! conflicts = []                        # optional: stage ids
//! frames = ["host"]                     # cuda | host | dmabuf (reserved)
//! formats = ["nv12"]                    # nv12 | i420 | bgra | rgba | rgb-f32-planar
//! ```
//!
//! An `audio-filter` module (ABI 1.2) declares audio stages: their ids
//! start with [`AUDIO_STAGE_PREFIX`] (and a video stage's never do), their
//! slot is one of the audio slots, they take host frames only, and they
//! give the block formats they accept instead of pixel formats:
//!
//! ```toml
//! kind = "audio-filter"
//!
//! [[stage]]
//! id = "mic-denoise"
//! slot = "clean"                        # clean | shape
//! rank = 200
//! device = "cpu"
//! frames = ["host"]                     # host only
//! rates = [48000]                       # samples per second
//! frame_samples = [480]                 # samples per channel per block
//! channels = 1                          # optional, 1 by default
//! ```
//!
//! Each kind's keys and vocabulary are checked: a video stage with `rates`,
//! an audio stage with `formats`, a slot (or a slot requirement) of the
//! other kind, or a stage id against the prefix rule makes the manifest
//! invalid.
//!
//! A module's `conflicts` names modules it cannot be loaded with, and
//! `[requires] modules` modules it cannot be loaded without; the registry
//! enforces both ([`crate::registry`]).
//!
//! [`Manifest::parse`] refuses unknown keys, so a typo makes the module
//! unavailable with the parse error as the reason instead of being ignored.
//! A manifest of another ABI major version parses (so that it can be listed
//! with its reason) but is never loaded ([`Manifest::abi_mismatch`]).
//!
//! Which kinds a host loads is its [`LoadKinds`]: the video daemon loads
//! video stages only ([`LoadKinds::Video`], the default) and lists audio
//! modules as unavailable; the audio worker loads audio filters only
//! ([`LoadKinds::Audio`]).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use obc_backend::StageId;
use obc_module_abi::{OBC_MAX_AUDIO_CHANNELS, OBC_MODULE_ABI_MAJOR};
use serde::Deserialize;

/// The longest module id, in bytes.
pub const MODULE_ID_MAX: usize = 64;

/// The position of a stage that declares none.
pub const DEFAULT_POSITION: i32 = 100;

/// The prefix of every audio stage id (`mic-denoise`, `mic-dereverb`,
/// `mic-enhance`), and of no video stage id: the two kinds share one
/// namespace of stage ids.
pub const AUDIO_STAGE_PREFIX: &str = "mic-";

/// The channels of an audio stage that declares none.
pub const DEFAULT_AUDIO_CHANNELS: u32 = 1;

/// Whether `id` is a valid module id: reverse-DNS,
/// `^[a-z0-9]+(\.[a-z0-9-]+)+$`, at most [`MODULE_ID_MAX`] bytes.
pub fn valid_module_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MODULE_ID_MAX {
        return false;
    }
    let mut segments = id.split('.');
    let first = segments.next().unwrap_or_default();
    if first.is_empty()
        || !first
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return false;
    }
    let mut rest = 0;
    for s in segments {
        rest += 1;
        if s.is_empty()
            || !s
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return false;
        }
    }
    rest > 0
}

/// What a module is. A host loads the kinds of its [`LoadKinds`] and lists
/// the others, unavailable ([`LoadKinds::refusal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleKind {
    /// Stages of the video pipeline (`video-stage`).
    VideoStage,
    /// A microphone filter (`audio-filter`), loaded by the audio worker
    /// (ABI 1.2).
    AudioFilter,
    /// An analyser of the stream, such as captions (`analyser`), reserved.
    Analyser,
}

impl ModuleKind {
    /// The manifest name.
    pub fn as_str(self) -> &'static str {
        match self {
            ModuleKind::VideoStage => "video-stage",
            ModuleKind::AudioFilter => "audio-filter",
            ModuleKind::Analyser => "analyser",
        }
    }

    /// Whether the module's stages are audio stages.
    pub fn is_audio(self) -> bool {
        self == ModuleKind::AudioFilter
    }
}

/// The reason of an audio module in a host of [`LoadKinds::Video`].
pub const REASON_AUDIO_IN_WORKER: &str = "Audio filter modules run in the audio worker";
/// The reason of a video module in a host of [`LoadKinds::Audio`].
pub const REASON_VIDEO_IN_DAEMON: &str = "Video stage modules run in the video daemon";
/// The reason of an analyser module, which no host loads yet.
pub const REASON_ANALYSER: &str = "Analyser modules are not supported yet";

/// Which module kinds a host loads. The video daemon is a
/// [`LoadKinds::Video`] host (the default) and never opens an audio
/// module's library; the audio worker is a [`LoadKinds::Audio`] host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LoadKinds {
    /// `video-stage` modules only.
    #[default]
    Video,
    /// `audio-filter` modules only.
    Audio,
}

impl LoadKinds {
    /// Whether this host loads modules of `kind`.
    pub fn admits(self, kind: ModuleKind) -> bool {
        self.refusal(kind).is_none()
    }

    /// Why this host does not load modules of `kind`, or `None` when it
    /// does.
    pub fn refusal(self, kind: ModuleKind) -> Option<&'static str> {
        match (self, kind) {
            (LoadKinds::Video, ModuleKind::VideoStage)
            | (LoadKinds::Audio, ModuleKind::AudioFilter) => None,
            (LoadKinds::Video, ModuleKind::AudioFilter) => Some(REASON_AUDIO_IN_WORKER),
            (LoadKinds::Audio, ModuleKind::VideoStage) => Some(REASON_VIDEO_IN_DAEMON),
            (_, ModuleKind::Analyser) => Some(REASON_ANALYSER),
        }
    }
}

/// Where a stage sits in the pipeline, in pipeline order: a stage in an
/// earlier slot always runs before one in a later slot. The video slots
/// ([`Slot::ALL`]) and the audio slots ([`Slot::AUDIO`]) are two
/// vocabularies: a stage takes a slot of its module's kind
/// ([`Slot::is_audio`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Slot {
    /// On the camera frame, before the cut-out (`pre-matte`).
    PreMatte,
    /// The cut-out itself (`matte`).
    Matte,
    /// After the cut-out, on the person (`post-matte`).
    PostMatte,
    /// On the composited frame (`post-composite`).
    PostComposite,
    /// Audio: removing what is not the voice, noise and room echo
    /// (`clean`).
    Clean,
    /// Audio: shaping the voice that is left, enhancement or equalisation
    /// (`shape`), after every `clean` stage.
    Shape,
}

impl Slot {
    /// Every video slot, in pipeline order.
    pub const ALL: [Slot; 4] = [
        Slot::PreMatte,
        Slot::Matte,
        Slot::PostMatte,
        Slot::PostComposite,
    ];

    /// Every audio slot, in chain order.
    pub const AUDIO: [Slot; 2] = [Slot::Clean, Slot::Shape];

    /// Whether this is an audio slot.
    pub fn is_audio(self) -> bool {
        matches!(self, Slot::Clean | Slot::Shape)
    }

    /// The manifest name.
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::PreMatte => "pre-matte",
            Slot::Matte => "matte",
            Slot::PostMatte => "post-matte",
            Slot::PostComposite => "post-composite",
            Slot::Clean => "clean",
            Slot::Shape => "shape",
        }
    }
}

/// The device a stage runs on, which the `battery` profile prefers by
/// ([`crate::registry::Profile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Device {
    /// An NVIDIA GPU, through CUDA (`nvidia-gpu`).
    NvidiaGpu,
    /// Any GPU (`gpu`).
    Gpu,
    /// A neural processing unit (`npu`).
    Npu,
    /// The CPU (`cpu`).
    Cpu,
}

impl Device {
    /// The manifest name.
    pub fn as_str(self) -> &'static str {
        match self {
            Device::NvidiaGpu => "nvidia-gpu",
            Device::Gpu => "gpu",
            Device::Npu => "npu",
            Device::Cpu => "cpu",
        }
    }
}

/// The memory a stage takes frames in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Frames {
    /// CUDA device memory (`cuda`).
    Cuda,
    /// Host memory (`host`).
    Host,
    /// A DMA-BUF (`dmabuf`), reserved: no host offers it yet.
    Dmabuf,
}

impl Frames {
    /// The manifest name.
    pub fn as_str(self) -> &'static str {
        match self {
            Frames::Cuda => "cuda",
            Frames::Host => "host",
            Frames::Dmabuf => "dmabuf",
        }
    }
}

/// A pixel format a stage accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Format {
    /// `nv12`.
    Nv12,
    /// `i420`.
    I420,
    /// `bgra`.
    Bgra,
    /// `rgba`.
    Rgba,
    /// `rgb-f32-planar`.
    RgbF32Planar,
}

impl Format {
    /// The manifest name.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Nv12 => "nv12",
            Format::I420 => "i420",
            Format::Bgra => "bgra",
            Format::Rgba => "rgba",
            Format::RgbF32Planar => "rgb-f32-planar",
        }
    }
}

macro_rules! named_enum {
    ($ty:ident, $what:literal, [$($v:ident),+]) => {
        impl FromStr for $ty {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, String> {
                [$($ty::$v),+]
                    .into_iter()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| {
                        let names: Vec<&str> = [$($ty::$v),+].iter().map(|v| v.as_str()).collect();
                        format!(concat!("unknown ", $what, " {:?} (one of {})"), s, names.join(", "))
                    })
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

named_enum!(ModuleKind, "kind", [VideoStage, AudioFilter, Analyser]);
named_enum!(
    Slot,
    "slot",
    [PreMatte, Matte, PostMatte, PostComposite, Clean, Shape]
);
named_enum!(Device, "device", [NvidiaGpu, Gpu, Npu, Cpu]);
named_enum!(Frames, "frame kind", [Cuda, Host, Dmabuf]);
named_enum!(Format, "format", [Nv12, I420, Bgra, Rgba, RgbF32Planar]);

/// What a stage needs before it in the stage order: a stage, or any stage
/// of a slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Requirement {
    /// That stage.
    Stage(StageId),
    /// Any stage of that slot (`matte`: a cut-out).
    Slot(Slot),
}

impl Requirement {
    /// Whether this is the cut-out: the `matte` slot or the `background`
    /// stage, which the phase-1 placement rule ties to the `background`
    /// provider.
    pub fn is_matte(&self) -> bool {
        match self {
            Requirement::Slot(s) => *s == Slot::Matte,
            Requirement::Stage(s) => *s == StageId::BACKGROUND,
        }
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Requirement::Stage(s) => f.write_str(s.as_str()),
            Requirement::Slot(s) => f.write_str(s.as_str()),
        }
    }
}

/// One stage a module provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageDecl {
    /// The stage id.
    pub id: StageId,
    /// Where it sits.
    pub slot: Slot,
    /// Its rank among the providers of the stage: higher is preferred by
    /// the `quality` profile.
    pub rank: u32,
    /// The device it runs on.
    pub device: Device,
    /// Its preferred place within the slot, lower first
    /// ([`DEFAULT_POSITION`] when not given).
    pub position: i32,
    /// What it needs before it in the stage order.
    pub requires: Vec<Requirement>,
    /// The stages it cannot be active with.
    pub conflicts: Vec<StageId>,
    /// The frame kinds it takes, in the module's order of preference.
    pub frames: Vec<Frames>,
    /// The pixel formats it takes; empty for an audio stage.
    pub formats: Vec<Format>,
    /// The block formats of an audio stage; `None` for a video stage.
    pub audio: Option<AudioDecl>,
}

/// The block formats an audio stage takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDecl {
    /// The sample rates, in samples per second, in the module's order of
    /// preference.
    pub rates: Vec<u32>,
    /// The samples per channel per block, in the module's order of
    /// preference.
    pub frame_samples: Vec<u32>,
    /// The channels ([`DEFAULT_AUDIO_CHANNELS`] when not given).
    pub channels: u32,
}

impl StageDecl {
    /// Whether the stage needs the cut-out (a requirement on the `matte`
    /// slot or the `background` stage).
    pub fn needs_matte(&self) -> bool {
        self.requires.iter().any(Requirement::is_matte)
    }
}

/// A parsed and validated manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The manifest file.
    pub path: PathBuf,
    /// The ABI major version the module is built for.
    pub abi: u32,
    /// The module id (reverse-DNS, [`valid_module_id`]).
    pub id: String,
    /// The name shown.
    pub name: String,
    /// The module's version.
    pub version: String,
    /// The module's licence, as text.
    pub licence: String,
    /// What the module is.
    pub kind: ModuleKind,
    /// The library, as an absolute path (resolved against the manifest's
    /// directory when the manifest gives a relative one).
    pub library: PathBuf,
    /// A one-line description, if any.
    pub description: Option<String>,
    /// The hardware the module says it needs (informational: the probe
    /// decides).
    pub hardware: Vec<String>,
    /// The modules it needs enabled (`[requires] modules`), by id, each
    /// once, never itself.
    pub requires_modules: Vec<String>,
    /// The modules it cannot be loaded with (`conflicts`), by id, each
    /// once, never itself.
    pub conflicts: Vec<String>,
    /// The stages, in manifest order.
    pub stages: Vec<StageDecl>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    abi: u32,
    id: String,
    name: String,
    version: String,
    licence: String,
    kind: String,
    library: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    conflicts: Vec<String>,
    #[serde(default)]
    requires: Option<RawRequires>,
    #[serde(default, rename = "stage")]
    stages: Vec<RawStage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequires {
    #[serde(default)]
    hardware: Vec<String>,
    #[serde(default)]
    modules: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStage {
    id: String,
    slot: String,
    rank: u32,
    device: String,
    #[serde(default)]
    position: Option<i32>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    conflicts: Vec<String>,
    frames: Vec<String>,
    #[serde(default)]
    formats: Option<Vec<String>>,
    #[serde(default)]
    rates: Option<Vec<u32>>,
    #[serde(default)]
    frame_samples: Option<Vec<u32>>,
    #[serde(default)]
    channels: Option<u32>,
}

/// The module ids of a module-level list (`conflicts`, `[requires]
/// modules`) of module `own`: each a valid module id, never `own`, each
/// once. `what` words the error (`conflicts with`).
fn module_list(own: &str, what: &str, raw: Vec<String>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for id in raw {
        if !valid_module_id(&id) {
            return Err(format!(
                "the module {what} {id:?}, which is not a module id"
            ));
        }
        if id == own {
            return Err(format!("the module {what} itself"));
        }
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

fn non_empty(what: &str, v: String) -> Result<String, String> {
    if v.trim().is_empty() {
        Err(format!("{what} is empty"))
    } else {
        Ok(v)
    }
}

fn parse_list<T: FromStr<Err = String> + PartialEq>(
    stage: &str,
    what: &str,
    raw: &[String],
) -> Result<Vec<T>, String> {
    if raw.is_empty() {
        return Err(format!("stage {stage}: {what} is empty"));
    }
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let v: T = r.parse().map_err(|e| format!("stage {stage}: {e}"))?;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    Ok(out)
}

/// Whether `id` follows the prefix rule of `kind`: an audio stage id starts
/// with [`AUDIO_STAGE_PREFIX`], a video one does not.
fn prefix_error(kind: ModuleKind, id: &str, what: &str) -> Option<String> {
    let audio = id.starts_with(AUDIO_STAGE_PREFIX);
    match (kind.is_audio(), audio) {
        (true, false) => Some(format!(
            "{what} {id} of an audio-filter module must start with {AUDIO_STAGE_PREFIX:?}"
        )),
        (false, true) => Some(format!(
            "{what} {id} of {} module must not start with {AUDIO_STAGE_PREFIX:?}, which is \
             for audio stages",
            a_kind(kind)
        )),
        _ => None,
    }
}

/// A list of positive numbers, deduplicated in order.
fn positive_list(stage: &StageId, what: &str, raw: Vec<u32>) -> Result<Vec<u32>, String> {
    if raw.is_empty() {
        return Err(format!("stage {stage}: {what} is empty"));
    }
    let mut out = Vec::with_capacity(raw.len());
    for v in raw {
        if v == 0 {
            return Err(format!("stage {stage}: {what} has a 0"));
        }
        if !out.contains(&v) {
            out.push(v);
        }
    }
    Ok(out)
}

impl RawStage {
    fn validate(self, kind: ModuleKind) -> Result<StageDecl, String> {
        let id =
            StageId::new(self.id.clone()).map_err(|e| format!("stage id {:?}: {e}", self.id))?;
        if let Some(e) = prefix_error(kind, id.as_str(), "the stage id") {
            return Err(e);
        }
        let slot: Slot = self.slot.parse().map_err(|e| format!("stage {id}: {e}"))?;
        if slot.is_audio() != kind.is_audio() {
            return Err(format!(
                "stage {id}: the slot {slot} is not a slot of {} module (one of {})",
                a_kind(kind),
                slot_names(kind)
            ));
        }
        let device: Device = self
            .device
            .parse()
            .map_err(|e| format!("stage {id}: {e}"))?;
        let mut requires = Vec::new();
        for r in &self.requires {
            let req = match r.parse::<Slot>() {
                Ok(slot) if slot.is_audio() != kind.is_audio() => {
                    return Err(format!(
                        "stage {id}: requires the slot {slot}, which is not a slot of {} module",
                        a_kind(kind)
                    ));
                }
                Ok(slot) => Requirement::Slot(slot),
                Err(_) => Requirement::Stage(StageId::new(r.clone()).map_err(|_| {
                    format!("stage {id}: requires {r:?}, which is neither a stage id nor a slot")
                })?),
            };
            if let Requirement::Stage(s) = &req
                && let Some(e) =
                    prefix_error(kind, s.as_str(), &format!("stage {id}: the required stage"))
            {
                return Err(e);
            }
            if req == Requirement::Stage(id.clone()) {
                return Err(format!("stage {id}: requires itself"));
            }
            if !requires.contains(&req) {
                requires.push(req);
            }
        }
        let mut conflicts = Vec::new();
        for c in &self.conflicts {
            let c = StageId::new(c.clone()).map_err(|_| {
                format!("stage {id}: conflicts with {c:?}, which is not a stage id")
            })?;
            if c == id {
                return Err(format!("stage {id}: conflicts with itself"));
            }
            if let Some(e) = prefix_error(
                kind,
                c.as_str(),
                &format!("stage {id}: the conflicting stage"),
            ) {
                return Err(e);
            }
            if !conflicts.contains(&c) {
                conflicts.push(c);
            }
        }
        let frames = parse_list(id.as_str(), "frames", &self.frames)?;
        let (formats, audio) = if kind.is_audio() {
            if self.formats.is_some() {
                return Err(format!(
                    "stage {id}: formats is a key of video stages; an audio stage gives rates, \
                     frame_samples and channels"
                ));
            }
            if frames != [Frames::Host] {
                return Err(format!("stage {id}: an audio stage takes host frames only"));
            }
            let rates = self
                .rates
                .ok_or_else(|| format!("stage {id}: an audio stage needs rates"))?;
            let frame_samples = self
                .frame_samples
                .ok_or_else(|| format!("stage {id}: an audio stage needs frame_samples"))?;
            let channels = self.channels.unwrap_or(DEFAULT_AUDIO_CHANNELS);
            if channels == 0 || channels as usize > OBC_MAX_AUDIO_CHANNELS {
                return Err(format!(
                    "stage {id}: channels {channels} is not 1 to {OBC_MAX_AUDIO_CHANNELS}"
                ));
            }
            let audio = AudioDecl {
                rates: positive_list(&id, "rates", rates)?,
                frame_samples: positive_list(&id, "frame_samples", frame_samples)?,
                channels,
            };
            (Vec::new(), Some(audio))
        } else {
            for (key, given) in [
                ("rates", self.rates.is_some()),
                ("frame_samples", self.frame_samples.is_some()),
                ("channels", self.channels.is_some()),
            ] {
                if given {
                    return Err(format!(
                        "stage {id}: {key} is a key of audio stages, not of {} module",
                        a_kind(kind)
                    ));
                }
            }
            let formats = self
                .formats
                .ok_or_else(|| format!("stage {id}: a video stage needs formats"))?;
            (parse_list(id.as_str(), "formats", &formats)?, None)
        };
        Ok(StageDecl {
            id,
            slot,
            rank: self.rank,
            device,
            position: self.position.unwrap_or(DEFAULT_POSITION),
            requires,
            conflicts,
            frames,
            formats,
            audio,
        })
    }
}

/// `a video-stage`, `an audio-filter`, `an analyser`: the kind with its
/// article, for a reason.
fn a_kind(kind: ModuleKind) -> String {
    match kind {
        ModuleKind::VideoStage => format!("a {kind}"),
        ModuleKind::AudioFilter | ModuleKind::Analyser => format!("an {kind}"),
    }
}

/// The slot names of `kind`, for a reason.
fn slot_names(kind: ModuleKind) -> String {
    let slots: &[Slot] = if kind.is_audio() {
        &Slot::AUDIO
    } else {
        &Slot::ALL
    };
    slots
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl Manifest {
    /// Reads and validates the manifest at `path` ([`Manifest::parse`]).
    pub fn load(path: &Path) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("the manifest {} could not be read: {e}", path.display()))?;
        Manifest::parse(&text, path)
    }

    /// Parses and validates the text of the manifest at `path`; `path`
    /// resolves a relative `library`. The error is the reason shown for the
    /// module.
    pub fn parse(text: &str, path: &Path) -> Result<Manifest, String> {
        let raw: RawManifest = toml::from_str(text)
            .map_err(|e| format!("the manifest is malformed: {}", e.message()))?;
        Manifest::validate(raw, path).map_err(|e| format!("the manifest is invalid: {e}"))
    }

    fn validate(raw: RawManifest, path: &Path) -> Result<Manifest, String> {
        if !valid_module_id(&raw.id) {
            return Err(format!(
                "the id {:?} is not a reverse-DNS module id (lower-case letters, digits and \
                 hyphens, at least two segments, at most {MODULE_ID_MAX} bytes)",
                raw.id
            ));
        }
        let kind: ModuleKind = raw.kind.parse()?;
        let library = non_empty("library", raw.library)?;
        let library = PathBuf::from(library);
        let library = if library.is_absolute() {
            library
        } else {
            path.parent().unwrap_or(Path::new(".")).join(library)
        };
        let mut stages: Vec<StageDecl> = Vec::with_capacity(raw.stages.len());
        for s in raw.stages {
            let s = s.validate(kind)?;
            if stages.iter().any(|t| t.id == s.id) {
                return Err(format!("the stage {} is declared twice", s.id));
            }
            stages.push(s);
        }
        if kind != ModuleKind::Analyser && stages.is_empty() {
            return Err(format!("{} module declares no stage", a_kind(kind)));
        }
        let (hardware, requires_modules) = raw
            .requires
            .map(|r| (r.hardware, r.modules))
            .unwrap_or_default();
        let requires_modules = module_list(&raw.id, "requires the module", requires_modules)?;
        let conflicts = module_list(&raw.id, "conflicts with", raw.conflicts)?;
        Ok(Manifest {
            path: path.to_path_buf(),
            abi: raw.abi,
            id: raw.id,
            name: non_empty("name", raw.name)?,
            version: non_empty("version", raw.version)?,
            licence: non_empty("licence", raw.licence)?,
            kind,
            library,
            description: raw.description.filter(|d| !d.trim().is_empty()),
            hardware,
            requires_modules,
            conflicts,
            stages,
        })
    }

    /// The declaration of stage `id`, if the module provides it.
    pub fn stage(&self, id: &str) -> Option<&StageDecl> {
        self.stages.iter().find(|s| s.id == id)
    }

    /// Why the module is not loaded because of its ABI major version, or
    /// `None` when it is this host's.
    pub fn abi_mismatch(&self) -> Option<String> {
        (self.abi != OBC_MODULE_ABI_MAJOR).then(|| {
            format!(
                "Built for module ABI {}; this daemon supports module ABI {OBC_MODULE_ABI_MAJOR}",
                self.abi
            )
        })
    }

    /// Why the module is not loaded at all by the video daemon (another ABI
    /// major version, or a kind it does not load: everything but
    /// `video-stage`), or `None` when it can be: the reason for the
    /// default [`LoadKinds::Video`] ([`Manifest::unloadable_reason_for`]).
    pub fn unloadable_reason(&self) -> Option<String> {
        self.unloadable_reason_for(LoadKinds::Video)
    }

    /// Why a host of `kinds` does not load the module at all (another ABI
    /// major version, or a kind it does not load), or `None` when it can.
    pub fn unloadable_reason_for(&self, kinds: LoadKinds) -> Option<String> {
        self.abi_mismatch()
            .or_else(|| kinds.refusal(self.kind).map(str::to_string))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = r#"
abi = 1
id = "org.example.hello"
name = "Hello tint"
version = "0.1.0"
licence = "MIT OR Apache-2.0"
kind = "video-stage"
library = "lib/libhello.so"
description = "A tint"

[requires]
hardware = ["any"]

[[stage]]
id = "hello"
slot = "post-composite"
rank = 100
device = "cpu"
frames = ["host"]
formats = ["nv12"]

[[stage]]
id = "relight"
slot = "post-matte"
rank = 300
device = "nvidia-gpu"
position = 20
requires = ["matte", "denoise"]
conflicts = ["hello2"]
frames = ["cuda", "host", "cuda"]
formats = ["nv12", "i420"]
"#;

    fn parse(text: &str) -> Result<Manifest, String> {
        Manifest::parse(text, Path::new("/etc/modules.d/hello.toml"))
    }

    #[test]
    fn a_full_manifest_parses() {
        let m = parse(HELLO).unwrap();
        assert_eq!(m.id, "org.example.hello");
        assert_eq!(m.kind, ModuleKind::VideoStage);
        assert_eq!(m.library, Path::new("/etc/modules.d/lib/libhello.so"));
        assert_eq!(m.description.as_deref(), Some("A tint"));
        assert_eq!(m.hardware, ["any"]);
        assert_eq!(m.stages.len(), 2);
        let hello = m.stage("hello").unwrap();
        assert_eq!(
            (hello.slot, hello.rank, hello.device, hello.position),
            (Slot::PostComposite, 100, Device::Cpu, DEFAULT_POSITION)
        );
        assert!(!hello.needs_matte());
        let relight = m.stage("relight").unwrap();
        assert_eq!(relight.position, 20);
        assert_eq!(
            relight.requires,
            [
                Requirement::Slot(Slot::Matte),
                Requirement::Stage(StageId::DENOISE)
            ]
        );
        assert!(relight.needs_matte());
        assert_eq!(relight.conflicts, [StageId::new("hello2").unwrap()]);
        assert_eq!(relight.frames, [Frames::Cuda, Frames::Host], "deduplicated");
        assert_eq!(relight.formats, [Format::Nv12, Format::I420]);
        assert_eq!(m.abi_mismatch(), None);
        assert_eq!(m.unloadable_reason(), None);
    }

    #[test]
    fn an_absolute_library_stays() {
        let text = HELLO.replace("lib/libhello.so", "/usr/lib/x/libhello.so");
        assert_eq!(
            parse(&text).unwrap().library,
            Path::new("/usr/lib/x/libhello.so")
        );
    }

    #[test]
    fn unknown_keys_are_refused() {
        let e = parse(&HELLO.replace("description =", "descripton =")).unwrap_err();
        assert!(e.contains("malformed") && e.contains("descripton"), "{e}");
        let e = parse(&HELLO.replace("rank = 100", "rank = 100\nweight = 3")).unwrap_err();
        assert!(e.contains("weight"), "{e}");
        let e = parse(&HELLO.replace("hardware =", "gpu =")).unwrap_err();
        assert!(e.contains("gpu"), "{e}");
    }

    #[test]
    fn ids_are_reverse_dns() {
        for ok in [
            "org.example.hello",
            "br.com.dias.maxine-vfx",
            "a.b",
            "x1.y-2",
        ] {
            assert!(valid_module_id(ok), "{ok}");
        }
        for bad in [
            "",
            "hello",
            "Org.example",
            "org..x",
            "org.",
            ".org",
            "org-x.y",
            "org.ex_ample",
            &format!("a.{}", "b".repeat(63)),
        ] {
            assert!(!valid_module_id(bad), "{bad}");
        }
        let e = parse(&HELLO.replace("org.example.hello", "Hello")).unwrap_err();
        assert!(e.contains("reverse-DNS"), "{e}");
    }

    #[test]
    fn stage_fields_are_validated() {
        let cases = [
            (r#"id = "hello""#, r#"id = "Hello""#, "stage id"),
            (
                r#"slot = "post-composite""#,
                r#"slot = "late""#,
                "unknown slot",
            ),
            (r#"device = "cpu""#, r#"device = "fpga""#, "unknown device"),
            (
                r#"frames = ["host"]"#,
                r#"frames = ["vulkan"]"#,
                "unknown frame kind",
            ),
            (r#"frames = ["host"]"#, "frames = []", "frames is empty"),
            (
                r#"formats = ["nv12"]"#,
                r#"formats = ["yuyv"]"#,
                "unknown format",
            ),
            (
                r#"requires = ["matte", "denoise"]"#,
                r#"requires = ["Matte"]"#,
                "neither a stage id nor a slot",
            ),
            (
                r#"requires = ["matte", "denoise"]"#,
                r#"requires = ["relight"]"#,
                "requires itself",
            ),
            (
                r#"conflicts = ["hello2"]"#,
                r#"conflicts = ["relight"]"#,
                "conflicts with itself",
            ),
            (
                r#"kind = "video-stage""#,
                r#"kind = "audio""#,
                "unknown kind",
            ),
            (r#"name = "Hello tint""#, r#"name = " ""#, "name is empty"),
            (
                r#"library = "lib/libhello.so""#,
                r#"library = """#,
                "library is empty",
            ),
        ];
        for (from, to, want) in cases {
            assert!(HELLO.contains(from), "{from}");
            let e = parse(&HELLO.replacen(from, to, 1)).unwrap_err();
            assert!(e.contains(want), "{to}: {e}");
        }
        let twice = HELLO.replace(r#"id = "relight""#, r#"id = "hello""#);
        assert!(parse(&twice).unwrap_err().contains("declared twice"));
        let missing = HELLO.replace("rank = 100\n", "");
        assert!(parse(&missing).unwrap_err().contains("rank"));
    }

    #[test]
    fn module_conflicts_and_required_modules_parse_and_are_checked() {
        let m = parse(HELLO).unwrap();
        assert!(m.conflicts.is_empty());
        assert!(m.requires_modules.is_empty());
        let text = HELLO
            .replace(
                "description = \"A tint\"",
                "description = \"A tint\"\nconflicts = [\"org.x.b\", \"org.x.a\", \"org.x.b\"]",
            )
            .replace(
                "hardware = [\"any\"]",
                "hardware = [\"any\"]\nmodules = [\"org.x.base\", \"org.x.base\"]",
            );
        let m = parse(&text).unwrap();
        assert_eq!(
            m.conflicts,
            ["org.x.b", "org.x.a"],
            "deduplicated, in order"
        );
        assert_eq!(m.requires_modules, ["org.x.base"]);
        assert_eq!(m.hardware, ["any"]);
        // `[requires] modules` alone, without `hardware`.
        let m =
            parse(&HELLO.replace("hardware = [\"any\"]", "modules = [\"org.x.base\"]")).unwrap();
        assert!(m.hardware.is_empty());
        assert_eq!(m.requires_modules, ["org.x.base"]);
        let bad = |from: &str, to: &str| parse(&HELLO.replace(from, to)).unwrap_err();
        let desc = "description = \"A tint\"";
        let e = bad(desc, "description = \"A tint\"\nconflicts = [\"Other\"]");
        assert!(
            e.contains("invalid") && e.contains("not a module id"),
            "{e}"
        );
        let e = bad(
            desc,
            "description = \"A tint\"\nconflicts = [\"org.example.hello\"]",
        );
        assert!(e.contains("conflicts with itself"), "{e}");
        let e = bad(desc, "description = \"A tint\"\nconflicts = \"org.x.a\"");
        assert!(e.contains("malformed"), "{e}");
        let hw = "hardware = [\"any\"]";
        let e = bad(hw, "modules = [\"hello\"]");
        assert!(e.contains("not a module id"), "{e}");
        let e = bad(hw, "modules = [\"org.example.hello\"]");
        assert!(e.contains("requires the module itself"), "{e}");
    }

    const AUDIO: &str = r#"
abi = 1
id = "org.example.voice"
name = "Voice"
version = "0.1.0"
licence = "MIT"
kind = "audio-filter"
library = "libvoice.so"

[[stage]]
id = "mic-denoise"
slot = "clean"
rank = 200
device = "cpu"
frames = ["host"]
rates = [48000, 16000, 48000]
frame_samples = [480, 960]

[[stage]]
id = "mic-enhance"
slot = "shape"
rank = 100
device = "nvidia-gpu"
requires = ["clean", "mic-denoise"]
conflicts = ["mic-dereverb"]
frames = ["host"]
rates = [48000]
frame_samples = [480]
channels = 2
"#;

    #[test]
    fn a_video_or_audio_module_needs_a_stage_but_an_analyser_does_not() {
        let head: String = HELLO.split("[[stage]]").next().unwrap().to_string();
        assert!(parse(&head).unwrap_err().contains("declares no stage"));
        let e = parse(&head.replace("video-stage", "audio-filter")).unwrap_err();
        assert!(
            e.contains("an audio-filter module declares no stage"),
            "{e}"
        );
        let analyser = parse(&head.replace("video-stage", "analyser")).unwrap();
        assert_eq!(
            analyser.unloadable_reason().as_deref(),
            Some(REASON_ANALYSER)
        );
        assert_eq!(
            analyser.unloadable_reason_for(LoadKinds::Audio).as_deref(),
            Some(REASON_ANALYSER)
        );
    }

    #[test]
    fn an_audio_manifest_parses_with_its_block_formats() {
        let m = parse(AUDIO).unwrap();
        assert_eq!(m.kind, ModuleKind::AudioFilter);
        let d = m.stage("mic-denoise").unwrap();
        assert_eq!((d.slot, d.rank, d.device), (Slot::Clean, 200, Device::Cpu));
        assert_eq!(d.frames, [Frames::Host]);
        assert!(d.formats.is_empty());
        assert_eq!(
            d.audio,
            Some(AudioDecl {
                rates: vec![48000, 16000],
                frame_samples: vec![480, 960],
                channels: DEFAULT_AUDIO_CHANNELS,
            }),
            "deduplicated, one channel by default"
        );
        let e = m.stage("mic-enhance").unwrap();
        assert_eq!(e.slot, Slot::Shape);
        assert_eq!(
            e.requires,
            [
                Requirement::Slot(Slot::Clean),
                Requirement::Stage(StageId::new("mic-denoise").unwrap())
            ]
        );
        assert_eq!(e.audio.as_ref().unwrap().channels, 2);
        assert!(
            parse(HELLO)
                .unwrap()
                .stages
                .iter()
                .all(|s| s.audio.is_none())
        );
        assert!(Slot::Clean < Slot::Shape);
    }

    #[test]
    fn each_kind_is_loaded_by_its_own_host() {
        let audio = parse(AUDIO).unwrap();
        assert_eq!(
            audio.unloadable_reason().as_deref(),
            Some(REASON_AUDIO_IN_WORKER),
            "the video daemon never loads an audio module"
        );
        assert_eq!(audio.unloadable_reason_for(LoadKinds::Audio), None);
        let video = parse(HELLO).unwrap();
        assert_eq!(video.unloadable_reason_for(LoadKinds::Video), None);
        assert_eq!(
            video.unloadable_reason_for(LoadKinds::Audio).as_deref(),
            Some(REASON_VIDEO_IN_DAEMON)
        );
        assert_eq!(LoadKinds::default(), LoadKinds::Video);
        assert!(LoadKinds::Audio.admits(ModuleKind::AudioFilter));
        assert!(!LoadKinds::Audio.admits(ModuleKind::VideoStage));
        let other = parse(&AUDIO.replace("abi = 1", "abi = 2")).unwrap();
        assert!(
            other
                .unloadable_reason_for(LoadKinds::Audio)
                .unwrap()
                .contains("ABI 2")
        );
    }

    #[test]
    fn audio_stages_are_validated_per_kind() {
        let cases = [
            // The prefix rule, both ways.
            (
                r#"id = "mic-denoise""#,
                r#"id = "denoise""#,
                "must start with \"mic-\"",
            ),
            // An audio stage in a video slot.
            (
                r#"slot = "clean""#,
                r#"slot = "pre-matte""#,
                "not a slot of an audio-filter",
            ),
            (r#"slot = "clean""#, r#"slot = "loud""#, "unknown slot"),
            (
                r#"requires = ["clean", "mic-denoise"]"#,
                r#"requires = ["matte"]"#,
                "requires the slot matte",
            ),
            (
                r#"requires = ["clean", "mic-denoise"]"#,
                r#"requires = ["denoise"]"#,
                "the required stage denoise",
            ),
            (
                r#"conflicts = ["mic-dereverb"]"#,
                r#"conflicts = ["relight"]"#,
                "the conflicting stage relight",
            ),
            (
                "frame_samples = [480, 960]",
                "frame_samples = [480, 960]\nformats = [\"nv12\"]",
                "formats is a key of video stages",
            ),
            (r#"rates = [48000, 16000, 48000]"#, "", "needs rates"),
            ("frame_samples = [480, 960]", "", "needs frame_samples"),
            (
                r#"rates = [48000, 16000, 48000]"#,
                "rates = []",
                "rates is empty",
            ),
            (
                "frame_samples = [480, 960]",
                "frame_samples = [0]",
                "frame_samples has a 0",
            ),
            ("channels = 2", "channels = 9", "channels 9 is not 1 to 8"),
            ("channels = 2", "channels = 0", "channels 0"),
        ];
        for (from, to, want) in cases {
            assert!(AUDIO.contains(from), "{from}");
            let e = parse(&AUDIO.replacen(from, to, 1)).unwrap_err();
            assert!(e.contains(want), "{to}: {e}");
        }
        let cuda = AUDIO.replacen(r#"frames = ["host"]"#, r#"frames = ["cuda"]"#, 1);
        assert!(parse(&cuda).unwrap_err().contains("host frames only"));
    }

    #[test]
    fn video_stages_refuse_the_audio_vocabulary() {
        let cases = [
            (
                r#"id = "hello""#,
                r#"id = "mic-x""#,
                "must not start with \"mic-\"",
            ),
            (
                r#"slot = "post-composite""#,
                r#"slot = "shape""#,
                "not a slot of a video-stage",
            ),
            (
                r#"requires = ["matte", "denoise"]"#,
                r#"requires = ["clean"]"#,
                "requires the slot clean",
            ),
            (
                r#"requires = ["matte", "denoise"]"#,
                r#"requires = ["mic-denoise"]"#,
                "the required stage mic-denoise",
            ),
            (
                r#"formats = ["nv12"]"#,
                "formats = [\"nv12\"]\nrates = [48000]",
                "rates is a key of audio stages",
            ),
            (
                r#"formats = ["nv12"]"#,
                "formats = [\"nv12\"]\nchannels = 1",
                "channels is a key of audio stages",
            ),
            (r#"formats = ["nv12"]"#, "", "needs formats"),
        ];
        for (from, to, want) in cases {
            assert!(HELLO.contains(from), "{from}");
            let e = parse(&HELLO.replacen(from, to, 1)).unwrap_err();
            assert!(e.contains(want), "{to}: {e}");
        }
    }

    #[test]
    fn another_abi_major_parses_but_is_not_loadable() {
        let m = parse(&HELLO.replace("abi = 1", "abi = 2")).unwrap();
        let reason = m.abi_mismatch().unwrap();
        assert!(
            reason.contains("ABI 2") && reason.contains("ABI 1"),
            "{reason}"
        );
        assert_eq!(m.unloadable_reason(), Some(reason));
    }

    #[test]
    fn slots_are_in_pipeline_order_and_names_round_trip() {
        assert!(Slot::PreMatte < Slot::Matte);
        assert!(Slot::Matte < Slot::PostMatte);
        assert!(Slot::PostMatte < Slot::PostComposite);
        for s in Slot::ALL.into_iter().chain(Slot::AUDIO) {
            assert_eq!(s.as_str().parse::<Slot>().unwrap(), s);
        }
        assert!(Slot::AUDIO.iter().all(|s| s.is_audio()));
        assert!(!Slot::ALL.iter().any(|s| s.is_audio()));
        for d in [Device::NvidiaGpu, Device::Gpu, Device::Npu, Device::Cpu] {
            assert_eq!(d.to_string().parse::<Device>().unwrap(), d);
        }
        assert_eq!("rgb-f32-planar".parse::<Format>(), Ok(Format::RgbF32Planar));
        assert_eq!("dmabuf".parse::<Frames>(), Ok(Frames::Dmabuf));
    }
}
