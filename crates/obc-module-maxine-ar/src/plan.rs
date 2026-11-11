//! The order of a chain's stages on a fresh frame: the order of the chain
//! request (the host's contract: the module runs its stages in the order
//! the host shows them), with the GPU work behind [`FrameStages`] so that
//! the order is tested without a GPU.
//!
//! * Eye contact reads the camera image ([`Source::Input`]). After an
//!   auto-frame that framed the picture, the framed image is copied into
//!   the camera image first: eye contact runs on the framed picture.
//! * Auto-frame reads the image so far (the eye-contact result when that
//!   ran before it, else the camera image). It takes eye contact's face
//!   only when eye contact ran before it on the same frame; otherwise (eye
//!   contact after it, not in the chain, or dropped) it finds the face with
//!   its own detector.
//! * A failure is the stage's that failed: no later stage runs, and the
//!   host runs the frame again without it.

use crate::autoframe::Source;
use crate::ffi::NvArRect;
use crate::{AUTO_FRAME, EYE_CONTACT};

/// A stage of the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// `eye-contact`.
    Eye,
    /// `auto-frame`.
    Auto,
}

impl Step {
    /// The step of a stage id.
    pub(crate) fn of(id: &str) -> Option<Step> {
        match id {
            EYE_CONTACT => Some(Step::Eye),
            AUTO_FRAME => Some(Step::Auto),
            _ => None,
        }
    }
}

/// The module's stages of a request, in the request's order (each once).
pub(crate) fn order_of(stages: &[&str]) -> Vec<Step> {
    let mut order = Vec::new();
    for step in stages.iter().filter_map(|s| Step::of(s)) {
        if !order.contains(&step) {
            order.push(step);
        }
    }
    order
}

/// The work of the loaded stages on a fresh frame whose camera image is
/// written.
pub(crate) trait FrameStages {
    /// Whether `step` is loaded.
    fn has(&self, step: Step) -> bool;
    /// Copies the image of `from` into the camera image.
    fn to_camera(&mut self, from: Source) -> Result<(), String>;
    /// Runs eye contact on the camera image: the face it tracked.
    fn eye(&mut self) -> Result<Option<NvArRect>, String>;
    /// Runs auto-frame on the image of `from`, with eye contact's face on
    /// this frame (`Some`) or its own detector's (`None`): where the
    /// result is.
    fn auto(&mut self, from: Source, eye_face: Option<Option<NvArRect>>) -> Result<Source, String>;
}

/// What the stages of a fresh frame gave.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ran {
    /// The image that holds the frame's result.
    pub result: Source,
    /// The face eye contact tracked, when it ran (in the coordinates of
    /// the picture it ran on).
    pub eye_face: Option<Option<NvArRect>>,
}

/// Runs the loaded stages of `order` on a fresh frame; a failure is the
/// failing stage's, with its message.
pub(crate) fn run_fresh(order: &[Step], s: &mut impl FrameStages) -> Result<Ran, (Step, String)> {
    let mut from = Source::Input;
    let mut eye_face = None;
    for &step in order {
        if !s.has(step) {
            continue;
        }
        match step {
            Step::Eye => {
                s.to_camera(from).map_err(|e| (Step::Eye, e))?;
                eye_face = Some(s.eye().map_err(|e| (Step::Eye, e))?);
                from = Source::Eye;
            }
            Step::Auto => {
                from = s.auto(from, eye_face).map_err(|e| (Step::Auto, e))?;
            }
        }
    }
    Ok(Ran {
        result: from,
        eye_face,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        ToCamera(Source),
        Eye,
        Auto(Source, Option<Option<NvArRect>>),
    }

    /// Fake stages that record their calls.
    struct Fake {
        eye: bool,
        auto: bool,
        eye_fails: bool,
        auto_fails: bool,
        /// Auto-frame frames the picture (else its crop is the full frame).
        frames: bool,
        face: Option<NvArRect>,
        calls: Vec<Call>,
    }

    impl Fake {
        fn new(eye: bool, auto: bool) -> Fake {
            Fake {
                eye,
                auto,
                eye_fails: false,
                auto_fails: false,
                frames: true,
                face: Some(NvArRect {
                    x: 10.0,
                    y: 20.0,
                    width: 30.0,
                    height: 40.0,
                }),
                calls: Vec::new(),
            }
        }
    }

    impl FrameStages for Fake {
        fn has(&self, step: Step) -> bool {
            match step {
                Step::Eye => self.eye,
                Step::Auto => self.auto,
            }
        }
        fn to_camera(&mut self, from: Source) -> Result<(), String> {
            // The real copy does nothing from the camera image itself.
            if from != Source::Input {
                self.calls.push(Call::ToCamera(from));
            }
            Ok(())
        }
        fn eye(&mut self) -> Result<Option<NvArRect>, String> {
            self.calls.push(Call::Eye);
            if self.eye_fails {
                return Err("eye failed".into());
            }
            Ok(self.face)
        }
        fn auto(
            &mut self,
            from: Source,
            eye_face: Option<Option<NvArRect>>,
        ) -> Result<Source, String> {
            self.calls.push(Call::Auto(from, eye_face));
            if self.auto_fails {
                return Err("auto failed".into());
            }
            Ok(if self.frames { Source::Framed } else { from })
        }
    }

    const EYE_AUTO: &[Step] = &[Step::Eye, Step::Auto];
    const AUTO_EYE: &[Step] = &[Step::Auto, Step::Eye];

    #[test]
    fn the_order_is_the_requests() {
        assert_eq!(order_of(&[EYE_CONTACT, AUTO_FRAME]), EYE_AUTO);
        assert_eq!(order_of(&[AUTO_FRAME, EYE_CONTACT]), AUTO_EYE);
        assert_eq!(order_of(&[AUTO_FRAME]), [Step::Auto]);
        assert_eq!(order_of(&[EYE_CONTACT, EYE_CONTACT]), [Step::Eye]);
        assert_eq!(order_of(&[]), []);
    }

    #[test]
    fn eye_contact_first_hands_its_face_and_result_to_auto_frame() {
        let mut f = Fake::new(true, true);
        let face = f.face;
        let ran = run_fresh(EYE_AUTO, &mut f).unwrap();
        assert_eq!(
            f.calls,
            [Call::Eye, Call::Auto(Source::Eye, Some(face))],
            "auto-frame reads the eye-contact result with its face"
        );
        assert_eq!(ran.result, Source::Framed);
        assert_eq!(ran.eye_face, Some(face));
    }

    #[test]
    fn auto_frame_first_uses_its_own_detector_and_eye_contact_runs_on_the_framed_picture() {
        let mut f = Fake::new(true, true);
        let face = f.face;
        let ran = run_fresh(AUTO_EYE, &mut f).unwrap();
        assert_eq!(
            f.calls,
            [
                Call::Auto(Source::Input, None),
                Call::ToCamera(Source::Framed),
                Call::Eye
            ]
        );
        assert_eq!(ran.result, Source::Eye);
        assert_eq!(ran.eye_face, Some(face));
        // A crop at the full frame: nothing to copy.
        let mut f = Fake::new(true, true);
        f.frames = false;
        assert_eq!(run_fresh(AUTO_EYE, &mut f).unwrap().result, Source::Eye);
        assert_eq!(f.calls, [Call::Auto(Source::Input, None), Call::Eye]);
    }

    #[test]
    fn a_failing_eye_contact_stops_the_frame_and_auto_frame_then_runs_on_its_own_detector() {
        let mut f = Fake::new(true, true);
        f.eye_fails = true;
        assert_eq!(
            run_fresh(EYE_AUTO, &mut f),
            Err((Step::Eye, "eye failed".into()))
        );
        assert_eq!(f.calls, [Call::Eye], "auto-frame did not run");
        // The stage is dropped and the host runs the frame again.
        f.eye = false;
        f.calls.clear();
        let ran = run_fresh(EYE_AUTO, &mut f).unwrap();
        assert_eq!(f.calls, [Call::Auto(Source::Input, None)]);
        assert_eq!(ran.result, Source::Framed);
        assert_eq!(ran.eye_face, None);
    }

    #[test]
    fn a_failing_auto_frame_is_its_own_failure_and_stops_the_frame() {
        let mut f = Fake::new(true, true);
        f.auto_fails = true;
        assert_eq!(
            run_fresh(AUTO_EYE, &mut f),
            Err((Step::Auto, "auto failed".into()))
        );
        assert_eq!(f.calls, [Call::Auto(Source::Input, None)]);
        f.auto = false;
        f.calls.clear();
        assert_eq!(run_fresh(AUTO_EYE, &mut f).unwrap().result, Source::Eye);
        assert_eq!(f.calls, [Call::Eye]);
    }

    #[test]
    fn auto_frame_alone_at_the_full_frame_leaves_the_camera_image() {
        let mut f = Fake::new(false, true);
        f.frames = false;
        let ran = run_fresh(EYE_AUTO, &mut f).unwrap();
        assert_eq!(ran.result, Source::Input);
        assert_eq!(f.calls, [Call::Auto(Source::Input, None)]);
    }
}
