//! The element without a GPU: registration, caps, properties, settings
//! and status messages. Nothing here touches CUDA.

use std::sync::Arc;

use gst::glib;
use gst::prelude::*;
use gst_obc::{
    EffectFallback, EffectKind, EffectSettings, EffectStatus, ObcEffect, STATUS_MESSAGE, StageId,
    StageRequest, Value,
};
use obc_backend_maxine::MaxineFactory;

fn element() -> (gst::Element, ObcEffect) {
    gst_obc::register().expect("register obceffect");
    let e = gst::ElementFactory::make(gst_obc::ELEMENT_NAME)
        .build()
        .expect("obceffect is registered");
    let fx = gst_obc::from_element(&e)
        .expect("an obceffect element downcasts")
        .clone();
    (e, fx)
}

/// Applies the element's settings changed by `f`, as a client does.
fn change(fx: &ObcEffect, f: impl FnOnce(&mut EffectSettings)) {
    let mut s = fx.settings();
    f(&mut s);
    fx.apply(&s).unwrap();
}

/// The relight stage on, with `light`, a direction and a brightness.
fn relight(light: &str, direction: i64, brightness: i64) -> StageRequest {
    StageRequest::on(StageId::RELIGHT)
        .with_param("light", light)
        .with_live("direction", direction)
        .with_live("brightness", brightness)
}

#[test]
fn registers_once_and_has_cuda_nv12_pads_on_both_sides() {
    gst_obc::register().unwrap();
    gst_obc::register().expect("registering twice is fine");
    let (e, _) = element();
    let f = e.factory().unwrap();
    assert_eq!(f.name(), "obceffect");
    for name in ["sink", "src"] {
        let caps = e.static_pad(name).unwrap().pad_template_caps();
        assert_eq!(
            caps.to_string(),
            "video/x-raw(memory:CUDAMemory), format=(string)NV12",
            "{name}"
        );
    }
}

#[test]
fn properties_have_their_defaults() {
    let (e, fx) = element();
    assert_eq!(e.property::<String>("effect"), "none");
    assert_eq!(e.property::<f64>("blur-strength"), 0.5);
    assert_eq!(e.property::<String>("background"), "");
    assert_eq!(e.property::<String>("on-failure"), "black");
    assert_eq!(e.property::<String>("fallback-image"), "");
    for stage_property in ["denoise", "relight", "relight-light"] {
        assert!(
            e.find_property(stage_property).is_none(),
            "the stages have no property: {stage_property}"
        );
    }
    assert_eq!(e.property::<String>("status"), "off");
    assert_eq!(fx.settings(), EffectSettings::default());
    assert_eq!(fx.status(), EffectStatus::Off);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Off);
    let s = fx.stats();
    assert_eq!(s.effect, EffectKind::None);
    assert!(s.stages.is_empty());
    let d = s.stage(&StageId::DENOISE);
    assert_eq!(d.status, EffectStatus::Off);
    assert_eq!(d.frames, 0);
    assert_eq!((d.ms_p50, d.ms_p95), (None, None));
    assert!(!s.cuda_active);
    assert_eq!(
        s.frames_processed + s.frames_black + s.frames_passthrough,
        0
    );
    assert!(fx.backend_factory().is_none());
}

#[test]
fn properties_parse_and_invalid_values_are_ignored() {
    let (e, fx) = element();
    e.set_property("effect", "BLUR");
    assert_eq!(e.property::<String>("effect"), "blur");
    assert_eq!(fx.status(), EffectStatus::Loading);
    e.set_property("effect", "sepia");
    assert_eq!(
        e.property::<String>("effect"),
        "blur",
        "an unknown effect is ignored"
    );
    e.set_property("blur-strength", 0.8f64);
    assert!((fx.settings().blur_strength - 0.8).abs() < 1e-6);
    e.set_property("background", "relative.png");
    assert_eq!(
        e.property::<String>("background"),
        "",
        "a relative path is ignored"
    );
    e.set_property("background", "/srv/bg.png");
    assert_eq!(e.property::<String>("background"), "/srv/bg.png");
    e.set_property("on-failure", "none");
    assert_eq!(fx.settings().fallback, EffectFallback::None);
    e.set_property("on-failure", "raw");
    assert_eq!(e.property::<String>("on-failure"), "none");
    e.set_property("on-failure", "image");
    assert_eq!(fx.settings().fallback, EffectFallback::Image);
    e.set_property("fallback-image", "away.png");
    assert_eq!(
        e.property::<String>("fallback-image"),
        "",
        "a relative path is ignored"
    );
    e.set_property("fallback-image", "/srv/away.png");
    assert_eq!(fx.settings().fallback_image, Some("/srv/away.png".into()));
    // No caps yet: nothing is decoded, nothing is reported.
    assert_eq!(fx.stats().fallback_error, None);
    e.set_property("fallback-image", "");
    assert_eq!(fx.settings().fallback_image, None);
    e.set_property("on-failure", "none");
    e.set_property("background", "");
    assert_eq!(fx.settings().background, None);
    e.set_property("effect", "none");
    assert_eq!(fx.status(), EffectStatus::Off);
}

#[test]
fn blur_strength_outside_the_range_is_refused_by_the_param_spec() {
    let (e, _) = element();
    let pspec = e.find_property("blur-strength").unwrap();
    let p = pspec.downcast_ref::<glib::ParamSpecDouble>().unwrap();
    assert_eq!((p.minimum(), p.maximum()), (0.0, 1.0));
    assert!(
        pspec.flags().contains(gst::PARAM_FLAG_MUTABLE_PLAYING),
        "changeable while playing"
    );
}

#[test]
fn apply_validates_and_sets_everything_at_once() {
    let (_, fx) = element();
    let err = fx
        .apply(&EffectSettings::of(EffectKind::Replace))
        .unwrap_err();
    assert!(err.contains("background"), "{err}");
    assert_eq!(fx.settings(), EffectSettings::default(), "nothing applied");
    let bad = EffectSettings {
        blur_strength: 2.0,
        ..EffectSettings::of(EffectKind::Blur)
    };
    assert!(fx.apply(&bad).unwrap_err().contains("0 to 1"));
    let s = EffectSettings {
        kind: EffectKind::Replace,
        blur_strength: 0.25,
        background: Some("/srv/bg.png".into()),
        fallback: EffectFallback::None,
        fallback_image: Some("/srv/away.png".into()),
        stages: vec![
            StageRequest::on(StageId::DENOISE),
            relight("cool", -45, 125),
        ],
    };
    fx.apply(&s).unwrap();
    assert_eq!(fx.settings(), s);
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Loading);
    assert_eq!(
        fx.stats().stage(&StageId::DENOISE).status,
        EffectStatus::Loading
    );
    assert_eq!(fx.stage_status("relight"), EffectStatus::Loading);
    assert_eq!(
        fx.stats().stage(&StageId::RELIGHT).status,
        EffectStatus::Loading
    );
    let mut twice = s.clone();
    twice.stages.push(StageRequest::on(StageId::DENOISE));
    assert!(fx.apply(&twice).unwrap_err().contains("listed twice"));
    // The stages' parameters are the backend's to check.
    fx.set_backend_factory(Some(Arc::new(MaxineFactory::with_root(None))));
    let mut bad = s.clone();
    bad.stages[1] = relight("neutral", 0, 300);
    assert!(fx.apply(&bad).unwrap_err().contains("relight gain"));
    assert_eq!(fx.settings(), s, "nothing applied");
}

#[test]
fn relight_has_a_status_of_its_own() {
    let (_, fx) = element();
    assert_eq!(fx.stage_status("relight"), EffectStatus::Off);
    let st = fx.stats().stage(&StageId::RELIGHT);
    assert_eq!(st.status, EffectStatus::Off);
    assert_eq!(st.frames, 0);
    assert_eq!((st.ms_p50, st.ms_p95), (None, None));
    // none with relight: the effect stays off; the stage loads.
    change(&fx, |s| s.stage_mut(&StageId::RELIGHT).enabled = true);
    assert!(fx.settings().is_enabled("relight"));
    assert_eq!(fx.status(), EffectStatus::Off);
    assert_eq!(fx.stage_status("relight"), EffectStatus::Loading);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Off);
    change(&fx, |s| {
        s.stage_mut(&StageId::RELIGHT)
            .params
            .insert("light".into(), "warm".into());
    });
    change(&fx, |s| {
        let r = s.stage_mut(&StageId::RELIGHT);
        r.live.insert("direction".into(), Value::Int(90));
        r.live.insert("brightness".into(), Value::Int(150));
    });
    assert_eq!(
        fx.settings().stage("relight"),
        Some(&relight("warm", 90, 150))
    );
    change(&fx, |s| s.stage_mut(&StageId::RELIGHT).enabled = false);
    assert_eq!(fx.stage_status("relight"), EffectStatus::Off);
    assert_eq!(
        fx.settings().stage("relight").unwrap().params.get("light"),
        Some(&Value::from("warm")),
        "kept while the stage is off"
    );
}

#[test]
fn denoise_has_a_status_of_its_own() {
    let (e, fx) = element();
    // none with denoise: the effect stays off; the stage loads.
    change(&fx, |s| s.stage_mut(&StageId::DENOISE).enabled = true);
    assert!(fx.settings().is_enabled("denoise"));
    assert_eq!(fx.status(), EffectStatus::Off);
    assert_eq!(e.property::<String>("status"), "off");
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Loading);
    // An effect with it: both load.
    e.set_property("effect", "blur");
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Loading);
    // Turning it off is not a change of the effect.
    change(&fx, |s| s.stage_mut(&StageId::DENOISE).enabled = false);
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.stage_status("denoise"), EffectStatus::Off);
    assert_eq!(
        fx.settings(),
        EffectSettings::of(EffectKind::Blur).with_stage(StageRequest::new(StageId::DENOISE)),
        "the denoise request is part of the settings"
    );
}

#[test]
fn the_new_source_flag_is_a_free_video_buffer_flag() {
    let f = gst_obc::NEW_SOURCE_FLAG;
    assert_eq!(f.bits().count_ones(), 1);
    assert!(!f.intersects(gst::BufferFlags::all()), "a core buffer flag");
    // GST_BUFFER_FLAG_LAST is GST_VIDEO_BUFFER_FLAG_INTERLACED on video
    // buffers: the flag must be past every video flag.
    assert!(f.bits() > gst_video::ffi::GST_VIDEO_BUFFER_FLAG_INTERLACED);
    assert_eq!(f.bits(), gst_video::ffi::GST_VIDEO_BUFFER_FLAG_LAST);
    // It survives a shallow copy, as the output clock pushes frames, and
    // is read with the helper (`flags()` drops bits it has no name for).
    gst::init().unwrap();
    let mut b = gst::Buffer::new();
    assert!(!gst_obc::has_new_source_flag(&b));
    b.get_mut().unwrap().set_flags(f);
    let mut c = b.copy();
    assert!(gst_obc::has_new_source_flag(&c));
    assert!(!c.flags().contains(gst::BufferFlags::DISCONT));
    c.make_mut().unset_flags(f);
    assert!(!gst_obc::has_new_source_flag(&c));
    assert!(gst_obc::has_new_source_flag(&b));
}

#[test]
fn status_changes_are_posted_on_the_bus() {
    let (e, fx) = element();
    let pipeline = gst::Pipeline::new();
    pipeline.add(&e).unwrap();
    let bus = pipeline.bus().unwrap();
    fx.apply(&EffectSettings::of(EffectKind::Blur)).unwrap();
    // Strength alone changes nothing that is loaded: no message.
    e.set_property("blur-strength", 0.9f64);
    e.set_property("effect", "none");
    // A change of the denoise status alone is posted too.
    change(&fx, |s| s.stage_mut(&StageId::DENOISE).enabled = true);
    change(&fx, |s| s.stage_mut(&StageId::DENOISE).enabled = false);
    // So is one of the relight status; direction and gain post nothing, a
    // light that is off neither.
    change(&fx, |s| {
        s.stage_mut(&StageId::RELIGHT)
            .params
            .insert("light".into(), "cool".into());
    });
    change(&fx, |s| s.stage_mut(&StageId::RELIGHT).enabled = true);
    change(&fx, |s| {
        s.stage_mut(&StageId::RELIGHT)
            .live
            .insert("direction".into(), Value::Int(30));
    });
    change(&fx, |s| {
        s.stage_mut(&StageId::RELIGHT)
            .live
            .insert("brightness".into(), Value::Int(75));
    });
    change(&fx, |s| s.stage_mut(&StageId::RELIGHT).enabled = false);
    let mut seen = Vec::new();
    while let Some(m) = bus.pop_filtered(&[gst::MessageType::Element]) {
        let s = m.structure().unwrap();
        if s.name() == STATUS_MESSAGE {
            // A stage the element was never given has no field: it is off.
            let field = |f: &str| {
                s.get::<String>(f).unwrap_or_else(|_| {
                    assert!(
                        f.starts_with("denoise") || f.starts_with("relight"),
                        "{f} is always there"
                    );
                    if f.ends_with("-reason") { "" } else { "off" }.to_string()
                })
            };
            seen.push(
                [
                    "effect",
                    "status",
                    "reason",
                    "denoise",
                    "denoise-reason",
                    "relight",
                    "relight-reason",
                ]
                .map(field)
                .join(" "),
            );
        }
    }
    assert_eq!(
        seen,
        [
            "blur loading  off  off ",
            "none off  off  off ",
            "none off  loading  off ",
            "none off  off  off ",
            "none off  off  loading ",
            "none off  off  off ",
        ]
    );
}
