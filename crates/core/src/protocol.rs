//! Wire protocol: what two paired devices say to each other.
//!
//! Every connection starts with one bidirectional "control" stream opened by
//! the dialer. Both sides send a [`Hello`]; the device with the smaller id
//! then sends a [`Ready`] to put the connection into use. After that they
//! exchange [`Control`] messages for as long as the connection lives. Each
//! message is a frame: a 4-byte big-endian length followed by that many bytes
//! of postcard-encoded data.
//!
//! Compatibility rules:
//! - Unknown [`Control`] messages are skipped, so new message kinds can be
//!   added without breaking older apps.
//! - Fields may only ever be appended to [`Hello`]; older apps ignore them.
//! - Anything else bumps [`PROTOCOL_VERSION`].

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    device::DeviceInfo,
    error::{Error, Result},
};

/// Identifies the HENOS protocol during the encrypted handshake. Two devices
/// that do not offer the same value cannot connect at all.
pub const ALPN: &[u8] = b"henos/1";

/// Protocol version sent in [`Hello`]. Bumped on breaking wire changes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest frame accepted on the control stream.
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;

/// First message on the control stream, sent by both sides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub device: DeviceInfo,
    /// Features this device supports, for example `"clipboard"`. Strings so
    /// that unknown ones are simply ignored.
    pub capabilities: Vec<String>,
}

impl Hello {
    pub fn new(device: &DeviceInfo, capabilities: &[String]) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            device: device.clone(),
            capabilities: capabilities.to_vec(),
        }
    }
}

/// Sent after the hellos by the device with the smaller id, which alone
/// decides which connection the pair uses.
///
/// Two devices can dial each other at the same moment and end up with two
/// connections. If each side picked on its own, they could briefly disagree
/// and drop a connection the other was already using. Instead one side picks
/// and announces it; the other uses a connection only once it has seen this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    /// Random per run of the deciding device, so numbering restarts cleanly
    /// when that device restarts.
    pub instance: u64,
    /// Increases with every connection the deciding device puts into use.
    /// The connection with the highest number is the current one.
    pub seq: u64,
}

/// Messages exchanged on the control stream after the handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// Liveness and latency probe. Answered with a `Pong` carrying the nonce.
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

/// Largest header frame accepted at the start of a data stream.
pub const MAX_STREAM_HEADER: usize = 1024;

/// First frame on every stream other than the control stream, saying what the
/// stream carries. New kinds are added as new variants; an app that does not
/// know a kind drops that stream and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum StreamHeader {
    Clipboard(crate::clipboard::ClipboardHeader),
}

/// Encodes `msg` and writes it as one frame.
pub async fn write_frame<W, T>(writer: &mut W, msg: &T, limit: usize) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = postcard::to_allocvec(msg)
        .map_err(|e| Error::Protocol(format!("could not encode message: {e}")))?;
    if body.len() > limit {
        return Err(Error::FrameTooLarge {
            size: body.len(),
            limit,
        });
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    Ok(())
}

/// Reads one frame and returns its body.
///
/// Returns `Ok(None)` if the stream ended cleanly between frames. A stream
/// that ends in the middle of a frame, or a frame larger than `limit`, is an
/// error. The length is checked before anything is allocated, so a peer cannot
/// make us reserve memory by announcing a huge frame.
pub async fn read_frame<R>(reader: &mut R, limit: usize) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut len = [0u8; 4];
    let mut filled = 0;
    while filled < len.len() {
        let n = reader.read(&mut len[filled..]).await?;
        if n == 0 {
            if filled == 0 {
                return Ok(None);
            }
            return Err(Error::Protocol("stream ended inside a frame header".into()));
        }
        filled += n;
    }
    let size = u32::from_be_bytes(len) as usize;
    if size > limit {
        return Err(Error::FrameTooLarge { size, limit });
    }
    let mut body = vec![0u8; size];
    reader.read_exact(&mut body).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Error::Protocol("stream ended inside a frame".into())
        } else {
            Error::Io(e)
        }
    })?;
    Ok(Some(body))
}

/// Decodes a frame body.
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    postcard::from_bytes(body)
        .map_err(|e| Error::Protocol(format!("could not decode message: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Platform;

    fn hello() -> Hello {
        Hello::new(
            &DeviceInfo {
                name: "Test Mac".into(),
                platform: Platform::MacOs,
            },
            &["clipboard".to_owned()],
        )
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &hello(), MAX_CONTROL_FRAME)
            .await
            .unwrap();
        write_frame(&mut a, &Control::Ping { nonce: 7 }, MAX_CONTROL_FRAME)
            .await
            .unwrap();
        drop(a);

        let body = read_frame(&mut b, MAX_CONTROL_FRAME)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decode::<Hello>(&body).unwrap(), hello());
        let body = read_frame(&mut b, MAX_CONTROL_FRAME)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            decode::<Control>(&body).unwrap(),
            Control::Ping { nonce: 7 }
        );
        assert!(read_frame(&mut b, MAX_CONTROL_FRAME)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn oversized_frame_is_refused_before_reading_it() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // Announce 4 GiB but send nothing else: must fail on the header alone.
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let err = read_frame(&mut b, MAX_CONTROL_FRAME).await.unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn oversized_message_is_not_sent() {
        let (mut a, _b) = tokio::io::duplex(64);
        let err = write_frame(&mut a, &hello(), 4).await.unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn truncated_frame_is_an_error() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&10u32.to_be_bytes()).await.unwrap();
        a.write_all(b"abc").await.unwrap();
        drop(a);
        let err = read_frame(&mut b, MAX_CONTROL_FRAME).await.unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn garbage_does_not_decode() {
        assert!(decode::<Control>(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn hello_ignores_fields_appended_by_newer_versions() {
        let mut body = postcard::to_allocvec(&hello()).unwrap();
        body.extend_from_slice(&[1, 2, 3]);
        assert_eq!(decode::<Hello>(&body).unwrap(), hello());
    }
}
