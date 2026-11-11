//! The auto-frame stage: a digital pan and zoom that keeps the face framed.
//!
//! Per fresh frame the stage takes the face (the box the eye-contact
//! feature tracked on this frame when that stage ran, else its own
//! `FaceBoxDetection` feature's on the camera image), moves the crop
//! ([`crate::framing::Framer`]) and, unless the crop is the full frame,
//! scales the crop of its source image (the eye-contact result when that
//! ran, else the camera image) to the whole of [`Frames::framed`] with NPP
//! ([`crate::npp`]), on the host's stream. The frame keeps its size.
//!
//! The face-box feature is loaded whenever the stage is in the chain, eye
//! contact or not: when eye contact fails on a frame, the host runs that
//! frame again without it, with no new load, and the stage must still find
//! the face then. With eye contact running, the feature is loaded but not
//! run. With temporal tracking on, it reports the largest face, Kalman
//! filtered.

use std::ffi::{CStr, CString, c_ulong};

use crate::ffi::{self, CuStream, FeatureHandle, NvArBBoxes, NvArRect, Sdk};
use crate::framing::{Framer, Framing, Rect};
use crate::image::Frames;
use crate::npp::{Npp, NppStreamContext};

/// The boxes the face-box feature may report.
const MAX_BOXES: usize = 4;

/// The size of the face-box feature's box of a face, as a share of the
/// eye-contact feature's box of it (the same centre): that box is square
/// and larger. Measured on the AR SDK 1.2 (0.833 on faces of 380 and 500
/// pixels), so the framing is the same with eye contact on or off.
pub(crate) const GAZE_TO_FACE_BOX: f32 = 0.833;

/// The buffers the face-box feature writes to. Boxed: the feature keeps
/// their addresses.
struct BoxOutputs {
    rects: [NvArRect; MAX_BOXES],
    confidence: [f32; MAX_BOXES],
    boxes: NvArBBoxes,
}

/// A loaded `FaceBoxDetection` feature, bound to [`Frames::input`] of one
/// [`Frames`] (which must outlive it) and to one stream.
pub(crate) struct FaceBox {
    sdk: &'static Sdk,
    handle: FeatureHandle,
    outputs: Box<BoxOutputs>,
}

// SAFETY: the handle is an opaque SDK object used from one thread at a time
// (the host serialises the calls on an instance); the buffers are owned.
unsafe impl Send for FaceBox {}

impl FaceBox {
    /// Creates, configures and loads the feature for the camera image of
    /// `frames`, on `stream`, in the current context.
    pub(crate) fn load(
        sdk: &'static Sdk,
        frames: &mut Frames,
        stream: CuStream,
    ) -> Result<FaceBox, String> {
        let mut handle: FeatureHandle = std::ptr::null_mut();
        // SAFETY: a valid feature id and an out-pointer.
        let rc =
            unsafe { (sdk.NvAR_Create)(ffi::FEATURE_FACE_BOX_DETECTION.as_ptr(), &mut handle) };
        if rc != ffi::NVCV_SUCCESS || handle.is_null() {
            ffi::destroy_failed(sdk, handle);
            return Err(sdk.explain("NvAR_Create(FaceBoxDetection)", rc));
        }
        let mut outputs = Box::new(BoxOutputs {
            rects: [NvArRect::default(); MAX_BOXES],
            confidence: [0.0; MAX_BOXES],
            boxes: NvArBBoxes {
                boxes: std::ptr::null_mut(),
                num_boxes: 0,
                max_boxes: MAX_BOXES as u8,
            },
        });
        outputs.boxes.boxes = outputs.rects.as_mut_ptr();
        // From here on a failure destroys the feature (Drop).
        let mut fb = FaceBox {
            sdk,
            handle,
            outputs,
        };
        fb.configure(frames, stream)?;
        Ok(fb)
    }

    fn configure(&mut self, frames: &mut Frames, stream: CuStream) -> Result<(), String> {
        let sdk = self.sdk;
        let h = self.handle;
        let check = |what: &str, name: &CStr, rc: ffi::NvCvStatus| -> Result<(), String> {
            if rc == ffi::NVCV_SUCCESS {
                Ok(())
            } else {
                Err(sdk.explain(&format!("{what}({})", name.to_string_lossy()), rc))
            }
        };
        let model_dir = CString::new(sdk.model_dir.as_os_str().as_encoded_bytes())
            .map_err(|_| "the model directory has a NUL in its path".to_string())?;
        // SAFETY (the calls below): a live handle, NUL-terminated names, and
        // pointers to buffers that outlive the feature (the boxed outputs,
        // the camera image of `frames`, which the caller keeps for as long
        // as the feature lives). The stream is the host's.
        check("NvAR_SetCudaStream", ffi::P_CUDA_STREAM, unsafe {
            (sdk.NvAR_SetCudaStream)(h, ffi::P_CUDA_STREAM.as_ptr(), stream)
        })?;
        check("NvAR_SetString", ffi::P_MODEL_DIR, unsafe {
            (sdk.NvAR_SetString)(h, ffi::P_MODEL_DIR.as_ptr(), model_dir.as_ptr())
        })?;
        check("NvAR_SetU32", ffi::P_TEMPORAL, unsafe {
            (sdk.NvAR_SetU32)(h, ffi::P_TEMPORAL.as_ptr(), 1)
        })?;
        check("NvAR_Load", ffi::FEATURE_FACE_BOX_DETECTION, unsafe {
            (sdk.NvAR_Load)(h)
        })?;
        let image_size = std::mem::size_of::<ffi::NvCvImage>() as c_ulong;
        check("NvAR_SetObject", ffi::P_IN_IMAGE, unsafe {
            (sdk.NvAR_SetObject)(
                h,
                ffi::P_IN_IMAGE.as_ptr(),
                frames.input.as_mut_ptr().cast(),
                image_size,
            )
        })?;
        let o = &mut *self.outputs;
        check("NvAR_SetObject", ffi::P_OUT_BOUNDING_BOXES, unsafe {
            (sdk.NvAR_SetObject)(
                h,
                ffi::P_OUT_BOUNDING_BOXES.as_ptr(),
                (&mut o.boxes as *mut NvArBBoxes).cast(),
                std::mem::size_of::<NvArBBoxes>() as c_ulong,
            )
        })?;
        check(
            "NvAR_SetF32Array",
            ffi::P_OUT_BOUNDING_BOXES_CONFIDENCE,
            unsafe {
                (sdk.NvAR_SetF32Array)(
                    h,
                    ffi::P_OUT_BOUNDING_BOXES_CONFIDENCE.as_ptr(),
                    o.confidence.as_mut_ptr(),
                    MAX_BOXES as i32,
                )
            },
        )?;
        Ok(())
    }

    /// Runs the feature on the camera image; the largest face it reports.
    pub(crate) fn run(&mut self) -> Result<Option<NvArRect>, String> {
        self.outputs.boxes.num_boxes = 0;
        // SAFETY: a loaded feature whose bound buffers are alive.
        let rc = unsafe { (self.sdk.NvAR_Run)(self.handle) };
        if rc != ffi::NVCV_SUCCESS {
            return Err(self.sdk.explain("NvAR_Run(FaceBoxDetection)", rc));
        }
        let o = &self.outputs;
        let n = usize::from(o.boxes.num_boxes).min(MAX_BOXES);
        Ok(largest(&o.rects[..n]))
    }
}

impl Drop for FaceBox {
    fn drop(&mut self) {
        // SAFETY: created by NvAR_Create, destroyed once.
        unsafe { (self.sdk.NvAR_Destroy)(self.handle) };
    }
}

/// The box of the largest area, of those with one.
pub(crate) fn largest(boxes: &[NvArRect]) -> Option<NvArRect> {
    boxes
        .iter()
        .filter(|b| b.width > 0.0 && b.height > 0.0)
        .max_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)))
        .copied()
}

/// The eye-contact feature's box of a face as the face-box feature's.
pub(crate) fn from_gaze_box(r: NvArRect) -> NvArRect {
    let (cx, cy) = (r.x + r.width / 2.0, r.y + r.height / 2.0);
    let (w, h) = (r.width * GAZE_TO_FACE_BOX, r.height * GAZE_TO_FACE_BOX);
    NvArRect {
        x: cx - w / 2.0,
        y: cy - h / 2.0,
        width: w,
        height: h,
    }
}

/// A face box as the framer takes it.
pub(crate) fn rect_of(r: &NvArRect) -> Rect {
    Rect {
        x: f64::from(r.x),
        y: f64::from(r.y),
        width: f64::from(r.width),
        height: f64::from(r.height),
    }
}

/// Which image of the chain holds a frame's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// The camera image ([`Frames::input`]).
    Input,
    /// The eye-contact result ([`Frames::output`]).
    Eye,
    /// The auto-frame result ([`Frames::framed`]).
    Framed,
}

/// The auto-frame stage of a loaded chain.
pub(crate) struct AutoFrame {
    npp: &'static Npp,
    ctx: NppStreamContext,
    face_box: FaceBox,
    pub framer: Framer,
    /// The last fresh frame had a face, for the log.
    had_face: bool,
}

// SAFETY: the stream in the NPP context is the host's handle, used only on
// the instance's calls, which the host serialises; the face-box feature is
// `Send` (above).
unsafe impl Send for AutoFrame {}

impl AutoFrame {
    /// Loads the stage for the frames of `frames` (`width` x `height`), on
    /// `stream`, in the current context.
    pub(crate) fn load(
        sdk: &'static Sdk,
        npp: &'static Npp,
        frames: &mut Frames,
        width: u32,
        height: u32,
        stream: CuStream,
        framing: Framing,
    ) -> Result<AutoFrame, String> {
        let ctx = npp.stream_context(stream)?;
        let face_box = FaceBox::load(sdk, frames, stream)?;
        Ok(AutoFrame {
            npp,
            ctx,
            face_box,
            framer: Framer::new(width, height, framing),
            had_face: false,
        })
    }

    /// Frames a fresh frame at `now` (seconds): its face is `eye_face` when
    /// eye contact ran on it (`Some(None)`: it found none), else the
    /// face-box feature's. The camera image (and the eye-contact result)
    /// must be written. Returns where the result is, `from` itself when the
    /// crop is the full frame.
    pub(crate) fn run(
        &mut self,
        frames: &mut Frames,
        from: Source,
        eye_face: Option<Option<NvArRect>>,
        now: f64,
    ) -> Result<Source, String> {
        let face = match eye_face {
            Some(f) => f.map(from_gaze_box),
            None => {
                let f = self.face_box.run()?;
                if f.is_some() != self.had_face {
                    match f {
                        Some(f) => obc_module_sdk::log::debug!(
                            "auto-frame: a face at {:.0},{:.0} ({:.0}x{:.0})",
                            f.x,
                            f.y,
                            f.width,
                            f.height
                        ),
                        None => obc_module_sdk::log::debug!("auto-frame: no face"),
                    }
                }
                f
            }
        };
        self.had_face = face.is_some();
        let before = self.framer.is_full();
        let crop = self.framer.update(now, face.as_ref().map(rect_of).as_ref());
        if self.framer.is_full() != before {
            obc_module_sdk::log::debug!(
                "auto-frame: {}",
                if before {
                    "framing"
                } else {
                    "back to the full frame"
                }
            );
        }
        if self.framer.is_full() {
            return Ok(from);
        }
        let src = frames.view(from);
        let dst = frames.view(Source::Framed);
        // SAFETY: the chain's images, distinct (`from` is never the
        // framed one), of the current context and the loaded size; the
        // work is on the host's stream.
        unsafe {
            self.npp.crop_to(
                &self.ctx,
                &src,
                &dst,
                (crop.x, crop.y, crop.width, crop.height),
            )
        }?;
        Ok(Source::Framed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f32, y: f32, w: f32, h: f32) -> NvArRect {
        NvArRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn the_largest_box_is_the_face() {
        assert_eq!(largest(&[]), None);
        assert_eq!(largest(&[r(0.0, 0.0, 0.0, 10.0)]), None);
        let big = r(500.0, 100.0, 300.0, 320.0);
        assert_eq!(
            largest(&[r(10.0, 10.0, 50.0, 60.0), big, r(0.0, 0.0, 200.0, 200.0)]),
            Some(big)
        );
    }

    #[test]
    fn a_gaze_box_scales_about_its_centre() {
        let g = from_gaze_box(r(1201.0, 127.0, 379.0, 379.0));
        assert!((g.x + g.width / 2.0 - 1390.5).abs() < 1e-3, "{g:?}");
        assert!((g.y + g.height / 2.0 - 316.5).abs() < 1e-3, "{g:?}");
        assert!((g.height - 379.0 * GAZE_TO_FACE_BOX).abs() < 1e-3, "{g:?}");
    }

    #[test]
    fn a_box_converts_to_the_framers_rect() {
        let f = rect_of(&r(1.5, 2.0, 3.0, 4.25));
        assert_eq!(
            f,
            Rect {
                x: 1.5,
                y: 2.0,
                width: 3.0,
                height: 4.25
            }
        );
    }
}
