//! The pixel work, on host NV12 frames: the model's input, the matte, the
//! blur and the composite.
//!
//! All of it is plain integer and float code on the CPU, split in row
//! bands over a few threads where it reads or writes the whole frame. A
//! frame's rows may be longer than its width (`pitch`); NV12 of an odd
//! width or height is refused at load.

use crate::catalogue::{Layout, ModelSpec, Norm, Output};
use std::sync::OnceLock;

/// A host NV12 frame: two planes of rows `pitch` bytes apart. The memory
/// is the caller's, valid for the call it is used in.
#[derive(Debug, Clone, Copy)]
pub struct Planes {
    /// Pixel (0, 0) of the Y plane.
    pub y: *mut u8,
    /// Bytes between two Y rows.
    pub y_pitch: usize,
    /// The first U,V pair.
    pub uv: *mut u8,
    /// Bytes between two UV rows.
    pub uv_pitch: usize,
    /// Width in pixels (even).
    pub width: usize,
    /// Height in pixels (even).
    pub height: usize,
}

// SAFETY: a description of memory; the functions that use it from several
// threads give each thread disjoint rows to write and only read the rest.
unsafe impl Send for Planes {}
unsafe impl Sync for Planes {}

impl Planes {
    /// Checks the description: even non-zero size, pitches of at least the
    /// width, non-null planes.
    pub fn check(&self) -> Result<(), String> {
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(format!(
                "a {}x{} NV12 frame is not supported: the width and height must be even",
                self.width, self.height
            ));
        }
        if self.y.is_null() || self.uv.is_null() {
            return Err("an NV12 frame has a null plane".into());
        }
        if self.y_pitch < self.width || self.uv_pitch < self.width {
            return Err(format!(
                "NV12 rows of {} and {} bytes are shorter than the width {}",
                self.y_pitch, self.uv_pitch, self.width
            ));
        }
        Ok(())
    }

    /// Y row `r`, read.
    ///
    /// # Safety
    /// `r < height`; nothing writes the row while the slice lives.
    unsafe fn y_row(&self, r: usize) -> &[u8] {
        // SAFETY: by the caller's contract and the frame's.
        unsafe { std::slice::from_raw_parts(self.y.add(r * self.y_pitch), self.width) }
    }

    /// UV row `r` (of `height / 2`), read.
    ///
    /// # Safety
    /// As [`Planes::y_row`].
    unsafe fn uv_row(&self, r: usize) -> &[u8] {
        // SAFETY: by the caller's contract and the frame's.
        unsafe { std::slice::from_raw_parts(self.uv.add(r * self.uv_pitch), self.width) }
    }

    /// Copies Y row `r` into `dst` (raw: the row may be the one written
    /// next, for a frame processed in place).
    ///
    /// # Safety
    /// `r < height`; no other thread writes the row meanwhile.
    unsafe fn copy_y_row(&self, r: usize, dst: &mut [u8]) {
        // SAFETY: `dst` is our buffer of at least `width` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.y.add(r * self.y_pitch),
                dst.as_mut_ptr(),
                self.width,
            )
        }
    }

    /// As [`Planes::copy_y_row`] for UV row `r`.
    ///
    /// # Safety
    /// As [`Planes::copy_y_row`].
    unsafe fn copy_uv_row(&self, r: usize, dst: &mut [u8]) {
        // SAFETY: as above.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.uv.add(r * self.uv_pitch),
                dst.as_mut_ptr(),
                self.width,
            )
        }
    }

    /// Y row `r`, written.
    ///
    /// # Safety
    /// `r < height`; nothing else accesses the row while the slice lives.
    #[allow(clippy::mut_from_ref)]
    unsafe fn y_row_mut(&self, r: usize) -> &mut [u8] {
        // SAFETY: by the caller's contract and the frame's.
        unsafe { std::slice::from_raw_parts_mut(self.y.add(r * self.y_pitch), self.width) }
    }

    /// UV row `r`, written.
    ///
    /// # Safety
    /// As [`Planes::y_row_mut`].
    #[allow(clippy::mut_from_ref)]
    unsafe fn uv_row_mut(&self, r: usize) -> &mut [u8] {
        // SAFETY: by the caller's contract and the frame's.
        unsafe { std::slice::from_raw_parts_mut(self.uv.add(r * self.uv_pitch), self.width) }
    }

    /// Whether the two describe the same memory.
    pub fn same(&self, other: &Planes) -> bool {
        self.y == other.y
            && self.uv == other.uv
            && self.y_pitch == other.y_pitch
            && self.uv_pitch == other.uv_pitch
    }
}

/// An NV12 image the module owns, rows of `width` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12 {
    /// Width.
    pub width: usize,
    /// Height.
    pub height: usize,
    /// The Y plane.
    pub y: Vec<u8>,
    /// The UV plane.
    pub uv: Vec<u8>,
}

impl Nv12 {
    /// A black-free mid-grey image.
    pub fn new(width: usize, height: usize) -> Nv12 {
        Nv12 {
            width,
            height,
            y: vec![0; width * height],
            uv: vec![128; width * (height / 2)],
        }
    }

    /// The planes over it.
    pub fn planes(&mut self) -> Planes {
        Planes {
            y: self.y.as_mut_ptr(),
            y_pitch: self.width,
            uv: self.uv.as_mut_ptr(),
            uv_pitch: self.width,
            width: self.width,
            height: self.height,
        }
    }
}

/// The colour description of the frames: the matrix's `Kr` and `Kb`, and
/// the range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colour {
    /// The red coefficient.
    pub kr: f32,
    /// The blue coefficient.
    pub kb: f32,
    /// Full range (0-255) rather than limited (16-235, 16-240).
    pub full_range: bool,
}

impl Colour {
    /// BT.601.
    pub const BT601: (f32, f32) = (0.299, 0.114);
    /// BT.709.
    pub const BT709: (f32, f32) = (0.2126, 0.0722);
    /// BT.2020.
    pub const BT2020: (f32, f32) = (0.2627, 0.0593);

    /// A description from the coefficients and the range.
    pub fn new((kr, kb): (f32, f32), full_range: bool) -> Colour {
        Colour { kr, kb, full_range }
    }

    /// 8-bit R'G'B' (0-255) of a Y'CbCr sample.
    pub fn to_rgb(&self, y: f32, u: f32, v: f32) -> [f32; 3] {
        let (y, cb, cr) = if self.full_range {
            (y / 255.0, (u - 128.0) / 255.0, (v - 128.0) / 255.0)
        } else {
            ((y - 16.0) / 219.0, (u - 128.0) / 224.0, (v - 128.0) / 224.0)
        };
        let kg = 1.0 - self.kr - self.kb;
        let r = y + 2.0 * (1.0 - self.kr) * cr;
        let b = y + 2.0 * (1.0 - self.kb) * cb;
        let g = (y - self.kr * r - self.kb * b) / kg;
        [r, g, b].map(|c| (c * 255.0).clamp(0.0, 255.0))
    }

    /// The Y'CbCr sample (8-bit codes) of 8-bit R'G'B'.
    pub fn to_yuv(&self, r: f32, g: f32, b: f32) -> [u8; 3] {
        let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
        let kg = 1.0 - self.kr - self.kb;
        let y = self.kr * r + kg * g + self.kb * b;
        let cb = (b - y) / (2.0 * (1.0 - self.kb));
        let cr = (r - y) / (2.0 * (1.0 - self.kr));
        let (y, cb, cr) = if self.full_range {
            (y * 255.0, cb * 255.0 + 128.0, cr * 255.0 + 128.0)
        } else {
            (16.0 + y * 219.0, 128.0 + cb * 224.0, 128.0 + cr * 224.0)
        };
        [y, cb, cr].map(|c| c.round().clamp(0.0, 255.0) as u8)
    }
}

/// How many threads the frame-wide passes use.
pub fn workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 4)
}

/// The module's pool for the frame-wide passes: [`workers`] threads, made
/// the first time a frame needs them and kept for the life of the process
/// (no thread is started per frame). Its own pool, not rayon's global one,
/// which another library in the daemon's process may size or fill.
fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers())
            .thread_name(|i| format!("obc-onnx-{i}"))
            .build()
            .map_err(|e| {
                obc_module_sdk::log::warn!(
                    "ONNX cut-out: no worker threads ({e}); the frames are composited on one thread"
                )
            })
            .ok()
    })
    .as_ref()
}

/// Runs `f(start, end)` over `0..n` split in up to `threads` bands of whole
/// multiples of `align`, on the pool (the first band on this thread).
fn bands(n: usize, align: usize, threads: usize, f: impl Fn(usize, usize) + Sync) {
    let threads = threads.clamp(1, 8);
    let units = n.div_ceil(align);
    let pool = pool();
    if threads == 1 || units < 2 * threads || pool.is_none() {
        f(0, n);
        return;
    }
    let per = units.div_ceil(threads);
    let f = &f;
    pool.expect("checked").in_place_scope(|s| {
        let mut start = per * align;
        while start < n {
            let end = (start + per * align).min(n);
            s.spawn(move |_| f(start, end));
            start = end;
        }
        f(0, (per * align).min(n));
    });
}

/// Runs the tasks at once on the pool, the last on this thread.
fn run_tasks(tasks: &mut [&mut (dyn FnMut() + Send)]) {
    let Some((last, rest)) = tasks.split_last_mut() else {
        return;
    };
    match pool() {
        Some(pool) => pool.in_place_scope(|s| {
            for t in rest {
                s.spawn(move |_| t());
            }
            last();
        }),
        None => {
            for t in rest {
                t();
            }
            last();
        }
    }
}

/// The source index of each destination pixel of an area average from `m`
/// samples to `n` bins: `bin[i] = i * n / m`.
fn bins(m: usize, n: usize) -> Vec<u32> {
    (0..m).map(|i| (i * n / m) as u32).collect()
}

/// The source range `[start, end)` of each of `n` bins over `m` samples,
/// never empty (a bin of a smaller source takes its nearest sample).
fn ranges(m: usize, n: usize) -> Vec<(usize, usize)> {
    (0..n)
        .map(|i| {
            let a = (i * m / n).min(m - 1);
            let b = ((i + 1) * m / n).clamp(a + 1, m);
            (a, b)
        })
        .collect()
}

/// Fills `tensor` with the model's input from `frame`: an area average to
/// the model's size (every `step`th row and column when the frame is much
/// larger), R'G'B', normalised, in the model's layout.
pub fn preprocess(
    frame: &Planes,
    colour: &Colour,
    model: &ModelSpec,
    (mh, mw): (u32, u32),
    tensor: &mut [f32],
) {
    let (mh, mw) = (mh as usize, mw as usize);
    let (w, h) = (frame.width, frame.height);
    let (cw, ch) = (w / 2, h / 2);
    debug_assert_eq!(tensor.len(), 3 * mh * mw);
    // Each model pixel averages its source rectangle (at least one
    // sample, so a frame smaller than the model is sampled, not left
    // black), every `step`th row and column when the rectangle is large.
    let step_y = (h / mh / 3).max(1);
    let step_x = (w / mw / 3).max(1);
    let (rx, ry) = (ranges(w, mw), ranges(h, mh));
    let (rcx, rcy) = (ranges(cw, mw), ranges(ch, mh));
    let mut sy = vec![0u32; mh * mw];
    let mut ny = vec![0u32; mh * mw];
    let mut su = vec![0u32; mh * mw];
    let mut sv = vec![0u32; mh * mw];
    let mut nc = vec![0u32; mh * mw];
    for (my, &(y0, y1)) in ry.iter().enumerate() {
        for r in (y0..y1).step_by(step_y) {
            // SAFETY: r < height; the frame is only read here.
            let row = unsafe { frame.y_row(r) };
            for (mx, &(x0, x1)) in rx.iter().enumerate() {
                let i = my * mw + mx;
                for x in (x0..x1).step_by(step_x) {
                    sy[i] += u32::from(row[x]);
                    ny[i] += 1;
                }
            }
        }
    }
    for (my, &(y0, y1)) in rcy.iter().enumerate() {
        for r in (y0..y1).step_by(step_y) {
            // SAFETY: r < height / 2; read only.
            let row = unsafe { frame.uv_row(r) };
            for (mx, &(x0, x1)) in rcx.iter().enumerate() {
                let i = my * mw + mx;
                for c in (x0..x1).step_by(step_x) {
                    su[i] += u32::from(row[2 * c]);
                    sv[i] += u32::from(row[2 * c + 1]);
                    nc[i] += 1;
                }
            }
        }
    }
    let plane = mh * mw;
    for i in 0..plane {
        let y = sy[i] as f32 / ny[i].max(1) as f32;
        let n = nc[i].max(1) as f32;
        let (u, v) = (su[i] as f32 / n, sv[i] as f32 / n);
        let rgb = colour.to_rgb(y, u, v);
        let norm = |c: f32| match model.norm {
            Norm::Unit => c / 255.0,
            Norm::Signed => c / 127.5 - 1.0,
        };
        match model.layout {
            Layout::Nchw => {
                tensor[i] = norm(rgb[0]);
                tensor[plane + i] = norm(rgb[1]);
                tensor[2 * plane + i] = norm(rgb[2]);
            }
            Layout::Nhwc => {
                tensor[3 * i] = norm(rgb[0]);
                tensor[3 * i + 1] = norm(rgb[1]);
                tensor[3 * i + 2] = norm(rgb[2]);
            }
        }
    }
}

/// The matte (0 to 1, at the model's size) of the model's output.
pub fn matte_of_output(model: &ModelSpec, values: &[f32], (mh, mw): (u32, u32), matte: &mut [f32]) {
    let plane = mh as usize * mw as usize;
    let src = match model.output {
        // [1, H, W, 1] and [1, 1, H, W] are the same plane.
        Output::Alpha => &values[..plane],
        Output::Softmax2 => &values[plane..2 * plane],
    };
    for (m, v) in matte.iter_mut().zip(src) {
        *m = if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        };
    }
}

/// The light smoothing of the matte over time: an exponential moving
/// average, forgotten on a new source, a temporal reset and a load.
#[derive(Debug, Clone)]
pub struct Ema {
    /// The weight of the newest matte (1 keeps no history).
    pub weight: f32,
    state: Vec<f32>,
    primed: bool,
}

impl Ema {
    /// An average giving `weight` to each new matte.
    pub fn new(weight: f32) -> Ema {
        Ema {
            weight: weight.clamp(0.0, 1.0),
            state: Vec::new(),
            primed: false,
        }
    }

    /// Forgets the history: the next matte is taken as it is.
    pub fn reset(&mut self) {
        self.primed = false;
    }

    /// Mixes `matte` into the average and writes the average back to it.
    pub fn apply(&mut self, matte: &mut [f32]) {
        if !self.primed || self.state.len() != matte.len() {
            self.state.clear();
            self.state.extend_from_slice(matte);
            self.primed = true;
            return;
        }
        let a = self.weight;
        for (s, m) in self.state.iter_mut().zip(matte.iter_mut()) {
            *s = a * *m + (1.0 - a) * *s;
            *m = *s;
        }
    }

    /// Whether the next matte will be taken as it is.
    pub fn is_reset(&self) -> bool {
        !self.primed
    }
}

/// The bilinear mapping of `n` destination samples onto `m` source
/// samples, centres aligned: for each destination sample, the two source
/// samples and the weight of the second (0 to 256).
#[derive(Debug, Clone, Default)]
struct Axis {
    i0: Vec<u32>,
    i1: Vec<u32>,
    w: Vec<u32>,
}

impl Axis {
    fn new(m: usize, n: usize) -> Axis {
        let mut a = Axis {
            i0: Vec::with_capacity(n),
            i1: Vec::with_capacity(n),
            w: Vec::with_capacity(n),
        };
        let scale = m as f64 / n as f64;
        for i in 0..n {
            let f = ((i as f64 + 0.5) * scale - 0.5).clamp(0.0, (m - 1) as f64);
            let i0 = f.floor() as usize;
            let i1 = (i0 + 1).min(m - 1);
            a.i0.push(i0 as u32);
            a.i1.push(i1 as u32);
            a.w.push(((f - i0 as f64) * 256.0).round() as u32);
        }
        a
    }
}

/// The background effect.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// The room blurred.
    Blur,
    /// An image instead of the room, at the frame's size.
    Replace(Nv12),
}

/// The compositor of one frame geometry and model size: its tables and
/// buffers, kept across frames.
pub struct Compositor {
    width: usize,
    height: usize,
    mh: usize,
    mw: usize,
    threads: usize,
    // Matte (model size) to frame, for luma and chroma.
    ly: Axis,
    lx: Axis,
    cy: Axis,
    cx: Axis,
    // The blur's small images: luma at a quarter of the frame, chroma at a
    // quarter of the chroma plane.
    sw: usize,
    sh: usize,
    scw: usize,
    sch: usize,
    blur_y: Vec<f32>,
    blur_u: Vec<f32>,
    blur_v: Vec<f32>,
    weight_y: Vec<f32>,
    weight_c: Vec<f32>,
    weight_yb: Vec<f32>,
    weight_cb: Vec<f32>,
    // The blurred room, in 1/16 steps of a code value.
    small_y: Vec<u16>,
    small_u: Vec<u16>,
    small_v: Vec<u16>,
    // Small images to frame, for luma and chroma.
    by: Axis,
    bx: Axis,
    bcy: Axis,
    bcx: Axis,
}

/// The factor of the blur's downscale.
const BLUR_FACTOR: usize = 4;
/// The weight floor of the room in the blur: where the person covers
/// everything around, the blur falls back to the plain average.
const ROOM_WEIGHT_FLOOR: f32 = 0.02;

impl Compositor {
    /// A compositor for `width x height` frames and a matte of `(mh, mw)`.
    pub fn new(
        width: usize,
        height: usize,
        (mh, mw): (usize, usize),
        threads: usize,
    ) -> Compositor {
        let (cw, ch) = (width / 2, height / 2);
        let sw = width.div_ceil(BLUR_FACTOR).max(1);
        let sh = height.div_ceil(BLUR_FACTOR).max(1);
        let scw = cw.div_ceil(BLUR_FACTOR).max(1);
        let sch = ch.div_ceil(BLUR_FACTOR).max(1);
        Compositor {
            width,
            height,
            mh,
            mw,
            threads,
            ly: Axis::new(mh, height),
            lx: Axis::new(mw, width),
            cy: Axis::new(mh, ch),
            cx: Axis::new(mw, cw),
            sw,
            sh,
            scw,
            sch,
            blur_y: vec![0.0; sw * sh],
            blur_u: vec![0.0; scw * sch],
            blur_v: vec![0.0; scw * sch],
            weight_y: vec![0.0; sw * sh],
            weight_c: vec![0.0; scw * sch],
            weight_yb: vec![0.0; sw * sh],
            weight_cb: vec![0.0; scw * sch],
            small_y: vec![0; sw * sh],
            small_u: vec![0; scw * sch],
            small_v: vec![0; scw * sch],
            by: Axis::new(sh, height),
            bx: Axis::new(sw, width),
            bcy: Axis::new(sch, ch),
            bcx: Axis::new(scw, cw),
        }
    }

    /// The blur radius, in pixels of the luma small image, for `strength`
    /// (0 to 1): 0 for no blur, up to a 48th of the frame's width.
    pub fn blur_radius(&self, strength: f32) -> usize {
        if strength.is_nan() || strength <= 0.0 {
            return 0;
        }
        let max = self.width as f32 / 160.0;
        (strength.min(1.0) * max).round().max(1.0) as usize
    }

    /// Composites `input` into `output` (which may be the same memory)
    /// with `matte` (at the model's size, 0 to 1): the person over the
    /// blurred room (`strength`, 0 to 1) or over the replacement.
    pub fn composite(
        &mut self,
        input: &Planes,
        output: &Planes,
        matte: &[f32],
        mode: &Mode,
        strength: f32,
    ) {
        debug_assert_eq!(matte.len(), self.mh * self.mw);
        // The matte in 12-bit fixed point.
        let m12: Vec<u16> = matte.iter().map(|m| (m * 4096.0).round() as u16).collect();
        match mode {
            Mode::Replace(bg) => {
                debug_assert_eq!((bg.width, bg.height), (self.width, self.height));
                let bg = Bg::Image(bg);
                self.blend(input, output, &m12, &bg);
            }
            Mode::Blur => {
                let r = self.blur_radius(strength);
                if r == 0 {
                    self.blend(input, output, &m12, &Bg::Same);
                    return;
                }
                self.blur_room(input, matte, r);
                let bg = Bg::Small {
                    y: &self.small_y,
                    u: &self.small_u,
                    v: &self.small_v,
                };
                // `blend` borrows the tables only.
                let this = &*self;
                this.blend(input, output, &m12, &bg);
            }
        }
    }

    /// Fills the small blurred images of the room: the frame averaged
    /// down, weighted by how much of each small pixel is room (so the
    /// person does not bleed into the blur), blurred, and normalised.
    fn blur_room(&mut self, input: &Planes, matte: &[f32], r: usize) {
        let (sw, sh, scw, sch) = (self.sw, self.sh, self.scw, self.sch);
        // Room weights at the small sizes, from the matte.
        let room = |w: usize, h: usize, out: &mut [f32]| {
            let (ay, ax) = (Axis::new(self.mh, h), Axis::new(self.mw, w));
            for y in 0..h {
                for x in 0..w {
                    let m = bilinear(matte, self.mw, &ay, &ax, y, x);
                    out[y * w + x] = (1.0 - m).max(ROOM_WEIGHT_FLOOR);
                }
            }
        };
        room(sw, sh, &mut self.weight_y);
        room(scw, sch, &mut self.weight_c);
        // Area averages of the planes premultiplied by the weights, and
        // the weights, blurred: four tasks at once.
        let rc = r.div_ceil(2);
        let Compositor {
            blur_y,
            blur_u,
            blur_v,
            weight_y,
            weight_c,
            weight_yb,
            weight_cb,
            ..
        } = self;
        let (weight_y, weight_c) = (&*weight_y, &*weight_c);
        run_tasks(&mut [
            &mut || {
                downscale_y(input, sw, sh, blur_y);
                for (p, w) in blur_y.iter_mut().zip(weight_y) {
                    *p *= *w;
                }
                box_blur3(blur_y, sw, sh, r);
            },
            &mut || {
                weight_yb.copy_from_slice(weight_y);
                box_blur3(weight_yb, sw, sh, r);
            },
            &mut || {
                weight_cb.copy_from_slice(weight_c);
                box_blur3(weight_cb, scw, sch, rc);
            },
            &mut || {
                downscale_uv(input, scw, sch, blur_u, blur_v);
                for ((u, v), w) in blur_u.iter_mut().zip(blur_v.iter_mut()).zip(weight_c) {
                    *u *= *w;
                    *v *= *w;
                }
                box_blur3(blur_u, scw, sch, rc);
                box_blur3(blur_v, scw, sch, rc);
            },
        ]);
        let fixed = |v: f32, w: f32| ((v / w.max(1e-6)) * 16.0).round().clamp(0.0, 4080.0) as u16;
        for ((o, p), w) in self
            .small_y
            .iter_mut()
            .zip(&self.blur_y)
            .zip(&self.weight_yb)
        {
            *o = fixed(*p, *w);
        }
        for (i, w) in self.weight_cb.iter().enumerate() {
            self.small_u[i] = fixed(self.blur_u[i], *w);
            self.small_v[i] = fixed(self.blur_v[i], *w);
        }
    }

    /// Writes `output = bg + matte * (input - bg)`, row bands over the
    /// threads.
    fn blend(&self, input: &Planes, output: &Planes, m12: &[u16], bg: &Bg<'_>) {
        let (w, h) = (self.width, self.height);
        let (cw, ch) = (w / 2, h / 2);
        bands(h, 2, self.threads, |start, end| {
            let mut fg = vec![0u8; w];
            let mut mrow = vec![0u32; self.mw];
            let mut brow = vec![0u32; self.sw.max(self.scw)];
            let mut brow2 = vec![0u32; self.scw];
            for y in start..end {
                vlerp_u16(
                    m12,
                    self.mw,
                    self.ly.i0[y],
                    self.ly.i1[y],
                    self.ly.w[y],
                    &mut mrow,
                );
                // SAFETY: y < height; this band alone touches row y, and
                // it is copied before it is written (in place or not).
                unsafe { input.copy_y_row(y, &mut fg) };
                let out = unsafe { output.y_row_mut(y) };
                match bg {
                    Bg::Same => out.copy_from_slice(&fg),
                    Bg::Image(img) => {
                        let b = &img.y[y * w..(y + 1) * w];
                        for x in 0..w {
                            let m = hlerp(&mrow, &self.lx, x);
                            out[x] = mix16(u32::from(b[x]) << 4, fg[x], m);
                        }
                    }
                    Bg::Small { y: sy, .. } => {
                        vlerp_u16(
                            sy,
                            self.sw,
                            self.by.i0[y],
                            self.by.i1[y],
                            self.by.w[y],
                            &mut brow,
                        );
                        for x in 0..w {
                            let m = hlerp(&mrow, &self.lx, x);
                            let b = hlerp(&brow, &self.bx, x);
                            out[x] = mix16(b, fg[x], m);
                        }
                    }
                }
            }
            for cyr in start / 2..(end / 2).min(ch) {
                vlerp_u16(
                    m12,
                    self.mw,
                    self.cy.i0[cyr],
                    self.cy.i1[cyr],
                    self.cy.w[cyr],
                    &mut mrow,
                );
                // SAFETY: as for the luma rows.
                unsafe { input.copy_uv_row(cyr, &mut fg) };
                let out = unsafe { output.uv_row_mut(cyr) };
                match bg {
                    Bg::Same => out.copy_from_slice(&fg),
                    Bg::Image(img) => {
                        let b = &img.uv[cyr * w..(cyr + 1) * w];
                        for c in 0..cw {
                            let m = hlerp(&mrow, &self.cx, c);
                            out[2 * c] = mix16(u32::from(b[2 * c]) << 4, fg[2 * c], m);
                            out[2 * c + 1] = mix16(u32::from(b[2 * c + 1]) << 4, fg[2 * c + 1], m);
                        }
                    }
                    Bg::Small { u, v, .. } => {
                        let (i0, i1, wy) = (self.bcy.i0[cyr], self.bcy.i1[cyr], self.bcy.w[cyr]);
                        vlerp_u16(u, self.scw, i0, i1, wy, &mut brow[..self.scw]);
                        vlerp_u16(v, self.scw, i0, i1, wy, &mut brow2);
                        for c in 0..cw {
                            let m = hlerp(&mrow, &self.cx, c);
                            let bu = hlerp(&brow[..self.scw], &self.bcx, c);
                            let bv = hlerp(&brow2, &self.bcx, c);
                            out[2 * c] = mix16(bu, fg[2 * c], m);
                            out[2 * c + 1] = mix16(bv, fg[2 * c + 1], m);
                        }
                    }
                }
            }
        });
    }
}

/// What the person goes over.
enum Bg<'a> {
    /// Nothing: the frame as it is (a blur of strength 0).
    Same,
    /// A frame-sized image.
    Image(&'a Nv12),
    /// The small blurred images.
    Small {
        y: &'a [u16],
        u: &'a [u16],
        v: &'a [u16],
    },
}

/// `bg + m * (fg - bg)`: `bg` in 1/16 steps of a code value, `m` in
/// 12-bit fixed point (0 to 4096).
#[inline(always)]
fn mix16(bg16: u32, fg: u8, m: u32) -> u8 {
    ((bg16 * (4096 - m) + (u32::from(fg) << 4) * m + 32768) >> 16) as u8
}

/// Row `i0`/`i1` of a `w`-wide fixed-point matte, mixed by `wy` (0 to
/// 256), into `out`.
#[inline]
fn vlerp_u16(src: &[u16], w: usize, i0: u32, i1: u32, wy: u32, out: &mut [u32]) {
    let (a, b) = (&src[i0 as usize * w..][..w], &src[i1 as usize * w..][..w]);
    for ((o, &a), &b) in out.iter_mut().zip(a).zip(b) {
        *o = (u32::from(a) * (256 - wy) + u32::from(b) * wy + 128) >> 8;
    }
}

#[inline(always)]
fn hlerp(row: &[u32], ax: &Axis, x: usize) -> u32 {
    let (a, b, w) = (row[ax.i0[x] as usize], row[ax.i1[x] as usize], ax.w[x]);
    (a * (256 - w) + b * w + 128) >> 8
}

/// The bilinear sample of a float image of width `w` at destination
/// `(y, x)` of the axes.
fn bilinear(src: &[f32], w: usize, ay: &Axis, ax: &Axis, y: usize, x: usize) -> f32 {
    let (y0, y1, wy) = (ay.i0[y] as usize, ay.i1[y] as usize, ay.w[y] as f32 / 256.0);
    let (x0, x1, wx) = (ax.i0[x] as usize, ax.i1[x] as usize, ax.w[x] as f32 / 256.0);
    let top = src[y0 * w + x0] + (src[y0 * w + x1] - src[y0 * w + x0]) * wx;
    let bot = src[y1 * w + x0] + (src[y1 * w + x1] - src[y1 * w + x0]) * wx;
    top + (bot - top) * wy
}

/// The Y plane averaged into `sw x sh` bins.
fn downscale_y(frame: &Planes, sw: usize, sh: usize, out: &mut [f32]) {
    let (w, h) = (frame.width, frame.height);
    if w == 4 * sw && h == 4 * sh {
        // Exact 4x4 blocks.
        let mut acc = vec![0u32; sw];
        for by in 0..sh {
            acc.fill(0);
            for r in 4 * by..4 * by + 4 {
                // SAFETY: r < height; read only.
                let row = unsafe { frame.y_row(r) };
                for (a, q) in acc.iter_mut().zip(row.as_chunks::<4>().0) {
                    *a += u32::from(q[0]) + u32::from(q[1]) + u32::from(q[2]) + u32::from(q[3]);
                }
            }
            for (o, a) in out[by * sw..(by + 1) * sw].iter_mut().zip(&acc) {
                *o = *a as f32 / 16.0;
            }
        }
        return;
    }
    let (bx, by) = (bins(w, sw), bins(h, sh));
    let mut n = vec![0u32; sw * sh];
    let mut s = vec![0u32; sw * sh];
    for (r, &bin) in by.iter().enumerate() {
        // SAFETY: r < height; read only.
        let row = unsafe { frame.y_row(r) };
        let base = bin as usize * sw;
        for (x, &p) in row.iter().enumerate() {
            let i = base + bx[x] as usize;
            s[i] += u32::from(p);
            n[i] += 1;
        }
    }
    for ((o, s), n) in out.iter_mut().zip(&s).zip(&n) {
        *o = *s as f32 / (*n).max(1) as f32;
    }
}

/// The UV plane averaged into `scw x sch` bins of U and of V.
fn downscale_uv(frame: &Planes, scw: usize, sch: usize, u: &mut [f32], v: &mut [f32]) {
    let (cw, ch) = (frame.width / 2, frame.height / 2);
    if cw == 4 * scw && ch == 4 * sch {
        // Exact blocks of 4x4 pairs.
        let mut au = vec![0u32; scw];
        let mut av = vec![0u32; scw];
        for by in 0..sch {
            au.fill(0);
            av.fill(0);
            for r in 4 * by..4 * by + 4 {
                // SAFETY: r < height / 2; read only.
                let row = unsafe { frame.uv_row(r) };
                for ((a, b), q) in au.iter_mut().zip(av.iter_mut()).zip(row.as_chunks::<8>().0) {
                    *a += u32::from(q[0]) + u32::from(q[2]) + u32::from(q[4]) + u32::from(q[6]);
                    *b += u32::from(q[1]) + u32::from(q[3]) + u32::from(q[5]) + u32::from(q[7]);
                }
            }
            for i in 0..scw {
                u[by * scw + i] = au[i] as f32 / 16.0;
                v[by * scw + i] = av[i] as f32 / 16.0;
            }
        }
        return;
    }
    let (bx, by) = (bins(cw, scw), bins(ch, sch));
    let mut n = vec![0u32; scw * sch];
    let mut su = vec![0u32; scw * sch];
    let mut sv = vec![0u32; scw * sch];
    for (r, &bin) in by.iter().enumerate() {
        // SAFETY: r < height / 2; read only.
        let row = unsafe { frame.uv_row(r) };
        let base = bin as usize * scw;
        for c in 0..cw {
            let i = base + bx[c] as usize;
            su[i] += u32::from(row[2 * c]);
            sv[i] += u32::from(row[2 * c + 1]);
            n[i] += 1;
        }
    }
    for i in 0..scw * sch {
        let k = n[i].max(1) as f32;
        u[i] = su[i] as f32 / k;
        v[i] = sv[i] as f32 / k;
    }
}

/// Three passes of a box blur of radius `r` (edges clamped) in each
/// direction: close to a Gaussian. The vertical pass sweeps whole rows, so
/// it reads memory in order.
pub fn box_blur3(img: &mut [f32], w: usize, h: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0.0f32; w * h];
    let mut line = vec![0.0f32; w];
    let mut acc = vec![0.0f32; w];
    let k = 1.0 / (2 * r + 1) as f32;
    for _ in 0..3 {
        for y in 0..h {
            box_row(&mut img[y * w..(y + 1) * w], r, k, &mut line);
        }
        // Vertical: a running sum of whole rows.
        let row = |i: isize| i.clamp(0, h as isize - 1) as usize * w;
        acc.fill(0.0);
        for i in -(r as isize)..=(r as isize) {
            let o = row(i);
            for (a, v) in acc.iter_mut().zip(&img[o..o + w]) {
                *a += *v;
            }
        }
        for y in 0..h {
            for (t, a) in tmp[y * w..(y + 1) * w].iter_mut().zip(&acc) {
                *t = *a * k;
            }
            let (add, sub) = (
                row(y as isize + r as isize + 1),
                row(y as isize - r as isize),
            );
            for x in 0..w {
                acc[x] += img[add + x] - img[sub + x];
            }
        }
        img.copy_from_slice(&tmp);
    }
}

/// One horizontal box blur of a row.
fn box_row(row: &mut [f32], r: usize, k: f32, line: &mut [f32]) {
    let n = row.len();
    let at = |i: isize| -> usize { i.clamp(0, n as isize - 1) as usize };
    let mut acc = 0.0f32;
    for i in -(r as isize)..=(r as isize) {
        acc += row[at(i)];
    }
    for (i, l) in line.iter_mut().enumerate().take(n) {
        *l = acc * k;
        let i = i as isize;
        acc += row[at(i + r as isize + 1)] - row[at(i - r as isize)];
    }
    row.copy_from_slice(&line[..n]);
}

/// The replacement image at the frame's size and colour: `bgr` (rows
/// `pitch` bytes apart, `bw x bh`) scaled to cover the frame (the excess
/// cropped evenly, the aspect kept), bilinear.
pub fn background_nv12(
    bgr: &[u8],
    pitch: usize,
    (bw, bh): (usize, usize),
    (w, h): (usize, usize),
    colour: &Colour,
) -> Nv12 {
    let mut out = Nv12::new(w, h);
    let scale = (w as f64 / bw as f64).max(h as f64 / bh as f64);
    let (ox, oy) = (
        (bw as f64 - w as f64 / scale) / 2.0,
        (bh as f64 - h as f64 / scale) / 2.0,
    );
    let sample = |x: f64, y: f64| -> [f32; 3] {
        let x = x.clamp(0.0, (bw - 1) as f64);
        let y = y.clamp(0.0, (bh - 1) as f64);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(bw - 1), (y0 + 1).min(bh - 1));
        let (fx, fy) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
        let px = |x: usize, y: usize, c: usize| f32::from(bgr[y * pitch + 3 * x + c]);
        let mut rgb = [0.0f32; 3];
        for (i, c) in [2usize, 1, 0].into_iter().enumerate() {
            let top = px(x0, y0, c) + (px(x1, y0, c) - px(x0, y0, c)) * fx;
            let bot = px(x0, y1, c) + (px(x1, y1, c) - px(x0, y1, c)) * fx;
            rgb[i] = top + (bot - top) * fy;
        }
        rgb
    };
    let mut rgb_row: Vec<[f32; 3]> = vec![[0.0; 3]; w];
    let mut prev: Vec<[f32; 3]> = vec![[0.0; 3]; w];
    for y in 0..h {
        for (x, px) in rgb_row.iter_mut().enumerate() {
            *px = sample(
                ox + (x as f64 + 0.5) / scale - 0.5,
                oy + (y as f64 + 0.5) / scale - 0.5,
            );
        }
        for (x, p) in rgb_row.iter().enumerate() {
            out.y[y * w + x] = colour.to_yuv(p[0], p[1], p[2])[0];
        }
        if y % 2 == 1 {
            let cy = y / 2;
            for c in 0..w / 2 {
                let mut avg = [0.0f32; 3];
                for p in [
                    prev[2 * c],
                    prev[2 * c + 1],
                    rgb_row[2 * c],
                    rgb_row[2 * c + 1],
                ] {
                    for k in 0..3 {
                        avg[k] += p[k] / 4.0;
                    }
                }
                let yuv = colour.to_yuv(avg[0], avg[1], avg[2]);
                out.uv[cy * w + 2 * c] = yuv[1];
                out.uv[cy * w + 2 * c + 1] = yuv[2];
            }
        }
        std::mem::swap(&mut prev, &mut rgb_row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue;

    /// A `w x h` NV12 buffer with rows `pitch` bytes apart, filled with
    /// `y` and `(u, v)`, and the guard bytes past each row set to 0xEE.
    fn frame(w: usize, h: usize, pitch: usize, y: u8, uv: (u8, u8)) -> Vec<u8> {
        let mut b = vec![0xEEu8; pitch * (h + h / 2)];
        for r in 0..h {
            b[r * pitch..r * pitch + w].fill(y);
        }
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                b[(h + r) * pitch + 2 * c] = uv.0;
                b[(h + r) * pitch + 2 * c + 1] = uv.1;
            }
        }
        b
    }

    fn planes(buf: &mut [u8], w: usize, h: usize, pitch: usize) -> Planes {
        let p = buf.as_mut_ptr();
        Planes {
            y: p,
            y_pitch: pitch,
            // SAFETY: inside the buffer.
            uv: unsafe { p.add(pitch * h) },
            uv_pitch: pitch,
            width: w,
            height: h,
        }
    }

    fn colour() -> Colour {
        Colour::new(Colour::BT709, false)
    }

    #[test]
    fn colour_conversion_round_trips() {
        for c in [
            Colour::new(Colour::BT601, false),
            Colour::new(Colour::BT709, true),
            Colour::new(Colour::BT2020, false),
        ] {
            for rgb in [[0.0, 0.0, 0.0], [255.0, 255.0, 255.0], [200.0, 30.0, 90.0]] {
                let yuv = c.to_yuv(rgb[0], rgb[1], rgb[2]);
                let back = c.to_rgb(f32::from(yuv[0]), f32::from(yuv[1]), f32::from(yuv[2]));
                for k in 0..3 {
                    assert!(
                        (back[k] - rgb[k]).abs() < 3.0,
                        "{c:?} {rgb:?} -> {yuv:?} -> {back:?}"
                    );
                }
            }
        }
        // Limited-range black and white.
        assert_eq!(colour().to_yuv(0.0, 0.0, 0.0), [16, 128, 128]);
        assert_eq!(colour().to_yuv(255.0, 255.0, 255.0), [235, 128, 128]);
    }

    #[test]
    fn odd_sizes_and_short_rows_are_refused() {
        let mut b = frame(8, 8, 8, 0, (128, 128));
        let mut p = planes(&mut b, 8, 8, 8);
        p.check().unwrap();
        p.width = 7;
        assert!(p.check().unwrap_err().contains("even"));
        p.width = 8;
        p.height = 5;
        assert!(p.check().unwrap_err().contains("even"));
        p.height = 8;
        p.y_pitch = 6;
        assert!(p.check().unwrap_err().contains("shorter"));
    }

    #[test]
    fn preprocessing_follows_each_models_layout_and_normalisation() {
        // A mid-grey limited-range frame: Y 126 = RGB about 128.
        let (w, h, pitch) = (64, 36, 80);
        let mut b = frame(w, h, pitch, 126, (128, 128));
        let p = planes(&mut b, w, h, pitch);
        let grey = colour().to_rgb(126.0, 128.0, 128.0)[0];
        // MediaPipe: NHWC, [0, 1].
        let mp = catalogue::default_model();
        let mut t = vec![9.0f32; 3 * 144 * 256];
        preprocess(&p, &colour(), mp, (144, 256), &mut t);
        assert!(
            t.iter().all(|v| (v - grey / 255.0).abs() < 1e-3),
            "{}",
            t[0]
        );
        // PP-HumanSeg: NCHW, [-1, 1].
        let pp = catalogue::find("pphumanseg-v2-mobile").unwrap();
        let mut t = vec![9.0f32; 3 * 192 * 192];
        preprocess(&p, &colour(), pp, (192, 192), &mut t);
        assert!(t.iter().all(|v| (v - (grey / 127.5 - 1.0)).abs() < 1e-3));

        // A frame red on the left half, blue on the right: the layout
        // puts red in channel 0 of the left pixels.
        let red = colour().to_yuv(255.0, 0.0, 0.0);
        let blue = colour().to_yuv(0.0, 0.0, 255.0);
        let mut b = frame(w, h, pitch, 0, (0, 0));
        for r in 0..h {
            for x in 0..w {
                b[r * pitch + x] = if x < w / 2 { red[0] } else { blue[0] };
            }
        }
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                let px = if c < w / 4 { red } else { blue };
                b[(h + r) * pitch + 2 * c] = px[1];
                b[(h + r) * pitch + 2 * c + 1] = px[2];
            }
        }
        let p = planes(&mut b, w, h, pitch);
        let (mh, mw) = (4usize, 8usize);
        let nhwc = catalogue::ModelSpec {
            geometry: catalogue::Geometry::Fixed {
                height: 4,
                width: 8,
            },
            ..*mp
        };
        let mut t = vec![0.0f32; 3 * mh * mw];
        preprocess(&p, &colour(), &nhwc, (4, 8), &mut t);
        assert!(
            t[0] > 0.95 && t[2] < 0.05,
            "left pixel is red: {:?}",
            &t[..3]
        );
        let last = 3 * (mw - 1);
        assert!(t[last] < 0.05 && t[last + 2] > 0.95, "right pixel is blue");
        let nchw = catalogue::ModelSpec {
            geometry: catalogue::Geometry::Fixed {
                height: 4,
                width: 8,
            },
            ..*pp
        };
        preprocess(&p, &colour(), &nchw, (4, 8), &mut t);
        let plane = mh * mw;
        assert!(t[0] > 0.9 && t[2 * plane] < -0.9, "R plane first");
        assert!(t[mw - 1] < -0.9 && t[2 * plane + mw - 1] > 0.9);
    }

    #[test]
    fn the_output_reads_as_a_matte() {
        let mp = catalogue::default_model();
        let mut m = vec![0.0f32; 4];
        matte_of_output(mp, &[0.2, 1.5, -1.0, f32::NAN], (2, 2), &mut m);
        assert_eq!(m, [0.2, 1.0, 0.0, 0.0]);
        let pp = catalogue::find("pphumanseg-v2-lite").unwrap();
        matte_of_output(
            pp,
            &[0.9, 0.9, 0.9, 0.9, 0.1, 0.2, 0.3, 0.4],
            (2, 2),
            &mut m,
        );
        assert_eq!(m, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn the_ema_smooths_and_resets() {
        let mut e = Ema::new(0.5);
        assert!(e.is_reset());
        let mut m = vec![1.0f32; 3];
        e.apply(&mut m);
        assert_eq!(m, [1.0; 3]);
        let mut m = vec![0.0f32; 3];
        e.apply(&mut m);
        assert_eq!(m, [0.5; 3]);
        let mut m = vec![0.0f32; 3];
        e.apply(&mut m);
        assert_eq!(m, [0.25; 3]);
        e.reset();
        assert!(e.is_reset());
        let mut m = vec![0.0f32; 3];
        e.apply(&mut m);
        assert_eq!(m, [0.0; 3], "a reset takes the next matte as it is");
        // A matte of another size starts over too.
        let mut m = vec![1.0f32; 5];
        e.apply(&mut m);
        assert_eq!(m, [1.0; 5]);
    }

    /// A frame whose left half is a person (bright, warm) and whose right
    /// half is a room with fine detail (a checkerboard), with the matte to
    /// match.
    fn scene(w: usize, h: usize, pitch: usize) -> (Vec<u8>, Vec<f32>, (usize, usize)) {
        let mut b = frame(w, h, pitch, 0, (128, 128));
        for r in 0..h {
            for x in 0..w {
                b[r * pitch + x] = if x < w / 2 {
                    200
                } else if (x / 2 + r / 2) % 2 == 0 {
                    16
                } else {
                    235
                };
            }
        }
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                let (u, v) = if c < w / 4 { (100, 170) } else { (128, 128) };
                b[(h + r) * pitch + 2 * c] = u;
                b[(h + r) * pitch + 2 * c + 1] = v;
            }
        }
        let (mh, mw) = (8, 16);
        let matte = (0..mh * mw)
            .map(|i| if i % mw < mw / 2 { 1.0 } else { 0.0 })
            .collect();
        (b, matte, (mh, mw))
    }

    #[test]
    fn blur_keeps_the_person_and_flattens_the_room() {
        for (w, h, pitch, threads) in [(128, 72, 128, 1), (128, 72, 144, 4), (96, 54, 100, 3)] {
            let (mut b, matte, m) = scene(w, h, pitch);
            let guard = b.clone();
            let input = planes(&mut b, w, h, pitch);
            let mut out_buf = vec![0xEEu8; pitch * (h + h / 2)];
            let output = planes(&mut out_buf, w, h, pitch);
            let mut c = Compositor::new(w, h, m, threads);
            c.composite(&input, &output, &matte, &Mode::Blur, 1.0);
            assert_eq!(b, guard, "the input is not written");
            let y = |r: usize, x: usize| out_buf[r * pitch + x];
            // The person (well inside the left half) is unchanged.
            assert_eq!(y(h / 2, w / 8), 200);
            assert_eq!(out_buf[(h + h / 4) * pitch + 2 * (w / 16)], 100);
            // The room's checkerboard is averaged to a flat grey, and the
            // person's brightness does not bleed into it.
            let room: Vec<u8> = (w * 5 / 8..w).map(|x| y(h / 2, x)).collect();
            let (lo, hi) = (*room.iter().min().unwrap(), *room.iter().max().unwrap());
            assert!(hi - lo < 40, "{room:?}");
            assert!((110..=150).contains(&lo), "{room:?}");
            // The bytes past each row (pitch > width) are untouched.
            if pitch > w {
                for r in 0..h + h / 2 {
                    assert!(
                        out_buf[r * pitch + w..(r + 1) * pitch]
                            .iter()
                            .all(|&v| v == 0xEE)
                    );
                }
            }
        }
    }

    #[test]
    fn a_blur_of_strength_zero_copies_the_frame_and_in_place_works() {
        let (w, h, pitch) = (64, 36, 64);
        let (mut b, matte, m) = scene(w, h, pitch);
        let orig = b.clone();
        let p = planes(&mut b, w, h, pitch);
        let mut c = Compositor::new(w, h, m, 2);
        assert_eq!(c.blur_radius(0.0), 0);
        assert_eq!(c.blur_radius(f32::NAN), 0);
        assert!(c.blur_radius(0.01) >= 1);
        c.composite(&p, &p, &matte, &Mode::Blur, 0.0);
        assert_eq!(b, orig);
        // In place, with a blur: the person is kept, the room changes.
        let p = planes(&mut b, w, h, pitch);
        c.composite(&p, &p, &matte, &Mode::Blur, 1.0);
        assert_eq!(b[(h / 2) * pitch + 4], 200);
        assert_ne!(b[(h / 2) * pitch + w - 4], orig[(h / 2) * pitch + w - 4]);
    }

    #[test]
    fn replace_puts_the_image_where_the_matte_is_zero() {
        let (w, h, pitch) = (64, 36, 72);
        let (mut b, matte, m) = scene(w, h, pitch);
        let input = planes(&mut b, w, h, pitch);
        let mut bg = Nv12::new(w, h);
        bg.y.fill(50);
        bg.uv.chunks_mut(2).for_each(|c| {
            c[0] = 90;
            c[1] = 160;
        });
        let mut out_buf = vec![0u8; pitch * (h + h / 2)];
        let output = planes(&mut out_buf, w, h, pitch);
        let mut c = Compositor::new(w, h, m, 2);
        c.composite(&input, &output, &matte, &Mode::Replace(bg), 1.0);
        assert_eq!(out_buf[(h / 2) * pitch + 4], 200, "the person");
        assert_eq!(out_buf[(h / 2) * pitch + w - 4], 50, "the image");
        let uv = (h + h / 4) * pitch;
        assert_eq!(out_buf[uv + w - 4..uv + w - 2], [90, 160]);
        assert_eq!(out_buf[uv + 2..uv + 4], [100, 170]);
        // Half a matte gives half of each.
        let half = vec![0.5f32; m.0 * m.1];
        let mut bg = Nv12::new(w, h);
        bg.y.fill(0);
        c.composite(&input, &output, &half, &Mode::Replace(bg), 1.0);
        assert_eq!(out_buf[(h / 2) * pitch + 4], 100);
    }

    #[test]
    fn the_background_image_covers_the_frame() {
        // A 4x2 BGR image: left half red, right half blue, padded rows.
        let (bw, bh, pitch) = (4, 2, 16);
        let mut bgr = vec![0u8; pitch * bh];
        for y in 0..bh {
            for x in 0..bw {
                let px = if x < 2 { [0, 0, 255] } else { [255, 0, 0] };
                bgr[y * pitch + 3 * x..y * pitch + 3 * x + 3].copy_from_slice(&px);
            }
        }
        // A square frame crops the sides: the middle of the image.
        let img = background_nv12(&bgr, pitch, (bw, bh), (8, 8), &colour());
        let red = colour().to_yuv(255.0, 0.0, 0.0);
        let blue = colour().to_yuv(0.0, 0.0, 255.0);
        assert!((i32::from(img.y[0]) - i32::from(red[0])).abs() <= 2);
        assert!((i32::from(img.y[7]) - i32::from(blue[0])).abs() <= 2);
        assert!((i32::from(img.uv[1]) - i32::from(red[2])).abs() <= 3);
        // A wide frame covers by height.
        let img = background_nv12(&bgr, pitch, (bw, bh), (16, 4), &colour());
        assert_eq!(
            (img.width, img.height, img.y.len(), img.uv.len()),
            (16, 4, 64, 32)
        );
    }

    #[test]
    fn box_blur_preserves_the_mean_and_flattens() {
        let (w, h) = (16, 8);
        let mut img: Vec<f32> = (0..w * h)
            .map(|i| if i % 2 == 0 { 0.0 } else { 100.0 })
            .collect();
        box_blur3(&mut img, w, h, 2);
        // Away from the edges (which repeat their last sample).
        for y in 0..h {
            for x in 4..w - 4 {
                let v = img[y * w + x];
                assert!((v - 50.0).abs() < 5.0, "{x},{y}: {v}");
            }
        }
        let mean = img.iter().sum::<f32>() / img.len() as f32;
        assert!((mean - 50.0).abs() < 2.0, "{mean}");
        let mut flat = vec![7.0f32; w * h];
        box_blur3(&mut flat, w, h, 3);
        assert!(flat.iter().all(|v| (v - 7.0).abs() < 1e-4));
    }

    #[test]
    fn bands_cover_every_row_once() {
        use std::sync::Mutex;
        for (n, threads) in [(1080, 4), (10, 4), (36, 3), (2, 1)] {
            let seen = Mutex::new(vec![0u8; n]);
            bands(n, 2, threads, |a, b| {
                assert!(a % 2 == 0);
                let mut s = seen.lock().unwrap();
                for r in a..b {
                    s[r] += 1;
                }
            });
            assert!(seen.into_inner().unwrap().iter().all(|&c| c == 1));
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore = "a timing, run with --release --ignored"]
    fn composite_1080p() {
        let (w, h) = (1920usize, 1080usize);
        let mut buf = vec![100u8; w * (h + h / 2)];
        let mut out = vec![0u8; w * (h + h / 2)];
        let p = Planes {
            y: buf.as_mut_ptr(),
            y_pitch: w,
            uv: unsafe { buf.as_mut_ptr().add(w * h) },
            uv_pitch: w,
            width: w,
            height: h,
        };
        let o = Planes {
            y: out.as_mut_ptr(),
            y_pitch: w,
            uv: unsafe { out.as_mut_ptr().add(w * h) },
            uv_pitch: w,
            width: w,
            height: h,
        };
        let matte: Vec<f32> = (0..144 * 256).map(|i| (i % 256) as f32 / 256.0).collect();
        let mut c = Compositor::new(w, h, (144, 256), workers());
        for _ in 0..5 {
            let t = std::time::Instant::now();
            c.blur_room(&p, &matte, 12);
            let t1 = t.elapsed();
            let m12: Vec<u16> = matte.iter().map(|m| (m * 4096.0) as u16).collect();
            let t = std::time::Instant::now();
            let bg = Bg::Small {
                y: &c.small_y,
                u: &c.small_u,
                v: &c.small_v,
            };
            c.blend(&p, &o, &m12, &bg);
            let t2 = t.elapsed();
            let t = std::time::Instant::now();
            downscale_y(&p, c.sw, c.sh, &mut c.blur_y.clone());
            let t3 = t.elapsed();
            let mut img = c.blur_y.clone();
            let t = std::time::Instant::now();
            box_blur3(&mut img, c.sw, c.sh, 12);
            let t4 = t.elapsed();
            eprintln!("blur_room {t1:?} blend {t2:?} downscale_y {t3:?} box_blur3(y) {t4:?}");
        }
        // The whole composite, and the cost of handing out work to the
        // threads with nothing to do (the overhead per frame).
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let mut all = Vec::new();
        for _ in 0..200 {
            let t = std::time::Instant::now();
            c.composite(&p, &o, &matte, &Mode::Blur, 1.0);
            all.push(t.elapsed().as_secs_f64() * 1e6);
        }
        let mut empty = Vec::new();
        for _ in 0..200 {
            let t = std::time::Instant::now();
            // As many hand-outs as a composite makes: four tasks and the
            // blend's bands.
            run_tasks(&mut [&mut || {}, &mut || {}, &mut || {}, &mut || {}]);
            bands(h, 2, c.threads, |_, _| {});
            empty.push(t.elapsed().as_secs_f64() * 1e6);
        }
        eprintln!(
            "composite {:.0} us (median of 200), the hand-outs alone {:.1} us",
            median(all),
            median(empty)
        );
    }
}
