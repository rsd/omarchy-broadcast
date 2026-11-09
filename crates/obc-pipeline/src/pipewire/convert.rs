//! The one copy into a PipeWire buffer, converting on the way: a frame of
//! the contract (NV12 or I420, any strides) into the layout of the format
//! the consumer chose (NV12, YUY2 or I420, [`Layout`]). Pure CPU code,
//! byte exact: the planes are copied, chroma is (de)interleaved between
//! NV12 and I420, and 4:2:0 is packed into YUY2 by repeating each chroma
//! row for the two luma rows it covers (`Y0 U Y1 V`, chroma of row y/2).
//! Padding bytes of the destination are left as they are.

use obc_core::OutputFormat;

use super::format::{Layout, PwFormat};

/// One frame of the contract format, as mapped from a GStreamer buffer.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    pub format: OutputFormat,
    pub width: usize,
    pub height: usize,
    /// NV12: Y and UV (the third unused); I420: Y, U, V.
    pub planes: [&'a [u8]; 3],
    /// Bytes per row of each plane.
    pub strides: [usize; 3],
}

/// Why a frame could not be converted (the frame is dropped).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    #[error("source is {src_w}x{src_h}, destination {dst_w}x{dst_h}")]
    Size {
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
    },
    #[error("source plane {plane} holds {have} bytes, {need} needed")]
    SourcePlane {
        plane: usize,
        have: usize,
        need: usize,
    },
    #[error("destination holds {have} bytes, {need} needed")]
    Destination { have: usize, need: usize },
}

/// Bytes a plane of `rows` rows of `row_bytes` needs at `stride`.
fn plane_need(rows: usize, stride: usize, row_bytes: usize) -> usize {
    if rows == 0 {
        0
    } else {
        (rows - 1) * stride + row_bytes
    }
}

fn copy_plane(
    src: &[u8],
    src_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    row_bytes: usize,
    rows: usize,
) {
    for r in 0..rows {
        dst[r * dst_stride..r * dst_stride + row_bytes]
            .copy_from_slice(&src[r * src_stride..r * src_stride + row_bytes]);
    }
}

/// The U and V of chroma row `r`, pixel pair `i`.
#[inline]
fn chroma(src: &Source<'_>, r: usize, i: usize) -> (u8, u8) {
    match src.format {
        OutputFormat::Nv12 => {
            let row = &src.planes[1][r * src.strides[1]..];
            (row[2 * i], row[2 * i + 1])
        }
        OutputFormat::I420 => (
            src.planes[1][r * src.strides[1] + i],
            src.planes[2][r * src.strides[2] + i],
        ),
    }
}

/// Checks that `src` and `dst` hold a whole frame of `layout`.
fn check(src: &Source<'_>, dst: &[u8], layout: &Layout) -> Result<(), ConvertError> {
    let (w, h) = (src.width, src.height);
    if (w, h) != (layout.width, layout.height) {
        return Err(ConvertError::Size {
            src_w: w,
            src_h: h,
            dst_w: layout.width,
            dst_h: layout.height,
        });
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let needs: &[(usize, usize)] = match src.format {
        OutputFormat::Nv12 => &[(h, w), (ch, 2 * cw)],
        OutputFormat::I420 => &[(h, w), (ch, cw), (ch, cw)],
    };
    for (plane, &(rows, row_bytes)) in needs.iter().enumerate() {
        let need = plane_need(rows, src.strides[plane], row_bytes);
        let have = src.planes[plane].len();
        if have < need || (rows > 1 && src.strides[plane] < row_bytes) {
            return Err(ConvertError::SourcePlane { plane, have, need });
        }
    }
    if dst.len() < layout.size {
        return Err(ConvertError::Destination {
            have: dst.len(),
            need: layout.size,
        });
    }
    Ok(())
}

/// Converts `src` into `dst`, laid out as `layout` (its first
/// `layout.size` bytes are the frame).
pub fn convert(src: &Source<'_>, dst: &mut [u8], layout: &Layout) -> Result<(), ConvertError> {
    check(src, dst, layout)?;
    let (w, h) = (src.width, src.height);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let dst = &mut dst[..layout.size];
    match layout.format {
        PwFormat::Nv12 | PwFormat::I420 => {
            let (luma, chroma_part) = dst.split_at_mut(layout.offsets[1]);
            copy_plane(src.planes[0], src.strides[0], luma, layout.strides[0], w, h);
            match (src.format, layout.format) {
                (OutputFormat::Nv12, PwFormat::Nv12) => copy_plane(
                    src.planes[1],
                    src.strides[1],
                    chroma_part,
                    layout.strides[1],
                    2 * cw,
                    ch,
                ),
                (OutputFormat::I420, PwFormat::I420) => {
                    let (u, v) = chroma_part.split_at_mut(layout.offsets[2] - layout.offsets[1]);
                    copy_plane(src.planes[1], src.strides[1], u, layout.strides[1], cw, ch);
                    copy_plane(src.planes[2], src.strides[2], v, layout.strides[2], cw, ch);
                }
                (OutputFormat::I420, PwFormat::Nv12) => {
                    let ds = layout.strides[1];
                    for r in 0..ch {
                        let row = &mut chroma_part[r * ds..r * ds + 2 * cw];
                        for (i, uv) in row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                            let (u, v) = chroma(src, r, i);
                            uv[0] = u;
                            uv[1] = v;
                        }
                    }
                }
                (OutputFormat::Nv12, PwFormat::I420) => {
                    let (u, v) = chroma_part.split_at_mut(layout.offsets[2] - layout.offsets[1]);
                    let ds = layout.strides[1];
                    for r in 0..ch {
                        let src_row = &src.planes[1][r * src.strides[1]..][..2 * cw];
                        let (urow, vrow) = (&mut u[r * ds..][..cw], &mut v[r * ds..][..cw]);
                        for (i, uv) in src_row.as_chunks::<2>().0.iter().enumerate() {
                            urow[i] = uv[0];
                            vrow[i] = uv[1];
                        }
                    }
                }
                (_, PwFormat::Yuy2) => unreachable!("matched above"),
            }
        }
        PwFormat::Yuy2 => {
            let ds = layout.strides[0];
            for y in 0..h {
                let luma = &src.planes[0][y * src.strides[0]..][..w];
                let out = &mut dst[y * ds..][..4 * cw];
                for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (u, v) = chroma(src, y / 2, i);
                    let y0 = luma[2 * i];
                    // An odd width repeats the last luma sample.
                    let y1 = luma.get(2 * i + 1).copied().unwrap_or(y0);
                    *px = [y0, u, y1, v];
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic source frame: Y = `luma(x, y)`, U and V = `u(cx, cy)`,
    /// `v(cx, cy)`, with each plane's stride padded by `pad` bytes (filled
    /// with 0xEE, which must never reach the destination).
    struct Frame {
        format: OutputFormat,
        w: usize,
        h: usize,
        planes: Vec<Vec<u8>>,
        strides: Vec<usize>,
    }

    type Gen = fn(usize, usize) -> u8;

    impl Frame {
        fn new(
            format: OutputFormat,
            w: usize,
            h: usize,
            pad: usize,
            y: Gen,
            u: Gen,
            v: Gen,
        ) -> Self {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let plane = |pw: usize, ph: usize, stride: usize, f: &dyn Fn(usize, usize) -> u8| {
                let mut p = vec![0xEE; stride * ph];
                for r in 0..ph {
                    for c in 0..pw {
                        p[r * stride + c] = f(c, r);
                    }
                }
                p
            };
            let (planes, strides) = match format {
                OutputFormat::Nv12 => {
                    let s = 2 * cw + pad;
                    let uv = plane(2 * cw, ch, s, &|c, r| {
                        if c % 2 == 0 { u(c / 2, r) } else { v(c / 2, r) }
                    });
                    (vec![plane(w, h, w + pad, &y), uv], vec![w + pad, s])
                }
                OutputFormat::I420 => (
                    vec![
                        plane(w, h, w + pad, &y),
                        plane(cw, ch, cw + pad, &u),
                        plane(cw, ch, cw + pad, &v),
                    ],
                    vec![w + pad, cw + pad, cw + pad],
                ),
            };
            Frame {
                format,
                w,
                h,
                planes,
                strides,
            }
        }

        fn source(&self) -> Source<'_> {
            let empty: &[u8] = &[];
            Source {
                format: self.format,
                width: self.w,
                height: self.h,
                planes: [
                    &self.planes[0],
                    &self.planes[1],
                    self.planes.get(2).map_or(empty, |p| p.as_slice()),
                ],
                strides: [
                    self.strides[0],
                    self.strides[1],
                    self.strides.get(2).copied().unwrap_or(0),
                ],
            }
        }
    }

    fn run(src: &Frame, to: PwFormat) -> (Layout, Vec<u8>) {
        let layout = Layout::of(to, src.w as u32, src.h as u32);
        let mut dst = vec![0xAB; layout.size];
        convert(&src.source(), &mut dst, &layout).unwrap();
        (layout, dst)
    }

    /// Reads (Y, U, V) of pixel (x, y) from a converted frame.
    fn pixel(l: &Layout, d: &[u8], x: usize, y: usize) -> (u8, u8, u8) {
        match l.format {
            PwFormat::Nv12 => {
                let uv = l.offsets[1] + (y / 2) * l.strides[1] + 2 * (x / 2);
                (d[y * l.strides[0] + x], d[uv], d[uv + 1])
            }
            PwFormat::I420 => {
                let c = (y / 2) * l.strides[1] + x / 2;
                (
                    d[y * l.strides[0] + x],
                    d[l.offsets[1] + c],
                    d[l.offsets[2] + c],
                )
            }
            PwFormat::Yuy2 => {
                let p = y * l.strides[0] + 4 * (x / 2);
                (
                    d[p + if x.is_multiple_of(2) { 0 } else { 2 }],
                    d[p + 1],
                    d[p + 3],
                )
            }
        }
    }

    const SOURCES: [OutputFormat; 2] = [OutputFormat::Nv12, OutputFormat::I420];

    /// Limited-range black (16/128) stays black in every target format,
    /// whatever the source strides; padding never leaks in.
    #[test]
    fn black_stays_black_in_every_format() {
        for from in SOURCES {
            for pad in [0, 3, 12] {
                let src = Frame::new(from, 8, 6, pad, |_, _| 16, |_, _| 128, |_, _| 128);
                for to in PwFormat::ALL {
                    let (l, d) = run(&src, to);
                    for y in 0..6 {
                        for x in 0..8 {
                            assert_eq!(
                                pixel(&l, &d, x, y),
                                (16, 128, 128),
                                "{from}->{to} pad {pad}"
                            );
                        }
                    }
                    assert!(!d.contains(&0xEE), "{from}->{to}: source padding copied");
                }
            }
        }
    }

    /// Every sample lands where its format puts it: distinct Y per pixel,
    /// distinct U/V per chroma site, from both sources into all targets,
    /// with non-default source strides.
    #[test]
    fn every_sample_lands_in_place() {
        let y: Gen = |x, y| (16 + x + 10 * y) as u8;
        let u: Gen = |cx, cy| (100 + cx + 7 * cy) as u8;
        let v: Gen = |cx, cy| (200 - cx - 5 * cy) as u8;
        for from in SOURCES {
            for (w, h) in [(8, 4), (6, 6), (10, 2)] {
                let src = Frame::new(from, w, h, 5, y, u, v);
                for to in PwFormat::ALL {
                    let (l, d) = run(&src, to);
                    for py in 0..h {
                        for px in 0..w {
                            assert_eq!(
                                pixel(&l, &d, px, py),
                                (y(px, py), u(px / 2, py / 2), v(px / 2, py / 2)),
                                "{from}->{to} {w}x{h} at ({px},{py})"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The YUY2 packing in bytes: `Y0 U Y1 V`, each chroma row used by two
    /// luma rows.
    #[test]
    fn yuy2_packs_y0_u_y1_v() {
        let src = Frame::new(
            OutputFormat::Nv12,
            4,
            2,
            0,
            |x, y| (x + 4 * y) as u8,
            |cx, _| 50 + cx as u8,
            |cx, _| 60 + cx as u8,
        );
        let (l, d) = run(&src, PwFormat::Yuy2);
        assert_eq!(l.strides[0], 8);
        assert_eq!(
            d,
            vec![0, 50, 1, 60, 2, 51, 3, 61, 4, 50, 5, 60, 6, 51, 7, 61]
        );
    }

    /// A same-format copy is the source frame, rows re-strided.
    #[test]
    fn a_same_format_copy_is_the_source() {
        let src = Frame::new(
            OutputFormat::I420,
            6,
            4,
            0,
            |x, y| (x * y) as u8,
            |_, _| 1,
            |_, _| 2,
        );
        // 6 wide: chroma 3, padded to 4 in the destination.
        let (l, d) = run(&src, PwFormat::I420);
        assert_eq!(l.strides, [8, 4, 4]);
        for r in 0..4 {
            assert_eq!(&d[r * 8..r * 8 + 6], &src.planes[0][r * 6..r * 6 + 6]);
        }
    }

    #[test]
    fn short_planes_and_destinations_are_refused() {
        let src = Frame::new(
            OutputFormat::Nv12,
            8,
            4,
            0,
            |_, _| 16,
            |_, _| 128,
            |_, _| 128,
        );
        let layout = Layout::of(PwFormat::Nv12, 8, 4);
        let mut small = vec![0; layout.size - 1];
        assert!(matches!(
            convert(&src.source(), &mut small, &layout),
            Err(ConvertError::Destination { .. })
        ));
        let mut dst = vec![0; layout.size];
        let mut s = src.source();
        s.planes[1] = &s.planes[1][..s.planes[1].len() - 1];
        assert!(matches!(
            convert(&s, &mut dst, &layout),
            Err(ConvertError::SourcePlane { plane: 1, .. })
        ));
        let other = Layout::of(PwFormat::Nv12, 8, 2);
        assert!(matches!(
            convert(&src.source(), &mut dst, &other),
            Err(ConvertError::Size { .. })
        ));
    }
}
