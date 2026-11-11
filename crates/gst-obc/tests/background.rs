//! Decoding the replace effect's background image with GStreamer's CPU
//! elements (core, base and good: `pngenc`/`pngdec`, `jpegenc`/`jpegdec`,
//! `videoconvert`, `videocrop`, `videoscale`). No GPU.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gst_obc::background::{MAX_DIMENSION, decode};

/// Writes an RGB image of `width` x `height` to `path` as PNG, with each
/// pixel's colour from `colour(x, y)`.
fn write_png(path: &Path, width: u32, height: u32, colour: impl Fn(u32, u32) -> [u8; 3]) {
    write_image(path, "pngenc", width, height, colour);
}

/// Writes an RGB image with the GStreamer image encoder `encoder`.
fn write_image(
    path: &Path,
    encoder: &str,
    width: u32,
    height: u32,
    colour: impl Fn(u32, u32) -> [u8; 3],
) {
    gst::init().unwrap();
    let stride = (width as usize * 3 + 3) & !3;
    let mut data = vec![0u8; stride * height as usize];
    for y in 0..height {
        for x in 0..width {
            let o = y as usize * stride + x as usize * 3;
            data[o..o + 3].copy_from_slice(&colour(x, y));
        }
    }
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGB")
        .field("width", width as i32)
        .field("height", height as i32)
        .field("framerate", gst::Fraction::new(0, 1))
        .build();
    let src = gst_app::AppSrc::builder()
        .caps(&caps)
        .format(gst::Format::Time)
        .build();
    let enc = gst::ElementFactory::make(encoder)
        .property("snapshot", true)
        .build()
        .unwrap_or_else(|e| panic!("{encoder} (gst-plugins-good): {e}"));
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    let p = gst::Pipeline::new();
    p.add_many([src.upcast_ref(), &enc, &sink]).unwrap();
    gst::Element::link_many([src.upcast_ref(), &enc, &sink]).unwrap();
    p.set_state(gst::State::Playing).unwrap();
    let mut buffer = gst::Buffer::from_mut_slice(data);
    buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    src.push_buffer(buffer).unwrap();
    src.end_of_stream().unwrap();
    let msg = p
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("the PNG was written");
    assert_eq!(msg.type_(), gst::MessageType::Eos, "{msg:?}");
    p.set_state(gst::State::Null).unwrap();
}

fn pixel(img: &obc_backend::HostBgr, x: u32, y: u32) -> [u8; 3] {
    let o = y as usize * img.stride + x as usize * 3;
    [img.data[o], img.data[o + 1], img.data[o + 2]]
}

fn tmp(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

#[test]
fn a_wide_image_is_cropped_to_its_centre_and_scaled() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "bands.png");
    // 400x100: red | green (the middle 200 columns) | blue.
    write_png(&path, 400, 100, |x, _| match x {
        0..100 => [255, 0, 0],
        100..300 => [0, 255, 0],
        _ => [0, 0, 255],
    });
    // Into 160x80 (2:1): the kept region is the middle 200x100 columns,
    // all green.
    let img = decode(&path, 160, 80).expect("decodes");
    assert_eq!((img.width, img.height), (160, 80));
    assert!(img.stride >= 160 * 3);
    img.validate().unwrap();
    for (x, y) in [(0, 0), (159, 0), (80, 40), (0, 79), (159, 79)] {
        let [b, g, r] = pixel(&img, x, y);
        assert!(g > 200 && b < 40 && r < 40, "({x},{y}) is BGR {b},{g},{r}");
    }
}

#[test]
fn a_tall_image_loses_top_and_bottom() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "rows.png");
    // 100x400: blue on top, white in the middle 100 rows... into 1:1.
    write_png(&path, 100, 400, |_, y| match y {
        0..150 => [0, 0, 255],
        150..250 => [255, 255, 255],
        _ => [255, 0, 0],
    });
    let img = decode(&path, 64, 64).expect("decodes");
    assert_eq!((img.width, img.height), (64, 64));
    for (x, y) in [(0, 0), (63, 63), (32, 32)] {
        let [b, g, r] = pixel(&img, x, y);
        assert!(
            b > 200 && g > 200 && r > 200,
            "({x},{y}) is BGR {b},{g},{r}"
        );
    }
}

#[test]
fn an_image_larger_than_the_limit_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "huge.png");
    write_png(&path, MAX_DIMENSION + 8, 2, |_, _| [9, 9, 9]);
    let e = decode(&path, 640, 360).unwrap_err();
    assert!(e.contains("8200x2") && e.contains("8192x8192"), "{e}");
}

#[test]
fn an_undecodable_or_missing_file_is_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "garbage.png");
    std::fs::write(&path, b"this is not an image").unwrap();
    let e = decode(&path, 640, 360).unwrap_err();
    assert!(
        e.starts_with("cannot decode the background image") && e.contains("garbage.png"),
        "{e}"
    );
    let e = decode(Path::new("/nonexistent/bg.png"), 640, 360).unwrap_err();
    assert!(e.contains("unreadable"), "{e}");
}

#[test]
fn a_jpeg_image_is_decoded() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "bands.jpg");
    write_image(&path, "jpegenc", 400, 100, |x, _| match x {
        0..100 => [255, 0, 0],
        100..300 => [0, 255, 0],
        _ => [0, 0, 255],
    });
    let img = decode(&path, 160, 80).expect("decodes");
    assert_eq!((img.width, img.height), (160, 80));
    let [b, g, r] = pixel(&img, 80, 40);
    assert!(g > 200 && b < 60 && r < 60, "centre is BGR {b},{g},{r}");
}

/// The decode must be refused from the header: no decoder runs, so it
/// returns in far less than any decode of the declared size would take
/// (allocating 60000 x 60000 RGB alone is about 10 GB).
fn assert_refused_quickly(path: &Path, want: &str) {
    let e = refused_quickly(path);
    assert!(e.contains(want) && e.contains("8192x8192"), "{e}");
}

/// Decodes `path`, which must be refused within 500 ms; returns the reason.
fn refused_quickly(path: &Path) -> String {
    let started = Instant::now();
    let e = decode(path, 640, 360).unwrap_err();
    let took = started.elapsed();
    assert!(took < Duration::from_millis(500), "took {took:?}: {e}");
    e
}

#[test]
fn a_small_file_declaring_a_huge_image_is_refused_before_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "bomb.png");
    std::fs::write(&path, png_bomb()).unwrap();
    assert_refused_quickly(&path, "60000x60000");
    let path = tmp(&dir, "bomb.jpg");
    std::fs::write(&path, jpeg_bomb()).unwrap();
    assert_refused_quickly(&path, "65000x65000");
}

/// A PNG: signature, then an IHDR chunk declaring 60000x60000 (8-bit RGB),
/// then an empty IEND; about 50 bytes.
fn png_bomb() -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(png_chunk(
        b"IHDR",
        &[0, 0, 0xEA, 0x60, 0, 0, 0xEA, 0x60, 8, 2, 0, 0, 0],
    ));
    png.extend(png_chunk(b"IEND", &[]));
    png
}

/// A JPEG: SOI, a baseline frame header declaring 65000x65000, EOI.
fn jpeg_bomb() -> Vec<u8> {
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 8];
    jpeg.extend_from_slice(&65_000u16.to_be_bytes());
    jpeg.extend_from_slice(&65_000u16.to_be_bytes());
    jpeg.extend_from_slice(&[1, 1, 0x11, 0, 0xFF, 0xD9]);
    jpeg
}

/// The CRC-32 of PNG chunks (ISO 3309, as in zlib).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A PNG chunk of `kind` holding `data`, with its CRC.
fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = (data.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(data);
    v.extend_from_slice(&crc32(&v[4..]).to_be_bytes());
    v
}

/// A small valid image written by `encoder`, as bytes.
fn small_image(dir: &tempfile::TempDir, encoder: &str) -> Vec<u8> {
    let path = tmp(dir, &format!("small-{encoder}"));
    write_image(&path, encoder, 16, 16, |x, y| {
        [x as u8 * 16, y as u8 * 16, 0]
    });
    std::fs::read(&path).unwrap()
}

/// A small valid image followed by `trailing`, decoded to 32x32 within
/// 500 ms; the result must be the small image's content (its red channel
/// rises along x, its green channel along y), whatever follows.
fn assert_decodes_ignoring(dir: &tempfile::TempDir, encoder: &str, trailing: &[u8], name: &str) {
    let mut file = small_image(dir, encoder);
    file.extend_from_slice(trailing);
    let path = tmp(dir, name);
    std::fs::write(&path, &file).unwrap();
    let started = Instant::now();
    let img = decode(&path, 32, 32).expect("decodes");
    let took = started.elapsed();
    assert!(took < Duration::from_millis(500), "took {took:?}");
    assert_eq!((img.width, img.height), (32, 32));
    // The 16x16 source is [x * 16, y * 16, 0] (RGB); in BGR the blue
    // channel is 0 throughout, red rises to the right, green downwards.
    let [b, g, r] = pixel(&img, 1, 1);
    let [b2, g2, r2] = pixel(&img, 30, 30);
    assert!(b < 40 && b2 < 40, "blue is {b}, {b2}");
    assert!(
        r2 > r + 100 && g2 > g + 100,
        "BGR {b},{g},{r} to {b2},{g2},{r2}"
    );
}

/// pngdec and jpegdec decode every image in their input, and only the
/// first one's header is read before decoding: a second image after the
/// first, declaring 60000x60000 pixels, never reaches the decoder, so the
/// file decodes to the first image, quickly.
#[test]
fn a_second_huge_image_is_never_decoded() {
    let dir = tempfile::tempdir().unwrap();
    for (encoder, bomb, name) in [
        ("pngenc", png_bomb(), "two.png"),
        ("jpegenc", jpeg_bomb(), "two.jpg"),
    ] {
        assert_decodes_ignoring(&dir, encoder, &bomb, name);
    }
    // A second PNG that is itself cut short (no IEND): still ignored.
    let mut cut_bomb = png_bomb();
    cut_bomb.truncate(cut_bomb.len() - 12);
    assert_decodes_ignoring(&dir, "pngenc", &cut_bomb, "two-cut.png");
}

/// Phone photos carry data after the image (an Ultra HDR gain map, a
/// Motion Photo's video); padding is common too. All of it is ignored.
#[test]
fn data_after_the_image_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let mut mp4 = vec![0, 0, 0, 24];
    mp4.extend_from_slice(b"ftypisom");
    mp4.extend_from_slice(&[0, 0, 2, 0]);
    mp4.extend_from_slice(b"isomiso2mp41");
    let trailers: [(&str, Vec<u8>); 3] = [
        ("0xAB", vec![0xAB; 4096]),
        ("ftyp", mp4),
        ("zeros", vec![0; 16]),
    ];
    for (encoder, ext) in [("pngenc", "png"), ("jpegenc", "jpg")] {
        for (label, trailing) in &trailers {
            assert_decodes_ignoring(&dir, encoder, trailing, &format!("{label}.{ext}"));
        }
    }
}

#[test]
fn a_truncated_image_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for (encoder, want, name) in [
        ("pngenc", "before its IEND chunk", "cut.png"),
        ("jpegenc", "before its end-of-image marker", "cut.jpg"),
    ] {
        let small = small_image(&dir, encoder);
        let path = tmp(&dir, name);
        for cut in [1, 2, small.len() / 2] {
            std::fs::write(&path, &small[..small.len() - cut]).unwrap();
            let e = refused_quickly(&path);
            assert!(e.contains(want) && e.contains(name), "cut {cut}: {e}");
        }
    }
}

#[test]
fn a_png_with_an_ancillary_chunk_after_its_data_is_decoded() {
    let dir = tempfile::tempdir().unwrap();
    let mut png = small_image(&dir, "pngenc");
    // Before IEND (the last 12 bytes), after IDAT.
    let iend = png.len() - 12;
    assert_eq!(&png[iend + 4..iend + 8], b"IEND");
    png.splice(
        iend..iend,
        png_chunk(b"tEXt", b"Comment\0omarchy-broadcast"),
    );
    let path = tmp(&dir, "text.png");
    std::fs::write(&path, &png).unwrap();
    let img = decode(&path, 32, 32).expect("decodes");
    assert_eq!((img.width, img.height), (32, 32));
}

/// A 16x16 greyscale baseline JPEG with a restart interval of one MCU, so
/// its scan holds three restart markers (RST0 to RST2) and a stuffed byte
/// (`FF 00`). Generated for this test with libjpeg-turbo's `cjpeg
/// -grayscale -quality 90 -restart 1B` from 16x16 pseudo-random grey
/// levels (Python `random.Random(7)`).
const JPEG_WITH_RESTARTS: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x03, 0x02, 0x02, 0x03, 0x02, 0x02, 0x03,
    0x03, 0x03, 0x03, 0x04, 0x03, 0x03, 0x04, 0x05, 0x08, 0x05, 0x05, 0x04, 0x04, 0x05, 0x0A, 0x07,
    0x07, 0x06, 0x08, 0x0C, 0x0A, 0x0C, 0x0C, 0x0B, 0x0A, 0x0B, 0x0B, 0x0D, 0x0E, 0x12, 0x10, 0x0D,
    0x0E, 0x11, 0x0E, 0x0B, 0x0B, 0x10, 0x16, 0x10, 0x11, 0x13, 0x14, 0x15, 0x15, 0x15, 0x0C, 0x0F,
    0x17, 0x18, 0x16, 0x14, 0x18, 0x12, 0x14, 0x15, 0x14, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10,
    0x00, 0x10, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04,
    0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10, 0x00, 0x02, 0x01, 0x03,
    0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00, 0x01, 0x7D, 0x01, 0x02, 0x03, 0x00,
    0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32,
    0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72,
    0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35,
    0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55,
    0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75,
    0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94,
    0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2,
    0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9,
    0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6,
    0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDD,
    0x00, 0x04, 0x00, 0x01, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xCF, 0xD4,
    0x7C, 0x17, 0xE1, 0xF1, 0xE1, 0x4B, 0x2F, 0x14, 0xEA, 0x37, 0xEB, 0xA9, 0x78, 0xD2, 0xEA, 0xC6,
    0xF6, 0x54, 0x8E, 0xF7, 0x50, 0xB6, 0x87, 0x44, 0xB8, 0x8D, 0xF5, 0x17, 0x53, 0x05, 0xC4, 0xC4,
    0xAC, 0x90, 0xF9, 0xD3, 0x79, 0x90, 0x88, 0xA0, 0x68, 0x77, 0x19, 0x5D, 0x24, 0x4B, 0x40, 0x6E,
    0x80, 0xFF, 0xD0, 0xD3, 0x6F, 0x0E, 0x68, 0x82, 0xDF, 0x5C, 0xF8, 0x6B, 0x71, 0xE1, 0xC7, 0xD1,
    0xAE, 0x74, 0xBD, 0x3A, 0x6D, 0x6B, 0x4C, 0xB4, 0xD3, 0x6D, 0xEF, 0x2F, 0x6E, 0x2F, 0xAF, 0xAE,
    0x2D, 0xE2, 0x8A, 0xE2, 0xE2, 0x78, 0xB7, 0xCB, 0x1B, 0x2A, 0x3C, 0x92, 0xC0, 0xA6, 0x59, 0x43,
    0x37, 0x99, 0x18, 0x69, 0x62, 0xC3, 0xBB, 0x7F, 0xFF, 0xD1, 0xAB, 0xA9, 0x5C, 0xE8, 0x3E, 0x28,
    0xF1, 0x7F, 0x87, 0xB4, 0xCD, 0x7E, 0xF2, 0xF8, 0x5F, 0x6A, 0x91, 0xAD, 0x9D, 0xDE, 0x85, 0x01,
    0xB9, 0xBC, 0xD2, 0xC5, 0xDC, 0xB6, 0xA5, 0x1F, 0x51, 0x0D, 0x77, 0x23, 0x6F, 0x80, 0x2D, 0xD4,
    0x31, 0x0B, 0xC6, 0x94, 0x48, 0x04, 0x2A, 0xB1, 0x87, 0x81, 0xA1, 0x63, 0xFF, 0xD2, 0x5F, 0x11,
    0xDA, 0x69, 0xF7, 0x7A, 0xF6, 0x8D, 0x6B, 0xE2, 0x08, 0xAD, 0xE7, 0xBA, 0xD6, 0xE4, 0x47, 0xF0,
    0xF4, 0x56, 0xFA, 0x07, 0xF6, 0xC4, 0x01, 0xFE, 0xCE, 0xA5, 0x22, 0xBA, 0xB5, 0x16, 0x92, 0x19,
    0xA0, 0x8A, 0xD1, 0xCC, 0x66, 0x19, 0x1A, 0x6B, 0x88, 0xA4, 0x63, 0xB6, 0x58, 0xA3, 0x6F, 0x38,
    0xFF, 0x00, 0xFF, 0xD9,
];

#[test]
fn a_jpeg_with_restart_markers_and_stuffed_bytes_is_decoded() {
    let jpeg = JPEG_WITH_RESTARTS;
    let scan = jpeg
        .windows(2)
        .position(|w| w == [0xFF, 0xDA])
        .expect("a scan");
    let data = &jpeg[scan..];
    assert!(data.windows(2).any(|w| w == [0xFF, 0x00]), "a stuffed byte");
    for rst in 0xD0..=0xD2u8 {
        assert!(
            data.windows(2).any(|w| w == [0xFF, rst]),
            "RST{}",
            rst - 0xD0
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "restarts.jpg");
    std::fs::write(&path, jpeg).unwrap();
    let img = decode(&path, 32, 32).expect("decodes");
    assert_eq!((img.width, img.height), (32, 32));
}

#[test]
fn a_fifo_is_refused_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "bg.png");
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
    // Run on another thread so a regression fails the test instead of
    // hanging it: opening a FIFO for reading blocks until a writer comes.
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let _ = tx.send((gst_obc::background::check_file(&p), decode(&p, 640, 360)));
    });
    let (checked, decoded) = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the FIFO was refused within 2 s, without blocking on open");
    for e in [checked.unwrap_err(), decoded.unwrap_err()] {
        assert!(
            e.contains("not a regular file") && e.contains("bg.png"),
            "{e}"
        );
    }
}

#[test]
fn the_fallback_image_decodes_to_yuv_in_the_colour_asked() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp(&dir, "away.png");
    // Red on the left half, blue on the right, 64x32 into 32x16.
    write_png(&path, 64, 32, |x, _| {
        if x < 32 { [255, 0, 0] } else { [0, 0, 255] }
    });
    let img =
        gst_obc::background::decode_yuv420(&path, 32, 16, obc_backend::ColourSpec::LIMITED_BT601)
            .unwrap();
    assert_eq!((img.width, img.height), (32, 16));
    assert_eq!((img.y.len(), img.u.len(), img.v.len()), (512, 128, 128));
    // Away from the seam the scaler leaves the colours alone: BT.601
    // limited red (81, 90, 240) and blue (41, 240, 110).
    let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
    let (y0, y1) = (img.y[16 * 2], img.y[16 * 2 + 31]);
    assert!(near(y0, 81) && near(y1, 41), "{y0} {y1}");
    let (u0, v0) = (img.u[0], img.v[0]);
    assert!(near(u0, 90) && near(v0, 240), "{u0} {v0}");
    assert_eq!(img.nv12_packed().len(), 512 + 256);
    // The reason names the file, not the path (the caller does).
    let missing = tmp(&dir, "gone.png");
    let e = gst_obc::background::decode_yuv420(
        &missing,
        32,
        16,
        obc_backend::ColourSpec::LIMITED_BT601,
    )
    .unwrap_err();
    assert!(e.starts_with("the image file is unreadable: "), "{e}");
    assert_eq!(
        gst_obc::fallback_unreadable(&missing, &e),
        format!(
            "Fallback image {} could not be read ({e}): showing black",
            missing.display()
        )
    );
}
