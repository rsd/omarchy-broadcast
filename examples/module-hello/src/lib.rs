//! An example omarchy-broadcast module, built with `obc-module-sdk`.
//!
//! It provides one stage, `hello`, which tints the picture warm: on host
//! NV12 frames, in place, it moves every chroma sample towards red and away
//! from blue by an amount set with the `strength` setting (0 to 100). Luma
//! is never touched, so the picture keeps its brightness and detail.
//!
//! The module shows the whole surface a module implements:
//!
//! * [`Module::probe`]: the stage needs nothing, so it is always available;
//! * [`Module::settings`]: a `hello` group with an on/off switch for the
//!   stage and a live `strength` parameter;
//! * [`Instance::load`]: reads the load-time strength and the colour range;
//! * [`Instance::process`]: reads the per-frame strength and tints.

use obc_module_sdk::{
    Apply, ChainRequest, Error, Frame, FrameKind, Group, Instance, LoadReport, Module, Op, Param,
    PixelFormat, ProbeReport, Role, Setting, SettingsDescription, log, serde_json,
};

/// The stage id.
pub const STAGE: &str = "hello";
/// The parameter key of the strength.
pub const STRENGTH_KEY: &str = "strength";
/// The strength when none is given.
pub const DEFAULT_STRENGTH: i64 = 50;
/// How far, in 8-bit chroma steps, a strength of 100 moves each sample.
pub const MAX_SHIFT: i64 = 24;

/// The module object.
pub struct HelloModule;

impl Module for HelloModule {
    type Instance = HelloInstance;

    fn init(_config: serde_json::Value) -> Result<Self, Error> {
        log::debug!("hello module initialised");
        Ok(HelloModule)
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new().available(STAGE)
    }

    fn settings(&self) -> SettingsDescription {
        SettingsDescription::new().group(
            Group::new("hello", "Hello module")
                .setting(
                    Setting::bool("effect.hello.enabled", "Warm tint", false)
                        .stage(STAGE)
                        .role(Role::Enable)
                        .hint("switch"),
                )
                .setting(
                    Setting::int(
                        "effect.hello.strength",
                        "Strength",
                        0,
                        100,
                        5,
                        DEFAULT_STRENGTH,
                    )
                    .unit("%")
                    .stage(STAGE)
                    .key(STRENGTH_KEY)
                    .apply(Apply::Live)
                    .enabled_when("effect.hello.enabled", Op::Eq, true)
                    .disabled_reason("Turn on the warm tint to change this")
                    .hint("slider"),
                ),
        )
    }

    fn create_instance(&self) -> Result<HelloInstance, Error> {
        Ok(HelloInstance::default())
    }
}

/// One `hello` chain.
#[derive(Debug, Default)]
pub struct HelloInstance {
    strength: i64,
    full_range: bool,
}

/// The strength in `value`, clamped to 0 to 100.
fn strength_of(value: Option<obc_module_sdk::Value<'_>>) -> Option<i64> {
    value.and_then(|v| v.as_int()).map(|s| s.clamp(0, 100))
}

impl Instance for HelloInstance {
    fn load(&mut self, req: &ChainRequest<'_>) -> Result<LoadReport, Error> {
        if let Some(other) = req.stages().iter().find(|s| **s != STAGE) {
            return Err(Error::invalid(format!("unknown stage {other:?}")));
        }
        if req.input_kind() != FrameKind::Host || req.input_format() != PixelFormat::Nv12 {
            return Err(Error::unsupported(
                "the hello stage runs on host NV12 frames only",
            ));
        }
        if req.output_format() != PixelFormat::Nv12 || req.output_size() != req.input_size() {
            return Err(Error::unsupported(
                "the hello stage does not convert or scale",
            ));
        }
        self.strength = strength_of(req.param(STAGE, STRENGTH_KEY)).unwrap_or(DEFAULT_STRENGTH);
        self.full_range = req.full_range();
        Ok(LoadReport::default())
    }

    fn process(
        &mut self,
        input: &Frame<'_>,
        output: &Frame<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        if let Some(s) = strength_of(Param::find(live, STAGE, STRENGTH_KEY)) {
            self.strength = s;
        }
        for frame in [input, output] {
            if frame.kind() != FrameKind::Host || frame.format() != PixelFormat::Nv12 {
                return Err(Error::unsupported(
                    "the hello stage runs on host NV12 frames only",
                ));
            }
        }
        if (input.width(), input.height()) != (output.width(), output.height()) {
            return Err(Error::invalid("input and output sizes differ"));
        }
        if !input.same_memory(output) {
            if input.overlaps(output) {
                return Err(Error::invalid(
                    "the input and output frames overlap without being the same memory",
                ));
            }
            copy_plane(input, output, 0)?;
            copy_plane(input, output, 1)?;
        }
        let pitch = output.plane(1).map_or(0, |p| p.pitch as usize);
        let (rows, row_bytes) = output.plane_geometry(1).unwrap_or((0, 0));
        // SAFETY: the host owns the output frame for the call and nothing
        // else accesses its chroma plane meanwhile (the input's planes are
        // no longer borrowed).
        let uv = unsafe { output.host_plane_mut(1) }?;
        tint_nv12_chroma(uv, pitch, rows, row_bytes, self.strength, self.full_range);
        Ok(())
    }

    fn unload(&mut self) {}
}

/// Copies plane `index` of `input` into `output`, row by row.
fn copy_plane(input: &Frame<'_>, output: &Frame<'_>, index: usize) -> Result<(), Error> {
    let (rows, row_bytes) = input
        .plane_geometry(index)
        .ok_or_else(|| Error::invalid("not an NV12 frame"))?;
    let src_pitch = input.plane(index).map_or(0, |p| p.pitch as usize);
    let dst_pitch = output.plane(index).map_or(0, |p| p.pitch as usize);
    // SAFETY: frames owned by the host for the call that share no byte
    // (checked by the caller); the input is only read and the output only
    // written.
    let (src, dst) = unsafe { (input.host_plane(index)?, output.host_plane_mut(index)?) };
    for row in 0..rows {
        dst[row * dst_pitch..row * dst_pitch + row_bytes]
            .copy_from_slice(&src[row * src_pitch..row * src_pitch + row_bytes]);
    }
    Ok(())
}

/// Tints the interleaved UV plane of an NV12 frame warm: each U (blue
/// difference) sample goes down and each V (red difference) sample goes up
/// by `strength * MAX_SHIFT / 100`, rounded, kept within the range (16 to
/// 240 limited, 0 to 255 full). `rows` rows of `row_bytes` bytes, `pitch`
/// bytes apart. A strength of 0 changes nothing.
pub fn tint_nv12_chroma(
    uv: &mut [u8],
    pitch: usize,
    rows: usize,
    row_bytes: usize,
    strength: i64,
    full_range: bool,
) {
    let shift = (strength.clamp(0, 100) * MAX_SHIFT + 50) / 100;
    if shift == 0 {
        return;
    }
    let (lo, hi) = if full_range { (0, 255) } else { (16, 240) };
    for row in 0..rows {
        let start = row * pitch;
        let Some(line) = uv.get_mut(start..start + row_bytes) else {
            return;
        };
        for [u, v] in line.as_chunks_mut::<2>().0 {
            *u = (i64::from(*u) - shift).clamp(lo, hi) as u8;
            *v = (i64::from(*v) + shift).clamp(lo, hi) as u8;
        }
    }
}

obc_module_sdk::export_module!(HelloModule);

#[cfg(test)]
mod tests {
    use super::*;
    use obc_module_sdk::abi::{
        OBC_FORMAT_NV12, OBC_FRAME_HOST, obc_chain_request, obc_frame, obc_param, obc_plane,
        obc_value,
    };

    const W: u32 = 4;
    const H: u32 = 2;

    /// A tightly packed host NV12 frame over `buf`: 4x2 luma, then one row
    /// of two UV pairs.
    fn frame(buf: &mut [u8]) -> obc_frame {
        let mut f = obc_frame::empty();
        f.kind = OBC_FRAME_HOST;
        f.format = OBC_FORMAT_NV12;
        f.width = W;
        f.height = H;
        f.n_planes = 2;
        let base = buf.as_mut_ptr() as u64;
        f.planes[0] = obc_plane {
            ptr_or_fd: base,
            pitch: W as u64,
            offset: 0,
        };
        f.planes[1] = obc_plane {
            ptr_or_fd: base,
            pitch: W as u64,
            offset: (W * H) as u64,
        };
        f
    }

    fn loaded(strength: Option<i64>) -> HelloInstance {
        let stages = [c"hello".as_ptr()];
        let params = strength.map(|s| obc_param {
            stage: c"hello".as_ptr(),
            key: c"strength".as_ptr(),
            value: obc_value::from_int(s),
        });
        let mut raw = obc_chain_request::empty();
        raw.stages = stages.as_ptr();
        raw.n_stages = 1;
        raw.input_kind = OBC_FRAME_HOST;
        raw.input_format = OBC_FORMAT_NV12;
        raw.input_width = W;
        raw.input_height = H;
        raw.output_format = OBC_FORMAT_NV12;
        raw.output_width = W;
        raw.output_height = H;
        if let Some(p) = &params {
            raw.params = p;
            raw.n_params = 1;
        }
        // SAFETY: everything lives across the call.
        let req = unsafe { ChainRequest::from_ptr(&raw) }.unwrap();
        let mut instance = HelloModule.create_instance().unwrap();
        instance.load(&req).unwrap();
        instance
    }

    fn run(instance: &mut HelloInstance, buf: &mut [u8], live: &[Param<'_>]) {
        let raw = frame(buf);
        let f = Frame::new(&raw).unwrap();
        instance.process(&f, &f, live).unwrap();
    }

    #[test]
    fn strength_zero_changes_nothing() {
        let mut uv = [128, 128, 200, 60];
        tint_nv12_chroma(&mut uv, 4, 1, 4, 0, false);
        assert_eq!(uv, [128, 128, 200, 60]);
    }

    #[test]
    fn chroma_moves_with_strength() {
        let mut uv = [128, 128];
        tint_nv12_chroma(&mut uv, 2, 1, 2, 100, false);
        assert_eq!(uv, [128 - 24, 128 + 24]);
        let mut uv = [128, 128];
        tint_nv12_chroma(&mut uv, 2, 1, 2, 50, false);
        assert_eq!(uv, [116, 140]);
    }

    #[test]
    fn chroma_stays_in_range() {
        let mut uv = [20, 235];
        tint_nv12_chroma(&mut uv, 2, 1, 2, 100, false);
        assert_eq!(uv, [16, 240]);
        let mut uv = [20, 235];
        tint_nv12_chroma(&mut uv, 2, 1, 2, 100, true);
        assert_eq!(uv, [0, 255]);
    }

    #[test]
    fn row_padding_is_left_alone() {
        // Two rows of one UV pair, pitch 4: bytes 2, 3 are padding.
        let mut uv = [128, 128, 7, 7, 128, 128];
        tint_nv12_chroma(&mut uv, 4, 2, 2, 100, true);
        assert_eq!(uv, [104, 152, 7, 7, 104, 152]);
    }

    #[test]
    fn process_tints_chroma_and_keeps_luma() {
        let mut instance = loaded(None);
        let mut buf = [60u8; 12];
        buf[8..].fill(128);
        run(&mut instance, &mut buf, &[]);
        assert_eq!(&buf[..8], &[60; 8], "luma untouched");
        assert_eq!(&buf[8..], &[116, 140, 116, 140], "default strength 50");
    }

    #[test]
    fn load_and_live_strengths_apply() {
        let mut instance = loaded(Some(0));
        let mut buf = [128u8; 12];
        run(&mut instance, &mut buf, &[]);
        assert_eq!(buf, [128; 12], "load-time strength 0");

        let live = [Param {
            stage: STAGE,
            key: STRENGTH_KEY,
            value: obc_module_sdk::Value::Int(100),
        }];
        run(&mut instance, &mut buf, &live);
        assert_eq!(&buf[8..], &[104, 152, 104, 152], "live strength 100");
        // The live value stays until the next one.
        let mut buf = [128u8; 12];
        run(&mut instance, &mut buf, &[]);
        assert_eq!(&buf[8..], &[104, 152, 104, 152]);
    }

    #[test]
    fn process_copies_into_a_separate_output() {
        let mut instance = loaded(Some(100));
        let mut src = [50u8; 12];
        src[8..].fill(128);
        let mut dst = [0u8; 12];
        let (raw_in, raw_out) = (frame(&mut src), frame(&mut dst));
        let (input, output) = (Frame::new(&raw_in).unwrap(), Frame::new(&raw_out).unwrap());
        instance.process(&input, &output, &[]).unwrap();
        assert_eq!(&dst[..8], &[50; 8]);
        assert_eq!(&dst[8..], &[104, 152, 104, 152]);
        assert_eq!(&src[8..], &[128; 4], "input untouched");
    }

    #[test]
    fn describes_its_stage_and_settings() {
        let module = HelloModule::init(serde_json::json!({})).unwrap();
        assert_eq!(
            module.probe().to_json(),
            r#"{"stages":[{"id":"hello","available":true}]}"#
        );
        let json: serde_json::Value = serde_json::from_str(&module.settings().to_json()).unwrap();
        let group = &json["groups"][0];
        assert_eq!(
            (group["id"].as_str(), group["label"].as_str()),
            (Some("hello"), Some("Hello module"))
        );
        let enabled = &group["settings"][0];
        assert_eq!(enabled["id"], "effect.hello.enabled");
        assert_eq!(
            (enabled["type"].as_str(), enabled["role"].as_str()),
            (Some("bool"), Some("enable"))
        );
        assert_eq!(enabled["stage"], "hello");
        let strength = &group["settings"][1];
        assert_eq!(strength["id"], "effect.hello.strength");
        assert_eq!(
            (
                &strength["min"],
                &strength["max"],
                &strength["step"],
                &strength["default"]
            ),
            (
                &serde_json::json!(0),
                &serde_json::json!(100),
                &serde_json::json!(5),
                &serde_json::json!(50)
            )
        );
        assert_eq!(
            (strength["key"].as_str(), strength["apply"].as_str()),
            (Some("strength"), Some("live"))
        );
        assert_eq!(
            strength["enabled_when"],
            serde_json::json!([["effect.hello.enabled", "eq", true]])
        );
    }

    #[test]
    fn refuses_other_frames() {
        let stages = [c"hello".as_ptr()];
        let mut raw = obc_chain_request::empty();
        raw.stages = stages.as_ptr();
        raw.n_stages = 1;
        raw.input_kind = obc_module_sdk::abi::OBC_FRAME_CUDA;
        raw.input_format = OBC_FORMAT_NV12;
        // SAFETY: everything lives across the call.
        let req = unsafe { ChainRequest::from_ptr(&raw) }.unwrap();
        let err = HelloInstance::default().load(&req).unwrap_err();
        assert_eq!(err.kind(), obc_module_sdk::ErrorKind::Unsupported);
    }
}
