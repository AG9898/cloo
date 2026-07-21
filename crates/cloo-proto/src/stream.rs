//! Length-prefixed framing over an async byte stream.
//!
//! [`frame`](crate::frame) is pure buffer arithmetic: it does not read, write,
//! or know what a socket is. This module is the one place that pairs it with a
//! transport, so the drain-and-retry loop around [`ProtoError::Incomplete`]
//! exists once rather than once per side of the connection.
//!
//! It lives in `cloo-proto` rather than in `cloo-server` or `cloo-client`
//! because both of them need it and neither may depend on the other. It is
//! generic over the transport and still knows nothing about PTYs or rendering —
//! `UnixStream` in production, a duplex pipe in a test.
//!
//! Two rules the callers depend on. A clean end of stream *between* frames is
//! [`FrameStream::recv`] returning `Ok(None)`: a peer that closed its side is
//! ordinary, not a fault. Bytes that stop *inside* a frame are
//! [`StreamError::Truncated`], which is a real error — the peer died mid-write
//! or the framing desynced, and continuing to read would interpret garbage.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt;
use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::ProtoError;
use crate::frame::{decode, encode};

/// How many bytes to ask for per read.
///
/// Damage frames are the large ones and arrive in bursts, so a read big enough
/// to swallow a whole frame is worth the buffer.
const READ_CHUNK: usize = 16 * 1024;

/// Everything that can go wrong moving frames over a transport.
#[derive(Debug)]
pub enum StreamError {
    /// The transport failed.
    Io(io::Error),
    /// The bytes were fine but the frame was not.
    Proto(ProtoError),
    /// The peer stopped sending part way through a frame. Distinct from a clean
    /// close, which is `Ok(None)` and not an error at all.
    Truncated {
        /// How many bytes of the partial frame had arrived.
        have: usize,
    },
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "connection failed: {e}"),
            Self::Proto(e) => write!(f, "{e}"),
            Self::Truncated { have } => {
                write!(f, "connection closed inside a frame after {have} bytes")
            }
        }
    }
}

impl std::error::Error for StreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Proto(e) => Some(e),
            Self::Truncated { .. } => None,
        }
    }
}

impl From<io::Error> for StreamError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<ProtoError> for StreamError {
    fn from(value: ProtoError) -> Self {
        Self::Proto(value)
    }
}

/// A transport carrying length-prefixed postcard frames.
///
/// Holds the read buffer that makes partial frames a non-event: bytes
/// accumulate until a whole frame is present, and only whole frames are
/// handed out.
#[derive(Debug)]
pub struct FrameStream<T> {
    inner: T,
    /// Bytes received and not yet decoded. Never contains a complete frame
    /// after [`recv`](Self::recv) returns.
    buf: Vec<u8>,
}

impl<T> FrameStream<T> {
    /// Wraps a transport.
    #[must_use]
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(READ_CHUNK),
        }
    }

    /// The wrapped transport.
    #[must_use]
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Gives the transport back, discarding any partial frame.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// How many buffered bytes are waiting for the rest of their frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

impl<T: AsyncWrite + Unpin> FrameStream<T> {
    /// Encodes `message` and writes the whole frame.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Proto`] if the message could not be encoded, or
    /// [`StreamError::Io`] if the write failed.
    pub async fn send<M: Serialize>(&mut self, message: &M) -> Result<(), StreamError> {
        let frame = encode(message)?;
        self.inner.write_all(&frame).await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Shuts the write half down, so the peer sees a clean end of stream.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Io`] if the shutdown failed.
    pub async fn shutdown(&mut self) -> Result<(), StreamError> {
        self.inner.shutdown().await?;
        Ok(())
    }
}

impl<T: AsyncRead + Unpin> FrameStream<T> {
    /// Reads until one whole frame is available and decodes it.
    ///
    /// Returns `Ok(None)` when the peer closed cleanly between frames.
    ///
    /// Cancel-safe in the way the session loops need: bytes are appended to the
    /// buffer before anything is decoded, so a dropped future loses a wakeup
    /// and never a byte.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Truncated`] if the peer closed mid-frame,
    /// [`StreamError::Proto`] for a malformed payload or an implausible length
    /// prefix, and [`StreamError::Io`] for a transport failure.
    pub async fn recv<M: DeserializeOwned>(&mut self) -> Result<Option<M>, StreamError> {
        loop {
            match decode::<M>(&self.buf) {
                Ok((message, consumed)) => {
                    self.buf.drain(..consumed);
                    return Ok(Some(message));
                }
                Err(ProtoError::Incomplete) => {}
                Err(other) => return Err(other.into()),
            }

            let before = self.buf.len();
            let read = self.inner.read_buf(&mut self.buf).await?;
            if read == 0 {
                return if before == 0 {
                    Ok(None)
                } else {
                    Err(StreamError::Truncated { have: before })
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::PROTOCOL_VERSION;
    use crate::message::{ClientMessage, ServerMessage, Size, TermCaps};
    use tokio::io::duplex;

    fn attach() -> ClientMessage {
        ClientMessage::Attach {
            protocol_version: PROTOCOL_VERSION,
            size: Size::new(80, 24),
            term_caps: TermCaps::default(),
            session: None,
        }
    }

    #[tokio::test]
    async fn a_message_survives_the_transport() {
        let (a, b) = duplex(64);
        let mut writer = FrameStream::new(a);
        let mut reader = FrameStream::new(b);

        writer.send(&attach()).await.expect("send succeeds");
        let got: Option<ClientMessage> = reader.recv().await.expect("recv succeeds");
        assert_eq!(got, Some(attach()));
    }

    #[tokio::test]
    async fn frames_split_across_reads_are_reassembled() {
        // A duplex buffer smaller than one frame forces the writer to block and
        // the reader to see the message in pieces.
        let (a, b) = duplex(8);
        let mut reader = FrameStream::new(b);

        let sent = ServerMessage::Refused {
            reason: "a reason long enough to exceed the transport buffer".into(),
        };
        let expected = sent.clone();
        let writer = tokio::spawn(async move {
            let mut writer = FrameStream::new(a);
            writer.send(&sent).await.expect("send succeeds");
        });

        let got: Option<ServerMessage> = reader.recv().await.expect("recv succeeds");
        assert_eq!(got, Some(expected));
        writer.await.expect("the writer task finishes");
    }

    #[tokio::test]
    async fn queued_frames_come_back_in_order() {
        let (a, b) = duplex(1024);
        let mut writer = FrameStream::new(a);
        let mut reader = FrameStream::new(b);

        let messages = vec![
            ClientMessage::Input(vec![1, 2, 3]),
            ClientMessage::Resize(Size::new(100, 30)),
            ClientMessage::Detach,
        ];
        for message in &messages {
            writer.send(message).await.expect("send succeeds");
        }

        for expected in &messages {
            let got: Option<ClientMessage> = reader.recv().await.expect("recv succeeds");
            assert_eq!(got.as_ref(), Some(expected));
        }
    }

    #[tokio::test]
    async fn a_clean_close_between_frames_is_not_an_error() {
        let (a, b) = duplex(1024);
        let mut writer = FrameStream::new(a);
        let mut reader = FrameStream::new(b);

        writer.send(&ClientMessage::Detach).await.expect("send");
        writer.shutdown().await.expect("shutdown succeeds");
        drop(writer);

        let first: Option<ClientMessage> = reader.recv().await.expect("recv succeeds");
        assert_eq!(first, Some(ClientMessage::Detach));
        let second: Option<ClientMessage> = reader.recv().await.expect("a clean close is Ok(None)");
        assert_eq!(second, None);
        assert_eq!(reader.buffered(), 0);
    }

    #[tokio::test]
    async fn a_close_inside_a_frame_is_an_error() {
        let (mut a, b) = duplex(1024);
        let mut reader = FrameStream::new(b);

        let frame = encode(&attach()).expect("attach encodes");
        a.write_all(&frame[..frame.len() - 1])
            .await
            .expect("a partial frame is writable");
        a.shutdown().await.expect("shutdown succeeds");
        drop(a);

        let err = reader
            .recv::<ClientMessage>()
            .await
            .expect_err("a truncated frame must be refused");
        assert!(
            matches!(err, StreamError::Truncated { have } if have == frame.len() - 1),
            "expected Truncated, got {err}"
        );
    }

    #[tokio::test]
    async fn an_implausible_length_prefix_is_refused_before_reading_it() {
        let (mut a, b) = duplex(1024);
        let mut reader = FrameStream::new(b);

        a.write_all(&u32::MAX.to_be_bytes())
            .await
            .expect("a prefix is writable");

        let err = reader
            .recv::<ClientMessage>()
            .await
            .expect_err("an oversized frame must be refused");
        assert!(
            matches!(err, StreamError::Proto(ProtoError::FrameTooLarge { .. })),
            "expected FrameTooLarge, got {err}"
        );
    }
}
