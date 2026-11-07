//! Fitting a source frame into the output frame: the crop rectangle of the
//! crop-to-fill policy, computed without GStreamer.
//!
//! Crop-to-fill keeps the centre of the source at the output's aspect ratio
//! and discards the excess on two opposite sides; `cudaconvertscale` then
//! scales the kept region to the output size. Letterbox crops nothing (the
//! scaler adds the borders). Every crop value is even, because the frames
//! being cropped are 4:2:0 (NV12/I420): an odd offset would split a chroma
//! sample. The two sides of a crop differ by at most one pixel pair.

use obc_core::AspectPolicy;

/// Pixels removed from each edge of a source frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Crop {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

impl Crop {
    /// Whether nothing is removed.
    pub fn is_empty(&self) -> bool {
        *self == Crop::default()
    }
}

/// Splits `total` pixels of crop into two even halves, the larger on the
/// second side. With an odd `total` (an odd source dimension) the odd pixel
/// is kept rather than cropped, so both values stay even.
fn split_even(total: u32) -> (u32, u32) {
    let first = (total / 2) & !1;
    let second = (total - first) & !1;
    (first, second)
}

/// `round(num / den)` to the nearest even integer, for `den > 0`.
fn round_to_even(num: u64, den: u64) -> u64 {
    (num + den) / (2 * den) * 2
}

/// The crop that fits a `src_w` x `src_h` frame into `out_w` x `out_h` under
/// `policy`. Crop-to-fill removes the excess width (a wider source) or
/// height (a taller one), centred; a source with the output's aspect ratio,
/// letterbox, and degenerate (zero) sizes crop nothing. Total: any sizes,
/// including a source one pixel wide or tall, give a crop that leaves at
/// least one pixel on each axis.
pub fn crop_for(policy: AspectPolicy, src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> Crop {
    if policy == AspectPolicy::Letterbox || src_w == 0 || src_h == 0 || out_w == 0 || out_h == 0 {
        return Crop::default();
    }
    let (sw, sh, ow, oh) = (
        u64::from(src_w),
        u64::from(src_h),
        u64::from(out_w),
        u64::from(out_h),
    );
    // Compare aspect ratios exactly: sw/sh vs ow/oh.
    let (src_side, out_side) = (sw * oh, ow * sh);
    if src_side > out_side {
        // Wider than the output: keep sh * ow / oh columns (at least 2,
        // never more than the source has).
        let keep = round_to_even(sh * ow, oh).max(2).min(sw);
        let (left, right) = split_even(src_w - keep as u32);
        Crop {
            left,
            right,
            ..Crop::default()
        }
    } else if src_side < out_side {
        // Taller than the output: keep sw * oh / ow rows.
        let keep = round_to_even(sw * oh, ow).max(2).min(sh);
        let (top, bottom) = split_even(src_h - keep as u32);
        Crop {
            top,
            bottom,
            ..Crop::default()
        }
    } else {
        Crop::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> Crop {
        crop_for(AspectPolicy::Crop, src_w, src_h, out_w, out_h)
    }

    fn crop(left: u32, right: u32, top: u32, bottom: u32) -> Crop {
        Crop {
            left,
            right,
            top,
            bottom,
        }
    }

    #[test]
    fn four_by_three_into_sixteen_by_nine_crops_top_and_bottom() {
        assert_eq!(fill(640, 480, 1920, 1080), crop(0, 0, 60, 60));
        assert_eq!(fill(1920, 1440, 1920, 1080), crop(0, 0, 180, 180));
        assert_eq!(fill(1440, 1080, 1920, 1080), crop(0, 0, 134, 136));
    }

    #[test]
    fn same_aspect_crops_nothing() {
        assert!(fill(1920, 1080, 1920, 1080).is_empty());
        assert!(fill(1280, 720, 1920, 1080).is_empty());
        assert!(fill(3840, 2160, 1920, 1080).is_empty());
        assert!(fill(640, 480, 1024, 768).is_empty());
    }

    #[test]
    fn wider_source_crops_left_and_right() {
        // 16:9 into 4:3: keep 1440 of 1920 columns.
        assert_eq!(fill(1920, 1080, 640, 480), crop(240, 240, 0, 0));
        // 21:9-ish 2560x1080 into 16:9: keep 1920.
        assert_eq!(fill(2560, 1080, 1920, 1080), crop(320, 320, 0, 0));
    }

    #[test]
    fn crops_are_even_and_balanced() {
        for &(sw, sh, ow, oh) in &[
            (640u32, 480u32, 1920u32, 1080u32),
            (960, 540, 1280, 720),
            (1280, 960, 1920, 1080),
            (800, 600, 1920, 1080),
            (1024, 768, 1920, 1080),
            (1920, 1080, 1000, 1000),
            (1922, 1080, 1920, 1080),
            (642, 480, 640, 480),
            (641, 481, 1920, 1080),
            (1921, 1081, 640, 480),
            (2, 2, 1920, 1080),
        ] {
            let c = fill(sw, sh, ow, oh);
            for v in [c.left, c.right, c.top, c.bottom] {
                assert_eq!(v % 2, 0, "{sw}x{sh}->{ow}x{oh}: {c:?}");
            }
            assert!(c.left.abs_diff(c.right) <= 2, "{c:?}");
            assert!(c.top.abs_diff(c.bottom) <= 2, "{c:?}");
            assert!(c.left + c.right < sw && c.top + c.bottom < sh, "{c:?}");
            // Only one axis is cropped.
            assert!(c.left + c.right == 0 || c.top + c.bottom == 0, "{c:?}");
            // The kept region is within 2 px (one pair, plus rounding) of the
            // output aspect ratio.
            let (kw, kh) = (
                f64::from(sw - c.left - c.right),
                f64::from(sh - c.top - c.bottom),
            );
            let want = f64::from(ow) / f64::from(oh);
            let err_px = if kw / kh > want {
                kw - kh * want
            } else {
                kh - kw / want
            };
            assert!(
                err_px.abs() <= 2.0,
                "{sw}x{sh}->{ow}x{oh}: {c:?} ({err_px})"
            );
        }
    }

    #[test]
    fn odd_source_keeps_the_odd_pixel() {
        // 481 rows, keep 360: 121 to remove; both sides even, one row kept.
        assert_eq!(fill(640, 481, 1920, 1080), crop(0, 0, 60, 60));
        // 1922 wide into 16:9 (keep 1920): 2 columns, one per side would be
        // odd, so both go to the second side.
        assert_eq!(fill(1922, 1080, 1920, 1080), crop(0, 2, 0, 0));
    }

    #[test]
    fn letterbox_and_degenerate_sizes_crop_nothing() {
        assert!(crop_for(AspectPolicy::Letterbox, 640, 480, 1920, 1080).is_empty());
        assert!(fill(0, 480, 1920, 1080).is_empty());
        assert!(fill(640, 480, 0, 1080).is_empty());
    }

    /// Sources one or two pixels wide or tall, and extreme ratios, never
    /// panic and never crop the whole source away.
    #[test]
    fn tiny_and_extreme_sizes_are_total() {
        for &(sw, sh, ow, oh) in &[
            (1u32, 1u32, 1920u32, 1080u32),
            (1, 1, 1, 2),
            (1, 1, 2, 1),
            (1, 1080, 1920, 1080),
            (1920, 1, 1920, 1080),
            (1, 3, 1920, 1080),
            (3, 1, 1080, 1920),
            (2, 1, 1, 1080),
            (1, 2, 1920, 1),
            (u32::MAX, 1, 2, 2),
            (1, u32::MAX, 2, 2),
            (u32::MAX, u32::MAX, 1, u32::MAX),
        ] {
            let c = fill(sw, sh, ow, oh);
            assert!(
                u64::from(c.left) + u64::from(c.right) < u64::from(sw)
                    && u64::from(c.top) + u64::from(c.bottom) < u64::from(sh),
                "{sw}x{sh}->{ow}x{oh}: {c:?}"
            );
            for v in [c.left, c.right, c.top, c.bottom] {
                assert_eq!(v % 2, 0, "{sw}x{sh}->{ow}x{oh}: {c:?}");
            }
        }
        // A one-pixel-wide source into a wider output keeps its only column
        // and crops rows down towards the output's aspect ratio.
        assert_eq!(fill(1, 1080, 1920, 1080), crop(0, 0, 538, 540));
        assert!(fill(1, 1, 1920, 1080).is_empty());
    }
}
