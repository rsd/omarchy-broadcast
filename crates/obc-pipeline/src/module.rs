//! Module stages in the output: chains of stages of loadable modules
//! (`obc_backend::chain`), each run by its own `obcmodule` element
//! ([`gst_obc::module`]) at the head of the output pipeline, around the
//! effect element ([`crate::effect`]).
//!
//! An output built with [`OutputConfig::module_stages`](crate::OutputConfig::module_stages)
//! holds one element per [`ModuleStageSpec`] for its whole life, named
//! [`element_name`] of the spec's name: the specs placed
//! [`ModulePlacement::BeforeEffect`] run before `obceffect`, in the order
//! they are listed, and those placed [`ModulePlacement::AfterEffect`] after
//! it (with no effect element, the first group then the second, where it
//! would be). Their stages change while the output streams
//! ([`Output::set_module_stages`](crate::Output::set_module_stages)),
//! without renegotiating caps. An element with no stage on is passthrough:
//! no copy, no chain call, no CUDA call. An output built without any (the
//! default) has no such element and cannot take one later.
//!
//! Each element reports the status of each of its stages as an
//! [`EffectStatus`] ([`Output::module_stage_status`](crate::Output::module_stage_status)),
//! its counters as [`ModuleStats`], and posts every change of a status on
//! the output pipeline's bus as an element message named
//! [`STATUS_MESSAGE`], from the element (its name is [`element_name`] of
//! the spec's), with the fields `module` (the factory's name) and `<id>`
//! and `<id>-reason` for every stage it has been given. Module stages hide
//! nothing, so they fail open: a stage that fails alone is dropped and the
//! others run; any other failure passes the frames through unprocessed.
//! The element resets the chain's temporal state on the first frame of each
//! new source ([`crate::effect::NEW_SOURCE_FLAG`]), and passes the output
//! clock's placeholder image through untouched, with no chain call
//! ([`crate::effect::PLACEHOLDER_FLAG`]).

use std::fmt;
use std::sync::Arc;

pub use gst_obc::module::{ELEMENT_NAME, STATUS_MESSAGE, chain_kind};
pub use gst_obc::{EffectStatus, ModuleStats, ObcModule, StageRequest};
use obc_backend::StageList;
pub use obc_backend::{FrameKind, StageChainFactory};

use crate::pipeline::PipelineError;

/// Where a module's stages run in the output, relative to the effect
/// element: before it (stages of the `pre-matte` slot) or after it
/// (`post-matte` and `post-composite`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModulePlacement {
    BeforeEffect,
    AfterEffect,
}

/// One `obcmodule` element of an output: a name, unique in the output (a
/// module id, say), the factory its chain comes from, its stages (as for
/// [`ObcModule::apply`]), and where it runs.
#[derive(Clone)]
pub struct ModuleStageSpec {
    pub name: String,
    pub factory: Arc<dyn StageChainFactory>,
    pub stages: Vec<StageRequest>,
    pub placement: ModulePlacement,
}

impl fmt::Debug for ModuleStageSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModuleStageSpec")
            .field("name", &self.name)
            .field("factory", &self.factory.name())
            .field("stages", &self.stages)
            .field("placement", &self.placement)
            .finish()
    }
}

/// The longest spec name.
pub const MAX_NAME_LEN: usize = 64;

/// The name of the element of the spec named `name`: `module_<name>`.
pub fn element_name(name: &str) -> String {
    format!("module_{name}")
}

/// The spec name of a module element's name (`None` for any other
/// element): what the source of a [`STATUS_MESSAGE`] maps back to.
pub fn spec_name(element: &str) -> Option<&str> {
    element.strip_prefix("module_")
}

/// Checks a spec name: 1 to [`MAX_NAME_LEN`] bytes of lower-case ASCII
/// letters, digits, `.`, `-` and `_`.
pub fn check_name(name: &str) -> Result<(), PipelineError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(PipelineError::Module(format!(
            "invalid module stage name {name:?}: 1 to {MAX_NAME_LEN} lower-case letters, digits, \
             '.', '-' or '_'"
        )))
    }
}

/// Checks a list of stages for an element: each stage listed once.
pub fn check_stages(stages: &[StageRequest]) -> Result<(), PipelineError> {
    StageList::new(stages.iter().map(|s| s.id.clone()))
        .map(|_| ())
        .map_err(|e| PipelineError::Module(e.to_string()))
}

/// Checks the specs of an output: valid and unique names, valid stage
/// lists, and, for frames in system memory (`cuda` false), a factory that
/// takes host frames.
pub(crate) fn check_specs(specs: &[ModuleStageSpec], cuda: bool) -> Result<(), PipelineError> {
    for (i, s) in specs.iter().enumerate() {
        check_name(&s.name)?;
        if specs[..i].iter().any(|o| o.name == s.name) {
            return Err(PipelineError::Module(format!(
                "the module stage name {:?} is used twice",
                s.name
            )));
        }
        check_stages(&s.stages)?;
        chain_kind(s.factory.as_ref(), cuda).map_err(PipelineError::Module)?;
    }
    Ok(())
}

/// The element for `spec`, configured and named [`element_name`].
pub(crate) fn module_element(spec: &ModuleStageSpec) -> Result<ObcModule, PipelineError> {
    gst_obc::module::register()
        .map_err(|e| PipelineError::Module(format!("cannot register {ELEMENT_NAME}: {e}")))?;
    let m = ObcModule::new(Some(&element_name(&spec.name)));
    m.set_factory(Some(Arc::clone(&spec.factory)));
    m.apply(&spec.stages).map_err(PipelineError::Module)?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_backend::{BackendError, StageChain, StageId};

    struct Kinds(Vec<FrameKind>);
    impl StageChainFactory for Kinds {
        fn name(&self) -> &str {
            "kinds"
        }
        fn kinds(&self) -> &[FrameKind] {
            &self.0
        }
        fn create(&self) -> Result<Box<dyn StageChain>, BackendError> {
            Err(BackendError::Unavailable("none here".into()))
        }
    }

    fn spec(name: &str, kinds: Vec<FrameKind>) -> ModuleStageSpec {
        ModuleStageSpec {
            name: name.into(),
            factory: Arc::new(Kinds(kinds)),
            stages: vec![StageRequest::on(StageId::new("hello").unwrap())],
            placement: ModulePlacement::AfterEffect,
        }
    }

    #[test]
    fn names_are_checked_and_map_to_element_names() {
        for good in ["hello", "br.com.dias.maxine-vfx", "a_1"] {
            check_name(good).unwrap();
            assert_eq!(spec_name(&element_name(good)), Some(good));
        }
        for bad in ["", "Hello", "a/b", "a b", &"x".repeat(65)] {
            let e = check_name(bad).unwrap_err();
            assert!(matches!(e, PipelineError::Module(_)), "{bad:?}: {e}");
        }
        assert_eq!(spec_name("effect"), None);
    }

    #[test]
    fn specs_are_checked_for_names_stages_and_frame_memory() {
        let host = spec("h", vec![FrameKind::Host]);
        let cuda = spec("c", vec![FrameKind::Cuda]);
        check_specs(&[host.clone(), cuda.clone()], true).unwrap();
        check_specs(std::slice::from_ref(&host), false).unwrap();
        let e = check_specs(std::slice::from_ref(&cuda), false).unwrap_err();
        assert!(e.to_string().contains("cuda frames only"), "{e}");
        let e = check_specs(&[host.clone(), host.clone()], true).unwrap_err();
        assert!(e.to_string().contains("used twice"), "{e}");
        let mut twice = host.clone();
        twice.stages.push(twice.stages[0].clone());
        let e = check_specs(&[twice], true).unwrap_err();
        assert!(e.to_string().contains("listed twice"), "{e}");
        let debug = format!("{host:?}");
        assert!(
            debug.contains("name: \"h\", factory: \"kinds\"") && debug.contains("AfterEffect"),
            "{debug}"
        );
    }

    #[test]
    fn the_element_is_named_and_configured_from_the_spec() {
        gst::init().unwrap();
        let s = spec("br.com.example.hello", vec![FrameKind::Host]);
        let m = module_element(&s).unwrap();
        use gst::prelude::*;
        assert_eq!(m.name(), "module_br.com.example.hello");
        assert_eq!(m.stages(), s.stages);
        assert_eq!(m.factory().unwrap().name(), "kinds");
    }
}
