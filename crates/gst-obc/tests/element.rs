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
    assert!(!e.property::<bool>("denoise"));
    assert_eq!(e.property::<String>("status"), "off");
    assert_eq!(fx.settings(), EffectSettings::default());
    assert_eq!(fx.status(), EffectStatus::Off);
    assert_eq!(fx.denoise_status(), EffectStatus::Off);
    let s = fx.stats();
    assert_eq!(s.effect, EffectKind::None);
    assert_eq!(s.denoise, EffectStatus::Off);
    assert_eq!(s.frames_denoised, 0);
    assert_eq!((s.denoise_ms_p50, s.denoise_ms_p95), (None, None));
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
        denoise: true,
    };
    fx.apply(&s).unwrap();
    assert_eq!(fx.settings(), s);
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.denoise_status(), EffectStatus::Loading);
    assert_eq!(fx.stats().denoise, EffectStatus::Loading);
}

#[test]
fn denoise_is_a_property_of_its_own_and_has_a_status_of_its_own() {
    let (e, fx) = element();
    let pspec = e.find_property("denoise").unwrap();
    assert!(
        pspec.flags().contains(gst::PARAM_FLAG_MUTABLE_PLAYING),
        "changeable while playing"
    );
    // none with denoise: the effect stays off; the stage loads.
    e.set_property("denoise", true);
    assert!(e.property::<bool>("denoise"));
    assert!(fx.settings().denoise);
    assert_eq!(fx.status(), EffectStatus::Off);
    assert_eq!(e.property::<String>("status"), "off");
    assert_eq!(fx.denoise_status(), EffectStatus::Loading);
    // An effect with it: both load.
    e.set_property("effect", "blur");
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.denoise_status(), EffectStatus::Loading);
    // Turning it off is not a change of the effect.
    e.set_property("denoise", false);
    assert_eq!(fx.status(), EffectStatus::Loading);
    assert_eq!(fx.denoise_status(), EffectStatus::Off);
    assert_eq!(
        fx.settings(),
        EffectSettings::of(EffectKind::Blur),
        "the denoise flag is part of the settings"
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
    e.set_property("denoise", true);
    e.set_property("denoise", false);
    let mut seen = Vec::new();
    while let Some(m) = bus.pop_filtered(&[gst::MessageType::Element]) {
        let s = m.structure().unwrap();
        if s.name() == STATUS_MESSAGE {
            let field = |f: &str| s.get::<String>(f).unwrap();
            seen.push(
                ["effect", "status", "reason", "denoise", "denoise-reason"]
                    .map(field)
                    .join(" "),
            );
        }
    }
    assert_eq!(
        seen,
        [
            "blur loading  off ",
            "none off  off ",
            "none off  loading ",
            "none off  off ",
        ]
    );
}
