//! Bench-only frame stamp written into the first luma bytes of a frame.
//!
//! The through-the-loopback latency measurement needs to recognise, in the
//! consumer, which camera frame it received. The loopback re-timestamps
//! buffers, so the stamp carries the original camera PTS inside the pixels:
//! 16 bytes = magic `OBC1` + PTS (u64 LE) + check word (u32 LE). Raw NV12/I420
//! pass through v4l2loopback byte-exact, so no error correction is needed.
//!
//! This alters the first 16 luma pixels of the top row, in bench mode only.

/// Number of luma bytes (pixels) the stamp overwrites.
pub const STAMP_LEN: usize = 16;
const MAGIC: [u8; 4] = *b"OBC1";

fn check(pts: u64) -> u32 {
    ((pts as u32) ^ ((pts >> 32) as u32)).rotate_left(7) ^ 0xA5A5_5A5A
}

/// Writes the stamp for `pts` at the start of `data`. Returns `false` when
/// the buffer is too small.
pub fn write_stamp(data: &mut [u8], pts: u64) -> bool {
    let Some(dst) = data.get_mut(..STAMP_LEN) else {
        return false;
    };
    dst[..4].copy_from_slice(&MAGIC);
    dst[4..12].copy_from_slice(&pts.to_le_bytes());
    dst[12..16].copy_from_slice(&check(pts).to_le_bytes());
    true
}

/// Reads a stamp from the start of `data`; `None` when absent or corrupt.
pub fn read_stamp(data: &[u8]) -> Option<u64> {
    let src = data.get(..STAMP_LEN)?;
    if src[..4] != MAGIC {
        return None;
    }
    let pts = u64::from_le_bytes(src[4..12].try_into().ok()?);
    let chk = u32::from_le_bytes(src[12..16].try_into().ok()?);
    (chk == check(pts)).then_some(pts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = vec![0x10u8; 64];
        for pts in [0u64, 1, 16_666_666, u64::MAX, 0x0123_4567_89ab_cdef] {
            assert!(write_stamp(&mut buf, pts));
            assert_eq!(read_stamp(&buf), Some(pts));
        }
        // Bytes after the stamp are untouched.
        assert!(buf[STAMP_LEN..].iter().all(|&b| b == 0x10));
    }

    #[test]
    fn rejects_unstamped_and_corrupt() {
        assert_eq!(read_stamp(&[0x10; 64]), None);
        assert_eq!(read_stamp(&[0u8; 4]), None);
        let mut buf = [0u8; 16];
        assert!(write_stamp(&mut buf, 42));
        buf[5] ^= 1;
        assert_eq!(read_stamp(&buf), None);
        assert!(!write_stamp(&mut [0u8; 8], 1));
    }
}
