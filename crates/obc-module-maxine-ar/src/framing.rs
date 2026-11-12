//! The framing of the auto-frame stage: where the crop of the frame is,
//! frame after frame, from the face the stage sees. Pure arithmetic: no
//! GPU, no SDK.
//!
//! The crop always has the frame's aspect and lies inside the frame; it is
//! scaled back to the frame's size, so its height alone says the zoom
//! (`frame height / crop height`), at most [`MAX_ZOOM`].
//!
//! * **Target.** From a face box, the crop in which the face's height is
//!   the framing's share of the crop's ([`Framing::face_share`]), its centre
//!   on the face's vertical centre line, with the face a little above the
//!   middle ([`FACE_LINE`]); then the zoom is capped and the crop clamped
//!   inside the frame.
//! * **Dead zone and hysteresis.** At rest, the crop ignores a target that
//!   is within [`DEAD_ZONE`] of where it rests: a small movement of the head
//!   does not move it. A target further away starts a move, and while
//!   moving the crop follows the target closely, until it is within
//!   [`SETTLE`] of it: then it rests again. A head that sways by a few per
//!   cent is still, and a crop that has started to move finishes the move.
//! * **Smoothing.** The crop eases towards its goal exponentially, with the
//!   time constant [`TIME_CONSTANT_S`], by the time between frames (the
//!   frames' presentation times when the host gives them, else a monotonic
//!   clock; not their count): the path is the same at any frame rate. A gap
//!   is counted as [`MAX_STEP_S`] at most.
//! * **No face.** The crop holds where it is for [`HOLD_S`] after the last
//!   face, then eases back to the full frame.
//! * **Reset.** A new source snaps the crop to the full frame at once.

/// The largest zoom: the smallest crop is the frame divided by this.
pub const MAX_ZOOM: f64 = 1.5;
/// How far a target must be from where the crop rests to move it, as a
/// share of the crop's size (its centre in either direction, or its
/// height).
pub const DEAD_ZONE: f64 = 0.08;
/// How close a moving crop must come to its target to rest again (the
/// other threshold of the hysteresis), in the same measure.
pub const SETTLE: f64 = 0.015;
/// The time constant of the easing, in seconds: after it, the crop has
/// covered about 63 % of the way to its goal (about 95 % after three).
pub const TIME_CONSTANT_S: f64 = 0.8;
/// How long the crop holds after the face is lost, in seconds.
pub const HOLD_S: f64 = 2.0;
/// The longest time between two frames that the easing counts, in seconds.
pub const MAX_STEP_S: f64 = 0.5;
/// Where the face's centre sits in the crop, as a share of the crop's
/// height from its top: a little above the middle, which leaves room for
/// the shoulders.
pub const FACE_LINE: f64 = 0.42;

/// How close the framing is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Head and shoulders, close.
    Tight,
    /// The upper body.
    Medium,
    /// More of the room.
    Wide,
}

impl Framing {
    /// Every framing, in the order of the setting's options.
    pub const ALL: [Framing; 3] = [Framing::Tight, Framing::Medium, Framing::Wide];

    /// The setting's value id.
    pub fn as_str(self) -> &'static str {
        match self {
            Framing::Tight => "tight",
            Framing::Medium => "medium",
            Framing::Wide => "wide",
        }
    }

    /// The setting's label.
    pub fn label(self) -> &'static str {
        match self {
            Framing::Tight => "Tight",
            Framing::Medium => "Medium",
            Framing::Wide => "Wide",
        }
    }

    /// The framing of a value id.
    pub fn parse(s: &str) -> Option<Framing> {
        Framing::ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// The share of the crop's height the face's box takes. Tuned for a
    /// desk, where a face is 35-45 % of the frame's height: at a 40 % face
    /// every setting zooms past the dead zone (a zoom of about 1.087), Tight
    /// to the cap, Medium to about 1.3, Wide to about 1.1. Wide stays at the
    /// full frame for a face above about 41 % (0.45 / 1.087).
    pub fn face_share(self) -> f64 {
        match self {
            Framing::Tight => 0.65,
            Framing::Medium => 0.52,
            Framing::Wide => 0.45,
        }
    }
}

/// `v` clamped to `[lo, hi]`, which cannot panic: an inverted range (by a
/// rounding of a crop as large as the frame) or one that is not a number
/// gives its middle.
fn within(v: f64, lo: f64, hi: f64) -> f64 {
    if lo > hi || lo.is_nan() || hi.is_nan() {
        (lo + hi) / 2.0
    } else {
        v.clamp(lo, hi)
    }
}

/// A rectangle in pixels of the frame: a face box or a crop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn centre(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

/// A crop as the easing moves it: its centre and its height (the width
/// follows from the frame's aspect).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Crop {
    cx: f64,
    cy: f64,
    h: f64,
}

/// The crop of a frame from one frame to the next.
#[derive(Debug, Clone)]
pub struct Framer {
    width: f64,
    height: f64,
    framing: Framing,
    /// Where the crop is.
    current: Crop,
    /// Where it is going (or resting).
    goal: Crop,
    /// Following the target (between the two thresholds).
    moving: bool,
    /// The time of the last update, in seconds.
    last: Option<f64>,
    /// The time of the last face.
    last_face: Option<f64>,
}

impl Framer {
    /// The framer of `width` x `height` frames, at the full frame.
    pub fn new(width: u32, height: u32, framing: Framing) -> Framer {
        let full = Crop {
            cx: f64::from(width) / 2.0,
            cy: f64::from(height) / 2.0,
            h: f64::from(height),
        };
        Framer {
            width: f64::from(width),
            height: f64::from(height),
            framing,
            current: full,
            goal: full,
            moving: false,
            last: None,
            last_face: None,
        }
    }

    /// The framing in use.
    pub fn framing(&self) -> Framing {
        self.framing
    }

    /// Changes the framing: the next face's target is that framing's (a
    /// target beyond the dead zone, so the crop moves to it smoothly).
    pub fn set_framing(&mut self, framing: Framing) {
        self.framing = framing;
    }

    fn full(&self) -> Crop {
        Crop {
            cx: self.width / 2.0,
            cy: self.height / 2.0,
            h: self.height,
        }
    }

    /// Snaps to the full frame and forgets the face and the clock.
    pub fn reset(&mut self) {
        let full = self.full();
        self.current = full;
        self.goal = full;
        self.moving = false;
        self.last = None;
        self.last_face = None;
    }

    /// The width of a crop of height `h`: the frame's aspect, never wider
    /// than the frame (`h * width / height` can round past it).
    fn crop_width(&self, h: f64) -> f64 {
        (h * self.width / self.height).min(self.width)
    }

    /// `c` with its zoom capped and inside the frame.
    fn clamp(&self, c: Crop) -> Crop {
        let h = c.h.clamp(self.height / MAX_ZOOM, self.height);
        let w = self.crop_width(h);
        Crop {
            cx: within(c.cx, w / 2.0, self.width - w / 2.0),
            cy: within(c.cy, h / 2.0, self.height - h / 2.0),
            h,
        }
    }

    /// The crop that frames `face` with the current framing.
    fn target(&self, face: &Rect) -> Crop {
        let (fx, fy) = face.centre();
        // The zoom capped first, so that the face keeps its line.
        let h =
            (face.height / self.framing.face_share()).clamp(self.height / MAX_ZOOM, self.height);
        self.clamp(Crop {
            cx: fx,
            cy: fy + (0.5 - FACE_LINE) * h,
            h,
        })
    }

    /// The target crop of `face`, as a rectangle (for tests and logs).
    pub fn target_rect(&self, face: &Rect) -> Rect {
        self.rect(self.target(face))
    }

    /// How far `b` is from `a`, as a share of `a`'s size.
    fn distance(&self, a: Crop, b: Crop) -> f64 {
        let w = self.crop_width(a.h);
        ((b.cx - a.cx).abs() / w)
            .max((b.cy - a.cy).abs() / a.h)
            .max((b.h - a.h).abs() / a.h)
    }

    fn rect(&self, c: Crop) -> Rect {
        let w = self.crop_width(c.h);
        Rect {
            x: c.cx - w / 2.0,
            y: c.cy - c.h / 2.0,
            width: w,
            height: c.h,
        }
    }

    /// The crop now.
    pub fn crop(&self) -> Rect {
        self.rect(self.current)
    }

    /// Whether the crop is the full frame (to within half a pixel): no
    /// scaling is needed.
    pub fn is_full(&self) -> bool {
        self.current.h >= self.height - 0.5
    }

    /// Moves the crop to the time `now` (seconds, of any origin, increasing)
    /// with `face`, the face seen on the frame of that time (or none), and
    /// returns it.
    pub fn update(&mut self, now: f64, face: Option<&Rect>) -> Rect {
        let dt = match self.last {
            Some(last) => (now - last).clamp(0.0, MAX_STEP_S),
            None => 0.0,
        };
        self.last = Some(now);
        match face.filter(|f| f.width > 0.0 && f.height > 0.0) {
            Some(face) => {
                self.last_face = Some(now);
                let target = self.target(face);
                if self.moving || self.distance(self.goal, target) > DEAD_ZONE {
                    self.goal = target;
                    self.moving = true;
                }
            }
            None => {
                let held = self
                    .last_face
                    .is_some_and(|t| now - t <= HOLD_S && now >= t);
                if !held {
                    let full = self.full();
                    if self.goal != full {
                        self.goal = full;
                        self.moving = true;
                    }
                }
            }
        }
        let k = 1.0 - (-dt / TIME_CONSTANT_S).exp();
        let (c, g) = (self.current, self.goal);
        self.current = self.clamp(Crop {
            cx: c.cx + (g.cx - c.cx) * k,
            cy: c.cy + (g.cy - c.cy) * k,
            h: c.h + (g.h - c.h) * k,
        });
        if self.moving && self.distance(self.current, self.goal) < SETTLE {
            self.moving = false;
        }
        if self.distance(self.current, self.goal) < 1e-4 {
            self.current = self.goal;
        }
        self.crop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1920;
    const H: u32 = 1080;

    fn face(cx: f64, cy: f64, size: f64) -> Rect {
        Rect {
            x: cx - size / 2.0,
            y: cy - size / 2.0,
            width: size,
            height: size,
        }
    }

    fn assert_aspect_and_inside(r: &Rect) {
        assert!(
            (r.width / r.height - 16.0 / 9.0).abs() < 1e-9,
            "aspect of {r:?}"
        );
        assert!(r.x >= -1e-9 && r.y >= -1e-9, "{r:?}");
        assert!(r.x + r.width <= 1920.0 + 1e-9, "{r:?}");
        assert!(r.y + r.height <= 1080.0 + 1e-9, "{r:?}");
        assert!(r.height >= 1080.0 / MAX_ZOOM - 1e-9, "{r:?}");
    }

    /// Runs `framer` for `seconds` at `fps` from `start` with `f`.
    fn run(framer: &mut Framer, start: f64, seconds: f64, fps: f64, f: Option<&Rect>) -> f64 {
        let n = (seconds * fps).round() as usize;
        let mut t = start;
        for i in 1..=n {
            t = start + i as f64 / fps;
            framer.update(t, f);
        }
        t
    }

    fn close(a: &Rect, b: &Rect, tol: f64) -> bool {
        (a.x - b.x).abs() < tol
            && (a.y - b.y).abs() < tol
            && (a.width - b.width).abs() < tol
            && (a.height - b.height).abs() < tol
    }

    #[test]
    fn the_framings_parse_and_name_themselves() {
        for f in Framing::ALL {
            assert_eq!(Framing::parse(f.as_str()), Some(f));
        }
        assert_eq!(Framing::parse("close"), None);
        assert_eq!(
            Framing::ALL.map(Framing::label),
            ["Tight", "Medium", "Wide"]
        );
    }

    #[test]
    fn a_new_framer_is_the_full_frame() {
        let f = Framer::new(W, H, Framing::Medium);
        assert!(f.is_full());
        assert_eq!(
            f.crop(),
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0
            }
        );
    }

    #[test]
    fn the_target_frames_the_face_by_the_framing() {
        // A face of 400 pixels: tight is at the zoom cap, medium and wide
        // frame it as they say.
        let face = face(900.0, 450.0, 400.0);
        let target = |framing| Framer::new(W, H, framing).target_rect(&face);
        let (tight, medium, wide) = (
            target(Framing::Tight),
            target(Framing::Medium),
            target(Framing::Wide),
        );
        for t in [&tight, &medium, &wide] {
            assert_aspect_and_inside(t);
        }
        assert!(tight.height < medium.height && medium.height < wide.height);
        assert!((tight.height - 1080.0 / MAX_ZOOM).abs() < 1e-9, "{tight:?}");
        assert!((400.0 / wide.height - Framing::Wide.face_share()).abs() < 1e-9);
        // Medium: the face's share of the crop, centred on the face, the
        // face a little above the middle.
        assert!((400.0 / medium.height - Framing::Medium.face_share()).abs() < 1e-9);
        assert!((medium.x + medium.width / 2.0 - 900.0).abs() < 1e-9);
        assert!(((450.0 - medium.y) / medium.height - FACE_LINE).abs() < 1e-9);
        // Capped, the face keeps its line too.
        assert!(((450.0 - tight.y) / tight.height - FACE_LINE).abs() < 1e-9);
    }

    #[test]
    fn every_framing_zooms_past_the_dead_zone_on_a_desk_face() {
        // Desk faces are 35-45 % of the frame's height.
        for pct in [0.35, 0.40, 0.45] {
            let size = H as f64 * pct;
            let mut row = Vec::new();
            for framing in Framing::ALL {
                let mut f = Framer::new(W, H, framing);
                let fc = face(960.0, 500.0, size);
                run(&mut f, 0.0, 10.0, 30.0, Some(&fc));
                row.push(H as f64 / f.crop().height);
            }
            // A 45 % face is exactly Wide's share (0.45 * 1080 / 486 = 1.0):
            // Wide has nothing to zoom on a face that large, by design.
            let zooming = if pct > 0.44 { &row[..2] } else { &row[..] };
            for z in zooming {
                assert!(*z > 1.0 + DEAD_ZONE + 1e-3, "{pct}: {row:?}");
            }
            assert!(row[0] >= row[1] && row[1] >= row[2], "{pct}: {row:?}");
        }
    }

    #[test]
    fn the_zoom_is_capped() {
        // A small face, tight: the crop would be far smaller.
        let f = Framer::new(W, H, Framing::Tight);
        let t = f.target_rect(&face(960.0, 540.0, 60.0));
        assert!((t.height - 1080.0 / MAX_ZOOM).abs() < 1e-9, "{t:?}");
        assert!((t.width - 1920.0 / MAX_ZOOM).abs() < 1e-9, "{t:?}");
        // A face too large for the framing: the full frame, no more.
        let t = f.target_rect(&face(960.0, 540.0, 900.0));
        assert!((t.height - 1080.0).abs() < 1e-9, "{t:?}");
        assert_aspect_and_inside(&t);
    }

    #[test]
    fn the_crop_stays_inside_the_frame_at_the_edges() {
        let f = Framer::new(W, H, Framing::Tight);
        for (cx, cy) in [
            (0.0, 0.0),
            (1920.0, 1080.0),
            (10.0, 1070.0),
            (1910.0, 5.0),
            (-300.0, 2000.0),
        ] {
            let t = f.target_rect(&face(cx, cy, 250.0));
            assert_aspect_and_inside(&t);
        }
        // A face at the left edge: the crop touches the edge.
        let t = f.target_rect(&face(30.0, 540.0, 250.0));
        assert!(t.x.abs() < 1e-9, "{t:?}");
        // Moving there keeps every crop of the way inside and of the
        // frame's aspect.
        let mut m = Framer::new(W, H, Framing::Tight);
        for i in 1..=120 {
            let r = m.update(f64::from(i) / 30.0, Some(&face(1900.0, 20.0, 250.0)));
            assert_aspect_and_inside(&r);
        }
    }

    #[test]
    fn the_crop_moves_to_a_face_slowly_and_reaches_it() {
        let mut f = Framer::new(W, H, Framing::Medium);
        let face = face(1200.0, 400.0, 220.0);
        let target = f.target_rect(&face);
        // The first frame starts the clock: no movement yet.
        f.update(0.0, Some(&face));
        assert!(f.is_full());
        let after_a_frame = f.update(1.0 / 30.0, Some(&face));
        assert!(!f.is_full());
        // A frame moves it a little only.
        assert!(after_a_frame.height > 1080.0 - 0.1 * (1080.0 - target.height) - 1.0);
        let t = run(&mut f, 1.0 / 30.0, 6.0, 30.0, Some(&face));
        assert!(close(&f.crop(), &target, 1.0), "{:?} {target:?}", f.crop());
        assert_aspect_and_inside(&f.update(t + 0.1, Some(&face)));
    }

    #[test]
    fn the_smoothing_is_by_time_not_by_frame_count() {
        let face = face(700.0, 500.0, 200.0);
        let mut at30 = Framer::new(W, H, Framing::Tight);
        let mut at60 = Framer::new(W, H, Framing::Tight);
        let mut at15 = Framer::new(W, H, Framing::Tight);
        for f in [&mut at30, &mut at60, &mut at15] {
            f.update(0.0, Some(&face));
        }
        run(&mut at30, 0.0, 1.0, 30.0, Some(&face));
        run(&mut at60, 0.0, 1.0, 60.0, Some(&face));
        run(&mut at15, 0.0, 1.0, 15.0, Some(&face));
        let (a, b, c) = (at30.crop(), at60.crop(), at15.crop());
        assert!(close(&a, &b, 0.5), "{a:?} {b:?}");
        assert!(close(&a, &c, 0.5), "{a:?} {c:?}");
        // Half way or so: it is still moving.
        assert!(!at30.is_full() && a.height > at30.target_rect(&face).height + 10.0);
    }

    #[test]
    fn a_small_movement_inside_the_dead_zone_does_not_move_the_crop() {
        let mut f = Framer::new(W, H, Framing::Medium);
        let start = face(960.0, 450.0, 220.0);
        f.update(0.0, Some(&start));
        let t = run(&mut f, 0.0, 10.0, 30.0, Some(&start));
        let rest = f.crop();
        // The head sways by about 5 % of the crop's width, and its size by
        // 4 %: the crop stays exactly where it is.
        let sway = 0.05 * rest.width;
        let t = run(
            &mut f,
            t,
            2.0,
            30.0,
            Some(&face(960.0 + sway, 450.0, 220.0 * 1.04)),
        );
        assert_eq!(f.crop(), rest);
        run(
            &mut f,
            t,
            2.0,
            30.0,
            Some(&face(960.0 - sway, 460.0, 220.0)),
        );
        assert_eq!(f.crop(), rest);
    }

    #[test]
    fn a_move_beyond_the_dead_zone_moves_and_the_hysteresis_finishes_it() {
        let mut f = Framer::new(W, H, Framing::Medium);
        let start = face(960.0, 450.0, 220.0);
        f.update(0.0, Some(&start));
        let t = run(&mut f, 0.0, 10.0, 30.0, Some(&start));
        let rest = f.crop();
        // A step of 15 % of the crop's width: it moves.
        let step = 0.15 * rest.width;
        let t = run(
            &mut f,
            t,
            0.3,
            30.0,
            Some(&face(960.0 + step, 450.0, 220.0)),
        );
        assert!(f.crop().x > rest.x + 1.0, "it did not start");
        // While it moves, the head comes back to 5 % from the start, inside
        // the dead zone of where it rested: the crop still follows it, to
        // the head, not back to the old rest.
        let back = face(960.0 + 0.05 * rest.width, 450.0, 220.0);
        let t = run(&mut f, t, 10.0, 30.0, Some(&back));
        let expected = f.target_rect(&back);
        assert!(close(&f.crop(), &expected, 1.0), "{:?}", f.crop());
        assert!(!close(&f.crop(), &rest, 5.0));
        // At rest again: the same small sway back is ignored now.
        let settled = f.crop();
        run(&mut f, t, 2.0, 30.0, Some(&start));
        assert_eq!(f.crop(), settled);
    }

    #[test]
    fn with_no_face_the_crop_holds_then_eases_back_to_the_full_frame() {
        let mut f = Framer::new(W, H, Framing::Tight);
        let face = face(1300.0, 400.0, 200.0);
        f.update(0.0, Some(&face));
        let t = run(&mut f, 0.0, 10.0, 30.0, Some(&face));
        let framed = f.crop();
        assert!(!f.is_full());
        // Lost: it holds for the hold time.
        let t = run(&mut f, t, HOLD_S - 0.1, 30.0, None);
        assert_eq!(f.crop(), framed);
        // Then it eases back, smoothly: not at once.
        let t = run(&mut f, t, 0.5, 30.0, None);
        assert!(f.crop().height > framed.height + 1.0);
        assert!(!f.is_full());
        run(&mut f, t, 10.0, 30.0, None);
        assert!(f.is_full(), "{:?}", f.crop());
        assert_aspect_and_inside(&f.crop());
    }

    #[test]
    fn a_face_that_comes_back_within_the_hold_keeps_the_crop() {
        let mut f = Framer::new(W, H, Framing::Tight);
        let face = face(1300.0, 400.0, 200.0);
        f.update(0.0, Some(&face));
        let t = run(&mut f, 0.0, 10.0, 30.0, Some(&face));
        let framed = f.crop();
        let t = run(&mut f, t, 1.5, 30.0, None);
        run(&mut f, t, 3.0, 30.0, Some(&face));
        assert_eq!(f.crop(), framed);
    }

    #[test]
    fn a_reset_snaps_to_the_full_frame() {
        let mut f = Framer::new(W, H, Framing::Tight);
        let face = face(400.0, 300.0, 200.0);
        f.update(0.0, Some(&face));
        run(&mut f, 0.0, 5.0, 30.0, Some(&face));
        assert!(!f.is_full());
        f.reset();
        assert!(f.is_full());
        // The clock restarts too: a frame long after moves nothing yet.
        f.update(1000.0, Some(&face));
        assert!(f.is_full());
        // And with no face after a reset, nothing is held.
        let mut g = Framer::new(W, H, Framing::Tight);
        g.update(0.0, None);
        run(&mut g, 0.0, 1.0, 30.0, None);
        assert!(g.is_full());
    }

    #[test]
    fn a_gap_in_time_counts_as_the_longest_step_and_time_going_back_as_none() {
        let face = face(700.0, 500.0, 200.0);
        let mut a = Framer::new(W, H, Framing::Tight);
        a.update(0.0, Some(&face));
        a.update(30.0, Some(&face));
        let mut b = Framer::new(W, H, Framing::Tight);
        b.update(0.0, Some(&face));
        b.update(MAX_STEP_S, Some(&face));
        assert_eq!(a.crop(), b.crop());
        let before = a.crop();
        a.update(10.0, Some(&face));
        assert_eq!(a.crop(), before);
    }

    #[test]
    fn a_change_of_framing_moves_to_the_new_target() {
        let mut f = Framer::new(W, H, Framing::Wide);
        let face = face(960.0, 450.0, 400.0);
        f.update(0.0, Some(&face));
        let t = run(&mut f, 0.0, 10.0, 30.0, Some(&face));
        let wide = f.crop();
        f.set_framing(Framing::Tight);
        assert_eq!(f.framing(), Framing::Tight);
        run(&mut f, t, 10.0, 30.0, Some(&face));
        assert!(f.crop().height < wide.height - 50.0, "{:?}", f.crop());
    }

    #[test]
    fn a_bound_range_never_panics() {
        assert_eq!(within(5.0, 0.0, 10.0), 5.0);
        assert_eq!(within(-1.0, 0.0, 10.0), 0.0);
        assert_eq!(within(872.0, 872.0000000000001, 871.9999999999999), 872.0);
        assert!(within(1.0, f64::NAN, 2.0).is_nan());
    }

    /// Every even frame size of the sweep, with every framing, faces
    /// anywhere (the corners, the edges, the centre, of any size) and the
    /// crop's moves between them: no panic, and every crop inside the frame
    /// with the zoom capped. Rounding made a full-height crop a hair wider
    /// than the frame at sizes like 1744x540 or 992x480.
    #[test]
    fn every_even_size_and_framing_keeps_the_crop_inside_without_panicking() {
        let heights = [240u32, 360, 480, 540, 600, 720, 768, 1080, 1200, 1440, 2160];
        for height in heights {
            for width in (320u32..=3840).step_by(2) {
                let (w, h) = (f64::from(width), f64::from(height));
                for framing in Framing::ALL {
                    let mut f = Framer::new(width, height, framing);
                    let inside = |r: &Rect| {
                        assert!(
                            r.x >= -1e-6
                                && r.y >= -1e-6
                                && r.x + r.width <= w + 1e-6
                                && r.y + r.height <= h + 1e-6
                                && r.width <= w
                                && r.height <= h
                                && r.height >= h / MAX_ZOOM - 1e-6,
                            "{width}x{height} {framing:?}: {r:?}"
                        );
                    };
                    inside(&f.crop());
                    let mut t = 0.0;
                    for (fx, fy, size) in [
                        (0.0, 0.0, h * 0.1),
                        (w, h, h * 0.1),
                        (w / 2.0, h / 2.0, h * 0.9),
                        (w, 0.0, h * 2.0),
                        (0.0, h, 1.0),
                        (w * 0.7, h * 0.3, h * 0.25),
                    ] {
                        let face = Rect {
                            x: fx - size / 2.0,
                            y: fy - size / 2.0,
                            width: size,
                            height: size,
                        };
                        inside(&f.target_rect(&face));
                        for _ in 0..4 {
                            t += 0.4;
                            inside(&f.update(t, Some(&face)));
                        }
                    }
                    // Then no face: the crop eases back to the full frame.
                    for _ in 0..12 {
                        t += 0.5;
                        inside(&f.update(t, None));
                    }
                }
            }
        }
    }
}
