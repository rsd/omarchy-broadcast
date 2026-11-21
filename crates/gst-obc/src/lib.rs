//! The GStreamer effect element of omarchy-broadcast, `obceffect`, written in
//! Rust in the gst-plugins-rs style and registered in-process
//! ([`register`]), not as a plugin file
//!.
//!
//! * [`ObcEffect`]: the element and its Rust API (backend factory, settings,
//!   status, the status of each stage, statistics); see [`effect`] for its
//!   behaviour.
//! * [`NEW_SOURCE_FLAG`]: the buffer flag the output clock marks the first
//!   frame of a new source with, [`REPEAT_FLAG`] the one it marks a
//!   repeated frame with, and [`PLACEHOLDER_FLAG`] the one it marks its
//!   placeholder image with (passed through untouched).
//! * [`EffectSettings`] (with its [`StageRequest`]s), [`EffectFallback`],
//!   [`EffectStatus`], [`EffectStats`] (with its [`StageStats`]): what it
//!   is asked to do and what it reports.
//! * [`background`]: the replace effect's image (file checks, decoding,
//!   crop to fill), and the fallback image of `on-failure=image` as YUV
//!   4:2:0 ([`background::decode_yuv420`]).
//! * [`cuda`]: the subset of GStreamer's CUDA library it uses, opened at run
//!   time.
//! * [`video_background`]: the replace effect's video, decoded in a loop
//!   into the composite (the file and container checks, the reasons the
//!   element reports).

pub mod background;
pub mod cuda;
pub mod effect;
pub mod enhance;
pub mod module;
mod settings;
mod video;
pub mod video_background;

pub use effect::{
    ELEMENT_NAME, NEW_SOURCE_FLAG, ObcEffect, PLACEHOLDER_FLAG, REPEAT_FLAG, STATUS_MESSAGE,
    from_element, has_new_source_flag, has_placeholder_flag, has_repeat_flag, register,
};
pub use enhance::{EnhanceStats, ObcEnhance};
pub use module::{ModuleStats, ObcModule};
pub use settings::{
    EffectFallback, EffectKind, EffectSettings, EffectStats, EffectStatus, Params, StageId,
    StageRequest, StageStats, Value, fallback_unreadable, parse_effect,
};
