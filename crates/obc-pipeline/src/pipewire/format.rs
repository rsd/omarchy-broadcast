//! The formats the node offers, their memory layouts, and the SPA params
//! built from them: the EnumFormat list, the parse of the format a
//! consumer chose, and the Buffers and Meta params that answer it.

use obc_core::{OutputContract, OutputFormat};
use pipewire::spa;
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::serialize::PodSerializer;
use spa::pod::{ChoiceValue, Object, Pod, Property, Value, object, property};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

/// A raw video format the node offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PwFormat {
    /// 4:2:0, a Y plane and an interleaved UV plane.
    Nv12,
    /// 4:2:2 packed, `Y0 U Y1 V`.
    Yuy2,
    /// 4:2:0, Y, U and V planes.
    I420,
}

impl PwFormat {
    /// The formats in their canonical order (the contract's format first is
    /// [`offered_formats`]'s business).
    pub const ALL: [PwFormat; 3] = [PwFormat::Nv12, PwFormat::Yuy2, PwFormat::I420];

    /// The SPA video format.
    pub fn spa(self) -> VideoFormat {
        match self {
            PwFormat::Nv12 => VideoFormat::NV12,
            PwFormat::Yuy2 => VideoFormat::YUY2,
            PwFormat::I420 => VideoFormat::I420,
        }
    }

    /// The offered format a SPA video format names, if any.
    pub fn from_spa(format: VideoFormat) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.spa() == format)
    }

    /// The GStreamer `format=` name.
    pub fn gst_name(self) -> &'static str {
        match self {
            PwFormat::Nv12 => "NV12",
            PwFormat::Yuy2 => "YUY2",
            PwFormat::I420 => "I420",
        }
    }
}

impl From<OutputFormat> for PwFormat {
    fn from(f: OutputFormat) -> Self {
        match f {
            OutputFormat::Nv12 => PwFormat::Nv12,
            OutputFormat::I420 => PwFormat::I420,
        }
    }
}

impl std::fmt::Display for PwFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.gst_name())
    }
}

/// The EnumFormat order: the contract's format first (no conversion for a
/// consumer that takes the first entry), then the other two in
/// [`PwFormat::ALL`] order.
pub fn offered_formats(contract: &OutputContract) -> [PwFormat; 3] {
    let first = PwFormat::from(contract.format);
    let mut out = [first; 3];
    let mut i = 1;
    for f in PwFormat::ALL {
        if f != first {
            out[i] = f;
            i += 1;
        }
    }
    out
}

fn round_up_2(v: usize) -> usize {
    (v + 1) & !1
}

fn round_up_4(v: usize) -> usize {
    (v + 3) & !3
}

/// Where the planes of one frame lie in a single contiguous block: the
/// layout `GstVideoInfo` gives the same caps by default (rows padded to 4
/// bytes; the I420 chroma stride rounds up to 4 too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub format: PwFormat,
    pub width: usize,
    pub height: usize,
    pub n_planes: usize,
    /// Byte offset of each plane (unused entries 0).
    pub offsets: [usize; 3],
    /// Bytes per row of each plane (unused entries 0).
    pub strides: [usize; 3],
    /// Bytes of the whole frame.
    pub size: usize,
}

impl Layout {
    pub fn of(format: PwFormat, width: u32, height: u32) -> Self {
        let (w, h) = (width as usize, height as usize);
        // 4:2:0 chroma rows: GStreamer rounds the height up to even.
        let chroma_rows = round_up_2(h) / 2;
        match format {
            PwFormat::Nv12 => {
                let stride = round_up_4(w);
                let uv = stride * round_up_2(h);
                Layout {
                    format,
                    width: w,
                    height: h,
                    n_planes: 2,
                    offsets: [0, uv, 0],
                    strides: [stride, stride, 0],
                    size: uv + stride * chroma_rows,
                }
            }
            PwFormat::I420 => {
                let ys = round_up_4(w);
                let cs = round_up_4(round_up_2(w) / 2);
                let u = ys * round_up_2(h);
                let v = u + cs * chroma_rows;
                Layout {
                    format,
                    width: w,
                    height: h,
                    n_planes: 3,
                    offsets: [0, u, v],
                    strides: [ys, cs, cs],
                    size: v + cs * chroma_rows,
                }
            }
            PwFormat::Yuy2 => {
                let stride = round_up_4(round_up_2(w) * 2);
                Layout {
                    format,
                    width: w,
                    height: h,
                    n_planes: 1,
                    offsets: [0; 3],
                    strides: [stride, 0, 0],
                    size: stride * h,
                }
            }
        }
    }
}

/// Serialises a pod value into its bytes.
fn serialize(value: &Value) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
        .expect("serialising a pod into memory cannot fail")
        .0
        .into_inner()
}

/// The EnumFormat object for one format at the contract's exact size and
/// frame rate: fixed values, no ranges, so a consumer asking another size
/// or rate finds no intersection.
pub fn enum_format(contract: &OutputContract, format: PwFormat) -> Object {
    object! {
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(FormatProperties::VideoFormat, Id, format.spa()),
        property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: contract.width,
                height: contract.height,
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction {
                num: contract.fps_num,
                denom: contract.fps_den,
            }
        ),
    }
}

/// The serialised EnumFormat pods, in [`offered_formats`] order.
pub fn enum_format_pods(contract: &OutputContract) -> Vec<Vec<u8>> {
    offered_formats(contract)
        .into_iter()
        .map(|f| serialize(&Value::Object(enum_format(contract, f))))
        .collect()
}

/// The format a consumer chose (a `Format` param), checked against what
/// was offered: one of the three formats at the contract's size and frame
/// rate. Anything else is refused with the reason.
pub fn negotiate(pod: &Pod, contract: &OutputContract) -> Result<Layout, String> {
    let (media_type, media_subtype) = spa::param::format_utils::parse_format(pod)
        .map_err(|e| format!("not a format param: {e}"))?;
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return Err(format!("not raw video: {media_type:?}/{media_subtype:?}"));
    }
    let mut info = VideoInfoRaw::new();
    info.parse(pod)
        .map_err(|e| format!("unparsable raw video format: {e}"))?;
    let format = PwFormat::from_spa(info.format())
        .ok_or_else(|| format!("format {:?} not offered", info.format()))?;
    let size = info.size();
    if (size.width, size.height) != (contract.width, contract.height) {
        return Err(format!(
            "size {}x{} not offered (only {}x{})",
            size.width, size.height, contract.width, contract.height
        ));
    }
    let rate = info.framerate();
    // Compared as fractions: 60/2 is 30/1. A 0/1 rate (variable) is not
    // what was offered either.
    if u64::from(rate.num) * u64::from(contract.fps_den)
        != u64::from(contract.fps_num) * u64::from(rate.denom)
        || rate.denom == 0
    {
        return Err(format!(
            "frame rate {}/{} not offered (only {}/{})",
            rate.num, rate.denom, contract.fps_num, contract.fps_den
        ));
    }
    Ok(Layout::of(format, contract.width, contract.height))
}

/// Buffers allocated per stream: the default, minimum and maximum asked.
/// A consumer holding a few frames (a GStreamer `appsink` queue, say)
/// still leaves free buffers; with 4, such a consumer made every fourth
/// frame wait for none. Shared memory is allocated only while a consumer
/// is linked.
pub const BUFFERS: (i32, i32, i32) = (8, 2, 16);

fn int_choice(default: i32, min: i32, max: i32) -> Value {
    Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range { default, min, max },
    )))
}

/// The Buffers param answering a chosen format: [`BUFFERS`] buffers of one
/// data block holding a whole frame of `layout`, in shared memory (memfd)
/// or plain memory, mapped by the stream.
pub fn buffers_param(layout: &Layout) -> Vec<u8> {
    let (default, min, max) = BUFFERS;
    let int = |v: usize| Value::Int(i32::try_from(v).unwrap_or(i32::MAX));
    let data_types = (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr);
    serialize(&Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                int_choice(default, min, max),
            ),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_size, int(layout.size)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, int(layout.strides[0])),
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: data_types,
                        flags: vec![],
                    },
                ))),
            ),
        ],
    }))
}

/// The Meta param asking for a `spa_meta_header` on every buffer (PTS and
/// sequence number).
pub fn meta_header_param() -> Vec<u8> {
    serialize(&Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(Id(spa::sys::SPA_META_Header)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            ),
        ],
    }))
}

/// Views serialised pod bytes as a [`Pod`] (bytes this module built).
pub fn as_pod(bytes: &[u8]) -> &Pod {
    Pod::from_bytes(bytes).expect("pod bytes built by this module")
}

#[cfg(test)]
mod tests {
    use obc_core::OutputFormat;
    use spa::pod::deserialize::PodDeserializer;

    use super::*;

    fn contract(format: OutputFormat, s: &str) -> OutputContract {
        OutputContract {
            format,
            ..s.parse().unwrap()
        }
    }

    #[test]
    fn the_contract_format_is_offered_first() {
        let nv12 = OutputContract::default();
        assert_eq!(
            offered_formats(&nv12),
            [PwFormat::Nv12, PwFormat::Yuy2, PwFormat::I420]
        );
        let i420 = contract(OutputFormat::I420, "1920x1080@30");
        assert_eq!(
            offered_formats(&i420),
            [PwFormat::I420, PwFormat::Nv12, PwFormat::Yuy2]
        );
        for f in PwFormat::ALL {
            assert_eq!(PwFormat::from_spa(f.spa()), Some(f));
        }
        assert_eq!(PwFormat::from_spa(VideoFormat::RGBA), None);
    }

    fn video_info(format: PwFormat, w: u32, h: u32) -> gst_video::VideoInfo {
        let caps: gst::Caps = format!(
            "video/x-raw,format={},width={w},height={h},framerate=30/1",
            format.gst_name()
        )
        .parse()
        .unwrap();
        gst_video::VideoInfo::from_caps(&caps).unwrap()
    }

    /// Every layout equals `GstVideoInfo`'s default for the same caps,
    /// including a width whose half is odd (I420 chroma stride rounded up
    /// to 4) and odd sizes.
    #[test]
    fn layouts_match_video_info() {
        gst::init().unwrap();
        for (w, h) in [(1920, 1080), (1918, 1080), (642, 480), (320, 240), (17, 9)] {
            for f in PwFormat::ALL {
                let l = Layout::of(f, w, h);
                let info = video_info(f, w, h);
                let n = info.n_planes() as usize;
                assert_eq!(l.n_planes, n, "{f} {w}x{h}");
                assert_eq!(l.size, info.size(), "{f} {w}x{h} size");
                for p in 0..n {
                    assert_eq!(l.offsets[p], info.offset()[p], "{f} {w}x{h} offset {p}");
                    assert_eq!(
                        l.strides[p],
                        info.stride()[p] as usize,
                        "{f} {w}x{h} stride {p}"
                    );
                }
            }
        }
        // The I420 chroma of 1918 wide: 959 bytes, padded to 960.
        assert_eq!(Layout::of(PwFormat::I420, 1918, 1080).strides[1], 960);
        assert_eq!(
            Layout::of(PwFormat::Nv12, 1920, 1080).size,
            1920 * 1080 * 3 / 2
        );
        assert_eq!(Layout::of(PwFormat::Yuy2, 1920, 1080).size, 1920 * 1080 * 2);
    }

    fn prop(obj: &Object, key: FormatProperties) -> &Value {
        &obj.properties
            .iter()
            .find(|p| p.key == key.as_raw())
            .unwrap_or_else(|| panic!("no {key:?}"))
            .value
    }

    /// The EnumFormat pods deserialise back to what was built: one fixed
    /// format each, in offer order, at the contract's size and rate.
    #[test]
    fn enum_format_pods_round_trip() {
        let c = contract(OutputFormat::I420, "1280x720@30000/1001");
        let pods = enum_format_pods(&c);
        assert_eq!(pods.len(), 3);
        for (bytes, want) in pods.iter().zip(offered_formats(&c)) {
            let (rest, value): (_, Value) = PodDeserializer::deserialize_from(bytes).unwrap();
            assert!(rest.is_empty());
            let Value::Object(obj) = value else {
                panic!("not an object: {value:?}")
            };
            assert_eq!(obj.type_, spa::sys::SPA_TYPE_OBJECT_Format);
            assert_eq!(obj.id, spa::sys::SPA_PARAM_EnumFormat);
            assert_eq!(obj, enum_format(&c, want));
            assert_eq!(
                prop(&obj, FormatProperties::VideoFormat),
                &Value::Id(Id(want.spa().as_raw()))
            );
            assert_eq!(
                prop(&obj, FormatProperties::VideoSize),
                &Value::Rectangle(Rectangle {
                    width: 1280,
                    height: 720
                })
            );
            assert_eq!(
                prop(&obj, FormatProperties::VideoFramerate),
                &Value::Fraction(Fraction {
                    num: 30000,
                    denom: 1001
                })
            );
            // The node's own parse reads the same values back.
            let layout = negotiate(as_pod(bytes), &c).unwrap();
            assert_eq!(layout, Layout::of(want, 1280, 720));
        }
    }

    /// A Format param as a consumer would fix it.
    fn format_pod(format: VideoFormat, w: u32, h: u32, num: u32, denom: u32) -> Vec<u8> {
        serialize(&Value::Object(object! {
            SpaTypes::ObjectParamFormat,
            ParamType::Format,
            property!(FormatProperties::MediaType, Id, MediaType::Video),
            property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            property!(FormatProperties::VideoFormat, Id, format),
            property!(
                FormatProperties::VideoSize,
                Rectangle,
                Rectangle {
                    width: w,
                    height: h
                }
            ),
            property!(FormatProperties::VideoFramerate, Fraction, Fraction { num, denom }),
        }))
    }

    /// Each offered format maps to its layout; an unoffered format, size or
    /// rate, or another media type, is refused.
    #[test]
    fn negotiation_maps_offered_formats_and_refuses_others() {
        let c = OutputContract::default();
        for f in PwFormat::ALL {
            let pod = format_pod(f.spa(), 1920, 1080, 30, 1);
            assert_eq!(
                negotiate(as_pod(&pod), &c),
                Ok(Layout::of(f, 1920, 1080)),
                "{f}"
            );
        }
        // 60/2 is the contract's 30/1.
        let pod = format_pod(VideoFormat::NV12, 1920, 1080, 60, 2);
        assert!(negotiate(as_pod(&pod), &c).is_ok());
        for (pod, why) in [
            (
                format_pod(VideoFormat::RGBA, 1920, 1080, 30, 1),
                "not offered",
            ),
            (
                format_pod(VideoFormat::NV12, 640, 480, 30, 1),
                "size 640x480",
            ),
            (
                format_pod(VideoFormat::YUY2, 1920, 1080, 60, 1),
                "frame rate 60/1",
            ),
            (
                format_pod(VideoFormat::I420, 1920, 1080, 0, 1),
                "frame rate 0/1",
            ),
        ] {
            let err = negotiate(as_pod(&pod), &c).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        let audio = serialize(&Value::Object(object! {
            SpaTypes::ObjectParamFormat,
            ParamType::Format,
            property!(FormatProperties::MediaType, Id, MediaType::Audio),
            property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        }));
        assert!(
            negotiate(as_pod(&audio), &c)
                .unwrap_err()
                .contains("not raw video")
        );
    }

    /// The Buffers and Meta params carry the layout's size and stride, one
    /// block, the buffer count range, memfd or plain memory, and the header
    /// meta's size.
    #[test]
    fn buffers_and_meta_params_round_trip() {
        let l = Layout::of(PwFormat::Yuy2, 1920, 1080);
        let bytes = buffers_param(&l);
        let (_, Value::Object(obj)): (_, Value) =
            PodDeserializer::deserialize_from(&bytes).unwrap()
        else {
            panic!("not an object")
        };
        assert_eq!(obj.type_, spa::sys::SPA_TYPE_OBJECT_ParamBuffers);
        assert_eq!(obj.id, spa::sys::SPA_PARAM_Buffers);
        let get = |k: u32| &obj.properties.iter().find(|p| p.key == k).unwrap().value;
        assert_eq!(get(spa::sys::SPA_PARAM_BUFFERS_blocks), &Value::Int(1));
        assert_eq!(
            get(spa::sys::SPA_PARAM_BUFFERS_size),
            &Value::Int(1920 * 1080 * 2)
        );
        assert_eq!(get(spa::sys::SPA_PARAM_BUFFERS_stride), &Value::Int(3840));
        assert_eq!(
            get(spa::sys::SPA_PARAM_BUFFERS_buffers),
            &int_choice(8, 2, 16)
        );
        let Value::Choice(ChoiceValue::Int(Choice(_, ChoiceEnum::Flags { default, .. }))) =
            get(spa::sys::SPA_PARAM_BUFFERS_dataType)
        else {
            panic!("dataType is not a flags choice")
        };
        assert_eq!(
            *default,
            (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)
        );

        let bytes = meta_header_param();
        let (_, Value::Object(obj)): (_, Value) =
            PodDeserializer::deserialize_from(&bytes).unwrap()
        else {
            panic!("not an object")
        };
        assert_eq!(obj.type_, spa::sys::SPA_TYPE_OBJECT_ParamMeta);
        assert_eq!(
            obj.properties[0].value,
            Value::Id(Id(spa::sys::SPA_META_Header))
        );
        assert_eq!(obj.properties[1].value, Value::Int(32));
    }
}
