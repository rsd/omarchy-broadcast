use std::ffi::OsStr;

use obc_module_sdk::abi;

use super::*;

#[test]
fn the_settings_are_the_eye_contact_then_the_auto_frame_group() {
    let doc: serde_json::Value = serde_json::from_str(&settings().to_json()).unwrap();
    let expected = serde_json::json!({"groups":[
        {"id":"eye_contact","label":"Eye contact","settings":[
            {"id":"effect.eye_contact.enabled","label":"Eye contact","type":"bool",
             "default":false,"stage":"eye-contact","role":"enable","hint":"switch"},
            {"id":"effect.eye_contact.look_away","label":"Natural look-away","type":"bool",
             "default":false,"stage":"eye-contact","key":"look_away","apply":"reload",
             "enabled_when":[["effect.eye_contact.enabled","eq",true]],
             "disabled_reason":"Turn on eye contact to change this","hint":"switch"}]},
        {"id":"auto_frame","label":"Auto-framing","settings":[
            {"id":"effect.auto_frame.enabled","label":"Auto-framing","type":"bool",
             "default":false,"stage":"auto-frame","role":"enable","hint":"switch"},
            {"id":"effect.auto_frame.framing","label":"Framing","type":"enum",
             "options":[["tight","Tight"],["medium","Medium"],["wide","Wide"]],
             "default":"medium","stage":"auto-frame","key":"framing","apply":"live",
             "enabled_when":[["effect.auto_frame.enabled","eq",true]],
             "disabled_reason":"Turn on auto-framing to change this","hint":"segmented"}]}
    ]});
    assert_eq!(doc, expected);
}

#[test]
fn the_root_is_the_environment_then_the_configuration_then_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let default = dir.path().join("default");
    let missing = dir.path().join("missing");
    let configured = Path::new("/configured");
    let env = OsStr::new("/from/env");
    // The environment wins, when not empty.
    assert_eq!(
        resolve_root(Some(env), Some(configured), &missing),
        Some(PathBuf::from("/from/env"))
    );
    assert_eq!(
        resolve_root(Some(OsStr::new("")), Some(configured), &missing),
        Some(PathBuf::from("/configured"))
    );
    // Then the configuration, when not empty.
    assert_eq!(
        resolve_root(None, Some(configured), &missing),
        Some(PathBuf::from("/configured"))
    );
    // Then the default, only when it is a directory.
    assert_eq!(resolve_root(None, Some(Path::new("")), &missing), None);
    assert_eq!(resolve_root(None, None, &missing), None);
    std::fs::create_dir(&default).unwrap();
    assert_eq!(resolve_root(None, None, &default), Some(default.clone()));
    std::fs::write(&missing, b"a file").unwrap();
    assert_eq!(resolve_root(None, None, &missing), None);
}

#[test]
fn the_configuration_takes_the_root_and_ignores_the_host_keys() {
    let c: Config = serde_json::from_value(serde_json::json!({
        "sdk_root": "/opt/ar", "_host": {"compute_capability": [12, 0], "later": 1}
    }))
    .unwrap();
    assert_eq!(c.sdk_root.as_deref(), Some(Path::new("/opt/ar")));
    assert_eq!(c.host.compute_capability, Some((12, 0)));
    let c: Config =
        serde_json::from_value(serde_json::json!({"_host": {"compute_capability": null}})).unwrap();
    assert_eq!(c.host.compute_capability, None);
    assert!(serde_json::from_value::<Config>(serde_json::json!({"sdk-root": "/x"})).is_err());
    assert!(ArModule::init(serde_json::json!({"mode": 1})).is_err());
    assert!(ArModule::init(serde_json::Value::Null).is_ok());
}

/// A fake SDK tree: the core libraries when `core`, the feature libraries
/// of `features` (directory names) and the model files `models`.
fn tree(core: bool, features: &[&str], models: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("lib");
    std::fs::create_dir_all(lib.join("models")).unwrap();
    if core {
        std::fs::write(lib.join("libnvARPose.so"), b"not a library").unwrap();
        std::fs::write(lib.join("libNVCVImage.so.1"), b"not a library").unwrap();
    }
    for f in features {
        let l = dir.path().join("features").join(f).join("lib");
        std::fs::create_dir_all(&l).unwrap();
        std::fs::write(l.join(format!("lib{f}.so")), b"not a library").unwrap();
    }
    for m in models {
        std::fs::write(lib.join("models").join(m), b"").unwrap();
    }
    dir
}

const ALL: &[&str] = &[
    "nvarfaceboxdetection",
    "nvarlandmarkdetection",
    "nvargazeredirection",
];
/// Every model eye contact needs, for sm_120 (the names of the AR SDK 1.2),
/// with a 68-point landmark model that does not count.
const SM120: &[&str] = &[
    "gazeredir_encoder_fp16_multiprofile_120.engine.trtpkg",
    "gazeredir_decoder_fp16_multiprofile_120.engine.trtpkg",
    "face_detection_multiprofile_120.engine.trtpkg",
    "faceland_fp16_multiprofile_126_mode0_120.engine.trtpkg",
    "faceland_fp16_multiprofile_68_mode1_120.engine.trtpkg",
];

/// The reason of `stage` in a report of both stages, in order.
fn reason_of(report: &ProbeReport, stage: &str) -> Option<String> {
    let ids: Vec<&str> = report.stages.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, [EYE_CONTACT, AUTO_FRAME]);
    let s = report.stages.iter().find(|s| s.id == stage).unwrap();
    assert_eq!(s.available, s.reason.is_none());
    s.reason.clone()
}

fn reason(report: &ProbeReport) -> Option<String> {
    reason_of(report, EYE_CONTACT)
}

fn auto_reason(report: &ProbeReport) -> Option<String> {
    reason_of(report, AUTO_FRAME)
}

#[test]
fn the_probe_reads_auto_frame_from_the_face_detection_alone() {
    // Face detection and its models only: auto-frame, no eye contact.
    let only = tree(
        true,
        &["nvarfaceboxdetection"],
        &["face_detection_multiprofile_120.engine.trtpkg"],
    );
    let p = probe_root(Some(only.path()), Some((12, 0)));
    assert_eq!(auto_reason(&p), None);
    assert_eq!(
        reason(&p).as_deref(),
        Some("Maxine AR landmark detection not installed")
    );
    assert_eq!(auto_reason(&probe_root(Some(only.path()), None)), None);
    // Face detection models of another GPU only.
    assert_eq!(
        auto_reason(&probe_root(Some(only.path()), Some((8, 6)))).as_deref(),
        Some("No face detection models for this GPU (sm_86)")
    );
    // Eye contact's models of this GPU, the face detection's of another:
    // neither stage (eye contact finds the face with it too).
    let other = tree(
        true,
        ALL,
        &[
            "gazeredir_encoder_fp16_multiprofile_120.engine.trtpkg",
            "gazeredir_decoder_fp16_multiprofile_120.engine.trtpkg",
            "faceland_fp16_multiprofile_126_mode0_120.engine.trtpkg",
            "face_detection_multiprofile_86.engine.trtpkg",
        ],
    );
    let p = probe_root(Some(other.path()), Some((12, 0)));
    assert_eq!(
        auto_reason(&p).as_deref(),
        Some("No face detection models for this GPU (sm_120)")
    );
    assert_eq!(
        reason(&p).as_deref(),
        Some("No face detection models for this GPU (sm_120)")
    );
    // The face detection of another GPU alone: auto-frame there, and eye
    // contact nowhere but here.
    let p = probe_root(Some(other.path()), Some((8, 6)));
    assert_eq!(auto_reason(&p), None);
    assert_eq!(
        reason(&p).as_deref(),
        Some("No Eye Contact models for this GPU (sm_86)")
    );
    // No face detection models at all.
    let none = tree(true, ALL, &["gazeredir_encoder_120.trtpkg"]);
    assert_eq!(
        auto_reason(&probe_root(Some(none.path()), None)).as_deref(),
        Some("No face detection models installed")
    );
    // No face detection feature: neither stage.
    let no_box = tree(
        true,
        &["nvarlandmarkdetection", "nvargazeredirection"],
        &["face_detection_multiprofile_120.engine.trtpkg"],
    );
    let p = probe_root(Some(no_box.path()), None);
    assert_eq!(
        auto_reason(&p).as_deref(),
        Some("Maxine AR face detection not installed")
    );
    assert_eq!(
        reason(&p).as_deref(),
        Some("Maxine AR face detection not installed")
    );
    // No SDK: the same reason for both.
    let empty = tempfile::tempdir().unwrap();
    for root in [Some(empty.path()), None] {
        let p = probe_root(root, Some((12, 0)));
        assert_eq!(auto_reason(&p).as_deref(), Some(REASON_NO_SDK));
        assert_eq!(reason(&p).as_deref(), Some(REASON_NO_SDK));
    }
}

#[test]
fn the_probe_reads_the_sdk_tree() {
    let full = tree(true, ALL, SM120);
    assert_eq!(reason(&probe_root(Some(full.path()), Some((12, 0)))), None);
    assert_eq!(reason(&probe_root(Some(full.path()), None)), None);
    // Models of another GPU only.
    assert_eq!(
        reason(&probe_root(Some(full.path()), Some((8, 6)))).as_deref(),
        Some("No Eye Contact models for this GPU (sm_86)")
    );
    // No eye-contact models at all.
    let none = tree(
        true,
        ALL,
        &["face_detection_multiprofile_120.engine.trtpkg"],
    );
    assert_eq!(
        reason(&probe_root(Some(none.path()), None)).as_deref(),
        Some("No Eye Contact models installed")
    );
    assert_eq!(
        reason(&probe_root(Some(none.path()), Some((12, 0)))).as_deref(),
        Some("No Eye Contact models for this GPU (sm_120)")
    );
    // Each model eye contact needs, missing, named; the 68-point landmark
    // model is not the 126-point one.
    for (missing, why) in [
        (
            "gazeredir_encoder_",
            "No Eye Contact models for this GPU (sm_120)",
        ),
        (
            "gazeredir_decoder_",
            "No Eye Contact models for this GPU (sm_120)",
        ),
        (
            "face_detection_",
            "No face detection models for this GPU (sm_120)",
        ),
        (
            "faceland_fp16_multiprofile_126",
            "No 126-point face landmark models for this GPU (sm_120)",
        ),
    ] {
        let models: Vec<&str> = SM120
            .iter()
            .copied()
            .filter(|m| !m.starts_with(missing))
            .collect();
        let part = tree(true, ALL, &models);
        assert_eq!(
            reason(&probe_root(Some(part.path()), Some((12, 0)))).as_deref(),
            Some(why),
            "without {missing}"
        );
    }
    let no_land = tree(true, ALL, &SM120[..3]);
    assert_eq!(
        reason(&probe_root(Some(no_land.path()), None)).as_deref(),
        Some("No 126-point face landmark models installed")
    );
    // A missing feature, named.
    for (missing, label) in FEATURES {
        let features: Vec<&str> = ALL.iter().copied().filter(|f| *f != missing).collect();
        let part = tree(true, &features, SM120);
        assert_eq!(
            reason(&probe_root(Some(part.path()), Some((12, 0)))),
            Some(format!("{label} not installed"))
        );
    }
    // No core, an empty root, a missing root, no root.
    let bare = tree(false, ALL, SM120);
    let empty = tempfile::tempdir().unwrap();
    for root in [
        Some(bare.path()),
        Some(empty.path()),
        Some(Path::new("/nonexistent/maxine-ar")),
        None,
    ] {
        assert_eq!(
            reason(&probe_root(root, Some((12, 0)))).as_deref(),
            Some(REASON_NO_SDK),
            "{root:?}"
        );
    }
}

#[test]
fn a_feature_directory_without_a_library_is_not_installed() {
    let t = tree(true, ALL, SM120);
    let lib = t.path().join("features/nvargazeredirection/lib");
    std::fs::remove_file(lib.join("libnvargazeredirection.so")).unwrap();
    std::fs::write(lib.join("README"), b"").unwrap();
    assert_eq!(
        reason(&probe_root(Some(t.path()), None)).as_deref(),
        Some("Maxine AR eye contact not installed")
    );
}

#[test]
fn without_the_sdk_the_check_says_it_is_not_installed() {
    let empty = tempfile::tempdir().unwrap();
    let m = ArModule {
        root: Some(empty.path().to_path_buf()),
        compute_capability: None,
    };
    for stages in [
        &[EYE_CONTACT][..],
        &[AUTO_FRAME],
        &[EYE_CONTACT, AUTO_FRAME],
    ] {
        let e = m.check(stages).unwrap_err();
        assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Unavailable);
        assert!(e.message().contains("not in"), "{}", e.message());
    }
    // Nothing to check for no stage.
    m.check(&[]).unwrap();
    let none = ArModule {
        root: None,
        compute_capability: None,
    };
    assert_eq!(
        none.check(&[EYE_CONTACT]).unwrap_err().message(),
        REASON_NO_SDK
    );
    assert_eq!(
        none.check(&[AUTO_FRAME]).unwrap_err().message(),
        REASON_NO_SDK
    );
}

#[test]
fn an_instance_unloads_when_nothing_was_loaded() {
    let m = ArModule {
        root: None,
        compute_capability: None,
    };
    let mut i = m.create_instance().unwrap();
    i.unload();
    i.unload();
    assert!(!i.eye());
    assert!(!i.auto_frame());
    assert_eq!(i.framing(), framing::Framing::Medium);
    assert_eq!(i.face(), None);
    assert!(i.reset_temporal().unwrap().is_empty());
    assert!(i.status_values().is_empty(), "no auto-frame, no zoom");
}

/// Auto-frame reports its zoom, 1.0 at the full frame included; without
/// the stage there is none.
#[test]
fn auto_frame_reports_its_zoom() {
    assert!(status_values_of(None).is_empty());
    let mut f = framing::Framer::new(1920, 1080, Framing::Tight);
    assert_eq!(
        status_values_of(Some(&f)),
        [StatusValue::new("auto-frame", "zoom", 1.0)]
    );
    let face = framing::Rect {
        x: 930.0,
        y: 510.0,
        width: 60.0,
        height: 60.0,
    };
    for i in 0..300 {
        f.update(f64::from(i) / 30.0, Some(&face));
    }
    let v = status_values_of(Some(&f));
    assert_eq!(
        (v[0].stage.as_str(), v[0].name.as_str()),
        (AUTO_FRAME, ZOOM_VALUE)
    );
    assert!(
        v[0].value > 1.0 && (v[0].value - f.zoom()).abs() < 1e-12,
        "{v:?}"
    );
}

/// A raw chain request of `stages` for `width` x `height` NV12 CUDA frames
/// in place, with a non-null CUDA context (never used: every refusal comes
/// before the SDK). The pointers in `stages` must outlive the request.
fn raw_request(stages: &[*const std::ffi::c_char]) -> abi::obc_chain_request {
    let mut raw = abi::obc_chain_request::empty();
    raw.stages = stages.as_ptr();
    raw.n_stages = stages.len();
    raw.input_kind = abi::OBC_FRAME_CUDA;
    raw.input_format = abi::OBC_FORMAT_NV12;
    raw.input_width = 1280;
    raw.input_height = 720;
    raw.output_format = abi::OBC_FORMAT_NV12;
    raw.output_width = 1280;
    raw.output_height = 720;
    raw.matrix = abi::OBC_MATRIX_BT709;
    raw.chroma_siting = abi::OBC_CHROMA_SITING_MPEG2;
    // Addresses only: no refusal below dereferences them.
    raw.cuda.context = std::ptr::dangling_mut();
    raw.cuda.stream = std::ptr::dangling_mut();
    raw
}

/// Loads `raw` on a fresh instance with no SDK root: the error.
fn load_error(raw: &abi::obc_chain_request) -> Error {
    let m = ArModule {
        root: None,
        compute_capability: None,
    };
    let mut i = m.create_instance().unwrap();
    // SAFETY: the request points to live locals and static strings.
    let req = unsafe { ChainRequest::from_ptr(raw) }.unwrap();
    let e = i.load(&req).unwrap_err();
    // Nothing is loaded after a refusal.
    assert!(!i.eye() && !i.auto_frame());
    e
}

#[test]
fn a_load_refuses_host_frames_other_sizes_unknown_stages_and_no_cuda_context() {
    let both = [c"eye-contact".as_ptr(), c"auto-frame".as_ptr()];
    // Host frames.
    let mut raw = raw_request(&both);
    raw.input_kind = abi::OBC_FRAME_HOST;
    let e = load_error(&raw);
    assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Unsupported);
    assert!(e.message().contains("CUDA frames only"), "{e}");
    // An output of another size, or another format.
    let mut raw = raw_request(&both);
    raw.output_width = 1920;
    raw.output_height = 1080;
    let e = load_error(&raw);
    assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Unsupported);
    assert!(e.message().contains("of the same size"), "{e}");
    let mut raw = raw_request(&both);
    raw.output_format = abi::OBC_FORMAT_BGRA;
    assert_eq!(
        load_error(&raw).kind(),
        obc_module_sdk::ErrorKind::Unsupported
    );
    // A stage of another module.
    let other = [c"eye-contact".as_ptr(), c"denoise".as_ptr()];
    let e = load_error(&raw_request(&other));
    assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Invalid);
    assert!(e.message().contains("no stage \"denoise\""), "{e}");
    // No CUDA context.
    let mut raw = raw_request(&both);
    raw.cuda.context = std::ptr::null_mut();
    let e = load_error(&raw);
    assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Invalid);
    assert!(e.message().contains("CUDA context"), "{e}");
    // None of these is a stage's failure: the host does not drop a stage.
    assert_eq!(e.failed_stage(), None);
}

#[test]
fn a_load_without_the_sdk_fails_the_requests_first_stage() {
    // The SDK missing is no stage's alone: it is the first stage's, in the
    // request's order.
    for (stages, first) in [
        (
            [c"eye-contact".as_ptr(), c"auto-frame".as_ptr()],
            EYE_CONTACT,
        ),
        (
            [c"auto-frame".as_ptr(), c"eye-contact".as_ptr()],
            AUTO_FRAME,
        ),
    ] {
        let e = load_error(&raw_request(&stages));
        assert_eq!(e.failed_stage(), Some(first), "{e}");
        assert!(e.message().contains(REASON_NO_SDK), "{e}");
    }
}

#[test]
fn a_frame_before_a_load_is_refused() {
    let m = ArModule {
        root: None,
        compute_capability: None,
    };
    let mut i = m.create_instance().unwrap();
    let mut raw = abi::obc_frame::empty();
    raw.kind = abi::OBC_FRAME_CUDA;
    raw.format = abi::OBC_FORMAT_NV12;
    raw.width = 1280;
    raw.height = 720;
    // SAFETY: a frame of no planes, which nothing reads.
    let f = unsafe { Frame::from_ptr(&raw) }.unwrap();
    let e = i.process(&f, &f, &[]).unwrap_err();
    assert_eq!(e.kind(), obc_module_sdk::ErrorKind::State);
    // And after an unload.
    i.unload();
    assert_eq!(
        i.process(&f, &f, &[]).unwrap_err().kind(),
        obc_module_sdk::ErrorKind::State
    );
}

#[test]
fn the_manifest_declares_the_stage_and_the_crate_version() {
    let text = include_str!("../maxine-ar.toml");
    let has = |line: &str| text.lines().any(|l| l.trim() == line);
    assert!(has(&format!("id = \"{MODULE_ID}\"")));
    assert!(
        has(&format!("version = \"{}\"", env!("CARGO_PKG_VERSION"))),
        "the version line of crates/obc-module-maxine-ar/maxine-ar.toml must be the crate's \
         (the workspace version): set it to version = \"{}\"",
        env!("CARGO_PKG_VERSION")
    );
    assert!(has(
        "library = \"/usr/lib/omarchy-broadcast/modules/libobc_module_maxine_ar.so\""
    ));
    assert!(has(&format!("id = \"{EYE_CONTACT}\"")));
    assert!(has(&format!("id = \"{AUTO_FRAME}\"")));
}
