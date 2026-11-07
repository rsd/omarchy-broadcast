//! The latest-frame slot between an ingest pipeline and the output clock.
//!
//! The slot holds one entry: the newest frame, its sequence number and its
//! arrival time. Ingest replaces it on every frame; the output clock reads it
//! once per tick. Only references move: for GStreamer buffers a clone is a
//! reference-count increment, so neither side copies frame data, and neither
//! side waits for the other beyond a short mutex section that makes no
//! GStreamer call.
//!
//! Sequence numbers start at 1 and are shared by every source writing into
//! the slot, so a frame from a new source is always newer than the last one
//! emitted. Frames replaced before the output used them are not tracked
//! here: the pacer derives them from the gap in sequence numbers
//! ([`crate::clock::Pacer`]).
//!
//! Times are nanoseconds since an epoch chosen by the owner (the output's
//! start), so the logic is testable without a clock.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One frame in the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<T> {
    pub frame: T,
    /// 1 for the first frame ever written, then +1 per frame.
    pub seq: u64,
    /// When the frame was written, in nanoseconds since the owner's epoch.
    pub arrived_ns: u64,
}

#[derive(Debug)]
struct Inner<T> {
    latest: Option<Entry<T>>,
    next_seq: u64,
}

/// A single-entry, latest-wins frame slot. See the module docs.
#[derive(Debug)]
pub struct Slot<T> {
    inner: Mutex<Inner<T>>,
    written: AtomicU64,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            inner: Mutex::new(Inner {
                latest: None,
                next_seq: 1,
            }),
            written: AtomicU64::new(0),
        }
    }
}

impl<T: Clone> Slot<T> {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        // No code path panics while holding the lock; recover regardless.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces the slot's frame and returns the new frame's sequence
    /// number. The replaced frame is dropped after the lock is released, so
    /// releasing it (a buffer going back to its pool) never runs under it.
    pub fn put(&self, frame: T, arrived_ns: u64) -> u64 {
        let (seq, old) = {
            let mut g = self.lock();
            let seq = g.next_seq;
            g.next_seq += 1;
            let old = g.latest.replace(Entry {
                frame,
                seq,
                arrived_ns,
            });
            (seq, old)
        };
        self.written.fetch_add(1, Ordering::Relaxed);
        drop(old);
        seq
    }

    /// A copy of the newest entry (for buffers: one more reference).
    pub fn latest(&self) -> Option<Entry<T>> {
        self.lock().latest.clone()
    }

    /// Empties the slot, releasing the frame it holds. Sequence numbers keep
    /// counting from where they were.
    pub fn clear(&self) {
        let old = self.lock().latest.take();
        drop(old);
    }

    /// Sequence number of the last frame written (0: none yet), whether or
    /// not the slot still holds it.
    pub fn last_seq(&self) -> u64 {
        self.lock().next_seq - 1
    }

    /// Frames written since the slot was created.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn latest_wins_and_sequence_counts_every_write() {
        let slot = Slot::new();
        assert_eq!(slot.latest(), None);
        assert_eq!(slot.last_seq(), 0);
        assert_eq!(slot.put("a", 10), 1);
        assert_eq!(slot.put("b", 20), 2);
        assert_eq!(
            slot.latest(),
            Some(Entry {
                frame: "b",
                seq: 2,
                arrived_ns: 20
            })
        );
        assert_eq!(slot.written(), 2);
        slot.clear();
        assert_eq!(slot.latest(), None);
        assert_eq!(slot.last_seq(), 2, "the last sequence survives a clear");
        assert_eq!(slot.put("c", 30), 3, "sequence survives a clear");
    }

    #[test]
    fn replaced_frames_are_released() {
        let slot = Slot::new();
        let first = Arc::new(1);
        slot.put(Arc::clone(&first), 0);
        assert_eq!(Arc::strong_count(&first), 2);
        slot.put(Arc::new(2), 1);
        assert_eq!(Arc::strong_count(&first), 1, "old frame dropped");
        let held = slot.latest().unwrap();
        assert_eq!(Arc::strong_count(&held.frame), 2, "a read is a reference");
    }

    #[test]
    fn concurrent_writers_get_distinct_sequences() {
        let slot = Arc::new(Slot::new());
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let s = Arc::clone(&slot);
                std::thread::spawn(move || (0..1000).map(|i| s.put(t, i)).collect::<Vec<_>>())
            })
            .collect();
        let mut seqs: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 4000);
        assert_eq!(slot.written(), 4000);
        assert_eq!(slot.latest().unwrap().seq, 4000);
    }
}
