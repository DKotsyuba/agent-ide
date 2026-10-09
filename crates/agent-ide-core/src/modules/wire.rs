//! `bundled-module/0` framing: `u32be length | u8 kind | body` over owned stdio.
//!
//! The length counts the kind byte and the body. Kind 0 is one UTF-8 JSON control object of at
//! most [`MAX_CONTROL`] bytes; kind 1 is one raw attachment chunk: a fixed header (request id,
//! attachment id, byte offset, all big-endian) and at most [`MAX_CHUNK`] data bytes. Zero, unknown
//! and over-cap lengths are refused before anything is allocated. Bytes never travel as JSON
//! number arrays: a control frame declares its attachments ([`AttachmentDecl`]) and their chunks
//! follow it, in declaration order, before the next control frame; [`Assembler`] accepts only a
//! contiguous, exactly sized, in-order delivery.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest control body in bytes.
pub const MAX_CONTROL: usize = 1 << 20;
/// Largest data bytes in one attachment chunk.
pub const MAX_CHUNK: usize = 64 * 1024;
/// Chunk header: request id (8), attachment id (4), byte offset (8).
pub const CHUNK_HEADER: usize = 8 + 4 + 8;
/// Default ceiling of all attachment bytes one message may declare: two 64 MiB output streams
/// plus room for the request's own source.
pub const MAX_MESSAGE_ATTACHMENTS: u64 = 129 << 20;
/// Frame kind of a control object.
const KIND_CONTROL: u8 = 0;
/// Frame kind of an attachment chunk.
const KIND_DATA: u8 = 1;

/// A transport fault; every one poisons the channel it happened on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireError {
    /// The peer closed the stream at a frame boundary.
    Eof,
    /// The stream ended inside a frame.
    Truncated,
    /// A frame announced length zero.
    ZeroLength,
    /// A frame announced more bytes than its kind allows.
    Oversized(u64),
    /// A frame carried an unknown kind byte.
    UnknownKind(u8),
    /// A control body is not one JSON object, or a chunk is shorter than its header.
    Malformed,
    /// The stream failed with this I/O error kind.
    Io(std::io::ErrorKind),
}

impl fmt::Display for WireError {
    /// Writes a short lowercase cause.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eof => f.write_str("end of stream"),
            Self::Truncated => f.write_str("truncated frame"),
            Self::ZeroLength => f.write_str("zero-length frame"),
            Self::Oversized(length) => write!(f, "oversized frame ({length} bytes)"),
            Self::UnknownKind(kind) => write!(f, "unknown frame kind {kind}"),
            Self::Malformed => f.write_str("malformed frame"),
            Self::Io(kind) => write!(f, "stream error ({kind})"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<std::io::Error> for WireError {
    /// Maps an unexpected end to [`WireError::Truncated`], anything else to [`WireError::Io`].
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::UnexpectedEof => Self::Truncated,
            kind => Self::Io(kind),
        }
    }
}

/// One raw attachment chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chunk {
    /// The request the attachment belongs to.
    pub request_id: u64,
    /// The attachment id its control frame declared.
    pub attachment: u32,
    /// Byte offset of `bytes` inside the attachment.
    pub offset: u64,
    /// At most [`MAX_CHUNK`] bytes.
    pub bytes: Vec<u8>,
}

/// One decoded frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// A JSON control object.
    Control(Value),
    /// An attachment chunk.
    Data(Chunk),
}

/// Reads one frame. Length and kind are validated before the body is allocated.
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(reader: &mut R) -> Result<Frame, WireError> {
    let mut length = [0u8; 4];
    let mut filled = 0;
    while filled < length.len() {
        match reader.read(&mut length[filled..]).await? {
            0 if filled == 0 => return Err(WireError::Eof),
            0 => return Err(WireError::Truncated),
            read => filled += read,
        }
    }
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 {
        return Err(WireError::ZeroLength);
    }
    if length > 1 + MAX_CONTROL.max(CHUNK_HEADER + MAX_CHUNK) {
        return Err(WireError::Oversized(length as u64));
    }
    let kind = reader.read_u8().await?;
    let body = length - 1;
    match kind {
        KIND_CONTROL if body > MAX_CONTROL => Err(WireError::Oversized(length as u64)),
        KIND_DATA if body > CHUNK_HEADER + MAX_CHUNK => Err(WireError::Oversized(length as u64)),
        KIND_DATA if body < CHUNK_HEADER => Err(WireError::Malformed),
        KIND_CONTROL => {
            let mut bytes = vec![0; body];
            reader.read_exact(&mut bytes).await?;
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) if value.is_object() => Ok(Frame::Control(value)),
                _ => Err(WireError::Malformed),
            }
        }
        KIND_DATA => {
            let request_id = reader.read_u64().await?;
            let attachment = reader.read_u32().await?;
            let offset = reader.read_u64().await?;
            let mut bytes = vec![0; body - CHUNK_HEADER];
            reader.read_exact(&mut bytes).await?;
            Ok(Frame::Data(Chunk {
                request_id,
                attachment,
                offset,
                bytes,
            }))
        }
        other => Err(WireError::UnknownKind(other)),
    }
}

/// Writes one control object; a body over [`MAX_CONTROL`] is refused before anything is written.
pub async fn write_control<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    value: &impl Serialize,
) -> Result<(), WireError> {
    let body = serde_json::to_vec(value).map_err(|_| WireError::Malformed)?;
    if body.len() > MAX_CONTROL {
        return Err(WireError::Oversized(body.len() as u64 + 1));
    }
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.extend_from_slice(&(body.len() as u32 + 1).to_be_bytes());
    frame.push(KIND_CONTROL);
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Writes `bytes` as attachment `attachment` of `request_id`, in chunks of at most [`MAX_CHUNK`].
/// An empty attachment sends no chunk.
pub async fn write_attachment<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    request_id: u64,
    attachment: u32,
    bytes: &[u8],
) -> Result<(), WireError> {
    for (index, part) in bytes.chunks(MAX_CHUNK).enumerate() {
        write_chunk(
            writer,
            &Chunk {
                request_id,
                attachment,
                offset: (index * MAX_CHUNK) as u64,
                bytes: part.to_vec(),
            },
        )
        .await?;
    }
    Ok(())
}

/// Writes one chunk; more than [`MAX_CHUNK`] data bytes are refused before anything is written.
pub async fn write_chunk<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    chunk: &Chunk,
) -> Result<(), WireError> {
    if chunk.bytes.len() > MAX_CHUNK {
        return Err(WireError::Oversized(
            (1 + CHUNK_HEADER + chunk.bytes.len()) as u64,
        ));
    }
    let mut frame = Vec::with_capacity(5 + CHUNK_HEADER + chunk.bytes.len());
    frame.extend_from_slice(&((1 + CHUNK_HEADER + chunk.bytes.len()) as u32).to_be_bytes());
    frame.push(KIND_DATA);
    frame.extend_from_slice(&chunk.request_id.to_be_bytes());
    frame.extend_from_slice(&chunk.attachment.to_be_bytes());
    frame.extend_from_slice(&chunk.offset.to_be_bytes());
    frame.extend_from_slice(&chunk.bytes);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// One attachment a control frame announces.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentDecl {
    /// Id, unique within its message.
    pub id: u32,
    /// Exact byte length.
    pub length: u64,
    /// Content type: `application/octet-stream`, `text/plain; charset=utf-8`, `application/json`.
    pub content_type: String,
}

/// One fully received attachment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attachment {
    /// The declared id.
    pub id: u32,
    /// The declared content type.
    pub content_type: String,
    /// Every declared byte.
    pub bytes: Vec<u8>,
}

impl Attachment {
    /// Raw bytes with the octet-stream content type.
    pub fn octets(id: u32, bytes: Vec<u8>) -> Self {
        Self {
            id,
            content_type: "application/octet-stream".to_owned(),
            bytes,
        }
    }

    /// The declaration announcing this attachment.
    pub fn decl(&self) -> AttachmentDecl {
        AttachmentDecl {
            id: self.id,
            length: self.bytes.len() as u64,
            content_type: self.content_type.clone(),
        }
    }
}

/// Why an attachment delivery was refused; every one poisons the channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttachmentError {
    /// Two declarations share an id.
    Duplicate(u32),
    /// The declared total exceeds the message ceiling.
    OverBudget(u64),
    /// A chunk names another request.
    WrongRequest(u64),
    /// A chunk names an undeclared attachment, or arrived when none was expected.
    Unknown(u32),
    /// A chunk starts after the bytes received so far.
    Gap(u32),
    /// A chunk starts before the bytes received so far.
    Overlap(u32),
    /// A chunk runs past the declared length.
    Overrun(u32),
    /// A chunk of a later attachment arrived before an earlier one completed.
    OutOfOrder(u32),
    /// The delivery ended before this attachment completed.
    Truncated(u32),
}

impl fmt::Display for AttachmentError {
    /// Writes a short lowercase cause.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(id) => write!(f, "duplicate attachment {id}"),
            Self::OverBudget(total) => write!(f, "attachments over budget ({total} bytes)"),
            Self::WrongRequest(id) => write!(f, "chunk for request {id}"),
            Self::Unknown(id) => write!(f, "undeclared attachment {id}"),
            Self::Gap(id) => write!(f, "gap in attachment {id}"),
            Self::Overlap(id) => write!(f, "overlap in attachment {id}"),
            Self::Overrun(id) => write!(f, "overrun of attachment {id}"),
            Self::OutOfOrder(id) => write!(f, "attachment {id} out of order"),
            Self::Truncated(id) => write!(f, "attachment {id} truncated"),
        }
    }
}

impl std::error::Error for AttachmentError {}

/// Assembles the attachments one control frame declared from the chunks that follow it.
#[derive(Debug)]
pub struct Assembler {
    /// The request every chunk must name.
    request_id: u64,
    /// Declarations with the bytes received so far, in declaration order.
    entries: Vec<(AttachmentDecl, Vec<u8>)>,
    /// Index of the attachment currently being received.
    current: usize,
}

impl Assembler {
    /// Starts assembling `decls` of `request_id`; duplicate ids or a total over `budget` refuse.
    pub fn new(
        request_id: u64,
        decls: &[AttachmentDecl],
        budget: u64,
    ) -> Result<Self, AttachmentError> {
        let mut total = 0u64;
        for (index, decl) in decls.iter().enumerate() {
            if decls[..index].iter().any(|seen| seen.id == decl.id) {
                return Err(AttachmentError::Duplicate(decl.id));
            }
            total = total.saturating_add(decl.length);
        }
        if total > budget {
            return Err(AttachmentError::OverBudget(total));
        }
        let mut assembler = Self {
            request_id,
            entries: decls
                .iter()
                .map(|decl| (decl.clone(), Vec::new()))
                .collect(),
            current: 0,
        };
        assembler.skip_complete();
        Ok(assembler)
    }

    /// Advances past attachments that already hold every declared byte (empty ones included).
    fn skip_complete(&mut self) {
        while self
            .entries
            .get(self.current)
            .is_some_and(|(decl, bytes)| bytes.len() as u64 == decl.length)
        {
            self.current += 1;
        }
    }

    /// Whether every declared byte arrived.
    pub fn is_complete(&self) -> bool {
        self.current == self.entries.len()
    }

    /// Accepts the next chunk: it must name this request and the attachment in progress, start
    /// exactly where the received bytes end, and stay inside the declared length.
    pub fn accept(&mut self, chunk: Chunk) -> Result<(), AttachmentError> {
        if chunk.request_id != self.request_id {
            return Err(AttachmentError::WrongRequest(chunk.request_id));
        }
        let Some(position) = self
            .entries
            .iter()
            .position(|(decl, _)| decl.id == chunk.attachment)
        else {
            return Err(AttachmentError::Unknown(chunk.attachment));
        };
        if position != self.current {
            return Err(if position < self.current {
                AttachmentError::Overrun(chunk.attachment)
            } else {
                AttachmentError::OutOfOrder(chunk.attachment)
            });
        }
        let (decl, bytes) = &mut self.entries[position];
        let received = bytes.len() as u64;
        if chunk.offset > received {
            return Err(AttachmentError::Gap(decl.id));
        }
        if chunk.offset < received {
            return Err(AttachmentError::Overlap(decl.id));
        }
        if received + chunk.bytes.len() as u64 > decl.length {
            return Err(AttachmentError::Overrun(decl.id));
        }
        bytes.extend_from_slice(&chunk.bytes);
        self.skip_complete();
        Ok(())
    }

    /// The assembled attachments; an incomplete delivery is [`AttachmentError::Truncated`].
    pub fn finish(self) -> Result<Vec<Attachment>, AttachmentError> {
        if let Some((decl, _)) = self.entries.get(self.current) {
            return Err(AttachmentError::Truncated(decl.id));
        }
        Ok(self
            .entries
            .into_iter()
            .map(|(decl, bytes)| Attachment {
                id: decl.id,
                content_type: decl.content_type,
                bytes,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Encodes a raw frame header plus `rest`.
    fn raw(length: u32, rest: &[u8]) -> Vec<u8> {
        let mut bytes = length.to_be_bytes().to_vec();
        bytes.extend_from_slice(rest);
        bytes
    }

    /// Control objects and chunks round-trip; a multi-chunk attachment splits at [`MAX_CHUNK`].
    #[tokio::test]
    async fn frames_round_trip() {
        let mut buffer = Vec::new();
        write_control(&mut buffer, &json!({"type": "x"}))
            .await
            .unwrap();
        let payload = (0..MAX_CHUNK * 2 + 7).map(|i| i as u8).collect::<Vec<_>>();
        write_attachment(&mut buffer, 9, 3, &payload).await.unwrap();
        let mut reader = buffer.as_slice();
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            Frame::Control(json!({"type": "x"}))
        );
        let decl = AttachmentDecl {
            id: 3,
            length: payload.len() as u64,
            content_type: "application/octet-stream".into(),
        };
        let mut assembler = Assembler::new(9, &[decl], MAX_MESSAGE_ATTACHMENTS).unwrap();
        for _ in 0..3 {
            let Frame::Data(chunk) = read_frame(&mut reader).await.unwrap() else {
                panic!("chunk expected");
            };
            assert!(chunk.bytes.len() <= MAX_CHUNK);
            assembler.accept(chunk).unwrap();
        }
        assert_eq!(read_frame(&mut reader).await, Err(WireError::Eof));
        assert_eq!(assembler.finish().unwrap()[0].bytes, payload);
    }

    /// Oversized lengths are refused from the header alone: the reader holds four bytes and a
    /// kind, never the announced body, so a refusal cannot have allocated it.
    #[tokio::test]
    async fn oversize_is_refused_before_allocation() {
        let error = read_frame(&mut raw(u32::MAX, &[]).as_slice()).await;
        assert_eq!(error, Err(WireError::Oversized(u32::MAX as u64)));
        let control = raw(MAX_CONTROL as u32 + 2, &[KIND_CONTROL]);
        assert_eq!(
            read_frame(&mut control.as_slice()).await,
            Err(WireError::Oversized(MAX_CONTROL as u64 + 2))
        );
        let data = (1 + CHUNK_HEADER + MAX_CHUNK + 1) as u32;
        assert_eq!(
            read_frame(&mut raw(data, &[KIND_DATA]).as_slice()).await,
            Err(WireError::Oversized(data as u64))
        );
        let mut sink = Vec::new();
        let big = json!({"text": "x".repeat(MAX_CONTROL)});
        assert!(matches!(
            write_control(&mut sink, &big).await,
            Err(WireError::Oversized(_))
        ));
        assert!(sink.is_empty(), "nothing written for an oversized control");
    }

    /// Zero length, unknown kind, short chunks, non-object bodies and truncated streams refuse.
    #[tokio::test]
    async fn malformed_frames_are_refused() {
        let cases: [(Vec<u8>, WireError); 7] = [
            (raw(0, &[]), WireError::ZeroLength),
            (raw(1, &[7]), WireError::UnknownKind(7)),
            (raw(3, &[KIND_DATA, 0, 0]), WireError::Malformed),
            (
                raw(4, &[KIND_CONTROL, b'4', b'2', b' ']),
                WireError::Malformed,
            ),
            (
                raw(6, &[KIND_CONTROL, b'{', b'n', b'o', b't', b'}']),
                WireError::Malformed,
            ),
            (raw(9, &[KIND_CONTROL, b'{']), WireError::Truncated),
            (vec![0, 0], WireError::Truncated),
        ];
        for (bytes, expected) in cases {
            assert_eq!(read_frame(&mut bytes.as_slice()).await, Err(expected));
        }
    }

    /// Declaration of `id` with `length` bytes.
    fn decl(id: u32, length: u64) -> AttachmentDecl {
        AttachmentDecl {
            id,
            length,
            content_type: "application/octet-stream".into(),
        }
    }

    /// Chunk of request 1.
    fn chunk(attachment: u32, offset: u64, bytes: &[u8]) -> Chunk {
        Chunk {
            request_id: 1,
            attachment,
            offset,
            bytes: bytes.to_vec(),
        }
    }

    /// Gap, overlap, overrun, wrong request, unknown id, order, duplicate, budget and truncation
    /// each refuse with their own cause; empty attachments complete without chunks.
    #[test]
    fn attachment_delivery_is_exact() {
        let fresh = || Assembler::new(1, &[decl(1, 4), decl(2, 2)], 64).unwrap();
        let mut gap = fresh();
        assert_eq!(gap.accept(chunk(1, 1, b"a")), Err(AttachmentError::Gap(1)));
        let mut overlap = fresh();
        overlap.accept(chunk(1, 0, b"ab")).unwrap();
        assert_eq!(
            overlap.accept(chunk(1, 1, b"b")),
            Err(AttachmentError::Overlap(1))
        );
        let mut overrun = fresh();
        assert_eq!(
            overrun.accept(chunk(1, 0, b"abcde")),
            Err(AttachmentError::Overrun(1))
        );
        let mut done = fresh();
        done.accept(chunk(1, 0, b"abcd")).unwrap();
        assert_eq!(
            done.accept(chunk(1, 4, b"e")),
            Err(AttachmentError::Overrun(1))
        );
        let mut wrong = fresh();
        let mut other = chunk(1, 0, b"a");
        other.request_id = 2;
        assert_eq!(wrong.accept(other), Err(AttachmentError::WrongRequest(2)));
        assert_eq!(
            fresh().accept(chunk(9, 0, b"a")),
            Err(AttachmentError::Unknown(9))
        );
        assert_eq!(
            fresh().accept(chunk(2, 0, b"a")),
            Err(AttachmentError::OutOfOrder(2))
        );
        assert_eq!(
            Assembler::new(1, &[decl(1, 1), decl(1, 1)], 64).unwrap_err(),
            AttachmentError::Duplicate(1)
        );
        assert_eq!(
            Assembler::new(1, &[decl(1, 65)], 64).unwrap_err(),
            AttachmentError::OverBudget(65)
        );
        let mut truncated = fresh();
        truncated.accept(chunk(1, 0, b"abcd")).unwrap();
        truncated.accept(chunk(2, 0, b"x")).unwrap();
        assert!(!truncated.is_complete());
        assert_eq!(truncated.finish(), Err(AttachmentError::Truncated(2)));
        let empty = Assembler::new(1, &[decl(1, 0)], 64).unwrap();
        assert!(empty.is_complete());
        assert_eq!(empty.finish().unwrap()[0].bytes, b"");
    }
}
