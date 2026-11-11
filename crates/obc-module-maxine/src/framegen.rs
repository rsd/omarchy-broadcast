//! What the `frame-generation` stage does with each frame, apart from the
//! GPU work: the steps of a fresh frame, a repeat and the first frame.
//!
//! The output clock runs at twice the camera rate, so fresh frames and
//! repeats alternate. With the stage in the chain:
//!
//! * a **fresh** frame runs the other stages, keeps the result as the held
//!   frame, and outputs the frame midway between the previous held frame
//!   and this one ([`Step::Interpolate`]);
//! * a **repeat** skips the other stages and outputs the held frame
//!   ([`Step::Held`]);
//! * the **first** frame after a load or a temporal reset, which has no
//!   previous frame, runs the other stages, keeps the result and outputs it
//!   as it is ([`Step::First`]). A repeat with nothing held is a first
//!   frame too (the element passed the fresh frame through while it was
//!   loading).
//!
//! So the output is one output tick late: the frame midway between camera
//! frames `n - 1` and `n` goes out on the tick of frame `n`, and frame `n`
//! itself on the repeat after it.

/// What to do with one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Run the other stages, keep the result and output it as it is.
    First,
    /// Run the other stages, keep the result and output the frame midway
    /// between the previous held frame and it.
    Interpolate,
    /// Skip the other stages and output the held frame.
    Held,
}

impl Step {
    /// Whether the other stages of the chain run on this frame.
    pub fn runs_stages(self) -> bool {
        self != Step::Held
    }
}

/// Whether a frame is held. A step is decided by [`FrameGen::step`] and
/// takes effect only once it succeeded ([`FrameGen::done`]): a frame that
/// failed leaves the state as it was.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrameGen {
    held: bool,
}

impl FrameGen {
    /// The step for a frame that is a `repeat` or a fresh frame.
    pub fn step(&self, repeat: bool) -> Step {
        match (self.held, repeat) {
            (false, _) => Step::First,
            (true, false) => Step::Interpolate,
            (true, true) => Step::Held,
        }
    }

    /// `step` succeeded.
    pub fn done(&mut self, step: Step) {
        if step.runs_stages() {
            self.held = true;
        }
    }

    /// Forgets the held frame: after a load or a temporal reset, the next
    /// frame is a first one.
    pub fn reset(&mut self) {
        self.held = false;
    }

    /// Whether a frame is held.
    pub fn is_holding(&self) -> bool {
        self.held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `frames` (true: a repeat) and returns the steps.
    fn run(fg: &mut FrameGen, frames: &[bool]) -> Vec<Step> {
        frames
            .iter()
            .map(|&repeat| {
                let s = fg.step(repeat);
                fg.done(s);
                s
            })
            .collect()
    }

    #[test]
    fn fresh_frames_interpolate_and_repeats_show_the_held_frame() {
        let mut fg = FrameGen::default();
        assert_eq!(
            run(&mut fg, &[false, true, false, true, false, true]),
            [
                Step::First,
                Step::Held,
                Step::Interpolate,
                Step::Held,
                Step::Interpolate,
                Step::Held
            ]
        );
        assert!(Step::First.runs_stages() && Step::Interpolate.runs_stages());
        assert!(!Step::Held.runs_stages(), "a repeat skips the stages");
    }

    #[test]
    fn two_fresh_frames_or_two_repeats_in_a_row() {
        // The clock's phase wraps now and then: two fresh frames or two
        // repeats in a row.
        let mut fg = FrameGen::default();
        assert_eq!(
            run(&mut fg, &[false, false, false, true, true, false]),
            [
                Step::First,
                Step::Interpolate,
                Step::Interpolate,
                Step::Held,
                Step::Held,
                Step::Interpolate
            ]
        );
    }

    #[test]
    fn a_reset_or_a_load_makes_the_next_frame_a_first_one() {
        let mut fg = FrameGen::default();
        run(&mut fg, &[false, true, false]);
        assert!(fg.is_holding());
        fg.reset();
        assert!(!fg.is_holding());
        assert_eq!(fg.step(false), Step::First, "no previous frame");
        assert_eq!(
            fg.step(true),
            Step::First,
            "a repeat with nothing held runs the stages"
        );
        assert_eq!(
            run(&mut fg, &[true, true, false]),
            [Step::First, Step::Held, Step::Interpolate]
        );
    }

    #[test]
    fn a_failed_frame_leaves_the_state_as_it_was() {
        let mut fg = FrameGen::default();
        assert_eq!(fg.step(false), Step::First);
        // Not done: the frame failed.
        assert_eq!(fg.step(false), Step::First);
        fg.done(Step::First);
        assert_eq!(fg.step(false), Step::Interpolate);
        fg.done(Step::Held);
        assert!(fg.is_holding());
    }
}
