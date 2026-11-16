//! Safe views of [`obc_frame`].

use std::ffi::c_void;

use obc_module_abi::{
    OBC_FORMAT_BGR, OBC_FORMAT_BGRA, OBC_FORMAT_I420, OBC_FORMAT_NV12, OBC_FORMAT_RGB_F32_PLANAR,
    OBC_FORMAT_RGBA, OBC_FRAME_CUDA, OBC_FRAME_DMABUF, OBC_FRAME_FLAG_NEW_SOURCE,
    OBC_FRAME_FLAG_REPEAT, OBC_FRAME_HOST, OBC_FRAME_SIZE_1_0, OBC_MAX_PLANES, obc_cuda,
    obc_format, obc_frame, obc_frame_kind, obc_plane,
};

use crate::Error;

/// Where a frame's pixels live.
///
/// A later release may name more codes, which until then are `Other`: a
/// `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameKind {
    /// CUDA device memory ([`OBC_FRAME_CUDA`]).
    Cuda,
    /// Host memory ([`OBC_FRAME_HOST`]).
    Host,
    /// A DMA-BUF ([`OBC_FRAME_DMABUF`], reserved).
    Dmabuf,
    /// A kind this SDK does not know, by its code.
    Other(u32),
}

impl FrameKind {
    /// The kind of a code.
    pub fn from_raw(code: obc_frame_kind) -> Self {
        match code {
            OBC_FRAME_CUDA => FrameKind::Cuda,
            OBC_FRAME_HOST => FrameKind::Host,
            OBC_FRAME_DMABUF => FrameKind::Dmabuf,
            other => FrameKind::Other(other),
        }
    }

    /// The code of the kind.
    pub fn to_raw(self) -> obc_frame_kind {
        match self {
            FrameKind::Cuda => OBC_FRAME_CUDA,
            FrameKind::Host => OBC_FRAME_HOST,
            FrameKind::Dmabuf => OBC_FRAME_DMABUF,
            FrameKind::Other(code) => code,
        }
    }

    /// The name manifests use (`cuda`, `host`, `dmabuf`); `None` for an
    /// unknown kind.
    pub fn name(self) -> Option<&'static str> {
        match self {
            FrameKind::Cuda => Some("cuda"),
            FrameKind::Host => Some("host"),
            FrameKind::Dmabuf => Some("dmabuf"),
            FrameKind::Other(_) => None,
        }
    }
}

/// The pixel format of a frame.
///
/// A later release may name more codes, which until then are `Other`: a
/// `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// NV12: a Y plane and an interleaved UV plane at half resolution.
    Nv12,
    /// I420: Y, U and V planes, U and V at half resolution.
    I420,
    /// BGRA, 8 bits per channel.
    Bgra,
    /// RGBA, 8 bits per channel.
    Rgba,
    /// Planar RGB, a 32-bit float per sample.
    RgbF32Planar,
    /// BGR, 8 bits per channel.
    Bgr,
    /// A format this SDK does not know, by its code.
    Other(u32),
}

impl PixelFormat {
    /// The format of a code.
    pub fn from_raw(code: obc_format) -> Self {
        match code {
            OBC_FORMAT_NV12 => PixelFormat::Nv12,
            OBC_FORMAT_I420 => PixelFormat::I420,
            OBC_FORMAT_BGRA => PixelFormat::Bgra,
            OBC_FORMAT_RGBA => PixelFormat::Rgba,
            OBC_FORMAT_RGB_F32_PLANAR => PixelFormat::RgbF32Planar,
            OBC_FORMAT_BGR => PixelFormat::Bgr,
            other => PixelFormat::Other(other),
        }
    }

    /// The code of the format.
    pub fn to_raw(self) -> obc_format {
        match self {
            PixelFormat::Nv12 => OBC_FORMAT_NV12,
            PixelFormat::I420 => OBC_FORMAT_I420,
            PixelFormat::Bgra => OBC_FORMAT_BGRA,
            PixelFormat::Rgba => OBC_FORMAT_RGBA,
            PixelFormat::RgbF32Planar => OBC_FORMAT_RGB_F32_PLANAR,
            PixelFormat::Bgr => OBC_FORMAT_BGR,
            PixelFormat::Other(code) => code,
        }
    }

    /// The name manifests use (`nv12`, `i420`, `bgra`, `rgba`,
    /// `rgb-f32-planar`, `bgr`); `None` for an unknown format.
    pub fn name(self) -> Option<&'static str> {
        match self {
            PixelFormat::Nv12 => Some("nv12"),
            PixelFormat::I420 => Some("i420"),
            PixelFormat::Bgra => Some("bgra"),
            PixelFormat::Rgba => Some("rgba"),
            PixelFormat::RgbF32Planar => Some("rgb-f32-planar"),
            PixelFormat::Bgr => Some("bgr"),
            PixelFormat::Other(_) => None,
        }
    }

    /// How many planes the format has; `None` for an unknown format.
    pub fn planes(self) -> Option<usize> {
        match self {
            PixelFormat::Nv12 => Some(2),
            PixelFormat::I420 | PixelFormat::RgbF32Planar => Some(3),
            PixelFormat::Bgra | PixelFormat::Rgba | PixelFormat::Bgr => Some(1),
            PixelFormat::Other(_) => None,
        }
    }

    /// The `(rows, bytes per row)` of plane `plane` of a `width` x `height`
    /// frame, without row padding; `None` for an unknown format or a plane
    /// the format does not have. Subsampled planes round up.
    pub fn plane_geometry(self, width: u32, height: u32, plane: usize) -> Option<(usize, usize)> {
        let (w, h) = (width as usize, height as usize);
        let (half_w, half_h) = (w.div_ceil(2), h.div_ceil(2));
        let geometry = match (self, plane) {
            (PixelFormat::Nv12, 0) | (PixelFormat::I420, 0) => (h, w),
            (PixelFormat::Nv12, 1) => (half_h, 2 * half_w),
            (PixelFormat::I420, 1 | 2) => (half_h, half_w),
            (PixelFormat::Bgra | PixelFormat::Rgba, 0) => (h, 4 * w),
            (PixelFormat::Bgr, 0) => (h, 3 * w),
            (PixelFormat::RgbF32Planar, 0..=2) => (h, 4 * w),
            _ => return None,
        };
        Some(geometry)
    }
}

/// The CUDA handles of a frame or a chain (`CUcontext`, `CUstream`); both
/// null for host memory.
///
/// It mirrors `obc_cuda`, which is frozen for the life of major 1
/// (the structure has no `struct_size` and never grows), so it is not
/// `#[non_exhaustive]`: a module may build one with a struct literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CudaHandles {
    /// The `CUcontext`, current on the calling thread during instance
    /// calls of a CUDA chain.
    pub context: *mut c_void,
    /// The `CUstream` to enqueue work on; null is the legacy default
    /// stream.
    pub stream: *mut c_void,
}

impl CudaHandles {
    pub(crate) fn from_raw(raw: obc_cuda) -> Self {
        CudaHandles {
            context: raw.context,
            stream: raw.stream,
        }
    }

    /// Whether there is no context.
    pub fn is_null(&self) -> bool {
        self.context.is_null()
    }
}

// The view reads the whole structure once `struct_size` reaches the base
// size: sound while no field has been appended. A field appended in a later
// minor version is read only when it fits in `struct_size`.
const _: () = assert!(size_of::<obc_frame>() == OBC_FRAME_SIZE_1_0 as usize);

/// A frame the daemon hands to [`crate::Instance::process`], or the
/// background image of a [`crate::ChainRequest`]: a checked view of an
/// [`obc_frame`], valid for the duration of the call.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    raw: &'a obc_frame,
}

impl<'a> Frame<'a> {
    /// A view of `raw`, checked: refused when `struct_size` is smaller than
    /// the frame of ABI 1.0 ([`OBC_FRAME_SIZE_1_0`]) or `n_planes` is more than
    /// [`OBC_MAX_PLANES`].
    pub fn new(raw: &'a obc_frame) -> Result<Self, Error> {
        if raw.struct_size < OBC_FRAME_SIZE_1_0 {
            return Err(Error::invalid(format!(
                "frame struct_size {} is smaller than {OBC_FRAME_SIZE_1_0}",
                raw.struct_size
            )));
        }
        if raw.n_planes as usize > OBC_MAX_PLANES {
            return Err(Error::invalid(format!(
                "frame has {} planes, more than {OBC_MAX_PLANES}",
                raw.n_planes
            )));
        }
        Ok(Frame { raw })
    }

    /// A view of the frame at `ptr`, checked as [`Frame::new`] does and
    /// refused when null.
    ///
    /// # Safety
    ///
    /// `ptr` is null or points to a readable frame of at least the
    /// `struct_size` it states, valid for `'a`.
    pub unsafe fn from_ptr(ptr: *const obc_frame) -> Result<Self, Error> {
        if ptr.is_null() {
            return Err(Error::invalid("frame is null"));
        }
        // SAFETY: non-null and readable; only the first field is read
        // before the size is known to cover the whole structure.
        let size = unsafe { (*ptr).struct_size } as usize;
        if size < OBC_FRAME_SIZE_1_0 as usize {
            return Err(Error::invalid(format!(
                "frame struct_size {size} is smaller than {OBC_FRAME_SIZE_1_0}"
            )));
        }
        // SAFETY: at least the base size, the size of `obc_frame` (asserted
        // above).
        Frame::new(unsafe { &*ptr })
    }

    /// The underlying structure.
    pub fn raw(&self) -> &'a obc_frame {
        self.raw
    }

    /// Where the pixels live.
    pub fn kind(&self) -> FrameKind {
        FrameKind::from_raw(self.raw.kind)
    }

    /// The pixel format.
    pub fn format(&self) -> PixelFormat {
        PixelFormat::from_raw(self.raw.format)
    }

    /// The width in pixels.
    pub fn width(&self) -> u32 {
        self.raw.width
    }

    /// The height in pixels.
    pub fn height(&self) -> u32 {
        self.raw.height
    }

    /// The planes that are set.
    pub fn planes(&self) -> &'a [obc_plane] {
        &self.raw.planes[..self.raw.n_planes as usize]
    }

    /// Plane `index`, when set.
    pub fn plane(&self, index: usize) -> Option<obc_plane> {
        self.planes().get(index).copied()
    }

    /// The CUDA handles (null for host frames).
    pub fn cuda(&self) -> CudaHandles {
        CudaHandles::from_raw(self.raw.cuda)
    }

    /// The DRM format modifier of a DMA-BUF frame.
    pub fn dmabuf_modifier(&self) -> u64 {
        self.raw.dmabuf_modifier
    }

    /// The presentation time in nanoseconds, when known.
    pub fn pts_ns(&self) -> Option<i64> {
        (self.raw.pts_ns >= 0).then_some(self.raw.pts_ns)
    }

    /// The `OBC_FRAME_FLAG_*` bits.
    pub fn flags(&self) -> u32 {
        self.raw.flags
    }

    /// Whether this is the first frame of a new source: temporal state
    /// should not carry over.
    pub fn is_new_source(&self) -> bool {
        self.raw.flags & OBC_FRAME_FLAG_NEW_SOURCE != 0
    }

    /// Whether this frame repeats the camera frame of the previous call
    /// ([`OBC_FRAME_FLAG_REPEAT`], ABI 1.1): the output clock had no new
    /// camera frame for this tick and emitted the last one again. A module
    /// that does not look at it processes the repeat as any other frame,
    /// which is what a module of ABI 1.0 does (the host never sets the flag
    /// for one).
    pub fn is_repeat(&self) -> bool {
        self.raw.flags & OBC_FRAME_FLAG_REPEAT != 0
    }

    /// Whether `other` describes the same memory (same kind, and planes
    /// that start at the same address, `ptr_or_fd + offset`, with the same
    /// pitch): an in-place call.
    pub fn same_memory(&self, other: &Frame<'_>) -> bool {
        let start = |p: &obc_plane| p.ptr_or_fd.wrapping_add(p.offset);
        self.raw.kind == other.raw.kind
            && self.planes().len() == other.planes().len()
            && self
                .planes()
                .iter()
                .zip(other.planes())
                .all(|(a, b)| start(a) == start(b) && a.pitch == b.pitch)
    }

    /// Whether host frames `self` and `other` share any byte: a plane of
    /// one overlaps a plane of the other. Frames that are
    /// [`Frame::same_memory`] overlap; frames that overlap without being
    /// the same memory cannot be read and written as two frames (a copy
    /// from one into the other is undefined). `false` for frames that are
    /// not host frames, whose memory cannot be compared here.
    pub fn overlaps(&self, other: &Frame<'_>) -> bool {
        if self.kind() != FrameKind::Host || other.kind() != FrameKind::Host {
            return false;
        }
        let spans = |f: &Frame<'_>| -> Vec<(usize, usize)> {
            (0..f.planes().len())
                .filter_map(|i| f.host_span(i).ok())
                .map(|(p, len)| (p as usize, len))
                .filter(|&(_, len)| len > 0)
                .collect()
        };
        let theirs = spans(other);
        spans(self).iter().any(|&(a, alen)| {
            theirs
                .iter()
                .any(|&(b, blen)| a < b.saturating_add(blen) && b < a.saturating_add(alen))
        })
    }

    /// The `(rows, bytes per row)` of plane `index` for this frame's format
    /// and size, without padding ([`PixelFormat::plane_geometry`]).
    pub fn plane_geometry(&self, index: usize) -> Option<(usize, usize)> {
        self.format()
            .plane_geometry(self.raw.width, self.raw.height, index)
    }

    /// The address and length of host plane `index`: from its first byte
    /// to the end of its last row's pixels, `pitch * (rows - 1) + bytes per
    /// row`.
    fn host_span(&self, index: usize) -> Result<(*mut u8, usize), Error> {
        if self.kind() != FrameKind::Host {
            return Err(Error::unsupported(format!(
                "plane bytes need a host frame, not {:?}",
                self.kind()
            )));
        }
        let plane = self
            .plane(index)
            .ok_or_else(|| Error::invalid(format!("the frame has no plane {index}")))?;
        let (rows, row_bytes) = self.plane_geometry(index).ok_or_else(|| {
            Error::unsupported(format!("no plane {index} in format {:?}", self.format()))
        })?;
        let pitch = usize::try_from(plane.pitch)
            .map_err(|_| Error::invalid("the plane pitch does not fit in memory"))?;
        if pitch < row_bytes {
            return Err(Error::invalid(format!(
                "plane {index} pitch {pitch} is smaller than its {row_bytes} bytes per row"
            )));
        }
        let address = plane
            .ptr_or_fd
            .checked_add(plane.offset)
            .and_then(|a| usize::try_from(a).ok())
            .ok_or_else(|| Error::invalid(format!("plane {index} address overflows")))?;
        if address == 0 {
            return Err(Error::invalid(format!("plane {index} is null")));
        }
        let len = match rows {
            0 => 0,
            rows => pitch
                .checked_mul(rows - 1)
                .and_then(|n| n.checked_add(row_bytes))
                .ok_or_else(|| Error::invalid(format!("plane {index} size overflows")))?,
        };
        Ok((address as *mut u8, len))
    }

    /// The bytes of host plane `index`, rows `pitch` bytes apart (iterate
    /// them with `chunks(pitch)`; the last row is not padded).
    ///
    /// Refused for a frame that is not [`FrameKind::Host`], a plane that is
    /// not set or does not exist in the format, or a pitch smaller than a
    /// row.
    ///
    /// # Safety
    ///
    /// The frame must describe host memory that is readable for its whole
    /// extent during `'a`, and nothing may write it meanwhile (in an
    /// in-place call, do not hold this together with
    /// [`Frame::host_plane_mut`] of the other frame).
    pub unsafe fn host_plane(&self, index: usize) -> Result<&'a [u8], Error> {
        let (ptr, len) = self.host_span(index)?;
        // SAFETY: the caller guarantees the memory; the span is within
        // the plane the frame describes.
        Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
    }

    /// The bytes of host plane `index`, writable; as
    /// [`Frame::host_plane`].
    ///
    /// # Safety
    ///
    /// As [`Frame::host_plane`], and the memory must be writable and not
    /// accessed through any other reference during `'a` (call it at most
    /// once per plane, and not on the input of an in-place call while
    /// using the output's).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn host_plane_mut(&self, index: usize) -> Result<&'a mut [u8], Error> {
        let (ptr, len) = self.host_span(index)?;
        // SAFETY: the caller guarantees exclusive, writable memory.
        Ok(unsafe { std::slice::from_raw_parts_mut(ptr, len) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12_frame(buf: &mut [u8], width: u32, height: u32, pitch: u64) -> obc_frame {
        let mut f = obc_frame::empty();
        f.kind = OBC_FRAME_HOST;
        f.format = OBC_FORMAT_NV12;
        f.width = width;
        f.height = height;
        f.n_planes = 2;
        let base = buf.as_mut_ptr() as u64;
        f.planes[0] = obc_plane {
            ptr_or_fd: base,
            pitch,
            offset: 0,
        };
        f.planes[1] = obc_plane {
            ptr_or_fd: base,
            pitch,
            offset: pitch * height as u64,
        };
        f
    }

    #[test]
    fn codes_round_trip() {
        for code in 0..8 {
            assert_eq!(FrameKind::from_raw(code).to_raw(), code);
            assert_eq!(PixelFormat::from_raw(code).to_raw(), code);
        }
        assert_eq!(PixelFormat::RgbF32Planar.name(), Some("rgb-f32-planar"));
        assert_eq!(FrameKind::Host.name(), Some("host"));
        assert_eq!(FrameKind::Other(9).name(), None);
    }

    #[test]
    fn plane_geometry_of_formats() {
        let nv12 = PixelFormat::Nv12;
        assert_eq!(nv12.plane_geometry(5, 3, 0), Some((3, 5)));
        assert_eq!(nv12.plane_geometry(5, 3, 1), Some((2, 6)));
        assert_eq!(nv12.plane_geometry(5, 3, 2), None);
        assert_eq!(PixelFormat::I420.plane_geometry(4, 4, 2), Some((2, 2)));
        assert_eq!(PixelFormat::Bgr.plane_geometry(4, 2, 0), Some((2, 12)));
        assert_eq!(
            PixelFormat::RgbF32Planar.plane_geometry(4, 2, 2),
            Some((2, 16))
        );
        assert_eq!(PixelFormat::Other(42).plane_geometry(4, 2, 0), None);
    }

    #[test]
    fn host_planes_span_their_rows() {
        let mut buf = vec![0u8; 8 * 6];
        let raw = nv12_frame(&mut buf, 6, 4, 8);
        let frame = Frame::new(&raw).unwrap();
        // SAFETY: `buf` holds both planes and is not otherwise used.
        let y = unsafe { frame.host_plane(0) }.unwrap();
        assert_eq!(y.len(), 8 * 3 + 6);
        // SAFETY: as above.
        let uv = unsafe { frame.host_plane_mut(1) }.unwrap();
        assert_eq!(uv.len(), 8 + 6);
        uv[0] = 7;
        assert_eq!(buf[8 * 4], 7);
    }

    #[test]
    fn bad_frames_are_refused() {
        let mut buf = vec![0u8; 64];
        let mut raw = nv12_frame(&mut buf, 6, 4, 4);
        let frame = Frame::new(&raw).unwrap();
        // SAFETY: refused before any access.
        let e = unsafe { frame.host_plane(0) }.unwrap_err();
        assert!(e.message().contains("pitch"), "{e}");
        // SAFETY: as above.
        assert!(unsafe { frame.host_plane(2) }.is_err());

        raw.kind = OBC_FRAME_CUDA;
        // SAFETY: as above.
        assert_eq!(
            unsafe { Frame::new(&raw).unwrap().host_plane(0) }
                .unwrap_err()
                .kind(),
            crate::ErrorKind::Unsupported
        );

        raw.n_planes = 5;
        assert!(Frame::new(&raw).is_err());
        raw.n_planes = 2;
        raw.struct_size = 16;
        assert!(
            Frame::new(&raw)
                .unwrap_err()
                .message()
                .contains("struct_size")
        );
        // SAFETY: a null pointer is refused before any read.
        assert!(unsafe { Frame::from_ptr(std::ptr::null()) }.is_err());
    }

    #[test]
    fn same_memory_and_flags() {
        let mut buf = vec![0u8; 64];
        let mut a = nv12_frame(&mut buf, 4, 2, 4);
        let b = a;
        assert!(
            Frame::new(&a)
                .unwrap()
                .same_memory(&Frame::new(&b).unwrap())
        );
        a.planes[1].offset += 1;
        assert!(
            !Frame::new(&a)
                .unwrap()
                .same_memory(&Frame::new(&b).unwrap())
        );
        // The same address, spelt with another offset, is the same memory.
        a.planes[1].ptr_or_fd -= 1;
        assert!(
            Frame::new(&a)
                .unwrap()
                .same_memory(&Frame::new(&b).unwrap())
        );
        // Another pitch is not.
        let mut c = b;
        c.planes[0].pitch = 8;
        c.height = 1;
        assert!(
            !Frame::new(&c)
                .unwrap()
                .same_memory(&Frame::new(&b).unwrap())
        );
        // It overlaps, though: a copy from one into the other is refused.
        assert!(Frame::new(&c).unwrap().overlaps(&Frame::new(&b).unwrap()));
        assert!(Frame::new(&b).unwrap().overlaps(&Frame::new(&b).unwrap()));
        let mut other = vec![0u8; 64];
        let d = nv12_frame(&mut other, 4, 2, 4);
        assert!(!Frame::new(&d).unwrap().overlaps(&Frame::new(&b).unwrap()));
        a.flags = OBC_FRAME_FLAG_NEW_SOURCE;
        assert!(Frame::new(&a).unwrap().is_new_source());
        assert!(!Frame::new(&a).unwrap().is_repeat());
        a.flags = OBC_FRAME_FLAG_REPEAT;
        assert!(Frame::new(&a).unwrap().is_repeat());
        assert!(!Frame::new(&a).unwrap().is_new_source());
        assert_eq!(Frame::new(&a).unwrap().pts_ns(), None);
        a.pts_ns = 5;
        assert_eq!(Frame::new(&a).unwrap().pts_ns(), Some(5));
    }
}
