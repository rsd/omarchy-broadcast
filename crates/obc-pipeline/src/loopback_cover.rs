//! The crash cover: what a consumer of the loopback receives while no
//! writer streams into it, after the daemon died (`kill -9`, a crash) and
//! until its restart attaches a new writer.
//!
//! Without a writer, v4l2loopback serves no frame: a reader blocked in
//! `VIDIOC_DQBUF` just waits, but Chromium-based applications restart their
//! capture after about a second without a frame, and v4l2loopback refuses
//! that `VIDIOC_STREAMON` (`EIO`) while no writer streams: the application
//! loses the camera. The driver's `timeout` control covers the gap: once a
//! writer has written a frame, the driver re-arms a timer at every frame
//! written, and when it fires (`timeout` milliseconds without a frame) it
//! raises one device-wide flag and wakes the readers, then again every
//! `timeout` milliseconds, until a writer writes again or the last opener
//! closes. The timer keeps re-arming itself after the writer's process is
//! gone, which is the point. The image the driver allocates is zero-filled
//! (green in NV12), so the writer uploads its own first: black, or the
//! cover image the writer is given
//! ([`crate::loopback::LoopbackWriter::set_cover_image`], the user's "away
//! image").
//!
//! One tick is one frame for one reader, not for each of them: the first
//! reader to dequeue after a tick clears the flag (the driver copies the
//! timeout image into the last written buffer and returns that buffer), and
//! a reader that comes later finds nothing new and waits for the next tick,
//! unless it was already past the wait in the same wake-up. So a single
//! reader gets a frame every `timeout` milliseconds; several readers share
//! the ticks between them, and one of them can go several ticks without a
//! frame, past Chromium's limit of about a second. The cover keeps one
//! consumer of the loopback alive across a crash; with several, it may not.
//!
//! [`install`], run by [`crate::loopback::LoopbackWriter`] once its
//! `v4l2sink` has set its format and allocated its buffers on the device
//! (the timeout image is sized to that format), at every writer start, and
//! again whenever the cover image changes after that (see
//! "Re-installs" below):
//!
//! 1. **The timeout image**, at the device's format as `VIDIOC_G_FMT`
//!    reports it ([`fill_cover`]). While the cover image is decoded
//!    ([`crate::loopback::LoopbackWriter::cover_image_state`] is
//!    [`crate::loopback::PlaceholderState::Image`]), that frame,
//!    copied byte for byte: it is a frame of the contract laid out as
//!    `GstVideoInfo` lays out the writer's caps, which is the device's
//!    layout as long as `bytesperline`, `height` and `sizeimage` say so
//!    ([`frame_fits`]); when they do not, black, and the outcome says why
//!    ([`CoverShows::BlackForImage`]). Otherwise black: the luma plane
//!    (`bytesperline` x `height` bytes) at the contract's black (Y = 16,
//!    limited range), the rest of `sizeimage` (NV12's interleaved chroma
//!    plane, I420's two) at 128. Never the last frame: a camera image left
//!    on screen by a dead daemon would show the user after they believe the
//!    camera is off. The cover image is no camera frame: it is the
//!    picture the user chose to be seen instead. The upload protocol of
//!    v4l2loopback 0.15.4: the `timeout_image_io` button control sets a
//!    device-wide flag, and every `open()` of the node while it is set
//!    (and nobody holds the timeout image) gives a descriptor in
//!    timeout-image mode. The flag is cleared only when one of those
//!    descriptors takes the image (`VIDIOC_REQBUFS` or `VIDIOC_S_FMT`). So
//!    the cover presses the button, opens its own descriptor and calls
//!    `VIDIOC_REQBUFS` at once (output, MMAP, one buffer; no `VIDIOC_S_FMT`,
//!    which with another format would reallocate the device's buffers),
//!    which takes the image and clears the flag; `VIDIOC_QUERYBUF` gives
//!    the image's offset (past every frame buffer, never 0), and `mmap` of
//!    that offset maps the image itself, written in place. `VIDIOC_QBUF` is
//!    a no-op in that mode, and `VIDIOC_REQBUFS` with no buffer (or the
//!    close) gives the image back.
//!
//!    The race of upstream's `v4l2loopback-ctl set-timeout-image`, made as
//!    short as the cover can make it (nothing runs between the button, the
//!    `open()` and the `VIDIOC_REQBUFS`) but not closed: a program that
//!    opens the node in that instant also gets a descriptor in timeout-image
//!    mode, which is no capture of the loopback (every dequeue returns the
//!    timeout image itself, never a frame written; it has to reopen the
//!    node), and if it takes the image first, the cover's `VIDIOC_REQBUFS`
//!    fails. And when
//!    the cover fails between the button and its `VIDIOC_REQBUFS` (its
//!    `open()` refused, the request refused or out of memory), the flag
//!    would stay set and catch the next program to open the node (a
//!    browser starting its capture, say): on every such failure the cover
//!    takes the flag back with one more descriptor
//!    ([`clear_timeout_image_io`]), and its warning says that a program
//!    that opened the node at that instant may have been affected.
//! 2. **The `timeout` control** ([`timeout_ms`]): at least
//!    [`MIN_TIMEOUT`] (250 ms, the value with which a crash was measured to
//!    keep a Chromium capture, the only reader, alive; Chromium gives up
//!    after about 1 s without a frame), and above the writer's worst gap
//!    between two frames while it is alive by [`TIMEOUT_MARGIN`], so the
//!    timer never fires while the writer runs. Capped at the control's maximum (100000 ms in
//!    v4l2loopback 0.15.4); a driver whose maximum is not positive cannot
//!    have a timeout (0 turns it off) and fails the cover.
//!
//! **Re-installs.** When the frame of the cover image changes after the
//! start's install was handed to its helper (the image becomes ready,
//! another one replaces it once decoded, black is asked for, or the image
//! cannot be read), the writer hands the cover the new frame
//! ([`CoverSlot::set_image`]) and the whole of
//! [`install`] runs again on a helper thread of its own: the button is
//! pressed again and the race window is the same as at the start (the
//! writer is streaming in both cases; nothing runs between the button, the
//! `open()` and the `VIDIOC_REQBUFS`). In v4l2loopback 0.15.4 the timeout
//! image allocated by the first install is reused (`VIDIOC_REQBUFS` in
//! timeout-image mode allocates one only when there is none), the cover's
//! descriptor gives its timeout token back on `VIDIOC_REQBUFS` with no
//! buffer or on close, and the timer never copies the image while the
//! writer streams (the `timeout` stays above its worst gap), so the image
//! is rewritten in place with no reader seeing a half-written one.
//! Installs never overlap: they run one at a time under one lock (a second
//! button press while the first's descriptor holds the timeout token would
//! open a descriptor that is not in timeout-image mode and leave the mode
//! pending), and each reads the frame to show when it runs, so the last
//! one shows the last change. Each outcome is reported like the start's (a
//! failed re-install says so, the previous image staying in place). Only
//! the cover image counts: the writer's placeholder switching (black, its
//! own image, a camera turned on or off) installs nothing. A stop disarms
//! the cover while the device is still open, so nothing is started once it
//! closes (not even by a decode ending during the stop), and joins the
//! installs running.
//!
//! Nothing is reset when the writer stops: the cover must outlive the
//! process. After a clean stop, a reader still holding the device gets the
//! timeout image (black, or the cover image) every `timeout`
//! milliseconds instead of a frozen last frame.
//! The controls are device-wide and reset only by a module reload;
//! `keep_format` and `sustain_framerate` are never touched.
//!
//! A driver without these controls (v4l2loopback before the timeout image
//! was added) is a [`ConsumerCover::Failed`] outcome, which the daemon logs
//! as a warning: the writer runs as before. The cover's descriptors are the
//! daemon's own, open for a few milliseconds; the daemon's consumer count
//! excludes its own process. The one that sets the controls is opened
//! read-only (control ioctls do not check the open mode), so its close
//! raises no `IN_CLOSE_WRITE`; the timeout image's has to be writable, for
//! its shared writable mapping.
//!
//! The image keeps its timestamp: a frame the driver serves on a timeout is
//! the last written buffer with the timeout image copied into it, so it
//! carries that buffer's timestamp (`V4L2_BUF_FLAG_TIMESTAMP_COPY`), equal to
//! the last frame the writer wrote.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use obc_core::OutputFormat;

use crate::controls::{self, CTRL_TYPE_INTEGER, CtrlInfo, VIDIOC_S_CTRL};

/// The shortest `timeout` the cover sets.
pub const MIN_TIMEOUT: Duration = Duration::from_millis(250);
/// How far above the writer's worst gap between two frames the `timeout`
/// stays.
pub const TIMEOUT_MARGIN: Duration = Duration::from_millis(50);

/// The v4l2loopback control names (as `v4l2-ctl -l` lists them).
pub const KEEP_FORMAT: &str = "keep_format";
pub const SUSTAIN_FRAMERATE: &str = "sustain_framerate";
pub const TIMEOUT: &str = "timeout";
pub const TIMEOUT_IMAGE_IO: &str = "timeout_image_io";

/// `V4L2_CTRL_TYPE_BUTTON`
pub const CTRL_TYPE_BUTTON: u32 = 4;

/// The outcome of the crash cover for one writer start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumerCover {
    /// None for this writer: not started, or its sink is no loopback.
    Off,
    /// The writer started; the cover goes in once its first frame reached
    /// the device.
    Pending,
    /// The timeout image is uploaded (black, or the cover image:
    /// [`crate::loopback::LoopbackWriter::cover_shows`]) and the driver's
    /// `timeout` set.
    Ready { timeout_ms: u32 },
    /// Not in place, and why (a driver without the controls included). The
    /// writer runs regardless.
    Failed(String),
}

/// Called with each writer start's cover outcome once it is known
/// ([`ConsumerCover::Ready`] or [`ConsumerCover::Failed`]), and with the
/// outcome of each re-install (see the module docs), on the cover's
/// helper thread: it must not block for long.
pub type CoverReport = Arc<dyn Fn(&ConsumerCover) + Send + Sync>;

/// The `timeout` (ms) the cover sets for a writer whose longest gap between
/// two frames while alive is `writer_worst_gap`: that plus
/// [`TIMEOUT_MARGIN`], at least [`MIN_TIMEOUT`], rounded up to a whole
/// millisecond, and at most `driver_max_ms` (the control's maximum).
/// `None` when that maximum is not positive: 0 would turn the timeout off.
pub fn timeout_ms(writer_worst_gap: Duration, driver_max_ms: i32) -> Option<u32> {
    let max = u32::try_from(driver_max_ms).ok().filter(|&m| m > 0)?;
    let want = writer_worst_gap
        .saturating_add(TIMEOUT_MARGIN)
        .max(MIN_TIMEOUT);
    let ms = u32::try_from(want.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX);
    Some(ms.min(max))
}

/// One v4l2loopback control: its id and what `VIDIOC_QUERYCTRL` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackControl {
    pub id: u32,
    pub info: CtrlInfo,
}

/// The v4l2loopback controls of a node, found by name; `None`: the driver
/// does not have it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopbackControls {
    pub keep_format: Option<LoopbackControl>,
    pub sustain_framerate: Option<LoopbackControl>,
    pub timeout: Option<LoopbackControl>,
    pub timeout_image_io: Option<LoopbackControl>,
}

/// Enumeration stops after this many controls, whatever the driver says.
const MAX_CONTROLS: usize = 1024;

impl LoopbackControls {
    /// The controls of the node at `path` (opened read-only, briefly).
    pub fn query(path: &Path) -> io::Result<LoopbackControls> {
        Self::query_on(&controls::open_node(path, false)?)
    }

    fn query_on(file: &fs::File) -> io::Result<LoopbackControls> {
        Self::collect(|after| controls::query_next_ctrl_on(file, after))
    }

    /// Walks the controls with `next` (`VIDIOC_QUERYCTRL` with
    /// `V4L2_CTRL_FLAG_NEXT_CTRL` after an id) and keeps the four by name.
    /// Stops at the end, at an id that does not increase, or after
    /// [`MAX_CONTROLS`].
    fn collect(
        mut next: impl FnMut(u32) -> io::Result<Option<(u32, CtrlInfo)>>,
    ) -> io::Result<LoopbackControls> {
        let mut out = LoopbackControls::default();
        let mut after = 0;
        for _ in 0..MAX_CONTROLS {
            let Some((id, info)) = next(after)? else {
                break;
            };
            if id <= after && after != 0 {
                break;
            }
            after = id;
            let slot = match info.name.as_str() {
                KEEP_FORMAT => &mut out.keep_format,
                SUSTAIN_FRAMERATE => &mut out.sustain_framerate,
                TIMEOUT => &mut out.timeout,
                TIMEOUT_IMAGE_IO => &mut out.timeout_image_io,
                _ => continue,
            };
            slot.get_or_insert(LoopbackControl { id, info });
        }
        Ok(out)
    }
}

/// `V4L2_BUF_TYPE_VIDEO_OUTPUT`
const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
/// `V4L2_MEMORY_MMAP`
const MEMORY_MMAP: u32 = 1;

/// The V4L2 fourcc of a contract format: `NV12`, or `YU12` for I420.
pub fn fourcc(format: OutputFormat) -> u32 {
    let code = match format {
        OutputFormat::Nv12 => *b"NV12",
        OutputFormat::I420 => *b"YU12",
    };
    u32::from_le_bytes(code)
}

fn fourcc_string(code: u32) -> String {
    code.to_le_bytes().iter().map(|&b| b as char).collect()
}

/// `struct v4l2_pix_format`
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// The `fmt` union of `struct v4l2_format`: 200 bytes, aligned as a
/// pointer (its `v4l2_window` member holds pointers).
#[repr(C)]
#[derive(Clone, Copy)]
union V4l2FormatUnion {
    pix: V4l2PixFormat,
    raw: [u8; 200],
    _align: [libc::c_ulong; 0],
}

/// `struct v4l2_format`
#[repr(C)]
struct V4l2Format {
    type_: u32,
    fmt: V4l2FormatUnion,
}

/// `struct v4l2_requestbuffers`
#[repr(C)]
struct V4l2RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct v4l2_timecode`
#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

/// The `m` union of `struct v4l2_buffer`.
#[repr(C)]
#[derive(Clone, Copy)]
union V4l2BufferM {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut libc::c_void,
    fd: i32,
}

/// `struct v4l2_buffer`
#[repr(C)]
struct V4l2Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: V4l2Timecode,
    sequence: u32,
    memory: u32,
    m: V4l2BufferM,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

/// `_IOWR('V', nr, size)`
const fn iowr(nr: libc::c_ulong, size: usize) -> libc::c_ulong {
    (3 << 30) | ((size as libc::c_ulong) << 16) | ((b'V' as libc::c_ulong) << 8) | nr
}

/// `VIDIOC_G_FMT`
const VIDIOC_G_FMT: libc::c_ulong = iowr(4, std::mem::size_of::<V4l2Format>());
/// `VIDIOC_REQBUFS`
const VIDIOC_REQBUFS: libc::c_ulong = iowr(8, std::mem::size_of::<V4l2RequestBuffers>());
/// `VIDIOC_QUERYBUF`
const VIDIOC_QUERYBUF: libc::c_ulong = iowr(9, std::mem::size_of::<V4l2Buffer>());
/// `VIDIOC_QBUF`
const VIDIOC_QBUF: libc::c_ulong = iowr(15, std::mem::size_of::<V4l2Buffer>());

/// Issues one `_IOWR` V4L2 ioctl on `file`.
///
/// # Safety
/// `arg` must be the struct type `request` expects.
unsafe fn ioctl_rw<T>(file: &fs::File, request: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: the caller pairs `request` with its struct; `arg` is a valid,
    // exclusively borrowed instance that outlives the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _, arg as *mut T) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// An all-zero V4L2 argument struct.
///
/// # Safety
/// `T` must be a plain C struct for which all-zero bytes are a valid value
/// (integers, arrays of them, unions of them, raw pointers).
unsafe fn zeroed<T>() -> T {
    // SAFETY: guaranteed by the caller.
    unsafe { std::mem::zeroed() }
}

/// What the black fill needs of the device's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageLayout {
    pub fourcc: u32,
    pub bytesperline: u32,
    pub height: u32,
    pub sizeimage: u32,
}

/// Writes a black frame of `layout` into `image` (the mapped timeout image):
/// the luma plane (`bytesperline` x `height` bytes, first in NV12 and I420
/// alike) at `y`, the rest of `sizeimage` (the chroma planes) at `chroma`.
/// Bytes past `sizeimage` (the page-aligned tail) are left as they are.
pub(crate) fn fill_black(
    image: &mut [u8],
    layout: &ImageLayout,
    (y, chroma): (u8, u8),
) -> Result<(), String> {
    let luma = layout.bytesperline as usize * layout.height as usize;
    let size = layout.sizeimage as usize;
    if luma == 0 || luma >= size {
        return Err(format!(
            "the device's format makes no sense for {}: {}x{} luma bytes in a {size}-byte image",
            fourcc_string(layout.fourcc),
            layout.bytesperline,
            layout.height
        ));
    }
    let Some(frame) = image.get_mut(..size) else {
        return Err(format!(
            "the timeout image ({} bytes) is smaller than a frame ({size} bytes)",
            image.len()
        ));
    };
    let (luma_plane, chroma_planes) = frame.split_at_mut(luma);
    luma_plane.fill(y);
    chroma_planes.fill(chroma);
    Ok(())
}

/// What the timeout image of an installed cover shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverShows {
    /// Black: no cover image asked for
    /// ([`crate::loopback::LoopbackWriter::set_cover_image`]), or not
    /// decoded yet.
    Black,
    /// The cover image.
    Image,
    /// Black in place of the cover image, and why: the image cannot be
    /// read (the reason as `gst_obc::fallback_unreadable` words it), or the
    /// device's format does not lay frames out as the image frame is.
    BlackForImage(String),
}

/// How one of the writer's frames is laid out in memory, as
/// `GstVideoInfo` lays out the writer's caps: plane strides and offsets,
/// and the frame's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameLayout {
    pub fourcc: u32,
    pub height: u32,
    pub planes: usize,
    pub strides: [usize; 3],
    pub offsets: [usize; 3],
    pub size: usize,
}

/// Whether frames laid out as `frame` are frames of the device's format
/// `device` byte for byte: the same fourcc and height, the luma stride
/// `bytesperline`, the chroma planes where single-planar V4L2 puts them
/// (NV12: one plane of `bytesperline`; YU12: two of `bytesperline / 2`,
/// each `ceil(height / 2)` rows) and the size `sizeimage`. The reason
/// names what differs.
pub(crate) fn frame_fits(device: &ImageLayout, frame: &FrameLayout) -> Result<(), String> {
    let bpl = device.bytesperline as usize;
    let luma = bpl * device.height as usize;
    let chroma_rows = (device.height as usize).div_ceil(2);
    let (planes, strides, offsets) = if device.fourcc == fourcc(OutputFormat::Nv12) {
        (2, [bpl, bpl, 0], [0, luma, 0])
    } else if device.fourcc == fourcc(OutputFormat::I420) {
        let c = bpl / 2;
        (3, [bpl, c, c], [0, luma, luma + c * chroma_rows])
    } else {
        return Err(format!(
            "the device carries {} frames",
            fourcc_string(device.fourcc)
        ));
    };
    let device_says = || {
        format!(
            "{} {}x{} lines, {} bytes",
            fourcc_string(device.fourcc),
            device.bytesperline,
            device.height,
            device.sizeimage
        )
    };
    if frame.fourcc != device.fourcc
        || frame.height != device.height
        || frame.planes != planes
        || frame.strides[..planes] != strides[..planes]
        || frame.offsets[..planes] != offsets[..planes]
        || frame.size != device.sizeimage as usize
    {
        return Err(format!(
            "the device's format ({}) does not lay frames out as the image frame is ({} {} \
             rows, strides {:?}, offsets {:?}, {} bytes)",
            device_says(),
            fourcc_string(frame.fourcc),
            frame.height,
            &frame.strides[..frame.planes.min(3)],
            &frame.offsets[..frame.planes.min(3)],
            frame.size
        ));
    }
    Ok(())
}

/// What the timeout image of a device of format `device` shows when the
/// cover image is `image` (its frame layout; `None`: black):
/// the image when its frame is a frame of the device's format
/// ([`frame_fits`]), black otherwise, saying why.
pub(crate) fn cover_choice(device: &ImageLayout, image: Option<&FrameLayout>) -> CoverShows {
    match image.map(|frame| frame_fits(device, frame)) {
        None => CoverShows::Black,
        Some(Ok(())) => CoverShows::Image,
        Some(Err(why)) => CoverShows::BlackForImage(why),
    }
}

/// Writes the cover into `timeout` (the mapped timeout image of a device of
/// format `device`): the cover image `image` (its layout and bytes)
/// when [`cover_choice`] picks it, black (`black`, [`fill_black`])
/// otherwise. What it wrote. Bytes past `sizeimage` are left as they are.
pub(crate) fn fill_cover(
    timeout: &mut [u8],
    device: &ImageLayout,
    black: (u8, u8),
    image: Option<(&FrameLayout, &[u8])>,
) -> Result<CoverShows, String> {
    let shows = cover_choice(device, image.map(|(layout, _)| layout));
    match (&shows, image) {
        (CoverShows::Image, Some((_, bytes))) => {
            let size = device.sizeimage as usize;
            let Some(frame) = bytes.get(..size) else {
                return Err(format!(
                    "the image frame ({} bytes) is smaller than a frame ({size} bytes)",
                    bytes.len()
                ));
            };
            let Some(dst) = timeout.get_mut(..size) else {
                return Err(format!(
                    "the timeout image ({} bytes) is smaller than a frame ({size} bytes)",
                    timeout.len()
                ));
            };
            dst.copy_from_slice(frame);
        }
        _ => fill_black(timeout, device, black)?,
    }
    Ok(shows)
}

/// A shared mapping of the timeout image, unmapped when dropped.
struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    fn new(file: &fs::File, len: usize, offset: u32) -> io::Result<Mapping> {
        // SAFETY: a fresh shared mapping of `len` bytes of the open node at
        // `offset`; nothing else in this process refers to it.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                libc::off_t::from(offset),
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { ptr, len })
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` maps `len` writable bytes until `drop`, and `&mut
        // self` makes this the only reference to them.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// What [`install`] puts on which device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoverPlan {
    pub device: PathBuf,
    /// The writer's format: the device must have it.
    pub format: OutputFormat,
    /// `(Y, chroma)` of black in the writer's range.
    pub black: (u8, u8),
    /// The writer's longest gap between two frames while it is alive.
    pub writer_worst_gap: Duration,
    /// How the writer lays out its frames, the cover image's included.
    pub frame: FrameLayout,
}

/// What [`install`] put in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Installed {
    /// The `timeout` set, in ms.
    pub timeout_ms: u32,
    pub shows: CoverShows,
}

/// Puts the crash cover on `plan.device` (see the module docs): the
/// timeout image (the cover image `image`, a frame laid out as
/// `plan.frame`, or black: [`fill_cover`]), then the `timeout` control.
pub(crate) fn install(plan: &CoverPlan, image: Option<&[u8]>) -> Result<Installed, String> {
    let device = plan.device.display();
    // Read-only: the control ioctls do not need more, and its close raises
    // no IN_CLOSE_WRITE.
    let fd1 = controls::open_node(&plan.device, false)
        .map_err(|e| format!("cannot open {device}: {e}"))?;
    let found = LoopbackControls::query_on(&fd1)
        .map_err(|e| format!("cannot list the controls of {device}: {e}"))?;
    let (Some(timeout), Some(image_io)) = (found.timeout, found.timeout_image_io) else {
        return Err(format!(
            "{device} has no v4l2loopback `{TIMEOUT}` and `{TIMEOUT_IMAGE_IO}` controls (an \
             older v4l2loopback?)"
        ));
    };
    if timeout.info.type_ != CTRL_TYPE_INTEGER || image_io.info.type_ != CTRL_TYPE_BUTTON {
        return Err(format!(
            "{device}: the `{TIMEOUT}` / `{TIMEOUT_IMAGE_IO}` controls are not the integer and \
             button v4l2loopback has (types {} and {})",
            timeout.info.type_, image_io.info.type_
        ));
    }
    let Some(ms) = timeout_ms(plan.writer_worst_gap, timeout.info.maximum) else {
        return Err(format!(
            "{device}: the `{TIMEOUT}` control's maximum is {} ms: no timeout can be set",
            timeout.info.maximum
        ));
    };
    let shows = match upload_image(&fd1, plan, image, image_io.id) {
        Ok(shows) => shows,
        Err(failure) => {
            // upload_image's descriptors are closed: a pending mode is free to
            // be taken back.
            return Err(if failure.left_pending {
                pending_mode_failure(
                    &failure.why,
                    &device.to_string(),
                    &clear_timeout_image_io(&plan.device),
                )
            } else {
                failure.why
            });
        }
    };
    controls::ioctl_ctrl_on(
        &fd1,
        VIDIOC_S_CTRL,
        timeout.id,
        i32::try_from(ms).unwrap_or(i32::MAX),
    )
    .map_err(|e| format!("setting `{TIMEOUT}` to {ms} ms on {device}: {e}"))?;
    Ok(Installed {
        timeout_ms: ms,
        shows,
    })
}

/// Why [`upload_image`] failed, and whether the button's timeout-image mode
/// may still be pending (the failure came between the button and the
/// `VIDIOC_REQBUFS` that takes the mode back).
#[derive(Debug)]
struct UploadFailure {
    why: String,
    left_pending: bool,
}

impl UploadFailure {
    fn after(why: String) -> UploadFailure {
        UploadFailure {
            why,
            left_pending: false,
        }
    }
}

/// The failure message of an upload that may have left the timeout-image
/// mode pending (`why`), with the outcome of [`clear_timeout_image_io`].
pub(crate) fn pending_mode_failure(why: &str, device: &str, cleared: &io::Result<bool>) -> String {
    let left = match cleared {
        Ok(true) => format!("the timeout-image mode it left pending on {device} was taken back"),
        Ok(false) => format!("no timeout-image mode was left pending on {device}"),
        Err(e) => format!(
            "the timeout-image mode it may have left pending on {device} could not be taken \
             back ({e}): the next program to open the node may get a descriptor that captures \
             none of the frames written"
        ),
    };
    format!(
        "{why}; {left}. A program that opened {device} at that instant may have got a \
         descriptor in timeout-image mode, which captures none of the frames written (it has \
         to reopen the node)"
    )
}

/// A `struct v4l2_requestbuffers` for `count` output MMAP buffers.
fn output_request(count: u32) -> V4l2RequestBuffers {
    V4l2RequestBuffers {
        count,
        type_: BUF_TYPE_VIDEO_OUTPUT,
        memory: MEMORY_MMAP,
        capabilities: 0,
        flags: 0,
        reserved: [0; 3],
    }
}

/// Takes back a timeout-image mode left pending on the node at `path` (the
/// `timeout_image_io` button pressed, no descriptor having taken the image
/// since): opens one more descriptor and, if that one is in timeout-image
/// mode, has it take the image (which clears the mode) and give it back.
/// `Ok(true)`: a mode was pending, and is no longer; `Ok(false)`: none was.
///
/// The descriptor's `VIDIOC_REQBUFS` (output, MMAP, one buffer) tells: in
/// timeout-image mode it is granted, and its buffer is the timeout image
/// (`VIDIOC_QUERYBUF` offset past every frame buffer, never 0). Otherwise,
/// while a writer holds the output, it is refused (`EBUSY`); without a
/// writer it is granted as to a writer (offset 0) and given back at once.
/// Opened read-only; no image is mapped or changed.
pub fn clear_timeout_image_io(path: &Path) -> io::Result<bool> {
    let fd = controls::open_node(path, false)?;
    clear_with(
        |count| {
            let mut req = output_request(count);
            // SAFETY: VIDIOC_REQBUFS takes a `struct v4l2_requestbuffers`.
            unsafe { ioctl_rw(&fd, VIDIOC_REQBUFS, &mut req) }
        },
        || {
            // SAFETY: all-zero is a valid `struct v4l2_buffer`.
            let mut buf: V4l2Buffer = unsafe { zeroed() };
            buf.type_ = BUF_TYPE_VIDEO_OUTPUT;
            buf.memory = MEMORY_MMAP;
            // SAFETY: VIDIOC_QUERYBUF takes a `struct v4l2_buffer`.
            unsafe { ioctl_rw(&fd, VIDIOC_QUERYBUF, &mut buf) }?;
            // SAFETY: an MMAP buffer's `m` is its offset.
            Ok(unsafe { buf.m.offset })
        },
    )
}

/// [`clear_timeout_image_io`]'s steps, with `reqbufs(count)` and
/// `query_offset()` (buffer 0's `VIDIOC_QUERYBUF` offset) on its
/// descriptor: a granted request is always given back (`reqbufs(0)`).
fn clear_with(
    mut reqbufs: impl FnMut(u32) -> io::Result<()>,
    query_offset: impl FnOnce() -> io::Result<u32>,
) -> io::Result<bool> {
    match reqbufs(1) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => return Ok(false),
        Err(e) => return Err(e),
    }
    let offset = query_offset();
    let released = reqbufs(0);
    let pending = offset? != 0;
    released?;
    Ok(pending)
}

/// Uploads the timeout image (the module docs, step 1): `image` or black
/// ([`fill_cover`]). `fd1` is any descriptor of the node.
fn upload_image(
    fd1: &fs::File,
    plan: &CoverPlan,
    image: Option<&[u8]>,
    image_io_id: u32,
) -> Result<CoverShows, UploadFailure> {
    let device = plan.device.display();
    let mut req = output_request(1);
    controls::ioctl_ctrl_on(fd1, VIDIOC_S_CTRL, image_io_id, 1).map_err(|e| {
        UploadFailure::after(format!("pressing `{TIMEOUT_IMAGE_IO}` on {device}: {e}"))
    })?;
    // The mode is pending from here until a descriptor in it takes the
    // image: nothing between the button, the open() and the request.
    // Writable: the image is mapped shared and written.
    let fd2 = controls::open_node(&plan.device, true).map_err(|e| UploadFailure {
        why: format!("cannot open {device} for its timeout image: {e}"),
        left_pending: true,
    })?;
    // SAFETY: VIDIOC_REQBUFS takes a `struct v4l2_requestbuffers`.
    unsafe { ioctl_rw(&fd2, VIDIOC_REQBUFS, &mut req) }.map_err(|e| UploadFailure {
        why: format!(
            "the timeout image of {device} was not granted (VIDIOC_REQBUFS: {e}): another \
             process may have opened the node at the same instant"
        ),
        left_pending: true,
    })?;
    upload_on(&fd2, plan, image, &mut req).map_err(UploadFailure::after)
}

/// The rest of [`upload_image`], on `fd2`, the descriptor that took the
/// timeout image with `req` (which took the mode back).
fn upload_on(
    fd2: &fs::File,
    plan: &CoverPlan,
    image: Option<&[u8]>,
    req: &mut V4l2RequestBuffers,
) -> Result<CoverShows, String> {
    let device = plan.device.display();
    // From here on, closing fd2 also gives the image back.
    // SAFETY: all-zero is a valid `struct v4l2_format`.
    let mut fmt: V4l2Format = unsafe { zeroed() };
    fmt.type_ = BUF_TYPE_VIDEO_OUTPUT;
    // SAFETY: VIDIOC_G_FMT takes a `struct v4l2_format`.
    unsafe { ioctl_rw(fd2, VIDIOC_G_FMT, &mut fmt) }
        .map_err(|e| format!("VIDIOC_G_FMT on {device}: {e}"))?;
    // SAFETY: a single-planar output format fills `pix`; every bit pattern
    // is a valid `V4l2PixFormat`.
    let pix = unsafe { fmt.fmt.pix };
    let want = fourcc(plan.format);
    if pix.pixelformat != want {
        return Err(format!(
            "{device} carries {} frames, not the writer's {}",
            fourcc_string(pix.pixelformat),
            fourcc_string(want)
        ));
    }
    // SAFETY: all-zero is a valid `struct v4l2_buffer`.
    let mut buf: V4l2Buffer = unsafe { zeroed() };
    buf.index = 0;
    buf.type_ = BUF_TYPE_VIDEO_OUTPUT;
    buf.memory = MEMORY_MMAP;
    // SAFETY: VIDIOC_QUERYBUF takes a `struct v4l2_buffer`.
    unsafe { ioctl_rw(fd2, VIDIOC_QUERYBUF, &mut buf) }
        .map_err(|e| format!("VIDIOC_QUERYBUF on {device}: {e}"))?;
    // SAFETY: an MMAP buffer's `m` is its offset.
    let offset = unsafe { buf.m.offset };
    if offset == 0 {
        // The first frame buffer's offset: not the timeout image.
        return Err(format!(
            "{device} gave a frame buffer, not its timeout image"
        ));
    }
    let mut map = Mapping::new(fd2, buf.length as usize, offset)
        .map_err(|e| format!("cannot map the timeout image of {device}: {e}"))?;
    let shows = fill_cover(
        map.bytes(),
        &ImageLayout {
            fourcc: pix.pixelformat,
            bytesperline: pix.bytesperline,
            height: pix.height,
            sizeimage: pix.sizeimage,
        },
        plan.black,
        image.map(|bytes| (&plan.frame, bytes)),
    )
    .map_err(|e| format!("{device}: {e}"))?;
    drop(map);
    // A no-op for the timeout image, and the image given back; both also
    // happen when fd2 closes, so their errors do not matter.
    // SAFETY: VIDIOC_QBUF takes a `struct v4l2_buffer`.
    let _ = unsafe { ioctl_rw(fd2, VIDIOC_QBUF, &mut buf) };
    req.count = 0;
    // SAFETY: VIDIOC_REQBUFS takes a `struct v4l2_requestbuffers`.
    let _ = unsafe { ioctl_rw(fd2, VIDIOC_REQBUFS, req) };
    Ok(shows)
}

/// The cover's outcome for the writer's current start, its report, and the
/// helper thread that installs it.
#[derive(Default)]
pub(crate) struct CoverSlot {
    state: Mutex<SlotState>,
    changed: Condvar,
    /// The helper threads not joined yet (an install, re-installs).
    helpers: Mutex<Vec<JoinHandle<()>>>,
    /// What the cover shows: the cover image's frame, or `None` for black,
    /// with the reason the image asked for cannot be read (black then).
    /// The frame is read by each install when it runs.
    image: Mutex<(Option<gst::Buffer>, Option<String>)>,
    /// Held by an install from its first ioctl to its last: installs never
    /// overlap (a second button press while the first's descriptor holds
    /// the timeout image would leave the mode pending).
    install: Mutex<()>,
}

struct SlotState {
    cover: ConsumerCover,
    shows: CoverShows,
    /// Bumped at every start and stop: an outcome for an older one is
    /// dropped.
    generation: u64,
    /// The plan of the current start, once its install was handed to a
    /// helper: a re-install uses it. Cleared by a stop.
    armed: Option<CoverPlan>,
    /// Set by [`CoverSlot::disarm`] until the next start: no install or
    /// re-install is started any more, not even the start's own.
    disarmed: bool,
    report: Option<CoverReport>,
}

impl Default for SlotState {
    fn default() -> Self {
        SlotState {
            cover: ConsumerCover::Off,
            shows: CoverShows::Black,
            generation: 0,
            armed: None,
            disarmed: false,
            report: None,
        }
    }
}

impl std::fmt::Debug for CoverSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverSlot")
            .field("cover", &self.get())
            .finish_non_exhaustive()
    }
}

impl CoverSlot {
    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A writer start: the cover is pending; the generation of its outcome.
    pub(crate) fn begin(&self) -> u64 {
        let mut st = self.lock();
        st.generation += 1;
        st.cover = ConsumerCover::Pending;
        st.armed = None;
        st.disarmed = false;
        self.changed.notify_all();
        st.generation
    }

    /// The end of a writer start, before [`Self::join_helper`] and before
    /// the device is closed: no install or re-install is started any more
    /// (an image ready now, or the start's trigger firing late, starts
    /// nothing) until the next [`Self::begin`].
    pub(crate) fn disarm(&self) {
        let mut st = self.lock();
        st.armed = None;
        st.disarmed = true;
    }

    /// A writer stop (after [`Self::join_helper`]): a cover still pending
    /// will not come; one in place stays reported.
    pub(crate) fn cancel(&self) {
        let mut st = self.lock();
        st.generation += 1;
        st.armed = None;
        if st.cover == ConsumerCover::Pending {
            st.cover = ConsumerCover::Off;
        }
        self.changed.notify_all();
    }

    /// Records the outcome of an install (or re-install) for start
    /// `generation` and reports it, unless a later start or a stop came
    /// first.
    pub(crate) fn finish(&self, generation: u64, outcome: Result<Installed, String>) {
        let (cover, report) = {
            let mut st = self.lock();
            if st.generation != generation {
                return;
            }
            st.cover = match outcome {
                Ok(Installed { timeout_ms, shows }) => {
                    st.shows = shows;
                    ConsumerCover::Ready { timeout_ms }
                }
                Err(why) => ConsumerCover::Failed(why),
            };
            self.changed.notify_all();
            (st.cover.clone(), st.report.clone())
        };
        if let Some(report) = report {
            report(&cover);
        }
    }

    pub(crate) fn get(&self) -> ConsumerCover {
        self.lock().cover.clone()
    }

    /// The image frame an install would show now (`None`: black).
    #[cfg(test)]
    pub(crate) fn image(&self) -> Option<gst::Buffer> {
        self.image
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone()
    }

    /// What the timeout image of the last install that succeeded shows;
    /// black there while the cover image cannot be read is
    /// [`CoverShows::BlackForImage`] with the reason.
    pub(crate) fn shows(&self) -> CoverShows {
        let unreadable = self
            .image
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone();
        match (self.lock().shows.clone(), unreadable) {
            (CoverShows::Black, Some(why)) => CoverShows::BlackForImage(why),
            (shows, _) => shows,
        }
    }

    /// The outcome once it is no longer pending, or after `limit` (then
    /// still [`ConsumerCover::Pending`]).
    pub(crate) fn wait(&self, limit: Duration) -> ConsumerCover {
        let deadline = Instant::now() + limit;
        let mut st = self.lock();
        while st.cover == ConsumerCover::Pending {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        st.cover.clone()
    }

    pub(crate) fn set_report(&self, report: Option<CoverReport>) {
        self.lock().report = report;
    }

    /// Installs `plan` on a helper thread, for start `generation`, and
    /// keeps it for re-installs ([`Self::set_image`]). Never blocks the
    /// caller (a streaming thread) beyond the thread's spawn.
    pub(crate) fn spawn_helper(self: &Arc<Self>, generation: u64, plan: CoverPlan) {
        let mut st = self.lock();
        if st.generation != generation || st.disarmed {
            return;
        }
        st.armed = Some(plan.clone());
        if let Err(why) = self.spawn_locked(generation, plan, false) {
            drop(st);
            self.finish(generation, Err(why));
        }
    }

    /// Sets what the cover shows: the cover image's frame (`Some`, laid out
    /// as the plan's [`CoverPlan::frame`]) or black (`None`), with the
    /// reason the image asked for cannot be read (`unreadable`, black
    /// then). When the frame changes and the current start's install has
    /// been handed to a helper already, the cover is installed again, on a
    /// helper thread of its own (never blocking the caller); an install
    /// still to come reads it when it runs. A change of `unreadable` alone
    /// installs nothing (the cover is black either way): it changes what
    /// [`Self::shows`] reports.
    pub(crate) fn set_image(
        self: &Arc<Self>,
        image: Option<gst::Buffer>,
        unreadable: Option<String>,
    ) {
        {
            let mut current = self.image.lock().unwrap_or_else(|e| e.into_inner());
            current.1 = unreadable;
            let same = match (&current.0, &image) {
                (None, None) => true,
                (Some(a), Some(b)) => a.as_ptr() == b.as_ptr(),
                _ => false,
            };
            if same {
                return;
            }
            let old = std::mem::replace(&mut current.0, image);
            drop(current);
            drop(old);
        }
        let st = self.lock();
        let Some(plan) = st.armed.clone() else {
            return;
        };
        let generation = st.generation;
        if let Err(why) = self.spawn_locked(generation, plan, true) {
            drop(st);
            self.finish(generation, Err(why));
        }
    }

    /// The helper thread of an install (`again`: a re-install) of `plan`,
    /// spawned with the state locked (so a stop's [`Self::disarm`] either
    /// comes first or sees it in `helpers`). The helper runs under the
    /// install lock and reads the image when it runs: the last install
    /// shows the image of the last change.
    fn spawn_locked(
        self: &Arc<Self>,
        generation: u64,
        plan: CoverPlan,
        again: bool,
    ) -> Result<(), String> {
        let slot = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("obc-cover".into())
            .spawn(move || {
                let _one = slot.install.lock().unwrap_or_else(|e| e.into_inner());
                let image = slot
                    .image
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .0
                    .clone();
                let map = image.as_ref().and_then(|b| b.map_readable().ok());
                let outcome = install(&plan, map.as_ref().map(|m| m.as_slice())).map_err(|why| {
                    if again {
                        format!("installing it again for a new cover image: {why}")
                    } else {
                        why
                    }
                });
                drop(map);
                slot.finish(generation, outcome);
            });
        match spawned {
            Ok(handle) => {
                let mut helpers = self.helpers.lock().unwrap_or_else(|e| e.into_inner());
                // Finished helpers are joined now (a no-op).
                let (done, live): (Vec<_>, Vec<_>) =
                    helpers.drain(..).partition(JoinHandle::is_finished);
                *helpers = live;
                helpers.push(handle);
                drop(helpers);
                for h in done {
                    let _ = h.join();
                }
                Ok(())
            }
            Err(e) => Err(format!("cannot start its thread: {e}")),
        }
    }

    /// Waits for the helper threads, if any ran: their few ioctls are done
    /// and their descriptors closed when this returns.
    pub(crate) fn join_helper(&self) {
        let handles = std::mem::take(&mut *self.helpers.lock().unwrap_or_else(|e| e.into_inner()));
        for h in handles {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn struct_layouts_and_ioctl_numbers() {
        assert_eq!(std::mem::size_of::<V4l2PixFormat>(), 48);
        assert_eq!(std::mem::size_of::<V4l2Format>(), 208);
        assert_eq!(std::mem::offset_of!(V4l2Format, fmt), 8);
        assert_eq!(std::mem::size_of::<V4l2RequestBuffers>(), 20);
        assert_eq!(std::mem::size_of::<V4l2Timecode>(), 16);
        assert_eq!(std::mem::size_of::<V4l2Buffer>(), 88);
        assert_eq!(std::mem::offset_of!(V4l2Buffer, timestamp), 24);
        assert_eq!(std::mem::offset_of!(V4l2Buffer, m), 64);
        assert_eq!(std::mem::offset_of!(V4l2Buffer, length), 72);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
    }

    #[test]
    fn fourcc_codes() {
        assert_eq!(fourcc(OutputFormat::Nv12), 0x3231_564e);
        assert_eq!(fourcc(OutputFormat::I420), 0x3231_5559);
        assert_eq!(fourcc_string(fourcc(OutputFormat::I420)), "YU12");
    }

    /// 250 ms at the usual rates; above the writer's worst gap plus the
    /// margin at slow ones; whole milliseconds, rounded up (to the
    /// nanosecond); never above the driver's maximum; none when that
    /// maximum is not positive.
    #[test]
    fn the_timeout_covers_the_writers_worst_gap() {
        let gap = |period_ns: u64| Duration::from_nanos(3 * period_ns);
        let ms = |gap: Duration| timeout_ms(gap, 100_000).unwrap();
        // 30, 60 and 15 fps: three periods + 50 ms stay under 250 ms.
        assert_eq!(ms(gap(33_333_333)), 250);
        assert_eq!(ms(gap(16_666_666)), 250);
        assert_eq!(ms(gap(66_666_666)), 250);
        // 10 fps: 300 + 50; 5 fps: 600 + 50; 1 fps: 3000 + 50.
        assert_eq!(ms(gap(100_000_000)), 350);
        assert_eq!(ms(gap(200_000_000)), 650);
        assert_eq!(ms(gap(1_000_000_000)), 3050);
        // 7 fps: 3 x 142.857142 ms = 428.571426 ms + 50, rounded up.
        assert_eq!(ms(gap(142_857_142)), 479);
        // One nanosecond past a whole millisecond is the next one.
        assert_eq!(ms(Duration::from_nanos(200_000_001)), 251);
        assert_eq!(ms(Duration::from_nanos(200_000_000)), 250);
        assert_eq!(ms(Duration::ZERO), 250);
        // Capped at the control's maximum.
        assert_eq!(ms(Duration::from_secs(1000)), 100_000);
        assert_eq!(timeout_ms(Duration::ZERO, 200), Some(200));
        assert_eq!(timeout_ms(Duration::ZERO, 1), Some(1));
        assert_eq!(timeout_ms(Duration::MAX, i32::MAX), Some(i32::MAX as u32));
        // A maximum of 0 (or below) would turn the timeout off.
        assert_eq!(timeout_ms(Duration::ZERO, 0), None);
        assert_eq!(timeout_ms(Duration::ZERO, -1), None);
        assert_eq!(timeout_ms(Duration::from_secs(1), i32::MIN), None);
    }

    /// Taking back a pending timeout-image mode: a request refused with
    /// `EBUSY` is a descriptor not in that mode (none pending); a granted
    /// one is given back in every case, and was the mode if its buffer is
    /// the timeout image (offset not 0); other errors are errors.
    #[test]
    fn a_pending_timeout_image_mode_is_taken_back() {
        let run = |first: io::Result<()>, offset: io::Result<u32>, release: io::Result<()>| {
            let mut calls = Vec::new();
            let (mut first, mut release) = (Some(first), Some(release));
            let out = clear_with(
                |count| {
                    calls.push(count);
                    if count == 1 {
                        first.take().unwrap()
                    } else {
                        release.take().unwrap()
                    }
                },
                || offset,
            );
            (out.map_err(|e| e.raw_os_error()), calls)
        };
        fn err<T>(code: i32) -> io::Result<T> {
            Err(io::Error::from_raw_os_error(code))
        }
        // In timeout-image mode: granted, the timeout image, given back.
        assert_eq!(run(Ok(()), Ok(4 << 20), Ok(())), (Ok(true), vec![1, 0]));
        // A plain descriptor while a writer holds the output.
        assert_eq!(run(err(libc::EBUSY), Ok(0), Ok(())), (Ok(false), vec![1]));
        // A plain descriptor without a writer: granted a frame buffer, given
        // back.
        assert_eq!(run(Ok(()), Ok(0), Ok(())), (Ok(false), vec![1, 0]));
        // Errors: the request's, the query's (the request still given
        // back), the release's.
        assert_eq!(
            run(err(libc::ENOMEM), Ok(0), Ok(())),
            (Err(Some(libc::ENOMEM)), vec![1])
        );
        assert_eq!(
            run(Ok(()), err(libc::EINVAL), Ok(())),
            (Err(Some(libc::EINVAL)), vec![1, 0])
        );
        assert_eq!(
            run(Ok(()), Ok(4 << 20), err(libc::EIO)),
            (Err(Some(libc::EIO)), vec![1, 0])
        );
    }

    /// The warning of an upload that may have left the mode pending names
    /// the failure, what became of the mode, and the program that may have
    /// opened the node at that instant.
    #[test]
    fn a_failure_after_the_button_says_who_may_be_affected() {
        let why = "the timeout image of /dev/video11 was not granted";
        let msg = |cleared| pending_mode_failure(why, "/dev/video11", &cleared);
        let taken = msg(Ok(true));
        assert!(taken.starts_with(why), "{taken}");
        assert!(taken.contains("was taken back"), "{taken}");
        let none = msg(Ok(false));
        assert!(
            none.contains("no timeout-image mode was left pending"),
            "{none}"
        );
        let stuck = msg(Err(io::Error::from_raw_os_error(libc::EBUSY)));
        assert!(stuck.contains("could not be taken back"), "{stuck}");
        assert!(
            stuck.contains("the next program to open the node"),
            "{stuck}"
        );
        for m in [&taken, &none, &stuck] {
            assert!(
                m.contains("A program that opened /dev/video11 at that instant may have"),
                "{m}"
            );
        }
    }

    #[test]
    fn the_black_fill_is_luma_then_chroma_up_to_sizeimage() {
        // NV12 1920x1080: 1920 x 1080 luma, half that of chroma, in a
        // page-aligned buffer whose tail is left alone.
        let layout = ImageLayout {
            fourcc: fourcc(OutputFormat::Nv12),
            bytesperline: 1920,
            height: 1080,
            sizeimage: 1920 * 1080 * 3 / 2,
        };
        let len = (layout.sizeimage as usize).div_ceil(4096) * 4096;
        let mut image = vec![0u8; len];
        fill_black(&mut image, &layout, (16, 128)).unwrap();
        let luma = 1920 * 1080;
        assert!(image[..luma].iter().all(|&b| b == 16));
        assert!(image[luma..luma * 3 / 2].iter().all(|&b| b == 128));
        assert!(image[luma * 3 / 2..].iter().all(|&b| b == 0));
        // I420 with a padded stride: the luma plane is stride x height.
        let layout = ImageLayout {
            fourcc: fourcc(OutputFormat::I420),
            bytesperline: 656,
            height: 480,
            sizeimage: 656 * 480 + 2 * (328 * 240),
        };
        let mut image = vec![7u8; layout.sizeimage as usize];
        fill_black(&mut image, &layout, (16, 128)).unwrap();
        assert!(image[..656 * 480].iter().all(|&b| b == 16));
        assert!(image[656 * 480..].iter().all(|&b| b == 128));
        // Full-range black.
        fill_black(&mut image, &layout, (0, 128)).unwrap();
        assert_eq!((image[0], image[656 * 480]), (0, 128));
    }

    #[test]
    fn a_black_fill_that_does_not_fit_is_refused() {
        let layout = ImageLayout {
            fourcc: fourcc(OutputFormat::Nv12),
            bytesperline: 320,
            height: 240,
            sizeimage: 320 * 240 * 3 / 2,
        };
        let mut small = vec![0u8; 320 * 240];
        let e = fill_black(&mut small, &layout, (16, 128)).unwrap_err();
        assert!(e.contains("smaller than a frame"), "{e}");
        assert!(small.iter().all(|&b| b == 0), "written before refusing");
        for bad in [
            ImageLayout {
                bytesperline: 0,
                ..layout
            },
            ImageLayout {
                sizeimage: 320 * 240,
                ..layout
            },
        ] {
            let mut image = vec![0u8; 320 * 240 * 2];
            let e = fill_black(&mut image, &bad, (16, 128)).unwrap_err();
            assert!(e.contains("NV12"), "{e}");
        }
    }

    fn info(name: &str, type_: u32) -> CtrlInfo {
        CtrlInfo {
            name: name.into(),
            minimum: 0,
            maximum: 100_000,
            step: 1,
            default_value: 0,
            flags: 0,
            type_,
        }
    }

    /// The controls are found by name among all the node's, whatever their
    /// ids; the walk ends at the end, at an id that does not increase, or
    /// at an error.
    #[test]
    fn loopback_controls_are_found_by_name() {
        let list = [
            (0x0098_0900, info("brightness", CTRL_TYPE_INTEGER)),
            (0x0098_f900, info(KEEP_FORMAT, 2)),
            (0x0098_f901, info(SUSTAIN_FRAMERATE, 2)),
            (0x0098_f902, info(TIMEOUT, CTRL_TYPE_INTEGER)),
            (0x0098_f903, info(TIMEOUT_IMAGE_IO, CTRL_TYPE_BUTTON)),
        ];
        let mut asked = Vec::new();
        let found = LoopbackControls::collect(|after| {
            asked.push(after);
            Ok(list.iter().find(|(id, _)| *id > after).cloned())
        })
        .unwrap();
        assert_eq!(
            asked,
            [
                0,
                0x0098_0900,
                0x0098_f900,
                0x0098_f901,
                0x0098_f902,
                0x0098_f903
            ]
        );
        assert_eq!(found.keep_format.unwrap().id, 0x0098_f900);
        assert_eq!(found.sustain_framerate.unwrap().id, 0x0098_f901);
        let timeout = found.timeout.unwrap();
        assert_eq!((timeout.id, timeout.info.maximum), (0x0098_f902, 100_000));
        assert_eq!(found.timeout_image_io.unwrap().info.type_, CTRL_TYPE_BUTTON);

        // A driver without them.
        let none = LoopbackControls::collect(|after| {
            Ok((after == 0).then(|| (1, info("brightness", CTRL_TYPE_INTEGER))))
        })
        .unwrap();
        assert_eq!(none, LoopbackControls::default());
        // A driver answering the same id forever.
        let mut calls = 0;
        LoopbackControls::collect(|_| {
            calls += 1;
            Ok(Some((5, info("x", CTRL_TYPE_INTEGER))))
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert!(
            LoopbackControls::collect(|_| Err(io::Error::from_raw_os_error(libc::EIO))).is_err()
        );
    }

    /// NV12 at `w x h` as `GstVideoInfo` lays it out for even sizes with
    /// a width that is a multiple of 4.
    fn nv12_frame(w: u32, h: u32) -> FrameLayout {
        let luma = (w * h) as usize;
        FrameLayout {
            fourcc: fourcc(OutputFormat::Nv12),
            height: h,
            planes: 2,
            strides: [w as usize, w as usize, 0],
            offsets: [0, luma, 0],
            size: luma * 3 / 2,
        }
    }

    fn nv12_device(bytesperline: u32, h: u32) -> ImageLayout {
        ImageLayout {
            fourcc: fourcc(OutputFormat::Nv12),
            bytesperline,
            height: h,
            sizeimage: bytesperline * h * 3 / 2,
        }
    }

    fn missing_node_plan() -> CoverPlan {
        CoverPlan {
            device: "/nonexistent/obc-no-such-video-node".into(),
            format: OutputFormat::Nv12,
            black: (16, 128),
            writer_worst_gap: Duration::from_millis(100),
            frame: nv12_frame(320, 240),
        }
    }

    /// The frame layout the writer has, as `GstVideoInfo` computes it,
    /// fits the device's format v4l2loopback reports for the same caps
    /// (NV12 1920x1080: 1920 bytes per line, 3110400 bytes), in both
    /// formats.
    #[test]
    fn the_writers_frames_fit_the_devices_format() {
        gst::init().unwrap();
        for (format, w, h) in [
            (OutputFormat::Nv12, 1920, 1080),
            (OutputFormat::Nv12, 1280, 720),
            (OutputFormat::I420, 1920, 1080),
            (OutputFormat::I420, 640, 480),
        ] {
            let contract = obc_core::OutputContract {
                format,
                width: w,
                height: h,
                ..obc_core::OutputContract::default()
            };
            let frame = crate::loopback::frame_layout(&contract).unwrap();
            let device = ImageLayout {
                fourcc: fourcc(format),
                bytesperline: w,
                height: h,
                sizeimage: w * h * 3 / 2,
            };
            assert_eq!(frame_fits(&device, &frame), Ok(()), "{format:?} {w}x{h}");
        }
    }

    /// The cover shows black with no image, the image when its frame is a
    /// frame of the device's format, and black with the reason otherwise
    /// (another stride, size, height or fourcc).
    #[test]
    fn the_cover_shows_the_image_only_when_the_device_format_is_its_layout() {
        let frame = nv12_frame(320, 240);
        let device = nv12_device(320, 240);
        assert_eq!(cover_choice(&device, None), CoverShows::Black);
        assert_eq!(cover_choice(&device, Some(&frame)), CoverShows::Image);
        let padded = ImageLayout {
            bytesperline: 384,
            ..nv12_device(384, 240)
        };
        let bigger = ImageLayout {
            sizeimage: device.sizeimage + 4096,
            ..device
        };
        let taller = ImageLayout {
            height: 248,
            sizeimage: 320 * 248 * 3 / 2,
            ..device
        };
        let i420 = ImageLayout {
            fourcc: fourcc(OutputFormat::I420),
            ..device
        };
        let yuyv = ImageLayout {
            fourcc: u32::from_le_bytes(*b"YUYV"),
            ..device
        };
        for bad in [padded, bigger, taller, i420, yuyv] {
            match cover_choice(&bad, Some(&frame)) {
                CoverShows::BlackForImage(why) => {
                    assert!(why.contains("the device"), "{why}");
                }
                other => panic!("{bad:?}: {other:?}"),
            }
            // Black is still written, at the device's own layout.
            if bad.fourcc == device.fourcc {
                let mut timeout = vec![0u8; bad.sizeimage as usize];
                let image = vec![200u8; frame.size];
                let shows =
                    fill_cover(&mut timeout, &bad, (16, 128), Some((&frame, &image))).unwrap();
                assert!(matches!(shows, CoverShows::BlackForImage(_)));
                assert_eq!(timeout[0], 16);
                assert!(!timeout.contains(&200), "image bytes in a black cover");
            }
        }
    }

    /// The cover's bytes: the image frame, byte for byte up to
    /// `sizeimage` (the page-aligned tail untouched), or black.
    #[test]
    fn the_cover_writes_the_image_or_black() {
        let frame = nv12_frame(320, 240);
        let device = nv12_device(320, 240);
        let image: Vec<u8> = (0..frame.size).map(|i| (i % 251) as u8).collect();
        let len = frame.size.div_ceil(4096) * 4096;
        let mut timeout = vec![7u8; len];
        let shows = fill_cover(&mut timeout, &device, (16, 128), Some((&frame, &image))).unwrap();
        assert_eq!(shows, CoverShows::Image);
        assert_eq!(&timeout[..frame.size], &image[..]);
        assert!(timeout[frame.size..].iter().all(|&b| b == 7));
        let shows = fill_cover(&mut timeout, &device, (16, 128), None).unwrap();
        assert_eq!(shows, CoverShows::Black);
        assert!(timeout[..320 * 240].iter().all(|&b| b == 16));
        assert!(timeout[320 * 240..frame.size].iter().all(|&b| b == 128));
        // An image frame or a timeout image too short is refused.
        let e = fill_cover(
            &mut timeout,
            &device,
            (16, 128),
            Some((&frame, &image[..100])),
        )
        .unwrap_err();
        assert!(e.contains("image frame"), "{e}");
        let mut short = vec![0u8; 100];
        let e = fill_cover(&mut short, &device, (16, 128), Some((&frame, &image))).unwrap_err();
        assert!(e.contains("timeout image"), "{e}");
    }

    #[test]
    fn a_missing_node_fails_the_cover() {
        let plan = missing_node_plan();
        let e = install(&plan, None).unwrap_err();
        assert!(e.contains("cannot open"), "{e}");
        assert!(LoopbackControls::query(&plan.device).is_err());
    }

    /// The slot: pending from a start, the outcome of that start only
    /// (reported once), off after a stop that came first; a waiter returns
    /// at the outcome or after its limit.
    #[test]
    fn the_slot_keeps_the_outcome_of_the_current_start() {
        let ready = |timeout_ms| {
            Ok(Installed {
                timeout_ms,
                shows: CoverShows::Black,
            })
        };
        let slot = Arc::new(CoverSlot::default());
        assert_eq!(slot.get(), ConsumerCover::Off);
        let reports = Arc::new(AtomicUsize::new(0));
        let seen: Arc<Mutex<Vec<ConsumerCover>>> = Arc::default();
        let (r, s) = (Arc::clone(&reports), Arc::clone(&seen));
        slot.set_report(Some(Arc::new(move |c: &ConsumerCover| {
            r.fetch_add(1, Ordering::Relaxed);
            s.lock().unwrap().push(c.clone());
        })));

        let g1 = slot.begin();
        assert_eq!(slot.get(), ConsumerCover::Pending);
        let t = Instant::now();
        assert_eq!(slot.wait(Duration::from_millis(30)), ConsumerCover::Pending);
        assert!(t.elapsed() >= Duration::from_millis(30));
        let waiter = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || slot.wait(Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(20));
        slot.finish(g1, ready(250));
        assert_eq!(
            waiter.join().unwrap(),
            ConsumerCover::Ready { timeout_ms: 250 }
        );
        // A stop keeps a cover in place.
        slot.cancel();
        assert_eq!(slot.get(), ConsumerCover::Ready { timeout_ms: 250 });

        // A stale outcome (its start was stopped) is dropped, unreported.
        let g2 = slot.begin();
        slot.cancel();
        assert_eq!(slot.get(), ConsumerCover::Off);
        slot.finish(g2, ready(250));
        assert_eq!(slot.get(), ConsumerCover::Off);

        let g3 = slot.begin();
        slot.finish(g3, Err("no controls".into()));
        assert_eq!(slot.get(), ConsumerCover::Failed("no controls".into()));
        assert_eq!(reports.load(Ordering::Relaxed), 2);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                ConsumerCover::Ready { timeout_ms: 250 },
                ConsumerCover::Failed("no controls".into())
            ]
        );

        // The helper thread reports a failed install, and is joined.
        let g4 = slot.begin();
        slot.spawn_helper(g4, missing_node_plan());
        slot.join_helper();
        assert!(matches!(slot.get(), ConsumerCover::Failed(e) if e.contains("cannot open")));
        assert_eq!(reports.load(Ordering::Relaxed), 3);
    }

    /// A change of the image after the start's install was handed to a
    /// helper installs the cover again (reported, said to be a
    /// re-install); an unchanged image does not; before the install and
    /// after a stop nothing is started, and an install still to come
    /// reads the image when it runs. The installs fail here (no device):
    /// their reports are what is counted.
    #[test]
    fn a_new_cover_image_installs_the_cover_again() {
        gst::init().unwrap();
        let slot = Arc::new(CoverSlot::default());
        let seen: Arc<Mutex<Vec<ConsumerCover>>> = Arc::default();
        let s = Arc::clone(&seen);
        slot.set_report(Some(Arc::new(move |c: &ConsumerCover| {
            s.lock().unwrap().push(c.clone());
        })));
        let image = gst::Buffer::from_mut_slice(vec![0u8; 16]);
        // Before any start: nothing to install again.
        slot.set_image(Some(image.clone()), None);
        slot.join_helper();
        assert!(seen.lock().unwrap().is_empty());

        let g = slot.begin();
        // Started, its install not handed over yet: nothing either.
        slot.set_image(None, None);
        slot.join_helper();
        assert!(seen.lock().unwrap().is_empty());
        slot.spawn_helper(g, missing_node_plan());
        slot.join_helper();
        assert_eq!(seen.lock().unwrap().len(), 1);

        slot.set_image(Some(image.clone()), None);
        slot.join_helper();
        // The same frame again: no install.
        slot.set_image(Some(image.clone()), None);
        slot.join_helper();
        slot.set_image(None, None);
        slot.join_helper();
        slot.set_image(None, None);
        slot.join_helper();
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 3, "{seen:?}");
            for c in &seen[1..] {
                assert!(
                    matches!(c, ConsumerCover::Failed(e) if e.contains("installing it again for a new cover image")),
                    "{c:?}"
                );
            }
        }
        // Stopped: no install.
        slot.disarm();
        slot.cancel();
        slot.set_image(Some(image), None);
        slot.join_helper();
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    /// A stop disarms the cover first: once disarmed, neither the start's
    /// trigger firing late nor a new image starts an install, and the next
    /// start arms it again.
    #[test]
    fn a_disarmed_cover_starts_no_install() {
        gst::init().unwrap();
        let slot = Arc::new(CoverSlot::default());
        let seen = Arc::new(AtomicUsize::new(0));
        let s = Arc::clone(&seen);
        slot.set_report(Some(Arc::new(move |_: &ConsumerCover| {
            s.fetch_add(1, Ordering::Relaxed);
        })));
        let g = slot.begin();
        slot.spawn_helper(g, missing_node_plan());
        slot.join_helper();
        assert_eq!(seen.load(Ordering::Relaxed), 1);
        slot.disarm();
        slot.set_image(Some(gst::Buffer::from_mut_slice(vec![0u8; 16])), None);
        slot.spawn_helper(g, missing_node_plan());
        slot.join_helper();
        assert_eq!(
            seen.load(Ordering::Relaxed),
            1,
            "an install after the disarm"
        );
        slot.cancel();
        let g = slot.begin();
        slot.spawn_helper(g, missing_node_plan());
        slot.join_helper();
        assert_eq!(seen.load(Ordering::Relaxed), 2, "the next start installs");
    }

    /// An unreadable cover image: black in place, and the reason in what
    /// the cover is said to show; the reason alone installs nothing.
    #[test]
    fn an_unreadable_cover_image_shows_black_with_the_reason() {
        gst::init().unwrap();
        let slot = Arc::new(CoverSlot::default());
        let seen = Arc::new(AtomicUsize::new(0));
        let s = Arc::clone(&seen);
        slot.set_report(Some(Arc::new(move |_: &ConsumerCover| {
            s.fetch_add(1, Ordering::Relaxed);
        })));
        let g = slot.begin();
        slot.spawn_helper(g, missing_node_plan());
        slot.join_helper();
        assert_eq!(slot.shows(), CoverShows::Black);
        slot.set_image(None, Some("Away image x could not be read".into()));
        slot.join_helper();
        assert_eq!(seen.load(Ordering::Relaxed), 1, "black either way");
        assert_eq!(
            slot.shows(),
            CoverShows::BlackForImage("Away image x could not be read".into())
        );
        slot.set_image(None, None);
        assert_eq!(slot.shows(), CoverShows::Black);
    }
}
