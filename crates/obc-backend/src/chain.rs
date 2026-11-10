//! The generic seam of a chain of stages: frames in CUDA or in host memory,
//! stages named by [`StageId`], parameters as [`Params`].
//!
//! [`InferenceBackend`](crate::InferenceBackend) is the seam of the effect
//! element: one effect ([`EffectKind`](crate::EffectKind)) with its stages,
//! on CUDA frames only. A [`StageChain`] is the seam of any other chain of
//! stages, which a pipeline element runs between two others: it knows no
//! effect, reads and writes one frame in place, and declares the memory it
//! takes frames in ([`FrameKind`]), so that a chain that does not run on
//! CUDA can be offered host frames ([`HostNv12`]).
//!
//! * a [`StageChainFactory`] says which frame kinds its chains take and
//!   creates them;
//! * a [`StageChain`] loads one ordered list of stages for one frame
//!   geometry ([`StageChain::load`], [`ChainRequest`]), processes one frame
//!   at a time in place ([`StageChain::process`], [`ChainFrame`]) with the
//!   stages' live parameters, forgets its temporal state on a new source
//!   ([`StageChain::reset_temporal`]) and releases everything
//!   ([`StageChain::unload`]).
//!
//! The rules of the crate documentation apply: a load may block and runs on
//! a worker thread; calls on one chain are serialised by `&mut self`; for
//! [`FrameKind::Cuda`] the caller makes the request's CUDA context current
//! around every call, and the chain never creates or makes current a context
//! of its own. A stage alone failing is [`BackendError::Stage`], with the
//! meaning it has for [`InferenceBackend`](crate::InferenceBackend).

use std::collections::BTreeMap;

use crate::{
    BackendError, ColourSpec, DeviceNv12, GpuContext, LoadReport, Params, StageId, StageList,
};

/// The memory a chain takes frames in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FrameKind {
    /// CUDA device memory ([`DeviceNv12`]), in the caller's context and
    /// stream ([`GpuContext`]).
    Cuda,
    /// Host memory ([`HostNv12`]).
    Host,
}

impl FrameKind {
    /// The lower-case name: `cuda` or `host`.
    pub fn as_str(self) -> &'static str {
        match self {
            FrameKind::Cuda => "cuda",
            FrameKind::Host => "host",
        }
    }
}

impl std::fmt::Display for FrameKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A canonical NV12 frame in host memory, as [`DeviceNv12`] describes one
/// in device memory: the Y plane is `height` rows of at least `width`
/// bytes, `y_pitch` bytes apart; the interleaved UV plane is `height / 2`
/// rows of at least `width` bytes, `uv_pitch` bytes apart. `width` and
/// `height` are even.
///
/// A `HostNv12` borrows memory; it owns nothing. Whoever makes one
/// guarantees that both planes are valid for reads and writes of their
/// whole extent, and not accessed through anything else, for as long as a
/// call it is passed to runs ([`HostNv12::planes_mut`] relies on it).
#[derive(Debug, PartialEq, Eq)]
pub struct HostNv12 {
    /// Address of pixel (0, 0) of the Y plane.
    pub y: *mut u8,
    /// Bytes between the starts of two Y rows.
    pub y_pitch: usize,
    /// Address of the first U,V pair of the UV plane.
    pub uv: *mut u8,
    /// Bytes between the starts of two UV rows.
    pub uv_pitch: usize,
    /// Width in pixels (even).
    pub width: u32,
    /// Height in pixels (even).
    pub height: u32,
}

impl HostNv12 {
    /// Checks the description is self-consistent: non-null planes, even
    /// non-zero size, pitches of at least `width` bytes.
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.y.is_null() || self.uv.is_null() {
            return Err(BackendError::InvalidInput(
                "host NV12 frame has a null plane pointer".into(),
            ));
        }
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(BackendError::InvalidInput(format!(
                "host NV12 frame size {}x{} must be even and non-zero",
                self.width, self.height
            )));
        }
        let w = self.width as usize;
        if self.y_pitch < w || self.uv_pitch < w {
            return Err(BackendError::InvalidInput(format!(
                "host NV12 pitches (Y {}, UV {}) are smaller than the width {}",
                self.y_pitch, self.uv_pitch, self.width
            )));
        }
        Ok(())
    }

    /// The bytes of the Y plane and of the UV plane, each from its first
    /// row to the end of its last row (`pitch * (rows - 1) + width`), after
    /// [`HostNv12::validate`].
    ///
    /// # Safety
    /// The planes must be what the type documentation says: valid for reads
    /// and writes of that extent, and not accessed through anything else
    /// while the slices live.
    pub unsafe fn planes_mut(&mut self) -> Result<(&mut [u8], &mut [u8]), BackendError> {
        self.validate()?;
        let (w, h) = (self.width as usize, self.height as usize);
        let y_len = self.y_pitch * (h - 1) + w;
        let uv_len = self.uv_pitch * (h / 2 - 1) + w;
        // SAFETY: the caller's contract above; `validate` checked the
        // pointers are non-null and the extents are those of the frame.
        unsafe {
            Ok((
                std::slice::from_raw_parts_mut(self.y, y_len),
                std::slice::from_raw_parts_mut(self.uv, uv_len),
            ))
        }
    }
}

/// What [`StageChain::load`] should prepare.
#[derive(Debug, Clone)]
pub struct ChainRequest {
    /// The stages, in the order they run. Never empty.
    pub stages: StageList,
    /// The load-time parameters of the stages, by stage, as
    /// [`LoadRequest::params`](crate::LoadRequest::params); an entry for a
    /// stage that is not listed is refused.
    pub params: BTreeMap<StageId, Params>,
    /// Frame width in pixels (even); every frame passed to `process` has it.
    pub width: u32,
    /// Frame height in pixels (even); every frame passed to `process` has it.
    pub height: u32,
    /// The colour matrix and range of the frames.
    pub colour: ColourSpec,
    /// The memory the frames are in; one of the factory's
    /// [`StageChainFactory::kinds`].
    pub kind: FrameKind,
    /// The CUDA context and stream of the frames, for [`FrameKind::Cuda`]
    /// (the chain binds its work to the stream at load); `None` for
    /// [`FrameKind::Host`].
    pub gpu: Option<GpuContext>,
}

impl ChainRequest {
    /// Checks the request is self-consistent: at least one stage, an even
    /// non-zero size, parameters only for listed stages, and a CUDA context
    /// exactly for CUDA frames.
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.stages.is_empty() {
            return Err(BackendError::InvalidInput(
                "a stage chain needs at least one stage".into(),
            ));
        }
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(BackendError::InvalidInput(format!(
                "frame size {}x{} must be even and non-zero",
                self.width, self.height
            )));
        }
        if let Some(stage) = self
            .params
            .keys()
            .find(|s| !self.stages.contains(s.as_str()))
        {
            return Err(BackendError::InvalidInput(format!(
                "parameters for the stage {stage}, which is not in the chain {}",
                self.stages
            )));
        }
        match (self.kind, &self.gpu) {
            (FrameKind::Cuda, Some(g)) if !g.context.is_null() => Ok(()),
            (FrameKind::Cuda, _) => Err(BackendError::InvalidInput(
                "a chain on CUDA frames needs the frames' CUDA context".into(),
            )),
            (FrameKind::Host, None) => Ok(()),
            (FrameKind::Host, Some(_)) => Err(BackendError::InvalidInput(
                "a chain on host frames takes no CUDA context".into(),
            )),
        }
    }
}

/// One frame for [`StageChain::process`], processed in place.
#[derive(Debug)]
pub enum ChainFrame<'a> {
    /// A frame in CUDA memory, in `gpu`'s context; the work is enqueued on
    /// `gpu.stream` (the stream of the load).
    Cuda {
        /// The context and stream of the load.
        gpu: &'a GpuContext,
        /// The frame, read and written in place.
        frame: &'a DeviceNv12,
    },
    /// A frame in host memory, read and written in place before the call
    /// returns.
    Host(&'a mut HostNv12),
}

impl ChainFrame<'_> {
    /// The memory the frame is in.
    pub fn kind(&self) -> FrameKind {
        match self {
            ChainFrame::Cuda { .. } => FrameKind::Cuda,
            ChainFrame::Host(_) => FrameKind::Host,
        }
    }

    /// The frame's size.
    pub fn size(&self) -> (u32, u32) {
        match self {
            ChainFrame::Cuda { frame, .. } => (frame.width, frame.height),
            ChainFrame::Host(f) => (f.width, f.height),
        }
    }
}

/// One instance of a chain of stages: the models and buffers for one list
/// of stages at one frame geometry. See the module documentation for the
/// threading and context rules.
pub trait StageChain: Send {
    /// Loads the stages of `req`. May block for seconds. Loading again on a
    /// loaded chain is a switch: the chain may keep what both requests
    /// share ([`LoadReport::reused_model`]). If it fails, the previous chain
    /// stays loaded, less the stage of a [`BackendError::Stage`], or, after
    /// a failed full load, nothing is loaded.
    fn load(&mut self, req: &ChainRequest) -> Result<LoadReport, BackendError>;

    /// Runs the loaded stages on `frame`, in place, with the stages' live
    /// parameters (`live`, by stage; values outside their range are
    /// clamped, keys the chain does not know are ignored). The frame has the
    /// loaded size and kind.
    fn process(
        &mut self,
        frame: ChainFrame<'_>,
        live: &BTreeMap<StageId, Params>,
    ) -> Result<(), BackendError>;

    /// Forgets the temporal state of the loaded stages, so the next frame is
    /// treated as the first of a new source, and returns the stages that had
    /// to be dropped instead, as
    /// [`InferenceBackend::reset_temporal`](crate::InferenceBackend::reset_temporal)
    /// does. A no-op when nothing is loaded.
    fn reset_temporal(&mut self) -> Result<StageList, BackendError>;

    /// Releases models, buffers and device resources. Idempotent.
    fn unload(&mut self);
}

/// Creates [`StageChain`]s and says what frames they take.
pub trait StageChainFactory: Send + Sync {
    /// A short name for logs.
    fn name(&self) -> &str;

    /// The frame kinds the chains take, in order of preference.
    fn kinds(&self) -> &[FrameKind];

    /// Creates an unloaded chain. Cheap: loads no model.
    fn create(&self) -> Result<Box<dyn StageChain>, BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ChainRequest {
        ChainRequest {
            stages: StageList::new([StageId::new("hello").unwrap()]).unwrap(),
            params: BTreeMap::new(),
            width: 1280,
            height: 720,
            colour: ColourSpec::default(),
            kind: FrameKind::Host,
            gpu: None,
        }
    }

    #[test]
    fn chain_requests_are_validated() {
        assert!(request().validate().is_ok());
        let e = ChainRequest {
            stages: StageList::default(),
            ..request()
        }
        .validate()
        .unwrap_err();
        assert!(e.to_string().contains("at least one stage"), "{e}");
        assert!(
            ChainRequest {
                width: 1279,
                ..request()
            }
            .validate()
            .is_err()
        );
        let mut stray = request();
        stray.params.insert(StageId::DENOISE, Params::new());
        let e = stray.validate().unwrap_err();
        assert!(e.to_string().contains("denoise, which is not in"), "{e}");
        let mut own = request();
        own.params
            .insert(StageId::new("hello").unwrap(), Params::new());
        assert!(own.validate().is_ok());

        let gpu = GpuContext {
            context: 0x10 as *mut _,
            stream: std::ptr::null_mut(),
        };
        let cuda = ChainRequest {
            kind: FrameKind::Cuda,
            gpu: Some(gpu),
            ..request()
        };
        assert!(cuda.validate().is_ok());
        let e = ChainRequest {
            gpu: None,
            ..cuda.clone()
        }
        .validate()
        .unwrap_err();
        assert!(
            e.to_string().contains("needs the frames' CUDA context"),
            "{e}"
        );
        let null = GpuContext {
            context: std::ptr::null_mut(),
            ..gpu
        };
        assert!(
            ChainRequest {
                gpu: Some(null),
                ..cuda.clone()
            }
            .validate()
            .is_err()
        );
        let e = ChainRequest {
            kind: FrameKind::Host,
            ..cuda
        }
        .validate()
        .unwrap_err();
        assert!(e.to_string().contains("takes no CUDA context"), "{e}");
    }

    #[test]
    fn host_frames_are_validated_and_give_their_planes() {
        let (w, h, pitch) = (4u32, 2u32, 8usize);
        let mut y = vec![0u8; pitch * h as usize];
        let mut uv = vec![0u8; pitch * (h as usize / 2)];
        let mut f = HostNv12 {
            y: y.as_mut_ptr(),
            y_pitch: pitch,
            uv: uv.as_mut_ptr(),
            uv_pitch: pitch,
            width: w,
            height: h,
        };
        assert!(f.validate().is_ok());
        // SAFETY: the planes are the vectors above, alive and unaliased.
        let (yp, uvp) = unsafe { f.planes_mut() }.unwrap();
        assert_eq!((yp.len(), uvp.len()), (pitch + 4, 4));
        yp[0] = 7;
        uvp[3] = 9;
        assert_eq!((y[0], uv[3]), (7, 9));

        let (y_ptr, uv_ptr) = (y.as_mut_ptr(), uv.as_mut_ptr());
        let bad = |f: HostNv12| f.validate().is_err();
        let base = || HostNv12 {
            y: y_ptr,
            y_pitch: pitch,
            uv: uv_ptr,
            uv_pitch: pitch,
            width: w,
            height: h,
        };
        assert!(bad(HostNv12 {
            y: std::ptr::null_mut(),
            ..base()
        }));
        assert!(bad(HostNv12 { width: 3, ..base() }));
        assert!(bad(HostNv12 {
            height: 0,
            ..base()
        }));
        assert!(bad(HostNv12 {
            uv_pitch: 3,
            ..base()
        }));
        let mut f = base();
        let frame = ChainFrame::Host(&mut f);
        assert_eq!((frame.kind(), frame.size()), (FrameKind::Host, (4, 2)));
    }

    #[test]
    fn frame_kinds_have_names_and_the_traits_are_object_safe() {
        assert_eq!(FrameKind::Cuda.to_string(), "cuda");
        assert_eq!(FrameKind::Host.as_str(), "host");
        fn _takes(_: &dyn StageChain, _: &dyn StageChainFactory) {}
        fn _send<T: Send>() {}
        _send::<Box<dyn StageChain>>();
        _send::<ChainRequest>();
        fn _send_sync<T: Send + Sync>() {}
        _send_sync::<Box<dyn StageChainFactory>>();
    }
}
