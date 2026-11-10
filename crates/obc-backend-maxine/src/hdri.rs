//! The lighting environments of the relight stage, made in code.
//!
//! The relighting effect lights the person from a high-dynamic-range
//! environment map: an equirectangular image (360 degrees of longitude
//! across, 180 of latitude down, so 2:1) of linear RGB floats, where light
//! sources are much brighter than what merely reflects light. The SDK ships
//! none, so [`studio`] builds one per [`RelightLight`]: a deterministic
//! virtual studio with
//!
//! * a dim ambient, a little brighter above than below, so no side of the
//!   face goes black;
//! * a large, soft key light above and to one side of the camera, at the
//!   light's colour temperature: a broad lobe rather than a point, so
//!   shadows stay soft and faces are not lit harshly;
//! * a weaker, neutral fill on the other side, which keeps the shadow side
//!   open.
//!
//! Longitude 0 (the centre column) is what lies behind the person, the
//! direction the camera looks in (at `AnglePan` 0); the light that falls on
//! the face comes from around longitude 180, behind the camera. The key
//! light sits [`KEY_LONGITUDE_DEG`], 40 degrees off the camera's position,
//! so it lights the face from the left of the picture at pan 0 (the
//! person's right); the relight stage's pan turns the whole environment
//! around the vertical axis, a positive pan moving the light towards the
//! right of the picture. (These were measured on the effect, which does not
//! document its conventions.)

use obc_backend::RelightLight;

/// The width of every environment map, in pixels.
pub const WIDTH: u32 = 512;
/// The height of every environment map: half the width (2:1).
pub const HEIGHT: u32 = 256;

/// The longitude of the key light, in degrees from the centre column.
pub const KEY_LONGITUDE_DEG: f32 = -140.0;
/// The elevation of the key light, in degrees above the horizon.
pub const KEY_ELEVATION_DEG: f32 = 30.0;
/// The peak radiance of the key light (linear units).
pub const KEY_PEAK: f32 = 4.0;
/// The concentration of the key's lobe, `exp(k * (cos(a) - 1))` at angle
/// `a` from its centre: about 25 degrees to half intensity, a soft box.
const KEY_CONCENTRATION: f32 = 7.0;
/// The fill light, 55 degrees off the camera's position on the side away
/// from the key: longitude, elevation, peak radiance and concentration.
const FILL: (f32, f32, f32, f32) = (125.0, 10.0, 0.8, 3.0);
/// The ambient radiance at the horizon, the extra towards the zenith, and
/// the floor's radiance.
const AMBIENT: (f32, f32, f32) = (0.25, 0.15, 0.12);

/// A lighting environment: [`WIDTH`] x [`HEIGHT`] pixels of R, G, B linear
/// floats, rows top (the zenith) to bottom, tightly packed.
#[derive(Debug, Clone, PartialEq)]
pub struct Hdri {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 3` floats, R G B per pixel.
    pub data: Vec<f32>,
}

impl Hdri {
    /// The R, G, B of pixel (`x`, `y`).
    pub fn pixel(&self, x: u32, y: u32) -> [f32; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }
}

/// The linear RGB of a black body at `kelvin`, scaled to a luminance
/// (Rec. 709 weights) of 1: the colour of the key light. From the usual
/// fit of the CIE 1931 black-body locus to sRGB (valid 1000 K to 40000 K),
/// converted to linear light.
pub fn kelvin_rgb(kelvin: f32) -> [f32; 3] {
    let t = (kelvin.clamp(1000.0, 40000.0) / 100.0) as f64;
    let r = if t <= 66.0 {
        255.0
    } else {
        329.698_727_446 * (t - 60.0).powf(-0.133_204_759_2)
    };
    let g = if t <= 66.0 {
        99.470_802_586_1 * t.ln() - 161.119_568_166_1
    } else {
        288.122_169_528_3 * (t - 60.0).powf(-0.075_514_849_2)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.517_731_223_1 * (t - 10.0).ln() - 305.044_792_730_7
    };
    let lin = |c: f64| {
        let c = (c / 255.0).clamp(0.0, 1.0);
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    [(r / y) as f32, (g / y) as f32, (b / y) as f32]
}

/// The unit direction of longitude `lon` and elevation `elev` (radians):
/// x to the right, y up, z along longitude 0.
fn direction(lon: f32, elev: f32) -> [f32; 3] {
    [elev.cos() * lon.sin(), elev.sin(), elev.cos() * lon.cos()]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// The virtual studio for `light` (see the module documentation).
/// Deterministic: the same light always gives the same map.
pub fn studio(light: RelightLight) -> Hdri {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let key_colour = kelvin_rgb(light.kelvin());
    let key_dir = direction(
        KEY_LONGITUDE_DEG.to_radians(),
        KEY_ELEVATION_DEG.to_radians(),
    );
    let fill_dir = direction(FILL.0.to_radians(), FILL.1.to_radians());
    let mut data = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        // Pixel centres: +90 degrees at the top row, -90 at the bottom.
        let elev = std::f32::consts::FRAC_PI_2 - (y as f32 + 0.5) / h as f32 * std::f32::consts::PI;
        let ambient = if elev >= 0.0 {
            AMBIENT.0 + AMBIENT.1 * elev.sin()
        } else {
            // Down to the floor's level just below the horizon.
            AMBIENT.2 + (AMBIENT.0 - AMBIENT.2) * (1.0 + elev.sin()).powi(8)
        };
        for x in 0..w {
            let lon = ((x as f32 + 0.5) / w as f32 - 0.5) * std::f32::consts::TAU;
            let d = direction(lon, elev);
            let key = KEY_PEAK * (KEY_CONCENTRATION * (dot(d, key_dir) - 1.0)).exp();
            let fill = FILL.2 * (FILL.3 * (dot(d, fill_dir) - 1.0)).exp();
            for c in key_colour {
                data.push(ambient + fill + key * c);
            }
        }
    }
    Hdri {
        width: WIDTH,
        height: HEIGHT,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The column and row of the pixel nearest to longitude `lon` and
    /// elevation `elev` (degrees).
    fn at(lon: f32, elev: f32) -> (u32, u32) {
        let lon = (lon + 540.0) % 360.0 - 180.0;
        let x = ((lon / 360.0 + 0.5) * WIDTH as f32) as u32;
        let y = ((0.5 - elev / 180.0) * HEIGHT as f32) as u32;
        (x.min(WIDTH - 1), y.min(HEIGHT - 1))
    }

    fn luma(p: [f32; 3]) -> f32 {
        0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]
    }

    #[test]
    fn every_map_is_2_to_1_finite_and_positive() {
        for light in RelightLight::ALL {
            let m = studio(light);
            assert_eq!((m.width, m.height), (WIDTH, HEIGHT));
            assert_eq!(m.width, 2 * m.height, "2:1");
            assert_eq!(m.data.len(), (WIDTH * HEIGHT * 3) as usize);
            assert!(
                m.data.iter().all(|v| v.is_finite() && *v > 0.0),
                "{light}: every value finite and positive"
            );
            assert_eq!(m, studio(light), "{light}: deterministic");
        }
    }

    #[test]
    fn the_key_light_is_bright_soft_and_where_it_is_said_to_be() {
        let m = studio(RelightLight::Neutral);
        let (kx, ky) = at(KEY_LONGITUDE_DEG, KEY_ELEVATION_DEG);
        let key = luma(m.pixel(kx, ky));
        // The brightest pixel is the key's centre (within a pixel).
        let (mut best, mut bx, mut by) = (0.0f32, 0, 0);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let l = luma(m.pixel(x, y));
                if l > best {
                    (best, bx, by) = (l, x, y);
                }
            }
        }
        assert!(bx.abs_diff(kx) <= 1 && by.abs_diff(ky) <= 1, "{bx},{by}");
        let opposite = at(KEY_LONGITUDE_DEG + 180.0, -KEY_ELEVATION_DEG);
        assert!(key > 20.0 * luma(m.pixel(opposite.0, opposite.1)));
        // A soft box: still more than half as bright 15 degrees away, but
        // below a tenth of it 90 degrees of longitude away (on the side
        // away from the fill).
        let near = at(KEY_LONGITUDE_DEG + 15.0, KEY_ELEVATION_DEG);
        let far = at(KEY_LONGITUDE_DEG + 90.0, KEY_ELEVATION_DEG);
        assert!(luma(m.pixel(near.0, near.1)) > 0.5 * key);
        assert!(luma(m.pixel(far.0, far.1)) < 0.1 * key);
        // Above the horizon is brighter than the floor.
        let up = at(150.0, 60.0);
        let down = at(150.0, -60.0);
        assert!(luma(m.pixel(up.0, up.1)) > luma(m.pixel(down.0, down.1)));
    }

    #[test]
    fn warm_is_redder_and_cool_is_bluer_at_the_same_brightness() {
        let (kx, ky) = at(KEY_LONGITUDE_DEG, KEY_ELEVATION_DEG);
        let ratio = |light| {
            let p = studio(light).pixel(kx, ky);
            (p[0] / p[2], luma(p))
        };
        let (warm, lw) = ratio(RelightLight::Warm);
        let (neutral, ln) = ratio(RelightLight::Neutral);
        let (cool, lc) = ratio(RelightLight::Cool);
        assert!(warm > neutral && neutral > cool, "{warm} {neutral} {cool}");
        assert!(warm > 2.0, "3200 K is clearly warm: R/B {warm}");
        assert!(
            (0.8..1.25).contains(&cool),
            "6500 K is close to white: R/B {cool}"
        );
        for l in [lw, ln, lc] {
            assert!(
                (l - lw).abs() / lw < 0.02,
                "same brightness: {lw} {ln} {lc}"
            );
        }
    }

    #[test]
    fn black_body_colours_have_unit_luminance() {
        for k in [1000.0, 3200.0, 5000.0, 6500.0, 10000.0] {
            let c = kelvin_rgb(k);
            assert!(c.iter().all(|v| v.is_finite() && *v >= 0.0), "{k}: {c:?}");
            assert!((luma(c) - 1.0).abs() < 1e-4, "{k}: {c:?}");
        }
        let d65 = kelvin_rgb(6500.0);
        assert!(
            (d65[0] - d65[2]).abs() < 0.15,
            "6500 K is near white: {d65:?}"
        );
    }
}
