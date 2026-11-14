//! Tests of the config set, the managed writer and the migration.

use super::*;
use crate::config_schema::{ModuleFact, StageFact};
use crate::modules::BackendProfile;
use crate::{AspectPolicy, OutputFormat};

/// A configuration directory in a temporary directory, with
/// `config.toml` at `dir/config.toml`.
struct Dir {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl Dir {
    fn new() -> Dir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("omarchy-broadcast");
        fs::create_dir_all(&dir).unwrap();
        Dir { _tmp: tmp, dir }
    }

    fn config_toml(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.dir.join(rel)).unwrap()
    }

    fn load(&self) -> Result<ConfigSet, ConfigError> {
        ConfigSet::load(&self.config_toml())
    }
}

fn facts() -> RegistryFacts {
    RegistryFacts {
        modules: vec![
            ModuleFact {
                id: "org.example.hello".into(),
                name: "Hello tint".into(),
                forceable: true,
            },
            ModuleFact {
                id: "br.com.dias.maxine-vfx".into(),
                name: "NVIDIA Maxine".into(),
                forceable: true,
            },
        ],
        stages: vec![StageFact {
            id: "background".into(),
            modules: vec!["br.com.dias.maxine-vfx".into()],
        }],
        output_modes: vec![
            ("1920x1080@30".into(), "1920x1080 \u{b7} 30 fps".into()),
            ("1280x720@30".into(), "1280x720 \u{b7} 30 fps".into()),
        ],
        ..RegistryFacts::default()
    }
}

#[test]
fn no_files_is_the_defaults_and_lists_the_two_known_files() {
    let d = Dir::new();
    let set = d.load().unwrap();
    assert_eq!(set.config, Config::default());
    assert!(set.provenance.is_empty());
    assert!(set.managed.is_empty());
    assert_eq!(
        set.files,
        [
            ConfigFile {
                path: "config.toml".into(),
                status: FileStatus::Absent,
                managed: false,
                reason: String::new(),
            },
            ConfigFile {
                path: "config.d/50-panel.toml".into(),
                status: FileStatus::Absent,
                managed: true,
                reason: String::new(),
            },
        ]
    );
    assert_eq!(set.source("output.format"), "");
}

#[test]
fn tables_merge_and_other_values_are_replaced_in_lexical_order() {
    let d = Dir::new();
    d.write(
        "config.toml",
        "[output]\nformat = \"i420\"\naspect = \"letterbox\"\n\
         [camera]\npreference = [\"Brio\", \"C920\"]\n\
         [modules]\npin = { background = \"org.x.a\", relight = \"org.x.b\" }\n",
    );
    // Byte order of the names: "10-a" < "9-z" ('-' < '0') < "90-b" < "90-c".
    d.write("config.d/90-c.toml", "[output]\nhold_timeout_s = 2.0\n");
    d.write(
        "config.d/10-a.toml",
        "[output]\nhold_timeout_s = 1.0\nformat = \"nv12\"\n",
    );
    d.write(
        "config.d/90-b.toml",
        "[camera]\npreference = [\"Other\"]\n[modules.pin]\nrelight = \"\"\n",
    );
    d.write("config.d/9-z.toml", "[output]\nhold_timeout_s = 4.0\n");
    // Ignored: hidden, other extensions, directories.
    d.write("config.d/.hidden.toml", "nonsense = [");
    d.write("config.d/notes.txt", "nonsense = [");
    d.write("config.d/99-dir.toml/inner.toml", "nonsense = [");
    let set = d.load().unwrap();
    let c = &set.config;
    assert_eq!(c.output.format, OutputFormat::Nv12);
    assert_eq!(c.output.aspect, AspectPolicy::Letterbox);
    assert_eq!(c.output.hold_timeout_s, 2.0);
    // An array is replaced whole.
    assert_eq!(c.camera.preference.len(), 1);
    // A table merges key by key; an empty pin clears the earlier one.
    assert_eq!(c.modules.pin["background"], "org.x.a");
    assert_eq!(c.modules.pin["relight"], "");
    assert_eq!(
        c.modules.pins(),
        [("background".to_string(), "org.x.a".to_string())].into()
    );
    let paths: Vec<&str> = set.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "config.toml",
            "config.d/10-a.toml",
            "config.d/50-panel.toml",
            "config.d/9-z.toml",
            "config.d/90-b.toml",
            "config.d/90-c.toml",
        ]
    );
    // Provenance: the file of the value in force, by leaf key.
    assert_eq!(set.source("output.format"), "config.d/10-a.toml");
    assert_eq!(set.source("output.aspect"), "config.toml");
    assert_eq!(set.source("output.hold_timeout_s"), "config.d/90-c.toml");
    assert_eq!(set.source("camera.preference"), "config.d/90-b.toml");
    assert_eq!(set.source("modules.pin.background"), "config.toml");
    assert_eq!(set.source("modules.pin.relight"), "config.d/90-b.toml");
    assert_eq!(set.source("output.input_gap_ms"), "");
    // Overridden: set by a file after the managed one.
    assert_eq!(
        set.overridden_by("output.hold_timeout_s"),
        "config.d/90-c.toml"
    );
    assert_eq!(set.overridden_by("output.format"), "");
    assert_eq!(set.overridden_by("output.aspect"), "");
    assert_eq!(
        set.overridden_by("modules.pin.relight"),
        "config.d/90-b.toml"
    );
}

#[test]
fn module_ids_with_dots_are_one_key_segment() {
    assert_eq!(
        key_segments("modules.enabled.org.example.hello"),
        ["modules", "enabled", "org.example.hello"]
    );
    assert_eq!(
        key_segments("modules.pin.background"),
        ["modules", "pin", "background"]
    );
    assert_eq!(key_segments("output.mode"), ["output", "mode"]);
    assert_eq!(key_segments("modules.enabled"), ["modules", "enabled"]);
    let d = Dir::new();
    d.write(
        "config.d/20-x.toml",
        "[modules.enabled]\n\"org.example.hello\" = false\n",
    );
    let set = d.load().unwrap();
    assert!(!set.config.modules.is_enabled("org.example.hello"));
    assert_eq!(
        set.source("modules.enabled.org.example.hello"),
        "config.d/20-x.toml"
    );
}

#[test]
fn the_forced_table_merges_across_files_and_takes_writes() {
    assert_eq!(
        key_segments("modules.forced.org.example.hello"),
        ["modules", "forced", "org.example.hello"]
    );
    let d = Dir::new();
    d.write(
        "config.toml",
        "[modules.forced]\n\"org.example.hello\" = true\n\"br.com.dias.maxine-vfx\" = true\n",
    );
    d.write(
        "config.d/20-x.toml",
        "[modules.forced]\n\"br.com.dias.maxine-vfx\" = false\n",
    );
    let set = d.load().unwrap();
    assert!(set.config.modules.is_forced("org.example.hello"));
    assert!(!set.config.modules.is_forced("br.com.dias.maxine-vfx"));
    assert_eq!(
        set.source("modules.forced.br.com.dias.maxine-vfx"),
        "config.d/20-x.toml"
    );
    // A write goes to the managed file, and a reset removes it.
    let set = set_config(
        &d.config_toml(),
        &facts(),
        "modules.forced.org.example.hello",
        &ConfigValue::Bool(false),
    )
    .unwrap();
    assert!(!set.config.modules.is_forced("org.example.hello"));
    assert_eq!(set.source("modules.forced.org.example.hello"), MANAGED_REL);
    assert!(d.read(MANAGED_REL).contains("[modules.forced]"));
    let set = reset_config(
        &d.config_toml(),
        &facts(),
        "modules.forced.org.example.hello",
    )
    .unwrap();
    assert!(set.config.modules.is_forced("org.example.hello"));
    // A file after the managed one that sets it refuses the write.
    d.write(
        "config.d/90-z.toml",
        "[modules.forced]\n\"br.com.dias.maxine-vfx\" = false\n",
    );
    let e = set_config(
        &d.config_toml(),
        &facts(),
        "modules.forced.br.com.dias.maxine-vfx",
        &ConfigValue::Bool(true),
    )
    .unwrap_err();
    assert!(matches!(e, WriteError::Overridden { .. }), "{e:?}");
    // Only for an installed module, and only a switch.
    let e = set_config(
        &d.config_toml(),
        &facts(),
        "modules.forced.org.not.installed",
        &ConfigValue::Bool(true),
    )
    .unwrap_err();
    assert!(matches!(e, WriteError::UnknownKey(_)), "{e:?}");
    let e = set_config(
        &d.config_toml(),
        &facts(),
        "modules.forced.org.example.hello",
        &ConfigValue::Int(1),
    )
    .unwrap_err();
    assert!(e.to_string().contains("true or false"), "{e}");
    // A module id that is not one makes the file invalid.
    d.write("config.d/30-y.toml", "[modules.forced]\nhello = true\n");
    assert!(d.load().is_err());
}

#[test]
fn an_error_in_a_file_names_that_file_and_line_and_rejects_the_set() {
    let d = Dir::new();
    d.write("config.toml", "[output]\nformat = \"i420\"\n");
    d.write(
        "config.d/90-x.toml",
        "# comment\n[output]\n\nformatt = \"nv12\"\n",
    );
    let e = d.load().unwrap_err();
    let text = e.to_string();
    assert!(text.contains("config.d/90-x.toml:4:"), "{text}");
    assert!(text.contains("formatt"), "{text}");
    // A type error, per file too.
    d.write(
        "config.d/90-x.toml",
        "[lifecycle]\nidle_grace_s = \"long\"\n",
    );
    let text = d.load().unwrap_err().to_string();
    assert!(text.contains("config.d/90-x.toml:2:"), "{text}");
    // A range error, per file too.
    d.write("config.d/90-x.toml", "[output]\ninput_gap_ms = 0\n");
    let text = d.load().unwrap_err().to_string();
    assert!(text.contains("config.d/90-x.toml:2:"), "{text}");
    let (_, files) = config_files(&d.config_toml());
    let f = files
        .iter()
        .find(|f| f.path == "config.d/90-x.toml")
        .unwrap();
    assert_eq!(f.status, FileStatus::Error);
    assert!(f.reason.starts_with("line 2, column"), "{}", f.reason);
    assert!(
        !f.reason.contains(&*d.dir.to_string_lossy()),
        "{}",
        f.reason
    );
    assert_eq!(files[0].status, FileStatus::Loaded);
    // Not TOML at all.
    d.write("config.d/90-x.toml", "[output\n");
    let text = d.load().unwrap_err().to_string();
    assert!(text.contains("config.d/90-x.toml:1:"), "{text}");
}

#[test]
fn a_check_across_files_names_the_file_that_set_the_key() {
    let d = Dir::new();
    d.write("config.toml", "[effect]\nblur_strength = 0.3\n");
    // Alone, each file is fine; merged, replace has no image.
    d.write("config.d/70-x.toml", "\n[effect]\nmode = \"replace\"\n");
    let e = d.load().unwrap_err();
    let text = e.to_string();
    assert!(text.contains("config.d/70-x.toml:3"), "{text}");
    assert!(text.contains("effect.mode"), "{text}");
    let (_, files) = config_files(&d.config_toml());
    let f = files
        .iter()
        .find(|f| f.path == "config.d/70-x.toml")
        .unwrap();
    assert_eq!(f.status, FileStatus::Error);
    assert_eq!(files[0].status, FileStatus::Loaded);
    // An earlier file supplying the image makes the set valid.
    d.write(
        "config.toml",
        "[effect]\nbackground_image = \"/srv/bg.png\"\n",
    );
    let set = d.load().unwrap();
    assert_eq!(set.config.effect.mode, crate::EffectMode::Replace);
}

#[test]
fn config_load_and_parse_are_unchanged() {
    let d = Dir::new();
    d.write("config.toml", "[output]\nformat = \"i420\"\n");
    d.write("config.d/90-x.toml", "[output]\nformat = \"nv12\"\n");
    // Config::load reads the one file it is given.
    let c = Config::load(&d.config_toml()).unwrap();
    assert_eq!(c.output.format, OutputFormat::I420);
}

#[test]
fn set_config_writes_only_the_managed_file_atomically_with_its_header() {
    let d = Dir::new();
    let original = "# mine\n[output]\nformat = \"i420\" # keep\n";
    d.write("config.toml", original);
    let set = set_config(
        &d.config_toml(),
        &facts(),
        "output.hold_timeout_s",
        &ConfigValue::Int(2),
    )
    .unwrap();
    assert_eq!(set.config.output.hold_timeout_s, 2.0);
    assert_eq!(set.config.output.format, OutputFormat::I420);
    assert_eq!(set.source("output.hold_timeout_s"), MANAGED_REL);
    let set = set_config(
        &d.config_toml(),
        &facts(),
        "modules.enabled.org.example.hello",
        &ConfigValue::Bool(false),
    )
    .unwrap();
    assert!(!set.config.modules.is_enabled("org.example.hello"));
    let set = set_config(
        &d.config_toml(),
        &facts(),
        "modules.pin.background",
        &ConfigValue::Str("br.com.dias.maxine-vfx".into()),
    )
    .unwrap();
    assert_eq!(
        set.config.modules.pin["background"],
        "br.com.dias.maxine-vfx"
    );
    let text = d.read(MANAGED_REL);
    assert!(text.starts_with(MANAGED_HEADER), "{text}");
    assert!(text.contains("hold_timeout_s = 2.0"), "{text}");
    assert!(text.contains("\"org.example.hello\" = false"), "{text}");
    // config.toml is byte-identical, and no temporary file is left.
    assert_eq!(d.read("config.toml"), original);
    let names: Vec<String> = fs::read_dir(d.dir.join(CONFIG_D))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [MANAGED_FILE]);
    // Reset removes the key; the earlier file's value is in force again.
    let set = reset_config(&d.config_toml(), &facts(), "output.hold_timeout_s").unwrap();
    assert_eq!(set.config.output.hold_timeout_s, 3.0);
    assert!(!d.read(MANAGED_REL).contains("hold_timeout_s"));
    let set = reset_config(
        &d.config_toml(),
        &facts(),
        "modules.enabled.org.example.hello",
    )
    .unwrap();
    assert!(set.config.modules.is_enabled("org.example.hello"));
    // The emptied table is gone, the other one stays.
    let text = d.read(MANAGED_REL);
    assert!(!text.contains("[modules.enabled]"), "{text}");
    assert!(text.contains("[modules.pin]"), "{text}");
    assert_eq!(d.read("config.toml"), original);
}

#[test]
fn output_mode_is_set_in_the_managed_file_with_config_toml_untouched() {
    let d = Dir::new();
    let original = "# mine\n[output]\nmode = \"1920x1080@30\" # keep\n";
    d.write("config.toml", original);
    let set = set_config(
        &d.config_toml(),
        &facts(),
        "output.mode",
        &ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    assert_eq!(
        set.config.output_contract().to_string(),
        "nv12:1280x720@30 limited"
    );
    assert_eq!(set.source("output.mode"), MANAGED_REL);
    let text = d.read(MANAGED_REL);
    assert!(text.contains("[output]\nmode = \"1280x720@30\""), "{text}");
    assert_eq!(d.read("config.toml"), original);
    // Not an option: refused, nothing written.
    let e = set_config(
        &d.config_toml(),
        &facts(),
        "output.mode",
        &ConfigValue::Str("1281x720@30".into()),
    )
    .unwrap_err();
    assert!(e.to_string().contains("is not one of"), "{e}");
    assert_eq!(d.read(MANAGED_REL), text);
    // Reset: config.toml's value again.
    let set = reset_config(&d.config_toml(), &facts(), "output.mode").unwrap();
    assert_eq!(
        set.config.output_contract().to_string(),
        "nv12:1920x1080@30 limited"
    );
    assert_eq!(d.read("config.toml"), original);
}

#[test]
fn a_write_that_changes_nothing_writes_nothing() {
    let d = Dir::new();
    reset_config(&d.config_toml(), &facts(), "output.format").unwrap();
    assert!(!d.dir.join(CONFIG_D).exists());
    assert!(!d.config_toml().exists());
}

#[test]
fn invalid_values_are_refused_before_anything_is_written() {
    let d = Dir::new();
    let set = |key: &str, v: ConfigValue| set_config(&d.config_toml(), &facts(), key, &v);
    let e = set("output.input_gap_ms", ConfigValue::Int(0)).unwrap_err();
    assert!(matches!(e, WriteError::Invalid(_)), "{e:?}");
    assert!(e.to_string().contains("output.input_gap_ms"), "{e}");
    let e = set("output.format", ConfigValue::Str("rgb".into())).unwrap_err();
    assert!(e.to_string().contains("not one of"), "{e}");
    let e = set("pipewire.enabled", ConfigValue::Int(1)).unwrap_err();
    assert!(e.to_string().contains("true or false"), "{e}");
    let e = set("output.hold_timeout_s", ConfigValue::Float(f64::NAN)).unwrap_err();
    assert!(matches!(e, WriteError::Invalid(_)), "{e:?}");
    let e = set("modules.pin.background", ConfigValue::Str("org.x.y".into())).unwrap_err();
    assert!(e.to_string().contains("not one of"), "{e}");
    assert!(!d.dir.join(CONFIG_D).exists());
    // Unknown and read-only keys.
    let e = set("output.colour", ConfigValue::Str("x".into())).unwrap_err();
    assert!(matches!(e, WriteError::UnknownKey(_)), "{e:?}");
    let e = set("modules.enabled.org.not.installed", ConfigValue::Bool(true)).unwrap_err();
    assert!(matches!(e, WriteError::UnknownKey(_)), "{e:?}");
    let e = set("modules.pin.relight", ConfigValue::Str(String::new())).unwrap_err();
    assert!(matches!(e, WriteError::UnknownKey(_)), "{e:?}");
    let e = set("output.mode", ConfigValue::Str("x".into())).unwrap_err();
    assert!(e.to_string().contains("not one of"), "{e}");
    for key in [
        "camera.mode",
        "camera.preference",
        "loopback.label",
        "loopback.device",
        "modules.order",
        "modules.dirs",
        "modules.disabled",
        "effect.mode",
        "effect.maxine_root",
        "effect.relight_brightness",
    ] {
        let e = set(key, ConfigValue::Str("x".into())).unwrap_err();
        assert!(matches!(e, WriteError::NotEditable(_)), "{key}: {e:?}");
        let e = reset_config(&d.config_toml(), &facts(), key).unwrap_err();
        assert!(matches!(e, WriteError::NotEditable(_)), "{key}: {e:?}");
    }
    assert!(!d.dir.join(CONFIG_D).exists());
}

#[test]
fn the_away_image_and_the_placeholder_are_written_to_the_managed_file() {
    let d = Dir::new();
    let original = "[effect]\nfallback_image = \"/srv/early.png\"\n";
    d.write("config.toml", original);
    let set = |key: &str, v: &str| {
        set_config(&d.config_toml(), &facts(), key, &ConfigValue::Str(v.into()))
    };
    let s = set("effect.fallback_image", "/home/u/away.png").unwrap();
    assert_eq!(
        s.config.effect.fallback_image,
        Some("/home/u/away.png".into())
    );
    assert_eq!(s.source("effect.fallback_image"), MANAGED_REL);
    let s = set("output.placeholder", "image").unwrap();
    assert_eq!(s.config.output.placeholder, crate::OutputPlaceholder::Image);
    assert!(d.read(MANAGED_REL).contains("placeholder = \"image\""));
    // The same checks as in the file: absolute, a known option.
    let e = set("effect.fallback_image", "away.png").unwrap_err();
    assert!(matches!(e, WriteError::Invalid(_)), "{e:?}");
    assert!(e.to_string().contains("effect.fallback_image"), "{e}");
    let e = set("output.placeholder", "none").unwrap_err();
    assert!(e.to_string().contains("not one of"), "{e}");
    // Empty clears, over the earlier file's image too.
    let s = set("effect.fallback_image", "").unwrap();
    assert_eq!(s.config.effect.fallback_image, None);
    assert_eq!(d.read("config.toml"), original);
    // A later file wins: the write is refused.
    d.write(
        "config.d/90-local.toml",
        "[effect]\nfallback_image = \"/srv/late.png\"\n",
    );
    let e = set("effect.fallback_image", "/home/u/away.png").unwrap_err();
    assert_eq!(e.to_string(), "Overridden by config.d/90-local.toml");
}

#[test]
fn a_key_set_by_a_later_file_is_refused() {
    let d = Dir::new();
    d.write("config.d/90-local.toml", "[output]\naspect = \"crop\"\n");
    d.write("config.d/10-early.toml", "[output]\nformat = \"i420\"\n");
    let e = set_config(
        &d.config_toml(),
        &facts(),
        "output.aspect",
        &ConfigValue::Str("letterbox".into()),
    )
    .unwrap_err();
    assert_eq!(e.to_string(), "Overridden by config.d/90-local.toml");
    let e = reset_config(&d.config_toml(), &facts(), "output.aspect").unwrap_err();
    assert!(matches!(e, WriteError::Overridden { .. }), "{e:?}");
    assert!(!d.dir.join(MANAGED_REL).exists());
    // An earlier file does not override.
    set_config(
        &d.config_toml(),
        &facts(),
        "output.format",
        &ConfigValue::Str("nv12".into()),
    )
    .unwrap();
    // The policy for the migration writes anyway.
    let set = persist_managed(
        &d.config_toml(),
        &[ManagedEdit::Set(
            "output.aspect".into(),
            toml::Value::String("letterbox".into()),
        )],
        OverridePolicy::Ignore,
    )
    .unwrap();
    assert_eq!(set.config.output.aspect, AspectPolicy::Crop);
    assert_eq!(set.overridden_by("output.aspect"), "config.d/90-local.toml");
    assert_eq!(
        table_get(&set.managed, "output.aspect").unwrap().as_str(),
        Some("letterbox")
    );
}

#[test]
fn a_managed_file_that_does_not_load_is_never_overwritten() {
    let d = Dir::new();
    for broken in ["[output\n", "[output]\nformatt = \"nv12\"\n"] {
        d.write(MANAGED_REL, broken);
        let e = set_config(
            &d.config_toml(),
            &facts(),
            "pipewire.enabled",
            &ConfigValue::Bool(true),
        )
        .unwrap_err();
        match &e {
            WriteError::ManagedUnreadable { file, .. } => assert_eq!(file, MANAGED_REL),
            other => panic!("{other:?}"),
        }
        assert!(e.to_string().contains(MANAGED_REL), "{e}");
        assert_eq!(d.read(MANAGED_REL), broken);
        // The loader reports it like any invalid file.
        assert!(d.load().is_err());
    }
    // Another file in error refuses the write too, naming that file.
    d.write(MANAGED_REL, "");
    d.write("config.d/90-x.toml", "[output\n");
    let e = set_config(
        &d.config_toml(),
        &facts(),
        "pipewire.enabled",
        &ConfigValue::Bool(true),
    )
    .unwrap_err();
    assert!(matches!(e, WriteError::Config(_)), "{e:?}");
    assert!(e.to_string().contains("90-x.toml"), "{e}");
    assert_eq!(d.read(MANAGED_REL), "");
}

#[test]
fn with_managed_checks_the_set_as_a_load_would() {
    let d = Dir::new();
    d.write(
        "config.d/90-x.toml",
        "[effect]\nmode = \"replace\"\nbackground_image = \"/a.png\"\n",
    );
    let set = d.load().unwrap();
    let mut managed = toml::Table::new();
    set_at(
        &mut managed,
        &["effect", "background_image"],
        toml::Value::String("relative".into()),
    );
    assert!(set.with_managed(&managed).is_err());
    let mut managed = toml::Table::new();
    set_at(
        &mut managed,
        &["output", "format"],
        toml::Value::String("i420".into()),
    );
    assert_eq!(
        set.with_managed(&managed).unwrap().output.format,
        OutputFormat::I420
    );
}

fn state_with_choices() -> RuntimeState {
    let mut st = RuntimeState {
        camera: Some("usb-cam".into()),
        ..RuntimeState::default()
    };
    st.modules.enabled.insert("org.example.hello".into(), false);
    st.modules
        .enabled
        .insert("br.com.dias.maxine-vfx".into(), true);
    st.modules.order = Some(vec!["denoise".into(), "background".into()]);
    st.modules.profile = Some(BackendProfile::Battery);
    st.modules
        .pin
        .insert("background".into(), "br.com.dias.maxine-vfx".into());
    st.modules.pin.insert("relight".into(), String::new());
    st.settings.insert(
        "effect.hello.enabled".into(),
        crate::modules::StoredValue::Bool(true),
    );
    st
}

/// The module choices in force: what the daemon resolves from the state
/// over the config.
fn in_force(
    state: &RuntimeState,
    config: &Config,
) -> (
    Vec<bool>,
    Vec<String>,
    BackendProfile,
    BTreeMap<String, String>,
) {
    let m = &config.modules;
    (
        ["org.example.hello", "br.com.dias.maxine-vfx", "org.other.x"]
            .iter()
            .map(|id| state.modules.enabled(m, id))
            .collect(),
        state.modules.order(m).to_vec(),
        state.modules.profile(m),
        state.modules.pins(m),
    )
}

#[test]
fn the_migration_moves_every_kind_of_choice_and_loses_nothing() {
    let d = Dir::new();
    let original = "[modules]\ndisabled = [\"br.com.dias.maxine-vfx\"]\n\
                    pin = { relight = \"org.x.r\" }\n";
    d.write("config.toml", original);
    let state = state_with_choices();
    let before = in_force(&state, &d.load().unwrap().config);
    let set = d.load().unwrap();
    let m = migrate_modules_state(&state, &set.managed);
    assert!(m.is_needed());
    assert!(m.notes.is_empty(), "{:?}", m.notes);
    assert_eq!(m.edits.len(), 6, "{:?}", m.edits);
    // Only the modules table leaves the state.
    assert!(m.state.modules.is_empty());
    assert_eq!(m.state.camera.as_deref(), Some("usb-cam"));
    assert_eq!(m.state.settings, state.settings);
    // Config first, then the state.
    let set = persist_managed(&d.config_toml(), &m.edits, OverridePolicy::Ignore).unwrap();
    assert_eq!(in_force(&m.state, &set.config), before);
    assert_eq!(d.read("config.toml"), original);
    // Run again (a crash before the state was saved): nothing more to write.
    let again = migrate_modules_state(&state, &set.managed);
    assert!(again.edits.is_empty());
    assert!(again.is_needed());
    // A state without choices needs nothing.
    let none = migrate_modules_state(&m.state, &set.managed);
    assert!(!none.is_needed());
}

#[test]
fn the_migration_keeps_the_managed_keys_already_set() {
    let d = Dir::new();
    d.write(
        MANAGED_REL,
        "[modules]\nprofile = \"quality\"\n[modules.enabled]\n\"org.example.hello\" = true\n",
    );
    let set = d.load().unwrap();
    let m = migrate_modules_state(&state_with_choices(), &set.managed);
    let keys: Vec<&str> = m.edits.iter().map(ManagedEdit::key).collect();
    assert_eq!(
        keys,
        [
            "modules.enabled.br.com.dias.maxine-vfx",
            "modules.order",
            "modules.pin.background",
            "modules.pin.relight",
        ]
    );
    let set = persist_managed(&d.config_toml(), &m.edits, OverridePolicy::Ignore).unwrap();
    let c = &set.config.modules;
    assert_eq!(c.profile, Some(BackendProfile::Quality));
    assert!(c.is_enabled("org.example.hello"));
    assert!(c.is_enabled("br.com.dias.maxine-vfx"));
    assert_eq!(c.order, ["denoise", "background"]);
    assert_eq!(c.pin["relight"], "");
    assert_eq!(c.pins().len(), 1);
}

#[test]
fn the_migration_drops_entries_that_are_not_valid() {
    let mut st = RuntimeState::default();
    st.modules.enabled.insert("hello".into(), true);
    st.modules.order = Some(vec!["denoise".into(), "denoise".into()]);
    st.modules.pin.insert("Bad".into(), "org.x.y".into());
    st.modules.pin.insert("background".into(), "nope".into());
    let m = migrate_modules_state(&st, &toml::Table::new());
    assert!(m.edits.is_empty(), "{:?}", m.edits);
    assert_eq!(m.notes.len(), 4, "{:?}", m.notes);
    assert!(m.is_needed());
}

#[test]
fn a_load_that_fails_after_the_write_returns_the_set_checked_before() {
    let d = Dir::new();
    d.write("config.toml", "[output]\nhold_timeout_s = 3.0\n");
    let before = d.load().unwrap();
    let set = persist_with(
        &d.config_toml(),
        &[ManagedEdit::Set(
            "output.hold_timeout_s".into(),
            toml::Value::Float(5.0),
        )],
        OverridePolicy::Refuse,
        |p| {
            // A file broken between the write and the load.
            fs::write(p, "[output]\nnope = 1\n").unwrap();
            ConfigSet::load(p)
        },
    )
    .unwrap();
    assert!(d.read(MANAGED_REL).contains("hold_timeout_s = 5.0"));
    assert_eq!(set.config.output.hold_timeout_s, 5.0);
    assert_eq!(set.source("output.hold_timeout_s"), MANAGED_REL);
    assert_ne!(set.provenance, before.provenance);
    let managed = set.files.iter().find(|f| f.managed).unwrap();
    assert_eq!(managed.status, FileStatus::Loaded);
    assert_eq!(
        table_get(&set.managed, "output.hold_timeout_s"),
        Some(&toml::Value::Float(5.0))
    );
    // The set checked before the write is the one a load would make.
    d.write("config.toml", "[output]\nhold_timeout_s = 3.0\n");
    let loaded = d.load().unwrap();
    assert_eq!(loaded.config, set.config);
    assert_eq!(loaded.provenance, set.provenance);
    assert_eq!(loaded.files, set.files);
}

#[test]
fn a_managed_entry_the_schema_does_not_know_can_be_reset() {
    let d = Dir::new();
    d.write(
        MANAGED_REL,
        &format!(
            "{MANAGED_HEADER}\n[modules]\nprofile = \"battery\"\n[modules.enabled]\n\
             \"org.gone.module\" = false\n[modules.pin]\nrelight = \"org.gone.module\"\n"
        ),
    );
    // Neither the module nor the stage is known: set is refused.
    assert!(matches!(
        set_config(
            &d.config_toml(),
            &facts(),
            "modules.enabled.org.gone.module",
            &ConfigValue::Bool(true),
        ),
        Err(WriteError::UnknownKey(_))
    ));
    let set = reset_config(
        &d.config_toml(),
        &facts(),
        "modules.enabled.org.gone.module",
    )
    .unwrap();
    assert!(set.config.modules.enabled.is_empty());
    let set = reset_config(&d.config_toml(), &facts(), "modules.pin.relight").unwrap();
    assert!(set.config.modules.pin.is_empty());
    let text = d.read(MANAGED_REL);
    assert!(!text.contains("org.gone.module"), "{text}");
    assert!(text.contains("profile = \"battery\""), "{text}");
    // Absent from the managed file (or a whole table): still unknown.
    for key in [
        "modules.pin.relight",
        "modules.enabled.org.other.x",
        "modules.nope",
    ] {
        assert!(
            matches!(
                reset_config(&d.config_toml(), &facts(), key),
                Err(WriteError::UnknownKey(_))
            ),
            "{key}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_symlinked_managed_file_is_never_replaced() {
    let d = Dir::new();
    let target = d.dir.join("elsewhere.toml");
    fs::write(&target, "[output]\nhold_timeout_s = 4.0\n").unwrap();
    fs::create_dir_all(d.dir.join(CONFIG_D)).unwrap();
    std::os::unix::fs::symlink(&target, d.dir.join(MANAGED_REL)).unwrap();
    assert_eq!(d.load().unwrap().config.output.hold_timeout_s, 4.0);
    let e = set_config(
        &d.config_toml(),
        &facts(),
        "output.hold_timeout_s",
        &ConfigValue::Int(9),
    )
    .unwrap_err();
    assert!(matches!(e, WriteError::Io { .. }), "{e:?}");
    assert!(e.to_string().contains("symbolic link"), "{e}");
    let meta = fs::symlink_metadata(d.dir.join(MANAGED_REL)).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "[output]\nhold_timeout_s = 4.0\n"
    );
}

#[test]
fn a_write_creates_the_missing_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let config_toml = tmp.path().join("a/omarchy-broadcast").join(CONFIG_FILE);
    let set = set_config(
        &config_toml,
        &facts(),
        "output.hold_timeout_s",
        &ConfigValue::Int(2),
    )
    .unwrap();
    assert_eq!(set.config.output.hold_timeout_s, 2.0);
    assert!(managed_path(&config_toml).is_file());
}

#[test]
fn the_privacy_keys_are_written_to_the_managed_file() {
    let d = Dir::new();
    d.write(
        "config.toml",
        "[effect]\nfallback_image = \"/srv/away.png\"\n",
    );
    let s = set_config(
        &d.config_toml(),
        &facts(),
        config_schema::PRIVACY,
        &ConfigValue::Bool(true),
    )
    .unwrap();
    assert_eq!(s.config.effect.policy(), EffectOnFailure::Black);
    let s = set_config(
        &d.config_toml(),
        &facts(),
        config_schema::PRIVACY_MODE,
        &ConfigValue::Str("freeze".into()),
    )
    .unwrap();
    assert_eq!(s.config.effect.policy(), EffectOnFailure::Freeze);
    let text = d.read(MANAGED_REL);
    assert!(text.contains("privacy = true"), "{text}");
    assert!(text.contains("privacy_mode = \"freeze\""), "{text}");
}

#[test]
fn a_legacy_on_failure_write_is_translated_into_the_privacy_keys() {
    let d = Dir::new();
    let original = "[effect]\non_failure = \"black\"\n";
    d.write("config.toml", original);
    let mut f = facts();
    let legacy = |f: &RegistryFacts, v: &str| {
        set_config(
            &d.config_toml(),
            f,
            config_schema::LEGACY_ON_FAILURE,
            &ConfigValue::Str(v.into()),
        )
    };
    // `none`: privacy off, no mode.
    let s = legacy(&f, "none").unwrap();
    assert_eq!(s.config.effect.policy(), EffectOnFailure::None);
    assert_eq!(table_get(&s.managed, "effect.privacy_mode"), None);
    assert_eq!(table_get(&s.managed, "effect.on_failure"), None);
    // `black`: on, black.
    let s = legacy(&f, "black").unwrap();
    assert_eq!(
        (s.config.effect.privacy, s.config.effect.privacy_mode),
        (Some(true), Some(crate::PrivacyMode::Black))
    );
    // `image` is refused while the away image is not offered, as a write
    // of the mode would be; with it offered, it is on, away.
    let e = legacy(&f, "image").unwrap_err();
    assert!(e.to_string().contains("is not one of"), "{e}");
    f.privacy_mode_options = config_schema::privacy_mode_options(&crate::config::EffectConfig {
        fallback_image: Some("/srv/away.png".into()),
        ..Default::default()
    });
    let s = legacy(&f, "image").unwrap();
    assert_eq!(s.config.effect.policy(), EffectOnFailure::Image);
    // Not a legacy value, or not a string: refused.
    for v in ["freeze", "raw"] {
        let e = legacy(&f, v).unwrap_err();
        assert!(matches!(e, WriteError::Invalid(_)), "{v}: {e:?}");
        assert!(e.to_string().contains("effect.on_failure"), "{e}");
    }
    let e = set_config(
        &d.config_toml(),
        &f,
        config_schema::LEGACY_ON_FAILURE,
        &ConfigValue::Bool(true),
    )
    .unwrap_err();
    assert!(matches!(e, WriteError::Invalid(_)), "{e:?}");
    let text = d.read(MANAGED_REL);
    assert!(!text.contains("on_failure"), "{text}");
    // A reset of the legacy key removes the privacy keys too.
    let s = reset_config(&d.config_toml(), &f, config_schema::LEGACY_ON_FAILURE).unwrap();
    assert_eq!(table_get(&s.managed, "effect.privacy"), None);
    assert_eq!(table_get(&s.managed, "effect.privacy_mode"), None);
    // config.toml's legacy key is in force again.
    assert_eq!(s.config.effect.policy(), EffectOnFailure::Black);
    assert_eq!(d.read("config.toml"), original);
}

#[test]
fn the_managed_on_failure_is_migrated_to_the_privacy_keys() {
    for (legacy, want) in [
        ("black", EffectOnFailure::Black),
        ("image", EffectOnFailure::Image),
        ("none", EffectOnFailure::None),
    ] {
        let d = Dir::new();
        let original = "[output]\nplaceholder = \"image\"\n";
        d.write("config.toml", original);
        d.write(
            MANAGED_REL,
            &format!("[effect]\non_failure = \"{legacy}\"\nfallback_image = \"/a.png\"\n"),
        );
        let set = d.load().unwrap();
        assert_eq!(set.config.effect.policy(), want, "{legacy}");
        let edits = migrate_managed_on_failure(&set);
        assert!(
            edits.contains(&ManagedEdit::Remove("effect.on_failure".into())),
            "{legacy}: {edits:?}"
        );
        let set = persist_managed(&d.config_toml(), &edits, OverridePolicy::Ignore).unwrap();
        // The policy in force is the same, from the new keys.
        assert_eq!(set.config.effect.policy(), want, "{legacy}");
        assert_eq!(set.config.effect.on_failure, None, "{legacy}");
        assert_eq!(set.config.effect.privacy, Some(want.hides()), "{legacy}");
        let text = d.read(MANAGED_REL);
        assert!(!text.contains("on_failure"), "{legacy}: {text}");
        assert!(text.contains("fallback_image"), "{legacy}: other keys stay");
        assert_eq!(d.read("config.toml"), original, "{legacy}");
        // Once done, nothing more to do.
        assert!(migrate_managed_on_failure(&set).is_empty(), "{legacy}");
    }
}

#[test]
fn the_migration_only_drops_an_on_failure_the_new_keys_override() {
    let d = Dir::new();
    d.write("config.toml", "[effect]\nprivacy = true\n");
    d.write(MANAGED_REL, "[effect]\non_failure = \"none\"\n");
    let set = d.load().unwrap();
    assert!(set.config.effect.legacy_on_failure_ignored());
    assert_eq!(set.config.effect.policy(), EffectOnFailure::Black);
    let edits = migrate_managed_on_failure(&set);
    assert_eq!(edits, [ManagedEdit::Remove("effect.on_failure".into())]);
    let set = persist_managed(&d.config_toml(), &edits, OverridePolicy::Ignore).unwrap();
    assert_eq!(set.config.effect.policy(), EffectOnFailure::Black);
    // A legacy key in config.toml alone is never touched.
    let d = Dir::new();
    d.write("config.toml", "[effect]\non_failure = \"black\"\n");
    let set = d.load().unwrap();
    assert!(migrate_managed_on_failure(&set).is_empty());
}

#[test]
fn a_privacy_key_of_the_panel_keeps_the_other_from_a_legacy_on_failure() {
    let d = Dir::new();
    d.write("config.toml", "[effect]\non_failure = \"black\"\n");
    let mut f = facts();
    f.privacy_mode_options = config_schema::privacy_mode_options(&crate::config::EffectConfig {
        fallback_image: Some("/srv/away.png".into()),
        ..Default::default()
    });
    let mode = |v: &str| {
        set_config(
            &d.config_toml(),
            &f,
            config_schema::PRIVACY_MODE,
            &ConfigValue::Str(v.into()),
        )
        .unwrap()
    };
    // Last frame chosen over the legacy black: privacy stays on.
    assert_eq!(
        mode("freeze").config.effect.policy(),
        EffectOnFailure::Freeze
    );
    // Privacy set, then reset, with the mode left: on again from the legacy key.
    let s = set_config(
        &d.config_toml(),
        &f,
        config_schema::PRIVACY,
        &ConfigValue::Bool(false),
    )
    .unwrap();
    assert_eq!(s.config.effect.policy(), EffectOnFailure::None);
    let s = reset_config(&d.config_toml(), &f, config_schema::PRIVACY).unwrap();
    assert_eq!(s.config.effect.policy(), EffectOnFailure::Freeze);
    // Legacy image, privacy on from the panel: the away image stays.
    let d = Dir::new();
    d.write("config.toml", "[effect]\non_failure = \"image\"\n");
    let s = set_config(
        &d.config_toml(),
        &f,
        config_schema::PRIVACY,
        &ConfigValue::Bool(true),
    )
    .unwrap();
    assert_eq!(s.config.effect.policy(), EffectOnFailure::Image);
}

#[test]
fn the_migration_only_translates_a_managed_on_failure_in_force() {
    // A later drop-in sets the legacy key: the managed one is only removed,
    // and the later file's value stays in force.
    let d = Dir::new();
    d.write(MANAGED_REL, "[effect]\non_failure = \"none\"\n");
    d.write("config.d/90-x.toml", "[effect]\non_failure = \"black\"\n");
    let set = d.load().unwrap();
    assert_eq!(set.config.effect.policy(), EffectOnFailure::Black);
    let edits = migrate_managed_on_failure(&set);
    assert_eq!(edits, [ManagedEdit::Remove("effect.on_failure".into())]);
    let set = persist_managed(&d.config_toml(), &edits, OverridePolicy::Ignore).unwrap();
    assert_eq!(set.config.effect.policy(), EffectOnFailure::Black);
    assert_eq!(
        set.overridden_by(config_schema::PRIVACY),
        "config.d/90-x.toml"
    );
    // A panel write of either key is then refused: the later file wins.
    for (key, value) in [
        (config_schema::PRIVACY, ConfigValue::Bool(false)),
        (
            config_schema::PRIVACY_MODE,
            ConfigValue::Str("freeze".into()),
        ),
    ] {
        let e = set_config(&d.config_toml(), &facts(), key, &value).unwrap_err();
        assert!(matches!(e, WriteError::Overridden { .. }), "{key}: {e:?}");
    }
    // Only the key no file sets is translated: privacy set elsewhere, the
    // mode from the managed legacy `image`.
    let d = Dir::new();
    d.write("config.toml", "[effect]\nprivacy = true\n");
    d.write(MANAGED_REL, "[effect]\non_failure = \"image\"\n");
    let set = d.load().unwrap();
    assert_eq!(set.config.effect.policy(), EffectOnFailure::Image);
    let edits = migrate_managed_on_failure(&set);
    assert_eq!(
        edits,
        [
            ManagedEdit::Set("effect.privacy_mode".into(), "away".into()),
            ManagedEdit::Remove("effect.on_failure".into())
        ]
    );
    let set = persist_managed(&d.config_toml(), &edits, OverridePolicy::Ignore).unwrap();
    assert_eq!(set.config.effect.policy(), EffectOnFailure::Image);
    assert!(set.config.effect.on_failure.is_none());
}
