//! The element without a GPU: registration, caps, properties, settings
//! and status messages. Nothing here touches CUDA.

use gst::glib;
use gst::prelude::*;
use gst_obc::{
    EffectFallback, EffectKind, EffectSettings, EffectStatus, ObcEffect, STATUS_MESSAGE,
};

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
    assert_eq!(e.property::<String>("status"), "off");
    assert_eq!(fx.settings(), EffectSettings::default());
    assert_eq!(fx.status(), EffectStatus::Off);
    let s = fx.stats();
    assert_eq!(s.effect, EffectKind::None);
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
    };
    fx.apply(&s).unwrap();
    assert_eq!(fx.settings(), s);
    assert_eq!(fx.status(), EffectStatus::Loading);
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
    let mut seen = Vec::new();
    while let Some(m) = bus.pop_filtered(&[gst::MessageType::Element]) {
        let s = m.structure().unwrap();
        if s.name() == STATUS_MESSAGE {
            seen.push((
                s.get::<String>("effect").unwrap(),
                s.get::<String>("status").unwrap(),
                s.get::<String>("reason").unwrap(),
            ));
        }
    }
    assert_eq!(
        seen,
        [
            ("blur".to_string(), "loading".to_string(), String::new()),
            ("none".to_string(), "off".to_string(), String::new()),
        ]
    );
}
