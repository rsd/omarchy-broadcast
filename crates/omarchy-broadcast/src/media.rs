//! `media`: the gallery of backgrounds and away images the daemon offers
//! (interface revision 15): list a kind's items, import a file into the
//! user's gallery (and select it as the background, or from revision 17
//! as the away image), remove an item of the user's gallery.
//!
//! `media` and `media list` never start the daemon; `media import` and
//! `media remove` may, through D-Bus activation.
//!
//! `media --json` prints a `media` line (the kind, the serial, the two
//! directories and how many items follow), then one `media_item` line per
//! item, each within [`json::MAX_LINE_BYTES`] like every other line.
//! Paths are never clipped, since clients open them: an item whose `path`
//! and `poster` together take more than [`MAX_PATH_BYTES`] bytes of JSON
//! encoding is left out, and a directory longer than that is printed as
//! `""` and its items left out; `items_truncated` says that items were
//! left out. At most [`MAX_ITEMS`] items are printed. A `label` is clipped
//! to [`MAX_LABEL_BYTES`]. `media import --json` prints the stored path
//! the same way: one longer is `""`, with `path_truncated`.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use obc_ipc::{MediaEntry, MediaReply};
use serde::Serialize;

use crate::bus::{self, EXIT_OK, Failure, READ_TIMEOUT};
use crate::human::printable;
use crate::json::{MAX_LINE_BYTES, VERSION, clip};
use crate::{CALL_TIMEOUT, IMPORT_TIMEOUT, out, report, within};

/// The kind listed when none is given.
pub const DEFAULT_KIND: &str = obc_ipc::media::KIND_BACKGROUND;
/// At most this many `media_item` lines follow a `media` line.
pub const MAX_ITEMS: usize = 500;
/// The most bytes of JSON encoding (escapes included, quotes excluded) of
/// an item's `path` and `poster` together, of a directory, and of an
/// imported path; past it the item is left out, the directory and the
/// path printed as `""` (a path is never clipped).
pub const MAX_PATH_BYTES: usize = 3800;
/// The longest `label` in JSON, in bytes of its encoding.
pub const MAX_LABEL_BYTES: usize = 96;
/// The longest `media_type` in JSON, in bytes.
const MAX_TYPE_BYTES: usize = 16;

/// The configuration key an away image selected by `media import away
/// --select` is written to.
pub const AWAY_IMAGE_KEY: &str = "effect.fallback_image";

/// The kinds `media import --select` can select.
const SELECTABLE: [&str; 2] = [obc_ipc::media::KIND_BACKGROUND, obc_ipc::media::KIND_AWAY];

/// The usage error of `media import KIND --select` for a kind that cannot
/// be selected (`None`: it can), checked before anything is sent.
pub fn select_refusal(kind: &str) -> Option<String> {
    (!SELECTABLE.contains(&kind))
        .then(|| format!("--select applies to `background` or `away`, not `{kind}`"))
}

/// The subcommands of `media`.
#[derive(Subcommand, Clone)]
pub enum MediaAction {
    /// List the items of a kind: the bundled ones, then yours.
    List {
        /// `background` (the default) or `away`.
        #[arg(value_parser = obc_ipc::media::KINDS)]
        kind: Option<String>,
        /// Print JSON lines: a `"type":"media"` line, then one
        /// `"type":"media_item"` line per item.
        #[arg(long)]
        json: bool,
    },
    /// Copy an image into your gallery, after the daemon checked it, and
    /// print the stored path. The same file imported again is not copied
    /// twice.
    Import {
        /// `background` or `away`.
        #[arg(value_parser = obc_ipc::media::KINDS)]
        kind: String,
        /// The file. A relative path is taken from the current directory;
        /// the daemon is given the absolute path.
        path: PathBuf,
        /// Then select it: a background as `effect replace --image <stored
        /// path>` does; an away image as the away image
        /// (`config set effect.fallback_image <stored path>`). Selecting an
        /// away image does not switch `output.placeholder`: set it to
        /// `image` (`config set output.placeholder image`) for the feed to
        /// show it while there is no camera picture.
        #[arg(long)]
        select: bool,
        /// Print one JSON line (`"type":"media_import"`).
        #[arg(long)]
        json: bool,
    },
    /// Remove an item of your gallery (bundled items cannot be removed).
    /// Removing the background in use clears it first, which the daemon
    /// refuses while `replace` is selected; a file the configuration names
    /// (`[effect] background_image` or `fallback_image`) is not removed.
    Remove {
        /// The item's path, as `media list` prints it.
        path: PathBuf,
    },
}

/// `path` made absolute against the current directory (not resolved:
/// the gallery's own paths are given as they are listed), as UTF-8 text,
/// which D-Bus needs. `Err` is the usage message.
pub fn absolute_text(path: &Path) -> Result<String, String> {
    let shown = path.display();
    let abs = std::path::absolute(path).map_err(|e| format!("{shown}: {e}"))?;
    abs.into_os_string().into_string().map_err(|p| {
        format!(
            "{}: the path is not valid UTF-8, which D-Bus cannot carry",
            Path::new(&p).display()
        )
    })
}

// ---------------------------------------------------------------------------
// Human output
// ---------------------------------------------------------------------------

/// The size of an item, `WxH`, or `?` when unknown.
fn size_text(e: &MediaEntry) -> String {
    if e.width == 0 || e.height == 0 {
        "?".into()
    } else {
        format!("{}x{}", e.width, e.height)
    }
}

/// `media list`: the two directories, then one line per item: its label,
/// type, size, `bundled` or `yours`, and path.
pub fn list_text(kind: &str, reply: &MediaReply) -> String {
    let mut out = format!(
        "Bundled: {}\nYours:   {}\n",
        printable(&reply.bundled_dir),
        printable(&reply.user_dir)
    );
    if reply.items.is_empty() {
        out.push_str(&format!("No {} items in the gallery\n", printable(kind)));
        return out;
    }
    for e in &reply.items {
        out.push_str(&format!(
            "{}  {}  {}  {}  {}\n",
            printable(&e.label),
            printable(&e.media_type),
            size_text(e),
            if e.bundled { "bundled" } else { "yours" },
            printable(&e.path)
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MediaLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind_type: &'static str,
    kind: &'a str,
    serial: u32,
    bundled_dir: &'a str,
    user_dir: &'a str,
    items: usize,
    items_truncated: bool,
}

#[derive(Serialize)]
struct ItemLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind_type: &'static str,
    kind: &'a str,
    path: &'a str,
    label: Cow<'a, str>,
    bundled: bool,
    media_type: Cow<'a, str>,
    width: u32,
    height: u32,
    duration_ms: u64,
    poster: &'a str,
}

#[derive(Serialize)]
struct ImportLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind_type: &'static str,
    kind: &'a str,
    path: &'a str,
    path_truncated: bool,
    selected: bool,
}

fn to_line<T: Serialize>(v: &T) -> String {
    let line = serde_json::to_string(v).expect("a line always serializes");
    debug_assert!(line.len() <= MAX_LINE_BYTES, "{} bytes", line.len());
    line
}

/// The bytes of `s`'s JSON encoding, escapes included, quotes excluded.
fn encoded_len(s: &str) -> usize {
    serde_json::to_string(s).map_or(usize::MAX, |e| e.len() - 2)
}

/// `dir`, or `""` when its encoding is over [`MAX_PATH_BYTES`].
fn bounded_dir(dir: &str) -> &str {
    if encoded_len(dir) <= MAX_PATH_BYTES {
        dir
    } else {
        ""
    }
}

/// The `media --json` lines of `kind`, each without its newline:
///
/// * `{"v":1,"type":"media","kind":K,"serial":S,"bundled_dir":D,
///   "user_dir":U,"items":N,"items_truncated":B}`, then
/// * `N` lines `{"v":1,"type":"media_item","kind":K,"path":P,"label":L,
///   "bundled":B,"media_type":T,"width":W,"height":H,"duration_ms":M,
///   "poster":Q}`, in the daemon's order.
///
/// The bounds are those of the module docs. Both directories at their
/// bound would not fit the `media` line together: then `bundled_dir` is
/// printed as `""` (and its items left out) too.
pub fn media_lines(kind: &str, reply: &MediaReply) -> Vec<String> {
    let mut bundled_dir = bounded_dir(&reply.bundled_dir);
    let user_dir = bounded_dir(&reply.user_dir);
    // Every other field of the `media` line takes well under this.
    const HEADER_ROOM: usize = 256;
    if encoded_len(bundled_dir) + encoded_len(user_dir) + HEADER_ROOM > MAX_LINE_BYTES {
        bundled_dir = "";
    }
    let dir_shown = |e: &MediaEntry| {
        let (dir, full) = if e.bundled {
            (bundled_dir, &reply.bundled_dir)
        } else {
            (user_dir, &reply.user_dir)
        };
        !dir.is_empty() || full.is_empty()
    };
    let fits = |e: &&MediaEntry| {
        dir_shown(e)
            && encoded_len(&e.path).saturating_add(encoded_len(&e.poster)) <= MAX_PATH_BYTES
    };
    let shown: Vec<&MediaEntry> = reply.items.iter().filter(fits).take(MAX_ITEMS).collect();
    let mut lines = Vec::with_capacity(shown.len() + 1);
    lines.push(to_line(&MediaLine {
        v: VERSION,
        kind_type: "media",
        kind,
        serial: reply.serial,
        bundled_dir,
        user_dir,
        items: shown.len(),
        items_truncated: shown.len() < reply.items.len(),
    }));
    lines.extend(shown.into_iter().map(|e| {
        to_line(&ItemLine {
            v: VERSION,
            kind_type: "media_item",
            kind,
            path: &e.path,
            label: clip(&e.label, MAX_LABEL_BYTES),
            bundled: e.bundled,
            media_type: clip(&e.media_type, MAX_TYPE_BYTES),
            width: e.width,
            height: e.height,
            duration_ms: e.duration_ms,
            poster: &e.poster,
        })
    }));
    lines
}

/// `{"v":1,"type":"media_import","kind":K,"path":P,"path_truncated":B,
/// "selected":B}`: the stored path of an import (never clipped: `""` with
/// `path_truncated` when its encoding is over [`MAX_PATH_BYTES`]), and
/// whether it was then selected (as the background, or as the away image).
pub fn import_line(kind: &str, path: &str, selected: bool) -> String {
    let fits = encoded_len(path) <= MAX_PATH_BYTES;
    to_line(&ImportLine {
        v: VERSION,
        kind_type: "media_import",
        kind,
        path: if fits { path } else { "" },
        path_truncated: !fits,
        selected,
    })
}

// ---------------------------------------------------------------------------
// Running the commands
// ---------------------------------------------------------------------------

/// The gallery of `kind`, without starting the daemon.
async fn read(kind: &str) -> Result<MediaReply, Failure> {
    let conn = bus::connect().await?;
    let snap = bus::fetch(&conn).await?;
    bus::require_media(snap.interface_revision)?;
    bus::media(&conn, kind).await
}

/// `media [list] [KIND] [--json]`.
pub async fn run_list(kind: Option<&str>, json: bool) -> u8 {
    let kind = kind.unwrap_or(DEFAULT_KIND);
    match within(READ_TIMEOUT, read(kind)).await {
        Ok(reply) => {
            if json {
                for line in media_lines(kind, &reply) {
                    out::line(&line);
                }
            } else {
                out::print(&list_text(kind, &reply));
            }
            EXIT_OK
        }
        Err(f) => report(&f, json),
    }
}

/// `media import KIND PATH [--select] [--json]`, the path already made
/// absolute: the import, then with `select` the background set to the
/// stored file and `replace` selected, or (`away`) the stored file set as
/// `effect.fallback_image` (`SetConfig`, revision 17).
///
/// The whole command is bound by [`IMPORT_TIMEOUT`], not [`CALL_TIMEOUT`]:
/// importing a video or an animation takes as long as the daemon's check,
/// copy and conversion do.
pub async fn run_import(kind: &str, path: &str, select: bool, json: bool) -> u8 {
    // The stored path, once the daemon has imported the file: a failure
    // after that is the selection's, and the message says so.
    let stored = std::cell::RefCell::new(None::<String>);
    let res = within(IMPORT_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_media(revision)?;
        let away = kind == obc_ipc::media::KIND_AWAY;
        if select && away {
            bus::require_away_image(revision)?;
        }
        let path = proxy
            .import_media(kind, path)
            .await
            .map_err(bus::classify_ipc)?;
        *stored.borrow_mut() = Some(path.clone());
        if select && away {
            proxy
                .set_config_value(AWAY_IMAGE_KEY, &obc_ipc::ConfigValue::Str(path.clone()))
                .await
                .map_err(bus::classify_ipc)?;
        } else if select {
            proxy
                .set_background_image(&path)
                .await
                .map_err(bus::classify_ipc)?;
            proxy
                .set_effect("replace")
                .await
                .map_err(bus::classify_ipc)?;
        }
        Ok(path)
    })
    .await;
    match res {
        Ok(path) => {
            if json {
                out::line(&import_line(kind, &path, select));
            } else {
                out::print(&format!("{}\n", printable(&path)));
            }
            EXIT_OK
        }
        Err(f) => match stored.into_inner() {
            Some(path) => {
                let msg = format!(
                    "{path} was imported, but selecting it failed: {}",
                    f.message()
                );
                if json {
                    out::line(&crate::json::error_line(f.json_code(), &msg));
                } else {
                    eprintln!("omarchy-broadcast: {}", printable(&msg));
                }
                f.exit_code()
            }
            None => report(&f, json),
        },
    }
}

/// `media remove PATH`, the path already made absolute.
pub async fn run_remove(path: &str) -> u8 {
    let res = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_media(revision)?;
        proxy.remove_media(path).await.map_err(bus::classify_ipc)
    })
    .await;
    match res {
        Ok(()) => {
            out::print(&format!("Removed {}\n", printable(path)));
            EXIT_OK
        }
        Err(f) => report(&f, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn entry(path: &str, label: &str, bundled: bool) -> MediaEntry {
        MediaEntry {
            path: path.into(),
            label: label.into(),
            bundled,
            media_type: "image".into(),
            width: 3840,
            height: 2160,
            duration_ms: 0,
            poster: String::new(),
        }
    }

    #[test]
    fn the_import_waits_longer_than_other_calls_but_within_the_plugin_watchdog() {
        use crate::{CALL_TIMEOUT, IMPORT_TIMEOUT};
        use std::time::Duration;
        assert!(IMPORT_TIMEOUT > CALL_TIMEOUT);
        // The daemon bounds an animation's conversion at 120 s.
        assert!(IMPORT_TIMEOUT >= Duration::from_secs(120 + 60));
        assert!(IMPORT_TIMEOUT <= Duration::from_secs(610));
    }

    fn reply(items: Vec<MediaEntry>) -> MediaReply {
        MediaReply {
            serial: 3,
            bundled_dir: "/usr/share/omarchy-broadcast/media/backgrounds".into(),
            user_dir: "/home/u/.local/share/omarchy-broadcast/media/backgrounds".into(),
            items,
        }
    }

    /// The lines parsed, each checked to be within the cap.
    fn parsed(lines: &[String]) -> Vec<Value> {
        lines
            .iter()
            .map(|l| {
                assert!(l.len() <= MAX_LINE_BYTES, "{} bytes", l.len());
                assert!(!l.contains('\n'));
                serde_json::from_str(l).unwrap()
            })
            .collect()
    }

    #[test]
    fn only_backgrounds_and_away_images_can_be_selected() {
        for kind in obc_ipc::media::KINDS {
            assert_eq!(select_refusal(kind), None, "{kind}");
        }
        assert_eq!(
            select_refusal("sound").as_deref(),
            Some("--select applies to `background` or `away`, not `sound`")
        );
    }

    #[test]
    fn the_media_lines_have_the_documented_shape() {
        let r = reply(vec![
            entry("/usr/share/a.webp", "Dusk", true),
            MediaEntry {
                media_type: "video".into(),
                width: 0,
                height: 0,
                duration_ms: 10_000,
                poster: "/home/u/.cache/p.jpg".into(),
                ..entry("/home/u/b.webm", "Loop", false)
            },
        ]);
        assert_eq!(
            media_lines("background", &r),
            [
                "{\"v\":1,\"type\":\"media\",\"kind\":\"background\",\"serial\":3,\
                 \"bundled_dir\":\"/usr/share/omarchy-broadcast/media/backgrounds\",\
                 \"user_dir\":\"/home/u/.local/share/omarchy-broadcast/media/backgrounds\",\
                 \"items\":2,\"items_truncated\":false}",
                "{\"v\":1,\"type\":\"media_item\",\"kind\":\"background\",\
                 \"path\":\"/usr/share/a.webp\",\"label\":\"Dusk\",\"bundled\":true,\
                 \"media_type\":\"image\",\"width\":3840,\"height\":2160,\"duration_ms\":0,\
                 \"poster\":\"\"}",
                "{\"v\":1,\"type\":\"media_item\",\"kind\":\"background\",\
                 \"path\":\"/home/u/b.webm\",\"label\":\"Loop\",\"bundled\":false,\
                 \"media_type\":\"video\",\"width\":0,\"height\":0,\"duration_ms\":10000,\
                 \"poster\":\"/home/u/.cache/p.jpg\"}",
            ]
        );
        assert_eq!(
            import_line("away", "/home/u/x.png", false),
            "{\"v\":1,\"type\":\"media_import\",\"kind\":\"away\",\"path\":\"/home/u/x.png\",\
             \"path_truncated\":false,\"selected\":false}"
        );
    }

    #[test]
    fn paths_are_never_clipped_and_long_ones_are_left_out() {
        // Measured after escaping: a path of quotes is twice its length.
        let long = format!("/home/u/{}", "\"".repeat(1900));
        let ok = format!("/home/u/{}x.png", "deep/".repeat(700));
        assert!(long.len() <= MAX_PATH_BYTES && encoded_len(&long) > MAX_PATH_BYTES);
        assert!(encoded_len(&ok) <= MAX_PATH_BYTES);
        // Within the bound alone, over it with its poster.
        let half = format!("/home/u/{}v.webm", "p/".repeat(1000));
        let with_poster = MediaEntry {
            poster: half.clone(),
            ..entry(&half, "v", false)
        };
        let r = reply(vec![
            entry(&long, "too long", false),
            entry(&ok, "fine", false),
            with_poster,
        ]);
        let v = parsed(&media_lines("background", &r));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["items"], 1);
        assert_eq!(v[0]["items_truncated"], true);
        assert_eq!(v[1]["type"], "media_item");
        assert_eq!(v[1]["path"], ok.as_str());

        // A directory over the bound is "" and its items left out.
        let mut r = reply(vec![
            entry("/usr/share/a.webp", "Dusk", true),
            entry("/home/u/b.png", "Mine", false),
        ]);
        r.user_dir = format!("/home/{}", "u\\".repeat(1900));
        let v = parsed(&media_lines("background", &r));
        assert_eq!(v[0]["user_dir"], "");
        assert_eq!(v[0]["bundled_dir"], r.bundled_dir.as_str());
        assert_eq!(
            (v[0]["items"].clone(), v[0]["items_truncated"].clone()),
            (1.into(), true.into())
        );
        assert_eq!(v[1]["bundled"], true);
        // Both within the bound but not together in a line: the bundled
        // one gives way.
        r.user_dir = format!("/home/{}", "u/".repeat(1890));
        r.bundled_dir = format!("/usr/{}", "s/".repeat(1890));
        let v = parsed(&media_lines("background", &r));
        assert_eq!(v[0]["bundled_dir"], "");
        assert_eq!(v[0]["user_dir"], r.user_dir.as_str());
        assert_eq!(v[0]["items"], 1);
        assert_eq!(v[1]["bundled"], false);

        // An imported path over the bound: "" and path_truncated.
        let v: Value = serde_json::from_str(&import_line("background", &long, true)).unwrap();
        assert_eq!(
            (v["path"].clone(), v["path_truncated"].clone()),
            ("".into(), true.into())
        );
        assert_eq!(v["selected"], true);
        let v: Value = serde_json::from_str(&import_line("background", &ok, false)).unwrap();
        assert_eq!(v["path"], ok.as_str());
    }

    #[test]
    fn every_line_at_every_bound_fits() {
        let path = format!("/{}", "\u{1}".repeat(MAX_PATH_BYTES / 12));
        assert!(encoded_len(&path) <= MAX_PATH_BYTES / 2);
        let worst = MediaEntry {
            label: "\u{1}".repeat(500),
            media_type: "\u{1}".repeat(500),
            width: u32::MAX,
            height: u32::MAX,
            duration_ms: u64::MAX,
            poster: path.clone(),
            ..entry(&path, "", false)
        };
        let mut r = reply(vec![worst]);
        r.serial = u32::MAX;
        r.user_dir = "x".repeat(MAX_PATH_BYTES);
        let v = parsed(&media_lines("background", &r));
        assert_eq!(v.len(), 2);
        let v = parsed(&[import_line(
            "background",
            &"\u{1}".repeat(MAX_PATH_BYTES / 6),
            true,
        )]);
        assert_eq!(v[0]["path_truncated"], false);
    }

    #[test]
    fn at_most_five_hundred_items_and_bounded_labels() {
        let label = format!("{}é", "\u{202e}\u{1}x".repeat(60));
        let items = (0..MAX_ITEMS + 3)
            .map(|i| entry(&format!("/g/{i}.png"), &label, false))
            .collect();
        let lines = media_lines("away", &reply(items));
        let v = parsed(&lines);
        assert_eq!(v.len(), MAX_ITEMS + 1);
        assert_eq!(v[0]["items"], MAX_ITEMS);
        assert_eq!(v[0]["items_truncated"], true);
        assert_eq!(v[MAX_ITEMS]["path"], "/g/499.png");
        let shown = v[1]["label"].as_str().unwrap();
        assert!(encoded_len(shown) <= MAX_LABEL_BYTES, "{shown}");
        assert!(shown.ends_with(crate::json::ELLIPSIS), "{shown}");
        // Control characters are escapes in the line, never raw.
        assert!(!lines[1].contains('\u{1}'));
        // Nothing to cut: not truncated, the header alone.
        let v = parsed(&media_lines("away", &reply(Vec::new())));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["items"], 0);
        assert_eq!(v[0]["items_truncated"], false);
    }

    #[test]
    fn the_list_text_shows_the_directories_and_each_item() {
        let r = reply(vec![
            entry("/usr/share/a.webp", "Dusk", true),
            MediaEntry {
                width: 0,
                ..entry("/home/u/\u{1b}[31mb.png", "Mine\u{202e}", false)
            },
        ]);
        assert_eq!(
            list_text("background", &r),
            "Bundled: /usr/share/omarchy-broadcast/media/backgrounds\n\
             Yours:   /home/u/.local/share/omarchy-broadcast/media/backgrounds\n\
             Dusk  image  3840x2160  bundled  /usr/share/a.webp\n\
             Mine\\u{202e}  image  ?  yours  /home/u/\\x1b[31mb.png\n"
        );
        assert!(list_text("away", &reply(Vec::new())).ends_with("No away items in the gallery\n"));
    }

    #[test]
    fn a_relative_path_is_made_absolute_against_the_current_directory() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            absolute_text(Path::new("pics/a.png")).unwrap(),
            cwd.join("pics/a.png").display().to_string()
        );
        assert_eq!(absolute_text(Path::new("/x/y.png")).unwrap(), "/x/y.png");
    }
}
