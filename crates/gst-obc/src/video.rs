//! Frame helpers shared by the elements: the colour description of the
//! caps, the CUDA context of an input buffer, and a video frame mapped
//! with `GST_MAP_CUDA`.

use std::ffi::c_void;

use gst::glib;
use gst_video::VideoInfo;
use obc_backend::{ColourMatrix, ColourSpec, DeviceNv12};

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
