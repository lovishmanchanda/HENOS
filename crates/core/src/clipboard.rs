//! Shared clipboard: copy on one device, paste on the others.
//!
//! Each platform supplies two things: a [`ClipboardBackend`] that can put
//! content on the system clipboard, and a call to
//! [`ClipboardSync::local_copy`] whenever the user copies something. This
//! module does the rest: sending to paired devices, applying what they send,
//! and making sure an item never bounces back and forth.
//!
//! Rules:
//!
//! - A device shares its clipboard only with devices it is paired with
//!   directly, over the encrypted connection. Nothing is relayed onward.
//! - Only fresh copies travel. Connecting does not send what is already on
//!   the clipboard.
//! - Every item carries a stamp (a clock value that only moves forward). If
//!   two devices copy at the same moment, both end up with the same item:
//!   the one with the higher stamp.
//! - When we put a received item on the system clipboard, the platform
//!   reports that as a change. It is recognised by its content hash and not
//!   sent back.
//!
//! This version carries plain text. The wire format names the kind of
//! content, so images can be added without breaking older apps, which skip
//! kinds they do not know.

use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::broadcast,
    task::JoinHandle,
};
use tracing::{debug, warn};

use crate::{
    error::{Error, Result},
    identity::DeviceId,
    node::{Node, NodeEvent},
};

/// Capability a device announces when it takes part in clipboard sync.
pub const CAPABILITY: &str = "clipboard";

/// Largest text item that is sent or accepted.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// A stamp from another device may be at most this far ahead of our clock.
/// Anything beyond is pulled back, so one device with a wild clock cannot
/// freeze sync for everyone else.
const MAX_STAMP_LEAD_MS: u64 = 5 * 60 * 1000;

/// Largest image that is sent or accepted, as a PNG file.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Largest image side accepted, in pixels. A small file can claim an
/// enormous picture; this keeps decoding it from eating the memory.
const MAX_IMAGE_SIDE: u32 = 12_000;
const MAX_IMAGE_DECODE_BYTES: u64 = 400 * 1024 * 1024;

const KIND_TEXT: &str = "text";
const KIND_PNG: &str = "png";

/// Something that can be on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClipboardContent {
    Text(String),
    Image(PngImage),
}

/// A picture, held as a PNG file that has been checked to decode.
#[derive(Clone, PartialEq, Eq)]
pub struct PngImage {
    png: Vec<u8>,
    width: u32,
    height: u32,
    /// Identifies the picture by its pixels, not by its file bytes: systems
    /// re-encode an image when it passes through the clipboard, and the
    /// same picture must still be recognised as the same.
    pixels: [u8; 32],
}

impl std::fmt::Debug for PngImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PngImage({}x{}, {} bytes)",
            self.width,
            self.height,
            self.png.len()
        )
    }
}

impl PngImage {
    /// Checks that `png` is a PNG of acceptable size and takes it.
    pub fn new(png: Vec<u8>) -> Result<Self> {
        if png.len() > MAX_IMAGE_BYTES {
            return Err(Error::FrameTooLarge {
                size: png.len(),
                limit: MAX_IMAGE_BYTES,
            });
        }
        let invalid = |why: String| Error::Protocol(format!("not a usable image: {why}"));
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(&png), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_IMAGE_SIDE);
        limits.max_image_height = Some(MAX_IMAGE_SIDE);
        limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
        reader.limits(limits);
        let decoded = reader.decode().map_err(|e| invalid(e.to_string()))?;
        let rgba = decoded.to_rgba8();
        let (width, height) = rgba.dimensions();
        if width == 0 || height == 0 {
            return Err(invalid("it has no pixels".into()));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(&width.to_be_bytes());
        hasher.update(&height.to_be_bytes());
        hasher.update(rgba.as_raw());
        Ok(Self {
            png,
            width,
            height,
            pixels: *hasher.finalize().as_bytes(),
        })
    }

    /// The image as a PNG file.
    pub fn png(&self) -> &[u8] {
        &self.png
    }

    /// Width and height in pixels.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl ClipboardContent {
    /// Payload size in bytes.
    pub fn len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Image(image) => image.png.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => KIND_TEXT,
            Self::Image(_) => KIND_PNG,
        }
    }

    fn payload(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Image(image) => &image.png,
        }
    }

    fn limit(&self) -> usize {
        match self {
            Self::Text(_) => MAX_TEXT_BYTES,
            Self::Image(_) => MAX_IMAGE_BYTES,
        }
    }

    /// Identifies the content for echo detection. Line endings are ignored,
    /// because some platforms change them when text passes through the
    /// system clipboard.
    fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.kind().as_bytes());
        hasher.update(&[0]);
        match self {
            Self::Text(text) => {
                for chunk in text.split('\r') {
                    hasher.update(chunk.as_bytes());
                }
            }
            Self::Image(image) => {
                hasher.update(&image.pixels);
            }
        }
        *hasher.finalize().as_bytes()
    }
}

/// A clipboard item as it travels between devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardItem {
    pub content: ClipboardContent,
    /// Clock value set by the device that copied it. Higher is newer.
    pub stamp: u64,
    /// The device that copied it.
    pub origin: DeviceId,
}

/// First frame on a clipboard stream. The payload follows as raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClipboardHeader {
    /// `"text"` today. A string so that unknown kinds can be skipped.
    kind: String,
    size: u64,
    stamp: u64,
}

impl ClipboardHeader {
    pub(crate) fn for_item(item: &ClipboardItem) -> Self {
        Self {
            kind: item.content.kind().to_owned(),
            size: item.content.len() as u64,
            stamp: item.stamp,
        }
    }
}

/// Writes an item's payload. The header has already been sent.
pub(crate) async fn write_payload<W>(send: &mut W, item: &ClipboardItem) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    send.write_all(item.content.payload()).await?;
    Ok(())
}

/// Reads the payload announced by `header`, sent by `from`.
///
/// The announced size is checked against the limit before any memory is
/// reserved, and the content is validated before it is handed on.
pub(crate) async fn read_payload<R>(
    recv: &mut R,
    header: &ClipboardHeader,
    from: DeviceId,
) -> Result<ClipboardItem>
where
    R: AsyncRead + Unpin,
{
    let limit = match header.kind.as_str() {
        KIND_TEXT => MAX_TEXT_BYTES,
        KIND_PNG => MAX_IMAGE_BYTES,
        other => {
            return Err(Error::Protocol(format!(
                "unknown clipboard content kind {other:?}"
            )))
        }
    };
    let size = usize::try_from(header.size).unwrap_or(usize::MAX);
    if size > limit {
        return Err(Error::FrameTooLarge { size, limit });
    }
    if size == 0 {
        return Err(Error::Protocol("empty clipboard item".into()));
    }

    let mut payload = vec![0u8; size];
    recv.read_exact(&mut payload).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Error::Protocol("clipboard item shorter than announced".into())
        } else {
            Error::Io(e)
        }
    })?;

    // The stream must end here. Reading the end also tells the sender that
    // the whole item was taken, rather than cut off.
    let mut extra = [0u8; 1];
    if recv.read(&mut extra).await? != 0 {
        return Err(Error::Protocol(
            "clipboard item longer than announced".into(),
        ));
    }

    let content = match header.kind.as_str() {
        KIND_TEXT => ClipboardContent::Text(
            String::from_utf8(payload)
                .map_err(|_| Error::Protocol("clipboard text is not valid UTF-8".into()))?,
        ),
        // Decoded here, so a damaged or oversized picture never reaches the
        // system clipboard.
        KIND_PNG => ClipboardContent::Image(PngImage::new(payload)?),
        _ => unreachable!("kind was checked above"),
    };
    Ok(ClipboardItem {
        content,
        stamp: header.stamp,
        // Items are never relayed, so the sender is always the origin. Taking
        // it from the authenticated connection means it cannot be forged.
        origin: from,
    })
}

/// Puts content on the system clipboard. Implemented by each platform.
pub trait ClipboardBackend: Send + Sync + 'static {
    /// Replaces the clipboard content. May block briefly.
    fn write(&self, content: &ClipboardContent) -> std::result::Result<(), String>;
}

/// What happened to a local copy handed to [`ClipboardSync::local_copy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalCopy {
    /// Sent to this many connected devices (possibly zero).
    Sent { devices: usize },
    /// Not new: it is what we already hold, usually our own write of a
    /// received item being reported back by the platform.
    Echo,
}

/// Progress reports for UIs. They never contain the clipboard content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardEvent {
    Sent {
        bytes: usize,
        devices: usize,
        /// The item was a picture rather than text.
        image: bool,
    },
    Received {
        from: DeviceId,
        bytes: usize,
        image: bool,
    },
}

#[derive(Default)]
struct SyncState {
    /// Stamp and origin of the item we currently hold.
    current: Option<(u64, [u8; 32])>,
    /// Fingerprint of the item we currently hold.
    fingerprint: Option<[u8; 32]>,
}

/// Keeps this device's clipboard in step with its paired devices.
pub struct ClipboardSync {
    node: Arc<Node>,
    backend: Arc<dyn ClipboardBackend>,
    state: Mutex<SyncState>,
    /// Held while a received item is being put on the system clipboard and
    /// while a local copy is being stamped, so the two never interleave. A
    /// copy made just as an item arrives is then always ordered after it,
    /// and what the clipboard holds matches what the other devices hold.
    order: tokio::sync::Mutex<()>,
    events: broadcast::Sender<ClipboardEvent>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ClipboardSync {
    /// Starts applying clipboard items received by `node`. The node must
    /// announce [`CAPABILITY`], or other devices will not send it anything.
    pub fn start(node: Arc<Node>, backend: Arc<dyn ClipboardBackend>) -> Arc<Self> {
        if !node.capabilities().iter().any(|c| c == CAPABILITY) {
            warn!(
                "clipboard sync started on a node that does not announce the clipboard capability"
            );
        }
        let (events, _) = broadcast::channel(64);
        let sync = Arc::new(Self {
            node: node.clone(),
            backend,
            state: Mutex::default(),
            order: tokio::sync::Mutex::new(()),
            events,
            task: Mutex::new(None),
        });

        // Subscribe before returning so nothing received from now on is missed.
        let mut node_events = node.subscribe();
        let weak = Arc::downgrade(&sync);
        let task = tokio::spawn(async move {
            loop {
                match node_events.recv().await {
                    Ok(NodeEvent::ClipboardReceived(item)) => {
                        let Some(sync) = weak.upgrade() else { break };
                        sync.apply_remote(&item).await;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!("clipboard sync fell behind and skipped {missed} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *sync.task.lock().unwrap() = Some(task);
        sync
    }

    /// Subscribes to progress reports.
    pub fn subscribe(&self) -> broadcast::Receiver<ClipboardEvent> {
        self.events.subscribe()
    }

    /// Call when the user copied something on this device. Sends it to every
    /// connected paired device that takes part in clipboard sync.
    pub async fn local_copy(&self, content: ClipboardContent) -> Result<LocalCopy> {
        if content.is_empty() {
            return Ok(LocalCopy::Echo);
        }
        let limit = content.limit();
        if content.len() > limit {
            return Err(Error::FrameTooLarge {
                size: content.len(),
                limit,
            });
        }

        let fingerprint = content.fingerprint();
        let ordered = self.order.lock().await;
        let stamp = {
            let mut state = self.state.lock().unwrap();
            if state.fingerprint == Some(fingerprint) {
                return Ok(LocalCopy::Echo);
            }
            // Never behind the wall clock, always ahead of what we hold.
            let after_current = state.current.map_or(0, |(s, _)| s.saturating_add(1));
            let stamp = now_ms().max(after_current);
            state.current = Some((stamp, *self.node.id().as_bytes()));
            state.fingerprint = Some(fingerprint);
            stamp
        };
        drop(ordered);

        let bytes = content.len();
        let image = matches!(content, ClipboardContent::Image(_));
        let item = Arc::new(ClipboardItem {
            content,
            stamp,
            origin: self.node.id(),
        });
        let devices = self.node.send_clipboard(item).await;
        debug!(bytes, devices, "clipboard sent");
        let _ = self.events.send(ClipboardEvent::Sent {
            bytes,
            devices,
            image,
        });
        Ok(LocalCopy::Sent { devices })
    }

    /// Applies an item received from a paired device, unless we already hold
    /// something newer.
    async fn apply_remote(&self, item: &Arc<ClipboardItem>) {
        // Pull back stamps from clocks running far ahead of ours.
        let stamp = item.stamp.min(now_ms().saturating_add(MAX_STAMP_LEAD_MS));
        let key = (stamp, *item.origin.as_bytes());
        let _ordered = self.order.lock().await;
        {
            let mut state = self.state.lock().unwrap();
            if state.current.is_some_and(|current| key <= current) {
                debug!(from = %item.origin.fmt_short(), "ignoring older clipboard item");
                return;
            }
            state.current = Some(key);
            // Set before writing, so the platform's change report for our own
            // write is recognised as an echo.
            state.fingerprint = Some(item.content.fingerprint());
        }

        let backend = self.backend.clone();
        let to_write = item.clone();
        let written = tokio::task::spawn_blocking(move || backend.write(&to_write.content)).await;
        match written {
            Ok(Ok(())) => {
                debug!(from = %item.origin.fmt_short(), bytes = item.content.len(), "clipboard received");
                let _ = self.events.send(ClipboardEvent::Received {
                    from: item.origin,
                    bytes: item.content.len(),
                    image: matches!(item.content, ClipboardContent::Image(_)),
                });
            }
            Ok(Err(e)) => warn!("could not write to the clipboard: {e}"),
            Err(e) => warn!("clipboard write task failed: {e}"),
        }
    }

    /// Stops applying received items.
    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.stop();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn text(s: &str) -> ClipboardContent {
        ClipboardContent::Text(s.to_owned())
    }

    fn header(kind: &str, size: u64) -> ClipboardHeader {
        ClipboardHeader {
            kind: kind.to_owned(),
            size,
            stamp: 7,
        }
    }

    #[tokio::test]
    async fn payload_round_trips() {
        let from = Identity::generate().id();
        let item = ClipboardItem {
            content: text("héllo\nwörld"),
            stamp: 42,
            origin: from,
        };
        let mut wire = Vec::new();
        write_payload(&mut wire, &item).await.unwrap();
        let head = ClipboardHeader::for_item(&item);
        let read = read_payload(&mut wire.as_slice(), &head, from)
            .await
            .unwrap();
        assert_eq!(read, item);
    }

    #[tokio::test]
    async fn oversized_item_is_refused_before_reading_it() {
        let from = Identity::generate().id();
        // Nothing follows the header: this must fail on the size alone.
        let err = read_payload(&mut [].as_slice(), &header("text", u64::MAX), from)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
        let err = read_payload(
            &mut [].as_slice(),
            &header("text", MAX_TEXT_BYTES as u64 + 1),
            from,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn malformed_items_are_refused() {
        let from = Identity::generate().id();
        // Shorter than announced.
        let err = read_payload(&mut b"abc".as_slice(), &header("text", 10), from)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
        // Longer than announced.
        let err = read_payload(&mut b"abcdef".as_slice(), &header("text", 3), from)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
        // Not UTF-8.
        let err = read_payload(&mut [0xff, 0xfe].as_slice(), &header("text", 2), from)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
        // Empty.
        let err = read_payload(&mut [].as_slice(), &header("text", 0), from)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
        // A kind from a newer version.
        let err = read_payload(&mut b"abc".as_slice(), &header("hologram", 3), from)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    /// A small PNG with the given pixel colour and compression setting.
    fn png(colour: [u8; 4], fast: bool) -> Vec<u8> {
        use image::{codecs::png, ImageEncoder};
        let pixels: Vec<u8> = colour.iter().copied().cycle().take(4 * 8 * 8).collect();
        let mut file = Vec::new();
        let level = if fast {
            png::CompressionType::Fast
        } else {
            png::CompressionType::Best
        };
        png::PngEncoder::new_with_quality(&mut file, level, png::FilterType::Adaptive)
            .write_image(&pixels, 8, 8, image::ExtendedColorType::Rgba8)
            .unwrap();
        file
    }

    #[tokio::test]
    async fn image_round_trips() {
        let from = Identity::generate().id();
        let picture = PngImage::new(png([200, 30, 30, 255], true)).unwrap();
        assert_eq!(picture.dimensions(), (8, 8));
        let item = ClipboardItem {
            content: ClipboardContent::Image(picture),
            stamp: 1,
            origin: from,
        };
        let mut wire = Vec::new();
        write_payload(&mut wire, &item).await.unwrap();
        let head = ClipboardHeader::for_item(&item);
        let read = read_payload(&mut wire.as_slice(), &head, from)
            .await
            .unwrap();
        assert_eq!(read, item);
    }

    #[test]
    fn same_picture_is_recognised_however_it_is_encoded() {
        let red_fast = PngImage::new(png([200, 30, 30, 255], true)).unwrap();
        let red_best = PngImage::new(png([200, 30, 30, 255], false)).unwrap();
        let blue = PngImage::new(png([30, 30, 200, 255], true)).unwrap();
        assert_ne!(red_fast.png(), red_best.png(), "the two files differ");
        assert_eq!(
            ClipboardContent::Image(red_fast.clone()).fingerprint(),
            ClipboardContent::Image(red_best).fingerprint()
        );
        assert_ne!(
            ClipboardContent::Image(red_fast).fingerprint(),
            ClipboardContent::Image(blue).fingerprint()
        );
    }

    #[tokio::test]
    async fn broken_or_oversized_images_are_refused() {
        let from = Identity::generate().id();
        assert!(PngImage::new(b"definitely not a png".to_vec()).is_err());
        assert!(PngImage::new(Vec::new()).is_err());
        let mut cut = png([1, 2, 3, 255], true);
        cut.truncate(cut.len() / 2);
        assert!(PngImage::new(cut).is_err());
        assert!(matches!(
            PngImage::new(vec![0; MAX_IMAGE_BYTES + 1]),
            Err(Error::FrameTooLarge { .. })
        ));
        // Announced as an image but too big: refused on the size alone.
        let err = read_payload(
            &mut [].as_slice(),
            &header("png", MAX_IMAGE_BYTES as u64 + 1),
            from,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
        // Announced as an image but not one.
        let err = read_payload(&mut b"junk".as_slice(), &header("png", 4), from)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn fingerprint_ignores_line_ending_style_only() {
        assert_eq!(text("a\r\nb").fingerprint(), text("a\nb").fingerprint());
        assert_ne!(text("a\nb").fingerprint(), text("a b").fingerprint());
        assert_ne!(text("ab").fingerprint(), text("ba").fingerprint());
    }
}
