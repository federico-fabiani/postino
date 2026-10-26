//! A scripted SMTP submission server for the P7 tests.
//!
//! It listens on a loopback port, speaks either implicit TLS or the plaintext greeting plus
//! `STARTTLS`, records every command line and the exact `DATA` bytes, and lets a test script each
//! reply. Like the IMAP fake server it panics when an expectation is broken: a mismatched script is
//! a broken test, not a runtime condition.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use postino_net::{Secret, SmtpConfig, TlsMode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::tls::TlsFixture;

/// Login name used by the SMTP tests.
pub const SMTP_USER: &str = "user@example.com";
/// Password used by the SMTP tests.
pub const SMTP_PASSWORD: &str = "smtp correct horse";

/// The SASL PLAIN payload for [`SMTP_USER`] and [`SMTP_PASSWORD`].
#[must_use]
pub fn expected_smtp_sasl() -> String {
    STANDARD.encode(format!("\0{SMTP_USER}\0{SMTP_PASSWORD}"))
}

/// The base64 username `AUTH LOGIN` expects first.
#[must_use]
pub fn expected_smtp_login_user() -> String {
    STANDARD.encode(SMTP_USER)
}

/// The base64 password `AUTH LOGIN` expects second.
#[must_use]
pub fn expected_smtp_login_password() -> String {
    STANDARD.encode(SMTP_PASSWORD)
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    Placeholder,
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(context, buffer),
            Self::Placeholder => Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::NotConnected))),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, context: &mut Context<'_>, bytes: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(context, bytes),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(context, bytes),
            Self::Placeholder => Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::NotConnected))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(context),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(context),
            Self::Placeholder => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(context),
            Self::Placeholder => Poll::Ready(Ok(())),
        }
    }
}

/// One accepted SMTP connection, with a line reader and the recorded transcript.
pub struct SmtpConn {
    stream: Stream,
    buffer: Vec<u8>,
    position: usize,
    received: Vec<String>,
    data: Option<Vec<u8>>,
    acceptor: Option<TlsAcceptor>,
}

impl SmtpConn {
    fn new(stream: Stream, acceptor: Option<TlsAcceptor>) -> Self {
        Self { stream, buffer: Vec::new(), position: 0, received: Vec::new(), data: None, acceptor }
    }

    /// Every command line the client sent, in order. DATA is not in this list; use [`Self::data`].
    #[must_use]
    pub fn received(&self) -> &[String] {
        &self.received
    }

    /// The un-stuffed DATA payload the client sent, once it has completed the transaction.
    #[must_use]
    pub fn data(&self) -> Option<&[u8]> {
        self.data.as_deref()
    }

    /// Whether any command line contains `needle`. Used to prove a password never appears outside
    /// the authentication exchange.
    #[must_use]
    pub fn transcript_contains(&self, needle: &str) -> bool {
        self.received.iter().any(|line| line.contains(needle))
    }

    /// Greeting plus the first `EHLO` exchange.
    pub async fn greet(&mut self, capabilities: &[&str]) {
        self.send_line("220 fake SMTP ready").await;
        self.ehlo(capabilities).await;
    }

    /// Expect an `EHLO` and answer with the given capability lines.
    pub async fn ehlo(&mut self, capabilities: &[&str]) {
        self.expect_prefix("EHLO ").await;
        if capabilities.is_empty() {
            self.send_line("250 fake.example.com").await;
            return;
        }
        for (index, capability) in capabilities.iter().enumerate() {
            let separator = if index + 1 == capabilities.len() { ' ' } else { '-' };
            self.send_line(&format!("250{separator}{capability}")).await;
        }
    }

    /// Read one CRLF-terminated line, recording it.
    pub async fn read_line(&mut self) -> String {
        self.read_line_opt().await.unwrap_or_else(|| panic!("client closed the connection; lines so far: {:?}", self.received))
    }

    /// Read one line, or `None` on EOF.
    pub async fn read_line_opt(&mut self) -> Option<String> {
        loop {
            if let Some(line) = self.take_line() {
                self.received.push(line.clone());
                return Some(line);
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    /// Assert the next line is exactly `expected`.
    pub async fn expect(&mut self, expected: &str) -> String {
        let line = self.read_line().await;
        assert_eq!(line, expected);
        line
    }

    /// Assert the next line starts with `prefix` and return it.
    pub async fn expect_prefix(&mut self, prefix: &str) -> String {
        let line = self.read_line().await;
        assert!(line.starts_with(prefix), "expected a line starting with {prefix:?}, got {line:?}");
        line
    }

    /// Write a line with CRLF.
    pub async fn send_line(&mut self, line: &str) {
        self.send(&format!("{line}\r\n")).await;
    }

    /// Write bytes exactly as given.
    pub async fn send(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).await.expect("server write");
        self.stream.flush().await.expect("server flush");
    }

    /// Write raw bytes.
    pub async fn send_bytes(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("server write");
        self.stream.flush().await.expect("server flush");
    }

    /// Wait before the next step.
    pub async fn delay(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    /// Read the DATA payload after the client's `DATA` command. Returns the un-stuffed bytes.
    pub async fn read_data(&mut self) -> Vec<u8> {
        let mut raw = Vec::new();
        loop {
            let line = self.read_line_raw().await.unwrap_or_else(|| panic!("client closed during DATA; lines so far: {:?}", self.received));
            if line == b"." {
                break;
            }
            let unstuffed = line.strip_prefix(b".").map_or(line.as_slice(), |rest| rest);
            raw.extend_from_slice(unstuffed);
            raw.extend_from_slice(b"\r\n");
        }
        self.data = Some(raw.clone());
        raw
    }

    /// Complete the server side of a STARTTLS handshake.
    pub async fn upgrade_tls(&mut self) -> bool {
        let Some(acceptor) = self.acceptor.take() else {
            panic!("this server was not started for STARTTLS");
        };
        let placeholder = std::mem::replace(&mut self.stream, Stream::Placeholder);
        let Stream::Plain(tcp) = placeholder else {
            panic!("TLS can only start on a plaintext connection");
        };
        match acceptor.accept(tcp).await {
            Ok(tls) => {
                self.stream = Stream::Tls(Box::new(tls));
                true
            }
            Err(_) => false,
        }
    }

    /// Evaluate `AUTH PLAIN` and answer 235 on a match, 535 otherwise.
    pub async fn auth_plain_ok(&mut self, expected: &str) {
        let line = self.expect_prefix("AUTH PLAIN ").await;
        let payload = line.strip_prefix("AUTH PLAIN ").unwrap_or_default();
        if payload == expected {
            self.send_line("235 2.7.0 authentication successful").await;
        } else {
            self.send_line("535 5.7.8 authentication credentials invalid").await;
        }
    }

    /// Evaluate `AUTH LOGIN` and answer 235 on a match, 535 otherwise.
    pub async fn auth_login_ok(&mut self, user: &str, password: &str) {
        self.expect("AUTH LOGIN").await;
        self.send_line("334 VXNlcm5hbWU6").await;
        let got_user = self.read_line().await;
        if got_user != user {
            self.send_line("535 5.7.8 authentication credentials invalid").await;
            return;
        }
        self.send_line("334 UGFzc3dvcmQ6").await;
        let got_password = self.read_line().await;
        if got_password == password {
            self.send_line("235 2.7.0 authentication successful").await;
        } else {
            self.send_line("535 5.7.8 authentication credentials invalid").await;
        }
    }

    /// Read one raw CRLF line, without recording it in the command transcript.
    async fn read_line_raw(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(line) = self.take_raw_line() {
                return Some(line);
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    fn take_raw_line(&mut self) -> Option<Vec<u8>> {
        let available = self.buffer.get(self.position..)?;
        let mut index = 0;
        while index + 1 < available.len() {
            if available.get(index) == Some(&b'\r') && available.get(index + 1) == Some(&b'\n') {
                let raw = available.get(..index)?.to_vec();
                self.position += index + 2;
                self.compact();
                return Some(raw);
            }
            index += 1;
        }
        None
    }

    fn take_line(&mut self) -> Option<String> {
        let available = self.buffer.get(self.position..)?;
        let mut index = 0;
        while index + 1 < available.len() {
            if available.get(index) == Some(&b'\r') && available.get(index + 1) == Some(&b'\n') {
                let raw = available.get(..index)?;
                let line = String::from_utf8_lossy(raw).into_owned();
                self.position += index + 2;
                self.compact();
                return Some(line);
            }
            index += 1;
        }
        None
    }

    async fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk).await {
            Ok(0) | Err(_) => false,
            Ok(count) => {
                self.buffer.extend_from_slice(chunk.get(..count).unwrap_or_default());
                true
            }
        }
    }

    fn compact(&mut self) {
        if self.position == self.buffer.len() {
            self.buffer.clear();
            self.position = 0;
        }
    }
}

/// A loopback SMTP server that runs one handler for one connection.
pub struct FakeSmtp {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl FakeSmtp {
    /// Implicit TLS: handshake first, then the handler sees the SMTP conversation.
    pub async fn start_tls<F, Fut>(fixture: &TlsFixture, handler: F) -> Self
    where
        F: FnOnce(SmtpConn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind fake SMTP");
        let address = listener.local_addr().expect("fake SMTP address");
        let acceptor = fixture.acceptor();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut conn = SmtpConn::new(Stream::Plain(stream), None);
            assert!(upgrade(&mut conn, acceptor).await, "the client failed the implicit TLS handshake");
            handler(conn).await;
        });
        Self { address, task }
    }

    /// Implicit TLS where a failed handshake ends the connection instead of failing the test. Used
    /// by the wrong-hostname test: the client must refuse the certificate.
    pub async fn start_tls_tolerant<F, Fut>(fixture: &TlsFixture, handler: F) -> Self
    where
        F: FnOnce(SmtpConn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind fake SMTP");
        let address = listener.local_addr().expect("fake SMTP address");
        let acceptor = fixture.acceptor();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut conn = SmtpConn::new(Stream::Plain(stream), None);
            if upgrade(&mut conn, acceptor).await {
                handler(conn).await;
            }
        });
        Self { address, task }
    }

    /// Accept `connections` implicit-TLS connections on one port, calling the handler with the
    /// zero-based connection index. The backoff pipeline test needs the same stored endpoint to
    /// answer 451 and then 250.
    pub async fn start_tls_many<F, Fut>(fixture: &TlsFixture, connections: usize, mut handler: F) -> Self
    where
        F: FnMut(usize, SmtpConn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind fake SMTP");
        let address = listener.local_addr().expect("fake SMTP address");
        let acceptor = fixture.acceptor();
        let task = tokio::spawn(async move {
            for index in 0..connections {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut conn = SmtpConn::new(Stream::Plain(stream), None);
                assert!(upgrade(&mut conn, acceptor.clone()).await, "the client failed the implicit TLS handshake");
                handler(index, conn).await;
            }
        });
        Self { address, task }
    }

    /// Plaintext greeting and `EHLO`, then a scripted `STARTTLS` upgrade.
    pub async fn start_starttls<F, Fut>(fixture: &TlsFixture, handler: F) -> Self
    where
        F: FnOnce(SmtpConn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind fake SMTP");
        let address = listener.local_addr().expect("fake SMTP address");
        let acceptor = fixture.acceptor();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let conn = SmtpConn::new(Stream::Plain(stream), Some(acceptor));
            handler(conn).await;
        });
        Self { address, task }
    }

    /// The bound port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// A client configuration: `localhost`, implicit TLS, the test CA trusted.
    #[must_use]
    pub fn config(&self, fixture: &TlsFixture) -> SmtpConfig {
        SmtpConfig::new("localhost", self.port(), TlsMode::Implicit, SMTP_USER, Secret::new(SMTP_PASSWORD))
            .with_timeout(Duration::from_secs(5))
            .with_extra_root(fixture.ca())
    }

    /// A client configuration for STARTTLS.
    #[must_use]
    pub fn starttls_config(&self, fixture: &TlsFixture) -> SmtpConfig {
        SmtpConfig::new("localhost", self.port(), TlsMode::StartTls, SMTP_USER, Secret::new(SMTP_PASSWORD))
            .with_timeout(Duration::from_secs(5))
            .with_extra_root(fixture.ca())
    }

    /// Wait for the handler and propagate its assertion failures.
    pub async fn finish(self) {
        if let Err(error) = self.task.await {
            std::panic::resume_unwind(error.into_panic());
        }
    }
}

async fn upgrade(conn: &mut SmtpConn, acceptor: TlsAcceptor) -> bool {
    let placeholder = std::mem::replace(&mut conn.stream, Stream::Placeholder);
    let Stream::Plain(tcp) = placeholder else {
        return false;
    };
    match acceptor.accept(tcp).await {
        Ok(tls) => {
            conn.stream = Stream::Tls(Box::new(tls));
            true
        }
        Err(_) => false,
    }
}
