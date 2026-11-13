//! Tests of the video side of the gallery: the hash, the check of videos,
//! the import of videos and animated images, posters, the ffmpeg runner
//! and the playback copies.
//!
//! Clips are made with the ffmpeg of `PATH` (lavfi `testsrc2`, a second
//! or less); without it, or without an encoder a test needs, the test
//! returns early and says so on stderr. The runner and the copy jobs are
//! also driven by fake ffmpeg scripts that record their arguments and
//! their niceness.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use super::ffmpeg::{self, ENCODE_SETTINGS_VERSION, Ffmpeg, FfmpegError, FfmpegJob};
use super::hash::{self, Sha256};
use super::playback::{CopyEvent, CopyState, PlaybackCopies, is_cpu_ready};
use super::probe::{Codec, Container, MAX_VIDEO_BYTES, VideoInfo, probe_video};
use super::*;

const EVENT_WAIT: Duration = Duration::from_secs(60);

/// A temporary root with a store that has a cache.
struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    store: MediaStore,
    src: PathBuf,
}

fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let store = MediaStore::new(root.join("bundled"), root.join("user"))
        .with_cache(MediaCache::new(root.join("cache")));
    Fx {
        _tmp: tmp,
        root,
        store,
        src,
    }
}

fn real_ffmpeg() -> Option<Ffmpeg> {
    let f = Ffmpeg::locate().ok();
    if f.is_none() {
        eprintln!("skipped: ffmpeg is not installed");
    }
    f
}

fn has_encoder(name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .any(|w| w == name)
        })
}

/// Makes `out` with ffmpeg from a `testsrc2` of `size`, `rate` and
/// `secs`, with `args` (codec, format) after the input; `None` (and a note)
/// when ffmpeg is missing or fails.
fn clip(out: &Path, size: &str, rate: u32, secs: f32, args: &[&str]) -> Option<PathBuf> {
    real_ffmpeg()?;
    let src = format!("testsrc2=s={size}:r={rate}:d={secs}");
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", &src])
        .args(args)
        .arg(out)
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("skipped: ffmpeg could not make {}", out.display());
        return None;
    }
    Some(out.to_path_buf())
}

fn h264(out: &Path, size: &str, rate: u32) -> Option<PathBuf> {
    clip(
        out,
        size,
        rate,
        1.0,
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ],
    )
}

fn vp9(out: &Path, size: &str, rate: u32) -> Option<PathBuf> {
    clip(
        out,
        size,
        rate,
        1.0,
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "webm",
        ],
    )
}

fn info(
    container: Container,
    codec: Codec,
    profile: &str,
    (width, height): (u32, u32),
    (fps_num, fps_den): (u32, u32),
) -> VideoInfo {
    VideoInfo {
        container,
        codec,
        profile: profile.into(),
        width,
        height,
        fps_num,
        fps_den,
        duration_ms: 1000,
    }
}

// --- the hash ---------------------------------------------------------

#[test]
fn sha256_matches_the_standard_vectors() {
    let hex = |d: &[u8]| hash::to_hex(&hash::sha256(d));
    assert_eq!(
        hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // A million 'a', fed in uneven pieces.
    let mut s = Sha256::new();
    let a = vec![b'a'; 1_000_000];
    for piece in a.chunks(997) {
        s.update(piece);
    }
    assert_eq!(
        hash::to_hex(&s.finish()),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
    assert!(hash::is_hash(&hex(b"abc")));
    assert!(!hash::is_hash("abc"));
    assert!(!hash::is_hash(&hex(b"abc").to_uppercase()));
}

#[test]
fn a_file_hash_is_its_content_hash_and_is_bounded() {
    let f = fx();
    let p = f.src.join("data.bin");
    std::fs::write(&p, b"abc").unwrap();
    let mut file = File::open(&p).unwrap();
    assert_eq!(
        hash::hash_file(&mut file, 3, None).unwrap(),
        hash::to_hex(&hash::sha256(b"abc"))
    );
    let q = f.src.join("more.bin");
    std::fs::write(&q, b"abcd").unwrap();
    let mut file = File::open(&q).unwrap();
    assert!(hash::hash_file(&mut file, 3, None).is_err());
    let cancel = AtomicBool::new(true);
    let r = f.src.join("other.bin");
    std::fs::write(&r, b"xyz").unwrap();
    let mut file = File::open(&r).unwrap();
    assert_eq!(
        hash::hash_file(&mut file, 3, Some(&cancel)).unwrap_err(),
        "cancelled"
    );
}

// --- what is animated -------------------------------------------------

#[test]
fn animated_images_are_told_from_stills() {
    assert_eq!(animated_format(b"GIF89a\x01\x00"), Some(Animated::Gif));
    assert_eq!(animated_format(b"GIF87a\x01\x00"), Some(Animated::Gif));
    let chunk = |kind: &[u8; 4], data: &[u8]| {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0; 4]);
        v
    };
    let png = |chunks: &[Vec<u8>]| {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend(chunks.concat());
        v
    };
    let ihdr = chunk(b"IHDR", &[0; 13]);
    let actl = chunk(b"acTL", &[0; 8]);
    let idat = chunk(b"IDAT", &[1, 2, 3]);
    let iend = chunk(b"IEND", &[]);
    assert_eq!(
        animated_format(&png(&[
            ihdr.clone(),
            actl.clone(),
            idat.clone(),
            iend.clone()
        ])),
        Some(Animated::Apng)
    );
    assert_eq!(
        animated_format(&png(&[ihdr.clone(), idat.clone(), iend.clone()])),
        None
    );
    // An acTL after the image data is not APNG.
    assert_eq!(animated_format(&png(&[ihdr, idat, actl, iend])), None);

    let riff = |kind: &[u8; 4], data: &[u8]| {
        let mut v = kind.to_vec();
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        if data.len() % 2 == 1 {
            v.push(0);
        }
        v
    };
    let webp = |chunks: &[Vec<u8>]| {
        let body = chunks.concat();
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
        v.extend_from_slice(b"WEBP");
        v.extend(body);
        v
    };
    let vp8x = |flags: u8| riff(b"VP8X", &[flags, 0, 0, 0, 1, 0, 0, 1, 0, 0]);
    assert_eq!(animated_format(&webp(&[vp8x(0x02)])), Some(Animated::Webp));
    assert_eq!(
        animated_format(&webp(&[vp8x(0x00), riff(b"ANIM", &[0; 6])])),
        Some(Animated::Webp)
    );
    assert_eq!(
        animated_format(&webp(&[vp8x(0x10), riff(b"VP8L", &[0x2F; 5])])),
        None
    );
    assert_eq!(animated_format(&[0xFF, 0xD8, 0xFF, 0xE0]), None);
    assert_eq!(Animated::Gif.ffmpeg_format(), "gif");
    assert_eq!(Animated::Apng.ffmpeg_format(), "apng");
    assert_eq!(Animated::Webp.ffmpeg_format(), "webp_anim");
}

// --- the check --------------------------------------------------------

#[test]
fn the_check_reads_container_codec_size_rate_and_duration() {
    let f = fx();
    let Some(mp4) = h264(&f.src.join("a.mp4"), "320x240", 25) else {
        return;
    };
    let i = probe_video(&mp4).unwrap();
    assert_eq!(i.container, Container::Mp4);
    assert_eq!(i.codec, Codec::H264);
    assert!(!i.profile.is_empty(), "{i:?}");
    assert_eq!((i.width, i.height), (320, 240));
    assert_eq!((i.fps_num, i.fps_den), (25, 1));
    assert!((900..=1100).contains(&i.duration_ms), "{i:?}");
    assert!(!is_cpu_ready(&i));

    let webm = vp9(&f.src.join("b.webm"), "320x240", 30).unwrap();
    let i = probe_video(&webm).unwrap();
    assert_eq!((i.container, i.codec), (Container::WebM, Codec::Vp9));
    assert_eq!(i.profile, "0");
    assert!(is_cpu_ready(&i), "{i:?}");

    // The same stream in a MOV and an MKV: the container is the content's.
    let mov = f.src.join("c.mov");
    let mkv = f.src.join("d.mkv");
    for (out, fmt) in [(&mov, "mov"), (&mkv, "matroska")] {
        let ok = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
            .arg(&mp4)
            .args(["-c", "copy", "-f", fmt])
            .arg(out)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    assert_eq!(probe_video(&mov).unwrap().container, Container::Mov);
    assert_eq!(probe_video(&mkv).unwrap().container, Container::Matroska);
    assert_eq!(Container::Matroska.extension(), "mkv");
    assert_eq!(Container::Mov.ffmpeg_format(), "mov");
    assert_eq!(Container::WebM.ffmpeg_format(), "matroska");

    // Audio is ignored.
    let with_audio = f.src.join("e.mp4");
    let ok = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(&mp4)
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=d=1",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&with_audio)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    assert_eq!(probe_video(&with_audio).unwrap().codec, Codec::H264);

    if has_encoder("libx265") {
        let hevc = clip(
            &f.src.join("f.mkv"),
            "320x240",
            30,
            0.5,
            &[
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ],
        )
        .unwrap();
        let i = probe_video(&hevc).unwrap();
        assert_eq!((i.container, i.codec), (Container::Matroska, Codec::Hevc));
    } else {
        eprintln!("skipped HEVC: no libx265");
    }
    for enc in ["libsvtav1", "libaom-av1"] {
        if !has_encoder(enc) {
            continue;
        }
        let mut args = vec!["-c:v", enc, "-pix_fmt", "yuv420p"];
        if enc == "libaom-av1" {
            args.extend(["-cpu-used", "8"]);
        }
        let av1 = clip(&f.src.join("g.mp4"), "320x240", 30, 0.3, &args).unwrap();
        let i = probe_video(&av1).unwrap();
        assert_eq!((i.container, i.codec), (Container::Mp4, Codec::Av1));
        break;
    }
}

#[test]
fn the_check_refuses_what_the_rules_refuse() {
    let f = fx();
    let Some(mp4) = h264(&f.src.join("ok.mp4"), "64x48", 10) else {
        return;
    };
    let refused = |p: &Path| probe_video(p).unwrap_err();

    let e = refused(
        &clip(
            &f.src.join("ten.webm"),
            "64x48",
            10,
            0.3,
            &[
                "-c:v",
                "libvpx-vp9",
                "-deadline",
                "realtime",
                "-pix_fmt",
                "yuv420p10le",
            ],
        )
        .unwrap(),
    );
    assert!(e.contains("10-bit") || e.contains("profile 2"), "{e}");
    let e = refused(
        &clip(
            &f.src.join("full.mov"),
            "64x48",
            10,
            0.3,
            &[
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv444p",
            ],
        )
        .unwrap(),
    );
    assert!(e.contains("4:4:4"), "{e}");
    let e = refused(
        &clip(
            &f.src.join("wide.webm"),
            "3842x16",
            10,
            0.1,
            &[
                "-c:v",
                "libvpx-vp9",
                "-deadline",
                "realtime",
                "-pix_fmt",
                "yuv420p",
            ],
        )
        .unwrap(),
    );
    assert!(e.contains("3842x16") && e.contains("3840x2160"), "{e}");
    let e = refused(&clip(&f.src.join("old.mkv"), "64x48", 10, 0.3, &["-c:v", "mpeg4"]).unwrap());
    assert!(
        e.contains("video codec") && e.contains("not accepted"),
        "{e}"
    );
    let e = refused(
        &clip(
            &f.src.join("stream.ts"),
            "64x48",
            10,
            0.3,
            &["-c:v", "libx264", "-preset", "ultrafast", "-f", "mpegts"],
        )
        .unwrap(),
    );
    assert!(e.contains("is not accepted") && e.contains("MP4"), "{e}");
    // Audio only.
    let m4a = f.src.join("tone.mp4");
    let ok = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=d=0.5",
            "-c:a",
            "aac",
        ])
        .arg(&m4a)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let e = refused(&m4a);
    assert!(e.contains("no video") || e.contains("not accepted"), "{e}");

    // Not a regular file, too large, missing: refused before GStreamer.
    let fifo = f.src.join("pipe.mp4");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(refused(&fifo).contains("not a regular file"));
    let big = f.src.join("big.mp4");
    std::fs::copy(&mp4, &big).unwrap();
    File::options()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len(MAX_VIDEO_BYTES + 1)
        .unwrap();
    assert!(refused(&big).contains("512 MiB"));
    assert!(refused(&f.src.join("missing.mp4")).contains("unreadable"));
    let junk = f.src.join("junk.mp4");
    std::fs::write(&junk, b"\0\0\0\x18ftypmp42 and then nothing").unwrap();
    assert!(!refused(&junk).is_empty());
}

#[test]
fn cpu_ready_is_vp9_profile_0_in_matroska_within_1080p30() {
    use Codec::*;
    use Container::*;
    let ready = |i: VideoInfo| is_cpu_ready(&i);
    assert!(ready(info(WebM, Vp9, "0", (1920, 1080), (30, 1))));
    assert!(ready(info(Matroska, Vp9, "0", (1280, 720), (30000, 1001))));
    assert!(ready(info(WebM, Vp9, "", (640, 360), (24, 1))));
    assert!(!ready(info(WebM, Vp9, "0", (1920, 1088), (30, 1))));
    assert!(!ready(info(WebM, Vp9, "0", (1928, 1080), (30, 1))));
    assert!(!ready(info(WebM, Vp9, "0", (1920, 1080), (60, 1))));
    assert!(!ready(info(WebM, Vp9, "0", (1920, 1080), (0, 1))));
    assert!(!ready(info(WebM, Vp9, "2", (1920, 1080), (30, 1))));
    assert!(!ready(info(Mp4, Vp9, "0", (1920, 1080), (30, 1))));
    assert!(!ready(info(WebM, Av1, "main", (1920, 1080), (30, 1))));
    assert!(!ready(info(Mp4, H264, "high", (1280, 720), (30, 1))));
}

// --- import -----------------------------------------------------------

#[test]
fn a_video_is_stored_as_it_is_with_the_extension_of_its_container() {
    let f = fx();
    let Some(src) = h264(&f.src.join("Holiday clip.dat"), "320x240", 25) else {
        return;
    };
    let kind = MediaKind::Background;
    let (stored, new) = f.store.import_stored(kind, &src).unwrap();
    assert!(new);
    assert_eq!(stored, f.store.user_dir(kind).join("Holiday clip.mp4"));
    assert_eq!(
        std::fs::read(&stored).unwrap(),
        std::fs::read(&src).unwrap()
    );
    // The same bytes again, under any name: the stored file.
    let again = f.src.join("again.mp4");
    std::fs::copy(&src, &again).unwrap();
    assert_eq!(
        f.store.import_stored(kind, &again).unwrap(),
        (stored.clone(), false)
    );

    // Its poster is in the cache, under its content hash.
    let content = hash::to_hex(&hash::sha256(&std::fs::read(&stored).unwrap()));
    let poster = f.store.cache().unwrap().poster_path(&content);
    assert!(
        cache::has_magic(&poster, cache::JPEG_MAGIC),
        "{}",
        poster.display()
    );

    let items = f.store.list(kind);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.media_type, MediaType::Video);
    assert_eq!((item.width, item.height), (320, 240));
    assert!((900..=1100).contains(&item.duration_ms), "{item:?}");
    assert_eq!(item.poster.as_deref(), Some(poster.as_path()));

    // A WebM named .mkv is stored as .webm.
    let w = vp9(&f.src.join("loop.mkv"), "64x48", 10).unwrap();
    assert_eq!(
        f.store.import(kind, &w).unwrap(),
        f.store.user_dir(kind).join("loop.webm")
    );
}

#[test]
fn an_away_image_must_be_a_still() {
    let f = fx();
    let Some(src) = h264(&f.src.join("clip.mp4"), "64x48", 10) else {
        return;
    };
    let e = f.store.import(MediaKind::Away, &src).unwrap_err();
    assert!(matches!(e, MediaError::VideoUnsupported(_)), "{e}");
    assert!(e.to_string().contains("must be a still image"), "{e}");
    let gif = clip(&f.src.join("a.gif"), "32x32", 10, 0.5, &["-f", "gif"]).unwrap();
    let e = f.store.import(MediaKind::Away, &gif).unwrap_err();
    assert!(matches!(e, MediaError::VideoUnsupported(_)), "{e}");
    assert!(names(&f.store.user_dir(MediaKind::Away)).is_empty());
}

#[test]
fn a_refused_video_lands_nowhere() {
    let f = fx();
    let Some(src) = clip(
        &f.src.join("full.mp4"),
        "64x48",
        10,
        0.3,
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv444p",
        ],
    ) else {
        return;
    };
    let e = f.store.import(MediaKind::Background, &src).unwrap_err();
    assert!(matches!(e, MediaError::NotAVideo { .. }), "{e}");
    assert!(e.to_string().contains("full.mp4"), "{e}");
    assert!(matches!(e.to_ipc(), obc_ipc::IpcError::InvalidArgument(_)));
    assert!(names(&f.store.user_dir(MediaKind::Background)).is_empty());
}

#[test]
fn an_animated_gif_becomes_a_cpu_ready_webm() {
    let f = fx();
    // 2000x1202 at 50 fps: fitted inside 1920x1080, even, at most 30 fps.
    let Some(gif) = clip(
        &f.src.join("Party.gif"),
        "2000x1202",
        50,
        0.5,
        &["-f", "gif"],
    ) else {
        return;
    };
    let kind = MediaKind::Background;
    let (stored, new) = f.store.import_stored(kind, &gif).unwrap();
    assert!(new);
    assert_eq!(stored, f.store.user_dir(kind).join("Party.webm"));
    let i = probe_video(&stored).unwrap();
    assert!(is_cpu_ready(&i), "{i:?}");
    assert_eq!((i.width % 2, i.height % 2), (0, 0));
    assert!(i.width <= 1920 && i.height <= 1080, "{i:?}");
    assert_eq!(i.height, 1080, "{i:?}");
    assert!(u64::from(i.fps_num) <= 30 * u64::from(i.fps_den), "{i:?}");
    // The same GIF again: the stored WebM, not a second one.
    assert_eq!(
        f.store.import_stored(kind, &gif).unwrap(),
        (stored.clone(), false)
    );
    // It has a poster; removing it removes the poster and the record.
    let items = f.store.list(kind);
    let poster = items[0].poster.clone().expect("a poster");
    f.store.remove(&stored).unwrap();
    assert!(!poster.exists());
    let records = f.store.user_dir(kind).join(TRANSCODED_DIR);
    assert_eq!(std::fs::read_dir(&records).unwrap().count(), 0);
    // So the next import stores it again.
    assert!(f.store.import_stored(kind, &gif).unwrap().1);
}

#[test]
fn a_small_odd_gif_an_apng_and_an_animated_webp_are_transcoded() {
    let f = fx();
    let kind = MediaKind::Background;
    let Some(gif) = clip(&f.src.join("odd.gif"), "99x51", 10, 0.5, &["-f", "gif"]) else {
        return;
    };
    let i = probe_video(&f.store.import(kind, &gif).unwrap()).unwrap();
    assert_eq!((i.width, i.height), (98, 50));
    assert!(is_cpu_ready(&i));
    let apng = clip(
        &f.src.join("moving.png"),
        "64x48",
        10,
        0.5,
        &["-plays", "0", "-f", "apng"],
    )
    .unwrap();
    let stored = f.store.import(kind, &apng).unwrap();
    assert_eq!(stored.extension().unwrap(), "webm");
    assert!(is_cpu_ready(&probe_video(&stored).unwrap()));
    if has_encoder("libwebp_anim") {
        let webp = clip(
            &f.src.join("moving.webp"),
            "64x48",
            10,
            0.5,
            &["-c:v", "libwebp_anim", "-loop", "0"],
        )
        .unwrap();
        match f.store.import(kind, &webp) {
            Ok(stored) => assert!(is_cpu_ready(&probe_video(&stored).unwrap())),
            // An ffmpeg without the animated WebP demuxer says so.
            Err(e) => assert!(e.to_string().contains("ffmpeg failed"), "{e}"),
        }
    }
}

#[test]
fn a_video_put_into_the_gallery_by_hand_gets_its_poster_when_listed() {
    let f = fx();
    let dir = f.store.user_dir(MediaKind::Background);
    std::fs::create_dir_all(&dir).unwrap();
    let Some(v) = vp9(&dir.join("by-hand.webm"), "64x48", 10) else {
        return;
    };
    // The listing makes no poster: there is none yet.
    let plain = f.store.list(MediaKind::Background);
    assert_eq!(plain[0].poster, None);
    assert_eq!((plain[0].width, plain[0].height), (64, 48));
    // The poster maker makes it, once however often it is asked
    // meanwhile.
    let made = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&made);
    let maker = PosterMaker::new(f.store.clone(), move |p| {
        seen.lock().unwrap().push(p.to_path_buf());
    });
    maker.request([v.clone(), v.clone()]);
    let end = Instant::now() + EVENT_WAIT;
    while maker.busy() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(*made.lock().unwrap(), std::slice::from_ref(&v));
    let items = f.store.list(MediaKind::Background);
    let poster = items[0].poster.clone().expect("a poster");
    assert!(cache::has_magic(&poster, cache::JPEG_MAGIC));
    let content = hash::to_hex(&hash::sha256(&std::fs::read(&v).unwrap()));
    assert_eq!(poster, f.store.cache().unwrap().poster_path(&content));
    // A store without a cache lists no poster.
    let bare = MediaStore::new(f.root.join("bundled"), f.root.join("user"));
    assert_eq!(bare.list(MediaKind::Background)[0].poster, None);
}

#[test]
fn a_bundled_video_lists_its_shipped_poster_from_the_cache_without_ffmpeg() {
    let f = fx();
    let assets = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/media/backgrounds"
    ));
    let dir = f.store.bundled_dir(MediaKind::Background);
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["slow-aurora.webm", "slow-aurora.poster.jpg"] {
        std::fs::copy(assets.join(name), dir.join(name)).unwrap();
    }
    let items = f.store.list(MediaKind::Background);
    // The poster is not an item.
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].path, dir.join("slow-aurora.webm"));
    let content = hash::to_hex(&hash::sha256(
        &std::fs::read(dir.join("slow-aurora.webm")).unwrap(),
    ));
    let cached = f.store.cache().unwrap().poster_path(&content);
    assert_eq!(items[0].poster.as_deref(), Some(cached.as_path()));
    assert_eq!(
        std::fs::read(&cached).unwrap(),
        std::fs::read(dir.join("slow-aurora.poster.jpg")).unwrap()
    );
    // A user file of that name is an item like any other.
    let user = f.store.user_dir(MediaKind::Background);
    std::fs::create_dir_all(&user).unwrap();
    std::fs::copy(
        assets.join("slow-aurora.poster.jpg"),
        user.join("mine.poster.jpg"),
    )
    .unwrap();
    assert_eq!(f.store.list(MediaKind::Background).len(), 2);
    // Not a JPEG: not copied, no poster.
    let g = fx();
    let dir = g.store.bundled_dir(MediaKind::Background);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(assets.join("slow-aurora.webm"), dir.join("loop.webm")).unwrap();
    std::fs::write(dir.join("loop.poster.jpg"), b"not a jpeg").unwrap();
    let items = g.store.list(MediaKind::Background);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].poster, None);
}

#[test]
fn the_cache_follows_xdg_cache_home_then_home() {
    let env = |vars: &'static [(&str, &str)]| {
        move |k: &str| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| std::ffi::OsString::from(v))
        }
    };
    let s = MediaStore::from_env_in(env(&[("XDG_CACHE_HOME", "/c"), ("HOME", "/h")])).unwrap();
    let c = s.cache().unwrap();
    assert_eq!(c.root(), Path::new("/c/omarchy-broadcast"));
    assert_eq!(c.posters_dir(), Path::new("/c/omarchy-broadcast/posters"));
    assert_eq!(c.playback_dir(), Path::new("/c/omarchy-broadcast/playback"));
    let s = MediaStore::from_env_in(env(&[("XDG_CACHE_HOME", "rel"), ("HOME", "/h")])).unwrap();
    assert_eq!(
        s.cache().unwrap().root(),
        Path::new("/h/.cache/omarchy-broadcast")
    );
    let s = MediaStore::from_env_in(env(&[("XDG_DATA_HOME", "/d")])).unwrap();
    assert!(s.cache().is_none());
    let h = "ab".repeat(32);
    let c = MediaCache::new("/c");
    assert_eq!(
        c.copy_path(&h),
        PathBuf::from(format!("/c/playback/{h}-v{ENCODE_SETTINGS_VERSION}.webm"))
    );
    assert_eq!(
        c.poster_path(&h),
        PathBuf::from(format!("/c/posters/{h}.jpg"))
    );
}

// --- the runner -------------------------------------------------------

/// A fake ffmpeg in `dir`: records its arguments (`argv`, one per line),
/// its niceness (`nice`), its pid (`pid`) and the first bytes of its
/// descriptor 3 (`input`), then runs `body` with `$out` the output path.
fn fake(dir: &Path, body: &str) -> Ffmpeg {
    std::fs::create_dir_all(dir).unwrap();
    let d = dir.display();
    let script = format!(
        "#!/bin/sh\n\
         for a; do last=$a; done\n\
         out=${{last#file:}}\n\
         printf '%s\\n' \"$@\" > '{d}/argv'\n\
         cut -d' ' -f19 /proc/$$/stat > '{d}/nice'\n\
         echo $$ > '{d}/pid'\n\
         head -c 4 /proc/self/fd/3 > '{d}/input'\n\
         {body}\n"
    );
    let path = dir.join("ffmpeg");
    let tmp = dir.join(".ffmpeg.tmp");
    std::fs::write(&tmp, script).unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    Ffmpeg::at(path)
}

/// Writes a minimal WebM start to `$out`.
const WRITES_WEBM: &str = "printf '\\032\\105\\337\\243 fake webm' > \"$out\"";
/// Fails with an error line.
const FAILS: &str = "echo 'boom: bad input' >&2; exit 1";
/// Hangs (exec keeps the pid, so the kill reaches it).
const HANGS: &str = "exec sleep 30";

fn argv_of(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("argv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn wait_for(path: &Path) {
    let end = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < end, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pid_alive(dir: &Path) -> bool {
    pid_number_alive(std::fs::read_to_string(dir.join("pid")).unwrap().trim())
}

/// Whether the process `pid` runs (not gone, not a zombie).
fn pid_number_alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

#[test]
fn the_runner_hardens_every_run() {
    let f = fx();
    let bin = f.root.join("bin");
    let ff = fake(&bin, WRITES_WEBM);
    let input_path = f.src.join("in.bin");
    std::fs::write(&input_path, b"WXYZ and more").unwrap();
    let input = File::open(&input_path).unwrap();
    let out = f.root.join("out.webm");
    let job = FfmpegJob {
        input: &input,
        input_format: "mov",
        output_args: ffmpeg::vp9_args(),
        output: &out,
        timeout: Duration::from_secs(20),
    };
    ff.run(&job, &AtomicBool::new(false)).unwrap();
    let argv = argv_of(&bin);
    let at = |s: &str| {
        argv.iter()
            .position(|a| a == s)
            .unwrap_or_else(|| panic!("{s} in {argv:?}"))
    };
    assert!(at("-nostdin") < at("-i"));
    assert!(at("-hide_banner") < at("-i"));
    assert_eq!(argv[at("-protocol_whitelist") + 1], "file");
    assert!(at("-protocol_whitelist") < at("-i"));
    assert_eq!(argv[at("-f") + 1], "mov", "the forced demuxer, before -i");
    assert!(at("-f") < at("-i"));
    assert_eq!(argv[at("-i") + 1], "file:/proc/self/fd/3");
    assert_eq!(argv[at("-threads") + 1], ffmpeg::job_threads().to_string());
    assert!(at("-n") > at("-i"));
    assert_eq!(argv.last().unwrap(), &format!("file:{}", out.display()));
    for (k, v) in [
        ("-c:v", "libvpx-vp9"),
        ("-profile:v", "0"),
        ("-pix_fmt", "yuv420p"),
        ("-deadline", "realtime"),
        ("-cpu-used", "8"),
        ("-row-mt", "1"),
        ("-crf", "32"),
        ("-b:v", "8M"),
        ("-fpsmax", "30"),
        ("-force_key_frames", "expr:gte(t,n_forced*2)"),
    ] {
        assert_eq!(argv[at(k) + 1], v, "{k}");
    }
    assert!(argv.contains(&"-an".to_string()));
    // nice 19, and the input is the descriptor the daemon opened.
    assert_eq!(
        std::fs::read_to_string(bin.join("nice")).unwrap().trim(),
        "19"
    );
    assert_eq!(std::fs::read(bin.join("input")).unwrap(), b"WXYZ");
    let n = std::thread::available_parallelism().map_or(1, |n| n.get());
    assert_eq!(ffmpeg::job_threads(), (n / 2).max(1));
}

#[test]
fn the_runner_reports_failure_timeout_and_cancel() {
    let f = fx();
    let input_path = f.src.join("in.bin");
    std::fs::write(&input_path, b"data").unwrap();
    let input = File::open(&input_path).unwrap();
    let out = f.root.join("out.webm");
    let job = |timeout| FfmpegJob {
        input: &input,
        input_format: "gif",
        output_args: Vec::new(),
        output: &out,
        timeout,
    };
    let failing = fake(&f.root.join("fail"), FAILS);
    let e = failing.run(&job(Duration::from_secs(20)), &AtomicBool::new(false));
    assert_eq!(
        e,
        Err(FfmpegError::Failed("boom: bad input (exit 1)".into()))
    );

    let hang_dir = f.root.join("hang");
    let hanging = fake(&hang_dir, HANGS);
    let t = Instant::now();
    let e = hanging.run(&job(Duration::from_millis(300)), &AtomicBool::new(false));
    assert_eq!(e, Err(FfmpegError::TimedOut(Duration::from_millis(300))));
    assert!(t.elapsed() < Duration::from_secs(10));
    assert!(!pid_alive(&hang_dir));

    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let c = std::sync::Arc::clone(&cancel);
    let pid = hang_dir.join("pid");
    std::fs::remove_file(&pid).unwrap();
    let stopper = std::thread::spawn(move || {
        wait_for(&pid);
        c.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let e = hanging.run(&job(Duration::from_secs(60)), &cancel);
    stopper.join().unwrap();
    assert_eq!(e, Err(FfmpegError::Cancelled));
    assert!(!pid_alive(&hang_dir));
}

#[test]
fn ffmpeg_is_found_on_path_or_reported_missing() {
    let f = fx();
    let a = f.root.join("a");
    let b = f.root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    // Not executable: skipped.
    std::fs::write(a.join("ffmpeg"), b"#!/bin/sh\n").unwrap();
    let real = fake(&b, "true");
    let path =
        std::env::join_paths(["relative", a.to_str().unwrap(), b.to_str().unwrap()]).unwrap();
    assert_eq!(Ffmpeg::locate_in(Some(&path)), Some(real));
    let none = std::env::join_paths([a.to_str().unwrap()]).unwrap();
    assert_eq!(Ffmpeg::locate_in(Some(&none)), None);
    assert_eq!(Ffmpeg::locate_in(None), None);
    assert_eq!(FfmpegError::Missing.to_string(), "ffmpeg is not installed");
}

// --- playback copies --------------------------------------------------

fn next_event(rx: &Receiver<CopyEvent>) -> CopyEvent {
    rx.recv_timeout(EVENT_WAIT).expect("a copy event")
}

/// The end of the next job (past its `Preparing` step).
fn end_of(rx: &Receiver<CopyEvent>) -> Result<PathBuf, String> {
    loop {
        match next_event(rx).state {
            CopyState::Preparing => {}
            CopyState::Ready(p) => return Ok(p),
            CopyState::Failed(r) => return Err(r),
            other => panic!("not a job's step: {other:?}"),
        }
    }
}

#[test]
fn a_copy_job_runs_once_and_its_copy_is_then_ready() {
    let f = fx();
    let Some(src) = h264(&f.src.join("a.mp4"), "64x48", 10) else {
        return;
    };
    let bin = f.root.join("bin");
    let cache = f.store.cache().unwrap().clone();
    let (copies, rx) = PlaybackCopies::new(cache.clone(), Ok(fake(&bin, WRITES_WEBM)));
    // Not hashed yet in this run: the job looks for a copy first, then
    // says it makes one.
    assert_eq!(copies.ensure(&src), CopyState::Checking);
    let ev = next_event(&rx);
    assert_eq!((&ev.source, &ev.state), (&src, &CopyState::Preparing));
    let path = end_of(&rx).unwrap();
    let content = hash::to_hex(&hash::sha256(&std::fs::read(&src).unwrap()));
    assert_eq!(path, cache.copy_path(&content));
    assert!(
        path.to_str()
            .unwrap()
            .ends_with(&format!("-v{ENCODE_SETTINGS_VERSION}.webm"))
    );
    assert_eq!(copies.copy_path(&src).unwrap(), path);
    // The demuxer forced is the source's container's.
    let argv = argv_of(&bin);
    let fi = argv.iter().position(|a| a == "-f").unwrap();
    assert_eq!(argv[fi + 1], "mov");
    assert_eq!(
        std::fs::read_to_string(bin.join("nice")).unwrap().trim(),
        "19"
    );
    // No temporary file is left.
    let left: Vec<_> = std::fs::read_dir(cache.playback_dir())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(copies.ensure(&src), CopyState::Ready(path.clone()));

    // A copy that is not a WebM any more is made again.
    std::fs::write(&path, b"garbage").unwrap();
    assert_eq!(copies.ensure(&src), CopyState::Preparing);
    assert_eq!(end_of(&rx), Ok(path.clone()));
    // So is a missing one.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(copies.ensure(&src), CopyState::Preparing);
    assert_eq!(end_of(&rx), Ok(path));
}

#[test]
fn a_failed_job_is_remembered_until_the_source_is_forgotten() {
    let f = fx();
    let Some(src) = h264(&f.src.join("a.mp4"), "64x48", 10) else {
        return;
    };
    let (copies, rx) = PlaybackCopies::new(
        f.store.cache().unwrap().clone(),
        Ok(fake(&f.root.join("bin"), FAILS)),
    );
    copies.ensure(&src);
    let reason = end_of(&rx).unwrap_err();
    // What failed, with no prefix of its own: `Warning` gives it one.
    assert_eq!(reason, "ffmpeg failed: boom: bad input (exit 1)");
    assert_eq!(copies.ensure(&src), CopyState::Failed(reason.clone()));
    assert!(!copies.busy());
    // A camera start runs it once more; a second failure stands.
    assert_eq!(copies.retry(&src), CopyState::Preparing);
    assert_eq!(end_of(&rx), Err(reason.clone()));
    assert_eq!(copies.retry(&src), CopyState::Failed(reason.clone()));
    assert_eq!(copies.ensure(&src), CopyState::Failed(reason));
    assert!(!copies.busy());
    copies.remove_for(&src);
    assert_eq!(copies.ensure(&src), CopyState::Preparing);
    assert!(end_of(&rx).is_err());
}

#[test]
fn a_newer_request_cancels_the_running_job_for_another_source() {
    let f = fx();
    let Some(a) = h264(&f.src.join("a.mp4"), "64x48", 10) else {
        return;
    };
    let b = h264(&f.src.join("b.mp4"), "64x48", 12).unwrap();
    let c = h264(&f.src.join("c.mp4"), "64x48", 15).unwrap();
    let bin = f.root.join("bin");
    // The first run hangs (and takes the marker); later ones write.
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("hang"), b"").unwrap();
    let body = format!(
        "if [ -e '{0}/hang' ]; then rm '{0}/hang'; exec sleep 30; fi\n{WRITES_WEBM}",
        bin.display()
    );
    let (copies, rx) = PlaybackCopies::new(f.store.cache().unwrap().clone(), Ok(fake(&bin, &body)));
    assert_eq!(copies.ensure(&a), CopyState::Checking);
    wait_for(&bin.join("pid"));
    let hung = std::fs::read_to_string(bin.join("pid")).unwrap();
    // The same source again: still preparing, no second job.
    assert_eq!(copies.ensure(&a), CopyState::Preparing);
    // Another source: the hung job is killed; a third replaces the second
    // if it is still queued.
    assert_eq!(copies.ensure(&b), CopyState::Checking);
    assert_eq!(copies.ensure(&c), CopyState::Checking);
    let mut seen = Vec::new();
    loop {
        let ev = next_event(&rx);
        assert!(
            matches!(ev.state, CopyState::Ready(_) | CopyState::Preparing),
            "{ev:?}"
        );
        if matches!(ev.state, CopyState::Ready(_)) {
            seen.push(ev.source.clone());
        }
        if ev.source == c && matches!(ev.state, CopyState::Ready(_)) {
            break;
        }
    }
    assert!(
        !seen.contains(&a),
        "the cancelled job sent no event: {seen:?}"
    );
    assert!(
        !pid_number_alive(hung.trim()),
        "the cancelled ffmpeg was killed"
    );
}

#[test]
fn choosing_back_the_source_of_a_cancelled_job_queues_it_again() {
    let f = fx();
    let Some(a) = h264(&f.src.join("a.mp4"), "64x48", 10) else {
        return;
    };
    let b = h264(&f.src.join("b.mp4"), "64x48", 12).unwrap();
    let bin = f.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("hang"), b"").unwrap();
    let body = format!(
        "if [ -e '{0}/hang' ]; then rm '{0}/hang'; exec sleep 30; fi\n{WRITES_WEBM}",
        bin.display()
    );
    let (copies, rx) = PlaybackCopies::new(f.store.cache().unwrap().clone(), Ok(fake(&bin, &body)));
    // A, then B (A's job cancelled), then A again: A must not wait on
    // its cancelled job, which sends nothing.
    copies.ensure(&a);
    wait_for(&bin.join("pid"));
    copies.ensure(&b);
    assert_ne!(copies.ensure(&a), CopyState::Checking, "A's hash is known");
    loop {
        let ev = next_event(&rx);
        if ev.source == a && matches!(ev.state, CopyState::Ready(_)) {
            break;
        }
        assert_ne!(
            ev.source, b,
            "B was replaced while queued, or cancelled: {ev:?}"
        );
    }
}

#[test]
fn an_existing_copy_is_found_without_running_ffmpeg() {
    let f = fx();
    let src = f.src.join("unhashed.mp4");
    std::fs::write(&src, b"\0\0\0\x18ftypisom some bytes no one hashed yet").unwrap();
    let cache = f.store.cache().unwrap().clone();
    // The copy of an earlier run, named by the content's hash.
    let content = hash::to_hex(&hash::sha256(&std::fs::read(&src).unwrap()));
    let copy = cache.copy_path(&content);
    std::fs::create_dir_all(cache.playback_dir()).unwrap();
    std::fs::write(&copy, b"\x1a\x45\xdf\xa3 an earlier copy").unwrap();
    let bin = f.root.join("bin");
    let (copies, rx) = PlaybackCopies::new(cache, Ok(fake(&bin, FAILS)));
    assert_eq!(copies.ensure(&src), CopyState::Checking, "not Preparing");
    let ev = next_event(&rx);
    assert_eq!(
        ev.state,
        CopyState::Ready(copy.clone()),
        "no Preparing step"
    );
    assert!(!bin.join("argv").exists(), "ffmpeg was not run");
    assert_eq!(copies.ensure(&src), CopyState::Ready(copy));
}

/// The ffmpeg [`relocating`] finds: none until set.
static LOCATED: std::sync::Mutex<Option<Ffmpeg>> = std::sync::Mutex::new(None);

fn relocating() -> Result<Ffmpeg, FfmpegError> {
    LOCATED.lock().unwrap().clone().ok_or(FfmpegError::Missing)
}

#[test]
fn ffmpeg_installed_while_running_is_found_at_the_next_request() {
    let f = fx();
    let Some(src) = h264(&f.src.join("late.mp4"), "64x48", 10) else {
        return;
    };
    let (copies, rx) = PlaybackCopies::with_locator(f.store.cache().unwrap().clone(), relocating);
    assert_eq!(copies.ensure(&src), CopyState::NoFfmpeg);
    assert_eq!(copies.ensure(&src), CopyState::NoFfmpeg);
    *LOCATED.lock().unwrap() = Some(fake(&f.root.join("bin"), WRITES_WEBM));
    assert_eq!(copies.ensure(&src), CopyState::Checking);
    assert!(end_of(&rx).is_ok());
}

#[test]
fn a_stopped_prune_removes_nothing() {
    let f = fx();
    let cache = f.store.cache().unwrap().clone();
    std::fs::create_dir_all(cache.playback_dir()).unwrap();
    let orphan = cache.copy_path(&"3".repeat(64));
    std::fs::write(&orphan, b"x").unwrap();
    let older = cache.copy_path(&"4".repeat(64));
    std::fs::write(&older, b"x").unwrap();
    // Stopped (the service dropped): what to keep is not known in full.
    let stop = AtomicBool::new(true);
    let report = super::playback::prune(&cache, &f.store, None, &stop);
    assert!(report.removed.is_empty());
    assert!(orphan.exists() && older.exists());
}

#[test]
fn dropping_the_service_kills_its_job() {
    let f = fx();
    let Some(a) = h264(&f.src.join("a.mp4"), "64x48", 10) else {
        return;
    };
    let bin = f.root.join("bin");
    let (copies, rx) = PlaybackCopies::new(f.store.cache().unwrap().clone(), Ok(fake(&bin, HANGS)));
    copies.ensure(&a);
    wait_for(&bin.join("pid"));
    assert!(copies.busy());
    let t = Instant::now();
    drop(copies);
    assert!(t.elapsed() < Duration::from_secs(10));
    assert!(!pid_alive(&bin));
    // Its transcode step at most: a cancelled job sends no end.
    let steps: Vec<_> = rx.try_iter().map(|e| e.state).collect();
    assert!(
        steps.iter().all(|s| *s == CopyState::Preparing),
        "{steps:?}"
    );
}

#[test]
fn without_ffmpeg_no_copy_and_a_cpu_ready_source_plays_as_it_is() {
    let f = fx();
    let cache = f.store.cache().unwrap().clone();
    let (none, _rx) = PlaybackCopies::new(cache.clone(), Err(FfmpegError::Missing));
    assert_eq!(none.ensure(Path::new("/nowhere.mp4")), CopyState::NoFfmpeg);
    let Some(w) = vp9(&f.src.join("ready.webm"), "64x48", 10) else {
        return;
    };
    let bin = f.root.join("bin");
    let (copies, rx) = PlaybackCopies::new(cache, Ok(fake(&bin, FAILS)));
    copies.ensure(&w);
    assert_eq!(end_of(&rx), Ok(w));
    assert!(!bin.join("argv").exists(), "ffmpeg was not run");
    let missing = copies.ensure(&f.src.join("missing.mp4"));
    assert!(matches!(missing, CopyState::Failed(r) if r.contains("unreadable")));
}

#[test]
fn a_real_copy_is_cpu_ready() {
    let f = fx();
    let Some(src) = h264(&f.src.join("fast.mp4"), "320x240", 60) else {
        return;
    };
    let (copies, rx) = PlaybackCopies::new(f.store.cache().unwrap().clone(), Ffmpeg::locate());
    copies.ensure(&src);
    let copy = end_of(&rx).unwrap();
    let i = probe_video(&copy).unwrap();
    assert!(is_cpu_ready(&i), "{i:?}");
    assert_eq!((i.width, i.height), (320, 240));
}

#[test]
fn removing_a_video_removes_its_poster_and_copies() {
    let f = fx();
    let Some(src) = h264(&f.src.join("a.mp4"), "64x48", 10) else {
        return;
    };
    let kind = MediaKind::Background;
    let stored = f.store.import(kind, &src).unwrap();
    let cache = f.store.cache().unwrap().clone();
    let (copies, rx) =
        PlaybackCopies::new(cache.clone(), Ok(fake(&f.root.join("bin"), WRITES_WEBM)));
    copies.ensure(&stored);
    let copy = end_of(&rx).unwrap();
    let content = hash::to_hex(&hash::sha256(&std::fs::read(&stored).unwrap()));
    let old = cache.playback_dir().join(format!("{content}-v0.webm"));
    std::fs::write(&old, b"old").unwrap();
    let poster = cache.poster_path(&content);
    assert!(poster.exists() && copy.exists());
    f.store.warm_hash(&stored);
    f.store.remove(&stored).unwrap();
    assert!(!stored.exists() && !poster.exists() && !copy.exists() && !old.exists());
}

#[test]
fn a_prune_keeps_what_gallery_videos_and_the_outside_file_need() {
    let f = fx();
    let dir = f.store.user_dir(MediaKind::Background);
    std::fs::create_dir_all(&dir).unwrap();
    let Some(item) = h264(&dir.join("item.mp4"), "64x48", 10) else {
        return;
    };
    let outside = h264(&f.src.join("outside.mp4"), "64x48", 12).unwrap();
    let h = |p: &Path| hash::to_hex(&hash::sha256(&std::fs::read(p).unwrap()));
    let (hi, ho) = (h(&item), h(&outside));
    let (h1, h2) = ("1".repeat(64), "2".repeat(64));
    let cache = f.store.cache().unwrap().clone();
    std::fs::create_dir_all(cache.playback_dir()).unwrap();
    std::fs::create_dir_all(cache.posters_dir()).unwrap();
    let put = |p: PathBuf| {
        std::fs::write(&p, b"x").unwrap();
        p
    };
    let keep_item = put(cache.copy_path(&hi));
    let old_version = put(cache.playback_dir().join(format!("{hi}-v0.webm")));
    let older = put(cache.copy_path(&h1));
    // Make h2's copy the newest.
    std::thread::sleep(Duration::from_millis(20));
    let newest = put(cache.copy_path(&h2));
    let outside_copy = put(cache.copy_path(&ho));
    let filetime = |p: &Path, secs_ago: u64| {
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    };
    filetime(&outside_copy, 3600);
    filetime(&older, 7200);
    let item_poster = put(cache.poster_path(&hi));
    let stray_poster = put(cache.poster_path(&h1));
    // Another process's temporary file: removed only once an hour old.
    let part = put(cache.playback_dir().join(".part-1-0.tmp"));
    filetime(&part, 2 * 3600);
    let young_part = put(cache.playback_dir().join(".part-1-1.tmp"));
    let stranger = put(cache.playback_dir().join("notes.txt"));

    let (copies, _rx) = PlaybackCopies::new(cache.clone(), Err(FfmpegError::Missing));
    let report = copies.prune_orphans(&f.store, None);
    let gone: BTreeSet<_> = report.removed.into_iter().collect();
    let want: BTreeSet<_> = [old_version, older, outside_copy.clone(), stray_poster, part]
        .into_iter()
        .collect();
    assert_eq!(gone, want);
    assert!(keep_item.exists() && newest.exists() && item_poster.exists() && stranger.exists());
    assert!(
        young_part.exists(),
        "a young temporary file of another process stays"
    );

    // Naming the outside file keeps its copy and drops the spare.
    let outside_copy = put(outside_copy);
    let report = copies.prune_orphans(&f.store, Some(&outside));
    assert_eq!(report.removed, [newest]);
    assert!(outside_copy.exists() && keep_item.exists());

    // The same through the worker.
    let spare = put(cache.copy_path(&h1));
    let stray = put(cache.copy_path(&h2));
    copies.request_prune(&f.store, Some(&outside));
    let end = Instant::now() + Duration::from_secs(20);
    while (spare.exists() || stray.exists()) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!spare.exists() && !stray.exists());
    assert!(outside_copy.exists() && keep_item.exists());
}

fn names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .map(|e| {
            e.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default()
}

use std::collections::BTreeSet;
