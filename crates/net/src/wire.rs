use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};
use tokio_rustls::client::TlsStream;

use crate::NetError;
use crate::error::protocol;
use crate::limits::Limits;
use crate::parser::Segment;

/// Largest number of literals accepted inside one logical response.
const MAX_LITERALS_PER_RESPONSE: usize = 4096;
/// Read chunk for socket and literal reads.
const CHUNK: usize = 8 * 1024;
/// Compact the read buffer once this many consumed bytes are still in front of it.
const COMPACT_AFTER: usize = 16 * 1024;

/// Bytes consumed by one command, to enforce [`Limits::max_response_bytes`].
pub(crate) struct Budget {
    remaining: usize,
    limit: usize,
}

impl Budget {
    pub(crate) fn new(limit: usize) -> Self {
        Self { remaining: limit, limit }
    }

    fn spend(&mut self, bytes: usize) -> Result<(), NetError> {
        if bytes > self.remaining {
            return Err(NetError::TooLarge { what: "response", limit: self.limit });
        }
        self.remaining -= bytes;
        Ok(())
    }
}

/// A TCP stream before or after the TLS handshake.
///
/// `Placeholder` exists so the stream can be moved into `TlsConnector::connect` without an
/// `Option` and without `unwrap`. Reading or writing it is a `NotConnected` I/O error, which can
/// only happen if the handshake failed, and the session is closed in that case.
pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<Stream>>),
    Placeholder,
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(context, buffer),
            Self::Placeholder => Poll::Ready(Err(io::Error::from(io::ErrorKind::NotConnected))),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, context: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(context, bytes),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(context, bytes),
            Self::Placeholder => Poll::Ready(Err(io::Error::from(io::ErrorKind::NotConnected))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(context),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(context),
            Self::Placeholder => Poll::Ready(Err(io::Error::from(io::ErrorKind::NotConnected))),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(context),
            Self::Placeholder => Poll::Ready(Ok(())),
        }
    }
}

/// A buffered IMAP connection: bounded line and literal reads, with one deadline per command.
pub(crate) struct Connection {
    stream: Stream,
    buffer: Vec<u8>,
    position: usize,
    limits: Limits,
    timeout: Duration,
    /// Whether the response being read is half-received: a line without CRLF, or a literal that is
    /// still short. A timeout with this set is not a clean end of an idle window.
    partial: bool,
}

impl Connection {
    pub(crate) fn new(stream: Stream, limits: Limits, timeout: Duration) -> Self {
        Self { stream, buffer: Vec::new(), position: 0, limits, timeout, partial: false }
    }

    pub(crate) fn take_stream(&mut self) -> Stream {
        std::mem::replace(&mut self.stream, Stream::Placeholder)
    }

    pub(crate) fn set_stream(&mut self, stream: Stream) {
        self.stream = stream;
    }

    /// Whether the socket has bytes buffered beyond what has been consumed.
    ///
    /// After a `STARTTLS` `OK`, any such byte is plaintext sent before the handshake: a command
    /// injection attempt, and the session refuses to continue.
    pub(crate) fn has_buffered(&self) -> bool {
        self.position < self.buffer.len()
    }

    pub(crate) fn discard_buffered(&mut self) {
        self.buffer.clear();
        self.position = 0;
    }

    /// Drop the stream immediately.
    ///
    /// A session that has closed because the stream is at an unknown position must not keep the
    /// socket open: the server would hold a dead connection, and an `IDLE` would never end.
    pub(crate) fn abandon(&mut self) {
        self.stream = Stream::Placeholder;
        self.buffer.clear();
        self.position = 0;
        self.partial = false;
    }

    /// Whether bytes of the current response were received without completing it.
    ///
    /// `IDLE` uses this to tell "the wait expired and the socket is quiet" — a normal end — from
    /// "the server started a line or a literal and stalled", which is a broken connection.
    pub(crate) fn has_partial_input(&self) -> bool {
        self.partial
    }

    pub(crate) async fn write_all(&mut self, bytes: &[u8], phase: &'static str, deadline: Instant) -> Result<(), NetError> {
        let write = self.stream.write_all(bytes);
        match timeout_at(deadline, write).await {
            Err(_) => Err(NetError::Timeout { phase, after: self.timeout }),
            Ok(Err(error)) => Err(closed_or_io(phase, &error)),
            Ok(Ok(())) => match timeout_at(deadline, self.stream.flush()).await {
                Err(_) => Err(NetError::Timeout { phase, after: self.timeout }),
                Ok(Err(error)) => Err(closed_or_io(phase, &error)),
                Ok(Ok(())) => Ok(()),
            },
        }
    }

    /// Read one logical response: a line, plus any literals that end it and their continuation
    /// lines.
    pub(crate) async fn read_response(&mut self, phase: &'static str, deadline: Instant, budget: &mut Budget) -> Result<Vec<Segment>, NetError> {
        let mut segments = Vec::new();
        let mut literals = 0usize;
        self.partial = false;
        loop {
            let line = self.read_line(phase, deadline, budget).await?;
            let Some((marker_at, size)) = trailing_literal(&line) else {
                segments.push(Segment::Text(line));
                return Ok(segments);
            };
            literals += 1;
            if literals > MAX_LITERALS_PER_RESPONSE {
                return Err(NetError::TooLarge { what: "literals per response", limit: MAX_LITERALS_PER_RESPONSE });
            }
            if size > self.limits.max_literal {
                return Err(NetError::TooLarge { what: "literal", limit: self.limits.max_literal });
            }
            let text = line.get(..marker_at).ok_or_else(|| protocol("literal marker is not at a character boundary"))?;
            segments.push(Segment::Text(text.to_owned()));
            // The marker line was consumed: from here on the response is half-read.
            self.partial = true;
            segments.push(Segment::Literal(self.read_exact(size, phase, deadline, budget).await?));
        }
    }

    async fn read_line(&mut self, phase: &'static str, deadline: Instant, budget: &mut Budget) -> Result<String, NetError> {
        loop {
            let available = self.buffer.get(self.position..).unwrap_or_default();
            if let Some(end) = find_crlf(available) {
                if end > self.limits.max_line {
                    return Err(NetError::TooLarge { what: "line", limit: self.limits.max_line });
                }
                let raw = self.buffer.get(self.position..self.position + end).unwrap_or_default();
                if raw.iter().any(|byte| *byte == 0 || *byte == b'\r' || *byte == b'\n') {
                    return Err(protocol("line contains NUL, a bare CR or a bare LF"));
                }
                let line = String::from_utf8_lossy(raw).into_owned();
                budget.spend(end + 2)?;
                self.position += end + 2;
                self.compact();
                return Ok(line);
            }
            if available.len() > self.limits.max_line + 2 {
                return Err(NetError::TooLarge { what: "line", limit: self.limits.max_line });
            }
            if !available.is_empty() {
                self.partial = true;
            }
            let mut chunk = [0u8; CHUNK];
            let read = timeout_at(deadline, self.stream.read(&mut chunk)).await;
            let count = match read {
                Err(_) => return Err(NetError::Timeout { phase, after: self.timeout }),
                Ok(Err(error)) => return Err(closed_or_io(phase, &error)),
                Ok(Ok(count)) => count,
            };
            if count == 0 {
                return Err(NetError::Closed { phase });
            }
            self.buffer.extend_from_slice(chunk.get(..count).unwrap_or_default());
        }
    }

    async fn read_exact(&mut self, size: usize, phase: &'static str, deadline: Instant, budget: &mut Budget) -> Result<Vec<u8>, NetError> {
        let mut out = Vec::with_capacity(size.min(CHUNK));
        let buffered = self.buffer.len().saturating_sub(self.position).min(size);
        if buffered > 0 {
            // The bytes were read from the socket earlier but belong to this literal: charge them
            // now, or a fully buffered literal would cost nothing against the response budget.
            budget.spend(buffered)?;
            out.extend_from_slice(self.buffer.get(self.position..self.position + buffered).unwrap_or_default());
            self.position += buffered;
            self.compact();
        }
        while out.len() < size {
            self.partial = true;
            let want = (size - out.len()).min(CHUNK);
            let mut chunk = [0u8; CHUNK];
            let target = chunk.get_mut(..want).unwrap_or_default();
            let read = timeout_at(deadline, self.stream.read(target)).await;
            let count = match read {
                Err(_) => return Err(NetError::Timeout { phase, after: self.timeout }),
                Ok(Err(error)) => return Err(closed_or_io(phase, &error)),
                Ok(Ok(count)) => count,
            };
            if count == 0 {
                return Err(NetError::Closed { phase });
            }
            out.extend_from_slice(chunk.get(..count).unwrap_or_default());
            budget.spend(count)?;
        }
        Ok(out)
    }

    fn compact(&mut self) {
        if self.position == 0 {
            return;
        }
        if self.position == self.buffer.len() {
            self.buffer.clear();
            self.position = 0;
        } else if self.position >= COMPACT_AFTER {
            self.buffer.drain(..self.position);
            self.position = 0;
        }
    }
}

fn closed_or_io(phase: &'static str, error: &io::Error) -> NetError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted | io::ErrorKind::BrokenPipe => {
            NetError::Closed { phase }
        }
        _ => protocol(format!("{phase} failed: {error}")),
    }
}

/// Find CRLF in a byte slice.
fn find_crlf(bytes: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes.get(index) == Some(&b'\r') && bytes.get(index + 1) == Some(&b'\n') {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// If a line ends with a literal marker outside a quoted string, return its offset and size.
fn trailing_literal(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut in_quote = false;
    let mut escaped = false;
    let mut candidate: Option<usize> = None;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if in_quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_quote = false;
            }
            continue;
        }
        match byte {
            b'"' => in_quote = true,
            b'{' => candidate = Some(index),
            _ => {}
        }
    }
    let start = candidate?;
    let before_ok = start == 0 || bytes.get(start.wrapping_sub(1)).is_some_and(|byte| *byte == b' ' || *byte == b'(');
    if !before_ok {
        return None;
    }
    let after = line.get(start + 1..)?;
    let inner = after.strip_suffix('}')?;
    let digits = inner.strip_suffix('+').unwrap_or(inner);
    if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let size = digits.parse::<usize>().ok()?;
    Some((start, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_crlf() {
        assert_eq!(find_crlf(b"abc\r\n"), Some(3));
        assert_eq!(find_crlf(b"abc\n"), None);
        assert_eq!(find_crlf(b""), None);
    }

    #[test]
    fn detects_literal_markers() {
        assert_eq!(trailing_literal("* 1 FETCH (BODY[] {5}"), Some((18, 5)));
        assert_eq!(trailing_literal("* 1 FETCH (BODY[] {5+}"), Some((18, 5)));
        assert_eq!(trailing_literal("* 1 FETCH (BODY[] \"x {5}\""), None);
        assert_eq!(trailing_literal("* 1 FETCH (BODY[] {x}"), None);
        assert_eq!(trailing_literal("* 1 FETCH (BODY[] {99999999999999999999}"), None);
        assert_eq!(trailing_literal("* 1 FETCH (BODY[] {5} tail"), None);
    }

    #[test]
    fn budget_refuses_the_total() {
        let mut budget = Budget::new(10);
        assert!(budget.spend(6).is_ok());
        assert!(matches!(budget.spend(6), Err(NetError::TooLarge { .. })));
    }
}
