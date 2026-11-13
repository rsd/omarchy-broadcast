use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;

use obc_core::config_d::ConfigSet;

use super::*;
use crate::media::tests::png;

struct Fixture {
    _tmp: tempfile::TempDir,
    store: MediaStore,
    /// `<data>/omarchy-broadcast`.
    data: PathBuf,
    /// `<data>/omarchy-broadcast/media`.
    old: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let store = MediaStore::new(
        root.join("bundled"),
        root.join("Pictures").join("omarchy-broadcast"),
    );
    let data = root.join("data/omarchy-broadcast");
    Fixture {
        _tmp: tmp,
        store,
        old: data.join("media"),
        data,
        config: root.join("config/config.toml"),
        state: root.join("state/state.toml"),
    }
}

impl Fixture {
    fn run(&self) -> Outcome {
        self.run_for(usize::MAX)
    }

    /// A run with time for `files` files.
    fn run_for(&self, files: usize) -> Outcome {
        let mut started = 0;
        run(
            &self.store,
            &self.old,
            &self.config,
            &self.state,
            &mut || {
                started += 1;
                started > files
            },
        )
    }

    fn old_dir(&self, kind: MediaKind) -> PathBuf {
        self.old.join(kind.dir_name())
    }

    fn new_dir(&self, kind: MediaKind) -> PathBuf {
        self.store.user_dir(kind)
    }
}

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

fn names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .map(|e| {
            e.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn outcome(migrated: usize, kept: usize, failed: usize) -> Outcome {
    Outcome {
        migrated,
        kept,
        failed,
        ..Outcome::default()
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn moves_files_and_records(copy: bool) {
    FORCE_COPY.with(|c| c.set(copy));
    let f = fixture();
    let bg = f.old_dir(MediaKind::Background);
    let away = f.old_dir(MediaKind::Away);
    write(&bg.join("office.png"), &png(10, 10, 1));
    write(&bg.join("party.webm"), b"a transcoded animation");
    write(&bg.join(TRANSCODED_DIR).join(HASH), b"party.webm\n");
    write(&away.join("me.png"), &png(10, 10, 2));
    fs::set_permissions(away.join("me.png"), fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(f.run(), outcome(3, 0, 0));
    let new_bg = f.new_dir(MediaKind::Background);
    let new_away = f.new_dir(MediaKind::Away);
    assert_eq!(
        names(&new_bg),
        set(&["office.png", "party.webm", "away", TRANSCODED_DIR])
    );
    assert_eq!(fs::read(new_bg.join("office.png")).unwrap(), png(10, 10, 1));
    assert_eq!(
        fs::read(new_bg.join("party.webm")).unwrap(),
        b"a transcoded animation"
    );
    assert_eq!(fs::read(new_away.join("me.png")).unwrap(), png(10, 10, 2));
    assert_eq!(
        fs::read_to_string(new_bg.join(TRANSCODED_DIR).join(HASH)).unwrap(),
        "party.webm"
    );
    // The modes an import gives; no temporary file is left.
    assert_eq!(mode(&new_away.join("me.png")), 0o600);
    assert_eq!(mode(f.store.user_root()), 0o700);
    assert_eq!(mode(&new_away), 0o700);
    assert_eq!(names(&new_away), set(&["me.png"]));
    // Everything old went, the data directory with it (it held nothing
    // else).
    assert!(!f.data.exists());
    // A rerun has nothing to do.
    assert_eq!(f.run(), Outcome::default());
    assert_eq!(names(&new_away), set(&["me.png"]));
    FORCE_COPY.with(|c| c.set(false));
}

#[test]
fn files_and_their_records_move_by_link() {
    moves_files_and_records(false);
}

#[test]
fn files_and_their_records_move_by_a_verified_copy() {
    moves_files_and_records(true);
}

#[test]
fn an_absent_old_gallery_is_nothing_to_do() {
    let f = fixture();
    assert_eq!(f.run(), Outcome::default());
    assert!(!f.store.user_root().exists());
    // A file where the old root was is not a gallery either.
    write(&f.old, b"x");
    assert_eq!(f.run(), Outcome::default());
    assert!(f.old.exists());
}

#[test]
fn the_old_root_comes_from_xdg_data_home_then_home() {
    let env = |vars: &'static [(&str, &str)]| {
        move |k: &str| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        }
    };
    assert_eq!(
        old_root_in(env(&[("XDG_DATA_HOME", "/d"), ("HOME", "/h")])),
        Some(PathBuf::from("/d/omarchy-broadcast/media"))
    );
    assert_eq!(
        old_root_in(env(&[("XDG_DATA_HOME", "rel"), ("HOME", "/h")])),
        Some(PathBuf::from("/h/.local/share/omarchy-broadcast/media"))
    );
    assert_eq!(old_root_in(env(&[])), None);
}

#[test]
fn a_name_holding_the_same_bytes_is_the_file_already_moved() {
    let f = fixture();
    write(&f.old_dir(MediaKind::Away).join("me.png"), &png(10, 10, 1));
    write(&f.new_dir(MediaKind::Away).join("me.png"), &png(10, 10, 1));
    assert_eq!(f.run(), outcome(1, 0, 0));
    assert_eq!(names(&f.new_dir(MediaKind::Away)), set(&["me.png"]));
    assert!(!f.old.exists());
}

#[test]
fn a_name_holding_other_bytes_gets_a_numeric_suffix() {
    let f = fixture();
    let old = f.old_dir(MediaKind::Background);
    let new = f.new_dir(MediaKind::Background);
    write(&old.join("party.webm"), b"the old animation");
    write(&old.join(TRANSCODED_DIR).join(HASH), b"party.webm");
    write(&old.join("plain"), b"no extension");
    write(&new.join("party.webm"), b"another file");
    write(&new.join("plain"), b"another plain file");
    assert_eq!(f.run(), outcome(2, 0, 0));
    assert_eq!(
        fs::read(new.join("party-2.webm")).unwrap(),
        b"the old animation"
    );
    assert_eq!(fs::read(new.join("party.webm")).unwrap(), b"another file");
    assert_eq!(fs::read(new.join("plain-2")).unwrap(), b"no extension");
    // The record follows the new name.
    assert_eq!(
        fs::read_to_string(new.join(TRANSCODED_DIR).join(HASH)).unwrap(),
        "party-2.webm"
    );
}

#[test]
fn a_partly_moved_gallery_is_finished_without_duplicates() {
    let f = fixture();
    let old = f.old_dir(MediaKind::Background);
    let new = f.new_dir(MediaKind::Background);
    // An earlier run placed party-2.webm but could not remove the old one.
    write(&old.join("party.webm"), b"the old animation");
    write(&new.join("party.webm"), b"another file");
    write(&new.join("party-2.webm"), b"the old animation");
    assert_eq!(f.run(), outcome(1, 0, 0));
    assert_eq!(names(&new), set(&["party.webm", "party-2.webm", "away"]));
}

#[test]
fn links_hidden_and_temporary_files_stay() {
    let f = fixture();
    let old = f.old_dir(MediaKind::Background);
    write(&old.join("mine.png"), &png(10, 10, 1));
    let outside = f.data.parent().unwrap().join("outside.png");
    write(&outside, &png(10, 10, 2));
    std::os::unix::fs::symlink(&outside, old.join("link.png")).unwrap();
    // A fresh temporary file of an import (a stale one is removed).
    write(&old.join(".import-1-1.tmp"), b"partial");
    write(&old.join(".hidden.png"), &png(10, 10, 3));
    fs::create_dir_all(old.join("sub.png")).unwrap();
    // Another program's directory in the data directory.
    fs::create_dir_all(f.data.join("modules.d")).unwrap();

    assert_eq!(f.run(), outcome(1, 0, 0));
    assert_eq!(
        names(&f.new_dir(MediaKind::Background)),
        set(&["mine.png", "away"])
    );
    assert_eq!(
        names(&old),
        set(&["link.png", ".import-1-1.tmp", ".hidden.png", "sub.png"])
    );
    assert!(
        fs::symlink_metadata(old.join("link.png"))
            .unwrap()
            .is_symlink()
    );
    assert!(outside.exists());
    // The empty away directory went; the rest stays.
    assert_eq!(names(&f.old), set(&["backgrounds"]));
    assert!(f.data.join("modules.d").is_dir());
}

#[test]
fn an_empty_old_gallery_leaves_a_busy_data_directory() {
    let f = fixture();
    write(&f.old_dir(MediaKind::Away).join("me.png"), &png(10, 10, 1));
    write(&f.data.join("modules.d/x.toml"), b"");
    assert_eq!(f.run(), outcome(1, 0, 0));
    assert!(!f.old.exists());
    assert_eq!(names(&f.data), set(&["modules.d"]));
}

#[test]
fn a_file_that_fails_stays_and_the_others_move() {
    // SAFETY: getuid has no preconditions.
    if unsafe { libc::getuid() } == 0 {
        eprintln!("running as root: an unreadable file cannot be made");
        return;
    }
    let f = fixture();
    let old = f.old_dir(MediaKind::Background);
    write(&old.join("a.png"), &png(10, 10, 1));
    write(&old.join("b.png"), &png(10, 10, 2));
    fs::set_permissions(old.join("a.png"), fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(f.run(), outcome(1, 0, 1));
    assert_eq!(names(&old), set(&["a.png"]));
    assert_eq!(
        names(&f.new_dir(MediaKind::Background)),
        set(&["b.png", "away"])
    );
    // Retried at the next start.
    fs::set_permissions(old.join("a.png"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(f.run(), outcome(1, 0, 0));
    assert!(!f.old.exists());
}

#[test]
fn the_managed_file_and_the_state_are_rewritten_to_the_new_paths() {
    let f = fixture();
    let old_away = f.old_dir(MediaKind::Away).join("me.png");
    let old_bg = f.old_dir(MediaKind::Background).join("office.png");
    write(&old_away, &png(10, 10, 1));
    write(&old_bg, &png(10, 10, 2));
    let managed = f.config.parent().unwrap().join(MANAGED_REL);
    write(
        &managed,
        format!("[effect]\nfallback_image = \"{}\"\n", old_away.display()).as_bytes(),
    );
    let state = RuntimeState {
        background_image: Some(old_bg.clone()),
        ..RuntimeState::default()
    };
    state.save(&f.state).unwrap();

    assert_eq!(f.run(), outcome(2, 0, 0));
    let new_away = f.new_dir(MediaKind::Away).join("me.png");
    let new_bg = f.new_dir(MediaKind::Background).join("office.png");
    let set = ConfigSet::load(&f.config).unwrap();
    assert_eq!(
        set.config.effect.fallback_image.as_deref(),
        Some(&*new_away)
    );
    assert_eq!(
        RuntimeState::load(&f.state).0.background_image.as_deref(),
        Some(&*new_bg)
    );
    assert!(!old_away.exists() && !old_bg.exists());
}

#[test]
fn a_file_another_config_file_names_is_copied_and_kept() {
    let f = fixture();
    let old = f.old_dir(MediaKind::Background).join("office.png");
    write(&old, &png(10, 10, 1));
    write(
        &f.config,
        format!(
            "[effect]\nmode = \"replace\"\nbackground_image = \"{}\"\n",
            old.display()
        )
        .as_bytes(),
    );
    assert_eq!(f.run(), outcome(0, 1, 0));
    let new = f.new_dir(MediaKind::Background).join("office.png");
    assert_eq!(fs::read(&new).unwrap(), png(10, 10, 1));
    assert_eq!(fs::read(&old).unwrap(), png(10, 10, 1));
    // config.toml is not touched.
    assert!(
        fs::read_to_string(&f.config)
            .unwrap()
            .contains(&*old.to_string_lossy())
    );
    // Remembered: a rerun leaves it, and a gallery copy the user removed
    // does not come back.
    assert_eq!(
        names(&f.old_dir(MediaKind::Background)),
        set(&["office.png", KEPT_RECORD])
    );
    let remembered = Outcome {
        remembered: 1,
        ..Outcome::default()
    };
    assert_eq!(f.run(), remembered);
    fs::remove_file(&new).unwrap();
    assert_eq!(f.run(), remembered);
    assert!(!new.exists());
    // Once nothing names it, it is moved as any other.
    write(&f.config, b"[effect]\n");
    assert_eq!(f.run(), outcome(1, 0, 0));
    assert_eq!(fs::read(&new).unwrap(), png(10, 10, 1));
    assert!(!f.old.exists());
}

#[test]
fn a_kept_file_that_changed_is_placed_again() {
    let f = fixture();
    let old = f.old_dir(MediaKind::Background).join("office.png");
    write(&old, &png(10, 10, 1));
    write(
        &f.config,
        format!("[effect]\nbackground_image = \"{}\"\n", old.display()).as_bytes(),
    );
    FORCE_COPY.with(|c| c.set(true));
    assert_eq!(f.run(), outcome(0, 1, 0));
    // A new file under the old name (another size).
    fs::remove_file(&old).unwrap();
    write(&old, &png(20, 20, 2));
    assert_eq!(f.run(), outcome(0, 1, 0));
    FORCE_COPY.with(|c| c.set(false));
    assert_eq!(
        names(&f.new_dir(MediaKind::Background)),
        set(&["office.png", "office-2.png", "away"])
    );
    assert_eq!(
        fs::read(f.new_dir(MediaKind::Background).join("office-2.png")).unwrap(),
        png(20, 20, 2)
    );
}

#[test]
fn a_start_out_of_time_leaves_the_rest_for_the_next() {
    let f = fixture();
    let old = f.old_dir(MediaKind::Background);
    for n in 1..=3 {
        write(&old.join(format!("{n}.png")), &png(10, 10, n));
    }
    write(&f.old_dir(MediaKind::Away).join("me.png"), &png(10, 10, 9));
    assert_eq!(
        f.run_for(1),
        Outcome {
            migrated: 1,
            deferred: 3,
            ..Outcome::default()
        }
    );
    assert_eq!(names(&old), set(&["2.png", "3.png"]));
    assert_eq!(
        names(&f.new_dir(MediaKind::Background)),
        set(&["1.png", "away"])
    );
    // No time at all: nothing is started.
    assert_eq!(
        f.run_for(0),
        Outcome {
            deferred: 3,
            ..Outcome::default()
        }
    );
    assert_eq!(f.run(), outcome(3, 0, 0));
    assert!(!f.old.exists());
}

#[test]
fn an_old_background_named_away_does_not_take_the_away_directory() {
    let f = fixture();
    write(
        &f.old_dir(MediaKind::Background).join("away"),
        &png(10, 10, 1),
    );
    write(&f.old_dir(MediaKind::Away).join("me.png"), &png(10, 10, 2));
    assert_eq!(f.run(), outcome(2, 0, 0));
    let root = f.new_dir(MediaKind::Background);
    assert_eq!(names(&root), set(&["away", "away-2"]));
    assert!(f.new_dir(MediaKind::Away).is_dir());
    assert_eq!(fs::read(root.join("away-2")).unwrap(), png(10, 10, 1));
    assert_eq!(names(&f.new_dir(MediaKind::Away)), set(&["me.png"]));
}

#[test]
fn a_gallery_another_process_is_moving_is_left_to_it() {
    use std::os::fd::AsRawFd;
    let f = fixture();
    write(&f.old_dir(MediaKind::Away).join("me.png"), &png(10, 10, 1));
    let held = File::open(&f.old).unwrap();
    // SAFETY: flock on a descriptor the test owns.
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(f.run(), Outcome::default());
    assert!(f.old_dir(MediaKind::Away).join("me.png").exists());
    drop(held);
    assert_eq!(f.run(), outcome(1, 0, 0));
}

#[test]
fn a_symlinked_old_root_is_not_followed() {
    let f = fixture();
    let elsewhere = f.data.parent().unwrap().join("elsewhere");
    write(&elsewhere.join("away/me.png"), &png(10, 10, 1));
    fs::create_dir_all(&f.data).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &f.old).unwrap();
    assert_eq!(f.run(), Outcome::default());
    assert!(elsewhere.join("away/me.png").exists());
}

#[test]
fn without_hard_links_a_copy_is_renamed_into_place() {
    FORCE_COPY.with(|c| c.set(true));
    crate::media::LINKS_REFUSED.with(|c| c.set(true));
    let f = fixture();
    let bg = f.old_dir(MediaKind::Background);
    write(&bg.join("party.webm"), b"a transcoded animation");
    write(&bg.join(TRANSCODED_DIR).join(HASH), b"party.webm\n");
    write(
        &f.new_dir(MediaKind::Background).join("party.webm"),
        b"other",
    );
    assert_eq!(f.run(), outcome(1, 0, 0));
    crate::media::LINKS_REFUSED.with(|c| c.set(false));
    FORCE_COPY.with(|c| c.set(false));
    let new_bg = f.new_dir(MediaKind::Background);
    assert_eq!(
        names(&new_bg),
        set(&["party.webm", "party-2.webm", "away", TRANSCODED_DIR])
    );
    assert_eq!(
        fs::read(new_bg.join("party-2.webm")).unwrap(),
        b"a transcoded animation"
    );
    assert_eq!(
        fs::read_to_string(new_bg.join(TRANSCODED_DIR).join(HASH)).unwrap(),
        "party-2.webm"
    );
    assert!(!f.old.exists());
}

#[test]
fn a_configuration_that_cannot_be_read_keeps_every_old_file() {
    let f = fixture();
    let old = f.old_dir(MediaKind::Background).join("office.png");
    write(&old, &png(10, 10, 1));
    write(
        &f.config.parent().unwrap().join("config.d/60-mine.toml"),
        b"[effect\n",
    );
    assert_eq!(f.run(), outcome(0, 1, 0));
    assert!(old.exists());
    assert!(f.new_dir(MediaKind::Background).join("office.png").exists());
    // Still unreadable: remembered, not placed again.
    assert_eq!(
        f.run(),
        Outcome {
            remembered: 1,
            ..Outcome::default()
        }
    );
    // Readable again: moved.
    fs::remove_file(f.config.parent().unwrap().join("config.d/60-mine.toml")).unwrap();
    assert_eq!(f.run(), outcome(1, 0, 0));
    assert!(!old.exists());
}

/// The fixture's user root replaced by a symbolic link to `target`.
fn link_user_root(f: &Fixture, target: &Path) {
    let root = f.store.user_root();
    fs::create_dir_all(root.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, root).unwrap();
}

#[test]
fn a_user_root_linked_to_the_old_gallery_leaves_it_as_it_is() {
    for copy in [false, true] {
        FORCE_COPY.with(|c| c.set(copy));
        let f = fixture();
        let bg = f.old_dir(MediaKind::Background);
        let away = f.old_dir(MediaKind::Away);
        write(&bg.join("office.png"), &png(10, 10, 1));
        write(&bg.join(TRANSCODED_DIR).join(HASH), b"office.png\n");
        write(&away.join("me.png"), &png(10, 10, 2));
        link_user_root(&f, &f.old);
        // The user root is the old root: away/ is the old away/, and the
        // old backgrounds/ sits in the root.
        assert_eq!(f.run(), Outcome::default());
        assert_eq!(names(&bg), set(&["office.png", TRANSCODED_DIR]));
        assert_eq!(fs::read(bg.join("office.png")).unwrap(), png(10, 10, 1));
        assert_eq!(
            fs::read(bg.join(TRANSCODED_DIR).join(HASH)).unwrap(),
            b"office.png\n"
        );
        assert_eq!(names(&away), set(&["me.png"]));
        assert_eq!(fs::read(away.join("me.png")).unwrap(), png(10, 10, 2));
        assert_eq!(names(&f.old), set(&["backgrounds", "away"]));
        // Again: still nothing.
        assert_eq!(f.run(), Outcome::default());
        assert_eq!(fs::read(away.join("me.png")).unwrap(), png(10, 10, 2));
        assert_eq!(fs::read(bg.join("office.png")).unwrap(), png(10, 10, 1));
        FORCE_COPY.with(|c| c.set(false));
    }
}

#[test]
fn a_user_root_linked_to_the_old_backgrounds_leaves_them_as_they_are() {
    for copy in [false, true] {
        FORCE_COPY.with(|c| c.set(copy));
        let f = fixture();
        let bg = f.old_dir(MediaKind::Background);
        let away = f.old_dir(MediaKind::Away);
        write(&bg.join("office.png"), &png(10, 10, 1));
        write(&bg.join("party.webm"), b"a transcoded animation");
        write(&bg.join(TRANSCODED_DIR).join(HASH), b"party.webm\n");
        write(&away.join("me.png"), &png(10, 10, 2));
        link_user_root(&f, &bg);
        // The new away/ is under the old backgrounds/ now: nothing moves,
        // the away images stay too, their bytes intact.
        assert_eq!(f.run(), Outcome::default());
        assert_eq!(
            names(&bg),
            set(&["office.png", "party.webm", "away", TRANSCODED_DIR])
        );
        assert_eq!(fs::read(bg.join("office.png")).unwrap(), png(10, 10, 1));
        assert_eq!(
            fs::read(bg.join("party.webm")).unwrap(),
            b"a transcoded animation"
        );
        assert_eq!(
            fs::read(bg.join(TRANSCODED_DIR).join(HASH)).unwrap(),
            b"party.webm\n"
        );
        assert_eq!(names(&away), set(&["me.png"]));
        assert_eq!(fs::read(away.join("me.png")).unwrap(), png(10, 10, 2));
        assert_eq!(names(&f.new_dir(MediaKind::Away)), set(&[]));
        assert_eq!(f.run(), Outcome::default());
        assert_eq!(fs::read(bg.join("office.png")).unwrap(), png(10, 10, 1));
        assert_eq!(fs::read(away.join("me.png")).unwrap(), png(10, 10, 2));
        FORCE_COPY.with(|c| c.set(false));
    }
}

#[test]
fn a_file_is_never_placed_in_its_own_directory() {
    let f = fixture();
    let bg = f.old_dir(MediaKind::Background);
    let src = bg.join("office.png");
    write(&src, &png(10, 10, 1));
    assert!(place(&src, &bg, &[], &[]).is_err());
    // Through a symbolic link to the directory too.
    let alias = f.data.join("alias");
    std::os::unix::fs::symlink(&bg, &alias).unwrap();
    assert!(place(&src, &alias, &[], &[]).is_err());
    // Nor anywhere in the old gallery.
    let away = f.old_dir(MediaKind::Away);
    fs::create_dir_all(&away).unwrap();
    let old = [f.old.canonicalize().unwrap()];
    assert!(place(&src, &away, &[], &old).is_err());
    assert_eq!(names(&away), set(&[]));
    assert_eq!(names(&bg), set(&["office.png"]));
    assert_eq!(fs::read(&src).unwrap(), png(10, 10, 1));
}

#[test]
fn a_copy_that_reads_back_wrong_keeps_the_old_file() {
    FORCE_COPY.with(|c| c.set(true));
    CORRUPT_COPY.with(|c| c.set(true));
    let f = fixture();
    let old = f.old_dir(MediaKind::Background).join("office.png");
    write(&old, &png(10, 10, 1));
    assert_eq!(f.run(), outcome(0, 0, 1));
    CORRUPT_COPY.with(|c| c.set(false));
    assert_eq!(fs::read(&old).unwrap(), png(10, 10, 1));
    // The bad copy is gone, no temporary file is left.
    assert_eq!(names(&f.new_dir(MediaKind::Background)), set(&["away"]));
    // The next start moves it.
    assert_eq!(f.run(), outcome(1, 0, 0));
    FORCE_COPY.with(|c| c.set(false));
    assert!(!old.exists());
    assert_eq!(
        fs::read(f.new_dir(MediaKind::Background).join("office.png")).unwrap(),
        png(10, 10, 1)
    );
}

/// The fixture with a videos root of its own (`<root>/Videos/…`), as a
/// session's store has.
fn fixture_with_videos() -> (Fixture, PathBuf) {
    let mut f = fixture();
    let videos = f
        .data
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("Videos/omarchy-broadcast");
    f.store = f.store.clone().with_video_root(&videos);
    (f, videos)
}

#[test]
fn old_videos_go_to_the_videos_folder_and_images_to_pictures() {
    for copy in [false, true] {
        FORCE_COPY.with(|c| c.set(copy));
        let (f, videos) = fixture_with_videos();
        let bg = f.old_dir(MediaKind::Background);
        let away = f.old_dir(MediaKind::Away);
        write(&bg.join("office.png"), &png(10, 10, 1));
        write(&bg.join("party.webm"), b"a transcoded animation");
        write(&bg.join("clip.MP4"), b"a video");
        write(&bg.join(TRANSCODED_DIR).join(HASH), b"party.webm\n");
        write(&away.join("me.png"), &png(10, 10, 2));

        assert_eq!(f.run(), outcome(4, 0, 0));
        let pictures = f.new_dir(MediaKind::Background);
        assert_eq!(names(&pictures), set(&["office.png", "away"]));
        assert_eq!(
            fs::read(pictures.join("office.png")).unwrap(),
            png(10, 10, 1)
        );
        assert_eq!(
            names(&videos),
            set(&["party.webm", "clip.MP4", TRANSCODED_DIR])
        );
        assert_eq!(
            fs::read(videos.join("party.webm")).unwrap(),
            b"a transcoded animation"
        );
        assert_eq!(fs::read(videos.join("clip.MP4")).unwrap(), b"a video");
        // The record follows the videos.
        assert_eq!(
            fs::read_to_string(videos.join(TRANSCODED_DIR).join(HASH)).unwrap(),
            "party.webm"
        );
        assert_eq!(
            fs::read(f.new_dir(MediaKind::Away).join("me.png")).unwrap(),
            png(10, 10, 2)
        );
        assert_eq!(mode(&videos), 0o700);
        assert_eq!(mode(&videos.join("clip.MP4")), 0o600);
        assert!(!f.data.exists());
        // A rerun has nothing to do; the store lists the image (these
        // fake videos fail the video check).
        assert_eq!(f.run(), Outcome::default());
        let listed: Vec<PathBuf> = f
            .store
            .list(MediaKind::Background)
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert_eq!(listed, [pictures.join("office.png")]);
        FORCE_COPY.with(|c| c.set(false));
    }
}

#[test]
fn the_state_names_a_moved_video_in_the_videos_folder() {
    let (f, videos) = fixture_with_videos();
    let bg = f.old_dir(MediaKind::Background);
    write(&bg.join("loop.webm"), b"a video");
    let st = RuntimeState {
        background_image: Some(bg.join("loop.webm")),
        ..RuntimeState::default()
    };
    st.save(&f.state).unwrap();
    assert_eq!(f.run(), outcome(1, 0, 0));
    let (st, damaged) = RuntimeState::load(&f.state);
    assert!(damaged.is_none());
    assert_eq!(st.background_image, Some(videos.join("loop.webm")));
}

#[test]
fn a_videos_root_linked_to_the_old_backgrounds_leaves_them_as_they_are() {
    for copy in [false, true] {
        FORCE_COPY.with(|c| c.set(copy));
        let (f, videos) = fixture_with_videos();
        let bg = f.old_dir(MediaKind::Background);
        let away = f.old_dir(MediaKind::Away);
        write(&bg.join("office.png"), &png(10, 10, 1));
        write(&bg.join("party.webm"), b"a transcoded animation");
        write(&bg.join(TRANSCODED_DIR).join(HASH), b"party.webm\n");
        write(&away.join("me.png"), &png(10, 10, 2));
        fs::create_dir_all(videos.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&bg, &videos).unwrap();
        // The backgrounds stay whole, the images too, and away.
        assert_eq!(f.run(), Outcome::default());
        assert_eq!(
            names(&bg),
            set(&["office.png", "party.webm", TRANSCODED_DIR])
        );
        assert_eq!(fs::read(bg.join("office.png")).unwrap(), png(10, 10, 1));
        assert_eq!(
            fs::read(bg.join("party.webm")).unwrap(),
            b"a transcoded animation"
        );
        assert_eq!(
            fs::read(bg.join(TRANSCODED_DIR).join(HASH)).unwrap(),
            b"party.webm\n"
        );
        assert_eq!(names(&f.new_dir(MediaKind::Background)), set(&["away"]));
        assert_eq!(fs::read(away.join("me.png")).unwrap(), png(10, 10, 2));
        // The old root holds the videos root: it stays.
        assert!(f.old.is_dir());
        assert_eq!(f.run(), Outcome::default());
        assert_eq!(fs::read(bg.join("office.png")).unwrap(), png(10, 10, 1));
        FORCE_COPY.with(|c| c.set(false));
    }
}

/// Every file of `files` (path, bytes) is still there with its bytes.
fn intact(files: &[(PathBuf, Vec<u8>)]) {
    for (path, data) in files {
        assert_eq!(&fs::read(path).unwrap(), data, "{}", path.display());
    }
}

#[test]
fn a_videos_root_linked_to_the_old_away_leaves_the_whole_gallery() {
    for copy in [false, true] {
        FORCE_COPY.with(|c| c.set(copy));
        let (f, videos) = fixture_with_videos();
        let bg = f.old_dir(MediaKind::Background);
        let away = f.old_dir(MediaKind::Away);
        let files = [
            (bg.join("office.png"), png(10, 10, 1)),
            (bg.join("bg.mp4"), b"a video".to_vec()),
            (away.join("me.png"), png(10, 10, 2)),
        ];
        for (p, d) in &files {
            write(p, d);
        }
        fs::create_dir_all(videos.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&away, &videos).unwrap();
        assert_eq!(f.run(), Outcome::default());
        intact(&files);
        assert_eq!(names(&away), set(&["me.png"]));
        assert_eq!(names(&bg), set(&["office.png", "bg.mp4"]));
        assert_eq!(f.run(), Outcome::default());
        intact(&files);
        FORCE_COPY.with(|c| c.set(false));
    }
}

#[test]
fn crosswise_links_into_the_old_gallery_lose_no_file() {
    // The videos root a link to the old away/, the new away/ a link to
    // the old backgrounds/: a video of backgrounds/ would be linked into
    // the old away/, then found again from away/ as its own copy in the
    // old backgrounds/, and both old names removed.
    for copy in [false, true] {
        FORCE_COPY.with(|c| c.set(copy));
        let (f, videos) = fixture_with_videos();
        let bg = f.old_dir(MediaKind::Background);
        let away = f.old_dir(MediaKind::Away);
        let files = [
            (bg.join("office.png"), png(10, 10, 1)),
            (bg.join("bg.mp4"), b"a video".to_vec()),
            (away.join("me.png"), png(10, 10, 2)),
        ];
        for (p, d) in &files {
            write(p, d);
        }
        fs::create_dir_all(videos.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&away, &videos).unwrap();
        fs::create_dir_all(f.store.user_root()).unwrap();
        std::os::unix::fs::symlink(&bg, f.new_dir(MediaKind::Away)).unwrap();
        assert_eq!(f.run(), Outcome::default());
        intact(&files);
        assert_eq!(names(&bg), set(&["office.png", "bg.mp4"]));
        assert_eq!(names(&away), set(&["me.png"]));
        assert_eq!(f.run(), Outcome::default());
        intact(&files);
        FORCE_COPY.with(|c| c.set(false));
    }
}

#[test]
fn a_copy_that_is_a_name_of_the_old_gallery_keeps_the_old_file() {
    let f = fixture();
    let bg = f.old_dir(MediaKind::Background);
    let away = f.old_dir(MediaKind::Away);
    write(&bg.join("a.png"), &png(10, 10, 1));
    write(&away.join("b.png"), &png(10, 10, 2));
    let (old, names_) = old_gallery(&f.old);
    let canonical = f.old.canonicalize().unwrap();
    assert!(old.contains(&canonical));
    assert!(names_.contains(&canonical.join("backgrounds/a.png")));
    assert!(names_.contains(&canonical.join("away/b.png")));
    // A new name in the old gallery, by its path or through a link.
    assert!(apart(&away.join("b.png"), &old, &names_).is_err());
    let alias = f.data.join("alias");
    std::os::unix::fs::symlink(&bg, &alias).unwrap();
    assert!(apart(&alias.join("a.png"), &old, &names_).is_err());
    // A name elsewhere is apart; only from a regular file.
    let new = f.store.user_root().join("a.png");
    write(&new, &png(10, 10, 1));
    assert!(apart(&new, &old, &names_).is_ok());
    assert!(apart(&f.store.user_root().join("gone.png"), &old, &names_).is_err());
    // Only names: an old name listed but outside the old directories.
    let mut only = HashSet::new();
    only.insert(new.canonicalize().unwrap());
    assert!(apart(&new, &[], &only).is_err());
}

#[test]
fn an_empty_directory_is_never_removed_through_a_link() {
    let f = fixture();
    let real = f.data.join("real");
    fs::create_dir_all(real.join("inner")).unwrap();
    let link = f.data.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // Neither the link itself nor a directory through it.
    remove_empty(&link);
    remove_empty(&link.join("inner"));
    assert!(real.join("inner").is_dir());
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    remove_empty(&real.join("inner"));
    assert!(!real.join("inner").exists());
}

#[test]
fn a_user_directory_that_cannot_be_made_keeps_the_record_of_kept_files() {
    let f = fixture();
    let away = f.old_dir(MediaKind::Away);
    write(&away.join("me.png"), &png(10, 10, 2));
    write(&away.join(KEPT_RECORD), b"1 2 3 other.png\n");
    // A file in place of the new away/: it cannot be made.
    write(&f.new_dir(MediaKind::Away), b"not a directory");
    assert_eq!(f.run(), outcome(0, 0, 1));
    assert_eq!(
        fs::read(away.join(KEPT_RECORD)).unwrap(),
        b"1 2 3 other.png\n"
    );
    assert_eq!(fs::read(away.join("me.png")).unwrap(), png(10, 10, 2));
}
