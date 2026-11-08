//! Hardening the green-screen mask before it is used.
//!
//! The `GreenScreen` model writes a soft 8-bit mask: 0 is background, 255
//! foreground, and the room behind the subject comes out as small non-zero
//! values (and the subject's interior tops out below 255). Used as it is,
//! replace and blur would let a faint copy of the room through. The backend
//! therefore remaps every mask value through a fixed table on the GPU
//! ([`obc_gpu::Lut8`]) and gives the remapped mask, never the raw one, to
//! both effects.
//!
//! The table is a linear ramp: values up to [`MATTE_LO`] become 0 (128 is
//! the model's decision boundary, confidence 0.5), values from [`MATTE_HI`]
//! up become 255, and values in between rise linearly, which keeps a soft
//! edge without flicker. The bounds are fixed, not a setting: a lower bound
//! below 128 would let the room through again.

/// Mask values up to this one become 0 (background).
pub const MATTE_LO: u8 = 128;

/// Mask values from this one up become 255 (foreground).
pub const MATTE_HI: u8 = 224;

// The ramp must never start below the decision boundary (that would let the
// room through), must leave the interior room to reach 255, and must rise.
const _: () = assert!(MATTE_LO >= 128, "MATTE_LO below 128 lets the room through");
const _: () = assert!(
    MATTE_HI <= 250,
    "MATTE_HI above 250 leaves the interior soft"
);
const _: () = assert!(MATTE_LO < MATTE_HI, "the ramp must rise");

/// The table the backend applies: [`hardening_lut`] of the fixed bounds.
pub const HARDENING_LUT: [u8; 256] = hardening_lut(MATTE_LO, MATTE_HI);

/// The ramp from `lo` to `hi` as a 256-entry table: `m <= lo` maps to 0,
/// `m >= hi` to 255, and values between to
/// `round((m - lo) * 255 / (hi - lo))`, rounding halves up.
///
/// # Panics
/// If `lo >= hi` (at compile time when used in a constant).
pub const fn hardening_lut(lo: u8, hi: u8) -> [u8; 256] {
    assert!(lo < hi, "the ramp must rise");
    let (lo, span) = (lo as u32, (hi - lo) as u32);
    let mut lut = [0u8; 256];
    let mut m = 0u32;
    while m < 256 {
        if m >= lo {
            let v = ((m - lo) * 255 + span / 2) / span;
            lut[m as usize] = if v > 255 { 255 } else { v as u8 };
        }
        m += 1;
    }
    lut
}

/// The host reference of the GPU kernel: `dst[y][x] = lut[src[y][x]]` for
/// `x < width`, `y < height`, rows `src_pitch` and `dst_pitch` bytes apart.
/// The bytes between `width` and each pitch are not touched. For tests and
/// tools, not for the streaming path.
///
/// # Panics
/// If a pitch is below `width` or a slice is too short for its rectangle.
pub fn apply_lut(
    lut: &[u8; 256],
    src: &[u8],
    src_pitch: usize,
    dst: &mut [u8],
    dst_pitch: usize,
    width: usize,
    height: usize,
) {
    assert!(
        src_pitch >= width && dst_pitch >= width,
        "pitches {src_pitch} and {dst_pitch} below the width {width}"
    );
    for y in 0..height {
        let s = &src[y * src_pitch..y * src_pitch + width];
        let d = &mut dst[y * dst_pitch..y * dst_pitch + width];
        for (d, s) in d.iter_mut().zip(s) {
            *d = lut[*s as usize];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixed_ramp_has_the_documented_entries() {
        let l = HARDENING_LUT;
        assert_eq!((MATTE_LO, MATTE_HI), (128, 224));
        for (m, want) in [
            (0, 0),
            (1, 0),
            (127, 0),
            (128, 0),
            (129, 3),
            (176, 128),
            (223, 252),
            (224, 255),
            (254, 255),
            (255, 255),
        ] {
            assert_eq!(l[m], want, "lut[{m}]");
        }
        assert!(l[..=128].iter().all(|&v| v == 0), "the room side is 0");
        assert!(l[224..].iter().all(|&v| v == 255), "the interior is opaque");
    }

    #[test]
    fn the_ramp_is_monotone_and_matches_a_float_reference() {
        for (lo, hi) in [
            (MATTE_LO, MATTE_HI),
            (128, 192),
            (128, 240),
            (0, 255),
            (10, 11),
        ] {
            let l = hardening_lut(lo, hi);
            assert!(
                l.windows(2).all(|w| w[0] <= w[1]),
                "[{lo},{hi}] not monotone"
            );
            for (m, &v) in l.iter().enumerate() {
                let want = if m < lo as usize {
                    0.0
                } else {
                    ((m as f64 - lo as f64) * 255.0 / (hi as f64 - lo as f64) + 0.5)
                        .floor()
                        .min(255.0)
                };
                assert_eq!(v as f64, want, "[{lo},{hi}] lut[{m}]");
            }
        }
    }

    #[test]
    #[should_panic(expected = "the ramp must rise")]
    fn a_ramp_that_does_not_rise_is_refused() {
        let _ = hardening_lut(200, 200);
    }

    /// A `w x h` plane with rows `pitch` apart holding a pattern, and
    /// `fill` in the padding.
    fn plane(w: usize, h: usize, pitch: usize, fill: u8) -> Vec<u8> {
        let mut p = vec![fill; pitch * h];
        for y in 0..h {
            for x in 0..w {
                p[y * pitch + x] = (x * 3 + y * 11) as u8;
            }
        }
        p
    }

    #[test]
    fn apply_lut_maps_pitched_and_odd_planes_and_leaves_padding() {
        let lut = HARDENING_LUT;
        for (w, h, sp, dp) in [
            (7, 5, 7, 7),
            (7, 5, 16, 9),
            (513, 289, 576, 1024),
            (1, 1, 1, 4),
        ] {
            let src = plane(w, h, sp, 0x5A);
            let mut dst = vec![0xABu8; dp * h];
            apply_lut(&lut, &src, sp, &mut dst, dp, w, h);
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(
                        dst[y * dp + x],
                        lut[src[y * sp + x] as usize],
                        "{w}x{h} ({x},{y})"
                    );
                }
                assert!(
                    dst[y * dp + w..(y + 1) * dp].iter().all(|&b| b == 0xAB),
                    "{w}x{h}: padding of row {y} written"
                );
            }
        }
    }

    #[test]
    #[should_panic(expected = "below the width")]
    fn apply_lut_refuses_a_pitch_below_the_width() {
        let mut dst = [0u8; 8];
        apply_lut(&HARDENING_LUT, &[0; 8], 2, &mut dst, 4, 4, 2);
    }
}
