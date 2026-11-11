//! The BGR images the AR features read and write, and the conversions of
//! the host's NV12 frames into and out of them.

use std::ffi::c_void;

use obc_backend::DeviceNv12;

use crate::autoframe::Source;
use crate::ffi::{self, CuStream, NvCvImage, Sdk};
use crate::npp::BgrView;

/// An `NvCVImage` the SDK allocated (or an empty one), freed on drop. Boxed:
/// a feature keeps the address of an image bound to it.
pub(crate) struct Image {
    sdk: &'static Sdk,
    image: Box<NvCvImage>,
}

// SAFETY: the descriptor owns its device buffer; nothing in it is tied to
// the thread that allocated it (CUDA memory belongs to the context).
unsafe impl Send for Image {}

impl Image {
    /// An empty descriptor, for a staging image the SDK grows as needed.
    pub(crate) fn empty(sdk: &'static Sdk) -> Image {
        Image {
            sdk,
            image: Box::new(NvCvImage::empty()),
        }
    }

    /// A chunky 8-bit BGR image of `width` x `height` in CUDA memory of the
    /// current context, tightly pitched (rows `3 * width` bytes apart): the
    /// AR features refuse another pitch.
    pub(crate) fn gpu_bgr(sdk: &'static Sdk, width: u32, height: u32) -> Result<Image, String> {
        let mut img = Image::empty(sdk);
        // SAFETY: `img.image` is an empty descriptor; alignment 1 makes the
        // pitch tight.
        let rc = unsafe {
            (sdk.NvCVImage_Alloc)(
                &mut *img.image,
                width,
                height,
                ffi::NVCV_BGR,
                ffi::NVCV_U8,
                ffi::NVCV_CHUNKY,
                ffi::NVCV_GPU,
                ffi::NVCV_TIGHT_PITCH,
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(sdk.explain(&format!("NvCVImage_Alloc {width}x{height}"), rc));
        }
        Ok(img)
    }

    /// The descriptor, for binding to a feature (its address is stable).
    pub(crate) fn as_mut_ptr(&mut self) -> *mut NvCvImage {
        &mut *self.image
    }

    /// The pixels of an allocated chunky BGR image, for NPP.
    pub(crate) fn bgr_view(&self) -> BgrView {
        BgrView {
            pixels: self.image.pixels,
            pitch: self.image.pitch,
            width: self.image.width,
            height: self.image.height,
        }
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: the descriptor was filled by NvCVImage_Alloc, grown by the
        // SDK as a staging image, or is still empty, which Dealloc accepts;
        // it is freed once.
        unsafe { (self.sdk.NvCVImage_Dealloc)(&mut *self.image) };
    }
}

/// The NvCV colour-space flags of a YUV frame: matrix, range and chroma
/// siting, ORed.
pub(crate) fn colour_flags(matrix: u32, full_range: bool, siting: u32) -> u32 {
    let range = if full_range {
        ffi::NVCV_FULL_RANGE
    } else {
        ffi::NVCV_VIDEO_RANGE
    };
    matrix | range | siting
}

/// The images of a loaded chain, for one geometry: the camera frame in BGR
/// (what every AR stage reads), the eye-contact result in BGR, the
/// auto-frame result in BGR, and the conversions' staging image.
pub(crate) struct Frames {
    sdk: &'static Sdk,
    /// The camera frame, converted from NV12.
    pub input: Image,
    /// The result of the eye-contact stage.
    pub output: Image,
    /// The result of the auto-frame stage.
    pub framed: Image,
    staging: Image,
    /// The colour-space flags of the NV12 frames.
    flags: u32,
}

impl Frames {
    /// Allocates the images for `width` x `height` NV12 frames of the
    /// colour-space `flags` ([`colour_flags`]), in the current context.
    pub(crate) fn alloc(
        sdk: &'static Sdk,
        width: u32,
        height: u32,
        flags: u32,
    ) -> Result<Frames, String> {
        Ok(Frames {
            sdk,
            input: Image::gpu_bgr(sdk, width, height)?,
            output: Image::gpu_bgr(sdk, width, height)?,
            framed: Image::gpu_bgr(sdk, width, height)?,
            staging: Image::empty(sdk),
            flags,
        })
    }

    /// Converts the NV12 frame `src` into [`Frames::input`], on `stream`.
    ///
    /// # Safety
    /// `src` must describe live NV12 device memory of the current context,
    /// of the size the images were allocated for, readable until the work
    /// on `stream` has run.
    pub(crate) unsafe fn convert_in(
        &mut self,
        src: &DeviceNv12,
        stream: CuStream,
    ) -> Result<(), String> {
        // SAFETY: the caller's contract for `src`; U and V interleave in the
        // UV plane (V one byte after U, two bytes per pair). The destination
        // and staging images are ours.
        let rc = unsafe {
            (self.sdk.NvCVImage_TransferFromYUV)(
                src.y as *const c_void,
                1,
                src.y_pitch as i32,
                src.uv as *const c_void,
                (src.uv + 1) as *const c_void,
                2,
                src.uv_pitch as i32,
                ffi::NVCV_YUV420,
                ffi::NVCV_U8,
                self.flags,
                ffi::NVCV_GPU,
                self.input.as_mut_ptr(),
                std::ptr::null(),
                1.0,
                stream,
                self.staging.as_mut_ptr(),
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(self.sdk.explain("NvCVImage_TransferFromYUV", rc));
        }
        Ok(())
    }

    /// The image of `source`.
    fn image(&mut self, source: Source) -> &mut Image {
        match source {
            Source::Input => &mut self.input,
            Source::Eye => &mut self.output,
            Source::Framed => &mut self.framed,
        }
    }

    /// The pixels of the image of `source`.
    pub(crate) fn view(&mut self, source: Source) -> BgrView {
        self.image(source).bgr_view()
    }

    /// Copies the image of `source` (not the camera image) into the camera
    /// image, [`Frames::input`], on `stream`: what a stage bound to it reads
    /// after an earlier stage changed the picture.
    pub(crate) fn copy_to_input(&mut self, source: Source, stream: CuStream) -> Result<(), String> {
        if source == Source::Input {
            return Ok(());
        }
        let src: *const NvCvImage = self.image(source).as_mut_ptr();
        // SAFETY: two distinct images of ours, of one size and format, in
        // the current context; the staging image is ours too.
        let rc = unsafe {
            (self.sdk.NvCVImage_Transfer)(
                src,
                self.input.as_mut_ptr(),
                1.0,
                stream,
                self.staging.as_mut_ptr(),
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(self.sdk.explain("NvCVImage_Transfer", rc));
        }
        Ok(())
    }

    /// Converts the image of `source` into the NV12 frame `dst`, on
    /// `stream`.
    ///
    /// # Safety
    /// `dst` must describe live, writable NV12 device memory of the current
    /// context, of the size the images were allocated for, until the work
    /// on `stream` has run.
    pub(crate) unsafe fn convert_out(
        &mut self,
        source: Source,
        dst: &DeviceNv12,
        stream: CuStream,
    ) -> Result<(), String> {
        let out: *const NvCvImage = self.image(source).as_mut_ptr();
        // SAFETY: the caller's contract for `dst`; the source and staging
        // images are ours.
        let rc = unsafe {
            (self.sdk.NvCVImage_TransferToYUV)(
                out,
                std::ptr::null(),
                dst.y as *const c_void,
                1,
                dst.y_pitch as i32,
                dst.uv as *const c_void,
                (dst.uv + 1) as *const c_void,
                2,
                dst.uv_pitch as i32,
                ffi::NVCV_YUV420,
                ffi::NVCV_U8,
                self.flags,
                ffi::NVCV_GPU,
                1.0,
                stream,
                self.staging.as_mut_ptr(),
            )
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(self.sdk.explain("NvCVImage_TransferToYUV", rc));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_flags_or_the_matrix_range_and_siting() {
        assert_eq!(
            colour_flags(ffi::NVCV_601, false, ffi::NVCV_CHROMA_COSITED),
            0
        );
        assert_eq!(
            colour_flags(ffi::NVCV_709, true, ffi::NVCV_CHROMA_INTSTITIAL),
            0x01 | 0x04 | 0x08
        );
        assert_eq!(
            colour_flags(ffi::NVCV_2020, false, ffi::NVCV_CHROMA_TOPLEFT),
            0x02 | 0x10
        );
    }
}
