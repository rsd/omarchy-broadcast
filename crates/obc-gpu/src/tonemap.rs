//! The HDR-to-SDR tone map: a pitched RGB10A2 image in PQ and BT.2020 (what
//! NVIDIA's TrueHDR effect writes) to RGBA8 in sRGB and BT.709, on the GPU
//! ([`PqToSdr`]), with the same function per pixel in Rust ([`pq_to_sdr`]),
//! which is the reference the kernel is tested against.
//!
//! # The function
//!
//! For one pixel ([`pq_to_sdr`], `kernels/pq_to_sdr.cu`), all in `f32`:
//!
//! 1. **Decode.** The 10-bit codes ([`unpack_rgb10a2`]) over 1023 are PQ
//!    values (SMPTE ST 2084); the PQ EOTF ([`pq_eotf`]) gives each channel
//!    in nits.
//! 2. **Anchor SDR white.** The nits are divided by the gain at which the
//!    source placed SDR diffuse white ([`DIFFUSE_GAIN`] for TrueHDR, which
//!    maps SDR linear 1.0 to about 240 nits and expands only above that).
//!    The mid-tones go back to their SDR level, so what remains of the HDR
//!    pass is its highlights, shadows and colour, not an exposure shift.
//!    SDR white itself lands in the roll-off of step 4, a little below 255
//!    (238 at TrueHDR's default peak).
//! 3. **Gamut.** BT.2020 to BT.709 linear ([`BT2020_TO_BT709`]); negative
//!    channels (colours outside BT.709) are clipped to 0.
//! 4. **Tone map.** The BT.709 luminance goes through the ITU-R BT.2390
//!    EETF ([`bt2390_eetf`]) from the source peak (`peak / gain`) down to
//!    [`SDR_WHITE_NITS`], and the three channels are scaled by the ratio of
//!    the new luminance to the old (the chromaticity is kept). The source
//!    peak must lie above SDR white (`peak > gain * 100`), or the curve is
//!    not defined: [`ToneMap::new`] refuses any other pair, and both the
//!    reference and the kernel take only a [`ToneMap`].
//! 5. **Clip.** A pixel whose largest channel is above SDR white is scaled
//!    down as a whole, so no channel clips alone (no hue shift).
//! 6. **Encode.** Over [`SDR_WHITE_NITS`], clipped to [0, 1], the sRGB
//!    encode ([`srgb_oetf`]): the transfer the source RGBA had, as
//!    `NvCVImage_TransferFromYUV` produced it (the camera's gamma-encoded
//!    values, read as sRGB). Times 255, rounded to nearest (ties to even),
//!    alpha 255.
//!
//! The kernel is compiled without fused multiply-adds, as Rust computes, so
//! the two agree to within one code value (the GPU's `powf` is not
//! correctly rounded).
//!
//! # The kernel
//!
//! As [`crate::Lut8`]: PTX text (`kernels/pq_to_sdr.ptx`, generated from
//! `kernels/pq_to_sdr.cu`, which has the procedure), embedded and compiled
//! by the driver at load, pinned to ISA `.version 7.0` and `.target sm_75`.

use std::ffi::{CStr, c_void};

use crate::driver::{CuFunction, CuModule};
use crate::kernel::module_load_hint;
use crate::{CuDevicePtr, CuStream, GpuError, driver};

/// The embedded PTX of `obc_pq_to_sdr`, NUL-terminated for
/// `cuModuleLoadData`.
pub const PQ_TO_SDR_PTX: &str = concat!(include_str!("kernels/pq_to_sdr.ptx"), "\0");

/// The kernel's entry point in [`PQ_TO_SDR_PTX`].
pub const PQ_TO_SDR_ENTRY: &CStr = c"obc_pq_to_sdr";

/// The thread block every [`PqToSdr::launch`] uses: 32 x 8 threads.
pub const PQ_TO_SDR_BLOCK: (u32, u32) = (32, 8);

/// SDR reference white, in nits: the tone map's target peak.
pub const SDR_WHITE_NITS: f32 = 100.0;

/// Where TrueHDR puts SDR diffuse white, as a multiple of
/// [`SDR_WHITE_NITS`]: measured on camera stills at its default settings,
/// `nits / (100 * linear SDR)` is 2.36 to 2.46 over linear 0.05 to 0.4.
pub const DIFFUSE_GAIN: f32 = 2.4;

/// The BT.2020 to BT.709 matrix on linear RGB (rows: R, G, B of BT.709).
pub const BT2020_TO_BT709: [[f32; 3]; 3] = [
    [1.6605, -0.5876, -0.0728],
    [-0.1246, 1.1329, -0.0083],
    [-0.0182, -0.1006, 1.1187],
];

/// The BT.709 luminance weights.
pub const BT709_LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// The two parameters of the tone map, checked once: `gain`, where the
/// source put SDR white over [`SDR_WHITE_NITS`] ([`DIFFUSE_GAIN`]), and
/// `peak`, the source's peak luminance in nits (TrueHDR's `Luminance`).
/// [`pq_to_sdr`] and [`PqToSdr::launch`] take nothing else, so neither runs
/// the curve where it is not defined.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneMap {
    gain: f32,
    peak: f32,
}

impl ToneMap {
    /// The tone map for `gain` and `peak`. Refused unless both are finite,
    /// `gain` is positive and the source peak lies above SDR white
    /// (`peak > gain * SDR_WHITE_NITS`): at or below it the BT.2390 knee
    /// starts at or past the target peak and the curve divides by zero.
    pub fn new(gain: f32, peak: f32) -> Result<ToneMap, GpuError> {
        if gain.is_finite() && peak.is_finite() && gain > 0.0 && peak > gain * SDR_WHITE_NITS {
            Ok(ToneMap { gain, peak })
        } else {
            Err(GpuError::InvalidArgument(format!(
                "tone map gain {gain} and peak {peak}: the gain must be positive and the \
                 peak above {SDR_WHITE_NITS} nits times the gain"
            )))
        }
    }

    /// Where the source put SDR white, over [`SDR_WHITE_NITS`].
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// The source's peak luminance, in nits.
    pub fn peak(&self) -> f32 {
        self.peak
    }
}

const PQ_M1: f32 = 2610.0 / 16384.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

/// The R, G, B codes (0 to 1023) and the alpha code (0 to 3) of an RGB10A2
/// pixel, as NvCVImage stores `NVCV_RGB10A2` (`NVCV_P32`): one
/// little-endian word, R in bits 0-9, G in 10-19, B in 20-29, A in 30-31.
pub fn unpack_rgb10a2(p: u32) -> [u16; 4] {
    [
        (p & 1023) as u16,
        ((p >> 10) & 1023) as u16,
        ((p >> 20) & 1023) as u16,
        (p >> 30) as u16,
    ]
}

/// The RGB10A2 word of `r`, `g`, `b` (each masked to 10 bits) and `a` (to
/// 2 bits); the inverse of [`unpack_rgb10a2`].
pub fn pack_rgb10a2(r: u16, g: u16, b: u16, a: u16) -> u32 {
    (u32::from(r) & 1023)
        | ((u32::from(g) & 1023) << 10)
        | ((u32::from(b) & 1023) << 20)
        | ((u32::from(a) & 3) << 30)
}

/// SMPTE ST 2084 EOTF: a PQ value in [0, 1] (clipped) to nits, 0 to 10000.
pub fn pq_eotf(e: f32) -> f32 {
    let p = e.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
    10000.0 * ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1)
}

/// The inverse of [`pq_eotf`]: nits (clipped to 0 to 10000) to a PQ value.
pub fn pq_oetf(nits: f32) -> f32 {
    let y = (nits.clamp(0.0, 10000.0) / 10000.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y)).powf(PQ_M2)
}

/// The ITU-R BT.2390 EETF, black at 0 on both sides: `nits` of a signal
/// peaking at `src_peak` to a signal peaking at [`SDR_WHITE_NITS`]. In the
/// PQ domain, normalised to the source peak: identity below the knee start
/// `ks = 1.5 * maxlum - 0.5` (`maxlum` the target peak over the source's),
/// a Hermite spline from there to the target peak above it.
pub fn bt2390_eetf(nits: f32, src_peak: f32) -> f32 {
    let ps = pq_oetf(src_peak);
    let maxlum = pq_oetf(SDR_WHITE_NITS) / ps;
    let ks = 1.5 * maxlum - 0.5;
    let mut e = pq_oetf(nits) / ps;
    if e >= ks {
        let t = ((e - ks) / (1.0 - ks)).clamp(0.0, 1.0);
        let t2 = t * t;
        let t3 = t2 * t;
        e = (2.0 * t3 - 3.0 * t2 + 1.0) * ks
            + (t3 - 2.0 * t2 + t) * (1.0 - ks)
            + (-2.0 * t3 + 3.0 * t2) * maxlum;
    }
    pq_eotf(e * ps)
}

/// The sRGB encode of a linear value in [0, 1] (clipped).
pub fn srgb_oetf(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round_ties_even() as u8
}

/// The tone map `tm` of one RGB10A2 pixel (see the module documentation):
/// the RGBA8 bytes, R first, alpha 255.
pub fn pq_to_sdr(p: u32, tm: ToneMap) -> [u8; 4] {
    let (gain, peak) = (tm.gain, tm.peak);
    let [r, g, b, _] = unpack_rgb10a2(p);
    // 1, 2: PQ to nits, over the gain.
    let nits = [r, g, b].map(|c| pq_eotf(f32::from(c) / 1023.0) / gain);
    // 3: BT.2020 to BT.709, out-of-gamut channels clipped.
    let m = &BT2020_TO_BT709;
    let mut lin = [0f32; 3];
    for (i, row) in m.iter().enumerate() {
        lin[i] = (row[0] * nits[0] + row[1] * nits[1] + row[2] * nits[2]).max(0.0);
    }
    // 4: the EETF on the luminance, the chromaticity kept.
    let lum = BT709_LUMA[0] * lin[0] + BT709_LUMA[1] * lin[1] + BT709_LUMA[2] * lin[2];
    let scale = bt2390_eetf(lum, peak / gain) / lum.max(1e-4);
    let lin = lin.map(|c| c * scale);
    // 5: no channel above white.
    let limit = (lin[0].max(lin[1]).max(lin[2]) / SDR_WHITE_NITS).max(1.0);
    // 6: sRGB, 8 bits.
    let [r, g, b] = lin.map(|c| to_u8(srgb_oetf(c / limit / SDR_WHITE_NITS)));
    [r, g, b, 255]
}

/// The `obc_pq_to_sdr` kernel, loaded into the context that was current
/// when it was created: [`pq_to_sdr`] from a pitched RGB10A2 image to a
/// pitched RGBA8 image of the same size. Its module is unloaded on drop,
/// under the rules of [`crate::Lut8`] (the loading context current, the
/// work that uses it completed).
pub struct PqToSdr {
    module: CuModule,
    function: CuFunction,
}

// SAFETY: as for `Lut8`: a module and its functions belong to a context,
// not to a thread.
unsafe impl Send for PqToSdr {}

impl PqToSdr {
    /// Loads the embedded kernel into the current context (the driver
    /// compiles the PTX here, so call it while nothing waits for a frame).
    pub fn load() -> Result<PqToSdr, GpuError> {
        let ptx = CStr::from_bytes_with_nul(PQ_TO_SDR_PTX.as_bytes())
            .expect("the embedded PTX is NUL-terminated and has no inner NUL");
        PqToSdr::load_ptx(ptx)
    }

    /// Loads `ptx`, which must define [`PQ_TO_SDR_ENTRY`] with the
    /// parameters of `kernels/pq_to_sdr.cu`. For tests that need a load to
    /// fail with a real driver error; use [`PqToSdr::load`].
    #[doc(hidden)]
    pub fn load_ptx(ptx: &CStr) -> Result<PqToSdr, GpuError> {
        let d = driver()?;
        let mut module: CuModule = std::ptr::null_mut();
        // SAFETY: `module` is a valid out-pointer; `ptx` is a NUL-terminated
        // image that outlives the call.
        let rc = unsafe { (d.cuModuleLoadData)(&mut module, ptx.as_ptr().cast::<c_void>()) };
        if let Err(mut e) = d.check("cuModuleLoadData", rc) {
            if let (GpuError::Call { description, .. }, Some(hint)) = (&mut e, module_load_hint(rc))
            {
                description.push_str("; ");
                description.push_str(hint);
            }
            return Err(e);
        }
        // Owned from here: an error below unloads the module.
        let mut kernel = PqToSdr {
            module,
            function: std::ptr::null_mut(),
        };
        // SAFETY: valid out-pointer; live module; static NUL-terminated name.
        d.check("cuModuleGetFunction", unsafe {
            (d.cuModuleGetFunction)(
                &mut kernel.function,
                kernel.module,
                PQ_TO_SDR_ENTRY.as_ptr(),
            )
        })?;
        Ok(kernel)
    }

    /// Enqueues the tone map `tm` of the `width x height` RGB10A2 image at
    /// `src` (rows `src_pitch` bytes apart) into the RGBA8 image at `dst`
    /// (rows `dst_pitch` bytes apart) on `stream`, as [`pq_to_sdr`] maps
    /// each pixel. Returns once the launch is queued.
    ///
    /// # Safety
    /// With the kernel's context current: both rectangles (`height` rows of
    /// `4 * width` bytes at their pitches) must lie inside live device
    /// allocations of that context, 4-byte aligned, and must not overlap;
    /// `stream` must be null or a live stream of the context. Nothing else
    /// may write those bytes until the kernel has run.
    #[allow(clippy::too_many_arguments)] // mirrors the kernel's parameters
    pub unsafe fn launch(
        &self,
        src: CuDevicePtr,
        src_pitch: usize,
        dst: CuDevicePtr,
        dst_pitch: usize,
        width: u32,
        height: u32,
        tm: ToneMap,
        stream: CuStream,
    ) -> Result<(), GpuError> {
        let row = width as usize * 4;
        if width == 0 || height == 0 || src_pitch < row || dst_pitch < row {
            return Err(GpuError::InvalidArgument(format!(
                "tone map over {width}x{height} with pitches {src_pitch} and {dst_pitch}"
            )));
        }
        let d = driver()?;
        // In order and with the PTX's widths: u64 src, u64 src_pitch,
        // u64 dst, u64 dst_pitch, u32 width, u32 height, f32 gain, f32 peak.
        let mut a_src: u64 = src;
        let mut a_src_pitch: u64 = src_pitch as u64;
        let mut a_dst: u64 = dst;
        let mut a_dst_pitch: u64 = dst_pitch as u64;
        let mut a_width: u32 = width;
        let mut a_height: u32 = height;
        let mut a_gain: f32 = tm.gain;
        let mut a_peak: f32 = tm.peak;
        let mut params: [*mut c_void; 8] = [
            (&raw mut a_src).cast(),
            (&raw mut a_src_pitch).cast(),
            (&raw mut a_dst).cast(),
            (&raw mut a_dst_pitch).cast(),
            (&raw mut a_width).cast(),
            (&raw mut a_height).cast(),
            (&raw mut a_gain).cast(),
            (&raw mut a_peak).cast(),
        ];
        let block = PQ_TO_SDR_BLOCK;
        let grid = (width.div_ceil(block.0), height.div_ceil(block.1));
        // SAFETY: the function is live in the current context (caller);
        // `params` holds one pointer per kernel parameter, each to a local
        // of the parameter's width, alive for the call; the caller
        // guarantees the memory and the stream.
        d.check("cuLaunchKernel", unsafe {
            (d.cuLaunchKernel)(
                self.function,
                grid.0,
                grid.1,
                1,
                block.0,
                block.1,
                1,
                0,
                stream,
                params.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        })
    }
}

impl Drop for PqToSdr {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            // SAFETY: loaded by us, unloaded once, its context current
            // (type contract).
            unsafe { (d.cuModuleUnload)(self.module) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TrueHDR's default `Luminance`, in nits.
    const PEAK: f32 = 650.0;

    fn default_map() -> ToneMap {
        ToneMap::new(DIFFUSE_GAIN, PEAK).unwrap()
    }

    fn grey(code: u16) -> u32 {
        pack_rgb10a2(code, code, code, 3)
    }

    fn tm(r: u16, g: u16, b: u16) -> [u8; 3] {
        let [r, g, b, a] = pq_to_sdr(pack_rgb10a2(r, g, b, 3), default_map());
        assert_eq!(a, 255);
        [r, g, b]
    }

    #[test]
    fn rgb10a2_has_red_in_the_low_bits_and_alpha_in_the_top_two() {
        assert_eq!(unpack_rgb10a2(0x3FF), [1023, 0, 0, 0]);
        assert_eq!(unpack_rgb10a2(0x3FF << 10), [0, 1023, 0, 0]);
        assert_eq!(unpack_rgb10a2(0x3FF << 20), [0, 0, 1023, 0]);
        assert_eq!(unpack_rgb10a2(0xC000_0000), [0, 0, 0, 3]);
        // A warm pixel as TrueHDR writes it, opaque.
        let p = (3 << 30) | (400 << 20) | (500 << 10) | 600;
        assert_eq!(unpack_rgb10a2(p), [600, 500, 400, 3]);
        assert_eq!(pack_rgb10a2(600, 500, 400, 3), p);
        // The word's bytes as they lie in memory, little-endian.
        assert_eq!(
            unpack_rgb10a2(u32::from_le_bytes([0x58, 0x02, 0x40, 0xD9])),
            [600, 0, 404, 3]
        );
        // Out-of-range inputs are masked, not carried into a neighbour.
        assert_eq!(pack_rgb10a2(1024, 0, 0, 4), 0);
    }

    #[test]
    fn the_pq_curve_has_its_standard_anchors() {
        // SMPTE ST 2084: 100 nits at PQ 0.508, 1000 at 0.752, 10000 at 1.
        assert!(
            (pq_oetf(100.0) - 0.508_078).abs() < 1e-5,
            "{}",
            pq_oetf(100.0)
        );
        assert!(
            (pq_oetf(1000.0) - 0.751_827).abs() < 1e-5,
            "{}",
            pq_oetf(1000.0)
        );
        assert!((pq_oetf(10000.0) - 1.0).abs() < 1e-6);
        assert_eq!(pq_eotf(0.0), 0.0);
        for nits in [0.5f32, 10.0, 100.0, 240.0, 1000.0, 4000.0] {
            let back = pq_eotf(pq_oetf(nits));
            assert!((back - nits).abs() / nits < 1e-4, "{nits}: {back}");
        }
        // The 10-bit codes of 100, 240 and 1000 nits.
        for (nits, code) in [(100.0, 520), (240.0, 612), (1000.0, 769)] {
            assert_eq!((pq_oetf(nits) * 1023.0).round() as u16, code, "{nits}");
        }
    }

    #[test]
    fn the_eetf_keeps_the_mid_tones_and_rolls_off_to_white() {
        let src_peak = PEAK / DIFFUSE_GAIN;
        // Below the knee: unchanged.
        for nits in [1.0f32, 10.0, 30.0] {
            let out = bt2390_eetf(nits, src_peak);
            assert!((out - nits).abs() / nits < 1e-3, "{nits}: {out}");
        }
        // The source peak lands on SDR white, anything above it too.
        assert!((bt2390_eetf(src_peak, src_peak) - SDR_WHITE_NITS).abs() < 0.05);
        assert!((bt2390_eetf(2000.0, src_peak) - SDR_WHITE_NITS).abs() < 0.05);
        // Monotonic over the knee.
        let mut last = 0.0;
        for i in 1..=100 {
            let out = bt2390_eetf(src_peak * i as f32 / 100.0, src_peak);
            assert!(out >= last, "{i}: {out} < {last}");
            last = out;
        }
    }

    /// The values a reference implementation of the tone map in Python
    /// gives for the same pixels (with a peak of 650, or 400 nits),
    /// computed with numpy and copied here.
    #[test]
    fn the_tone_map_matches_the_python_reference() {
        // Black stays black.
        assert_eq!(tm(0, 0, 0), [0, 0, 0]);
        // PQ 100 nits (code 520) is 41.7 nits after the gain: sRGB 173.
        assert_eq!(tm(520, 520, 520), [173, 173, 173]);
        // PQ 240 nits (612), TrueHDR's SDR white, lands in the knee, a
        // little below 255.
        assert_eq!(tm(612, 612, 612), [238, 238, 238]);
        // PQ 1000 nits (769) and the PQ maximum are above the source peak.
        assert_eq!(tm(769, 769, 769), [255, 255, 255]);
        assert_eq!(tm(1023, 1023, 1023), [255, 255, 255]);
        // A dark grey, below the knee.
        assert_eq!(tm(100, 100, 100), [4, 4, 4]);
        // Colours.
        assert_eq!(tm(580, 560, 540), [235, 203, 186]);
        assert_eq!(tm(560, 520, 480), [225, 169, 140]);
        assert_eq!(tm(200, 210, 190), [23, 27, 21]);
        assert_eq!(tm(450, 470, 430), [118, 140, 111]);
        assert_eq!(tm(520, 520, 560), [170, 173, 209]);
        assert_eq!(tm(700, 690, 680), [255, 237, 228]);
        // Saturated BT.2020 colours clip to the BT.709 gamut.
        assert_eq!(tm(600, 500, 400), [255, 128, 74]);
        assert_eq!(tm(300, 620, 350), [0, 255, 0]);
        assert_eq!(tm(700, 300, 200), [255, 0, 0]);
        // A lower source peak compresses less.
        let [r, ..] = pq_to_sdr(grey(612), ToneMap::new(DIFFUSE_GAIN, 400.0).unwrap());
        assert_eq!(r, 246);
        // The alpha code of the input does not matter.
        assert_eq!(
            pq_to_sdr(pack_rgb10a2(580, 560, 540, 0), default_map()),
            [235, 203, 186, 255]
        );
    }

    #[test]
    fn a_source_peak_at_or_below_sdr_white_is_refused() {
        // At gain * 100 the knee starts at the target peak and the curve
        // divides by zero; below it the knee lies past the target.
        for (gain, peak) in [
            (DIFFUSE_GAIN, DIFFUSE_GAIN * SDR_WHITE_NITS),
            (DIFFUSE_GAIN, 100.0),
            (1.0, 100.0),
            (0.0, 650.0),
            (-1.0, 650.0),
            (DIFFUSE_GAIN, f32::NAN),
            (f32::NAN, 650.0),
            (DIFFUSE_GAIN, f32::INFINITY),
        ] {
            let e = ToneMap::new(gain, peak).unwrap_err();
            assert!(
                matches!(e, GpuError::InvalidArgument(_)),
                "{gain} {peak}: {e}"
            );
        }
        // Just above it every code maps to a grey (to the rounding of the
        // gamut matrix) that rises with the code up to white.
        let tm = ToneMap::new(1.0, 100.5).unwrap();
        assert_eq!((tm.gain(), tm.peak()), (1.0, 100.5));
        let mut last = 0;
        for code in 0..1024 {
            let [r, g, b, a] = pq_to_sdr(grey(code), tm);
            assert_eq!(a, 255);
            assert!(
                r.abs_diff(g) <= 1 && b.abs_diff(g) <= 1,
                "{code}: {r} {g} {b}"
            );
            assert!(g >= last, "{code}: {g} < {last}");
            last = g;
        }
        assert_eq!(last, 255);
        assert!(ToneMap::new(DIFFUSE_GAIN, 400.0).is_ok());
    }

    #[test]
    fn the_embedded_ptx_is_pinned_and_takes_the_eight_parameters_launch_passes() {
        let directives: Vec<&str> = PQ_TO_SDR_PTX
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with(".version") || l.starts_with(".target"))
            .collect();
        assert_eq!(directives, [".version 7.0", ".target sm_75"]);
        assert!(PQ_TO_SDR_PTX.ends_with('\0'));
        assert_eq!(PQ_TO_SDR_PTX.matches('\0').count(), 1);
        assert_eq!(PQ_TO_SDR_PTX.matches(".entry").count(), 1);
        assert_eq!(PQ_TO_SDR_ENTRY.to_str().unwrap(), "obc_pq_to_sdr");
        let start = PQ_TO_SDR_PTX
            .find(".visible .entry obc_pq_to_sdr(")
            .expect("the entry point is declared");
        let rest = &PQ_TO_SDR_PTX[start..];
        let params: Vec<&str> = rest[..rest.find(')').unwrap()]
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with(".param"))
            .collect();
        let want = ["u64", "u64", "u64", "u64", "u32", "u32", "f32", "f32"];
        assert_eq!(params.len(), want.len(), "{params:?}");
        for (i, (line, ty)) in params.iter().zip(want).enumerate() {
            assert_eq!(
                line.trim_end_matches(','),
                format!(".param .{ty} obc_pq_to_sdr_param_{i}"),
                "parameter {i}"
            );
        }
        let threads = PQ_TO_SDR_BLOCK.0 * PQ_TO_SDR_BLOCK.1;
        assert!(threads <= 1024 && PQ_TO_SDR_BLOCK.0.is_multiple_of(32));
    }
}
