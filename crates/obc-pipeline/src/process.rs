//! The Process stage: the effect chain applied to canonical frames, at the
//! head of the output pipeline (`docs/adr/0009-ingest-process-output.md`).
//!
//! Its input and output are canonical frames (NV12 in CUDA memory, at the
//! contract size, with the contract's colour label). It is empty by default.
//! The effects themselves are the `obceffect` element ([`crate::effect`],
//! inserted when `OutputConfig::effect` is set). [`GpuStage::Roundtrip`]
//! stands in for an effect that works on BGR: it converts to BGR and back
//! with `cudaconvert`, in CUDA memory, so its cost is the conversion cost
//! such an effect pays; it does not combine with the effect element.

use std::fmt;
use std::str::FromStr;

use serde::Serialize;

use crate::pipeline::{PipelineError, capsfilter, cuda_caps, make, names};

/// What the Process stage does to canonical frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuStage {
    /// Nothing: frames pass through untouched.
    #[default]
    Passthrough,
    /// NV12 -> BGR -> NV12 with `cudaconvert`, in CUDA memory: the
    /// conversion cost an effect stage working on BGR pays.
    Roundtrip,
}

impl FromStr for GpuStage {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "passthrough" => Ok(GpuStage::Passthrough),
            "roundtrip" => Ok(GpuStage::Roundtrip),
            _ => Err(format!("unknown GPU stage {s:?} (passthrough|roundtrip)")),
        }
    }
}

impl fmt::Display for GpuStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            GpuStage::Passthrough => "passthrough",
            GpuStage::Roundtrip => "roundtrip",
        })
    }
}

/// The elements of the Process stage, in link order, for canonical frames
/// labelled `colorimetry`. Empty for [`GpuStage::Passthrough`].
///
/// The roundtrip's NV12 capsfilter fixes the canonical label: without it the
/// BGR hop would relabel the frames (sRGB transfer), and the output would no
/// longer carry the contract's colorimetry.
pub fn process_elements(
    stage: GpuStage,
    colorimetry: &str,
) -> Result<Vec<gst::Element>, PipelineError> {
    match stage {
        GpuStage::Passthrough => Ok(Vec::new()),
        GpuStage::Roundtrip => Ok(vec![
            make("cudaconvert", names::TO_BGR)?,
            capsfilter(names::BGR_CAPS, &cuda_caps("BGR"))?,
            make("cudaconvert", names::FROM_BGR)?,
            capsfilter(
                names::PROCESS_CAPS,
                &format!("{},colorimetry={colorimetry}", cuda_caps("NV12")),
            )?,
        ]),
    }
}

#[cfg(test)]
mod tests {
    use gst::prelude::*;

    use super::*;
    use crate::test_support::require_elements;

    #[test]
    fn process_stage_parse() {
        assert_eq!(
            "roundtrip".parse::<GpuStage>().unwrap(),
            GpuStage::Roundtrip
        );
        assert_eq!(
            "Passthrough".parse::<GpuStage>().unwrap(),
            GpuStage::Passthrough
        );
        assert!("blur".parse::<GpuStage>().is_err());
        assert_eq!(GpuStage::Roundtrip.to_string(), "roundtrip");
    }

    #[test]
    fn passthrough_has_no_elements() {
        assert!(
            process_elements(GpuStage::Passthrough, "2:4:5:1")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    #[ignore = "needs an NVIDIA GPU with the GStreamer CUDA elements"]
    fn gpu_roundtrip_stage_elements() {
        require_elements(&["cudaconvert", "capsfilter"]);
        let e = process_elements(GpuStage::Roundtrip, "2:4:5:1").unwrap();
        let names: Vec<_> = e.iter().map(|e| e.name().to_string()).collect();
        assert_eq!(
            names,
            [
                names::TO_BGR,
                names::BGR_CAPS,
                names::FROM_BGR,
                names::PROCESS_CAPS
            ]
        );
        assert_eq!(
            e[3].property::<gst::Caps>("caps").to_string(),
            "video/x-raw(memory:CUDAMemory), format=(string)NV12, colorimetry=(string)2:4:5:1"
        );
    }
}
