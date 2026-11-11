//! The handshake messages: hello (plugin to worker) and the reply (accept
//! or refuse). Little-endian, fixed offsets; see the crate documentation
//! for the layout table.

use std::fmt;

use crate::{BLOCK_SAMPLES, CHANNELS, PROTOCOL_VERSION, SAMPLE_RATE, shm};

/// `"OBCh"` read as a little-endian `u32`: the first word of a hello.
pub const HELLO_MAGIC: u32 = u32::from_le_bytes(*b"OBCh");
/// `"OBCr"` read as a little-endian `u32`: the first word of a reply.
pub const REPLY_MAGIC: u32 = u32::from_le_bytes(*b"OBCr");
/// The length of a hello of this version.
pub const HELLO_LEN: usize = 32;
/// The fixed part of a reply; a refusal's text follows it.
pub const REPLY_HEADER_LEN: usize = 24;
/// The longest refusal text, in bytes (longer text is cut at a character
/// boundary when encoding).
pub const MAX_REASON_LEN: usize = 232;
/// The largest message either side sends, and the receive buffer size.
pub const MAX_MESSAGE_LEN: usize = REPLY_HEADER_LEN + MAX_REASON_LEN;

const KIND_ACCEPT: u32 = 1;
const KIND_REFUSE: u32 = 2;

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn dword(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

/// The plugin's hello. It travels with two descriptors, in this order: the
/// memfd holding the shared region and the worker's end of the "input
/// available" wake socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// The protocol version the plugin speaks.
    pub version: u32,
    /// Always [`SAMPLE_RATE`] in this version.
    pub sample_rate: u32,
    /// Always [`CHANNELS`] (mono) in this version.
    pub channels: u32,
    /// Always [`BLOCK_SAMPLES`] in this version.
    pub block_samples: u32,
    /// Blocks per ring (both rings have the same capacity); a power of two
    /// within [`crate::MIN_RING_CAPACITY`]..=[`crate::MAX_RING_CAPACITY`].
    pub ring_capacity: u32,
    /// The size of the memfd in bytes, [`shm::region_size`] of the capacity.
    pub shm_size: u64,
}

impl Hello {
    /// The hello of this protocol version for a ring of `ring_capacity` blocks.
    pub fn new(ring_capacity: u32) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            block_samples: BLOCK_SAMPLES as u32,
            ring_capacity,
            shm_size: shm::region_size(ring_capacity) as u64,
        }
    }

    /// The wire form.
    pub fn encode(&self) -> [u8; HELLO_LEN] {
        let mut b = [0u8; HELLO_LEN];
        b[0..4].copy_from_slice(&HELLO_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.version.to_le_bytes());
        b[8..12].copy_from_slice(&self.sample_rate.to_le_bytes());
        b[12..16].copy_from_slice(&self.channels.to_le_bytes());
        b[16..20].copy_from_slice(&self.block_samples.to_le_bytes());
        b[20..24].copy_from_slice(&self.ring_capacity.to_le_bytes());
        b[24..32].copy_from_slice(&self.shm_size.to_le_bytes());
        b
    }

    /// Parses a hello. Only the magic and the version are read from a
    /// hello of another version (their offsets never change); the rest of
    /// its layout is that version's business.
    pub fn decode(bytes: &[u8]) -> Result<Self, HelloError> {
        if bytes.len() < 8 || word(bytes, 0) != HELLO_MAGIC {
            return Err(HelloError::Malformed("no hello magic"));
        }
        let version = word(bytes, 4);
        if version != PROTOCOL_VERSION {
            return Err(HelloError::Version(version));
        }
        if bytes.len() != HELLO_LEN {
            return Err(HelloError::Malformed("wrong hello length"));
        }
        Ok(Self {
            version,
            sample_rate: word(bytes, 8),
            channels: word(bytes, 12),
            block_samples: word(bytes, 16),
            ring_capacity: word(bytes, 20),
            shm_size: dword(bytes, 24),
        })
    }

    /// Checks the format fields against what this version supports.
    pub fn check_format(&self) -> Result<(), &'static str> {
        if self.sample_rate != SAMPLE_RATE {
            return Err("unsupported sample rate");
        }
        if self.channels != CHANNELS {
            return Err("unsupported channel count");
        }
        if self.block_samples != BLOCK_SAMPLES as u32 {
            return Err("unsupported block size");
        }
        if !shm::capacity_ok(self.ring_capacity) {
            return Err("ring capacity out of range or not a power of two");
        }
        if self.shm_size != shm::region_size(self.ring_capacity) as u64 {
            return Err("shared memory size does not match the ring capacity");
        }
        Ok(())
    }
}

/// Why a hello could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloError {
    /// Not a hello, or a hello of this version with the wrong length.
    Malformed(&'static str),
    /// A well-formed start of a hello of another protocol version.
    Version(u32),
}

/// Why the worker refused a client. Codes are stable across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The client speaks another protocol version.
    Version,
    /// The hello could not be parsed, or the descriptors were missing.
    Malformed,
    /// Sample rate, channels, block size or ring capacity not supported.
    Format,
    /// The memfd is too small, unsealed or cannot be mapped, or its header
    /// does not match the hello.
    SharedMemory,
    /// The worker already serves a client.
    Busy,
    /// Any other worker-side failure.
    Internal,
    /// A code this version does not know.
    Other(u32),
}

impl RefuseReason {
    /// The wire code.
    pub fn code(self) -> u32 {
        match self {
            Self::Version => 1,
            Self::Malformed => 2,
            Self::Format => 3,
            Self::SharedMemory => 4,
            Self::Busy => 5,
            Self::Internal => 6,
            Self::Other(c) => c,
        }
    }

    /// The reason for a wire code.
    pub fn from_code(code: u32) -> Self {
        match code {
            1 => Self::Version,
            2 => Self::Malformed,
            3 => Self::Format,
            4 => Self::SharedMemory,
            5 => Self::Busy,
            6 => Self::Internal,
            c => Self::Other(c),
        }
    }
}

/// The worker took the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accept {
    /// The worker's protocol version (equal to the client's).
    pub version: u32,
    /// How many blocks of processing time the worker asks for, 1 to
    /// [`crate::MAX_PIPELINE_DEPTH`]; the client sizes its playout delay
    /// from it ([`crate::transport_delay`]).
    pub pipeline_depth: u32,
    /// The algorithmic delay of the processing, in samples, at accept time;
    /// later changes go through the header's `latency_samples`.
    pub latency_samples: u32,
}

/// The worker turned the client away; the client stays in passthrough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The worker's protocol version.
    pub version: u32,
    /// The machine-readable reason.
    pub reason: RefuseReason,
    /// A short human-readable explanation, for logs.
    pub text: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refused ({:?}, worker protocol {}): {}",
            self.reason, self.version, self.text
        )
    }
}

/// The worker's answer to a hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Connected; the rings are live.
    Accept(Accept),
    /// Not connected.
    Refuse(Refusal),
}

/// Why a reply could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    /// Not a reply, or an unknown kind.
    Malformed(&'static str),
    /// A reply from a worker of another protocol version.
    Version(u32),
}

impl Reply {
    /// The wire form of an accept.
    pub fn encode_accept(pipeline_depth: u32, latency_samples: u32) -> Vec<u8> {
        let mut b = Self::header(KIND_ACCEPT, pipeline_depth, latency_samples, 0);
        b.truncate(REPLY_HEADER_LEN);
        b
    }

    /// The wire form of a refusal; `text` is cut to [`MAX_REASON_LEN`]
    /// bytes at a character boundary.
    pub fn encode_refuse(reason: RefuseReason, text: &str) -> Vec<u8> {
        let mut cut = text.len().min(MAX_REASON_LEN);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let text = &text.as_bytes()[..cut];
        let mut b = Self::header(KIND_REFUSE, reason.code(), 0, text.len() as u32);
        b.truncate(REPLY_HEADER_LEN);
        b.extend_from_slice(text);
        b
    }

    fn header(kind: u32, a: u32, b: u32, text_len: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity(MAX_MESSAGE_LEN);
        v.extend_from_slice(&REPLY_MAGIC.to_le_bytes());
        v.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(&a.to_le_bytes());
        v.extend_from_slice(&b.to_le_bytes());
        v.extend_from_slice(&text_len.to_le_bytes());
        v
    }

    /// Parses a reply. A reply of another version is
    /// [`ReplyError::Version`], which a client treats as a refusal.
    pub fn decode(bytes: &[u8]) -> Result<Self, ReplyError> {
        if bytes.len() < 8 || word(bytes, 0) != REPLY_MAGIC {
            return Err(ReplyError::Malformed("no reply magic"));
        }
        let version = word(bytes, 4);
        if version != PROTOCOL_VERSION {
            return Err(ReplyError::Version(version));
        }
        if bytes.len() < REPLY_HEADER_LEN {
            return Err(ReplyError::Malformed("short reply"));
        }
        let (kind, a, b, text_len) = (
            word(bytes, 8),
            word(bytes, 12),
            word(bytes, 16),
            word(bytes, 20),
        );
        match kind {
            KIND_ACCEPT if bytes.len() == REPLY_HEADER_LEN => Ok(Self::Accept(Accept {
                version,
                pipeline_depth: a,
                latency_samples: b,
            })),
            KIND_REFUSE if bytes.len() == REPLY_HEADER_LEN + text_len as usize => {
                Ok(Self::Refuse(Refusal {
                    version,
                    reason: RefuseReason::from_code(a),
                    text: String::from_utf8_lossy(&bytes[REPLY_HEADER_LEN..]).into_owned(),
                }))
            }
            KIND_ACCEPT | KIND_REFUSE => Err(ReplyError::Malformed("wrong reply length")),
            _ => Err(ReplyError::Malformed("unknown reply kind")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let h = Hello::new(8);
        assert_eq!(Hello::decode(&h.encode()), Ok(h));
        assert_eq!(h.check_format(), Ok(()));
    }

    #[test]
    fn malformed_hellos_are_rejected() {
        assert!(matches!(Hello::decode(b""), Err(HelloError::Malformed(_))));
        assert!(matches!(
            Hello::decode(b"garbage!garbage!"),
            Err(HelloError::Malformed(_))
        ));
        let h = Hello::new(8).encode();
        assert!(matches!(
            Hello::decode(&h[..20]),
            Err(HelloError::Malformed(_))
        ));
        let mut long = h.to_vec();
        long.push(0);
        assert!(matches!(
            Hello::decode(&long),
            Err(HelloError::Malformed(_))
        ));
    }

    #[test]
    fn another_version_is_reported_with_its_number() {
        let mut h = Hello::new(8).encode();
        h[4..8].copy_from_slice(&7u32.to_le_bytes());
        // Even a short hello of another version reports the version.
        assert_eq!(Hello::decode(&h[..8]), Err(HelloError::Version(7)));
    }

    #[test]
    fn format_checks() {
        let mut h = Hello::new(8);
        h.sample_rate = 44_100;
        assert!(h.check_format().is_err());
        let mut h = Hello::new(6);
        h.shm_size = shm::region_size(8) as u64;
        assert!(h.check_format().is_err(), "6 is not a power of two");
        let mut h = Hello::new(8);
        h.shm_size -= 1;
        assert!(h.check_format().is_err());
    }

    #[test]
    fn replies_round_trip() {
        let a = Reply::decode(&Reply::encode_accept(2, 960)).unwrap();
        assert_eq!(
            a,
            Reply::Accept(Accept {
                version: PROTOCOL_VERSION,
                pipeline_depth: 2,
                latency_samples: 960
            })
        );
        let r =
            Reply::decode(&Reply::encode_refuse(RefuseReason::Busy, "one client only")).unwrap();
        let Reply::Refuse(r) = r else {
            panic!("not a refusal")
        };
        assert_eq!(r.reason, RefuseReason::Busy);
        assert_eq!(r.text, "one client only");
    }

    #[test]
    fn long_refusal_text_is_cut_on_a_char_boundary() {
        let text = "é".repeat(200);
        let wire = Reply::encode_refuse(RefuseReason::Internal, &text);
        assert!(wire.len() <= MAX_MESSAGE_LEN);
        let Ok(Reply::Refuse(r)) = Reply::decode(&wire) else {
            panic!()
        };
        assert!(r.text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn reply_of_another_version_is_a_version_error() {
        let mut wire = Reply::encode_accept(1, 0);
        let other = PROTOCOL_VERSION + 1;
        wire[4..8].copy_from_slice(&other.to_le_bytes());
        assert_eq!(Reply::decode(&wire), Err(ReplyError::Version(other)));
        // Version 1 (an eventfd wake) is another version too.
        wire[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(Reply::decode(&wire), Err(ReplyError::Version(1)));
        assert!(matches!(
            Reply::decode(b"OBCrxxxx"),
            Err(ReplyError::Version(_))
        ));
        assert!(matches!(
            Reply::decode(b"nope"),
            Err(ReplyError::Malformed(_))
        ));
    }
}
