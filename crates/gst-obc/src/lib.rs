//! The GStreamer effect element of oma-broadcast, `obceffect`, written in
//! Rust in the gst-plugins-rs style and registered in-process
//! ([`register`]), not as a plugin file
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`).
//!
//! * [`ObcEffect`]: the element and its Rust API (backend factory, settings,
//!   status, statistics); see [`effect`] for its behaviour.
//! * [`EffectSettings`], [`EffectFallback`], [`EffectStatus`],
//!   [`EffectStats`]: what it is asked to do and what it reports.
//! * [`background`]: the replace effect's image (file checks, decoding,
//!   crop to fill).
//! * [`cuda`]: the subset of GStreamer's CUDA library it uses, opened at run
//!   time.

pub mod background;
pub mod cuda;
pub mod effect;
mod settings;

pub use effect::{ELEMENT_NAME, ObcEffect, STATUS_MESSAGE, from_element, register};
pub use settings::{
    EffectFallback, EffectKind, EffectSettings, EffectStats, EffectStatus, parse_effect,
};
