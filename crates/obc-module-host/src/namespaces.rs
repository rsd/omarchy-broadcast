//! The namespaces of module ABI 1 (since 1.6), and the host's cheap checks
//! of them:
//!
//! * **Stage ids** are one namespace across modules. The well-known ids
//!   ([`OBC_WELL_KNOWN_STAGES`]) and every other id without the private
//!   prefix are the registry's; a stage of a module's own has the vendor
//!   form `x-<vendor>-<name>` (`mic-x-<vendor>-<name>` for an audio stage).
//!   A module that declares an id of neither form is loaded, with a
//!   warning in its log ([`stage_id_warning`]).
//! * **Settings groups** likewise: the well-known groups of the module's
//!   kind ([`OBC_WELL_KNOWN_VIDEO_GROUPS`], [`OBC_WELL_KNOWN_AUDIO_GROUPS`])
//!   or the vendor form `x_<vendor>_<name>`; another id keeps its group,
//!   with a warning ([`group_id_warning`]).
//! * **Configuration keys** of `[module."<id>"]` that start with `_` are the
//!   host's: a key of that form the host does not define ([`HOST_KEYS`]) is
//!   not passed to the module, with a warning ([`reserved_config_keys`]).
//! * **Status value names** that start with `obc.` are the host's: a module's
//!   value of such a name is dropped ([`is_reserved_value_name`]).
//!
//! The last two hold a module of ABI 1.6 or later only
//! ([`crate::gating::RESERVED_NAMES_MINOR`]): an older module is passed such
//! a key and keeps such a value, as a host of 1.5 did, with a warning.

use obc_module_abi::{
    OBC_HOST_CONFIG_KEY, OBC_PRIVATE_GROUP_PREFIX, OBC_PRIVATE_STAGE_PREFIX,
    OBC_RESERVED_CONFIG_PREFIX, OBC_RESERVED_VALUE_PREFIX, OBC_WELL_KNOWN_AUDIO_GROUPS,
    OBC_WELL_KNOWN_STAGES, OBC_WELL_KNOWN_VIDEO_GROUPS,
};

use crate::manifest::{AUDIO_STAGE_PREFIX, ModuleKind};

/// The keys starting with `_` that the host defines in a module's
/// configuration.
pub const HOST_KEYS: [&str; 1] = [OBC_HOST_CONFIG_KEY];

/// Whether `rest` is `<vendor><sep><name>`, both non-empty.
fn vendor_form(rest: &str, sep: char) -> bool {
    rest.split_once(sep)
        .is_some_and(|(vendor, name)| !vendor.is_empty() && !name.is_empty())
}

/// Whether stage id `id` is of the private vendor form: `x-<vendor>-<name>`,
/// or `mic-x-<vendor>-<name>` for an audio stage.
pub fn is_private_stage(id: &str) -> bool {
    let id = id.strip_prefix(AUDIO_STAGE_PREFIX).unwrap_or(id);
    id.strip_prefix(OBC_PRIVATE_STAGE_PREFIX)
        .is_some_and(|rest| vendor_form(rest, '-'))
}

/// Whether `id` is a well-known stage id of this ABI version.
pub fn is_well_known_stage(id: &str) -> bool {
    OBC_WELL_KNOWN_STAGES.contains(&id)
}

/// The warning about stage id `id`, or `None` when it is well known or of
/// the vendor form.
pub fn stage_id_warning(id: &str) -> Option<String> {
    if is_well_known_stage(id) || is_private_stage(id) {
        return None;
    }
    let audio = id.starts_with(AUDIO_STAGE_PREFIX);
    Some(format!(
        "the stage id {id:?} is not a well-known stage id; a stage of the module's own should \
         have the vendor form {}{OBC_PRIVATE_STAGE_PREFIX}<vendor>-<name>, or a later release \
         may give the id another meaning",
        if audio { AUDIO_STAGE_PREFIX } else { "" }
    ))
}

/// Whether settings group id `id` is of the private vendor form,
/// `x_<vendor>_<name>`.
pub fn is_private_group(id: &str) -> bool {
    id.strip_prefix(OBC_PRIVATE_GROUP_PREFIX)
        .is_some_and(|rest| vendor_form(rest, '_'))
}

/// Whether `id` is a well-known settings group of a module of `kind`.
pub fn is_well_known_group(id: &str, kind: ModuleKind) -> bool {
    if kind.is_audio() {
        OBC_WELL_KNOWN_AUDIO_GROUPS.contains(&id)
    } else {
        OBC_WELL_KNOWN_VIDEO_GROUPS.contains(&id)
    }
}

/// The warning about the settings group `id` of a module of `kind`, or
/// `None` when it is well known or of the vendor form.
pub fn group_id_warning(id: &str, kind: ModuleKind) -> Option<String> {
    if is_well_known_group(id, kind) || is_private_group(id) {
        return None;
    }
    Some(format!(
        "the settings group {id:?} is not a well-known group; a group of the module's own should \
         have the vendor form {OBC_PRIVATE_GROUP_PREFIX}<vendor>_<name>, or a later release may \
         give the id another meaning"
    ))
}

/// The keys of the configuration object `config` that start with `_` and
/// that the host does not define: reserved, never passed to a module.
pub fn reserved_config_keys(config: &serde_json::Value) -> Vec<String> {
    config.as_object().map_or_else(Vec::new, |o| {
        o.keys()
            .filter(|k| {
                k.starts_with(OBC_RESERVED_CONFIG_PREFIX) && !HOST_KEYS.contains(&k.as_str())
            })
            .cloned()
            .collect()
    })
}

/// Whether a status value of name `name` is the host's: one that starts
/// with `obc.`.
pub fn is_reserved_value_name(name: &str) -> bool {
    name.starts_with(OBC_RESERVED_VALUE_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_backend::StageId;

    #[test]
    fn stage_ids_are_well_known_private_or_warned_about() {
        for id in OBC_WELL_KNOWN_STAGES {
            assert!(StageId::is_valid(id), "{id}");
            assert_eq!(stage_id_warning(id), None, "{id}");
        }
        for id in ["x-acme-glow", "mic-x-acme-gate", "x-a-b"] {
            assert!(is_private_stage(id), "{id}");
            assert_eq!(stage_id_warning(id), None, "{id}");
        }
        for id in [
            "glow",
            "x-glow",
            "x--glow",
            "x-acme-",
            "mic-gate",
            "mic-x-gate",
        ] {
            assert!(!is_private_stage(id) || is_well_known_stage(id), "{id}");
            let w = stage_id_warning(id).unwrap();
            assert!(w.contains("not a well-known stage id"), "{w}");
        }
        assert!(
            stage_id_warning("mic-gate")
                .unwrap()
                .contains("mic-x-<vendor>-<name>")
        );
        // The longest vendor form fits a stage id.
        assert!(StageId::is_valid(&format!(
            "x-{}-{}",
            "v".repeat(14),
            "n".repeat(14)
        )));
    }

    #[test]
    fn groups_are_well_known_by_kind_private_or_warned_about() {
        for id in OBC_WELL_KNOWN_VIDEO_GROUPS {
            assert_eq!(group_id_warning(id, ModuleKind::VideoStage), None, "{id}");
        }
        for id in OBC_WELL_KNOWN_AUDIO_GROUPS {
            assert_eq!(group_id_warning(id, ModuleKind::AudioFilter), None, "{id}");
        }
        assert!(group_id_warning("noise", ModuleKind::VideoStage).is_some());
        assert!(group_id_warning("lighting", ModuleKind::AudioFilter).is_some());
        for kind in [ModuleKind::VideoStage, ModuleKind::AudioFilter] {
            assert_eq!(group_id_warning("x_acme_glow", kind), None);
            for id in ["hello", "x_glow", "x__glow", "x_acme_"] {
                assert!(group_id_warning(id, kind).is_some(), "{id}");
            }
        }
        // The vendor form is a valid settings id segment.
        assert!(crate::settings::valid_setting_id("effect.x_acme_glow.on"));
    }

    #[test]
    fn config_keys_starting_with_an_underscore_are_reserved() {
        let config = serde_json::json!({"_host": {}, "_debug": 1, "_": 2, "sdk_root": "/x"});
        let mut reserved = reserved_config_keys(&config);
        reserved.sort();
        assert_eq!(reserved, ["_", "_debug"]);
        assert!(reserved_config_keys(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn value_names_starting_with_obc_are_reserved() {
        assert!(is_reserved_value_name("obc.zoom"));
        assert!(!is_reserved_value_name("zoom"));
        assert!(!is_reserved_value_name("obcx"));
    }
}
