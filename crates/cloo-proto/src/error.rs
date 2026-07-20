//! The crate-local error type for encoding, framing, and handshake failures.

use core::fmt;

/// Everything that can go wrong encoding, decoding, or framing a wire message.
///
/// Callers on the socket path must handle this rather than `unwrap()`: a panic
/// in a socket read takes down the session task with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// The peer speaks a different protocol version. The only cure is a
    /// matching build on both sides.
    VersionMismatch {
        /// The version this build speaks.
        ours: u16,
        /// The version the peer announced.
        theirs: u16,
    },
    /// A frame announced a payload larger than [`crate::MAX_FRAME_LEN`].
    /// Treated as a desync or a hostile peer, never as something to allocate for.
    FrameTooLarge {
        /// The announced payload length, in bytes.
        len: usize,
        /// The largest payload this build will accept.
        max: usize,
    },
    /// The buffer ended before a complete frame was available. The caller should
    /// read more bytes and retry.
    Incomplete,
    /// The payload was not valid postcard for the expected type.
    Malformed(String),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VersionMismatch { ours, theirs } => write!(
                f,
                "cloo protocol version mismatch: this build speaks v{ours}, the peer speaks \
                 v{theirs}. Restart the cloo server and reattach with a matching build."
            ),
            Self::FrameTooLarge { len, max } => {
                write!(
                    f,
                    "frame payload of {len} bytes exceeds the {max} byte limit"
                )
            }
            Self::Incomplete => write!(f, "incomplete frame: need more bytes"),
            Self::Malformed(why) => write!(f, "malformed frame payload: {why}"),
        }
    }
}

impl std::error::Error for ProtoError {}

impl From<postcard::Error> for ProtoError {
    fn from(err: postcard::Error) -> Self {
        match err {
            postcard::Error::DeserializeUnexpectedEnd => Self::Incomplete,
            other => Self::Malformed(other.to_string()),
        }
    }
}
