//! Tests of the video side of the gallery: the hash, the check of videos,
//! the import of videos and animated images, posters, the ffmpeg runner
//! the command that converts a video for the CPU, and the cache's prune.
//!
//! Clips are made with the ffmpeg of `PATH` (lavfi `testsrc2`, a second
//! or less); without it, or without an encoder a test needs, the test
//! returns early and says so on stderr. The runner is also driven by fake
//! ffmpeg scripts that record their arguments and their niceness.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use super::ffmpeg::{self, Ffmpeg, FfmpegError, FfmpegJob};
use super::hash::{self, Sha256};
use super::probe::{Codec, Container, MAX_VIDEO_BYTES, VideoInfo, is_cpu_ready, probe_video};
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
fn background_videos_and_animations_go_to_the_videos_folder() {
    let f = fx();
    let pictures = f.root.join("Pictures/omarchy-broadcast");
    let videos = f.root.join("Videos/omarchy-broadcast");
    let store = MediaStore::new(f.root.join("bundled"), &pictures)
        .with_video_root(&videos)
        .with_cache(MediaCache::new(f.root.join("cache")));
    let kind = MediaKind::Background;
    let Some(src) = h264(&f.src.join("clip.mp4"), "64x48", 10) else {
        return;
    };
    let stored = store.import(kind, &src).unwrap();
    assert_eq!(stored, videos.join("clip.mp4"));
    assert!(names(&pictures).is_empty(), "{:?}", names(&pictures));

    // A video dropped by hand into Pictures is listed, found as a
    // duplicate, and its poster kept by the prune; so is the stored one.
    let dropped = vp9(&pictures.join("dropped.webm"), "64x48", 10).unwrap();
    let again = f.src.join("again.webm");
    std::fs::copy(&dropped, &again).unwrap();
    assert_eq!(
        store.import_stored(kind, &again).unwrap(),
        (dropped.clone(), false)
    );
    let listed: Vec<PathBuf> = store.list(kind).into_iter().map(|i| i.path).collect();
    assert_eq!(listed, [stored.clone(), dropped.clone()]);
    let mut paths = store.video_paths(kind);
    paths.sort();
    assert_eq!(paths, [dropped.clone(), stored.clone()]);

    // An animated GIF's WebM and its source record are in Videos.
    let gif = clip(&f.src.join("Party.gif"), "32x32", 10, 0.5, &["-f", "gif"]).unwrap();
    let (webm, new) = store.import_stored(kind, &gif).unwrap();
    assert!(new);
    assert_eq!(webm, videos.join("Party.webm"));
    assert_eq!(
        std::fs::read_dir(videos.join(TRANSCODED_DIR))
            .unwrap()
            .count(),
        1
    );
    assert!(!pictures.join(TRANSCODED_DIR).exists());
    assert_eq!(
        store.import_stored(kind, &gif).unwrap(),
        (webm.clone(), false)
    );
    store.remove(&webm).unwrap();
    assert_eq!(
        std::fs::read_dir(videos.join(TRANSCODED_DIR))
            .unwrap()
            .count(),
        0
    );

    // The conversion of a video goes to the videos folder.
    let command = cpu_convert_command(store.convert_dir(), &stored).unwrap();
    assert_eq!(
        shell_words(&command).last().unwrap(),
        &videos.join("clip-cpu.webm").display().to_string()
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
    // Without HOME there is no gallery at all (its root is in the
    // pictures directory), so no store without a cache from the
    // environment.
    let e = MediaStore::from_env_in(env(&[("XDG_CACHE_HOME", "/c")])).unwrap_err();
    assert!(matches!(e, MediaError::NoHome), "{e}");
    let h = "ab".repeat(32);
    let c = MediaCache::new("/c");
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

// --- the command that converts a video for the CPU --------------------

/// The words a POSIX shell reads from `command` (`sh -c 'set -- …'`).
fn shell_words(command: &str) -> Vec<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("set -- {command}; printf '%s\\0' \"$@\""))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .split_terminator('\0')
        .map(str::to_string)
        .collect()
}

#[test]
fn the_convert_command_is_the_import_settings_as_one_shell_line() {
    let input = Path::new("/home/u/My clips/it's \\ $HOME `x` (1).mp4");
    let output = Path::new("/home/u/Pictures/omarchy-broadcast/it's (1)-cpu.webm");
    let command = ffmpeg::cpu_convert_command(input, output).unwrap();
    assert!(
        command.starts_with(
            "ffmpeg -nostdin -n -protocol_whitelist file -i '/home/u/My clips/it'\\''s"
        ),
        "{command}"
    );
    assert!(!command.contains('\n'));
    let mut want = vec![
        "ffmpeg".to_string(),
        "-nostdin".into(),
        "-n".into(),
        "-protocol_whitelist".into(),
        "file".into(),
        "-i".into(),
        input.display().to_string(),
    ];
    want.extend(
        ffmpeg::vp9_args()
            .iter()
            .map(|a| a.to_str().unwrap().to_string()),
    );
    want.push(output.display().to_string());
    assert_eq!(shell_words(&command), want);
    // The same settings as the animated import: the very arguments.
    let words = shell_words(&command);
    assert_eq!(
        words[7..words.len() - 1],
        ffmpeg::vp9_args()
            .iter()
            .map(|a| a.to_str().unwrap().to_string())
            .collect::<Vec<_>>()[..]
    );
    // A path that is not UTF-8 has no command.
    use std::os::unix::ffi::OsStrExt;
    let odd = Path::new(std::ffi::OsStr::from_bytes(b"/x/\xff.mp4"));
    assert_eq!(ffmpeg::cpu_convert_command(odd, output), None);
}

#[test]
fn the_conversion_goes_to_the_gallery_under_a_free_name() {
    let f = fx();
    let dir = f.store.user_dir(MediaKind::Background);
    std::fs::create_dir_all(&dir).unwrap();
    let input = f.src.join("Beach day.webm");
    assert_eq!(
        cpu_convert_output(&dir, &input),
        dir.join("Beach day-cpu.webm")
    );
    std::fs::write(dir.join("Beach day-cpu.webm"), b"x").unwrap();
    std::fs::write(dir.join("Beach day-cpu-2.webm"), b"x").unwrap();
    assert_eq!(
        cpu_convert_output(&dir, &input),
        dir.join("Beach day-cpu-3.webm")
    );
    let command = cpu_convert_command(&dir, &input).unwrap();
    assert_eq!(
        shell_words(&command).last().unwrap(),
        &dir.join("Beach day-cpu-3.webm").display().to_string()
    );
}

#[test]
fn the_convert_command_makes_a_cpu_ready_video() {
    let f = fx();
    let Some(src) = h264(&f.src.join("fast clip.mp4"), "320x240", 60) else {
        return;
    };
    if !has_encoder("libvpx-vp9") {
        eprintln!("skipped: no libvpx-vp9 encoder");
        return;
    }
    let out = f.root.join("out-cpu.webm");
    let command = ffmpeg::cpu_convert_command(&src, &out).unwrap();
    let run = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let i = probe_video(&out).unwrap();
    assert!(is_cpu_ready(&i), "{i:?}");
    assert_eq!((i.width, i.height), (320, 240));
}

// --- the cache's prune -------------------------------------------------

#[test]
fn removing_a_video_removes_its_poster() {
    let f = fx();
    let Some(src) = h264(&f.src.join("a.mp4"), "64x48", 10) else {
        return;
    };
    let kind = MediaKind::Background;
    let stored = f.store.import(kind, &src).unwrap();
    let cache = f.store.cache().unwrap().clone();
    let content = hash::to_hex(&hash::sha256(&std::fs::read(&stored).unwrap()));
    let poster = cache.poster_path(&content);
    assert!(poster.exists());
    f.store.warm_hash(&stored);
    f.store.remove(&stored).unwrap();
    assert!(!stored.exists() && !poster.exists());
}

#[test]
fn a_stopped_prune_removes_no_poster() {
    let f = fx();
    let cache = f.store.cache().unwrap().clone();
    std::fs::create_dir_all(cache.posters_dir()).unwrap();
    let orphan = cache.poster_path(&"3".repeat(64));
    std::fs::write(&orphan, b"x").unwrap();
    // Stopped (the service dropped): what to keep is not known in full.
    let stop = AtomicBool::new(true);
    let report = cache::prune(&cache, &f.store, None, &stop);
    assert!(report.removed.is_empty());
    assert!(orphan.exists());
}

#[test]
fn the_prune_removes_the_playback_copies_and_the_posters_no_video_needs() {
    let f = fx();
    let dir = f.store.user_dir(MediaKind::Background);
    std::fs::create_dir_all(&dir).unwrap();
    let Some(item) = h264(&dir.join("item.mp4"), "64x48", 10) else {
        return;
    };
    let outside = h264(&f.src.join("outside.mp4"), "64x48", 12).unwrap();
    let h = |p: &Path| hash::to_hex(&hash::sha256(&std::fs::read(p).unwrap()));
    let (hi, ho) = (h(&item), h(&outside));
    let cache = f.store.cache().unwrap().clone();
    // The copies an earlier version made, with what else was in there.
    let playback = cache.playback_dir();
    std::fs::create_dir_all(playback.join("nested")).unwrap();
    for name in [
        format!("{hi}-v1.webm"),
        ".part-1-0.tmp".into(),
        "notes.txt".into(),
    ] {
        std::fs::write(playback.join(name), b"x").unwrap();
    }
    std::fs::create_dir_all(cache.posters_dir()).unwrap();
    let put = |p: PathBuf| {
        std::fs::write(&p, b"x").unwrap();
        p
    };
    let item_poster = put(cache.poster_path(&hi));
    let outside_poster = put(cache.poster_path(&ho));
    let stray_poster = put(cache.poster_path(&"1".repeat(64)));
    let stranger = put(cache.posters_dir().join("notes.txt"));

    let report = cache::prune(&cache, &f.store, Some(&outside), &AtomicBool::new(false));
    let gone: BTreeSet<_> = report.removed.into_iter().collect();
    let want: BTreeSet<_> = [playback.clone(), stray_poster].into_iter().collect();
    assert_eq!(gone, want);
    assert!(!playback.exists());
    assert!(item_poster.exists() && outside_poster.exists() && stranger.exists());
    // Without the outside file named, its poster goes; no playback
    // directory is fine.
    let report = cache::prune(&cache, &f.store, None, &AtomicBool::new(false));
    assert_eq!(report.removed, [outside_poster]);
    // A link where the directory was is removed, never followed.
    let target = f.root.join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    let kept = put(target.join("kept"));
    std::os::unix::fs::symlink(&target, &playback).unwrap();
    cache::prune(&cache, &f.store, None, &AtomicBool::new(false));
    assert!(std::fs::symlink_metadata(&playback).is_err() && kept.exists());
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
