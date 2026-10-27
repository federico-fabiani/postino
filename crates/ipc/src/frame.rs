//! Newline-delimited JSON frames with a hard ceiling.
//!
//! `read_line` on a socket can allocate without bound; these readers never grow past the limit plus
//! one read chunk. An over-long line is drained (so the stream can still be closed cleanly) and
//! reported as [`IpcError::FrameTooLarge`], never stored.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::IpcError;

/// Maximum request or response payload, excluding the trailing newline. A 25 MiB attachment is
/// about 35 MB of base64 and therefore does not fit: the daemon documents the 16 MiB IPC payload
/// cap and clients refuse such an operation before writing a frame. See `docs/architecture.md`.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

const READ_CHUNK: usize = 64 * 1024;

/// A bounded reader over one stream. It keeps a partial frame across reads and discards an
/// oversized line without allocating it.
pub struct FrameReader<R> {
    inner: R,
    buffer: Vec<u8>,
    start: usize,
    discarding: bool,
    limit: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R, limit: usize) -> Self {
        Self { inner, buffer: Vec::with_capacity(READ_CHUNK.min(limit.saturating_add(1))), start: 0, discarding: false, limit }
    }

    /// Read one frame, or `None` at a clean EOF. A line that exceeds the limit is consumed through
    /// its newline and reported once.
    pub async fn next_frame(&mut self) -> Result<Option<Vec<u8>>, IpcError> {
        loop {
            if !self.discarding
                && let Some(offset) = self.buffer.get(self.start..).and_then(|slice| slice.iter().position(|byte| *byte == b'\n'))
            {
                let end = self.start.saturating_add(offset);
                if end.saturating_sub(self.start) > self.limit {
                    self.consume(end.saturating_add(1));
                    return Err(IpcError::FrameTooLarge { limit: self.limit });
                }
                let frame = self.buffer.get(self.start..end).map(<[u8]>::to_vec).unwrap_or_default();
                self.consume(end.saturating_add(1));
                // JSON may legally end with a CR; serde_json treats it as whitespace.
                return Ok(Some(frame));
            }

            let available = self.buffer.len().saturating_sub(self.start);
            if available > self.limit {
                // A line longer than the limit with no newline yet: drop what we have and drain
                // until the next newline. The allocation stays at roughly the limit.
                self.buffer.clear();
                self.start = 0;
                self.discarding = true;
            }

            let mut chunk = [0u8; READ_CHUNK];
            let read = self.inner.read(&mut chunk).await?;
            if read == 0 {
                if self.discarding {
                    self.discarding = false;
                    return Err(IpcError::FrameTooLarge { limit: self.limit });
                }
                let frame = self.buffer.get(self.start..).map(<[u8]>::to_vec).unwrap_or_default();
                self.buffer.clear();
                self.start = 0;
                return if frame.is_empty() { Ok(None) } else { Ok(Some(frame)) };
            }
            let bytes = chunk.get(..read).unwrap_or_default();
            if self.discarding {
                if let Some(offset) = bytes.iter().position(|byte| *byte == b'\n') {
                    self.discarding = false;
                    let rest = bytes.get(offset.saturating_add(1)..).unwrap_or_default();
                    if !rest.is_empty() {
                        self.buffer.extend_from_slice(rest);
                    }
                    return Err(IpcError::FrameTooLarge { limit: self.limit });
                }
                continue;
            }
            self.buffer.extend_from_slice(bytes);
        }
    }

    fn consume(&mut self, count: usize) {
        self.start = self.start.saturating_add(count).min(self.buffer.len());
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        } else if self.start > READ_CHUNK * 4 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
    }
}

/// Read one frame from a stream that is already buffered by the caller.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut FrameReader<R>) -> Result<Option<Vec<u8>>, IpcError> {
    reader.next_frame().await
}

/// Serialize one JSON value as a newline-terminated frame, refusing anything above the limit.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &serde_json::Value, limit: usize) -> Result<(), IpcError> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > limit {
        return Err(IpcError::FrameTooLarge { limit });
    }
    writer.write_all(&payload).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_split_across_reads_round_trip() {
        let input = b"{\"a\":1}\n{\"b\":2}\n";
        let mut reader = FrameReader::new(std::io::Cursor::new(input.to_vec()), 1024);
        let first = reader.next_frame().await.expect("first").expect("frame");
        assert_eq!(first, b"{\"a\":1}");
        let second = reader.next_frame().await.expect("second").expect("frame");
        assert_eq!(second, b"{\"b\":2}");
        assert!(reader.next_frame().await.expect("eof").is_none());
    }

    #[tokio::test]
    async fn one_read_containing_two_frames_keeps_the_remainder() {
        let input = b"{\"one\":1}\n{\"two\":2}\n";
        let mut reader = FrameReader::new(std::io::Cursor::new(input.to_vec()), 1024);
        assert_eq!(reader.next_frame().await.expect("first").expect("frame"), b"{\"one\":1}");
        assert_eq!(reader.next_frame().await.expect("second").expect("frame"), b"{\"two\":2}");
    }

    #[tokio::test]
    async fn an_oversized_line_is_drained_and_the_next_frame_survives() {
        let mut input = vec![b'x'; 64];
        input.push(b'\n');
        input.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = FrameReader::new(std::io::Cursor::new(input), 16);
        assert!(matches!(reader.next_frame().await, Err(IpcError::FrameTooLarge { .. })));
        assert_eq!(reader.next_frame().await.expect("next").expect("frame"), b"{\"ok\":true}");
    }

    #[tokio::test]
    async fn write_refuses_a_frame_above_the_limit() {
        let mut output = Vec::new();
        let value = serde_json::json!({ "text": "x".repeat(64) });
        assert!(matches!(write_frame(&mut output, &value, 16).await, Err(IpcError::FrameTooLarge { .. })));
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn a_trailing_cr_is_left_for_json_whitespace() {
        let input = b"{\"a\":1}\r\n";
        let mut reader = FrameReader::new(std::io::Cursor::new(input.to_vec()), 1024);
        let frame = reader.next_frame().await.expect("frame").expect("frame");
        assert_eq!(frame, b"{\"a\":1}\r");
        let _: serde_json::Value = serde_json::from_slice(&frame).expect("serde accepts the CR");
    }
}
