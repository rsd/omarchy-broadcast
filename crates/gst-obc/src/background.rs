//! The background image of the `replace` effect: checked, decoded with
//! GStreamer, cropped to fill the output's aspect ratio (centred) and scaled
//! to the output size, as packed BGR in host memory
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`).
//!
//! The file is limited to [`MAX_FILE_BYTES`], to PNG and JPEG, and to
//! [`MAX_DIMENSION`] pixels on each side; anything else is refused with a
//! reason before any decoder runs:
//!
//! * The file is opened once, without blocking (`O_NONBLOCK | O_NOCTTY`,
//!   and close-on-exec), checked on the open handle (`fstat`: a regular
//!   file of at most [`MAX_FILE_BYTES`]) and read into memory, bounded.
//!   The decoder reads those bytes, so nothing opens the path again, and a
//!   path that is (or is swapped for) a FIFO or a device is refused at once
//!   instead of blocking the caller.
//! * The size is read from the image's own header (the PNG `IHDR` chunk,
//!   the JPEG frame header `SOFn`) before the decoder is started, because a
//!   decoder allocates the full image as soon as it knows its size: a small
//!   file declaring 60000 x 60000 pixels would otherwise cost about 10 GB.
//!   Only formats whose header this module reads are accepted; GStreamer's
//!   parsers are not relied on for the size (they are in gst-plugins-bad,
//!   and not every format has one). The size the decoder reports is checked
//!   again before anything downstream of it allocates.
//! * The first image in the file is delimited by walking its structure
//!   (PNG chunks up to `IEND`, JPEG segments and scans up to `EOI`):
//!   `pngdec` and `jpegdec` decode every image in their input, and only the
//!   first one's header was read, so a small image followed by a second one
//!   declaring a huge size would otherwise reach the decoder. The decoder is
//!   handed only the first image's bytes, so it never sees a second image;
//!   anything after the first image (a gain map, an embedded video, padding)
//!   is ignored, and a file that ends before its first image does is refused.
//!
//! The fallback image of `on-failure=image` (shown in place of black frames
//! when an effect cannot run) goes through the same checks and decode, then
//! is converted on the CPU to YUV 4:2:0 in the frames' colour
//! ([`decode_yuv420`], [`Yuv420`]); its reasons say "the image file", the
//! caller naming the path ([`crate::fallback_unreadable`]).

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gst::prelude::*;
use gst_video::prelude::*;
use obc_backend::{ColourMatrix, ColourSpec, HostBgr};

/// The largest background file accepted, in bytes (32 MiB).
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// The largest decoded background accepted, in pixels on each side.
pub const MAX_DIMENSION: u32 = 8192;
/// How long a decode may take before it is abandoned.
const DECODE_TIMEOUT: Duration = Duration::from_secs(10);

/// Pixels removed from each edge of the source image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CropRect {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// The centred crop that gives a `src_w` x `src_h` image the aspect ratio of
/// `dst_w` x `dst_h`: the excess width of a wider image, or the excess
/// height of a taller one, split between the two sides (the odd pixel on
/// the right or bottom). An image of the same aspect ratio, and degenerate
/// (zero) sizes, crop nothing; at least one pixel is always kept.
///
/// The background is packed BGR, so unlike the camera crop
/// (`obc_pipeline::crop_for`, 4:2:0 frames) the values need not be even.
pub fn crop_to_fill(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> CropRect {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return CropRect::default();
    }
    let (sw, sh, dw, dh) = (
        u64::from(src_w),
        u64::from(src_h),
        u64::from(dst_w),
        u64::from(dst_h),
    );
    // Compare aspect ratios exactly: sw/sh vs dw/dh.
    let split = |excess: u64| {
        let first = excess / 2;
        (first as u32, (excess - first) as u32)
    };
    if sw * dh > dw * sh {
        // Wider: keep round(sh * dw / dh) columns.
        let keep = ((2 * sh * dw + dh) / (2 * dh)).clamp(1, sw);
        let (left, right) = split(sw - keep);
        CropRect {
            left,
            right,
            ..CropRect::default()
        }
    } else if sw * dh < dw * sh {
        // Taller: keep round(sw * dh / dw) rows.
        let keep = ((2 * sw * dh + dw) / (2 * dw)).clamp(1, sh);
        let (top, bottom) = split(sh - keep);
        CropRect {
            top,
            bottom,
            ..CropRect::default()
        }
    } else {
        CropRect::default()
    }
}

/// Opens `path` without blocking and checks the open handle: an absolute
/// path to a readable regular file of at most [`MAX_FILE_BYTES`]. Returns
/// the handle and its size. The reason names the path.
///
/// `O_NONBLOCK` makes opening a FIFO with no writer return at once (it
/// would otherwise wait for one), `O_NOCTTY` keeps a terminal from becoming
/// the daemon's controlling terminal, and Rust opens close-on-exec. The
/// type and size come from the handle (`fstat`), not from the path, so a
/// path swapped after the check cannot slip through.
fn open_checked(path: &Path, subject: Subject) -> Result<(std::fs::File, u64), String> {
    if !path.is_absolute() {
        return Err(format!(
            "{} path {} is not absolute",
            subject.noun,
            path.display()
        ));
    }
    let what = subject.of(path);
    let unreadable = |e: std::io::Error| format!("{what} is unreadable: {e}");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() {
        return Err(format!("{what} is not a regular file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(too_big(&what, meta.len()));
    }
    Ok((file, meta.len()))
}

fn too_big(what: &str, bytes: u64) -> String {
    format!(
        "{what} is {bytes} bytes, more than the {} MiB limit",
        MAX_FILE_BYTES / (1024 * 1024)
    )
}

/// How the reasons name the file.
#[derive(Debug, Clone, Copy)]
struct Subject {
    noun: &'static str,
    /// The path follows the noun.
    with_path: bool,
}

impl Subject {
    /// The replace effect's image: "the background image <path>".
    const BACKGROUND: Subject = Subject {
        noun: "the background image",
        with_path: true,
    };
    /// The fallback image: "the image file" (its caller names the path).
    const FALLBACK: Subject = Subject {
        noun: "the image file",
        with_path: false,
    };

    fn of(self, path: &Path) -> String {
        if self.with_path {
            format!("{} {}", self.noun, path.display())
        } else {
            self.noun.to_string()
        }
    }
}

/// Checks that `path` is an absolute path to a readable regular file of at
/// most [`MAX_FILE_BYTES`]; returns its size. The reason names the path.
/// Never blocks on a FIFO or a device (see [`read_file`]).
pub fn check_file(path: &Path) -> Result<u64, String> {
    open_checked(path, Subject::BACKGROUND).map(|(_, len)| len)
}

/// Reads the background file into memory after the checks of
/// [`check_file`], made on the same handle; at most [`MAX_FILE_BYTES`] are
/// read even if the file grows meanwhile.
pub fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    read_file_as(path, Subject::BACKGROUND)
}

fn read_file_as(path: &Path, subject: Subject) -> Result<Vec<u8>, String> {
    let (file, len) = open_checked(path, subject)?;
    let mut data = Vec::with_capacity(len as usize);
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("{} is unreadable: {e}", subject.of(path)))?;
    if data.len() as u64 > MAX_FILE_BYTES {
        return Err(too_big(&subject.of(path), data.len() as u64));
    }
    Ok(data)
}

/// An accepted background image format, recognised by its signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// The caps the decoder takes, and the decoder (both in gst-plugins-good).
    fn decoder(self) -> (&'static str, &'static str) {
        match self {
            ImageFormat::Png => ("image/png", "pngdec"),
            ImageFormat::Jpeg => ("image/jpeg", "jpegdec"),
        }
    }
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The format and the declared width and height of an image, read from
/// its header without decoding it: the PNG `IHDR` chunk (which the PNG
/// specification places first), or the first JPEG frame header (`SOF0` to
/// `SOF15`, other than the `DHT`, `JPG` and `DAC` markers that share the
/// range), reached by skipping the segments before it by their lengths.
pub fn image_header(data: &[u8]) -> Result<(ImageFormat, u32, u32), String> {
    let be32 = |o: usize| u32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    let be16 = |o: usize| u32::from(u16::from_be_bytes([data[o], data[o + 1]]));
    if data.starts_with(PNG_SIGNATURE) {
        // Signature, then the IHDR chunk: length 13, "IHDR", width, height.
        if data.len() < 24 || be32(8) != 13 || &data[12..16] != b"IHDR" {
            return Err("it is a PNG file without a valid IHDR header".into());
        }
        return Ok((ImageFormat::Png, be32(16), be32(20)));
    }
    if data.starts_with(&[0xFF, 0xD8]) {
        let no_frame = || "it is a JPEG file without a frame header".to_string();
        let mut i = 2;
        loop {
            // A marker: 0xFF (any number of fill bytes), then its code.
            if data.get(i) != Some(&0xFF) {
                return Err(no_frame());
            }
            while data.get(i) == Some(&0xFF) {
                i += 1;
            }
            let Some(&marker) = data.get(i) else {
                return Err(no_frame());
            };
            i += 1;
            match marker {
                // Markers without a segment: TEM, RSTn, SOI.
                0x01 | 0xD0..=0xD8 => continue,
                // End of image, or the scan data before any frame header.
                0xD9 | 0xDA => return Err(no_frame()),
                _ => {}
            }
            if i + 2 > data.len() {
                return Err(no_frame());
            }
            let len = be16(i) as usize;
            if len < 2 {
                return Err(no_frame());
            }
            let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if is_sof {
                // Length, sample precision, height, width.
                if len < 7 || i + 7 > data.len() {
                    return Err(no_frame());
                }
                return Ok((ImageFormat::Jpeg, be16(i + 5), be16(i + 3)));
            }
            i += len;
        }
    }
    Err("it is neither a PNG nor a JPEG file".into())
}

/// The length in bytes of the first image in `data`, whose format
/// [`image_header`] found: the PNG signature and its chunks up to and
/// including `IEND`, or the JPEG from `SOI` to its `EOI` marker. A file
/// that ends first is refused.
///
/// PNG chunks are skipped by their lengths (a length above 2^31 - 1 is
/// invalid). JPEG segments are skipped by their lengths; after each scan
/// header (`SOS`) the entropy-coded data runs to the next marker, where
/// `FF 00` (a stuffed `FF` byte) and `RST0` to `RST7` are part of the data
/// and `FF` fill bytes may precede a marker. This is how `pngdec` and
/// `jpegdec` split a byte stream into images, so the decoder, handed
/// exactly these bytes, sees one image: the one whose size was checked.
pub fn image_length(format: ImageFormat, data: &[u8]) -> Result<usize, String> {
    match format {
        ImageFormat::Png => png_length(data),
        ImageFormat::Jpeg => jpeg_length(data),
    }
}

fn png_length(data: &[u8]) -> Result<usize, String> {
    let truncated = || "it is a PNG file that ends before its IEND chunk".to_string();
    let mut i = PNG_SIGNATURE.len();
    loop {
        // A chunk: length, type, data, CRC.
        let Some(head) = data.get(i..i + 8) else {
            return Err(truncated());
        };
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        if len > i32::MAX as u32 {
            return Err("it is a PNG file with an invalid chunk length".into());
        }
        let end = (len as usize)
            .checked_add(12)
            .and_then(|n| n.checked_add(i))
            .filter(|&end| end <= data.len())
            .ok_or_else(truncated)?;
        if &head[4..8] == b"IEND" {
            return Ok(end);
        }
        i = end;
    }
}

fn jpeg_length(data: &[u8]) -> Result<usize, String> {
    let truncated = || "it is a JPEG file that ends before its end-of-image marker".to_string();
    let invalid = || "it is a JPEG file with an invalid marker".to_string();
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Err(invalid());
    }
    let mut i = 2;
    loop {
        // A marker: 0xFF (any number of fill bytes), then its code.
        match data.get(i) {
            Some(&0xFF) => {}
            Some(_) => return Err(invalid()),
            None => return Err(truncated()),
        }
        while data.get(i) == Some(&0xFF) {
            i += 1;
        }
        let Some(&marker) = data.get(i) else {
            return Err(truncated());
        };
        i += 1;
        match marker {
            0xD9 => return Ok(i),
            // Markers without a segment: TEM, RSTn.
            0x01 | 0xD0..=0xD7 => continue,
            // A second SOI, or a stuffed byte where a marker belongs.
            0xD8 | 0x00 => return Err(invalid()),
            _ => {}
        }
        let Some(len) = data.get(i..i + 2) else {
            return Err(truncated());
        };
        let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
        if len < 2 {
            return Err(invalid());
        }
        i = i
            .checked_add(len)
            .filter(|&end| end <= data.len())
            .ok_or_else(truncated)?;
        if marker == 0xDA {
            // The entropy-coded data, up to the next marker (left at `i`).
            loop {
                let Some(ff) = data
                    .get(i..)
                    .and_then(|d| d.iter().position(|&b| b == 0xFF))
                else {
                    return Err(truncated());
                };
                i += ff;
                match data.get(i + 1) {
                    None => return Err(truncated()),
                    // A stuffed 0xFF, or a restart marker: still data.
                    Some(0x00 | 0xD0..=0xD7) => i += 2,
                    // A fill byte: the marker follows.
                    Some(0xFF) => i += 1,
                    Some(_) => break,
                }
            }
        }
    }
}

/// The reason a `w` x `h` image is refused, if it is.
fn check_dimensions(what: &str, w: u32, h: u32) -> Result<(), String> {
    if w > MAX_DIMENSION || h > MAX_DIMENSION || w == 0 || h == 0 {
        return Err(format!(
            "{what} is {w}x{h} pixels, outside the {MAX_DIMENSION}x{MAX_DIMENSION} limit"
        ));
    }
    Ok(())
}

/// Decodes `path` into a `width` x `height` BGR image, cropped to fill
/// (centred) and scaled. The file is read into memory, its header checked
/// and its first image delimited first ([`read_file`], [`image_header`],
/// [`image_length`]; anything after the first image is ignored); then:
///
/// ```text
/// appsrc (the bytes) ! pngdec | jpegdec ! videoconvert ! BGR ! videocrop
///        ! videoscale ! BGR,width,height ! appsink
/// ```
///
/// `videocrop` is set from the decoded size when the caps reach
/// `videoconvert`, which also refuses a decoded size outside the limit (in
/// case it differs from the header's). Blocks for the decode (bounded by a
/// timeout); run it off the streaming thread.
pub fn decode(path: &Path, width: u32, height: u32) -> Result<HostBgr, String> {
    decode_as(path, width, height, Subject::BACKGROUND)
}

fn decode_as(path: &Path, width: u32, height: u32, subject: Subject) -> Result<HostBgr, String> {
    let mut data = read_file_as(path, subject)?;
    if width == 0 || height == 0 {
        return Err(format!("cannot scale an image to {width}x{height}"));
    }
    let what = subject.of(path);
    let (format, w, h) = image_header(&data).map_err(|e| format!("cannot decode {what}: {e}"))?;
    check_dimensions(&what, w, h)?;
    let len = image_length(format, &data).map_err(|e| format!("cannot decode {what}: {e}"))?;
    data.truncate(len);
    gst::init().map_err(|e| format!("GStreamer: {e}"))?;
    let make = |factory: &str| {
        gst::ElementFactory::make(factory)
            .build()
            .map_err(|e| format!("cannot decode {what}: GStreamer element {factory}: {e}"))
    };
    let (media_type, decoder) = format.decoder();
    let pipeline = gst::Pipeline::with_name("obc-background");
    let src = gst_app::AppSrc::builder()
        .caps(&gst::Caps::new_empty_simple(media_type))
        .format(gst::Format::Bytes)
        .stream_type(gst_app::AppStreamType::Stream)
        .build();
    let decode = make(decoder)?;
    let convert = make("videoconvert")?;
    let bgr = make("capsfilter")?;
    bgr.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "BGR")
            .build(),
    );
    let crop = make("videocrop")?;
    let scale = make("videoscale")?;
    scale.set_property("add-borders", false);
    let out_caps = make("capsfilter")?;
    out_caps.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "BGR")
            .field("width", width as i32)
            .field("height", height as i32)
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build(),
    );
    let sink = gst_app::AppSink::builder()
        .sync(false)
        .max_buffers(1)
        .enable_last_sample(false)
        .build();
    let chain = [
        src.upcast_ref::<gst::Element>(),
        &decode,
        &convert,
        &bgr,
        &crop,
        &scale,
        &out_caps,
        sink.upcast_ref::<gst::Element>(),
    ];
    pipeline
        .add_many(chain)
        .and_then(|()| gst::Element::link_many(chain))
        .map_err(|e| format!("cannot decode {what}: {e}"))?;

    let refused: Arc<Mutex<Option<String>>> = Arc::default();
    {
        let refused = Arc::clone(&refused);
        let crop = crop.clone();
        let what = what.clone();
        convert
            .static_pad("sink")
            .expect("videoconvert has a sink pad")
            .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                let Some(gst::PadProbeData::Event(e)) = &info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                let gst::EventView::Caps(c) = e.view() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(s) = c.caps().structure(0) else {
                    return gst::PadProbeReturn::Ok;
                };
                let (w, h) = (
                    s.get::<i32>("width").unwrap_or(0).max(0) as u32,
                    s.get::<i32>("height").unwrap_or(0).max(0) as u32,
                );
                if let Err(reason) = check_dimensions(&what, w, h) {
                    *refused.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                    return gst::PadProbeReturn::Drop;
                }
                let r = crop_to_fill(w, h, width, height);
                crop.set_property("left", r.left as i32);
                crop.set_property("right", r.right as i32);
                crop.set_property("top", r.top as i32);
                crop.set_property("bottom", r.bottom as i32);
                gst::PadProbeReturn::Ok
            });
    }

    let result = run_decode(&pipeline, &src, data, &sink, &what, &refused);
    if result.is_ok() {
        // The decoder has produced the file's one image: it is idle.
        let _ = pipeline.set_state(gst::State::Null);
    } else {
        stop_in_background(pipeline);
    }
    result
}

/// Stops `pipeline` on a thread of its own. Stopping waits for the
/// streaming thread to leave the decoder, which after a timeout may still be
/// decoding (a decoder cannot be interrupted, and a JPEG of an accepted size
/// can still take long, e.g. one with very many progressive scans); doing it
/// here would block the caller past [`DECODE_TIMEOUT`]. If no thread can be
/// started, it is stopped here.
fn stop_in_background(pipeline: gst::Pipeline) {
    let stop = |p: &gst::Pipeline| {
        let _ = p.set_state(gst::State::Null);
    };
    let spare = pipeline.clone();
    if std::thread::Builder::new()
        .name("obc-bg-stop".into())
        .spawn(move || stop(&pipeline))
        .is_err()
    {
        stop(&spare);
    }
}

fn run_decode(
    pipeline: &gst::Pipeline,
    src: &gst_app::AppSrc,
    data: Vec<u8>,
    sink: &gst_app::AppSink,
    what: &str,
    refused: &Mutex<Option<String>>,
) -> Result<HostBgr, String> {
    let refused_reason = || refused.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let bus_error = || {
        pipeline.bus().and_then(|bus| {
            bus.pop_filtered(&[gst::MessageType::Error])
                .and_then(|m| match m.view() {
                    gst::MessageView::Error(e) => Some(e.error().to_string()),
                    _ => None,
                })
        })
    };
    pipeline.set_state(gst::State::Playing).map_err(|_| {
        refused_reason().unwrap_or_else(|| {
            format!(
                "cannot decode {what}: {}",
                bus_error().unwrap_or_else(|| "the pipeline did not start".into())
            )
        })
    })?;
    // The one image as one buffer, then the end of the stream.
    let _ = src.push_buffer(gst::Buffer::from_mut_slice(data));
    let _ = src.end_of_stream();
    // Wait for the image, an error or the timeout, whichever comes first.
    let deadline = std::time::Instant::now() + DECODE_TIMEOUT;
    let sample = loop {
        if let Some(s) = sink.try_pull_sample(gst::ClockTime::from_mseconds(20)) {
            break s;
        }
        if let Some(r) = refused_reason() {
            return Err(r);
        }
        if let Some(e) = bus_error() {
            return Err(format!("cannot decode {what}: {e}"));
        }
        if sink.is_eos() {
            return Err(format!(
                "cannot decode {what}: the decoder produced no image"
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "cannot decode {what}: no image after {} s",
                DECODE_TIMEOUT.as_secs()
            ));
        }
    };
    let caps = sample
        .caps()
        .ok_or_else(|| format!("cannot decode {what}: no caps"))?;
    let info =
        gst_video::VideoInfo::from_caps(caps).map_err(|e| format!("cannot decode {what}: {e}"))?;
    let buffer = sample
        .buffer()
        .ok_or_else(|| format!("cannot decode {what}: no buffer"))?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|e| format!("cannot decode {what}: {e}"))?;
    let stride = frame.plane_stride()[0] as usize;
    let data = frame
        .plane_data(0)
        .map_err(|e| format!("cannot decode {what}: {e}"))?;
    let image = HostBgr {
        width: info.width(),
        height: info.height(),
        stride,
        data: data.to_vec(),
    };
    image
        .validate()
        .map_err(|e| format!("cannot decode {what}: {e}"))?;
    Ok(image)
}

/// A YUV 4:2:0 image in host memory, tightly packed planes: Y is `width x
/// height`, U and V are `(width + 1) / 2 x (height + 1) / 2` each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yuv420 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Yuv420 {
    /// The width of a chroma plane.
    pub fn chroma_width(&self) -> usize {
        (self.width as usize).div_ceil(2)
    }

    /// The height of a chroma plane.
    pub fn chroma_height(&self) -> usize {
        (self.height as usize).div_ceil(2)
    }

    /// The image as tightly packed NV12 (the Y rows, then the interleaved
    /// U,V rows), as [`obc_gpu`]'s packed uploads take it.
    pub fn nv12_packed(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.y.len() + self.u.len() * 2);
        out.extend_from_slice(&self.y);
        for (u, v) in self.u.iter().zip(&self.v) {
            out.push(*u);
            out.push(*v);
        }
        out
    }
}

/// Converts packed BGR to YUV 4:2:0 in `colour` (its matrix and range):
/// each luma sample from its pixel, each chroma sample from the mean of
/// the 2 x 2 pixels it covers (fewer at an odd edge).
pub fn bgr_to_yuv420(bgr: &HostBgr, colour: ColourSpec) -> Yuv420 {
    let (kr, kb) = match colour.matrix {
        ColourMatrix::Bt601 => (0.299, 0.114),
        ColourMatrix::Bt709 => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    // Y' 0..1 to the luma code, Pb/Pr -0.5..0.5 to the chroma code.
    let (y_off, y_scale, c_scale) = if colour.full_range {
        (0.0, 255.0, 255.0)
    } else {
        (16.0, 219.0, 224.0)
    };
    let code = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    let (w, h) = (bgr.width as usize, bgr.height as usize);
    let rgb = |x: usize, y: usize| {
        let o = y * bgr.stride + x * 3;
        let px = &bgr.data[o..o + 3];
        (
            f64::from(px[2]) / 255.0,
            f64::from(px[1]) / 255.0,
            f64::from(px[0]) / 255.0,
        )
    };
    let luma = |(r, g, b): (f64, f64, f64)| kr * r + kg * g + kb * b;
    let mut out = Yuv420 {
        width: bgr.width,
        height: bgr.height,
        y: Vec::with_capacity(w * h),
        u: Vec::new(),
        v: Vec::new(),
    };
    for y in 0..h {
        for x in 0..w {
            out.y.push(code(y_off + y_scale * luma(rgb(x, y))));
        }
    }
    let (cw, ch) = (out.chroma_width(), out.chroma_height());
    out.u.reserve(cw * ch);
    out.v.reserve(cw * ch);
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut r, mut g, mut b, mut n) = (0.0, 0.0, 0.0, 0.0);
            for y in (2 * cy)..(2 * cy + 2).min(h) {
                for x in (2 * cx)..(2 * cx + 2).min(w) {
                    let p = rgb(x, y);
                    r += p.0;
                    g += p.1;
                    b += p.2;
                    n += 1.0;
                }
            }
            let p = (r / n, g / n, b / n);
            let l = luma(p);
            let pb = (p.2 - l) / (2.0 * (1.0 - kb));
            let pr = (p.0 - l) / (2.0 * (1.0 - kr));
            out.u.push(code(128.0 + c_scale * pb));
            out.v.push(code(128.0 + c_scale * pr));
        }
    }
    out
}

/// The fallback image at `path`, checked and decoded as [`decode`] does
/// (cropped to fill `width x height`, centred), converted to YUV 4:2:0 in
/// `colour`. The reason names "the image file", not its path. Blocks for
/// the decode: run it off the streaming thread.
pub fn decode_yuv420(
    path: &Path,
    width: u32,
    height: u32,
    colour: ColourSpec,
) -> Result<Yuv420, String> {
    decode_as(path, width, height, Subject::FALLBACK).map(|bgr| bgr_to_yuv420(&bgr, colour))
}

/// What tells the file at a path from another one put in its place: its
/// device, inode, size, and modification and change times, from `stat`
/// (which opens nothing, so a FIFO cannot block it). `None` when the path
/// cannot be stat'ed. An image replaced at the same path (written over, or
/// renamed onto it) has another stamp: a holder of its decoded frame that
/// compares stamps reads it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileStamp {
    /// The stamp of the file at `path` now.
    pub fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_replaced_at_the_same_path_has_another_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("away.png");
        assert_eq!(FileStamp::of(&path), None, "no file");
        std::fs::write(&path, b"one").unwrap();
        let first = FileStamp::of(&path).unwrap();
        assert_eq!(FileStamp::of(&path), Some(first), "unchanged");
        // Renamed onto the path (as an editor saves): another inode.
        let next = dir.path().join("next.png");
        std::fs::write(&next, b"two").unwrap();
        std::fs::rename(&next, &path).unwrap();
        let second = FileStamp::of(&path).unwrap();
        assert_ne!(second, first);
        // Written over in place, same size: another change time at least.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"six").unwrap();
        assert_ne!(FileStamp::of(&path), Some(second));
    }

    fn crop(left: u32, right: u32, top: u32, bottom: u32) -> CropRect {
        CropRect {
            left,
            right,
            top,
            bottom,
        }
    }

    #[test]
    fn a_wider_image_loses_its_sides() {
        // 4000x2000 (2:1) into 16:9: keep 3556 columns.
        assert_eq!(crop_to_fill(4000, 2000, 1920, 1080), crop(222, 222, 0, 0));
        // An odd excess puts the extra pixel on the right.
        assert_eq!(crop_to_fill(1921, 1080, 1920, 1080), crop(0, 1, 0, 0));
    }

    #[test]
    fn a_taller_image_loses_top_and_bottom() {
        // 4:3 into 16:9.
        assert_eq!(crop_to_fill(1600, 1200, 1920, 1080), crop(0, 0, 150, 150));
        // Portrait 1080x1920 into 1280x720: keep 608 rows (1080*720/1280 =
        // 607.5, rounded half up).
        assert_eq!(crop_to_fill(1080, 1920, 1280, 720), crop(0, 0, 656, 656));
    }

    #[test]
    fn same_aspect_and_degenerate_sizes_crop_nothing() {
        assert_eq!(crop_to_fill(3840, 2160, 1920, 1080), CropRect::default());
        assert_eq!(crop_to_fill(0, 100, 1920, 1080), CropRect::default());
        assert_eq!(crop_to_fill(100, 100, 0, 1080), CropRect::default());
    }

    #[test]
    fn at_least_one_pixel_is_kept() {
        let r = crop_to_fill(10_000, 1, 1, 10_000);
        assert_eq!(10_000 - r.left - r.right, 1);
        let r = crop_to_fill(1, 10_000, 10_000, 1);
        assert_eq!(10_000 - r.top - r.bottom, 1);
    }

    #[test]
    fn the_kept_region_has_the_target_aspect() {
        for (sw, sh) in [(4032, 3024), (3000, 4000), (1920, 1200), (800, 600)] {
            for (dw, dh) in [(1920, 1080), (1280, 720), (640, 480)] {
                let r = crop_to_fill(sw, sh, dw, dh);
                let (kw, kh) = (sw - r.left - r.right, sh - r.top - r.bottom);
                let err = (kw as f64 / kh as f64 - dw as f64 / dh as f64).abs();
                assert!(err < 0.01, "{sw}x{sh} -> {dw}x{dh}: kept {kw}x{kh}");
                assert!(r.left.abs_diff(r.right) <= 1 && r.top.abs_diff(r.bottom) <= 1);
            }
        }
    }

    #[test]
    fn files_are_checked_before_decoding() {
        let rel = check_file(Path::new("relative.png")).unwrap_err();
        assert!(rel.contains("not absolute"), "{rel}");
        let missing = check_file(Path::new("/nonexistent/bg.png")).unwrap_err();
        assert!(missing.contains("unreadable"), "{missing}");
        let dir = tempfile::tempdir().unwrap();
        let not_file = check_file(dir.path()).unwrap_err();
        assert!(not_file.contains("not a regular file"), "{not_file}");
        let big = dir.path().join("big.png");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let e = check_file(&big).unwrap_err();
        assert!(e.contains("32 MiB"), "{e}");
        let ok = dir.path().join("ok.png");
        std::fs::write(&ok, b"x").unwrap();
        assert_eq!(check_file(&ok).unwrap(), 1);
        assert_eq!(read_file(&ok).unwrap(), b"x");
        assert!(read_file(&big).unwrap_err().contains("32 MiB"));
    }

    /// A PNG signature and IHDR chunk declaring `w` x `h` (CRC not checked).
    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    /// SOI, an APP0 and a DHT segment, fill bytes, then the frame header
    /// `marker` declaring `w` x `h`.
    fn jpeg_header(marker: u8, w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x07, b'J', b'F', b'I', b'F', 0]);
        v.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x03, 0x00]);
        v.extend_from_slice(&[0xFF, 0xFF, marker, 0x00, 0x0B, 8]);
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[1, 1, 0x11, 0]);
        v
    }

    #[test]
    fn the_size_is_read_from_the_header() {
        assert_eq!(
            image_header(&png_header(60_000, 3)).unwrap(),
            (ImageFormat::Png, 60_000, 3)
        );
        // Baseline and progressive JPEG, past APP0 and the DHT (0xC4, in
        // the SOF range but not a frame header).
        for marker in [0xC0, 0xC2] {
            assert_eq!(
                image_header(&jpeg_header(marker, 65_000, 480)).unwrap(),
                (ImageFormat::Jpeg, 65_000, 480)
            );
        }
    }

    #[test]
    fn a_header_that_cannot_be_read_is_a_reason() {
        let other = image_header(b"GIF89a....").unwrap_err();
        assert!(other.contains("neither a PNG nor a JPEG"), "{other}");
        assert!(image_header(&[]).is_err());
        let png = png_header(10, 10);
        let e = image_header(&png[..20]).unwrap_err();
        assert!(e.contains("IHDR"), "{e}");
        let jpeg = jpeg_header(0xC0, 10, 10);
        for cut in 2..jpeg.len() - 4 {
            let e = image_header(&jpeg[..cut]).unwrap_err();
            assert!(e.contains("without a frame header"), "cut {cut}: {e}");
        }
        // The scan or the end of the image before any frame header.
        for marker in [0xDA, 0xD9] {
            let e = image_header(&[0xFF, 0xD8, 0xFF, marker, 0, 2]).unwrap_err();
            assert!(e.contains("without a frame header"), "{e}");
        }
        // A segment length below 2 would loop forever.
        assert!(image_header(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]).is_err());
    }

    /// A PNG chunk of `kind` with `data` (CRC not computed).
    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0; 4]);
        v
    }

    /// `png_header` (signature and IHDR), IDAT, an ancillary tEXt, IEND.
    fn png_file() -> Vec<u8> {
        let mut v = png_header(4, 4);
        v.extend(png_chunk(b"IDAT", &[1, 2, 3]));
        v.extend(png_chunk(b"tEXt", b"k\0v"));
        v.extend(png_chunk(b"IEND", &[]));
        v
    }

    /// `jpeg_header` (SOI, APP0, DHT, SOF0), then two scans, each with
    /// stuffed bytes, a restart marker and fill bytes before the next
    /// marker (a DHT between the scans, as in a progressive JPEG), then EOI.
    fn jpeg_file() -> Vec<u8> {
        let mut v = jpeg_header(0xC0, 4, 4);
        let sos = [0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 63, 0];
        v.extend_from_slice(&sos);
        v.extend_from_slice(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD3, 0x56, 0xFF, 0xFF]);
        v.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x03, 0x00]);
        v.extend_from_slice(&sos);
        v.extend_from_slice(&[0xFF, 0x00, 0xFF, 0xD7, 0x00, 0xFF, 0xD9]);
        v
    }

    #[test]
    fn the_first_image_ends_at_iend_or_eoi() {
        let png = png_file();
        assert_eq!(image_length(ImageFormat::Png, &png), Ok(png.len()));
        let jpeg = jpeg_file();
        assert_eq!(image_length(ImageFormat::Jpeg, &jpeg), Ok(jpeg.len()));
        // A second image, or anything else, after the first is not part of
        // it.
        for (format, image) in [(ImageFormat::Png, &png), (ImageFormat::Jpeg, &jpeg)] {
            let mut two = image.clone();
            two.extend_from_slice(image);
            assert_eq!(image_length(format, &two), Ok(image.len()));
            two.truncate(image.len() + 1);
            assert_eq!(image_length(format, &two), Ok(image.len()));
        }
        // A JPEG of nothing but markers, without a scan.
        assert_eq!(
            image_length(ImageFormat::Jpeg, &[0xFF, 0xD8, 0xFF, 0xFF, 0xD9, 1]),
            Ok(5)
        );
    }

    #[test]
    fn an_image_that_ends_early_is_refused() {
        for (format, image, want) in [
            (ImageFormat::Png, png_file(), "before its IEND"),
            (ImageFormat::Jpeg, jpeg_file(), "before its end-of-image"),
        ] {
            for cut in 0..image.len() {
                let e = image_length(format, &image[..cut]).unwrap_err();
                // A cut inside the PNG signature or the JPEG SOI is not a
                // PNG or JPEG file at all.
                assert!(
                    e.contains(want) || e.contains("invalid marker"),
                    "cut {cut}: {e}"
                );
            }
        }
        // A chunk length beyond 2^31 - 1, or one past the end of the file.
        let mut png = png_header(4, 4);
        png.extend_from_slice(&0x8000_0000u32.to_be_bytes());
        png.extend_from_slice(b"IDAT");
        let e = image_length(ImageFormat::Png, &png).unwrap_err();
        assert!(e.contains("invalid chunk length"), "{e}");
        let at = png.len() - 8;
        png[at..at + 4].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
        let e = image_length(ImageFormat::Png, &png).unwrap_err();
        assert!(e.contains("before its IEND"), "{e}");
    }

    #[test]
    fn a_jpeg_with_an_invalid_marker_is_refused() {
        for bytes in [
            // A second SOI, a stuffed byte where a marker belongs, no
            // marker after the SOI, a segment length below 2.
            &[0xFF, 0xD8, 0xFF, 0xD8, 0xFF, 0xD9][..],
            &[0xFF, 0xD8, 0xFF, 0x00, 0xFF, 0xD9],
            &[0xFF, 0xD8, 0x12, 0xFF, 0xD9],
            &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x01, 0xFF, 0xD9],
            &[0x89, b'P', b'N', b'G'],
        ] {
            let e = image_length(ImageFormat::Jpeg, bytes).unwrap_err();
            assert!(e.contains("invalid marker"), "{bytes:02X?}: {e}");
        }
        // A SOI within the scan data ends the scan, and is refused.
        let mut jpeg = jpeg_header(0xC0, 4, 4);
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x55, 0xFF, 0xD8, 0xFF, 0xD9]);
        assert!(image_length(ImageFormat::Jpeg, &jpeg).is_err());
    }

    /// Hostile input never panics or loops: every single-byte change of
    /// each sample, at every position, through both parsers.
    #[test]
    fn the_parsers_survive_any_single_byte_change() {
        for image in [png_file(), jpeg_file()] {
            for at in 0..image.len() {
                for byte in [0x00, 0x01, 0x7F, 0x80, 0xD8, 0xD9, 0xDA, 0xFE, 0xFF] {
                    let mut v = image.clone();
                    v[at] = byte;
                    let _ = image_header(&v);
                    for format in [ImageFormat::Png, ImageFormat::Jpeg] {
                        if let Ok(len) = image_length(format, &v) {
                            assert!(len <= v.len());
                        }
                    }
                }
            }
        }
    }

    fn solid(w: u32, h: u32, bgr: [u8; 3]) -> HostBgr {
        HostBgr {
            width: w,
            height: h,
            stride: w as usize * 3,
            data: bgr.repeat((w * h) as usize),
        }
    }

    #[test]
    fn bgr_converts_to_yuv_in_the_range_and_matrix_asked() {
        let limited = ColourSpec::LIMITED_BT601;
        let full = ColourSpec::FULL_BT601;
        let bt709 = ColourSpec {
            matrix: ColourMatrix::Bt709,
            full_range: false,
        };
        let one = |bgr: [u8; 3], c: ColourSpec| {
            let y = bgr_to_yuv420(&solid(4, 2, bgr), c);
            assert_eq!((y.y.len(), y.u.len(), y.v.len()), (8, 2, 2));
            (y.y[0], y.u[0], y.v[0])
        };
        // Black and white: the range's ends, chroma neutral.
        assert_eq!(one([0, 0, 0], limited), (16, 128, 128));
        assert_eq!(one([255, 255, 255], limited), (235, 128, 128));
        assert_eq!(one([0, 0, 0], full), (0, 128, 128));
        assert_eq!(one([255, 255, 255], full), (255, 128, 128));
        // Pure red, BT.601 limited: Y 81, Cb 90, Cr 240 (the textbook
        // values); BT.709: Y 63, Cb 102, Cr 240.
        assert_eq!(one([0, 0, 255], limited), (81, 90, 240));
        assert_eq!(one([0, 0, 255], bt709), (63, 102, 240));
        // Pure blue, BT.601 limited: Y 41, Cb 240, Cr 110.
        assert_eq!(one([255, 0, 0], limited), (41, 240, 110));
    }

    #[test]
    fn chroma_is_the_mean_of_its_pixels_and_nv12_interleaves_it() {
        // Left half black, right half white, 4x2: one chroma sample each
        // half, both neutral; an odd width keeps its last column.
        let mut img = solid(4, 2, [0, 0, 0]);
        for y in 0..2 {
            for x in 2..4 {
                let o = y * 12 + x * 3;
                img.data[o..o + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let yuv = bgr_to_yuv420(&img, ColourSpec::LIMITED_BT601);
        assert_eq!(yuv.y, [16, 16, 235, 235, 16, 16, 235, 235]);
        assert_eq!(
            (yuv.u.as_slice(), yuv.v.as_slice()),
            (&[128, 128][..], &[128, 128][..])
        );
        assert_eq!(
            yuv.nv12_packed(),
            [16, 16, 235, 235, 16, 16, 235, 235, 128, 128, 128, 128]
        );
        let odd = bgr_to_yuv420(&solid(3, 3, [0, 0, 255]), ColourSpec::LIMITED_BT601);
        assert_eq!((odd.chroma_width(), odd.chroma_height()), (2, 2));
        assert_eq!(odd.u, [90; 4]);
    }

    #[test]
    fn dimensions_outside_the_limit_are_refused() {
        assert!(check_dimensions("x", MAX_DIMENSION, MAX_DIMENSION).is_ok());
        for (w, h) in [
            (MAX_DIMENSION + 1, 1),
            (1, MAX_DIMENSION + 1),
            (0, 5),
            (5, 0),
        ] {
            let e = check_dimensions("x", w, h).unwrap_err();
            assert!(
                e.contains(&format!("{w}x{h}")) && e.contains("8192x8192"),
                "{e}"
            );
        }
    }
}
