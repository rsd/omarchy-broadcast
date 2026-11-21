//! Canonical frames and the handle an ingest pipeline writes them with.
//!
//! The **canonical frame** is what every ingest pipeline produces and the
//! output pipeline consumes: NV12 at the processing size (the contract
//! size), square pixels, with the contract's colour label
//!. Its frame rate is whatever
//! the source delivers; the output clock sets the rate.
//!
//! **Memory.** Canonical frames live in the memory the output was built
//! for ([`VideoMemory`], [`crate::output::Output::build_in`]): CUDA memory
//! (the default, [`crate::output::Output::build`]) or system memory, where
//! the whole stream runs on the CPU and makes no CUDA call. Every pipeline
//! of a session uses the same memory: an ingest takes it from the
//! [`FrameWriter`] the output hands out.
//!
//! **Fan-out.** A writer writes into one output's slot, and, while a
//! session prepares its next output ([`crate::session::Session::prepare_output`]),
//! into that output's slot too: the same frame in both, by reference (the
//! buffer is shared, never copied), with the writer's one source id, each
//! slot with its own sequence number and arrival time (ns since its own
//! output's epoch). The session then makes the prepared output's slot the
//! only one ([`crate::session::Session::commit_prepared`]) or drops it
//! ([`crate::session::Session::discard_prepared`]). Every clone of a writer
//! (the ingest's sink holds one) follows these changes; a frame being
//! written while one happens lands in the slots of before or of after.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use obc_core::config::VideoMemory;
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

    /// The caps of canonical frames in `memory`, without a frame rate:
    /// [`Self::caps_string`] or [`Self::system_caps_string`].
    pub fn caps_in(&self, memory: VideoMemory) -> String {
        match memory {
            VideoMemory::Cuda => self.caps_string(),
            VideoMemory::System => self.system_caps_string(),
        }
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

/// A canonical frame in the slot, with where it came from.
#[derive(Debug, Clone)]
pub struct Frame {
    /// NV12 in the output's memory (CUDA or system). Cloning it takes a
    /// reference, never a copy.
    pub buffer: gst::Buffer,
    /// The source that produced it ([`FrameWriter::source_id`]).
    pub source_id: u64,
    /// Its timestamp in the ingest pipeline, in nanoseconds.
    pub source_pts_ns: Option<u64>,
}

/// One slot a writer writes into, with the epoch of its arrival times.
#[derive(Debug, Clone)]
pub(crate) struct SlotTarget {
    pub(crate) slot: Arc<Slot<Frame>>,
    pub(crate) epoch: Instant,
}

impl SlotTarget {
    fn put(&self, frame: Frame) -> u64 {
        let arrived = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.slot.put(frame, arrived)
    }
}

/// Where a writer writes (see the module docs, "Fan-out").
#[derive(Debug)]
struct Targets {
    /// The current output's slot.
    primary: SlotTarget,
    /// The prepared output's slot, written too.
    also: Option<SlotTarget>,
}

/// Writes one source's canonical frames into the output's slot (and into
/// a prepared output's too, see the module docs, "Fan-out"). Handed out by
/// [`crate::output::Output::frame_writer`], one per ingest pipeline.
#[derive(Debug, Clone)]
pub struct FrameWriter {
    /// Shared by every clone, so a retarget reaches the ingest's sink.
    targets: Arc<Mutex<Targets>>,
    source_id: u64,
    canonical: Canonical,
    memory: VideoMemory,
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
            targets: Arc::new(Mutex::new(Targets {
                primary: SlotTarget { slot, epoch },
                also: None,
            })),
            source_id,
            canonical,
            memory: VideoMemory::Cuda,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Targets> {
        // No code path panics while holding the lock; recover regardless.
        self.targets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes every frame into `also` as well (`None`: no longer), from the
    /// next frame on (see the module docs, "Fan-out").
    pub(crate) fn fan_out(&self, also: Option<SlotTarget>) {
        let old = std::mem::replace(&mut self.lock().also, also);
        // Its slot (and the frame in it) is released outside the lock.
        drop(old);
    }

    /// Writes every frame into `primary` only, from the next frame on: a
    /// prepared output's slot becomes the writer's own (see the module
    /// docs, "Fan-out").
    pub(crate) fn retarget(&self, primary: SlotTarget) {
        let old = {
            let mut t = self.lock();
            (std::mem::replace(&mut t.primary, primary), t.also.take())
        };
        drop(old);
    }

    /// The same writer for frames in `memory`.
    pub(crate) fn in_memory(self, memory: VideoMemory) -> Self {
        FrameWriter { memory, ..self }
    }

    /// Where the frames written must live: the memory of the output that
    /// handed the writer out ([`crate::output::Output::build_in`]).
    pub fn memory(&self) -> VideoMemory {
        self.memory
    }

    pub fn source_id(&self) -> u64 {
        self.source_id
    }

    /// The format the frames written must have.
    pub fn canonical(&self) -> Canonical {
        self.canonical
    }

    /// Replaces the slot's frame with `buffer` (and a prepared output's,
    /// with the same buffer, see the module docs, "Fan-out"); returns its
    /// sequence number in the current output's slot. O(1): short locks
    /// (the slots are taken under the writer's own, then written after
    /// it), no GStreamer call, no copy.
    pub fn put(&self, buffer: gst::Buffer) -> u64 {
        let source_pts_ns = buffer.pts().map(|p| p.nseconds());
        let (primary, also) = {
            let t = self.lock();
            (t.primary.clone(), t.also.clone())
        };
        let frame = Frame {
            buffer,
            source_id: self.source_id,
            source_pts_ns,
        };
        if let Some(also) = also {
            // A reference: the buffer is shared, never copied.
            also.put(frame.clone());
        }
        primary.put(frame)
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

    /// A fanned-out writer (see the module docs, "Fan-out") puts each frame
    /// into both slots, the same buffer (no copy), its one source id, each
    /// slot with its own sequence number; a clone follows the change. A
    /// retarget writes into the new slot only; a fan-out ended writes into
    /// the first slot only.
    #[test]
    fn a_fanned_out_writer_shares_each_frame_then_retargets() {
        gst::init().unwrap();
        let canonical = Canonical::of(&OutputContract::default());
        let (a, b) = (Arc::new(Slot::new()), Arc::new(Slot::new()));
        let w = FrameWriter::new(Arc::clone(&a), Instant::now(), 3, canonical);
        let sink_side = w.clone();
        let buffer = || gst::Buffer::with_size(16).unwrap();
        sink_side.put(buffer());
        w.fan_out(Some(SlotTarget {
            slot: Arc::clone(&b),
            epoch: Instant::now(),
        }));
        let shared = buffer();
        assert_eq!(
            sink_side.put(shared.clone()),
            2,
            "the first slot's sequence"
        );
        let (ea, eb) = (a.latest().unwrap(), b.latest().unwrap());
        assert_eq!((ea.seq, eb.seq), (2, 1));
        assert_eq!((ea.frame.source_id, eb.frame.source_id), (3, 3));
        assert_eq!(ea.frame.buffer.as_ptr(), shared.as_ptr(), "a copy");
        assert_eq!(eb.frame.buffer.as_ptr(), shared.as_ptr(), "a copy");

        w.fan_out(None);
        sink_side.put(buffer());
        assert_eq!((a.written(), b.written()), (3, 1));
        w.fan_out(Some(SlotTarget {
            slot: Arc::clone(&b),
            epoch: Instant::now(),
        }));
        w.retarget(SlotTarget {
            slot: Arc::clone(&b),
            epoch: Instant::now(),
        });
        assert_eq!(sink_side.put(buffer()), 2, "the new slot's sequence");
        assert_eq!((a.written(), b.written()), (3, 2), "written twice");
    }
}
