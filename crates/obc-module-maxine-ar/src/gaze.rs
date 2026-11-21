//! The eye-contact stage: the AR SDK's `GazeRedirection` feature.
//!
//! The feature finds the largest face (face detection and landmark
//! detection run inside it, with temporal tracking: one face), estimates
//! the gaze and writes the frame with the eyes redirected towards the
//! camera into its output image, full frame; with no face, the output is
//! the input. Fixed here: temporal tracking on, redirection on, 126
//! landmarks, CUDA graphs off (see `configure`), and the SDK's defaults for
//! everything else (the gaze and
//! head-pose thresholds, the eye-size sensitivity, the closure detection
//! and the look-away timing). The one choice is the randomised look-away
//! (`GazeConfig::look_away`), applied by a new load.

use std::ffi::{CStr, CString, c_ulong, c_void};

use crate::ffi::{
    self, CuStream, FeatureHandle, NvArBBoxes, NvArPoint2f, NvArPoint3f, NvArQuaternion, NvArRect,
    Sdk,
};
use crate::image::Frames;

/// The number of landmarks the feature tracks (68 or 126).
pub(crate) const LANDMARKS: usize = 126;

/// What a load of the feature is configured with, besides the frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GazeConfig {
    /// The randomised look-away (`EnableLookAway`).
    pub look_away: bool,
}

/// The buffers the feature writes its estimates to. Boxed: the feature
/// keeps their addresses.
struct Outputs {
    landmarks: [NvArPoint2f; LANDMARKS],
    confidence: [f32; LANDMARKS],
    head_pose: NvArQuaternion,
    gaze: [f32; 2],
    head_translation: [f32; 3],
    gaze_direction: [NvArPoint3f; 2],
    rects: [NvArRect; 1],
    boxes: NvArBBoxes,
}

/// A loaded `GazeRedirection` feature, bound to the images of one
/// [`Frames`] (which must outlive it) and to one stream.
pub(crate) struct EyeContact {
    sdk: &'static Sdk,
    handle: FeatureHandle,
    config: GazeConfig,
    outputs: Box<Outputs>,
}

// SAFETY: the handle is an opaque SDK object used from one thread at a time
// (the host serialises the calls on an instance); the output buffers are
// owned here.
unsafe impl Send for EyeContact {}

impl EyeContact {
    /// Creates, configures and loads the feature (its models) for the
    /// images of `frames` (`width` x `height`), on `stream`, in the current
    /// context. Blocks for as long as the models take to load.
    pub(crate) fn load(
        sdk: &'static Sdk,
        frames: &mut Frames,
        width: u32,
        height: u32,
        stream: CuStream,
        config: GazeConfig,
    ) -> Result<EyeContact, String> {
        let mut handle: FeatureHandle = std::ptr::null_mut();
        // SAFETY: a valid feature id and an out-pointer.
        let rc = unsafe { (sdk.NvAR_Create)(ffi::FEATURE_GAZE_REDIRECTION.as_ptr(), &mut handle) };
        if rc != ffi::NVCV_SUCCESS || handle.is_null() {
            ffi::destroy_failed(sdk, handle);
            return Err(sdk.explain("NvAR_Create(GazeRedirection)", rc));
        }
        let mut outputs = Box::new(Outputs {
            landmarks: [NvArPoint2f::default(); LANDMARKS],
            confidence: [0.0; LANDMARKS],
            head_pose: NvArQuaternion::default(),
            gaze: [0.0; 2],
            head_translation: [0.0; 3],
            gaze_direction: [NvArPoint3f::default(); 2],
            rects: [NvArRect::default()],
            boxes: NvArBBoxes {
                boxes: std::ptr::null_mut(),
                num_boxes: 0,
                max_boxes: 1,
            },
        });
        // The box list is the boxed array.
        outputs.boxes.boxes = outputs.rects.as_mut_ptr();
        // From here on a failure destroys the feature (Drop).
        let mut eye = EyeContact {
            sdk,
            handle,
            config,
            outputs,
        };
        eye.configure(frames, width, height, stream)?;
        Ok(eye)
    }

    fn configure(
        &mut self,
        frames: &mut Frames,
        width: u32,
        height: u32,
        stream: CuStream,
    ) -> Result<(), String> {
        let sdk = self.sdk;
        let h = self.handle;
        let check = |what: &str, name: &CStr, rc: ffi::NvCvStatus| -> Result<(), String> {
            if rc == ffi::NVCV_SUCCESS {
                Ok(())
            } else {
                Err(sdk.explain(&format!("{what}({})", name.to_string_lossy()), rc))
            }
        };
        let set_u32 = |name: &CStr, v: u32| -> Result<(), String> {
            // SAFETY: a live handle and a NUL-terminated parameter name.
            check("NvAR_SetU32", name, unsafe {
                (sdk.NvAR_SetU32)(h, name.as_ptr(), v)
            })
        };
        let model_dir = CString::new(sdk.model_dir.as_os_str().as_encoded_bytes())
            .map_err(|_| "the model directory has a NUL in its path".to_string())?;
        // SAFETY (the calls below): a live handle, NUL-terminated names, and
        // pointers to buffers that outlive the feature (the boxed outputs,
        // the images of `frames`, which the caller keeps for as long as the
        // feature lives). The stream is the host's.
        check("NvAR_SetCudaStream", ffi::P_CUDA_STREAM, unsafe {
            (sdk.NvAR_SetCudaStream)(h, ffi::P_CUDA_STREAM.as_ptr(), stream)
        })?;
        check("NvAR_SetString", ffi::P_MODEL_DIR, unsafe {
            (sdk.NvAR_SetString)(h, ffi::P_MODEL_DIR.as_ptr(), model_dir.as_ptr())
        })?;
        set_u32(ffi::P_LANDMARKS_SIZE, LANDMARKS as u32)?;
        set_u32(ffi::P_GAZE_REDIRECT, 1)?;
        set_u32(ffi::P_TEMPORAL, 1)?;
        set_u32(ffi::P_ENABLE_LOOK_AWAY, u32::from(self.config.look_away))?;
        // No CUDA graphs (the SDK's default is on). The feature captures
        // them on its first frames with a face, in the global capture mode:
        // while a capture runs, an unsafe call from any other thread of the
        // process (a `cuMemAlloc`, as a Video Effects load or a buffer pool
        // makes) fails with "operation not permitted when stream is
        // capturing", the capture is invalidated, `NvAR_Run` fails and leaves
        // the stream capturing, and every unsafe call after it fails too; a
        // TensorRT engine load meeting it throws through `NvVFX_Load` and
        // aborts the daemon. The other stages run on other threads, so the
        // capture cannot be kept to itself.
        set_u32(ffi::P_USE_CUDA_GRAPH, 0)?;
        check("NvAR_Load", ffi::FEATURE_GAZE_REDIRECTION, unsafe {
            (sdk.NvAR_Load)(h)
        })?;
        for (name, v) in [(ffi::P_IN_WIDTH, width), (ffi::P_IN_HEIGHT, height)] {
            let v = i32::try_from(v).map_err(|_| format!("a frame size of {v} is too large"))?;
            // SAFETY: as above.
            let mut rc = unsafe { (sdk.NvAR_SetS32)(h, name.as_ptr(), v) };
            if rc == ffi::NVCV_ERR_SELECTOR {
                // The documentation gives no type: an unsigned one, then.
                // SAFETY: as above.
                rc = unsafe { (sdk.NvAR_SetU32)(h, name.as_ptr(), v as u32) };
            }
            check("NvAR_SetS32", name, rc)?;
        }
        let image_size = std::mem::size_of::<ffi::NvCvImage>() as c_ulong;
        let set_object = |name: &CStr, ptr: *mut c_void, size: usize| -> Result<(), String> {
            // SAFETY: as above.
            check("NvAR_SetObject", name, unsafe {
                (sdk.NvAR_SetObject)(h, name.as_ptr(), ptr, size as c_ulong)
            })
        };
        let set_f32s = |name: &CStr, ptr: *mut f32, n: usize| -> Result<(), String> {
            // SAFETY: as above.
            check("NvAR_SetF32Array", name, unsafe {
                (sdk.NvAR_SetF32Array)(h, name.as_ptr(), ptr, n as i32)
            })
        };
        // SAFETY: as above.
        check("NvAR_SetObject", ffi::P_IN_IMAGE, unsafe {
            (sdk.NvAR_SetObject)(
                h,
                ffi::P_IN_IMAGE.as_ptr(),
                frames.input.as_mut_ptr().cast(),
                image_size,
            )
        })?;
        // SAFETY: as above.
        check("NvAR_SetObject", ffi::P_OUT_IMAGE, unsafe {
            (sdk.NvAR_SetObject)(
                h,
                ffi::P_OUT_IMAGE.as_ptr(),
                frames.output.as_mut_ptr().cast(),
                image_size,
            )
        })?;
        let o = &mut *self.outputs;
        set_object(
            ffi::P_OUT_LANDMARKS,
            o.landmarks.as_mut_ptr().cast(),
            std::mem::size_of::<NvArPoint2f>(),
        )?;
        set_f32s(
            ffi::P_OUT_LANDMARKS_CONFIDENCE,
            o.confidence.as_mut_ptr(),
            LANDMARKS,
        )?;
        set_object(
            ffi::P_OUT_HEAD_POSE,
            (&mut o.head_pose as *mut NvArQuaternion).cast(),
            std::mem::size_of::<NvArQuaternion>(),
        )?;
        set_f32s(ffi::P_OUT_GAZE_VECTOR, o.gaze.as_mut_ptr(), 2)?;
        set_f32s(
            ffi::P_OUT_HEAD_TRANSLATION,
            o.head_translation.as_mut_ptr(),
            3,
        )?;
        set_object(
            ffi::P_OUT_GAZE_DIRECTION,
            o.gaze_direction.as_mut_ptr().cast(),
            std::mem::size_of::<NvArPoint3f>(),
        )?;
        set_object(
            ffi::P_OUT_BOUNDING_BOXES,
            (&mut o.boxes as *mut NvArBBoxes).cast(),
            std::mem::size_of::<NvArBBoxes>(),
        )?;
        Ok(())
    }

    /// The configuration of the load.
    pub(crate) fn config(&self) -> GazeConfig {
        self.config
    }

    /// Runs the feature on the bound input image, into the bound output
    /// image. Returns the face it tracked, if it reported one.
    pub(crate) fn run(&mut self) -> Result<Option<NvArRect>, String> {
        self.outputs.boxes.num_boxes = 0;
        // SAFETY: a loaded feature whose bound buffers are alive (see
        // `load`).
        let rc = unsafe { (self.sdk.NvAR_Run)(self.handle) };
        if rc != ffi::NVCV_SUCCESS {
            return Err(self.sdk.explain("NvAR_Run(GazeRedirection)", rc));
        }
        let o = &self.outputs;
        Ok((o.boxes.num_boxes > 0).then_some(o.rects[0]))
    }
}

impl Drop for EyeContact {
    fn drop(&mut self) {
        // SAFETY: the handle was created by NvAR_Create and is destroyed
        // once; the SDK releases what a partial configuration set up.
        unsafe { (self.sdk.NvAR_Destroy)(self.handle) };
    }
}
