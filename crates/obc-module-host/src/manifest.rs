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
//!
//! [requires]                            # optional, informational
//! hardware = ["nvidia-turing+"]
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
//! [`Manifest::parse`] refuses unknown keys, so a typo makes the module
//! unavailable with the parse error as the reason instead of being ignored.
//! A manifest of another ABI major version parses (so that it can be listed
//! with its reason) but is never loaded ([`Manifest::abi_mismatch`]).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use obc_backend::StageId;
use obc_module_abi::OBC_MODULE_ABI_MAJOR;
use serde::Deserialize;

/// The longest module id, in bytes.
pub const MODULE_ID_MAX: usize = 64;

/// The position of a stage that declares none.
pub const DEFAULT_POSITION: i32 = 100;

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

/// What a module is. Only [`ModuleKind::VideoStage`] modules are loaded;
/// the others are listed, unavailable ([`ModuleKind::unsupported_reason`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleKind {
    /// Stages of the video pipeline (`video-stage`).
    VideoStage,
    /// A microphone filter (`audio-filter`), reserved.
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

    /// Why a module of this kind is not loaded, or `None` for a kind that
    /// is.
    pub fn unsupported_reason(self) -> Option<&'static str> {
        match self {
            ModuleKind::VideoStage => None,
            ModuleKind::AudioFilter => Some("Audio filter modules are not supported yet"),
            ModuleKind::Analyser => Some("Analyser modules are not supported yet"),
        }
    }
}

/// Where a stage sits in the pipeline, in pipeline order: a stage in an
/// earlier slot always runs before one in a later slot.
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
}

impl Slot {
    /// Every slot, in pipeline order.
    pub const ALL: [Slot; 4] = [
        Slot::PreMatte,
        Slot::Matte,
        Slot::PostMatte,
        Slot::PostComposite,
    ];

    /// The manifest name.
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::PreMatte => "pre-matte",
            Slot::Matte => "matte",
            Slot::PostMatte => "post-matte",
            Slot::PostComposite => "post-composite",
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
named_enum!(Slot, "slot", [PreMatte, Matte, PostMatte, PostComposite]);
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
    /// The pixel formats it takes.
    pub formats: Vec<Format>,
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
    requires: Option<RawRequires>,
    #[serde(default, rename = "stage")]
    stages: Vec<RawStage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequires {
    #[serde(default)]
    hardware: Vec<String>,
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
    formats: Vec<String>,
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

impl RawStage {
    fn validate(self) -> Result<StageDecl, String> {
        let id =
            StageId::new(self.id.clone()).map_err(|e| format!("stage id {:?}: {e}", self.id))?;
        let slot: Slot = self.slot.parse().map_err(|e| format!("stage {id}: {e}"))?;
        let device: Device = self
            .device
            .parse()
            .map_err(|e| format!("stage {id}: {e}"))?;
        let mut requires = Vec::new();
        for r in &self.requires {
            let req = match r.parse::<Slot>() {
                Ok(slot) => Requirement::Slot(slot),
                Err(_) => Requirement::Stage(StageId::new(r.clone()).map_err(|_| {
                    format!("stage {id}: requires {r:?}, which is neither a stage id nor a slot")
                })?),
            };
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
            if !conflicts.contains(&c) {
                conflicts.push(c);
            }
        }
        let frames = parse_list(id.as_str(), "frames", &self.frames)?;
        let formats = parse_list(id.as_str(), "formats", &self.formats)?;
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
        })
    }
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
            let s = s.validate()?;
            if stages.iter().any(|t| t.id == s.id) {
                return Err(format!("the stage {} is declared twice", s.id));
            }
            stages.push(s);
        }
        if kind == ModuleKind::VideoStage && stages.is_empty() {
            return Err("a video-stage module declares no stage".into());
        }
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
            hardware: raw.requires.map(|r| r.hardware).unwrap_or_default(),
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

    /// Why the module is not loaded at all (another ABI major version, or a
    /// kind that is not supported yet), or `None` when it can be.
    pub fn unloadable_reason(&self) -> Option<String> {
        self.abi_mismatch()
            .or_else(|| self.kind.unsupported_reason().map(str::to_string))
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
    fn a_video_module_needs_a_stage_but_a_reserved_kind_does_not() {
        let head: String = HELLO.split("[[stage]]").next().unwrap().to_string();
        assert!(parse(&head).unwrap_err().contains("declares no stage"));
        let audio = parse(&head.replace("video-stage", "audio-filter")).unwrap();
        assert_eq!(audio.kind, ModuleKind::AudioFilter);
        assert_eq!(
            audio.unloadable_reason().as_deref(),
            Some("Audio filter modules are not supported yet")
        );
        let analyser = parse(&head.replace("video-stage", "analyser")).unwrap();
        assert!(
            analyser
                .unloadable_reason()
                .unwrap()
                .contains("not supported yet")
        );
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
        for s in Slot::ALL {
            assert_eq!(s.as_str().parse::<Slot>().unwrap(), s);
        }
        for d in [Device::NvidiaGpu, Device::Gpu, Device::Npu, Device::Cpu] {
            assert_eq!(d.to_string().parse::<Device>().unwrap(), d);
        }
        assert_eq!("rgb-f32-planar".parse::<Format>(), Ok(Format::RgbF32Planar));
        assert_eq!("dmabuf".parse::<Frames>(), Ok(Frames::Dmabuf));
    }
}
