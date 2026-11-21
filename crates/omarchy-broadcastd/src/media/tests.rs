use std::collections::BTreeSet;
use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;

use super::*;

/// A PNG signature and IHDR chunk declaring `w` x `h`, an IDAT carrying
/// `tag` (so files differ), then IEND (CRCs not computed).
pub(crate) fn png(w: u32, h: u32, tag: u8) -> Vec<u8> {
    let chunk = |kind: &[u8; 4], data: &[u8]| {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0; 4]);
        v
    };
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend(chunk(b"IHDR", &ihdr));
    v.extend(chunk(b"IDAT", &[1, 2, tag]));
    v.extend(chunk(b"IEND", &[]));
    v
}

/// SOI, APP0, SOF0 declaring `w` x `h`, one scan carrying `tag`, EOI.
fn jpeg(w: u16, h: u16, tag: u8) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x07, b'J', b'F', b'I', b'F', 0]);
    v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 8]);
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&[1, 1, 0x11, 0]);
    v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 63, 0]);
    v.extend_from_slice(&[0x12, tag, 0xFF, 0xD9]);
    v
}

/// A RIFF chunk of `kind` with `data`, padded to even.
fn riff_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = kind.to_vec();
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    if data.len() % 2 == 1 {
        v.push(0);
    }
    v
}

/// A WebP RIFF container holding `chunks`, with the RIFF size set.
fn webp_file(chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
    v.extend_from_slice(b"WEBP");
    v.extend(body);
    v
}

/// A lossy `VP8 ` chunk declaring `w` x `h`.
fn vp8_chunk(w: u16, h: u16) -> Vec<u8> {
    let mut d = vec![0x10, 0x02, 0x00, 0x9D, 0x01, 0x2A];
    d.extend_from_slice(&w.to_le_bytes());
    d.extend_from_slice(&h.to_le_bytes());
    d.extend_from_slice(&[0xAB; 7]);
    riff_chunk(b"VP8 ", &d)
}

/// A lossless `VP8L` chunk declaring `w` x `h` (1 to 16384).
fn vp8l_chunk(w: u32, h: u32) -> Vec<u8> {
    let bits = (w - 1) | ((h - 1) << 14) | (1 << 28);
    let mut d = vec![0x2F];
    d.extend_from_slice(&bits.to_le_bytes());
    d.extend_from_slice(&[0x55; 4]);
    riff_chunk(b"VP8L", &d)
}

/// An extended `VP8X` chunk with `flags`, declaring a `w` x `h` canvas.
fn vp8x_chunk(flags: u8, w: u32, h: u32) -> Vec<u8> {
    let mut d = vec![flags, 0, 0, 0];
    d.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
    d.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
    riff_chunk(b"VP8X", &d)
}

/// Whether this system can decode WebP (`webpdec`); imports of WebP are
/// refused without it.
fn webp_decodes() -> bool {
    let ok = background::decoder_available(background::ImageFormat::Webp).is_ok();
    if !ok {
        eprintln!("webpdec is not installed: checking the refusal instead");
    }
    ok
}

struct Fixture {
    _tmp: tempfile::TempDir,
    store: MediaStore,
    /// A directory outside the gallery for import sources.
    src: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let store = MediaStore::new(root.join("bundled"), root.join("user"));
    Fixture {
        _tmp: tmp,
        store,
        src,
    }
}

fn write(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

fn names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .map(|e| {
            e.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn labels(items: &[MediaItem]) -> Vec<(&str, bool)> {
    items
        .iter()
        .map(|i| (i.label.as_str(), i.bundled))
        .collect()
}

#[test]
fn kinds_parse_and_name_their_directories() {
    assert_eq!(
        "background".parse::<MediaKind>().unwrap(),
        MediaKind::Background
    );
    assert_eq!("away".parse::<MediaKind>().unwrap(), MediaKind::Away);
    let e = "backgrounds".parse::<MediaKind>().unwrap_err().to_string();
    assert!(e.contains("\"backgrounds\""), "{e}");
    assert_eq!(MediaKind::Background.dir_name(), "backgrounds");
    assert_eq!(MediaKind::Away.dir_name(), "away");
    assert_eq!(MediaKind::Away.as_str(), "away");
    assert_eq!(MediaType::Video.as_str(), "video");
    assert_eq!(
        MediaType::from_path(Path::new("a.JPEG")),
        Some(MediaType::Image)
    );
    assert_eq!(
        MediaType::from_path(Path::new("a.webm")),
        Some(MediaType::Video)
    );
    assert_eq!(MediaType::from_path(Path::new("a.gif")), None);
}

/// An environment of `vars`, for [`MediaStore::from_env_in`].
fn env_of(vars: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<OsString> {
    move |k: &str| {
        vars.iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| OsString::from(v))
    }
}

#[test]
fn the_user_root_is_in_the_pictures_directory_of_user_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config = home.join(".config");
    std::fs::create_dir_all(&config).unwrap();
    let h = home.display().to_string();
    let root_for = |line: &str, vars: Vec<(&'static str, String)>| {
        std::fs::write(config.join("user-dirs.dirs"), line).unwrap();
        MediaStore::from_env_in(env_of(vars))
            .unwrap()
            .user_root()
            .to_path_buf()
    };
    let only_home = || vec![("HOME", h.clone())];

    // `$HOME/...`, the form xdg-user-dirs writes, with comments around.
    assert_eq!(
        root_for(
            "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\n\
             XDG_PICTURES_DIR=\"$HOME/Bilder\"\n",
            only_home()
        ),
        home.join("Bilder/omarchy-broadcast")
    );
    // An absolute path, with spaces around `=` and an escaped quote; the
    // last line setting it wins.
    assert_eq!(
        root_for(
            "XDG_PICTURES_DIR=\"$HOME/old\"\n  XDG_PICTURES_DIR = \"/srv/My \\\"pics\\\"\"\n",
            only_home()
        ),
        Path::new("/srv/My \"pics\"/omarchy-broadcast")
    );
    // Ignored: relative, `$HOME` itself (disabled), malformed.
    for bad in [
        "XDG_PICTURES_DIR=\"Pictures\"",
        "XDG_PICTURES_DIR=\"$HOME\"",
        "XDG_PICTURES_DIR=\"$HOME/\"",
        &format!("XDG_PICTURES_DIR=\"{h}\""),
        "XDG_PICTURES_DIR=\"$HOMEX/p\"",
        "XDG_PICTURES_DIR=$HOME/p",
        "XDG_PICTURES_DIR=\"$HOME/unterminated",
        "XDG_PICTURES_DIRS=\"/x\"",
    ] {
        assert_eq!(
            root_for(bad, only_home()),
            home.join("Pictures/omarchy-broadcast"),
            "{bad}"
        );
    }
    // XDG_CONFIG_HOME is read first; a relative one is ignored.
    let other = tmp.path().join("cfg");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("user-dirs.dirs"),
        "XDG_PICTURES_DIR=\"$HOME/Imagens\"\n",
    )
    .unwrap();
    assert_eq!(
        root_for(
            "XDG_PICTURES_DIR=\"$HOME/Bilder\"\n",
            vec![
                ("HOME", h.clone()),
                ("XDG_CONFIG_HOME", other.display().to_string())
            ]
        ),
        home.join("Imagens/omarchy-broadcast")
    );
    assert_eq!(
        root_for(
            "XDG_PICTURES_DIR=\"$HOME/Bilder\"\n",
            vec![("HOME", h.clone()), ("XDG_CONFIG_HOME", "cfg".into())]
        ),
        home.join("Bilder/omarchy-broadcast")
    );
    // No file: $HOME/Pictures. The data directory plays no part.
    std::fs::remove_file(config.join("user-dirs.dirs")).unwrap();
    let s = MediaStore::from_env_in(env_of(vec![
        ("HOME", h.clone()),
        ("XDG_DATA_HOME", "/d".into()),
    ]))
    .unwrap();
    assert_eq!(s.user_root(), home.join("Pictures/omarchy-broadcast"));
    assert_eq!(s.bundled_root(), Path::new(BUNDLED_ROOT));
    // The cache is unchanged.
    assert_eq!(
        s.cache().unwrap().root(),
        home.join(".cache/omarchy-broadcast")
    );
    // No HOME (or a relative one): no gallery.
    for vars in [vec![], vec![("HOME", "rel".to_string())]] {
        let e = MediaStore::from_env_in(env_of(vars)).unwrap_err();
        assert!(matches!(e, MediaError::NoHome), "{e}");
    }
}

#[test]
fn backgrounds_are_the_root_and_away_images_its_away_directory() {
    let f = fixture();
    let root = f.store.user_root().to_path_buf();
    assert_eq!(f.store.user_dir(MediaKind::Background), root);
    assert_eq!(f.store.user_dir(MediaKind::Away), root.join("away"));
    // The bundled tree keeps its kind directories.
    assert_eq!(
        f.store.bundled_dir(MediaKind::Background),
        f.store.bundled_root().join("backgrounds")
    );

    // Each kind lists only its own files: never the `away` directory, the
    // hidden `.sources`, or a directory with an image's name.
    write(&root.join("bg.png"), &png(10, 10, 1));
    write(&root.join("away/me.png"), &png(10, 10, 2));
    write(&root.join(".sources/x"), b"bg.png");
    std::fs::create_dir_all(root.join("dir.png")).unwrap();
    write(&root.join("away.png"), &png(10, 10, 3));
    let paths = |kind| -> Vec<PathBuf> { f.store.list(kind).into_iter().map(|i| i.path).collect() };
    assert_eq!(
        paths(MediaKind::Background),
        [root.join("away.png"), root.join("bg.png")]
    );
    assert_eq!(paths(MediaKind::Away), [root.join("away/me.png")]);

    // A kind is the directory's exactly.
    assert_eq!(
        f.store.is_gallery_item(&root.join("bg.png")),
        Some((MediaKind::Background, false))
    );
    assert_eq!(
        f.store.is_gallery_item(&root.join("away/me.png")),
        Some((MediaKind::Away, false))
    );
    assert_eq!(f.store.is_gallery_item(&root.join("away")), None);
    assert_eq!(f.store.is_gallery_item(&root.join(".sources/x")), None);
    write(&root.join("away/sub/deep.png"), &png(10, 10, 4));
    assert_eq!(
        f.store.is_gallery_item(&root.join("away/sub/deep.png")),
        None
    );

    // An import goes to its kind's directory; the same bytes in the other
    // kind are not a duplicate.
    let src = f.src.join("me.png");
    write(&src, &png(10, 10, 2));
    let bg = f.store.import(MediaKind::Background, &src).unwrap();
    assert_eq!(bg, root.join("me.png"));
    assert_eq!(
        f.store.import(MediaKind::Away, &src).unwrap(),
        root.join("away/me.png")
    );

    // Removals: an away item, then a background; the other kind stays.
    f.store.remove(&root.join("away/me.png")).unwrap();
    assert!(!root.join("away/me.png").exists());
    assert!(bg.exists());
    f.store.remove(&bg).unwrap();
    assert!(!bg.exists());
    assert!(root.join("away").is_dir());
    let e = f.store.remove(&root.join("away")).unwrap_err();
    assert!(matches!(e, MediaError::NotAUserItem(_)), "{e}");
    let e = f.store.remove(&root.join(".sources/x")).unwrap_err();
    assert!(matches!(e, MediaError::NotAUserItem(_)), "{e}");
    assert_eq!(
        names(&root.join("away")),
        BTreeSet::from(["sub".to_string()])
    );
}

#[test]
fn the_user_directories_are_made_private() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let dir = f.store.ensure_user_dir(MediaKind::Away).unwrap();
    assert_eq!(dir, f.store.user_root().join("away"));
    for d in [f.store.user_root(), dir.as_path()] {
        let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", d.display());
    }
}

/// A store whose videos root is a folder of its own (`<root>/Videos/…`
/// beside `<root>/Pictures/…`), as a session's is.
fn two_folders() -> (Fixture, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let pictures = root.join("Pictures/omarchy-broadcast");
    let videos = root.join("Videos/omarchy-broadcast");
    let store = MediaStore::new(root.join("bundled"), &pictures).with_video_root(&videos);
    (
        Fixture {
            _tmp: tmp,
            store,
            src,
        },
        pictures,
        videos,
    )
}

#[test]
fn the_videos_root_is_in_the_videos_directory_of_user_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config = home.join(".config");
    std::fs::create_dir_all(&config).unwrap();
    let h = home.display().to_string();
    let roots_for = |text: &str| {
        std::fs::write(config.join("user-dirs.dirs"), text).unwrap();
        let s = MediaStore::from_env_in(env_of(vec![("HOME", h.clone())])).unwrap();
        (s.user_root().to_path_buf(), s.video_root().to_path_buf())
    };
    // Each key read on its own, the same way.
    assert_eq!(
        roots_for(
            "XDG_PICTURES_DIR=\"$HOME/Bilder\"\nXDG_VIDEOS_DIR=\"$HOME/Filme\"\n\
             XDG_VIDEOS_DIRS=\"/x\"\n"
        ),
        (
            home.join("Bilder/omarchy-broadcast"),
            home.join("Filme/omarchy-broadcast")
        )
    );
    assert_eq!(
        roots_for("XDG_VIDEOS_DIR=\"/srv/clips\"\n").1,
        Path::new("/srv/clips/omarchy-broadcast")
    );
    // Missing, disabled or malformed: $HOME/Videos.
    for text in [
        "",
        "XDG_VIDEOS_DIR=\"$HOME\"",
        "XDG_VIDEOS_DIR=\"Videos\"",
        "XDG_VIDEOS_DIRS=\"/x\"",
    ] {
        assert_eq!(
            roots_for(text).1,
            home.join("Videos/omarchy-broadcast"),
            "{text}"
        );
    }
    let env = env_of(vec![("HOME", h.clone())]);
    assert_eq!(videos_dir_in(&env), Some(home.join("Videos")));
    assert_eq!(videos_dir_in(env_of(vec![])), None);
    // A plain store keeps one folder.
    let one = MediaStore::new("/b", "/u");
    assert_eq!(one.video_root(), Path::new("/u"));
    assert_eq!(one.user_dirs(MediaKind::Background), [PathBuf::from("/u")]);
}

#[test]
fn backgrounds_are_listed_from_both_folders_and_away_from_pictures_only() {
    let (f, pictures, videos) = two_folders();
    assert_eq!(
        f.store.video_dir(MediaKind::Background),
        Some(videos.clone())
    );
    assert_eq!(f.store.video_dir(MediaKind::Away), None);
    assert_eq!(
        f.store.user_dirs(MediaKind::Background),
        [pictures.clone(), videos.clone()]
    );
    assert_eq!(f.store.user_dirs(MediaKind::Away), [pictures.join("away")]);
    assert_eq!(f.store.convert_dir(), videos);

    // Made at start, private, the videos root on its own.
    use std::os::unix::fs::PermissionsExt;
    f.store.ensure_user_dir(MediaKind::Background).unwrap();
    assert!(!videos.exists());
    assert_eq!(f.store.ensure_video_dir(MediaKind::Away).unwrap(), None);
    assert_eq!(
        f.store.ensure_video_dir(MediaKind::Background).unwrap(),
        Some(videos.clone())
    );
    let mode = std::fs::metadata(&videos).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);

    // Files dropped by hand in either folder are listed, sorted together;
    // a still in the videos folder too. Away lists neither.
    write(&pictures.join("b.png"), &png(10, 10, 1));
    write(&videos.join("a still.png"), &png(10, 10, 2));
    write(&videos.join("C.png"), &png(10, 10, 3));
    write(&videos.join(".sources/x"), b"a.webm");
    write(&videos.join("sub/deep.png"), &png(10, 10, 4));
    let paths = |kind| -> Vec<PathBuf> { f.store.list(kind).into_iter().map(|i| i.path).collect() };
    assert_eq!(
        paths(MediaKind::Background),
        [
            videos.join("a still.png"),
            pictures.join("b.png"),
            videos.join("C.png")
        ]
    );
    assert!(paths(MediaKind::Away).is_empty());

    // A still is imported into Pictures, but one already in Videos with
    // the same bytes is a duplicate.
    let src = f.src.join("x.png");
    write(&src, &png(10, 10, 2));
    assert_eq!(
        f.store.import_stored(MediaKind::Background, &src).unwrap(),
        (videos.join("a still.png"), false)
    );
    write(&src, &png(10, 10, 9));
    assert_eq!(
        f.store.import(MediaKind::Background, &src).unwrap(),
        pictures.join("x.png")
    );

    // Either folder's file is an item of the kind, removed in place; a
    // file deeper down or beside the folder is not.
    assert_eq!(
        f.store.is_gallery_item(&videos.join("C.png")),
        Some((MediaKind::Background, false))
    );
    for not_item in [
        videos.join("sub/deep.png"),
        videos.join(".sources/x"),
        videos.parent().unwrap().join("C.png"),
    ] {
        write(&videos.parent().unwrap().join("C.png"), &png(10, 10, 5));
        let e = f.store.remove(&not_item).unwrap_err();
        assert!(matches!(e, MediaError::NotAUserItem(_)), "{e}");
        assert!(not_item.exists());
    }
    f.store.remove(&videos.join("C.png")).unwrap();
    assert!(!videos.join("C.png").exists());
    assert_eq!(
        names(&videos),
        BTreeSet::from([
            ".sources".to_string(),
            "a still.png".to_string(),
            "sub".to_string()
        ])
    );
}

#[test]
fn one_folder_for_pictures_and_videos_is_listed_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let pictures = root.join("Pictures/omarchy-broadcast");
    std::fs::create_dir_all(&pictures).unwrap();
    // The videos root a link to the same folder.
    std::fs::create_dir_all(root.join("Videos")).unwrap();
    std::os::unix::fs::symlink(&pictures, root.join("Videos/omarchy-broadcast")).unwrap();
    let store = MediaStore::new(root.join("bundled"), &pictures)
        .with_video_root(root.join("Videos/omarchy-broadcast"));
    assert_eq!(
        store.user_dirs(MediaKind::Background),
        std::slice::from_ref(&pictures)
    );
    write(&pictures.join("one.png"), &png(10, 10, 1));
    let items = store.list(MediaKind::Background);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(
        store.video_paths(MediaKind::Background),
        Vec::<PathBuf>::new()
    );
    store.ensure_user_dir(MediaKind::Background).unwrap();
    store.remove(&pictures.join("one.png")).unwrap();
}

#[test]
fn stale_temporary_files_go_from_both_folders() {
    let (f, pictures, videos) = two_folders();
    let old = SystemTime::now() - STALE_TEMP_AGE - Duration::from_secs(60);
    for dir in [&pictures, &videos] {
        let t = dir.join(".import-1-1.tmp");
        write(&t, b"x");
        File::options()
            .write(true)
            .open(&t)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    let src = f.src.join("y.png");
    write(&src, &png(10, 10, 7));
    f.store.import(MediaKind::Background, &src).unwrap();
    assert!(!pictures.join(".import-1-1.tmp").exists());
    assert!(!videos.join(".import-1-1.tmp").exists());
}

#[test]
fn the_default_background_is_the_first_bundled_still() {
    let f = fixture();
    assert_eq!(f.store.default_background(), None, "no bundled directory");
    // The user's own background is never the default.
    write(
        &f.store.user_dir(MediaKind::Background).join("mine.png"),
        &png(10, 10, 9),
    );
    assert_eq!(f.store.default_background(), None);
    let bundled = f.store.bundled_dir(MediaKind::Background);
    write(&bundled.join("b.png"), &png(10, 10, 2));
    write(&bundled.join("A.png"), &png(10, 10, 3));
    write(&bundled.join("c.png"), &png(10, 10, 4));
    write(
        &bundled.join(SOURCES_FILE),
        b"[[item]]\nfile = \"c.png\"\ntitle = \"C\"\n",
    );
    // SOURCES.toml order first, then by name: the same order as the listing.
    assert_eq!(f.store.default_background(), Some(bundled.join("c.png")));
    assert_eq!(
        f.store.list(MediaKind::Background)[0].path,
        bundled.join("c.png")
    );
}

#[test]
fn bundled_items_come_first_in_sources_order_then_user_items_by_name() {
    let f = fixture();
    let bundled = f.store.bundled_dir(MediaKind::Background);
    write(&bundled.join("a.jpg"), &jpeg(640, 480, 1));
    write(&bundled.join("b.png"), &png(1920, 1080, 2));
    write(&bundled.join("warm-bokeh.png"), &png(10, 10, 3));
    write(&bundled.join("Calm_sea.png"), &png(10, 10, 4));
    write(
        &bundled.join(SOURCES_FILE),
        br#"
[[item]]
file = "b.png"
title = "Bee title"
author = "x"
licence = "CC0-1.0"
source = "y"

[[item]]
file = "a.jpg"
title = "  "
"#,
    );
    let user = f.store.user_dir(MediaKind::Background);
    write(&user.join("Zed.png"), &png(10, 10, 5));
    write(&user.join("alpha.png"), &png(10, 10, 6));
    write(&user.join("Beta.jpg"), &jpeg(10, 10, 7));

    let items = f.store.list(MediaKind::Background);
    assert_eq!(
        labels(&items),
        [
            ("Bee title", true),
            // An empty title falls back to the stem.
            ("A", true),
            ("Calm sea", true),
            ("Warm bokeh", true),
            ("Alpha", false),
            ("Beta", false),
            ("Zed", false),
        ]
    );
    assert_eq!(items[0].path, bundled.join("b.png"));
    assert_eq!((items[0].width, items[0].height), (1920, 1080));
    assert_eq!((items[1].width, items[1].height), (640, 480));
    assert_eq!(items[0].media_type, MediaType::Image);
    assert_eq!(items[0].duration_ms, 0);
    assert_eq!(items[0].poster, None);
    assert_eq!(items[4].path, user.join("alpha.png"));
    // The other kind is separate.
    assert!(f.store.list(MediaKind::Away).is_empty());
}

#[test]
fn a_broken_sources_file_only_loses_the_titles() {
    let f = fixture();
    let bundled = f.store.bundled_dir(MediaKind::Away);
    write(&bundled.join("dusk-gradient.png"), &png(10, 10, 1));
    write(&bundled.join(SOURCES_FILE), b"[[item]\nfile = ");
    assert_eq!(
        labels(&f.store.list(MediaKind::Away)),
        [("Dusk gradient", true)]
    );
}

#[test]
fn listing_skips_bad_headers_links_directories_and_special_files() {
    let f = fixture();
    let user = f.store.user_dir(MediaKind::Background);
    let bundled = f.store.bundled_dir(MediaKind::Background);
    let good = f.src.join("good.png");
    write(&good, &png(10, 10, 1));
    write(&user.join("ok.png"), &png(10, 10, 2));
    write(&user.join("junk.png"), b"not an image at all");
    write(&user.join("huge.png"), &png(9000, 10, 3));
    write(&user.join("notes.txt"), b"hello");
    write(&user.join(".hidden.png"), &png(10, 10, 4));
    std::fs::create_dir_all(user.join("sub.png")).unwrap();
    write(&user.join("sub.png").join("inner.png"), &png(10, 10, 5));
    std::os::unix::fs::symlink(&good, user.join("link.png")).unwrap();
    mkfifo(&user.join("pipe.png"));
    // A video the check refuses is skipped as a bad image header is.
    write(&user.join("clip.mp4"), b"\0\0\0\x18ftypmp42");
    // A bundled symbolic link is followed (packaging may link files).
    std::fs::create_dir_all(&bundled).unwrap();
    std::os::unix::fs::symlink(&good, bundled.join("shared.png")).unwrap();

    let items = f.store.list(MediaKind::Background);
    assert_eq!(labels(&items), [("Shared", true), ("Ok", false)]);
}

#[test]
fn a_jpeg_frame_header_past_a_large_segment_is_found() {
    let f = fixture();
    let user = f.store.user_dir(MediaKind::Background);
    // SOI, three 65535-byte APP1 segments (about 192 KiB), then the rest.
    let mut data = vec![0xFF, 0xD8];
    for _ in 0..3 {
        data.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xFF]);
        data.extend(std::iter::repeat_n(0u8, 65_533));
    }
    data.extend_from_slice(&jpeg(320, 200, 1)[2..]);
    write(&user.join("exif.jpg"), &data);
    let items = f.store.list(MediaKind::Background);
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].width, items[0].height), (320, 200));
}

#[test]
fn importing_copies_under_the_detected_format_extension() {
    let f = fixture();
    let src = f.src.join("Holiday photo.png");
    write(&src, &jpeg(800, 600, 1));
    let stored = f.store.import(MediaKind::Background, &src).unwrap();
    let dir = f.store.user_dir(MediaKind::Background);
    assert_eq!(stored, dir.join("Holiday photo.jpg"));
    assert_eq!(std::fs::read(&stored).unwrap(), jpeg(800, 600, 1));
    // No temporary file is left behind.
    assert_eq!(names(&dir), BTreeSet::from(["Holiday photo.jpg".into()]));

    let src = f.src.join("plain.jpeg");
    write(&src, &png(16, 9, 2));
    let stored = f.store.import(MediaKind::Away, &src).unwrap();
    assert_eq!(stored, f.store.user_dir(MediaKind::Away).join("plain.png"));
    let items = f.store.list(MediaKind::Away);
    assert_eq!(labels(&items), [("Plain", false)]);
    assert_eq!((items[0].width, items[0].height), (16, 9));
}

#[test]
fn importing_without_hard_links_renames_and_never_overwrites() {
    LINKS_REFUSED.with(|c| c.set(true));
    let f = fixture();
    let a = f.src.join("one/shot.png");
    let b = f.src.join("two/shot.png");
    write(&a, &png(10, 10, 1));
    write(&b, &png(10, 10, 2));
    let dir = f.store.user_dir(MediaKind::Background);
    assert_eq!(
        f.store.import(MediaKind::Background, &a).unwrap(),
        dir.join("shot.png")
    );
    assert_eq!(
        f.store.import(MediaKind::Background, &b).unwrap(),
        dir.join("shot-2.png")
    );
    assert_eq!(std::fs::read(dir.join("shot.png")).unwrap(), png(10, 10, 1));
    assert_eq!(
        std::fs::read(dir.join("shot-2.png")).unwrap(),
        png(10, 10, 2)
    );
    // No temporary file is left behind.
    assert_eq!(
        names(&dir),
        BTreeSet::from(["shot.png".into(), "shot-2.png".into()])
    );
    // The rename itself refuses an existing name.
    let other = dir.join("other");
    write(&other, b"x");
    let err = rename_noreplace(&other, &dir.join("shot.png")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    LINKS_REFUSED.with(|c| c.set(false));
}

#[test]
fn importing_the_same_bytes_again_returns_the_stored_file() {
    let f = fixture();
    let a = f.src.join("a.png");
    let b = f.src.join("renamed copy.png");
    write(&a, &png(10, 10, 1));
    write(&b, &png(10, 10, 1));
    let first = f.store.import(MediaKind::Background, &a).unwrap();
    assert_eq!(f.store.import(MediaKind::Background, &a).unwrap(), first);
    assert_eq!(f.store.import(MediaKind::Background, &b).unwrap(), first);
    // `import_stored` tells a copy from a file found in the gallery.
    assert_eq!(
        f.store.import_stored(MediaKind::Background, &b).unwrap(),
        (first.clone(), false)
    );
    let dir = f.store.user_dir(MediaKind::Background);
    assert_eq!(names(&dir), BTreeSet::from(["a.png".into()]));
    let c = f.src.join("c.png");
    write(&c, &png(10, 10, 3));
    assert_eq!(
        f.store.import_stored(MediaKind::Background, &c).unwrap(),
        (dir.join("c.png"), true)
    );
    // The same bytes in the other kind are a separate item.
    let away = f.store.import(MediaKind::Away, &a).unwrap();
    assert_eq!(away, f.store.user_dir(MediaKind::Away).join("a.png"));
}

#[test]
fn a_taken_name_gets_a_numeric_suffix() {
    let f = fixture();
    let dir = f.store.user_dir(MediaKind::Background);
    let mut stored = Vec::new();
    for tag in 1..=3 {
        let src = f.src.join(format!("{tag}")).join("x.png");
        write(&src, &png(10, 10, tag));
        stored.push(f.store.import(MediaKind::Background, &src).unwrap());
    }
    assert_eq!(
        stored,
        [dir.join("x.png"), dir.join("x-2.png"), dir.join("x-3.png")]
    );
    for (i, path) in stored.iter().enumerate() {
        assert_eq!(std::fs::read(path).unwrap(), png(10, 10, i as u8 + 1));
    }
    // A name taken by something else (a directory) is skipped too.
    std::fs::create_dir(dir.join("y.png")).unwrap();
    let src = f.src.join("y.png");
    write(&src, &png(10, 10, 9));
    assert_eq!(
        f.store.import(MediaKind::Background, &src).unwrap(),
        dir.join("y-2.png")
    );
}

#[test]
fn stored_names_are_sanitised() {
    let fb = "background";
    assert_eq!(sanitise_stem("\u{202E}gnp.exe", fb), "gnp.exe");
    assert_eq!(sanitise_stem("a\u{2066}b\u{200F}c\u{061C}", fb), "abc");
    assert_eq!(sanitise_stem("a\u{1}b\u{7F}c", fb), "abc");
    assert_eq!(sanitise_stem("  two \n\t lines  ", fb), "two lines");
    assert_eq!(sanitise_stem("a/b", fb), "ab");
    assert_eq!(sanitise_stem("..", fb), fb);
    assert_eq!(sanitise_stem(" ...hidden", fb), "hidden");
    assert_eq!(sanitise_stem("\u{202E}\u{1} ", "away"), "away");
    // Cut at 96 bytes on a character boundary (é is two bytes, € three).
    let s = sanitise_stem(&"é".repeat(100), fb);
    assert_eq!(s, "é".repeat(48));
    let s = sanitise_stem(&"€".repeat(40), fb);
    assert_eq!(s.len(), 96);
    assert_eq!(s, "€".repeat(32));
    let s = sanitise_stem(&format!("a{}", "€".repeat(40)), fb);
    assert_eq!(s.len(), 94);
    assert!(sanitise_stem(&"x".repeat(500), fb).len() == MAX_STEM_BYTES);

    let f = fixture();
    let dir = f.store.user_dir(MediaKind::Away);
    let src = f.src.join("\u{202E}evil\u{7}.png");
    write(&src, &png(10, 10, 1));
    assert_eq!(
        f.store.import(MediaKind::Away, &src).unwrap(),
        dir.join("evil.png")
    );
    let src = f.src.join("..png");
    write(&src, &png(10, 10, 2));
    assert_eq!(
        f.store.import(MediaKind::Away, &src).unwrap(),
        dir.join("away.png")
    );
}

#[test]
fn imports_refuse_what_the_background_checks_refuse() {
    let f = fixture();
    let kind = MediaKind::Background;
    let err = |src: &Path| f.store.import(kind, src).unwrap_err().to_string();

    // A video signature with nothing playable behind it.
    let video = f.src.join("clip.MP4");
    write(&video, b"\0\0\0\x18ftypmp42");
    let e = err(&video);
    assert!(e.contains("could not be read as a video"), "{e}");
    assert!(e.contains("clip.MP4"), "{e}");
    // Neither an image nor a video.
    let text = f.src.join("notes.png");
    write(&text, b"hello, not an image");
    let e = err(&text);
    assert!(e.contains("is not an accepted image"), "{e}");
    assert!(e.contains("notes.png"), "{e}");

    let e = err(Path::new("relative/x.png"));
    assert!(e.contains("relative/x.png is not an absolute path"), "{e}");

    let big = f.src.join("big.png");
    write(&big, &png(10, 10, 1));
    let file = std::fs::File::options().write(true).open(&big).unwrap();
    file.set_len(MAX_FILE_BYTES + 1).unwrap();
    let e = err(&big);
    assert!(e.contains("32 MiB"), "{e}");
    assert!(e.contains("big.png"), "{e}");

    let fifo = f.src.join("pipe.png");
    mkfifo(&fifo);
    let e = err(&fifo);
    assert!(e.contains("not a regular file"), "{e}");

    // A GIF (whatever its name) goes to the transcode, which junk fails.
    let gif = f.src.join("anim.png");
    write(&gif, b"GIF89a................");
    let e = err(&gif);
    assert!(e.contains("anim.png"), "{e}");
    assert!(
        e.contains("converting the animation to a video failed") || e.contains("needs ffmpeg"),
        "{e}"
    );

    let huge = f.src.join("huge.png");
    write(&huge, &png(9000, 100, 1));
    let e = err(&huge);
    assert!(e.contains("9000x100"), "{e}");
    assert!(e.contains("8192x8192"), "{e}");
    let zero = f.src.join("zero.png");
    write(&zero, &png(0, 100, 1));
    assert!(err(&zero).contains("0x100"));

    let truncated = f.src.join("cut.png");
    let mut data = png(10, 10, 1);
    data.truncate(data.len() - 12);
    write(&truncated, &data);
    let e = err(&truncated);
    assert!(e.contains("IEND"), "{e}");

    let missing = f.src.join("missing.png");
    assert!(err(&missing).contains("missing.png"));

    // Nothing landed in the gallery.
    assert!(names(&f.store.user_dir(kind)).is_empty());
}

#[test]
fn only_regular_files_of_the_user_root_are_removed() {
    let f = fixture();
    let src = f.src.join("mine.png");
    write(&src, &png(10, 10, 1));
    let stored = f.store.import(MediaKind::Background, &src).unwrap();
    assert_eq!(
        f.store.is_gallery_item(&stored),
        Some((MediaKind::Background, false))
    );
    f.store.remove(&stored).unwrap();
    assert!(!stored.exists());
    assert!(src.exists());

    let not_user = |path: &Path| {
        let e = f.store.remove(path).unwrap_err();
        assert!(matches!(e, MediaError::NotAUserItem(_)), "{path:?}: {e}");
        assert!(e.to_string().contains(&*path.to_string_lossy()), "{e}");
    };
    let bundled = f.store.bundled_dir(MediaKind::Away).join("b.png");
    write(&bundled, &png(10, 10, 2));
    assert_eq!(
        f.store.is_gallery_item(&bundled),
        Some((MediaKind::Away, true))
    );
    not_user(&bundled);
    assert!(bundled.exists());

    not_user(&src);
    let user = f.store.user_dir(MediaKind::Background);
    let link = user.join("link.png");
    std::os::unix::fs::symlink(&src, &link).unwrap();
    assert_eq!(f.store.is_gallery_item(&link), None);
    not_user(&link);
    assert!(link.symlink_metadata().is_ok());
    assert!(src.exists());

    let nested = user.join("sub").join("n.png");
    write(&nested, &png(10, 10, 3));
    not_user(&nested);
    std::fs::create_dir(user.join("d.png")).unwrap();
    not_user(&user.join("d.png"));
    not_user(&user.join("gone.png"));
    // A path that walks out of the kind directory.
    not_user(&user.join("..").join("..").join("src").join("mine.png"));
    // A user directory reached through a symbolic link is still the one.
    let alias = f.src.join("alias");
    std::os::unix::fs::symlink(&user, &alias).unwrap();
    let again = f.store.import(MediaKind::Background, &src).unwrap();
    let via_alias = alias.join(again.file_name().unwrap());
    f.store.remove(&via_alias).unwrap();
    assert!(!again.exists());

    let e = f.store.remove(Path::new("x.png")).unwrap_err();
    assert!(matches!(e, MediaError::NotAbsolute(_)), "{e}");

    // Only what the listing shows: no hidden file, no other file.
    for name in [".sources", ".hidden.png", "notes.txt", "README"] {
        let p = user.join(name);
        write(&p, b"x");
        not_user(&p);
        assert!(p.exists(), "{name}");
    }
}

#[test]
fn an_image_is_imported_when_the_videos_folder_cannot_be_made() {
    let (f, pictures, videos) = two_folders();
    // A file where the videos folder's parent should be.
    write(videos.parent().unwrap(), b"not a directory");
    let src = f.src.join("mine.png");
    write(&src, &png(10, 10, 1));
    let stored = f.store.import(MediaKind::Background, &src).unwrap();
    assert_eq!(stored, pictures.join("mine.png"));
    assert!(f.store.ensure_video_dir(MediaKind::Background).is_err());
}

/// Every bundled file of the repository is described in its directory's
/// `SOURCES.toml` (exactly once, with a title, an author, an accepted
/// licence and a source), every entry names an existing file, and every
/// image passes the checks an import makes. A shipped poster
/// (`<stem>.poster.jpg`) is a derived file, not an item: it has no entry,
/// its video exists, and it is a JPEG the cache accepts.
#[test]
fn bundled_assets_are_covered_by_their_sources() {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/media"));
    const LICENCES: &[&str] = &["CC0-1.0", "MIT OR Apache-2.0"];
    for kind in MediaKind::ALL {
        let dir = root.join(kind.dir_name());
        if !dir.is_dir() {
            continue;
        }
        let text = std::fs::read_to_string(dir.join(SOURCES_FILE))
            .unwrap_or_else(|e| panic!("{}: {e}", dir.join(SOURCES_FILE).display()));
        let sources: Sources = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.join(SOURCES_FILE).display()));
        for entry in &sources.item {
            let field = |name: &str, v: &Option<String>| {
                let v = v.as_deref().unwrap_or("").trim().to_string();
                assert!(
                    !v.is_empty(),
                    "{}: an entry has no {name}: {entry:?}",
                    dir.display()
                );
                v
            };
            let file = field("file", &entry.file);
            field("title", &entry.title);
            field("author", &entry.author);
            field("source", &entry.source);
            let licence = field("licence", &entry.licence);
            assert!(
                LICENCES.contains(&licence.as_str()),
                "{file}: licence {licence:?} is not one of {LICENCES:?}"
            );
            assert!(dir.join(&file).is_file(), "{file} is listed but missing");
        }
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == SOURCES_FILE || name.starts_with("LICENSE") {
                continue;
            }
            if let Some(stem) = name.strip_suffix(SHIPPED_POSTER_SUFFIX) {
                let video = VIDEO_EXTENSIONS
                    .iter()
                    .any(|ext| dir.join(format!("{stem}.{ext}")).is_file());
                assert!(video, "{name} is the poster of no video");
                assert!(
                    !sources
                        .item
                        .iter()
                        .any(|e| e.file.as_deref() == Some(name.as_str())),
                    "{name} is derived: no SOURCES.toml entry"
                );
                let data = std::fs::read(entry.path()).unwrap();
                assert!(data.starts_with(cache::JPEG_MAGIC), "{name}");
                assert!(data.len() < 8 << 20, "{name}");
                continue;
            }
            let count = sources
                .item
                .iter()
                .filter(|e| e.file.as_deref() == Some(name.as_str()))
                .count();
            assert_eq!(
                count,
                1,
                "{}/{name} has {count} SOURCES.toml entries",
                dir.display()
            );
            let path = entry.path();
            if MediaType::from_path(&path) != Some(MediaType::Image) {
                continue;
            }
            let data = background::read_image_file(&path.canonicalize().unwrap())
                .unwrap_or_else(|e| panic!("{e}"));
            let (format, w, h) =
                background::image_header(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
            background::check_dimensions(&name, w, h).unwrap_or_else(|e| panic!("{e}"));
            background::image_length(format, &data).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
}

#[test]
fn webp_content_is_stored_and_listed_as_webp() {
    let f = fixture();
    let dir = f.store.user_dir(MediaKind::Background);
    let src = f.src.join("x.png");
    let data = webp_file(&[vp8l_chunk(640, 360)]);
    write(&src, &data);
    let result = f.store.import(MediaKind::Background, &src);
    if !webp_decodes() {
        let e = result.unwrap_err();
        assert!(matches!(e, MediaError::NoDecoder { .. }), "{e}");
        assert!(e.to_string().contains("webpdec"), "{e}");
        assert!(names(&dir).is_empty());
        return;
    }
    assert_eq!(result.unwrap(), dir.join("x.webp"));
    // An extended WebP with an alpha chunk before its bitstream.
    let src = f.src.join("alpha.jpg");
    write(
        &src,
        &webp_file(&[
            vp8x_chunk(0x10, 320, 200),
            riff_chunk(b"ALPH", &[0; 5]),
            vp8_chunk(320, 200),
        ]),
    );
    assert_eq!(
        f.store.import(MediaKind::Background, &src).unwrap(),
        dir.join("alpha.webp")
    );
    let items = f.store.list(MediaKind::Background);
    let sizes: Vec<_> = items
        .iter()
        .map(|i| (i.label.as_str(), i.width, i.height))
        .collect();
    assert_eq!(sizes, [("Alpha", 320, 200), ("X", 640, 360)]);
}

#[test]
fn animated_or_oversized_webp_is_refused() {
    let f = fixture();
    let kind = MediaKind::Background;
    let err = |name: &str, data: &[u8]| {
        let src = f.src.join(name);
        write(&src, data);
        f.store.import(kind, &src).unwrap_err().to_string()
    };
    let e = err(
        "anim.webp",
        &webp_file(&[vp8x_chunk(0x12, 64, 64), riff_chunk(b"ANIM", &[0; 6])]),
    );
    // An animated WebP goes to the transcode, which this one fails (or
    // needs ffmpeg where there is none).
    assert!(
        e.contains("converting the animation to a video failed") || e.contains("needs ffmpeg"),
        "{e}"
    );
    let e = err(
        "big.webp",
        &webp_file(&[vp8x_chunk(0x00, 9000, 10), vp8_chunk(9000, 10)]),
    );
    assert!(e.contains("9000x10") && e.contains("8192x8192"), "{e}");
    // A canvas that is not the size of the image the decoder would make.
    let e = err(
        "liar.webp",
        &webp_file(&[vp8x_chunk(0x00, 64, 64), vp8l_chunk(8192, 8192)]),
    );
    assert!(e.contains("canvas and its image differ"), "{e}");
    assert!(names(&f.store.user_dir(kind)).is_empty());
}

#[test]
fn dedupe_only_matches_image_files() {
    let f = fixture();
    let dir = f.store.user_dir(MediaKind::Background);
    let data = png(10, 10, 1);
    // The same bytes under a name that is not listed as an image.
    write(&dir.join("decoy.txt"), &data);
    write(&dir.join(".hidden.png"), &data);
    let src = f.src.join("real.png");
    write(&src, &data);
    let stored = f.store.import(MediaKind::Background, &src).unwrap();
    assert_eq!(stored, dir.join("real.png"));
    assert_eq!(f.store.import(MediaKind::Background, &src).unwrap(), stored);
}

#[test]
fn format_characters_are_stripped_from_names_and_labels() {
    let fb = "background";
    assert_eq!(
        sanitise_stem("a\u{200B}b\u{00AD}c\u{FEFF}d\u{2060}e\u{180E}f", fb),
        "abcdef"
    );
    assert_eq!(sanitise_stem("\u{2063}\u{E0041}\u{200D}", fb), fb);
    assert_eq!(label_from_stem("zero\u{200B}width-name"), "Zerowidth name");
    assert_eq!(label_from_stem("\u{202E}\u{FEFF}"), "Untitled");
    assert_eq!(label_from_stem("---"), "---");

    let f = fixture();
    let user = f.store.user_dir(MediaKind::Away);
    // A file dropped into the directory by hand, not imported.
    write(&user.join("\u{2060}so\u{00AD}ft-drop.png"), &png(10, 10, 1));
    assert_eq!(
        labels(&f.store.list(MediaKind::Away)),
        [("Soft drop", false)]
    );
    let src = f.src.join("in\u{200C}visible\u{FEFF}.png");
    write(&src, &png(10, 10, 2));
    assert_eq!(
        f.store.import(MediaKind::Away, &src).unwrap(),
        user.join("invisible.png")
    );
}

#[test]
fn a_jpeg_with_over_a_mebibyte_before_its_frame_imports_and_lists() {
    let f = fixture();
    // SOI, seventeen 65535-byte APP1 segments (about 1.06 MiB), then the
    // rest.
    let mut data = vec![0xFF, 0xD8];
    for _ in 0..17 {
        data.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xFF]);
        data.extend(std::iter::repeat_n(0u8, 65_533));
    }
    assert!(data.len() > 1024 * 1024);
    data.extend_from_slice(&jpeg(320, 200, 1)[2..]);
    let src = f.src.join("exif.jpg");
    write(&src, &data);
    let stored = f.store.import(MediaKind::Background, &src).unwrap();
    let items = f.store.list(MediaKind::Background);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].path, stored);
    assert_eq!((items[0].width, items[0].height), (320, 200));
}

#[test]
fn an_away_import_names_the_image_file() {
    let f = fixture();
    let missing = f.src.join("missing.png");
    let e = f
        .store
        .import(MediaKind::Away, &missing)
        .unwrap_err()
        .to_string();
    assert!(e.contains("the image file"), "{e}");
    assert!(e.contains("missing.png"), "{e}");
    assert!(!e.contains("background"), "{e}");
}

#[test]
fn an_interrupted_import_is_cleaned_up_later() {
    let f = fixture();
    let dir = f.store.user_dir(MediaKind::Background);
    let old = dir.join(".import-1-0.tmp");
    let young = dir.join(".import-1-1.tmp");
    write(&old, b"partial");
    write(&young, b"partial");
    let file = std::fs::File::options().write(true).open(&old).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(11 * 60))
        .unwrap();
    let src = f.src.join("a.png");
    write(&src, &png(10, 10, 1));
    f.store.import(MediaKind::Background, &src).unwrap();
    assert_eq!(
        names(&dir),
        BTreeSet::from(["a.png".into(), ".import-1-1.tmp".into()])
    );
}
