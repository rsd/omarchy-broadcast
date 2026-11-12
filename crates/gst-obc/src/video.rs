//! Frame helpers shared by the elements: the colour description of the
//! caps, the CUDA context of an input buffer, a video frame mapped with
//! `GST_MAP_CUDA`, and the pixel operations on NV12 frames in system memory
//! (a fill, a copy, a copy of a packed image).

use std::ffi::c_void;

use gst::glib;
use gst_video::VideoInfo;
use obc_backend::{ColourMatrix, ColourSpec, DeviceNv12, HostNv12};

use crate::cuda::{self, CudaMemoryHead};

/// The backend's colour description of a GStreamer colorimetry: BT.601 or
/// BT.709 matrix, full or limited range. Anything else is refused.
pub(crate) fn colour_of(c: &gst_video::VideoColorimetry) -> Result<ColourSpec, String> {
    let full_range = match c.range() {
        gst_video::VideoColorRange::Range0_255 => true,
        gst_video::VideoColorRange::Range16_235 => false,
        other => {
            return Err(format!(
                "colour range {other:?} (colorimetry {c}) cannot be processed: full or \
                 limited range is needed"
            ));
        }
    };
    let matrix = match c.matrix() {
        gst_video::VideoColorMatrix::Bt601 => ColourMatrix::Bt601,
        gst_video::VideoColorMatrix::Bt709 => ColourMatrix::Bt709,
        other => {
            return Err(format!(
                "colour matrix {other:?} (colorimetry {c}) cannot be processed: BT.601 or \
                 BT.709 is needed"
            ));
        }
    };
    Ok(ColourSpec { matrix, full_range })
}

/// The `GstCudaContext` of the input's (single) memory, or why there is
/// none.
pub(crate) fn input_context(
    lib: &cuda::GstCuda,
    inbuf: &gst::BufferRef,
) -> Result<*mut c_void, String> {
    if inbuf.n_memory() != 1 {
        return Err(format!(
            "an input frame has {} memories, not one CUDA memory",
            inbuf.n_memory()
        ));
    }
    let mem = inbuf.peek_memory(0).as_mut_ptr();
    // SAFETY: `mem` is a live GstMemory of the buffer.
    if unsafe { (lib.gst_is_cuda_memory)(mem) } == glib::ffi::GFALSE {
        return Err("an input frame is not in CUDA memory".into());
    }
    // SAFETY: `gst_is_cuda_memory` said it is a GstCudaMemory, whose public
    // head is `CudaMemoryHead`.
    let ctx = unsafe { (*mem.cast::<CudaMemoryHead>()).context };
    if ctx.is_null() {
        return Err("an input CUDA memory has no context".into());
    }
    Ok(ctx)
}

/// A video frame mapped with `GST_MAP_CUDA`: plane pointers are device
/// addresses, laid out as the buffer's video meta says. Unmapped on drop.
pub(crate) struct CudaFrame {
    frame: gst_video::ffi::GstVideoFrame,
}

impl CudaFrame {
    /// # Safety
    /// `buffer` must be a live buffer of CUDA memory described by `info`,
    /// writable if `flags` has `GST_MAP_WRITE`, and outlive the frame.
    pub(crate) unsafe fn map(
        buffer: *mut gst::ffi::GstBuffer,
        info: &VideoInfo,
        flags: u32,
    ) -> Result<CudaFrame, String> {
        use gst::glib::translate::ToGlibPtr;
        // SAFETY: a zeroed GstVideoFrame is the documented initial state.
        let mut frame: gst_video::ffi::GstVideoFrame = unsafe { std::mem::zeroed() };
        // SAFETY: the caller guarantees `buffer`; NO_REF keeps the buffer's
        // refcount (and so its writability) unchanged.
        let ok = unsafe {
            gst_video::ffi::gst_video_frame_map(
                &mut frame,
                info.to_glib_none().0 as *mut _,
                buffer,
                flags | gst_video::ffi::GST_VIDEO_FRAME_MAP_FLAG_NO_REF,
            )
        };
        if ok == glib::ffi::GFALSE {
            return Err("could not map a frame's CUDA memory".into());
        }
        Ok(CudaFrame { frame })
    }

    pub(crate) fn nv12(&self) -> DeviceNv12 {
        let f = &self.frame;
        DeviceNv12 {
            y: f.data[0] as u64,
            y_pitch: f.info.stride[0].max(0) as usize,
            uv: f.data[1] as u64,
            uv_pitch: f.info.stride[1].max(0) as usize,
            width: f.info.width.max(0) as u32,
            height: f.info.height.max(0) as u32,
        }
    }
}

impl Drop for CudaFrame {
    fn drop(&mut self) {
        // SAFETY: mapped by `map`, unmapped once.
        unsafe { gst_video::ffi::gst_video_frame_unmap(&mut self.frame) };
    }
}

/// A mapped system-memory NV12 frame as the backend seam describes it: its
/// plane pointers, pitches and size.
///
/// The pointers are those of the map: valid while `frame` stays mapped.
/// For a frame mapped read-only the planes are only ever read (the seam's
/// rule for a source frame).
pub(crate) fn host_nv12(frame: &impl gst_video::VideoFrameExt) -> HostNv12 {
    // SAFETY: `GstVideoFrame::data` holds the plane addresses of the mapped
    // frame; reading them takes no access to the pixels.
    let data = unsafe { (*frame.as_ptr()).data };
    HostNv12 {
        y: data[0].cast(),
        y_pitch: frame.plane_stride()[0].max(0) as usize,
        uv: data[1].cast(),
        uv_pitch: frame.plane_stride()[1].max(0) as usize,
        width: frame.width(),
        height: frame.height(),
    }
}

/// Fills the visible part of `dst` (every row's first `width` bytes) with
/// luma `y` and chroma `uv`: black for [`ColourSpec::black_nv12`].
///
/// **Even sizes only.** This and the other pixel operations here take
/// frames of even, non-zero width and height (NV12's chroma plane is half
/// the size both ways, and `HostNv12::planes_mut` refuses anything else).
/// A frame of odd size is an error, and nothing of it is written: the
/// effect then fails the stream rather than show a partly written frame.
/// That never happens in this daemon's pipeline, whose canonical sizes are
/// always even; it is a deliberate fail-closed limit, not a gap.
///
/// # Safety
/// `dst`'s planes are as [`HostNv12`] documents: valid for writes of their
/// whole extent and not accessed through anything else during the call.
pub(crate) unsafe fn fill_nv12(dst: &mut HostNv12, (y, uv): (u8, u8)) -> Result<(), String> {
    let (w, h) = (dst.width as usize, dst.height as usize);
    let (yp, uvp) = (dst.y_pitch, dst.uv_pitch);
    // SAFETY: the caller's contract.
    let (y_plane, uv_plane) = unsafe { dst.planes_mut() }.map_err(|e| e.to_string())?;
    for r in 0..h {
        y_plane[r * yp..r * yp + w].fill(y);
    }
    for r in 0..h / 2 {
        uv_plane[r * uvp..r * uvp + w].fill(uv);
    }
    Ok(())
}

/// Copies the visible part of `src` into `dst`, row by row. Both have the
/// same, even size (see [`fill_nv12`]).
///
/// # Safety
/// Both are as [`HostNv12`] documents; `src` is only read (it may be
/// mapped read-only), and the two do not overlap.
pub(crate) unsafe fn copy_nv12(src: &HostNv12, dst: &mut HostNv12) -> Result<(), String> {
    src.validate().map_err(|e| e.to_string())?;
    if (src.width, src.height) != (dst.width, dst.height) {
        return Err(format!(
            "cannot copy a {}x{} frame into a {}x{} one",
            src.width, src.height, dst.width, dst.height
        ));
    }
    let (w, h) = (src.width as usize, src.height as usize);
    let (dyp, duvp) = (dst.y_pitch, dst.uv_pitch);
    // SAFETY: the caller's contract for `dst`.
    let (y_plane, uv_plane) = unsafe { dst.planes_mut() }.map_err(|e| e.to_string())?;
    for r in 0..h {
        // SAFETY: row `r` of the source's Y plane is `w` readable bytes
        // (validated above), apart from `dst`.
        let row = unsafe { std::slice::from_raw_parts(src.y.add(r * src.y_pitch), w) };
        y_plane[r * dyp..r * dyp + w].copy_from_slice(row);
    }
    for r in 0..h / 2 {
        // SAFETY: as above, for the UV plane.
        let row = unsafe { std::slice::from_raw_parts(src.uv.add(r * src.uv_pitch), w) };
        uv_plane[r * duvp..r * duvp + w].copy_from_slice(row);
    }
    Ok(())
}

/// Copies packed NV12 of `dst`'s size (the Y rows, then the UV rows, each
/// `width` bytes, as the fallback image is decoded) into `dst`, of even size
/// (see [`fill_nv12`]).
///
/// # Safety
/// As [`fill_nv12`].
pub(crate) unsafe fn copy_packed_nv12(packed: &[u8], dst: &mut HostNv12) -> Result<(), String> {
    let (w, h) = (dst.width as usize, dst.height as usize);
    if packed.len() != w * h * 3 / 2 {
        return Err(format!(
            "a packed {w}x{h} NV12 image is {} bytes, not {}",
            w * h * 3 / 2,
            packed.len()
        ));
    }
    let (yp, uvp) = (dst.y_pitch, dst.uv_pitch);
    // SAFETY: the caller's contract.
    let (y_plane, uv_plane) = unsafe { dst.planes_mut() }.map_err(|e| e.to_string())?;
    let (y_rows, uv_rows) = packed.split_at(w * h);
    for (r, row) in y_rows.chunks_exact(w).enumerate() {
        y_plane[r * yp..r * yp + w].copy_from_slice(row);
    }
    for (r, row) in uv_rows.chunks_exact(w).enumerate() {
        uv_plane[r * uvp..r * uvp + w].copy_from_slice(row);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x4 NV12 frame with 6-byte rows, every byte `v`.
    struct Planes {
        y: Vec<u8>,
        uv: Vec<u8>,
    }

    const W: u32 = 4;
    const H: u32 = 4;
    const PITCH: usize = 6;

    impl Planes {
        fn new(v: u8) -> Planes {
            Planes {
                y: vec![v; PITCH * H as usize],
                uv: vec![v; PITCH * H as usize / 2],
            }
        }

        fn frame(&mut self) -> HostNv12 {
            HostNv12 {
                y: self.y.as_mut_ptr(),
                y_pitch: PITCH,
                uv: self.uv.as_mut_ptr(),
                uv_pitch: PITCH,
                width: W,
                height: H,
            }
        }

        /// The visible bytes, packed.
        fn visible(&self) -> Vec<u8> {
            let w = W as usize;
            let mut out = Vec::new();
            for r in 0..H as usize {
                out.extend_from_slice(&self.y[r * PITCH..r * PITCH + w]);
            }
            for r in 0..H as usize / 2 {
                out.extend_from_slice(&self.uv[r * PITCH..r * PITCH + w]);
            }
            out
        }

        /// The padding bytes past the width of every row.
        fn padding(&self) -> Vec<u8> {
            let w = W as usize;
            self.y
                .chunks(PITCH)
                .chain(self.uv.chunks(PITCH))
                .flat_map(|row| row[w..].to_vec())
                .collect()
        }
    }

    #[test]
    fn a_fill_writes_the_visible_rows_only() {
        let mut p = Planes::new(7);
        // SAFETY: the planes are `p`'s vectors, alive and unaliased.
        unsafe { fill_nv12(&mut p.frame(), ColourSpec::LIMITED_BT601.black_nv12()) }.unwrap();
        let v = p.visible();
        let y_len = (W * H) as usize;
        assert!(v[..y_len].iter().all(|b| *b == 16), "{v:?}");
        assert!(v[y_len..].iter().all(|b| *b == 128), "{v:?}");
        assert!(p.padding().iter().all(|b| *b == 7), "the padding is kept");
        // SAFETY: as above.
        unsafe { fill_nv12(&mut p.frame(), ColourSpec::FULL_BT601.black_nv12()) }.unwrap();
        assert!(p.visible()[..y_len].iter().all(|b| *b == 0));
    }

    #[test]
    fn a_copy_takes_every_visible_byte_and_reads_the_source_only() {
        let mut src = Planes::new(0);
        for (i, b) in src.y.iter_mut().chain(src.uv.iter_mut()).enumerate() {
            *b = i as u8;
        }
        let before = (src.y.clone(), src.uv.clone());
        let mut dst = Planes::new(9);
        let s = src.frame();
        // SAFETY: two distinct pairs of vectors, alive and unaliased.
        unsafe { copy_nv12(&s, &mut dst.frame()) }.unwrap();
        assert_eq!(dst.visible(), src.visible());
        assert!(dst.padding().iter().all(|b| *b == 9));
        assert_eq!((src.y, src.uv), before, "the source is unchanged");
        let mut small = Planes::new(0);
        let mut f = small.frame();
        f.width = 2;
        // SAFETY: as above.
        let e = unsafe { copy_nv12(&s, &mut f) }.unwrap_err();
        assert!(e.contains("4x4 frame into a 2x4"), "{e}");
    }

    #[test]
    fn a_packed_image_lands_row_by_row() {
        let packed: Vec<u8> = (0..(W * H * 3 / 2) as u8).collect();
        let mut dst = Planes::new(5);
        // SAFETY: the planes are `dst`'s vectors, alive and unaliased.
        unsafe { copy_packed_nv12(&packed, &mut dst.frame()) }.unwrap();
        assert_eq!(dst.visible(), packed);
        assert!(dst.padding().iter().all(|b| *b == 5));
        // SAFETY: as above.
        let e = unsafe { copy_packed_nv12(&packed[1..], &mut dst.frame()) }.unwrap_err();
        assert!(e.contains("is 24 bytes, not 23"), "{e}");
    }
}
