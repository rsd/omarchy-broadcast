//! NVIDIA Maxine AR effects as an omarchy-broadcast module.
//!
//! The library this crate builds (`libobc_module_maxine_ar.so`, with the
//! manifest `maxine-ar.toml`) runs features of the user's NVIDIA Maxine AR
//! SDK on the camera's CUDA frames. The SDK is opened at run time, when a
//! chain is loaded (or by the deeper check): the library loads on a machine
//! with no SDK and no GPU.
//!
//! # Stages
//!
//! | Stage | Slot | Runs |
//! |---|---|---|
//! | `eye-contact` | pre-matte (position 10) | the `GazeRedirection` feature: the eyes of the largest face, redirected towards the camera ([`gaze`]) |
//! | `auto-frame` | pre-matte (position 20) | a digital pan and zoom that keeps the largest face framed: a crop of the frame (at most [`framing::MAX_ZOOM`]), scaled back to the frame's size ([`framing`], `autoframe`) |
//!
//! The two are available, and fail, independently: auto-frame finds the
//! face with the `FaceBoxDetection` feature, or takes the face eye contact
//! tracked on the same frame when that stage runs.
//!
//! # Configuration
//!
//! The module's configuration table (`[module."br.com.dias.maxine-ar"]`):
//!
//! * `sdk_root`: the SDK root. `MAXINE_AR_ROOT` takes precedence when set
//!   and not empty; with neither, [`DEFAULT_ROOT`] when that directory
//!   exists ([`resolve_root`]). The SDK is loaded once per process, from
//!   the first root used.
//! * `_host.compute_capability`: `[major, minor]` of the GPU, reserved for
//!   the daemon, so the probe looks for the models of that architecture;
//!   absent or null, models of any architecture count.
//!
//! Any other key is refused.
//!
//! # Probe and check
//!
//! The probe reads the SDK tree and nothing else (no `dlopen`, no CUDA), so
//! it never wakes a suspended GPU ([`probe_root`]). The deeper check opens
//! the SDK's libraries by absolute path, with the libraries they need
//! (`preload`), and resolves the entry points, never creating a CUDA
//! context.
//!
//! # A chain
//!
//! A chain runs on CUDA NV12 frames of one size, in place, on the host's
//! context and stream; the colour description of the request (BT.601,
//! BT.709 or BT.2020, the range and the chroma siting) is the conversion's.
//! The stages run in the order of the chain request (the host's; the
//! default positions put eye contact first). Per frame:
//!
//! 1. NV12 to BGR (`NvCVImage_TransferFromYUV`) into the camera image of
//!    the chain (`image::Frames`), which the AR features read;
//! 2. the stages, in order (`plan`):
//!    * eye contact: the feature reads the camera image and writes its
//!      result image (the camera image as it is when it finds no face);
//!      after an auto-frame that framed the picture, the framed image is
//!      copied into the camera image first, so it runs on the framed
//!      picture;
//!    * auto-frame: the face (eye contact's when that ran before it on the
//!      frame, else its own face-box feature's on the camera image) moves
//!      the crop, time-based: from the frames' presentation times when the
//!      host gives them, else from a monotonic clock (the host gives none
//!      today); unless the crop is the full frame, NPP scales the crop of
//!      the image so far (the eye-contact result, else the camera image)
//!      into the auto-frame image, on the host's stream;
//! 3. BGR to NV12 into the output frame (`NvCVImage_TransferToYUV`), from
//!    the last image written; when no stage changed the picture
//!    (auto-frame alone, at the full frame), the frame is left as it is
//!    (copied, when the call is not in place) instead.
//!
//! A repeated frame (the output clock's, [`obc_module_sdk::Frame::is_repeat`])
//! runs step 3 alone, from the result of the last fresh frame: the crop
//! does not move. The framing (`framing`) is a per-frame parameter. The models
//! are loaded by the chain's load only. Any failure of the SDK at load or on
//! a frame is a failure of the stage alone ([`Error::stage`]): it fails
//! open, and the camera image goes on without it.
//!
//! A temporal reset (a new camera) keeps the stages: the features'
//! trackers lose the previous face and find the new one by themselves, and
//! the crop snaps back to the full frame.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use obc_backend::DeviceNv12;
use obc_module_sdk::{
    Apply, ChainRequest, ChromaSiting, ColourMatrix, Error, Frame, FrameKind, Group, Instance,
    LoadReport, Module, Op, Param, PixelFormat, ProbeReport, Role, Setting, SettingsDescription,
    log, serde_json,
};
use serde::Deserialize;

mod autoframe;
mod ffi;
pub mod framing;
pub mod gaze;
pub mod image;
mod npp;
mod plan;
mod preload;

pub use ffi::NvArRect;

use autoframe::{AutoFrame, Source};
use framing::Framing;
use gaze::{EyeContact, GazeConfig};
use image::Frames;
use plan::{FrameStages, Ran, Step};

/// The module id.
pub const MODULE_ID: &str = "br.com.dias.maxine-ar";

/// The stage id of eye contact.
pub const EYE_CONTACT: &str = "eye-contact";
/// The stage id of the automatic framing.
pub const AUTO_FRAME: &str = "auto-frame";

/// The environment variable that names the SDK root, before the
/// configuration's.
pub const ROOT_ENV: &str = "MAXINE_AR_ROOT";

/// Where the `maxine-ar-sdk-local` package installs the SDK.
pub const DEFAULT_ROOT: &str = "/opt/nvidia/maxine/arsdk";

/// The SDK features the module uses, with what the probe calls them: the
/// directory names under `<root>/features`. Their libraries are loaded with
/// the SDK.
pub const FEATURES: [(&str, &str); 3] = [
    ("nvarfaceboxdetection", "Maxine AR face detection"),
    ("nvarlandmarkdetection", "Maxine AR landmark detection"),
    ("nvargazeredirection", "Maxine AR eye contact"),
];

/// The probe's reason when the SDK's core is not there.
pub const REASON_NO_SDK: &str = "Maxine AR SDK not installed";

/// The settings group of eye contact.
pub const GROUP: &str = "eye_contact";
/// The stage's switch.
pub const ENABLED: &str = "effect.eye_contact.enabled";
/// The randomised look-away.
pub const LOOK_AWAY: &str = "effect.eye_contact.look_away";
/// The eye-contact stage's load-time parameter of [`LOOK_AWAY`].
pub const LOOK_AWAY_KEY: &str = "look_away";
/// Why the look-away switch is greyed while eye contact is off.
pub const REASON_TURN_ON: &str = "Turn on eye contact to change this";

/// The settings group of the automatic framing.
pub const AUTO_GROUP: &str = "auto_frame";
/// The auto-frame stage's switch.
pub const AUTO_ENABLED: &str = "effect.auto_frame.enabled";
/// How close the framing is.
pub const FRAMING: &str = "effect.auto_frame.framing";
/// The auto-frame stage's per-frame parameter of [`FRAMING`].
pub const FRAMING_KEY: &str = "framing";
/// Why the framing is greyed while auto-framing is off.
pub const REASON_TURN_ON_AUTO: &str = "Turn on auto-framing to change this";

/// The keys of `_host`, which the daemon fills in.
#[derive(Debug, Default, Deserialize)]
struct HostKeys {
    #[serde(default)]
    compute_capability: Option<(u32, u32)>,
}

/// The module's configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    sdk_root: Option<PathBuf>,
    #[serde(default, rename = "_host")]
    host: HostKeys,
}

/// The SDK root: `env` (the value of [`ROOT_ENV`]) when set and not empty,
/// else `configured` when not empty, else `default` when it is a directory,
/// else none.
pub fn resolve_root(
    env: Option<&OsStr>,
    configured: Option<&Path>,
    default: &Path,
) -> Option<PathBuf> {
    if let Some(e) = env.filter(|e| !e.is_empty()) {
        return Some(PathBuf::from(e));
    }
    if let Some(c) = configured.filter(|c| !c.as_os_str().is_empty()) {
        return Some(c.to_path_buf());
    }
    default.is_dir().then(|| default.to_path_buf())
}

/// The settings of the module: the eye-contact group, then the
/// auto-frame group.
pub fn settings() -> SettingsDescription {
    SettingsDescription::new()
        .group(
            Group::new(GROUP, "Eye contact")
                .setting(
                    Setting::bool(ENABLED, "Eye contact", false)
                        .stage(EYE_CONTACT)
                        .role(Role::Enable)
                        .hint("switch"),
                )
                .setting(
                    Setting::bool(LOOK_AWAY, "Natural look-away", false)
                        .stage(EYE_CONTACT)
                        .key(LOOK_AWAY_KEY)
                        .apply(Apply::Reload)
                        .enabled_when(ENABLED, Op::Eq, true)
                        .disabled_reason(REASON_TURN_ON)
                        .hint("switch"),
                ),
        )
        .group(
            Group::new(AUTO_GROUP, "Auto-framing")
                .setting(
                    Setting::bool(AUTO_ENABLED, "Auto-framing", false)
                        .stage(AUTO_FRAME)
                        .role(Role::Enable)
                        .hint("switch"),
                )
                .setting(
                    Setting::enumeration(
                        FRAMING,
                        "Framing",
                        Framing::ALL.map(|f| (f.as_str(), f.label())),
                        Framing::Medium.as_str(),
                    )
                    .stage(AUTO_FRAME)
                    .key(FRAMING_KEY)
                    .apply(Apply::Live)
                    .enabled_when(AUTO_ENABLED, Op::Eq, true)
                    .disabled_reason(REASON_TURN_ON_AUTO)
                    .hint("segmented"),
                ),
        )
}

/// `name` or its major-versioned soname is a file in `dir`.
fn has_library(dir: &Path, name: &str) -> bool {
    dir.join(name).is_file() || dir.join(format!("{name}.1")).is_file()
}

/// A model file in `models` whose name starts with `prefix`, has the field
/// `field` when one is given (`"126"`: a field of the file name between
/// underscores or dots), and is of the GPU architecture `sm` (`"120"` for
/// compute capability 12.0, a field too), or of any architecture for
/// `None`.
fn has_models(models: &Path, prefix: &str, field: Option<&str>, sm: Option<&str>) -> bool {
    let Ok(entries) = std::fs::read_dir(models) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|e| {
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        let has = |f: &str| name.split(['_', '.']).any(|x| x == f);
        name.starts_with(prefix)
            && name.ends_with(".trtpkg")
            && field.is_none_or(has)
            && sm.is_none_or(has)
            && e.path().is_file()
    })
}

/// The models eye contact needs, in the order the probe names a missing
/// one: the file name's prefix, a field it must have, and what the reason
/// calls them. Gaze redirection finds the face and its 126 landmarks with
/// the face detection and landmark models before it runs its own encoder
/// and decoder.
const EYE_CONTACT_MODELS: [(&str, Option<&str>, &str); 4] = [
    ("gazeredir_encoder_", None, "Eye Contact"),
    ("gazeredir_decoder_", None, "Eye Contact"),
    ("face_detection_", None, "face detection"),
    ("faceland_", Some("126"), "126-point face landmark"),
];

/// The probe's reason when the models called `what` are not in the tree:
/// for this GPU (`sm`), or at all.
fn no_models(what: &str, sm: Option<&str>) -> String {
    match sm {
        Some(sm) => format!("No {what} models for this GPU (sm_{sm})"),
        None => format!("No {what} models installed"),
    }
}

/// The SDK root when it has the SDK's core libraries.
fn core_root(root: Option<&Path>) -> Option<&Path> {
    let root = root.filter(|r| r.is_dir())?;
    let lib = root.join("lib");
    (has_library(&lib, ffi::AR) && has_library(&lib, ffi::NVCVIMAGE)).then_some(root)
}

/// Why the features `features` (directory names of [`FEATURES`]) are not
/// all in the SDK tree at `root`, if they are not.
fn missing_feature(root: &Path, features: &[&str]) -> Option<String> {
    FEATURES
        .iter()
        .filter(|(dir, _)| features.contains(dir))
        .find(|(dir, _)| {
            preload::shared_objects(&root.join("features").join(dir).join("lib")).is_empty()
        })
        .map(|(_, label)| format!("{label} not installed"))
}

/// Why the SDK tree at `root` cannot run eye contact on a GPU of
/// `compute_capability` (`None`: any), in plain words for the user; `None`
/// when it can. It needs the three features and the models of
/// `EYE_CONTACT_MODELS` for that GPU. Reads directories only.
pub fn eye_contact_unavailable(
    root: Option<&Path>,
    compute_capability: Option<(u32, u32)>,
) -> Option<String> {
    let Some(root) = core_root(root) else {
        return Some(REASON_NO_SDK.into());
    };
    let all: Vec<&str> = FEATURES.iter().map(|(d, _)| *d).collect();
    if let Some(why) = missing_feature(root, &all) {
        return Some(why);
    }
    let models = root.join("lib/models");
    let sm = compute_capability.map(|(major, minor)| format!("{major}{minor}"));
    EYE_CONTACT_MODELS
        .iter()
        .find(|(prefix, field, _)| !has_models(&models, prefix, *field, sm.as_deref()))
        .map(|(_, _, what)| no_models(what, sm.as_deref()))
}

/// Why the SDK tree at `root` cannot run the automatic framing on a GPU of
/// `compute_capability` (`None`: any), in plain words for the user; `None`
/// when it can. It needs the face detection feature and its models only.
/// Reads directories only.
pub fn auto_frame_unavailable(
    root: Option<&Path>,
    compute_capability: Option<(u32, u32)>,
) -> Option<String> {
    let Some(root) = core_root(root) else {
        return Some(REASON_NO_SDK.into());
    };
    if let Some(why) = missing_feature(root, &["nvarfaceboxdetection"]) {
        return Some(why);
    }
    let sm = compute_capability.map(|(major, minor)| format!("{major}{minor}"));
    if !has_models(
        &root.join("lib/models"),
        "face_detection_",
        None,
        sm.as_deref(),
    ) {
        return Some(no_models("face detection", sm.as_deref()));
    }
    None
}

/// The probe of the SDK tree at `root` for a GPU of `compute_capability`:
/// eye contact, then auto-frame.
pub fn probe_root(root: Option<&Path>, compute_capability: Option<(u32, u32)>) -> ProbeReport {
    let mut report = ProbeReport::new();
    for (stage, why) in [
        (
            EYE_CONTACT,
            eye_contact_unavailable(root, compute_capability),
        ),
        (AUTO_FRAME, auto_frame_unavailable(root, compute_capability)),
    ] {
        report = match why {
            None => report.available(stage),
            Some(why) => report.unavailable(stage, why),
        };
    }
    report
}

/// The module object.
pub struct ArModule {
    root: Option<PathBuf>,
    compute_capability: Option<(u32, u32)>,
}

impl ArModule {
    /// The SDK root the module uses.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    fn sdk(&self) -> Result<&'static ffi::Sdk, Error> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| Error::unavailable(REASON_NO_SDK))?;
        let features: Vec<&str> = FEATURES.iter().map(|(d, _)| *d).collect();
        ffi::open(root, &features).map_err(Error::unavailable)
    }

    fn npp(&self) -> Result<&'static npp::Npp, Error> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| Error::unavailable(REASON_NO_SDK))?;
        npp::open(root).map_err(Error::unavailable)
    }
}

impl Module for ArModule {
    type Instance = ArInstance;

    fn init(config: serde_json::Value) -> Result<Self, Error> {
        let config: Config = if config.is_null() {
            Config::default()
        } else {
            serde_json::from_value(config)?
        };
        let env = std::env::var_os(ROOT_ENV);
        let root = resolve_root(
            env.as_deref(),
            config.sdk_root.as_deref(),
            Path::new(DEFAULT_ROOT),
        );
        match &root {
            Some(r) => log::debug!("the Maxine AR SDK root is {}", r.display()),
            None => log::debug!("no Maxine AR SDK root"),
        }
        Ok(ArModule {
            root,
            compute_capability: config.host.compute_capability,
        })
    }

    fn probe(&self) -> ProbeReport {
        probe_root(self.root.as_deref(), self.compute_capability)
    }

    fn settings(&self) -> SettingsDescription {
        settings()
    }

    /// Opens the SDK's libraries and resolves its entry points. No CUDA
    /// context, no model.
    fn check(&self, stages: &[&str]) -> Result<(), Error> {
        if stages.contains(&EYE_CONTACT) || stages.contains(&AUTO_FRAME) {
            self.sdk()?;
        }
        if stages.contains(&AUTO_FRAME) {
            self.npp()?;
        }
        Ok(())
    }

    fn create_instance(&self) -> Result<ArInstance, Error> {
        Ok(ArInstance {
            root: self.root.clone(),
            eye: None,
            auto: None,
            frames: None,
            geometry: None,
            face: None,
            framing: Framing::Medium,
            result: Source::Input,
            replay: false,
            order: Vec::new(),
            clock: std::time::Instant::now(),
        })
    }
}

/// What the frames of a loaded chain are: a load with the same is a switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    width: u32,
    height: u32,
    /// The NvCV colour-space flags.
    flags: u32,
    /// The CUDA context and stream, as addresses.
    context: usize,
    stream: usize,
}

impl Geometry {
    fn stream(&self) -> ffi::CuStream {
        self.stream as ffi::CuStream
    }
}

/// One chain of the module's stages. The fields drop in order: the
/// features before the images they are bound to.
pub struct ArInstance {
    root: Option<PathBuf>,
    /// The eye-contact stage, when it is in the loaded chain.
    eye: Option<EyeContact>,
    /// The auto-frame stage, when it is in the loaded chain.
    auto: Option<AutoFrame>,
    /// The images of the loaded chain.
    frames: Option<Frames>,
    /// The frames of the loaded chain (`None`: nothing loaded).
    geometry: Option<Geometry>,
    /// The face the eye-contact stage tracked on the last fresh frame, in
    /// pixels of the picture it ran on (the framed one after an
    /// auto-frame).
    face: Option<NvArRect>,
    /// The framing of the auto-frame stage: the load's, then the last
    /// per-frame value.
    framing: Framing,
    /// The image that holds the result of the last fresh frame.
    result: Source,
    /// [`ArInstance::result`] holds the output of the last fresh frame, for
    /// a repeat.
    replay: bool,
    /// The order of the loaded chain's stages: the request's.
    order: Vec<Step>,
    /// The clock of frames without a presentation time.
    clock: std::time::Instant,
}

/// The stage error of eye contact.
fn eye_error(message: impl Into<String>) -> Error {
    Error::stage(EYE_CONTACT, message)
}

/// The stage error of auto-frame.
fn auto_error(message: impl Into<String>) -> Error {
    Error::stage(AUTO_FRAME, message)
}

/// The NvCV colour flags of a request, or why it cannot be converted.
fn request_flags(req: &ChainRequest<'_>) -> Result<u32, String> {
    let matrix = match req.matrix() {
        ColourMatrix::Bt601 => ffi::NVCV_601,
        ColourMatrix::Bt709 => ffi::NVCV_709,
        ColourMatrix::Bt2020 => ffi::NVCV_2020,
        ColourMatrix::Other(c) => return Err(format!("unknown colour matrix {c}")),
    };
    let siting = match req.chroma_siting() {
        ChromaSiting::Mpeg2 => ffi::NVCV_CHROMA_COSITED,
        ChromaSiting::Jpeg => ffi::NVCV_CHROMA_INTSTITIAL,
        ChromaSiting::Cosited => ffi::NVCV_CHROMA_TOPLEFT,
        ChromaSiting::Other(c) => return Err(format!("unknown chroma siting {c}")),
    };
    Ok(image::colour_flags(matrix, req.full_range(), siting))
}

/// Plane `i`'s address and pitch.
fn plane(f: &Frame<'_>, i: usize) -> Result<(u64, usize), Error> {
    let p = f
        .plane(i)
        .ok_or_else(|| Error::invalid(format!("the frame has no plane {i}")))?;
    Ok((p.ptr_or_fd + p.offset, p.pitch as usize))
}

/// A CUDA NV12 frame of the loaded size.
fn device_nv12(f: &Frame<'_>, g: &Geometry) -> Result<DeviceNv12, Error> {
    if f.kind() != FrameKind::Cuda || f.format() != PixelFormat::Nv12 {
        return Err(Error::unsupported(
            "the Maxine AR module takes CUDA NV12 frames only",
        ));
    }
    if (f.width(), f.height()) != (g.width, g.height) {
        return Err(Error::invalid(format!(
            "a {}x{} frame for a chain loaded for {}x{}",
            f.width(),
            f.height(),
            g.width,
            g.height
        )));
    }
    let (y, y_pitch) = plane(f, 0)?;
    let (uv, uv_pitch) = plane(f, 1)?;
    Ok(DeviceNv12 {
        y,
        y_pitch,
        uv,
        uv_pitch,
        width: f.width(),
        height: f.height(),
    })
}

/// The frame `input` unchanged in `output` (`src` and `dst`, checked):
/// nothing to do in place, else a copy on `stream`.
fn pass_through(
    input: &Frame<'_>,
    output: &Frame<'_>,
    src: &DeviceNv12,
    dst: &DeviceNv12,
    stream: ffi::CuStream,
) -> Result<(), Error> {
    if input.same_memory(output) {
        return Ok(());
    }
    // SAFETY: the host's frames, of the current context and one size, for
    // the call; distinct.
    unsafe { obc_gpu::copy_nv12_async(src, dst, stream) }.map_err(|e| Error::failed(e.to_string()))
}

impl ArInstance {
    /// The face the eye-contact stage tracked on the last fresh frame.
    pub fn face(&self) -> Option<NvArRect> {
        self.face
    }

    /// The eye-contact stage, when it is in the loaded chain.
    pub fn eye(&self) -> bool {
        self.eye.is_some()
    }

    /// The auto-frame stage, when it is in the loaded chain.
    pub fn auto_frame(&self) -> bool {
        self.auto.is_some()
    }

    /// The framing the auto-frame stage uses.
    pub fn framing(&self) -> Framing {
        self.framing
    }

    fn sdk(&self) -> Result<&'static ffi::Sdk, String> {
        let root = self.root.as_deref().ok_or(REASON_NO_SDK)?;
        let features: Vec<&str> = FEATURES.iter().map(|(d, _)| *d).collect();
        ffi::open(root, &features)
    }

    /// Sets the framing, for the loaded stage too.
    fn set_framing(&mut self, framing: Framing) {
        self.framing = framing;
        if let Some(a) = self.auto.as_mut() {
            a.framer.set_framing(framing);
        }
    }

    /// The time of a frame, in seconds: its presentation time, else the
    /// instance's clock.
    fn time_of(&self, f: &Frame<'_>) -> f64 {
        match f.pts_ns() {
            Some(ns) => ns as f64 / 1e9,
            None => self.clock.elapsed().as_secs_f64(),
        }
    }

    /// Whether `step` is loaded.
    fn has(&self, step: Step) -> bool {
        match step {
            Step::Eye => self.eye.is_some(),
            Step::Auto => self.auto.is_some(),
        }
    }

    /// Drops `step` after its failure on a frame: the host runs the frame
    /// again without it.
    fn drop_step(&mut self, step: Step, e: String) -> Error {
        self.replay = false;
        match step {
            Step::Auto => {
                self.auto = None;
                log::warn!("auto-frame failed on a frame and is dropped: {e}");
                auto_error(e)
            }
            Step::Eye => {
                self.eye = None;
                self.face = None;
                log::warn!("eye contact failed on a frame and is dropped: {e}");
                eye_error(e)
            }
        }
    }

    /// Drops the last loaded stage of the chain's order after a failure of
    /// the chain's own conversions, which no stage alone caused: dropping
    /// one at a time, the host's run again without it finds whether the
    /// rest still runs.
    fn drop_last(&mut self, e: String) -> Error {
        let last = self
            .order
            .iter()
            .rev()
            .copied()
            .find(|s| self.has(*s))
            .unwrap_or(if self.auto.is_some() {
                Step::Auto
            } else {
                Step::Eye
            });
        self.drop_step(last, e)
    }
}

/// The stages a load brings in: `None` keeps the loaded one (or none).
struct NewStages {
    eye: Option<EyeContact>,
    auto: Option<AutoFrame>,
}

/// What a load asks for, besides the frames.
struct Wanted {
    eye: Option<GazeConfig>,
    auto: Option<Framing>,
}

/// Loads the stages of `wanted` that `eye` and `auto` (the loaded ones) do
/// not already provide, each new one beside the loaded one: a failure keeps
/// what is loaded.
fn load_stages(
    root: &Path,
    sdk: &'static ffi::Sdk,
    frames: &mut Frames,
    g: Geometry,
    eye: Option<&EyeContact>,
    auto: Option<&AutoFrame>,
    wanted: &Wanted,
) -> Result<NewStages, Error> {
    let mut new = NewStages {
        eye: None,
        auto: None,
    };
    if let Some(config) = wanted.eye
        && !eye.is_some_and(|e| e.config() == config)
    {
        new.eye = Some(
            EyeContact::load(sdk, frames, g.width, g.height, g.stream(), config)
                .map_err(eye_error)?,
        );
    }
    if let Some(framing) = wanted.auto
        && auto.is_none()
    {
        let npp = npp::open(root).map_err(auto_error)?;
        new.auto = Some(
            AutoFrame::load(sdk, npp, frames, g.width, g.height, g.stream(), framing)
                .map_err(auto_error)?,
        );
    }
    Ok(new)
}

impl Instance for ArInstance {
    fn load(&mut self, req: &ChainRequest<'_>) -> Result<LoadReport, Error> {
        let start = std::time::Instant::now();
        if req.input_kind() != FrameKind::Cuda {
            return Err(Error::unsupported(
                "the Maxine AR module runs on CUDA frames only",
            ));
        }
        if req.input_format() != PixelFormat::Nv12
            || req.output_format() != PixelFormat::Nv12
            || req.input_size() != req.output_size()
        {
            return Err(Error::unsupported(
                "the Maxine AR effects run on NV12 frames, in place of the same size",
            ));
        }
        if let Some(other) = req
            .stages()
            .iter()
            .find(|s| **s != EYE_CONTACT && **s != AUTO_FRAME)
        {
            return Err(Error::invalid(format!(
                "the Maxine AR module has no stage {other:?}"
            )));
        }
        let cuda = req.cuda();
        if cuda.is_null() {
            return Err(Error::invalid(
                "a Maxine AR chain needs the frames' CUDA context",
            ));
        }
        if let Some(f) = req
            .param(AUTO_FRAME, FRAMING_KEY)
            .and_then(|v| v.as_str())
            .and_then(Framing::parse)
        {
            self.framing = f;
        }
        let wanted = Wanted {
            eye: req.has_stage(EYE_CONTACT).then(|| GazeConfig {
                look_away: req
                    .param(EYE_CONTACT, LOOK_AWAY_KEY)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }),
            auto: req.has_stage(AUTO_FRAME).then_some(self.framing),
        };
        let order = plan::order_of(req.stages());
        // A failure that is no stage's alone is the first stage's: the host
        // then loads the rest, which finds it again or runs.
        let first_error = |e: String| match order.first() {
            Some(Step::Auto) => auto_error(e),
            _ => eye_error(e),
        };
        let flags = request_flags(req).map_err(first_error)?;
        let (width, height) = req.input_size();
        let geometry = Geometry {
            width,
            height,
            flags,
            context: cuda.context as usize,
            stream: cuda.stream as usize,
        };
        if self.geometry != Some(geometry) {
            // A first load or a reload: nothing of the previous chain stays.
            self.unload();
        }
        let report = |reused: bool| LoadReport {
            load_ms: start.elapsed().as_secs_f32() * 1000.0,
            warmup_ms: None,
            reused_model: reused,
        };
        self.replay = false;
        if wanted.eye.is_none() && wanted.auto.is_none() {
            self.eye = None;
            self.auto = None;
            self.order = order;
            self.geometry = Some(geometry);
            return Ok(report(false));
        }
        // A switch keeps the loaded chain when it fails.
        let switch = self.eye.is_some() || self.auto.is_some();
        let sdk = self.sdk().map_err(first_error)?;
        if self.frames.is_none() {
            self.frames = Some(Frames::alloc(sdk, width, height, flags).map_err(first_error)?);
            self.geometry = Some(geometry);
        }
        let root = self.root.clone().unwrap_or_default();
        let Some(frames) = self.frames.as_mut() else {
            return Err(Error::state("no frames are loaded"));
        };
        let new = match load_stages(
            &root,
            sdk,
            frames,
            geometry,
            self.eye.as_ref(),
            self.auto.as_ref(),
            &wanted,
        ) {
            Ok(n) => n,
            Err(e) => {
                if !switch {
                    // Nothing of the request is in force.
                    self.unload();
                }
                return Err(e);
            }
        };
        let reused = new.eye.is_none() && new.auto.is_none();
        match (wanted.eye, new.eye) {
            (None, _) => {
                self.eye = None;
                self.face = None;
            }
            (Some(_), Some(e)) => self.eye = Some(e),
            (Some(_), None) => {}
        }
        match (wanted.auto, new.auto) {
            (None, _) => self.auto = None,
            (Some(_), Some(a)) => self.auto = Some(a),
            (Some(f), None) => self.set_framing(f),
        }
        self.order = order;
        Ok(report(reused))
    }

    fn process(
        &mut self,
        input: &Frame<'_>,
        output: &Frame<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        let g = self
            .geometry
            .ok_or_else(|| Error::state("nothing is loaded"))?;
        let (src, dst) = (device_nv12(input, &g)?, device_nv12(output, &g)?);
        let stream = g.stream();
        if let Some(value) = Param::find(live, AUTO_FRAME, FRAMING_KEY) {
            match value.as_str().and_then(Framing::parse) {
                Some(f) if f != self.framing => self.set_framing(f),
                Some(_) => {}
                None => log::debug!("auto-frame: an unknown framing {value:?} is ignored"),
            }
        }
        if input.is_new_source()
            && let Some(a) = self.auto.as_mut()
        {
            a.framer.reset();
        }
        let now = self.time_of(input);
        let staged = self.eye.is_some() || self.auto.is_some();
        let Some(frames) = self.frames.as_mut().filter(|_| staged) else {
            // No stage: the frame passes through.
            return pass_through(input, output, &src, &dst, stream);
        };
        if !(input.is_repeat() && self.replay) {
            // SAFETY: the host's frame, of the current context and the
            // loaded size, for the call (the work is on its stream).
            if let Err(e) = unsafe { frames.convert_in(&src, stream) } {
                return Err(self.drop_last(e));
            }
            let mut live = Live {
                eye: self.eye.as_mut(),
                auto: self.auto.as_mut(),
                frames,
                stream,
                now,
            };
            let ran = plan::run_fresh(&self.order, &mut live);
            let Ran { result, eye_face } = match ran {
                Ok(r) => r,
                // The stage alone failed: it is dropped, and the host runs
                // the frame again without it.
                Err((step, e)) => return Err(self.drop_step(step, e)),
            };
            if let Some(face) = eye_face {
                if face.is_some() != self.face.is_some() {
                    match face {
                        Some(f) => log::debug!(
                            "eye contact: a face at {:.0},{:.0} ({:.0}x{:.0})",
                            f.x,
                            f.y,
                            f.width,
                            f.height
                        ),
                        None => log::debug!("eye contact: no face"),
                    }
                }
                self.face = face;
            }
            self.result = result;
            self.replay = true;
        }
        if self.result == Source::Input {
            // No stage changed the picture (auto-frame alone, at the full
            // frame): the frame stays as it is, with no round trip through
            // BGR. A repeat repeats the camera frame, so this holds for it
            // too.
            return pass_through(input, output, &src, &dst, stream);
        }
        let Some(frames) = self.frames.as_mut() else {
            return Err(Error::state("no frames are loaded"));
        };
        // SAFETY: as above; the result image is written.
        if let Err(e) = unsafe { frames.convert_out(self.result, &dst, stream) } {
            return Err(self.drop_last(e));
        }
        Ok(())
    }

    fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
        // The trackers find the new face by themselves; the crop starts
        // from the full frame; nothing to replay.
        self.face = None;
        self.replay = false;
        if let Some(a) = self.auto.as_mut() {
            a.framer.reset();
        }
        Ok(Vec::new())
    }

    fn unload(&mut self) {
        // The features before the images they are bound to.
        self.eye = None;
        self.auto = None;
        self.frames = None;
        self.geometry = None;
        self.face = None;
        self.result = Source::Input;
        self.replay = false;
        self.order.clear();
    }
}

/// The loaded stages of a frame, for [`plan::run_fresh`].
struct Live<'a> {
    eye: Option<&'a mut EyeContact>,
    auto: Option<&'a mut AutoFrame>,
    frames: &'a mut Frames,
    stream: ffi::CuStream,
    now: f64,
}

impl FrameStages for Live<'_> {
    fn has(&self, step: Step) -> bool {
        match step {
            Step::Eye => self.eye.is_some(),
            Step::Auto => self.auto.is_some(),
        }
    }

    fn to_camera(&mut self, from: Source) -> Result<(), String> {
        self.frames.copy_to_input(from, self.stream)
    }

    fn eye(&mut self) -> Result<Option<NvArRect>, String> {
        match self.eye.as_mut() {
            Some(e) => e.run(),
            None => Err("eye contact is not loaded".into()),
        }
    }

    fn auto(&mut self, from: Source, eye_face: Option<Option<NvArRect>>) -> Result<Source, String> {
        match self.auto.as_mut() {
            Some(a) => a.run(self.frames, from, eye_face, self.now),
            None => Err("auto-frame is not loaded".into()),
        }
    }
}

obc_module_sdk::export_module!(ArModule);

#[cfg(test)]
mod tests;
