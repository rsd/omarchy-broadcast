//! Version gating: the host presents itself to a module as a host of the
//! module's declared minor version of ABI 1 (what `obc_module_abi_version`
//! returns). It sets no flag bit, passes no constant, sets no
//! `struct_size` larger than that version's, accepts no settings type and
//! calls no vtable entry that the declared version does not define,
//! whatever the size of the module's vtable says.
//!
//! Each thing the ABI added after 1.0 that crosses the boundary is one
//! line of a table here, with the minor version that defines it; a later
//! minor version adds its lines and nothing else. The minor versions:
//!
//! * 1.1: [`OBC_FRAME_FLAG_REPEAT`];
//! * 1.2: the audio entries, structures, flags and format;
//! * 1.3: `instance_status_values`;
//! * 1.4: `instance_set_background`;
//! * 1.5: the load report's `notice` (its `struct_size` grows from
//!   [`OBC_LOAD_REPORT_SIZE_1_0`] to [`OBC_LOAD_REPORT_SIZE_1_5`]);
//! * 1.6: nothing a module is handed (rules, names and the per-kind
//!   required entries, [`KIND_ENTRIES_MINOR`]); the reserved names are
//!   enforced from it on ([`RESERVED_NAMES_MINOR`]).
//!
//! The 1.0 entries `instance_park` and `instance_unpark` are reserved: no
//! minor version of ABI 1 defines a call of them yet, so the host never
//! calls them ([`RESERVED_ENTRIES`]).

use obc_module_abi::{
    OBC_AUDIO_BUFFER_SIZE_1_2, OBC_AUDIO_F32_PLANAR, OBC_AUDIO_FLAG_DISCONT,
    OBC_AUDIO_FLAG_NEW_SOURCE, OBC_AUDIO_LOAD_REPORT_SIZE_1_2, OBC_AUDIO_REQUEST_SIZE_1_2,
    OBC_CHAIN_REQUEST_SIZE_1_0, OBC_CHROMA_SITING_COSITED, OBC_CHROMA_SITING_JPEG,
    OBC_CHROMA_SITING_MPEG2, OBC_FORMAT_BGR, OBC_FORMAT_BGRA, OBC_FORMAT_I420, OBC_FORMAT_NV12,
    OBC_FORMAT_RGB_F32_PLANAR, OBC_FORMAT_RGBA, OBC_FRAME_CUDA, OBC_FRAME_DMABUF,
    OBC_FRAME_FLAG_NEW_SOURCE, OBC_FRAME_FLAG_REPEAT, OBC_FRAME_HOST, OBC_FRAME_SIZE_1_0,
    OBC_LOAD_REPORT_SIZE_1_0, OBC_LOAD_REPORT_SIZE_1_5, OBC_MATRIX_BT601, OBC_MATRIX_BT709,
    OBC_MATRIX_BT2020, OBC_VALUE_BOOL, OBC_VALUE_FLOAT, OBC_VALUE_INT, OBC_VALUE_STRING,
    obc_module_abi_minor, obc_module_vtable,
};

/// The first minor version with [`OBC_FRAME_FLAG_REPEAT`].
pub const REPEAT_FLAG_MINOR: u16 = 1;
/// The first minor version with audio.
pub const AUDIO_MINOR: u16 = 2;
/// The first minor version with `instance_status_values`.
pub const STATUS_VALUES_MINOR: u16 = 3;
/// The first minor version with the per-frame background
/// (`instance_set_background`).
pub const LIVE_BACKGROUND_MINOR: u16 = 4;
/// The first minor version with the load report's `notice`.
pub const LOAD_NOTICE_MINOR: u16 = 5;
/// The first minor version whose hosts require the entries of a module's
/// own kind only (an `audio-filter` module may leave `instance_load` and
/// `instance_process` null). The host relaxes for every module: the rule
/// is the host's, and a module of an older version sets them anyway.
pub const KIND_ENTRIES_MINOR: u16 = 6;
/// The first minor version whose modules are held to the host's reserved
/// names: a configuration key starting with `_` that the host does not
/// define is not passed to the module, and a status value whose name
/// starts with `obc.` is dropped. A module of an older version gets both,
/// as a host of 1.5 gave them, with a warning in its log.
pub const RESERVED_NAMES_MINOR: u16 = 6;

/// Whether the host holds a module of minor version `minor` to the
/// reserved names ([`RESERVED_NAMES_MINOR`]).
pub fn enforces_reserved_names(minor: u16) -> bool {
    minor >= RESERVED_NAMES_MINOR
}

/// The minor version of a packed ABI version.
pub fn minor_of(version: u32) -> u16 {
    obc_module_abi_minor(version)
}

/// A function that clears one entry of a vtable.
type Clear = fn(&mut obc_module_vtable);

/// Every vtable entry appended after ABI 1.0: its name, the minor version
/// that defines it, and how to clear it.
pub const APPENDED_ENTRIES: [(&str, u16, Clear); 4] = [
    ("audio_instance_load", AUDIO_MINOR, |v| {
        v.audio_instance_load = None
    }),
    ("audio_instance_process", AUDIO_MINOR, |v| {
        v.audio_instance_process = None
    }),
    ("instance_status_values", STATUS_VALUES_MINOR, |v| {
        v.instance_status_values = None
    }),
    ("instance_set_background", LIVE_BACKGROUND_MINOR, |v| {
        v.instance_set_background = None
    }),
];

/// The entries no minor version of ABI 1 defines a call of yet: reserved,
/// never called, whatever the module sets.
pub const RESERVED_ENTRIES: [(&str, Clear); 2] = [
    ("instance_park", |v| v.instance_park = None),
    ("instance_unpark", |v| v.instance_unpark = None),
];

/// The vtable `vt` of a library built for minor version `minor`, less the
/// entries that version does not define (an entry appended after it, even
/// when the vtable's `struct_size` holds it) and less the reserved ones.
pub fn mask_vtable(mut vt: obc_module_vtable, minor: u16) -> obc_module_vtable {
    for (_, since, clear) in APPENDED_ENTRIES {
        if minor < since {
            clear(&mut vt);
        }
    }
    for (_, clear) in RESERVED_ENTRIES {
        clear(&mut vt);
    }
    vt
}

/// The `OBC_FRAME_FLAG_*` bits, with the minor version that defines each.
pub const FRAME_FLAGS: [(u32, u16); 2] = [
    (OBC_FRAME_FLAG_NEW_SOURCE, 0),
    (OBC_FRAME_FLAG_REPEAT, REPEAT_FLAG_MINOR),
];

/// The `OBC_AUDIO_FLAG_*` bits, with the minor version that defines each.
pub const AUDIO_FLAGS: [(u32, u16); 2] = [
    (OBC_AUDIO_FLAG_NEW_SOURCE, AUDIO_MINOR),
    (OBC_AUDIO_FLAG_DISCONT, AUDIO_MINOR),
];

fn bits(table: &[(u32, u16)], minor: u16) -> u32 {
    table
        .iter()
        .filter(|(_, since)| *since <= minor)
        .fold(0, |acc, (bit, _)| acc | bit)
}

/// The frame flag bits minor version `minor` defines.
pub fn frame_flags(minor: u16) -> u32 {
    bits(&FRAME_FLAGS, minor)
}

/// The audio flag bits minor version `minor` defines.
pub fn audio_flags(minor: u16) -> u32 {
    bits(&AUDIO_FLAGS, minor)
}

/// A constant the host passes a module, by what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constant {
    /// An `OBC_FRAME_*` kind.
    FrameKind(u32),
    /// An `OBC_FORMAT_*` pixel format.
    Format(u32),
    /// An `OBC_VALUE_*` kind.
    ValueKind(u32),
    /// An `OBC_MATRIX_*` matrix.
    Matrix(u32),
    /// An `OBC_CHROMA_SITING_*` siting.
    ChromaSiting(u32),
    /// An `OBC_AUDIO_*` sample format.
    AudioFormat(u32),
}

impl Constant {
    /// What it is, in words.
    pub fn what(self) -> &'static str {
        match self {
            Constant::FrameKind(_) => "frame kind",
            Constant::Format(_) => "pixel format",
            Constant::ValueKind(_) => "value kind",
            Constant::Matrix(_) => "colour matrix",
            Constant::ChromaSiting(_) => "chroma siting",
            Constant::AudioFormat(_) => "audio format",
        }
    }
}

/// Every constant the host may pass a module, with the minor version that
/// defines it.
pub const CONSTANTS: [(Constant, u16); 19] = [
    (Constant::FrameKind(OBC_FRAME_CUDA), 0),
    (Constant::FrameKind(OBC_FRAME_HOST), 0),
    (Constant::FrameKind(OBC_FRAME_DMABUF), 0),
    (Constant::Format(OBC_FORMAT_NV12), 0),
    (Constant::Format(OBC_FORMAT_I420), 0),
    (Constant::Format(OBC_FORMAT_BGRA), 0),
    (Constant::Format(OBC_FORMAT_RGBA), 0),
    (Constant::Format(OBC_FORMAT_RGB_F32_PLANAR), 0),
    (Constant::Format(OBC_FORMAT_BGR), 0),
    (Constant::ValueKind(OBC_VALUE_BOOL), 0),
    (Constant::ValueKind(OBC_VALUE_INT), 0),
    (Constant::ValueKind(OBC_VALUE_FLOAT), 0),
    (Constant::ValueKind(OBC_VALUE_STRING), 0),
    (Constant::Matrix(OBC_MATRIX_BT601), 0),
    (Constant::Matrix(OBC_MATRIX_BT709), 0),
    (Constant::Matrix(OBC_MATRIX_BT2020), 0),
    (Constant::ChromaSiting(OBC_CHROMA_SITING_MPEG2), 0),
    (Constant::ChromaSiting(OBC_CHROMA_SITING_JPEG), 0),
    (Constant::ChromaSiting(OBC_CHROMA_SITING_COSITED), 0),
];

/// The audio constants, apart: only an audio module (1.2 or later) is
/// handed one.
pub const AUDIO_CONSTANTS: [(Constant, u16); 1] =
    [(Constant::AudioFormat(OBC_AUDIO_F32_PLANAR), AUDIO_MINOR)];

/// Whether minor version `minor` defines `c`; a constant this host does
/// not know is defined by none.
pub fn defines(minor: u16, c: Constant) -> bool {
    CONSTANTS
        .iter()
        .chain(&AUDIO_CONSTANTS)
        .any(|(k, since)| *k == c && *since <= minor)
}

/// Why a module of minor version `minor` cannot be handed `c`, or `None`
/// when it can.
pub fn refusal(minor: u16, c: Constant) -> Option<String> {
    if defines(minor, c) {
        return None;
    }
    let value = match c {
        Constant::FrameKind(v)
        | Constant::Format(v)
        | Constant::ValueKind(v)
        | Constant::Matrix(v)
        | Constant::ChromaSiting(v)
        | Constant::AudioFormat(v) => v,
    };
    Some(format!(
        "the {} {value} is not defined by module ABI 1.{minor}, which the module is built for",
        c.what()
    ))
}

/// A structure the host fills in or sizes for a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStruct {
    /// `obc_frame`.
    Frame,
    /// `obc_chain_request`.
    ChainRequest,
    /// `obc_load_report` (the module writes it, in the host's
    /// `struct_size`).
    LoadReport,
    /// `obc_audio_buffer`.
    AudioBuffer,
    /// `obc_audio_request`.
    AudioRequest,
    /// `obc_audio_load_report`.
    AudioLoadReport,
}

/// The size of each structure the host hands a module, from the minor
/// version that gave it that size.
pub const STRUCT_SIZES: [(HostStruct, u16, u32); 7] = [
    (HostStruct::Frame, 0, OBC_FRAME_SIZE_1_0),
    (HostStruct::ChainRequest, 0, OBC_CHAIN_REQUEST_SIZE_1_0),
    (HostStruct::LoadReport, 0, OBC_LOAD_REPORT_SIZE_1_0),
    (
        HostStruct::LoadReport,
        LOAD_NOTICE_MINOR,
        OBC_LOAD_REPORT_SIZE_1_5,
    ),
    (
        HostStruct::AudioBuffer,
        AUDIO_MINOR,
        OBC_AUDIO_BUFFER_SIZE_1_2,
    ),
    (
        HostStruct::AudioRequest,
        AUDIO_MINOR,
        OBC_AUDIO_REQUEST_SIZE_1_2,
    ),
    (
        HostStruct::AudioLoadReport,
        AUDIO_MINOR,
        OBC_AUDIO_LOAD_REPORT_SIZE_1_2,
    ),
];

/// The `struct_size` the host sets in `s` for a module of minor version
/// `minor`: the size of the latest version not newer than `minor` (the
/// base size for a structure the version predates).
pub fn struct_size(s: HostStruct, minor: u16) -> u32 {
    let mut sizes = STRUCT_SIZES.iter().filter(|(t, _, _)| *t == s);
    let base = sizes.clone().map(|(_, _, n)| *n).next().unwrap_or(0);
    sizes
        .rfind(|(_, since, _)| *since <= minor)
        .map_or(base, |(_, _, n)| *n)
}

/// The settings types a module may describe, with the minor version that
/// defines each. (`action` was accepted before ABI 1.6 and reached no
/// module; it is no longer a module settings type.)
pub const SETTING_TYPES: [(&str, u16); 3] = [("bool", 0), ("int", 0), ("enum", 0)];

/// Whether minor version `minor` defines the settings type `name`: `None`
/// when no version this host knows does.
pub fn setting_type(minor: u16, name: &str) -> Option<bool> {
    SETTING_TYPES
        .iter()
        .find(|(t, _)| *t == name)
        .map(|(_, since)| *since <= minor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_module_abi::{
        OBC_MODULE_ABI_MINOR, OBC_MODULE_VTABLE_SIZE_1_0, OBC_MODULE_VTABLE_SIZE_1_2,
        OBC_MODULE_VTABLE_SIZE_1_3, OBC_MODULE_VTABLE_SIZE_1_4,
    };
    use std::mem::offset_of;

    const CURRENT: u16 = OBC_MODULE_ABI_MINOR as u16;

    /// A vtable with every entry set to a function of its type.
    fn full_vtable() -> obc_module_vtable {
        use obc_module_abi::*;
        unsafe extern "C" fn park(_: *mut obc_instance) -> obc_status {
            0
        }
        unsafe extern "C" fn audio_load(
            _: *mut obc_instance,
            _: *const obc_audio_request,
            _: *mut obc_audio_load_report,
        ) -> obc_status {
            0
        }
        unsafe extern "C" fn audio_process(
            _: *mut obc_instance,
            _: *const obc_audio_buffer,
            _: *mut obc_audio_buffer,
            _: *const obc_param,
            _: usize,
        ) -> obc_status {
            0
        }
        unsafe extern "C" fn values(
            _: *mut obc_instance,
            _: *mut *const obc_status_value,
            _: *mut usize,
        ) -> obc_status {
            0
        }
        unsafe extern "C" fn background(_: *mut obc_instance, _: *const obc_frame) -> obc_status {
            0
        }
        // SAFETY: all zero bytes are a valid vtable.
        let mut vt: obc_module_vtable = unsafe { std::mem::zeroed() };
        vt.instance_park = Some(park);
        vt.instance_unpark = Some(park);
        vt.audio_instance_load = Some(audio_load);
        vt.audio_instance_process = Some(audio_process);
        vt.instance_status_values = Some(values);
        vt.instance_set_background = Some(background);
        vt
    }

    fn set(vt: &obc_module_vtable) -> [bool; 4] {
        [
            vt.audio_instance_load.is_some(),
            vt.audio_instance_process.is_some(),
            vt.instance_status_values.is_some(),
            vt.instance_set_background.is_some(),
        ]
    }

    /// Every entry appended after 1.0 is masked by the declared minor
    /// version, whatever the vtable's size; the table covers every entry
    /// past the 1.0 vtable.
    #[test]
    fn every_entry_appended_after_1_0_is_masked_by_the_declared_minor() {
        let vt = full_vtable();
        let expect = |minor: u16| [minor >= 2, minor >= 2, minor >= 3, minor >= 4];
        for minor in 0..=CURRENT {
            assert_eq!(set(&mask_vtable(vt, minor)), expect(minor), "1.{minor}");
        }
        // The appended entries end the vtables of their minor versions.
        let entry = size_of::<usize>() as u32;
        let ends = [
            (
                offset_of!(obc_module_vtable, audio_instance_load) as u32,
                OBC_MODULE_VTABLE_SIZE_1_0,
            ),
            (
                offset_of!(obc_module_vtable, audio_instance_process) as u32 + entry,
                OBC_MODULE_VTABLE_SIZE_1_2,
            ),
            (
                offset_of!(obc_module_vtable, instance_status_values) as u32 + entry,
                OBC_MODULE_VTABLE_SIZE_1_3,
            ),
            (
                offset_of!(obc_module_vtable, instance_set_background) as u32 + entry,
                OBC_MODULE_VTABLE_SIZE_1_4,
            ),
        ];
        for (end, size) in ends {
            assert_eq!(end, size);
        }
        assert_eq!(
            size_of::<obc_module_vtable>() as u32,
            OBC_MODULE_VTABLE_SIZE_1_0 + entry * APPENDED_ENTRIES.len() as u32,
            "an entry appended to the vtable needs its line in APPENDED_ENTRIES"
        );
    }

    /// `instance_park` and `instance_unpark` are never kept, for any minor
    /// version.
    #[test]
    fn the_reserved_entries_are_never_kept() {
        for minor in 0..=CURRENT {
            let vt = mask_vtable(full_vtable(), minor);
            assert!(
                vt.instance_park.is_none() && vt.instance_unpark.is_none(),
                "1.{minor}"
            );
        }
    }

    #[test]
    fn flags_are_gated_by_the_declared_minor() {
        assert_eq!(frame_flags(0), OBC_FRAME_FLAG_NEW_SOURCE);
        for minor in 1..=CURRENT {
            assert_eq!(
                frame_flags(minor),
                obc_module_abi::OBC_FRAME_FLAGS_ALL,
                "1.{minor}"
            );
        }
        assert_eq!(audio_flags(0) | audio_flags(1), 0);
        for minor in 2..=CURRENT {
            assert_eq!(
                audio_flags(minor),
                obc_module_abi::OBC_AUDIO_FLAGS_ALL,
                "1.{minor}"
            );
        }
    }

    #[test]
    fn constants_are_gated_by_the_declared_minor() {
        for (c, _) in CONSTANTS {
            assert!(defines(0, c), "{c:?} is of 1.0");
            assert_eq!(refusal(0, c), None);
        }
        let audio = Constant::AudioFormat(OBC_AUDIO_F32_PLANAR);
        assert!(!defines(1, audio) && defines(2, audio));
        let e = refusal(1, audio).unwrap();
        assert!(e.contains("audio format 1") && e.contains("ABI 1.1"), "{e}");
        // A constant no version defines (a format of a later minor).
        let later = Constant::Format(99);
        assert!(!defines(CURRENT, later));
        assert!(refusal(CURRENT, later).unwrap().contains("pixel format 99"));
    }

    #[test]
    fn struct_sizes_are_gated_by_the_declared_minor() {
        for minor in 0..LOAD_NOTICE_MINOR {
            assert_eq!(
                struct_size(HostStruct::LoadReport, minor),
                OBC_LOAD_REPORT_SIZE_1_0,
                "1.{minor}"
            );
        }
        for minor in LOAD_NOTICE_MINOR..=CURRENT {
            assert_eq!(
                struct_size(HostStruct::LoadReport, minor),
                OBC_LOAD_REPORT_SIZE_1_5,
                "1.{minor}"
            );
        }
        for minor in 0..=CURRENT {
            assert_eq!(struct_size(HostStruct::Frame, minor), OBC_FRAME_SIZE_1_0);
            assert_eq!(
                struct_size(HostStruct::ChainRequest, minor),
                OBC_CHAIN_REQUEST_SIZE_1_0
            );
        }
        // The current sizes are the structures' own.
        use obc_module_abi::{
            obc_audio_buffer, obc_audio_load_report, obc_audio_request, obc_chain_request,
            obc_frame, obc_load_report,
        };
        let current = [
            (HostStruct::Frame, size_of::<obc_frame>()),
            (HostStruct::ChainRequest, size_of::<obc_chain_request>()),
            (HostStruct::LoadReport, size_of::<obc_load_report>()),
            (HostStruct::AudioBuffer, size_of::<obc_audio_buffer>()),
            (HostStruct::AudioRequest, size_of::<obc_audio_request>()),
            (
                HostStruct::AudioLoadReport,
                size_of::<obc_audio_load_report>(),
            ),
        ];
        for (s, n) in current {
            assert_eq!(struct_size(s, CURRENT) as usize, n, "{s:?}");
        }
    }

    #[test]
    fn reserved_names_are_enforced_from_1_6() {
        for minor in 0..RESERVED_NAMES_MINOR {
            assert!(!enforces_reserved_names(minor), "1.{minor}");
        }
        for minor in RESERVED_NAMES_MINOR..=CURRENT {
            assert!(enforces_reserved_names(minor), "1.{minor}");
        }
    }

    #[test]
    fn settings_types_are_gated_by_the_declared_minor() {
        for t in ["bool", "int", "enum"] {
            assert_eq!(setting_type(0, t), Some(true), "{t}");
        }
        assert_eq!(setting_type(CURRENT, "action"), None);
        assert_eq!(setting_type(CURRENT, "colour"), None);
    }
}
