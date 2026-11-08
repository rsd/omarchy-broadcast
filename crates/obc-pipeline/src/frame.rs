//! Canonical frames and the handle an ingest pipeline writes them with.
//!
//! The **canonical frame** is what every ingest pipeline produces and the
//! output pipeline consumes: NV12 in CUDA memory, at the processing size
//! (the contract size), square pixels, with the contract's colour label
//! (`docs/adr/0009-ingest-process-output.md`). Its frame rate is whatever
//! the source delivers; the output clock sets the rate.

use std::sync::Arc;
use std::time::Instant;

use obc_core::{ColourRange, OutputContract};

use crate::pipeline::CUDA_MEMORY_FEATURE;
use crate::slot::Slot;

/// The canonical frame format for an output contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Canonical {
    pub width: u32,
    pub height: u32,
    pub range: ColourRange,
    /// GStreamer colorimetry label ([`OutputContract::colorimetry`]).
    pub colorimetry: &'static str,
}

impl Canonical {
    pub fn of(contract: &OutputContract) -> Self {
        Canonical {
            width: contract.width,
            height: contract.height,
            range: contract.range,
            colorimetry: contract.colorimetry(),
        }
    }

    /// The caps of canonical frames in CUDA memory, without a frame rate.
    pub fn caps_string(&self) -> String {
        format!(
            "video/x-raw({CUDA_MEMORY_FEATURE}),format=NV12,width={},height={},\
             pixel-aspect-ratio=1/1,colorimetry={}",
            self.width, self.height, self.colorimetry
        )
    }

    /// The same caps in system memory.
    pub fn system_caps_string(&self) -> String {
        format!(
            "video/x-raw,format=NV12,width={},height={},pixel-aspect-ratio=1/1,colorimetry={}",
            self.width, self.height, self.colorimetry
        )
    }

    /// Black in this format: `(Y, chroma)`. Limited range puts black at
    /// Y=16, full range at Y=0; neutral chroma is 128 in both.
    pub fn black(&self) -> (u8, u8) {
        match self.range {
            ColourRange::Limited => (16, 128),
            ColourRange::Full => (0, 128),
        }
    }
}

/// Where canonical frames live. Production frames are always in CUDA
/// memory; system memory runs the same ingest, clock and output code on the
/// CPU (the CPU-only session tests), with CPU stand-ins for the CUDA
/// elements and no CUDA context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FrameMemory {
    #[default]
    Cuda,
    #[cfg_attr(not(test), allow(dead_code))]
    System,
}

impl FrameMemory {
    /// The caps of canonical frames in this memory, without a frame rate.
    pub(crate) fn caps_string(self, canonical: &Canonical) -> String {
        match self {
            FrameMemory::Cuda => canonical.caps_string(),
            FrameMemory::System => canonical.system_caps_string(),
        }
    }
}

/// A canonical frame in the slot, with where it came from.
#[derive(Debug, Clone)]
pub struct Frame {
    /// NV12 in CUDA memory. Cloning it takes a reference, never a copy.
    pub buffer: gst::Buffer,
    /// The source that produced it ([`FrameWriter::source_id`]).
    pub source_id: u64,
    /// Its timestamp in the ingest pipeline, in nanoseconds.
    pub source_pts_ns: Option<u64>,
}

/// Writes one source's canonical frames into the output's slot. Handed out
/// by [`crate::output::Output::frame_writer`], one per ingest pipeline.
#[derive(Debug, Clone)]
pub struct FrameWriter {
    slot: Arc<Slot<Frame>>,
    epoch: Instant,
    source_id: u64,
    canonical: Canonical,
    memory: FrameMemory,
}

impl FrameWriter {
    /// A writer for source `source_id` into `slot`, with arrival times in
    /// nanoseconds since `epoch`.
    pub fn new(
        slot: Arc<Slot<Frame>>,
        epoch: Instant,
        source_id: u64,
        canonical: Canonical,
    ) -> Self {
        FrameWriter {
            slot,
            epoch,
            source_id,
            canonical,
            memory: FrameMemory::Cuda,
        }
    }

    /// The same writer for frames in `memory`.
    pub(crate) fn in_memory(self, memory: FrameMemory) -> Self {
        FrameWriter { memory, ..self }
    }

    /// Where the frames written must live.
    pub(crate) fn memory(&self) -> FrameMemory {
        self.memory
    }

    pub fn source_id(&self) -> u64 {
        self.source_id
    }

    /// The format the frames written must have.
    pub fn canonical(&self) -> Canonical {
        self.canonical
    }

    /// Replaces the slot's frame with `buffer`. O(1): a short lock, no
    /// GStreamer call, no copy.
    pub fn put(&self, buffer: gst::Buffer) -> u64 {
        let source_pts_ns = buffer.pts().map(|p| p.nseconds());
        let arrived = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.slot.put(
            Frame {
                buffer,
                source_id: self.source_id,
                source_pts_ns,
            },
            arrived,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_caps_follow_the_contract() {
        let c = Canonical::of(&OutputContract::default());
        assert_eq!(
            c.caps_string(),
            "video/x-raw(memory:CUDAMemory),format=NV12,width=1920,height=1080,\
             pixel-aspect-ratio=1/1,colorimetry=2:4:5:1"
        );
        assert_eq!(c.black(), (16, 128));
        let full = Canonical::of(&OutputContract {
            range: ColourRange::Full,
            width: 1280,
            height: 720,
            ..OutputContract::default()
        });
        assert_eq!(
            full.system_caps_string(),
            "video/x-raw,format=NV12,width=1280,height=720,pixel-aspect-ratio=1/1,\
             colorimetry=1:4:5:1"
        );
        assert_eq!(full.black(), (0, 128));
    }

    #[test]
    fn writer_tags_frames_with_source_and_pts() {
        gst::init().unwrap();
        let slot = Arc::new(Slot::new());
        let w = FrameWriter::new(
            Arc::clone(&slot),
            Instant::now(),
            7,
            Canonical::of(&OutputContract::default()),
        );
        let mut b = gst::Buffer::new();
        b.get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_nseconds(42));
        assert_eq!(w.put(b), 1);
        let e = slot.latest().unwrap();
        assert_eq!(e.frame.source_id, 7);
        assert_eq!(e.frame.source_pts_ns, Some(42));
        assert_eq!(slot.written(), 1);
    }
}
