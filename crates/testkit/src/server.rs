//! The scripted server: one loopback listener, one connection, exact command recording.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use postino_net::{ImapConfig, Secret, TlsMode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::tls::TlsFixture;

/// Login name used by every test.
pub const USER: &str = "user@example.com";
/// Password used by every test. It contains a space on purpose: quoting must work.
pub const PASSWORD: &str = "correct horse battery";

/// The SASL PLAIN payload the client must send for [`USER`] and [`PASSWORD`].
#[must_use]
pub fn expected_sasl() -> String {
    STANDARD.encode(format!("\0{USER}\0{PASSWORD}"))
}

/// A server-side stream, plain or after the TLS handshake.
enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    /// Temporary value while the TLS handshake takes ownership of the socket.
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
            Self::Placeholder => Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::NotConnected))),
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

/// One accepted connection, with a line reader and a record of what the client sent.
pub struct Conn {
    stream: Stream,
    buffer: Vec<u8>,
    position: usize,
    received: Vec<String>,
}

impl Conn {
    fn new(stream: Stream) -> Self {
        Self { stream, buffer: Vec::new(), position: 0, received: Vec::new() }
    }

    /// Every line the client has sent so far, including command lines and SASL payloads.
    #[must_use]
    pub fn received(&self) -> &[String] {
        &self.received
    }

    /// Whether the client sent a line starting with `prefix`.
    #[must_use]
    pub fn sent_prefix(&self, prefix: &str) -> bool {
        self.received.iter().any(|line| line.starts_with(prefix))
    }

    fn take_line(&mut self) -> Option<String> {
        let available = self.buffer.get(self.position..)?;
        let mut index = 0;
        while index + 1 < available.len() {
            if available.get(index) == Some(&b'\r') && available.get(index + 1) == Some(&b'\n') {
                let raw = available.get(..index)?;
                let line = String::from_utf8_lossy(raw).into_owned();
                self.position += index + 2;
                if self.position == self.buffer.len() {
                    self.buffer.clear();
                    self.position = 0;
                }
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

    /// Read one CRLF-terminated line. Panics on a disconnect: a test server that expected a command
    /// and got EOF is a failed expectation.
    pub async fn read_line(&mut self) -> String {
        match self.read_line_opt().await {
            Some(line) => line,
            None => panic!("client closed the connection; lines so far: {:?}", self.received),
        }
    }

    /// Read one line, or `None` when the client closed the connection.
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

    /// Assert the next client line is exactly `expected`.
    pub async fn expect(&mut self, expected: &str) {
        let line = self.read_line().await;
        assert_eq!(line, expected);
    }

    /// Assert the next client line starts with `prefix`.
    pub async fn expect_prefix(&mut self, prefix: &str) {
        let line = self.read_line().await;
        assert!(line.starts_with(prefix), "expected a line starting with {prefix:?}, got {line:?}");
    }

    /// Write a string exactly as given.
    pub async fn send(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).await.expect("server write");
        self.stream.flush().await.expect("server flush");
    }

    /// Write a line with CRLF.
    pub async fn send_line(&mut self, line: &str) {
        self.send(&format!("{line}\r\n")).await;
    }

    /// Write raw bytes, literals included.
    pub async fn send_bytes(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("server write");
        self.stream.flush().await.expect("server flush");
    }

    /// Wait before the next step.
    pub async fn delay(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    /// Complete the server side of a TLS handshake. Returns `false` when the client rejected the
    /// certificate (which is the point of the wrong-hostname test).
    pub async fn accept_tls(&mut self, acceptor: TlsAcceptor) -> bool {
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

    /// Greeting and CAPABILITY, answered with `* CAPABILITY IMAP4rev1` plus `extra`.
    pub async fn prepare(&mut self, extra: &str) {
        self.send_line("* OK [CAPABILITY IMAP4rev1] fake IMAP ready").await;
        self.expect("A0001 CAPABILITY").await;
        if extra.is_empty() {
            self.send_line("* CAPABILITY IMAP4rev1").await;
        } else {
            self.send_line(&format!("* CAPABILITY IMAP4rev1 {extra}")).await;
        }
        self.send_line("A0001 OK done").await;
    }

    /// Answer the `LOGIN` command the client sends when `AUTH=PLAIN` is not advertised.
    pub async fn login_ok(&mut self) {
        self.expect_prefix("A0002 LOGIN ").await;
        self.send_line("A0002 OK logged in").await;
    }

    /// Greeting, CAPABILITY with `AUTH=PLAIN`, the full SASL exchange, and the post-login state.
    pub async fn prepare_sasl(&mut self) {
        self.send_line("* OK fake IMAP ready").await;
        self.expect("A0001 CAPABILITY").await;
        self.send_line("* CAPABILITY IMAP4rev1 AUTH=PLAIN IDLE").await;
        self.send_line("A0001 OK done").await;
        self.expect("A0002 AUTHENTICATE PLAIN").await;
        self.send_line("+ ").await;
        let payload = self.read_line().await;
        assert_eq!(payload, expected_sasl());
        self.send_line("A0002 OK [CAPABILITY IMAP4rev1 IDLE] logged in").await;
    }
}

/// A listener on `127.0.0.1` that runs one handler for one connection.
pub struct FakeServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl FakeServer {
    /// Accept one plaintext connection and run the handler.
    pub async fn start<F, Fut>(handler: F) -> Self
    where
        F: FnOnce(Conn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        Self::start_inner(None, handler).await
    }

    /// Accept one connection, complete the TLS handshake, then run the handler.
    pub async fn start_tls<F, Fut>(fixture: &TlsFixture, handler: F) -> Self
    where
        F: FnOnce(Conn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        Self::start_inner(Some(fixture.acceptor()), handler).await
    }

    /// Accept `connections` TLS connections on the same port, one after another, and run the handler
    /// for each.
    ///
    /// A reconciler test needs several sessions against one account: the account stores a port, and a
    /// second run must reach the same mailbox. The handler is called once per connection, so it can
    /// keep state (for example a mailbox behind an `Arc<Mutex<_>>`) and script a different outcome
    /// for each run. Each connection completes its handshake before the next is accepted.
    pub async fn start_tls_many<F, Fut>(fixture: &TlsFixture, connections: usize, handler: F) -> Self
    where
        F: Fn(Conn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind fake server");
        let address = listener.local_addr().expect("fake server address");
        let acceptor = fixture.acceptor();
        let task = tokio::spawn(async move {
            for _ in 0..connections {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut conn = Conn::new(Stream::Plain(stream));
                assert!(conn.accept_tls(acceptor.clone()).await, "the client failed the TLS handshake");
                handler(conn).await;
            }
        });
        Self { address, task }
    }

    async fn start_inner<F, Fut>(acceptor: Option<TlsAcceptor>, handler: F) -> Self
    where
        F: FnOnce(Conn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind fake server");
        let address = listener.local_addr().expect("fake server address");
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut conn = Conn::new(Stream::Plain(stream));
            if let Some(acceptor) = acceptor {
                assert!(conn.accept_tls(acceptor).await, "the client failed the TLS handshake");
            }
            handler(conn).await;
        });
        Self { address, task }
    }

    /// The port the listener was bound to.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// Client configuration for this server: `localhost`, implicit TLS, the test CA trusted.
    #[must_use]
    pub fn config(&self, fixture: &TlsFixture) -> ImapConfig {
        ImapConfig::new("localhost", self.port(), TlsMode::Implicit, USER, Secret::new(PASSWORD))
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

/// Shorthand: a configured `ImapConfig` with a short timeout, for corpus and failure tests.
#[must_use]
pub fn config_with_timeout(server: &FakeServer, fixture: &TlsFixture, timeout: Duration) -> ImapConfig {
    server.config(fixture).with_timeout(timeout)
}
